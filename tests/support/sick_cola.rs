//! Independent `CoLa A` wire fixture from `TiM4xx` 8026151/1ONO Table 78.
use super::{Device, Home, accept, assert_unpublished, listener, receive, support::cli::Run};
use crate::support::time;
use serde_json::{Value, json};
use std::{fs, io::Write as _, thread, time::Duration};

const REQUEST: &[u8] = b"\x02sRN LMDscandata\x03";
// Literal telegram data, including the published FFFFFFD3 angle. The manual's
// normative angle table uses 1/10000 degree; no example-unit correction is made.
const VENDOR: &[u8] = b"\x02sRA LMDscandata 1 1 114D32E 0 0 9E9C 9EAF A2574CA2 A2576D22 0 0 0 0 0 5DC A2 0 1 DIST1 3F800000 00000000 FFFFFFD3 D05 10 35F 35A 353 356 34C 350 353 356 348 34A 349 34D 342 34A 34E 33B 1 RSSI1 3F800000 00000000 FFFFFFD3 D05 10 BF BF BC BF BC BF C2 C2 B4 A9 AC AC A3 AC B1 85 0 1 B not defined 0 0 0\x03";

fn record(home: &Home, sources: &Value) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(650),
        "-o",
        "run.jsonl",
    ])
}

fn readings(home: &Home, filename: &str) -> Vec<Value> {
    fs::read_to_string(home.path(filename))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "lidar")
        .collect()
}

#[test]
fn sick_cola_replays_vendor_single_scan_and_preserves_raw_exports_with_simulator() {
    let home = Home::new("sick-cola-vendor");
    let (listener, address) = listener();
    let scanner = Device::start(move |stop| {
        let mut socket = accept(&listener, &stop).unwrap();
        let mut request = vec![0; REQUEST.len()];
        assert!(receive(&mut socket, &mut request, &stop));
        assert_eq!(request, REQUEST);
        for part in VENDOR.chunks(13) {
            socket.write_all(part).unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        // No login, measurement start, subscription, or unrelated command.
        assert!(!receive(&mut socket, &mut [0], &stop));
        1
    });
    let sources = json!([
        {"id":"lidar","protocol":"sick_cola","address":address,"signal":"front_scan","every_ms":3_600_000,"timeout_ms":time::ms(600)},
        {"id":"counter","protocol":"simulator","signal":"count","every_ms":3_600_000}
    ]);
    assert_eq!(record(&home, &sources).success()["samples"], 2);
    assert_eq!(scanner.finish(), 1);
    let rows = readings(&home, "run.jsonl");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["signal"], "front_scan");
    assert!(rows[0].get("source_time").is_none());
    let value = &rows[0]["value"];
    assert_eq!(value["serial_number"], 0x0114_d32e_u32);
    assert_eq!(value["device_status"], json!([0, 0]));
    assert_eq!(value["telegram_counter"], 0x9e9c_u16);
    assert_eq!(value["scan_counter"], 0x9eaf_u16);
    assert_eq!(value["startup_us"], 0xa257_4ca2_u32);
    assert_eq!(value["transmit_us"], 0xa257_6d22_u32);
    assert_eq!(value["scan_frequency_raw"], 1500);
    assert_eq!(value["measurement_frequency_raw"], 162);
    assert_eq!(value["device_name"], "not defined");
    assert_eq!(
        value["distance"],
        json!({"scale_bits":0x3f80_0000_u32,"offset_bits":0,
        "start_angle_raw":-45,"angle_step_raw":3333,
        "values":[863,858,851,854,844,848,851,854,840,842,841,845,834,842,846,827]})
    );
    assert_eq!(
        value["rssi"]["values"],
        json!([
            191, 191, 188, 191, 188, 191, 194, 194, 180, 169, 172, 172, 163, 172, 177, 133
        ])
    );
    assert_eq!(value["rssi"]["start_angle_raw"], -45);
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
    assert_eq!(&readings(&home, "jsonl")[0]["value"], value);
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    assert!(
        csv.contains(&value.to_string().replace('"', "\"\"")),
        "{csv}"
    );
}

#[test]
fn sick_cola_rejects_unsolicited_mismatched_and_partial_scans_atomically() {
    for mode in ["unsolicited", "rssi", "partial"] {
        let home = Home::new(&format!("sick-cola-{mode}"));
        let (listener, address) = listener();
        let scanner = Device::start(move |stop| {
            let mut socket = accept(&listener, &stop).unwrap();
            let mut request = vec![0; REQUEST.len()];
            assert!(receive(&mut socket, &mut request, &stop));
            assert_eq!(request, REQUEST);
            let vendor = std::str::from_utf8(VENDOR).unwrap();
            match mode {
                "unsolicited" => socket
                    .write_all(vendor.replacen("sRA", "sSN", 1).as_bytes())
                    .unwrap(),
                "rssi" => socket
                    .write_all(
                        vendor
                            .replace(
                                "RSSI1 3F800000 00000000 FFFFFFD3",
                                "RSSI1 3F800000 00000000 FFFFFFD4",
                            )
                            .as_bytes(),
                    )
                    .unwrap(),
                "partial" => socket.write_all(&VENDOR[..VENDOR.len() - 1]).unwrap(),
                _ => unreachable!(),
            }
            assert!(!receive(&mut socket, &mut [0], &stop));
            1
        });
        let sources = json!([{"id":"lidar","protocol":"sick_cola","address":address,
            "every_ms":3_600_000,"timeout_ms":time::ms(250)}]);
        let result = record(&home, &sources).failure(if mode == "partial" { 4 } else { 1 });
        assert_eq!(
            result["code"],
            if mode == "partial" {
                "timeout"
            } else {
                "failed"
            }
        );
        scanner.finish();
        assert_unpublished(&home);
    }
}

#[test]
fn sick_cola_requires_a_specific_endpoint_and_rejects_unknown_commands() {
    for extra in [
        json!({"address":"0.0.0.0:2111"}),
        json!({"address":"127.0.0.1:0"}),
        json!({"address":"127.0.0.1:2111","command":"sEN LMDscandata 1"}),
    ] {
        let home = Home::new("sick-cola-config");
        let mut source = extra;
        source["id"] = json!("lidar");
        source["protocol"] = json!("sick_cola");
        let error = record(&home, &json!([source])).failure(2);
        assert_eq!(error["code"], "usage");
    }
}
