//! Independent FINS/TCP and FINS/UDP PLC fixtures using Omron W421 sections
//! 7-3 and 7-4 and W342 MEMORY AREA READ (0101), through the actual CLI.
mod support;
#[path = "support/fins_timer.rs"]
mod timer;

use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

const CLIENT: u8 = 239;
const SERVER: u8 = 42;

#[derive(Clone, Copy)]
struct Route {
    gateway_count: u8,
    source: [u8; 3],
    target: [u8; 3],
}
const LOCAL: Route = Route {
    gateway_count: 2,
    source: [0, CLIENT, 0],
    target: [0, SERVER, 0],
};

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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "FINS client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn receive(stream: &mut TcpStream) -> Vec<u8> {
    let mut prefix = [0; 8];
    stream.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix[..4], b"FINS");
    let length = usize::try_from(u32::from_be_bytes(prefix[4..8].try_into().unwrap())).unwrap();
    assert!((8..=2048).contains(&length));
    let mut packet = vec![0; length];
    stream.read_exact(&mut packet).unwrap();
    packet
}
fn packet(command: u32, error: u32, frame: &[u8]) -> Vec<u8> {
    let mut packet = b"FINS".to_vec();
    packet.extend_from_slice(&u32::try_from(frame.len() + 8).unwrap().to_be_bytes());
    packet.extend_from_slice(&command.to_be_bytes());
    packet.extend_from_slice(&error.to_be_bytes());
    packet.extend_from_slice(frame);
    packet
}
fn send(stream: &mut TcpStream, packet: &[u8]) -> io::Result<()> {
    // The TCP prefix, command, routing header and value can all be split.
    for chunk in packet.chunks(3) {
        stream.write_all(chunk)?;
    }
    Ok(())
}
fn node_reply(client: u32, server: u32) -> Vec<u8> {
    packet(1, 0, &[client.to_be_bytes(), server.to_be_bytes()].concat())
}
fn handshake(stream: &mut TcpStream, requested: u8, assigned: u8) {
    assert_eq!(
        receive(stream),
        [
            0u32.to_be_bytes(),
            0u32.to_be_bytes(),
            u32::from(requested).to_be_bytes()
        ]
        .concat()
    );
    send(stream, &node_reply(u32::from(assigned), u32::from(SERVER))).unwrap();
}
fn read_request(
    stream: &mut TcpStream,
    route: Route,
    sid: u8,
    area: u8,
    address: u16,
    bit: u8,
    count: u16,
) -> Vec<u8> {
    let request = receive(stream);
    assert!(request.len() >= 8);
    assert_eq!(&request[..8], &[0, 0, 0, 2, 0, 0, 0, 0]);
    assert_eq!(
        &request[8..],
        request_frame(route, sid, area, address, bit, count),
        "only configured read-only 0101 requests are permitted"
    );
    request[8..].to_vec()
}
fn request_frame(route: Route, sid: u8, area: u8, address: u16, bit: u8, count: u16) -> Vec<u8> {
    let mut expected = vec![0x80, 0, route.gateway_count];
    expected.extend_from_slice(&route.target);
    expected.extend_from_slice(&route.source);
    expected.extend_from_slice(&[sid, 1, 1, area]);
    expected.extend_from_slice(&address.to_be_bytes());
    expected.push(bit);
    expected.extend_from_slice(&count.to_be_bytes());
    expected
}
fn response_frame(request: &[u8], bytes: &[u8]) -> Vec<u8> {
    assert!(request.len() >= 12);
    let mut frame = vec![0xc0, 0, 2];
    frame.extend_from_slice(&request[6..9]);
    frame.extend_from_slice(&request[3..6]);
    frame.extend_from_slice(&[request[9], 1, 1, 0, 0]);
    frame.extend_from_slice(bytes);
    frame
}
fn response(request: &[u8], bytes: &[u8]) -> Vec<u8> {
    packet(2, 0, &response_frame(request, bytes))
}
fn reply(stream: &mut TcpStream, request: &[u8], bytes: &[u8]) {
    send(stream, &response(request, bytes)).unwrap();
}
fn config(listener: &TcpListener, signals: &Value) -> Value {
    json!({"version":1,"sources":[
        {"id":"plc","protocol":"fins_tcp","address":listener.local_addr().unwrap().to_string(),
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
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn words(bytes: &[u8], little_words: bool, little_bytes: bool) -> Vec<u8> {
    assert!(bytes.len().is_multiple_of(2));
    let mut words = bytes.as_chunks::<2>().0.to_vec();
    if little_words {
        words.reverse();
    }
    if little_bytes {
        for word in &mut words {
            word.reverse();
        }
    }
    words.concat()
}

#[test]
fn negotiated_node_reads_preserve_typed_arrays_word_orders_and_exact_counters() {
    let home = Home::new("fins-values");
    let listener = listener();
    let values = [
        ("flag", "bool", vec![1], json!(true)),
        (
            "word",
            "u16",
            u16::MAX.to_be_bytes().to_vec(),
            json!(u16::MAX),
        ),
        (
            "signed_word",
            "i16",
            i16::MIN.to_be_bytes().to_vec(),
            json!(i16::MIN),
        ),
        (
            "dword",
            "u32",
            words(&u32::MAX.to_be_bytes(), true, false),
            json!(u32::MAX),
        ),
        (
            "signed_dword",
            "i32",
            words(&i32::MIN.to_be_bytes(), true, false),
            json!(i32::MIN),
        ),
        (
            "counter",
            "u64",
            words(&u64::MAX.to_be_bytes(), true, false),
            json!(u64::MAX),
        ),
        (
            "signed_counter",
            "i64",
            words(&i64::MIN.to_be_bytes(), true, false),
            json!(i64::MIN),
        ),
        (
            "pressure",
            "f32",
            words(&12.5_f32.to_be_bytes(), true, false),
            json!(12.5),
        ),
        (
            "temperature",
            "f64",
            words(&37.5_f64.to_be_bytes(), true, false),
            json!(19.75),
        ),
        (
            "bits",
            "bool",
            vec![1, 0, 1, 0],
            json!([true, false, true, false]),
        ),
        (
            "counters",
            "u64",
            [0x0123_4567_89ab_cdefu64, u64::MAX, 0]
                .into_iter()
                .flat_map(|value| words(&value.to_be_bytes(), true, false))
                .collect(),
            json!([0x0123_4567_89ab_cdefu64, u64::MAX, 0]),
        ),
        (
            "array",
            "i16",
            [i16::MIN, 0, i16::MAX]
                .into_iter()
                .flat_map(i16::to_be_bytes)
                .collect(),
            json!([i16::MIN, 0, i16::MAX]),
        ),
        (
            "little_words",
            "u32",
            vec![0x56, 0x78, 0x12, 0x34],
            json!(0x1234_5678u32),
        ),
        (
            "big_words",
            "u32",
            vec![0x12, 0x34, 0x56, 0x78],
            json!(0x1234_5678u32),
        ),
        (
            "little_bytes",
            "u32",
            vec![0x34, 0x12, 0x78, 0x56],
            json!(0x1234_5678u32),
        ),
        (
            "both_little",
            "u32",
            vec![0x78, 0x56, 0x34, 0x12],
            json!(0x1234_5678u32),
        ),
    ];
    let areas = [
        ("dm", 0x82, 0x02),
        ("cio", 0xb0, 0x30),
        ("wr", 0xb1, 0x31),
        ("hr", 0xb2, 0x32),
        ("ar", 0xb3, 0x33),
    ];
    let signals = values.iter().enumerate().map(|(index, (name, kind, _, _))| {
        let mut signal = json!({"name":name,"area":areas[index % areas.len()].0,"address":index * 32,"data_type":kind});
        match *name {
            "flag" => signal["bit"] = json!(15),
            "bits" => { signal["bit"] = json!(14); signal["elements"] = json!(4); }
            "counters" | "array" => signal["elements"] = json!(3),
            "temperature" => { signal["scale"] = json!(0.5); signal["offset"] = json!(1); }
            "big_words" => signal["word_order"] = json!("big"),
            "little_bytes" => { signal["word_order"] = json!("big"); signal["byte_order"] = json!("little"); }
            "both_little" => signal["byte_order"] = json!("little"),
            _ => {}
        }
        signal
    }).collect::<Vec<_>>();
    let config = config(&listener, &json!(signals));
    let expected = values
        .iter()
        .map(|(name, _, _, value)| (*name, value.clone()))
        .collect::<Vec<_>>();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0, CLIENT);
        for (index, (name, kind, bytes, _)) in values.iter().enumerate() {
            let boolean = *kind == "bool";
            let bit = match *name {
                "flag" => 15,
                "bits" => 14,
                _ => 0,
            };
            let area = if boolean {
                areas[index % areas.len()].2
            } else {
                areas[index % areas.len()].1
            };
            let count = u16::try_from(if boolean {
                bytes.len()
            } else {
                bytes.len() / 2
            })
            .unwrap();
            let request = read_request(
                &mut stream,
                LOCAL,
                u8::try_from(index + 1).unwrap(),
                area,
                u16::try_from(index * 32).unwrap(),
                bit,
                count,
            );
            reply(&mut stream, &request, bytes);
        }
        let mut closed = [0];
        assert_eq!(
            stream.read(&mut closed).unwrap(),
            0,
            "capture must not send writes or control commands"
        );
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
        assert_eq!(sample["elapsed_ns"], samples[0]["elapsed_ns"]);
        assert!(sample.get("source_time").is_none());
    }
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
fn explicit_em_banks_preserve_bit_boundaries_wide_arrays_and_exact_exports() {
    let home = Home::new("fins-em-banks");
    let listener = listener();
    // W342 section 5-3-2: bank D and above require a supporting CJ2 or
    // CS1D-CPU68HA CPU. This fixture validates their wire codes, not hardware.
    let banks = [
        (0_u8, 0xa0, 0x20),
        (12, 0xac, 0x2c),
        (13, 0xad, 0x2d),
        (15, 0xaf, 0x2f),
        (16, 0x60, 0xe0),
        (24, 0x68, 0xe8),
    ];
    let values = [u64::MAX, 0x0123_4567_89ab_cdef];
    let bits = (0..17).map(|index| index % 2 == 0).collect::<Vec<_>>();
    let mut signals = Vec::new();
    for (bank, _, _) in banks {
        let bank_value: Value = match bank {
            0 => serde_json::from_str("-0.0").unwrap(),
            12 => serde_json::from_str("1.2e1").unwrap(),
            24 => serde_json::from_str("2.4e1").unwrap(),
            _ => json!(bank),
        };
        let mut words = json!({"name":format!("words_{bank}"),"area":"em","bank":bank_value,
            "address":32760,"data_type":"u64","elements":2});
        if bank == 16 {
            words["word_order"] = json!("big");
            words["byte_order"] = json!("little");
        }
        signals.push(words);
        signals.push(
            json!({"name":format!("bits_{bank}"),"area":"em","bank":bank_value,
            "address":32766,"bit":15,"data_type":"bool","elements":17}),
        );
    }
    let config = config(&listener, &json!(signals));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0, CLIENT);
        for (index, (bank, word_code, bit_code)) in banks.into_iter().enumerate() {
            let sid = u8::try_from(index * 2 + 1).unwrap();
            let request = read_request(&mut stream, LOCAL, sid, word_code, 32760, 0, 8);
            let bytes = values
                .iter()
                .flat_map(|value| words(&value.to_be_bytes(), bank != 16, bank == 16))
                .collect::<Vec<_>>();
            reply(&mut stream, &request, &bytes);
            let request = read_request(&mut stream, LOCAL, sid + 1, bit_code, 32766, 15, 17);
            let bytes = (0..17)
                .map(|index| u8::from(index % 2 == 0))
                .collect::<Vec<_>>();
            reply(&mut stream, &request, &bytes);
        }
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let rows = rows(&home);
    let samples = rows
        .iter()
        .filter(|row| row["source"] == "plc")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), banks.len() * 2);
    for (bank, _, _) in banks {
        let words = samples
            .iter()
            .find(|sample| sample["signal"] == format!("words_{bank}"))
            .unwrap();
        assert_eq!(words["value"], json!(values));
        let flags = samples
            .iter()
            .find(|sample| sample["signal"] == format!("bits_{bank}"))
            .unwrap();
        assert_eq!(flags["value"], json!(bits));
    }
    for (format, path) in [("csv", "em.csv"), ("jsonl", "em.jsonl")] {
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
    let csv = fs::read_to_string(home.path("em.csv")).unwrap();
    assert_eq!(csv.lines().count(), samples.len() + 1);
    let unquote = |cell: &str| {
        cell.strip_prefix('"')
            .and_then(|cell| cell.strip_suffix('"'))
            .unwrap_or(cell)
            .replace("\"\"", "\"")
    };
    for (line, sample) in csv.lines().skip(1).zip(&samples) {
        // The fixture's source/signal/time cells have no commas. Only the
        // final JSON value cell needs CSV unquoting, including its array commas.
        let cells = line.splitn(9, ',').collect::<Vec<_>>();
        assert_eq!(cells.len(), 9);
        assert_eq!(unquote(cells[2]), sample["signal"].as_str().unwrap());
        let value = unquote(cells[8]);
        assert_eq!(
            serde_json::from_str::<Value>(&value).unwrap(),
            sample["value"]
        );
    }
    let exported = fs::read_to_string(home.path("em.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), samples.len());
    for (exported, sample) in exported.iter().zip(samples) {
        assert_eq!(exported["signal"], sample["signal"]);
        assert_eq!(exported["value"], sample["value"]);
    }
}

#[test]
fn fixed_nodes_routed_units_and_terminal_word_address_are_exact() {
    let home = Home::new("fins-routes");
    let listener = listener();
    let mut config = config(
        &listener,
        &json!([{"name":"last","area":"dm","address":65535}]),
    );
    config["sources"][0]["gateway_count"] = serde_json::from_str("7e0").unwrap();
    config["sources"][0]["source_network"] = json!(4);
    config["sources"][0]["source_node"] = json!(20);
    config["sources"][0]["source_unit"] = json!(9);
    config["sources"][0]["target_network"] = json!(7);
    config["sources"][0]["target_node"] = json!(17);
    config["sources"][0]["target_unit"] = json!(12);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 20, 20);
        let request = read_request(
            &mut stream,
            Route {
                gateway_count: 7,
                source: [4, 20, 9],
                target: [7, 17, 12],
            },
            1,
            0x82,
            65535,
            0,
            1,
        );
        let mut packet = response(&request, &[0x12, 0x34]);
        packet[18] = 0; // A response may have traversed the remaining gateways.
        send(&mut stream, &packet).unwrap();
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    assert!(
        rows(&home)
            .iter()
            .any(|row| row["source"] == "plc" && row["value"] == 0x1234)
    );
}

#[test]
fn maximum_ethernet_word_and_bit_reads_survive_split_tcp_frames() {
    let home = Home::new("fins-maximal");
    let listener = listener();
    let config = config(
        &listener,
        &json!([
            {"name":"words","address":64000,"elements":999},
            {"name":"bits","area":"cio","address":100,"bit":15,"data_type":"bool","elements":1998}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0, CLIENT);
        let request = read_request(&mut stream, LOCAL, 1, 0x82, 64000, 0, 999);
        reply(&mut stream, &request, &[0x12, 0x34].repeat(999));
        let request = read_request(&mut stream, LOCAL, 2, 0x30, 100, 15, 1998);
        reply(&mut stream, &request, &[0, 1].repeat(999));
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let rows = rows(&home);
    let word_values = rows.iter().find(|row| row["signal"] == "words").unwrap()["value"]
        .as_array()
        .unwrap();
    assert_eq!(word_values.len(), 999);
    assert!(word_values.iter().all(|word| *word == 0x1234));
    let bit_values = rows.iter().find(|row| row["signal"] == "bits").unwrap()["value"]
        .as_array()
        .unwrap();
    assert_eq!(bit_values.len(), 1998);
    assert!(
        bit_values
            .iter()
            .enumerate()
            .all(|(index, bit)| *bit == (index % 2 == 1))
    );
}

#[test]
fn system_owned_header_fields_do_not_reject_valid_memory_replies() {
    // W342 section 3-3-3 prohibits checking response ICF bits 1–5, RSV and
    // GCT. Exercise each reserved ICF bit and combined system-owned fields.
    let home = Home::new("fins-system-header");
    let listener = listener();
    let headers = [
        [0xc0_u8, 0, 0],
        [0xc0, 0, 2],
        [0xc2, 1, 3],
        [0xc4, 2, 7],
        [0xc8, 4, 255],
        [0xd0, 8, 7],
        [0xe0, 128, 2],
        [0xfe, 255, 7],
        [0xc0, 255, 255],
    ];
    let signals = (0..headers.len())
        .map(
            |index| json!({"name":format!("header_{index}"),"address":index * 4,"data_type":"u64"}),
        )
        .collect::<Vec<_>>();
    let config = config(&listener, &json!(signals));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        handshake(&mut stream, 0, CLIENT);
        for (index, header) in headers.into_iter().enumerate() {
            let request = read_request(
                &mut stream,
                LOCAL,
                u8::try_from(index + 1).unwrap(),
                0x82,
                u16::try_from(index * 4).unwrap(),
                0,
                4,
            );
            let value = u64::MAX - u64::try_from(index).unwrap();
            let mut packet = response(&request, &words(&value.to_be_bytes(), true, false));
            assert!(packet.len() >= 19);
            packet[16..19].copy_from_slice(&header);
            send(&mut stream, &packet).unwrap();
        }
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let rows = rows(&home);
    let samples = rows
        .iter()
        .filter(|row| row["source"] == "plc")
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), headers.len());
    for (index, sample) in samples.iter().enumerate() {
        assert_eq!(sample["signal"], format!("header_{index}"));
        assert_eq!(
            sample["value"].as_u64(),
            Some(u64::MAX - u64::try_from(index).unwrap())
        );
    }
}

#[test]
fn malformed_transport_routes_service_ids_statuses_and_values_never_publish_partial_polls() {
    for mode in [
        "magic",
        "short_length",
        "oversized",
        "tcp_command",
        "tcp_error",
        "icf_command",
        "icf_fixed_bit",
        "icf_response_request",
        "destination_network",
        "destination_node",
        "destination_unit",
        "source_network",
        "source_node",
        "source_unit",
        "sid",
        "fins_command",
        "end_code",
        "short_data",
        "extra_data",
        "nonfinite",
        "bool",
        "truncated",
    ] {
        let home = Home::new("fins-invalid");
        let listener = listener();
        let config = config(
            &listener,
            &json!([
                {"name":"first","address":0},
                {"name":"second","area":"em","bank":16,"address":2,
                 "data_type":if mode == "bool" { "bool" } else { "f32" }}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            handshake(&mut stream, 0, CLIENT);
            let first = read_request(&mut stream, LOCAL, 1, 0x82, 0, 0, 1);
            reply(&mut stream, &first, &[0, 7]);
            let request = read_request(
                &mut stream,
                LOCAL,
                2,
                if mode == "bool" { 0xe0 } else { 0x60 },
                2,
                0,
                if mode == "bool" { 1 } else { 2 },
            );
            let value = match mode {
                "bool" => vec![2],
                "short_data" => vec![0, 1],
                "extra_data" => vec![0; 6],
                "nonfinite" => words(&f32::NAN.to_be_bytes(), true, false),
                _ => words(&12.5_f32.to_be_bytes(), true, false),
            };
            let mut packet = response(&request, &value);
            assert!(packet.len() >= 30);
            // Ignored system-owned fields must not mask identity/status errors.
            packet[16..19].copy_from_slice(&[0xfe, 255, 7]);
            match mode {
                "magic" => packet[0] = 0,
                "short_length" | "oversized" => {
                    packet[4..8].copy_from_slice(
                        &(if mode == "oversized" { u32::MAX } else { 7 }).to_be_bytes(),
                    );
                    let _ = stream.write_all(&packet[..8]);
                    return;
                }
                "tcp_command" => packet[8..12].copy_from_slice(&3u32.to_be_bytes()),
                "tcp_error" => packet[12..16].copy_from_slice(&0x20u32.to_be_bytes()),
                "icf_command" => packet[16] &= !0x40,
                "icf_fixed_bit" => packet[16] &= !0x80,
                "icf_response_request" => packet[16] |= 1,
                "destination_network" => packet[19] ^= 1,
                "destination_node" => packet[20] ^= 1,
                "destination_unit" => packet[21] ^= 1,
                "source_network" => packet[22] ^= 1,
                "source_node" => packet[23] ^= 1,
                "source_unit" => packet[24] ^= 1,
                "sid" => packet[25] ^= 1,
                "fins_command" => packet[27] = 2,
                "end_code" => packet[28..30].copy_from_slice(&0x1103u16.to_be_bytes()),
                "truncated" => {
                    packet.truncate(packet.len() - 2);
                    let _ = send(&mut stream, &packet);
                    stream.shutdown(Shutdown::Write).unwrap();
                    return;
                }
                _ => {}
            }
            let _ = send(&mut stream, &packet);
        });
        let truncated = mode == "truncated";
        let error = record(&home, &config).failure(if truncated { 3 } else { 1 });
        assert_eq!(
            error["code"],
            if truncated { "disconnected" } else { "failed" },
            "{mode}: {error}"
        );
        if mode == "end_code" {
            assert_eq!(error["details"]["fins_end_code"], 0x1103);
        }
        if mode == "tcp_error" {
            assert_eq!(error["details"]["fins_tcp_error"], 0x20);
        }
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["source"] == "plc"), "{mode}");
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn invalid_negotiated_nodes_and_handshake_errors_prevent_memory_reads() {
    for mode in [
        "client_zero",
        "client_255",
        "server_zero",
        "server_255",
        "fixed_mismatch",
        "tcp_error",
        "command",
        "short",
        "extra",
    ] {
        let home = Home::new("fins-handshake");
        let listener = listener();
        let mut config = config(&listener, &json!([{"name":"value","address":0}]));
        if mode == "fixed_mismatch" {
            config["sources"][0]["source_node"] = json!(20);
        }
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let request = receive(&mut stream);
            assert_eq!(request.len(), 12);
            assert_eq!(&request[..8], &[0; 8]);
            let mut response = node_reply(
                match mode {
                    "client_zero" => 0,
                    "client_255" => 255,
                    "fixed_mismatch" => 21,
                    _ => u32::from(CLIENT),
                },
                match mode {
                    "server_zero" => 0,
                    "server_255" => 255,
                    _ => u32::from(SERVER),
                },
            );
            match mode {
                "tcp_error" => response[12..16].copy_from_slice(&0x20u32.to_be_bytes()),
                "command" => response[8..12].copy_from_slice(&2u32.to_be_bytes()),
                "short" => {
                    response.truncate(20);
                    response[4..8].copy_from_slice(&12u32.to_be_bytes());
                }
                "extra" => {
                    response.extend([0; 4]);
                    response[4..8].copy_from_slice(&20u32.to_be_bytes());
                }
                _ => {}
            }
            let _ = send(&mut stream, &response);
            let mut byte = [0];
            match stream.read(&mut byte) {
                Ok(0) => {}
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
                other => panic!("invalid handshake must not issue memory reads: {other:?}"),
            }
        });
        assert_eq!(
            record(&home, &config).failure(1)["code"],
            "failed",
            "{mode}"
        );
        server.join().unwrap();
        assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
    }
}

#[test]
fn trickling_handshake_and_multiple_reads_share_one_total_poll_deadline() {
    for trickle in [false, true] {
        let home = Home::new("fins-timeout");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([{"name":"first","address":0},{"name":"second","address":1}]),
        );
        config["sources"][0]["timeout_ms"] = json!(160);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert_eq!(receive(&mut stream).len(), 12);
            let negotiated = node_reply(u32::from(CLIENT), u32::from(SERVER));
            if trickle {
                for byte in negotiated {
                    if stream.write_all(&[byte]).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(8));
                }
            } else {
                thread::sleep(Duration::from_millis(60));
                send(&mut stream, &negotiated).unwrap();
            }
            let first = read_request(&mut stream, LOCAL, 1, 0x82, 0, 0, 1);
            if !trickle {
                thread::sleep(Duration::from_millis(60));
            }
            reply(&mut stream, &first, &[0, 1]);
            let second = read_request(&mut stream, LOCAL, 2, 0x82, 1, 0, 1);
            if !trickle {
                thread::sleep(Duration::from_millis(80));
            }
            let _ = send(&mut stream, &response(&second, &[0, 2]));
        });
        let start = Instant::now();
        assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
    }
}

