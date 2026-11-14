//! Classic S7 PLC reads through a real CLI recording, with no SDK/runtime.
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_nodelay(true).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "S7 client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn receive(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[..2], &[3, 0]);
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    assert!((7..=1028).contains(&length));
    let mut body = vec![0; length - 4];
    stream.read_exact(&mut body).unwrap();
    body
}
fn send(stream: &mut TcpStream, body: &[u8]) {
    let mut packet = vec![3, 0];
    packet.extend_from_slice(&u16::try_from(body.len() + 4).unwrap().to_be_bytes());
    packet.extend_from_slice(body);
    // Split TCP sends at non-protocol boundaries; read_exact must not assume
    // that a TCP read returns one TPKT, COTP header or S7 response.
    for chunk in packet.chunks(3) {
        stream.write_all(chunk).unwrap();
    }
}
fn response(reference: &[u8], parameters: &[u8], data: &[u8]) -> Vec<u8> {
    let mut pdu = vec![0x32, 3, 0, 0];
    pdu.extend_from_slice(reference);
    pdu.extend_from_slice(&u16::try_from(parameters.len()).unwrap().to_be_bytes());
    pdu.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
    pdu.extend_from_slice(&[0, 0]);
    pdu.extend_from_slice(parameters);
    pdu.extend_from_slice(data);
    pdu
}
fn send_pdu(stream: &mut TcpStream, pdu: &[u8], segmented: bool) {
    if segmented {
        let split = pdu.len() / 2;
        let mut first = vec![2, 0xf0, 0];
        first.extend_from_slice(&pdu[..split]);
        send(stream, &first);
        let mut last = vec![2, 0xf0, 0x80];
        last.extend_from_slice(&pdu[split..]);
        send(stream, &last);
    } else {
        let mut packet = vec![2, 0xf0, 0x80];
        packet.extend_from_slice(pdu);
        send(stream, &packet);
    }
}
fn handshake(stream: &mut TcpStream, local: u16, remote: u16) {
    handshake_pdu(stream, local, remote, 240);
}
fn handshake_pdu(stream: &mut TcpStream, local: u16, remote: u16, pdu: u16) {
    let request = receive(stream);
    let mut expected = vec![17, 0xe0, 0, 0, 0, 1, 0, 0xc0, 1, 10, 0xc1, 2];
    expected.extend_from_slice(&local.to_be_bytes());
    expected.extend_from_slice(&[0xc2, 2]);
    expected.extend_from_slice(&remote.to_be_bytes());
    assert_eq!(request, expected);
    send(stream, &[6, 0xd0, 0, 1, 0, 2, 0]); // Optional COTP params may be absent.
    let setup = receive(stream);
    assert_eq!(&setup[..7], &[2, 0xf0, 0x80, 0x32, 1, 0, 0]);
    assert_eq!(&setup[9..], &[0, 8, 0, 0, 0xf0, 0, 0, 1, 0, 1, 3, 0xc0]);
    let mut parameters = vec![0xf0, 0, 0, 1, 0, 1];
    parameters.extend_from_slice(&pdu.to_be_bytes());
    send_pdu(stream, &response(&setup[7..9], &parameters, &[]), false);
}
fn read_request(stream: &mut TcpStream, area: u8, db: u16, address: u32, width: u16) -> Vec<u8> {
    let mut expected = vec![4, 1, 0x12, 10, 0x10, 2];
    expected.extend_from_slice(&width.to_be_bytes());
    expected.extend_from_slice(&db.to_be_bytes());
    expected.push(area);
    expected.extend_from_slice(&(address * 8).to_be_bytes()[1..]);
    expect_read_var(stream, &expected)
}
fn expect_read_var(stream: &mut TcpStream, parameters: &[u8]) -> Vec<u8> {
    let request = receive(stream);
    assert_eq!(&request[..7], &[2, 0xf0, 0x80, 0x32, 1, 0, 0]);
    assert_eq!(&request[9..13], &[0, 14, 0, 0]);
    assert_eq!(
        &request[13..],
        parameters,
        "only read-only S7 Read Var is allowed"
    );
    request[7..9].to_vec()
}
fn reply(stream: &mut TcpStream, reference: &[u8], bytes: &[u8], segmented: bool) {
    let mut data = vec![0xff, 4];
    data.extend_from_slice(&u16::try_from(bytes.len() * 8).unwrap().to_be_bytes());
    data.extend_from_slice(bytes);
    send_pdu(stream, &response(reference, &[4, 1], &data), segmented);
}
fn config(listener: &TcpListener, signals: &Value) -> Value {
    json!({"version":1,"sources":[
        {"id":"plc","protocol":"s7","address":listener.local_addr().unwrap().to_string(),
         "every_ms":3_600_000,"timeout_ms":1000,"signals":signals},
        {"id":"sensor","protocol":"simulator","signal":"tick","every_ms":10}
    ]})
}
fn record(home: &Home, config: &Value) -> support::cli::Run {
    record_for(home, config, "500ms")
}
fn record_for(home: &Home, config: &Value, duration: &str) -> support::cli::Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
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
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect()
}

