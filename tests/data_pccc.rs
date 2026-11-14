//! SLC/`MicroLogix` `Execute_PCCC` reads, independently framed from Rockwell
//! 1770-RM516 section 7-17 and 22COMM-UM004 appendix C, through the actual CLI.
#[path = "support/pccc_csp.rs"]
mod pccc_csp;
#[path = "support/pccc_plc5.rs"]
mod pccc_plc5;
#[path = "support/pccc_st.rs"]
mod pccc_st;
#[path = "support/pccc_tc.rs"]
mod pccc_tc;
mod support;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
use support::{cli::Home, time};

const SESSION: u32 = 0x1234_5678;
const IDENTITY: [u8; 7] = [7, 0, 0, 0x12, 0x34, 0x56, 0x78];
fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let end = Instant::now() + time::secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(time::secs(3))).unwrap();
                stream.set_write_timeout(Some(time::secs(3))).unwrap();
                stream.set_nodelay(true).unwrap();
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < end, "PCCC client did not connect");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
struct Packet {
    header: [u8; 24],
    body: Vec<u8>,
}
fn receive(stream: &mut TcpStream) -> Packet {
    let mut header = [0; 24];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[8..12], &[0; 4]);
    assert_eq!(&header[20..24], &[0; 4]);
    assert!(
        matches!(
            u16::from_le_bytes([header[0], header[1]]),
            0x65 | 0x66 | 0x6f
        ),
        "only session housekeeping and reads are allowed"
    );
    let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
    assert!(size <= 1024);
    let mut body = vec![0; size];
    stream.read_exact(&mut body).unwrap();
    Packet { header, body }
}
fn reply(request: &Packet, body: &[u8], session: u32) -> Vec<u8> {
    let mut response = request.header.to_vec();
    response[2..4].copy_from_slice(&u16::try_from(body.len()).unwrap().to_le_bytes());
    response[4..8].copy_from_slice(&session.to_le_bytes());
    response.extend_from_slice(body);
    response
}
fn send(stream: &mut TcpStream, response: &[u8]) {
    for part in response.chunks(3) {
        stream.write_all(part).unwrap();
    }
}
fn register(stream: &mut TcpStream) {
    let packet = receive(stream);
    assert_eq!(
        &packet.header[..12],
        &[0x65, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(&packet.header[12..20], &[0; 8]);
    assert_eq!(packet.body, [1, 0, 0, 0]);
    send(stream, &reply(&packet, &[1, 0, 0, 0], SESSION));
}
fn cleanup(stream: &mut TcpStream) {
    let packet = receive(stream);
    assert_eq!(&packet.header[..4], &[0x66, 0, 0, 0]);
    assert_eq!(&packet.header[4..8], &SESSION.to_le_bytes());
    assert_eq!(&packet.header[12..20], &[0; 8]);
    assert_eq!(packet.body, [] as [u8; 0]);
}
fn execute_pccc(packet: &Packet, slot: Option<u8>) -> &[u8] {
    assert_eq!(&packet.header[..2], &[0x6f, 0]);
    assert_eq!(&packet.header[4..8], &SESSION.to_le_bytes());
    assert!(packet.body.len() >= 16);
    assert_eq!(
        &packet.body[..14],
        &[0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0]
    );
    assert_eq!(
        usize::from(u16::from_le_bytes(packet.body[14..16].try_into().unwrap())),
        packet.body.len() - 16
    );
    let data = &packet.body[16..];
    let bytes = slot.map_or(data, |slot| {
        let bytes = data;
        assert!(bytes.len() >= 14);
        assert_eq!(&bytes[..8], &[0x52, 2, 0x20, 6, 0x24, 1, 0x0a, 0x0e]);
        let length = usize::from(u16::from_le_bytes(bytes[8..10].try_into().unwrap()));
        let tail = 10 + length;
        let padded = tail + length % 2;
        assert_eq!(&bytes[padded..], &[1, 0, 1, slot]);
        if padded != tail {
            assert_eq!(bytes[tail], 0);
        }
        &bytes[10..tail]
    });
    assert!(bytes.len() >= 23);
    assert_eq!(&bytes[..6], &[0x4b, 2, 0x20, 0x67, 0x24, 1]);
    bytes
}
fn inner(packet: &Packet, slot: Option<u8>) -> &[u8] {
    let bytes = execute_pccc(packet, slot);
    assert!(bytes.len() >= 18);
    assert_eq!(&bytes[6..13], &IDENTITY);
    assert_eq!(&bytes[13..15], &[0x0f, 0]);
    assert_ne!(&bytes[15..17], &[0, 0]);
    assert_eq!(
        bytes[17], 0xa2,
        "only protected typed logical read is permitted"
    );
    bytes
}
fn logical(bytes: &mut &[u8]) -> u16 {
    let first = *bytes.first().unwrap();
    *bytes = &bytes[1..];
    if first != 0xff {
        return u16::from(first);
    }
    let value = u16::from_le_bytes(*bytes.first_chunk::<2>().unwrap());
    *bytes = &bytes[2..];
    value
}
fn read_request(
    stream: &mut TcpStream,
    slot: Option<u8>,
    expected: (u16, u8, u16, usize),
) -> Packet {
    let packet = receive(stream);
    let pccc = inner(&packet, slot);
    assert_eq!(usize::from(pccc[18]), expected.3);
    let mut fields = &pccc[19..];
    assert_eq!(logical(&mut fields), expected.0);
    assert_eq!(fields[0], expected.1);
    fields = &fields[1..];
    assert_eq!(logical(&mut fields), expected.2);
    assert_eq!(logical(&mut fields), 0);
    assert_eq!(fields, [] as [u8; 0]);
    packet
}
fn cpf(cip: &[u8]) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
    body.extend_from_slice(&u16::try_from(cip.len()).unwrap().to_le_bytes());
    body.extend_from_slice(cip);
    body
}
fn response(request: &Packet, slot: Option<u8>, data: &[u8]) -> Vec<u8> {
    let payload = inner(request, slot);
    assert!(payload.len() >= 17);
    let mut cip = vec![0xcb, 0, 0, 0];
    cip.extend_from_slice(&IDENTITY);
    cip.extend_from_slice(&[0x4f, 0, payload[15], payload[16]]);
    cip.extend_from_slice(data);
    if slot.is_some() {
        cip.splice(..0, [0xd2, 0, 0, 0]);
    }
    reply(request, &cpf(&cip), SESSION)
}
fn config(listener: &TcpListener, signals: Value) -> Value {
    let mut config = json!({"version":1,"sources":[{"id":"plc","protocol":"pccc","address":listener.local_addr().unwrap().to_string(),"every_ms":3_600_000,"timeout_ms":1000,"vendor_id":0,"serial_number":0x7856_3412_u32},{"id":"sensor","protocol":"simulator","signal":"tick","every_ms":10}]});
    config["sources"][0]["signals"] = signals;
    config
}

fn record(home: &Home, config: &Value, duration: &str) -> support::cli::Run {
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
fn readings(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["kind"] == "sample" && row["source"] == "plc")
        .collect()
}

#[test]
fn direct_and_routed_typed_files_segment_arrays_preserve_values_and_export() {
    for slot in [None, Some(4)] {
        let home = Home::new("pccc-values");
        let server = listener();
        let mut config = config(
            &server,
            json!([
                {"name":"integers","address":"N7:254","elements":5},
                {"name":"floats","address":"F8:1","elements":2},
                {"name":"bit","address":"B3:0/15"},
                {"name":"unsigned","address":"N65535:65535","data_type":"u16"},
                {"name":"scaled","address":"n10:1","scale":0.5,"offset":100.0}
            ]),
        );
        config["sources"][0]["max_read_bytes"] = 4.into();
        if let Some(slot) = slot {
            config["sources"][0]["slot"] = slot.into();
        }
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let cases = [
                (
                    (7, 0x89, 254, 4),
                    [-32768_i16, 32767]
                        .into_iter()
                        .flat_map(i16::to_le_bytes)
                        .collect::<Vec<_>>(),
                ),
                (
                    (7, 0x89, 256, 4),
                    [-1_i16, 0].into_iter().flat_map(i16::to_le_bytes).collect(),
                ),
                ((7, 0x89, 258, 2), 19_i16.to_le_bytes().to_vec()),
                ((8, 0x8a, 1, 4), 1.25_f32.to_le_bytes().to_vec()),
                ((8, 0x8a, 2, 4), (-12.5_f32).to_le_bytes().to_vec()),
                ((3, 0x85, 0, 2), 0x8000_u16.to_le_bytes().to_vec()),
                ((65535, 0x89, 65535, 2), u16::MAX.to_le_bytes().to_vec()),
                ((10, 0x89, 1, 2), (-5_i16).to_le_bytes().to_vec()),
            ];
            let mut last_tns = 0;
            let mut last_context = 0;
            for (expected, data) in cases {
                let packet = read_request(&mut stream, slot, expected);
                let pccc = inner(&packet, slot);
                let tns = u16::from_le_bytes(pccc[15..17].try_into().unwrap());
                let context = u64::from_le_bytes(packet.header[12..20].try_into().unwrap());
                assert!(tns > last_tns && context > last_context);
                last_tns = tns;
                last_context = context;
                send(&mut stream, &response(&packet, slot, &data));
            }
            cleanup(&mut stream);
        });
        let run = record(&home, &config, "300ms");
        assert!(run.code == Some(0), "{}", run.stderr);
        worker.join().unwrap();
        let rows = readings(&home);
        assert_eq!(rows.len(), 5);
        let values = rows
            .iter()
            .map(|row| row["value"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            [
                json!([-32768, 32767, -1, 0, 19]),
                json!([1.25, -12.5]),
                json!(true),
                json!(65535),
                json!(97.5)
            ]
        );
        assert!(rows.iter().all(|row| row["elapsed_ns"].is_u64()
            && row["elapsed_ns"] == rows[0]["elapsed_ns"]
            && row.get("source_time").is_none()));
        let csv = home.run(&["--json", "data", "export", "run.jsonl", "-o", "out.csv"]);
        assert!(csv.code == Some(0), "{}", csv.stderr);
        assert!(
            fs::read_to_string(home.path("out.csv"))
                .unwrap()
                .contains("65535")
        );
        let jsonl = home.run(&[
            "--json",
            "data",
            "export",
            "run.jsonl",
            "--format",
            "jsonl",
            "-o",
            "out.jsonl",
        ]);
        assert!(jsonl.code == Some(0), "{}", jsonl.stderr);
        assert!(
            fs::read_to_string(home.path("out.jsonl"))
                .unwrap()
                .contains("97.5")
        );
    }
}

