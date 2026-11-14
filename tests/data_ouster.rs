//! Spec-derived UDP fixtures through the CLI; independent SDK provenance is separate.
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
const SERIAL: u64 = 123_456_789_012;
const INIT: u32 = 0x12_3456;
const ARGS: [&str; 9] = [
    "--json",
    "data",
    "record",
    "--config",
    "sources.json",
    "--for",
    "500ms",
    "-o",
    "run.jsonl",
];
fn crc(raw: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for byte in raw {
        crc ^= u64::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xc96c_5795_d787_0f42
            };
        }
    }
    !crc
}
fn seal(raw: &mut [u8]) {
    let end = raw.len() - 8;
    let value = crc(&raw[..end]);
    raw[end..].copy_from_slice(&value.to_le_bytes());
}
// macOS defaults UDP sends to 9216 bytes; these literal packets include H128.
// A per-socket buffer exercises the real datagrams without changing host sysctls.
fn datagram_socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&socket)
        .set_send_buffer_size(128 * 1024)
        .unwrap();
    socket
}
fn header(raw: &mut [u8], kind: u16, frame: u16) {
    assert!(raw.len() >= 12);
    raw[..2].copy_from_slice(&kind.to_le_bytes());
    raw[2..4].copy_from_slice(&frame.to_le_bytes());
    raw[4..12].copy_from_slice(&((SERIAL << 24) | u64::from(INIT)).to_le_bytes());
}
fn metadata(profile: &str, stream: &str, height: usize) -> Value {
    json!({"sensor_info":{"prod_line":"OS-1-32","prod_sn":SERIAL.to_string(),"build_rev":"v3.2.0","initialization_id":INIT},
        "lidar_data_format":{"header_type":"STANDARD","udp_profile_lidar":if stream=="imu" {"RNG19_RFL8_SIG16_NIR16_ZONE16"}else{profile},"udp_profile_imu":if profile=="ACCEL32_GYRO32_NMEA" {profile}else{"LEGACY"},
        "pixels_per_column":height,"columns_per_packet":16,"columns_per_frame":512,"column_window":[0,511]},
        "imu_data_format":{"udp_profile_imu":if profile=="ACCEL32_GYRO32_NMEA" {profile}else{"LEGACY"},"imu_measurements_per_packet":8,"accel_fsr":"EXTENDED","gyro_fsr":"EXTENDED"},
        "config_params":{"timestamp_mode":"TIME_FROM_INTERNAL_OSC"},"uninterpreted_future":{"keep":[0,128,255]}})
}
fn lidar(profile: &str, height: usize, frame: u16) -> Vec<u8> {
    let legacy = profile == "LEGACY";
    let stride = match profile {
        "RNG19_RFL8_SIG16_NIR16_DUAL" => 16,
        "RNG15_RFL8_NIR8" => 4,
        "RNG15_RFL8_NIR8_DUAL" => 8,
        _ => 12,
    };
    let column = (if legacy { 20 } else { 12 }) + height * stride;
    let mut raw = vec![0; 16 * column + if legacy { 0 } else { 64 }];
    if !legacy {
        header(&mut raw, 1, frame);
    }
    for index in 0..16 {
        let start = if legacy { 0 } else { 32 } + index * column;
        raw[start..start + 8]
            .copy_from_slice(&(u64::MAX - u64::try_from(index).unwrap()).to_le_bytes());
        raw[start + 8..start + 10].copy_from_slice(&u16::try_from(index).unwrap().to_le_bytes());
        if legacy {
            raw[start + 10..start + 12].copy_from_slice(&frame.to_le_bytes());
            raw[start + 12..start + 16]
                .copy_from_slice(&(u32::try_from(index).unwrap() * 176).to_le_bytes());
            let status = if index == 3 {
                0x1234_5678_u32
            } else {
                u32::MAX
            };
            raw[start + column - 4..start + column].copy_from_slice(&status.to_le_bytes());
        } else {
            raw[start + 10..start + 12].copy_from_slice(&1_u16.to_le_bytes());
        }
        let offset = start + if legacy { 16 } else { 12 };
        raw[offset..offset + height * stride].fill(255);
    }
    if !legacy {
        seal(&mut raw);
    }
    raw
}
fn imu(synchronous: bool) -> Vec<u8> {
    let words = [
        0x8000_0000_u32,
        0x7fc0_1234,
        0x7f80_0000,
        0xff80_0000,
        1.25_f32.to_bits(),
        (-2.5_f32).to_bits(),
    ];
    if !synchronous {
        let mut raw = Vec::new();
        for stamp in [u64::MAX, u64::MAX - 1, u64::MAX - 2] {
            raw.extend_from_slice(&stamp.to_le_bytes());
        }
        for word in words {
            raw.extend_from_slice(&word.to_le_bytes());
        }
        return raw;
    }
    let mut raw = vec![0; 164 + 8 * 36];
    header(&mut raw, 2, u16::MAX);
    raw[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    raw[40..48].copy_from_slice(b"$GPRMC,x");
    raw[128..132].copy_from_slice(&3_u32.to_le_bytes());
    for index in 0..8 {
        let offset = 132 + index * 36;
        raw[offset..offset + 8]
            .copy_from_slice(&(u64::MAX - u64::try_from(index).unwrap()).to_le_bytes());
        raw[offset + 8..offset + 12].copy_from_slice(&0xffff_ffff_u32.to_le_bytes());
        for (axis, word) in words.iter().enumerate() {
            raw[offset + 12 + axis * 4..offset + 16 + axis * 4]
                .copy_from_slice(&word.to_le_bytes());
        }
    }
    seal(&mut raw);
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
        .filter(|row| row["value"]["kind"] == "ouster_udp_group")
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
        meta: &Value,
        stream: &str,
        patch: Value,
    ) -> (Self, SocketAddr, Vec<u8>) {
        let reservation = datagram_socket();
        let target = reservation.local_addr().unwrap();
        let original = serde_json::to_vec_pretty(meta).unwrap();
        fs::write(home.path("sensor.json"), &original).unwrap();
        let mut selected = json!({"id":"lidar","protocol":"ouster_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),"stream":stream,"metadata":"sensor.json","output":"raw.ouster","max_bytes":100_000_000,"packets_per_group":2,"timeout_ms":120,"every_ms":3_600_000});
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
        (Self(Some(child)), target, original)
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
fn archive(home: &Home, metadata: &[u8], packets: &[Vec<u8>]) {
    let raw = fs::read(home.path("raw.ouster")).unwrap();
    assert_eq!(&raw[..8], b"CFOUST1\0");
    assert_eq!(u32_at(&raw, 12), u32::try_from(metadata.len()).unwrap());
    assert_eq!(&raw[80..80 + metadata.len()], metadata);
    assert_eq!(&raw[40..72], Sha256::digest(metadata).as_slice());
    let mut index = 0;
    let mut offset = 80 + metadata.len();
    for row in groups(home) {
        let value = &row["value"];
        assert_eq!(value["offset"], offset);
        assert!(serde_json::to_vec(&row).unwrap().len() < 1024 * 1024);
        let length = usize::try_from(value["length"].as_u64().unwrap()).unwrap();
        let group = &raw[offset..offset + length];
        assert_eq!(&group[..8], b"CFOGRP1\0");
        assert_eq!(value["sha256"], digest(group));
        assert_eq!(value["metadata"]["sha256"], digest(metadata));
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
        assert_eq!(&group[cursor..cursor + 8], b"CFOCMT1\0");
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
    assert_eq!(terminal["kind"], "ouster_udp_file");
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
fn modern_native_profiles_preserve_all_returns_and_raw_metadata_through_exports() {
    for profile in [
        "RNG19_RFL8_SIG16_NIR16",
        "RNG19_RFL8_SIG16_NIR16_DUAL",
        "RNG15_RFL8_NIR8",
        "RNG15_RFL8_NIR8_DUAL",
    ] {
        let home = Home::new("ouster-modern");
        let source = datagram_socket();
        let meta = metadata(profile, "lidar", 32);
        let (capture, target, original) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            &meta,
            "lidar",
            json!({"max_bytes":1e8,"packets_per_group":2.0}),
        );
        source.send_to(&imu(false), target).unwrap();
        let packets = vec![lidar(profile, 32, u16::MAX), lidar(profile, 32, 0)];
        for packet in &packets {
            source.send_to(packet, target).unwrap();
        }
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, &original, &packets);
        let native = groups(&home);
        let column = &native[0]["value"]["datagrams"][0]["columns"][0];
        let maximum = if profile.starts_with("RNG15") {
            262_136
        } else {
            524_287
        };
        assert_eq!(column["range_mm"][0], maximum);
        assert_eq!(column["reflectivity_raw"][0], 255);
        if profile.ends_with("DUAL") {
            assert_eq!(column["range2_mm"][0], maximum);
        }
        let rows = samples(&home);
        assert_eq!(rows.last().unwrap()["value"]["filtered_other_packets"], 1);
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
fn legacy_lidar_and_imu_preserve_unknown_states_ieee_words_and_exact_archives() {
    for stream in ["lidar", "imu"] {
        let home = Home::new("ouster-legacy");
        let source = datagram_socket();
        let mut meta = metadata("LEGACY", stream, 16);
        meta["lidar_data_format"]["udp_profile_lidar"] = json!("LEGACY");
        let (capture, target, original) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            &meta,
            stream,
            json!({"packets_per_group":1}),
        );
        let packet = if stream == "lidar" {
            lidar("LEGACY", 16, 42)
        } else {
            imu(false)
        };
        source.send_to(&packet, target).unwrap();
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, &original, &[packet]);
        let native = groups(&home);
        let value = &native[0]["value"]["datagrams"][0];
        if stream == "lidar" {
            assert_eq!(value["columns"][3]["validity"], "unknown");
            assert!(value["columns"][3].get("valid").is_none());
            assert_eq!(value["columns"][0]["range_mm"][0], 1_048_575);
        } else {
            assert_eq!(value["acceleration_g"][0]["ieee754_raw"], 0x8000_0000_u32);
            assert_eq!(value["acceleration_g"][1]["status"], "nan");
            assert!(value["acceleration_g"][1].get("value").is_none());
            assert_eq!(
                value["gyro_timestamp_ns_raw"].as_u64().unwrap(),
                u64::MAX - 2
            );
        }
        exports(&home);
    }
}
#[test]
fn synchronous_imu_uses_wire_units_and_ignores_unsupported_lidar_body_by_header_proof() {
    for bad_zone in [false, true] {
        let home = Home::new("ouster-sync");
        let source = datagram_socket();
        let mut meta = metadata("ACCEL32_GYRO32_NMEA", "imu", 128);
        meta["config_params"]["udp_port_zm"] = json!(7514);
        meta["zone_set"] = json!("fixture-declared-zone-set");
        let (capture, target, original) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            &meta,
            "imu",
            json!({"packets_per_group":1}),
        );
        let mut zone = vec![0; 80];
        header(&mut zone, 3, 42);
        seal(&mut zone);
        if bad_zone {
            zone[40] ^= 1;
        }
        source.send_to(&zone, target).unwrap();
        if bad_zone {
            assert!(
                capture.finish().failure(1)["message"]
                    .as_str()
                    .unwrap()
                    .contains("CRC64")
            );
            assert!(!home.path("raw.ouster").exists());
            continue;
        }
        let mut unselected = vec![0; 80];
        header(&mut unselected, 1, 42);
        seal(&mut unselected);
        source.send_to(&unselected, target).unwrap();
        let packet = imu(true);
        source.send_to(&packet, target).unwrap();
        assert_eq!(capture.finish().success()["status"], "complete");
        archive(&home, &original, &[packet]);
        let native = groups(&home);
        let value = &native[0]["value"]["datagrams"][0];
        assert_eq!(value["units_status"], "unresolved_vendor_documentation");
        assert_eq!(value["measurements"][0]["correlation_word_raw"], u32::MAX);
        assert_eq!(
            value["measurements"][0]["acceleration_wire"][0]["ieee754_raw"],
            0x8000_0000_u32
        );
        assert_eq!(value["nmea"]["sentence_bytes"], 88);
        assert!(value.get("acceleration_g").is_none());
        assert_eq!(
            samples(&home).last().unwrap()["value"]["filtered_other_packets"],
            2
        );
        exports(&home);
    }
}
#[test]
fn strict_crc_and_identity_failures_keep_earlier_committed_ranges_and_retire() {
    for mode in ["crc", "serial", "init", "size"] {
        let home = Home::new("ouster-retire");
        let source = datagram_socket();
        let profile = "RNG15_RFL8_NIR8";
        let meta = metadata(profile, "lidar", 32);
        let (capture, target, _) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            &meta,
            "lidar",
            json!({"packets_per_group":1}),
        );
        let first = lidar(profile, 32, 1);
        source.send_to(&first, target).unwrap();
        wait(|| groups(&home).len() == 1);
        let mut bad = lidar(profile, 32, 2);
        match mode {
            "crc" => bad[100] ^= 1,
            "serial" => {
                bad[7] ^= 1;
                seal(&mut bad);
            }
            "init" => {
                bad[4] ^= 1;
                seal(&mut bad);
            }
            "size" => {
                bad.pop();
            }
            _ => unreachable!(),
        }
        source.send_to(&bad, target).unwrap();
        let error = capture.finish().failure(if mode == "init" { 3 } else { 1 });
        let detail = &error["details"]["ouster_udp"];
        assert_eq!(detail["packets"], 1);
        assert!(detail["committed_bytes"].as_u64().unwrap() > 0);
        assert_eq!(groups(&home).len(), 1);
        let raw = fs::read(home.path("raw.ouster")).unwrap();
        assert_eq!(detail["sha256_committed_prefix"], digest(&raw));
    }
}
#[test]
fn documented_old_crc_policy_is_explicit_and_extra_version_components_cannot_weaken_it() {
    let profile = "RNG19_RFL8_SIG16_NIR16_DUAL";
    for build in ["v2.2.0-rc.2-17-gbc0879e", "v2.4.0.99"] {
        let home = Home::new("ouster-old-crc");
        let source = datagram_socket();
        let mut meta = metadata(profile, "lidar", 32);
        meta["sensor_info"]["build_rev"] = json!(build);
        meta["lidar_data_format"]
            .as_object_mut()
            .unwrap()
            .remove("header_type");
        let (capture, target, original) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            &meta,
            "lidar",
            json!({"header_type":"STANDARD","packets_per_group":1}),
        );
        let mut packet = lidar(profile, 32, 1);
        let end = packet.len();
        packet[end - 32..].fill(0);
        source.send_to(&packet, target).unwrap();
        if build.starts_with("v2.2") {
            assert_eq!(capture.finish().success()["status"], "complete");
            archive(&home, &original, &[packet]);
            let values = groups(&home);
            assert_eq!(
                values[0]["value"]["datagrams"][0]["checksum_status"],
                "unavailable_version_qualified"
            );
        } else {
            assert!(
                capture.finish().failure(1)["message"]
                    .as_str()
                    .unwrap()
                    .contains("CRC64")
            );
            assert!(!home.path("raw.ouster").exists());
        }
    }
}
#[test]
fn groups_bound_native_json_and_preserve_accepted_boundary_packets() {
    let home = Home::new("ouster-group-bounds");
    let source = datagram_socket();
    let profile = "RNG19_RFL8_SIG16_NIR16_DUAL";
    let (capture, target, original) = Capture::start(
        &home,
        source.local_addr().unwrap(),
        &metadata(profile, "lidar", 128),
        "lidar",
        json!({"packets_per_group":32}),
    );
    let packets = (0..9)
        .map(|frame| lidar(profile, 128, frame))
        .collect::<Vec<_>>();
    for packet in &packets {
        source.send_to(packet, target).unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(capture.finish().success()["status"], "complete");
    archive(&home, &original, &packets);
    assert!(groups(&home).len() >= 2);
    for group in groups(&home) {
        assert!(group["value"]["packet_count"].as_u64().unwrap() <= 8);
    }
}
#[test]
fn archive_caps_existing_output_and_free_floor_never_replace_owned_or_existing_bytes() {
    let profile = "RNG15_RFL8_NIR8";
    for mode in ["exists", "floor", "limit"] {
        let home = Home::new("ouster-storage");
        let source = datagram_socket();
        let meta = metadata(profile, "lidar", 32);
        let packet = lidar(profile, 32, 1);
        let minimum =
            80 + serde_json::to_vec_pretty(&meta).unwrap().len() + 32 + 32 + packet.len() + 56;
        if mode == "exists" {
            fs::write(home.path("raw.ouster"), b"preexisting bytes").unwrap();
        }
        let patch = match mode {
            "floor" => json!({"min_free_bytes":u64::MAX,"packets_per_group":1}),
            "limit" => json!({"max_bytes":minimum,"packets_per_group":2}),
            _ => json!({"packets_per_group":1}),
        };
        let (capture, target, _) =
            Capture::start(&home, source.local_addr().unwrap(), &meta, "lidar", patch);
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
                fs::read(home.path("raw.ouster")).unwrap(),
                b"preexisting bytes"
            );
        } else if mode == "floor" {
            assert_eq!(error["code"], "storage_full");
            assert!(!home.path("raw.ouster").exists());
        } else {
            assert_eq!(error["code"], "storage_full");
            assert_eq!(error["details"]["ouster_udp"]["packets"], 1);
            assert_eq!(error["details"]["ouster_udp"]["uncommitted_packets"], 1);
            assert_eq!(
                fs::metadata(home.path("raw.ouster")).unwrap().len(),
                u64::try_from(minimum).unwrap()
            );
        }
    }
}
#[test]
fn bounded_foreign_traffic_and_malformed_group_preserve_prior_archive_counts() {
    for foreign in [false, true] {
        let home = Home::new("ouster-traffic");
        let source = datagram_socket();
        let outsider = datagram_socket();
        let profile = "RNG15_RFL8_NIR8";
        let (capture, target, _) = Capture::start(
            &home,
            source.local_addr().unwrap(),
            &metadata(profile, "lidar", 32),
            "lidar",
            json!({"packets_per_group":2}),
        );
        let packet = lidar(profile, 32, 1);
        for _ in 0..2 {
            source.send_to(&packet, target).unwrap();
        }
        wait(|| groups(&home).len() == 1);
        if foreign {
            let foreign_packet = vec![0; 40_000];
            for _ in 0..2 {
                outsider.send_to(&foreign_packet, target).unwrap();
            }
        } else {
            source.send_to(&packet, target).unwrap();
            source.send_to(&packet[..packet.len() - 1], target).unwrap();
        }
        let error = capture.finish().failure(1);
        assert_eq!(error["details"]["ouster_udp"]["packets"], 2);
        if foreign {
            assert_eq!(error["code"], "incomplete");
            assert_eq!(error["details"]["ouster_udp"]["foreign_packets"], 2);
        } else {
            assert_eq!(error["details"]["ouster_udp"]["uncommitted_packets"], 1);
        }
        assert_eq!(groups(&home).len(), 1);
    }
}
#[test]
fn configuration_and_metadata_bounds_fail_before_listening_or_artifacts() {
    for (key, value) in [
        ("listen", json!("0.0.0.0:0")),
        ("source_ip", json!("239.1.2.3")),
        ("source_port", json!(0)),
        ("packets_per_group", json!(33)),
        ("packets_per_group", json!(1.5)),
        ("max_bytes", json!(1)),
        ("output", json!("{seq}.ouster")),
        (
            "multicast",
            json!({"group":"239.1.2.3","interface":"0.0.0.0"}),
        ),
        (
            "multicast",
            json!({"group":"239.1.2.3","interface":"127.0.0.1"}),
        ),
        ("profile", json!("LEGACY")),
    ] {
        let home = Home::new("ouster-config");
        let meta = metadata("RNG15_RFL8_NIR8", "lidar", 32);
        fs::write(home.path("sensor.json"), meta.to_string()).unwrap();
        let mut source = json!({"id":"lidar","protocol":"ouster_udp","listen":"127.0.0.1:7502","source_ip":"127.0.0.1","stream":"lidar","metadata":"sensor.json","output":"raw.ouster","max_bytes":1_000_000});
        source[key] = value;
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[source]}).to_string(),
        )
        .unwrap();
        assert_eq!(home.run(&ARGS).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("raw.ouster").exists());
    }
}
