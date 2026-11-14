//! Spec-derived XGB Cnet peers over owned PTYs; no physical PLC claim.
use super::{Device, Home, assert_unpublished, receive, serial_pair, support::cli::Run};
use crate::support::time;
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    fs,
    io::{self, Read, Write as _},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

fn frame(body: &[u8], tail: u8) -> Vec<u8> {
    let mut result = body.to_vec();
    result.push(tail);
    let sum = result
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    result.extend(format!("{sum:02X}").bytes());
    result
}
fn query(port: &mut (impl Read + std::os::fd::AsRawFd), stop: &AtomicBool, expected: &[u8]) {
    let mut bytes = vec![0; expected.len()];
    assert!(receive(port, &mut bytes, stop));
    assert_eq!(bytes, expected);
}
fn request(unit: u8, variables: &[&str]) -> Vec<u8> {
    let mut body = format!("\x05{unit:02X}rSS{:02X}", variables.len());
    for variable in variables {
        write!(body, "{:02X}{variable}", variable.len()).unwrap();
    }
    frame(body.as_bytes(), 4)
}
fn reply(unit: u8, blocks: &[&str]) -> Vec<u8> {
    let mut body = format!("\x06{unit:02X}rSS{:02X}", blocks.len());
    for hex in blocks {
        write!(body, "{:02X}{hex}", hex.len() / 2).unwrap();
    }
    frame(body.as_bytes(), 3)
}
fn no_more(port: &mut (impl Read + std::os::fd::AsRawFd), stop: &AtomicBool) {
    let end = Instant::now() + time::secs(10);
    while !stop.load(Ordering::Acquire) {
        match port.read(&mut [0]) {
            Ok(0) => return,
            Ok(_) => panic!("unexpected Cnet query after transaction"),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                assert!(Instant::now() < end, "Cnet owner did not close");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) || error.raw_os_error() == Some(libc::EIO) =>
            {
                return;
            }
            Err(error) => panic!("Cnet fixture read: {error}"),
        }
    }
}
fn source(path: &str, signals: Value) -> Value {
    let mut config = json!({"id":"plc","protocol":"ls_cnet","profile":"xgb_dedicated","device":path,
        "baud":9.6e3,"parity":"none","data_bits":8,"every_ms":3_600_000});
    config["signals"] = signals;
    config
}
fn run(home: &Home, sources: &[Value], duration: &str) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":sources}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        &super::window(duration),
        "-o",
        "run.jsonl",
    ])
}
fn rows(home: &Home, path: &str) -> Vec<Value> {
    fs::read_to_string(home.path(path))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn samples(home: &Home, path: &str) -> Vec<Value> {
    rows(home, path)
        .into_iter()
        .filter(|row| row["source"] == "plc")
        .collect()
}
fn assert_output(home: &Home, expected: &[(String, Value)], complete: bool, total: usize) {
    let recorded = rows(home, "run.jsonl");
    let end = recorded.last().unwrap();
    assert_eq!(end["status"], if complete { "complete" } else { "failed" });
    assert_eq!(end["samples"], total);
    let actual = samples(home, "run.jsonl");
    assert_eq!(actual.len(), expected.len());
    for (row, (signal, value)) in actual.iter().zip(expected) {
        assert_eq!(row["signal"].as_str(), Some(signal.as_str()));
        assert_eq!(&row["value"], value);
        assert!(row.get("source_time").is_none());
        for key in ["elapsed_ns", "read_duration_ns", "delivery_latency_ns"] {
            assert!(row[key].is_u64());
        }
    }
    if !complete {
        assert_eq!(
            home.fails(1, &["data", "export", "run.jsonl", "-o", "refused.csv"])["code"],
            "incomplete"
        );
        assert!(!home.path("refused.csv").exists());
    }
    for format in ["jsonl", "csv"] {
        let mut args = vec![
            "--json",
            "data",
            "export",
            "run.jsonl",
            "-o",
            format,
            "--format",
            format,
        ];
        if !complete {
            args.push("--allow-incomplete");
        }
        assert_eq!(home.run(&args).success()["samples"], total);
    }
    let exported = samples(home, "jsonl");
    assert_eq!(exported.len(), actual.len());
    for (row, original) in exported.iter().zip(&actual) {
        let mut projected = original.clone();
        assert_eq!(
            projected.as_object_mut().unwrap().remove("kind"),
            Some(json!("sample"))
        );
        assert_eq!(row, &projected);
    }
    let csv = fs::read_to_string(home.path("csv")).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json"
    );
    let lines = lines.collect::<Vec<_>>();
    assert_eq!(lines.len(), total);
    let actual_values = lines
        .iter()
        .filter(|line| line.split(',').nth(1) == Some("\"plc\""))
        .map(|line| line.splitn(9, ',').nth(8).unwrap())
        .collect::<Vec<_>>();
    let values = expected
        .iter()
        .map(|(_, value)| format!("\"{}\"", value.to_string().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    assert_eq!(
        actual_values,
        values.iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[test]
fn ls_cnet_scalar_and_explicit_blocks_keep_native_widths_and_integer_exports() {
    let home = Home::new("cnet-types");
    let (mut master, slave, path) = serial_pair();
    let cases = [
        (0_u8, vec!["%MX00A"], "bool", false, vec!["01"], json!(true)),
        (
            255,
            vec!["%MX00A", "%PX000", "%MX00A"],
            "bool",
            true,
            vec!["01", "00", "01"],
            json!([true, false, true]),
        ),
        (
            1,
            vec!["%MB000", "%PB010"],
            "i8",
            true,
            vec!["80", "7F"],
            json!([-128, 127]),
        ),
        (
            10,
            vec!["%MW020", "%PW001"],
            "u16",
            true,
            vec!["1234", "5678"],
            json!([4660, 22136]),
        ),
        (1, vec!["%MW001"], "i16", false, vec!["8000"], json!(-32768)),
        (
            1,
            vec!["%MD002"],
            "u32",
            false,
            vec!["FFFFFFFF"],
            json!(u32::MAX),
        ),
        (
            1,
            vec!["%MD003"],
            "i32",
            false,
            vec!["80000000"],
            json!(i32::MIN),
        ),
        (
            1,
            vec!["%ML004", "%PL001", "%ML004"],
            "u64",
            true,
            vec!["FFFFFFFFFFFFFFFF", "0020000000000001", "0123456789ABCDEF"],
            json!([
                u64::MAX,
                9_007_199_254_740_993_u64,
                0x0123_4567_89ab_cdef_u64
            ]),
        ),
        (
            1,
            vec!["%ML005", "%PL003"],
            "i64",
            true,
            vec!["8000000000000000", "7FFFFFFFFFFFFFFF"],
            json!([i64::MIN, i64::MAX]),
        ),
        (
            1,
            vec!["%MB006"; 16],
            "u8",
            true,
            vec!["FF"; 16],
            json!(vec![255_u8; 16]),
        ),
        (20, vec!["%mw007"], "u16", false, vec!["A9F3"], json!(43507)),
        (1, vec!["%IW0.0.0"], "u16", false, vec!["0000"], json!(0)),
        (1, vec!["%MW000"], "u16", true, vec!["0007"], json!([7])),
        (
            1,
            vec!["%ML0"; 13],
            "u64",
            true,
            vec!["FFFFFFFFFFFFFFFF"; 13],
            json!(vec![u64::MAX; 13]),
        ),
        (
            1,
            vec!["%MW0000000000000"; 13],
            "u16",
            true,
            vec!["0007"; 13],
            json!(vec![7; 13]),
        ),
    ];
    let count = u64::try_from(cases.len()).unwrap();
    let mut signals = Vec::new();
    let mut expected = Vec::new();
    let mut exchanges = Vec::new();
    for (index, (unit, variables, data_type, array, data, value)) in cases.into_iter().enumerate() {
        let name = format!("v{index}");
        let mut signal = json!({"name":name,"unit":unit,"data_type":data_type});
        if array {
            signal["variables"] = json!(variables);
        } else {
            signal["variable"] = json!(variables.first().unwrap());
        }
        signals.push(signal);
        expected.push((name, value));
        exchanges.push((request(unit, &variables), reply(unit, &data)));
    }
    let device = Device::start(move |stop| {
        let _slave = slave;
        for (request, reply) in exchanges {
            query(&mut master, &stop, &request);
            for part in reply.chunks(3) {
                master.write_all(part).unwrap();
            }
        }
        no_more(&mut master, &stop);
        count
    });
    let sources = [
        source(&path, json!(signals)),
        json!({"id":"counter","protocol":"simulator","signal":"cycle","every_ms":3_600_000}),
    ];
    assert_eq!(
        run(&home, &sources, "500ms").success()["samples"],
        expected.len() + 1
    );
    assert_eq!(device.finish(), count);
    assert_output(&home, &expected, true, expected.len() + 1);
}

#[test]
fn ls_cnet_vendor_request_and_reconstructed_multi_reply_reuse_one_owner() {
    let home = Home::new("cnet-repeat");
    let (mut master, slave, path) = serial_pair();
    let device = Device::start(move |stop| {
        let _slave = slave;
        for _ in 0..3 {
            query(&mut master, &stop, b"\x0520rSS0106%MW100\x04A4");
            master.write_all(&reply(32, &["A9F3"])).unwrap();
            query(&mut master, &stop, b"\x0501rSS0206%MW02006%PW001\x0468");
            master.write_all(b"\x0601rSS02021234025678\x034C").unwrap();
        }
        no_more(&mut master, &stop);
        6_u64
    });
    let mut config = source(
        &path,
        json!([
            {"name":"vendor","unit":32,"variable":"%MW100"},
            {"name":"words","variables":["%MW020","%PW001"]}
        ]),
    );
    config["every_ms"] = json!(time::ms(200));
    assert_eq!(run(&home, &[config], "550ms").success()["samples"], 6);
    assert_eq!(device.finish(), 6);
    let mut expected = Vec::new();
    for _ in 0..3 {
        expected.extend([
            ("vendor".into(), json!(43507)),
            ("words".into(), json!([4660, 22136])),
        ]);
    }
    assert_output(&home, &expected, true, 6);
}

#[test]
fn ls_cnet_malformed_late_block_never_publishes_a_partial_poll() {
    for (index, bad) in [
        frame(b"\x0601rSS01027FFF", 3),
        frame(b"\x060ArSB01027FFF", 3),
        frame(b"\x060ARSS01027FFF", 3),
        frame(b"\x060ArSS02027FFF", 3),
        frame(b"\x060ArSS01017FFF", 3),
        frame(b"\x060ArSS01027FFG", 3),
        frame(b"\x060ArSS01027FFF", 4),
        b"\x060ArSS01027FFF\x0300".to_vec(),
        frame(b"\x150ArSS1132", 3),
        {
            let mut extra = reply(10, &["7FFF"]);
            extra.extend(b"extra");
            extra
        },
        frame(b"\x060ArSS010102", 3),
        frame(b"\x060ArSS02020007027FFG", 3),
        frame(b"\x060ArSS02020007017FFF", 3),
    ]
    .into_iter()
    .enumerate()
    {
        for prior in [false, true] {
            let home = Home::new(&format!("cnet-bad-{index}-{prior}"));
            let (mut master, slave, path) = serial_pair();
            let boolean = index == 10;
            let multi = index >= 11;
            let variables = if boolean {
                vec!["%MX001"]
            } else if multi {
                vec!["%MW001", "%PW002"]
            } else {
                vec!["%MW001"]
            };
            let request2 = request(10, &variables);
            let bad = bad.clone();
            let device = Device::start(move |stop| {
                let _slave = slave;
                let mut count = 0_u64;
                if prior {
                    query(&mut master, &stop, &request(10, &["%MW000"]));
                    master.write_all(&reply(10, &["0007"])).unwrap();
                    query(&mut master, &stop, &request2);
                    master
                        .write_all(&reply(
                            10,
                            if boolean {
                                &["01"]
                            } else if multi {
                                &["0007", "0007"]
                            } else {
                                &["0007"]
                            },
                        ))
                        .unwrap();
                    count += 2;
                }
                query(&mut master, &stop, &request(10, &["%MW000"]));
                master.write_all(&reply(10, &["0007"])).unwrap();
                query(&mut master, &stop, &request2);
                master.write_all(&bad).unwrap();
                no_more(&mut master, &stop);
                count + 2
            });
            let second = if boolean {
                json!({"name":"second","variable":"%MX001","data_type":"bool"})
            } else if multi {
                json!({"name":"second","variables":["%MW001","%PW002"]})
            } else {
                json!({"name":"second","variable":"%MW001"})
            };
            let mut config = source(&path, json!([{"name":"first","variable":"%MW000"},second]));
            config["unit"] = json!(10);
            if prior {
                config["every_ms"] = json!(time::ms(200));
            }
            assert_eq!(run(&home, &[config], "1s").failure(1)["code"], "failed");
            assert_eq!(device.finish(), if prior { 4 } else { 2 });
            let expected = if prior {
                vec![
                    ("first".into(), json!(7)),
                    (
                        "second".into(),
                        if boolean {
                            json!(true)
                        } else if multi {
                            json!([7, 7])
                        } else {
                            json!(7)
                        },
                    ),
                ]
            } else {
                vec![]
            };
            if !prior {
                assert_unpublished(&home);
            }
            assert_output(&home, &expected, false, expected.len());
        }
    }
}

#[test]
fn ls_cnet_request_and_acquisition_cutoffs_keep_original_budget_and_prefix() {
    for acquisition in [false, true] {
        let home = Home::new(&format!("cnet-deadline-{acquisition}"));
        let (mut master, slave, path) = serial_pair();
        let device = Device::start(move |stop| {
            let _slave = slave;
            query(&mut master, &stop, &request(1, &["%MW000"]));
            master.write_all(&reply(1, &["0007"])).unwrap();
            query(
                &mut master,
                &stop,
                &request(1, &[if acquisition { "%MW000" } else { "%MW001" }]),
            );
            if acquisition {
                master.write_all(b"\x0601rSS").unwrap();
            } else {
                for part in reply(1, &["0007"]).chunks(5) {
                    thread::sleep(time::millis(100));
                    master.write_all(part).unwrap();
                }
            }
            no_more(&mut master, &stop);
            2_u64
        });
        let signals = if acquisition {
            json!([{"name":"first","variable":"%MW000"}])
        } else {
            json!([{"name":"first","variable":"%MW000"},{"name":"second","variable":"%MW001"}])
        };
        let mut config = source(&path, signals);
        if acquisition {
            config["every_ms"] = json!(time::ms(200));
        } else {
            config["timeout_ms"] = json!(time::ms(250));
        }
        assert_eq!(
            run(&home, &[config], if acquisition { "350ms" } else { "1s" }).failure(4)["code"],
            "timeout"
        );
        assert_eq!(device.finish(), 2);
        let expected = if acquisition {
            vec![("first".into(), json!(7))]
        } else {
            vec![]
        };
        assert_output(&home, &expected, false, expected.len());
    }
}

#[test]
fn ls_cnet_invalid_config_never_opens_device_or_creates_recording() {
    let base = source(
        "/definitely-not-a-cnet-device",
        json!([{"name":"word","variable":"%MW000"}]),
    );
    let mut cases = Vec::new();
    for (field, value) in [
        ("profile", json!("xgt")),
        ("baud", json!(921_600)),
        ("baud", json!(300)),
        ("data_bits", json!(6)),
        ("stop_bits", json!(3)),
        ("unit", json!(256)),
        ("unit", json!(-1)),
        ("device", json!("")),
        ("device", json!("bad\u{0}device")),
        ("signals", json!([])),
        ("unknown", json!(1)),
    ] {
        let mut config = base.clone();
        config[field] = value;
        cases.push(config);
    }
    for signal in [
        json!({"name":"x","variable":"MW0"}),
        json!({"name":"x","variable":"%DW0","data_type":"u32"}),
        json!({"name":"x","variable":"%MX0"}),
        json!({"name":"x","variable":"%MW0","elements":1}),
        json!({"name":"x","variable":"%MW0","data_type":"u8"}),
        json!({"name":"x","variable":"%MW0","data_type":"f32"}),
        json!({"name":"x","variable":"%MW0\u{4}"}),
        json!({"name":"x","variable":"%MW0.0"}),
        json!({"name":"x","variable":"%IW0..0"}),
        json!({"name":"x","variable":"%MW000000000000000"}),
        json!({"name":"x","variable":"%MW0","unit":256}),
        json!({"name":"x","variable":"%MW0","scale":1}),
        json!({"name":"x","variable":"%NW0"}),
        json!({"name":"x","variable":"%UW00.00"}),
        json!({"name":"x","variable":"%ZW0"}),
        json!({"name":"x","variable":"%MW0","variables":["%MW0"]}),
        json!({"name":"x"}),
        json!({"name":"x","variables":[]}),
        json!({"name":"x","variables":["%MW0","%MB0"]}),
        json!({"name":"x","variables":vec!["%MW0";17]}),
        json!({"name":"x","variables":vec!["%ML0";14],"data_type":"u64"}),
        json!({"name":"x","variables":vec!["%MW0000000000000";14]}),
    ] {
        let mut config = base.clone();
        config["signals"] = json!([signal]);
        cases.push(config);
    }
    let mut duplicate = base;
    duplicate["signals"] = json!([{"name":"x","variable":"%MW0"},{"name":"x","variable":"%MW1"}]);
    cases.push(duplicate);
    for (index, config) in cases.into_iter().enumerate() {
        let home = Home::new(&format!("cnet-config-{index}"));
        assert_eq!(run(&home, &[config], "1ms").failure(2)["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}
