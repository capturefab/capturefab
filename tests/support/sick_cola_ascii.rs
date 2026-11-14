//! Explicit LMS `CoLa A` profiles: published reply replay and specification-derived
//! multi-channel/optional-field cases. No physical-scanner qualification.
use super::{Device, Home, accept, listener, receive, support::cli::Run};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    fs,
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpStream},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

const REQUEST: &[u8] = b"\x02sRN LMDscandata\x03";
const PUBLISHED: &[u8] = include_bytes!("../fixtures/sick/lms-published-sra.bin");
const HEADER: &str =
    "sRA LMDscandata 1 FFFF FFFFFFFF FF 80 FFFF 0 FFFFFFFF 0 12 34 AB CD 0 9C4 21C0";
const DIST: &str = "DIST1 40000000 BF800000 FFFF3CB0 683 4 0 1 F FFFF";

fn hex(raw: &[u8]) -> String {
    let mut text = String::with_capacity(raw.len() * 2);
    for byte in raw {
        write!(text, "{byte:02x}").unwrap();
    }
    text
}
fn frame(payload: &str) -> Vec<u8> {
    format!("\x02{payload}\x03").into_bytes()
}
fn rows(home: &Home, name: &str) -> Vec<Value> {
    fs::read_to_string(home.path(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "lidar")
        .collect()
}
fn record(home: &Home, address: SocketAddr, profile: &str, options: Value) -> Run {
    let mut source = json!({"id":"lidar","protocol":"sick_cola","address":address,
        "profile":profile,"every_ms":3_600_000,"timeout_ms":1000});
    let Value::Object(options) = options else {
        panic!("fixture options")
    };
    for (key, value) in options {
        source[key] = value;
    }
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source,
        {"id":"counter","protocol":"simulator","signal":"count","every_ms":3_600_000}]})
        .to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "650ms",
        "-o",
        "run.jsonl",
    ])
}
fn request(socket: &mut TcpStream, stop: &AtomicBool) {
    let mut raw = [0; REQUEST.len()];
    assert!(receive(socket, &mut raw, stop));
    assert_eq!(raw.as_slice(), REQUEST);
}
fn exports(home: &Home, expected: &Value) {
    for format in ["jsonl", "csv"] {
        assert_eq!(
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "-o",
                format,
                "--format",
                format
            ])["samples"],
            2
        );
    }
    let exported = rows(home, "jsonl");
    assert_eq!(exported.len(), 1);
    assert_eq!(&exported[0]["value"], expected);
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let all = lines.collect::<Vec<_>>();
    assert_eq!(all.len(), 2);
    let values = all
        .iter()
        .filter(|line| line.split(',').nth(1) == Some("\"lidar\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    // The eight preceding fixture fields may be quoted but contain no embedded
    // commas, quotes or newlines; the final JSON object is one RFC4180 quoted
    // field, with doubled quotes.
    let quoted = format!("\"{}\"", expected.to_string().replace('"', "\"\""));
    assert_eq!(values, [quoted.as_str()]);
}

#[test]
fn ascii_lms_published_transcript_is_exact_in_both_profiles_and_mixed_exports() {
    for profile in ["cola_a_lms1xx", "cola_a_lms5xx"] {
        let home = Home::new(profile);
        let (listen, address) = listener();
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listen, &stop).unwrap();
            socket.set_nodelay(true).unwrap();
            request(&mut socket, &stop);
            for chunk in PUBLISHED.chunks(3) {
                socket.write_all(chunk).unwrap();
            }
            assert!(!receive(&mut socket, &mut [0], &stop));
            1_u64
        });
        assert_eq!(
            record(
                &home,
                address,
                profile,
                json!({"max_frame_bytes":215.0,"max_points":2.1e1})
            )
            .success()["samples"],
            2
        );
        assert_eq!(scanner.finish(), 1);
        let saved = rows(&home, "run.jsonl");
        let [sample] = saved.as_slice() else {
            panic!("one complete scan")
        };
        assert!(sample.get("source_time").is_none());
        let value = &sample["value"];
        assert_eq!(value["encoding"], "cola_a");
        assert_eq!(
            value["model_profile"],
            profile.trim_start_matches("cola_a_")
        );
        assert_eq!(value["raw_frame_hex"], hex(PUBLISHED));
        assert_eq!(
            value["channels"][0]["values"],
            json!([
                2209, 2213, 2219, 2220, 2214, 2220, 2230, 2248, 2242, 2249, 2251, 2244, 2276, 2273,
                2283, 2272, 2293, 2312, 2300, 2311, 2310
            ])
        );
        assert!(value.get("distance").is_none());
        assert!(value.get("calendar_raw").is_none());
        exports(&home, value);
    }
}

