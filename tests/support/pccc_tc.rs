//! Independent A2 whole-element literals; transport wrappers reuse bounded peers.
use super::{
    accept, cleanup, config, listener, read_request, readings, record, register, response, send,
    support::cli::Home,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::TcpStream,
    thread,
    time::Duration,
};

pub(super) fn timer() -> Value {
    json!({"kind":"pccc_timer","control_word_raw":49_157,"preset":100,"accumulator":17,
        "native_words_raw":[49_157,100,17],"flags":{"en":true,"tt":true,"dn":false}})
}
pub(super) fn counter() -> Value {
    json!([
        {"kind":"pccc_counter","control_word_raw":41_985,"preset":-32_768,"accumulator":32_767,
         "native_words_raw":[41_985,32_768,32_767],"flags":{"cu":true,"cd":false,"dn":true,"ov":false,"un":false}},
        {"kind":"pccc_counter","control_word_raw":22_530,"preset":-1,"accumulator":0,
         "native_words_raw":[22_530,65_535,0],"flags":{"cu":false,"cd":true,"dn":false,"ov":true,"un":true}}
    ])
}
pub(super) fn exports(home: &Home, rows: &[Value]) {
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "--format",
        "jsonl",
        "-o",
        "out.jsonl",
    ]);
    let actual: Vec<Value> = fs::read_to_string(home.path("out.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "plc")
        .collect();
    let expected: Vec<_> = rows
        .iter()
        .map(|row| {
            let mut row = row.clone();
            row.as_object_mut().unwrap().remove("kind");
            row
        })
        .collect();
    assert_eq!(actual, expected);
    home.ok(&["data", "export", "run.jsonl", "-o", "out.csv"]);
    let csv = fs::read_to_string(home.path("out.csv")).unwrap();
    // The eight preceding literal metadata fields contain no embedded comma.
    let csv: Vec<Value> = csv
        .lines()
        .skip(1)
        .filter(|line| line.split(',').nth(1) == Some("\"plc\""))
        .map(|line| {
            let field = line.splitn(9, ',').nth(8).unwrap();
            let json = field
                .strip_prefix('"')
                .unwrap()
                .strip_suffix('"')
                .unwrap()
                .replace("\"\"", "\"");
            serde_json::from_str(&json).unwrap()
        })
        .collect();
    assert_eq!(
        csv,
        rows.iter()
            .map(|row| row["value"].clone())
            .collect::<Vec<_>>()
    );
}

fn retirement(stream: &mut TcpStream) {
    let mut header = [0; 24];
    let mut seen = 0;
    while seen < header.len() {
        match stream.read(&mut header[seen..]) {
            Ok(0) => return,
            Ok(count) => {
                seen += count;
                assert_eq!(header[0], 0x66, "extra application query");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return;
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert_eq!(&header[..4], &[0x66, 0, 0, 0]);
}

#[test]
fn native_timer_counter_direct_and_routed_chunks_and_exports_are_exact() {
    for (slot, cap) in [(None, 220), (Some(2), 224)] {
        let home = Home::new("pccc-tc-native");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([
                {"name":"timer","address":"T4:0","data_type":"timer"},
                {"name":"counter","address":"C5:7","elements":2},
                {"name":"array","address":"T9:0","elements":38},
                {"name":"last","address":"T255:255"},
                {"name":"numeric","address":"N7:0"}
            ]),
        );
        cfg["sources"][0]["max_read_bytes"] = cap.into();
        if let Some(slot) = slot {
            cfg["sources"][0]["slot"] = slot.into();
        }
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            for (fields, data) in [
                ((4, 0x86, 0, 6), vec![5, 0xc0, 100, 0, 17, 0]),
                (
                    (5, 0x87, 7, 12),
                    vec![1, 0xa4, 0, 0x80, 0xff, 0x7f, 2, 0x58, 0xff, 0xff, 0, 0],
                ),
            ] {
                let packet = read_request(&mut stream, slot, fields);
                send(&mut stream, &response(&packet, slot, &data));
            }
            let (first, tail) = if cap == 220 { (216, 12) } else { (222, 6) };
            let packet = read_request(&mut stream, slot, (9, 0x86, 0, first));
            send(&mut stream, &response(&packet, slot, &vec![0; first]));
            let packet = read_request(
                &mut stream,
                slot,
                (9, 0x86, u16::try_from(first / 6).unwrap(), tail),
            );
            send(&mut stream, &response(&packet, slot, &vec![0; tail]));
            let packet = read_request(&mut stream, slot, (255, 0x86, 255, 6));
            send(
                &mut stream,
                &response(&packet, slot, &[0, 0, 0, 0x80, 0, 0x80]),
            );
            let packet = read_request(&mut stream, slot, (7, 0x89, 0, 2));
            send(&mut stream, &response(&packet, slot, &[0xff, 0xff]));
            cleanup(&mut stream);
        });
        record(&home, &cfg, "300ms").success();
        worker.join().unwrap();
        let rows = readings(&home);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0]["value"], timer());
        assert_eq!(rows[1]["value"], counter());
        let zero = json!({"kind":"pccc_timer","control_word_raw":0,"preset":0,"accumulator":0,
            "native_words_raw":[0,0,0],"flags":{"en":false,"tt":false,"dn":false}});
        assert_eq!(rows[2]["value"], Value::Array(vec![zero; 38]));
        assert_eq!(
            rows[3]["value"],
            json!({"kind":"pccc_timer","control_word_raw":0,"preset":-32_768,"accumulator":-32_768,
            "native_words_raw":[0,32_768,32_768],"flags":{"en":false,"tt":false,"dn":false}})
        );
        assert_eq!(rows[4]["value"], -1);
        assert!(rows.iter().all(|row| row["elapsed_ns"].is_u64()
            && row["elapsed_ns"] == rows[0]["elapsed_ns"]
            && row.get("source_time").is_none()));
        exports(&home, &rows);
    }
}

