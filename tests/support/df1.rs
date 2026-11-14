//! Direct serial DF1 exchanges through the CLI, from Rockwell 1770-RM516
//! chapters 4–7 and `SCADAmetrics EtherMeter` 2.06 printed pages 58–59.

use super::{Device, Home, assert_unpublished, receive, serial_pair};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{Read as _, Write as _},
    sync::atomic::AtomicBool,
    thread,
    time::Duration,
};

const ACK: [u8; 2] = [0x10, 0x06];
const NAK: [u8; 2] = [0x10, 0x15];
const ENQ: [u8; 2] = [0x10, 0x05];

#[derive(Clone, Copy)]
enum Checksum {
    Crc,
    Bcc,
}
impl Checksum {
    fn append(self, body: &[u8], bytes: &mut Vec<u8>) {
        match self {
            Self::Bcc => {
                let sum = body.iter().copied().fold(0_u8, u8::wrapping_add);
                bytes.push(sum.wrapping_neg());
            }
            Self::Crc => {
                // Use the forward polynomial and reverse input/output bits,
                // independently of the production reflected implementation.
                let mut crc = 0_u16;
                for byte in body.iter().copied().chain([3]) {
                    crc ^= u16::from(byte.reverse_bits()) << 8;
                    for _ in 0..8 {
                        crc = (crc << 1) ^ if crc & 0x8000 == 0 { 0 } else { 0x8005 };
                    }
                }
                bytes.extend(crc.reverse_bits().to_le_bytes());
            }
        }
    }
}
fn frame(checksum: Checksum, body: &[u8]) -> Vec<u8> {
    let mut wire = vec![0x10, 2];
    for &byte in body {
        wire.push(byte);
        if byte == 0x10 {
            wire.push(0x10);
        }
    }
    wire.extend([0x10, 3]);
    checksum.append(body, &mut wire);
    wire
}

fn get(master: &mut File, stop: &AtomicBool) -> u8 {
    let mut byte = [0];
    assert!(receive(master, &mut byte, stop), "truncated DF1 request");
    byte[0]
}
fn expect(master: &mut File, stop: &AtomicBool, bytes: &[u8]) {
    let mut actual = vec![0; bytes.len()];
    assert!(receive(master, &mut actual, stop), "missing DF1 output");
    assert_eq!(actual, bytes);
}
fn command(
    master: &mut File,
    stop: &AtomicBool,
    checksum: Checksum,
    fields: &[u8],
) -> ([u8; 2], Vec<u8>) {
    expect(master, stop, &[0x10, 2]);
    let mut body = Vec::new();
    loop {
        let byte = get(master, stop);
        if byte == 0x10 {
            match get(master, stop) {
                0x03 => break,
                0x10 => {}
                control => panic!("unexpected DF1 request control {control:#04x}"),
            }
        }
        assert!(body.len() < 16);
        body.push(byte);
    }
    let [4, 0, 0x0f, 0, low, high, 0xa2, fields_seen @ ..] = body.as_slice() else {
        panic!("only protected typed logical reads are allowed: {body:02x?}");
    };
    assert_eq!(fields_seen, fields);
    let wire = frame(checksum, &body);
    let trailer = match checksum {
        Checksum::Crc => 2,
        Checksum::Bcc => 1,
    };
    expect(master, stop, &wire[wire.len() - trailer..]);
    ([*low, *high], wire)
}
fn send(master: &mut File, bytes: &[u8]) {
    for &byte in bytes {
        master.write_all(&[byte]).unwrap();
        thread::sleep(Duration::from_micros(100));
    }
}
fn response(checksum: Checksum, tns: [u8; 2], data: &[u8]) -> Vec<u8> {
    let [low, high] = tns;
    let mut body = vec![0, 4, 0x4f, 0, low, high];
    body.extend_from_slice(data);
    frame(checksum, &body)
}
fn source(device: &str, signals: &Value) -> Value {
    let baud: Value = serde_json::from_str("1.92e4").unwrap();
    let ack: Value = serde_json::from_str("9e1").unwrap();
    json!({"id":"plc","protocol":"df1","device":device,"destination_node":4,
        "baud":baud,"ack_timeout_ms":ack,"max_read_bytes":16,"every_ms":3_600_000,
        "timeout_ms":800,"signals":signals})
}
fn record(home: &Home, source: &Value) -> super::support::cli::Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1s",
        "-o",
        "run.jsonl",
    ])
}
fn rows(home: &Home, path: &str) -> Vec<Value> {
    fs::read_to_string(home.path(path))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}
