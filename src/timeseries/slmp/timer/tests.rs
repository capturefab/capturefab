use super::super::{Config, Device, DeviceFormat, Encoding, Reading, Signal, open};
use super::{Budget, MIN_LTN, decode, minimum_value};
use crate::types::{ErrorKind, classify, decode_hex};
use serde_json::{Value, json};
use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/slmp/timer-literals.json"
    ))
    .unwrap()
}
fn signal(value: Value) -> Signal {
    serde_json::from_value(value).unwrap()
}
fn config(signals: Value) -> Config {
    let mut value = json!({"address":"127.0.0.1:1","device_format":"iq_r"});
    value["signals"] = signals;
    serde_json::from_value(value).unwrap()
}
fn native_signal() -> Value {
    json!({"name":"timer","device":"LTN","address":0,"data_type":"long_timer","elements":2})
}

#[test]
fn frozen_binary_ascii_transcripts_and_every_native_word_agree() {
    let case = fixture();
    let mut configured = config(json!([native_signal()]));
    configured.validate().unwrap();
    let request = configured.request(&configured.signals[0]).unwrap();
    assert_eq!(crate::types::hex(&request), case["binary_request_hex"]);
    configured.encoding = Encoding::Ascii;
    assert_eq!(
        configured.request(&configured.signals[0]).unwrap(),
        case["ascii_request"].as_str().unwrap().as_bytes()
    );
    let reply = decode_hex(case["binary_reply_hex"].as_str().unwrap(), 27).unwrap();
    assert_eq!(reply.len(), 27);
    let value = decode(
        &configured.signals[0],
        &reply[11..],
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(value, case["expected"]);
    for (raw, object) in reply[11..]
        .as_chunks::<8>()
        .0
        .iter()
        .zip(value.as_array().unwrap())
    {
        let restored: Vec<u8> = object["raw_words"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|word| u16::try_from(word.as_u64().unwrap()).unwrap().to_le_bytes())
            .collect();
        assert_eq!(restored.as_slice(), raw);
        assert!(object.get("current_raw_u32").is_none());
        assert!(object.get("timebase").is_none());
    }
    configured.signals[0].elements = 1;
    let scalar = decode(
        &configured.signals[0],
        &reply[11..19],
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(scalar, case["expected"][0]);
    for bytes in [&reply[11..26], &reply[11..19]] {
        decode(
            &signal(native_signal()),
            bytes,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    }
}

#[test]
fn step_and_retentive_aliases_use_the_published_ascii_and_binary_codes() {
    for (device, standard, wide, code, bit) in [
        ("S", "S*", "S***", 0x98_u8, true),
        ("STS", "SS", "STS*", 0xc7, true),
        ("SS", "SS", "STS*", 0xc7, true),
        ("STC", "SC", "STC*", 0xc6, true),
        ("SC", "SC", "STC*", 0xc6, true),
        ("STN", "SN", "STN*", 0xc8, false),
        ("SN", "SN", "STN*", 0xc8, false),
    ] {
        let mut configured = config(
            json!([{"name":"native","device":device,"address":10,"data_type":if bit{"bool"}else{"u16"},"elements":3}]),
        );
        for extended in [false, true] {
            configured.device_format = if extended {
                DeviceFormat::IqR
            } else {
                DeviceFormat::Standard
            };
            configured.validate().unwrap();
            configured.encoding = Encoding::Binary;
            let raw = configured.request(&configured.signals[0]).unwrap();
            assert_eq!(&raw[11..13], &[1, 4]);
            assert_eq!(
                &raw[13..15],
                &[(u8::from(bit) | if extended { 2 } else { 0 }), 0]
            );
            let code_offset = if extended { 19 } else { 18 };
            assert_eq!(raw[code_offset], code);
            if extended {
                assert_eq!(raw[20], 0);
            }
            assert_eq!(&raw[raw.len() - 2..], &[3, 0]);
            configured.encoding = Encoding::Ascii;
            let expected = if extended {
                format!(
                    "500000FF03FF00001C00040401000{}{wide}000000100003",
                    if bit { 3 } else { 2 }
                )
            } else {
                format!(
                    "500000FF03FF00001800040401000{}{standard}0000100003",
                    u8::from(bit)
                )
            };
            assert_eq!(
                configured.request(&configured.signals[0]).unwrap(),
                expected.as_bytes()
            );
        }
    }
}

#[test]
fn whole_element_count_logical_range_and_closed_modes_are_checked_before_io() {
    let mut value = native_signal();
    value["address"] = json!(u32::MAX - 1);
    let configured = config(json!([value]));
    configured.validate().unwrap();
    assert_eq!(
        &configured.request(&configured.signals[0]).unwrap()[21..23],
        &[8, 0]
    );
    let mut maximum = config(
        json!([{"name":"max","device":"LSTN","address":0,"data_type":"long_timer","elements":240}]),
    );
    maximum.validate().unwrap();
    assert_eq!(maximum.signals[0].points().unwrap(), 960);
    assert_eq!(maximum.signals[0].bytes().unwrap(), 1920);
    maximum.encoding = Encoding::Ascii;
    maximum.validate().unwrap();
    for patch in [
        json!({"elements":0}),
        json!({"elements":241}),
        json!({"address":u32::MAX}),
        json!({"data_type":"u16"}),
        json!({"device":"STN"}),
        json!({"scale":2}),
        json!({"offset":1}),
    ] {
        let mut native = native_signal();
        for (key, item) in patch.as_object().unwrap() {
            native[key].clone_from(item);
        }
        let invalid = config(json!([native]));
        assert_eq!(
            classify(&invalid.validate().unwrap_err()).kind,
            ErrorKind::Usage
        );
    }
    let mut standard = config(json!([native_signal()]));
    standard.device_format = DeviceFormat::Standard;
    assert_eq!(
        classify(&standard.validate().unwrap_err()).kind,
        ErrorKind::Usage
    );
    for device in ["LTS", "LTC", "LSTS", "LSTC", "LZ"] {
        let mut native = native_signal();
        native["device"] = json!(device);
        serde_json::from_value::<Signal>(native).unwrap_err();
    }
    let mut ordinary = config(json!([{"name":"word","device":"D","address":0}]));
    ordinary.validate().unwrap();
    assert_eq!(ordinary.signals[0].points().unwrap(), 1);
    ordinary.signals[0].device = Device::Ltn;
    ordinary.validate().unwrap_err();
}

#[test]
fn full_schema_minimum_and_remaining_poll_budget_precede_later_queries() {
    let mut selected = signal(
        json!({"name":"quote\"and\\slash","device":"LTN","address":0,"data_type":"long_timer"}),
    );
    let raw = [0, 0, 0, 0, 3, 0, 0, 0];
    let native = decode(&selected, &raw, Instant::now() + Duration::from_secs(1)).unwrap();
    assert_eq!(serde_json::to_vec(&native).unwrap().len(), MIN_LTN);
    assert_eq!(minimum_value(&selected).unwrap(), MIN_LTN);
    let reading = Reading {
        signal: selected.name.clone(),
        value: native,
        source_time: None,
    };
    let mut minimum = Budget::new(&[selected.clone()]).unwrap();
    let actual = 2 + serde_json::to_vec(&reading).unwrap().len() + 1;
    assert_eq!(2 + minimum.remaining, actual);
    minimum
        .check(super::super::MAX_BATCH - minimum.remaining)
        .unwrap();
    minimum
        .check(super::super::MAX_BATCH - minimum.remaining + 1)
        .unwrap_err();
    minimum.advance();
    assert_eq!(minimum.remaining, 0);
    selected.device = Device::Lstn;
    assert_eq!(minimum_value(&selected).unwrap(), MIN_LTN + 1);
    selected.elements = 2;
    assert_eq!(minimum_value(&selected).unwrap(), (MIN_LTN + 1) * 2 + 3);
    let signals = vec![selected; 256];
    let mut budget = Budget::new(&signals).unwrap();
    budget.check(2).unwrap();
    budget.advance();
    // Exact earlier data can make the guaranteed remainder unpublishable.
    budget
        .check(super::super::MAX_BATCH - budget.remaining + 1)
        .unwrap_err();
    let impossible = config(json!((0..256).map(|i|json!({"name":format!("timer{i}"),"device":"LTN","address":0,"data_type":"long_timer","elements":240})).collect::<Vec<_>>()));
    assert_eq!(
        classify(&impossible.validate().unwrap_err()).kind,
        ErrorKind::OperationFailed
    );
}

#[test]
fn original_deadline_and_zero_budget_make_lazy_failure_permanent() {
    let selected = signal(native_signal());
    let error = decode(&selected, &[0; 16], Instant::now()).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    for timer in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let signals = if timer {
            json!([native_signal()])
        } else {
            json!([{"name":"word","device":"D","address":0}])
        };
        let mut configured = config(signals);
        configured.address = listener.local_addr().unwrap();
        let mut source = open(&configured).unwrap();
        assert_eq!(
            classify(&source.read(Duration::ZERO).unwrap_err()).kind,
            ErrorKind::Timeout
        );
        assert_eq!(
            classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
            ErrorKind::Disconnected
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

fn request(stream: &mut TcpStream) -> Vec<u8> {
    let mut raw = vec![0; 23];
    stream.read_exact(&mut raw).unwrap();
    assert_eq!(&raw[..9], &[0x50, 0, 0, 255, 255, 3, 0, 14, 0]);
    assert_eq!(&raw[11..15], &[1, 4, 2, 0]);
    raw
}

#[test]
fn failed_sibling_poll_closes_old_connection_and_never_connects_again() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let first = request(&mut stream);
        assert_eq!(&first[19..23], &[0xa8, 0, 1, 0]);
        stream
            .write_all(&decode_hex("d00000ffff0300040000003412", 13).unwrap())
            .unwrap();
        let second = request(&mut stream);
        assert_eq!(&second[19..23], &[0x52, 0, 4, 0]);
        stream
            .write_all(&decode_hex("d00000ffff0300020051c0", 11).unwrap())
            .unwrap();
        assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let mut configured = config(json!([{"name":"word","device":"D","address":0},
        {"name":"timer","device":"LTN","address":0,"data_type":"long_timer"}]));
    configured.address = address;
    let mut source = open(&configured).unwrap();
    let error = classify(&source.read(Duration::from_secs(2)).unwrap_err());
    assert_eq!(error.kind, ErrorKind::OperationFailed);
    assert_eq!(error.details.unwrap()["slmp_end_code"], 0xc051);
    assert_eq!(
        classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Disconnected
    );
    done_tx.send(()).unwrap();
    server.join().unwrap();
}
