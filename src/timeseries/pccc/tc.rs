//! Whole SLC timer/counter elements: control, preset, accumulator, each LE16.
//! Rockwell 1770-RM516 §7-17 (A2 types86/87), 1747-RM001G tables1.4/1.7.
//! No timebase, engineering units, CPU-specific status bits or snapshot claim.
use super::{Address, DataType, MAX_BATCH, Reading, Signal, SizeLimit, protocol, remaining};
use anyhow::Result;
use serde_json::{Value, json};
use std::{io::Write as _, time::Instant};

// The complete minimum object exceeds 128 bytes for either kind, including
// required number/flag literals (true is shorter than false).
const MIN_VALUE_BYTES: usize = 128;

pub(super) fn begin(signal: &Signal, size: &mut SizeLimit) -> Result<()> {
    let envelope = Reading {
        signal: signal.name.clone(),
        value: Value::Null,
        source_time: None,
    };
    serde_json::to_writer(&mut *size, &envelope).map_err(|_| limit())?;
    size.bytes -= 4; // Replace the serialized null with the exact native value.
    size.write_all(b",").map_err(|_| limit())?;
    let elements = usize::from(signal.elements);
    let punctuation = if elements == 1 { 0 } else { elements + 1 };
    let minimum = elements
        .checked_mul(MIN_VALUE_BYTES)
        .and_then(|bytes| bytes.checked_add(punctuation))
        .and_then(|bytes| bytes.checked_add(size.bytes))
        .ok_or_else(limit)?;
    if minimum > MAX_BATCH {
        return Err(limit());
    }
    // Array brackets and N−1 separators are charged before queries/allocation.
    size.bytes += punctuation;
    Ok(())
}

pub(super) fn decode(
    signal: &Signal,
    address: Address,
    data: &[u8],
    size: &mut SizeLimit,
    deadline: Instant,
) -> Result<Value> {
    if data.len() != usize::from(signal.elements) * 6 {
        return Err(protocol("T/C data differs from requested whole elements"));
    }
    let mut values = Vec::with_capacity(usize::from(signal.elements));
    for bytes in data.as_chunks::<6>().0 {
        remaining(deadline)?;
        let value = native(address.kind, *bytes)?;
        serde_json::to_writer(&mut *size, &value).map_err(|_| limit())?;
        values.push(value);
    }
    if signal.elements == 1 {
        values.pop().ok_or_else(|| protocol("missing T/C element"))
    } else {
        Ok(Value::Array(values))
    }
}

fn native(kind: DataType, bytes: [u8; 6]) -> Result<Value> {
    let control = u16::from_le_bytes([bytes[0], bytes[1]]);
    let preset = u16::from_le_bytes([bytes[2], bytes[3]]);
    let accumulator = u16::from_le_bytes([bytes[4], bytes[5]]);
    let bit = |index: u32| control & (1_u16 << index) != 0_u16;
    let (kind, flags) = match kind {
        DataType::Timer => (
            "pccc_timer",
            json!({"en":bit(15),"tt":bit(14),"dn":bit(13)}),
        ),
        DataType::Counter => (
            "pccc_counter",
            json!({"cu":bit(15),"cd":bit(14),"dn":bit(13),"ov":bit(12),"un":bit(11)}),
        ),
        _ => return Err(protocol("non-T/C kind used structured decoder")),
    };
    Ok(json!({
        "kind":kind,
        "control_word_raw":control,
        "preset":i16::from_le_bytes([bytes[2],bytes[3]]),
        "accumulator":i16::from_le_bytes([bytes[4],bytes[5]]),
        "native_words_raw":[control,preset,accumulator],
        "flags":flags
    }))
}

fn limit() -> anyhow::Error {
    protocol("serialized poll exceeds 1 MiB batch limit")
}