fn samples(home: &Home) -> impl Iterator<Item = Value> {
    rows(home, "run.jsonl")
        .into_iter()
        .filter(|row| row["kind"] == "sample")
}

#[test]
fn df1_crc_reads_typed_arrays_and_recovers_full_duplex_link_controls() {
    let home = Home::new("df1-crc");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        let checksum = Checksum::Crc;
        let (first, _) = command(&mut master, &stop, checksum, &[16, 7, 0x89, 0, 0]);
        // Vendor's full 16-byte example data, with the CLI's current TNS.
        let meter = response(
            checksum,
            first,
            &[0xd7, 9, 0, 0, 0xe8, 0xe0, 0xf5, 5, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        // Lost request ACK. The full application reply arrives first.
        send(&mut master, &meter);
        expect(&mut master, &stop, &ACK);
        expect(&mut master, &stop, &ENQ);
        send(&mut master, &ACK);

        let (long, original) = command(&mut master, &stop, checksum, &[16, 19, 0x91, 254, 0]);
        assert_eq!(
            u16::from_le_bytes(long),
            u16::from_le_bytes(first).wrapping_add(1)
        );
        send(&mut master, &meter); // Lost reply ACK: delayed duplicate.
        expect(&mut master, &stop, &ACK);
        send(&mut master, &NAK);
        expect(&mut master, &stop, &original);

        let mut reply = response(
            checksum,
            long,
            &[
                0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0x10, 0x10, 0, 0, 0, 0, 0, 0x80,
            ],
        );
        reply.splice(4..4, ACK); // Embedded link response between body symbols.
        let mut corrupt = reply.clone();
        *corrupt.last_mut().unwrap() ^= 0x40;
        send(&mut master, &corrupt);
        expect(&mut master, &stop, &NAK);
        send(&mut master, &ENQ);
        expect(&mut master, &stop, &NAK);
        // Resent reply after NAK, with the original transaction.
        send(&mut master, &reply);
        expect(&mut master, &stop, &ACK);

        let (next, _) = command(&mut master, &stop, checksum, &[4, 19, 0x91, 0xff, 2, 1, 0]);
        send(&mut master, &ENQ);
        expect(&mut master, &stop, &ACK); // Last valid frame's response.
        // A corrupted start invalidates the saved ACK before the peer asks.
        send(&mut master, &[2]);
        send(&mut master, &ENQ);
        expect(&mut master, &stop, &NAK);
        send(&mut master, &ACK);
        let [low, high] = next;
        let peer_command = frame(
            checksum,
            &[0, 4, 0x0f, 0, low, high, 0xa2, 2, 7, 0x89, 0, 0],
        );
        send(&mut master, &peer_command);
        expect(&mut master, &stop, &ACK);
        send(&mut master, &response(checksum, next, &3_u32.to_le_bytes()));
        expect(&mut master, &stop, &ACK);

        let (float, _) = command(&mut master, &stop, checksum, &[4, 8, 0x8a, 1, 0]);
        send(&mut master, &ACK);
        send(
            &mut master,
            &response(checksum, float, &1.25_f32.to_le_bytes()),
        );
        expect(&mut master, &stop, &ACK);

        let (bit, _) = command(&mut master, &stop, checksum, &[2, 3, 0x85, 0, 0]);
        send(&mut master, &ACK);
        send(&mut master, &response(checksum, bit, &[0x10, 0x80]));
        expect(&mut master, &stop, &ACK);
        assert!(!receive(&mut master, &mut [0], &stop));
        5
    });
    let config = source(
        &device,
        &json!([
            {"name":"meter","address":"N7:0","elements":8,"data_type":"u16"},
            {"name":"long","address":"L19:254","elements":5,"data_type":"u32"},
            {"name":"float","address":"F8:1"},
            {"name":"bit","address":"B3:0/15"}
        ]),
    );
    let run = record(&home, &config);
    assert!(run.code == Some(0), "{}", run.stderr);
    assert_eq!(server.finish(), 5);
    let samples: Vec<_> = samples(&home).collect();
    assert_eq!(
        samples.iter().map(|row| &row["value"]).collect::<Vec<_>>(),
        [
            &json!([2519, 0, 57576, 1525, 0, 0, 0, 0]),
            &json!([4_294_967_295_u32, 0, 4112, 2_147_483_648_u32, 3]),
            &json!(1.25),
            &json!(true)
        ]
    );
    assert!(samples.iter().all(|row| row["elapsed_ns"].is_u64()
        && row["elapsed_ns"] == samples[0]["elapsed_ns"]
        && row.get("source_time").is_none()));
    let csv = home.run(&["--json", "data", "export", "run.jsonl", "-o", "out.csv"]);
    assert!(csv.code == Some(0), "{}", csv.stderr);
    assert!(
        fs::read_to_string(home.path("out.csv"))
            .unwrap()
            .contains("4294967295")
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
    let expected = samples
        .into_iter()
        .map(|mut sample| {
            sample.as_object_mut().unwrap().remove("kind");
            sample
        })
        .collect::<Vec<_>>();
    assert_eq!(rows(&home, "out.jsonl"), expected);
}

#[test]
fn df1_bcc_accepts_raw_dle_checksum_at_maximum_typed_reply_size() {
    let home = Home::new("df1-bcc");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        let checksum = Checksum::Bcc;
        let (tns, _) = command(
            &mut master,
            &stop,
            checksum,
            &[224, 0xff, 0xff, 0xff, 0x89, 0, 0],
        );
        send(&mut master, &ACK);
        let mut data = [0x10; 224];
        let initial = response(checksum, tns, &data);
        data[223] = data[223].wrapping_add(initial.last().unwrap().wrapping_sub(0x10));
        let reply = response(checksum, tns, &data);
        let (trailer, body) = reply.split_last().unwrap();
        assert_eq!(*trailer, 0x10);
        master.write_all(body).unwrap();
        send(&mut master, &[*trailer]);
        expect(&mut master, &stop, &ACK);
        assert!(!receive(&mut master, &mut [0], &stop));
        u64::from(u16::from_le_bytes([data[222], data[223]]))
    });
    let mut config = source(
        &device,
        &json!([{"name":"maximum","address":"N65535:0","elements":112,"data_type":"u16"}]),
    );
    config["checksum"] = json!("bcc");
    config["max_read_bytes"] = json!(224);
    let run = record(&home, &config);
    assert!(run.code == Some(0), "{}", run.stderr);
    let final_word = server.finish();
    let samples: Vec<_> = samples(&home).collect();
    assert_eq!(samples.len(), 1);
    let expected: Vec<_> = std::iter::repeat_n(4112, 111).chain([final_word]).collect();
    assert_eq!(samples[0]["value"], json!(expected));
}

