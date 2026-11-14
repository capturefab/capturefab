//! Manual-layout RTDE peers over private loopback; no real robot or `URSim` claim.
use super::{Device, Home, accept, listener, receive, support::cli::Run};
use crate::support::time;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read, Write as _},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

fn packet(command: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = u16::try_from(payload.len() + 3)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    bytes.push(command);
    bytes.extend_from_slice(payload);
    bytes
}
fn hex(text: &str) -> Vec<u8> {
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    assert_eq!(tail, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn all_types() -> Vec<Value> {
    serde_json::from_str::<Value>(include_str!("../fixtures/ur_rtde/all-types.json")).unwrap()["fields"].as_array().unwrap().clone()
}
fn scalar() -> Vec<Value> {
    vec![
        json!({"field":"timestamp","wire_type":"DOUBLE","hex":"3FF8000000000000","value":{"ieee754_hex":"3FF8000000000000","classification":"finite","value":1.5}}),
    ]
}
fn declarations(fields: &[Value]) -> Value {
    json!(
        fields
            .iter()
            .map(|field| json!({"field":field["field"],"wire_type":field["wire_type"]}))
            .collect::<Vec<_>>()
    )
}
fn output(fields: &[Value]) -> Vec<u8> {
    let mut payload = vec![7];
    for field in fields {
        payload.extend(hex(field["hex"].as_str().unwrap()));
    }
    packet(b'U', &payload)
}
fn types(fields: &[Value], recipe: u8) -> Vec<u8> {
    let mut payload = vec![recipe];
    payload.extend(
        fields
            .iter()
            .map(|field| field["wire_type"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join(",")
            .bytes(),
    );
    packet(b'O', &payload)
}
fn query(stream: &mut (impl Read + std::os::fd::AsRawFd), stop: &AtomicBool, expected: &[u8]) {
    let mut bytes = vec![0; expected.len()];
    assert!(receive(stream, &mut bytes, stop));
    assert_eq!(bytes, expected);
}
fn no_more(stream: &mut (impl Read + std::os::fd::AsRawFd), stop: &AtomicBool) {
    let deadline = Instant::now() + time::secs(10);
    loop {
        match stream.read(&mut [0]) {
            Ok(0) => return,
            Ok(_) => panic!("unexpected RTDE control/input/reconnect bytes"),
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
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                assert!(Instant::now() < deadline, "RTDE owner stayed open");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("RTDE peer read: {error}"),
        }
    }
}
const VERSION: &[u8] = b"\x00\x13v\x00\x00\x00\x05\x00\x00\x00\x19\x00\x00\x00\x00\x00\x00\x04\xd2";
fn hello(stream: &mut impl ReadWrite, stop: &AtomicBool, fields: &[Value], pre_text: bool) {
    query(stream, stop, b"\x00\x05V\x00\x02");
    if pre_text {
        stream.write_all(b"\x00\x07M\x02old").unwrap();
    }
    stream.write_all(b"\x00\x04V\x01").unwrap();
    query(stream, stop, b"\x00\x03v");
    stream.write_all(VERSION).unwrap();
    let mut payload = hex("405F400000000000");
    payload.extend(
        fields
            .iter()
            .map(|field| field["field"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join(",")
            .bytes(),
    );
    query(stream, stop, &packet(b'O', &payload));
}
trait ReadWrite: Read + io::Write + std::os::fd::AsRawFd {}
impl<T: Read + io::Write + std::os::fd::AsRawFd> ReadWrite for T {}
fn start(stream: &mut impl ReadWrite, stop: &AtomicBool, fields: &[Value]) {
    hello(stream, stop, fields, false);
    stream.write_all(&types(fields, 7)).unwrap();
    query(stream, stop, b"\x00\x03S");
    stream.write_all(b"\x00\x04S\x01").unwrap();
}
fn source(address: &str, fields: &[Value]) -> Value {
    json!({"id":"robot","protocol":"ur_rtde","profile":"rtde_v2","address":address,"fields":declarations(fields),"every_ms":3_600_000})
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
fn rows(home: &Home, file: &str) -> Vec<Value> {
    fs::read_to_string(home.path(file))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn raw(bytes: &[u8]) -> Value {
    json!({"encoding":"base64","bytes":bytes.len(),"data":STANDARD.encode(bytes)})
}
fn software() -> Value {
    json!({"major":5,"minor":25,"bugfix":0,"build":1234})
}
fn state(fields: &[Value], index: u64) -> Value {
    json!({"kind":"ur_rtde_output","protocol_version":2,"controller_software":software(),"recipe_id":7,"requested_frequency_hz":125.0,"host_output_index":index,
        "fields":fields.iter().map(|field|json!({"field":field["field"],"wire_type":field["wire_type"],"value":field["value"]})).collect::<Vec<_>>(),"raw":raw(&output(fields))})
}
fn text(version: u16, frame: &[u8], message: &[u8], source: &[u8], level: u8, name: &str) -> Value {
    json!({"kind":"ur_rtde_text","protocol_version":2,"controller_software":software(),"message_protocol_version":version,
        "level_raw":level,"level":name,"message_base64":STANDARD.encode(message),"source_base64":STANDARD.encode(source),"raw":raw(frame)})
}
fn check(home: &Home, expected: &[Value], complete: bool, total: usize) {
    let recorded = rows(home, "run.jsonl");
    let end = recorded.last().unwrap();
    assert_eq!(end["status"], if complete { "complete" } else { "failed" });
    assert_eq!(end["samples"], total);
    let samples = recorded
        .iter()
        .filter(|row| row["kind"] == "sample")
        .collect::<Vec<_>>();
    let robot = samples
        .iter()
        .filter(|row| row["source"] == "robot")
        .collect::<Vec<_>>();
    assert_eq!(robot.len(), expected.len());
    let mut previous = 0;
    for (row, value) in robot.iter().zip(expected) {
        assert_eq!(row["signal"], "state");
        assert_eq!(&row["value"], value);
        assert!(row.get("source_time").is_none());
        for key in ["elapsed_ns", "read_duration_ns", "delivery_latency_ns"] {
            assert!(row[key].is_u64());
        }
        let elapsed = row["elapsed_ns"].as_u64().unwrap();
        assert!(elapsed >= previous);
        previous = elapsed;
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
    let exported = rows(home, "jsonl");
    assert_eq!(exported.len(), samples.len());
    for (actual, original) in exported.iter().zip(samples) {
        let mut projected = original.clone();
        assert_eq!(
            projected.as_object_mut().unwrap().remove("kind"),
            Some(json!("sample"))
        );
        assert_eq!(actual, &projected);
    }
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let lines = lines.collect::<Vec<_>>();
    assert_eq!(lines.len(), total);
    let values = lines
        .iter()
        .filter(|line| line.split(',').nth(1) == Some("\"robot\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        expected
            .iter()
            .map(|value| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
            .collect::<Vec<_>>()
    );
}

#[test]
fn ur_rtde_all_ten_types_async_text_and_early_output_keep_raw_clocks_and_exports() {
    let home = Home::new("rtde-types");
    let (listener, address) = listener();
    let fields = all_types();
    let expected_fields = fields.clone();
    let data = output(&fields);
    assert_eq!(data.len(), 150);
    let device = Device::start(move |stop| {
        let Some(mut stream) = accept(&listener, &stop) else {
            return 0;
        };
        hello(&mut stream, &stop, &fields, true);
        stream.write_all(&types(&fields, 7)).unwrap();
        query(&mut stream, &stop, b"\x00\x03S");
        let mut batch = b"\x00\x0eM\x04busy\x04RTDE\x02".to_vec();
        batch.extend(&data);
        batch.extend(b"\x00\x04S\x01");
        batch.extend(&data);
        for part in batch.chunks(11) {
            stream.write_all(part).unwrap();
        }
        query(&mut stream, &stop, b"\x00\x03P");
        // Valid output while seeking the pause ACK is cleanup traffic, not a
        // fresh post-cutoff observation. The original earlier packets remain.
        let mut tail = data;
        tail.extend(b"\x00\x04P\x01");
        stream.write_all(&tail).unwrap();
        no_more(&mut stream, &stop);
        5_u64
    });
    let sources = [
        source(&address.to_string(), &expected_fields),
        json!({"id":"counter","protocol":"simulator","signal":"cycle","every_ms":3_600_000}),
    ];
    assert_eq!(run(&home, &sources, "400ms").success()["samples"], 5);
    assert_eq!(device.finish(), 5);
    check(
        &home,
        &[
            text(1, b"\x00\x07M\x02old", b"old", b"", 2, "warning"),
            text(
                2,
                b"\x00\x0eM\x04busy\x04RTDE\x02",
                b"busy",
                b"RTDE",
                2,
                "warning",
            ),
            state(&expected_fields, 0),
            state(&expected_fields, 1),
        ],
        true,
        5,
    );
}

#[test]
fn ur_rtde_accepted_subscription_can_finish_healthy_before_first_output() {
    let home = Home::new("rtde-healthy-idle");
    let (listener, address) = listener();
    let fields = scalar();
    let peer_fields = fields.clone();
    let device = Device::start(move |stop| {
        let Some(mut stream) = accept(&listener, &stop) else {
            return 0;
        };
        start(&mut stream, &stop, &peer_fields);
        query(&mut stream, &stop, b"\x00\x03P");
        stream.write_all(b"\x00\x04P\x01").unwrap();
        no_more(&mut stream, &stop);
        5_u64
    });
    assert_eq!(
        run(&home, &[source(&address.to_string(), &fields)], "200ms").success()["samples"],
        0
    );
    assert_eq!(device.finish(), 5);
    check(&home, &[], true, 0);
}

#[test]
fn ur_rtde_setup_refusals_versions_types_and_async_caps_publish_nothing() {
    for mode in 0..10 {
        let home = Home::new(&format!("rtde-setup-{mode}"));
        let (listener, address) = listener();
        let fields = scalar();
        let peer_fields = fields.clone();
        let device = Device::start(move |stop| {
            let Some(mut stream) = accept(&listener, &stop) else {
                return 0;
            };
            if mode <= 2 {
                query(&mut stream, &stop, b"\x00\x05V\x00\x02");
                stream
                    .write_all(match mode {
                        0 => b"\x00\x04V\x00",
                        1 => b"\x00\x04V\x02",
                        _ => b"\x00\x06M\x01no",
                    })
                    .unwrap();
                no_more(&mut stream, &stop);
                return 1;
            }
            if mode == 3 {
                query(&mut stream, &stop, b"\x00\x05V\x00\x02");
                stream.write_all(&b"\x00\x07M\x02old".repeat(33)).unwrap();
                no_more(&mut stream, &stop);
                return 1;
            }
            hello(&mut stream, &stop, &peer_fields, false);
            match mode {
                4 => stream.write_all(&types(&peer_fields, 0)).unwrap(),
                5 => stream.write_all(&packet(b'O', b"\x00NOT_FOUND")).unwrap(),
                6 => stream.write_all(&packet(b'O', b"\x07UINT64")).unwrap(),
                7 => stream
                    .write_all(&packet(b'O', b"\x07DOUBLE,DOUBLE"))
                    .unwrap(),
                8 => {
                    stream.write_all(&types(&peer_fields, 7)).unwrap();
                    query(&mut stream, &stop, b"\x00\x03S");
                    stream.write_all(b"\x00\x04S\x00").unwrap();
                }
                _ => stream.write_all(b"\x00\x06M\x00\x00\x01").unwrap(),
            }
            no_more(&mut stream, &stop);
            if mode == 8 { 4_u64 } else { 3 }
        });
        let unsupported = matches!(mode, 0 | 5);
        let exit = if unsupported { 5 } else { 1 };
        let code = if unsupported { "unsupported" } else { "failed" };
        assert_eq!(
            run(&home, &[source(&address.to_string(), &fields)], "1s").failure(exit)["code"],
            code
        );
        assert_eq!(
            device.finish(),
            if mode <= 3 {
                1
            } else if mode == 8 {
                4
            } else {
                3
            }
        );
        check(&home, &[], false, 0);
    }
}

#[test]
fn ur_rtde_bad_later_packets_keep_complete_prefix_and_never_reconnect_or_pause() {
    for (index, bad) in [
        packet(b'U', b"\x08\x3f\xf8\x00\x00\x00\x00\x00\x00"),
        packet(b'U', b"\x07\x3f\xf8"),
        packet(b'U', b"\x07\x3f\xf8\x00\x00\x00\x00\x00\x00X"),
        packet(b'I', b""),
        packet(b'M', b"\x01x\x00\x02extra"),
        packet(b'M', b"\x00\x00\x00"),
        vec![0x20, 0x01, b'U'],
        vec![0, 2, b'U'],
    ]
    .into_iter()
    .enumerate()
    {
        let home = Home::new(&format!("rtde-bad-{index}"));
        let (listener, address) = listener();
        let fields = scalar();
        let peer_fields = fields.clone();
        let device = Device::start(move |stop| {
            let Some(mut stream) = accept(&listener, &stop) else {
                return 0;
            };
            start(&mut stream, &stop, &peer_fields);
            let mut data = output(&peer_fields);
            data.extend(bad);
            stream.write_all(&data).unwrap();
            no_more(&mut stream, &stop);
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            4_u64
        });
        assert_eq!(
            run(&home, &[source(&address.to_string(), &fields)], "1s").failure(1)["code"],
            "failed"
        );
        assert_eq!(device.finish(), 4);
        check(&home, &[state(&fields, 0)], false, 1);
    }
}

#[test]
fn ur_rtde_setup_original_budget_and_partial_cutoffs_are_strict() {
    let home = Home::new("rtde-setup-timeout");
    let (listener, address) = listener();
    let device = Device::start(move |stop| {
        let Some(mut stream) = accept(&listener, &stop) else {
            return 0;
        };
        query(&mut stream, &stop, b"\x00\x05V\x00\x02");
        thread::sleep(time::millis(50));
        stream.write_all(b"\x00\x04V\x01").unwrap();
        query(&mut stream, &stop, b"\x00\x03v");
        thread::sleep(time::millis(50));
        stream.write_all(VERSION).unwrap();
        let mut payload = hex("405F400000000000");
        payload.extend(b"timestamp");
        query(&mut stream, &stop, &packet(b'O', &payload));
        thread::sleep(time::millis(200));
        match stream.write_all(&packet(b'O', b"\x07DOUBLE")) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) => {}
            Err(error) => panic!("late setup reply: {error}"),
        }
        no_more(&mut stream, &stop);
        3_u64
    });
    let fields = scalar();
    let mut config = source(&address.to_string(), &fields);
    config["timeout_ms"] = json!(time::ms(250));
    assert_eq!(run(&home, &[config], "1s").failure(4)["code"], "timeout");
    assert_eq!(device.finish(), 3);
    check(&home, &[], false, 0);
    for acquisition in [false, true] {
        let home = Home::new(&format!("rtde-partial-{acquisition}"));
        let (listener, address) = super::listener();
        let fields = scalar();
        let peer_fields = fields.clone();
        let device = Device::start(move |stop| {
            let Some(mut stream) = accept(&listener, &stop) else {
                return 0;
            };
            start(&mut stream, &stop, &peer_fields);
            let mut data = output(&peer_fields);
            data.extend([0, 12]);
            stream.write_all(&data).unwrap();
            no_more(&mut stream, &stop);
            4_u64
        });
        let mut config = source(&address.to_string(), &fields);
        config["timeout_ms"] = json!(time::ms(if acquisition { 1000 } else { 250 }));
        assert_eq!(
            run(&home, &[config], if acquisition { "450ms" } else { "1s" }).failure(4)["code"],
            "timeout"
        );
        assert_eq!(device.finish(), 4);
        check(&home, &[state(&fields, 0)], false, 1);
    }
}

#[test]
fn ur_rtde_eof_and_failed_pause_keep_the_original_complete_output() {
    for pause in [false, true] {
        let home = Home::new(&format!("rtde-end-{pause}"));
        let (listener, address) = listener();
        let fields = scalar();
        let peer_fields = fields.clone();
        let device = Device::start(move |stop| {
            let Some(mut stream) = accept(&listener, &stop) else {
                return 0;
            };
            start(&mut stream, &stop, &peer_fields);
            stream.write_all(&output(&peer_fields)).unwrap();
            if pause {
                query(&mut stream, &stop, b"\x00\x03P");
                stream.write_all(b"\x00\x04P\x00").unwrap();
                no_more(&mut stream, &stop);
                5_u64
            } else {
                4
            }
        });
        assert_eq!(
            run(&home, &[source(&address.to_string(), &fields)], "300ms").failure(if pause {
                1
            } else {
                3
            })["code"],
            if pause { "failed" } else { "disconnected" }
        );
        assert_eq!(device.finish(), if pause { 5 } else { 4 });
        check(&home, &[state(&fields, 0)], false, 1);
    }
}

#[test]
fn ur_rtde_invalid_fields_rates_profiles_and_endpoints_fail_before_connection() {
    let (listener, address) = listener();
    let fields = scalar();
    let base = source(&address.to_string(), &fields);
    let mut cases = Vec::new();
    for (key, value) in [
        ("profile", json!("rtde_v1")),
        ("address", json!("127.0.0.1:0")),
        ("address", json!("0.0.0.0:30004")),
        ("address", json!("224.0.0.1:30004")),
        ("frequency_hz", json!(0)),
        ("frequency_hz", json!(500.1)),
        ("frequency_hz", json!(-1)),
        ("fields", json!([])),
        ("signal", json!("")),
    ] {
        let mut config = base.clone();
        config[key] = value;
        cases.push(config);
    }
    for declaration in [
        json!([{"field":"bad,name","wire_type":"DOUBLE"}]),
        json!([{"field":"0bad","wire_type":"DOUBLE"}]),
        json!([{"field":"x","wire_type":"STRING"}]),
        json!([{"field":"same","wire_type":"BOOL"},{"field":"same","wire_type":"BOOL"}]),
        json!([{"field":"x".repeat(2049),"wire_type":"DOUBLE"}]),
        json!(
            (0..129)
                .map(|index| json!({"field":format!("f{index}"),"wire_type":"VECTOR6D"}))
                .collect::<Vec<_>>()
        ),
    ] {
        let mut config = base.clone();
        config["fields"] = declaration;
        cases.push(config);
    }
    let mut input = base;
    input["inputs"] = json!(["speed_slider_fraction"]);
    cases.push(input);
    for (index, config) in cases.into_iter().enumerate() {
        let home = Home::new(&format!("rtde-config-{index}"));
        assert_eq!(run(&home, &[config], "20ms").failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