fn text_slot(wide: bool, capacity: u16, payload: &[u8]) -> Vec<u8> {
    let unit = if wide { 2 } else { 1 };
    assert!(payload.len().is_multiple_of(unit));
    let length = u16::try_from(payload.len() / unit).unwrap();
    assert!(length <= capacity);
    let mut bytes = if wide {
        [capacity.to_be_bytes(), length.to_be_bytes()].concat()
    } else {
        vec![
            u8::try_from(capacity).unwrap(),
            u8::try_from(length).unwrap(),
        ]
    };
    bytes.extend_from_slice(payload);
    let header = if wide { 4 } else { 2 };
    bytes.resize(header + usize::from(capacity) * unit, 0xa5);
    bytes
}
fn expected_text(
    wide: bool,
    capacity: u16,
    payload: &[u8],
    encoding: Option<&str>,
    text: Option<&str>,
    status: &str,
) -> Value {
    let mut raw = text_slot(wide, capacity, payload);
    let storage_bytes = raw.len();
    let header = if wide { 4 } else { 2 };
    raw.truncate(header + payload.len());
    let mut value = json!({"kind":"s7_text","data_type":if wide { "wstring" } else { "string" },
        "capacity":capacity,"length":payload.len() / if wide { 2 } else { 1 },
        "storage_bytes":storage_bytes,"decode_status":status,
        "raw":{"encoding":"base64","scope":"header_and_active","bytes":raw.len(),"data":STANDARD.encode(&raw)}});
    if let Some(encoding) = encoding {
        value["encoding"] = json!(encoding);
    }
    if let Some(text) = text {
        value["text"] = json!(text);
    }
    value
}
fn reply_fast(stream: &mut TcpStream, reference: &[u8], bytes: &[u8]) {
    let mut data = vec![0xff, 4];
    data.extend_from_slice(&u16::try_from(bytes.len() * 8).unwrap().to_be_bytes());
    data.extend_from_slice(bytes);
    let mut body = vec![2, 0xf0, 0x80];
    body.extend_from_slice(&response(reference, &[4, 1], &data));
    let mut packet = vec![3, 0];
    packet.extend_from_slice(&u16::try_from(body.len() + 4).unwrap().to_be_bytes());
    packet.extend_from_slice(&body);
    stream.write_all(&packet).unwrap();
}
fn serve_text(
    stream: &mut TcpStream,
    address: u32,
    slot: &[u8],
    pdu: u16,
    header: usize,
    fast: bool,
) {
    let maximum = usize::from(pdu) - 18;
    for (index, bytes) in slot.chunks(maximum).enumerate() {
        let reference = read_request(
            stream,
            0x84,
            1,
            address + u32::try_from(index * maximum).unwrap(),
            u16::try_from(bytes.len()).unwrap(),
        );
        if fast {
            reply_fast(stream, &reference, bytes);
        } else {
            reply(stream, &reference, bytes, index % 2 == 0);
        }
    }
    if slot.len() > maximum {
        let reference = read_request(stream, 0x84, 1, address, u16::try_from(header).unwrap());
        assert!(slot.len() >= header);
        let bytes = &slot[..header];
        if fast {
            reply_fast(stream, &reference, bytes);
        } else {
            reply(stream, &reference, bytes, true);
        }
    }
}
fn expect_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("no further query or write may follow: {other:?}"),
    }
}

