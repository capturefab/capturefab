//! Vendor no-sensor literal and spec-derived read-only TPG mnemonic exchanges.
//! The peer is a bounded transcript fixture, not a stock/physical controller.
use super::{Device, Home, accept, assert_unpublished, listener, receive, serial_pair};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write as _},
    net::TcpStream,
    sync::atomic::AtomicBool,
    thread,
    time::Instant,
};

fn expect(port: &mut (impl Read + std::os::fd::AsRawFd), bytes: &[u8], stop: &AtomicBool) {
    let mut actual = vec![0; bytes.len()];
    assert!(receive(port, &mut actual, stop));
    assert_eq!(actual, bytes);
}
fn no_more_tcp(port: &mut TcpStream) {
    // Accepted peers already have a short read timeout. Require closure and
    // reject every extra byte even if Device::finish has set its stop flag.
    let deadline = Instant::now() + time::secs(1);
    let mut byte = [0];
    loop {
        assert!(
            Instant::now() < deadline,
            "TPG client did not close its failed poll"
        );
        match port.read(&mut byte) {
            Ok(0) => return,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            other => panic!("unexpected request after TPG poll: {other:?}"),
        }
    }
}
fn transaction(
    port: &mut (impl Read + std::io::Write + std::os::fd::AsRawFd),
    command: &[u8],
    reply: &[u8],
    stop: &AtomicBool,
) {
    expect(port, command, stop);
    port.write_all(&[0x06]).unwrap();
    port.write_all(b"\r\n").unwrap();
    expect(port, &[0x05], stop);
    port.write_all(reply).unwrap();
    port.write_all(b"\r").unwrap();
    port.write_all(b"\n").unwrap();
}
fn source(address: &Value, model: &str, channels: u8) -> Value {
    json!({"address":address,"model":model,"signals":(1..=channels).map(|channel|
        json!({"name":format!("gauge{channel}"),"channel":channel})).collect::<Vec<_>>()})
}
fn record_for(home: &Home, mut source: Value, duration: &str) -> super::support::cli::Run {
    source["id"] = json!("vacuum");
    source["protocol"] = json!("pfeiffer_tpg");
    source["every_ms"] = json!(3_600_000);
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
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
fn samples(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .collect()
}
fn exported(home: &Home, expected: &[Value]) {
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
            expected.len()
        );
    }
    let jsonl = fs::read_to_string(home.path("jsonl")).unwrap();
    let values = jsonl
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["value"].clone())
        .collect::<Vec<_>>();
    assert_eq!(values, expected);
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    for value in expected {
        assert!(
            csv.contains(&value.to_string().replace('"', "\"\"")),
            "{csv}"
        );
    }
}

#[test]
fn tpg_models_verify_identity_channel_bounds_and_exact_exports() {
    for (model, name, part, channels) in [
        ("tpg361", "TPG361", "PTG28040", 1_u8),
        ("tpg362", "TPG362", "PTG28290", 2),
        ("tpg366", "TPG366", "PTG28770", 6),
    ] {
        let home = Home::new(&format!("tpg-{model}"));
        let (listener, address) = listener();
        let device = Device::start(move |stop| {
            let mut port = accept(&listener, &stop).unwrap();
            expect(&mut port, b"AYT\r", &stop);
            // The vendor documents startup continuous output stopping on the first
            // host character. An already queued printable tail is not a query reply.
            port.write_all(b"0,1.0000E+00\r\n\x06\r\n").unwrap();
            expect(&mut port, &[0x05], &stop);
            port.write_all(format!("{name},{part},44990000,010100,010100\r\n").as_bytes())
                .unwrap();
            transaction(&mut port, b"UNI\r", b"4", &stop);
            for channel in 1..=channels {
                let reply = if channel == 1 {
                    "0,+1.2345E-06".to_owned()
                } else if channel == 2 {
                    "5,2.0000E-2".to_owned()
                } else {
                    format!("{},-1.0000E+00", channel - 1)
                };
                transaction(
                    &mut port,
                    format!("PR{channel}\r").as_bytes(),
                    reply.as_bytes(),
                    &stop,
                );
            }
            transaction(&mut port, b"UNI\r", b"4", &stop);
            no_more_tcp(&mut port);
            1_u64
        });
        let run = record_for(&home, source(&json!(address), model, channels), "700ms");
        assert_eq!(run.success()["samples"], channels);
        assert_eq!(device.finish(), 1);
        let rows = samples(&home);
        assert_eq!(rows.len(), usize::from(channels));
        let values = rows
            .iter()
            .map(|row| row["value"].clone())
            .collect::<Vec<_>>();
        for (index, value) in values.iter().enumerate() {
            assert_eq!(value["device"]["model"], name);
            assert_eq!(value["device"]["firmware_raw"], "010100");
            assert_eq!(value["reported_unit"], "hPascal");
            assert_eq!(value["reported_unit_code"], 4);
            assert_eq!(value["startup_bytes_discarded"], 14);
            if index == 0 {
                assert_eq!(value["reading_raw"], "+1.2345E-06");
                assert_eq!(
                    value["value"],
                    serde_json::from_str::<Value>("1.2345E-06").unwrap()
                );
            } else {
                assert_eq!(value.get("value"), None);
            }
        }
        exported(&home, &values);
    }
}

