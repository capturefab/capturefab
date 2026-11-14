//! Independent literal `HiSLIP` headers from IVI-6.1 Tables2/4/12/17/18/28/32.
//! scpi-sim 0.7.4 covers synchronous basic exchange; overlap/SRQ/error fixtures
//! here are specification fixtures, not claims about that stock server.
use super::{Home, accept, config, decoded, hash, listener, record, samples, time};
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
};

struct Message {
    kind: u8,
    control: u8,
    parameter: u32,
    payload: Vec<u8>,
}
fn message(kind: u8, control: u8, parameter: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"HS".to_vec();
    bytes.extend_from_slice(&[kind, control]);
    bytes.extend_from_slice(&parameter.to_be_bytes());
    bytes.extend_from_slice(&u64::try_from(payload.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}
fn receive(stream: &mut TcpStream) -> Message {
    let mut header = [0; 16];
    stream.read_exact(&mut header).unwrap();
    let [h, s, kind, control, a, b, c, d, e, f, g, i, j, k, l, m] = header;
    assert_eq!([h, s], *b"HS");
    let parameter = u32::from_be_bytes([a, b, c, d]);
    let length = usize::try_from(u64::from_be_bytes([e, f, g, i, j, k, l, m])).unwrap();
    assert!(length <= 65_536);
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).unwrap();
    Message {
        kind,
        control,
        parameter,
        payload,
    }
}
fn send(stream: &mut TcpStream, kind: u8, parameter: u32, payload: &[u8]) {
    let bytes = message(kind, 0, parameter, payload);
    // The native reader must assemble partial fixed headers as well as payloads.
    let (first, rest) = bytes.split_at(5);
    stream.write_all(first).unwrap();
    stream.write_all(rest).unwrap();
}
fn initialize(listener: &TcpListener, overlap: bool, maximum: u64) -> (TcpStream, TcpStream) {
    let mut sync = accept(listener);
    let init = receive(&mut sync);
    assert_eq!(
        (init.kind, init.control, init.parameter),
        (0, 0, 0x0100_7878)
    );
    assert_eq!(init.payload, b"hislip0");
    sync.write_all(&message(1, u8::from(overlap), 0x0100_1234, &[]))
        .unwrap();
    let mut asynchronous = accept(listener);
    let init = receive(&mut asynchronous);
    assert_eq!((init.kind, init.control, init.parameter), (17, 0, 0x1234));
    assert_eq!(init.payload, Vec::<u8>::new());
    send(&mut asynchronous, 18, 0x5353, &[]);
    let maximum_request = receive(&mut asynchronous);
    assert_eq!(
        (
            maximum_request.kind,
            maximum_request.control,
            maximum_request.parameter
        ),
        (15, 0, 0)
    );
    assert_eq!(maximum_request.payload, 65_552_u64.to_be_bytes());
    // An unsolicited SRQ is not an AsyncMaxMsgSizeResponse.
    asynchronous.write_all(&message(20, 0x44, 0, &[])).unwrap();
    send(&mut asynchronous, 16, 0, &maximum.to_be_bytes());
    (sync, asynchronous)
}
fn query(stream: &mut TcpStream, bytes: &[u8], next: &mut u32, rmt: bool, maximum: usize) -> u32 {
    let last = bytes.len().div_ceil(maximum) - 1;
    let mut final_id = 0;
    for (index, chunk) in bytes.chunks(maximum).enumerate() {
        let read = receive(stream);
        assert_eq!(read.kind, if index == last { 7 } else { 6 });
        assert_eq!(read.control, u8::from(rmt && index == 0));
        assert_eq!(read.parameter, *next);
        assert_eq!(read.payload, chunk);
        final_id = *next;
        *next = next.wrapping_add(2);
    }
    final_id
}
fn closed(sync: &mut TcpStream, asynchronous: &mut TcpStream) {
    for stream in [sync, asynchronous] {
        // Retiring after an error header may discard unread diagnostic bytes,
        // so either orderly EOF or TCP reset is a valid closed session.
        match stream.read(&mut [0]) {
            Ok(count) => assert_eq!(count, 0, "unexpected client control operation"),
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::NotConnected
                ),
                "paired channel did not close: {error}"
            ),
        }
    }
}
fn hislip_config(listener: &TcpListener, signals: Value) -> Value {
    let mut result = config(listener, signals);
    result["sources"][0]["connection"] =
        json!({"transport":"hislip","address":listener.local_addr().unwrap().to_string()});
    result["sources"][0]["write_terminator"] = json!("none");
    result
}

