//! Yokogawa FA-M3 native TCP fixtures through the CLI. Only the first binary
//! exchange is a complete published vendor frame (IM34M6P41-01E p.1-6).
use super::{Device, Home, accept, assert_unpublished, listener, receive};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    sync::atomic::AtomicBool,
    thread,
    time::Instant,
};

const VENDOR_READ: [u8; 12] = [0x11, 1, 0, 8, 0, 0x18, 0, 0, 0, 0xd9, 0, 0x10];
const VENDOR_WORDS: [u16; 16] = [0xf0a5, 0, 0, 0, 0, 0, 0, 0x9876, 0, 0, 0, 0, 0, 0, 0, 0];
fn config(address: std::net::SocketAddr, signals: &Value) -> Value {
    json!({"id":"fam3","protocol":"yokogawa_fam3","address":address.to_string(),"every_ms":3_600_000,"timeout_ms":time::ms(1000),"signals":signals})
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
fn values(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample")
        .map(|row| row["value"].clone())
        .collect()
}
fn binary_request(stream: &mut TcpStream, stop: &AtomicBool, expected: [u8; 12]) {
    let mut actual = [0; 12];
    assert!(receive(stream, &mut actual, stop));
    assert_eq!(actual, expected);
}
fn command(cpu: u8, device: u16, address: u32, bits: bool, count: u16) -> [u8; 12] {
    let mut bytes = [
        if bits { 1 } else { 0x11 },
        cpu,
        0,
        8,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    bytes[4..6].copy_from_slice(&device.to_be_bytes());
    bytes[6..10].copy_from_slice(&address.to_be_bytes());
    bytes[10..].copy_from_slice(&count.to_be_bytes());
    bytes
}
fn ascii_request(stream: &mut TcpStream, stop: &AtomicBool, expected: &[u8]) {
    let mut actual = vec![0; expected.len()];
    assert!(receive(stream, &mut actual, stop));
    assert_eq!(actual, expected);
}
fn response(bits: bool, data: &[u8]) -> Vec<u8> {
    let mut frame = vec![if bits { 0x81 } else { 0x91 }, 0];
    frame.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(data);
    frame
}
fn words(values: &[u16]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}
fn send(stream: &mut TcpStream, bytes: &[u8]) {
    for part in bytes.chunks(3) {
        stream.write_all(part).unwrap();
    }
}
fn expect_eof(stream: &mut TcpStream) {
    // accept() already installed a short read timeout while the socket was
    // live. BSD can reject a late setsockopt after the client has closed it.
    let deadline = Instant::now() + time::secs(1);
    loop {
        assert!(
            Instant::now() < deadline,
            "owner did not close after failed poll"
        );
        match stream.read(&mut [0]) {
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
#[test]
fn yokogawa_fam3_replays_complete_published_word_read_and_relay_order() {
    let home = Home::new("fam3-vendor");
    let (listener, address) = listener();
    let cfg = config(
        address,
        &json!([
            {"name":"words","device":"X","address":217,"elements":16},
            {"name":"bits","device":"X","address":217,"data_type":"bool","elements":16}
        ]),
    );
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        binary_request(&mut stream, &stop, VENDOR_READ);
        send(&mut stream, &response(false, &words(&VENDOR_WORDS)));
        binary_request(&mut stream, &stop, command(1, 0x18, 217, true, 16));
        // The same published word's lowest bit is the first addressed relay.
        let bits = (0..16)
            .map(|index| u8::from(0xf0a5_u16 & (1 << index) != 0))
            .collect::<Vec<_>>();
        send(&mut stream, &response(true, &bits));
        2
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 2);
    assert_eq!(server.finish(), 2);
    assert_eq!(
        values(&home),
        [
            json!(VENDOR_WORDS),
            json!(
                (0..16)
                    .map(|index| 0xf0a5_u16 & (1 << index) != 0)
                    .collect::<Vec<_>>()
            )
        ]
    );
}
#[test]
fn yokogawa_fam3_binary_and_ascii_segment_exact_types_and_exports() {
    for ascii in [false, true] {
        let home = Home::new("fam3-types");
        let (listener, address) = listener();
        let mut cfg = config(
            address,
            &json!([
                {"name":"array","device":"D","address":123,"elements":65},
                {"name":"little","device":"D","address":200,"data_type":"u64","elements":2,"word_order":"little"},
                {"name":"big","device":"D","address":208,"data_type":"i64","word_order":"big"},
                {"name":"float","device":"D","address":212,"data_type":"f32","word_order":"little","scale":2,"offset":-1},
                {"name":"timer","device":"TP","address":499},
                {"name":"extended_relays","device":"E","address":2049,"data_type":"bool"},
                {"name":"extended_register","device":"R","address":1025}
            ]),
        );
        cfg["cpu"] = serde_json::from_str("4.0e0").unwrap();
        cfg["signals"][0]["address"] = serde_json::from_str("1.23e2").unwrap();
        cfg["signals"][0]["elements"] = serde_json::from_str("6.5e1").unwrap();
        cfg["encoding"] = json!(if ascii { "ascii" } else { "binary" });
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            for (device, label, address, data) in [
                (4, "D00123", 123, (0..64).collect::<Vec<u16>>()),
                (4, "D00187", 187, vec![65_535]),
                (
                    4,
                    "D00200",
                    200,
                    vec![65_535, 65_535, 65_535, 65_535, 1, 0, 0, 0x20],
                ),
                (4, "D00208", 208, vec![0x8000, 0, 0, 0]),
                (4, "D00212", 212, vec![0, 0x3fa0]),
                (0x21, "TP0499", 499, vec![1234]),
            ] {
                if ascii {
                    ascii_request(
                        &mut stream,
                        &stop,
                        format!("04WRD{label},{:02}\r\n", data.len()).as_bytes(),
                    );
                    let mut text = String::new();
                    for value in &data {
                        write!(&mut text, "{value:04X}").unwrap();
                    }
                    send(&mut stream, format!("14OK{text}\r\n").as_bytes());
                } else {
                    binary_request(
                        &mut stream,
                        &stop,
                        command(
                            4,
                            device,
                            address,
                            false,
                            u16::try_from(data.len()).unwrap(),
                        ),
                    );
                    send(&mut stream, &response(false, &words(&data)));
                }
            }
            if ascii {
                ascii_request(&mut stream, &stop, b"04BRDE02049,001\r\n");
                send(&mut stream, b"14OK1\r\n");
                ascii_request(&mut stream, &stop, b"04WRDR01025,01\r\n");
                send(&mut stream, b"14OK1234\r\n");
            } else {
                binary_request(&mut stream, &stop, command(4, 5, 2049, true, 1));
                send(&mut stream, &response(true, &[1]));
                binary_request(&mut stream, &stop, command(4, 0x12, 1025, false, 1));
                send(&mut stream, &response(false, &[0x12, 0x34]));
            }
            8
        });
        assert_eq!(record(&home, &cfg).success()["samples"], 7);
        assert_eq!(server.finish(), 8);
        let mut array = (0..64).collect::<Vec<u16>>();
        array.push(65_535);
        assert_eq!(
            values(&home),
            [
                json!(array),
                json!([u64::MAX, (1_u64 << 53) + 1]),
                json!(i64::MIN),
                json!(1.5),
                json!(1234),
                json!(true),
                json!(0x1234)
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
                7
            );
            let text = fs::read_to_string(home.path(output)).unwrap();
            assert!(text.contains("[18446744073709551615,9007199254740993]"));
            assert!(text.contains("-9223372036854775808"));
        }
    }
}
#[test]
fn yokogawa_fam3_io_holes_and_configured_link_bank_arrays() {
    let home = Home::new("fam3-addresses");
    let (listener, address) = listener();
    let mut cfg = config(
        address,
        &json!([
            {"name":"io","device":"Y","address":1664,"data_type":"bool","elements":258},
            {"name":"link","device":"L","address":10_017,"data_type":"bool","elements":300,"link_points":2048},
            {"name":"registers","device":"W","address":11_985,"elements":64,"link_points":2048}
        ]),
    );
    cfg["signals"][1]["link_points"] = serde_json::from_str("2.048e3").unwrap();
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        for (device, address, bits, count) in [
            (0x19, 1664, true, 256),
            (0x19, 10_464, true, 2),
            (0xc, 10_017, true, 256),
            (0xc, 10_273, true, 44),
            (0x17, 11_985, false, 64),
        ] {
            binary_request(&mut stream, &stop, command(1, device, address, bits, count));
            let data = if bits {
                (0..count).map(|index| u8::from(index % 2 == 0)).collect()
            } else {
                words(&vec![0x4321; usize::from(count)])
            };
            send(&mut stream, &response(bits, &data));
        }
        5
    });
    assert_eq!(record(&home, &cfg).success()["samples"], 3);
    assert_eq!(server.finish(), 5);
    let values = values(&home);
    assert_eq!(values.len(), 3);
    assert_eq!(values[0].as_array().unwrap().len(), 258);
    assert_eq!(values[0][256], true);
    assert_eq!(values[0][257], false);
    assert_eq!(values[1].as_array().unwrap().len(), 300);
    assert_eq!(values[2], json!(vec![0x4321; 64]));
}
#[test]
fn yokogawa_fam3_malformed_siblings_and_target_errors_are_atomic() {
    for (ascii, mode, reply, diagnostic) in [
        (false, "subheader", &[0x81, 0, 0, 2, 0, 42][..], "subheader"),
        (false, "length", &[0x91, 0, 1, 0][..], "parameter length"),
        (false, "truncated", &[0x91, 0, 0, 2, 0][..], "I/O"),
        (false, "bit", &[0x81, 0, 0, 1, 2][..], "bit reply"),
        (false, "error", &[0x91, 3, 0, 2, 7, 1][..], "PLC rejected"),
        (
            false,
            "error_cpu",
            &[0x91, 3, 0, 2, 7, 2][..],
            "CPU differs",
        ),
        (
            false,
            "nonfinite",
            &[0x91, 0, 0, 4, 0x7f, 0x80, 0, 0][..],
            "non-finite",
        ),
        (true, "cpu", &b"12OK002A\r\n"[..], "CPU/subheader"),
        (true, "status", &b"11NO002A\r\n"[..], "status or data width"),
        (true, "width", &b"11OK02A\r\n"[..], "data width"),
        (true, "hex", &b"11OKG02A\r\n"[..], "hex digit"),
        (true, "bit", &b"11OK2\r\n"[..], "bit must"),
        (true, "newline", &b"11OK002A\n"[..], "missing CRLF"),
        (true, "error", &b"11ER0307WRD\r\n"[..], "PLC rejected"),
        (
            true,
            "error_command",
            &b"11ER0307BWR\r\n"[..],
            "echoed command",
        ),
        (true, "bit_error", &b"11ER0307BRD\r\n"[..], "PLC rejected"),
        (
            true,
            "ignored_error_detail",
            &b"11ER02??WRD\r\n"[..],
            "PLC rejected",
        ),
    ] {
        let home = Home::new("fam3-malformed");
        let (listener, address) = listener();
        let bits = matches!(mode, "bit" | "bit_error");
        let floating = mode == "nonfinite";
        let mut cfg = config(
            address,
            &json!([
                {"name":"good","device":"D","address":1},
                {"name":"bad","device":if bits{"I"}else{"D"},"address":2,"data_type":if bits{"bool"}else if floating{"f32"}else{"u16"},"word_order":"big"}
            ]),
        );
        cfg["encoding"] = json!(if ascii { "ascii" } else { "binary" });
        let server = Device::start(move |stop| {
            let mut stream = accept(&listener, &stop).unwrap();
            if ascii {
                ascii_request(&mut stream, &stop, b"01WRDD00001,01\r\n");
                send(&mut stream, b"11OK002A\r\n");
                ascii_request(
                    &mut stream,
                    &stop,
                    if bits {
                        b"01BRDI00002,001\r\n"
                    } else {
                        b"01WRDD00002,01\r\n"
                    },
                );
            } else {
                binary_request(&mut stream, &stop, command(1, 4, 1, false, 1));
                send(&mut stream, &response(false, &[0, 42]));
                binary_request(
                    &mut stream,
                    &stop,
                    command(
                        1,
                        if bits { 9 } else { 4 },
                        2,
                        bits,
                        if floating { 2 } else { 1 },
                    ),
                );
            }
            stream.write_all(reply).unwrap();
            2
        });
        let error = record(&home, &cfg).failure(if mode == "truncated" { 3 } else { 1 });
        assert_eq!(server.finish(), 2);
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if matches!(mode, "error" | "bit_error") {
            assert_eq!(error["details"]["yokogawa_exit_code"], 3);
            assert_eq!(error["details"]["yokogawa_detail_code"], 7);
            assert_eq!(
                error["details"]["yokogawa_error_parameters"],
                if ascii {
                    json!(if bits {
                        b"07BRD".as_slice()
                    } else {
                        b"07WRD".as_slice()
                    })
                } else {
                    json!([7, 1])
                }
            );
        }
        if mode == "ignored_error_detail" {
            assert!(error["details"].get("yokogawa_detail_code").is_none());
        }
        assert_unpublished(&home);
    }
}
#[test]
fn yokogawa_fam3_fragmented_signal_retains_the_original_deadline() {
    let home = Home::new("fam3-deadline");
    let (listener, address) = listener();
    let mut cfg = config(
        address,
        &json!([{"name":"array","device":"D","address":1,"elements":65}]),
    );
    cfg["timeout_ms"] = json!(time::ms(350));
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        binary_request(&mut stream, &stop, command(1, 4, 1, false, 64));
        thread::sleep(time::millis(220));
        send(&mut stream, &response(false, &[0; 128]));
        binary_request(&mut stream, &stop, command(1, 4, 65, false, 1));
        thread::sleep(time::millis(220));
        let _ = stream.write_all(&response(false, &[0, 1]));
        expect_eof(&mut stream);
        2
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    assert_eq!(server.finish(), 2);
    assert_unpublished(&home);
}
#[test]
fn yokogawa_fam3_stops_before_next_signal_after_one_mib_poll_budget() {
    let home = Home::new("fam3-budget");
    let (listener, address) = listener();
    let signals = (0..14)
        .map(|index| json!({"name":format!("wide_{index:02}"),"device":"D","address":1,"data_type":"u64","word_order":"little","elements":4096}))
        .collect::<Vec<_>>();
    let mut cfg = config(address, &json!(signals));
    cfg["timeout_ms"] = json!(10_000);
    let server = Device::start(move |stop| {
        let mut stream = accept(&listener, &stop).unwrap();
        let packet = response(false, &[255; 128]);
        for _ in 0..13 {
            for offset in 0..256 {
                binary_request(
                    &mut stream,
                    &stop,
                    command(1, 4, offset * 64 + 1, false, 64),
                );
                stream.write_all(&packet).unwrap();
            }
        }
        expect_eof(&mut stream);
        13 * 256
    });
    let error = record_for(&home, &cfg, "10s").failure(1);
    assert_eq!(server.finish(), 13 * 256);
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
fn yokogawa_fam3_rejects_invalid_cpu_ranges_types_and_bank_maps() {
    for signal in [
        json!({"name":"bad","device":"D","address":0}),
        json!({"name":"bad","device":"D","address":1.5}),
        json!({"name":"bad","device":"D","address":4_294_967_296_u64}),
        json!({"name":"bad","device":"D","address":1,"data_type":"bool"}),
        json!({"name":"bad","device":"D","address":1,"data_type":"u32"}),
        json!({"name":"bad","device":"D","address":4_294_967_295_u32,"data_type":"u64","word_order":"little"}),
        json!({"name":"bad","device":"D","address":1,"elements":0}),
        json!({"name":"bad","device":"D","address":1,"elements":1.5}),
        json!({"name":"bad","device":"D","address":1,"elements":4097}),
        json!({"name":"bad","device":"X","address":200,"data_type":"bool"}),
        json!({"name":"bad","device":"X","address":265,"data_type":"bool"}),
        json!({"name":"bad","device":"X","address":1701,"data_type":"bool"}),
        json!({"name":"bad","device":"X","address":71_664,"data_type":"bool","elements":2}),
        json!({"name":"bad","device":"E","address":2048,"data_type":"bool","elements":2}),
        json!({"name":"bad","device":"R","address":1024,"data_type":"u32","word_order":"little"}),
        json!({"name":"bad","device":"W","address":1}),
        json!({"name":"bad","device":"W","address":2048,"elements":2,"link_points":2048}),
        json!({"name":"bad","device":"W","address":10_001,"link_points":17}),
        json!({"name":"bad","device":"W","address":10_001,"link_points":16.5}),
        json!({"name":"bad","device":"W","address":10_001,"link_points":8208}),
        json!({"name":"bad","device":"D","address":1,"link_points":2048}),
        json!({"name":"bad","device":"D","address":1,"data_type":"u64","word_order":"little","scale":2}),
    ] {
        let home = Home::new("fam3-config");
        let cfg = config("127.0.0.1:1".parse().unwrap(), &json!([signal]));
        assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
    }
    for value in [json!(0), json!(5), json!(1.5)] {
        let home = Home::new("fam3-cpu");
        let mut cfg = config(
            "127.0.0.1:1".parse().unwrap(),
            &json!([{"name":"bad","device":"D","address":1}]),
        );
        cfg["cpu"] = value;
        assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
    }
    let home = Home::new("fam3-ascii-range");
    let mut cfg = config(
        "127.0.0.1:1".parse().unwrap(),
        &json!([{"name":"bad","device":"B","address":100_000}]),
    );
    cfg["encoding"] = json!("ascii");
    assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
}
