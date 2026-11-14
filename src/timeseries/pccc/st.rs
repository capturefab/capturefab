//! Whole SLC/`MicroLogix` ST elements: LEN and41 character words, each native LE16.
//! 1747-RM001G §10-3/4 and1763-RM001D §String Data File Structure. Character0
//! is the upper byte, character1 lower. No terminator/padding/codepage inference.
use super::{Address, Encoding, MAX_BATCH, Reading, Signal, SizeLimit, protocol, remaining};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{io::Write as _, time::Instant};

const WIDTH: usize = 84;
const CAPACITY: u16 = 82;
const RAW_BASE64_BYTES: usize = 112;
// Both complete schema states count every required key/value/punctuation.
// Raw base64 is always112 unescaped ASCII bytes. Invalid83 is the shortest
// invalid object; valid0 has the shortest active field. Text can only add bytes.
const MIN_INVALID: usize = br#"{"kind":"pccc_string","length_word_raw":83,"capacity":82,"length_status":"invalid_length","raw":{"encoding":"base64","scope":"native_element","data":""}}"#.len() + RAW_BASE64_BYTES;
const MIN_VALID: usize = br#"{"kind":"pccc_string","length_word_raw":0,"capacity":82,"length_status":"valid","raw":{"encoding":"base64","scope":"native_element","data":""},"active":{"encoding":"base64","scope":"active_data","data":""}}"#.len() + RAW_BASE64_BYTES;
const MIN_VALUE_BYTES: usize = if MIN_INVALID < MIN_VALID {
    MIN_INVALID
} else {
    MIN_VALID
};

fn minimum(size: &SizeLimit, elements: usize) -> Result<()> {
    let total = elements
        .checked_mul(MIN_VALUE_BYTES)
        .and_then(|bytes| size.bytes.checked_add(bytes))
        .ok_or_else(limit)?;
    if total > MAX_BATCH {
        return Err(limit());
    }
    Ok(())
}

fn begin(signal: &Signal, size: &mut SizeLimit) -> Result<()> {
    let envelope = Reading {
        signal: signal.name.clone(),
        value: Value::Null,
        source_time: None,
    };
    serde_json::to_writer(&mut *size, &envelope).map_err(|_| limit())?;
    size.bytes -= 4;
    size.write_all(b",").map_err(|_| limit())?;
    let elements = usize::from(signal.elements);
    let punctuation = if elements == 1 { 0 } else { elements + 1 };
    size.bytes = size.bytes.checked_add(punctuation).ok_or_else(limit)?;
    minimum(size, elements)
}

pub(super) fn read(
    signal: &Signal,
    address: Address,
    max_read_bytes: u16,
    size: &mut SizeLimit,
    deadline: Instant,
    read_chunk: &mut impl FnMut(Address, u16, usize, &mut Vec<u8>) -> Result<()>,
) -> Result<Value> {
    if !(84..=224).contains(&max_read_bytes) || !(1..=256).contains(&signal.elements) {
        return Err(protocol("invalid whole-ST request bounds"));
    }
    begin(signal, size)?;
    let elements = usize::from(signal.elements);
    let mut values = Vec::with_capacity(elements);
    let mut data = Vec::with_capacity(usize::from(max_read_bytes).min(2 * WIDTH));
    while values.len() < elements {
        remaining(deadline)?;
        minimum(size, elements - values.len())?;
        let points = (usize::from(max_read_bytes) / WIDTH).min(elements - values.len());
        let element = address
            .element
            .checked_add(u16::try_from(values.len())?)
            .ok_or_else(|| protocol("ST logical element overflow"))?;
        data.clear();
        read_chunk(address, element, points * WIDTH, &mut data)?;
        if data.len() != points * WIDTH {
            return Err(protocol(
                "ST response differs from requested whole elements",
            ));
        }
        for raw in data.as_chunks::<WIDTH>().0 {
            remaining(deadline)?;
            let value = native(raw, signal.encoding)?;
            serde_json::to_writer(&mut *size, &value).map_err(|_| limit())?;
            // A larger earlier value may make the rest of this array impossible.
            // Refuse before Vec growth and any following application request.
            minimum(size, elements - values.len() - 1)?;
            remaining(deadline)?;
            values.push(value);
        }
    }
    remaining(deadline)?;
    if signal.elements == 1 {
        values.pop().ok_or_else(|| protocol("missing ST scalar"))
    } else {
        Ok(Value::Array(values))
    }
}

