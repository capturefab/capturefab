//! Passive VLP-16 UDP and committed-range retention through the production CLI.
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

const RECORD_BYTES: usize = 1238;
const FILE_HEADER: usize = 40;
const GROUP_HEADER: usize = 32;
const FOOTER_BYTES: usize = 56;
const RECORDING_ARGS: [&str; 9] = [
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

// Synthetic positive packets follow Rev F §9.3.2 field-by-field. They are
// independent of the historical captured product-21 negative fixture below.
fn packet(mode: u8, timestamp: u32) -> Vec<u8> {
    packet_product(mode, timestamp, 0x22)
}
fn packet_product(mode: u8, timestamp: u32, product: u8) -> Vec<u8> {
    let mut packet = Vec::with_capacity(1206);
    for block in 0..12u16 {
        packet.extend_from_slice(&[0xff, 0xee]);
        let azimuth = if mode == 0x39 { block / 2 } else { block };
        packet.extend_from_slice(&(azimuth * 2500).to_le_bytes());
        for channel in 0..32u16 {
            packet.extend_from_slice(&(u16::MAX - block * 32 - channel).to_le_bytes());
            packet.push(u8::try_from(channel * 8).unwrap());
        }
    }
    packet.extend_from_slice(&timestamp.to_le_bytes());
    packet.extend_from_slice(&[mode, product]);
    packet
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
fn group_rows(home: &Home) -> Vec<Value> {
    group_rows_profile(home, "velodyne_vlp16")
}
fn group_rows_profile(home: &Home, protocol: &str) -> Vec<Value> {
    samples(home)
        .into_iter()
        .filter(|row| row["value"]["kind"] == format!("{protocol}_group"))
        .collect()
}
fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "capture did not reach the expected state"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

struct Capture(Option<Child>);
impl Capture {
    fn start(home: &Home, source: SocketAddr, capacity: u16, limit: &str) -> (Self, SocketAddr) {
        Self::start_profile(home, source, capacity, limit, "velodyne_vlp16")
    }
    fn start_profile(
        home: &Home,
        source: SocketAddr,
        capacity: u16,
        limit: &str,
        protocol: &str,
    ) -> (Self, SocketAddr) {
        let reservation = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap();
        let mut config = json!({"version":1,"sources":[{
            "id":"lidar","protocol":protocol,
            "listen":format!("0.0.0.0:{}",address.port()),"source":source,
            "output":packet_file(protocol),"packets_per_group":capacity,
            "timeout_ms":120,"every_ms":3_600_000
        }]});
        config["sources"][0]["max_bytes"] = serde_json::from_str(limit).unwrap();
        fs::write(home.path("sources.json"), config.to_string()).unwrap();
        drop(reservation);
        let child = home
            .command()
            .args(RECORDING_ARGS)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let capture = Self(Some(child));
        // Adapters bind before the recorder writes its header. There is no
        // application handshake or speculative packet sent to the sensor.
        wait_until(|| {
            rows(home)
                .first()
                .is_some_and(|row| row["kind"] == "header")
        });
        (capture, address)
    }
    fn finish(mut self) -> Run {
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        Run::of_output(output, &RECORDING_ARGS)
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
fn packet_file(protocol: &str) -> &'static str {
    match protocol {
        "velodyne_vlp16" => "packets.vlp16",
        "velodyne_hdl32e" => "packets.hdl32e",
        _ => panic!("unknown test profile"),
    }
}
fn check_archive(home: &Home, expected: &[Vec<u8>]) {
    check_archive_profile(home, expected, "velodyne_vlp16");
}
fn check_archive_profile(home: &Home, expected: &[Vec<u8>], protocol: &str) {
    let raw = fs::read(home.path(packet_file(protocol))).unwrap();
    assert!(raw.len() >= FILE_HEADER);
    let magic = match protocol {
        "velodyne_vlp16" => b"CFVLP16\0",
        "velodyne_hdl32e" => b"CFHDL32E",
        _ => panic!("unknown test profile"),
    };
    assert_eq!(&raw[..8], magic);
    assert_eq!(u32_at(&raw, 12), 1206);
    let mut offset = FILE_HEADER;
    let mut index = 0usize;
    let mut last_received = 0u64;
    let mut acquisition_origin = None;
    let mut last_elapsed = 0u64;
    for (number, row) in group_rows_profile(home, protocol).iter().enumerate() {
        let value = &row["value"];
        assert_eq!(value["group"], number + 1);
        if protocol == "velodyne_hdl32e" {
            assert_eq!(value["model_profile"], "hdl32e");
            assert_eq!(value["product_id_raw"], 0x21);
        } else {
            assert!(value.get("model_profile").is_none());
            assert_eq!(value["product_id_raw"], 0x22);
        }
        assert_eq!(
            value["path"],
            home.real(packet_file(protocol)).to_str().unwrap()
        );
        assert_eq!(value["offset"], offset);
        let length = usize::try_from(value["length"].as_u64().unwrap()).unwrap();
        let group = &raw[offset..offset + length];
        assert!(group.len() >= GROUP_HEADER + FOOTER_BYTES);
        assert_eq!(&group[..8], b"CFVGRP1\0");
        assert_eq!(u64_at(group, 8), u64::try_from(number + 1).unwrap());
        let count = usize::try_from(u32_at(group, 16)).unwrap();
        assert_eq!(value["packets"], count);
        assert_eq!(length, GROUP_HEADER + count * RECORD_BYTES + FOOTER_BYTES);
        assert_eq!(
            u32_at(group, 20),
            u32::try_from(count * RECORD_BYTES).unwrap()
        );
        assert_eq!(u64_at(group, 24), u64::try_from(index + 1).unwrap());
        assert_eq!(value["sha256"], hex(&Sha256::digest(group)));
        for record in group[GROUP_HEADER..length - FOOTER_BYTES]
            .as_chunks::<RECORD_BYTES>()
            .0
        {
            assert_eq!(&record[..4], b"PKT1");
            assert_eq!(u32_at(record, 4), 1206);
            assert_eq!(u64_at(record, 8), u64::try_from(index + 1).unwrap());
            let received = u64_at(record, 16);
            assert!(received >= last_received);
            assert!(received >= u64_at(&raw, 24));
            let payload = &record[32..];
            assert_eq!(payload, &expected[index]);
            assert_eq!(u32_at(record, 24), u32_at(payload, 1200));
            assert_eq!(&record[28..30], &payload[1204..1206]);
            assert_eq!(&record[30..32], &[0, 0]);
            last_received = received;
            index += 1;
        }
        let footer = &group[length - FOOTER_BYTES..];
        assert_eq!(&footer[..8], b"CFVCMT1\0");
        assert_eq!(u64_at(footer, 8), u64::try_from(number + 1).unwrap());
        assert_eq!(
            u64_at(footer, 16),
            u64::try_from(length - FOOTER_BYTES).unwrap()
        );
        let digest: [u8; 32] = Sha256::digest(&group[..length - FOOTER_BYTES]).into();
        assert_eq!(&footer[24..], digest.as_slice());
        assert_eq!(value["last_received_monotonic_ns"], last_received);
        let elapsed = row["elapsed_ns"].as_u64().unwrap();
        let origin = last_received.checked_sub(elapsed).unwrap();
        assert!(origin >= u64_at(&raw, 24));
        if let Some(previous) = acquisition_origin {
            assert_eq!(origin, previous);
        }
        acquisition_origin = Some(origin);
        last_elapsed = elapsed;
        assert!(row.get("source_time").is_none());
        offset += length;
    }
    assert_eq!(index, expected.len());
    assert_eq!(offset, raw.len());
    let rows = samples(home);
    let terminal = &rows.last().unwrap()["value"];
    assert_eq!(rows.last().unwrap()["elapsed_ns"], last_elapsed);
    assert_eq!(terminal["kind"], format!("{protocol}_file"));
    if protocol == "velodyne_hdl32e" {
        assert_eq!(terminal["model_profile"], "hdl32e");
        assert_eq!(terminal["product_id_raw"], 0x21);
    } else {
        assert!(terminal.get("model_profile").is_none());
        assert!(terminal.get("product_id_raw").is_none());
    }
    assert_eq!(terminal["committed_bytes"], raw.len());
    assert_eq!(
        terminal["sha256_committed_prefix"],
        hex(&Sha256::digest(&raw))
    );
    assert_eq!(terminal["packets"], expected.len());
    assert_eq!(terminal["unpublished_tail_bytes"], 0);
    assert_eq!(terminal["storage_finalized"], true);
}
fn hex(raw: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(raw.len() * 2);
    for byte in raw {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}

#[test]
fn mixed_returns_duplicates_hour_wrap_and_original_receive_clocks_survive_exports() {
    let home = Home::new("vlp16-raw-groups");
    let source = UdpSocket::bind("127.0.0.1:0").unwrap();
    let foreign = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (capture, address) = Capture::start(&home, source.local_addr().unwrap(), 2, "1e6");
    // Foreign malformed input must not be parsed as this source's data.
    foreign.send_to(&[0x12; 1206], address).unwrap();
    let packets = vec![
        packet(0x37, 3_599_999_999),
        packet(0x39, 0),
        packet(0x38, 7),
        packet(0x37, 3_599_999_999),
    ];
    for packet in &packets {
        source.send_to(packet, address).unwrap();
    }
    let result = capture.finish().success();
    assert_eq!(result["status"], "complete");
    assert_eq!(group_rows(&home).len(), 2);
    check_archive(&home, &packets);
    // Capturing never sends a query, trigger, clock/config write or ACK.
    source
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let error = source.recv_from(&mut [0; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    check_exports(&home, false);
}

fn check_exports(home: &Home, allow_incomplete: bool) {
    for (format, name) in [("jsonl", "samples.jsonl"), ("csv", "samples.csv")] {
        let mut arguments = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            name,
        ];
        if allow_incomplete {
            arguments.push("--allow-incomplete");
        }
        home.ok(&arguments);
    }
    let exported: Vec<Value> = fs::read_to_string(home.path("samples.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let original = samples(home);
    assert_eq!(exported.len(), original.len());
    for (actual, mut expected) in exported.into_iter().zip(original) {
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(actual, expected);
    }
    let csv = fs::read_to_string(home.path("samples.csv")).unwrap();
    for sample in samples(home) {
        assert!(csv.contains(&format!(
            "\"{}\"",
            sample["value"].to_string().replace('"', "\"\"")
        )));
    }
}

#[test]
fn byte_ceiling_adjusts_group_size_and_keeps_its_committed_reference() {
    let home = Home::new("vlp16-byte-ceiling");
    let source = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (capture, address) = Capture::start(&home, source.local_addr().unwrap(), 128, "1366.0");
    let packet = packet(0x38, 42);
    source.send_to(&packet, address).unwrap();
    let error = capture.finish().failure(1);
    assert!(
        error["message"].as_str().unwrap().contains("max_bytes"),
        "{error}"
    );
    let prefix = &error["details"]["velodyne_vlp16"];
    assert_eq!(prefix["committed_bytes"], 1366);
    assert_eq!(prefix["packets"], 1);
    assert_eq!(group_rows(&home).len(), 1);
    check_archive(&home, &[packet]);
    assert_eq!(rows(&home).last().unwrap()["status"], "failed");
}

#[test]
fn historical_product21_is_rejected_unchanged_before_any_archive_is_created() {
    let home = Home::new("vlp16-historical-negative");
    let source = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (capture, address) = Capture::start(&home, source.local_addr().unwrap(), 128, "1e6");
    source.send_to(&historical_product21(), address).unwrap();
    let error = capture.finish().failure(5);
    assert_eq!(error["code"], "unsupported");
    assert_eq!(error["details"]["product_id_raw"], 0x21);
    assert_eq!(error["details"]["velodyne_vlp16"]["committed_bytes"], 0);
    assert_eq!(samples(&home), Vec::<Value>::new());
    assert!(!home.path("packets.vlp16").exists());
}

#[test]
fn idle_group_flushes_before_a_later_read_and_owned_file_never_replaces_another() {
    let home = Home::new("vlp16-idle-flush");
    let source = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (capture, address) = Capture::start(&home, source.local_addr().unwrap(), 128, "1e6");
    let packet = packet(0x37, 1);
    source.send_to(&packet, address).unwrap();
    wait_until(|| group_rows(&home).len() == 1);
    assert!(fs::metadata(home.path("packets.vlp16")).unwrap().len() > 0);
    capture.finish().success();
    check_archive(&home, std::slice::from_ref(&packet));
    let original = fs::read(home.path("packets.vlp16")).unwrap();
    fs::remove_file(home.path("run.jsonl")).unwrap();
    let (capture, address) = Capture::start(&home, source.local_addr().unwrap(), 1, "1e6");
    source.send_to(&packet, address).unwrap();
    let error = capture.finish().failure(1);
    assert_eq!(error["code"], "exists");
    assert_eq!(fs::read(home.path("packets.vlp16")).unwrap(), original);
    assert_eq!(samples(&home), Vec::<Value>::new());
}

#[test]
fn malformed_later_datagram_discards_its_uncommitted_siblings_and_keeps_earlier_ranges() {
    let mut cases = Vec::new();
    let base = packet(0x37, 0);
    assert_eq!(base.len(), 1206);
    cases.push(base[..1205].to_vec());
    let mut oversized = base.clone();
    oversized.push(0);
    cases.push(oversized);
    for (offset, bytes) in [
        (0, vec![0, 0]),
        (2, 36_000u16.to_le_bytes().to_vec()),
        (1200, 3_600_000_000u32.to_le_bytes().to_vec()),
        (1204, vec![0x36]),
        (1205, vec![0x21]),
    ] {
        let mut raw = base.clone();
        raw[offset..offset + bytes.len()].copy_from_slice(&bytes);
        cases.push(raw);
    }
    let mut dual = packet(0x39, 0);
    dual[102..104].copy_from_slice(&1u16.to_le_bytes());
    cases.push(dual);
    cases.push(historical_product21());
    for (case, bad) in cases.into_iter().enumerate() {
        let home = Home::new(&format!("vlp16-invalid-{case}"));
        let source = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (capture, address) = Capture::start(&home, source.local_addr().unwrap(), 2, "1e6");
        for _ in 0..2 {
            source.send_to(&base, address).unwrap();
        }
        wait_until(|| group_rows(&home).len() == 1);
        source.send_to(&base, address).unwrap();
        source.send_to(&bad, address).unwrap();
        let run = capture.finish();
        let error = run.failure(if matches!(case, 5 | 6 | 8) { 5 } else { 1 });
        assert_eq!(group_rows(&home).len(), 1, "{error}");
        assert_eq!(error["details"]["velodyne_vlp16"]["packets"], 2);
        check_archive(&home, &[base.clone(), base.clone()]);
    }
}

#[test]
fn configuration_bounds_are_checked_before_binding_or_creating_artifacts() {
    let home = Home::new("vlp16-invalid-config");
    for (key, value) in [
        ("max_bytes", json!(1365)),
        ("max_bytes", json!(1.25)),
        ("packets_per_group", json!(0)),
        ("packets_per_group", json!(129)),
        ("listen", json!("0.0.0.0:0")),
        ("source", json!("0.0.0.0:2368")),
        ("source", json!("239.1.2.3:2368")),
        ("output", json!("{seq}.vlp16")),
    ] {
        let mut source = json!({"id":"lidar","protocol":"velodyne_vlp16","listen":"127.0.0.1:2368","source":"127.0.0.1:2369","output":"packets.vlp16","max_bytes":1_000_000});
        source[key] = value;
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[source]}).to_string(),
        )
        .unwrap();
        home.fails(2, &RECORDING_ARGS[1..9]);
        assert!(!home.path("run.jsonl").exists());
        assert!(!home.path("packets.vlp16").exists());
    }
}

#[test]
fn hdl32e_replays_unchanged_captured21_and_keeps_its_own_container_and_exports() {
    let home = Home::new("hdl32e-captured-profile");
    let source = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (capture, address) = Capture::start_profile(
        &home,
        source.local_addr().unwrap(),
        2,
        "1e6",
        "velodyne_hdl32e",
    );
    // This captured telegram is never relabelled or rewritten to product22.
    // The explicit profile qualifies wire compatibility, not hardware identity.
    let original = historical_product21();
    let packets = vec![
        original.clone(),
        original,
        packet_product(0x38, 3_599_999_999, 0x21),
        packet_product(0x39, 0, 0x21),
    ];
    for packet in &packets {
        source.send_to(packet, address).unwrap();
    }
    assert_eq!(capture.finish().success()["samples"], 3);
    assert_eq!(group_rows_profile(&home, "velodyne_hdl32e").len(), 2);
    check_archive_profile(&home, &packets, "velodyne_hdl32e");
    check_exports(&home, false);
    assert_eq!(rows(&home)[0]["sources"][0]["protocol"], "velodyne_hdl32e");
}

#[test]
fn hdl32e_never_accepts_vlp22_or_infers_a_fallback_model() {
    let home = Home::new("hdl32e-vlp-cross-reject");
    let source = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (capture, address) = Capture::start_profile(
        &home,
        source.local_addr().unwrap(),
        128,
        "1e6",
        "velodyne_hdl32e",
    );
    source.send_to(&packet(0x37, 0), address).unwrap();
    let error = capture.finish().failure(5);
    assert_eq!(error["details"]["product_id_raw"], 0x22);
    assert_eq!(error["details"]["velodyne_hdl32e"]["committed_bytes"], 0);
    assert!(error["details"].get("velodyne_vlp16").is_none());
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("HDL-32E profile")
    );
    assert_eq!(samples(&home), Vec::<Value>::new());
    assert!(!home.path("packets.hdl32e").exists());
}

#[test]
fn hdl32e_limits_and_failed_siblings_keep_exact_earlier_ranges_and_error_identity() {
    for ceiling in [false, true] {
        let home = Home::new("hdl32e-retained-prefix");
        let source = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (capture, address) = Capture::start_profile(
            &home,
            source.local_addr().unwrap(),
            if ceiling { 128 } else { 2 },
            if ceiling { "1366.0" } else { "1e6" },
            "velodyne_hdl32e",
        );
        let original = historical_product21();
        let expected = if ceiling {
            vec![original.clone()]
        } else {
            vec![original.clone(), original.clone()]
        };
        for packet in &expected {
            source.send_to(packet, address).unwrap();
        }
        if !ceiling {
            wait_until(|| group_rows_profile(&home, "velodyne_hdl32e").len() == 1);
            source.send_to(&original, address).unwrap();
            let mut bad = packet_product(0x39, 1, 0x21);
            assert_eq!(bad.len(), 1206);
            bad[102..104].copy_from_slice(&1u16.to_le_bytes());
            source.send_to(&bad, address).unwrap();
        }
        let error = capture.finish().failure(1);
        assert!(error["details"].get("velodyne_vlp16").is_none());
        assert_eq!(
            error["details"]["velodyne_hdl32e"]["packets"],
            expected.len()
        );
        assert_eq!(
            error["details"]["velodyne_hdl32e"]["model_profile"],
            "hdl32e"
        );
        check_archive_profile(&home, &expected, "velodyne_hdl32e");
        check_exports(&home, true);
        if ceiling {
            assert_eq!(
                fs::metadata(home.path("packets.hdl32e")).unwrap().len(),
                1366
            );
        }
    }
}

fn historical_product21() -> Vec<u8> {
    // Exact unchanged first UDP payload in Kitware's catalogued 2014-11-10
    // County Fair capture, file 5b8000178d777f06857cb53c. Only bytes0..1287
    // were read by HTTP Range; the 209671884-byte capture was not downloaded.
    // Its factory interpretation is 0x37/0x21: negative under VLP-16, and
    // positive wire evidence only under the explicitly selected HDL profile.
    let text = HISTORICAL_PRODUCT21;
    let bytes = text
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                if byte.is_ascii_digit() {
                    byte - b'0'
                } else {
                    byte - b'a' + 10
                }
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect::<Vec<_>>();
    assert_eq!(bytes.len(), 1206);
    assert_eq!(
        hex(&Sha256::digest(&bytes)),
        "208e95dce5d90f5aff9d85f4ac458e0084e492171b145f18b2191560f7b02b5c"
    );
    bytes
}

const HISTORICAL_PRODUCT21: &str = "ffeecb6184062c04070764062400000164064000000168064c45320200000100000100000a00000200000100000200000700000082062c0307076d061c00000169062e00000162064900000100000100000100000a000002000001000002000007000000ffeef36184062afd060a5e062900000168063800000162064900000100000100000100000a00000200000100000200000700000084062cf3060b6006290000016a064600000166065000000100000100000100000a000002000001000002000007000000ffee1b6286062cf5060b60062900000164065e00000168064c00000100000100000100000a00000200000100000200000700000084062ceb060c67062100000165066200000162064900000100000100000100000a000002000001000002000007000000ffee426286062ae3060d6d061a00000165066400000160064c00000100000100000100000a00000200000100000200000700000084062adf060d6706210000016b066000000164064c00000100000100000100000a000002000001000002000007000000ffee6a6284062ae1060d68062200000166066600000162064c00000100000100000100000a00000200000100000200000700000084062ae1060e6406270000016e067500000168065000000100000100000100000a000002000001000002000007000000ffee926282062cdb060f6d061a00000100006400000164064c00000100000100000100000a00000200000100000200000700000084062ce806116b061e0000010000640000015e064c00000100000100000100000a000002000001000002000007000000ffeeb86286062ae006116b061e0000016206cd00000160064c00000100000100000100000a00000200000100000200000700000084062ce206116206240000016806b300000164064900000100000100000100000a000002000001000002000007000000ffeee16288062ae6061164062700000168068900000164061d00000100000100000100000a00000200000100000200000700000082062ce806116a062200000168067c00000162063a00000100000100000100000a000002000001000002000007000000ffee086384062ce206116806220000016b06720000015e064c00000100000100000100000a00000200000100000200000700000084062ce2061164062400000163066c00000162064900000100000100000100000a000002000001000002000007000000ffee316386062ce8061162063c00000167066700000160064c00000100000100000100000a00000200000100000200000700000086062ce406116d063300000164066600000162064900000100000100000100000a000002000001000002000007000000ffee576382062ae206116d062a00000168066500000162064c00000100000100000100000a00000200000100000200000700000086062ce006126b063000000168066300000164064900000100000100000100000a000002000001000002000007000000ffee806388062ae706176b064200000163065e0000015e064900000100000100000100000a00000200000100000200000700000086062ae5061765063f0000016f065e00000168065000000100000100000100000a0000020000010000020000070000002de9d7133721";
