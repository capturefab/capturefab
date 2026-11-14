//! Transparent Infwin-style SDI-12 through the CLI. The PTY checks the USB
//! serial bytes only. Published CRC and sequencing vectors are from the
//! public-domain SDI-12 v1.4 specification §4.4.12.3.

use super::{Device, Home, assert_unpublished, receive, serial_pair, support::cli::Run};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write as _,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

const FIRST: &[u8] = b"0+1.11+2.22+3.33+4.44+5.55+6.66I]q\r\n";
const LAST: &[u8] = b"0+7.77+8.88+9.99IvW\r\n";

fn expect(port: &mut File, stop: &AtomicBool, expected: &[u8]) {
    let mut actual = vec![0; expected.len()];
    assert!(receive(port, &mut actual, stop));
    assert_eq!(actual, expected);
}

fn source(device: &str, signals: &Value) -> Value {
    json!({
        "id":"soil","protocol":"sdi12_serial","device":device,"address":"0","crc":true,
        "every_ms":3_600_000,"timeout_ms":1000,"signals":signals
    })
}

fn record(home: &Home, config: &Value, duration: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[config]}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        duration,
        "-o",
        "run.jsonl",
    ])
}

fn bus(device: &str, sensors: &Value) -> Value {
    json!({
        "id":"soil","protocol":"sdi12_serial","device":device,
        "every_ms":3_600_000,"timeout_ms":1000,"sensors":sensors
    })
}

#[test]
fn shared_bridge_completes_one_measurement_before_addressing_the_next() {
    let home = Home::new("sdi12-bus");
    let (mut master, slave, device) = serial_pair();
    let worker = Device::start(move |stop| {
        let _slave = slave;
        expect(&mut master, &stop, b"0MC!");
        master.write_all(b"00359\r\n0\r\n").unwrap();
        expect(&mut master, &stop, b"0D0!");
        master.write_all(FIRST).unwrap();
        expect(&mut master, &stop, b"0D1!");
        master.write_all(LAST).unwrap();

        // Campbell's CS650/CS655 Example 2 uses numbered M4 on each
        // address. Here the second sensor returns its six-value mapping.
        expect(&mut master, &stop, b"AM4!");
        master.write_all(b"A0006\r\n").unwrap();
        expect(&mut master, &stop, b"AD0!");
        master
            .write_all(b"A+0.250+0.1+23.2+4.5+6.7+0.89\r\n")
            .unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        2
    });
    let config = bus(
        &device,
        &json!([
            {"address":"0","crc":true,"signals":[{"name":"first","index":0}]},
            {"address":"A","measurement":4.0,"signals":[{"name":"second","index":0}]}
        ]),
    );
    assert_eq!(record(&home, &config, "1s").success()["samples"], 2);
    assert_eq!(worker.finish(), 2);
    let output = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[1]["signal"], "first");
    assert_eq!(rows[1]["value"], json!({"raw":"+1.11","value":1.11}));
    assert_eq!(rows[2]["signal"], "second");
    assert_eq!(rows[2]["value"]["raw"], "+0.250");
    assert_eq!(rows[2]["value"]["value"].to_string(), "0.250");
    assert_eq!(rows[1]["elapsed_ns"], rows[2]["elapsed_ns"]);
}

#[test]
fn shared_bridge_does_not_publish_an_earlier_address_when_a_sibling_fails() {
    for (late, code, message) in [(false, 1, "address differs"), (true, 4, "deadline expired")] {
        let home = Home::new("sdi12-bus-failure");
        let (mut master, slave, device) = serial_pair();
        let worker = Device::start(move |stop| {
            let _slave = slave;
            expect(&mut master, &stop, b"0M!");
            master.write_all(b"00001\r\n").unwrap();
            expect(&mut master, &stop, b"0D0!");
            if late {
                thread::sleep(Duration::from_millis(175));
            }
            master.write_all(b"0+1.25\r\n").unwrap();
            expect(&mut master, &stop, b"AMC!");
            master.write_all(b"A0001\r\n").unwrap();
            expect(&mut master, &stop, b"AD0!");
            if late {
                thread::sleep(Duration::from_millis(230));
                master.write_all(b"A+2.50NIR\r\n").unwrap();
            } else {
                master.write_all(b"0+2.50@MX\r\n").unwrap();
            }
            assert!(!receive(&mut master, &mut [0], &stop));
            2
        });
        let mut config = bus(
            &device,
            &json!([
                {"address":"0","signals":[{"name":"first","index":0}]},
                {"address":"A","crc":true,"signals":[{"name":"second","index":0}]}
            ]),
        );
        config["timeout_ms"] = json!(340);
        let error = record(&home, &config, "1s").failure(code);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert_eq!(worker.finish(), 2);
        assert_unpublished(&home);
    }
}

