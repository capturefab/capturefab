//! Managed HART modem PTY fixtures through the CLI. The `Rosemount3051C`
//! command1 transaction is Fisher-Rosemount Technical Overview2nded Fig3-6.
//! PTYs validate host UART bytes, not physical FSK/modem arbitration.
use super::{Device, Home, assert_unpublished, receive, serial_pair, support::cli::Run};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{self, Read as _, Write as _},
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};
const UID: [u8; 5] = [0x26, 6, 0xbc, 0x61, 0x4e];
const PUBLISHED_REQUEST: [u8; 14] = [
    255, 255, 255, 255, 255, 0x82, 0xa6, 6, 0xbc, 0x61, 0x4e, 1, 0, 0xb0,
];
const PUBLISHED_REPLY: [u8; 21] = [
    255, 255, 255, 255, 255, 0x86, 0xa6, 6, 0xbc, 0x61, 0x4e, 1, 7, 0, 0, 6, 0x40, 0xb0, 0, 0, 0x45,
];
fn config(device: &str, signals: &Value) -> Value {
    json!({"id":"process","protocol":"hart_serial","device":device,"modem":"microlink_hm_101_0035","master":"primary","every_ms":3_600_000,"timeout_ms":1000,"signals":signals})
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
        "1s",
        "-o",
        "run.jsonl",
    ])
}
fn samples(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["kind"] == "sample")
        .collect()
}
fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, byte| sum ^ byte)
}
fn expect(port: &mut File, stop: &AtomicBool, bytes: &[u8]) {
    let mut actual = vec![0; bytes.len()];
    assert!(receive(port, &mut actual, stop));
    assert_eq!(actual, bytes);
}
fn command(address: &[u8], primary: bool, command: u8, preambles: usize) -> Vec<u8> {
    let mut frame = vec![255; preambles];
    frame.push(if address.len() == 1 { 2 } else { 0x82 });
    frame.push(address[0] | if primary { 0x80 } else { 0 });
    frame.extend_from_slice(&address[1..]);
    frame.extend_from_slice(&[command, 0]);
    frame.push(xor(&frame[preambles..]));
    frame
}
fn reply(
    address: &[u8],
    primary: bool,
    command: u8,
    status: [u8; 2],
    data: &[u8],
    burst: bool,
) -> Vec<u8> {
    let mut frame = vec![255; 5];
    frame.push(if address.len() == 1 {
        if burst { 1 } else { 6 }
    } else if burst {
        0x81
    } else {
        0x86
    });
    frame.push(address[0] | if primary { 0x80 } else { 0 });
    frame.extend_from_slice(&address[1..]);
    frame.extend_from_slice(&[
        command,
        u8::try_from(data.len() + 2).unwrap(),
        status[0],
        status[1],
    ]);
    frame.extend_from_slice(data);
    frame.push(xor(&frame[5..]));
    frame
}
fn identity(uid: [u8; 5], revision: u8) -> Vec<u8> {
    let mut data = vec![
        254, uid[0], uid[1], 5, revision, 1, 3, 8, 0, uid[2], uid[3], uid[4],
    ];
    if revision >= 6 {
        data.extend_from_slice(&[5, 4, 0x12, 0x34, 1]);
    }
    if revision >= 7 {
        data.extend_from_slice(&[0xab, 0xcd, 0x12, 0x34, 1]);
    }
    data
}
fn identify(
    port: &mut File,
    slave: File,
    stop: &AtomicBool,
    address: u8,
    primary: bool,
    uid: [u8; 5],
    revision: u8,
) {
    expect(port, stop, &command(&[address], primary, 0, 20));
    // Capturefab already owns the slave. Releasing this spare descriptor lets
    // the peer observe the recorder retiring its handle after a failure.
    drop(slave);
    send(
        port,
        &reply(
            &[address],
            primary,
            0,
            [0, 0x40],
            &identity(uid, revision),
            false,
        ),
    );
}
fn send(port: &mut File, mut bytes: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !bytes.is_empty() {
        match port.write(bytes) {
            Ok(0) => panic!("PTY write made no progress"),
            Ok(count) => bytes = &bytes[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                assert!(Instant::now() < deadline, "HART fixture write timeout");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("HART fixture write: {error}"),
        }
    }
}
fn no_more(port: &mut File) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match port.read(&mut [0]) {
            Ok(0) => return,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                assert!(
                    Instant::now() < deadline,
                    "failed HART session did not retire"
                );
                thread::sleep(Duration::from_millis(1));
            }
            value => panic!("no later HART query expected: {value:?}"),
        }
    }
}
#[test]
fn hart_replays_published_rosemount_3051c_and_exact_raw_status_exports() {
    let home = Home::new("hart-published");
    let (mut port, slave, device) = serial_pair();
    let mut cfg = config(
        &device,
        &json!([
            {"name":"pressure","polling_address":0,"read":"primary_variable","unique_address":"2606bc614e"},
            {"name":"loop","polling_address":0,"read":"loop_current_and_percent"},
            {"name":"dynamic","polling_address":0,"read":"dynamic_variables"}
        ]),
    );
    cfg["baud"] = serde_json::from_str("1.2e3").unwrap();
    cfg["signals"][0]["polling_address"] = serde_json::from_str("0e20").unwrap();
    let worker = Device::start(move |stop| {
        identify(&mut port, slave, &stop, 0, true, UID, 5);
        expect(&mut port, &stop, &PUBLISHED_REQUEST);
        for bytes in PUBLISHED_REPLY.chunks(3) {
            send(&mut port, bytes);
        }
        expect(&mut port, &stop, &command(&UID, true, 2, 5));
        let mut data = 12.5_f32.to_be_bytes().to_vec();
        data.extend_from_slice(&(-0.0_f32).to_be_bytes());
        send(&mut port, &reply(&UID, true, 2, [0, 0x40], &data, false));
        expect(&mut port, &stop, &command(&UID, true, 3, 5));
        let mut data = 12.5_f32.to_be_bytes().to_vec();
        for (unit, bits) in [
            (6, 5.5_f32.to_bits()),
            (32, 22.25_f32.to_bits()),
            (250, 0x7fa0_0000),
            (250, 0x7f80_0001),
        ] {
            data.push(unit);
            data.extend_from_slice(&bits.to_be_bytes());
        }
        send(&mut port, &reply(&UID, true, 3, [8, 0x81], &data, false));
        no_more(&mut port);
        4
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 3);
    assert_eq!(worker.finish(), 4);
    let rows = samples(&home);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["value"]["raw_reply"], json!(PUBLISHED_REPLY));
    assert_eq!(
        rows[0]["value"]["primary_variable"],
        json!({"unit_code":6,"ieee754_bits":0x40b0_0000_u32,"decode_status":"finite","value":5.5})
    );
    assert_eq!(rows[0]["value"]["identity"]["manufacturer_code"], 38);
    assert_eq!(rows[0]["value"]["identity"]["device_id"], 12_345_678);
    assert_eq!(rows[1]["value"]["loop_current"]["value"], 12.5);
    assert_eq!(
        rows[1]["value"]["percent_of_range"]["ieee754_bits"],
        0x8000_0000_u32
    );
    assert_eq!(rows[2]["value"]["response_code"], 8);
    assert_eq!(rows[2]["value"]["command_status"], "update_failure");
    assert_eq!(rows[2]["value"]["device_status"], 0x81);
    let variables = rows[2]["value"]["variables"].as_array().unwrap();
    assert_eq!(variables.len(), 4);
    for (index, bits) in [(2, 0x7fa0_0000_u32), (3, 0x7f80_0001_u32)] {
        assert_eq!(variables[index]["ieee754_bits"], bits);
        assert_eq!(variables[index]["unit_code"], 250);
        assert_eq!(variables[index]["decode_status"], "non_finite");
        assert!(variables[index].get("value").is_none());
    }
    let clocks = rows
        .iter()
        .map(|row| row["value"]["received_monotonic_ns"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(
        clocks
            .windows(2)
            .all(|pair| matches!(pair, [first, last] if first <= last))
    );
    for (format, file) in [("jsonl", "out.jsonl"), ("csv", "out.csv")] {
        assert_eq!(
            home.run(&[
                "--json",
                "data",
                "export",
                "run.jsonl",
                "--format",
                format,
                "-o",
                file
            ])
            .success()["samples"],
            3
        );
        let output = fs::read_to_string(home.path(file)).unwrap();
        assert!(output.contains("2141192192"));
        assert!(output.contains("2147483648"));
        if format == "jsonl" {
            let exported = output
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()["value"].clone())
                .collect::<Vec<_>>();
            assert_eq!(
                exported,
                rows.iter()
                    .map(|row| row["value"].clone())
                    .collect::<Vec<_>>()
            );
        }
    }
}
#[test]
fn hart_secondary_filters_valid_foreign_and_burst_frames_and_uses_revision_uid_bytes() {
    let home = Home::new("hart-secondary");
    let (mut port, slave, device) = serial_pair();
    let mut cfg = config(
        &device,
        &json!([
            {"name":"rev6","polling_address":1,"read":"primary_variable"},
            {"name":"rev7","polling_address":16,"read":"primary_variable","unique_address":"21cd241857"}
        ]),
    );
    cfg["master"] = json!("secondary");
    let modern_type = [0x61, 0xcd, 0x24, 0x18, 0x57];
    let modern = [0x21, 0xcd, 0x24, 0x18, 0x57];
    let worker = Device::start(move |stop| {
        identify(&mut port, slave, &stop, 1, false, UID, 6);
        expect(&mut port, &stop, &command(&UID, false, 1, 5));
        let data = [6, 0x40, 0xb0, 0, 0];
        send(&mut port, &reply(&UID, true, 1, [0, 0], &data, false));
        send(&mut port, &reply(&UID, false, 1, [0, 0], &data, true));
        send(
            &mut port,
            &reply(&[0x36, 1, 2, 3, 4], false, 1, [0, 0], &data, false),
        );
        let mut in_burst = UID;
        in_burst[0] |= 0x40;
        send(
            &mut port,
            &reply(&in_burst, false, 1, [0, 0x40], &data, false),
        );
        expect(&mut port, &stop, &command(&[16], false, 0, 20));
        send(
            &mut port,
            &reply(&[16], false, 0, [0, 0], &identity(modern_type, 7), false),
        );
        expect(&mut port, &stop, &command(&modern, false, 1, 5));
        send(&mut port, &reply(&modern, false, 1, [0, 0], &data, false));
        no_more(&mut port);
        4
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    assert_eq!(worker.finish(), 4);
    let rows = samples(&home);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["value"]["identity"]["universal_revision"], 6);
    assert_eq!(
        rows[0]["value"]["identity"]["configuration_change_counter"],
        0x1234
    );
    assert_eq!(rows[1]["value"]["unique_address"], "21cd241857");
    assert_eq!(rows[1]["value"]["identity"]["manufacturer_code"], 0xabcd);
    assert_eq!(rows[1]["value"]["identity"]["address_type_code"], 0x61cd);
}
#[test]
fn hart_malformed_siblings_and_command_rejections_are_atomic_and_terminal() {
    for mode in [
        "checksum",
        "command",
        "extent",
        "status",
        "preamble",
        "delimiter",
        "short_status",
        "truncated",
        "burst_only",
    ] {
        let home = Home::new("hart-malformed");
        let (mut port, slave, device) = serial_pair();
        let mut cfg = config(
            &device,
            &json!([
                {"name":"good","polling_address":0,"read":"primary_variable"},
                {"name":"bad","polling_address":0,"read":"primary_variable"}
            ]),
        );
        cfg["timeout_ms"] = json!(300);
        let worker = Device::start(move |stop| {
            identify(&mut port, slave, &stop, 0, true, UID, 5);
            expect(&mut port, &stop, &command(&UID, true, 1, 5));
            send(&mut port, &PUBLISHED_REPLY);
            expect(&mut port, &stop, &command(&UID, true, 1, 5));
            let mut frame = match mode {
                "command" => reply(&UID, true, 2, [0, 0], &[0; 8], false),
                "extent" => reply(&UID, true, 1, [0, 0], &[0; 6], false),
                "status" => reply(&UID, true, 1, [6, 0x80], &[], false),
                "preamble" => vec![255; 65],
                "delimiter" => vec![255, 255, 0xa6],
                "short_status" => {
                    let mut bytes = vec![255; 5];
                    bytes.extend_from_slice(&[0x86, 0xa6, 6, 0xbc, 0x61, 0x4e, 1, 1, 0]);
                    bytes.push(xor(&bytes[5..]));
                    bytes
                }
                "truncated" => {
                    let mut bytes = PUBLISHED_REPLY.to_vec();
                    bytes.truncate(15);
                    bytes
                }
                "burst_only" => reply(&UID, true, 1, [0, 0], &[6, 0x40, 0xb0, 0, 0], true),
                _ => PUBLISHED_REPLY.to_vec(),
            };
            if mode == "checksum" {
                *frame.last_mut().unwrap() ^= 1;
            }
            send(&mut port, &frame);
            no_more(&mut port);
            3
        });
        let timeout = matches!(mode, "truncated" | "burst_only");
        let error = record(&home, &cfg).failure(if timeout { 4 } else { 1 });
        assert_eq!(worker.finish(), 3);
        let diagnostic = match mode {
            "checksum" => "checksum",
            "command" => "command differs",
            "extent" => "command-1",
            "status" => "rejected read",
            "preamble" => "preamble",
            "delimiter" => "delimiter",
            "short_status" => "status bytes",
            _ => "deadline",
        };
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if mode == "status" {
            assert_eq!(error["details"]["hart_response_code"], 6);
            assert_eq!(error["details"]["hart_device_status"], 0x80);
        }
        assert_unpublished(&home);
    }
}
#[test]
fn hart_identity_validation_blocks_value_queries() {
    for mode in ["prefix", "short", "revision", "uid", "old_address"] {
        let home = Home::new("hart-identity");
        let (mut port, slave, device) = serial_pair();
        let address = if mode == "old_address" { 16 } else { 0 };
        let mut cfg = config(
            &device,
            &json!([{"name":"bad","polling_address":address,"read":"primary_variable"}]),
        );
        if mode == "uid" {
            cfg["signals"][0]["unique_address"] = json!("2606000001");
        }
        let worker = Device::start(move |stop| {
            expect(&mut port, &stop, &command(&[address], true, 0, 20));
            drop(slave);
            let mut data = identity(UID, 5);
            match mode {
                "prefix" => data[0] = 253,
                "short" => {
                    data.pop();
                }
                "revision" => data[4] = 8,
                _ => {}
            }
            send(&mut port, &reply(&[address], true, 0, [0, 0], &data, false));
            no_more(&mut port);
            1
        });
        record(&home, &cfg).failure(if mode == "revision" { 5 } else { 1 });
        assert_eq!(worker.finish(), 1);
        assert_unpublished(&home);
    }
}
#[test]
fn hart_foreign_frames_keep_the_original_deadline_and_later_siblings_are_not_published() {
    let home = Home::new("hart-deadline");
    let (mut port, slave, device) = serial_pair();
    let mut cfg = config(
        &device,
        &json!([
            {"name":"good","polling_address":0,"read":"primary_variable"},
            {"name":"late","polling_address":0,"read":"primary_variable"}
        ]),
    );
    cfg["timeout_ms"] = json!(300);
    let worker = Device::start(move |stop| {
        identify(&mut port, slave, &stop, 0, true, UID, 5);
        expect(&mut port, &stop, &command(&UID, true, 1, 5));
        thread::sleep(Duration::from_millis(150));
        send(&mut port, &PUBLISHED_REPLY);
        expect(&mut port, &stop, &command(&UID, true, 1, 5));
        for _ in 0..2 {
            send(
                &mut port,
                &reply(&UID, true, 1, [0, 0], &[6, 0x40, 0xb0, 0, 0], true),
            );
            thread::sleep(Duration::from_millis(100));
        }
        no_more(&mut port);
        3
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    assert_eq!(worker.finish(), 3);
    assert_unpublished(&home);
}
#[test]
fn hart_foreign_input_budget_stops_without_another_query() {
    let home = Home::new("hart-foreign-budget");
    let (mut port, slave, device) = serial_pair();
    let cfg = config(
        &device,
        &json!([{"name":"bad","polling_address":0,"read":"primary_variable"}]),
    );
    let worker = Device::start(move |stop| {
        identify(&mut port, slave, &stop, 0, true, UID, 5);
        expect(&mut port, &stop, &command(&UID, true, 1, 5));
        let frame = reply(&UID, true, 1, [0, 0], &[255; 253], true);
        assert_eq!(frame.len(), 269);
        for _ in 0..244 {
            send(&mut port, &frame);
        }
        no_more(&mut port);
        2
    });
    let error = record(&home, &cfg).failure(1);
    assert_eq!(worker.finish(), 2);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("per-query input limit")
    );
    assert_unpublished(&home);
}
#[test]
fn hart_cutoff_preserves_a_completed_poll_and_fails_a_later_partial_reply() {
    let home = Home::new("hart-cutoff");
    let (mut port, slave, device) = serial_pair();
    let mut cfg = config(
        &device,
        &json!([{"name":"pressure","polling_address":0,"read":"primary_variable"}]),
    );
    cfg["every_ms"] = json!(200);
    let worker = Device::start(move |stop| {
        identify(&mut port, slave, &stop, 0, true, UID, 5);
        expect(&mut port, &stop, &PUBLISHED_REQUEST);
        send(&mut port, &PUBLISHED_REPLY);
        expect(&mut port, &stop, &PUBLISHED_REQUEST);
        send(&mut port, &PUBLISHED_REPLY[..18]);
        no_more(&mut port);
        3
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    assert_eq!(worker.finish(), 3);
    let retained = samples(&home);
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0]["value"]["primary_variable"]["value"], 5.5);
    home.run(&[
        "--json",
        "data",
        "export",
        "run.jsonl",
        "--format",
        "jsonl",
        "-o",
        "recover.jsonl",
        "--allow-incomplete",
    ])
    .success();
    let recovered = fs::read_to_string(home.path("recover.jsonl")).unwrap();
    let rows: Vec<Value> = recovered
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"], retained[0]["value"]);
}

#[test]
fn hart_invalid_profile_numbers_and_identity_maps_fail_before_opening_port() {
    for signal in [
        json!({"name":"bad","polling_address":64}),
        json!({"name":"bad","polling_address":1.5}),
        json!({"name":"bad","polling_address":256}),
        json!({"name":"bad","polling_address":0,"read":"write_polling_address"}),
        json!({"name":"bad","polling_address":0,"unique_address":"a606bc614e"}),
        json!({"name":"bad","polling_address":0,"unique_address":"not_hex"}),
    ] {
        let home = Home::new("hart-config");
        let cfg = config("/missing/hart-modem", &json!([signal]));
        assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
    }
    for (key, value) in [
        ("modem", json!("viator")),
        ("master", json!("auto")),
        ("baud", json!(1200.5)),
        ("baud", json!(115_200)),
    ] {
        let home = Home::new("hart-config");
        let mut cfg = config(
            "/missing/hart-modem",
            &json!([{"name":"bad","polling_address":0}]),
        );
        cfg[key] = value;
        assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
    }
    let home = Home::new("hart-identity-map");
    let cfg = config(
        "/missing/hart-modem",
        &json!([
            {"name":"one","polling_address":0,"unique_address":"2606bc614e"},
            {"name":"two","polling_address":0,"unique_address":"2606000001"}
        ]),
    );
    assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
}
