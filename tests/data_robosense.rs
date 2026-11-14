//! Manual-derived `RoboSense` UDP fixtures; not physical-device qualification.
mod support;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    net::{SocketAddr, UdpSocket},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::cli::{Home, Run};
const ARGS: [&str; 9] = [
    "--json",
    "data",
    "record",
    "--config",
    "sources.json",
    "--for",
    "1s",
    "-o",
    "run.jsonl",
];
const PROFILES: [&str; 4] = [
    "helios16p_msop",
    "helios16p_difop",
    "rs16_msop",
    "rs16_difop",
];
fn datagram_socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&socket)
        .set_send_buffer_size(128 * 1024)
        .unwrap();
    socket
}
fn msop(profile: &str, unit_flag: u8) -> Vec<u8> {
    let helios = profile == "helios16p_msop";
    let mut raw = vec![0; 1248];
    if helios {
        raw[..4].copy_from_slice(&[0x55, 0xaa, 5, 0x5a]);
        raw[4..6].copy_from_slice(&1_u16.to_be_bytes());
        raw[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        raw[12..16].copy_from_slice(&0x8000_0000_u32.to_be_bytes());
        raw[17] = unit_flag;
        raw[20..30].fill(255);
        // EN v1.0.1: zero-based31=07; byte30 is reserved.
        raw[31] = 7;
    } else {
        raw[..8].copy_from_slice(&[0x55, 0xaa, 5, 0x0a, 0x5a, 0xa5, 0x50, 0xa0]);
        raw[20..26].copy_from_slice(&[255, 0, 255, 25, 61, 255]);
        raw[26..30].fill(255);
        raw[30] = 1;
    }
    for block in 0..12 {
        let start = 42 + block * 100;
        raw[start..start + 2].copy_from_slice(&[255, 0xee]);
        let azimuth = if block == 11 {
            u16::MAX
        } else {
            u16::try_from(block).unwrap() * 123
        };
        raw[start + 2..start + 4].copy_from_slice(&azimuth.to_be_bytes());
        for tuple in 0..32 {
            let offset = start + 4 + tuple * 3;
            let distance = match tuple {
                0 => 0_u16,
                1 => 1,
                2 => u16::MAX,
                _ => u16::try_from(tuple).unwrap(),
            };
            raw[offset..offset + 2].copy_from_slice(&distance.to_be_bytes());
            raw[offset + 2] = if tuple == 2 {
                255
            } else {
                u8::try_from(tuple).unwrap()
            };
        }
    }
    raw[1242..1246].copy_from_slice(&[0, 128, 255, 17]);
    raw[1246..1248].copy_from_slice(&[0, 255]);
    raw
}
fn difop(profile: &str, mode: u8) -> Vec<u8> {
    let helios = profile == "helios16p_difop";
    let mut raw = vec![0xdf; 1248];
    raw[..8].copy_from_slice(&[0xa5, 255, 0, 0x5a, 0x11, 0x11, 0x55, 0x55]);
    raw[8..10].copy_from_slice(&600_u16.to_be_bytes());
    raw[10..32].copy_from_slice(&[
        192, 0, 2, 1, 192, 0, 2, 2, 0, 128, 255, 1, 2, 3, 0x1a, 0x2b, 0x3c, 0x4d, 5, 6, 7, 8,
    ]);
    raw[32..34].copy_from_slice(&0_u16.to_be_bytes());
    raw[34..36].copy_from_slice(&35_999_u16.to_be_bytes());
    raw[38..40].copy_from_slice(&u16::MAX.to_be_bytes());
    raw[292..298].copy_from_slice(&[0, 128, 255, 1, 2, 3]);
    raw[298..300].copy_from_slice(&u16::MAX.to_be_bytes());
    raw[300] = mode;
    raw[301..303].copy_from_slice(&[255, 128]);
    raw[303..313].fill(255);
    raw[382..468].fill(0x80);
    raw[382..395].copy_from_slice(b"$GPRMC,x*00\r\n");
    if helios {
        for start in [468, 564] {
            for channel in 0..16 {
                let offset = start + channel * 3;
                raw[offset..offset + 3].copy_from_slice(&[
                    255,
                    128,
                    u8::try_from(channel).unwrap(),
                ]);
            }
        }
    } else {
        raw[1165..1213].fill(255);
    }
    raw[1246..1248].copy_from_slice(&[15, 240]);
    raw
}
fn packet(profile: &str) -> Vec<u8> {
    if profile.ends_with("_difop") {
        difop(profile, 0)
    } else {
        msop(profile, 0)
    }
}
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
fn samples(home: &Home) -> Vec<Value> {
    rows(home)
        .into_iter()
        .filter(|row| row["kind"] == "sample")
        .collect()
}
fn groups(home: &Home) -> Vec<Value> {
    samples(home)
        .into_iter()
        .filter(|row| row["value"]["kind"] == "robosense_udp_group")
        .collect()
}
fn wait(mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(Instant::now() < end, "fixture did not reach state");
        thread::sleep(Duration::from_millis(2));
    }
}
struct Capture(Option<Child>);
impl Capture {
    fn start(
        home: &Home,
        source: SocketAddr,
        profile: &str,
        context: Option<&[u8]>,
        patch: Value,
    ) -> (Self, SocketAddr) {
        let reservation = datagram_socket();
        let target = reservation.local_addr().unwrap();
        if let Some(context) = context {
            fs::write(home.path("configured-context"), context).unwrap();
        }
        let mut selected = json!({"id":"lidar","protocol":"robosense_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),
            "profile":profile,"output":"raw.robosense","max_bytes":100_000_000,"packets_per_group":32,"timeout_ms":120,"every_ms":3_600_000});
        if context.is_some() {
            selected["context"] = json!("configured-context");
        }
        let Value::Object(patch) = patch else {
            panic!("fixture patch must be an object")
        };
        for (key, value) in patch {
            selected[key] = value;
        }
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[selected]}).to_string(),
        )
        .unwrap();
        drop(reservation);
        let child = home
            .command()
            .args(ARGS)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait(|| {
            rows(home)
                .first()
                .is_some_and(|row| row["kind"] == "header")
        });
        (Self(Some(child)), target)
    }
    fn finish(mut self) -> Run {
        Run::of_output(self.0.take().unwrap().wait_with_output().unwrap(), &ARGS)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.kill());
            drop(child.wait());
        }
    }
}
fn u32_at(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap())
}
fn u64_at(raw: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap())
}
fn digest(raw: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw))
}
fn archive(home: &Home, context: Option<&[u8]>, packets: &[Vec<u8>]) {
    let original = context.unwrap_or_default();
    let raw = fs::read(home.path("raw.robosense")).unwrap();
    assert_eq!(&raw[..8], b"CFROBO1\0");
    assert_eq!(u32_at(&raw, 12), u32::try_from(original.len()).unwrap());
    assert_eq!(&raw[80..80 + original.len()], original);
    assert_eq!(&raw[40..72], Sha256::digest(original).as_slice());
    assert_eq!(
        u16::from_le_bytes(raw[22..24].try_into().unwrap()),
        u16::from(context.is_some())
    );
    assert_eq!(&raw[76..80], &[0; 4]);
    let mut index = 0;
    let mut offset = 80 + original.len();
    for row in groups(home) {
        let value = &row["value"];
        assert_eq!(value["offset"], offset);
        assert!(serde_json::to_vec(&row).unwrap().len() < 1024 * 1024);
        let length = usize::try_from(value["length"].as_u64().unwrap()).unwrap();
        let group = &raw[offset..offset + length];
        assert_eq!(&group[..8], b"CFRGRP1\0");
        assert_eq!(value["sha256"], digest(group));
        if let Some(context) = context {
            assert_eq!(value["context"]["sha256"], digest(context));
        } else {
            assert!(value.get("context").is_none());
        }
        assert_eq!(
            value["packet_count"],
            value["datagrams"].as_array().unwrap().len()
        );
        let mut cursor = 32;
        for packet in value["datagrams"].as_array().unwrap() {
            assert_eq!(&group[cursor..cursor + 4], b"PKT1");
            let bytes = usize::try_from(u32_at(group, cursor + 4)).unwrap();
            assert_eq!(&group[cursor + 32..cursor + 32 + bytes], &packets[index]);
            assert_eq!(packet["raw_offset"], offset + cursor + 32);
            assert_eq!(packet["raw_length"], bytes);
            assert_eq!(packet["host_packet_index"], index + 1);
            assert_eq!(packet["received_monotonic_ns"], u64_at(group, cursor + 16));
            assert!(row.get("source_time").is_none());
            index += 1;
            cursor += 32 + bytes;
        }
        assert_eq!(cursor + 56, length);
        assert_eq!(&group[cursor..cursor + 8], b"CFRCMT1\0");
        assert_eq!(u64_at(group, cursor + 16), u64::try_from(cursor).unwrap());
        assert_eq!(
            &group[cursor + 24..],
            Sha256::digest(&group[..cursor]).as_slice()
        );
        offset += length;
    }
    assert_eq!(index, packets.len());
    assert_eq!(offset, raw.len());
    let samples = samples(home);
    let terminal = &samples.last().unwrap()["value"];
    assert_eq!(terminal["kind"], "robosense_udp_file");
    assert_eq!(terminal["committed_bytes"], raw.len());
    assert_eq!(terminal["sha256_committed_prefix"], digest(&raw));
    assert_eq!(terminal["jsonl_publication_not_proven"], true);
    assert_eq!(terminal["storage_finalized"], true);
    assert_eq!(terminal["uncommitted_tail_bytes"], 0);
}
fn exports(home: &Home) {
    for format in ["jsonl", "csv"] {
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            &format!("export.{format}"),
        ]);
    }
    let exported = fs::read_to_string(home.path("export.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), samples(home).len());
    let csv = fs::read_to_string(home.path("export.csv")).unwrap();
    for (original, exported) in samples(home).iter().zip(exported) {
        assert_eq!(original["value"], exported["value"]);
        assert!(csv.contains(&format!(
            "\"{}\"",
            original["value"].to_string().replace('"', "\"\"")
        )));
    }
}

#[test]
fn all_four_profiles_keep_native_raw_context_clocks_and_exact_artifact_exports() {
    for profile in PROFILES {
        let home = Home::new("robosense-profiles");
        let source = datagram_socket();
        let context = b"Laser id,Elevation,Azimuth\n1,15.0,0.0\n";
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            profile,
            Some(context),
            json!({"packets_per_group":2.0,"max_bytes":1e8}),
        );
        let raw = packet(profile);
        source.send_to(&raw, target).unwrap();
        source.send_to(&raw, target).unwrap();
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, Some(context), &[raw.clone(), raw]);
        let all = groups(&home);
        let value = &all[0]["value"]["datagrams"][0];
        assert_eq!(value["profile"], profile);
        assert_eq!(value["checksum_status"], "no_checksum_in_format");
        assert_eq!(
            all[0]["value"]["sensor"]["difop_association"],
            "not_performed"
        );
        assert!(value.get("sequence_observation").is_none());
        assert!(value.get("source_time").is_none());
        if profile.ends_with("_msop") {
            assert_eq!(value["blocks"].as_array().unwrap().len(), 12);
            for block in value["blocks"].as_array().unwrap() {
                assert_eq!(block["distance_raw"].as_array().unwrap().len(), 32);
                assert_eq!(block["distance_raw"][0], 0);
                assert_eq!(block["distance_raw"][1], 1);
                assert_eq!(block["distance_raw"][2], u16::MAX);
                assert_eq!(block["reflectivity_raw"][2], 255);
            }
            assert_eq!(value["blocks"][11]["azimuth_range_status"], "unknown");
            if profile == "helios16p_msop" {
                assert_eq!(value["unit_status"], "documented_header");
                assert_eq!(value["distance_unit_um"], 5_000);
                assert_eq!(value["blocks"][0]["distance_um"][2], 327_675_000);
                assert_eq!(value["clock"]["seconds_raw"], 281_474_976_710_655_u64);
                assert_eq!(value["clock"]["subsecond_raw"], u32::MAX);
                assert_eq!(
                    value["clock"]["subsecond_units_status"],
                    "unresolved_vendor_documentation"
                );
            } else {
                assert_eq!(value["unit_status"], "unavailable_firmware_dependent");
                assert!(value["blocks"][0].get("distance_um").is_none());
                assert_eq!(value["clock"]["calendar_decoding"], "raw_only");
                assert_eq!(value["clock"]["millisecond_word_raw"], u16::MAX);
                assert_eq!(value["clock"]["microsecond_word_raw"], u16::MAX);
            }
        } else {
            assert_eq!(value["packet_type"], "difop");
            assert_eq!(value["motor_speed_rpm"], 600);
            assert_eq!(value["serial_raw"], json!([0, 128, 255, 1, 2, 3]));
            assert_eq!(value["return_mode_raw"], 0);
            assert_eq!(value["msop_association"], "unavailable");
            if profile == "helios16p_difop" {
                assert_eq!(value["vertical_calibration_raw"][0]["sign_raw"], 255);
                assert_eq!(
                    value["horizontal_calibration_raw"][0]["magnitude_raw"],
                    32_768
                );
            } else {
                assert_eq!(value["vertical_calibration_raw"][0], 0x00ff_ffff_u32);
            }
        }
        exports(&home);
        source
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        assert!(matches!(
            source.recv_from(&mut [0; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
    }
}
#[test]
fn helios_documented_header_variants_and_unknown_unit_or_mode_states_stay_explicit() {
    for newer in [false, true] {
        let home = Home::new("robosense-helios-variant");
        let source = datagram_socket();
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            "helios16p_msop",
            Some(b""),
            json!({"packets_per_group":2}),
        );
        let mut known = msop("helios16p_msop", 1);
        if newer {
            known[31] = 6;
            known[32] = 3;
            known[26..30].copy_from_slice(&999_999_u32.to_be_bytes());
        }
        let mut unknown = known.clone();
        unknown[17] = 0xfe;
        unknown[26..30].fill(255);
        source.send_to(&known, target).unwrap();
        source.send_to(&unknown, target).unwrap();
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, Some(b""), &[known, unknown]);
        let all = groups(&home);
        let values = all[0]["value"]["datagrams"].as_array().unwrap();
        assert_eq!(
            values[0]["header_variant"],
            if newer {
                "type_06_model_03"
            } else {
                "legacy_shared_07"
            }
        );
        assert_eq!(values[0]["distance_unit_um"], 2_500);
        assert_eq!(values[0]["blocks"][0]["distance_um"][2], 163_837_500);
        assert_eq!(values[1]["unit_status"], "unknown");
        assert!(values[1]["blocks"][0].get("distance_um").is_none());
        assert!(values[1].get("distance_unit_um").is_none());
        if newer {
            assert_eq!(
                values[0]["clock"]["subsecond_units_status"],
                "documented_microseconds"
            );
        }
    }
    for profile in ["helios16p_difop", "rs16_difop"] {
        let home = Home::new("robosense-mode-state");
        let source = datagram_socket();
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            profile,
            None,
            json!({"packets_per_group":2}),
        );
        let disputed = difop(profile, 6);
        let unknown = difop(profile, 0xff);
        source.send_to(&disputed, target).unwrap();
        source.send_to(&unknown, target).unwrap();
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, None, &[disputed, unknown]);
        let all = groups(&home);
        let values = all[0]["value"]["datagrams"].as_array().unwrap();
        assert_eq!(
            values[0]["return_mode_status"],
            if profile == "helios16p_difop" {
                "unresolved_vendor_documentation"
            } else {
                "unknown"
            }
        );
        assert_eq!(values[1]["return_mode_status"], "unknown");
        assert!(
            values
                .iter()
                .all(|value| value.get("return_mode").is_none())
        );
    }
}
#[test]
fn configured_rs16_units_scale_exactly_and_keep_the_unverified_declaration_in_archive() {
    for unit in [5_000_u32, 10_000] {
        let home = Home::new("robosense-configured-unit");
        let source = datagram_socket();
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            "rs16_msop",
            None,
            json!({"packets_per_group":1,"rs16_distance_unit_um":unit}),
        );
        let raw = msop("rs16_msop", 0);
        source.send_to(&raw, target).unwrap();
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, None, &[raw]);
        let all = groups(&home);
        let value = &all[0]["value"]["datagrams"][0];
        assert_eq!(value["unit_status"], "configured_unverified");
        assert_eq!(value["unit_origin"], "configured");
        assert_eq!(value["distance_unit_um"], unit);
        assert_eq!(
            value["blocks"][0]["distance_um"][2],
            u32::from(u16::MAX) * unit
        );
        assert_eq!(all[0]["value"]["sensor"]["rs16_distance_unit_um"], unit);
        assert_eq!(
            all[0]["value"]["sensor"]["distance_unit_origin"],
            "configured"
        );
        let archive = fs::read(home.path("raw.robosense")).unwrap();
        assert_eq!(u32_at(&archive, 72), unit);
        exports(&home);
    }
}
#[test]
fn malformed_or_unsupported_selected_packets_keep_the_earlier_immutable_prefix() {
    for (profile, mode) in [
        ("helios16p_msop", "short"),
        ("helios16p_msop", "long"),
        ("helios16p_msop", "block"),
        ("helios16p_msop", "version"),
        ("helios16p_msop", "model"),
        ("helios16p_msop", "wrong_1615"),
        ("helios16p_msop", "tail"),
        ("rs16_msop", "model"),
        ("rs16_msop", "magic"),
        ("rs16_msop", "cross"),
        ("helios16p_difop", "magic"),
        ("helios16p_difop", "tail"),
        ("rs16_difop", "short"),
        ("rs16_difop", "cross"),
    ] {
        let home = Home::new("robosense-retire");
        let source = datagram_socket();
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            profile,
            None,
            json!({"packets_per_group":1}),
        );
        let first = packet(profile);
        source.send_to(&first, target).unwrap();
        wait(|| groups(&home).len() == 1);
        let mut bad = packet(profile);
        match mode {
            "short" => {
                bad.pop();
            }
            "long" => bad.push(0),
            "block" => bad[42] = 0,
            "version" => bad[5] = 2,
            "model" => {
                if profile == "rs16_msop" {
                    bad[30] = 2;
                } else {
                    bad[31] = 0xff;
                }
            }
            "wrong_1615" => {
                bad[31] = 6;
                bad[32] = 2;
            }
            "tail" => bad[1247] = 0,
            "magic" => bad[0] = 0,
            "cross" => {
                bad = if profile.ends_with("_msop") {
                    difop("rs16_difop", 0)
                } else {
                    msop("rs16_msop", 0)
                };
            }
            _ => unreachable!(),
        }
        source.send_to(&bad, target).unwrap();
        let error =
            capture
                .finish()
                .failure(if matches!(mode, "version" | "model" | "wrong_1615") {
                    5
                } else {
                    1
                });
        let prefix = &error["details"]["robosense_udp"];
        assert_eq!(prefix["packets"], 1);
        assert_eq!(groups(&home).len(), 1);
        let raw = fs::read(home.path("raw.robosense")).unwrap();
        assert_eq!(prefix["committed_bytes"], raw.len());
        assert_eq!(prefix["sha256_committed_prefix"], digest(&raw));
    }
}
#[test]
fn paced_foreign_allowance_and_malformed_current_group_do_not_erase_committed_packets() {
    for foreign in [false, true] {
        let home = Home::new("robosense-traffic");
        let source = datagram_socket();
        let outsider = datagram_socket();
        let profile = "helios16p_msop";
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            profile,
            None,
            json!({"packets_per_group":2}),
        );
        let raw = packet(profile);
        source.send_to(&raw, target).unwrap();
        source.send_to(&raw, target).unwrap();
        wait(|| groups(&home).len() == 1);
        if foreign {
            let foreign_packet = vec![0; 24_000];
            for _ in 0..3 {
                outsider.send_to(&foreign_packet, target).unwrap();
                thread::sleep(Duration::from_millis(3));
            }
        } else {
            source.send_to(&raw, target).unwrap();
            source.send_to(&raw[..1247], target).unwrap();
        }
        let error = capture.finish().failure(1);
        let prefix = &error["details"]["robosense_udp"];
        assert_eq!(prefix["packets"], 2);
        assert_eq!(groups(&home).len(), 1);
        if foreign {
            assert_eq!(prefix["foreign_packets"], 3);
        } else {
            assert_eq!(prefix["uncommitted_packets"], 1);
        }
    }
}
#[test]
fn storage_caps_no_overwrite_and_free_floor_preserve_only_owned_committed_ranges() {
    for mode in ["exists", "floor", "limit"] {
        let home = Home::new("robosense-storage");
        let source = datagram_socket();
        let profile = "helios16p_msop";
        let raw = packet(profile);
        let minimum = 80 + 32 + 32 + raw.len() + 56;
        if mode == "exists" {
            fs::write(home.path("raw.robosense"), b"preexisting bytes").unwrap();
        }
        let patch = match mode {
            "floor" => json!({"min_free_bytes":u64::MAX,"packets_per_group":1}),
            "limit" => json!({"max_bytes":minimum,"packets_per_group":2}),
            _ => json!({"packets_per_group":1}),
        };
        let (capture, target) =
            Capture::start(&home, source.local_addr().unwrap(), profile, None, patch);
        source.send_to(&raw, target).unwrap();
        if mode == "limit" {
            source.send_to(&raw, target).unwrap();
        }
        let error = capture
            .finish()
            .failure(if mode == "exists" { 1 } else { 6 });
        if mode == "exists" {
            assert_eq!(error["code"], "exists");
            assert_eq!(
                fs::read(home.path("raw.robosense")).unwrap(),
                b"preexisting bytes"
            );
        } else if mode == "floor" {
            assert_eq!(error["code"], "storage_full");
            assert!(!home.path("raw.robosense").exists());
        } else {
            assert_eq!(error["code"], "storage_full");
            assert_eq!(error["details"]["robosense_udp"]["packets"], 1);
            assert_eq!(error["details"]["robosense_udp"]["uncommitted_packets"], 1);
            assert_eq!(
                fs::metadata(home.path("raw.robosense")).unwrap().len(),
                u64::try_from(minimum).unwrap()
            );
        }
    }
}
#[test]
fn all_configured_packets_are_retained_across_group_windows_and_tuple_bounds() {
    let home = Home::new("robosense-32-packets");
    let source = datagram_socket();
    let profile = "helios16p_msop";
    let (capture, target) = Capture::start(
        &home,
        source.local_addr().unwrap(),
        profile,
        None,
        json!({"packets_per_group":32}),
    );
    let packets = std::iter::repeat_with(|| packet(profile))
        .take(32)
        .collect::<Vec<_>>();
    for raw in &packets {
        source.send_to(raw, target).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(capture.finish().success()["status"], "complete");
    archive(&home, None, &packets);
    let all = groups(&home);
    assert!(
        all.iter()
            .all(|group| group["value"]["packet_count"].as_u64().unwrap() <= 32)
    );
}
#[test]
fn configuration_bounds_context_cap_and_unit_scope_fail_before_artifacts() {
    for (key, value) in [
        ("listen", json!("0.0.0.0:0")),
        ("source_ip", json!("239.1.2.3")),
        ("source_port", json!(0)),
        ("packets_per_group", json!(33)),
        ("packets_per_group", json!(1.5)),
        ("max_bytes", json!(1)),
        ("output", json!("{seq}.robosense")),
        ("rs16_distance_unit_um", json!(5e3)),
        (
            "multicast",
            json!({"group":"239.1.2.3","interface":"0.0.0.0"}),
        ),
        (
            "multicast",
            json!({"group":"239.1.2.3","interface":"127.0.0.1"}),
        ),
    ] {
        let home = Home::new("robosense-config");
        let mut source = json!({"id":"lidar","protocol":"robosense_udp","listen":"127.0.0.1:6699","source_ip":"127.0.0.1","profile":"helios16p_msop","output":"raw.robosense","max_bytes":1_000_000});
        source[key] = value;
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[source]}).to_string(),
        )
        .unwrap();
        assert_eq!(home.run(&ARGS).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("raw.robosense").exists());
    }
    let home = Home::new("robosense-context-cap");
    fs::write(home.path("too-large"), vec![0; 1024 * 1024 + 1]).unwrap();
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[{"id":"lidar","protocol":"robosense_udp","listen":"127.0.0.1:6699","source_ip":"127.0.0.1","profile":"helios16p_msop","context":"too-large","output":"raw.robosense","max_bytes":2_000_000}]}).to_string()).unwrap();
    assert_eq!(home.run(&ARGS).failure(2)["code"], "usage");
    assert!(!home.path("run.jsonl").exists());
    assert!(!home.path("raw.robosense").exists());
}
