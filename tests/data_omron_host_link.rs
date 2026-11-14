//! Literal Host Link frames from Omron W342 sections 2-2, 3-5-3 and 4-3-7.
//! A pseudoterminal exercises framing and deadlines with portable 8N1 settings.
#![cfg(unix)]

#[path = "support/omron_cpm.rs"]
mod cpm;
mod support;

use serde_json::{Value, json};
use std::{
    ffi::CStr,
    fs::{self, File},
    io::{Read as _, Write as _},
    os::fd::{AsRawFd as _, FromRawFd as _},
    thread,
    time::Instant,
};
use support::cli::{Home, Run};
use support::time;

const RD_REQUEST: &[u8] = b"@12RD0100000450*\r";
const RD_REPLY: &[u8] = b"@12RD0032107654BA98FEDC53*\r";
const RD_UNIT_ZERO_REQUEST: &[u8] = b"@00RD0100000156*\r";
const RD_UNIT_ZERO_REPLY: &[u8] = b"@00RD00BEEF52*\r";
const FA_DM_REQUEST: &[u8] = b"@12FA30000000001018227100000047D*\r";
const FA_DM_REPLY: &[u8] = b"@12FA00400000000101000032107654BA98FEDC46*\r";
const FA_BITS_REQUEST: &[u8] = b"@27FA30000000101013100070F000201*\r";
const FA_BITS_REPLY: &[u8] = b"@27FA004000000101010000010046*\r";
const EXACT: u64 = 0xfedc_ba98_7654_3210;

struct Terminal {
    master: File,
    _slave: File,
    device: String,
}

impl Terminal {
    fn open() -> Self {
        let (mut master, mut slave) = (-1, -1);
        let mut name: [libc::c_char; 256] = [0; 256];
        assert_eq!(
            // SAFETY: these live outputs/name accept the new descriptors and path.
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: a successful openpty returns two independently owned descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: this is the other successful openpty descriptor.
        let slave = unsafe { File::from_raw_fd(slave) };
        for file in [&master, &slave] {
            assert_eq!(
                // SAFETY: the descriptors remain open through this flag change.
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        // SAFETY: successful openpty initializes a NUL-terminated device path.
        let device = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        Self {
            master,
            _slave: slave,
            device,
        }
    }

    fn expect(&self, expected: &[u8]) {
        let deadline = Instant::now() + time::secs(3);
        let mut request = vec![0; expected.len()];
        let mut consumed = 0;
        while consumed < request.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert!(
                !left.is_zero(),
                "Host Link request stalled after {consumed} bytes"
            );
            let millis = i32::try_from(left.as_millis()).unwrap().max(1);
            assert_eq!(
                // SAFETY: poll points to one live entry with the master descriptor.
                unsafe { libc::poll(&raw mut poll, 1, millis) },
                1,
                "waiting for Host Link request"
            );
            let count = (&self.master).read(&mut request[consumed..]).unwrap();
            assert_ne!(count, 0);
            consumed += count;
        }
        assert_eq!(request, expected);
    }

    fn reply(&self, response: &[u8]) {
        for chunk in response.chunks(11) {
            (&self.master).write_all(chunk).unwrap();
            thread::sleep(time::millis(2));
        }
    }
}

fn config(terminal: &Terminal, mode: &str, signals: &Value) -> Value {
    json!({"version":1,"sources":[{
        "id":"plc","protocol":"omron_host_link","device":terminal.device,
        "mode":mode,"unit":12,"data_bits":8,"stop_bits":1,"parity":"none",
        "every_ms":3_600_000,"timeout_ms":time::ms(350),"signals":signals
    }]})
}

fn fa_config(terminal: &Terminal) -> Value {
    let mut config = config(
        terminal,
        "fins_direct",
        &json!([
            {"name":"count","address":10_000,"data_type":"u64"},
            {"name":"limits","unit":27,"area":"wr","address":7,"bit":15,"data_type":"bool","elements":2}
        ]),
    );
    config["sources"][0]["response_wait_ms"] = json!(30);
    config
}

fn record(home: &Home, config: &Value) -> Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &time::arg(600),
        "-o",
        "run.jsonl",
    ])
}

fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn c_mode_rd_reads_decimal_dm_address_and_exports_exact_words() {
    let home = Home::new("omron-host-link-rd");
    let terminal = Terminal::open();
    let config = config(
        &terminal,
        "c_mode_rd",
        &json!([
            {"name":"count","address":100,"data_type":"u64"},
            {"name":"other","unit":0,"address":100}
        ]),
    );
    thread::scope(|scope| {
        scope.spawn(|| {
            terminal.expect(RD_REQUEST);
            terminal.reply(RD_REPLY);
            terminal.expect(RD_UNIT_ZERO_REQUEST);
            terminal.reply(RD_UNIT_ZERO_REPLY);
        });
        assert_eq!(record(&home, &config).success()["samples"], 2);
    });
    let rows = rows(&home);
    assert_eq!(rows[0]["sources"][0]["protocol"], "omron_host_link");
    assert_eq!(rows[1]["value"].as_u64(), Some(EXACT));
    assert_eq!(rows[2]["value"], 0xbeef);
    assert_eq!(rows[3]["status"], "complete");
    home.ok(&["data", "export", "run.jsonl", "-o", "values.csv"]);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert!(csv.contains(&EXACT.to_string()), "{csv}");
}

#[test]
fn direct_fa_reads_large_dm_address_and_cross_word_bits() {
    let home = Home::new("omron-host-link-fa");
    let terminal = Terminal::open();
    let config = fa_config(&terminal);
    thread::scope(|scope| {
        scope.spawn(|| {
            terminal.expect(FA_DM_REQUEST);
            terminal.reply(FA_DM_REPLY);
            terminal.expect(FA_BITS_REQUEST);
            terminal.reply(FA_BITS_REPLY);
        });
        assert_eq!(record(&home, &config).success()["samples"], 2);
    });
    let rows = rows(&home);
    assert_eq!(rows[1]["value"].as_u64(), Some(EXACT));
    assert_eq!(rows[2]["value"], json!([true, false]));
    assert_eq!(rows[3]["status"], "complete");
}

#[test]
fn invalid_second_response_never_publishes_the_first_signal() {
    for (reply, cause, detail) in [
        (
            &b"@27FA004000000101010000010047*\r"[..],
            "FCS mismatch",
            None,
        ),
        (&b"@27FA004000000001010000010047*\r"[..], "SID", None),
        (
            &b"@12FA004000000101010000010040*\r"[..],
            "unexpected response unit",
            None,
        ),
        (
            &b"@27FA1340*\r"[..],
            "Host Link target rejected",
            Some(("host_link_end_code", 0x13)),
        ),
        (
            &b"@27FA00400000010101110344*\r"[..],
            "FINS target rejected",
            Some(("fins_end_code", 0x1103)),
        ),
    ] {
        let home = Home::new("omron-host-link-rejection");
        let terminal = Terminal::open();
        let config = fa_config(&terminal);
        let error = thread::scope(|scope| {
            scope.spawn(|| {
                terminal.expect(FA_DM_REQUEST);
                terminal.reply(FA_DM_REPLY);
                terminal.expect(FA_BITS_REQUEST);
                terminal.reply(reply);
            });
            record(&home, &config).failure(1)
        });
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        if let Some((key, code)) = detail {
            assert_eq!(error["details"][key], code);
        }
        let rows = rows(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["samples"], 0);
        assert_eq!(rows[1]["status"], "failed");
    }
}