#[test]
fn reads_areas_exact_counters_scaled_values_and_segmented_responses() {
    let home = Home::new("s7-values");
    let listener = listener();
    let mut config = config(
        &listener,
        &json!([
            {"name":"pressure","db":1,"address":0,"data_type":"f32"},
            {"name":"count","db":2,"address":8,"data_type":"u64"},
            {"name":"signed","area":"merker","address":16,"data_type":"i64"},
            {"name":"present","area":"input","address":3,"data_type":"bool","bit":7},
            {"name":"output","area":"output","address":2_097_150,"data_type":"u16"},
            {"name":"temperature","db":65535,"address":20,"data_type":"f64","scale":0.5,"offset":1}
        ]),
    );
    config["sources"][0]["local_tsap"] = json!(0x1000);
    config["sources"][0]["remote_tsap"] = json!(0x2001);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0x1000, 0x2001);
        for (area, db, address, value) in [
            (0x84, 1, 0, 12.5_f32.to_be_bytes().to_vec()),
            (0x84, 2, 8, u64::MAX.to_be_bytes().to_vec()),
            (0x83, 0, 16, i64::MIN.to_be_bytes().to_vec()),
            (0x81, 0, 3, vec![0x80]),
            (0x82, 0, 2_097_150, 65535_u16.to_be_bytes().to_vec()),
            (0x84, 65535, 20, 37.5_f64.to_be_bytes().to_vec()),
        ] {
            let reference = read_request(
                &mut stream,
                area,
                db,
                address,
                u16::try_from(value.len()).unwrap(),
            );
            reply(&mut stream, &reference, &value, true);
        }
        let mut closed = [0];
        let _ = stream.read(&mut closed);
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let rows = rows(&home);
    let samples: Vec<_> = rows.iter().filter(|row| row["source"] == "plc").collect();
    assert_eq!(samples.len(), 6);
    for (sample, (name, value)) in samples.iter().zip([
        ("pressure", json!(12.5)),
        ("count", json!(u64::MAX)),
        ("signed", json!(i64::MIN)),
        ("present", json!(true)),
        ("output", json!(65535)),
        ("temperature", json!(19.75)),
    ]) {
        assert_eq!(sample["signal"], name);
        assert_eq!(sample["value"], value);
        assert!(sample.get("source_time").is_none());
    }
    assert!(rows.iter().any(|row| row["source"] == "sensor"));
    assert_eq!(rows.last().unwrap()["status"], "complete");
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "plc.csv",
        "--source",
        "plc",
    ]);
    let csv = fs::read_to_string(home.path("plc.csv")).unwrap();
    assert!(csv.contains("18446744073709551615"));
    assert!(csv.contains("-9223372036854775808"));
}

