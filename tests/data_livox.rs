//! Independent native `Livox` UDP wire fixtures; no physical-device qualification.
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    net::{SocketAddr, UdpSocket},
    process::{Child, Stdio},
    sync::LazyLock,
    thread,
    time::{Duration, Instant},
};
use support::{
    cli::{Home, Run},
    time,
};
fn args() -> [&'static str; 9] {
    static WINDOW: LazyLock<String> = LazyLock::new(|| time::arg(1000));
    [
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        WINDOW.as_str(),
        "-o",
        "run.jsonl",
    ]
}
const PROFILES: [&str; 7] = [
    "sdk2_mid360_v0",
    "sdk2_mid360s_v0",
    "sdk2_mid360l_v0",
    "sdk2_hap_tx_v0",
    "sdk2_hap_t1_v0",
    "legacy_horizon_v5",
    "legacy_avia_v5",
];
const PLAN_SHA: &str = "df8fd753ebc3c8b8a9a553942364cb481e23600a5554f19e1f5c63cdb0e4586a";
const LEGACY_SHA: &str = "9bde0a249b22f15497514928a1692e7e7a3cad0827afc28f13e05e1e629fbb78";
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
fn recipes() -> Value {
    let mut result = json!({});
    for (raw, sha) in [
        (include_str!("fixtures/livox/plan-literals.json"), PLAN_SHA),
        (
            include_str!("fixtures/livox/legacy-return-literals.json"),
            LEGACY_SHA,
        ),
    ] {
        assert_eq!(digest(raw.as_bytes()), sha);
        let value: Value = serde_json::from_str(raw).unwrap();
        for (name, recipe) in value["recipes"].as_object().unwrap() {
            result[name].clone_from(recipe);
        }
    }
    result
}
fn packet(name: &str) -> Vec<u8> {
    let all = recipes();
    let recipe = &all[name];
    let mut raw = unhex(recipe["header_hex"].as_str().unwrap());
    raw.extend(unhex(recipe["prefix_hex"].as_str().unwrap()));
    raw.resize(
        raw.len() + usize::try_from(recipe["zero_tail_bytes"].as_u64().unwrap()).unwrap(),
        0,
    );
    assert_eq!(
        raw.len(),
        usize::try_from(recipe["bytes"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(digest(&raw), recipe["sha256"]);
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
        .filter(|r| r["value"]["kind"] == "livox_udp_group")
        .collect()
}
fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + time::secs(4);
    while !ready() {
        assert!(Instant::now() < deadline, "Livox fixture state not reached");
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
        let mut config = json!({"id":"lidar","protocol":"livox_udp","listen":target,"source_ip":"127.0.0.1","source_port":source.port(),"profile":profile,"stream":stream,"output":"raw.livox","max_bytes":100_000_000,"packets_per_group":32,"timeout_ms":time::ms(120),"every_ms":3_600_000});
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
            .args(args())
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
        let deadline = Instant::now() + time::secs(6);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "bounded Livox CLI completion");
            thread::sleep(Duration::from_millis(2));
        }
        Run::of_output(self.0.take().unwrap().wait_with_output().unwrap(), &args())
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
    assert_eq!(csv.lines().count(), original.len() + 1);
    for sample in &original {
        assert!(csv.contains(&format!(
            "\"{}\"",
            sample["value"].to_string().replace('"', "\"\"")
        )));
    }
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

fn extents(profile: &str, stream: &str) -> (u16, u32) {
    if stream == "imu" {
        if profile.starts_with("sdk2_") {
            (60, 60)
        } else {
            (42, 42)
        }
    } else if profile.starts_with("sdk2_") {
        (804, 1380)
    } else if profile == "legacy_avia_v5" {
        (678, 1362)
    } else {
        (786, 1362)
    }
}
fn archive(
    home: &Home,
    source: SocketAddr,
    target: SocketAddr,
    profile: &str,
    stream: &str,
    context: Option<&[u8]>,
    packets: &[Vec<u8>],
) -> Vec<Value> {
    let raw = fs::read(home.path("raw.livox")).unwrap();
    let original = context.unwrap_or_default();
    assert_eq!(&raw[..8], b"CFLIVOX1");
    assert_eq!(u16_at(&raw, 8), 1);
    assert_eq!(u16_at(&raw, 10), 80);
    assert_eq!(u32_at(&raw, 12), u32::try_from(original.len()).unwrap());
    let (min, max) = extents(profile, stream);
    assert_eq!(u32_at(&raw, 16), max);
    assert_eq!(u16_at(&raw, 74), min);
    assert_eq!(
        usize::from(u16_at(&raw, 20)),
        PROFILES.iter().position(|p| *p == profile).unwrap() + 1
    );
    assert_eq!(u16_at(&raw, 22), u16::from(context.is_some()));
    assert_eq!(&raw[24..28], &[127, 0, 0, 1]);
    assert_eq!(u16_at(&raw, 28), source.port());
    assert_eq!(u16_at(&raw, 30), target.port());
    assert!(u64_at(&raw, 32) > 0);
    assert_eq!(&raw[40..72], Sha256::digest(original).as_slice());
    assert_eq!(u16_at(&raw, 72), if stream == "points" { 1 } else { 2 });
    assert_eq!(&raw[76..80], &[0; 4]);
    assert_eq!(&raw[80..80 + original.len()], original);
    let mut offset = 80 + original.len();
    let mut index = 0;
    let mut decoded = Vec::new();
    let mut previous_clock = 0;
    for (group_index, row) in groups(home).iter().enumerate() {
        let value = &row["value"];
        assert_eq!(value["offset"], offset);
        assert!(row.get("source_time").is_none());
        assert_eq!(
            value["sensor"],
            json!({"profile":profile,"profile_source":"configured","stream":stream,"identity_status":"peer_filter_only","context_status":if context.is_some(){"configured_unverified"}else{"absent"}})
        );
        let length = usize::try_from(value["length"].as_u64().unwrap()).unwrap();
        let group = &raw[offset..offset + length];
        assert_eq!(&group[..8], b"CFLVGR1\0");
        assert_eq!(u64_at(group, 8), u64::try_from(group_index + 1).unwrap());
        assert_eq!(value["sha256"], digest(group));
        assert_eq!(u64_at(group, 24), u64::try_from(index + 1).unwrap());
        let records = value["datagrams"].as_array().unwrap();
        assert_eq!(value["packet_count"], records.len());
        assert_eq!(usize::try_from(u32_at(group, 16)).unwrap(), records.len());
        if let Some(context) = context {
            assert_eq!(value["context"]["sha256"], digest(context));
        } else {
            assert!(value.get("context").is_none());
        }
        let mut cursor = 32;
        for packet in records {
            assert_eq!(&group[cursor..cursor + 4], b"PKT1");
            let bytes = usize::try_from(u32_at(group, cursor + 4)).unwrap();
            assert_eq!(&group[cursor + 32..cursor + 32 + bytes], packets[index]);
            assert_eq!(u64_at(group, cursor + 8), u64::try_from(index + 1).unwrap());
            assert_eq!(packet["host_packet_index"], index + 1);
            assert_eq!(packet["raw_offset"], offset + cursor + 32);
            assert_eq!(packet["raw_length"], bytes);
            let received = u64_at(group, cursor + 16);
            assert!(received > 0 && received >= previous_clock);
            previous_clock = received;
            assert_eq!(packet["received_monotonic_ns"], received);
            assert_eq!(packet["source"], source.to_string());
            assert_eq!(&group[cursor + 24..cursor + 28], &[127, 0, 0, 1]);
            assert_eq!(u16_at(group, cursor + 28), source.port());
            assert_eq!(&group[cursor + 30..cursor + 32], &[0; 2]);
            decoded.push(packet.clone());
            index += 1;
            cursor += 32 + bytes;
        }
        assert_eq!(u32_at(group, 20), u32::try_from(cursor - 32).unwrap());
        assert_eq!(cursor + 56, length);
        assert_eq!(&group[cursor..cursor + 8], b"CFLVCM1\0");
        assert_eq!(
            u64_at(group, cursor + 8),
            u64::try_from(group_index + 1).unwrap()
        );
        assert_eq!(u64_at(group, cursor + 16), u64::try_from(cursor).unwrap());
        assert_eq!(
            &group[cursor + 24..],
            Sha256::digest(&group[..cursor]).as_slice()
        );
        offset += length;
    }
    assert_eq!(index, packets.len());
    assert_eq!(offset, raw.len());
    let all = samples(home);
    let terminal = &all.last().unwrap()["value"];
    assert_eq!(terminal["kind"], "livox_udp_file");
    assert_eq!(terminal["committed_bytes"], raw.len());
    assert_eq!(terminal["sha256_committed_prefix"], digest(&raw));
    assert_eq!(terminal["jsonl_publication_not_proven"], true);
    assert_eq!(terminal["storage_finalized"], true);
    assert_eq!(terminal["uncommitted_tail_bytes"], 0);
    clocks(home);
    decoded
}
fn native(value: &Value, name: &str) {
    let all = recipes();
    let e = &all[name]["expected"];
    let raw = packet(name);
    assert_eq!(value["data_type_raw"], e["data_type"]);
    assert_eq!(
        value["packet_version_raw"],
        if name.starts_with("sdk2") { 0 } else { 5 }
    );
    assert_eq!(
        value["header_base64"],
        STANDARD.encode(&raw[..if name.starts_with("sdk2") { 36 } else { 18 }])
    );
    assert_eq!(
        value["clock"]["timestamp_base64"],
        STANDARD.encode(if name.starts_with("sdk2") {
            &raw[28..36]
        } else {
            &raw[10..18]
        })
    );
    if name == "legacy_imu_pps" {
        assert_eq!(value["clock"]["pps_offset_ns_raw"], -1000);
    } else {
        assert_eq!(
            &value["clock"]["timestamp_word_raw"],
            e.get("timestamp_raw")
                .unwrap_or_else(|| &e["timestamp_word_raw"])
        );
    }
    assert_eq!(
        value["clock"]["timestamp_type_raw"],
        raw[if name.starts_with("sdk2") { 11 } else { 8 }]
    );
    assert_eq!(
        value["clock"]["synchronization_status"],
        "not_independently_verified"
    );
    assert!(value.get("source_time").is_none());
    if name.starts_with("sdk2") {
        assert_eq!(value["generation"], "sdk2_v0");
        assert_eq!(value["checksum_status"], "crc32_verified");
        assert_eq!(value["checksum_scope"], "timestamp_and_data");
        assert_eq!(value["packet_length_raw"], raw.len());
        assert_eq!(
            value["time_interval_100ns_raw"],
            if name == "sdk2_imu" { 0 } else { 975 }
        );
        assert_eq!(value["dot_num_raw"], e["records"]);
        assert_eq!(value["reserved_base64"], STANDARD.encode(&raw[12..24]));
        assert_eq!(value["crc32_raw"], u32_at(&raw, 24));
        assert_eq!(value["udp_cnt_raw"], u16_at(&raw, 7));
        assert_eq!(value["frame_cnt_raw"], raw[9]);
        if value["profile"].as_str().unwrap().contains("hap_") {
            assert_eq!(
                value["tag_semantics_status"],
                "device_reported_not_independently_verified"
            );
            assert_eq!(
                value["pack_info"],
                json!({"byte_raw":162,"safety_info_raw":2,"tag_type_raw":0,"reserved_high_nibble_raw":10})
            );
        } else {
            assert_eq!(value["tag_semantics_status"], "unqualified_header_selector");
            assert!(value.get("pack_info").is_none());
        }
    } else {
        assert_eq!(value["generation"], "legacy_v5");
        assert_eq!(value["checksum_status"], "not_present_in_sample_protocol");
        assert_eq!(value["checksum_scope"], "none");
        assert_eq!(value["slot_raw"], 1);
        assert_eq!(value["lidar_id_raw"], 2);
        assert_eq!(value["reserved_raw"], 127);
        assert_eq!(value["status_raw"], 0xc000_0042_u32);
        assert_eq!(value["tag_semantics_status"], "raw_return_slot_preserved");
    }
    let n = &value["native"];
    if name.ends_with("imu") || name == "legacy_imu_pps" {
        assert_eq!(n["layout"], "imu_float32");
        assert_eq!(n["record_count"], 1);
        assert_eq!(n["gyro_unit"], "rad/s");
        assert_eq!(n["acceleration_unit"], "g");
        assert_eq!(
            n["gyro_raw_words"],
            json!([0x3f80_0000_u32, 0xbf00_0000_u32, 0x8000_0000_u32])
        );
        assert_eq!(
            n["acceleration_raw_words"],
            json!([0, 0x3f80_0000_u32, 0x7fc0_0001_u32])
        );
        for (axis, bits, number) in [
            (0, 0x3f80_0000_u32, 1.0),
            (1, 0xbf00_0000_u32, -0.5),
            (2, 0x8000_0000_u32, -0.0),
        ] {
            assert_eq!(n["gyro"][axis]["bits"], bits);
            assert_eq!(n["gyro"][axis]["classification"], "finite");
            assert_eq!(n["gyro"][axis]["value"].as_f64().unwrap(), number);
            assert_eq!(n["gyro"][axis]["sign_bit"], bits & 0x8000_0000 != 0);
        }
        assert!(n["gyro"][2]["value"].as_f64().unwrap().is_sign_negative());
        assert_eq!(n["acceleration"][2]["bits"], 0x7fc0_0001_u32);
        assert_eq!(n["acceleration"][2]["classification"], "nan");
        assert!(n["acceleration"][2].get("value").is_none());
        return;
    }
    let legacy = name.starts_with("legacy");
    let expected = if legacy { &e["native"] } else { e };
    let count = usize::try_from(
        expected
            .get("record_count")
            .unwrap_or_else(|| &e["records"])
            .as_u64()
            .unwrap(),
    )
    .unwrap();
    let returns = usize::try_from(
        expected
            .get("returns_per_record")
            .and_then(Value::as_u64)
            .unwrap_or(1),
    )
    .unwrap();
    assert_eq!(n["record_count"], count);
    assert_eq!(n["returns_per_record"], returns);
    let spherical = name.contains("spherical")
        || matches!(e["data_type"].as_u64().unwrap(), 3 | 5 | 8) && legacy;
    let mut records = if legacy {
        expected["first_records"].as_array().unwrap().clone()
    } else if spherical {
        (0..2)
            .map(|i| {
                json!([[
                    e["depth_prefix"][i],
                    e["reflectivity_prefix"][i],
                    e["tag_prefix"][i]
                ]])
            })
            .collect()
    } else {
        (0..2)
            .map(|i| {
                json!([[
                    e[if i == 0 {
                        "first_xyz_raw"
                    } else {
                        "second_xyz_raw"
                    }][0],
                    e[if i == 0 {
                        "first_xyz_raw"
                    } else {
                        "second_xyz_raw"
                    }][1],
                    e[if i == 0 {
                        "first_xyz_raw"
                    } else {
                        "second_xyz_raw"
                    }][2],
                    e["reflectivity_prefix"][i],
                    e["tag_prefix"][i]
                ]])
            })
            .collect()
    };
    records.resize(
        count,
        json!(vec![vec![0; if spherical { 3 } else { 5 }]; returns]),
    );
    for (key, position) in if spherical {
        vec![("depth_mm", 0), ("reflectivity", 1), ("tag_raw", 2)]
    } else {
        vec![
            ("x_raw", 0),
            ("y_raw", 1),
            ("z_raw", 2),
            ("reflectivity", 3),
            ("tag_raw", 4),
        ]
    } {
        let dense: Vec<Value> = records
            .iter()
            .map(|record| {
                json!(
                    record
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| &r[position])
                        .collect::<Vec<_>>()
                )
            })
            .collect();
        assert_eq!(n[key], json!(dense), "{name} {key}");
    }
    if spherical {
        for (key, prefix_key, angle_index) in [
            ("theta_centidegrees_raw", "zenith_prefix", 0),
            ("phi_centidegrees_raw", "azimuth_prefix", 1),
        ] {
            let mut angles: Vec<Value> = if legacy {
                expected["first_angles"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a[angle_index].clone())
                    .collect()
            } else {
                e[prefix_key].as_array().unwrap().clone()
            };
            angles.resize(count, json!(0));
            assert_eq!(n[key], json!(angles));
        }
        let mut nominal = vec![true; count];
        nominal[1] = false;
        assert_eq!(n["angles_within_nominal"], json!(nominal));
        assert!(n.get("x_raw").is_none());
    } else {
        assert_eq!(
            n["coordinate_unit_mm"],
            if name == "sdk2_cartesian16" { 10 } else { 1 }
        );
        assert_eq!(
            n["layout"],
            if name == "sdk2_cartesian16" {
                "cartesian_i16"
            } else {
                "cartesian_i32"
            }
        );
    }
}

#[test]
fn immutable_manual_recipes_keep_exact_extents_sha_and_crc_domains() {
    let all = recipes();
    assert_eq!(all.as_object().unwrap().len(), 11);
    for name in all.as_object().unwrap().keys() {
        let raw = packet(name);
        if name.starts_with("sdk2") {
            assert_eq!(crc32(&raw[28..]), u32_at(&raw, 24));
        }
    }
    let mut raw = packet("sdk2_cartesian32");
    let checksum = crc32(&raw[28..]);
    raw[7] ^= 1;
    assert_eq!(
        crc32(&raw[28..]),
        checksum,
        "header counters are outside sample CRC"
    );
    raw[28] ^= 1;
    assert_ne!(
        crc32(&raw[28..]),
        checksum,
        "timestamp is inside sample CRC"
    );
}
#[test]
fn all_qualified_profiles_keep_dense_native_returns_context_and_full_raw_groups() {
    for (profile_index, profile) in PROFILES.into_iter().enumerate() {
        let names = if profile.starts_with("sdk2") {
            if profile_index < 2 {
                vec!["sdk2_cartesian32", "sdk2_cartesian16", "sdk2_spherical"]
            } else {
                vec!["sdk2_cartesian32", "sdk2_cartesian16"]
            }
        } else if profile == "legacy_horizon_v5" {
            vec![
                "legacy_type_2",
                "legacy_type_3",
                "legacy_type_4",
                "legacy_type_5",
            ]
        } else {
            vec![
                "legacy_type_2",
                "legacy_type_3",
                "legacy_type_4",
                "legacy_type_5",
                "legacy_type_7",
                "legacy_type_8",
            ]
        };
        let context: Option<&[u8]> = match profile_index % 3 {
            0 => None,
            1 => Some(b""),
            _ => Some(b"opaque\0context\xff\x80"),
        };
        let home = Home::new("livox-native-profiles");
        let source = socket();
        let peer = source.local_addr().unwrap();
        let (capture, target) = Capture::start(&home, peer, profile, "points", context, json!({}));
        let mut packets: Vec<_> = names.iter().map(|name| packet(name)).collect();
        if profile_index == 0 {
            let mut header_variant = packet("sdk2_cartesian32");
            header_variant[7] ^= 1;
            packets.push(header_variant);
        }
        for packet in &packets {
            source.send_to(packet, target).unwrap();
            thread::sleep(Duration::from_millis(3));
        }
        assert_eq!(capture.finish().success()["status"], "complete");
        let decoded = archive(&home, peer, target, profile, "points", context, &packets);
        for (value, name) in decoded.iter().zip(&names) {
            assert_eq!(value["profile"], profile);
            assert_eq!(value["stream"], "points");
            native(value, name);
        }
        if profile_index == 0 {
            assert_eq!(decoded[3]["native"], decoded[0]["native"]);
            assert_eq!(decoded[3]["udp_cnt_raw"], 65_534);
            assert_eq!(decoded[3]["crc32_raw"], decoded[0]["crc32_raw"]);
            assert_eq!(decoded[3]["checksum_status"], "crc32_verified");
            assert_ne!(decoded[3]["header_base64"], decoded[0]["header_base64"]);
        }
        exports(&home, false);
        no_control(&source);
    }
}
fn float_edges(sdk2: bool) -> Vec<u8> {
    let mut raw = packet(if sdk2 { "sdk2_imu" } else { "legacy_imu_pps" });
    let header = if sdk2 { 36 } else { 18 };
    for (i, bits) in [0x7f80_0000_u32, 0xff80_0000, 1, 0x8000_0000, 0x7fc0_0001, 0]
        .into_iter()
        .enumerate()
    {
        raw[header + i * 4..header + i * 4 + 4].copy_from_slice(&bits.to_le_bytes());
    }
    if sdk2 {
        let crc = crc32(&raw[28..]);
        raw[24..28].copy_from_slice(&crc.to_le_bytes());
    }
    raw
}
fn edge_values(value: &Value) {
    let n = &value["native"];
    for (axis, bits, label) in [
        (0, 0x7f80_0000_u32, "positive_infinity"),
        (1, 0xff80_0000, "negative_infinity"),
    ] {
        assert_eq!(n["gyro"][axis]["bits"], bits);
        assert_eq!(n["gyro"][axis]["classification"], label);
        assert!(n["gyro"][axis].get("value").is_none());
    }
    assert_eq!(n["gyro"][2]["bits"], 1);
    assert_eq!(n["gyro"][2]["classification"], "finite");
    assert_eq!(n["gyro"][2]["value"].as_f64().unwrap(), 2.0_f64.powi(-149));
    assert!(
        n["acceleration"][0]["value"]
            .as_f64()
            .unwrap()
            .is_sign_negative()
    );
    assert_eq!(n["acceleration"][0]["bits"], 0x8000_0000_u32);
    assert_eq!(n["acceleration"][1]["classification"], "nan");
    assert!(n["acceleration"][1].get("value").is_none());
    assert!(
        !n["acceleration"][2]["value"]
            .as_f64()
            .unwrap()
            .is_sign_negative()
    );
}
#[test]
fn imu_words_signed_zero_nonfinite_and_legacy_clock_states_remain_observations() {
    for profile in PROFILES.into_iter().filter(|p| *p != "sdk2_hap_t1_v0") {
        let sdk2 = profile.starts_with("sdk2");
        let name = if sdk2 { "sdk2_imu" } else { "legacy_imu_pps" };
        let home = Home::new("livox-imu-clock");
        let source = socket();
        let peer = source.local_addr().unwrap();
        let (capture, target) = Capture::start(
            &home,
            peer,
            profile,
            "imu",
            Some(b""),
            json!({"packets_per_group":2}),
        );
        let mut packets = vec![packet(name), float_edges(sdk2)];
        if profile == "legacy_avia_v5" {
            for valid in [true, false] {
                let mut gps = packet(name);
                gps[8] = 3;
                gps[10..14].copy_from_slice(if valid {
                    &[255, 12, 31, 23]
                } else {
                    &[0, 0, 0, 24]
                });
                gps[14..18].copy_from_slice(
                    &if valid {
                        3_599_999_999_u32
                    } else {
                        3_600_000_000_u32
                    }
                    .to_le_bytes(),
                );
                packets.push(gps);
            }
            for time_type in [2, 5] {
                let mut unknown = packet(name);
                unknown[8] = time_type;
                packets.push(unknown);
            }
        }
        for packet in &packets {
            source.send_to(packet, target).unwrap();
            thread::sleep(Duration::from_millis(3));
        }
        assert_eq!(capture.finish().success()["status"], "complete");
        let decoded = archive(&home, peer, target, profile, "imu", Some(b""), &packets);
        native(&decoded[0], name);
        edge_values(&decoded[1]);
        if !sdk2 {
            assert_eq!(decoded[0]["clock"]["timestamp_word_raw"], u64::MAX - 999);
        }
        if profile == "legacy_avia_v5" {
            for (i, valid) in [true, false].into_iter().enumerate() {
                let clock = &decoded[i + 2]["clock"];
                assert_eq!(clock["domain"], "reported_gps_utc_fields");
                assert_eq!(
                    clock["utc_fields"],
                    json!({"year_raw":if valid{255}else{0},"month_raw":if valid{12}else{0},"day_raw":if valid{31}else{0},"hour_raw":if valid{23}else{24},"microseconds_within_hour_raw":if valid{3_599_999_999_u32}else{3_600_000_000_u32},"field_ranges_status":if valid{"within_nominal"}else{"out_of_nominal"}})
                );
                assert!(clock.get("calendar").is_none());
                assert!(clock.get("calendar_unzoned").is_none());
            }
            for item in &decoded[4..6] {
                assert_eq!(item["clock"]["domain"], "unqualified");
                assert_eq!(item["clock"]["timestamp_word_raw"], u64::MAX - 999);
            }
        }
        exports(&home, false);
        no_control(&source);
    }
}

fn committed_prefix(home: &Home, error: &Value, packets: u64) {
    let raw = fs::read(home.path("raw.livox")).unwrap();
    let prefix = &error["details"]["livox_udp"];
    assert_eq!(prefix["packets"], packets);
    assert_eq!(prefix["committed_bytes"], raw.len());
    assert_eq!(prefix["sha256_committed_prefix"], digest(&raw));
    assert_eq!(prefix["jsonl_publication_not_proven"], true);
}
#[test]
fn required_crc_structural_extent_and_model_gates_retire_with_exact_prior_prefix() {
    for mode in [
        "timestamp_crc",
        "data_crc",
        "zero_crc",
        "count",
        "length",
        "short",
        "long",
        "oversize",
        "version",
        "unknown_type",
        "mid_l_spherical",
        "hap_spherical",
        "horizon_triple",
    ] {
        let profile = match mode {
            "mid_l_spherical" => "sdk2_mid360l_v0",
            "hap_spherical" => "sdk2_hap_tx_v0",
            "horizon_triple" => "legacy_horizon_v5",
            _ => "sdk2_mid360_v0",
        };
        let home = Home::new("livox-retired-prefix");
        let source = socket();
        let peer = source.local_addr().unwrap();
        let (capture, target) = Capture::start(
            &home,
            peer,
            profile,
            "points",
            None,
            json!({"packets_per_group":1}),
        );
        let first = packet(if profile.starts_with("legacy") {
            "legacy_type_2"
        } else {
            "sdk2_cartesian32"
        });
        source.send_to(&first, target).unwrap();
        wait(|| groups(&home).len() == 1);
        let mut bad = first.clone();
        match mode {
            "timestamp_crc" => bad[28] ^= 1,
            "data_crc" => bad[36] ^= 1,
            "zero_crc" => bad[24..28].fill(0),
            "count" => bad[5..7].copy_from_slice(&95_u16.to_le_bytes()),
            "length" => bad[1..3].copy_from_slice(&1379_u16.to_le_bytes()),
            "short" => {
                bad.pop();
            }
            "long" => bad.push(0),
            // A truncating receiver would see the original valid prefix and
            // incorrectly accept this datagram as an ordinary 1380B packet.
            "oversize" => bad.resize(65_000, 0),
            "version" => bad[0] = 1,
            "unknown_type" => bad[10] = 0x11,
            "mid_l_spherical" | "hap_spherical" => bad = packet("sdk2_spherical"),
            "horizon_triple" => bad = packet("legacy_type_7"),
            _ => unreachable!(),
        }
        source.send_to(&bad, target).unwrap();
        let unsupported = matches!(
            mode,
            "version" | "unknown_type" | "mid_l_spherical" | "hap_spherical" | "horizon_triple"
        );
        let error = capture.finish().failure(if unsupported { 5 } else { 1 });
        assert_eq!(
            error["code"],
            if unsupported {
                "unsupported"
            } else {
                "incomplete"
            }
        );
        committed_prefix(&home, &error, 1);
        assert_eq!(groups(&home).len(), 1);
        let raw = fs::read(home.path("raw.livox")).unwrap();
        assert_eq!(&raw[144..144 + first.len()], first);
        exports(&home, true);
        no_control(&source);
    }
}
#[test]
fn foreign_work_and_late_selected_malformed_data_keep_only_committed_groups() {
    for foreign in [false, true] {
        let home = Home::new("livox-work-prefix");
        let source = socket();
        let outsider = socket();
        let peer = source.local_addr().unwrap();
        let (capture, target) = Capture::start(
            &home,
            peer,
            "sdk2_mid360_v0",
            "points",
            None,
            json!({"packets_per_group":2}),
        );
        let raw = packet("sdk2_cartesian16");
        outsider.send_to(&raw, target).unwrap(); // wrong peer is work, never a selected sample
        source.send_to(&raw, target).unwrap();
        source.send_to(&raw, target).unwrap();
        wait(|| groups(&home).len() == 1);
        if foreign {
            for _ in 0..3 {
                outsider.send_to(&vec![0; 24_000], target).unwrap();
                thread::sleep(Duration::from_millis(3));
            }
        } else {
            source.send_to(&raw, target).unwrap();
            let mut bad = raw;
            bad[36] ^= 1;
            source.send_to(&bad, target).unwrap();
        }
        let error = capture.finish().failure(1);
        assert_eq!(error["code"], "incomplete");
        committed_prefix(&home, &error, 2);
        assert_eq!(groups(&home).len(), 1);
        if foreign {
            assert_eq!(error["details"]["livox_udp"]["foreign_packets"], 4);
        } else {
            assert_eq!(error["details"]["livox_udp"]["uncommitted_packets"], 1);
        }
        exports(&home, true);
        no_control(&source);
        no_control(&outsider);
    }
}
#[test]
fn quota_no_overwrite_and_free_floor_preserve_owned_committed_prefix_only() {
    for mode in ["limit", "floor", "exists"] {
        let home = Home::new("livox-storage");
        let source = socket();
        let peer = source.local_addr().unwrap();
        let raw = packet(if mode == "limit" {
            "sdk2_cartesian32"
        } else {
            "sdk2_cartesian16"
        });
        let minimum = 80 + 32 + 32 + raw.len() + 56;
        if mode == "exists" {
            fs::write(home.path("raw.livox"), b"preexisting Livox bytes").unwrap();
        }
        let patch = match mode {
            "limit" => json!({"max_bytes":minimum,"packets_per_group":2}),
            "floor" => json!({"min_free_bytes":u64::MAX,"packets_per_group":1}),
            _ => json!({"packets_per_group":1}),
        };
        let (capture, target) =
            Capture::start(&home, peer, "sdk2_mid360_v0", "points", None, patch);
        source.send_to(&raw, target).unwrap();
        if mode == "limit" {
            source.send_to(&raw, target).unwrap();
        }
        let error = capture
            .finish()
            .failure(if mode == "exists" { 1 } else { 6 });
        assert_eq!(
            error["code"],
            if mode == "exists" {
                "exists"
            } else {
                "storage_full"
            }
        );
        if mode == "exists" {
            assert_eq!(
                fs::read(home.path("raw.livox")).unwrap(),
                b"preexisting Livox bytes"
            );
        } else if mode == "floor" {
            assert!(!home.path("raw.livox").exists());
        } else {
            committed_prefix(&home, &error, 1);
            assert_eq!(
                fs::metadata(home.path("raw.livox")).unwrap().len(),
                u64::try_from(minimum).unwrap()
            );
            assert_eq!(error["details"]["livox_udp"]["uncommitted_packets"], 1);
        }
        no_control(&source);
    }
}
#[test]
fn count_boundaries_and_original_cutoff_drain_each_accepted_packet_once() {
    for cutoff in [false, true] {
        let home = Home::new("livox-cutoff");
        let source = socket();
        let peer = source.local_addr().unwrap();
        let (capture, target) = Capture::start(
            &home,
            peer,
            "legacy_avia_v5",
            "points",
            None,
            if cutoff {
                json!({"packets_per_group":32,"timeout_ms":time::ms(1500)})
            } else {
                json!({"packets_per_group":2})
            },
        );
        let count = if cutoff { 1 } else { 5 };
        let mut packets = Vec::new();
        for i in 0..count {
            let mut raw = packet("legacy_type_7");
            raw[3] = u8::try_from(i).unwrap();
            source.send_to(&raw, target).unwrap();
            packets.push(raw);
            thread::sleep(Duration::from_millis(3));
        }
        if cutoff {
            thread::sleep(time::millis(1100));
            source.send_to(&packet("legacy_type_7"), target).unwrap();
        }
        assert_eq!(capture.finish().success()["status"], "complete");
        let decoded = archive(
            &home,
            peer,
            target,
            "legacy_avia_v5",
            "points",
            None,
            &packets,
        );
        assert_eq!(decoded.len(), count);
        for (i, value) in decoded.iter().enumerate() {
            assert_eq!(value["reserved_raw"], i);
            assert_eq!(value["native"]["record_count"], 30);
            assert_eq!(value["native"]["returns_per_record"], 3);
        }
        if !cutoff {
            assert!(
                groups(&home)
                    .iter()
                    .all(|row| row["value"]["packet_count"].as_u64().unwrap() <= 2)
            );
        }
        exports(&home, false);
        no_control(&source);
    }
}