fn native(raw: &[u8; WIDTH], encoding: Option<Encoding>) -> Result<Value> {
    let length = u16::from_le_bytes([raw[0], raw[1]]);
    let valid = length <= CAPACITY;
    let mut value = json!({
        "kind":"pccc_string", "length_word_raw":length, "capacity":CAPACITY,
        "length_status":if valid {"valid"} else {"invalid_length"},
        "raw":{"encoding":"base64","scope":"native_element","data":STANDARD.encode(raw)}
    });
    if !valid {
        return Ok(value);
    }
    let mut canonical: [u8; 82] = raw
        .get(2..)
        .ok_or_else(|| protocol("missing ST characters"))?
        .try_into()?;
    for pair in canonical.as_chunks_mut::<2>().0 {
        pair.reverse();
    }
    let active = canonical
        .get(..usize::from(length))
        .ok_or_else(|| protocol("invalid ST active length"))?;
    value["active"] =
        json!({"encoding":"base64","scope":"active_data","data":STANDARD.encode(active)});
    if let Some(encoding) = encoding {
        if encoding == Encoding::Ascii && !active.is_ascii() {
            return Err(protocol(
                "ST active bytes are not the configured strict ASCII",
            ));
        }
        let text = std::str::from_utf8(active)
            .map_err(|_| protocol("ST active bytes are not the configured strict UTF8"))?;
        value["encoding"] = json!(encoding.name());
        value["text"] = json!(text);
    }
    Ok(value)
}

fn limit() -> anyhow::Error {
    protocol("serialized poll exceeds 1 MiB batch limit")
}