#[cfg(test)]
mod tests {
    use super::super::{
        Config, Profile, Signal, SizeLimit, address, open, read_signals, signal_addresses,
        typed_request,
    };
    use super::{DataType, MAX_BATCH, MIN_VALUE_BYTES, begin, native};
    use crate::types::{ErrorKind, classify};
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    fn signal(value: Value) -> Signal {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn independent_three_word_literals_preserve_flags_signed_counts_and_all_bits() {
        let cases: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/pccc/tc-literals.json"
        ))
        .unwrap();
        for case in cases["cases"].as_array().unwrap() {
            let source = signal(
                json!({"name":"native","address":case["address"],"elements":case["elements"]}),
            );
            let parsed = address(&source).unwrap();
            let mut request = Vec::new();
            typed_request(
                &mut request,
                parsed,
                parsed.element,
                usize::from(source.elements) * 6,
                u16::try_from(case["transaction_raw"].as_u64().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(crate::types::hex(&request), case["request_hex"]);
            let bytes =
                crate::types::decode_hex(case["data_hex"].as_str().unwrap(), 1_536).unwrap();
            for (bytes, expected) in bytes
                .as_chunks::<6>()
                .0
                .iter()
                .zip(case["expected"].as_array().unwrap())
            {
                let value = native(parsed.kind, *bytes).unwrap();
                assert_eq!(value["control_word_raw"], expected["control_word_raw"]);
                assert_eq!(value["preset"], expected["preset"]);
                assert_eq!(value["accumulator"], expected["accumulator"]);
                assert_eq!(value["native_words_raw"], expected["native_words_raw"]);
                assert_eq!(value["flags"], expected["portable_flags"]);
                assert_eq!(value["flags"]["ua"], Value::Null);
                let words = value["native_words_raw"].as_array().unwrap();
                let restored: Vec<_> = words
                    .iter()
                    .flat_map(|word| u16::try_from(word.as_u64().unwrap()).unwrap().to_le_bytes())
                    .collect();
                assert_eq!(restored.as_slice(), bytes.as_slice());
            }
        }
    }

    #[test]
    fn structured_scope_and_all_transport_caps_are_checked_before_io() {
        let base = json!({"name":"native","address":"T4:0"});
        for invalid in [
            json!({"address":"T5:0"}),
            json!({"address":"C4:0"}),
            json!({"address":"T256:0"}),
            json!({"address":"T4:256"}),
            json!({"address":"T4:255","elements":2}),
            json!({"address":"T4:0/0"}),
            json!({"data_type":"counter"}),
            json!({"data_type":"u16"}),
            json!({"scale":2}),
            json!({"offset":1}),
            json!({"elements":0}),
        ] {
            let mut value = base.clone();
            for (key, item) in invalid.as_object().unwrap() {
                value[key].clone_from(item);
            }
            let source = signal(value);
            assert_eq!(
                classify(&signal_addresses(&[source]).unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        for cap in [4, 5] {
            let source =
                json!({"address":"127.0.0.1:1","max_read_bytes":cap,"signals":[base.clone()]});
            let config: Config = serde_json::from_value(source.clone()).unwrap();
            assert_eq!(
                classify(&open(&config).map(drop).unwrap_err()).kind,
                ErrorKind::Usage
            );
            let config: super::super::csp::Config = serde_json::from_value(source).unwrap();
            assert_eq!(
                classify(&super::super::csp::open(&config).map(drop).unwrap_err()).kind,
                ErrorKind::Usage
            );
            let config: super::super::df1::Config = serde_json::from_value(json!({"device":"must-not-open","destination_node":4,"max_read_bytes":cap,"signals":[base.clone()]})).unwrap();
            assert_eq!(
                classify(&super::super::df1::open(&config).map(drop).unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        let mut config: Config = serde_json::from_value(
            json!({"address":"127.0.0.1:1","serial_number":1,"signals":[base]}),
        )
        .unwrap();
        config.profile = Profile::Plc5;
        assert_eq!(
            classify(&open(&config).map(drop).unwrap_err()).kind,
            ErrorKind::Usage
        );
    }

    #[test]
    fn six_byte_chunk_counts_advance_native_elements_and_preserve_exact_json_charge() {
        for (cap, expected) in [
            (220, vec![(0, 216), (36, 12)]),
            (224, vec![(0, 222), (37, 6)]),
        ] {
            let signals = [signal(
                json!({"name":"timers","address":"T4:0","elements":38}),
            )];
            let addresses = signal_addresses(&signals).unwrap();
            let mut calls = Vec::new();
            let readings = read_signals(
                &signals,
                &addresses,
                cap,
                Instant::now() + Duration::from_secs(2),
                |_, element, bytes, data| {
                    calls.push((element, bytes));
                    data.resize(data.len() + bytes, 0);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(calls, expected);
            assert_eq!(readings[0].value.as_array().unwrap().len(), 38);
            let mut charged = SizeLimit { bytes: 2 };
            begin(&signals[0], &mut charged).unwrap();
            let value = super::decode(
                &signals[0],
                addresses[0],
                &[0; 228],
                &mut charged,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            let reading = super::super::Reading {
                signal: "timers".into(),
                value,
                source_time: None,
            };
            assert_eq!(
                charged.bytes,
                3 + serde_json::to_vec(&reading).unwrap().len()
            );
        }
        let source = signal(json!({"name":"exact","address":"C5:7","elements":2}));
        let parsed = address(&source).unwrap();
        let bytes = [1, 0xa4, 0, 0x80, 0xff, 0x7f, 2, 0x58, 0xff, 0xff, 0, 0];
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut measured = SizeLimit { bytes: 2 };
        begin(&source, &mut measured).unwrap();
        super::decode(&source, parsed, &bytes, &mut measured, deadline).unwrap();
        let cost = measured.bytes - 2;
        let mut exact = SizeLimit {
            bytes: MAX_BATCH - cost,
        };
        begin(&source, &mut exact).unwrap();
        super::decode(&source, parsed, &bytes, &mut exact, deadline).unwrap();
        assert_eq!(exact.bytes, MAX_BATCH);
        let mut short = SizeLimit {
            bytes: MAX_BATCH - cost + 1,
        };
        begin(&source, &mut short).unwrap();
        assert_eq!(
            classify(&super::decode(&source, parsed, &bytes, &mut short, deadline).unwrap_err())
                .kind,
            ErrorKind::Incomplete
        );
        for kind in [DataType::Timer, DataType::Counter] {
            assert!(
                serde_json::to_vec(&native(kind, [0; 6]).unwrap())
                    .unwrap()
                    .len()
                    >= MIN_VALUE_BYTES
            );
        }
    }

    #[test]
    fn failed_poll_retires_direct_and_csp_without_a_second_connection_attempt() {
        for name in ["N7:0", "T4:0"] {
            let config: Config = serde_json::from_value(json!({"address":"127.0.0.1:1","serial_number":1,"signals":[{"name":"value","address":name}]})).unwrap();
            let mut direct = open(&config).unwrap();
            assert_eq!(
                classify(&direct.read(Duration::ZERO).unwrap_err()).kind,
                ErrorKind::Timeout
            );
            assert_eq!(
                classify(&direct.read(Duration::from_secs(1)).unwrap_err()).kind,
                ErrorKind::Disconnected
            );
            let config: super::super::csp::Config = serde_json::from_value(
                json!({"address":"127.0.0.1:1","signals":[{"name":"value","address":name}]}),
            )
            .unwrap();
            let mut csp = super::super::csp::open(&config).unwrap();
            assert_eq!(
                classify(&csp.read(Duration::ZERO).unwrap_err()).kind,
                ErrorKind::Timeout
            );
            assert_eq!(
                classify(&csp.read(Duration::from_secs(1)).unwrap_err()).kind,
                ErrorKind::Disconnected
            );
        }
    }

    #[test]
    fn structured_budget_and_original_deadline_stop_before_next_query() {
        let signals: Vec<_> = (0..256)
            .map(|i| signal(json!({"name":format!("timers{i}"),"address":"T4:0","elements":256})))
            .collect();
        let addresses = signal_addresses(&signals).unwrap();
        let mut calls = 0;
        let error = read_signals(
            &signals,
            &addresses,
            224,
            Instant::now() + Duration::from_secs(2),
            |_, _, bytes, data| {
                calls += 1;
                data.resize(data.len() + bytes, 0);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
        assert!(calls > 0 && calls < 256 * 7);
        let one = [signal(json!({"name":"timer","address":"T4:0"}))];
        let mut size = SizeLimit {
            bytes: MAX_BATCH - 10,
        };
        assert_eq!(
            classify(&begin(&one[0], &mut size).unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        let addresses = signal_addresses(&one).unwrap();
        let mut queries = 0;
        let error = read_signals(&one, &addresses, 6, Instant::now(), |_, _, _, _| {
            queries += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Timeout);
        assert_eq!(queries, 0);
    }
}
