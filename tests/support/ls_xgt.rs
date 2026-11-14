//! Native LS ELECTRIC `FEnet` CLI fixtures; the first exchange is the literal
//! XBL-EMTA v2.1 manual's tables 5-6/5-7 (printed p. 5-7).
use super::{Device, Home, accept, assert_unpublished, listener, receive};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    sync::atomic::AtomicBool,
    thread,
    time::Instant,
};

const VENDOR_REQUEST: &[u8] =
    b"LSIS-XGT\0\0\0\0\xa0\x33\0\0\x0e\0\0\x3c\x54\0\x02\0\0\0\x01\0\x04\0%MW0";
const VENDOR_RESPONSE: &[u8] =
    b"LSIS-XGT\0\0\x11\x01\xa0\x11\0\0\x0e\0\x03\x2f\x55\0\x02\0\x08\x01\0\0\x01\0\x02\0\0\0";

fn config(address: std::net::SocketAddr, signals: &Value) -> Value {
    json!({"id":"ls","protocol":"ls_xgt","address":address.to_string(),
        "every_ms":3_600_000,"timeout_ms":time::ms(1000),"signals":signals})
}
fn record(home: &Home, config: &Value) -> super::support::cli::Run {
    record_for(home, config, "1s")
}
fn record_for(home: &Home, config: &Value, duration: &str) -> super::support::cli::Run {
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
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
}
fn request(stream: &mut TcpStream, stop: &AtomicBool) -> Option<Vec<u8>> {
    let mut header = [0; 20];
    if !receive(stream, &mut header, stop) {
        return None;
    }
    assert_eq!(&header[..10], b"LSIS-XGT\0\0");
    assert_eq!(&header[10..12], &[0, 0]);
    assert_eq!(header[13], 0x33);
    assert_eq!(header[18], 0);
    assert_eq!(header[19], checksum(&header[..19]));
    let length = usize::from(u16::from_le_bytes([header[16], header[17]]));
    assert!((14..=28).contains(&length));
    let mut result = header.to_vec();
    result.resize(20 + length, 0);
    assert!(receive(stream, &mut result[20..], stop));
    Some(result)
}
fn expect(
    stream: &mut TcpStream,
    stop: &AtomicBool,
    invoke: u16,
    cpu: u8,
    wire_type: u8,
    label: &str,
    count: Option<u16>,
) {
    let received = request(stream, stop).unwrap();
    assert!(received.len() >= 20);
    assert_eq!(received[12], cpu);
    assert_eq!(&received[14..16], &invoke.to_le_bytes());
    let mut expected = vec![0x54, 0, wire_type, 0, 0, 0, 1, 0];
    expected.extend_from_slice(&u16::try_from(label.len()).unwrap().to_le_bytes());
    expected.extend_from_slice(label.as_bytes());
    if let Some(count) = count {
        expected.extend_from_slice(&count.to_le_bytes());
    }
    assert_eq!(received[20..], expected);
}
fn reply(invoke: u16, cpu: u8, wire_type: u8, data: &[u8]) -> Vec<u8> {
    let mut response = b"LSIS-XGT\0\0\x11\x01\0\x11\0\0\0\0\x03\0".to_vec();
    response[12] = cpu;
    response[14..16].copy_from_slice(&invoke.to_le_bytes());
    response.extend_from_slice(&[0x55, 0, wire_type, 0, 8, 1, 0, 0, 1, 0]);
    response.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
    response.extend_from_slice(data);
    fix_header(&mut response);
    response
}
fn fix_header(response: &mut [u8]) {
    assert!(response.len() >= 20);
    let length = u16::try_from(response.len() - 20).unwrap();
    response[16..18].copy_from_slice(&length.to_le_bytes());
    response[19] = checksum(&response[..19]);
}
fn send(stream: &mut TcpStream, bytes: &[u8]) {
    for part in bytes.chunks(3) {
        stream.write_all(part).unwrap();
    }
}
fn no_more_requests(stream: &mut TcpStream) {
    // accept() already sets a short read timeout. Do not change socket options
    // after the recorder has closed: macOS may reject that with EINVAL.
    let deadline = Instant::now() + time::secs(1);
    let mut byte = [0];
    loop {
        assert!(
            Instant::now() < deadline,
            "recorder did not close after failed poll"
        );
        match stream.read(&mut byte) {
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
            other => panic!("no later request after failed poll: {other:?}"),
        }
    }
}
fn values(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .map(|row| row["value"].clone())
        .collect()
}

