//! Explicit Logix D3 reads. Wire/bit vectors are specification-derived;
//! no physical-controller or stock-simulator claim is made by these peers.
use super::{
    Home, accept, config, listener, path, query, read_request, record, register, reply, rows, send,
    unregister,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{fs, io::Read as _, net::TcpStream, thread, time::Duration};

const TWO_WORDS: [u8; 8] = [0x81, 0, 0, 0x80, 3, 0, 0, 1];

fn packed(words: &[u32], bits: &[bool], raw: &[u8]) -> Value {
    json!({"kind":"logix_bool_array","type_descriptor_raw":[211,0],
        "dword_count":words.len(),"bit_count":bits.len(),"bit_order":"lsb0",
        "bit_index_base":"requested_range","words":words,"bits":bits,
        "raw":{"encoding":"base64","bytes":raw.len(),"data":STANDARD.encode(raw)}})
}
fn literal() -> Value {
    packed(
        &[2_147_483_777, 16_777_219],
        &(0..64)
            .map(|index| [0, 7, 31, 32, 33, 56].contains(&index))
            .collect::<Vec<_>>(),
        &TWO_WORDS,
    )
}
fn zeros() -> Value {
    packed(&[0; 256], &[false; 8192], &[0; 1024])
}
fn no_more(stream: &mut TcpStream) {
    unregister(stream);
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}
fn values(home: &Home, filename: &str) -> Vec<Value> {
    fs::read_to_string(home.path(filename))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["source"] == "plc")
        .map(|row| row["value"].clone())
        .collect()
}
fn exports(home: &Home, expected: &[Value]) {
    assert_eq!(values(home, "run.jsonl"), expected);
    home.ok(&[
        "data",
        "export",
        "run.jsonl",
        "-o",
        "out.jsonl",
        "--format",
        "jsonl",
    ]);
    assert_eq!(values(home, "out.jsonl"), expected);
    home.ok(&["data", "export", "run.jsonl", "-o", "out.csv"]);
    let csv = fs::read_to_string(home.path("out.csv")).unwrap();
    let values = csv
        .lines()
        .skip(1)
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    let quoted = expected
        .iter()
        .map(|value| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    assert_eq!(
        values,
        quoted.iter().map(String::as_str).collect::<Vec<_>>()
    );
}
fn no_samples(home: &Home) {
    let rows = rows(home);
    assert_eq!(rows.iter().filter(|row| row["kind"] == "sample").count(), 0);
    assert_eq!(rows.last().unwrap()["status"], "failed");
}
fn transfer(
    stream: &mut TcpStream,
    slot: Option<u8>,
    name: &[u8],
    index: Option<u8>,
    count: u16,
    bytes: &[u8],
    chunk: usize,
) {
    for (number, data) in bytes.chunks(chunk).enumerate() {
        let offset = number * chunk;
        let packet = query(
            stream,
            slot,
            &read_request(
                &path(name, index),
                count,
                (number != 0).then_some(u32::try_from(offset).unwrap()),
            ),
        );
        let partial = offset + data.len() < bytes.len();
        send(
            stream,
            &packet,
            &reply(
                if number == 0 { 0xcc } else { 0xd2 },
                if partial { 6 } else { 0 },
                0xd3,
                data,
            ),
        )
        .unwrap();
    }
}

#[test]
fn explicit_dword_scalars_arrays_and_packed_bits_preserve_exact_values_and_exports() {
    for slot in [None, Some(3)] {
        let home = Home::new("logix-dword-positive");
        let listener = listener();
        let mut cfg = config(
            &listener,
            json!([
                {"name":"scalar","tag":"Word","data_type":"logix_dword"},
                {"name":"array","tag":"Flags[3]","data_type":"logix_dword","elements":2},
                {"name":"bits","tag":"Flags[3]","data_type":"logix_bool_array","elements":2},
                {"name":"large","tag":"Many[0]","data_type":"logix_bool_array","elements":256},
                {"name":"old","tag":"Exact64"}
            ]),
            slot,
        );
        cfg["sources"][0]["signals"][2]["elements"] = serde_json::from_str("2e0").unwrap();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            transfer(&mut stream, slot, b"Word", None, 1, &[0xff; 4], 4);
            transfer(&mut stream, slot, b"Flags", Some(3), 2, &TWO_WORDS, 8);
            // Split inside a DWORD, proving offsets count data bytes, not
            // DWORDs or BOOLs. Exact initial path ends in 28 03, count02 00.
            transfer(&mut stream, slot, b"Flags", Some(3), 2, &TWO_WORDS, 3);
            transfer(&mut stream, slot, b"Many", Some(0), 256, &[0; 1024], 333);
            let packet = query(
                &mut stream,
                slot,
                &read_request(&path(b"Exact64", None), 1, None),
            );
            send(
                &mut stream,
                &packet,
                &reply(0xcc, 0, 0xc9, &u64::MAX.to_le_bytes()),
            )
            .unwrap();
            no_more(&mut stream);
        });
        assert_eq!(record(&home, &cfg).success()["samples"], 5);
        server.join().unwrap();
        exports(
            &home,
            &[
                json!(u32::MAX),
                json!([2_147_483_777_u32, 16_777_219_u32]),
                literal(),
                zeros(),
                json!(u64::MAX),
            ],
        );
    }
}