#[test]
fn indexed_timer_and_counter_reads_preserve_words_and_decode_s5time() {
    let home = Home::new("s7-indexed");
    let listener = listener();
    let config = config(
        &listener,
        &json!([
            {"name":"timer_ms","area":"timer","address":7,"data_type":"s5time"},
            {"name":"counter_word","area":"counter","address":21},
            {"name":"preset_ms","db":1,"address":9,"data_type":"s5time"},
            {"name":"timer_word","area":"timer","address":31}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0x0100, 0x0102);
        for (parameters, data) in [
            (
                [4, 1, 0x12, 10, 0x10, 0x1d, 0, 1, 0, 0, 0x1d, 0, 0, 7],
                [0xff, 9, 0, 2, 0xd1, 0x23],
            ),
            (
                [4, 1, 0x12, 10, 0x10, 0x1c, 0, 1, 0, 0, 0x1c, 0, 0, 21],
                [0xff, 9, 0, 2, 0x09, 0x99],
            ),
            (
                [4, 1, 0x12, 10, 0x10, 2, 0, 2, 0, 1, 0x84, 0, 0, 72],
                [0xff, 4, 0, 16, 0x39, 0x99],
            ),
            (
                [4, 1, 0x12, 10, 0x10, 0x1d, 0, 1, 0, 0, 0x1d, 0, 0, 31],
                [0xff, 4, 0, 16, 0xf3, 0x45],
            ),
        ] {
            let reference = expect_read_var(&mut stream, &parameters);
            send_pdu(&mut stream, &response(&reference, &[4, 1], &data), true);
        }
        expect_closed(&mut stream);
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let rows = rows(&home);
    let samples: Vec<_> = rows.iter().filter(|row| row["source"] == "plc").collect();
    assert_eq!(samples.len(), 4);
    for (row, (name, value)) in samples.iter().zip([
        ("timer_ms", 12_300),
        ("counter_word", 0x0999),
        ("preset_ms", 9_990_000),
        ("timer_word", 0xf345),
    ]) {
        assert_eq!(row["signal"], name);
        assert_eq!(row["value"], value);
    }
}

#[test]
fn malformed_indexed_timer_items_discard_successful_siblings() {
    for (data, cause) in [
        ([0xff, 9, 0, 1, 0x12, 0x34], "transport or data length"),
        ([0xff, 9, 0, 2, 0x1a, 0x34], "invalid S5TIME BCD"),
    ] {
        let home = Home::new("s7-indexed-invalid");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"valid","db":1,"address":0},
                {"name":"timer","area":"timer","address":3,"data_type":"s5time"}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 2);
            reply(&mut stream, &reference, &[0, 7], false);
            let reference = expect_read_var(
                &mut stream,
                &[4, 1, 0x12, 10, 0x10, 0x1d, 0, 1, 0, 0, 0x1d, 0, 0, 3],
            );
            send_pdu(&mut stream, &response(&reference, &[4, 1], &data), false);
            expect_closed(&mut stream);
        });
        let error = record(&home, &config).failure(1);
        server.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
    }
}