#[test]
fn malformed_second_structured_poll_keeps_only_the_prior_complete_poll() {
    for fault in ["length", "transaction", "status"] {
        let home = Home::new("pccc-tc-prefix");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([
                {"name":"timer","address":"T4:0"},{"name":"counter","address":"C5:7","elements":2}
            ]),
        );
        cfg["sources"][0]["every_ms"] = 200.into();
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            for poll in 0..2 {
                let packet = read_request(&mut stream, None, (4, 0x86, 0, 6));
                send(
                    &mut stream,
                    &response(&packet, None, &[5, 0xc0, 100, 0, 17, 0]),
                );
                let packet = read_request(&mut stream, None, (5, 0x87, 7, 12));
                let mut data = vec![1, 0xa4, 0, 0x80, 0xff, 0x7f, 2, 0x58, 0xff, 0xff, 0, 0];
                if poll == 1 && fault == "length" {
                    data.pop();
                }
                let mut wire = response(&packet, None, &data);
                if poll == 1 {
                    // 24B encapsulation +16B CPF +4B CIP +7B requestor identity.
                    if fault == "transaction" {
                        wire[53] ^= 0x80;
                    }
                    if fault == "status" {
                        wire[52] = 0x10;
                    }
                }
                send(&mut stream, &wire);
            }
            cleanup(&mut stream);
            server.set_nonblocking(true).unwrap();
            assert_eq!(
                server.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        let run = record(&home, &cfg, "550ms");
        let error = run.failure(1);
        worker.join().unwrap();
        assert_eq!(
            error["code"],
            if fault == "status" {
                "failed"
            } else {
                "incomplete"
            }
        );
        let rows = readings(&home);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["value"], timer());
        assert_eq!(rows[1]["value"], counter());
        let end: Value = serde_json::from_str(
            fs::read_to_string(home.path("run.jsonl"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(end["status"], "failed");
    }
}

#[test]
fn structured_siblings_share_original_request_and_acquisition_deadlines() {
    for acquisition in [false, true] {
        let home = Home::new("pccc-tc-cutoff");
        let server = listener();
        let mut cfg = config(
            &server,
            json!([
                {"name":"first","address":"T4:0"},{"name":"second","address":"C5:0"},{"name":"never","address":"T4:1"}
            ]),
        );
        cfg["sources"][0]["timeout_ms"] = if acquisition { 1000.into() } else { 250.into() };
        let worker = thread::spawn(move || {
            let mut stream = accept(&server);
            register(&mut stream);
            let packet = read_request(&mut stream, None, (4, 0x86, 0, 6));
            thread::sleep(Duration::from_millis(160));
            send(&mut stream, &response(&packet, None, &[0; 6]));
            let packet = read_request(&mut stream, None, (5, 0x87, 0, 6));
            thread::sleep(Duration::from_millis(160));
            // A complete response after the original250ms cutoff is never a sample.
            match stream.write_all(&response(&packet, None, &[0; 6])) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(error) => panic!("{error}"),
            }
            // Existing cleanup is allowed; a new A2 request is not.
            retirement(&mut stream);
        });
        let run = record(&home, &cfg, if acquisition { "250ms" } else { "700ms" });
        assert_eq!(run.failure(4)["code"], "timeout");
        worker.join().unwrap();
        assert_eq!(readings(&home), [] as [Value; 0]);
    }
}
