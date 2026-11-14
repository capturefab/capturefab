//! PTY verification for the Vaisala WXT530 guide's ASCII composite query.
use super::{Device, Home, assert_unpublished, receive, serial_pair, support::cli::Run};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write as _,
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

const PLAIN: &[u8] =
    b"0R0,Dx=005D,Sx=2.8M,Ta=23.0C,Ua=30.0P,Pa=1028.2H, Rc=0.00M,Rd=10s,Th=23.6C\r\n";
// Synthetic R0 field mixture with an independently calculated CRC. The guide
// publishes the `10.6#` unavailable heating field in its r5 CRC example.
const CHECKED: &[u8] = b"0r0,Dx=005D,Sm=#,Ta=-02.50C,Pa=1028.2H,Vh=10.6#,Id=HEL/___CHT\r\n";

fn expect_query(port: &mut File, stop: &AtomicBool, crc: bool) {
    let expected: &[u8] = if crc { b"0r0Kld\r\n" } else { b"0R0\r\n" };
    let mut request = vec![0; expected.len()];
    assert!(receive(port, &mut request, stop));
    assert_eq!(request, expected);
}

fn source(device: &str, crc: bool, signals: &Value) -> Value {
    json!({"id":"weather","protocol":"vaisala_wxt","device":device,"baud":1.92e4,
        "crc":crc,"every_ms":3_600_000,"timeout_ms":time::ms(900),"signals":signals})
}

fn record(home: &Home, config: &Value) -> Run {
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
        &time::arg(650),
        "-o",
        "run.jsonl",
    ])
}

fn values(home: &Home, path: &str) -> Vec<Value> {
    fs::read_to_string(home.path(path))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|mut row| row.as_object_mut().unwrap().remove("value"))
        .collect()
}

#[test]
fn wxt_replays_vaisala_literal_composite_and_exports_exact_fields() {
    let home = Home::new("wxt-r0");
    let (mut master, slave, device) = serial_pair();
    let weather = Device::start(move |stop| {
        let _slave = slave;
        expect_query(&mut master, &stop, false);
        for part in PLAIN.chunks(12) {
            master.write_all(part).unwrap();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let config = source(
        &device,
        false,
        &json!([
            {"name":"direction","field":"Dx"}, {"name":"rain","field":"Rc"},
            {"name":"duration","field":"Rd"}
        ]),
    );
    assert_eq!(record(&home, &config).success()["samples"], 3);
    assert_eq!(weather.finish(), 1);
    let original = values(&home, "run.jsonl");
    assert_eq!(
        original[0],
        json!({"field":"Dx","raw":"005D","value":5,"unit_code":"D"})
    );
    assert_eq!(original[1]["value"].to_string(), "0.00");
    assert_eq!(original[1]["raw"], "0.00M");
    assert_eq!(
        original[2],
        json!({"field":"Rd","raw":"10s","value":10,"unit_code":"s"})
    );
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
            3
        );
    }
    assert_eq!(values(&home, "jsonl"), original);
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    assert!(csv.contains(r#"""value"":0.00"#));
    assert!(csv.contains(r#"""raw"":""005D"""#));
}

#[test]
fn wxt_crc_preserves_invalid_status_signed_decimal_and_information_field() {
    let home = Home::new("wxt-crc");
    let (mut master, slave, device) = serial_pair();
    let weather = Device::start(move |stop| {
        let _slave = slave;
        expect_query(&mut master, &stop, true);
        master.write_all(CHECKED).unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let config = source(
        &device,
        true,
        &json!([
            {"name":"temperature","field":"Ta"}, {"name":"heating","field":"Vh"},
            {"name":"wind","field":"Sm"}, {"name":"information","field":"Id"}
        ]),
    );
    assert_eq!(record(&home, &config).success()["samples"], 4);
    assert_eq!(weather.finish(), 1);
    let original = values(&home, "run.jsonl");
    assert_eq!(original[0]["raw"], "-02.50C");
    assert_eq!(original[0]["value"].to_string(), "-2.50");
    assert_eq!(
        original[1],
        json!({"field":"Vh","raw":"10.6#","status":"invalid"})
    );
    assert_eq!(
        original[2],
        json!({"field":"Sm","raw":"#","status":"invalid"})
    );
    assert_eq!(
        original[3],
        json!({"field":"Id","raw":"HEL/___","value":"HEL/___"})
    );
}

#[test]
fn wxt_rejects_uncorrelated_corrupt_and_incomplete_maps_atomically() {
    for (reply, crc, message) in [
        (
            &b"1R0,Dx=005D,Ta=23.0C\r\n"[..],
            false,
            "address or command",
        ),
        (
            b"0r0,Dx=005D,Sm=#,Ta=-02.50C,Pa=1028.2H,Vh=10.6#,Id=HEL/___CHU\r\n",
            true,
            "CRC mismatch",
        ),
        (b"0R0,Dx=005D,Ta=23.0C,Ta=24.0C\r\n", false, "duplicate"),
        (b"0R0,Dx=005D\r\n", false, "missing configured field Ta"),
        (
            b"0R0,Dx=005D,Ta=23.0M\r\n",
            false,
            "unexpected suffix for Ta",
        ),
        (b"0R0,Dx=005D,Ta=23.0C\n", false, "must end with CRLF"),
    ] {
        let home = Home::new("wxt-invalid");
        let (mut master, slave, device) = serial_pair();
        let weather = Device::start(move |stop| {
            let _slave = slave;
            expect_query(&mut master, &stop, crc);
            master.write_all(reply).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            1
        });
        let config = source(
            &device,
            crc,
            &json!([
                {"name":"direction","field":"Dx"}, {"name":"temperature","field":"Ta"}
            ]),
        );
        let error = record(&home, &config).failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert_eq!(weather.finish(), 1);
        assert_unpublished(&home);
    }
}

#[test]
fn wxt_late_terminator_expires_the_whole_poll() {
    let home = Home::new("wxt-late");
    let (mut master, slave, device) = serial_pair();
    let weather = Device::start(move |stop| {
        let _slave = slave;
        expect_query(&mut master, &stop, false);
        thread::sleep(time::millis(125));
        master.write_all(b"0R0,Dx=005D\r").unwrap();
        thread::sleep(time::millis(125));
        master.write_all(b"\n").unwrap();
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let mut config = source(&device, false, &json!([{"name":"direction","field":"Dx"}]));
    config["timeout_ms"] = json!(time::ms(200));
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    assert_eq!(weather.finish(), 1);
    assert_unpublished(&home);
}
