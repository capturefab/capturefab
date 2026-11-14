//! Mitsubishi 3E device reads through the CLI, without a vendor runtime.
#[path = "support/slmp_ascii.rs"]
mod slmp_ascii;
#[path = "support/slmp_timer.rs"]
mod slmp_timer;
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
                assert!(Instant::now() < deadline, "SLMP client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn receive(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0; 9];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[..2], &[0x50, 0]);
    let length = usize::from(u16::from_le_bytes(header[7..9].try_into().unwrap()));
    assert!((12..=14).contains(&length));
    let mut request = header.to_vec();
    request.resize(9 + length, 0);
    stream.read_exact(&mut request[9..]).unwrap();
    assert_eq!(
        &request[11..13],
        &[1, 4],
        "only read-only 0401 device access is allowed"
    );
    request
}
fn request(
    stream: &mut TcpStream,
    wide: bool,
    route: [u8; 5],
    timer: u16,
    device: (u8, u32, u16, bool),
) -> Vec<u8> {
    let (device, address, points, bits) = device;
    let request = receive(stream);
    assert_eq!(&request[2..7], route);
    let mut expected = timer.to_le_bytes().to_vec();
    expected.extend_from_slice(&0x0401u16.to_le_bytes());
    expected.extend_from_slice(&(u16::from(bits) + if wide { 2 } else { 0 }).to_le_bytes());
    let address = address.to_le_bytes();
    expected.extend_from_slice(if wide { &address } else { &address[..3] });
    expected.push(device);
    if wide {
        expected.push(0);
    }
    expected.extend_from_slice(&points.to_le_bytes());
    assert_eq!(&request[9..], expected);
    request
}
fn response(request: &[u8], status: u16, data: &[u8]) -> Vec<u8> {
    let mut packet = vec![0xd0, 0];
    packet.extend_from_slice(&request[2..7]);
    packet.extend_from_slice(&u16::try_from(data.len() + 2).unwrap().to_le_bytes());
    packet.extend_from_slice(&status.to_le_bytes());
    packet.extend_from_slice(data);
    packet
}
fn send(stream: &mut TcpStream, data: &[u8]) {
    // Exercise splits inside both the 3E prefix and returned words.
    for chunk in data.chunks(5) {
        stream.write_all(chunk).unwrap();
    }
}
fn config(listener: &TcpListener, signals: &Value) -> Value {
    json!({"version":1,"sources":[
        {"id":"plc","protocol":"slmp","address":listener.local_addr().unwrap().to_string(),
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
fn standard_and_iq_r_read_words_bits_arrays_and_exact_counters() {
    for wide in [false, true] {
        let home = Home::new("slmp-values");
        let listener = listener();
        let values = [
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
                "array",
                "i64",
                [i64::MIN, 0, i64::MAX]
                    .into_iter()
                    .flat_map(i64::to_le_bytes)
                    .collect(),
                json!([i64::MIN, 0, i64::MAX]),
            ),
        ];
        let start = if wide { 0xff00_0000 } else { 0 };
        let mut signals: Vec<_> = values.iter().enumerate().map(|(index,(name,kind,_,_))| {
            let mut signal = json!({"name":name,"device":"D","address":start + u32::try_from(index).unwrap() * 32,"data_type":kind});
            if *name == "array" { signal["elements"] = json!(3); }
            if *name == "temperature" { signal["scale"] = json!(0.5); signal["offset"] = json!(1); }
            signal
        }).collect();
        signals.push(
            json!({"name":"flags","device":"M","address":10,"data_type":"bool","elements":5}),
        );
        signals.push(json!({"name":"last_address","device":"W","address":if wide { u32::MAX } else { 0xff_ffff }}));
        let mut config = config(&listener, &json!(signals));
        let route = if wide {
            [2, 18, 0xe3, 3, 4]
        } else {
            [0, 255, 255, 3, 0]
        };
        let timer = if wide { 16 } else { 4 };
        if wide {
            config["sources"][0]["device_format"] = json!("iq_r");
            config["sources"][0]["network"] = json!(2);
            config["sources"][0]["station"] = json!(18);
            config["sources"][0]["module_io"] = json!(0x3e3);
            config["sources"][0]["multidrop"] = json!(4);
            config["sources"][0]["monitoring_timer"] = json!(timer);
        }
        let expected: Vec<_> = values
            .iter()
            .map(|(name, _, _, value)| (*name, value.clone()))
            .chain([
                ("flags", json!([true, false, true, false, true])),
                ("last_address", json!(0x1234)),
            ])
            .collect();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            for (index, (_, _, bytes, _)) in values.iter().enumerate() {
                let request = request(
                    &mut stream,
                    wide,
                    route,
                    timer,
                    (
                        0xa8,
                        start + u32::try_from(index).unwrap() * 32,
                        u16::try_from(bytes.len() / 2).unwrap(),
                        false,
                    ),
                );
                send(&mut stream, &response(&request, 0, bytes));
            }
            let read = request(&mut stream, wide, route, timer, (0x90, 10, 5, true));
            send(&mut stream, &response(&read, 0, &[0x10, 0x10, 0x10]));
            let read = request(
                &mut stream,
                wide,
                route,
                timer,
                (0xb4, if wide { u32::MAX } else { 0xff_ffff }, 1, false),
            );
            send(&mut stream, &response(&read, 0, &[0x34, 0x12]));
            let mut closed = [0];
            assert_eq!(
                stream.read(&mut closed).unwrap(),
                0,
                "capture must not emit control or write services"
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
}

#[test]
fn malformed_and_rejected_frames_preserve_atomic_batches() {
    for mode in [
        "subheader",
        "network",
        "station",
        "module",
        "multidrop",
        "zero_length",
        "oversized",
        "short",
        "nonfinite",
        "nibble",
        "padding",
        "end_code",
    ] {
        let home = Home::new("slmp-malformed");
        let listener = listener();
        let bits = matches!(mode, "nibble" | "padding");
        let signal = if bits {
            json!({"name":"second","device":"M","address":10,"data_type":"bool","elements":3})
        } else {
            json!({"name":"second","device":"D","address":2,"data_type":"f32"})
        };
        let config = config(
            &listener,
            &json!([{"name":"first","device":"D","address":0},signal]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let read = request(
                &mut stream,
                false,
                [0, 255, 255, 3, 0],
                4,
                (0xa8, 0, 1, false),
            );
            send(&mut stream, &response(&read, 0, &[7, 0]));
            let read = request(
                &mut stream,
                false,
                [0, 255, 255, 3, 0],
                4,
                (
                    if bits { 0x90 } else { 0xa8 },
                    if bits { 10 } else { 2 },
                    if bits { 3 } else { 2 },
                    bits,
                ),
            );
            let bytes = match mode {
                "short" => vec![0, 0],
                "nonfinite" => f32::INFINITY.to_le_bytes().to_vec(),
                "nibble" => vec![0x21, 0],
                "padding" => vec![0x10, 0x11],
                _ => 12.5_f32.to_le_bytes().to_vec(),
            };
            let mut packet = response(&read, if mode == "end_code" { 0xc051 } else { 0 }, &bytes);
            match mode {
                "subheader" => packet[0] = 0xd4,
                "network" => packet[2] ^= 1,
                "station" => packet[3] ^= 1,
                "module" => packet[4] ^= 1,
                "multidrop" => packet[6] ^= 1,
                "zero_length" => {
                    packet[7..9].copy_from_slice(&0u16.to_le_bytes());
                    packet.truncate(9);
                }
                "oversized" => {
                    packet[7..9].copy_from_slice(&u16::MAX.to_le_bytes());
                    packet.truncate(9);
                }
                _ => {}
            }
            if matches!(
                mode,
                "subheader" | "network" | "station" | "module" | "multidrop"
            ) {
                // The client rejects this complete header before reading the
                // body. An immediate close can reset later body writes on Linux.
                send(&mut stream, &packet[..9]);
                for chunk in packet[9..].chunks(5) {
                    match stream.write_all(chunk) {
                        Ok(()) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) =>
                        {
                            break;
                        }
                        Err(error) => panic!("{mode} fixture response failed: {error}"),
                    }
                }
            } else {
                send(&mut stream, &packet);
            }
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed", "{mode}: {error}");
        let diagnostic = match mode {
            "subheader" | "network" | "station" | "module" | "multidrop" => {
                "3E response subheader or route differs from request"
            }
            "zero_length" | "oversized" => "invalid or oversized 3E response length",
            "short" => "read response length differs from configured signal width",
            "nonfinite" => "non-finite floating-point value",
            "nibble" => "bit response nibble must be 0 or 1",
            "padding" => "odd bit response has nonzero padding nibble",
            "end_code" => "SLMP target rejected read",
            _ => unreachable!("unknown malformed mode"),
        };
        assert!(
            error["message"].as_str().unwrap().contains(diagnostic),
            "{mode}: {error}"
        );
        if mode == "end_code" {
            assert_eq!(error["details"]["slmp_end_code"], 0xc051);
        }
        server.join().unwrap();
        let rows = rows(&home);
        assert!(!rows.iter().any(|row| row["source"] == "plc"), "{mode}");
        assert_eq!(rows.last().unwrap()["status"], "failed");
    }
}

#[test]
fn trickled_response_and_multiple_reads_share_one_poll_deadline() {
    for trickle in [true, false] {
        let home = Home::new("slmp-timeout");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([
                {"name":"first","device":"D","address":0},
                {"name":"second","device":"D","address":2}
            ]),
        );
        config["sources"][0]["timeout_ms"] = json!(100);
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            let read = receive(&mut stream);
            if trickle {
                for byte in response(&read, 0, &[0, 0]) {
                    if stream.write_all(&[byte]).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            } else {
                thread::sleep(Duration::from_millis(65));
                send(&mut stream, &response(&read, 0, &[0, 0]));
                let read = receive(&mut stream);
                thread::sleep(Duration::from_millis(65));
                let _ = stream.write_all(&response(&read, 0, &[0, 0]));
            }
        });
        let start = Instant::now();
        assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
        assert!(start.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        assert!(!rows(&home).iter().any(|row| row["source"] == "plc"));
    }
}

#[test]
fn invalid_config_is_rejected_before_output_or_network_access() {
    for signal in [
        json!({"name":"bad","device":"D","address":0,"data_type":"bool"}),
        json!({"name":"bad","device":"M","address":0}),
        json!({"name":"bad","device":"D","address":0,"elements":961}),
        json!({"name":"bad","device":"M","address":0,"data_type":"bool","elements":7169}),
        json!({"name":"bad","device":"D","address":0xff_ffff,"data_type":"u32"}),
        json!({"name":"bad","device":"D","address":0,"data_type":"u64","scale":0.1}),
        json!({"name":"bad","device":"D","address":0,"elements":0}),
    ] {
        let home = Home::new("slmp-config");
        let listener = listener();
        assert_eq!(
            record(&home, &config(&listener, &json!([signal]))).failure(2)["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
    for (key, value) in [
        ("network", json!(240)),
        ("station", json!(124)),
        ("multidrop", json!(32)),
        ("monitoring_timer", json!(0)),
    ] {
        let home = Home::new("slmp-route-config");
        let listener = listener();
        let mut config = config(
            &listener,
            &json!([{"name":"value","device":"D","address":0}]),
        );
        config["sources"][0][key] = value;
        assert_eq!(record(&home, &config).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}
