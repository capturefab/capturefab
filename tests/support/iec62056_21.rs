//! IEC 62056-21 optical Mode C through the CLI. The PTY observes host
//! termios settings and transmits even parity in the eighth bit. It does not
//! emulate a physical UART's bit timing.

use super::{Device, Home, assert_unpublished, receive, serial_pair};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Write as _,
    os::fd::AsRawFd as _,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

const REQUEST: &[u8; 5] = b"\xaf?!\x8d\n";
const ACK300: &[u8; 6] = b"\x06000\x8d\n";
const ACK9600: &[u8; 6] = b"\x06050\x8d\n";
const E350_IDENT: &[u8] = b"/LGZ0ZMF100AC.MXX\r\n";
// Landis+Gyr D000027979 en t Figure 5.7 (printed page 46). Its text
// specifies ETX/BCC but does not print the BCC byte. XOR is 0x03, so the
// final two bytes are both 0x03 and both must be consumed.
const E350: &[u8] = b"\x02F.F(00)\r\nC.1.1(0000000074892473)\r\n1.8.0(000065.3*kWh)\r\n2.8.0(000003.5*kWh)\r\n1.8.1(000021.5*kWh)\r\n1.8.2(00043.8*kWh)\r\n2.8.1(000001.5*kWh)\r\n2.8.2(00000.0*kWh)\r\nC.5.0(03)\r\n!\r\n\x03\x03";

fn parity(message: &[u8]) -> Vec<u8> {
    message
        .iter()
        .map(|byte| byte | (u8::from(!byte.count_ones().is_multiple_of(2)) << 7))
        .collect()
}

fn receive_request(port: &mut File, stop: &AtomicBool, expected: &[u8]) {
    let mut wire = vec![0; expected.len()];
    assert!(receive(port, &mut wire, stop));
    assert_eq!(wire, expected);
}

fn transmit(port: &mut File, message: &[u8]) {
    port.write_all(&parity(message)).unwrap();
}

fn source(device: &str, signals: &Value) -> Value {
    json!({"id":"meter", "protocol":"iec62056_21", "device":device,
        "timeout_ms":1800, "every_ms":3_600_000, "signals":signals})
}

