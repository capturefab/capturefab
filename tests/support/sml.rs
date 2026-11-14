//! Passive SML on a Unix PTY using an original BSI/FNN-format push frame.

use super::{Device, Home, assert_unpublished, receive, serial_pair, support::cli::Run};
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

// Original fixture. X.25 values were computed over the unescaped message
// bodies and, independently, the escaped wire data through the padding count.
// Four payload escape bytes are doubled at unaligned wire offset 133.
const FRAME: [u8; 196] = [
    0x1b, 0x1b, 0x1b, 0x1b, 0x01, 0x01, 0x01, 0x01, 0x76, 0x02, 0x01, 0x62, 0x00, 0x62, 0x00, 0x72,
    0x65, 0x00, 0x00, 0x01, 0x01, 0x76, 0x01, 0x01, 0x02, 0x52, 0x09, 0x4c, 0x41, 0x42, 0x4d, 0x45,
    0x54, 0x45, 0x52, 0x01, 0x01, 0x63, 0x5a, 0x0e, 0x00, 0x76, 0x02, 0x02, 0x62, 0x00, 0x62, 0x00,
    0x72, 0x65, 0x00, 0x00, 0x07, 0x01, 0x77, 0x01, 0x09, 0x4c, 0x41, 0x42, 0x4d, 0x45, 0x54, 0x45,
    0x52, 0x01, 0x01, 0x74, 0x77, 0x07, 0x01, 0x00, 0x01, 0x08, 0x00, 0xff, 0x69, 0x01, 0x23, 0x45,
    0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x62, 0x1e, 0x52, 0xfd, 0x69, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0x01, 0x77, 0x07, 0x01, 0x00, 0x10, 0x07, 0x00, 0xff, 0x01, 0x01, 0x62, 0x1b,
    0x52, 0xff, 0x54, 0xff, 0x7f, 0xff, 0x01, 0x77, 0x07, 0x81, 0x81, 0xc7, 0x82, 0x03, 0xff, 0x01,
    0x01, 0x01, 0x01, 0x09, 0xde, 0x1b, 0x1b, 0x1b, 0x1b, 0x1b, 0x1b, 0x1b, 0x1b, 0x1b, 0x1a, 0xbe,
    0x01, 0x77, 0x07, 0x01, 0x00, 0x02, 0x08, 0x00, 0xff, 0x01, 0x01, 0x01, 0x01, 0x62, 0x00, 0x01,
    0x01, 0x01, 0x63, 0xe1, 0xe5, 0x00, 0x76, 0x02, 0x03, 0x62, 0x00, 0x62, 0x00, 0x72, 0x65, 0x00,
    0x00, 0x02, 0x01, 0x71, 0x01, 0x63, 0x28, 0x94, 0x00, 0x00, 0x00, 0x00, 0x1b, 0x1b, 0x1b, 0x1b,
    0x1a, 0x03, 0x4e, 0xf4,
];

fn await_terminal(terminal: &File, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + time::secs(3);
    while !stop.load(Ordering::Acquire) {
        let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: live PTY descriptor and writable termios output allocation.
        let result = unsafe { libc::tcgetattr(terminal.as_raw_fd(), mode.as_mut_ptr()) };
        assert_eq!(result, 0);
        // SAFETY: tcgetattr succeeded.
        let mode = unsafe { mode.assume_init() };
        // SAFETY: mode remains initialized through cfgetispeed.
        let baud = unsafe { libc::cfgetispeed(&raw const mode) };
        if baud == libc::B9600 && mode.c_lflag & (libc::ICANON | libc::ECHO) == 0 {
            assert_eq!(mode.c_cflag & libc::CSIZE, libc::CS8);
            assert_eq!(mode.c_cflag & (libc::PARENB | libc::CSTOPB), 0);
            return true;
        }
        assert!(
            Instant::now() < deadline,
            "recorder did not configure the PTY"
        );
        thread::sleep(Duration::from_millis(2));
    }
    false
}