#[test]
fn ls_xgt_replays_literal_vendor_read_and_documented_reserved_fields() {
    let home = Home::new("ls-xgt-vendor");
    let (listener, address) = listener();
    let config = config(
        address,
        &json!([
            {"name":"word","variable":"%MW0"},
            {"name":"continuous","variable":"%MB0"},
            {"name":"bit","variable":"%MX2764","data_type":"bool"}
        ]),
    );
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        assert_eq!(request(&mut stream, &stop).unwrap(), VENDOR_REQUEST);
        send(&mut stream, VENDOR_RESPONSE);
        expect(&mut stream, &stop, 1, 0xa0, 0x14, "%MB0", Some(2));
        // Same continuous-response layout as table 5-9, with this request's Invoke ID.
        send(&mut stream, &reply(1, 0xa0, 0x14, &[0, 0]));
        expect(&mut stream, &stop, 2, 0xa0, 0, "%MX2764", None);
        send(&mut stream, &reply(2, 0xa0, 0, &[0xff]));
        3
    });
    assert_eq!(record(&home, &config).success()["samples"], 3);
    assert_eq!(server.finish(), 3);
    assert_eq!(values(&home), [json!(0), json!(0), json!(true)]);
}

#[test]
fn ls_xgt_reads_exact_types_segments_byte_ranges_and_exports() {
    let home = Home::new("ls-xgt-types");
    let (listener, address) = listener();
    let mut config = config(
        address,
        &json!([
            {"name":"wide","variable":"%DB19999999996","data_type":"u64","elements":2},
            {"name":"signed","variable":"%ML0","data_type":"i64"},
            {"name":"float","variable":"%MD0","data_type":"f32","scale":2,"offset":-1},
            {"name":"bytes","variable":"%MB20","data_type":"i8","elements":3},
            {"name":"native_byte","variable":"%MB100","data_type":"u8"}
        ]),
    );
    config["max_read_bytes"] = serde_json::from_str("8.0e0").unwrap();
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        let reads = [
            (
                0,
                0x14,
                "%DB19999999996",
                Some(8),
                u64::MAX.to_le_bytes().to_vec(),
            ),
            (
                1,
                0x14,
                "%DB20000000004",
                Some(8),
                ((1_u64 << 53) + 1).to_le_bytes().to_vec(),
            ),
            (2, 4, "%ML0", None, i64::MIN.to_le_bytes().to_vec()),
            (3, 3, "%MD0", None, 1.25_f32.to_le_bytes().to_vec()),
            (4, 0x14, "%MB20", Some(3), vec![0x80, 0xff, 0x7f]),
            (5, 1, "%MB100", None, vec![255]),
        ];
        for (invoke, kind, label, count, data) in reads {
            expect(&mut stream, &stop, invoke, 0xa0, kind, label, count);
            send(&mut stream, &reply(invoke, 0xa0, kind, &data));
        }
        6
    });
    assert_eq!(record(&home, &config).success()["samples"], 5);
    assert_eq!(server.finish(), 6);
    assert_eq!(
        values(&home),
        [
            json!([u64::MAX, (1_u64 << 53) + 1]),
            json!(i64::MIN),
            json!(1.5),
            json!([-128, -1, 127]),
            json!(255)
        ]
    );
    for (format, output) in [("csv", "out.csv"), ("jsonl", "out.jsonl")] {
        assert_eq!(
            home.run(&[
                "--json",
                "data",
                "export",
                "run.jsonl",
                "--format",
                format,
                "-o",
                output
            ])
            .success()["samples"],
            5
        );
        let text = fs::read_to_string(home.path(output)).unwrap();
        assert!(text.contains("[18446744073709551615,9007199254740993]"));
        assert!(text.contains("-9223372036854775808"));
    }
}