#[test]
fn df1_rejects_corrupt_or_mismatched_replies_without_publishing_a_sibling() {
    for (mode, exit, code, cause) in [
        ("status", 1, "failed", "target rejected typed file read"),
        ("command", 1, "incomplete", "invalid PCCC response command"),
        ("width", 1, "incomplete", "data length differs"),
        ("route", 4, "timeout", "poll deadline expired"),
        ("tns", 4, "timeout", "poll deadline expired"),
        (
            "oversized",
            1,
            "incomplete",
            "invalid response frame retry limit",
        ),
    ] {
        let home = Home::new("df1-invalid");
        let (mut master, slave, device) = serial_pair();
        let server = Device::start(move |stop| {
            let _slave = slave;
            let checksum = Checksum::Crc;
            let (first, _) = command(&mut master, &stop, checksum, &[2, 7, 0x89, 0, 0]);
            send(&mut master, &ACK);
            send(&mut master, &response(checksum, first, &[11, 0]));
            expect(&mut master, &stop, &ACK);

            let ([low, high], _) = command(&mut master, &stop, checksum, &[2, 7, 0x89, 1, 0]);
            send(&mut master, &ACK);
            let mut body = vec![0, 4, 0x4f, 0, low, high, 12, 0];
            match mode {
                "status" => {
                    body[3] = 0xf0;
                    body.truncate(7);
                }
                "command" => body[2] = 0x41,
                "width" => body.push(0),
                "route" => body[0] = 2,
                "tns" => body[4] ^= 0x80,
                "oversized" => body.resize(231, 0),
                _ => unreachable!(),
            }
            let reply = frame(checksum, &body);
            // The CLI may exit immediately after writing the final link
            // control. Check those queued bytes even if finish signals stop.
            let required = AtomicBool::new(false);
            if mode == "oversized" {
                for _ in 0..4 {
                    master.write_all(&reply).unwrap();
                    expect(&mut master, &required, &NAK);
                }
            } else {
                send(&mut master, &reply);
                expect(&mut master, &required, &ACK);
            }
            assert!(!receive(&mut master, &mut [0], &stop));
            2
        });
        let mut config = source(
            &device,
            &json!([{"name":"first","address":"N7:0"},{"name":"second","address":"N7:1"}]),
        );
        config["timeout_ms"] = json!(400);
        let run = record(&home, &config);
        let error = run.failure(exit);
        assert_eq!(server.finish(), 2);
        assert_eq!(error["code"], code, "{mode}: {error}");
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{mode}: {error}"
        );
        if mode == "status" {
            assert_eq!(error["details"]["pccc_status"], 0xf0);
            assert_eq!(error["details"]["extended_status"], 12);
        }
        assert_unpublished(&home);
    }
}

