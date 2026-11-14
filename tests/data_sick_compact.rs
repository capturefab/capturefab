//! Independent SICK Compact UDP/archive fixtures; no physical-device qualification.
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fmt::Write as _,
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
const CORPUS: &str = include_str!("fixtures/sick_compact/packet-literals.json");
const CORPUS_SHA: &str = "4773e8ee8313727aa2fcf8cdf755c28eb4fcf74b1928add0e88c9e17f50ba119";
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn unhex(text: &str) -> Vec<u8> {
    assert!(text.is_ascii());
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    assert_eq!(remainder, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn vector(name: &str) -> Value {
    assert_eq!(digest(CORPUS.as_bytes()), CORPUS_SHA);
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    corpus["variants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["name"] == name)
        .unwrap()
        .clone()
}
fn packet(name: &str) -> Vec<u8> {
    let value = vector(name);
    let raw = unhex(value["packet_hex"].as_str().unwrap());
    assert_eq!(value["packet_bytes"], raw.len());
    assert_eq!(value["packet_sha256"], digest(&raw));
    raw
}
// Table-free independent CRC-32/ISO-HDLC arithmetic, not the product codec.
fn crc32(raw: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in raw {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&socket)
        .set_send_buffer_size(128 * 1024)
        .unwrap();
    socket
}
fn no_control(socket: &UdpSocket) {
    socket.set_nonblocking(true).unwrap();
    for _ in 0..32 {
        match socket.recv_from(&mut [0; 1]) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            // A deliberately late fixture send can generate an asynchronous
            // loopback ICMP error; this is not an incoming control datagram.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
            other => panic!("passive source emitted data or unexpected receive state: {other:?}"),
        }
    }
    panic!("bounded ICMP queue did not terminate");
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
        .filter(|r| r["kind"] == "sample")
        .collect()
}
fn groups(home: &Home) -> Vec<Value> {
    samples(home)
        .into_iter()
        .filter(|r| r["value"]["kind"] == "sick_compact_udp_group")
        .collect()
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "SICK Compact fixture state not reached"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
struct Capture(Option<Child>);
impl Capture {
    fn start(
        home: &Home,
        source: SocketAddr,
        profile: &str,
        stream: &str,
        context: Option<&[u8]>,
        patch: Value,
    ) -> (Self, SocketAddr) {
        let reservation = socket();
        let target = reservation.local_addr().unwrap();
        if let Some(context) = context {
            fs::write(home.path("configured-context"), context).unwrap();
        }
        let mut config = json!({"id":"lidar","protocol":"sick_compact_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),"profile":profile,"stream":stream,"output_directory":"capture","max_bytes":100_000_000,"packets_per_group":1,"timeout_ms":120,"every_ms":3_600_000});
        if context.is_some() {
            config["context"] = json!("configured-context");
        }
        let Value::Object(patch) = patch else {
            panic!("fixture patch must be an object")
        };
        for (key, value) in patch {
            config[key] = value;
        }
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[config]}).to_string(),
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
        let capture = Self(Some(child));
        wait(|| {
            rows(home)
                .first()
                .is_some_and(|row| row["kind"] == "header")
        });
        (capture, target)
    }
    fn finish(mut self) -> Run {
        let deadline = Instant::now() + Duration::from_secs(6);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "bounded SICK Compact CLI completion"
            );
            thread::sleep(Duration::from_millis(2));
        }
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
fn u16_at(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(raw[offset..offset + 2].try_into().unwrap())
}
fn u32_at(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap())
}
fn u64_at(raw: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(raw[offset..offset + 8].try_into().unwrap())
}
fn exports(home: &Home, incomplete: bool) {
    for format in ["jsonl", "csv"] {
        let output = format!("export.{format}");
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            &output,
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
    }
    let original = samples(home);
    let projected: Vec<_> = original
        .iter()
        .map(|sample| {
            let mut value = sample.clone();
            value.as_object_mut().unwrap().remove("kind");
            value
        })
        .collect();
    let exported: Vec<Value> = fs::read_to_string(home.path("export.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(exported, projected);
    let csv = fs::read_to_string(home.path("export.csv")).unwrap();
    let mut expected = String::from(
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n",
    );
    for sample in &original {
        let quote = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));
        writeln!(
            expected,
            "{},{},{},{},{},{},{},{},{}",
            sample["seq"].as_u64().unwrap(),
            quote(sample["source"].as_str().unwrap()),
            quote(sample["signal"].as_str().unwrap()),
            quote(sample["time"].as_str().unwrap()),
            sample["elapsed_ns"].as_u64().unwrap(),
            sample["read_duration_ns"].as_u64().unwrap(),
            sample["delivery_latency_ns"].as_u64().unwrap(),
            quote(""),
            quote(&sample["value"].to_string())
        )
        .unwrap();
    }
    assert_eq!(csv, expected);
}
fn clocks(home: &Home) {
    let all = rows(home);
    let header = &all[0];
    let start =
        chrono::DateTime::parse_from_rfc3339(header["started_at"].as_str().unwrap()).unwrap();
    let mut previous = 0;
    for (index, sample) in samples(home).iter().enumerate() {
        assert_eq!(sample["seq"], index + 1);
        assert!(sample.get("source_time").is_none());
        let elapsed = sample["elapsed_ns"].as_u64().unwrap();
        assert!(elapsed >= previous);
        previous = elapsed;
        let actual =
            chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
        assert_eq!(
            actual,
            start + chrono::Duration::nanoseconds(i64::try_from(elapsed).unwrap())
        );
        sample["read_duration_ns"].as_u64().unwrap();
        sample["delivery_latency_ns"].as_u64().unwrap();
        assert!(serde_json::to_vec(sample).unwrap().len() < 1024 * 1024);
    }
}

fn packed(column: &Value, encoding: &str, shape: &Value, order: &str) -> Vec<u8> {
    assert_eq!(column["encoding"], encoding);
    assert_eq!(&column["shape"], shape);
    assert_eq!(column["order"], order);
    STANDARD.decode(column["data"].as_str().unwrap()).unwrap()
}
fn integer_words(raw: &[u8], width: usize) -> Vec<u64> {
    assert_eq!(raw.len() % width, 0);
    raw.chunks(width)
        .map(|word| {
            word.iter()
                .enumerate()
                .fold(0_u64, |n, (i, b)| n | (u64::from(*b) << (i * 8)))
        })
        .collect()
}
fn metadata_column(actual: &Value, expected: &Value, field: &str, encoding: &str, width: usize) {
    let values = expected[field].as_array().unwrap();
    let raw = packed(actual, encoding, &json!([values.len()]), "line");
    assert_eq!(
        integer_words(&raw, width),
        values
            .iter()
            .map(|n| n.as_u64().unwrap())
            .collect::<Vec<_>>()
    );
}
fn check_module(actual: &Value, expected: &Value, version: u64) {
    for field in [
        "module_bytes",
        "segment_counter",
        "frame_number",
        "sender_id",
        "line_count",
        "beams_per_line",
        "echoes_per_beam",
        "next_module_size",
        "reserved_raw",
        "echo_mask_raw",
        "beam_mask_raw",
    ] {
        assert_eq!(&actual[field], &expected[field], "field {field}");
    }
    metadata_column(
        &actual["timestamp_start"],
        expected,
        "timestamp_start_raw",
        "base64_le_u64",
        8,
    );
    metadata_column(
        &actual["timestamp_stop"],
        expected,
        "timestamp_stop_raw",
        "base64_le_u64",
        8,
    );
    for key in ["timestamp_start", "timestamp_stop"] {
        assert_eq!(
            actual[key]["basis"],
            "device_timebase_microseconds_declared"
        );
        assert_eq!(actual[key]["unit"], "us");
    }
    for (key, raw) in [
        ("phi", "phi_words"),
        ("theta_start", "theta_start_words"),
        ("theta_stop", "theta_stop_words"),
    ] {
        metadata_column(&actual[key], expected, raw, "ieee754_le_u32_bits", 4);
        assert_eq!(actual[key]["unit"], "rad");
    }
    if version == 4 {
        let bits = u32::try_from(expected["distance_scale_word"].as_u64().unwrap()).unwrap();
        assert_eq!(actual["distance_scale"]["bits"], bits);
        assert_eq!(actual["distance_scale"]["classification"], "finite");
        assert_eq!(
            actual["distance_scale"]["value"].as_f64().unwrap(),
            f64::from(f32::from_bits(bits))
        );
        assert_eq!(actual["distance_relation"], "raw_times_distance_scale_mm");
    } else {
        assert!(actual.get("distance_scale").is_none());
        assert_eq!(actual["distance_relation"], "raw_mm");
    }
    let b = expected["beams_per_line"].as_u64().unwrap();
    let l = expected["line_count"].as_u64().unwrap();
    let e = expected["echoes_per_beam"].as_u64().unwrap();
    assert_eq!(actual["beam_line_count"], b * l);
    assert_eq!(actual["echo_cell_count"], b * l * e);
    let echo_mask = expected["echo_mask_raw"].as_u64().unwrap();
    for (field, mask) in [("distance", 1), ("rssi", 2)] {
        if echo_mask & mask == 0 {
            assert!(actual.get(field).is_none());
            continue;
        }
        let raw = packed(
            &actual[field],
            "base64_le_u16",
            &json!([b, l, e]),
            "beam_line_echo",
        );
        let values = integer_words(&raw, 2);
        assert_eq!(u64::try_from(values.len()).unwrap(), b * l * e);
        for item in expected["echo_values"].as_array().unwrap() {
            let at = ((item["beam"].as_u64().unwrap() * l + item["line"].as_u64().unwrap()) * e)
                + item["echo"].as_u64().unwrap();
            let key = if field == "distance" {
                "distance_raw"
            } else {
                "rssi_raw"
            };
            assert_eq!(
                values[usize::try_from(at).unwrap()],
                item[key].as_u64().unwrap()
            );
        }
    }
    let beam_mask = expected["beam_mask_raw"].as_u64().unwrap();
    for (field, mask, width, encoding, key) in [
        ("azimuth", 2, 2, "base64_le_u16", "azimuth_raw"),
        ("properties", 1, 1, "base64_u8", "properties_raw"),
    ] {
        if beam_mask & mask == 0 {
            assert!(actual.get(field).is_none());
            continue;
        }
        let raw = packed(&actual[field], encoding, &json!([b, l]), "beam_line");
        let values = integer_words(&raw, width);
        assert_eq!(u64::try_from(values.len()).unwrap(), b * l);
        for item in expected["beam_values"].as_array().unwrap() {
            let at = item["beam"].as_u64().unwrap() * l + item["line"].as_u64().unwrap();
            assert_eq!(
                values[usize::try_from(at).unwrap()],
                item[key].as_u64().unwrap()
            );
        }
    }
    assert_eq!(actual["reflector_mask"], 1);
    assert_eq!(actual["property_reserved_mask"], 254);
}
fn check_native(actual: &Value, expected: &Value) {
    assert_eq!(actual["kind"], "sick_compact");
    assert_eq!(actual["byte_order"], "little_endian");
    assert_eq!(actual["checksum"]["algorithm"], "crc32_iso_hdlc");
    assert_eq!(actual["checksum"]["scope"], "frame_without_crc");
    assert_eq!(actual["checksum"]["status"], "verified");
    assert_eq!(
        actual["checksum"]["stored"],
        actual["checksum"]["calculated"]
    );
    assert_eq!(actual["clock_sync"], "not_verified");
    assert!(actual.get("source_time").is_none());
    assert_eq!(actual["telegram_type"], expected["telegram_type"]);
    assert_eq!(actual["telegram_version"], expected["version"]);
    if expected["telegram_type"] == 1 {
        assert_eq!(actual["telegram_counter"], expected["telegram_counter"]);
        assert_eq!(
            actual["transmit_timestamp"]["raw"],
            expected["transmit_timestamp_raw"]
        );
        assert_eq!(
            actual["transmit_timestamp"]["basis"],
            "unix_microseconds_declared"
        );
        assert_eq!(actual["size_module_0"], expected["size_module_0"]);
        let modules = actual["modules"].as_array().unwrap();
        let expected_modules = expected["modules"].as_array().unwrap();
        assert_eq!(modules.len(), expected_modules.len());
        for (module, expected_module) in modules.iter().zip(expected_modules) {
            check_module(
                module,
                expected_module,
                expected["version"].as_u64().unwrap(),
            );
        }
    } else {
        assert_eq!(actual["timestamp"]["raw"], expected["timestamp_raw"]);
        assert_eq!(actual["timestamp"]["basis"], "unix_microseconds_declared");
        for (key, raw, count, order, unit) in [
            (
                "acceleration",
                "acceleration_words",
                3,
                "x_y_z",
                Some("m/s^2"),
            ),
            (
                "angular_velocity",
                "angular_velocity_words",
                3,
                "x_y_z",
                Some("rad/s"),
            ),
            ("orientation", "orientation_words_wxyz", 4, "w_x_y_z", None),
        ] {
            let data = packed(
                &actual["imu"][key],
                "ieee754_le_u32_bits",
                &json!([count]),
                order,
            );
            assert_eq!(
                integer_words(&data, 4),
                expected[raw]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap())
                    .collect::<Vec<_>>()
            );
            if let Some(unit) = unit {
                assert_eq!(actual["imu"][key]["unit"], unit);
            } else {
                assert!(actual["imu"][key].get("unit").is_none());
            }
        }
    }
    assert!(serde_json::to_vec(actual).unwrap().len() <= (1024 * 1024 - 16 * 1024));
}

fn archive(
    home: &Home,
    source: SocketAddr,
    target: SocketAddr,
    profile: &str,
    stream: &str,
    context: Option<&[u8]>,
    expected: &[Vec<u8>],
) -> Vec<Value> {
    let raw = fs::read(home.path("capture/datagrams.sickcompact")).unwrap();
    let context_raw = context.unwrap_or_default();
    assert_eq!(&raw[..8], b"CFSCMP01");
    assert_eq!(u16_at(&raw, 8), 1);
    assert_eq!(u16_at(&raw, 10), 80);
    assert_eq!(u32_at(&raw, 12), u32::try_from(context_raw.len()).unwrap());
    assert_eq!(u32_at(&raw, 16), 65_507);
    assert_eq!(
        u16_at(&raw, 20),
        if profile == "multiscan100" { 1 } else { 2 }
    );
    assert_eq!(u16_at(&raw, 22), u16::from(context.is_some()));
    assert_eq!(&raw[24..28], &[127, 0, 0, 1]);
    assert_eq!(u16_at(&raw, 28), source.port());
    assert_eq!(u16_at(&raw, 30), target.port());
    assert!(u64_at(&raw, 32) > 0);
    assert_eq!(&raw[40..72], Sha256::digest(context_raw).as_slice());
    assert_eq!(u16_at(&raw, 72), if stream == "points" { 1 } else { 2 });
    assert_eq!(u16_at(&raw, 74), if stream == "points" { 36 } else { 64 });
    assert_eq!(&raw[76..80], &[0; 4]);
    assert_eq!(&raw[80..80 + context_raw.len()], context_raw);
    let mut offset = 80 + context_raw.len();
    let mut index = 0;
    let mut native = Vec::new();
    let mut prior_receive = u64_at(&raw, 32);
    let mut inferred_run_origin = None;
    let all_groups = groups(home);
    for (number, sample) in all_groups.iter().enumerate() {
        let value = &sample["value"];
        let elapsed = sample["elapsed_ns"].as_u64().unwrap();
        let receive = value["last_received_monotonic_ns"].as_u64().unwrap();
        let run_origin = receive.checked_sub(elapsed).unwrap();
        if let Some(origin) = inferred_run_origin {
            assert_eq!(run_origin, origin);
        }
        inferred_run_origin = Some(run_origin);
        assert!(elapsed <= 1_000_000_000);
        assert_eq!(value["offset"], offset);
        assert_eq!(
            value["sensor"],
            json!({"profile":profile,"profile_source":"configured","stream":stream,"identity_status":"peer_filter_only","context_status":if context.is_some(){"configured_unverified"}else{"absent"}})
        );
        if let Some(context) = context {
            assert_eq!(value["context"]["offset"], 80);
            assert_eq!(value["context"]["length"], context.len());
            assert_eq!(value["context"]["sha256"], digest(context));
        } else {
            assert!(value.get("context").is_none());
        }
        let length = usize::try_from(value["length"].as_u64().unwrap()).unwrap();
        let group = &raw[offset..offset + length];
        assert_eq!(&group[..8], b"CFSCGR1\0");
        assert_eq!(u64_at(group, 8), u64::try_from(number + 1).unwrap());
        assert_eq!(value["group"], number + 1);
        assert_eq!(value["sha256"], digest(group));
        assert_eq!(u64_at(group, 24), u64::try_from(index + 1).unwrap());
        assert_eq!(value["first_packet_index"], index + 1);
        let packets = value["datagrams"].as_array().unwrap();
        assert_eq!(value["packet_count"], packets.len());
        assert_eq!(usize::try_from(u32_at(group, 16)).unwrap(), packets.len());
        let mut cursor = 32;
        for packet in packets {
            assert_eq!(&group[cursor..cursor + 4], b"PKT1");
            let size = usize::try_from(u32_at(group, cursor + 4)).unwrap();
            assert_eq!(&group[cursor + 32..cursor + 32 + size], expected[index]);
            assert_eq!(u64_at(group, cursor + 8), u64::try_from(index + 1).unwrap());
            assert_eq!(packet["host_packet_index"], index + 1);
            assert_eq!(packet["raw_offset"], offset + cursor + 32);
            assert_eq!(packet["raw_length"], size);
            let received = u64_at(group, cursor + 16);
            assert!(received >= prior_receive);
            prior_receive = received;
            assert_eq!(packet["received_monotonic_ns"], received);
            assert_eq!(packet["source"], source.to_string());
            assert_eq!(&group[cursor + 24..cursor + 28], &[127, 0, 0, 1]);
            assert_eq!(u16_at(group, cursor + 28), source.port());
            assert_eq!(&group[cursor + 30..cursor + 32], &[0; 2]);
            native.push(packet.clone());
            index += 1;
            cursor += 32 + size;
        }
        assert_eq!(value["last_packet_index"], index);
        assert_eq!(
            value["first_received_monotonic_ns"],
            packets[0]["received_monotonic_ns"]
        );
        assert_eq!(
            value["last_received_monotonic_ns"],
            packets.last().unwrap()["received_monotonic_ns"]
        );
        assert_eq!(u32_at(group, 20), u32::try_from(cursor - 32).unwrap());
        assert_eq!(cursor + 56, length);
        assert_eq!(&group[cursor..cursor + 8], b"CFSCCM1\0");
        assert_eq!(
            u64_at(group, cursor + 8),
            u64::try_from(number + 1).unwrap()
        );
        assert_eq!(u64_at(group, cursor + 16), u64::try_from(cursor).unwrap());
        assert_eq!(
            &group[cursor + 24..],
            Sha256::digest(&group[..cursor]).as_slice()
        );
        offset += length;
    }
    assert_eq!(index, expected.len());
    assert_eq!(offset, raw.len());
    if !expected.is_empty() {
        let all = samples(home);
        let terminal = &all.last().unwrap()["value"];
        assert_eq!(terminal["kind"], "sick_compact_udp_file");
        assert_eq!(terminal["groups"], all_groups.len());
        assert_eq!(terminal["packets"], expected.len());
        assert_eq!(terminal["committed_bytes"], raw.len());
        assert_eq!(terminal["sha256_committed_prefix"], digest(&raw));
        assert_eq!(terminal["archive_committed_prefix_only"], true);
        assert_eq!(terminal["jsonl_publication_not_proven"], true);
        assert_eq!(terminal["storage_finalized"], true);
        assert_eq!(terminal["uncommitted_tail_bytes"], 0);
    }
    clocks(home);
    native
}
fn send(socket: &UdpSocket, target: SocketAddr, bytes: &[u8]) {
    assert_eq!(socket.send_to(bytes, target).unwrap(), bytes.len());
}
fn rejected(home: &Home, error: &Value, raw: &[u8], context: Option<&[u8]>) {
    let prefix = &error["details"]["sick_compact_udp"];
    let evidence = &prefix["rejected_datagram"];
    assert_eq!(evidence["complete"], true);
    assert_eq!(evidence["raw"]["length"], raw.len());
    assert_eq!(evidence["raw"]["sha256"], digest(raw));
    assert_eq!(
        fs::read(home.path("capture/rejected-datagram.raw")).unwrap(),
        raw
    );
    let encoded = fs::read(home.path("capture/rejected-datagram.json")).unwrap();
    assert_eq!(evidence["metadata"]["length"], encoded.len());
    assert_eq!(evidence["metadata"]["sha256"], digest(&encoded));
    let metadata: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(metadata["kind"], "sick_compact_rejected_datagram");
    assert_eq!(metadata["raw_length"], raw.len());
    assert_eq!(metadata["sha256"], digest(raw));
    assert_eq!(metadata["cause"]["code"], error["code"]);
    metadata["received_monotonic_ns"].as_u64().unwrap();
    assert_eq!(prefix["diagnostic_bytes"], raw.len() + encoded.len());
    assert_eq!(prefix["failure_reserved_bytes"], 81_891);
    if let Some(context) = context {
        assert_eq!(metadata["context"]["offset"], 80);
        assert_eq!(metadata["context"]["length"], context.len());
        assert_eq!(metadata["context"]["sha256"], digest(context));
        assert_eq!(metadata["context"]["file"], "datagrams.sickcompact");
    }
}
#[test]
fn immutable_primary_corpus_precedes_decoder_and_all_hashes_are_exact() {
    assert_eq!(digest(CORPUS.as_bytes()), CORPUS_SHA);
    let corpus: Value = serde_json::from_str(CORPUS).unwrap();
    let cases = corpus["variants"].as_array().unwrap();
    assert_eq!(cases.len(), 43);
    for case in cases {
        let raw = unhex(case["packet_hex"].as_str().unwrap());
        assert_eq!(case["packet_sha256"], digest(&raw));
        assert_eq!(case["packet_bytes"], raw.len());
    }
}
#[test]
fn both_configured_profiles_v3_v4_keep_repeated_native_counters_and_exact_archives() {
    for profile in ["multiscan100", "picoscan100"] {
        for name in ["plan_type1_v3", "plan_type1_v4"] {
            let home = Home::new("sick-points");
            let socket = socket();
            let source = socket.local_addr().unwrap();
            let context = [0, 0xff, 0x80, b'\n', b'{', b'}'];
            let (capture, target) =
                Capture::start(&home, source, profile, "points", Some(&context), json!({}));
            let raw = packet(name);
            send(&socket, target, &raw);
            wait(|| groups(&home).len() == 1);
            send(&socket, target, &raw);
            capture.finish().success();
            let native = archive(
                &home,
                source,
                target,
                profile,
                "points",
                Some(&context),
                &[raw.clone(), raw],
            );
            assert_eq!(native.len(), 2);
            for value in &native {
                check_native(value, &vector(name)["oracle"]);
            }
            assert_eq!(native[0]["telegram_counter"], native[1]["telegram_counter"]);
            assert_eq!(native[0]["host_packet_index"], 1);
            assert_eq!(native[1]["host_packet_index"], 2);
            exports(&home, false);
            no_control(&socket);
        }
    }
}
#[test]
fn imu_ten_ieee_words_include_nonfinite_and_signed_zero_without_json_nulls() {
    let home = Home::new("sick-imu");
    let socket = socket();
    let source = socket.local_addr().unwrap();
    let (capture, target) = Capture::start(&home, source, "picoscan100", "imu", None, json!({}));
    let raw = packet("plan_type2_imu_v1");
    send(&socket, target, &raw);
    capture.finish().success();
    let values = archive(&home, source, target, "picoscan100", "imu", None, &[raw]);
    check_native(&values[0], &vector("plan_type2_imu_v1")["oracle"]);
    let full = serde_json::to_string(&values[0]).unwrap();
    assert!(!full.contains(":null"));
    exports(&home, false);
    no_control(&socket);
}
#[test]
fn selected_failure_preserves_prior_prefix_and_exact_raw_evidence_or_context_only_archive() {
    for (name, code, exit) in [
        ("crc_covers_echo_data", "incomplete", 1),
        ("first_module_too_small", "incomplete", 1),
        ("wrong_magic_crc_valid", "incomplete", 1),
        ("unsupported_type_crc_valid", "unsupported", 5),
    ] {
        for prior in [false, true] {
            let home = Home::new("sick-rejected");
            let socket = socket();
            let source = socket.local_addr().unwrap();
            let context = [0xff, 0, 17, 128];
            let (capture, target) = Capture::start(
                &home,
                source,
                "multiscan100",
                "points",
                Some(&context),
                json!({}),
            );
            let good = packet("plan_type1_v3");
            if prior {
                send(&socket, target, &good);
                wait(|| groups(&home).len() == 1);
            }
            let raw = packet(name);
            send(&socket, target, &raw);
            let error = capture.finish().failure(exit);
            assert_eq!(error["code"], code);
            rejected(&home, &error, &raw, Some(&context));
            if prior {
                let actual = archive(
                    &home,
                    source,
                    target,
                    "multiscan100",
                    "points",
                    Some(&context),
                    &[good],
                );
                check_native(&actual[0], &vector("plan_type1_v3")["oracle"]);
                assert_eq!(error["details"]["sick_compact_udp"]["packets"], 1);
            } else {
                assert_eq!(samples(&home), Vec::<Value>::new());
                let prefix = &error["details"]["sick_compact_udp"];
                assert_eq!(prefix["groups"], 0);
                assert_eq!(prefix["packets"], 0);
                assert_eq!(prefix["committed_bytes"], 0);
                assert_eq!(prefix["context_evidence_archive"]["valid_group_count"], 0);
                assert_eq!(prefix["context_evidence_archive"]["length"], 84);
                archive(
                    &home,
                    source,
                    target,
                    "multiscan100",
                    "points",
                    Some(&context),
                    &[],
                );
            }
            exports(&home, true);
            no_control(&socket);
        }
    }
}
#[test]
fn max_bytes_preserves_one_group_floor_failure_and_existing_directory_do_not_overwrite() {
    let raw = packet("plan_type1_v3");
    for mode in ["quota", "floor", "exists"] {
        let home = Home::new("sick-limits");
        let peer = socket();
        let source = peer.local_addr().unwrap();
        if mode == "exists" {
            fs::create_dir(home.path("capture")).unwrap();
            fs::write(home.path("capture/foreign"), b"operator").unwrap();
        }
        // Reserve failure evidence separately: exactly one 126B packet group fits.
        let patch = match mode {
            "quota" => json!({"max_bytes":81_891 + 80 + 32 + 32 + raw.len() + 56}),
            "floor" => json!({"min_free_bytes":u64::MAX}),
            _ => json!({}),
        };
        if mode == "exists" {
            let reservation = socket();
            let target = reservation.local_addr().unwrap();
            drop(reservation);
            let config = json!({"version":1,"sources":[{"id":"lidar","protocol":"sick_compact_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),"profile":"multiscan100","stream":"points","output_directory":"capture","max_bytes":100_000_000}]});
            fs::write(home.path("sources.json"), config.to_string()).unwrap();
            let error = home.run(&ARGS).failure(1);
            assert_eq!(error["code"], "exists");
            assert_eq!(fs::read(home.path("capture/foreign")).unwrap(), b"operator");
            assert_eq!(fs::read_dir(home.path("capture")).unwrap().count(), 1);
            no_control(&peer);
            continue;
        }
        let (capture, target) =
            Capture::start(&home, source, "multiscan100", "points", None, patch);
        send(&peer, target, &raw);
        if mode == "quota" {
            wait(|| groups(&home).len() == 1);
            send(&peer, target, &raw);
        }
        let error = capture.finish().failure(6);
        assert_eq!(error["code"], "storage_full");
        let prefix = &error["details"]["sick_compact_udp"];
        if mode == "quota" {
            archive(
                &home,
                source,
                target,
                "multiscan100",
                "points",
                None,
                std::slice::from_ref(&raw),
            );
            assert_eq!(prefix["packets"], 1);
            assert_eq!(prefix["groups"], 1);
        } else {
            assert_eq!(prefix["groups"], 0);
            assert_eq!(samples(&home), Vec::<Value>::new());
            assert!(!home.path("capture/datagrams.sickcompact").exists());
        }
        exports(&home, true);
        no_control(&peer);
    }
}
#[test]
fn foreign_ports_and_unvalidated_opposite_stream_share_a_finite_ignored_budget() {
    for overflow in [false, true] {
        let home = Home::new("sick-ignore");
        let selected = socket();
        let foreign = socket();
        let source = selected.local_addr().unwrap();
        let (capture, target) =
            Capture::start(&home, source, "multiscan100", "points", None, json!({}));
        if overflow {
            let junk = vec![0x11; 24_000];
            for _ in 0..3 {
                send(&foreign, target, &junk);
                thread::sleep(Duration::from_millis(2));
            }
            let error = capture.finish().failure(1);
            assert_eq!(error["code"], "incomplete");
            assert_eq!(error["details"]["sick_compact_udp"]["foreign_packets"], 3);
            assert_eq!(error["details"]["sick_compact_udp"]["groups"], 0);
            assert!(!home.path("capture/rejected-datagram.raw").exists());
        } else {
            send(&foreign, target, b"not a selected peer frame");
            // Exact known-opposite header only: deliberately no payload/CRC.
            send(&selected, target, &[2, 2, 2, 2, 2, 0, 0, 0]);
            let raw = packet("plan_type1_v3");
            send(&selected, target, &raw);
            capture.finish().success();
            archive(
                &home,
                source,
                target,
                "multiscan100",
                "points",
                None,
                &[raw],
            );
            let all = samples(&home);
            let terminal = &all.last().unwrap()["value"];
            assert_eq!(terminal["foreign_packets"], 1);
            assert_eq!(terminal["other_stream_packets"], 1);
            assert!(!home.path("capture/rejected-datagram.raw").exists());
            exports(&home, false);
        }
        no_control(&selected);
        no_control(&foreign);
    }
}
#[test]
fn maximum_ipv4_udp_payload_is_archived_exactly_as_selected_rejected_evidence() {
    let home = Home::new("sick-max-udp");
    let socket = socket();
    let source = socket.local_addr().unwrap();
    let (capture, target) = Capture::start(&home, source, "picoscan100", "points", None, json!({}));
    let mut raw = vec![0x5a; 65_507];
    raw[..8].copy_from_slice(&[2, 2, 2, 2, 1, 0, 0, 0]);
    let expected_crc = u32::from_le_bytes(raw[raw.len() - 4..].try_into().unwrap());
    assert_ne!(crc32(&raw[..raw.len() - 4]), expected_crc);
    send(&socket, target, &raw);
    let error = capture.finish().failure(1);
    assert_eq!(error["code"], "incomplete");
    rejected(&home, &error, &raw, None);
    assert_eq!(samples(&home), Vec::<Value>::new());
    archive(&home, source, target, "picoscan100", "points", None, &[]);
    no_control(&socket);
}