#[test]
fn text_replies_preserve_encoding_raw_bytes_and_negotiated_pdu_boundaries() {
    for pdu in [240_u16, 241, 960] {
        let home = Home::new("s7-text");
        let listener = listener();
        let wide = "中Ж\0".repeat(100);
        let wide_bytes = wide
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let maximum = "A".repeat(254);
        let cases = [
            (
                "ascii",
                false,
                5,
                Some("ascii"),
                b"A\0B\nC".to_vec(),
                Some("A\0B\nC".to_owned()),
                "decoded",
            ),
            (
                "empty",
                false,
                4,
                Some("ascii"),
                vec![],
                Some(String::new()),
                "decoded",
            ),
            (
                "zero",
                false,
                0,
                Some("ascii"),
                vec![],
                Some(String::new()),
                "decoded",
            ),
            (
                "zero_wide",
                true,
                0,
                None,
                vec![],
                Some(String::new()),
                "decoded",
            ),
            ("legacy", false, 4, None, vec![0x80, 0xff], None, "raw_only"),
            (
                "codepage",
                false,
                4,
                Some("windows1252"),
                vec![0x80, 0x9f, 0xe9],
                Some("€Ÿé".to_owned()),
                "decoded",
            ),
            (
                "undefined",
                false,
                5,
                Some("windows1252"),
                vec![0x81, 0x8d, 0x8f, 0x90, 0x9d],
                None,
                "invalid_encoding",
            ),
            (
                "utf8",
                false,
                16,
                Some("utf8"),
                "中é".as_bytes().to_vec(),
                Some("中é".to_owned()),
                "decoded",
            ),
            (
                "invalid_utf8",
                false,
                4,
                Some("utf8"),
                vec![0xff],
                None,
                "invalid_encoding",
            ),
            (
                "invalid_ascii",
                false,
                4,
                Some("ascii"),
                vec![0x80],
                None,
                "invalid_encoding",
            ),
            (
                "maximum",
                false,
                254,
                Some("ascii"),
                maximum.as_bytes().to_vec(),
                Some(maximum),
                "decoded",
            ),
            ("wide", true, 300, None, wide_bytes, Some(wide), "decoded"),
            (
                "empty_wide",
                true,
                4,
                None,
                vec![],
                Some(String::new()),
                "decoded",
            ),
            (
                "invalid_wide",
                true,
                2,
                None,
                vec![0xd8, 0],
                None,
                "invalid_encoding",
            ),
        ];
        let signals = cases
            .iter()
            .enumerate()
            .map(|(index, (name, wide, capacity, encoding, _, _, _))| {
                let mut signal = json!({"name":name,"db":1,"address":index * 1024,
                "data_type":if *wide { "wstring" } else { "string" },"capacity":capacity});
                if let Some(encoding) = encoding {
                    signal["encoding"] = json!(encoding);
                }
                if *name == "wide" {
                    signal["capacity"] = serde_json::from_str("3e2").unwrap();
                }
                if *name == "zero" {
                    signal["capacity"] = serde_json::from_str("-0.0").unwrap();
                }
                if *name == "maximum" {
                    signal["capacity"] = serde_json::from_str("2.54e2").unwrap();
                }
                signal
            })
            .collect::<Vec<_>>();
        let expected = cases
            .iter()
            .map(|(name, wide, capacity, encoding, payload, text, status)| {
                (
                    *name,
                    expected_text(
                        *wide,
                        *capacity,
                        payload,
                        if *wide { Some("utf16be") } else { *encoding },
                        text.as_deref(),
                        status,
                    ),
                )
            })
            .collect::<Vec<_>>();
        let config = config(&listener, &json!(signals));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake_pdu(&mut stream, 0x0100, 0x0102, pdu);
            for (index, (_, wide, capacity, _, payload, _, _)) in cases.iter().enumerate() {
                serve_text(
                    &mut stream,
                    u32::try_from(index * 1024).unwrap(),
                    &text_slot(*wide, *capacity, payload),
                    pdu,
                    if *wide { 4 } else { 2 },
                    false,
                );
            }
            expect_closed(&mut stream);
        });
        assert_eq!(record(&home, &config).success()["status"], "complete");
        server.join().unwrap();
        let rows = rows(&home);
        let samples = rows
            .iter()
            .filter(|row| row["source"] == "plc")
            .collect::<Vec<_>>();
        assert_eq!(samples.len(), expected.len());
        for (sample, (name, value)) in samples.iter().zip(expected) {
            assert_eq!(sample["signal"], name);
            assert_eq!(sample["value"], value);
        }
        for (format, path) in [("csv", "text.csv"), ("jsonl", "text.jsonl")] {
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "-o",
                path,
                "--format",
                format,
                "--source",
                "plc",
            ]);
        }
        let exported = fs::read_to_string(home.path("text.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(exported.len(), samples.len());
        for (exported, sample) in exported.iter().zip(samples) {
            assert_eq!(exported["value"], sample["value"]);
        }
        let csv = fs::read_to_string(home.path("text.csv")).unwrap();
        assert!(csv.contains("header_and_active"));
        assert!(csv.contains("中Ж"));
    }
}

#[test]
fn malformed_text_headers_and_middle_chunks_never_publish_completed_siblings() {
    for mode in [
        "maximum",
        "current",
        "header_change",
        "middle_reference",
        "middle_length",
        "middle_return_code",
    ] {
        let home = Home::new("s7-text-invalid");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"first","db":1,"address":0},
                {"name":"text","db":1,"address":100,"data_type":"wstring","capacity":300}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 2);
            reply(&mut stream, &reference, &[0, 7], false);
            let mut slot = text_slot(true, 300, &[0, b'A'].repeat(300));
            assert!(slot.len() >= 4);
            if mode == "maximum" {
                slot[..2].copy_from_slice(&301_u16.to_be_bytes());
            }
            if mode == "current" {
                slot[2..4].copy_from_slice(&301_u16.to_be_bytes());
            }
            for (index, bytes) in slot.chunks(222).enumerate() {
                let mut reference = read_request(
                    &mut stream,
                    0x84,
                    1,
                    100 + u32::try_from(index * 222).unwrap(),
                    u16::try_from(bytes.len()).unwrap(),
                );
                if index == 1 && mode.starts_with("middle_") {
                    if mode == "middle_reference" {
                        reference[1] ^= 1;
                    }
                    let mut data = vec![0xff, 4];
                    data.extend_from_slice(&u16::try_from(bytes.len() * 8).unwrap().to_be_bytes());
                    data.extend_from_slice(bytes);
                    if mode == "middle_length" {
                        data[3] ^= 1;
                    }
                    if mode == "middle_return_code" {
                        data = vec![5, 0, 0, 0];
                    }
                    send_pdu(&mut stream, &response(&reference, &[4, 1], &data), false);
                    expect_closed(&mut stream);
                    return;
                }
                reply(&mut stream, &reference, bytes, true);
            }
            if mode == "header_change" {
                let reference = read_request(&mut stream, 0x84, 1, 100, 4);
                reply(&mut stream, &reference, &[1, 44, 1, 43], false);
            }
            expect_closed(&mut stream);
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed", "{mode}: {error}");
        if mode == "middle_return_code" {
            assert_eq!(error["details"]["return_code"], 5);
        }
        server.join().unwrap();
        assert!(
            !rows(&home).iter().any(|row| row["source"] == "plc"),
            "{mode}"
        );
    }
}