#[test]
fn supported_micrologix_long_files_preserve_exact_signed_unsigned_and_bits() {
    let home = Home::new("pccc-long");
    let server = listener();
    let mut config = config(
        &server,
        json!([
            {"name":"signed","address":"L19:0","elements":2},
            {"name":"unsigned","address":"L19:5","data_type":"u32"},
            {"name":"high_bit","address":"L19:6/31"},
            {"name":"low_bit","address":"L19:7","data_type":"bool"}
        ]),
    );
    config["sources"][0]["max_read_bytes"] = 4.into();
    let worker = thread::spawn(move || {
        let mut stream = accept(&server);
        register(&mut stream);
        for (element, data) in [
            (0, i32::MIN.to_le_bytes()),
            (1, i32::MAX.to_le_bytes()),
            (5, u32::MAX.to_le_bytes()),
            (6, 0x8000_0000_u32.to_le_bytes()),
            (7, 2_u32.to_le_bytes()),
        ] {
            let packet = read_request(&mut stream, None, (19, 0x91, element, 4));
            send(&mut stream, &response(&packet, None, &data));
        }
        cleanup(&mut stream);
    });
    let run = record(&home, &config, "250ms");
    assert_eq!(run.code, Some(0), "{}", run.stdout);
    worker.join().unwrap();
    let values = readings(&home)
        .into_iter()
        .map(|row| row["value"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        [
            json!([i32::MIN, i32::MAX]),
            json!(u32::MAX),
            json!(true),
            json!(false)
        ]
    );
}

#[test]
fn malformed_layers_and_rejected_reads_never_publish_partial_polls() {
    for mode in 0..26 {
        let home = Home::new("pccc-invalid-reply");
        let server = listener();
        let config = config(
            &server,
            json!([{"name":"first","address":"N7:0"},{"name":"second","address":"N7:1"}]),
        );
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let first = read_request(&mut stream, None, (7, 0x89, 0, 2));
            send(&mut stream, &response(&first, None, &[1, 0]));
            let request = read_request(&mut stream, None, (7, 0x89, 1, 2));
            let mut packet = response(&request, None, &[2, 0]);
            // Offsets: ENIP24, CPF16, CIP4, requestor7, command/status/TNS4.
            match mode {
                0 => packet[0] = 0x65,
                1 => packet[4] ^= 1,
                2 => packet[4..8].fill(0),
                3 => packet[8] = 1,
                4 => packet[12] ^= 1,
                5 => packet[20] = 1,
                6 => packet[24] = 1,
                7 => packet[30] = 3,
                8 => packet[36] = 0xb1,
                9 => packet[38] += 1,
                10 => packet[40] = 0xcc,
                11 => packet[41] = 1,
                12 => packet[42] = 8,
                13 => packet[43] = 2,
                14 => packet[44] = 0,
                15 => packet[45] = 1,
                16 => packet[47] ^= 1,
                17 => packet[51] = 0x0f,
                18 => packet[52] = 0x10,
                19 => packet[53..55].fill(0),
                20 => packet[53] ^= 1,
                21 => {
                    packet.pop();
                    let size = u16::try_from(packet.len() - 24).unwrap();
                    packet[2..4].copy_from_slice(&size.to_le_bytes());
                    let length = u16::try_from(packet.len() - 40).unwrap();
                    packet[38..40].copy_from_slice(&length.to_le_bytes());
                }
                22 => {
                    packet.push(0);
                    let size = u16::try_from(packet.len() - 24).unwrap();
                    packet[2..4].copy_from_slice(&size.to_le_bytes());
                    let length = u16::try_from(packet.len() - 40).unwrap();
                    packet[38..40].copy_from_slice(&length.to_le_bytes());
                }
                23 => {
                    packet[52] = 0xf0;
                    packet[55] = 0x10;
                }
                24 => packet[53..55].copy_from_slice(&1_u16.to_le_bytes()),
                25 => packet[12..20].copy_from_slice(&1_u64.to_le_bytes()),
                _ => unreachable!(),
            }
            let _ = stream.write_all(&packet);
        });
        let run = record(&home, &config, "300ms");
        assert!(run.code != Some(0), "mode {mode}: {}", run.stdout);
        worker.join().unwrap();
        assert_eq!(readings(&home), [] as [Value; 0], "mode {mode}");
        assert!(run.json()["error"].is_object());
    }
}

