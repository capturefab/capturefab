//! Specification-derived `CoLa B` polls, not captured physical-scanner responses.
//! The production unit fixtures retain the unchanged official sSN and bad manual
//! sRA bytes separately; neither is relabeled as a valid captured poll reply.
use super::{Device, Home, accept, listener, receive, support::cli::Run};
use serde_json::{Value, json};
use std::{fs, io::Write as _, thread, time::Duration};

const REQUEST: &[u8] = b"\x02\x02\x02\x02\x00\x00\x00\x0fsRN LMDscandata\x05";

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn record(home: &Home, address: std::net::SocketAddr, profile: &str, every_ms: u64) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[
            {"id":"lidar","protocol":"sick_cola","address":address,"profile":profile,
             "signal":"scan","every_ms":every_ms,"timeout_ms":1000},
            {"id":"counter","protocol":"simulator","signal":"count","every_ms":3_600_000}
        ]})
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

fn rows(home: &Home, file: &str) -> Vec<Value> {
    fs::read_to_string(home.path(file))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "lidar")
        .collect()
}

fn next_request(socket: &mut std::net::TcpStream, stop: &std::sync::atomic::AtomicBool) {
    let mut request = vec![0; REQUEST.len()];
    assert!(receive(socket, &mut request, stop));
    assert_eq!(request, REQUEST);
}

#[test]
fn binary_profiles_keep_all_channels_and_optional_fields_in_mixed_exports() {
    for (profile, fixture, count) in [("cola_b_lms1xx", LMS1, 4), ("cola_b_lms5xx", LMS5, 10)] {
        let home = Home::new(profile);
        let (listener, address) = listener();
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listener, &stop).unwrap();
            next_request(&mut socket, &stop);
            for part in hex(fixture).chunks(7) {
                socket.write_all(part).unwrap();
                thread::sleep(Duration::from_millis(1));
            }
            // No protocol switch, login, start, subscription, or retry command.
            assert!(!receive(&mut socket, &mut [0], &stop));
            1_u64
        });
        assert_eq!(
            record(&home, address, profile, 3_600_000).success()["samples"],
            2
        );
        assert_eq!(scanner.finish(), 1);
        let saved = rows(&home, "run.jsonl");
        let [sample] = saved.as_slice() else {
            panic!("one complete scan")
        };
        assert!(sample.get("source_time").is_none());
        let value = &sample["value"];
        assert_eq!(value["raw_frame_hex"], fixture);
        assert_eq!(value["channels"].as_array().unwrap().len(), count);
        assert_eq!(value["serial_number"], u32::MAX);
        assert_eq!(value["startup_us"], u32::MAX);
        assert_eq!(value["channels"][0]["values"], json!([0, 1, 15, 65535]));
        assert_eq!(value["channels"][0]["scale_bits"], 0x4000_0000_u32);
        assert_eq!(value["channels"][0]["offset_bits"], 0xbf80_0000_u32);
        if profile == "cola_b_lms1xx" {
            assert_eq!(
                value["encoders"][0],
                json!({"position_raw":u32::MAX,"speed_raw":u16::MAX})
            );
            assert_eq!(value["channels"][2]["sample_bits"], 8);
            assert_eq!(value["channels"][2]["values"], json!([0, 1, 255]));
            assert_ne!(
                value["channels"][0]["angle_step_raw"],
                value["channels"][2]["angle_step_raw"]
            );
        } else {
            assert_eq!(value["channels"][9]["name"], "RSSI5");
            assert_eq!(value["channels"][9]["sample_bits"], 16);
            assert_eq!(value["device_name"], "A B  ");
            assert_eq!(value["events"][0]["encoder_position_raw"], u32::MAX);
            assert_eq!(value["calendar_raw"]["microsecond"], 999_999);
            assert_eq!(value["events"].as_array().unwrap().len(), 3);
            assert_eq!(value["encoders"], json!([]));
        }
        for format in ["csv", "jsonl"] {
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
        assert_eq!(rows(&home, "jsonl")[0]["value"], *value);
        let csv = fs::read_to_string(home.path("csv")).unwrap();
        assert!(
            csv.contains(&value.to_string().replace('"', "\"\"")),
            "{csv}"
        );
    }
}

fn checksum(frame: &mut [u8]) {
    let (checksum, frame) = frame.split_last_mut().unwrap();
    *checksum = frame[8..].iter().fold(0, |xor, byte| xor ^ byte);
}