#[test]
fn text_continuations_and_final_header_checks_share_the_original_deadline() {
    for header_delay in [false, true] {
        let home = Home::new("s7-text-timeout");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"first","db":1,"address":0},
                {"name":"text","db":1,"address":100,"data_type":"wstring","capacity":300}
            ]),
        );
        config["sources"][0]["timeout_ms"] = json!(250);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 2);
            thread::sleep(Duration::from_millis(50));
            reply(&mut stream, &reference, &[0, 7], false);
            let slot = text_slot(true, 300, &[0, b'A'].repeat(300));
            for (index, bytes) in slot.chunks(222).enumerate() {
                let reference = read_request(
                    &mut stream,
                    0x84,
                    1,
                    100 + u32::try_from(index * 222).unwrap(),
                    u16::try_from(bytes.len()).unwrap(),
                );
                if index == 0 {
                    thread::sleep(Duration::from_millis(50));
                }
                if index == 1 && !header_delay {
                    thread::sleep(Duration::from_millis(200));
                    expect_closed(&mut stream);
                    return;
                }
                reply(&mut stream, &reference, bytes, false);
            }
            let _ = read_request(&mut stream, 0x84, 1, 100, 4);
            thread::sleep(Duration::from_millis(200));
            expect_closed(&mut stream);
        });
        let start = Instant::now();
        assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
    }
}

#[test]
fn serialized_text_budget_stops_before_the_next_signal_without_publishing() {
    let home = Home::new("s7-text-budget");
    let listener = listener();
    let capacity = 65_534_u16;
    let payload = vec![0; usize::from(capacity) * 2];
    let text = "\0".repeat(usize::from(capacity));
    let expected = expected_text(
        true,
        capacity,
        &payload,
        Some("utf16be"),
        Some(&text),
        "decoded",
    );
    let encoded = serde_json::to_vec(&json!({"signal":"text_0","value":expected}))
        .unwrap()
        .len()
        + 1;
    assert!(encoded + 2 < 1 << 20);
    assert!(encoded * 2 + 2 > 1 << 20);
    let signals = (0..3)
        .map(|index| {
            json!({"name":format!("text_{index}"),"db":1,"address":0,
        "data_type":"wstring","capacity":capacity})
        })
        .collect::<Vec<_>>();
    let mut config = config(&listener, &json!(signals));
    config["sources"][0]["timeout_ms"] = json!(7000);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake_pdu(&mut stream, 0x0100, 0x0102, 960);
        let slot = text_slot(true, capacity, &payload);
        for _ in 0..2 {
            serve_text(&mut stream, 0, &slot, 960, 4, true);
        }
        expect_closed(&mut stream); // A third full slot must never be queried.
    });
    let error = record_for(&home, &config, "8s").failure(1);
    assert_eq!(error["code"], "failed");
    assert!(error["message"].as_str().unwrap().contains("1 MiB"));
    server.join().unwrap();
    let rows = rows(&home);
    assert!(!rows.iter().any(|row| row["source"] == "plc"));
    assert_eq!(rows.last().unwrap()["status"], "failed");
}