#[test]
fn invalid_registration_prevents_all_file_reads() {
    for mode in 0..9 {
        let home = Home::new("pccc-registration");
        let server = listener();
        let config = config(&server, json!([{"name":"word","address":"N7:0"}]));
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            let request = receive(&mut stream);
            let mut packet = reply(&request, &[1, 0, 0, 0], SESSION);
            match mode {
                0 => packet[0] = 0x64,
                1 => packet[4..8].fill(0),
                2 => packet[8] = 1,
                3 => packet[12] ^= 1,
                4 => packet[20] = 1,
                5 => packet[24] = 2,
                6 => packet[26] = 1,
                7 => {
                    packet[2..4].copy_from_slice(&4097_u16.to_le_bytes());
                    packet.truncate(24);
                }
                8 => {
                    packet.pop();
                    packet[2] = 3;
                }
                _ => unreachable!(),
            }
            let _ = stream.write_all(&packet);
        });
        let run = record(&home, &config, "250ms");
        assert!(run.code != Some(0), "mode {mode}: {}", run.stdout);
        worker.join().unwrap();
        assert_eq!(readings(&home), [] as [Value; 0]);
    }
}

#[test]
fn segmentation_and_registration_share_one_poll_deadline() {
    let home = Home::new("pccc-deadline");
    let server = listener();
    let mut config = config(
        &server,
        json!([{"name":"words","address":"N7:0","elements":4}]),
    );
    config["sources"].as_array_mut().unwrap().truncate(1);
    config["sources"][0]["timeout_ms"] = time::ms(100).into();
    config["sources"][0]["max_read_bytes"] = 4.into();
    let worker = thread::spawn(move || {
        let mut stream = accept(&server);
        let server_began = Instant::now();
        let registration = receive(&mut stream);
        thread::sleep(time::millis(25));
        send(&mut stream, &reply(&registration, &[1, 0, 0, 0], SESSION));
        let first = read_request(&mut stream, None, (7, 0x89, 0, 4));
        thread::sleep(time::millis(25));
        send(&mut stream, &response(&first, None, &[1, 0, 2, 0]));
        let second = read_request(&mut stream, None, (7, 0x89, 2, 4));
        let response = response(&second, None, &[3, 0, 4, 0]);
        for byte in response {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(time::millis(5));
        }
        server_began.elapsed()
    });
    let began = Instant::now();
    let run = record(&home, &config, &time::arg(1000));
    assert_ne!(run.code, Some(0));
    assert_eq!(run.json()["error"]["code"], "timeout");
    let server_elapsed = worker.join().unwrap();
    assert!(
        server_elapsed < time::millis(250),
        "network elapsed={server_elapsed:?}"
    );
    assert!(began.elapsed() < time::secs(2));
    assert_eq!(readings(&home), [] as [Value; 0]);
}