#[test]
fn hislip_synchronized_ids_rmt_srq_end_and_exact_artifacts_survive_exports() {
    let home = Home::new("scpi-hislip-sync");
    let listener = listener();
    let config = hislip_config(
        &listener,
        json!([
            {"name":"identity","query":"*IDN?","format":"text","response_terminator":"none"},
            {"name":"count","query":"FETC?"},
            {"name":"wave","query":"CURVE?","format":"binary_block","response_terminator":"none","output":"wave.scpi","max_bytes":3}
        ]),
    );
    let server = thread::spawn(move || {
        let (mut sync, mut asynchronous) = initialize(&listener, false, 19);
        let mut next = 0xffff_ff00;
        let id = query(&mut sync, b"*IDN?", &mut next, false, 3);
        assert_eq!(id, 0xffff_ff02);
        asynchronous.write_all(&message(20, 0x54, 0, &[])).unwrap();
        send(&mut sync, 6, u32::MAX, b"ACME,");
        send(&mut sync, 7, id, b"DMM,123,1.0");
        let id = query(&mut sync, b"FETC?", &mut next, true, 3);
        send(&mut sync, 6, id, b"18446744073709551615\n");
        thread::sleep(time::millis(10));
        send(&mut sync, 7, id, &[]);
        let id = query(&mut sync, b"CURVE?", &mut next, true, 3);
        send(&mut sync, 7, id, b"#13abc");
        closed(&mut sync, &mut asynchronous);
    });
    assert_eq!(record(&home, &config).success()["samples"], 3);
    server.join().unwrap();
    let rows = samples(&home);
    let [identity, count, wave] = rows.as_slice() else {
        panic!("missing exact replies")
    };
    assert_eq!(identity["value"]["value"], "ACME,DMM,123,1.0");
    assert_eq!(decoded(&identity["value"]["raw"]), b"ACME,DMM,123,1.0");
    assert_eq!(count["value"]["value"].to_string(), "18446744073709551615");
    assert_eq!(fs::read(home.path("wave.scpi")).unwrap(), b"#13abc");
    assert_eq!(wave["value"]["raw"]["sha256"], hash(b"#13abc"));
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
    assert!(
        fs::read_to_string(home.path("csv"))
            .unwrap()
            .contains("18446744073709551615")
    );
}

#[test]
fn hislip_overlap_uses_independent_response_ids_and_rmt() {
    let home = Home::new("scpi-hislip-overlap");
    let listener = listener();
    let config = hislip_config(
        &listener,
        json!([
            {"name":"a","query":"FETC?","response_terminator":"none"},
            {"name":"b","query":"FETC?","response_terminator":"none"}
        ]),
    );
    let server = thread::spawn(move || {
        let (mut sync, mut asynchronous) = initialize(&listener, true, 19);
        let mut next = 0xffff_ff00;
        assert_eq!(query(&mut sync, b"FETC?", &mut next, false, 3), 0xffff_ff02);
        send(&mut sync, 6, 0xffff_ff00, b"12");
        send(&mut sync, 7, 0xffff_ff02, b"3");
        assert_eq!(query(&mut sync, b"FETC?", &mut next, true, 3), 0xffff_ff06);
        send(&mut sync, 7, 0xffff_ff04, b"456");
        closed(&mut sync, &mut asynchronous);
    });
    assert_eq!(record(&home, &config).success()["samples"], 2);
    server.join().unwrap();
    let rows = samples(&home);
    let [a, b] = rows.as_slice() else {
        panic!("missing overlap replies")
    };
    assert_eq!(a["value"]["value"], 123);
    assert_eq!(b["value"]["value"], 456);
}