#[test]
fn ascii_lms_model_channels_encoders_and_pro_events_preserve_native_fields() {
    let lms1 = format!(
        "{HEADER} 3 FFFFFFFF FFFF FFFFFFFE FFFE FFFFFFFD FFFD 3 {DIST} DIST2 3F800000 0 FFFF3CB1 684 1 FFFF RSSI2 7FC12345 80000000 80000000 FFFF 2 0 FFFF 1 RSSI1 3F800000 0 FFFF3CBA 68D 3 0 1 FF 0 1 5 A B   0 1 7F5 0 0 17 3B 3B F423F 0"
    );
    let lms5 = format!(
        "{} 0 A {} 0 0 1 5 A B   0 1 7EA B A 17 3B 3A F423F 3 FDIN FFFFFFFF FFFFFFFF 36EE80 FDIN FFFFFFFE FFFFFFFE 36EE7F FDIN FFFFFFFD FFFFFFFD 36EE7E",
        HEADER.replacen("scandata 1 ", "scandata 0 ", 1),
        (1..=5)
            .map(|n| format!("DIST{n} 40000000 BF800000 FFFF3CB0 683 4 0 1 F FFFF"))
            .chain((1..=5).map(|n| format!("RSSI{n} 7FC12345 80000000 80000000 FFFF 2 0 FFFF")))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for (profile, payload, count) in [("cola_a_lms1xx", lms1, 4), ("cola_a_lms5xx", lms5, 10)] {
        let home = Home::new(profile);
        let (listen, address) = listener();
        let raw = frame(&payload);
        let sent = raw.clone();
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listen, &stop).unwrap();
            socket.set_nodelay(true).unwrap();
            request(&mut socket, &stop);
            for chunk in sent.chunks(7) {
                socket.write_all(chunk).unwrap();
            }
            assert!(!receive(&mut socket, &mut [0], &stop));
            1_u64
        });
        assert_eq!(
            record(&home, address, profile, json!({})).success()["samples"],
            2
        );
        assert_eq!(scanner.finish(), 1);
        let saved = rows(&home, "run.jsonl");
        let [sample] = saved.as_slice() else {
            panic!("one complete scan")
        };
        let value = &sample["value"];
        assert_eq!(value["channels"].as_array().unwrap().len(), count);
        assert_eq!(value["serial_number"], u32::MAX);
        assert_eq!(value["startup_us"], u32::MAX);
        assert_eq!(value["channels"][0]["values"], json!([0, 1, 15, 65_535]));
        assert_eq!(value["channels"][0]["offset_bits"], 0xbf80_0000_u32);
        assert_eq!(value["device_name"], "A B  ");
        assert_eq!(value["calendar_raw"]["microsecond"], 999_999);
        if profile == "cola_a_lms1xx" {
            assert_eq!(
                value["encoders"][2],
                json!({"position_raw":4_294_967_293_u32,"speed_raw":65_533})
            );
            assert_eq!(value["channels"][3]["sample_bits"], 8);
            assert_eq!(value["channels"][3]["values"], json!([0, 1, 255]));
        } else {
            assert_eq!(value["version"], 0);
            assert_eq!(value["encoders"], json!([]));
            assert_eq!(value["channels"][9]["name"], "RSSI5");
            assert_eq!(value["channels"][9]["sample_bits"], 16);
            assert_eq!(value["events"].as_array().unwrap().len(), 3);
            assert_eq!(
                value["events"][0],
                json!({"type":"FDIN","encoder_position_raw":u32::MAX,"startup_us":u32::MAX,"angle_raw":3_600_000})
            );
        }
        assert_eq!(value["raw_frame_hex"], hex(&raw));
        exports(&home, value);
    }
}

#[test]
fn ascii_lms_malformed_late_sibling_and_tail_never_publish_partial_scan() {
    let base = format!("{HEADER} 0 1 {DIST} 0 0 0 0 0 0");
    for (payload, status, message) in [
        (base.replacen("scandata 1 ", "scandata 0 ", 1), 5, "version"),
        (
            format!("{HEADER} 0 2 {DIST} DIST2 3F800000 0 0 1 1 10000 0 0 0 0 0 0"),
            1,
            "wire type",
        ),
        (
            format!("{HEADER} 0 1 {DIST} 1 RSSI1 3F800000 0 0 1 1 100 0 0 0 0 0"),
            1,
            "wire type",
        ),
        (base.replace("DIST1", "DIST3"), 5, "echo"),
        (
            base.replace(" 0 0 0 0 0 0", " 0 0 1 3 AB 0 0 0"),
            1,
            "device name",
        ),
        (
            base.replace(" 0 0 0 0 0 0", " 0 0 0 0 1 7E8 1 1 0 0 0 F4240 0"),
            1,
            "calendar",
        ),
        (base.replacen("sRA", "sSN", 1), 1, "sRA"),
        (format!("{base} 0"), 1, "unexpected fields"),
        (format!("{base}\x03X"), 1, "extra bytes"),
    ] {
        let home = Home::new("lms-ascii-rejected");
        let (listen, address) = listener();
        let raw = frame(&payload);
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listen, &stop).unwrap();
            request(&mut socket, &stop);
            socket.write_all(&raw).unwrap();
            assert!(!receive(&mut socket, &mut [0], &stop));
            1_u64
        });
        let error = record(&home, address, "cola_a_lms1xx", json!({})).failure(status);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert_eq!(scanner.finish(), 1);
        assert_eq!(rows(&home, "run.jsonl"), [] as [Value; 0]);
        let text = fs::read_to_string(home.path("run.jsonl")).unwrap();
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["status"], "failed");
    }
}

