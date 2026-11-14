//! Passive GNSS fixtures use complete manufacturer NMEA literals.
use super::{
    Device, Home, accept, assert_unpublished, listener, receive, serial_pair, support::cli::Run,
};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write as _,
    os::fd::AsRawFd as _,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

// NovAtel OEM7 GPGGALONG and GPRMC documentation, D100402 v6A.
const LONG: &[u8] =
    b"$GPGGA,202601.00,5109.0262339,N,11401.8407317,W,5,40,0.5,1097.350,M,-17.00,M,09,TSTR*5F\r\n";
const MODERN: &[u8] =
    b"$GNRMC,204520.00,A,5109.0262239,N,11401.8407338,W,0.004,102.3,130522,0.0,E,D*3B\r\n";
// Trimble OEM GNSS RMC documentation: classic form without optional flags.
const CLASSIC: &[u8] = b"$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A\r\n";

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

fn source(endpoint: &Value) -> Value {
    let mut config = endpoint.clone();
    config["id"] = json!("position");
    config["protocol"] = json!("nmea_gnss");
    config["timeout_ms"] = json!(time::ms(35));
    config["signals"] = json!([{"name":"fix","sentence":"GGA"},{"name":"motion","sentence":"RMC"}]);
    config
}

fn values(home: &Home, filename: &str) -> Vec<Value> {
    fs::read_to_string(home.path(filename))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "position")
        .collect()
}

fn await_terminal(terminal: &File, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + time::secs(3);
    while !stop.load(Ordering::Acquire) {
        let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: live PTY descriptor and writable termios allocation.
        let status = unsafe { libc::tcgetattr(terminal.as_raw_fd(), mode.as_mut_ptr()) };
        assert_eq!(status, 0);
        // SAFETY: successful tcgetattr initialized mode.
        let mode = unsafe { mode.assume_init() };
        // SAFETY: mode is initialized for cfgetispeed.
        let baud = unsafe { libc::cfgetispeed(&raw const mode) };
        if baud == libc::B38400 && mode.c_lflag & (libc::ICANON | libc::ECHO) == 0 {
            assert_eq!(mode.c_cflag & libc::CSIZE, libc::CS8);
            assert_eq!(mode.c_cflag & (libc::PARENB | libc::CSTOPB), 0);
            return true;
        }
        assert!(
            Instant::now() < deadline,
            "recorder did not configure the GNSS PTY"
        );
        thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn nmea_serial_retains_manufacturer_precision_across_idle_reads_and_exports() {
    let home = Home::new("nmea-gnss-serial");
    let (mut master, slave, device) = serial_pair();
    let receiver = Device::start(move |stop| {
        if !await_terminal(&slave, &stop) {
            return 0;
        }
        master
            .write_all(b"tail of a sentence before attachment\r\n")
            .unwrap();
        master.write_all(&LONG[..23]).unwrap();
        thread::sleep(time::millis(95));
        master.write_all(&LONG[23..]).unwrap();
        master.write_all(MODERN).unwrap();
        master.write_all(&LONG[..10]).unwrap();
        assert!(
            !receive(&mut master, &mut [0], &stop),
            "GNSS adapter transmitted a byte"
        );
        2
    });
    let config = source(&json!({"serial":{"device":device,"baud":3.84e4}}));
    let sources = json!([config,{"id":"counter","protocol":"simulator","signal":"count","every_ms":3_600_000}]);
    assert_eq!(record(&home, &sources).success()["samples"], 3);
    assert_eq!(receiver.finish(), 2);
    let rows = values(&home, "run.jsonl");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["signal"], "fix");
    assert_eq!(rows[1]["signal"], "motion");
    let fix = &rows[0]["value"];
    assert_eq!(fix["latitude_dm"], "5109.0262339");
    assert_eq!(fix["longitude_dm"], "11401.8407317");
    assert_eq!(fix["longitude_hemisphere"], "W");
    assert_eq!(fix["quality"], 5);
    assert_eq!(fix["satellites"], 40);
    assert_eq!(fix["altitude"].to_string(), "1097.350");
    assert_eq!(fix["geoid_separation"].to_string(), "-17.00");
    assert_eq!(fix["station_id"], "TSTR");
    assert_eq!(fix["checksum"], "5F");
    assert_eq!(fix["raw"], std::str::from_utf8(LONG).unwrap().trim_end());
    assert_eq!(rows[1]["value"]["talker"], "GN");
    assert_eq!(rows[1]["value"]["date_ddmmyy"], "130522");
    assert_eq!(rows[1]["value"]["mode"], "D");
    assert!(rows[1]["value"].get("navigation_status").is_none());
    for row in &rows {
        assert!(row.get("source_time").is_none());
    }
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
            3
        );
    }
    let exported = values(&home, "jsonl");
    for (before, after) in rows.iter().zip(exported) {
        assert_eq!(before["value"], after["value"]);
    }
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    assert!(csv.contains(&fix.to_string().replace('"', "\"\"")), "{csv}");
}