#[test]
fn df1_ack_recovery_and_all_signal_reads_share_one_poll_deadline() {
    let home = Home::new("df1-deadline");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        let checksum = Checksum::Crc;
        let (first, _) = command(&mut master, &stop, checksum, &[2, 7, 0x89, 0, 0]);
        send(&mut master, &ACK);
        thread::sleep(Duration::from_millis(350));
        send(&mut master, &response(checksum, first, &[11, 0]));
        expect(&mut master, &stop, &ACK);
        let _ = command(&mut master, &stop, checksum, &[2, 7, 0x89, 1, 0]);
        expect(&mut master, &stop, &ENQ);
        // Leave every inquiry unanswered. There is only time for one.
        assert!(!receive(&mut master, &mut [0], &stop));
        2
    });
    let mut config = source(
        &device,
        &json!([{"name":"first","address":"N7:0"},{"name":"second","address":"N7:1"}]),
    );
    config["timeout_ms"] = json!(650);
    config["ack_timeout_ms"] = json!(200);
    let run = record(&home, &config);
    assert_eq!(server.finish(), 2);
    assert_eq!(run.failure(4)["code"], "timeout");
    assert_unpublished(&home);
}

fn tc_no_extra(master: &mut File, stop: &AtomicBool) {
    assert!(!receive(master, &mut [0], stop));
    // The CLI process has returned before finish raises stop. Check any queued
    // bytes directly too; the fixture still owns a live slave descriptor.
    match master.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        result => panic!("unexpected queued DF1 output: {result:?}"),
    }
}

