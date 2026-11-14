//! Frozen independent literals (tests/fixtures/optommp) against the codec, model and poll.
use super::*;
use model::{Model, Resource};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

fn corpus() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/optommp/groov-inputs.json"
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
fn rows(corpus: &Value, group: &str) -> Vec<Value> {
    corpus[group].as_array().unwrap().clone()
}
fn text(value: &Value) -> &str {
    value.as_str().unwrap()
}
fn number(value: &Value) -> u64 {
    value.as_u64().unwrap()
}
fn label(value: &Value) -> u8 {
    u8::try_from(number(value)).unwrap()
}
fn address(value: &Value) -> u64 {
    u64::from_str_radix(text(value), 16).unwrap()
}
fn prefix_of(hex: &str) -> [u8; codec::PREFIX] {
    bytes(hex)[..codec::PREFIX].try_into().unwrap()
}
fn reason(error: &anyhow::Error) -> String {
    crate::types::classify(error)
        .details
        .and_then(|details| details["reason"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[derive(Default)]
struct Wire {
    output: Vec<u8>,
    consumed: usize,
}
/// Delivers the peer bytes in fixed chunks, then end of stream.
struct Memory {
    input: io::Cursor<Vec<u8>>,
    chunks: Vec<usize>,
    wire: Arc<Mutex<Wire>>,
}
impl io::Read for Memory {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        let limit = if self.chunks.is_empty() {
            target.len()
        } else {
            self.chunks.remove(0).min(target.len())
        };
        let count = self.input.read(target.split_at_mut(limit).0)?;
        self.wire
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .consumed += count;
        Ok(count)
    }
}
impl Write for Memory {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.wire
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?
            .output
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl net::Transport for Memory {
    fn set_deadline(&mut self, _deadline: Instant) {}
}

fn config(model: &Value, channels: &Value) -> Config {
    serde_json::from_value(json!({
        "profile":"groov_expanded_inputs","model":model,
        "address":"127.0.0.1:2001","channels":channels
    }))
    .unwrap()
}
fn fake(
    config: &Config,
    label: u8,
    input: Vec<u8>,
    chunks: Vec<usize>,
) -> (Client, Arc<Mutex<Wire>>) {
    let wire = Arc::new(Mutex::new(Wire::default()));
    let mut source = open(config).unwrap();
    drop(source.finish(Duration::ZERO));
    let addresses = config
        .channels
        .iter()
        .map(|channel| {
            model::addresses(
                config.model,
                channel.module,
                channel.channel,
                channel.resource,
            )
            .unwrap()
        })
        .collect();
    let client = Client {
        config: config.clone(),
        addresses,
        owner: Some(Box::new(Memory {
            input: io::Cursor::new(input),
            chunks,
            wire: wire.clone(),
        })),
        label,
        retired: false,
        polls: 0,
        observed: None,
        cutoff: None,
    };
    (client, wire)
}

#[test]
fn published_manufacturer_control_round_trips() {
    let corpus = corpus();
    let control = &corpus["published_control"];
    let request = codec::request(label(&control["label"]), address(&control["address"])).unwrap();
    assert_eq!(request.as_slice(), bytes(text(&control["request_hex"])));
    let response = bytes(text(&control["response_hex"]));
    let prefix = codec::prefix(&prefix_of(text(&control["response_hex"])), 1).unwrap();
    assert_eq!(prefix.response_code, 0);
    assert_eq!(
        response.get(12..),
        Some(bytes(text(&control["word_hex"])).as_slice())
    );
    let word = u32::from_str_radix(text(&control["word_hex"]), 16).unwrap();
    assert_eq!(
        codec::value(Resource::AnalogEu, word)["value"].as_f64(),
        control["f32"].as_f64()
    );
}

#[test]
fn request_frames_status_and_address_arithmetic_match_literals() {
    let corpus = corpus();
    for row in rows(&corpus, "request_frames") {
        let request = codec::request(label(&row["label"]), address(&row["address"])).unwrap();
        assert_eq!(
            request.as_slice(),
            bytes(text(&row["wire_hex"])),
            "{}",
            row["name"]
        );
    }
    let status = &corpus["status_addresses"];
    assert_eq!(model::STATUS_UNIT_TYPE, address(&status["unit_type"]));
    assert_eq!(model::STATUS_PUC, address(&status["puc_flag"]));
    for (model, name) in [
        (Model::EpicPr1, "epic_pr1"),
        (Model::EpicPr2, "epic_pr2"),
        (Model::RioMm1, "rio_mm1"),
        (Model::RioMm2, "rio_mm2"),
    ] {
        assert_eq!(model.name(), name);
        assert_eq!(
            u64::from(model.unit_type()),
            address(&corpus["unit_types"][name])
        );
    }
    for row in rows(&corpus, "addresses") {
        let resource = if row["resource"] == "analog_eu" {
            Resource::AnalogEu
        } else {
            Resource::DigitalState
        };
        let module = u32::try_from(number(&row["module"])).unwrap();
        let channel = u32::try_from(number(&row["channel"])).unwrap();
        let roles = model::addresses(Model::EpicPr1, module, channel, resource).unwrap();
        assert_eq!(
            [
                roles.module_id,
                roles.channel_type,
                roles.value,
                roles.quality
            ],
            [
                address(&row["module_id_address"]),
                address(&row["channel_type_address"]),
                address(&row["value_address"]),
                address(&row["quality_address"]),
            ],
            "{}",
            row["name"]
        );
    }
}

#[test]
fn response_prefixes_preserve_opaque_source_ids_and_native_codes() {
    let corpus = corpus();
    for row in rows(&corpus, "valid_responses") {
        let wire = bytes(text(&row["wire_hex"]));
        let prefix =
            codec::prefix(&prefix_of(text(&row["wire_hex"])), label(&row["label"])).unwrap();
        assert_eq!(u64::from(prefix.source_id), number(&row["source_id"]));
        assert_eq!(prefix.response_code, 0);
        assert_eq!(
            wire.get(12..),
            Some(bytes(text(&row["word_hex"])).as_slice())
        );
    }
    let naks = rows(&corpus, "nak_prefixes");
    assert_eq!(naks.len(), 15);
    for row in naks {
        let header = prefix_of(text(&row["wire_hex"]));
        assert_eq!(bytes(text(&row["wire_hex"])).len(), 12);
        let prefix = codec::prefix(&header, label(&row["label"])).unwrap();
        assert_eq!(
            u64::from(prefix.response_code),
            number(&row["response_code"])
        );
        assert_eq!(u64::from(prefix.source_id), number(&row["source_id"]));
        let error = codec::native_error(&header, prefix, label(&row["label"]), 0xFFFF_F030_0020);
        let details = crate::types::classify(&error).details.unwrap();
        assert_eq!(
            details["optommp_response"]["raw_header_hex"],
            row["wire_hex"]
        );
        assert_eq!(
            details["optommp_response"]["response_code"],
            row["response_code"]
        );
        assert_eq!(
            details["optommp_response"]["requested_address"],
            "fffff0300020"
        );
    }
    for row in rows(&corpus, "malformed_responses") {
        let wire = bytes(text(&row["wire_hex"]));
        let header: [u8; 12] = wire[..12].try_into().unwrap();
        let error = codec::prefix(&header, label(&row["expected_label"])).unwrap_err();
        let reason = reason(&error);
        assert!(
            matches!(reason.as_str(), "malformed_response" | "label_mismatch"),
            "{}: {reason}",
            row["name"]
        );
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    }
}

#[test]
fn words_preserve_ieee_and_unsigned_bits_without_narrowing() {
    for row in rows(&corpus(), "words") {
        let word = u32::from_str_radix(text(&row["word_hex"]), 16).unwrap();
        if row["resource"] == "analog_eu" {
            let value = codec::value(Resource::AnalogEu, word);
            assert_eq!(
                value["classification"], row["classification"],
                "{}",
                row["name"]
            );
            assert_eq!(value["encoding"], "ieee754_binary32_be");
            match row.get("value") {
                Some(expected) => {
                    let actual = value["value"].as_f64().unwrap();
                    let expected = expected.as_f64().unwrap();
                    assert_eq!(actual.to_bits(), expected.to_bits(), "{}", row["name"]);
                }
                None => assert_eq!(value.get("value"), None, "{}", row["name"]),
            }
        } else {
            let value = codec::value(Resource::DigitalState, word);
            assert_eq!(value["value"], row["value"], "{}", row["name"]);
            assert_eq!(value["encoding"], "unsigned32_be");
        }
    }
}

fn model_of(value: &Value) -> Model {
    match text(value) {
        "epic_pr1" => Model::EpicPr1,
        "epic_pr2" => Model::EpicPr2,
        "rio_mm1" => Model::RioMm1,
        "rio_mm2" => Model::RioMm2,
        other => panic!("unknown fixture model {other}"),
    }
}

#[test]
fn admission_table_matches_every_literal_accept_and_reject() {
    let corpus = corpus();
    for (group, expected) in [("accept", true), ("reject", false)] {
        for row in corpus["admission"][group].as_array().unwrap() {
            let resource = if row["resource"] == "analog_eu" {
                Resource::AnalogEu
            } else {
                Resource::DigitalState
            };
            let module = u32::try_from(number(&row["module"])).unwrap();
            let channel = u32::try_from(number(&row["channel"])).unwrap();
            let id = u32::from_str_radix(text(&row["module_id_hex"]), 16).unwrap();
            let kind = u32::from_str_radix(text(&row["channel_type_hex"]), 16).unwrap();
            let model = model_of(&row["model"]);
            assert_eq!(
                model::allows_tuple(model, module, channel, resource, id, kind)
                    && model::addresses(model, module, channel, resource).is_some(),
                expected,
                "{}",
                row["name"]
            );
        }
    }
}

#[test]
fn no_admitted_selector_or_encodable_request_reaches_a_forbidden_address() {
    let corpus = corpus();
    for row in rows(&corpus, "forbidden_addresses") {
        let forbidden = address(&row["address"]);
        let models = row["models"].as_array().unwrap();
        if models.iter().any(|model| text(model).starts_with("epic")) {
            assert!(!model::readable(forbidden), "{}", row["name"]);
            codec::request(1, forbidden).unwrap_err();
        }
        for model in models.iter().map(model_of) {
            for module in 0..16 {
                for channel in 0..24 {
                    for resource in [Resource::AnalogEu, Resource::DigitalState] {
                        if let Some(roles) = model::addresses(model, module, channel, resource) {
                            assert_ne!(roles.value, forbidden, "{}", row["name"]);
                            assert_ne!(roles.quality, forbidden, "{}", row["name"]);
                            if model::plausible(model, module, channel) {
                                assert!(model::readable(roles.value));
                                assert!(model::readable(roles.quality));
                                assert!(model::readable(roles.module_id));
                                assert!(model::readable(roles.channel_type));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn poll_scenarios_match_exact_wire_order_outcome_and_values() {
    for scenario in rows(&corpus(), "poll_scenarios") {
        let name = text(&scenario["name"]).to_owned();
        let config = config(&scenario["model"], &scenario["channels"]);
        let exchanges = scenario["exchanges"].as_array().unwrap();
        let input = exchanges
            .iter()
            .flat_map(|exchange| bytes(text(&exchange["response_hex"])))
            .collect();
        let requests = exchanges
            .iter()
            .flat_map(|exchange| bytes(text(&exchange["request_hex"])))
            .collect::<Vec<_>>();
        let (mut client, wire) = fake(&config, label(&scenario["first_label"]), input, Vec::new());
        let result = client.read(Duration::from_secs(5));
        assert_eq!(
            wire.lock().unwrap().output,
            requests,
            "{name}: request order"
        );
        if scenario["expected"] == "ok" {
            let readings = result.unwrap();
            let expected = scenario["expected_values"].as_array().unwrap();
            assert_eq!(readings.len(), expected.len(), "{name}");
            for (reading, expected) in readings.iter().zip(expected) {
                assert_eq!(reading.signal, text(&expected["channel"]), "{name}");
                assert_eq!(reading.source_time, None);
                for (key, value) in expected.as_object().unwrap() {
                    if key == "channel" {
                        continue;
                    }
                    if key == "negative_zero" {
                        // Oracle annotation: the contract keeps signed zero in the raw word.
                        assert_eq!(
                            Some(reading.value["raw_hex"] == "80000000"),
                            value.as_bool(),
                            "{name}"
                        );
                        continue;
                    }
                    if key == "value" && value.is_f64() {
                        assert_eq!(
                            reading.value[key].as_f64().map(f64::to_bits),
                            value.as_f64().map(f64::to_bits),
                            "{name}: {key}"
                        );
                    } else {
                        assert_eq!(&reading.value[key], value, "{name}: {key}");
                    }
                }
                assert_eq!(
                    reading.value["units_provenance"],
                    "externally_configured_unqueried"
                );
                assert_eq!(reading.value["poll_ordinal"], 1);
                let clocks = [
                    number(&reading.value["host_value_observed_ns"]),
                    number(&reading.value["host_quality_observed_ns"]),
                    number(&reading.value["poll_observed_ns"]),
                ];
                assert!(clocks[0] <= clocks[1] && clocks[1] <= clocks[2], "{name}");
                assert_eq!(client.observed_ns(), Some(clocks[2]));
            }
            assert!(client.ready());
        } else {
            let error = result.unwrap_err();
            assert_eq!(reason(&error), text(&scenario["expected_error"]), "{name}");
            if let Some(detail) = scenario.get("expected_error_detail") {
                let details = crate::types::classify(&error).details.unwrap();
                for (key, value) in detail.as_object().unwrap() {
                    assert_eq!(&details["optommp_response"][key], value, "{name}: {key}");
                }
            }
            assert_eq!(client.observed_ns(), None, "{name}: withheld");
            // Retired: no retry, reconnect or further request.
            let before = wire.lock().unwrap().output.len();
            client.read(Duration::from_secs(1)).unwrap_err();
            assert_eq!(wire.lock().unwrap().output.len(), before, "{name}");
        }
    }
}

#[test]
fn fragmented_and_partial_streams_never_read_past_the_expected_frame() {
    let corpus = corpus();
    let stream = &corpus["stream_controls"];
    let frame = bytes(text(&stream["ack_frame_hex"]));
    let one = json!([{"name":"a","module":0,"channel":0,"resource":"analog_eu",
        "expected_module_id":0xF000_0022_u32,"expected_channel_type":0x6000_0015_u32}]);
    let config = config(&json!("rio_mm1"), &one);
    for chunks in stream["chunkings"].as_array().unwrap() {
        let chunks = chunks
            .as_array()
            .unwrap()
            .iter()
            .map(|size| usize::try_from(number(size)).unwrap())
            .collect();
        let (mut client, wire) = fake(
            &config,
            label(&stream["expected_label"]),
            frame.clone(),
            chunks,
        );
        let word = client
            .exchange(0xFFFF_F026_0040, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            format!("{:08x}", word.word),
            text(&stream["expected_word_hex"])
        );
        assert_eq!(wire.lock().unwrap().consumed, 16);
    }
    let tail = &stream["nak_with_unseen_tail"];
    let (mut client, wire) = fake(
        &config,
        label(&tail["expected_label"]),
        bytes(text(&tail["wire_hex"])),
        Vec::new(),
    );
    let error = client
        .exchange(0xFFFF_F026_0040, Instant::now() + Duration::from_secs(5))
        .unwrap_err();
    let details = crate::types::classify(&error).details.unwrap();
    assert_eq!(
        details["optommp_response"]["raw_header_hex"],
        tail["raw_header_hex"]
    );
    assert_eq!(
        details["optommp_response"]["response_code"],
        tail["response_code"]
    );
    assert_eq!(
        u64::try_from(wire.lock().unwrap().consumed).unwrap(),
        number(&tail["consume_bytes"])
    );
    for row in rows(&corpus, "partial_frames") {
        let (mut client, _) = fake(
            &config,
            label(&row["expected_label"]),
            bytes(text(&row["wire_hex"])),
            Vec::new(),
        );
        let error = client
            .exchange(0xFFFF_F026_0040, Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(reason(&error), "truncated_response", "{}", row["name"]);
    }
}

#[test]
fn prior_poll_is_kept_and_next_poll_continues_labels_then_withholds() {
    let corpus = corpus();
    let scenario = rows(&corpus, "poll_scenarios").remove(0);
    let lifecycle = rows(&corpus, "lifecycle_controls")
        .into_iter()
        .find(|row| row["name"] == "prior poll retention")
        .unwrap();
    let config = config(&scenario["model"], &scenario["channels"]);
    let input = scenario["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|exchange| bytes(text(&exchange["response_hex"])))
        .collect();
    let (mut client, wire) = fake(&config, 1, input, Vec::new());
    let first = client.read(Duration::from_secs(5)).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(client.label, label(&lifecycle["poll_n_plus_1_first_label"]));
    let length = wire.lock().unwrap().output.len();
    // The peer is now silent: the next poll sends exactly one request, then fails whole.
    client.read(Duration::from_secs(5)).unwrap_err();
    assert_eq!(
        wire.lock().unwrap().output.get(length..),
        Some(bytes(text(&lifecycle["poll_n_plus_1_first_request_hex"])).as_slice())
    );
    assert_eq!(client.observed_ns(), None);
}

#[test]
fn cutoff_is_exclusive_and_expired_deadline_sends_nothing() {
    let corpus = corpus();
    let scenario = rows(&corpus, "poll_scenarios").remove(0);
    let config = config(&scenario["model"], &scenario["channels"]);
    let input: Vec<u8> = scenario["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|exchange| bytes(text(&exchange["response_hex"])))
        .collect();
    let (mut client, _) = fake(&config, 1, input.clone(), Vec::new());
    client.set_acquisition_end(crate::host::monotonic_ns());
    let error = client.read(Duration::from_secs(5)).unwrap_err();
    assert_eq!(reason(&error), "acquisition_cutoff");
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    let (mut client, wire) = fake(&config, 1, input, Vec::new());
    let error = client.read(Duration::ZERO).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert_eq!(wire.lock().unwrap().output, Vec::<u8>::new());
}

#[test]
fn every_request_is_a_read_quadlet() {
    for scenario in rows(&corpus(), "poll_scenarios") {
        for exchange in scenario["exchanges"].as_array().unwrap() {
            let request = bytes(text(&exchange["request_hex"]));
            assert_eq!(request.get(3), Some(&0x40));
        }
    }
}