#[test]
fn nmea_tcp_preserves_classic_rmc_without_transmitting_or_guessing_century() {
    let home = Home::new("nmea-gnss-tcp");
    let (listener, address) = listener();
    let receiver = Device::start(move |stop| {
        let mut socket = accept(&listener, &stop).unwrap();
        socket.write_all(CLASSIC).unwrap();
        assert!(
            !receive(&mut socket, &mut [0], &stop),
            "GNSS adapter transmitted a byte"
        );
        1
    });
    assert_eq!(
        record(&home, &json!([source(&json!({"address":address}))])).success()["samples"],
        1
    );
    receiver.finish();
    let rows = values(&home, "run.jsonl");
    assert_eq!(rows[0]["value"]["date_ddmmyy"], "230394");
    assert_eq!(rows[0]["value"]["longitude_dm"], "01131.000");
    assert_eq!(rows[0]["value"]["speed_knots"].to_string(), "22.4");
    assert_eq!(rows[0]["value"]["magnetic_variation_direction"], "W");
    assert!(rows[0]["value"].get("mode").is_none());
    assert!(rows[0].get("source_time").is_none());
}

#[test]
fn nmea_corrupt_complete_frame_does_not_publish_a_fix() {
    let home = Home::new("nmea-gnss-corrupt");
    let (listener, address) = listener();
    let receiver = Device::start(move |stop| {
        let mut socket = accept(&listener, &stop).unwrap();
        let corrupt = std::str::from_utf8(LONG).unwrap().replace("*5F", "*5E");
        socket.write_all(corrupt.as_bytes()).unwrap();
        assert!(!receive(&mut socket, &mut [0], &stop));
        1
    });
    let error = record(&home, &json!([source(&json!({"address":address}))])).failure(1);
    assert_eq!(error["code"], "failed");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("checksum mismatch")
    );
    receiver.finish();
    assert_unpublished(&home);
}

#[test]
fn nmea_rejects_ambiguous_endpoints_and_duplicate_sentence_maps() {
    for endpoint in [
        json!({"address":"0.0.0.0:5017"}),
        json!({"address":"127.0.0.1:0"}),
        json!({"address":"127.0.0.1:5017","serial":{"device":"unused","baud":38_400}}),
    ] {
        let home = Home::new("nmea-gnss-config");
        assert_eq!(
            record(&home, &json!([source(&endpoint)])).failure(2)["code"],
            "usage"
        );
    }
    let home = Home::new("nmea-gnss-maps");
    let mut config = source(&json!({"address":"127.0.0.1:5017"}));
    config["signals"] = json!([{"name":"a","sentence":"GGA"},{"name":"b","sentence":"GGA"}]);
    let error = record(&home, &json!([config])).failure(2);
    assert!(error["message"].as_str().unwrap().contains("unique"));
}
