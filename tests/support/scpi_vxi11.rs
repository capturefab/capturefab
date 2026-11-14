//! Independent RPC/XDR fixtures from VXI-11 Appendix C and RFC5531 §11.
use super::{Home, accept, config, decoded, hash, listener, record, samples, time};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    thread,
};

const CORE: u32 = 0x0006_07af;
const LINK: u32 = 0xffff_fffe;

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn call(stream: &mut TcpStream, expected: u32) -> (u32, Vec<u8>) {
    let mut marker = [0; 4];
    stream.read_exact(&mut marker).unwrap();
    let marker = u32::from_be_bytes(marker);
    assert_ne!(marker & 0x8000_0000, 0);
    let length = usize::try_from(marker & 0x7fff_ffff).unwrap();
    assert!((40..=70_000).contains(&length));
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame).unwrap();
    assert_eq!(word(&frame, 4), 0);
    assert_eq!(word(&frame, 8), 2);
    assert_eq!(word(&frame, 12), CORE);
    assert_eq!(word(&frame, 16), 1);
    assert_eq!(word(&frame, 20), expected);
    assert_eq!(&frame[24..40], [0; 16]);
    (word(&frame, 0), frame[40..].to_vec())
}
fn response(stream: &mut TcpStream, xid: u32, body: &[u8]) {
    let mut frame = Vec::new();
    for word in [xid, 1, 0, 0, 0, 0] {
        frame.extend_from_slice(&word.to_be_bytes());
    }
    frame.extend_from_slice(body);
    // Fragment across XDR integer boundaries, not just TCP packet boundaries.
    for (part, last) in [(&frame[..13], false), (&frame[13..], true)] {
        let marker = u32::try_from(part.len()).unwrap() | if last { 0x8000_0000 } else { 0 };
        stream.write_all(&marker.to_be_bytes()).unwrap();
        stream.write_all(part).unwrap();
    }
}
fn words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_be_bytes()).collect()
}
fn open_link(stream: &mut TcpStream) {
    let (xid, body) = call(stream, 10);
    assert_eq!(
        body,
        words(&[0, 0, 0, 5])
            .into_iter()
            .chain(*b"inst0\0\0\0")
            .collect::<Vec<_>>()
    );
    response(stream, xid, &words(&[0, LINK, 4567, 1024]));
}
fn write_query(stream: &mut TcpStream, expected: &[u8]) -> u32 {
    let (xid, body) = call(stream, 11);
    assert_eq!(word(&body, 0), LINK);
    assert_eq!(word(&body, 8), 0);
    assert_eq!(word(&body, 12), 8);
    assert_eq!(usize::try_from(word(&body, 16)).unwrap(), expected.len());
    assert_eq!(&body[20..20 + expected.len()], expected);
    assert!(body[20 + expected.len()..].iter().all(|byte| *byte == 0));
    response(
        stream,
        xid,
        &words(&[0, u32::try_from(expected.len()).unwrap()]),
    );
    word(&body, 4)
}
fn read_request(stream: &mut TcpStream) -> (u32, u32, u32) {
    let (xid, body) = call(stream, 12);
    assert_eq!(body.len(), 24);
    assert_eq!(word(&body, 0), LINK);
    assert_eq!(&body[12..], [0; 12]);
    (xid, word(&body, 4), word(&body, 8))
}
fn read_response(stream: &mut TcpStream, xid: u32, reason: u32, data: &[u8]) {
    let mut body = words(&[0, reason, u32::try_from(data.len()).unwrap()]);
    body.extend_from_slice(data);
    body.resize(body.len() + (4 - data.len() % 4) % 4, 0);
    response(stream, xid, &body);
}
fn close_link(stream: &mut TcpStream) {
    let (xid, body) = call(stream, 23);
    assert_eq!(body, LINK.to_be_bytes());
    response(stream, xid, &[0; 4]);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}
fn vxi_config(listener: &std::net::TcpListener, signals: Value) -> Value {
    let mut config = config(listener, signals);
    config["sources"][0]["connection"] = json!({"transport":"vxi11","host":"127.0.0.1","core_port":listener.local_addr().unwrap().port()});
    config
}