#[test]
fn tpg_usb_serial_preserves_all_unit_codes_and_fault_statuses() {
    for code in 0..=5_u8 {
        let home = Home::new(&format!("tpg-usb-{code}"));
        let (mut master, slave, path) = serial_pair();
        let device = Device::start(move |stop| {
            let _slave = slave;
            transaction(
                &mut master,
                b"AYT\r",
                b"TPG362,PTG28290,44990000,010100,010100",
                &stop,
            );
            transaction(&mut master, b"UNI\r", code.to_string().as_bytes(), &stop);
            transaction(&mut master, b"PR1\r", b"0,-9.9999E+99", &stop);
            transaction(&mut master, b"PR2\r", b"6,1.0000E+00", &stop);
            transaction(&mut master, b"UNI\r", code.to_string().as_bytes(), &stop);
            assert!(!receive(&mut master, &mut [0], &stop));
            1_u64
        });
        let mut configured = source(&Value::Null, "tpg362", 2);
        configured.as_object_mut().unwrap().remove("address");
        configured["serial"] = json!({"device":path,"baud":9.6e3});
        let run = record_for(&home, configured, "700ms");
        assert_eq!(run.success()["samples"], 2);
        assert_eq!(device.finish(), 1);
        let rows = samples(&home);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row["value"]["reported_unit_code"], code);
        }
        let first = rows.first().unwrap();
        assert_eq!(first["value"]["reading_raw"], "-9.9999E+99");
        assert!(first["value"]["value"].is_number());
        let last = rows.last().unwrap();
        assert_eq!(last["value"]["status"], "identification_error");
        assert_eq!(last["value"].get("value"), None);
    }
}