#[test]
fn shared_bridge_rejects_ambiguous_config_before_opening_the_terminal() {
    let home = Home::new("sdi12-bus-config");
    let first = json!({"address":"0","signals":[{"name":"first","index":0}]});
    let second = json!({"address":"A","signals":[{"name":"second","index":0}]});
    let device = "unused";
    let mut mixed = bus(device, &json!([first, second]));
    mixed["address"] = json!("A");
    let mut repeated = second;
    repeated["signals"][0]["name"] = json!("first");
    for config in [
        mixed,
        bus(device, &json!([first, first])),
        bus(device, &json!([first, repeated])),
        bus(device, &json!([])),
    ] {
        let error = record(&home, &config, "1s").failure(2);
        assert!(
            error["message"].as_str().unwrap().contains("SDI-12")
                || error["message"].as_str().unwrap().contains("unknown field"),
            "{error}"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn reads_published_mc_service_and_data_sequence_with_exact_exports() {
    let home = Home::new("sdi12-published");
    let (mut master, _slave, device) = serial_pair();
    let sensor = Device::start(move |stop| {
        expect(&mut master, &stop, b"0MC!");
        // The bridge can deliver the acknowledgement and the readiness line
        // in one USB read, without an intervening read from the recorder.
        master.write_all(b"00359\r\n0\r\n").unwrap();
        expect(&mut master, &stop, b"0D0!");
        for part in FIRST.chunks(7) {
            master.write_all(part).unwrap();
            thread::sleep(Duration::from_millis(2));
        }
        expect(&mut master, &stop, b"0D1!");
        master.write_all(LAST).unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        2
    });
    let config = source(
        &device,
        &json!([
            {"name":"first","index":0},
            {"name":"last","index":8}
        ]),
    );
    assert_eq!(record(&home, &config, "900ms").success()["samples"], 2);
    assert_eq!(sensor.finish(), 2);
    let recorded = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = recorded
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["sources"][0]["protocol"], "sdi12_serial");
    assert_eq!(rows[1]["value"], json!({"raw":"+1.11","value":1.11}));
    assert_eq!(rows[2]["value"], json!({"raw":"+9.99","value":9.99}));
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
        let output = fs::read_to_string(home.path(format)).unwrap();
        assert!(output.contains("+1.11"));
        assert!(output.contains("+9.99"));
    }
}

#[test]
fn waits_until_ttt_before_requesting_data_without_a_service_line() {
    let home = Home::new("sdi12-ttt");
    let (mut master, _slave, device) = serial_pair();
    let sensor = Device::start(move |stop| {
        expect(&mut master, &stop, b"0MC!");
        let started = Instant::now();
        master.write_all(b"00012\r\n").unwrap();
        expect(&mut master, &stop, b"0D0!");
        assert!(started.elapsed() >= Duration::from_secs(1));
        master.write_all(b"0+3.14+2.718IWO\r\n").unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let mut config = source(&device, &json!([{"name":"second","index":1}]));
    config["timeout_ms"] = json!(1600);
    assert_eq!(record(&home, &config, "1800ms").success()["samples"], 1);
    assert_eq!(sensor.finish(), 1);
}

#[test]
fn numbered_non_crc_measurement_keeps_the_raw_decimal_text() {
    let home = Home::new("sdi12-numbered");
    let (mut master, _slave, device) = serial_pair();
    let sensor = Device::start(move |stop| {
        expect(&mut master, &stop, b"AM2!");
        master.write_all(b"A0002\r\n").unwrap();
        expect(&mut master, &stop, b"AD0!");
        master.write_all(b"A+000.2500-.0123\r\n").unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let mut config = source(
        &device,
        &json!([{"name":"value","index":0},{"name":"negative","index":1}]),
    );
    config["address"] = json!("A");
    config["measurement"] = json!(2);
    config["crc"] = json!(false);
    assert_eq!(record(&home, &config, "800ms").success()["samples"], 2);
    assert_eq!(sensor.finish(), 1);
    let output = fs::read_to_string(home.path("run.jsonl")).unwrap();
    assert!(output.contains(r#""raw":"+000.2500","value":0.2500"#));
    assert!(output.contains(r#""raw":"-.0123","value":-0.0123"#));
}

#[test]
fn rejects_wrong_station_crc_or_a_late_second_data_response_atomically() {
    for (case, code, message) in [
        ("station", 1, "address differs"),
        ("crc", 1, "CRC mismatch"),
        ("timeout", 4, "deadline expired"),
    ] {
        let home = Home::new("sdi12-invalid");
        let (mut master, _slave, device) = serial_pair();
        let sensor = Device::start(move |stop| {
            expect(&mut master, &stop, b"0MC!");
            master.write_all(b"00359\r\n0\r\n").unwrap();
            expect(&mut master, &stop, b"0D0!");
            if case == "timeout" {
                thread::sleep(Duration::from_millis(120));
            }
            master.write_all(FIRST).unwrap();
            expect(&mut master, &stop, b"0D1!");
            match case {
                "station" => master.write_all(b"1+7.77+8.88+9.99IvW\r\n").unwrap(),
                "crc" => master.write_all(b"0+7.77+8.88+9.99IvX\r\n").unwrap(),
                "timeout" => {
                    // A per-request timer would incorrectly allow this
                    // complete line. The whole measurement expires first.
                    thread::sleep(Duration::from_millis(260));
                    master.write_all(LAST).unwrap();
                }
                _ => unreachable!(),
            }
            // Keep the PTY alive until the recorder has handled the reply.
            assert!(!receive(&mut master, &mut [0], &stop));
            2
        });
        let mut config = source(&device, &json!([{"name":"first","index":0}]));
        config["timeout_ms"] = json!(340);
        let error = record(&home, &config, "1s").failure(code);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{case}: {error}"
        );
        assert_eq!(sensor.finish(), 2);
        assert_unpublished(&home);
    }
}