#[test]
fn successful_individual_phases_cannot_reset_the_total_deadline() {
    let home = Home::new("pccc-phases");
    let server = listener();
    let mut config = config(
        &server,
        json!([{"name":"words","address":"N7:0","elements":4}]),
    );
    config["sources"].as_array_mut().unwrap().truncate(1);
    config["sources"][0]["timeout_ms"] = time::ms(100).into();
    config["sources"][0]["max_read_bytes"] = 4.into();
    let worker = thread::spawn(move || {
        let mut stream = accept(&server);
        let registration = receive(&mut stream);
        thread::sleep(time::millis(65));
        send(&mut stream, &reply(&registration, &[1, 0, 0, 0], SESSION));
        let first = read_request(&mut stream, None, (7, 0x89, 0, 4));
        thread::sleep(time::millis(65));
        if stream
            .write_all(&response(&first, None, &[1, 0, 2, 0]))
            .is_err()
        {
            return;
        }
        let mut header = [0; 24];
        if stream.read_exact(&mut header).is_err() || header[0] == 0x66 {
            return;
        }
        let mut body = vec![0; usize::from(u16::from_le_bytes([header[2], header[3]]))];
        stream.read_exact(&mut body).unwrap();
        let second = Packet { header, body };
        let pccc = inner(&second, None);
        assert_eq!(&pccc[19..], &[7, 0x89, 2, 0]);
        let _ = stream.write_all(&response(&second, None, &[3, 0, 4, 0]));
    });
    let run = record(&home, &config, &time::arg(500));
    assert_eq!(run.json()["error"]["code"], "timeout");
    worker.join().unwrap();
    assert_eq!(readings(&home), [] as [Value; 0]);
}

