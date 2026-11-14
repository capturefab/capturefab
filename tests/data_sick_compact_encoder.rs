//! Independent picoScan150 encoder/mixed UDP/archive fixtures; no physical encoder qualification.
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fmt::Write as _,
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    net::{SocketAddr, UdpSocket},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::{
    cli::{Home, Run},
    time,
};
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
            // loopback ICMP error (Windows reports it as a reset); this is not an
            // incoming control datagram.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                ) => {}
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
    let deadline = Instant::now() + time::secs(4);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "SICK Compact fixture state not reached"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn capture_args(seconds: u64) -> [String; 9] {
    [
        "--json".into(),
        "data".into(),
        "record".into(),
        "--config".into(),
        "sources.json".into(),
        "--for".into(),
        time::arg(seconds * 1000),
        "-o".into(),
        "run.jsonl".into(),
    ]
}
struct Capture(Option<Child>, u64);
impl Capture {
    fn start(
        home: &Home,
        source: SocketAddr,
        profile: &str,
        stream: &str,
        context: Option<&[u8]>,
        patch: Value,
    ) -> (Self, SocketAddr) {
        Self::start_for(home, source, profile, stream, context, patch, 1)
    }
    fn start_for(
        home: &Home,
        source: SocketAddr,
        profile: &str,
        stream: &str,
        context: Option<&[u8]>,
        patch: Value,
        seconds: u64,
    ) -> (Self, SocketAddr) {
        assert!(matches!(seconds, 1 | 3 | 6));
        fs::write(
            home.path("fixture-acquisition-ns"),
            time::millis(seconds * 1000).as_nanos().to_string(),
        )
        .unwrap();
        let reservation = socket();
        let target = reservation.local_addr().unwrap();
        if let Some(context) = context {
            fs::write(home.path("configured-context"), context).unwrap();
        }
        let mut config = json!({"id":"lidar","protocol":"sick_compact_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),"profile":profile,"stream":stream,"output_directory":"capture","max_bytes":100_000_000,"packets_per_group":1,"timeout_ms":time::ms(120),"every_ms":3_600_000});
        if context.is_some() {
            config["context"] = json!("configured-context");
        }
        let Value::Object(patch) = patch else {
            panic!("fixture patch must be an object")
        };
        for (key, value) in patch {
            if value.is_null() {
                config.as_object_mut().unwrap().remove(&key);
            } else {
                config[key] = value;
            }
        }
        fs::write(
            home.path("sources.json"),
            json!({"version":1,"sources":[config]}).to_string(),
        )
        .unwrap();
        drop(reservation);
        let child = home
            .command()
            .args(capture_args(seconds))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let capture = Self(Some(child), seconds);
        wait(|| {
            rows(home)
                .first()
                .is_some_and(|row| row["kind"] == "header")
        });
        (capture, target)
    }
    fn finish(mut self) -> Run {
        let deadline = Instant::now() + time::secs(6);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "bounded SICK Compact CLI completion"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let args = capture_args(self.1);
        let refs = args.each_ref().map(String::as_str);
        Run::of_output(self.0.take().unwrap().wait_with_output().unwrap(), &refs)
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
    (source, target): (SocketAddr, SocketAddr),
    profile: &str,
    stream: &str,
    context: Option<&[u8]>,
    expected: &[Vec<u8>],
    sources: &[SocketAddr],
) -> Vec<Value> {
    let raw = fs::read(home.path("capture/datagrams.sickcompact")).unwrap();
    assert_eq!(expected.len(), sources.len());
    let context_raw = context.unwrap_or_default();
    assert_eq!(&raw[..8], b"CFSCMP01");
    assert_eq!(u16_at(&raw, 8), 1);
    assert_eq!(u16_at(&raw, 10), 80);
    assert_eq!(u32_at(&raw, 12), u32::try_from(context_raw.len()).unwrap());
    assert_eq!(u32_at(&raw, 16), 65_507);
    assert_eq!(
        u16_at(&raw, 20),
        match profile {
            "multiscan100" => 1,
            "picoscan100" => 2,
            "picoscan150" => 3,
            _ => panic!("unknownprofile"),
        }
    );
    assert_eq!(u16_at(&raw, 22), u16::from(context.is_some()));
    assert_eq!(&raw[24..28], &[127, 0, 0, 1]);
    assert_eq!(
        u16_at(&raw, 28),
        if stream == "mixed" { 0 } else { source.port() }
    );
    assert_eq!(u16_at(&raw, 30), target.port());
    assert!(u64_at(&raw, 32) > 0);
    assert_eq!(&raw[40..72], Sha256::digest(context_raw).as_slice());
    assert_eq!(
        u16_at(&raw, 72),
        match stream {
            "points" => 1,
            "imu" => 2,
            "encoder" => 3,
            "mixed" => 4,
            _ => panic!("unknownstream"),
        }
    );
    assert_eq!(
        u16_at(&raw, 74),
        match stream {
            "points" | "mixed" => 36,
            "imu" => 64,
            "encoder" => 88,
            _ => panic!("unknownstream"),
        }
    );
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
        let cutoff: u64 = fs::read_to_string(home.path("fixture-acquisition-ns"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(elapsed <= cutoff);
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
            assert_eq!(packet["source"], sources[index].to_string());
            assert_eq!(&group[cursor + 24..cursor + 28], &[127, 0, 0, 1]);
            assert_eq!(u16_at(group, cursor + 28), sources[index].port());
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
    assert_eq!(
        index,
        expected.len(),
        "stream={stream}, first_raw_bytes={}",
        expected.first().map_or(0, Vec::len)
    );
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
const ENCODERS: &str = include_str!("fixtures/sick_compact_encoder/literals.json");
const ENCODER_SHA: &str = "f4dc9cb4755012a7b24414dde613b34038c79e9b3ab3ea4fc6ed59b16f6c5e68";
const DENSE: &str = include_str!("fixtures/sick_compact_encoder/dense-points-recipe.json");
const DENSE_SHA: &str = "ebdf1c76dd8500645c00fbce705b6071808a16fcc49248a6d69dab2a4f60c8e9";
fn encoder_cases(section: &str) -> Vec<Value> {
    assert_eq!(digest(ENCODERS.as_bytes()), ENCODER_SHA);
    let value: Value = serde_json::from_str(ENCODERS).unwrap();
    value[section].as_array().unwrap().clone()
}
fn encoder_raw(case: &Value) -> Vec<u8> {
    let raw = unhex(case["hex"].as_str().unwrap());
    assert_eq!(raw.len(), number(&case["bytes"]));
    assert_eq!(digest(&raw), case["sha256"]);
    raw
}
fn number(value: &Value) -> usize {
    usize::try_from(value.as_u64().unwrap()).unwrap()
}
fn raw_clock(value: &Value, raw: &Value) {
    assert_eq!(
        value,
        &json!({"raw":raw,"unit":"us","basis":"unix_microseconds_declared"})
    );
}
fn check_encoder(value: &Value, case: &Value) {
    let expected = &case["expected"];
    assert_eq!(value["kind"], "sick_compact");
    assert_eq!(value["telegram_type"], 4);
    assert_eq!(value["telegram_version"], expected["version"]);
    assert_eq!(value["byte_order"], "little_endian");
    assert_eq!(value["clock_sync"], "not_verified");
    assert_eq!(value["telegram_counter"], expected["telegram_counter"]);
    assert_eq!(value["payload_length"], expected["payload_length"]);
    assert_eq!(value["sender_id"], expected["sender_id"]);
    raw_clock(
        &value["transmit_timestamp"],
        &expected["transmit_timestamp_raw"],
    );
    assert_eq!(
        value["checksum"],
        json!({"algorithm":"crc32_iso_hdlc","scope":"frame_without_crc","stored":case["crc32"],"calculated":case["crc32"],"status":"verified"})
    );
    let encoder = &value["encoder"];
    for (actual, key) in [
        ("frame_number", "frame_number"),
        ("tick_count", "tick_count"),
        ("tick_count_at_reference_signal_1", "reference_1_tick_count"),
        ("tick_count_at_reference_signal_2", "reference_2_tick_count"),
    ] {
        assert_eq!(encoder[actual], expected[key]);
    }
    for (actual, key) in [
        ("tick_count_timestamp", "tick_timestamp_raw"),
        (
            "timestamp_of_reference_signal_1",
            "reference_1_timestamp_raw",
        ),
        (
            "timestamp_of_reference_signal_2",
            "reference_2_timestamp_raw",
        ),
    ] {
        raw_clock(&encoder[actual], &expected[key]);
    }
    let speed = &encoder["speed"];
    assert_eq!(speed["bits"], expected["speed_bits"]);
    assert_eq!(speed["classification"], expected["speed_classification"]);
    let bits = u32::try_from(expected["speed_bits"].as_u64().unwrap()).unwrap();
    assert_eq!(speed["sign_bit"], bits >> 31 != 0);
    if expected["speed_classification"] == "nan" {
        assert!(speed.get("value").is_none());
    } else {
        let actual = speed["value"].as_f64().unwrap();
        assert_eq!(actual, f64::from(f32::from_bits(bits)));
        assert_eq!(actual.is_sign_negative(), bits >> 31 != 0);
    }
    assert_eq!(
        encoder["speed_unit_status"],
        "configured_conversion_unobserved"
    );
    assert_eq!(encoder["reference_validity"], "not_observed");
    assert_eq!(encoder["input_mode"], "not_observed");
    assert_eq!(encoder["angle_conversion"], "not_inferred");
    assert!(encoder.get("angle").is_none());
    assert!(speed.get("unit").is_none());
    assert!(value.get("source_time").is_none());
    assert!(!value.to_string().contains(":null"));
}
fn dense() -> Vec<u8> {
    assert_eq!(digest(DENSE.as_bytes()), DENSE_SHA);
    let recipe: Value = serde_json::from_str(DENSE).unwrap();
    let mut raw = unhex(recipe["header_and_module_hex"].as_str().unwrap());
    raw.extend(unhex(recipe["first_record_hex"].as_str().unwrap()));
    raw.resize(raw.len() + 4 * number(&recipe["middle_zero_records"]), 0);
    raw.extend(unhex(recipe["last_record_hex"].as_str().unwrap()));
    assert_eq!(u64::from(crc32(&raw)), recipe["crc32"].as_u64().unwrap());
    raw.extend(unhex(recipe["crc32_le_hex"].as_str().unwrap()));
    assert_eq!(raw.len(), number(&recipe["packet_bytes"]));
    assert_eq!(digest(&raw), recipe["packet_sha256"]);
    raw
}
fn check_dense(value: &Value) {
    assert_eq!(value["telegram_type"], 1);
    let module = &value["modules"][0];
    assert_eq!(module["beams_per_line"], 16_000);
    assert_eq!(module["line_count"], 1);
    assert_eq!(module["echoes_per_beam"], 1);
    for (id, first, last) in [("distance", 0x1234_u64, 0xffff_u64), ("rssi", 0xabcd, 1)] {
        let bytes = packed(
            &module[id],
            "base64_le_u16",
            &json!([16_000, 1, 1]),
            "beam_line_echo",
        );
        let words = integer_words(&bytes, 2);
        assert_eq!(words.len(), 16_000);
        assert_eq!(words[0], first);
        assert_eq!(words[15_999], last);
        assert!(words[1..15_999].iter().all(|n| *n == 0));
    }
}
#[test]
fn independent_encoder_literals_before_decoder_keep_v1_v2_payload_rule_and_crc() {
    assert_eq!(digest(CORPUS.as_bytes()), CORPUS_SHA);
    for case in encoder_cases("positive") {
        let raw = encoder_raw(&case);
        assert_eq!(raw.len(), 88);
        assert_eq!(u32_at(&raw, 4), 4);
        assert_eq!(
            u32_at(&raw, 28),
            if u32_at(&raw, 24) == 1 { 52 } else { 48 }
        );
        assert_eq!(crc32(&raw[..84]), u32_at(&raw, 84));
    }
    for case in encoder_cases("negative") {
        encoder_raw(&case);
    }
    assert_eq!(dense().len(), 64_108);
}
#[test]
fn encoder_only_preserves_unsigned_tick_edges_speed_and_three_distinct_raw_clocks() {
    let home = Home::new("sick-encoder");
    let socket = socket();
    let source = socket.local_addr().unwrap();
    let context = b"encoder settings remain externally configured\0\xff";
    let (capture, target) = Capture::start(
        &home,
        source,
        "picoscan150",
        "encoder",
        Some(context),
        json!({}),
    );
    let cases = encoder_cases("positive");
    let raw: Vec<_> = cases.iter().map(encoder_raw).collect();
    for (index, bytes) in raw.iter().enumerate() {
        send(&socket, target, bytes);
        wait(|| groups(&home).len() == index + 1);
    }
    capture.finish().success();
    let values = archive(
        &home,
        (source, target),
        "picoscan150",
        "encoder",
        Some(context),
        &raw,
        &vec![source; raw.len()],
    );
    for (value, case) in values.iter().zip(&cases) {
        check_encoder(value, case);
    }
    exports(&home, false);
    no_control(&socket);
}
#[test]
fn mixed_keeps_one_socket_actual_three_source_ports_and_reception_order_without_joining() {
    let home = Home::new("sick-mixed");
    let point = socket();
    let encoder = socket();
    let imu = socket();
    let source = point.local_addr().unwrap();
    let (capture, target) = Capture::start(
        &home,
        source,
        "picoscan150",
        "mixed",
        None,
        json!({"source_port":null}),
    );
    let cases = encoder_cases("positive");
    let raw = vec![
        packet("v4_two_unequal_modules"),
        encoder_raw(&cases[0]),
        packet("plan_type2_imu_v1"),
        encoder_raw(&cases[1]),
    ];
    let sources = vec![
        source,
        encoder.local_addr().unwrap(),
        imu.local_addr().unwrap(),
        encoder.local_addr().unwrap(),
    ];
    for (index, (socket, bytes)) in [&point, &encoder, &imu, &encoder]
        .into_iter()
        .zip(&raw)
        .enumerate()
    {
        send(socket, target, bytes);
        wait(|| groups(&home).len() == index + 1);
    }
    capture.finish().success();
    let values = archive(
        &home,
        (source, target),
        "picoscan150",
        "mixed",
        None,
        &raw,
        &sources,
    );
    assert_eq!(
        values
            .iter()
            .map(|v| v["telegram_type"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 4, 2, 4]
    );
    check_native(&values[0], &vector("v4_two_unequal_modules")["oracle"]);
    check_encoder(&values[1], &cases[0]);
    check_native(&values[2], &vector("plan_type2_imu_v1")["oracle"]);
    check_encoder(&values[3], &cases[1]);
    exports(&home, false);
    for socket in [&point, &encoder, &imu] {
        no_control(socket);
    }
}
#[test]
fn selected_encoder_bad_packet_retains_exact_first_or_prior_evidence_and_typed_cause() {
    for case in encoder_cases("negative") {
        for prefix in [false, true] {
            let home = Home::new("sick-encoder-fault");
            let socket = socket();
            let source = socket.local_addr().unwrap();
            let context = b"opaque encoder context";
            let (capture, target) = Capture::start(
                &home,
                source,
                "picoscan150",
                "encoder",
                Some(context),
                json!({}),
            );
            let good = encoder_raw(&encoder_cases("positive")[0]);
            if prefix {
                send(&socket, target, &good);
                wait(|| groups(&home).len() == 1);
            }
            let bad = encoder_raw(&case);
            send(&socket, target, &bad);
            let error = capture
                .finish()
                .failure(i32::try_from(case["exit"].as_i64().unwrap()).unwrap());
            assert_eq!(error["code"], case["expected_code"]);
            rejected(&home, &error, &bad, Some(context));
            let expected = if prefix { vec![good] } else { Vec::new() };
            let values = archive(
                &home,
                (source, target),
                "picoscan150",
                "encoder",
                Some(context),
                &expected,
                &vec![source; expected.len()],
            );
            if prefix {
                check_encoder(&values[0], &encoder_cases("positive")[0]);
            }
            exports(&home, true);
            no_control(&socket);
        }
    }
}
#[test]
fn single_stream_known_others_are_bounded_ignores_and_unknown_type_still_retires() {
    let cases = encoder_cases("positive");
    let bad_case = encoder_cases("negative")
        .into_iter()
        .find(|c| c["name"] == "unknown_type5")
        .unwrap();
    let bad = encoder_raw(&bad_case);
    for stream in ["points", "imu", "encoder"] {
        let home = Home::new("sick-encoder-filter");
        let socket = socket();
        let source = socket.local_addr().unwrap();
        let (capture, target) =
            Capture::start(&home, source, "picoscan150", stream, None, json!({}));
        let selected = match stream {
            "points" => 1,
            "imu" => 2,
            _ => 4,
        };
        for kind in [1_u32, 2, 4] {
            if kind != selected {
                let mut marker = vec![2; 4];
                marker.extend(kind.to_le_bytes());
                send(&socket, target, &marker);
            }
        }
        let good = match stream {
            "points" => packet("v4_two_unequal_modules"),
            "imu" => packet("plan_type2_imu_v1"),
            _ => encoder_raw(&cases[0]),
        };
        send(&socket, target, &good);
        wait(|| groups(&home).len() == 1);
        send(&socket, target, &bad);
        let error = capture.finish().failure(5);
        assert_eq!(error["code"], "unsupported");
        assert_eq!(
            error["details"]["sick_compact_udp"]["other_stream_packets"],
            2
        );
        let values = archive(
            &home,
            (source, target),
            "picoscan150",
            stream,
            None,
            &[good],
            &[source],
        );
        if stream == "encoder" {
            check_encoder(&values[0], &cases[0]);
        } else {
            check_native(
                &values[0],
                &vector(if stream == "points" {
                    "v4_two_unequal_modules"
                } else {
                    "plan_type2_imu_v1"
                })["oracle"],
            );
        }
        rejected(&home, &error, &bad, None);
        exports(&home, true);
        no_control(&socket);
    }
    let home = Home::new("sick-encoder-work-bound");
    let socket = socket();
    let source = socket.local_addr().unwrap();
    let (capture, target) =
        Capture::start(&home, source, "picoscan150", "encoder", None, json!({}));
    let mut other = vec![0; 24_000];
    other[..8].copy_from_slice(&[2, 2, 2, 2, 1, 0, 0, 0]);
    for _ in 0..3 {
        send(&socket, target, &other);
        thread::sleep(Duration::from_millis(2));
    }
    let error = capture.finish().failure(1);
    assert_eq!(error["code"], "incomplete");
    assert_eq!(
        error["details"]["sick_compact_udp"]["other_stream_packets"],
        3
    );
    assert_eq!(samples(&home), Vec::<Value>::new());
    assert!(!home.path("capture/rejected-datagram.raw").exists());
    no_control(&socket);
}
const AMPLIFIER: &str =
    include_str!("fixtures/sick_compact_encoder/metadata-amplifier-recipe.json");
const AMPLIFIER_SHA: &str = "c0494338caba8491e99c20733b6e3f7e80b9d30450433ab4838c04754b94379d";
fn amplifier_recipe() -> Value {
    assert_eq!(digest(AMPLIFIER.as_bytes()), AMPLIFIER_SHA);
    serde_json::from_str(AMPLIFIER).unwrap()
}
fn amplifier() -> Vec<u8> {
    let recipe = amplifier_recipe();
    let mut raw = unhex(recipe["header_hex"].as_str().unwrap());
    let count = number(&recipe["module_count"]);
    for index in 0..count {
        let mut module = unhex(recipe["module_template_hex"].as_str().unwrap());
        assert_eq!(module.len(), 44);
        module[..8].copy_from_slice(&u64::try_from(index).unwrap().to_le_bytes());
        module[36..40].copy_from_slice(&(if index + 1 < count { 44_u32 } else { 0 }).to_le_bytes());
        raw.extend(module);
    }
    assert_eq!(u64::from(crc32(&raw)), recipe["crc32"].as_u64().unwrap());
    raw.extend(unhex(recipe["crc32_le_hex"].as_str().unwrap()));
    assert_eq!(raw.len(), number(&recipe["packet_bytes"]));
    assert_eq!(digest(&raw), recipe["packet_sha256"]);
    raw
}
fn check_amplifier(value: &Value) {
    let recipe = amplifier_recipe();
    let modules = value["modules"].as_array().unwrap();
    let count = number(&recipe["module_count"]);
    assert_eq!(modules.len(), count);
    let mut expected = recipe["expected_module_template"].clone();
    for (index, module) in modules.iter().enumerate() {
        expected["segment_counter"] = json!(index);
        expected["next_module_size"] = json!(if index + 1 < count { 44 } else { 0 });
        assert_eq!(module, &expected);
    }
    let mut native = value.clone();
    for key in [
        "host_packet_index",
        "received_monotonic_ns",
        "source",
        "raw_offset",
        "raw_length",
    ] {
        native.as_object_mut().unwrap().remove(key);
    }
    assert_eq!(
        serde_json::to_vec(&native).unwrap().len(),
        number(&recipe["expected_native_json_bytes"])
    );
}
fn committed_packets(home: &Home) -> usize {
    let mut file = match fs::File::open(home.path("capture/datagrams.sickcompact")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!("raw credit open: {error}"),
    };
    let length = file.metadata().unwrap().len();
    assert!(length <= 100_000_000);
    if length < 80 {
        return 0;
    }
    let mut header = [0_u8; 80];
    if file.read_exact(&mut header).is_err() {
        return 0;
    }
    assert_eq!(&header[..8], b"CFSCMP01");
    let context_length = u32_at(&header, 12);
    assert!(context_length <= 1024 * 1024);
    let mut offset = 80 + u64::from(context_length);
    let mut packets = 0;
    while offset + 32 <= length {
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut group = [0_u8; 32];
        if file.read_exact(&mut group).is_err() {
            break;
        }
        assert_eq!(&group[..8], b"CFSCGR1\0");
        let body = usize::try_from(u32_at(&group, 20)).unwrap();
        assert!(body <= 4 * 1024 * 1024);
        let prefix_length = 32 + body;
        let end = offset + u64::try_from(prefix_length + 56).unwrap();
        if end > length {
            break;
        }
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut prefix = vec![0_u8; prefix_length];
        if file.read_exact(&mut prefix).is_err() {
            break;
        }
        let mut footer = [0_u8; 56];
        if file.read_exact(&mut footer).is_err() {
            break;
        }
        assert_eq!(&footer[..8], b"CFSCCM1\0");
        assert_eq!(u64_at(&footer, 8), u64_at(&group, 8));
        assert_eq!(u64_at(&footer, 16), u64::try_from(prefix_length).unwrap());
        assert_eq!(&footer[24..], Sha256::digest(&prefix).as_slice());
        packets += usize::try_from(u32_at(&group, 16)).unwrap();
        offset = end;
    }
    packets
}
fn wait_credit(home: &Home, required: usize, mode: &str) {
    let deadline = Instant::now() + time::secs(8);
    loop {
        let observed = committed_packets(home);
        if observed >= required {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "raw commit footer credit stalled at {observed}/{required}, stress_branch={mode}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn max32_and_exact_json_group_boundary_keep_pending_packet_order_and_original_clocks() {
    for mode in ["small", "dense", "metadata"] {
        let home = Home::new(&format!("sick-encoder-pending-{mode}"));
        let socket = socket();
        let source = socket.local_addr().unwrap();
        let (capture, target) = Capture::start_for(
            &home,
            source,
            "picoscan150",
            "mixed",
            None,
            json!({"source_port":null,"packets_per_group":32,"timeout_ms":time::ms(600)}),
            if mode == "small" { 1 } else { 6 },
        );
        let raw = match mode {
            "dense" => dense(),
            "metadata" => amplifier(),
            _ => encoder_raw(&encoder_cases("positive")[2]),
        };
        for sent in 1..=33 {
            send(&socket, target, &raw);
            if mode == "small" {
                // All 33 tiny datagrams go out back to back, so they arrive
                // within the fixed 100ms window however late a sleep would wake.
            } else if sent % 2 == 0 || sent == 33 {
                // At most two new UDP datagrams are outstanding. Credit comes
                // from a verified raw commit footer, never a control ACK or
                // polling/re-parsing the potentially large JSONL recording.
                wait_credit(&home, sent, mode);
            }
        }
        capture.finish().success();
        let expected = vec![raw; 33];
        let values = archive(
            &home,
            (source, target),
            "picoscan150",
            "mixed",
            None,
            &expected,
            &vec![source; 33],
        );
        assert_eq!(values.len(), 33, "stress branch {mode}");
        let counts: Vec<_> = groups(&home)
            .iter()
            .map(|g| number(&g["value"]["packet_count"]))
            .collect();
        assert!(counts.iter().all(|n| *n <= 32));
        match mode {
            "metadata" => {
                assert_eq!(
                    counts[0], 1,
                    "two admitted metadata packets cannot share one JSON group"
                );
                let first_group = &groups(&home)[0]["value"];
                let first_bytes = serde_json::to_vec(first_group).unwrap().len();
                let next_bytes = serde_json::to_vec(&values[counts[0]]).unwrap().len();
                assert!(first_bytes + next_bytes.saturating_sub(512) > 1024 * 1024 - 16 * 1024);
                for value in &values {
                    check_amplifier(value);
                }
            }
            "dense" => {
                for value in &values {
                    check_dense(value);
                }
            }
            _ => {
                assert_eq!(counts, [32, 1]);
                for value in &values {
                    check_encoder(value, &encoder_cases("positive")[2]);
                }
            }
        }
        exports(&home, false);
        no_control(&socket);
    }
}
#[test]
fn cutoff_finish_never_receives_new_encoder_packet() {
    let home = Home::new("sick-encoder-cutoff");
    let socket = socket();
    let source = socket.local_addr().unwrap();
    let (capture, target) =
        Capture::start(&home, source, "picoscan150", "encoder", None, json!({}));
    let case = &encoder_cases("positive")[0];
    let raw = encoder_raw(case);
    send(&socket, target, &raw);
    wait(|| groups(&home).len() == 1);
    thread::sleep(time::millis(1100));
    send(&socket, target, &raw);
    capture.finish().success();
    let values = archive(
        &home,
        (source, target),
        "picoscan150",
        "encoder",
        None,
        &[raw],
        &[source],
    );
    assert_eq!(values.len(), 1);
    check_encoder(&values[0], case);
    exports(&home, false);
    no_control(&socket);
}
#[test]
fn storage_limits_keep_synced_prefix_and_decoder_cause_survives_evidence_failure() {
    let case = &encoder_cases("positive")[0];
    let raw = encoder_raw(case);
    for mode in ["quota", "floor", "evidence_floor", "exists"] {
        let home = Home::new("sick-encoder-storage");
        let socket = socket();
        let source = socket.local_addr().unwrap();
        if mode == "exists" {
            fs::create_dir(home.path("capture")).unwrap();
            fs::write(home.path("capture/foreign"), b"operator").unwrap();
            let reservation = UdpSocket::bind("127.0.0.1:0").unwrap();
            let target = reservation.local_addr().unwrap();
            drop(reservation);
            let source_config = json!({"version":1,"sources":[{"id":"lidar","protocol":"sick_compact_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),"profile":"picoscan150","stream":"encoder","output_directory":"capture","max_bytes":100_000_000}]});
            fs::write(home.path("sources.json"), source_config.to_string()).unwrap();
            assert_eq!(home.run(&ARGS).failure(1)["code"], "exists");
            assert_eq!(fs::read(home.path("capture/foreign")).unwrap(), b"operator");
            assert_eq!(fs::read_dir(home.path("capture")).unwrap().count(), 1);
            no_control(&socket);
            continue;
        }
        let patch = if mode == "quota" {
            json!({"max_bytes":81_891+80+32+32+raw.len()+56})
        } else {
            json!({"min_free_bytes":u64::MAX})
        };
        let (capture, target) =
            Capture::start(&home, source, "picoscan150", "encoder", None, patch);
        if mode == "evidence_floor" {
            let bad = encoder_cases("negative")
                .into_iter()
                .find(|c| c["name"] == "crc_mutation")
                .unwrap();
            send(&socket, target, &encoder_raw(&bad));
            let error = capture.finish().failure(1);
            assert_eq!(error["code"], "incomplete");
            assert_eq!(
                error["details"]["sick_compact_udp"]["evidence_write_error"]["code"],
                "storage_full"
            );
            assert_eq!(samples(&home), Vec::<Value>::new());
            assert!(!home.path("capture/rejected-datagram.raw").exists());
        } else {
            send(&socket, target, &raw);
            if mode == "quota" {
                wait(|| groups(&home).len() == 1);
                send(&socket, target, &raw);
            }
            let error = capture.finish().failure(6);
            assert_eq!(error["code"], "storage_full");
            if mode == "quota" {
                let values = archive(
                    &home,
                    (source, target),
                    "picoscan150",
                    "encoder",
                    None,
                    std::slice::from_ref(&raw),
                    &[source],
                );
                check_encoder(&values[0], case);
            } else {
                assert_eq!(samples(&home), Vec::<Value>::new());
                assert!(!home.path("capture/datagrams.sickcompact").exists());
            }
        }
        exports(&home, true);
        no_control(&socket);
    }
}
#[test]
fn old_profiles_reject_new_streams_before_output_or_socket() {
    for profile in ["multiscan100", "picoscan100"] {
        for stream in ["encoder", "mixed"] {
            let home = Home::new("sick-encoder-preopen");
            let reservation = socket();
            let target = reservation.local_addr().unwrap();
            drop(reservation);
            let config = json!({"version":1,"sources":[{"id":"lidar","protocol":"sick_compact_udp","listen":target,"source_ip":"127.0.0.1","profile":profile,"stream":stream,"output_directory":"capture","max_bytes":100_000_000}]});
            fs::write(home.path("sources.json"), config.to_string()).unwrap();
            assert_eq!(home.run(&ARGS).failure(5)["code"], "unsupported");
            assert!(!home.path("run.jsonl").exists());
            assert!(!home.path("capture").exists());
            let reclaim = UdpSocket::bind(target).unwrap();
            no_control(&reclaim);
        }
    }
}