fn record(home: &Home, config: &Value, duration: &str) -> super::support::cli::Run {
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

fn rows(home: &Home, file: &str) -> Vec<Value> {
    fs::read_to_string(home.path(file))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn speed(terminal: &File) -> libc::speed_t {
    let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: the descriptor is live and the allocation is writable.
    let status = unsafe { libc::tcgetattr(terminal.as_raw_fd(), mode.as_mut_ptr()) };
    assert_eq!(status, 0);
    // SAFETY: successful tcgetattr initialized this allocation.
    let mode = unsafe { mode.assume_init() };
    assert_eq!(mode.c_cflag & libc::CSIZE, libc::CS8);
    assert_eq!(mode.c_cflag & (libc::PARENB | libc::CSTOPB), 0);
    // SAFETY: mode is initialized and remains live for this call.
    unsafe { libc::cfgetispeed(&raw const mode) }
}

#[test]
fn iec62056_reads_the_published_e350_frame_and_preserves_exact_values() {
    let home = Home::new("iec62056-e350");
    let (mut master, slave, device) = serial_pair();
    let meter = Device::start(move |stop| {
        receive_request(&mut master, &stop, REQUEST);
        assert_eq!(speed(&slave), libc::B300);
        transmit(&mut master, E350_IDENT);
        receive_request(&mut master, &stop, ACK300);
        for fragment in E350.chunks(11) {
            transmit(&mut master, fragment);
        }
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let cfg = source(
        &device,
        &json!([
            {"name":"energy", "obis":"1.8.0", "format":"decimal"},
            {"name":"serial", "obis":"C.1.1"},
            {"name":"status", "obis":"C.5.0"}
        ]),
    );
    let result = record(&home, &cfg, "1s").success();
    assert_eq!(meter.finish(), 1);
    assert_eq!(result["samples"], 3);
    let original = rows(&home, "run.jsonl");
    assert_eq!(original[0]["sources"][0]["protocol"], "iec62056_21");
    let samples = original
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 3);
    for (row, (name, obis, values)) in samples.iter().zip([
        (
            "energy",
            "1.8.0",
            json!([{"raw":"000065.3","number":65.3,"unit":"kWh"}]),
        ),
        ("serial", "C.1.1", json!([{"raw":"0000000074892473"}])),
        ("status", "C.5.0", json!([{"raw":"03"}])),
    ]) {
        assert_eq!(row["signal"], name);
        assert_eq!(
            row["value"],
            json!({"obis":obis,"baud":300,"block_check":"verified",
            "identification":"/LGZ0ZMF100AC.MXX", "values":values})
        );
    }
    home.ok(&["data", "export", "run.jsonl", "-o", "out.csv"]);
    let csv = fs::read_to_string(home.path("out.csv")).unwrap();
    assert!(csv.contains("000065.3") && csv.contains("0000000074892473") && csv.contains("kWh"));
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "out.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported = rows(&home, "out.jsonl");
    assert_eq!(exported.len(), samples.len());
    for (actual, sample) in exported.iter().zip(samples) {
        let mut expected = sample.clone();
        expected.as_object_mut().unwrap().remove("kind");
        assert_eq!(*actual, expected);
    }
}

#[test]
fn iec62056_mt174_negotiates_and_resets_baud_on_the_owned_terminal() {
    let home = Home::new("iec62056-mt174");
    let (mut master, slave, device) = serial_pair();
    let meter = Device::start(move |stop| {
        // Iskraemeco MT174 Technical Description v1.2 §2.8.5.
        let request = parity(b"/?00001234!\r\n");
        for _ in 0..2 {
            receive_request(&mut master, &stop, &request);
            assert_eq!(speed(&slave), libc::B300);
            master.write_all(&request).unwrap(); // Optical head echo.
            transmit(&mut master, b"/ISK5MT174-0030\r\n");
            receive_request(&mut master, &stop, ACK9600);
            assert_eq!(speed(&slave), libc::B300);
            master.write_all(ACK9600).unwrap();
            let started = Instant::now();
            while speed(&slave) != libc::B9600 {
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "baud did not change"
                );
                thread::sleep(Duration::from_millis(3));
            }
            assert!(
                started.elapsed() >= Duration::from_millis(170),
                "changed baud before ACK wire time"
            );
            // Model's checked readout framing with exact synthetic OBIS data.
            // The first record has an additional timestamp group. Its text is
            // preserved even when its primary value is selected as a decimal.
            let readout = b"1.8.0(9007199254740993.001*kWh)(2026-11-10 08:30)\r\n1.8.0(+00001.250*kWh)\r\n!\r\n\x03";
            transmit(&mut master, b"\x02");
            transmit(&mut master, readout);
            transmit(
                &mut master,
                &[readout.iter().fold(0, |sum, byte| sum ^ byte)],
            );
        }
        assert!(!receive(&mut master, &mut [0], &stop));
        2
    });
    let mut cfg = source(
        &device,
        &json!([{"name":"energy", "obis":"1.8.0", "format":"decimal"}]),
    );
    cfg["every_ms"] = json!(1200);
    cfg["meter_address"] = json!("00001234");
    cfg["maximum_baud"] = serde_json::from_str("1.92e4").unwrap();
    let result = record(&home, &cfg, "2s").success();
    assert_eq!(meter.finish(), 2);
    assert_eq!(result["samples"], 2);
    let recorded = rows(&home, "run.jsonl");
    let samples = recorded
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 2);
    let expected = serde_json::from_str::<Value>(
        r#"{
        "obis":"1.8.0","baud":9600,"block_check":"verified","identification":"/ISK5MT174-0030",
        "values":[{"raw":"9007199254740993.001","number":9007199254740993.001,"unit":"kWh"},
        {"raw":"2026-11-10 08:30"},{"raw":"+00001.250","number":1.250,"unit":"kWh"}]
    }"#,
    )
    .unwrap();
    for sample in samples {
        assert_eq!(sample["value"], expected);
    }
}