#[test]
fn tpg_protocol_failures_publish_no_completed_sibling_and_do_not_send_recovery_commands() {
    for mode in 0..13 {
        let home = Home::new(&format!("tpg-invalid-{mode}"));
        let (listener, address) = listener();
        let device = Device::start(move |stop| {
            let mut port = accept(&listener, &stop).unwrap();
            if mode == 0 {
                transaction(
                    &mut port,
                    b"AYT\r",
                    b"TPG366,PTG28770,44990000,010100,010100",
                    &stop,
                );
            } else if mode == 1 {
                transaction(&mut port, b"AYT\r", b"TPG362,a,b,c", &stop);
            } else if mode == 2 {
                expect(&mut port, b"AYT\r", &stop);
                port.write_all(&[b'x'; 4097]).unwrap();
            } else {
                transaction(
                    &mut port,
                    b"AYT\r",
                    b"TPG362,PTG28290,44990000,010100,010100",
                    &stop,
                );
                transaction(&mut port, b"UNI\r", b"4", &stop);
                transaction(&mut port, b"PR1\r", b"0,1.2345E-06", &stop);
                if mode == 3 {
                    transaction(&mut port, b"PR2\r", b"0,1.0000E+00", &stop);
                    transaction(&mut port, b"UNI\r", b"5", &stop);
                } else {
                    expect(&mut port, b"PR2\r", &stop);
                    if mode == 4 {
                        port.write_all(b"\x15\r\n").unwrap();
                    } else if mode == 5 {
                        port.write_all(b"0,1.0000E+00\r\n\x06\r\n").unwrap();
                    } else if mode == 6 {
                        port.write_all(b"\x06\n").unwrap();
                    } else {
                        port.write_all(b"\x06\r\n").unwrap();
                        expect(&mut port, &[0x05], &stop);
                        let reply = match mode {
                            7 => b"7,1.0000E+00\r\n".to_vec(),
                            8 => b"0,1.000E+00\r\n".to_vec(),
                            9 => b"0,1.0000E+00,0\r\n".to_vec(),
                            10 => b"0,NaN\r\n".to_vec(),
                            11 => b"0,1.0000E+00\rX".to_vec(),
                            12 => vec![b'x'; 257],
                            _ => unreachable!(),
                        };
                        let _ = port.write_all(&reply);
                    }
                }
            }
            no_more_tcp(&mut port);
            1_u64
        });
        let run = record_for(&home, source(&json!(address), "tpg362", 2), "700ms");
        let expected = if mode == 0 { 5 } else { 1 };
        let error = run.failure(expected);
        let code = if mode == 0 {
            "unsupported"
        } else if mode == 4 {
            "failed"
        } else {
            "incomplete"
        };
        assert_eq!(error["code"], code, "mode{mode}: {error}");
        assert_eq!(device.finish(), 1);
        assert_unpublished(&home);
    }
}

fn delayed_pressure_reply(
    port: &mut (impl Read + std::io::Write + std::os::fd::AsRawFd),
    stop: &AtomicBool,
) -> u64 {
    transaction(
        port,
        b"AYT\r",
        b"TPG361,PTG28040,44990000,010100,010100",
        stop,
    );
    transaction(port, b"UNI\r", b"4", stop);
    expect(port, b"PR1\r", stop);
    thread::sleep(time::millis(160));
    let _ = port.write_all(b"\x06\r\n");
    let mut enquiry = [0];
    if receive(port, &mut enquiry, stop) {
        assert_eq!(enquiry, [0x05]);
        thread::sleep(time::millis(160));
        let _ = port.write_all(b"0,1.0000E+00\r\n");
    }
    assert!(!receive(port, &mut [0], stop));
    1_u64
}

#[test]
fn tpg_ack_and_body_share_one_poll_deadline() {
    for (serial, acquisition_cutoff) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let home = Home::new(if serial {
            "tpg-serial-deadline"
        } else {
            "tpg-tcp-deadline"
        });
        let (device, mut configured) = if serial {
            let (mut master, slave, path) = serial_pair();
            let device = Device::start(move |stop| {
                let _slave = slave;
                delayed_pressure_reply(&mut master, &stop)
            });
            let mut configured = source(&Value::Null, "tpg361", 1);
            configured.as_object_mut().unwrap().remove("address");
            configured["serial"] = json!({"device":path});
            (device, configured)
        } else {
            let (listener, address) = listener();
            let device = Device::start(move |stop| {
                let mut port = accept(&listener, &stop).unwrap();
                delayed_pressure_reply(&mut port, &stop)
            });
            (device, source(&json!(address), "tpg361", 1))
        };
        configured["timeout_ms"] = json!(time::ms(if acquisition_cutoff { 1000 } else { 250 }));
        let duration = if acquisition_cutoff { "100ms" } else { "700ms" };
        let run = record_for(&home, configured, duration);
        assert_eq!(run.failure(4)["code"], "timeout");
        assert_eq!(device.finish(), 1);
        assert_unpublished(&home);
    }
}

#[test]
fn tpg_invalid_profiles_fail_before_connecting() {
    for (model, channel) in [("tpg361", 2), ("tpg362", 3), ("tpg366", 7), ("tpg362", 0)] {
        let home = Home::new("tpg-invalid-config");
        let (listener, address) = listener();
        let mut configured = source(&json!(address), model, 1);
        configured["signals"] = json!([{"name":"gauge","channel":channel}]);
        record_for(&home, configured, "100ms").failure(2);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
