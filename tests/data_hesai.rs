//! Manual-derived literal datagrams through the CLI; not physical-device qualification.
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
    // Give concurrent CI processes headroom before the acquisition cutoff.
    // The source's original 120 ms read deadline remains unchanged.
    "1s",
    "-o",
    "run.jsonl",
];
const PROFILE: &str = "pandar_xt32_v6_1";
const MODES: [u8; 6] = [0x33, 0x37, 0x38, 0x39, 0x3b, 0x3c];
fn datagram_socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&socket)
        .set_send_buffer_size(128 * 1024)
        .unwrap();
    socket
}
fn lidar(mode: u8, sequence: u32) -> Vec<u8> {
    // Exact independent literal layout from PandarXT X01-en-250220 printed44..52.
    let mut raw = vec![0; 1080];
    raw[..12].copy_from_slice(&[0xee, 0xff, 6, 1, 0x80, 0xff, 32, 8, 0xff, 4, 2, 0x81]);
    for block in 0..8 {
        let start = 12 + block * 130;
        let azimuth = u16::try_from(if mode >= 0x39 { block / 2 } else { block }).unwrap() * 123;
        raw[start..start + 2].copy_from_slice(&azimuth.to_le_bytes());
        for channel in 0..32 {
            let offset = start + 2 + channel * 4;
            let distance = match channel {
                0 => 0_u16,
                1 => 1,
                2 => u16::MAX,
                _ => u16::try_from(channel).unwrap(),
            };
            raw[offset..offset + 2].copy_from_slice(&distance.to_le_bytes());
            raw[offset + 2] = if channel == 2 {
                u8::MAX
            } else {
                u8::try_from(channel).unwrap()
            };
            raw[offset + 3] = u8::try_from(block * 32 + channel).unwrap();
        }
    }
    raw[1052..1062].copy_from_slice(&[0, 128, 255, 3, 4, 5, 6, 7, 8, 9]);
    raw[1062] = mode;
    raw[1063..1065].copy_from_slice(&600_u16.to_le_bytes());
    raw[1065..1071].copy_from_slice(&[126, 11, 11, 3, 4, 5]);
    raw[1071..1075].copy_from_slice(&999_999_u32.to_le_bytes());
    raw[1075] = 0x42;
    raw[1076..1080].copy_from_slice(&sequence.to_le_bytes());
    raw
}
fn gps() -> Vec<u8> {
    let mut raw = vec![0; 512];
    raw[..2].copy_from_slice(&[0xff, 0xee]);
    raw[2..8].copy_from_slice(b"260111");
    raw[8..14].copy_from_slice(b"030405");
    raw[14..18].copy_from_slice(&[0, 128, 255, 1]);
    raw[18..102].fill(0x80);
    raw[18..31].copy_from_slice(b"$GPRMC,x*00\r\n");
    raw[102..506].fill(0xdf);
    raw[506] = 0xff;
    raw[507] = 1;
    raw[508..512].copy_from_slice(&[0, 128, 255, 2]);
    raw
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
        .filter(|row| row["value"]["kind"] == "hesai_udp_group")
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
        let mut selected = json!({"id":"lidar","protocol":"hesai_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),
            "profile":profile,"output":"raw.hesai","max_bytes":100_000_000,"packets_per_group":32,"timeout_ms":120,"every_ms":3_600_000});
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
    let raw = fs::read(home.path("raw.hesai")).unwrap();
    assert_eq!(&raw[..8], b"CFHESA1\0");
    assert_eq!(u32_at(&raw, 12), u32::try_from(original.len()).unwrap());
    assert_eq!(&raw[80..80 + original.len()], original);
    assert_eq!(&raw[40..72], Sha256::digest(original).as_slice());
    assert_eq!(
        u16::from_le_bytes(raw[22..24].try_into().unwrap()),
        u16::from(context.is_some())
    );
    let mut index = 0;
    let mut offset = 80 + original.len();
    for row in groups(home) {
        let value = &row["value"];
        assert_eq!(value["offset"], offset);
        assert!(serde_json::to_vec(&row).unwrap().len() < 1024 * 1024);
        let length = usize::try_from(value["length"].as_u64().unwrap()).unwrap();
        let group = &raw[offset..offset + length];
        assert_eq!(&group[..8], b"CFHGRP1\0");
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
        assert_eq!(&group[cursor..cursor + 8], b"CFHCMT1\0");
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
    assert_eq!(terminal["kind"], "hesai_udp_file");
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
fn all_six_returns_keep_native_arrays_reserved_context_clocks_and_sequence_observations() {
    let home = Home::new("hesai-modes");
    let source = datagram_socket();
    let context = b"Laser id,Elevation,Azimuth\n1,15.0,0.0\n2,14.0,0.0\n";
    let (capture, target) = Capture::start(
        &home,
        source.local_addr().unwrap(),
        PROFILE,
        Some(context),
        json!({"max_bytes":1e8,"packets_per_group":32.0}),
    );
    let sequences = [u32::MAX, 0, 0, 2, 1, 0x8000_0001];
    let packets = MODES
        .into_iter()
        .zip(sequences)
        .map(|(mode, sequence)| lidar(mode, sequence))
        .collect::<Vec<_>>();
    for raw in &packets {
        source.send_to(raw, target).unwrap();
    }
    assert_eq!(capture.finish().success()["status"], "complete");
    archive(&home, Some(context), &packets);
    let all = groups(&home);
    let native = all
        .iter()
        .flat_map(|row| row["value"]["datagrams"].as_array().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(native.len(), 6);
    for (index, value) in native.iter().enumerate() {
        assert_eq!(value["return_mode_raw"], MODES[index]);
        assert_eq!(value["sequence_raw"], sequences[index]);
        assert_eq!(value["motor_speed_rpm"], 600);
        assert_eq!(value["clock"]["calendar_unzoned"], "2026-11-11T03:04:05");
        assert_eq!(value["clock"]["microsecond_raw"], 999_999);
        assert_eq!(value["blocks"].as_array().unwrap().len(), 8);
        for block in 0..8 {
            let row = &value["blocks"][block];
            assert_eq!(row["distance_raw"][0], 0);
            assert_eq!(row["distance_raw"][1], 1);
            assert_eq!(row["distance_raw"][2], u16::MAX);
            assert_eq!(row["distance_mm"][2], 262_140);
            assert_eq!(row["reflectivity_raw"][2], 255);
            assert_eq!(row["channel_extra_raw"][31], block * 32 + 31);
            assert!(row.get("confidence").is_none());
        }
        assert_eq!(value["sequence_observation"]["loss_count_proven"], false);
    }
    for (value, relation) in native.iter().zip([
        "first_observed",
        "next_modulo",
        "equal",
        "forward_or_gap",
        "backward_or_large_forward",
        "backward_or_large_forward",
    ]) {
        assert_eq!(value["sequence_observation"]["relation"], relation);
    }
    assert_eq!(native[1]["sequence_observation"]["delta_modulo_u32"], 1);
    assert_eq!(
        native[4]["sequence_observation"]["delta_modulo_u32"],
        u32::MAX
    );
    assert_eq!(
        all[0]["value"]["sensor"]["context_status"],
        "configured_unverified"
    );
    exports(&home);
    source
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    assert!(matches!(
        source.recv_from(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
}
#[test]
fn application_clock_and_azimuth_states_do_not_drop_native_measurements_or_empty_context() {
    let home = Home::new("hesai-states");
    let source = datagram_socket();
    let (capture, target) = Capture::start(
        &home,
        source.local_addr().unwrap(),
        PROFILE,
        Some(b""),
        json!({"packets_per_group":1}),
    );
    let mut raw = lidar(0x33, 0);
    raw[12..14].copy_from_slice(&u16::MAX.to_le_bytes());
    raw[1065..1071].copy_from_slice(&[126, 2, 31, 3, 4, 5]);
    raw[1071..1075].copy_from_slice(&u32::MAX.to_le_bytes());
    source.send_to(&raw, target).unwrap();
    assert_eq!(capture.finish().success()["status"], "complete");
    archive(&home, Some(b""), &[raw]);
    let all = groups(&home);
    let value = &all[0]["value"]["datagrams"][0];
    assert_eq!(value["clock"]["calendar_status"], "invalid_value");
    assert_eq!(value["clock"]["microsecond_status"], "invalid_value");
    assert!(value["clock"].get("calendar_unzoned").is_none());
    assert_eq!(value["blocks"][0]["azimuth_range_status"], "unknown");
    assert_eq!(value["blocks"][0]["distance_mm"][2], 262_140);
    exports(&home);
}
#[test]
fn gps_companion_keeps_all_status_nmea_reserved_and_ascii_bytes_without_utc_inference() {
    let home = Home::new("hesai-gps");
    let source = datagram_socket();
    let (capture, target) = Capture::start(
        &home,
        source.local_addr().unwrap(),
        "pandar_xt32_gps",
        None,
        json!({"packets_per_group":2}),
    );
    let first = gps();
    let mut second = first.clone();
    second[102] = 0;
    second[507] = 0xff;
    for raw in [&first, &second] {
        source.send_to(raw, target).unwrap();
    }
    assert_eq!(capture.finish().success()["status"], "complete");
    archive(&home, None, &[first, second]);
    let all = groups(&home);
    let values = all[0]["value"]["datagrams"].as_array().unwrap();
    assert_eq!(values[0]["date_ascii_raw"], json!([50, 54, 48, 49, 49, 49]));
    assert_eq!(values[0]["time_ascii_raw"], json!([48, 51, 48, 52, 48, 53]));
    assert_eq!(values[0]["pps_lock_status"], "locked");
    assert_eq!(values[1]["pps_lock_status"], "unknown");
    assert_eq!(values[0]["reserved_pattern_status"], "expected_df");
    assert_eq!(values[1]["reserved_pattern_status"], "unexpected");
    for value in values {
        assert!(value.get("sequence_observation").is_none());
        assert!(value.get("source_time").is_none());
    }
    exports(&home);
}
#[test]
fn strict_profile_layout_failures_keep_prior_commits_and_never_reopen_the_epoch() {
    for mode in [
        "short", "long", "magic", "version", "channels", "blocks", "unit", "returns", "sequence",
        "factory", "mode", "pair", "gps",
    ] {
        let home = Home::new("hesai-retire");
        let source = datagram_socket();
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            PROFILE,
            None,
            json!({"packets_per_group":1}),
        );
        let first = lidar(0x39, 1);
        source.send_to(&first, target).unwrap();
        wait(|| groups(&home).len() == 1);
        let mut bad = lidar(0x39, 2);
        match mode {
            "short" => {
                bad.pop();
            }
            "long" => bad.push(0),
            "magic" => bad[0] = 0,
            "version" => bad[3] = 2,
            "channels" => bad[6] = 16,
            "blocks" => bad[7] = 4,
            "unit" => bad[9] = 2,
            "returns" => bad[10] = 1,
            "sequence" => bad[11] = 0x80,
            "factory" => bad[1075] = 0,
            "mode" => bad[1062] = 0x3a,
            "pair" => bad[142] = 1,
            "gps" => bad = gps(),
            _ => unreachable!(),
        }
        source.send_to(&bad, target).unwrap();
        let error = capture
            .finish()
            .failure(if matches!(mode, "version" | "mode") {
                5
            } else {
                1
            });
        let prefix = &error["details"]["hesai_udp"];
        assert_eq!(prefix["packets"], 1);
        assert_eq!(groups(&home).len(), 1);
        let raw = fs::read(home.path("raw.hesai")).unwrap();
        assert_eq!(prefix["committed_bytes"], raw.len());
        assert_eq!(prefix["sha256_committed_prefix"], digest(&raw));
        assert_eq!(prefix["jsonl_publication_not_proven"], true);
    }
}
#[test]
fn bounded_foreign_traffic_and_unpublished_malformed_group_preserve_exact_prefix() {
    for foreign in [false, true] {
        let home = Home::new("hesai-traffic");
        let source = datagram_socket();
        let outsider = datagram_socket();
        let (capture, target) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            PROFILE,
            None,
            json!({"packets_per_group":2}),
        );
        let packet = lidar(0x38, 1);
        source.send_to(&packet, target).unwrap();
        source.send_to(&packet, target).unwrap();
        wait(|| groups(&home).len() == 1);
        if foreign {
            // Exceed the same 64 KiB allowance without requiring a large
            // back-to-back UDP burst to survive the kernel's receive queue.
            let foreign_packet = vec![0; 24_000];
            for _ in 0..3 {
                outsider.send_to(&foreign_packet, target).unwrap();
                thread::sleep(Duration::from_millis(3));
            }
        } else {
            source.send_to(&packet, target).unwrap();
            source.send_to(&packet[..1079], target).unwrap();
        }
        let error = capture.finish().failure(1);
        let prefix = &error["details"]["hesai_udp"];
        assert_eq!(prefix["packets"], 2);
        assert_eq!(groups(&home).len(), 1);
        if foreign {
            assert_eq!(prefix["foreign_packets"], 3);
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("bounded allowance")
            );
        } else {
            assert_eq!(prefix["uncommitted_packets"], 1);
        }
    }
}
#[test]
fn storage_caps_no_overwrite_and_free_floor_preserve_only_owned_committed_ranges() {
    for mode in ["exists", "floor", "limit"] {
        let home = Home::new("hesai-storage");
        let source = datagram_socket();
        let packet = lidar(0x38, 1);
        let minimum = 80 + 32 + 32 + packet.len() + 56;
        if mode == "exists" {
            fs::write(home.path("raw.hesai"), b"preexisting bytes").unwrap();
        }
        let patch = match mode {
            "floor" => json!({"min_free_bytes":u64::MAX,"packets_per_group":1}),
            "limit" => json!({"max_bytes":minimum,"packets_per_group":2}),
            _ => json!({"packets_per_group":1}),
        };
        let (capture, target) =
            Capture::start(&home, source.local_addr().unwrap(), PROFILE, None, patch);
        source.send_to(&packet, target).unwrap();
        if mode == "limit" {
            source.send_to(&packet, target).unwrap();
        }
        let error = capture
            .finish()
            .failure(if mode == "exists" { 1 } else { 6 });
        if mode == "exists" {
            assert_eq!(error["code"], "exists");
            assert_eq!(
                fs::read(home.path("raw.hesai")).unwrap(),
                b"preexisting bytes"
            );
        } else if mode == "floor" {
            assert_eq!(error["code"], "storage_full");
            assert!(!home.path("raw.hesai").exists());
        } else {
            assert_eq!(error["code"], "storage_full");
            assert_eq!(error["details"]["hesai_udp"]["packets"], 1);
            assert_eq!(error["details"]["hesai_udp"]["uncommitted_packets"], 1);
            assert_eq!(
                fs::metadata(home.path("raw.hesai")).unwrap().len(),
                u64::try_from(minimum).unwrap()
            );
        }
    }
}
#[test]
fn group_limits_and_finish_keep_each_received_packet_once_with_its_original_clock() {
    let home = Home::new("hesai-grouping");
    let source = datagram_socket();
    let (capture, target) = Capture::start(
        &home,
        source.local_addr().unwrap(),
        PROFILE,
        None,
        json!({"packets_per_group":2}),
    );
    let packets = (0..5)
        .map(|sequence| lidar(0x3b, sequence))
        .collect::<Vec<_>>();
    for raw in &packets {
        source.send_to(raw, target).unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(capture.finish().success()["status"], "complete");
    archive(&home, None, &packets);
    assert!(
        groups(&home)
            .iter()
            .all(|row| row["value"]["packet_count"].as_u64().unwrap() <= 2)
    );
}
#[test]
fn configuration_bounds_and_opaque_context_cap_fail_before_artifacts() {
    for (key, value) in [
        ("listen", json!("0.0.0.0:0")),
        ("source_ip", json!("239.1.2.3")),
        ("source_port", json!(0)),
        ("packets_per_group", json!(33)),
        ("packets_per_group", json!(1.5)),
        ("max_bytes", json!(1)),
        ("output", json!("{seq}.hesai")),
        (
            "multicast",
            json!({"group":"239.1.2.3","interface":"0.0.0.0"}),
        ),
        (
            "multicast",
            json!({"group":"239.1.2.3","interface":"127.0.0.1"}),
        ),
    ] {
        let home = Home::new("hesai-config");
        let mut source = json!({"id":"lidar","protocol":"hesai_udp","listen":"127.0.0.1:7502","source_ip":"127.0.0.1","profile":PROFILE,"output":"raw.hesai","max_bytes":1_000_000});
        source[key] = value;
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[source]}).to_string(),
        )
        .unwrap();
        assert_eq!(home.run(&ARGS).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("raw.hesai").exists());
    }
    let home = Home::new("hesai-context-cap");
    fs::write(home.path("too-large"), vec![0; 1024 * 1024 + 1]).unwrap();
    fs::write(home.path("sources.json"),json!({"version":1,"sources":[{"id":"lidar","protocol":"hesai_udp","listen":"127.0.0.1:7502","source_ip":"127.0.0.1","profile":PROFILE,"context":"too-large","output":"raw.hesai","max_bytes":2_000_000}]}).to_string()).unwrap();
    assert_eq!(home.run(&ARGS).failure(2)["code"], "usage");
    assert!(!home.path("run.jsonl").exists());
    assert!(!home.path("raw.hesai").exists());
}