#[test]
fn unsupported_files_invalid_shapes_and_scaling_create_nothing() {
    let signals = [
        json!({"name":"v","address":"PLC5:N7:0"}),
        json!({"name":"v","address":"ST9:0.LEN"}),
        json!({"name":"v","address":"T4:0.ACC"}),
        json!({"name":"v","address":"N65536:0"}),
        json!({"name":"v","address":"N7:65535","elements":2}),
        json!({"name":"v","address":"N7:0","elements":0}),
        json!({"name":"v","address":"N7:0","elements":4097}),
        json!({"name":"v","address":"F8:0/1"}),
        json!({"name":"v","address":"N7:0/16"}),
        json!({"name":"v","address":"B3:0/1","elements":2}),
        json!({"name":"v","address":"B3:0/1","scale":2.0}),
        json!({"name":"v","address":"N7:0/1","data_type":"u16"}),
        json!({"name":"v","address":"N7:0","data_type":"f32"}),
        json!({"name":"v","address":"L19:0","data_type":"u16"}),
        json!({"name":"v","address":"L19:0/32"}),
        json!({"name":"v","address":"F8:0","data_type":"i16"}),
        json!({"name":"v","address":"N7:0:-1"}),
        json!({"name":"v","address":"N7:0","unknown":true}),
    ];
    for signal in signals {
        let home = Home::new("pccc-config");
        let server = listener();
        let config = config(&server, json!([signal]));
        let run = record(&home, &config, "100ms");
        assert_eq!(run.json()["error"]["code"], "usage", "{}", run.stdout);
        assert!(!home.path("run.jsonl").exists());
        server.set_nonblocking(true).unwrap();
        assert_eq!(
            server.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn invalid_limits_requestor_identity_and_duplicate_names_create_nothing() {
    for change in [
        json!({"max_read_bytes":0}),
        json!({"max_read_bytes":3}),
        json!({"max_read_bytes":225}),
        json!({"serial_number":0}),
        json!({"signals":[]}),
        json!({"signals":[{"name":"same","address":"N7:0"},{"name":"same","address":"B3:0"}]}),
        json!({"address":"127.0.0.1:0"}),
    ] {
        let home = Home::new("pccc-limits");
        let server = listener();
        let mut config = config(&server, json!([{"name":"v","address":"N7:0"}]));
        for (key, value) in change.as_object().unwrap() {
            config["sources"][0][key] = value.clone();
        }
        let run = record(&home, &config, "100ms");
        assert_eq!(run.json()["error"]["code"], "usage", "{}", run.stdout);
        assert!(!home.path("run.jsonl").exists());
        server.set_nonblocking(true).unwrap();
        assert_eq!(
            server.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn floating_point_nan_and_infinite_scaling_fail_without_samples() {
    for nan in [true, false] {
        let home = Home::new("pccc-nonfinite");
        let server = listener();
        let signal = if nan {
            json!({"name":"v","address":"F8:0"})
        } else {
            json!({"name":"v","address":"N7:0","scale":1e308})
        };
        let config = config(&server, json!([signal]));
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let request = read_request(
                &mut stream,
                None,
                (
                    if nan { 8 } else { 7 },
                    if nan { 0x8a } else { 0x89 },
                    0,
                    if nan { 4 } else { 2 },
                ),
            );
            let data = if nan {
                f32::NAN.to_le_bytes().to_vec()
            } else {
                i16::MAX.to_le_bytes().to_vec()
            };
            send(&mut stream, &response(&request, None, &data));
        });
        assert_ne!(record(&home, &config, "250ms").code, Some(0));
        worker.join().unwrap();
        assert_eq!(readings(&home), [] as [Value; 0]);
    }
}