#[test]
fn iec62056_uses_only_the_standard_unchecked_branch_after_explicit_opt_in() {
    let home = Home::new("iec62056-unchecked");
    let (mut master, slave, device) = serial_pair();
    let meter = Device::start(move |stop| {
        receive_request(&mut master, &stop, REQUEST);
        transmit(&mut master, b"/ISk5SIM\r\n");
        receive_request(&mut master, &stop, ACK300);
        assert_eq!(speed(&slave), libc::B300);
        transmit(&mut master, b"32.7.0(231.4*V)14.7.0(49.98*Hz)\r\n!\r\n");
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let mut cfg = source(
        &device,
        &json!([
            {"name":"voltage", "obis":"32.7.0"}, {"name":"frequency", "obis":"14.7.0"}
        ]),
    );
    cfg["maximum_baud"] = json!(300);
    cfg["allow_unchecked"] = json!(true);
    let result = record(&home, &cfg, "1s").success();
    assert_eq!(meter.finish(), 1);
    assert_eq!(result["samples"], 2);
    let records = rows(&home, "run.jsonl");
    assert_eq!(records[1]["value"]["block_check"], "unchecked");
    assert_eq!(
        records[1]["value"]["values"],
        json!([{"raw":"231.4","unit":"V"}])
    );
    assert_eq!(
        records[2]["value"]["values"],
        json!([{"raw":"49.98","unit":"Hz"}])
    );
}

#[test]
fn iec62056_invalid_frames_and_absent_siblings_publish_no_samples() {
    let mut bad_bcc = parity(E350);
    *bad_bcc.last_mut().unwrap() = 0; // valid parity, wrong checksum
    let mut bad_parity = parity(E350);
    *bad_parity.last_mut().unwrap() ^= 0x80; // same seven-bit checksum, wrong parity
    for (reply, allow_unchecked, signal, cause) in [
        (bad_bcc, true, "1.8.0", "BCC mismatch"),
        (bad_parity, false, "1.8.0", "invalid even parity"),
        (
            parity(b"1.8.0(1*kWh)\r\n!\r\n"),
            false,
            "1.8.0",
            "requires STX",
        ),
        (parity(E350), false, "32.7.0", "absent from readout"),
        (
            parity(b"\x021.8.0(1*kWh)\r\n!\r\n\x04"),
            true,
            "1.8.0",
            "requires ETX",
        ),
    ] {
        let home = Home::new("iec62056-reject");
        let (mut master, slave, device) = serial_pair();
        let meter = Device::start(move |stop| {
            let _slave = slave;
            receive_request(&mut master, &stop, REQUEST);
            transmit(&mut master, E350_IDENT);
            receive_request(&mut master, &stop, ACK300);
            master.write_all(&reply).unwrap();
            assert!(!receive(&mut master, &mut [0], &stop));
            1
        });
        let mut cfg = source(
            &device,
            &json!([
                {"name":"first", "obis":"C.1.1"}, {"name":"second", "obis":signal}
            ]),
        );
        cfg["allow_unchecked"] = json!(allow_unchecked);
        let error = record(&home, &cfg, "1s").failure(1);
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        assert_eq!(meter.finish(), 1);
        assert_unpublished(&home);
    }
}

#[test]
fn iec62056_switch_wait_and_missing_checksum_respect_the_same_poll_deadline() {
    for switch in [false, true] {
        let home = Home::new("iec62056-deadline");
        let (mut master, slave, device) = serial_pair();
        let meter = Device::start(move |stop| {
            receive_request(&mut master, &stop, REQUEST);
            if switch {
                transmit(&mut master, b"/ISK5MT174-0030\r\n");
                receive_request(&mut master, &stop, ACK9600);
            } else {
                transmit(&mut master, E350_IDENT);
                receive_request(&mut master, &stop, ACK300);
                transmit(&mut master, E350.split_last().unwrap().1);
            }
            assert!(!receive(&mut master, &mut [0], &stop));
            assert_eq!(speed(&slave), libc::B300);
            1
        });
        let mut cfg = source(&device, &json!([{"name":"energy", "obis":"1.8.0"}]));
        cfg["timeout_ms"] = json!(350);
        cfg["allow_unchecked"] = json!(true);
        let error = record(&home, &cfg, "1s").failure(4);
        assert_eq!(error["code"], "timeout");
        assert_eq!(meter.finish(), 1);
        assert_unpublished(&home);
    }
}

#[test]
fn iec62056_rejects_non_mode_c_identification_without_sending_ack() {
    let home = Home::new("iec62056-mode-e");
    let (mut master, slave, device) = serial_pair();
    let meter = Device::start(move |stop| {
        let _slave = slave;
        receive_request(&mut master, &stop, REQUEST);
        transmit(&mut master, b"/ISK5\\2MT174\r\n");
        assert!(!receive(&mut master, &mut [0], &stop));
        1
    });
    let cfg = source(&device, &json!([{"name":"energy", "obis":"1.8.0"}]));
    let error = record(&home, &cfg, "1s").failure(1);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("unsupported Mode C")
    );
    assert_eq!(meter.finish(), 1);
    assert_unpublished(&home);
}

#[test]
fn iec62056_validates_address_and_exact_baud_before_opening_terminal() {
    for (key, value, message) in [
        ("meter_address", json!("123!\r\n/?"), "meter_address"),
        ("maximum_baud", json!(9600.5), "unsigned integer"),
        ("maximum_baud", json!(4801), "maximum_baud"),
    ] {
        let home = Home::new("iec62056-config");
        let mut cfg = source(
            "/missing-device",
            &json!([{"name":"energy", "obis":"1.8.0"}]),
        );
        cfg[key] = value;
        let error = record(&home, &cfg, "1s").failure(2);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