#[test]
fn ascii_lms_high_resolution_scan_exceeds_legacy_limit_with_exact_native_arrays() {
    let points = 4561;
    let channel = (1..=5)
        .map(|echo| {
            format!(
                "DIST{echo} 3F800000 0 FFFF3CB0 1A1 11D1 {}",
                std::iter::repeat_n("FFFF", points)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let raw = frame(&format!("{HEADER} 0 5 {channel} 0 0 0 0 0 0"));
    assert!(raw.len() > 65_536 && raw.len() < 128_000);
    for (options, success) in [
        (json!({"max_frame_bytes":1.28e5,"max_points":4.561e3}), true),
        (json!({"max_frame_bytes":70_000}), false),
        (json!({"max_points":4560}), false),
    ] {
        let home = Home::new("lms-ascii-large");
        let (listen, address) = listener();
        let sent = raw.clone();
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listen, &stop).unwrap();
            socket.set_nodelay(true).unwrap();
            request(&mut socket, &stop);
            for chunk in sent.chunks(4096) {
                if socket.write_all(chunk).is_err() {
                    break;
                }
            }
            // Cap rejection can close with unread inbound bytes, producing RST.
            // Success must retire cleanly; neither path may send another query.
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            match socket.read(&mut [0]) {
                Ok(0) => {}
                Err(error)
                    if !success
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) => {}
                other => panic!("expected scanner retirement, got {other:?}"),
            }
            1_u64
        });
        let run = record(&home, address, "cola_a_lms5xx", options);
        if success {
            assert_eq!(run.success()["samples"], 2);
            let saved = rows(&home, "run.jsonl");
            let [sample] = saved.as_slice() else {
                panic!("one large scan")
            };
            let channels = sample["value"]["channels"].as_array().unwrap();
            assert_eq!(channels.len(), 5);
            for (number, channel) in channels.iter().enumerate() {
                assert_eq!(channel["name"], format!("DIST{}", number + 1));
                assert_eq!(channel["values"], json!(vec![u16::MAX; points]));
            }
            exports(&home, &sample["value"]);
        } else {
            let error = run.failure(1);
            assert!(
                error["message"]
                    .as_str()
                    .unwrap()
                    .contains("configured max_"),
                "{error}"
            );
            assert_eq!(rows(&home, "run.jsonl"), [] as [Value; 0]);
        }
        assert_eq!(scanner.finish(), 1);
    }
}

#[test]
fn ascii_lms_original_poll_deadline_includes_late_final_tail_and_retires_socket() {
    let home = Home::new("lms-ascii-deadline");
    let (listen, address) = listener();
    let scanner = Device::start(move |stop| {
        let mut socket = accept(&listen, &stop).unwrap();
        request(&mut socket, &stop);
        socket.write_all(&PUBLISHED[..PUBLISHED.len() - 1]).unwrap();
        thread::sleep(Duration::from_millis(180));
        let _ = socket.write_all(&[3]);
        assert!(!receive(&mut socket, &mut [0], &stop));
        1_u64
    });
    let error = record(&home, address, "cola_a_lms1xx", json!({"timeout_ms":80})).failure(4);
    assert_eq!(error["code"], "timeout");
    assert_eq!(scanner.finish(), 1);
    assert_eq!(rows(&home, "run.jsonl"), [] as [Value; 0]);
}

#[test]
fn ascii_lms_profiles_reject_bad_exact_limits_before_connecting() {
    for (field, value) in [
        ("max_points", json!(0)),
        ("max_points", json!(65_536)),
        ("max_frame_bytes", json!(127)),
        ("max_frame_bytes", json!(262_145)),
        ("max_points", json!(1.1)),
        ("profile", json!("cola_a_lms9xx")),
    ] {
        let home = Home::new("lms-ascii-config");
        let mut source = json!({"version":1,"sources":[
            {"id":"lidar","protocol":"sick_cola","address":"127.0.0.1:1","profile":"cola_a_lms5xx"}]});
        source["sources"][0][field] = value;
        fs::write(home.path("sources.json"), source.to_string()).unwrap();
        let error = home
            .run(&[
                "--json",
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "1ms",
                "-o",
                "never.jsonl",
            ])
            .failure(2);
        assert_eq!(error["code"], "usage");
        assert!(!home.path("never.jsonl").exists());
    }
}