#[test]
fn binary_replies_require_checksum_command_model_and_complete_fields() {
    for mode in [
        "checksum",
        "unsolicited",
        "command",
        "partial",
        "model",
        "length",
    ] {
        let home = Home::new(&format!("cola-b-{mode}"));
        let (listener, address) = listener();
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listener, &stop).unwrap();
            next_request(&mut socket, &stop);
            let mut frame = hex(LMS1);
            match mode {
                "checksum" => *frame.last_mut().unwrap() ^= 1,
                "unsolicited" => {
                    frame[8..11].copy_from_slice(b"sSN");
                    checksum(&mut frame);
                }
                "command" => {
                    frame[12..23].copy_from_slice(b"LMDscandatb");
                    checksum(&mut frame);
                }
                "partial" => frame.truncate(10),
                "length" => {
                    frame[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
                    frame.truncate(8);
                }
                "model" => {}
                _ => unreachable!(),
            }
            socket.write_all(&frame).unwrap();
            assert!(!receive(&mut socket, &mut [0], &stop));
            1_u64
        });
        let profile = if mode == "model" {
            "cola_b_tim5xx"
        } else {
            "cola_b_lms1xx"
        };
        let code = match mode {
            "partial" => 4,
            "model" => 5,
            _ => 1,
        };
        record(&home, address, profile, 3_600_000).failure(code);
        scanner.finish();
        // Simulator may have completed; the failed lidar poll publishes nothing.
        assert_eq!(rows(&home, "run.jsonl"), Vec::<Value>::new());
        let text = fs::read_to_string(home.path("run.jsonl")).unwrap();
        let end: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(end["status"], "failed");
    }
}

#[test]
fn binary_in_flight_cutoff_after_success_retains_only_complete_scan() {
    let home = Home::new("cola-b-cutoff");
    let (listener, address) = listener();
    let scanner = Device::start(move |stop| {
        let mut socket = accept(&listener, &stop).unwrap();
        next_request(&mut socket, &stop);
        socket.write_all(&hex(LMS1)).unwrap();
        next_request(&mut socket, &stop);
        socket.write_all(&hex(LMS1)[..10]).unwrap();
        assert!(!receive(&mut socket, &mut [0], &stop));
        2_u64
    });
    let error = record(&home, address, "cola_b_lms1xx", 100).failure(4);
    assert_eq!(error["code"], "timeout");
    assert_eq!(scanner.finish(), 2);
    let saved = rows(&home, "run.jsonl");
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["value"]["raw_frame_hex"], LMS1);
    let text = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let end: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["status"], "failed");
}

#[test]
fn binary_profiles_are_explicit_and_unknown_profile_is_usage_error() {
    let home = Home::new("cola-b-profile");
    record(
        &home,
        "127.0.0.1:2112".parse().unwrap(),
        "cola_b",
        3_600_000,
    )
    .failure(2);
}

// Exact specification-derived frames: independent of the Rust encoder/decoder.
const LMS1: &str = "02020202000000c0735241204c4d447363616e64617461200001ffffffffffff8001ffff0000ffffffff000000001234abcd0000000009c40000021c0003fffffffffffffffffffefffefffffffdfffd0002444953543140000000bf800000ffff3cb00683000400000001000fffff444953543240000000bf800000ffff3cb10684000400000001000fffff0002525353493140000000bf800000ffff3cba068d00030001ff525353493240000000bf800000ffff3cbb068e00030001ff000000000000000000003e";
const LMS5: &str = "02020202000001a8735241204c4d447363616e64617461200001ffffffffffff8001ffff0000ffffffff000000001234abcd0000000009c40000021c0000000a444953543140000000bf800000ffff3cb00683000400000001000fffff444953543240000000bf800000ffff3cb10684000400000001000fffff444953543340000000bf800000ffff3cb20685000400000001000fffff444953543440000000bf800000ffff3cb30686000400000001000fffff444953543540000000bf800000ffff3cb40687000400000001000fffff525353493140000000bf800000ffff3cb50688000400000001000fffff525353493240000000bf800000ffff3cb60689000400000001000fffff525353493340000000bf800000ffff3cb7068a000400000001000fffff525353493440000000bf800000ffff3cb8068b000400000001000fffff525353493540000000bf800000ffff3cb9068c000400000001000fffff000000000001000541204220200000000107ea0b0a173b3a000f423f00034644494effffffffffffffff0036ee804644494efffffffefffffffe0036ee7f4644494efffffffdfffffffd0036ee7edd";
