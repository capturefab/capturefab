//! Independent four-word timer and retentive-device CLI checks.
use super::support::cli::Home;
use super::{accept, config, listener, receive, record, request, response, rows, send, time};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
    thread,
};
const SHA: &str = "fdddd22d455065dbe98e1c443ab13b87e7ec69457919e77c4d42950d45b14442";
const ROUTE: [u8; 5] = [0, 255, 255, 3, 0];
fn literal() -> Value {
    let raw = include_str!("../fixtures/slmp/timer-literals.json");
    assert_eq!(format!("{:x}", Sha256::digest(raw.as_bytes())), SHA);
    serde_json::from_str(raw).unwrap()
}
fn hex(raw: &str) -> Vec<u8> {
    let (pairs, tail) = raw.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn configure(listener: &TcpListener, signals: &Value, ascii: bool, wide: bool) -> Value {
    let mut c = config(listener, signals);
    c["sources"].as_array_mut().unwrap().truncate(1);
    c["sources"][0]["encoding"] = json!(if ascii { "ascii" } else { "binary" });
    c["sources"][0]["device_format"] = json!(if wide { "iq_r" } else { "standard" });
    c
}
fn ascii_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0; 18];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(&header[..14], b"500000FF03FF00");
    let n =
        usize::from(u16::from_str_radix(std::str::from_utf8(&header[14..]).unwrap(), 16).unwrap());
    assert!((24..=28).contains(&n));
    let mut raw = header.to_vec();
    raw.resize(18 + n, 0);
    stream.read_exact(&mut raw[18..]).unwrap();
    assert_eq!(&raw[22..26], b"0401");
    raw
}
fn query(
    stream: &mut TcpStream,
    ascii: bool,
    wide: bool,
    device: (u8, &str, u32, u16, bool),
) -> Vec<u8> {
    let (code, name, address, count, bits) = device;
    if !ascii {
        return request(stream, wide, ROUTE, 4, (code, address, count, bits));
    }
    let raw = ascii_request(stream);
    let width = if wide { 4 } else { 2 };
    let digits = if wide { 8 } else { 6 };
    let body = format!(
        "00040401{:04X}{name:*<width$}{address:0digits$}{count:04X}",
        u16::from(bits) + if wide { 2 } else { 0 }
    );
    assert_eq!(
        raw,
        format!("500000FF03FF00{:04X}{body}", body.len()).as_bytes()
    );
    raw
}
fn reply(raw: &[u8], ascii: bool, status: u16, words: &[u16]) -> Vec<u8> {
    if !ascii {
        return response(
            raw,
            status,
            &words
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>(),
        );
    }
    let mut text = String::new();
    for word in words {
        write!(&mut text, "{word:04X}").unwrap();
    }
    format!("D00000FF03FF00{:04X}{status:04X}{text}", 4 + text.len()).into_bytes()
}
fn closed(stream: &mut TcpStream, listener: &TcpListener) {
    match stream.read(&mut [0]) {
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("unexpected next write/read/control: {other:?}"),
    }
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
fn expected(device: &str, words: [u16; 4]) -> Value {
    json!({"kind":"slmp_long_timer","device":device,"raw_words":words,"current_word_pair_raw":[words[0],words[1]],"coil":words[2]&1!=0,"contact":words[2]&2!=0})
}
fn samples(home: &Home, status: &str) -> Vec<Value> {
    let all = rows(home);
    assert_eq!(all.last().unwrap()["status"], status);
    let origin =
        chrono::DateTime::parse_from_rfc3339(all[0]["started_at"].as_str().unwrap()).unwrap();
    let result: Vec<_> = all.into_iter().filter(|r| r["kind"] == "sample").collect();
    let mut previous = 0;
    for (i, r) in result.iter().enumerate() {
        assert_eq!(r["seq"], i + 1);
        assert!(r.get("source_time").is_none());
        let ns = r["elapsed_ns"].as_u64().unwrap();
        assert!(ns >= previous);
        previous = ns;
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(r["time"].as_str().unwrap()).unwrap(),
            origin + chrono::Duration::nanoseconds(i64::try_from(ns).unwrap())
        );
        r["read_duration_ns"].as_u64().unwrap();
        r["delivery_latency_ns"].as_u64().unwrap();
    }
    result
}
fn exports(home: &Home, incomplete: bool) {
    for (format, path) in [("jsonl", "values.jsonl"), ("csv", "values.csv")] {
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
    }
    let original: Vec<_> = rows(home)
        .into_iter()
        .filter(|r| r["kind"] == "sample")
        .collect();
    let projection: Vec<_> = original
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r.as_object_mut().unwrap().remove("kind");
            r
        })
        .collect();
    let actual: Vec<Value> = fs::read_to_string(home.path("values.jsonl"))
        .unwrap()
        .lines()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();
    assert_eq!(actual, projection);
    let csv = fs::read_to_string(home.path("values.csv")).unwrap();
    assert_eq!(csv.lines().count(), original.len() + 1);
    for r in original {
        assert!(csv.contains(&format!(
            "\"{}\"",
            r["value"].to_string().replace('"', "\"\"")
        )));
    }
}
#[test]
fn hand_binary_ascii_long_records_keep_each_system_word_and_old_numeric_paths() {
    for ascii in [false, true] {
        let home = Home::new("slmp-long-native");
        let listener = listener();
        let c = configure(
            &listener,
            &json!([
 {"name":"two","device":"LTN","address":0,"data_type":"long_timer","elements":2},
 {"name":"retained","device":"LSTN","address":37,"data_type":"long_timer"},
 {"name":"old_word","device":"D","address":5},{"name":"old_timer","device":"TN","address":7}]),
            ascii,
            true,
        );
        let server = thread::spawn(move || {
            let mut s = accept(&listener);
            let l = literal();
            let read = if ascii {
                ascii_request(&mut s)
            } else {
                receive(&mut s)
            };
            let want = if ascii {
                l["ascii_request"].as_str().unwrap().as_bytes().to_vec()
            } else {
                hex(l["binary_request_hex"].as_str().unwrap())
            };
            assert_eq!(read, want);
            let reply = if ascii {
                l["ascii_reply"].as_str().unwrap().as_bytes().to_vec()
            } else {
                hex(l["binary_reply_hex"].as_str().unwrap())
            };
            send(&mut s, &reply);
            let r = query(&mut s, ascii, true, (0x5a, "LSTN", 37, 4, false));
            send(
                &mut s,
                &reply_words(&r, ascii, &[65_535, 32_768, 23_122, 291]),
            );
            for (code, name, address, value) in [(0xa8, "D", 5, 7), (0xc2, "TN", 7, 9)] {
                let r = query(&mut s, ascii, true, (code, name, address, 1, false));
                send(&mut s, &reply_words(&r, ascii, &[value]));
            }
            closed(&mut s, &listener);
        });
        assert_eq!(record(&home, &c).success()["status"], "complete");
        server.join().unwrap();
        let r = samples(&home, "complete");
        assert_eq!(r.len(), 4);
        assert_eq!(r[0]["value"], literal()["expected"]);
        assert_eq!(
            r[1]["value"],
            expected("LSTN", [65_535, 32_768, 23_122, 291])
        );
        assert_eq!(r[2]["value"], 7);
        assert_eq!(r[3]["value"], 9);
        exports(&home, false);
    }
}
fn reply_words(raw: &[u8], ascii: bool, words: &[u16]) -> Vec<u8> {
    reply(raw, ascii, 0, words)
}
#[test]
fn step_retentive_codes_and_ascii_aliases_keep_standard_and_extended_geometry() {
    for ascii in [false, true] {
        for wide in [false, true] {
            let home = Home::new("slmp-step-retentive");
            let listener = listener();
            let c = configure(
                &listener,
                &json!([
 {"name":"steps","device":"S","address":10,"data_type":"bool","elements":3},
 {"name":"contacts","device":if ascii&&!wide{"SS"}else{"STS"},"address":20,"data_type":"bool","elements":3},
 {"name":"coil","device":if ascii&&!wide{"SC"}else{"STC"},"address":30,"data_type":"bool"},
 {"name":"values","device":if ascii&&!wide{"SN"}else{"STN"},"address":40,"elements":2}]),
                ascii,
                wide,
            );
            let server = thread::spawn(move || {
                let mut s = accept(&listener);
                for (code, name, address, count) in [
                    (0x98, "S", 10, 3),
                    (0xc7, if wide { "STS" } else { "SS" }, 20, 3),
                    (0xc6, if wide { "STC" } else { "SC" }, 30, 1),
                ] {
                    let r = query(&mut s, ascii, wide, (code, name, address, count, true));
                    if ascii {
                        let body = if count == 1 { "1" } else { "101" };
                        send(
                            &mut s,
                            format!("D00000FF03FF00{:04X}0000{body}", 4 + body.len()).as_bytes(),
                        );
                    } else {
                        send(
                            &mut s,
                            &response(&r, 0, if count == 1 { &[0x10] } else { &[0x10, 0x10] }),
                        );
                    }
                }
                let r = query(
                    &mut s,
                    ascii,
                    wide,
                    (0xc8, if wide { "STN" } else { "SN" }, 40, 2, false),
                );
                send(&mut s, &reply_words(&r, ascii, &[65_535, 32_768]));
                closed(&mut s, &listener);
            });
            record(&home, &c).success();
            server.join().unwrap();
            let r = samples(&home, "complete");
            assert_eq!(r.len(), 4);
            assert_eq!(r[0]["value"], json!([true, false, true]));
            assert_eq!(r[1]["value"], json!([true, false, true]));
            assert_eq!(r[2]["value"], true);
            assert_eq!(r[3]["value"], json!([65_535, 32_768]));
            exports(&home, false);
        }
    }
}
#[test]
fn maximum_whole_timer_count_uses_960_words_and_240_logical_objects() {
    for ascii in [false, true] {
        let home = Home::new("slmp-long-max");
        let listener = listener();
        let c = configure(
            &listener,
            &json!([{"name":"max","device":"LTN","address":7,"data_type":"long_timer","elements":240}]),
            ascii,
            true,
        );
        let words: Vec<[u16; 4]> = (0..240_u16)
            .map(|i| [i, 65_535 - i, 0xa500 | (i % 4), 0xbeef])
            .collect();
        let wanted: Vec<_> = words.iter().map(|&w| expected("LTN", w)).collect();
        let server = thread::spawn(move || {
            let mut s = accept(&listener);
            let r = query(&mut s, ascii, true, (0x52, "LTN", 7, 960, false));
            send(
                &mut s,
                &reply_words(&r, ascii, &words.into_iter().flatten().collect::<Vec<_>>()),
            );
            closed(&mut s, &listener);
        });
        record(&home, &c).success();
        server.join().unwrap();
        let r = samples(&home, "complete");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["value"], json!(wanted));
        exports(&home, false);
    }
}
#[test]
fn malformed_late_timer_or_remote_fx5_rejection_keeps_prior_poll_only() {
    for mode in ["short", "extra", "route", "unsupported"] {
        let home = Home::new("slmp-long-prefix");
        let listener = listener();
        let mut c = configure(
            &listener,
            &json!([{"name":"word","device":"D","address":5},{"name":"long","device":"LTN","address":0,"data_type":"long_timer","elements":2}]),
            false,
            true,
        );
        c["sources"][0]["every_ms"] = json!(200);
        let server = thread::spawn(move || {
            let mut s = accept(&listener);
            for poll in 0..2 {
                let r = query(&mut s, false, true, (0xa8, "D", 5, 1, false));
                send(&mut s, &reply_words(&r, false, &[7]));
                let r = query(&mut s, false, true, (0x52, "LTN", 0, 8, false));
                let mut bytes = hex(literal()["binary_reply_hex"].as_str().unwrap());
                if poll == 1 {
                    match mode {
                        "short" => {
                            bytes.pop();
                            bytes[7..9].copy_from_slice(&17_u16.to_le_bytes());
                        }
                        "extra" => {
                            bytes.extend([0, 0]);
                            bytes[7..9].copy_from_slice(&20_u16.to_le_bytes());
                        }
                        "route" => bytes[2] = 1,
                        "unsupported" => bytes = response(&r, 0xc059, &[]),
                        _ => unreachable!(),
                    }
                }
                if poll == 1 && matches!(mode, "route" | "extra") {
                    s.write_all(&bytes[..9]).unwrap();
                    let _ = s.write_all(&bytes[9..]);
                } else {
                    send(&mut s, &bytes);
                }
            }
            closed(&mut s, &listener);
        });
        let error = record(&home, &c).failure(1);
        server.join().unwrap();
        assert_eq!(error["code"], "failed");
        if mode == "unsupported" {
            assert_eq!(error["details"]["slmp_end_code"], 0xc059);
        }
        let r = samples(&home, "failed");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["value"], 7);
        assert_eq!(r[1]["value"], literal()["expected"]);
        exports(&home, true);
    }
}
#[test]
fn progressing_long_timer_replies_and_siblings_share_original_deadline() {
    for ascii in [false, true] {
        let home = Home::new("slmp-long-deadline");
        let listener = listener();
        let mut c = configure(
            &listener,
            &json!([{"name":"first","device":"LTN","address":0,"data_type":"long_timer"},{"name":"second","device":"LSTN","address":1,"data_type":"long_timer"},{"name":"never","device":"D","address":0}]),
            ascii,
            true,
        );
        c["sources"][0]["timeout_ms"] = json!(time::ms(250));
        let server = thread::spawn(move || {
            let mut s = accept(&listener);
            let r = query(&mut s, ascii, true, (0x52, "LTN", 0, 4, false));
            thread::sleep(time::millis(100));
            send(&mut s, &reply_words(&r, ascii, &[1, 2, 3, 4]));
            let r = query(&mut s, ascii, true, (0x5a, "LSTN", 1, 4, false));
            let bytes = reply_words(&r, ascii, &[5, 6, 7, 8]);
            for chunk in bytes.chunks(if ascii { 13 } else { 7 }) {
                thread::sleep(time::millis(100));
                match s.write_all(chunk) {
                    Ok(()) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        break;
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            closed(&mut s, &listener);
        });
        assert_eq!(record(&home, &c).failure(4)["code"], "timeout");
        server.join().unwrap();
        assert_eq!(samples(&home, "failed").len(), 0);
    }
}
#[test]
fn long_timer_admission_rejects_partial_records_forbidden_devices_and_scaling_before_io() {
    for signal in [
        json!({"name":"bad","device":"LTN","address":0}),
        json!({"name":"bad","device":"LTN","address":0,"data_type":"u32"}),
        json!({"name":"bad","device":"D","address":0,"data_type":"long_timer"}),
        json!({"name":"bad","device":"LTN","address":0,"data_type":"long_timer","elements":0}),
        json!({"name":"bad","device":"LTN","address":0,"data_type":"long_timer","elements":241}),
        json!({"name":"bad","device":"LTN","address":u32::MAX,"data_type":"long_timer","elements":2}),
        json!({"name":"bad","device":"LSTN","address":0,"data_type":"long_timer","scale":0.5}),
        json!({"name":"bad","device":"LTN","address":0,"data_type":"long_timer","offset":1}),
        json!({"name":"bad","device":"LTS","address":0,"data_type":"bool"}),
        json!({"name":"bad","device":"LTC","address":0,"data_type":"bool"}),
        json!({"name":"bad","device":"LSTS","address":0,"data_type":"bool"}),
        json!({"name":"bad","device":"LSTC","address":0,"data_type":"bool"}),
    ] {
        let home = Home::new("slmp-long-preopen");
        let listener = listener();
        let c = configure(&listener, &json!([signal]), false, true);
        assert_eq!(record(&home, &c).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    for (ascii, wide, address) in [(false, false, 0), (true, true, 99_999_999)] {
        let home = Home::new("slmp-long-range");
        let listener = listener();
        let c = configure(
            &listener,
            &json!([{"name":"bad","device":"LTN","address":address,"data_type":"long_timer","elements":2}]),
            ascii,
            wide,
        );
        assert_eq!(record(&home, &c).failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn actual_whole_poll_budget_refuses_later_query_and_definite_minimum_refuses_connection() {
    // Independent compact-JSON geometry, not the product Budget helper.
    let low = expected("LTN", [0, 0, 3, 0]);
    let high = expected("LTN", [65_535, 65_535, 65_532, 65_535]);
    let minimum = serde_json::to_vec(&json!({"signal":"timer00","value":vec![low;240]}))
        .unwrap()
        .len()
        + 1;
    let expanded = serde_json::to_vec(&json!({"signal":"timer00","value":vec![high;240]}))
        .unwrap()
        .len()
        + 1;
    assert_eq!(2 + 36 * minimum, 1_046_558);
    assert_eq!(2 + expanded + 35 * minimum, 1_052_798);
    assert!(2 + 36 * minimum < 1 << 20);
    assert!(2 + expanded + 35 * minimum > 1 << 20);
    for definite in [false, true] {
        let home = Home::new("slmp-long-budget");
        let listener = listener();
        let signals:Vec<Value>=(0..if definite{37}else{36}).map(|i|json!({"name":format!("timer{i:02}"),"device":"LTN","address":0,"data_type":"long_timer","elements":240})).collect();
        let c = configure(&listener, &json!(signals), false, true);
        if definite {
            assert_eq!(record(&home, &c).failure(1)["code"], "failed");
            assert!(!home.path("run.jsonl").exists());
            listener.set_nonblocking(true).unwrap();
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        } else {
            let server = thread::spawn(move || {
                let mut stream = accept(&listener);
                let r = query(&mut stream, false, true, (0x52, "LTN", 0, 960, false));
                let words: Vec<u16> = [65_535, 65_535, 65_532, 65_535]
                    .into_iter()
                    .cycle()
                    .take(960)
                    .collect();
                send(&mut stream, &reply_words(&r, false, &words));
                // The first expanded object batch plus required remaining minima
                // cannot fit. There must be no second0401 or new connection.
                closed(&mut stream, &listener);
            });
            assert_eq!(record(&home, &c).failure(1)["code"], "failed");
            server.join().unwrap();
            assert_eq!(samples(&home, "failed").len(), 0);
            exports(&home, true);
        }
    }
}