#[test]
fn ls_xgt_iec_literal_io_variables_and_all_cpu_identifiers() {
    for (cpu, code, variable, kind) in [
        ("xgk", 0xa0, "%MW0", 2),
        ("xgi", 0xa4, "%IW0.0.0", 2),
        ("xgi", 0xa4, "%IB0.0.0", 1),
        ("xgr", 0xa8, "%QW0.0.0", 2),
        ("xgb_mk", 0xb0, "%MW0", 2),
        ("xgb_iec", 0xb4, "%UW00.00", 2),
    ] {
        let home = Home::new("ls-xgt-cpu");
        let (listener, address) = listener();
        let mut config = config(
            address,
            &json!([{"name":"value","variable":variable,"data_type":if kind == 1 {"u8"} else {"u16"}}]),
        );
        config["cpu"] = json!(cpu);
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            expect(&mut stream, &stop, 0, code, kind, variable, None);
            let mut response = reply(
                0,
                0xa0,
                kind,
                if kind == 1 { &[255] } else { &[0x34, 0x12] },
            );
            // CPU info and position are server metadata, not request echoes.
            // Reserved zero BCC is supported; Invoke ID and framing remain strict.
            response[19] = 0;
            send(&mut stream, &response);
            1
        });
        assert_eq!(record(&home, &config).success()["samples"], 1);
        assert_eq!(server.finish(), 1);
        assert_eq!(values(&home), [json!(if kind == 1 { 255 } else { 0x1234 })]);
    }
}

#[test]
fn ls_xgt_rejects_malformed_or_target_rejected_siblings_atomically() {
    for (mode, diagnostic) in [
        ("company", "company"),
        ("direction", "direction"),
        ("invoke", "Invoke ID"),
        ("checksum", "checksum"),
        ("required_checksum", "checksum"),
        ("command", "command"),
        ("type", "data type"),
        ("blocks", "block count"),
        ("count", "data count"),
        ("extent", "application length"),
        ("truncated", "I/O"),
        ("error8", "PLC rejected"),
        ("error16", "PLC rejected"),
        ("malformed_error", "error response width"),
        ("nonfinite", "non-finite"),
    ] {
        let home = Home::new("ls-xgt-malformed");
        let (listener, address) = listener();
        let kind = if mode == "nonfinite" { 3 } else { 2 };
        let mut config = config(
            address,
            &json!([
                {"name":"good","variable":"%MW0"},
                {"name":"bad","variable":if mode == "nonfinite" {"%MD0"} else {"%MW1"},"data_type":if mode == "nonfinite" {"f32"} else {"u16"}}
            ]),
        );
        config["require_header_checksum"] = json!(mode == "required_checksum");
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            expect(&mut stream, &stop, 0, 0xa0, 2, "%MW0", None);
            send(&mut stream, VENDOR_RESPONSE);
            expect(
                &mut stream,
                &stop,
                1,
                0xa0,
                kind,
                if kind == 3 { "%MD0" } else { "%MW1" },
                None,
            );
            let mut response = reply(
                1,
                0xa0,
                kind,
                if kind == 3 {
                    &[0, 0, 0x80, 0x7f]
                } else {
                    &[42, 0]
                },
            );
            match mode {
                "company" => response[0] = b'G',
                "direction" => response[13] = 0x33,
                "invoke" => response[14] = 2,
                "command" => response[20] = 0x59,
                "type" => response[22] = 1,
                "blocks" => response[28] = 2,
                "count" => response[30] = 1,
                "extent" => response.push(0),
                "error8" | "error16" | "malformed_error" => {
                    response.truncate(28);
                    response[26] = 0xff;
                    response[27] = 0xff;
                    response.push(0x21);
                    if mode != "error8" {
                        response.push(0);
                    }
                    if mode == "malformed_error" {
                        response.push(0);
                    }
                }
                _ => {}
            }
            fix_header(&mut response);
            if mode == "checksum" {
                response[19] = response[19].wrapping_add(1).max(1);
            }
            if mode == "required_checksum" {
                response[19] = 0;
            }
            if mode == "truncated" {
                response.pop();
            }
            stream.write_all(&response).unwrap();
            2
        });
        let error = record(&home, &config).failure(if mode == "truncated" { 3 } else { 1 });
        assert_eq!(server.finish(), 2);
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if matches!(mode, "error8" | "error16") {
            assert_eq!(error["details"]["ls_xgt_error_code"], 0x21);
            assert_eq!(error["details"]["ls_xgt_status"], 65535);
        }
        assert_unpublished(&home);
    }
}

