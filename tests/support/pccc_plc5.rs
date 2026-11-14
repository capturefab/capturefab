//! Direct PLC-5/E typed reads through the CLI. Published 10-word transaction
//! from Rockwell EtherNet/IP Explicit Messaging Rev. 1.2 packets 5 and 6.

use super::support::cli::Home;
use super::{
    Packet, accept, cleanup, config, cpf, execute_pccc, listener, readings, receive, record,
    register, reply, send,
};
use serde_json::{Value, json};
use std::{fs, net::TcpStream, thread};

const PUBLISHED_ID: [u8; 7] = [7, 1, 0, 0xf2, 0x0c, 2, 0];
const PUBLISHED_COMMAND: &[u8] = b"\x68\0\0\x0a\0\0$N7:0\0\x0a\0";
const PUBLISHED_DATA: &[u8] = b"\x99\x09\x15\x42\x55\x55\xaa\xaa\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

fn request(stream: &mut TcpStream, command: &[u8]) -> Packet {
    let packet = receive(stream);
    let cip = execute_pccc(&packet, None);
    let (header, body) = cip.split_first_chunk::<17>().unwrap();
    assert_eq!(&header[6..13], PUBLISHED_ID);
    assert_eq!(&header[13..15], &[0x0f, 0]);
    assert_eq!(body, command, "only PLC-5 typed reads");
    packet
}
fn response(stream: &mut TcpStream, request: &Packet, status: u8, data: &[u8]) {
    let cip = execute_pccc(request, None);
    assert!(cip.len() >= 17);
    let mut packet = vec![0xcb, 0, 0, 0];
    packet.extend(PUBLISHED_ID);
    packet.extend([0x4f, status, cip[15], cip[16]]);
    packet.extend_from_slice(data);
    send(stream, &reply(request, &cpf(&packet), super::SESSION));
}

fn plc5_config(listener: &std::net::TcpListener, signals: Value) -> Value {
    let mut config = config(listener, signals);
    let source = &mut config["sources"][0];
    source["profile"] = json!("plc5");
    source["vendor_id"] = json!(1);
    source["serial_number"] = json!(0x0002_0cf2_u32);
    source["max_read_bytes"] = json!(20);
    config
}