fn source(device: &str) -> Value {
    json!({
        "id":"meter","protocol":"sml_serial","device":device,"timeout_ms":time::ms(50),
        "signals":[
            {"name":"energy","obis":"1-0:1.8.0*255"},
            {"name":"power","obis":"1-0:16.7.0*255"},
            {"name":"unscaled","obis":"1-0:2.8.0*255"}
        ]
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
        &super::window(duration),
        "-o",
        "run.jsonl",
    ])
}

#[test]
fn receives_fragmented_escaped_push_files_without_transmitting() {
    let home = Home::new("sml-push");
    let (mut master, slave, device) = serial_pair();
    let meter = Device::start(move |stop| {
        if !await_terminal(&slave, &stop) {
            return 0;
        }
        // Attach midway through a previous push and split the next valid
        // start marker across two serial receive deadlines.
        master.write_all(b"noise\x1b\x1b\x1b\x00").unwrap();
        master.write_all(&FRAME[..5]).unwrap();
        thread::sleep(time::millis(130));
        master.write_all(&FRAME[5..136]).unwrap();
        thread::sleep(time::millis(90));
        let mut tail = FRAME[136..].to_vec();
        tail.extend_from_slice(&FRAME);
        master.write_all(&tail).unwrap();
        assert!(
            !receive(&mut master, &mut [0], &stop),
            "passive meter received a byte"
        );
        2
    });
    assert_eq!(
        record(&home, &source(&device), "1100ms").success()["samples"],
        6
    );
    assert_eq!(meter.finish(), 2);
    let recording = fs::read_to_string(home.path("run.jsonl")).unwrap();
    let rows = recording
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["sources"][0]["protocol"], "sml_serial");
    assert_eq!(rows[1]["value"]["raw"], json!(u64::MAX));
    assert_eq!(
        rows[1]["value"]["value"].to_string(),
        "18446744073709551615e-3"
    );
    assert_eq!(rows[1]["value"]["status"], json!(0x0123_4567_89ab_cdef_u64));
    assert_eq!(rows[1]["value"]["unit"], 30);
    assert_eq!(rows[1]["value"]["scaler"], -3);
    assert_eq!(rows[2]["value"]["raw"], -32769);
    assert_eq!(rows[2]["value"]["value"].to_string(), "-32769e-1");
    assert_eq!(rows[2]["value"]["unit"], 27);
    assert!(rows[2]["value"].get("status").is_none());
    assert_eq!(
        rows[3]["value"],
        json!({"obis":"1-0:2.8.0*255","raw":0,"value":0})
    );
    for (first, second) in rows[1..4].iter().zip(&rows[4..7]) {
        assert_eq!(first["value"], second["value"]);
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
            6
        );
        let exported = fs::read_to_string(home.path(format)).unwrap();
        assert!(exported.contains("18446744073709551615e-3"));
        assert!(exported.contains("81985529216486895"));
    }
}

#[test]
fn rejects_corrupt_transport_message_or_missing_mapping_without_a_partial_push() {
    for (fault, message) in [
        ("transport", "transport CRC mismatch"),
        ("message", "message CRC mismatch"),
        ("padding", "nonzero transport padding"),
        ("missing", "absent from the push list"),
    ] {
        let home = Home::new("sml-invalid");
        let (mut master, slave, device) = serial_pair();
        let mut config = source(&device);
        if fault == "missing" {
            config["signals"][2]["obis"] = json!("1-0:32.7.0*255");
        }
        let meter = Device::start(move |stop| {
            if !await_terminal(&slave, &stop) {
                return 0;
            }
            let mut frame = FRAME;
            match fault {
                "transport" => frame[195] ^= 1,
                "message" => {
                    frame[182] ^= 1; // Corrupt the closing message CRC only.
                    frame[194..].copy_from_slice(&[0x1b, 0x71]);
                }
                "padding" => {
                    frame[185] ^= 1;
                    frame[194..].copy_from_slice(&[0xb3, 0xb9]);
                }
                "missing" => {}
                _ => unreachable!(),
            }
            master.write_all(&frame).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            1
        });
        let error = record(&home, &config, "1s").failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert_eq!(meter.finish(), 1);
        assert_unpublished(&home);
    }
}