#[test]
fn ls_xgt_continuations_share_the_original_poll_deadline() {
    let home = Home::new("ls-xgt-deadline");
    let (listener, address) = listener();
    let mut config = config(
        address,
        &json!([{"name":"wide","variable":"%DB0","data_type":"u64","elements":3}]),
    );
    config["max_read_bytes"] = json!(8);
    config["timeout_ms"] = json!(time::ms(350));
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        expect(&mut stream, &stop, 0, 0xa0, 0x14, "%DB0", Some(8));
        thread::sleep(time::millis(220));
        send(&mut stream, &reply(0, 0xa0, 0x14, &1_u64.to_le_bytes()));
        expect(&mut stream, &stop, 1, 0xa0, 0x14, "%DB8", Some(8));
        thread::sleep(time::millis(220));
        let _ = stream.write_all(&reply(1, 0xa0, 0x14, &2_u64.to_le_bytes()));
        no_more_requests(&mut stream);
        2
    });
    let error = record(&home, &config).failure(4);
    assert_eq!(server.finish(), 2);
    assert_eq!(error["code"], "timeout");
    assert_unpublished(&home);
}

#[test]
fn ls_xgt_stops_before_another_signal_when_serialized_poll_is_full() {
    let home = Home::new("ls-xgt-budget");
    let (listener, address) = listener();
    let signals = (0..14).map(|index| json!({"name":format!("wide_{index:02}"),"variable":"%MB0","data_type":"u64","elements":4096})).collect::<Vec<_>>();
    let mut config = config(address, &json!(signals));
    config["max_read_bytes"] = json!(1400);
    config["timeout_ms"] = json!(10000);
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        let mut invoke = 0;
        for _ in 0..13 {
            let mut done = 0;
            while done < 4096 {
                let elements = (4096 - done).min(175);
                let count = u16::try_from(elements * 8).unwrap();
                expect(
                    &mut stream,
                    &stop,
                    invoke,
                    0xa0,
                    0x14,
                    &format!("%MB{}", done * 8),
                    Some(count),
                );
                // Large replies use one write; the separate vendor fixture tests tiny fragments.
                stream
                    .write_all(&reply(invoke, 0xa0, 0x14, &vec![255; usize::from(count)]))
                    .unwrap();
                done += elements;
                invoke += 1;
            }
        }
        no_more_requests(&mut stream);
        u64::from(invoke)
    });
    let error = record_for(&home, &config, "5s").failure(1);
    assert_eq!(server.finish(), 13 * 24);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("1 MiB batch limit"),
        "{error}"
    );
    assert_unpublished(&home);
}

#[test]
fn ls_xgt_rejects_invalid_counts_variables_widths_and_scaling() {
    let valid = json!({"name":"value","variable":"%MW0"});
    for signal in [
        json!({"name":"value","variable":"%MW0","elements":2}),
        json!({"name":"value","variable":"%MX0"}),
        json!({"name":"value","variable":"%MW0","data_type":"bool"}),
        json!({"name":"value","variable":"%DD0","data_type":"u32"}),
        json!({"name":"value","variable":"%MB0","elements":4097}),
        json!({"name":"value","variable":"%MB0","elements":0}),
        json!({"name":"value","variable":"%MB0","elements":1.1}),
        json!({"name":"value","variable":"%MB0","data_type":"u64","scale":2}),
        json!({"name":"value","variable":"%MZ0"}),
        json!({"name":"value","variable":"%MW-1"}),
        json!({"name":"value","variable":"%MW0;RESET"}),
        json!({"name":"value","variable":"%MW0.0"}),
        json!({"name":"value","variable":"%MW10000000000000"}),
        json!({"name":"value","variable":"%MB9999999999999","data_type":"u8","elements":2}),
    ] {
        let home = Home::new("ls-xgt-invalid");
        let config = config("127.0.0.1:1".parse().unwrap(), &json!([signal]));
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
    }
    for (field, value) in [
        ("max_read_bytes", json!(0)),
        ("max_read_bytes", json!(1)),
        ("max_read_bytes", json!(1401)),
        ("max_read_bytes", json!(1.25)),
        ("cpu", json!("unknown")),
    ] {
        let home = Home::new("ls-xgt-invalid-config");
        let mut config = config("127.0.0.1:1".parse().unwrap(), &json!([valid]));
        config[field] = value;
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
    }
}