#[test]
fn plc5e_replays_the_published_ten_words_and_reads_segmented_float_elements() {
    let home = Home::new("pccc-plc5");
    let listener = listener();
    let config = plc5_config(
        &listener,
        json!([
            {"name":"words","address":"N7:0","elements":10},
            {"name":"floats","address":"F8:1994","elements":6},
            {"name":"bit","address":"B3:1999/15"},
            {"name":"uint","address":"N999:1999","data_type":"u16"}
        ]),
    );
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        let published = request(&mut stream, PUBLISHED_COMMAND);
        response(&mut stream, &published, 0, PUBLISHED_DATA);

        // Both count fields are five elements, even for a 20-byte value block.
        let float_array = request(&mut stream, b"\x68\0\0\x05\0\0$F8:1994\0\x05\0");
        let mut data = vec![0x9a, 9, 22, 0, 0x94, 8]; // 20 value + 2 element-descriptor bytes
        for value in [1.25_f32, -2.5, 4.0, 8.0, -16.0] {
            data.extend(value.to_le_bytes());
        }
        response(&mut stream, &float_array, 0, &data);

        let float_tail = request(&mut stream, b"\x68\0\0\x01\0\0$F8:1999\0\x01\0");
        let mut data = vec![0x94, 8];
        data.extend(32.0_f32.to_le_bytes());
        response(&mut stream, &float_tail, 0, &data);

        let bit = request(&mut stream, b"\x68\0\0\x01\0\0$B3:1999\0\x01\0");
        response(&mut stream, &bit, 0, &[0x22, 0, 0x80]);

        let unsigned = request(&mut stream, b"\x68\0\0\x01\0\0$N999:1999\0\x01\0");
        // Two-byte extended type ID and two-byte extended size, including
        // allowed zero high bytes (RM516 7-36). An array can have one element.
        response(
            &mut stream,
            &unsigned,
            0,
            &[0xaa, 9, 0, 7, 0, 0xaa, 4, 0, 2, 0, 0xff, 0xff],
        );
        cleanup(&mut stream);
    });
    let run = record(&home, &config, "1s");
    assert!(run.code == Some(0), "{}", run.stderr);
    server.join().unwrap();
    let rows = readings(&home);
    assert_eq!(
        rows.iter().map(|row| &row["value"]).collect::<Vec<_>>(),
        [
            &json!([21845, -21846, 0, 0, 0, 0, 0, 0, 0, 0]),
            &json!([1.25, -2.5, 4.0, 8.0, -16.0, 32.0]),
            &json!(true),
            &json!(65535)
        ]
    );
    let csv = home.run(&[
        "--json",
        "data",
        "export",
        "run.jsonl",
        "--source",
        "plc",
        "-o",
        "out.csv",
    ]);
    assert!(csv.code == Some(0), "{}", csv.stderr);
    assert!(
        fs::read_to_string(home.path("out.csv"))
            .unwrap()
            .contains("-21846")
    );
    let jsonl = home.run(&[
        "--json",
        "data",
        "export",
        "run.jsonl",
        "--source",
        "plc",
        "--format",
        "jsonl",
        "-o",
        "out.jsonl",
    ]);
    assert!(jsonl.code == Some(0), "{}", jsonl.stderr);
    let expected = rows
        .into_iter()
        .map(|mut row| {
            row.as_object_mut().unwrap().remove("kind");
            row
        })
        .collect::<Vec<_>>();
    let actual = fs::read_to_string(home.path("out.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn plc5e_rejects_wrong_descriptor_or_payload_without_publishing_earlier_reads() {
    for (status, data, cause) in [
        (
            0,
            &[0x99, 9, 4, 0x42, 1, 0, 2, 0][..],
            "array descriptor length",
        ),
        (
            0,
            &[0x99, 9, 5, 0x44, 1, 0, 2, 0],
            "element descriptor differs",
        ),
        (
            0,
            &[0x99, 9, 5, 0x22, 1, 0, 2, 0],
            "element descriptor differs",
        ),
        (
            0,
            &[0x99, 9, 7, 0x95, 9, 0x42, 1, 0, 2, 0],
            "element descriptor differs",
        ),
        (0, &[0x99, 9, 3, 0x42, 1, 0], "data length differs"),
        (0, &[0x42, 1, 0, 2, 0], "missing its array descriptor"),
        (0, &[0xa9, 9], "truncated PLC-5 type/data descriptor"),
        (0, &[0x89, 9], "zero-length extension"),
        (0xf0, &[0x07], "target rejected typed file read"),
    ] {
        let home = Home::new("pccc-plc5-invalid");
        let listener = listener();
        let config = plc5_config(
            &listener,
            json!([
                {"name":"first","address":"N7:0"},
                {"name":"second","address":"N7:1","elements":2}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let first = request(&mut stream, b"\x68\0\0\x01\0\0$N7:0\0\x01\0");
            response(&mut stream, &first, 0, &[0x42, 0x34, 0x12]);
            let second = request(&mut stream, b"\x68\0\0\x02\0\0$N7:1\0\x02\0");
            response(&mut stream, &second, status, data);
            cleanup(&mut stream);
        });
        let run = record(&home, &config, "1s");
        let error = run.failure(1);
        server.join().unwrap();
        assert!(
            error["message"].as_str().unwrap().contains(cause),
            "{error}"
        );
        if status != 0 {
            assert_eq!(error["details"]["pccc_status"], 0xf0);
            assert_eq!(error["details"]["extended_status"], 0x07);
        }
        assert_eq!(readings(&home), Vec::<Value>::new());
    }
}

#[test]
fn plc5e_rejects_other_file_families_and_out_of_range_addresses_before_connecting() {
    for (address, elements, slot) in [
        ("L19:0", 1, None),
        ("N2:0", 1, None),
        ("N1000:0", 1, None),
        ("N7:1999", 2, None),
        ("N7:0", 1, Some(1)),
    ] {
        let home = Home::new("pccc-plc5-config");
        let listener = listener();
        listener.set_nonblocking(true).unwrap();
        let mut config = plc5_config(
            &listener,
            json!([{"name":"bad","address":address,"elements":elements}]),
        );
        if let Some(slot) = slot {
            config["sources"][0]["slot"] = json!(slot);
        }
        let run = record(&home, &config, "1s");
        assert_eq!(run.failure(2)["code"], "usage");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