#[test]
fn vxi11_end_boundaries_preserve_exact_values_binary_artifacts_and_exports() {
    let home = Home::new("scpi-vxi11-end");
    let listener = listener();
    let mut config = vxi_config(
        &listener,
        json!([
            {"name":"identity","query":"*IDN?","format":"text","response_terminator":"none"},
            {"name":"count","query":"FETC?"},
            {"name":"waveform","query":"CURVE?","format":"binary_block","response_terminator":"none","output":"wave-{seq}.scpi","max_bytes":3}
        ]),
    );
    config["sources"][0]["write_terminator"] = json!("none");
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        open_link(&mut stream);
        write_query(&mut stream, b"*IDN?");
        let (xid, requested, _) = read_request(&mut stream);
        assert!(requested >= 4);
        read_response(&mut stream, xid, 0, b"ACME");
        let (xid, requested, _) = read_request(&mut stream);
        assert!(requested >= 12);
        read_response(&mut stream, xid, 4, b",DMM,123,1.0");
        write_query(&mut stream, b"FETC?");
        let (xid, requested, _) = read_request(&mut stream);
        assert!(requested >= 21);
        read_response(&mut stream, xid, 0, b"18446744073709551615\n");
        // A terminated payload still needs the subsequent END confirmation.
        let (xid, requested, _) = read_request(&mut stream);
        assert_eq!(requested, 1);
        thread::sleep(time::millis(10));
        read_response(&mut stream, xid, 4, b"");
        write_query(&mut stream, b"CURVE?");
        let (xid, requested, _) = read_request(&mut stream);
        assert!(requested >= 6);
        read_response(&mut stream, xid, 4, b"#13abc");
        close_link(&mut stream);
    });
    assert_eq!(record(&home, &config).success()["samples"], 3);
    server.join().unwrap();
    let rows = samples(&home);
    assert_eq!(rows.len(), 3);
    let [identity, count, waveform] = rows.as_slice() else {
        panic!("missing replies")
    };
    assert_eq!(identity["value"]["value"], "ACME,DMM,123,1.0");
    assert_eq!(decoded(&identity["value"]["raw"]), b"ACME,DMM,123,1.0");
    assert_eq!(count["value"]["value"].to_string(), "18446744073709551615");
    let raw = &waveform["value"]["raw"];
    assert_eq!(raw["bytes"], 6);
    assert_eq!(raw["sha256"], hash(b"#13abc"));
    assert_eq!(fs::read(raw["path"].as_str().unwrap()).unwrap(), b"#13abc");
    assert_eq!(waveform["value"]["block"]["payload_offset"], 3);
    for format in ["csv", "jsonl"] {
        assert_eq!(
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "-o",
                format,
                "--format",
                format
            ])["samples"],
            3
        );
    }
    let exported = fs::read_to_string(home.path("jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(exported.len(), rows.len());
    for (original, exported) in rows.iter().zip(exported) {
        assert_eq!(original["value"], exported["value"]);
    }
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    assert!(csv.contains("18446744073709551615"));
}

#[test]
fn vxi11_malformed_end_rpc_and_partial_blocks_abort_the_whole_poll() {
    for mode in ["extra", "xid", "partial", "oversize"] {
        let home = Home::new(&format!("scpi-vxi11-{mode}"));
        let listener = listener();
        let config = vxi_config(
            &listener,
            json!([
                {"name":"block","query":"CURVE?","format":"binary_block","response_terminator":"none","output":"artifact.scpi"},
                {"name":"sibling","query":"FETC?"}
            ]),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            open_link(&mut stream);
            write_query(&mut stream, b"CURVE?\n");
            let (xid, _, _) = read_request(&mut stream);
            match mode {
                "extra" => {
                    read_response(&mut stream, xid, 4, b"#13abc");
                    write_query(&mut stream, b"FETC?\n");
                    let (xid, _, _) = read_request(&mut stream);
                    read_response(&mut stream, xid, 4, b"42\njunk");
                }
                "xid" => read_response(&mut stream, xid + 1, 4, b"#13abc"),
                "partial" => read_response(&mut stream, xid, 4, b"#13ab"),
                "oversize" => stream.write_all(&0xffff_ffff_u32.to_be_bytes()).unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        record(&home, &config).failure(1);
        server.join().unwrap();
        assert_eq!(samples(&home), Vec::<Value>::new());
        assert!(!home.path("artifact.scpi").exists());
        assert!(!home.path("artifact.scpi.json").exists());
    }
}

#[test]
fn vxi11_link_write_and_all_read_chunks_share_one_timeout() {
    let home = Home::new("scpi-vxi11-deadline");
    let listener = listener();
    let mut config = vxi_config(
        &listener,
        json!([{"name":"value","query":"FETC?","response_terminator":"none"}]),
    );
    config["sources"][0]["timeout_ms"] = json!(time::ms(400));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        thread::sleep(time::millis(35));
        open_link(&mut stream);
        thread::sleep(time::millis(35));
        let write_timeout = write_query(&mut stream, b"FETC?\n");
        let (xid, _, first_timeout) = read_request(&mut stream);
        assert!(u64::from(first_timeout) + time::ms(20) < u64::from(write_timeout));
        thread::sleep(time::millis(35));
        read_response(&mut stream, xid, 0, b"12");
        let (_, _, second_timeout) = read_request(&mut stream);
        assert!(u64::from(second_timeout) + time::ms(20) < u64::from(first_timeout));
        // No response: the core read must expire within the original poll budget.
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let error = record(&home, &config).failure(4);
    assert_eq!(error["code"], "timeout");
    server.join().unwrap();
    assert_eq!(samples(&home), Vec::<Value>::new());
}