#[test]
fn both_fa_signals_share_one_deadline() {
    let home = Home::new("omron-host-link-deadline");
    let terminal = Terminal::open();
    let config = fa_config(&terminal);
    thread::scope(|scope| {
        scope.spawn(|| {
            terminal.expect(FA_DM_REQUEST);
            thread::sleep(time::millis(210));
            terminal.reply(FA_DM_REPLY);
            terminal.expect(FA_BITS_REQUEST);
            thread::sleep(time::millis(210));
            terminal.reply(FA_BITS_REPLY);
        });
        let error = record(&home, &config).failure(4);
        assert_eq!(error["details"]["samples"], 0);
    });
    assert_eq!(rows(&home).len(), 2);
}

#[test]
fn mode_specific_limits_reject_invalid_config_before_opening() {
    let home = Home::new("omron-host-link-config");
    let terminal = Terminal::open();
    for (mode, signal, reason) in [
        (
            "c_mode_rd",
            json!({"name":"outside","address":9999,"elements":2}),
            "D0–D9999",
        ),
        (
            "c_mode_rd",
            json!({"name":"fragmented","address":0,"elements":31}),
            "1–30 words",
        ),
        (
            "fins_direct",
            json!({"name":"too_many","address":0,"elements":270}),
            "269 words",
        ),
        (
            "fins_direct",
            json!({"name":"wrong_unit","address":0,"unit":32}),
            "unit must be 0–31",
        ),
    ] {
        let mut config = config(&terminal, mode, &json!([signal]));
        config["sources"][0]["device"] = json!(home.path("no-serial-port"));
        let error = record(&home, &config).failure(2);
        assert!(
            error["message"].as_str().unwrap().contains(reason),
            "{error}"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn direct_fa_reads_native_timer_counter_indices_without_bit_progression() {
    let corpus: Value =
        serde_json::from_str(include_str!("fixtures/fins/timer-literals.json")).unwrap();
    let cases = corpus["cases"].as_array().unwrap();
    let home = Home::new("omron-fa-timer-counter");
    let terminal = Terminal::open();
    let signals = Value::Array(cases.iter().map(|case| case["signal"].clone()).collect());
    let config = config(&terminal, "fins_direct", &signals);
    thread::scope(|scope| {
        scope.spawn(|| {
            for case in cases {
                terminal.expect(case["fa_request"].as_str().unwrap().as_bytes());
                terminal.reply(case["fa_reply"].as_str().unwrap().as_bytes());
            }
        });
        assert_eq!(record(&home, &config).success()["samples"], cases.len());
    });
    let all = rows(&home);
    assert_eq!(all.len(), cases.len() + 2);
    for (index, case) in cases.iter().enumerate() {
        assert_eq!(all[index + 1]["seq"], index + 1);
        assert_eq!(all[index + 1]["signal"], case["signal"]["name"]);
        assert_eq!(all[index + 1]["value"], case["expected"]);
        assert_eq!(all[index + 1]["elapsed_ns"], all[1]["elapsed_ns"]);
        assert!(all[index + 1].get("source_time").is_none());
    }
    assert_eq!(all.last().unwrap()["status"], "complete");
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "values.jsonl",
        "--format",
        "jsonl",
    ]);
    let exported: Vec<Value> = fs::read_to_string(home.path("values.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (index, case) in cases.iter().enumerate() {
        assert_eq!(exported[index]["value"], case["expected"]);
    }
}

#[test]
fn timer_counter_host_link_limits_fail_before_opening_the_serial_device() {
    for area in ["timer", "counter"] {
        for (mode, kind, count) in [
            ("fins_direct", "u16", 270),
            ("fins_direct", "bool", 539),
            ("c_mode_rd", "u16", 1),
        ] {
            let home = Home::new("omron-fa-timer-invalid");
            let terminal = Terminal::open();
            let mut config = config(
                &terminal,
                mode,
                &json!([{"name":"value","area":area,"address":0,"data_type":kind,"elements":count}]),
            );
            config["sources"][0]["device"] = json!(home.path("missing-serial-device"));
            assert_eq!(record(&home, &config).failure(2)["code"], "usage");
            assert!(!home.path("run.jsonl").exists());
        }
    }
}