#[test]
fn malformed_dword_fragments_roll_back_earlier_signals_without_fallback_or_later_queries() {
    for mode in [
        "dint",
        "high_type_byte",
        "short",
        "long",
        "empty",
        "empty_partial",
        "cap_partial",
        "changed",
        "status",
        "default_atomic",
    ] {
        let home = Home::new(&format!("logix-dword-{mode}"));
        let listener = listener();
        let cfg = config(
            &listener,
            json!([
                {"name":"old","tag":"Ready"},
                {"name":"bad","tag":"Flags[3]","elements":2,"data_type":if mode=="default_atomic" {"atomic"}else{"logix_bool_array"}},
                {"name":"future","tag":"Never","data_type":"logix_dword"}
            ]),
            Some(3),
        );
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            register(&mut stream);
            let packet = query(
                &mut stream,
                Some(3),
                &read_request(&path(b"Ready", None), 1, None),
            );
            send(
                &mut stream,
                &packet,
                &reply(0xcc, 0, 0xc4, &7_i32.to_le_bytes()),
            )
            .unwrap();
            let packet = query(
                &mut stream,
                Some(3),
                &read_request(&path(b"Flags", Some(3)), 2, None),
            );
            let (kind, status, data): (u16, u8, &[u8]) = match mode {
                "dint" => (0xc4, 0, &TWO_WORDS),
                "high_type_byte" => (0x01d3, 0, &TWO_WORDS),
                "short" => (0xd3, 0, &TWO_WORDS[..7]),
                "long" => (0xd3, 0, &[0; 9]),
                "empty" => (0xd3, 0, &[]),
                "empty_partial" => (0xd3, 6, &[]),
                "cap_partial" => (0xd3, 6, &TWO_WORDS),
                "changed" => (0xd3, 6, &TWO_WORDS[..3]),
                "status" => (0xd3, 5, &[]),
                _ => (0xd3, 0, &TWO_WORDS),
            };
            send(&mut stream, &packet, &reply(0xcc, status, kind, data)).unwrap();
            if mode == "changed" {
                let packet = query(
                    &mut stream,
                    Some(3),
                    &read_request(&path(b"Flags", Some(3)), 2, Some(3)),
                );
                send(&mut stream, &packet, &reply(0xd2, 0, 0xc4, &TWO_WORDS[3..])).unwrap();
            }
            no_more(&mut stream);
        });
        let code = match mode {
            "default_atomic" => 5,
            "status" => 3,
            _ => 1,
        };
        record(&home, &cfg).failure(code);
        server.join().unwrap();
        no_samples(&home);
    }
}

#[test]
fn packed_json_budget_stops_before_an_unaffordable_sibling_query() {
    let home = Home::new("logix-dword-json-budget");
    let listener = listener();
    let output = zeros();
    let per_read = serde_json::to_vec(&json!({"signal":"bits_000","value":output}))
        .unwrap()
        .len();
    let accepted = ((1 << 20) - 1) / (per_read + 1);
    assert_eq!(accepted, 20);
    let signals=(0..accepted+2).map(|index|json!({"name":format!("bits_{index:03}"),"tag":"Many[0]","data_type":"logix_bool_array","elements":256})).collect::<Vec<_>>();
    let cfg = config(&listener, json!(signals), None);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        for _ in 0..accepted {
            transfer(&mut stream, None, b"Many", Some(0), 256, &[0; 1024], 333);
        }
        no_more(&mut stream);
    });
    assert_eq!(record(&home, &cfg).failure(1)["code"], "incomplete");
    server.join().unwrap();
    no_samples(&home);
}

#[test]
fn explicit_dword_continuations_share_one_original_deadline() {
    let home = Home::new("logix-dword-deadline");
    let listener = listener();
    let mut cfg = config(
        &listener,
        json!([{"name":"bits","tag":"Flags[3]","data_type":"logix_bool_array","elements":2}]),
        None,
    );
    cfg["sources"][0]["timeout_ms"] = json!(250);
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        register(&mut stream);
        let packet = query(
            &mut stream,
            None,
            &read_request(&path(b"Flags", Some(3)), 2, None),
        );
        thread::sleep(Duration::from_millis(150));
        send(&mut stream, &packet, &reply(0xcc, 6, 0xd3, &TWO_WORDS[..3])).unwrap();
        let _packet = query(
            &mut stream,
            None,
            &read_request(&path(b"Flags", Some(3)), 2, Some(3)),
        );
        // The original 250ms cutoff has about100ms left; restarting it for
        // this fragment would keep the connection open beyond this150ms wait.
        stream
            .set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        no_more(&mut stream);
    });
    assert_eq!(record(&home, &cfg).failure(4)["code"], "timeout");
    server.join().unwrap();
    no_samples(&home);
}

#[test]
fn explicit_dword_modes_validate_profile_counts_and_options_before_connecting() {
    for (index, mut signal) in [
        json!({"elements":0}),
        json!({"elements":257}),
        json!({"elements":1.5}),
        json!({"capacity":1}),
        json!({"encoding":"ascii"}),
        json!({"max_bytes":4}),
        json!({"tag":"Bad..Path"}),
    ]
    .into_iter()
    .enumerate()
    {
        let home = Home::new(&format!("logix-dword-config-{index}"));
        let listener = listener();
        let object = signal.as_object_mut().unwrap();
        object.entry("name").or_insert(json!("bits"));
        object.entry("tag").or_insert(json!("Flags[3]"));
        object.insert("data_type".into(), json!("logix_bool_array"));
        let cfg = config(&listener, json!([signal]), None);
        record(&home, &cfg).failure(2);
        assert!(!home.path("run.jsonl").exists());
    }
    for profile in ["omron_nj_nx", "micro800"] {
        for data_type in ["logix_dword", "logix_bool_array"] {
            let home = Home::new("logix-dword-wrong-profile");
            let listener = listener();
            let mut cfg = config(
                &listener,
                json!([{"name":"bits","tag":"Flags[0]","data_type":data_type}]),
                None,
            );
            cfg["sources"][0]["profile"] = json!(profile);
            record(&home, &cfg).failure(2);
            assert!(!home.path("run.jsonl").exists());
        }
    }
}