#[test]
fn hislip_protocol_errors_and_async_interruptions_abort_the_whole_staged_poll() {
    for mode in [
        "extra",
        "wrong_id",
        "wildcard_end",
        "oversize",
        "control",
        "async_error",
        "async_fatal",
        "async_fatal_header",
        "async_data",
        "async_srq_payload",
        "async_first",
        "sync_first",
        "truncated",
    ] {
        let home = Home::new(&format!("scpi-hislip-{mode}"));
        let listener = listener();
        let config = hislip_config(
            &listener,
            json!([
                {"name":"block","query":"CURVE?","format":"binary_block","response_terminator":"none","output":"artifact.scpi"},
                {"name":"sibling","query":"FETC?"}
            ]),
        );
        let server = thread::spawn(move || {
            let (mut sync, mut asynchronous) = initialize(&listener, false, 65_552);
            let mut next = 0xffff_ff00;
            let id = query(&mut sync, b"CURVE?", &mut next, false, 65_536);
            send(&mut sync, 7, id, b"#13abc");
            let id = query(&mut sync, b"FETC?", &mut next, true, 65_536);
            match mode {
                "extra" => send(&mut sync, 7, id, b"42\njunk"),
                "wrong_id" => send(&mut sync, 7, id.wrapping_add(2), b"42\n"),
                "wildcard_end" => send(&mut sync, 7, u32::MAX, b"42\n"),
                "oversize" => {
                    let mut bytes = message(7, 0, id, &[]);
                    bytes[8..16].copy_from_slice(&65_537_u64.to_be_bytes());
                    sync.write_all(&bytes).unwrap();
                }
                "control" => sync.write_all(&message(7, 1, id, b"42\n")).unwrap(),
                "async_fatal_header" => {
                    let mut header = message(2, 1, 0, b"diagnostic never arrives");
                    header.truncate(16);
                    asynchronous.write_all(&header).unwrap();
                }
                "async_data" => send(&mut asynchronous, 6, id, b"42"),
                "async_srq_payload" => send(&mut asynchronous, 20, 0, b"x"),
                "async_error" | "async_fatal" => {
                    // The sync side stays open without data. Only async handling
                    // can return operation_failed rather than the poll timeout.
                    send(
                        &mut asynchronous,
                        if mode == "async_error" { 3 } else { 2 },
                        0,
                        b"read failed",
                    );
                }
                "async_first" => {
                    send(&mut asynchronous, 14, id, &[]);
                    let _ = sync.write_all(&message(13, 0, id, &[]));
                }
                "sync_first" => {
                    send(&mut sync, 13, id, &[]);
                    let _ = asynchronous.write_all(&message(14, 0, id, &[]));
                }
                "truncated" => {
                    send(&mut sync, 7, id, b"42");
                }
                _ => unreachable!(),
            }
            closed(&mut sync, &mut asynchronous);
        });
        record(&home, &config).failure(1);
        server.join().unwrap();
        assert_eq!(samples(&home), Vec::<Value>::new());
        assert!(!home.path("artifact.scpi").exists());
        assert!(!home.path("artifact.scpi.json").exists());
    }
}

#[test]
fn hislip_timeout_closes_both_channels_even_with_an_incomplete_async_header() {
    let home = Home::new("scpi-hislip-deadline");
    let listener = listener();
    let mut config = hislip_config(&listener, json!([{"name":"value","query":"FETC?"}]));
    config["sources"][0]["timeout_ms"] = json!(time::ms(250));
    let server = thread::spawn(move || {
        let (mut sync, mut asynchronous) = initialize(&listener, false, 65_552);
        let mut next = 0xffff_ff00;
        let id = query(&mut sync, b"FETC?", &mut next, false, 65_536);
        // A complete line is insufficient without END. Shutdown must also
        // wake the async thread blocked on the rest of this header.
        send(&mut sync, 6, id, b"42\n");
        asynchronous.write_all(b"HS\x14").unwrap();
        closed(&mut sync, &mut asynchronous);
    });
    let failure = record(&home, &config).failure(4);
    assert_eq!(failure["code"], "timeout");
    server.join().unwrap();
    assert_eq!(samples(&home), Vec::<Value>::new());
}

#[test]
fn hislip_negotiation_rejects_windows_without_payload_capacity() {
    for maximum in [0, 16] {
        let home = Home::new(&format!("scpi-hislip-window-{maximum}"));
        let listener = listener();
        let config = hislip_config(&listener, json!([{"name":"value","query":"FETC?"}]));
        let server = thread::spawn(move || {
            let (mut sync, mut asynchronous) = initialize(&listener, false, maximum);
            closed(&mut sync, &mut asynchronous);
        });
        record(&home, &config).failure(1);
        server.join().unwrap();
        assert_eq!(samples(&home), Vec::<Value>::new());
    }
}
