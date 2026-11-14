//! Published and spec-derived FATEK frames over an owned transparent TCP peer.
//! These fixtures do not qualify a physical PLC, RS232 cable or USB driver.
use super::{Device, Home, accept, assert_unpublished, listener, receive, support::cli::Run};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write as _},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = body.to_vec();
    let sum = bytes
        .iter()
        .fold(0_u8, |value, byte| value.wrapping_add(*byte));
    bytes.extend(format!("{sum:02X}").bytes());
    bytes.push(3);
    bytes
}
fn request(unit: u8, command: &str, address: &str, count: u16) -> Vec<u8> {
    frame(format!("\x02{unit:02X}{command}{:02X}{address}", count % 256).as_bytes())
}
fn reply(unit: u8, command: &str, data: &str) -> Vec<u8> {
    frame(format!("\x02{unit:02X}{command}0{data}").as_bytes())
}
fn query(input: &mut (impl Read + std::os::fd::AsRawFd), stop: &AtomicBool, expected: &[u8]) {
    let mut bytes = vec![0; expected.len()];
    assert!(receive(input, &mut bytes, stop));
    assert_eq!(bytes, expected);
}
fn no_more(input: &mut (impl Read + std::os::fd::AsRawFd), stop: &AtomicBool) {
    let deadline = Instant::now() + time::secs(10);
    loop {
        match input.read(&mut [0]) {
            Ok(0) => return,
            Ok(_) => panic!("unexpected FATEK request after transaction"),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ) =>
            {
                return;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                assert!(Instant::now() < deadline, "FATEK owner did not close");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("FATEK fixture read: {error}"),
        }
    }
}
fn source(address: &str, signals: Value) -> Value {
    let mut config = json!({"id":"plc","protocol":"fatek_fbs","profile":"fbs_port0","address":address,
        "every_ms":3_600_000});
    config["signals"] = signals;
    config
}
fn run(home: &Home, sources: &[Value], duration: &str) -> Run {
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
        &super::window(duration),
        "-o",
        "run.jsonl",
    ])
}
fn rows(home: &Home, path: &str) -> Vec<Value> {
    fs::read_to_string(home.path(path))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn plc_rows(home: &Home, path: &str) -> Vec<Value> {
    rows(home, path)
        .into_iter()
        .filter(|row| row["source"] == "plc")
        .collect()
}
fn assert_output(home: &Home, expected: &[(String, Value)], complete: bool, total: usize) {
    let recorded = rows(home, "run.jsonl");
    let end = recorded.last().unwrap();
    assert_eq!(end["status"], if complete { "complete" } else { "failed" });
    assert_eq!(end["samples"], total);
    let actual = plc_rows(home, "run.jsonl");
    assert_eq!(actual.len(), expected.len());
    for (row, (name, value)) in actual.iter().zip(expected) {
        assert_eq!(row["signal"].as_str(), Some(name.as_str()));
        assert_eq!(&row["value"], value);
        assert!(row.get("source_time").is_none());
        for key in ["elapsed_ns", "read_duration_ns", "delivery_latency_ns"] {
            assert!(row[key].is_u64());
        }
    }
    if !complete {
        assert_eq!(
            home.fails(1, &["data", "export", "run.jsonl", "-o", "refused.csv"])["code"],
            "incomplete"
        );
        assert!(!home.path("refused.csv").exists());
    }
    for format in ["jsonl", "csv"] {
        let mut args = vec![
            "--json",
            "data",
            "export",
            "run.jsonl",
            "-o",
            format,
            "--format",
            format,
        ];
        if !complete {
            args.push("--allow-incomplete");
        }
        assert_eq!(home.run(&args).success()["samples"], total);
    }
    let exported = plc_rows(home, "jsonl");
    assert_eq!(exported.len(), actual.len());
    for (row, original) in exported.iter().zip(&actual) {
        let mut projected = original.clone();
        assert_eq!(
            projected.as_object_mut().unwrap().remove("kind"),
            Some(json!("sample"))
        );
        assert_eq!(row, &projected);
    }
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let lines = lines.collect::<Vec<_>>();
    assert_eq!(lines.len(), total);
    let actual = lines
        .iter()
        .filter(|line| line.split(',').nth(1) == Some("\"plc\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    let values = expected
        .iter()
        .map(|(_, value)| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        values.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[test]
fn fatek_published_words_native_types_and_265_byte_maxima_keep_exact_exports() {
    let home = Home::new("fatek-types");
    let (listener, address) = listener();
    let cases = [
        (
            "R00012",
            "u16",
            3_u16,
            "10A57FC40001".into(),
            json!([4261, 32708, 1]),
            1_u8,
        ),
        (
            "X0050",
            "bool",
            6,
            "010110".into(),
            json!([false, true, false, true, true, false]),
            1,
        ),
        (
            "DR00012",
            "u32",
            2,
            "FFFFFFFF80000000".into(),
            json!([u32::MAX, 2_147_483_648_u32]),
            1,
        ),
        (
            "DR00012",
            "i32",
            2,
            "FFFFFFFF80000000".into(),
            json!([-1, i32::MIN]),
            1,
        ),
        (
            "RT0001",
            "i16",
            2,
            "80007FFF".into(),
            json!([i16::MIN, i16::MAX]),
            254,
        ),
        ("RC9999", "u16", 1, "FFFF".into(), json!(65_535), 10),
        ("D65535", "i16", 1, "FFFF".into(), json!(-1), 1),
        ("F00000", "u16", 1, "0000".into(), json!(0), 1),
        ("DD65534", "i32", 1, "7FFFFFFF".into(), json!(i32::MAX), 1),
        (
            "DF00000",
            "u32",
            1,
            "89ABCDEF".into(),
            json!(0x89ab_cdef_u32),
            1,
        ),
        ("DRT0000", "u32", 1, "FFFFFFFF".into(), json!(u32::MAX), 1),
        ("DRC9998", "i32", 1, "80000000".into(), json!(i32::MIN), 1),
        ("Y9999", "bool", 1, "1".into(), json!(true), 1),
        ("M0000", "bool", 1, "0".into(), json!(false), 1),
        ("S0000", "bool", 1, "1".into(), json!(true), 1),
        ("T0000", "bool", 1, "0".into(), json!(false), 1),
        ("C0000", "bool", 1, "1".into(), json!(true), 1),
        (
            "X0000",
            "bool",
            256,
            "0".repeat(256),
            json!(vec![false; 256]),
            1,
        ),
        (
            "R00000",
            "u16",
            64,
            "F".repeat(256),
            json!(vec![65_535; 64]),
            1,
        ),
        (
            "DR00000",
            "u32",
            32,
            "F".repeat(256),
            json!(vec![u32::MAX; 32]),
            1,
        ),
    ];
    let mut signals = Vec::new();
    let mut expected = Vec::new();
    let mut exchanges = Vec::new();
    for (index, (variable, data_type, elements, data, value, unit)) in cases.into_iter().enumerate()
    {
        let name = format!("v{index}");
        signals.push(json!({"name":name,"address":variable,"data_type":data_type,"elements":elements,"unit":unit}));
        expected.push((name, value));
        let command = if data_type == "bool" { "44" } else { "46" };
        let request = if index == 0 {
            b"\x02014603R0001275\x03".to_vec()
        } else {
            request(unit, command, variable, elements)
        };
        let reply = if index == 0 {
            b"\x020146010A57FC4000189\x03".to_vec()
        } else {
            reply(unit, command, &data)
        };
        if elements == 256 || elements == 64 || elements == 32 {
            assert_eq!(reply.len(), 265);
        }
        exchanges.push((request, reply));
    }
    // Independently hand-written combined scalar/array literals, mirrored in
    // combined-literals.json. No next-address or bit-order expectation is made.
    for (index, (variable, data_type, elements, unit, request, reply, value)) in [
        (
            "WX0008",
            "u16",
            1,
            1,
            &b"\x02014601WX0008A5\x03"[..],
            &b"\x0201460ABCD07\x03"[..],
            json!(43981),
        ),
        (
            "WX0008",
            "u16",
            2,
            1,
            &b"\x02014602WX0008A6\x03"[..],
            &b"\x02014608000FFFFDD\x03"[..],
            json!([32768, 65535]),
        ),
        (
            "DWM0016",
            "u32",
            1,
            254,
            &b"\x02FE4601DWM001607\x03"[..],
            &b"\x02FE460FFFFFFFF57\x03"[..],
            json!(u32::MAX),
        ),
        (
            "DWY0008",
            "u32",
            2,
            1,
            &b"\x02014602DWY0008EB\x03"[..],
            &b"\x020146012345678800000012A\x03"[..],
            json!([305_419_896_u32, 2_147_483_649_u32]),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("combined{index}");
        signals.push(json!({"name":name,"address":variable,"data_type":data_type,"elements":elements,"unit":unit}));
        expected.push((name, value));
        exchanges.push((request.to_vec(), reply.to_vec()));
    }
    let count = u64::try_from(exchanges.len()).unwrap();
    let device = Device::start(move |stop| {
        let Some(mut stream) = accept(&listener, &stop) else {
            return 0;
        };
        for (request, reply) in exchanges {
            query(&mut stream, &stop, &request);
            for part in reply.chunks(7) {
                stream.write_all(part).unwrap();
            }
        }
        no_more(&mut stream, &stop);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        count
    });
    let sources = [
        source(&address.to_string(), json!(signals)),
        json!({"id":"counter","protocol":"simulator","signal":"cycle","every_ms":3_600_000}),
    ];
    assert_eq!(
        run(&home, &sources, "500ms").success()["samples"],
        expected.len() + 1
    );
    assert_eq!(device.finish(), count);
    assert_output(&home, &expected, true, expected.len() + 1);
}

#[test]
fn fatek_repeat_polls_keep_one_owner_without_hidden_commands() {
    let home = Home::new("fatek-repeat");
    let (listener, address) = listener();
    let device = Device::start(move |stop| {
        let Some(mut stream) = accept(&listener, &stop) else {
            return 0;
        };
        for _ in 0..3 {
            query(&mut stream, &stop, b"\x02014603R0001275\x03");
            stream.write_all(b"\x020146010A57FC4000189\x03").unwrap();
            query(&mut stream, &stop, b"\x02014406X00504E\x03");
            stream.write_all(b"\x02014400101101E\x03").unwrap();
        }
        no_more(&mut stream, &stop);
        6_u64
    });
    let mut config = source(
        &address.to_string(),
        json!([
            {"name":"vendor","address":"R00012","elements":3},
            {"name":"bits","address":"X0050","data_type":"bool","elements":6}
        ]),
    );
    config["every_ms"] = json!(time::ms(200));
    assert_eq!(run(&home, &[config], "550ms").success()["samples"], 6);
    assert_eq!(device.finish(), 6);
    let mut expected = Vec::new();
    for _ in 0..3 {
        expected.extend([
            ("vendor".into(), json!([4261, 32708, 1])),
            (
                "bits".into(),
                json!([false, true, false, true, true, false]),
            ),
        ]);
    }
    assert_output(&home, &expected, true, 6);
}

#[test]
fn fatek_damaged_late_reply_rolls_back_current_poll_and_keeps_prior_prefix() {
    for (index, bad) in [
        frame(b"\x02024607FFF"),
        frame(b"\x02014407FFF"),
        frame(b"\x020146A"),
        frame(b"\x020146F"),
        frame(b"\x020146A0000"),
        frame(b"\x02014607FFG"),
        frame(b"\x02014607fff"),
        b"\x02014607FFF00\x03".to_vec(),
        b"\x02014607FFF85\x04".to_vec(),
        b"\x01014607FFF85\x03".to_vec(),
        frame(b"\x02014607F"),
        frame(b"\x02014607FFF0000"),
        {
            let mut bad = reply(1, "46", "7FFF");
            bad.extend(b"extra");
            bad
        },
        frame(b"\x02014402"),
        frame(format!("\x0201460{}", "F".repeat(260)).as_bytes()),
    ]
    .into_iter()
    .enumerate()
    {
        for prior in [false, true] {
            let home = Home::new(&format!("fatek-bad-{index}-{prior}"));
            let (listener, address) = listener();
            let boolean = index == 13;
            let command = if boolean { "44" } else { "46" };
            let variable = if boolean { "X0000" } else { "R00001" };
            let bad = bad.clone();
            let device = Device::start(move |stop| {
                let Some(mut stream) = accept(&listener, &stop) else {
                    return 0;
                };
                if prior {
                    query(&mut stream, &stop, &request(1, "46", "R00000", 1));
                    stream.write_all(&reply(1, "46", "0007")).unwrap();
                    query(&mut stream, &stop, &request(1, command, variable, 1));
                    stream
                        .write_all(&reply(1, command, if boolean { "1" } else { "7FFF" }))
                        .unwrap();
                }
                query(&mut stream, &stop, &request(1, "46", "R00000", 1));
                stream.write_all(&reply(1, "46", "0007")).unwrap();
                query(&mut stream, &stop, &request(1, command, variable, 1));
                stream.write_all(&bad).unwrap();
                no_more(&mut stream, &stop);
                assert_eq!(
                    listener.accept().unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
                if prior { 4_u64 } else { 2 }
            });
            let mut config = source(
                &address.to_string(),
                json!([
                    {"name":"first","address":"R00000"},
                    {"name":"second","address":variable,"data_type":if boolean { "bool" } else { "i16" }}
                ]),
            );
            if prior {
                config["every_ms"] = json!(time::ms(200));
            }
            assert_eq!(run(&home, &[config], "1s").failure(1)["code"], "failed");
            assert_eq!(device.finish(), if prior { 4 } else { 2 });
            let expected = if prior {
                vec![
                    ("first".into(), json!(7)),
                    (
                        "second".into(),
                        if boolean { json!(true) } else { json!(32767) },
                    ),
                ]
            } else {
                vec![]
            };
            if !prior {
                assert_unpublished(&home);
            }
            assert_output(&home, &expected, false, expected.len());
        }
    }
}

#[test]
fn fatek_request_and_acquisition_deadlines_are_shared_and_terminal() {
    for acquisition in [false, true] {
        let home = Home::new(&format!("fatek-deadline-{acquisition}"));
        let (listener, address) = listener();
        let device = Device::start(move |stop| {
            let Some(mut stream) = accept(&listener, &stop) else {
                return 0;
            };
            query(&mut stream, &stop, &request(1, "46", "R00000", 1));
            stream.write_all(&reply(1, "46", "0007")).unwrap();
            query(
                &mut stream,
                &stop,
                &request(1, "46", if acquisition { "R00000" } else { "R00001" }, 1),
            );
            if acquisition {
                stream.write_all(b"\x0201460").unwrap();
            } else {
                for part in reply(1, "46", "0007").chunks(5) {
                    thread::sleep(time::millis(100));
                    match stream.write_all(part) {
                        Ok(()) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                            ) =>
                        {
                            break;
                        }
                        Err(error) => panic!("deadline peer write: {error}"),
                    }
                }
            }
            no_more(&mut stream, &stop);
            2_u64
        });
        let signals = if acquisition {
            json!([{"name":"first","address":"R00000"}])
        } else {
            json!([{"name":"first","address":"R00000"},{"name":"second","address":"R00001"}])
        };
        let mut config = source(&address.to_string(), signals);
        config["timeout_ms"] = json!(time::ms(if acquisition { 1000 } else { 250 }));
        if acquisition {
            config["every_ms"] = json!(time::ms(200));
        }
        assert_eq!(
            run(&home, &[config], if acquisition { "450ms" } else { "1s" }).failure(4)["code"],
            "timeout"
        );
        assert_eq!(device.finish(), 2);
        let expected = if acquisition {
            vec![("first".into(), json!(7))]
        } else {
            vec![]
        };
        assert_output(&home, &expected, false, expected.len());
    }
}

#[test]
fn fatek_eof_after_completed_poll_is_disconnected_without_partial_republication() {
    let home = Home::new("fatek-eof");
    let (listener, address) = listener();
    let device = Device::start(move |stop| {
        let Some(mut stream) = accept(&listener, &stop) else {
            return 0;
        };
        query(&mut stream, &stop, &request(1, "46", "R00000", 1));
        stream.write_all(&reply(1, "46", "0007")).unwrap();
        query(&mut stream, &stop, &request(1, "46", "R00000", 1));
        stream.write_all(b"\x0201460").unwrap();
        2_u64
    });
    let mut config = source(
        &address.to_string(),
        json!([{"name":"word","address":"R00000"}]),
    );
    config["every_ms"] = json!(time::ms(200));
    assert_eq!(
        run(&home, &[config], "1s").failure(3)["code"],
        "disconnected"
    );
    assert_eq!(device.finish(), 2);
    assert_output(&home, &[("word".into(), json!(7))], false, 1);
}

#[test]
fn fatek_invalid_profile_address_count_station_and_serial_settings_fail_before_connect() {
    let (listener, address) = listener();
    let base = source(
        &address.to_string(),
        json!([{"name":"word","address":"R00012"}]),
    );
    let mut cases = Vec::new();
    for (key, value) in [
        ("profile", json!("fbs_usb")),
        ("unit", json!(0)),
        ("unit", json!(255)),
        ("address", json!("127.0.0.1:0")),
        ("address", json!("0.0.0.0:500")),
        ("address", json!("224.0.0.1:500")),
        ("signals", json!([])),
        (
            "signals",
            json!(vec![json!({"name":"same","address":"R00012"}); 257]),
        ),
    ] {
        let mut config = base.clone();
        config[key] = value;
        cases.push(config);
    }
    for signal in [
        json!({"name":"word","address":"R00012","unit":0}),
        json!({"name":"word","address":"R00012","unit":255}),
        json!({"name":"word","address":"R00012","elements":0}),
        json!({"name":"word","address":"R00012","elements":65}),
        json!({"name":"word","address":"DR00012","elements":33,"data_type":"u32"}),
        json!({"name":"word","address":"X0000","elements":257,"data_type":"bool"}),
        json!({"name":"word","address":"DR65535","data_type":"u32"}),
        json!({"name":"word","address":"R65535","elements":2}),
        json!({"name":"word","address":"DRC9998","elements":2,"data_type":"i32"}),
        json!({"name":"word","address":"X9999","elements":2,"data_type":"bool"}),
        json!({"name":"word","address":"WX0001"}),
        json!({"name":"word","address":"DWX0012","data_type":"u32"}),
        json!({"name":"word","address":"WX9992"}),
        json!({"name":"word","address":"DWX9976","data_type":"u32"}),
        json!({"name":"word","address":"WX0000","data_type":"u32"}),
        json!({"name":"word","address":"DWX0000"}),
        json!({"name":"word","address":"r00012"}),
        json!({"name":"word","address":"R12"}),
        json!({"name":"word","address":"R0000A"}),
        json!({"name":"word","address":"R00012","data_type":"f32"}),
    ] {
        let mut config = base.clone();
        config["signals"] = json!([signal]);
        cases.push(config);
    }
    for setting in [
        json!({"device":""}),
        json!({"device":"bad\0path"}),
        json!({"device":"missing","baud":4800}),
        json!({"device":"missing","baud":691_200}),
        json!({"device":"missing","parity":"none"}),
        json!({"device":"missing","data_bits":8}),
        json!({"device":"missing","stop_bits":2}),
    ] {
        let mut config = base.clone();
        config.as_object_mut().unwrap().remove("address");
        config["serial"] = setting;
        cases.push(config);
    }
    let mut both = base.clone();
    both["serial"] = json!({"device":"missing"});
    cases.push(both);
    let mut neither = base.clone();
    neither.as_object_mut().unwrap().remove("address");
    cases.push(neither);
    let mut duplicate = base;
    duplicate["signals"] =
        json!([{"name":"same","address":"R00000"},{"name":"same","address":"R00001"}]);
    cases.push(duplicate);
    for (index, config) in cases.into_iter().enumerate() {
        let home = Home::new(&format!("fatek-config-{index}"));
        assert_eq!(run(&home, &[config], "20ms").failure(2)["code"], "usage");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