#[test]
fn df1_whole_timer_counter_words_keep_native_flags_counts_and_scalar_shapes() {
    for checksum in [Checksum::Crc, Checksum::Bcc] {
        let home = Home::new("df1-tc");
        let (mut master, slave, device) = serial_pair();
        let server = Device::start(move |stop| {
            let _slave = slave;
            for (fields, data) in [
                (vec![6, 4, 0x86, 0, 0], vec![5, 0xc0, 100, 0, 17, 0]),
                (
                    vec![12, 5, 0x87, 7, 0],
                    vec![1, 0xa4, 0, 0x80, 0xff, 0x7f, 2, 0x58, 0xff, 0xff, 0, 0],
                ),
                (vec![6, 5, 0x87, 9, 0], vec![0; 6]),
                (vec![2, 7, 0x89, 0, 0], vec![0xff, 0xff]),
            ] {
                let (tns, _) = command(&mut master, &stop, checksum, &fields);
                send(&mut master, &ACK);
                send(&mut master, &response(checksum, tns, &data));
                let required = AtomicBool::new(false);
                expect(&mut master, &required, &ACK);
            }
            tc_no_extra(&mut master, &stop);
            4
        });
        let mut cfg = source(
            &device,
            &json!([
                {"name":"timer","address":"T4:0","data_type":"timer"},
                {"name":"counter","address":"C5:7","elements":3},
                {"name":"old","address":"N7:0"}
            ]),
        );
        cfg["max_read_bytes"] = 12.into();
        cfg["checksum"] = json!(match checksum {
            Checksum::Crc => "crc",
            Checksum::Bcc => "bcc",
        });
        record(&home, &cfg).success();
        assert_eq!(server.finish(), 4);
        let rows: Vec<_> = samples(&home).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0]["value"],
            json!({"kind":"pccc_timer","control_word_raw":49_157,"preset":100,"accumulator":17,
            "native_words_raw":[49_157,100,17],"flags":{"en":true,"tt":true,"dn":false}})
        );
        assert_eq!(
            rows[1]["value"],
            json!([
                {"kind":"pccc_counter","control_word_raw":41_985,"preset":-32_768,"accumulator":32_767,
                 "native_words_raw":[41_985,32_768,32_767],"flags":{"cu":true,"cd":false,"dn":true,"ov":false,"un":false}},
                {"kind":"pccc_counter","control_word_raw":22_530,"preset":-1,"accumulator":0,
                 "native_words_raw":[22_530,65_535,0],"flags":{"cu":false,"cd":true,"dn":false,"ov":true,"un":true}},
                {"kind":"pccc_counter","control_word_raw":0,"preset":0,"accumulator":0,
                 "native_words_raw":[0,0,0],"flags":{"cu":false,"cd":false,"dn":false,"ov":false,"un":false}}
            ])
        );
        assert_eq!(rows[2]["value"], -1);
        assert!(rows.iter().all(|row| row["elapsed_ns"].is_u64()
            && row["elapsed_ns"] == rows[0]["elapsed_ns"]
            && row.get("source_time").is_none()));
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--format",
            "jsonl",
            "-o",
            "out.jsonl",
        ]);
        let expected: Vec<_> = rows
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row.as_object_mut().unwrap().remove("kind");
                row
            })
            .collect();
        let exported: Vec<Value> = fs::read_to_string(home.path("out.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(exported, expected);
        home.ok(&["data", "export", "run.jsonl", "-o", "out.csv"]);
        let text = fs::read_to_string(home.path("out.csv")).unwrap();
        let actual: Vec<Value> = text
            .lines()
            .skip(1)
            .map(|line| {
                let field = line.splitn(9, ',').nth(8).unwrap();
                serde_json::from_str(
                    &field
                        .strip_prefix('"')
                        .unwrap()
                        .strip_suffix('"')
                        .unwrap()
                        .replace("\"\"", "\""),
                )
                .unwrap()
            })
            .collect();
        assert_eq!(
            actual,
            rows.iter()
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn df1_partial_counter_sibling_is_atomic_and_closes_the_link() {
    let home = Home::new("df1-tc-invalid");
    let (mut master, slave, device) = serial_pair();
    let server = Device::start(move |stop| {
        let _slave = slave;
        let checksum = Checksum::Crc;
        let (tns, _) = command(&mut master, &stop, checksum, &[6, 4, 0x86, 0, 0]);
        send(&mut master, &ACK);
        send(&mut master, &response(checksum, tns, &[0; 6]));
        let required = AtomicBool::new(false);
        expect(&mut master, &required, &ACK);
        let (tns, _) = command(&mut master, &stop, checksum, &[6, 5, 0x87, 0, 0]);
        send(&mut master, &ACK);
        send(&mut master, &response(checksum, tns, &[0; 5]));
        let required = AtomicBool::new(false);
        expect(&mut master, &required, &ACK);
        tc_no_extra(&mut master, &stop);
        2
    });
    let cfg = source(
        &device,
        &json!([{"name":"timer","address":"T4:0"},{"name":"counter","address":"C5:0"}]),
    );
    assert_eq!(record(&home, &cfg).failure(1)["code"], "incomplete");
    assert_eq!(server.finish(), 2);
    assert_eq!(samples(&home).collect::<Vec<_>>(), [] as [Value; 0]);
}

fn st_hex(text: &str) -> Vec<u8> {
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    assert_eq!(tail, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn st_literal(id: &str, encoding: Option<&str>) -> (Vec<u8>, Value) {
    let literals: Value =
        serde_json::from_str(include_str!("../fixtures/pccc/st-literals.json")).unwrap();
    let item = literals["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == id)
        .unwrap();
    let raw = item["native_hex_chunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chunk| st_hex(chunk.as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(raw.len(), 84);
    let mut value = json!({"kind":"pccc_string","length_word_raw":item["length_word_raw"],
        "capacity":82,"length_status":item["length_status"],
        "raw":{"encoding":"base64","scope":"native_element","data":STANDARD.encode(&raw)}});
    if let Some(active) = item.get("active_hex") {
        value["active"] = json!({"encoding":"base64","scope":"active_data",
            "data":STANDARD.encode(st_hex(active.as_str().unwrap()))});
        if let Some(encoding) = encoding {
            value["encoding"] = encoding.into();
            value["text"] = item[match encoding {
                "ascii" => "ascii_text",
                "utf8" => "utf8_text",
                _ => panic!("unknown test encoding"),
            }]
            .clone();
        }
    }
    (raw, value)
}

fn st_exports(home: &Home, samples: &[Value], incomplete: bool) {
    for (format, path) in [("jsonl", "st.jsonl"), ("csv", "st.csv")] {
        let mut args = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            format,
            "-o",
            path,
        ];
        if incomplete {
            args.push("--allow-incomplete");
        }
        home.ok(&args);
        let text = fs::read_to_string(home.path(path)).unwrap();
        if format == "jsonl" {
            let expected = samples
                .iter()
                .map(|sample| {
                    let mut sample = sample.clone();
                    sample.as_object_mut().unwrap().remove("kind");
                    sample
                })
                .collect::<Vec<_>>();
            let actual = text
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        } else {
            let actual = text
                .lines()
                .skip(1)
                .map(|line| {
                    let field = line.splitn(9, ',').nth(8).unwrap();
                    let text = field.strip_prefix('"').map_or_else(
                        || field.to_owned(),
                        |quoted| quoted.strip_suffix('"').unwrap().replace("\"\"", "\""),
                    );
                    serde_json::from_str::<Value>(&text).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                samples
                    .iter()
                    .map(|sample| sample["value"].clone())
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn df1_native_strings_keep_complete_raw_and_strict_active_bytes_for_both_checksums() {
    for (checksum, cap) in [(Checksum::Crc, 84_u16), (Checksum::Bcc, 168)] {
        let home = Home::new("df1-st");
        let (mut master, slave, device) = serial_pair();
        let (abc, a) = st_literal("odd_abc", Some("ascii"));
        let (nul, n) = st_literal("embedded_nul", Some("ascii"));
        let (one, o) = st_literal("odd_one", Some("ascii"));
        let (full, f) = st_literal("full_82", None);
        let (empty, e) = st_literal("empty_nonzero", Some("ascii"));
        let (invalid, i) = st_literal("invalid_83", Some("ascii"));
        let (last, l) = st_literal("invalid_65535", Some("utf8"));
        let (high, h) = st_literal("high_bytes", None);
        let expected = vec![
            json!([a, n, o]),
            f,
            e,
            i,
            l,
            h,
            json!(-1),
            json!({"kind":"pccc_timer","control_word_raw":49_157,"preset":100,"accumulator":17,
                "native_words_raw":[49_157,100,17],"flags":{"en":true,"tt":true,"dn":false}}),
        ];
        let server = Device::start(move |stop| {
            let _slave = slave;
            let mut cases = Vec::new();
            if cap == 84 {
                for (element, raw) in [abc, nul, one].into_iter().enumerate() {
                    cases.push((vec![84, 3, 0x8d, u8::try_from(element).unwrap(), 0], raw));
                }
            } else {
                cases.push((vec![168, 3, 0x8d, 0, 0], [abc, nul].concat()));
                cases.push((vec![84, 3, 0x8d, 2, 0], one));
            }
            cases.extend([
                (vec![84, 8, 0x8d, 7, 0], full),
                (vec![84, 9, 0x8d, 0, 0], empty),
                (vec![84, 9, 0x8d, 1, 0], invalid),
                (vec![84, 0xff, 0xff, 0, 0x8d, 0xff, 0xff, 0, 0], last),
                (vec![84, 8, 0x8d, 8, 0], high),
                (vec![2, 7, 0x89, 0, 0], vec![0xff, 0xff]),
                (vec![6, 4, 0x86, 0, 0], vec![5, 0xc0, 100, 0, 17, 0]),
            ]);
            for (fields, data) in &cases {
                let (tns, _) = command(&mut master, &stop, checksum, fields);
                send(&mut master, &ACK);
                send(&mut master, &response(checksum, tns, data));
                let required = AtomicBool::new(false);
                expect(&mut master, &required, &ACK);
            }
            tc_no_extra(&mut master, &stop);
            u64::try_from(cases.len()).unwrap()
        });
        let mut cfg = source(
            &device,
            &json!([
                {"name":"array","address":"st3:0","elements":3,"encoding":"ascii"},
                {"name":"full","address":"ST8:7","data_type":"string"},
                {"name":"empty","address":"ST9:0","encoding":"ascii"},
                {"name":"invalid","address":"ST9:1","encoding":"ascii"},
                {"name":"last","address":"ST255:255","encoding":"utf8"},
                {"name":"high","address":"ST8:8"},
                {"name":"old","address":"N7:0"},
                {"name":"timer","address":"T4:0"}
            ]),
        );
        cfg["max_read_bytes"] = cap.into();
        cfg["checksum"] = json!(match checksum {
            Checksum::Crc => "crc",
            Checksum::Bcc => "bcc",
        });
        record(&home, &cfg).success();
        assert_eq!(server.finish(), if cap == 84 { 10 } else { 9 });
        let rows = samples(&home).collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(rows.iter().all(|row| row["elapsed_ns"].is_u64()
            && row["elapsed_ns"] == rows[0]["elapsed_ns"]
            && row.get("source_time").is_none()));
        st_exports(&home, &rows, false);
    }
}

#[test]
fn df1_late_string_width_or_text_failure_retains_only_the_prior_complete_poll() {
    for mode in ["short", "ascii", "utf8"] {
        let home = Home::new("df1-st-prefix");
        let (mut master, slave, device) = serial_pair();
        let (abc, a) = st_literal("odd_abc", None);
        let (good, g) = st_literal(
            if mode == "utf8" {
                "utf8_six"
            } else {
                "odd_abc"
            },
            if mode == "short" { None } else { Some(mode) },
        );
        let (bad, _) = st_literal("high_bytes", None);
        let server = Device::start(move |stop| {
            let _slave = slave;
            let checksum = Checksum::Crc;
            for poll in 0..2 {
                for (element, data) in [
                    (0, abc.as_slice()),
                    (
                        1,
                        if poll == 0 {
                            &good
                        } else if mode == "short" {
                            &good[..83]
                        } else {
                            &bad
                        },
                    ),
                ] {
                    let (tns, _) =
                        command(&mut master, &stop, checksum, &[84, 9, 0x8d, element, 0]);
                    send(&mut master, &ACK);
                    send(&mut master, &response(checksum, tns, data));
                    let required = AtomicBool::new(false);
                    expect(&mut master, &required, &ACK);
                }
            }
            tc_no_extra(&mut master, &stop);
            4
        });
        let mut cfg = source(
            &device,
            &json!([
                {"name":"first","address":"ST9:0"},
                {"name":"second","address":"ST9:1"}
            ]),
        );
        cfg["max_read_bytes"] = 168.into();
        cfg["every_ms"] = 200.into();
        if mode != "short" {
            cfg["signals"][1]["encoding"] = mode.into();
        }
        assert_eq!(record(&home, &cfg).failure(1)["code"], "incomplete");
        assert_eq!(server.finish(), 4);
        let rows = samples(&home).collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>(),
            [a, g]
        );
        st_exports(&home, &rows, true);
    }
}

#[test]
fn df1_string_chunks_keep_one_deadline_and_small_caps_fail_before_serial_open() {
    let home = Home::new("df1-st-cutoff");
    let (mut master, slave, device) = serial_pair();
    let (raw, _) = st_literal("odd_abc", None);
    let server = Device::start(move |stop| {
        let _slave = slave;
        let checksum = Checksum::Crc;
        let (tns, _) = command(&mut master, &stop, checksum, &[84, 9, 0x8d, 0, 0]);
        send(&mut master, &ACK);
        thread::sleep(Duration::from_millis(350));
        send(&mut master, &response(checksum, tns, &raw));
        let required = AtomicBool::new(false);
        expect(&mut master, &required, &ACK);
        let _ = command(&mut master, &stop, checksum, &[84, 9, 0x8d, 1, 0]);
        expect(&mut master, &required, &ENQ);
        tc_no_extra(&mut master, &stop);
        2
    });
    let mut cfg = source(
        &device,
        &json!([{"name":"array","address":"ST9:0","elements":3}]),
    );
    cfg["max_read_bytes"] = 84.into();
    cfg["timeout_ms"] = 650.into();
    cfg["ack_timeout_ms"] = 200.into();
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    assert_eq!(server.finish(), 2);
    assert_eq!(samples(&home).collect::<Vec<_>>(), [] as [Value; 0]);

    let home = Home::new("df1-st-cap");
    let (mut master, _slave, device) = serial_pair();
    let mut cfg = source(&device, &json!([{"name":"s","address":"ST9:0"}]));
    cfg["max_read_bytes"] = 83.into();
    assert_eq!(record(&home, &cfg).failure(2)["code"], "usage");
    assert!(!home.path("run.jsonl").exists());
    assert_eq!(
        master.read(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
