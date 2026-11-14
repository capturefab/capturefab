//! Beckhoff ADS/AMS over TCP through the actual CLI, without a vendor runtime.
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::cli::Home;

const TARGET: [u8; 8] = [192, 0, 2, 10, 1, 1, 0x53, 3];
const SOURCE: [u8; 8] = [192, 0, 2, 20, 1, 1, 0x89, 0x80];

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
                assert!(Instant::now() < deadline, "ADS client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn receive(stream: &mut TcpStream, invoke: u32, command: u16) -> Vec<u8> {
    let mut tcp = [0; 6];
    stream.read_exact(&mut tcp).unwrap();
    assert_eq!(&tcp[..2], &[0, 0]);
    let length = u32::from_le_bytes(tcp[2..].try_into().unwrap()) as usize;
    assert!((32..=2048).contains(&length));
    let mut request = vec![0; length];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(&request[..8], TARGET);
    assert_eq!(&request[8..16], SOURCE);
    assert_eq!(&request[16..18], command.to_le_bytes());
    assert_eq!(&request[18..20], 4u16.to_le_bytes());
    assert_eq!(
        &request[20..24],
        u32::try_from(length - 32).unwrap().to_le_bytes()
    );
    assert_eq!(&request[24..28], [0; 4]);
    assert_eq!(&request[28..32], invoke.to_le_bytes());
    request
}
fn read_request(
    stream: &mut TcpStream,
    invoke: u32,
    group: u32,
    offset: u32,
    length: u32,
) -> Vec<u8> {
    let request = receive(stream, invoke, 2);
    let body: Vec<_> = [group, offset, length]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    assert_eq!(
        &request[32..],
        body,
        "only the configured ADS read is allowed"
    );
    request
}
fn response(request: &[u8], data: &[u8]) -> Vec<u8> {
    assert!(request.len() >= 32);
    let mut packet = request[8..16].to_vec();
    packet.extend_from_slice(&request[..8]);
    packet.extend_from_slice(&request[16..18]);
    packet.extend_from_slice(&5u16.to_le_bytes());
    packet.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    packet.extend_from_slice(&[0; 4]);
    packet.extend_from_slice(&request[28..32]);
    packet.extend_from_slice(data);
    packet
}
fn send(stream: &mut TcpStream, packet: &[u8]) {
    let mut tcp = vec![0, 0];
    tcp.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_le_bytes());
    tcp.extend_from_slice(packet);
    // TCP may split the prefix, AMS header and payload at any byte.
    for chunk in tcp.chunks(7) {
        stream.write_all(chunk).unwrap();
    }
}
fn read_body(bytes: &[u8]) -> Vec<u8> {
    let mut data = vec![0; 4];
    data.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
    data.extend_from_slice(bytes);
    data
}
fn reply(stream: &mut TcpStream, request: &[u8], bytes: &[u8]) {
    send(stream, &response(request, &read_body(bytes)));
}
fn config(listener: &TcpListener, signals: &Value) -> Value {
    json!({"version":1,"sources":[
        {"id":"plc","protocol":"ads","address":listener.local_addr().unwrap().to_string(),
         "target_net_id":"192.0.2.10.1.1","source_net_id":"192.0.2.20.1.1",
         "every_ms":3_600_000,"timeout_ms":1000,"signals":signals},
        {"id":"sensor","protocol":"simulator","signal":"tick","every_ms":10}
    ]})
}
fn record(home: &Home, config: &Value) -> support::cli::Run {
    fs::write(home.path("sources.json"), config.to_string()).unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "500ms",
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

#[test]
fn direct_reads_preserve_exact_values_arrays_strings_and_opaque_bytes() {
    let home = Home::new("ads-values");
    let listener = listener();
    let values = [
        ("flag", "bool", vec![1], json!(true)),
        ("small", "i8", vec![0x80], json!(-128)),
        ("byte", "u8", vec![255], json!(255)),
        ("word", "u16", u16::MAX.to_le_bytes().to_vec(), json!(65535)),
        (
            "signed_word",
            "i16",
            i16::MIN.to_le_bytes().to_vec(),
            json!(-32768),
        ),
        (
            "dword",
            "u32",
            u32::MAX.to_le_bytes().to_vec(),
            json!(u32::MAX),
        ),
        (
            "signed_dword",
            "i32",
            i32::MIN.to_le_bytes().to_vec(),
            json!(i32::MIN),
        ),
        (
            "counter",
            "u64",
            u64::MAX.to_le_bytes().to_vec(),
            json!(u64::MAX),
        ),
        (
            "signed_counter",
            "i64",
            i64::MIN.to_le_bytes().to_vec(),
            json!(i64::MIN),
        ),
        (
            "pressure",
            "f32",
            12.5_f32.to_le_bytes().to_vec(),
            json!(12.5),
        ),
        (
            "temperature",
            "f64",
            37.5_f64.to_le_bytes().to_vec(),
            json!(19.75),
        ),
        (
            "opaque",
            "bytes",
            vec![0, 0xff, 0, 0x80],
            json!({"encoding":"base64","data":"AP8AgA=="}),
        ),
        (
            "label",
            "string",
            "café\0\0".as_bytes().to_vec(),
            json!("café"),
        ),
        (
            "wide",
            "wstring",
            "温度😀\0"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect(),
            json!("温度😀"),
        ),
        (
            "latin1_array",
            "string",
            b"caf\xe9\0\xff\0stale".to_vec(),
            json!(["café", ""]),
        ),
        (
            "wide_array",
            "wstring",
            [0x6e29_u16, 0, 0xd800, 0xffff, 0x00e9, 0, 0xd800, 0xffff]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
            json!(["温", "é"]),
        ),
        (
            "array",
            "i64",
            [i64::MIN, 0, i64::MAX]
                .into_iter()
                .flat_map(i64::to_le_bytes)
                .collect(),
            json!([i64::MIN, 0, i64::MAX]),
        ),
    ];
    let signals: Vec<_> = values.iter().enumerate().map(|(index, (name, kind, bytes, _))| {
        let mut signal = json!({"name":name,"data_type":kind,"index_group":0x4020,"index_offset":index * 32});
        if matches!(*kind, "bytes" | "string" | "wstring") {
            signal["length"] = json!(bytes.len());
        }
        if matches!(*name, "latin1_array" | "wide_array") {
            signal["length"] = json!(bytes.len() / 2);
            signal["elements"] = serde_json::from_str::<Value>("2e0").unwrap();
        }
        if *name == "latin1_array" { signal["encoding"] = json!("latin1"); }
        if *name == "array" { signal["elements"] = json!(3); }
        if *name == "temperature" { signal["scale"] = json!(0.5); signal["offset"] = json!(1); }
        signal
    }).collect();
    let config = config(&listener, &json!(signals));
    let expected: Vec<_> = values
        .iter()
        .map(|(name, _, _, value)| (*name, value.clone()))
        .collect();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        for (index, (_, _, bytes, _)) in values.iter().enumerate() {
            let request = read_request(
                &mut stream,
                u32::try_from(index).unwrap() + 1,
                0x4020,
                u32::try_from(index).unwrap() * 32,
                u32::try_from(bytes.len()).unwrap(),
            );
            reply(&mut stream, &request, bytes);
        }
        let mut closed = [0];
        assert_eq!(
            stream.read(&mut closed).unwrap(),
            0,
            "direct reads must not send writes"
        );
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    let rows = rows(&home);
    let samples: Vec<_> = rows.iter().filter(|row| row["source"] == "plc").collect();
    assert_eq!(samples.len(), expected.len());
    for (sample, (name, value)) in samples.iter().zip(expected) {
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

fn lookup(stream: &mut TcpStream, invoke: u32, name: &str, handle: u32) {
    let request = receive(stream, invoke, 9);
    let mut expected: Vec<_> = [0xf003, 0, 4, u32::try_from(name.len()).unwrap() + 1]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    expected.extend_from_slice(name.as_bytes());
    expected.push(0);
    assert_eq!(
        &request[32..],
        expected,
        "ReadWrite is restricted to obtaining a symbol handle"
    );
    reply(stream, &request, &handle.to_le_bytes());
}
fn release(stream: &mut TcpStream, invoke: u32, handle: u32, status: u32) {
    let request = receive(stream, invoke, 3);
    let expected: Vec<_> = [0xf006, 0, 4, handle]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    assert_eq!(
        &request[32..],
        expected,
        "Write is restricted to releasing a symbol handle"
    );
    send(stream, &response(&request, &status.to_le_bytes()));
}

#[test]
fn symbolic_handles_are_shared_then_released_without_plc_data_writes() {
    let home = Home::new("ads-symbols");
    let listener = listener();
    let config = config(
        &listener,
        &json!([
            {"name":"first","symbol":"MAIN.counter","data_type":"u64"},
            {"name":"same","symbol":"MAIN.counter","data_type":"u64"},
            {"name":"other","symbol":"MAIN.ready","data_type":"bool"}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        lookup(&mut stream, 1, "MAIN.counter", 0x1234);
        for invoke in [2, 3] {
            let request = read_request(&mut stream, invoke, 0xf005, 0x1234, 8);
            reply(&mut stream, &request, &u64::MAX.to_le_bytes());
        }
        lookup(&mut stream, 4, "MAIN.ready", 0x4321);
        let request = read_request(&mut stream, 5, 0xf005, 0x4321, 1);
        reply(&mut stream, &request, &[1]);
        release(&mut stream, 6, 0x1234, 0);
        release(&mut stream, 7, 0x4321, 0);
    });
    assert_eq!(record(&home, &config).success()["status"], "complete");
    server.join().unwrap();
    assert_eq!(
        rows(&home)
            .iter()
            .filter(|row| row["source"] == "plc")
            .count(),
        3
    );
}

#[test]
fn rejected_reads_and_failed_release_are_reported_and_handles_cleaned_up() {
    for release_failure in [false, true] {
        let home = Home::new("ads-service-error");
        let listener = listener();
        let config = config(&listener, &json!([{"name":"value","symbol":"MAIN.value"}]));
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            lookup(&mut stream, 1, "MAIN.value", 17);
            let request = read_request(&mut stream, 2, 0xf005, 17, 2);
            if release_failure {
                reply(&mut stream, &request, &[7, 0]);
            } else {
                let mut data = 0x710_u32.to_le_bytes().to_vec();
                data.extend_from_slice(&0u32.to_le_bytes());
                send(&mut stream, &response(&request, &data));
            }
            release(&mut stream, 3, 17, if release_failure { 0x714 } else { 0 });
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed");
        assert_eq!(
            error["details"]["ads_error"],
            if release_failure { 0x714 } else { 0x710 }
        );
        server.join().unwrap();
        let rows = rows(&home);
        assert_eq!(rows.last().unwrap()["status"], "failed");
        assert_eq!(
            rows.iter().filter(|row| row["source"] == "plc").count(),
            usize::from(release_failure)
        );
    }
}

#[test]
fn malformed_and_rejected_responses_never_publish_a_partial_poll() {
    for mode in [
        "invoke",
        "target",
        "source",
        "command",
        "flags",
        "ams_length",
        "ads_length",
        "oversized",
        "reserved",
        "ams_error",
        "ads_error",
        "nonfinite",
        "short",
        "bool",
        "utf8",
        "utf16",
        "unterminated",
        "unterminated_array_slot",
    ] {
        let home = Home::new("ads-malformed");
        let listener = listener();
        let mut second =
            json!({"name":"second","index_group":0x4020,"index_offset":2,"data_type":"f32"});
        match mode {
            "bool" => second["data_type"] = json!("bool"),
            "utf8" | "unterminated" | "unterminated_array_slot" => {
                second["data_type"] = json!("string");
                second["length"] = json!(4);
                if mode == "unterminated_array_slot" {
                    second["elements"] = json!(2);
                }
            }
            "utf16" => {
                second["data_type"] = json!("wstring");
                second["length"] = json!(4);
            }
            _ => {}
        }
        let config = config(
            &listener,
            &json!([
                {"name":"first","index_group":0x4020,"index_offset":0}, second
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let request = read_request(&mut stream, 1, 0x4020, 0, 2);
            reply(&mut stream, &request, &[7, 0]);
            let request = read_request(
                &mut stream,
                2,
                0x4020,
                2,
                match mode {
                    "bool" => 1,
                    "unterminated_array_slot" => 8,
                    _ => 4,
                },
            );
            if mode == "oversized" || mode == "reserved" {
                let mut tcp = vec![u8::from(mode == "reserved"), 0];
                tcp.extend_from_slice(
                    &(if mode == "oversized" { u32::MAX } else { 44 }).to_le_bytes(),
                );
                stream.write_all(&tcp).unwrap();
                return;
            }
            let bytes = match mode {
                "nonfinite" => f32::NAN.to_le_bytes().to_vec(),
                "short" => vec![0, 0],
                "bool" => vec![2],
                "utf8" => vec![0xff, 0, 0, 0],
                "utf16" => vec![0, 0xd8, 0, 0],
                "unterminated" => vec![b'a'; 4],
                "unterminated_array_slot" => b"a\0\xff\xffbbbb".to_vec(),
                _ => 12.5_f32.to_le_bytes().to_vec(),
            };
            let mut packet = response(&request, &read_body(&bytes));
            match mode {
                "invoke" => packet[28] ^= 1,
                "target" => packet[0] ^= 1,
                "source" => packet[8] ^= 1,
                "command" => packet[16] ^= 1,
                "flags" => packet[18] = 4,
                "ams_length" => packet[20] ^= 1,
                "ads_length" => packet[36] ^= 1,
                "ams_error" => packet[24..28].copy_from_slice(&7u32.to_le_bytes()),
                "ads_error" => packet[32..36].copy_from_slice(&0x702u32.to_le_bytes()),
                _ => {}
            }
            send(&mut stream, &packet);
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed", "{mode}: {error}");
        if mode == "ams_error" {
            assert_eq!(error["details"]["ams_error"], 7);
        }
        if mode == "ads_error" {
            assert_eq!(error["details"]["ads_error"], 0x702);
        }
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["source"] == "plc"), "{mode}");
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn trickled_symbol_lookup_and_multiple_reads_share_one_poll_deadline() {
    for symbolic in [false, true] {
        let home = Home::new("ads-timeout");
        let listener = listener();
        let signal = if symbolic {
            json!({"name":"value","symbol":"MAIN.value"})
        } else {
            json!({"name":"value","index_group":0x4020,"index_offset":0})
        };
        let mut config = config(&listener, &json!([signal]));
        config["sources"][0]["timeout_ms"] = json!(80);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let request = receive(&mut stream, 1, if symbolic { 9 } else { 2 });
            let packet = response(&request, &read_body(&[0; 4]));
            let mut tcp = vec![0, 0];
            tcp.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_le_bytes());
            tcp.extend_from_slice(&packet);
            for byte in tcp {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(25));
            }
        });
        let start = Instant::now();
        assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
    let home = Home::new("ads-poll-budget");
    let listener = listener();
    let mut config = config(
        &listener,
        &json!([
            {"name":"first","index_group":0x4020,"index_offset":0},
            {"name":"second","index_group":0x4020,"index_offset":2}
        ]),
    );
    config["sources"][0]["timeout_ms"] = json!(100);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let request = read_request(&mut stream, 1, 0x4020, 0, 2);
        thread::sleep(Duration::from_millis(65));
        reply(&mut stream, &request, &[0, 0]);
        let request = read_request(&mut stream, 2, 0x4020, 2, 2);
        thread::sleep(Duration::from_millis(65));
        let packet = response(&request, &read_body(&[0, 0]));
        let mut tcp = vec![0, 0];
        tcp.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_le_bytes());
        tcp.extend_from_slice(&packet);
        let _ = stream.write_all(&tcp);
    });
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    server.join().unwrap();
    assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
}

#[test]
fn invalid_configuration_is_rejected_before_output_or_network_access() {
    let cases = [
        json!({"name":"value","index_group":0x4020}),
        json!({"name":"value","symbol":"MAIN.value","index_group":0x4020,"index_offset":0}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"u64","scale":0.5}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"bytes","length":65537}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"wstring","length":3}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"wstring","length":4,"encoding":"utf8"}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"string","length":32769,"elements":2}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"bytes","length":4,"elements":2}),
        json!({"name":"value","symbol":"MAIN.value","elements":4097}),
        json!({"name":"value","symbol":"MAIN.value","data_type":"string"}),
    ];
    for signal in cases {
        let home = Home::new("ads-config");
        let listener = listener();
        let config = config(&listener, &json!([signal]));
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn json_string_expansion_is_counted_in_the_atomic_batch_limit() {
    let home = Home::new("ads-batch-bound");
    let listener = listener();
    let signals: Vec<_> = (0..3).map(|index| json!({"name":format!("text_{index}"),"index_group":0x4020,"index_offset":index,"data_type":"string","length":65536})).collect();
    let config = config(&listener, &json!(signals));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let mut bytes = vec![1; 65536];
        bytes[65535] = 0;
        for index in 0..3 {
            let request = read_request(&mut stream, index + 1, 0x4020, index, 65536);
            let packet = response(&request, &read_body(&bytes));
            let mut tcp = vec![0, 0];
            tcp.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_le_bytes());
            tcp.extend_from_slice(&packet);
            stream.write_all(&tcp).unwrap();
        }
    });
    let error = record(&home, &config).failure(1);
    assert_eq!(error["code"], "failed");
    assert!(
        error["message"].as_str().unwrap().contains("1 MiB"),
        "{error}"
    );
    server.join().unwrap();
    assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
}

#[test]
fn symbol_cleanup_has_its_own_bounded_deadline() {
    let home = Home::new("ads-cleanup-deadline");
    let listener = listener();
    let config = config(&listener, &json!([{"name":"value","symbol":"MAIN.value"}]));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        lookup(&mut stream, 1, "MAIN.value", 17);
        let request = read_request(&mut stream, 2, 0xf005, 17, 2);
        reply(&mut stream, &request, &[7, 0]);
        let request = receive(&mut stream, 3, 3);
        assert_eq!(&request[32..36], 0xf006u32.to_le_bytes());
        let packet = response(&request, &[0; 4]);
        let mut tcp = vec![0, 0];
        tcp.extend_from_slice(&u32::try_from(packet.len()).unwrap().to_le_bytes());
        tcp.extend_from_slice(&packet);
        for byte in tcp {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }
    });
    let start = Instant::now();
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    assert!(start.elapsed() < Duration::from_secs(3));
    server.join().unwrap();
    assert_eq!(rows(&home).last().unwrap()["status"], "failed");
}