#[test]
fn invalid_configuration_is_rejected_before_output_or_network_access() {
    let cases = [
        json!({"signals":[{"name":"value","address":0,"data_type":"bool","bit":16}]}),
        json!({"signals":[{"name":"value","address":0,"bit":0}]}),
        json!({"signals":[{"name":"value","address":65535,"data_type":"u32"}]}),
        json!({"signals":[{"name":"value","address":0,"elements":0}]}),
        json!({"signals":[{"name":"value","address":0,"elements":1000}]}),
        json!({"signals":[{"name":"value","address":0,"data_type":"bool","elements":1999}]}),
        json!({"signals":[{"name":"value","address":0,"area":"em"}]}),
        json!({"signals":[{"name":"value","address":0,"area":"em","bank":null}]}),
        json!({"signals":[{"name":"value","address":0,"area":"em","bank":25}]}),
        json!({"signals":[{"name":"value","address":0,"area":"em","bank":256}]}),
        json!({"signals":[{"name":"value","address":0,"area":"em","bank":-1}]}),
        json!({"signals":[{"name":"value","address":0,"area":"em","bank":1.5}]}),
        serde_json::from_str(r#"{"signals":[{"name":"value","address":0,"area":"em","bank":24.00000000000000000001}]}"#).unwrap(),
        json!({"signals":[{"name":"value","address":0,"area":"em","bank":"3"}]}),
        json!({"signals":[{"name":"value","address":0,"bank":0}]}),
        json!({"signals":[{"name":"value","address":0,"area":"cio","bank":1}]}),
        json!({"signals":[{"name":"value","address":32768,"area":"em","bank":0}]}),
        json!({"signals":[{"name":"value","address":32767,"area":"em","bank":24,"data_type":"u32"}]}),
        json!({"signals":[{"name":"value","address":32761,"area":"em","bank":0,"data_type":"u64","elements":2}]}),
        json!({"signals":[{"name":"value","address":32767,"area":"em","bank":16,"data_type":"bool","bit":15,"elements":2}]}),
        json!({"signals":[{"name":"value","address":0,"data_type":"u64","scale":0.5}]}),
        json!({"signals":[{"name":"value","address":0,"data_type":"i64","offset":1}]}),
        json!({"signals":[{"name":"value","address":0,"data_type":"bool","scale":2}]}),
        json!({"signals":[{"name":"value","address":0,"write":true}]}),
        json!({"signals":[]}),
        json!({"gateway_count":0}),
        json!({"gateway_count":1}),
        json!({"gateway_count":3}),
        json!({"gateway_count":6}),
        json!({"gateway_count":8}),
        json!({"gateway_count":255}),
        json!({"gateway_count":2.5}),
        serde_json::from_str(r#"{"gateway_count":7.00000000000000000001}"#).unwrap(),
        json!({"source_network":128}),
        json!({"target_network":128}),
        json!({"source_node":255}),
        json!({"target_node":0}),
        json!({"target_node":255}),
        json!({"source_unit":255}),
        json!({"target_unit":255}),
        json!({"address":"omron.local:9600"}),
    ];
    for case in cases {
        let home = Home::new("fins-config");
        let listener = listener();
        let mut config = config(&listener, &json!([{"name":"value","address":0}]));
        for (name, value) in case.as_object().unwrap() {
            config["sources"][0][name].clone_from(value);
        }
        assert_eq!(record(&home, &config).failure(2)["code"], "usage", "{case}");
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

fn udp_socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
}

fn udp_config(socket: &UdpSocket, route: Route, signals: &Value) -> Value {
    json!({"version":1,"sources":[{
        "id":"plc","protocol":"fins_udp","address":socket.local_addr().unwrap().to_string(),
        "gateway_count":route.gateway_count,"source_network":route.source[0],
        "source_node":route.source[1],"source_unit":route.source[2],
        "target_network":route.target[0],"target_node":route.target[1],"target_unit":route.target[2],
        "every_ms":3_600_000,"timeout_ms":1000,"signals":signals
    }]})
}

fn udp_request(socket: &UdpSocket, expected: &[u8]) -> SocketAddr {
    let mut request = [0; 32];
    let (length, client) = socket.recv_from(&mut request).unwrap();
    assert_eq!(length, expected.len());
    assert_eq!(
        &request[..length],
        expected,
        "one raw FINS MEMORY AREA READ datagram"
    );
    client
}

fn udp_reply(socket: &UdpSocket, client: SocketAddr, request: &[u8], data: &[u8]) {
    let frame = response_frame(request, data);
    assert_eq!(socket.send_to(&frame, client).unwrap(), frame.len());
}

#[test]
fn udp_reads_the_documented_dm_route_and_preserves_exact_em_arrays() {
    let home = Home::new("fins-udp-values");
    let socket = udp_socket();
    let route = Route {
        gateway_count: 2,
        source: [1, 50, 0],
        target: [1, 100, 0],
    };
    let mut config = udp_config(
        &socket,
        route,
        &json!([
            {"name":"dm","address":100,"elements":150},
            {"name":"counters","area":"em","bank":24,"address":32760,"data_type":"u64","elements":2},
            {"name":"flags","area":"em","bank":16,"address":32766,"bit":15,"data_type":"bool","elements":17},
            {"name":"max_words","address":64000,"elements":999}
        ]),
    );
    config["sources"][0]["source_node"] = serde_json::from_str("5e1").unwrap();
    let counters = [u64::MAX, 0x0123_4567_89ab_cdef];
    let server = thread::spawn(move || {
        // W421 section 7-3's DM100 read of 150 words, with network 1 and an initial SID of 0.
        let dm = [
            0x80, 0, 2, 1, 100, 0, 1, 50, 0, 0, 1, 1, 0x82, 0, 100, 0, 0, 150,
        ];
        let client = udp_request(&socket, &dm);
        let wrong_peer = udp_socket();
        udp_reply(&wrong_peer, client, &dm, &[0, 99].repeat(150));
        let mut late = response_frame(&dm, &[0, 88].repeat(150));
        late[9] = 255;
        socket.send_to(&late, client).unwrap();
        let mut reply = response_frame(&dm, &[0x12, 0x34].repeat(150));
        reply[..3].copy_from_slice(&[0xfe, 255, 0]);
        socket.send_to(&reply, client).unwrap();
        let em_words = request_frame(route, 1, 0x68, 32760, 0, 8);
        assert_eq!(udp_request(&socket, &em_words), client);
        let raw = counters
            .into_iter()
            .flat_map(|value| words(&value.to_be_bytes(), true, false))
            .collect::<Vec<_>>();
        udp_reply(&socket, client, &em_words, &raw);
        let em_bits = request_frame(route, 2, 0xe0, 32766, 15, 17);
        assert_eq!(udp_request(&socket, &em_bits), client);
        udp_reply(
            &socket,
            client,
            &em_bits,
            &[1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1],
        );
        let max_words = request_frame(route, 3, 0x82, 64000, 0, 999);
        assert_eq!(udp_request(&socket, &max_words), client);
        udp_reply(&socket, client, &max_words, &[0x56, 0x78].repeat(999));
    });
    assert_eq!(record(&home, &config).success()["samples"], 4);
    server.join().unwrap();
    let recorded = rows(&home);
    assert_eq!(recorded[0]["sources"][0]["protocol"], "fins_udp");
    assert_eq!(recorded[1]["value"], json!(vec![0x1234; 150]));
    assert_eq!(recorded[2]["value"], json!(counters));
    assert_eq!(
        recorded[3]["value"],
        json!((0..17).map(|i| i % 2 == 0).collect::<Vec<_>>())
    );
    assert_eq!(recorded[4]["value"], json!(vec![0x5678; 999]));
    for format in ["csv", "jsonl"] {
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "-o",
            format,
            "--format",
            format,
        ]);
        let output = fs::read_to_string(home.path(format)).unwrap();
        assert!(output.contains("[18446744073709551615,81985529216486895]"));
    }
}

#[test]
fn udp_rejects_correlated_malformed_or_truncated_datagrams_atomically() {
    for mode in [
        "route", "command", "flags", "status", "short", "width", "extra", "oversize",
    ] {
        let home = Home::new("fins-udp-invalid");
        let socket = udp_socket();
        let config = udp_config(
            &socket,
            LOCAL,
            &json!([
                {"name":"first","address":0}, {"name":"last","address":1,"elements":999}
            ]),
        );
        let server = thread::spawn(move || {
            let first = request_frame(LOCAL, 0, 0x82, 0, 0, 1);
            let client = udp_request(&socket, &first);
            udp_reply(&socket, client, &first, &[0, 7]);
            let last = request_frame(LOCAL, 1, 0x82, 1, 0, 999);
            assert_eq!(udp_request(&socket, &last), client);
            let mut frame = response_frame(&last, &[0x12, 0x34].repeat(999));
            assert_eq!(frame.len(), 2012);
            match mode {
                "route" => frame[4] ^= 1,
                "command" => frame[11] = 2,
                "flags" => frame[0] |= 1,
                "status" => {
                    frame.truncate(14);
                    frame[12..14].copy_from_slice(&0x1103u16.to_be_bytes());
                }
                "short" => frame.truncate(11),
                "width" => frame.truncate(2010),
                "extra" => frame.push(0),
                "oversize" => frame.resize(8192, 0),
                _ => unreachable!(),
            }
            socket.send_to(&frame, client).unwrap();
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed", "{mode}: {error}");
        if mode == "status" {
            assert_eq!(error["details"]["fins_end_code"], 0x1103);
        }
        server.join().unwrap();
        let recorded = rows(&home);
        assert_eq!(recorded.len(), 2, "{mode}");
        assert_eq!(recorded[1]["status"], "failed");
    }
}

#[test]
fn udp_reads_and_late_service_ids_share_one_total_deadline() {
    let home = Home::new("fins-udp-deadline");
    let socket = udp_socket();
    let mut config = udp_config(
        &socket,
        LOCAL,
        &json!([
            {"name":"first","address":0}, {"name":"last","address":1}
        ]),
    );
    config["sources"][0]["timeout_ms"] = json!(240);
    let server = thread::spawn(move || {
        let first = request_frame(LOCAL, 0, 0x82, 0, 0, 1);
        let client = udp_request(&socket, &first);
        thread::sleep(Duration::from_millis(80));
        udp_reply(&socket, client, &first, &[0, 7]);
        let last = request_frame(LOCAL, 1, 0x82, 1, 0, 1);
        assert_eq!(udp_request(&socket, &last), client);
        thread::sleep(Duration::from_millis(70));
        udp_reply(&socket, client, &first, &[0, 7]);
        thread::sleep(Duration::from_millis(130));
        let _ = socket.send_to(&response_frame(&last, &[0, 9]), client);
    });
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    server.join().unwrap();
    assert_eq!(rows(&home).len(), 2);
}

#[test]
fn udp_wrap_rotates_ephemeral_ports_or_guards_the_fixed_binding() {
    for mode in ["ephemeral", "fixed", "blocked"] {
        let home = Home::new("fins-udp-wrap");
        let socket = udp_socket();
        let mut config = udp_config(
            &socket,
            LOCAL,
            &json!(
                (0..=255)
                    .map(|i| json!({"name":format!("signal_{i}"),"address":i}))
                    .collect::<Vec<_>>()
            ),
        );
        config["sources"][0]["every_ms"] = json!(200);
        let fixed = if mode == "ephemeral" {
            None
        } else {
            let reservation = udp_socket();
            let address = reservation.local_addr().unwrap();
            config["sources"][0]["bind"] = json!(address.to_string());
            Some(address)
        };
        if mode == "blocked" {
            config["sources"][0]["reply_horizon_ms"] = serde_json::from_str("5e3").unwrap();
        }
        let server = thread::spawn(move || {
            let first = request_frame(LOCAL, 0, 0x82, 0, 0, 1);
            let client = udp_request(&socket, &first);
            if let Some(fixed) = fixed {
                assert_eq!(client, fixed);
            }
            udp_reply(&socket, client, &first, &[0, 7]);
            for sid in 1..=255 {
                let frame = request_frame(LOCAL, sid, 0x82, u16::from(sid), 0, 1);
                assert_eq!(udp_request(&socket, &frame), client);
                udp_reply(&socket, client, &frame, &[0, 7]);
            }
            let mut duplicate = response_frame(&first, &[]);
            duplicate[12..14].copy_from_slice(&0x1103u16.to_be_bytes());
            let _ = socket.send_to(&duplicate, client);
            if mode == "blocked" {
                socket
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let error = socket.recv_from(&mut [0; 32]).unwrap_err();
                assert!(matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ));
                return;
            }
            let next = udp_request(&socket, &first);
            assert_eq!(next == client, fixed.is_some());
            udp_reply(&socket, next, &first, &[0, 8]);
            let second = request_frame(LOCAL, 1, 0x82, 1, 0, 1);
            assert_eq!(udp_request(&socket, &second), next);
            let mut error = response_frame(&second, &[]);
            error[12..14].copy_from_slice(&0x2102u16.to_be_bytes());
            socket.send_to(&error, next).unwrap();
        });
        let blocked = mode == "blocked";
        let error = record_for(&home, &config, "2500ms").failure(if blocked { 4 } else { 1 });
        if blocked {
            assert_eq!(error["details"]["fins_sid"], 0);
            assert_eq!(error["details"]["reply_horizon_ms"], 5000);
        } else {
            assert_eq!(error["details"]["fins_end_code"], 0x2102, "{mode}: {error}");
        }
        server.join().unwrap();
        let recorded = rows(&home);
        assert_eq!(recorded.len(), 258, "{mode}");
        assert!(recorded[1..257].iter().all(|row| row["value"] == 7));
        assert_eq!(recorded[257]["status"], "failed");
    }
}

#[test]
fn udp_requires_configured_unicast_endpoints_and_explicit_nodes() {
    for case in [
        json!({"source_node":null}),
        json!({"target_node":null}),
        json!({"source_node":0}),
        json!({"target_node":255}),
        json!({"address":"0.0.0.0:9600"}),
        json!({"address":"224.0.0.1:9600"}),
        json!({"address":"255.255.255.255:9600"}),
        json!({"address":"[::1]:9600"}),
        json!({"address":"127.0.0.1:0"}),
        json!({"bind":"224.0.0.1:9600"}),
        json!({"reply_horizon_ms":999}),
        json!({"reply_horizon_ms":30_001}),
        json!({"reply_horizon_ms":2000.5}),
    ] {
        let home = Home::new("fins-udp-config");
        let socket = udp_socket();
        let mut config = udp_config(&socket, LOCAL, &json!([{"name":"value","address":0}]));
        for (key, value) in case.as_object().unwrap() {
            config["sources"][0][key].clone_from(value);
        }
        assert_eq!(record(&home, &config).failure(2)["code"], "usage", "{case}");
        assert!(!home.path("run.jsonl").exists());
        socket.set_nonblocking(true).unwrap();
        assert_eq!(
            socket.recv_from(&mut [0; 32]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn udp_requires_exclusive_source_identity_per_peer() {
    let home = Home::new("fins-udp-duplicate-node");
    let socket = udp_socket();
    let mut config = udp_config(&socket, LOCAL, &json!([{"name":"value","address":0}]));
    let mut duplicate = config["sources"][0].clone();
    duplicate["id"] = json!("other");
    duplicate["source_unit"] = json!(2);
    duplicate["target_node"] = json!(19);
    config["sources"].as_array_mut().unwrap().push(duplicate);
    let error = record(&home, &config).failure(2);
    assert_eq!(error["code"], "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("one signal list")
    );
    assert!(!home.path("run.jsonl").exists());
    socket.set_nonblocking(true).unwrap();
    assert_eq!(
        socket.recv_from(&mut [0; 32]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
