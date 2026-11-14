//! Frozen independent literals (tests/fixtures/gopxl) against the grammar and reader.
use super::*;
use serde_json::Value;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/gopxl/ascii-measurement.json"
    ))
    .unwrap()
}
fn bytes(hex: &str) -> Vec<u8> {
    let (pairs, tail) = hex.as_bytes().as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn reason(error: &anyhow::Error) -> String {
    crate::types::classify(error)
        .details
        .and_then(|details| details["reason"].as_str().map(str::to_owned))
        .unwrap_or_default()
}
/// The fixture's base configuration with one case's framing and requested IDs.
fn config(case: &Value) -> Config {
    let mut value = corpus()["base_config"].clone();
    for key in [
        "delimiter",
        "terminator",
        "invalid_token",
        "trailing_delimiter",
    ] {
        if let Some(field) = case.get(key) {
            value[key] = field.clone();
        }
    }
    if let Some(ids) = case.get("ids").and_then(Value::as_array) {
        value["measurements"] = json!(
            ids.iter()
                .enumerate()
                .map(|(index, id)| json!({"name":format!("m{index}"),"id":id}))
                .collect::<Vec<_>>()
        );
    }
    if let Some(map) = value.as_object_mut() {
        map.remove("protocol");
    }
    serde_json::from_value(value).unwrap()
}

#[derive(Default)]
struct Wire {
    output: Vec<u8>,
}
/// Delivers the peer bytes in fixed splits; then end of stream or a quiet peer.
struct Memory {
    input: io::Cursor<Vec<u8>>,
    splits: Vec<usize>,
    close: bool,
    wire: Arc<Mutex<Wire>>,
}
impl io::Read for Memory {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let limit = if self.splits.is_empty() {
            target.len()
        } else {
            self.splits.remove(0).min(target.len())
        };
        let count = self.input.read(target.split_at_mut(limit).0)?;
        if count == 0 && !self.close {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(count)
    }
}
impl Write for Memory {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.wire
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .output
            .extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl net::Transport for Memory {
    fn set_deadline(&mut self, _deadline: Instant) {}
}

fn fake(
    config: &Config,
    input: Vec<u8>,
    splits: Vec<usize>,
    close: bool,
) -> (Client, Arc<Mutex<Wire>>) {
    let wire = Arc::new(Mutex::new(Wire::default()));
    let client = Client {
        config: config.clone(),
        owner: Some(Box::new(Memory {
            input: io::Cursor::new(input),
            splits,
            close,
            wire: wire.clone(),
        })),
        retired: false,
        polls: 0,
        observed: None,
        cutoff: None,
    };
    (client, wire)
}

fn check_ok(name: &str, readings: &[Reading], case: &Value) {
    let expected = case["values"].as_array().unwrap();
    assert_eq!(readings.len(), expected.len(), "{name}");
    for (reading, expected) in readings.iter().zip(expected) {
        assert_eq!(reading.source_time, None);
        assert_eq!(
            reading.value["trailing_delimiter_observed"], case["trailing_delimiter_observed"],
            "{name}"
        );
        for (key, value) in expected.as_object().unwrap() {
            match key.as_str() {
                // The exact decimal is a JSON number whose text must match the oracle.
                "value" => assert_eq!(
                    reading.value["value"].to_string(),
                    value.as_str().unwrap(),
                    "{name}"
                ),
                "raw_value_text_length" => assert_eq!(
                    reading.value["raw_value_text"].as_str().map(str::len),
                    value
                        .as_u64()
                        .and_then(|length| usize::try_from(length).ok()),
                    "{name}"
                ),
                _ => assert_eq!(&reading.value[key], value, "{name}: {key}"),
            }
        }
        for key in ["value", "scaled_i32"] {
            if expected.get(key).is_none() {
                assert_eq!(reading.value.get(key), None, "{name}: {key} must be absent");
            }
        }
        assert_eq!(
            reading.value["units_provenance"],
            "externally_configured_unqueried"
        );
    }
}

fn run(case: &Value, wire: Vec<u8>, splits: Vec<usize>) {
    let name = case["name"].as_str().unwrap().to_owned();
    let config = config(case);
    let close = case["after_wire"] == "close";
    let (mut client, written) = fake(&config, wire, splits, close);
    let result = client.read(Duration::from_secs(5));
    assert_eq!(
        written.lock().unwrap().output,
        codec::request(&config),
        "{name}"
    );
    if case["expected"] == "ok" {
        let readings = result.unwrap_or_else(|error| panic!("{name}: {error:#}"));
        check_ok(&name, &readings, case);
        assert!(client.observed_ns().is_some());
    } else {
        let error = result.err().unwrap_or_else(|| panic!("{name}: accepted"));
        assert_eq!(
            reason(&error),
            case["expected_error"].as_str().unwrap(),
            "{name}: {error:#}"
        );
        if case["expected_error"] == "device_error" {
            let details = crate::types::classify(&error).details.unwrap();
            assert_eq!(
                details["gopxl_reply"]["classification"], case["classification"],
                "{name}"
            );
            assert_eq!(
                details["gopxl_reply"]["error_text"], case["error_text"],
                "{name}"
            );
        }
        assert_eq!(client.observed_ns(), None, "{name}");
        // Retired: no retry, reconnect or second request.
        let before = written.lock().unwrap().output.len();
        client.read(Duration::from_secs(1)).unwrap_err();
        assert_eq!(written.lock().unwrap().output.len(), before, "{name}");
    }
}

#[test]
fn requests_match_manufacturer_and_chosen_literals() {
    for case in corpus()["requests"].as_array().unwrap() {
        assert_eq!(
            codec::request(&config(case)),
            bytes(case["wire_hex"].as_str().unwrap()),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn every_reply_literal_has_its_frozen_outcome() {
    let corpus = corpus();
    let replies = corpus["replies"].as_array().unwrap();
    assert!(replies.len() >= 90);
    for case in replies {
        run(case, bytes(case["wire_hex"].as_str().unwrap()), Vec::new());
    }
}

#[test]
fn split_replies_parse_identically() {
    let corpus = corpus();
    let chunking = &corpus["chunkings"];
    let reference = corpus["replies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == chunking["expected_reply"])
        .unwrap()
        .clone();
    for splits in chunking["splits"].as_array().unwrap() {
        let splits = splits
            .as_array()
            .unwrap()
            .iter()
            .map(|size| usize::try_from(size.as_u64().unwrap()).unwrap())
            .collect();
        run(
            &reference,
            bytes(chunking["wire_hex"].as_str().unwrap()),
            splits,
        );
    }
}

#[test]
fn size_bound_counts_body_bytes_only() {
    let corpus = corpus();
    for case in corpus["size_limits"]["cases"].as_array().unwrap() {
        let fill = case["fill_char"]
            .as_str()
            .unwrap()
            .repeat(usize::try_from(case["fill_count"].as_u64().unwrap()).unwrap());
        let wire = format!(
            "{}{fill}{}",
            case["prefix_text"].as_str().unwrap(),
            case["suffix_text"].as_str().unwrap()
        )
        .into_bytes();
        assert_eq!(
            u64::try_from(wire.len()).ok(),
            case["wire_bytes"].as_u64(),
            "{}",
            case["name"]
        );
        run(case, wire, Vec::new());
    }
}

#[test]
fn configuration_literals_are_accepted_or_rejected() {
    let corpus = corpus();
    let apply = |case: &Value| {
        let mut value = corpus["base_config"].clone();
        let field = case["field"].as_str().unwrap();
        if case["remove"] == true {
            value.as_object_mut().unwrap().remove(field);
        } else {
            value[field] = case["value"].clone();
        }
        value
    };
    for case in corpus["config_accepts"].as_array().unwrap() {
        let mut value = apply(case);
        value.as_object_mut().unwrap().remove("protocol");
        serde_json::from_value::<Config>(value)
            .unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
    }
    for case in corpus["config_rejects"].as_array().unwrap() {
        let value = apply(case);
        // The protocol key selects the adapter; it is checked by the outer dispatch.
        if case["field"] == "protocol" {
            let config = json!({"version":1,"sources":[{"id":"s"}]});
            let mut source = value.clone();
            source["id"] = json!("s");
            let mut whole = config;
            whole["sources"] = json!([source]);
            serde_json::from_value::<super::super::Config>(whole).unwrap_err();
            continue;
        }
        let mut value = value;
        value.as_object_mut().unwrap().remove("protocol");
        let result = serde_json::from_value::<Config>(value);
        assert!(result.is_err(), "{} accepted", case["name"]);
    }
}

#[test]
fn only_the_measurement_command_is_ever_sent() {
    let corpus = corpus();
    let request = codec::request(&config(&corpus["base_config"]));
    for command in corpus["forbidden_commands"].as_array().unwrap() {
        let command = command.as_str().unwrap().as_bytes();
        assert!(!request.starts_with(command), "{command:?}");
    }
    assert!(request.starts_with(b"measurement,"));
}

#[test]
fn cutoff_is_exclusive_and_decimals_are_exact() {
    let corpus = corpus();
    let case = corpus["replies"]
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .clone();
    let config = config(&case);
    let (mut client, _) = fake(
        &config,
        bytes(case["wire_hex"].as_str().unwrap()),
        Vec::new(),
        false,
    );
    client.set_acquisition_end(crate::host::monotonic_ns());
    let error = client.read(Duration::from_secs(5)).unwrap_err();
    assert_eq!(reason(&error), "acquisition_cutoff");
    for (scaled, text) in [
        (0, "0.000"),
        (1, "0.001"),
        (-1, "-0.001"),
        (-5000, "-5.000"),
        (i32::MAX, "2147483.647"),
        (i32::MIN, "-2147483.648"),
    ] {
        assert_eq!(codec::decimal(scaled), text);
    }
}