#[cfg(test)]
mod tests {
    use super::super::{
        Config, DataType, Encoding, Profile, Reading, Signal, SizeLimit, address, open,
        read_signals, signal_addresses, typed_request,
    };
    use super::{
        CAPACITY, MIN_VALUE_BYTES, RAW_BASE64_BYTES, STANDARD, WIDTH, begin, native, read,
    };
    use crate::types::{ErrorKind, classify, decode_hex, hex};
    use base64::Engine as _;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    fn vectors() -> Vec<Value> {
        let data: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/pccc/st-literals.json"
        ))
        .unwrap();
        data["vectors"].as_array().unwrap().clone()
    }
    fn raw(case: &Value) -> [u8; WIDTH] {
        let text: String = case["native_hex_chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part.as_str().unwrap())
            .collect();
        decode_hex(&text, WIDTH).unwrap().try_into().unwrap()
    }
    fn signal(value: Value) -> Signal {
        serde_json::from_value(value).unwrap()
    }
    fn expected(case: &Value, encoding: Option<Encoding>) -> Value {
        let mut value = json!({"kind":"pccc_string","length_word_raw":case["length_word_raw"],"capacity":82,
            "length_status":case["length_status"],"raw":{"encoding":"base64","scope":"native_element","data":STANDARD.encode(raw(case))}});
        if case["length_status"] == "valid" {
            let active = if case["active_hex"] == "" {
                Vec::new()
            } else {
                decode_hex(case["active_hex"].as_str().unwrap(), 82).unwrap()
            };
            value["active"] =
                json!({"encoding":"base64","scope":"active_data","data":STANDARD.encode(&active)});
            if let Some(encoding) = encoding {
                value["encoding"] = json!(encoding.name());
                let text = case[if encoding == Encoding::Ascii {
                    "ascii_text"
                } else {
                    "utf8_text"
                }]
                .as_str()
                .unwrap_or_else(|| std::str::from_utf8(&active).unwrap());
                value["text"] = json!(text);
            }
        }
        value
    }

    #[test]
    fn independent_native_literals_keep_every_byte_and_only_explicit_strict_active_text() {
        for case in vectors() {
            for encoding in [None, Some(Encoding::Ascii), Some(Encoding::Utf8)] {
                let invalid = encoding.is_some_and(|encoding| {
                    case[if encoding == Encoding::Ascii {
                        "ascii_valid"
                    } else {
                        "utf8_valid"
                    }] == false
                });
                if invalid {
                    assert_eq!(
                        classify(&native(&raw(&case), encoding).unwrap_err()).kind,
                        ErrorKind::Incomplete
                    );
                } else {
                    let value = native(&raw(&case), encoding).unwrap();
                    assert_eq!(value, expected(&case, encoding), "{}", case["id"]);
                    assert_eq!(
                        STANDARD
                            .decode(value["raw"]["data"].as_str().unwrap())
                            .unwrap()
                            .as_slice(),
                        raw(&case).as_slice()
                    );
                    if case["length_status"] == "invalid_length" {
                        assert!(
                            value.get("active").is_none()
                                && value.get("encoding").is_none()
                                && value.get("text").is_none()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn whole_st_ranges_options_and_all_transport_caps_are_admitted_before_io() {
        for file in [3, 8, 9, 255] {
            for prefix in ["ST", "st", "St"] {
                let parsed=address(&signal(json!({"name":"string","address":format!("{prefix}{file}:255"),"data_type":"string"}))).unwrap();
                assert_eq!(parsed.file_type, 0x8d);
                assert_eq!(parsed.width(), WIDTH);
                assert_eq!(parsed.file, file);
            }
        }
        for invalid in [
            json!({"address":"ST0:0"}),
            json!({"address":"ST1:0"}),
            json!({"address":"ST2:0"}),
            json!({"address":"ST256:0"}),
            json!({"address":"ST9:256"}),
            json!({"address":"ST9:255","elements":2}),
            json!({"address":"ST9:0","elements":257}),
            json!({"address":"ST9:0/0"}),
            json!({"address":"ST9:0.LEN"}),
            json!({"address":"ST9:0.DATA[0]"}),
            json!({"data_type":"i16"}),
            json!({"data_type":"timer"}),
            json!({"scale":2}),
            json!({"offset":1}),
            json!({"address":"N7:0","encoding":"ascii"}),
            json!({"address":"T4:0","encoding":"utf8"}),
        ] {
            let mut item = json!({"name":"string","address":"ST9:0"});
            for (key, value) in invalid.as_object().unwrap() {
                item[key].clone_from(value);
            }
            assert_eq!(
                classify(&signal_addresses(&[signal(item)]).unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        for cap in [4, 5, 6, 83] {
            let body = json!({"address":"127.0.0.1:1","serial_number":1,"max_read_bytes":cap,"signals":[{"name":"string","address":"ST3:0"}]});
            let config: Config = serde_json::from_value(body.clone()).unwrap();
            assert_eq!(
                classify(&open(&config).map(drop).unwrap_err()).kind,
                ErrorKind::Usage
            );
            let mut body = body;
            body.as_object_mut().unwrap().remove("serial_number");
            let config: super::super::csp::Config = serde_json::from_value(body).unwrap();
            assert_eq!(
                classify(&super::super::csp::open(&config).map(drop).unwrap_err()).kind,
                ErrorKind::Usage
            );
            let config:super::super::df1::Config=serde_json::from_value(json!({"device":"must-not-open","destination_node":4,"max_read_bytes":cap,"signals":[{"name":"string","address":"ST3:0"}]})).unwrap();
            assert_eq!(
                classify(&super::super::df1::open(&config).map(drop).unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        for cap in [84, 167, 168, 220, 224] {
            let config:Config=serde_json::from_value(json!({"address":"127.0.0.1:1","serial_number":1,"max_read_bytes":cap,"signals":[{"name":"string","address":"ST3:0"}]})).unwrap();
            drop(open(&config).unwrap());
        }
        let mut config:Config=serde_json::from_value(json!({"address":"127.0.0.1:1","serial_number":1,"signals":[{"name":"string","address":"ST9:0"}]})).unwrap();
        config.profile = Profile::Plc5;
        assert_eq!(
            classify(&open(&config).map(drop).unwrap_err()).kind,
            ErrorKind::Usage
        );
    }

    #[test]
    fn whole_st_chunks_use_native_element_counts_and_match_independent_requests() {
        let cases = vectors();
        let bytes: Vec<_> = cases[..3].iter().flat_map(raw).collect();
        for cap in [84, 167, 168, 220, 224] {
            let source = signal(json!({"name":"strings","address":"ST9:10","elements":3}));
            let parsed = address(&source).unwrap();
            let mut queries = Vec::new();
            let mut size = SizeLimit { bytes: 2 };
            let value = read(
                &source,
                parsed,
                cap,
                &mut size,
                Instant::now() + Duration::from_secs(5),
                &mut |a, index, len, data| {
                    queries.push((index, len));
                    assert_eq!(a.file_type, 0x8d);
                    let start = usize::from(index - 10) * WIDTH;
                    data.extend_from_slice(&bytes[start..start + len]);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                queries,
                if cap < 168 {
                    vec![(10, 84), (11, 84), (12, 84)]
                } else {
                    vec![(10, 168), (12, 84)]
                }
            );
            assert_eq!(
                value,
                Value::Array(cases[..3].iter().map(|case| expected(case, None)).collect())
            );
            let reading = Reading {
                signal: "strings".into(),
                value,
                source_time: None,
            };
            assert_eq!(size.bytes, 3 + serde_json::to_vec(&reading).unwrap().len());
        }
        for (addr, elements, element, len, tns, expected_hex) in [
            ("ST9:10", 3, 10, 168, 0x1234, "0f003412a2a8098d0a00"),
            ("ST9:10", 3, 12, 84, 0x1235, "0f003512a254098d0c00"),
            (
                "ST255:255",
                1,
                255,
                84,
                0x1236,
                "0f003612a254ffff008dffff0000",
            ),
        ] {
            let parsed = address(&signal(
                json!({"name":"string","address":addr,"elements":elements}),
            ))
            .unwrap();
            let mut request = Vec::new();
            typed_request(&mut request, parsed, element, len, tns).unwrap();
            assert_eq!(hex(request), expected_hex);
        }
        let source = signal(json!({"name":"max","address":"ST8:0","elements":256}));
        let parsed = address(&source).unwrap();
        let data = raw(&cases[0]);
        let mut calls = 0;
        let mut size = SizeLimit { bytes: 2 };
        let value = read(
            &source,
            parsed,
            224,
            &mut size,
            Instant::now() + Duration::from_secs(5),
            &mut |_, element, len, out| {
                assert_eq!(usize::from(element), calls * 2);
                assert_eq!(len, 168);
                calls += 1;
                out.extend_from_slice(&data);
                out.extend_from_slice(&data);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls, 128);
        assert_eq!(value.as_array().unwrap().len(), 256);
        assert!(
            value
                .as_array()
                .unwrap()
                .iter()
                .all(|value| value == &expected(&cases[0], None))
        );
    }

    #[test]
    fn exact_json_limits_include_raw_metadata_text_and_every_undecoded_element() {
        let cases = vectors();
        let invalid = raw(&cases[8]);
        let empty = raw(&cases[5]);
        let minimum = [
            serde_json::to_vec(&native(&invalid, None).unwrap())
                .unwrap()
                .len(),
            serde_json::to_vec(&native(&empty, None).unwrap())
                .unwrap()
                .len(),
        ]
        .into_iter()
        .min()
        .unwrap();
        assert_eq!(MIN_VALUE_BYTES, minimum);
        assert_eq!(STANDARD.encode(invalid).len(), RAW_BASE64_BYTES);
        assert_eq!(CAPACITY, 82);
        let source = signal(json!({"name":"exact","address":"ST9:0","elements":2}));
        let parsed = address(&source).unwrap();
        let mut measured = SizeLimit { bytes: 2 };
        let deadline = Instant::now() + Duration::from_secs(5);
        let value = read(
            &source,
            parsed,
            168,
            &mut measured,
            deadline,
            &mut |_, _, _, out| {
                out.extend_from_slice(&invalid);
                out.extend_from_slice(&invalid);
                Ok(())
            },
        )
        .unwrap();
        let reading = Reading {
            signal: "exact".into(),
            value,
            source_time: None,
        };
        assert_eq!(
            measured.bytes,
            3 + serde_json::to_vec(&reading).unwrap().len()
        );
        let cost = measured.bytes - 2;
        let mut exact = SizeLimit {
            bytes: super::MAX_BATCH - cost,
        };
        read(
            &source,
            parsed,
            168,
            &mut exact,
            deadline,
            &mut |_, _, _, out| {
                out.extend_from_slice(&invalid);
                out.extend_from_slice(&invalid);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(exact.bytes, super::MAX_BATCH);
        let mut small = SizeLimit {
            bytes: super::MAX_BATCH - cost + 1,
        };
        let mut queries = 0;
        assert_eq!(
            classify(
                &read(
                    &source,
                    parsed,
                    168,
                    &mut small,
                    deadline,
                    &mut |_, _, _, _| {
                        queries += 1;
                        Ok(())
                    }
                )
                .unwrap_err()
            )
            .kind,
            ErrorKind::Incomplete
        );
        assert_eq!(queries, 0);
        // Initial minimum fits, but the first escaped-text object makes the
        // remaining two objects impossible. No second callback is permitted.
        let source =
            signal(json!({"name":"remaining","address":"ST9:0","elements":3,"encoding":"ascii"}));
        let parsed = address(&source).unwrap();
        let full = raw(&cases[7]);
        let first = serde_json::to_vec(&native(&full, Some(Encoding::Ascii)).unwrap())
            .unwrap()
            .len();
        let envelope = Reading {
            signal: source.name.clone(),
            value: Value::Null,
            source_time: None,
        };
        let overhead = serde_json::to_vec(&envelope).unwrap().len() - 4 + 1 + 4;
        let mut size = SizeLimit {
            bytes: super::MAX_BATCH - overhead - first - 2 * MIN_VALUE_BYTES + 1,
        };
        assert!(size.bytes + overhead + 3 * MIN_VALUE_BYTES <= super::MAX_BATCH);
        let mut queries = 0;
        let error = read(
            &source,
            parsed,
            84,
            &mut size,
            deadline,
            &mut |_, _, _, out| {
                queries += 1;
                out.extend_from_slice(&full);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        assert_eq!(queries, 1);
    }

    #[test]
    fn strict_text_and_budget_failures_are_atomic_and_stop_later_chunks_and_siblings() {
        let cases = vectors();
        let signals = [
            signal(json!({"name":"invalid_state","address":"ST9:0"})),
            signal(json!({"name":"text","address":"ST9:1","elements":3,"encoding":"utf8"})),
            signal(json!({"name":"never","address":"N7:0"})),
        ];
        let addresses = signal_addresses(&signals).unwrap();
        let mut queries = 0;
        let error = read_signals(
            &signals,
            &addresses,
            84,
            Instant::now() + Duration::from_secs(5),
            |a, index, _, out| {
                assert_eq!(a.file_type, 0x8d);
                queries += 1;
                out.extend_from_slice(&raw(&cases[match index {
                    0 => 8,
                    1 => 0,
                    2 => 3,
                    _ => panic!("unexpected later query"),
                }]));
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        assert_eq!(queries, 3);
        let signals:Vec<_>=(0..256).map(|i|signal(json!({"name":format!("strings{i}"),"address":"ST9:0","elements":256,"encoding":"ascii"}))).collect();
        let addresses = signal_addresses(&signals).unwrap();
        let full = raw(&cases[7]);
        let mut queries = 0;
        let error = read_signals(
            &signals,
            &addresses,
            224,
            Instant::now() + Duration::from_secs(5),
            |_, _, len, out| {
                queries += 1;
                for _ in 0..len / WIDTH {
                    out.extend_from_slice(&full);
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        assert!(queries > 0 && queries < 256 * 128);
        let source = signal(json!({"name":"late","address":"ST9:0"}));
        let parsed = address(&source).unwrap();
        let mut size = SizeLimit { bytes: 2 };
        let mut queries = 0;
        let error = read(
            &source,
            parsed,
            84,
            &mut size,
            Instant::now(),
            &mut |_, _, _, _| {
                queries += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        assert_eq!(queries, 0);
        let mut size = SizeLimit { bytes: 0 };
        begin(&source, &mut size).unwrap();
        assert!(size.bytes > 0);
        assert_eq!(address(&source).unwrap().kind, DataType::String);
    }
}