#[test]
fn malformed_and_rejected_reads_preserve_atomic_batches_and_fail_explicitly() {
    for mode in [
        "reference",
        "length",
        "return_code",
        "oversized",
        "nonfinite",
    ] {
        let home = Home::new("s7-invalid");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"first","db":1,"address":0},
                {"name":"second","db":1,"address":2,"data_type":"f32"}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0x0100, 0x0102);
            let reference = read_request(&mut stream, 0x84, 1, 0, 2);
            reply(&mut stream, &reference, &[0, 7], false);
            let mut reference = read_request(&mut stream, 0x84, 1, 2, 4);
            if mode == "oversized" {
                stream.write_all(&[3, 0, 0xff, 0xff]).unwrap();
                return;
            }
            if mode == "reference" {
                reference[1] ^= 1;
            }
            let data = match mode {
                "return_code" => vec![5, 0, 0, 0],
                "length" => vec![0xff, 4, 0, 8, 0, 0, 0, 0],
                "nonfinite" => {
                    let mut data = vec![0xff, 4, 0, 32];
                    data.extend_from_slice(&f32::NAN.to_be_bytes());
                    data
                }
                _ => vec![0xff, 4, 0, 32, 0, 0, 0, 0],
            };
            send_pdu(&mut stream, &response(&reference, &[4, 1], &data), false);
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed");
        if mode == "return_code" {
            assert_eq!(error["details"]["return_code"], 5);
        }
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["source"] == "plc"));
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn trickling_handshake_cannot_extend_the_total_poll_deadline() {
    let home = Home::new("s7-timeout");
    let listener = listener();
    let mut config = config(&listener, &json!([{"name":"value","db":1,"address":0}]));
    config["sources"][0]["timeout_ms"] = json!(80);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let _ = receive(&mut stream);
        for byte in [3, 0, 0, 11, 6, 0xd0, 0, 1, 0, 2, 0] {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
    });
    let start = Instant::now();
    let error = record(&home, &config).failure(4);
    assert_eq!(error["code"], "timeout");
    assert!(start.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
    assert_eq!(rows(&home).last().unwrap()["status"], "failed");
}

#[test]
fn invalid_configuration_is_rejected_before_creating_output() {
    let cases = [
        json!({"data_type":"u64","scale":0.5}),
        json!({"capacity":2}),
        json!({"encoding":"ascii"}),
        json!({"data_type":"string"}),
        json!({"data_type":"string","capacity":null}),
        json!({"data_type":"string","capacity":255}),
        json!({"data_type":"string","capacity":1.5}),
        serde_json::from_str(r#"{"data_type":"string","capacity":2.00000000000000000001}"#)
            .unwrap(),
        json!({"data_type":"string","capacity":"2"}),
        json!({"data_type":"string","capacity":65536}),
        json!({"data_type":"string","capacity":-1}),
        json!({"data_type":"string","capacity":2,"encoding":"latin1"}),
        json!({"data_type":"string","capacity":2,"scale":2}),
        json!({"data_type":"string","capacity":2,"offset":1}),
        json!({"data_type":"string","capacity":2,"bit":1}),
        json!({"data_type":"string","capacity":2,"area":"merker","db":0}),
        json!({"data_type":"string","capacity":254,"address":2_097_000}),
        json!({"data_type":"wstring","capacity":65535}),
        json!({"data_type":"wstring","capacity":1,"encoding":"utf8"}),
    ];
    for patch in cases {
        let home = Home::new("s7-config");
        let listener = listener();
        let mut signal = json!({"name":"value","db":1,"address":0});
        for (key, value) in patch.as_object().unwrap() {
            signal[key].clone_from(value);
        }
        let config = config(&listener, &json!([signal]));
        assert_eq!(
            record(&home, &config).failure(2)["code"],
            "usage",
            "{patch}"
        );
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[path = "support/s7_arrays.rs"]
mod arrays;

#[path = "support/s7_temporal.rs"]
mod temporal;

#[path = "support/s7_long_time.rs"]
mod long_time;
