use super::{
    Config, MAX_ROW, Measurement, TerminalConfig,
    config::{
        ColdJunction, CurrentExcitation, CurrentShunt, MixedChannel, ResistanceConfig, RtdType,
        TemperatureUnits, Timing,
    },
    wire::TimingDescriptor,
    wire::{self, Block, Descriptor, Failure, Notice, Request, Response},
};
use crate::types::{Categorized, ErrorKind};
use serde_json::{Value, json};

fn config() -> Config {
    Config {
        signal: "voltage".into(),
        measurement: Measurement::Voltage {
            channels: vec!["Dev1/ai0".into(), "Dev1/ai1".into()],
            terminal_config: TerminalConfig::Differential,
            min_volts: -10.0,
            max_volts: 10.0,
        },
        timing: Timing::Continuous {
            rate_hz: 1000.0,
            max_samples_per_channel: 4096,
            host_buffer_samples_per_channel: 8192,
        },
        cleanup_timeout_ms: 30_000,
        library: None,
    }
}
fn descriptor(c: &Config) -> Descriptor {
    Descriptor::for_config(c, c.physical_channels(), Some(800.0)).unwrap()
}
fn clocked(c: &mut Config) -> (&mut f64, &mut u32, &mut u32) {
    let Timing::Continuous {
        rate_hz,
        max_samples_per_channel,
        host_buffer_samples_per_channel,
    } = &mut c.timing
    else {
        panic!("expected continuous timing")
    };
    (
        rate_hz,
        max_samples_per_channel,
        host_buffer_samples_per_channel,
    )
}
fn clocked_rates(d: &mut Descriptor) -> (&mut f64, &mut f64) {
    let TimingDescriptor::Continuous {
        requested_rate_hz,
        nominal_rate_hz,
        ..
    } = &mut d.timing
    else {
        panic!("expected continuous timing")
    };
    (requested_rate_hz, nominal_rate_hz)
}
fn voltage_channels(c: &mut Config) -> &mut Vec<String> {
    let Measurement::Voltage { channels, .. } = &mut c.measurement else {
        panic!("expected voltage")
    };
    channels
}
fn voltage_range(c: &mut Config) -> (&mut f64, &mut f64) {
    let Measurement::Voltage {
        min_volts,
        max_volts,
        ..
    } = &mut c.measurement
    else {
        panic!("expected voltage")
    };
    (min_volts, max_volts)
}

fn oracle() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/abi.json")).unwrap()
}
fn notice() -> Notice {
    Notice {
        operation: "DAQmxReadAnalogF64".into(),
        status_code: 12_345,
        diagnostic_hex: "7761726e".into(),
        diagnostic_truncated: false,
        diagnostic_source: "error_string".into(),
        diagnostic_encoding: "locale_bytes".into(),
        diagnostic_lookup_status: 0,
    }
}

#[test]
fn physical_channel_and_buffer_bounds_precede_driver_effects() {
    let good = config();
    good.validate().unwrap();
    for bad in [
        "Dev1/ai0:63",
        "Dev1/ai0,Dev1/ai1",
        "Dev1/*",
        "MAXchannel",
        "Dev1/ao0",
        "/Dev1/ai0",
        "Dev1/ai0\0",
        "dévice/ai0",
        "Dev1/ai4294967296",
        "Dev1/ai1/extra",
    ] {
        let mut c = good.clone();
        voltage_channels(&mut c)[0] = bad.into();
        c.validate().expect_err(bad);
    }
    for channels in [
        vec![],
        vec!["Dev1/ai0".into(), "dev1/ai00".into()],
        (0..65).map(|n| format!("Dev1/ai{n}")).collect(),
    ] {
        let mut c = good.clone();
        *voltage_channels(&mut c) = channels;
        c.validate().unwrap_err();
    }
    let mut c = good.clone();
    *voltage_channels(&mut c) = (0..64).map(|n| format!("cDAQ1Mod1/ai{n}")).collect();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    *clocked(&mut c).2 += 1;
    c.validate().unwrap_err();
    for n in [0, 1_048_577] {
        let mut c = good.clone();
        *clocked(&mut c).2 = n;
        c.validate().unwrap_err();
    }
    for n in [0, 4097] {
        let mut c = good.clone();
        *clocked(&mut c).1 = n;
        c.validate().unwrap_err();
    }
}

#[test]
fn numeric_config_is_exact_for_counts_and_rejects_invalid_voltage_ranges() {
    let mut value = serde_json::to_value(config()).unwrap();
    value["max_samples_per_channel"] = serde_json::from_str("4.096e3").unwrap();
    let c: Config = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(c.max_samples_per_channel(), 4096);
    value["max_samples_per_channel"] = serde_json::from_str("1.00000000000000001").unwrap();
    serde_json::from_value::<Config>(value).unwrap_err();
    for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut c = config();
        *clocked(&mut c).0 = rate;
        c.validate().unwrap_err();
    }
    let mut c = config();
    {
        let (min, max) = voltage_range(&mut c);
        *min = *max;
    }
    c.validate().unwrap_err();
    *voltage_range(&mut c).0 = f64::NEG_INFINITY;
    c.validate().unwrap_err();
    let mut c = config();
    c.library = Some("relative-driver.so".into());
    c.validate().unwrap_err();
    let mut v = serde_json::to_value(config()).unwrap();
    v["clock_source"] = json!("/Dev1/PFI0");
    serde_json::from_value::<Config>(v).unwrap_err();
}

#[test]
fn actual_driver_count_order_and_clock_are_required() {
    let c = config();
    let mut d = descriptor(&c);
    d.validate(&c).unwrap();
    d.channels.reverse();
    d.validate(&c).unwrap_err();
    d = descriptor(&c);
    d.channels.pop();
    d.validate(&c).unwrap_err();
    d = descriptor(&c);
    d.channels[0] = "dev1/ai00".into();
    d.validate(&c).unwrap();
    *clocked_rates(&mut d).1 = f64::NAN;
    d.validate(&c).unwrap_err();
    d = descriptor(&c);
    *clocked_rates(&mut d).0 = 999.0;
    d.validate(&c).unwrap_err();
    d = descriptor(&c);
    if let Measurement::Voltage { min_volts, .. } = &mut d.measurement {
        *min_volts = -5.0;
    }
    d.validate(&c).unwrap_err();
    let mut c = config();
    voltage_channels(&mut c).clear();
    d.channels.clear();
    d.validate(&c).unwrap_err();
}

#[test]
fn independent_manual_ieee_patterns_survive_scan_blocks_and_json_roundtrip() {
    let c = config();
    let d = descriptor(&c);
    let o = oracle();
    let b = Block {
        sample_index: 0,
        samples_per_channel: 6,
        bits_le_hex: o["two_channel_six_scan_sequence"]["all_words_le_hex"]
            .as_str()
            .unwrap()
            .into(),
        status_code: 12_345,
    };
    let reading = wire::block_reading(&c, &d, &b, &[notice()]).unwrap();
    assert!(reading.source_time.is_none());
    let restored: super::super::Reading =
        serde_json::from_slice(&serde_json::to_vec(&reading).unwrap()).unwrap();
    let values = restored.value["samples"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|scan| scan.as_array().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 12);
    for (actual, expected) in values.iter().zip(o["patterns"].as_array().unwrap()) {
        assert_eq!(actual["ieee754_le_hex"], expected["ieee754_le_hex"]);
        if let Some(decimal) = expected["finite_decimal"].as_str() {
            assert_eq!(
                actual["volts"].as_f64().unwrap().to_bits(),
                decimal.parse::<f64>().unwrap().to_bits()
            );
        } else {
            assert!(actual.get("volts").is_none());
        }
    }
    assert_eq!(restored.value["status_code"], 12_345);
    assert_eq!(restored.value["nominal_rate_hz"], 800.0);
    assert_eq!(restored.value["notices"][0]["status_code"], 12_345);
}

#[test]
fn block_shape_hex_and_index_checks_reject_unqualified_samples() {
    let c = config();
    let d = descriptor(&c);
    let good = Block {
        sample_index: 0,
        samples_per_channel: 1,
        bits_le_hex: "000000000000f03f00000000000004c0".into(),
        status_code: 0,
    };
    wire::block_reading(&c, &d, &good, &[]).unwrap();
    for count in [0, 4097, u32::MAX] {
        let mut b = good.clone();
        b.samples_per_channel = count;
        wire::block_reading(&c, &d, &b, &[]).unwrap_err();
    }
    for bytes in [
        "",
        "000000000000F03f00000000000004c0",
        "z00000000000f03f00000000000004c0",
    ] {
        let mut b = good.clone();
        b.bits_le_hex = bytes.into();
        wire::block_reading(&c, &d, &b, &[]).unwrap_err();
    }
    let mut b = good;
    b.sample_index = u64::MAX;
    wire::block_reading(&c, &d, &b, &[]).unwrap_err();
}

#[test]
fn maximum_matrix_and_notices_fit_the_declared_row_budget() {
    let mut c = config();
    *voltage_channels(&mut c) = (0..64)
        .map(|n| format!("{}Dev/ai{n}", "D".repeat(220)))
        .collect();
    let d = descriptor(&c);
    let mut n = notice();
    n.diagnostic_hex = "ff".repeat(256);
    let notices = vec![n; wire::MAX_NOTICES];
    let b = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffef7f".repeat(4096),
        status_code: 12_345,
    };
    let r = wire::block_reading(&c, &d, &b, &notices).unwrap();
    assert!(serde_json::to_vec(&r).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &d, "configured", &notices).unwrap();
    let mut excessive = notices;
    excessive.push(notice());
    wire::validate_notices(&excessive).unwrap_err();
    let mut n = notice();
    n.status_code = 0;
    wire::validate_notices(&[n]).unwrap_err();
}

#[test]
fn private_frames_preserve_large_clocks_and_reject_version_size_and_truncation() {
    let requests = [
        Request::Open {
            config: config(),
            deadline_ns: u64::MAX - 1,
            cutoff_ns: u64::MAX,
        },
        Request::Read {
            deadline_ns: u64::MAX - 1,
            cutoff_ns: u64::MAX,
        },
        Request::Finish {
            deadline_ns: u64::MAX,
        },
    ];
    for request in requests {
        let mut bytes = Vec::new();
        wire::write(&mut bytes, &request).unwrap();
        let round: Request = wire::read(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::to_value(round).unwrap()
        );
        wire::read::<Request>(&mut &bytes[..bytes.len() - 1]).unwrap_err();
    }
    assert!(wire::read::<Request>(&mut &b""[..]).unwrap().is_none());
    wire::read::<Request>(&mut &(u32::try_from(MAX_ROW + 1).unwrap().to_le_bytes())[..])
        .unwrap_err();
    for value in [
        json!({"protocol":2,"body":{"finish":{"deadline_ns":1}}}),
        json!({"protocol":wire::PROTOCOL,"body":{"finish":{"deadline_ns":1,"unexpected":0}}}),
    ] {
        let b = serde_json::to_vec(&value).unwrap();
        let mut framed = (u32::try_from(b.len()).unwrap()).to_le_bytes().to_vec();
        framed.extend(b);
        wire::read::<Request>(&mut framed.as_slice()).unwrap_err();
    }
}

#[test]
fn original_native_status_and_typed_failure_cross_private_protocol() {
    let error = Categorized::new(ErrorKind::Incomplete, "native acquisition loss")
        .details(json!({"native_status":-200_279,"operation":"DAQmxReadAnalogF64"}))
        .into();
    let response = Response::Failed {
        error: Failure::from_error(&error),
    };
    let mut framed = Vec::new();
    wire::write(&mut framed, &response).unwrap();
    let Some(Response::Failed { error }) = wire::read::<Response>(&mut framed.as_slice()).unwrap()
    else {
        panic!("wrong response")
    };
    let classified = crate::types::classify(&error.error());
    assert_eq!(classified.kind, ErrorKind::Incomplete);
    assert_eq!(classified.details.unwrap()["native_status"], -200_279);
    assert!(wire::now().unwrap() > 0);
    assert_eq!(
        crate::types::classify(&wire::check_deadline(0).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(
        crate::types::classify(
            &wire::check_deadline(wire::now().unwrap() + 32_000_000_000).unwrap_err()
        )
        .kind,
        ErrorKind::Usage
    );
}

#[test]
fn cleanup_evidence_keeps_the_original_acquisition_cause() {
    let first = Categorized::new(ErrorKind::Timeout, "native partial read")
        .details(json!({"native_status":-200_284}))
        .into();
    let cleanup = Categorized::new(ErrorKind::OperationFailed, "native StopTask failed")
        .details(json!({"native_status":-990_201}))
        .into();
    let error =
        super::super::combine_source_results(Err(first), Err(cleanup), "finish").unwrap_err();
    let c = crate::types::classify(&error);
    assert_eq!(c.kind, ErrorKind::Timeout);
    let d = c.details.unwrap();
    assert_eq!(d["native_status"], -200_284);
    assert_eq!(
        d["source_finish_error"]["tag"]["details"]["native_status"],
        -990_201
    );
    assert!(!serde_json::to_string(&d).unwrap().contains(":null"));
}

fn thermocouple() -> Config {
    serde_json::from_value(json!({
        "signal":"temperature", "measurement":"thermocouple", "rate_hz":10.0,
        "max_samples_per_channel":4096,"host_buffer_samples_per_channel":8192,
        "channels":[
            {"physical":"Dev1/ai0","thermocouple_type":"k","units":"deg_c",
             "min_temperature":-50.0,"max_temperature":500.0,"cjc":{"source":"built_in"}},
            {"physical":"Dev1/ai1","thermocouple_type":"t","units":"deg_f",
             "min_temperature":-50.0,"max_temperature":300.0,"cjc":{"source":"constant","value":77.0}}
        ]
    })).unwrap()
}
fn tc_channels(c: &mut Config) -> &mut Vec<super::config::ThermocoupleChannel> {
    let Measurement::Thermocouple { channels } = &mut c.measurement else {
        panic!("expected thermocouple")
    };
    channels
}
fn tc_oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/nidaqmx/thermocouple.json"
    ))
    .unwrap()
}

#[test]
fn legacy_voltage_and_explicit_measurement_requests_remain_compatible() {
    let voltage = config();
    let mut legacy = serde_json::to_value(&voltage).unwrap();
    assert_eq!(
        legacy
            .as_object_mut()
            .unwrap()
            .remove("measurement")
            .unwrap(),
        "voltage"
    );
    let parsed: Config = serde_json::from_value(legacy).unwrap();
    parsed.validate().unwrap();
    assert!(parsed.measurement.same_request(&voltage.measurement));
    let c = thermocouple();
    c.validate().unwrap();
    let restored: Config = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
    assert!(restored.measurement.same_request(&c.measurement));
    assert_eq!(c.cleanup_timeout_ms, 30_000);
}

#[test]
fn thermocouple_decimal_requests_cross_private_frames_without_accepting_duplicate_keys() {
    let mut value = serde_json::to_value(thermocouple()).unwrap();
    value["channels"][0]["min_temperature"] = serde_json::from_str("-5.0e1").unwrap();
    value["channels"][1]["max_temperature"] = serde_json::from_str("3.00e2").unwrap();
    value["channels"][1]["cjc"]["value"] = serde_json::from_str("7.7e1").unwrap();
    value["max_samples_per_channel"] = serde_json::from_str("4.096e3").unwrap();
    let raw = serde_json::to_string(&value).unwrap();
    let direct: Config = serde_json::from_str(&raw).unwrap();
    direct.validate().unwrap();
    let buffered: Config = serde_json::from_value(value).unwrap();
    assert!(direct.measurement.same_request(&buffered.measurement));
    let response = Response::Ready {
        descriptor: descriptor(&direct),
        observed_ns: u64::MAX - 1,
        notices: Vec::new(),
    };
    let mut bytes = Vec::new();
    wire::write(&mut bytes, &response).unwrap();
    let Some(Response::Ready {
        descriptor,
        observed_ns,
        ..
    }) = wire::read::<Response>(&mut bytes.as_slice()).unwrap()
    else {
        panic!("expected ready")
    };
    assert_eq!(observed_ns, u64::MAX - 1);
    descriptor.validate(&direct).unwrap();
    for (needle, replacement) in [
        (
            "\"units\":\"deg_c\"",
            "\"units\":\"deg_c\",\"units\":\"deg_f\"",
        ),
        (
            "\"source\":\"built_in\"",
            "\"source\":\"built_in\",\"source\":\"constant\"",
        ),
        (
            "\"signal\":\"temperature\"",
            "\"signal\":\"temperature\",\"signal\":\"other\"",
        ),
    ] {
        assert!(raw.contains(needle));
        serde_json::from_str::<Config>(&raw.replace(needle, replacement)).unwrap_err();
    }
}

#[test]
fn thermocouple_config_rejects_mixed_shapes_unknown_fields_and_unbounded_requests() {
    let good = serde_json::to_value(thermocouple()).unwrap();
    let mut invalid = Vec::new();
    for field in ["terminal_config", "min_volts", "max_volts"] {
        let mut v = good.clone();
        v[field] = Value::Null;
        invalid.push(v);
        let mut v = good.clone();
        v[field] = json!(0);
        invalid.push(v);
    }
    for (field, value) in [
        ("units", json!("celsius")),
        ("thermocouple_type", json!("unknown")),
        ("terminal_config", json!("auto_detect")),
        ("cjc", json!({"source":"external","channel":"MAXalias"})),
        ("cjc", json!({"source":"built_in","value":25})),
        ("cjc", json!({"source":"constant"})),
        (
            "cjc",
            json!({"source":"constant","value":77,"channel":"other"}),
        ),
        ("min_volts", json!(-10)),
        ("min_temperature", Value::Null),
    ] {
        let mut v = good.clone();
        v["channels"][0][field] = value;
        invalid.push(v);
    }
    let mut mixed = good.clone();
    mixed["channels"][1] = json!("Dev1/ai1");
    invalid.push(mixed);
    let mut untagged = good.clone();
    untagged.as_object_mut().unwrap().remove("measurement");
    invalid.push(untagged);
    let mut unknown = good;
    unknown["measurement"] = json!("custom_temperature");
    invalid.push(unknown);
    for v in invalid {
        serde_json::from_value::<Config>(v).expect_err("invalid TC request must fail");
    }
    for names in [
        vec![],
        vec!["Dev1/ai0", "dev1/ai00"],
        vec!["Dev1/ai0:1", "Dev1/ai1"],
    ] {
        let mut c = thermocouple();
        let template = tc_channels(&mut c)[0].clone();
        *tc_channels(&mut c) = names
            .iter()
            .map(|name| {
                let mut channel = template.clone();
                channel.physical = (*name).into();
                channel
            })
            .collect();
        c.validate().unwrap_err();
    }
    let mut c = thermocouple();
    tc_channels(&mut c)[0].cjc = ColdJunction::Constant {
        value: f64::INFINITY,
    };
    c.validate().unwrap_err();
    let mut c = thermocouple();
    tc_channels(&mut c)[0].max_temperature = -50.0;
    c.validate().unwrap_err();
    let mut c = thermocouple();
    tc_channels(&mut c)[0].min_temperature = f64::NAN;
    c.validate().unwrap_err();
}

#[test]
fn thermocouple_descriptor_preserves_each_requested_type_unit_range_and_cjc_bits() {
    use super::config::{ColdJunction, TemperatureUnits, ThermocoupleType};
    let c = thermocouple();
    let d = descriptor(&c);
    d.validate(&c).unwrap();
    let mut altered = Vec::new();
    let mut changed = c.clone();
    tc_channels(&mut changed)[0].thermocouple_type = ThermocoupleType::J;
    altered.push(changed);
    let mut changed = c.clone();
    tc_channels(&mut changed)[0].units = TemperatureUnits::DegF;
    altered.push(changed);
    let mut changed = c.clone();
    tc_channels(&mut changed)[0].min_temperature = -49.0;
    altered.push(changed);
    let mut changed = c.clone();
    tc_channels(&mut changed)[0].terminal_config = Some(TerminalConfig::Rse);
    altered.push(changed);
    let mut changed = c.clone();
    tc_channels(&mut changed)[1].cjc = ColdJunction::Constant { value: 25.0 };
    altered.push(changed);
    for changed in altered {
        let mut bad = d.clone();
        bad.measurement = changed.measurement;
        bad.validate(&c).unwrap_err();
    }
    let mut positive = c.clone();
    tc_channels(&mut positive)[1].cjc = ColdJunction::Constant { value: 0.0 };
    let mut negative = positive.clone();
    tc_channels(&mut negative)[1].cjc = ColdJunction::Constant { value: -0.0 };
    assert!(!positive.measurement.same_request(&negative.measurement));
    let mut wrong_order = d;
    wrong_order.channels.reverse();
    wrong_order.validate(&c).unwrap_err();
}

#[test]
fn thermocouple_manual_f64_words_keep_declared_units_without_voltage_or_conversion() {
    let c = thermocouple();
    let d = descriptor(&c);
    let oracle = tc_oracle();
    let block = Block {
        sample_index: 0,
        samples_per_channel: 6,
        bits_le_hex: oracle["two_channel_six_scan_sequence"]["all_words_le_hex"]
            .as_str()
            .unwrap()
            .into(),
        status_code: 0,
    };
    let reading = wire::block_reading(&c, &d, &block, &[]).unwrap();
    let restored: super::super::Reading =
        serde_json::from_slice(&serde_json::to_vec(&reading).unwrap()).unwrap();
    assert!(restored.source_time.is_none());
    assert_eq!(restored.value["event"], "thermocouple_temperature_block");
    assert_eq!(
        restored.value["representation"],
        "driver_scaled_f64_temperature"
    );
    assert_eq!(restored.value["channel_measurements"][0]["units"], "deg_c");
    assert_eq!(restored.value["channel_measurements"][1]["units"], "deg_f");
    assert!(
        restored.value["channel_measurements"][0]["cjc"]
            .get("value")
            .is_none()
    );
    assert_eq!(
        restored.value["channel_measurements"][1]["cjc"]["value"],
        77.0
    );
    let words = restored.value["samples"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|scan| scan.as_array().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(words.len(), 12);
    for (actual, expected) in words
        .iter()
        .zip(oracle["manual_sample_patterns"].as_array().unwrap())
    {
        assert!(actual.get("volts").is_none());
        assert_eq!(actual["ieee754_le_hex"], expected["ieee754_le_hex"]);
        if let Some(decimal) = expected["finite_decimal"].as_str() {
            assert_eq!(
                actual["temperature"].as_f64().unwrap().to_bits(),
                decimal.parse::<f64>().unwrap().to_bits()
            );
        } else {
            assert!(actual.get("temperature").is_none());
        }
    }
    assert_eq!(words[0]["temperature"], 25.0);
    assert_eq!(words[1]["temperature"], 25.0);
    let mut c = c;
    tc_channels(&mut c)[0].terminal_config = Some(TerminalConfig::Default);
    tc_channels(&mut c)[1].terminal_config = Some(TerminalConfig::Differential);
    let event = wire::event_reading(&c, &descriptor(&c), "configured", &[]).unwrap();
    assert_eq!(
        event.value["channel_measurements"][0]["terminal_config_provenance"],
        "driver_default_unqueried"
    );
    assert_eq!(
        event.value["channel_measurements"][1]["terminal_config_provenance"],
        "explicit_request_not_read_back"
    );
}

#[test]
fn maximum_thermocouple_matrix_metadata_and_notices_fit_the_same_row_budget() {
    use super::config::ColdJunction;
    let mut c = thermocouple();
    let template = tc_channels(&mut c)[1].clone();
    *tc_channels(&mut c) = (0..64)
        .map(|n| {
            let mut channel = template.clone();
            channel.physical = format!("{}D/ai{n}", "D".repeat(248));
            channel.min_temperature = -f64::MAX;
            channel.max_temperature = f64::MAX;
            channel.cjc = ColdJunction::Constant { value: -f64::MAX };
            channel.terminal_config = Some(TerminalConfig::Pseudodifferential);
            channel
        })
        .collect();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    n.diagnostic_source = "extended_error_info".into();
    n.diagnostic_lookup_status = i32::MIN;
    let notices = vec![n; wire::MAX_NOTICES];
    let b = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let r = wire::block_reading(&c, &descriptor(&c), &b, &notices).unwrap();
    assert!(serde_json::to_vec(&r).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &descriptor(&c), "configured", &notices).unwrap();
}

fn current() -> Config {
    serde_json::from_value(json!({"signal":"current","measurement":"current","rate_hz":1000.0,
        "max_samples_per_channel":4096,"host_buffer_samples_per_channel":8192,"channels":[
            {"physical":"Dev1/ai0","terminal_config":"differential","min_amperes":0.0,"max_amperes":0.02,
             "shunt":{"source":"built_in"}},
            {"physical":"Dev1/ai1","terminal_config":"rse","min_amperes":-0.02,"max_amperes":0.02,
             "shunt":{"source":"external","ohms":250.0}}
        ]})).unwrap()
}
fn current_channels(c: &mut Config) -> &mut Vec<super::config::CurrentChannel> {
    let Measurement::Current { channels } = &mut c.measurement else {
        panic!("expected current")
    };
    channels
}
fn current_oracle() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/current.json")).unwrap()
}

fn software(mut c: Config) -> Config {
    c.timing = Timing::OnDemand {
        poll_interval_ms: 250,
    };
    c
}

#[test]
fn on_demand_configuration_requires_an_exact_interval_and_excludes_continuous_fields() {
    let good = serde_json::to_value(software(config())).unwrap();
    for interval in [1, 250, 3_600_000] {
        let mut value = good.clone();
        value["poll_interval_ms"] = json!(interval);
        serde_json::from_value::<Config>(value)
            .unwrap()
            .validate()
            .unwrap();
    }
    for interval in [
        json!(0),
        json!(3_600_001),
        json!(-1),
        json!(null),
        json!("250"),
        json!(1.5),
    ] {
        let mut value = good.clone();
        value["poll_interval_ms"] = interval;
        if let Ok(c) = serde_json::from_value::<Config>(value) {
            c.validate().unwrap_err();
        }
    }
    for key in [
        "rate_hz",
        "max_samples_per_channel",
        "host_buffer_samples_per_channel",
    ] {
        for extra in [json!(1), json!(null)] {
            let mut value = good.clone();
            value[key] = extra;
            serde_json::from_value::<Config>(value).unwrap_err();
        }
    }
    let mut missing = good;
    missing.as_object_mut().unwrap().remove("poll_interval_ms");
    serde_json::from_value::<Config>(missing).unwrap_err();
    let mut continuous = serde_json::to_value(config()).unwrap();
    continuous.as_object_mut().unwrap().remove("timing");
    serde_json::from_value::<Config>(continuous.clone())
        .unwrap()
        .validate()
        .unwrap();
    continuous["poll_interval_ms"] = json!(null);
    serde_json::from_value::<Config>(continuous).unwrap_err();
}

#[test]
fn on_demand_decimal_intervals_and_descriptors_round_trip_without_number_coercion() {
    let text = serde_json::to_string(&software(thermocouple()))
        .unwrap()
        .replace("\"poll_interval_ms\":250", "\"poll_interval_ms\":2.5e2");
    let c: Config = serde_json::from_str(&text).unwrap();
    c.validate().unwrap();
    for bad in ["250.00000000000000001", "2.5000000000000000001e2", "null"] {
        serde_json::from_str::<Config>(&text.replace("2.5e2", bad)).unwrap_err();
    }
    serde_json::from_str::<Config>(
        &text.replace("\"timing\":", "\"timing\":\"on_demand\",\"timing\":"),
    )
    .unwrap_err();
    let descriptor = Descriptor::for_config(&c, c.physical_channels(), None).unwrap();
    let text = serde_json::to_string(&descriptor)
        .unwrap()
        .replace("\"poll_interval_ms\":250", "\"poll_interval_ms\":2.5e2");
    let restored: Descriptor = serde_json::from_str(&text).unwrap();
    restored.validate(&c).unwrap();
    let mut framed = Vec::new();
    wire::write(
        &mut framed,
        &Response::Ready {
            descriptor: restored,
            observed_ns: 1,
            notices: Vec::new(),
        },
    )
    .unwrap();
    let Some(Response::Ready { descriptor, .. }) =
        wire::read::<Response>(&mut framed.as_slice()).unwrap()
    else {
        panic!("wrong response")
    };
    descriptor.validate(&c).unwrap();
}

#[test]
fn on_demand_descriptors_bind_mode_interval_channels_and_measurement() {
    let c = software(config());
    Descriptor::for_config(&c, c.physical_channels(), Some(1000.0)).unwrap_err();
    let mut d = Descriptor::for_config(&c, c.physical_channels(), None).unwrap();
    d.validate(&config()).unwrap_err();
    d.timing = TimingDescriptor::OnDemand {
        poll_interval_ms: 251,
    };
    d.validate(&c).unwrap_err();
    d.timing = TimingDescriptor::OnDemand {
        poll_interval_ms: 250,
    };
    d.channels.reverse();
    d.validate(&c).unwrap_err();
    Descriptor::for_config(&config(), config().physical_channels(), None).unwrap_err();
}

#[test]
fn on_demand_rows_keep_quantity_words_without_implying_a_sample_clock() {
    for (c, quantity) in [
        (software(config()), "volts"),
        (software(thermocouple()), "temperature"),
        (software(current()), "amperes"),
    ] {
        let d = Descriptor::for_config(&c, c.physical_channels(), None).unwrap();
        let b = Block {
            sample_index: 0,
            samples_per_channel: 1,
            bits_le_hex: "0000000000000080341200000000f87f".into(),
            status_code: 0,
        };
        let row = wire::block_reading(&c, &d, &b, &[]).unwrap();
        assert_eq!(
            row.value["samples"][0][0][quantity]
                .as_f64()
                .unwrap()
                .to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(
            row.value["samples"][0][1]["ieee754_le_hex"],
            "341200000000f87f"
        );
        assert!(row.value["samples"][0][1].get(quantity).is_none());
        for row in [
            row,
            wire::event_reading(&c, &d, "configured", &[]).unwrap(),
            wire::event_reading(&c, &d, "finished", &[]).unwrap(),
        ] {
            assert!(row.source_time.is_none());
            assert_eq!(row.value["clock"], "software_on_demand_host_completion");
            assert_eq!(
                row.value["timing"],
                json!({"mode":"on_demand","poll_interval_ms":250,"basis":"host_software_request_spacing"})
            );
            for key in [
                "requested_rate_hz",
                "nominal_rate_hz",
                "host_buffer_samples_per_channel",
            ] {
                assert!(row.value.get(key).is_none());
            }
        }
    }
}

#[test]
fn on_demand_sample_blocks_are_limited_to_one_scan() {
    let c = software(config());
    let d = Descriptor::for_config(&c, c.physical_channels(), None).unwrap();
    for count in [0, 2] {
        let b = Block {
            sample_index: 0,
            samples_per_channel: count,
            bits_le_hex: "0000000000000000".repeat(usize::try_from(count).unwrap() * 2),
            status_code: 0,
        };
        wire::block_reading(&c, &d, &b, &[]).unwrap_err();
    }
}

#[test]
fn current_requires_explicit_typed_shunts_ranges_and_terminals_before_driver_effects() {
    let c = current();
    c.validate().unwrap();
    let good = serde_json::to_value(&c).unwrap();
    let mut invalid = Vec::new();
    for (field, value) in [
        ("shunt", json!({"source":"default"})),
        ("shunt", json!({"source":"built_in","ohms":249.0})),
        ("shunt", json!({"source":"built_in","ohms":null})),
        ("shunt", json!({"source":"external"})),
        ("shunt", json!({"source":"external","ohms":null})),
        ("shunt", json!({"source":"external","ohms":250,"scale":1})),
        ("terminal_config", json!("auto_detect")),
        ("units", json!("milliamps")),
        ("cjc", json!({"source":"built_in"})),
        ("min_volts", json!(-10)),
    ] {
        let mut changed = good.clone();
        changed["channels"][0][field] = value;
        invalid.push(changed);
    }
    for (field, value) in [
        ("terminal_config", json!("rse")),
        ("min_volts", json!(0)),
        ("max_volts", Value::Null),
    ] {
        let mut changed = good.clone();
        changed[field] = value;
        invalid.push(changed);
    }
    let mut missing = good.clone();
    missing["channels"][0]
        .as_object_mut()
        .unwrap()
        .remove("terminal_config");
    invalid.push(missing);
    let mut mixed = good;
    mixed["channels"][1] = json!("Dev1/ai1");
    invalid.push(mixed);
    for value in invalid {
        serde_json::from_value::<Config>(value)
            .expect_err("closed current config must reject invalid fields");
    }
    for ohms in [0.0, -0.0, -1.0, f64::INFINITY, f64::NAN] {
        let mut c = current();
        current_channels(&mut c)[1].shunt = CurrentShunt::External { ohms };
        c.validate().unwrap_err();
    }
    let mut c = current();
    current_channels(&mut c)[0].min_amperes = 0.02;
    c.validate().unwrap_err();
    let mut c = current();
    current_channels(&mut c)[1].physical = "dev1/ai00".into();
    c.validate().unwrap_err();
    let mut c = current();
    current_channels(&mut c).clear();
    c.validate().unwrap_err();
}

#[test]
fn current_decimal_requests_and_descriptors_preserve_numbers_and_reject_duplicate_keys() {
    let mut value = serde_json::to_value(current()).unwrap();
    value["channels"][0]["max_amperes"] = serde_json::from_str("2.0e-2").unwrap();
    value["channels"][1]["min_amperes"] = serde_json::from_str("-2e-2").unwrap();
    value["channels"][1]["shunt"]["ohms"] = serde_json::from_str("2.50e2").unwrap();
    value["max_samples_per_channel"] = serde_json::from_str("4.096e3").unwrap();
    let raw = serde_json::to_string(&value).unwrap();
    let direct: Config = serde_json::from_str(&raw).unwrap();
    direct.validate().unwrap();
    let buffered: Config = serde_json::from_value(value).unwrap();
    assert!(direct.measurement.same_request(&buffered.measurement));
    let response = Response::Ready {
        descriptor: descriptor(&direct),
        observed_ns: u64::MAX - 1,
        notices: Vec::new(),
    };
    let mut bytes = Vec::new();
    wire::write(&mut bytes, &response).unwrap();
    let Some(Response::Ready {
        descriptor,
        observed_ns,
        ..
    }) = wire::read::<Response>(&mut bytes.as_slice()).unwrap()
    else {
        panic!("expected ready")
    };
    assert_eq!(observed_ns, u64::MAX - 1);
    descriptor.validate(&direct).unwrap();
    for (needle, replacement) in [
        (
            "\"source\":\"external\"",
            "\"source\":\"external\",\"source\":\"built_in\"",
        ),
        (
            "\"terminal_config\":\"rse\"",
            "\"terminal_config\":\"rse\",\"terminal_config\":\"default\"",
        ),
    ] {
        assert!(raw.contains(needle));
        serde_json::from_str::<Config>(&raw.replace(needle, replacement)).unwrap_err();
    }
}

#[test]
fn current_descriptor_binds_exact_shunt_range_terminal_and_actual_channel_order() {
    use super::config::CurrentShunt;
    let c = current();
    let d = descriptor(&c);
    d.validate(&c).unwrap();
    let mut altered = Vec::new();
    let mut changed = c.clone();
    current_channels(&mut changed)[1].shunt = CurrentShunt::External { ohms: 249.0 };
    altered.push(changed);
    let mut changed = c.clone();
    current_channels(&mut changed)[0].shunt = CurrentShunt::External { ohms: 249.0 };
    altered.push(changed);
    let mut changed = c.clone();
    current_channels(&mut changed)[0].terminal_config = TerminalConfig::Default;
    altered.push(changed);
    let mut changed = c.clone();
    current_channels(&mut changed)[0].min_amperes = -0.0;
    altered.push(changed);
    for changed in altered {
        let mut bad = d.clone();
        bad.measurement = changed.measurement;
        bad.validate(&c).unwrap_err();
    }
    let mut reversed = d;
    reversed.channels.reverse();
    reversed.validate(&c).unwrap_err();
}

#[test]
fn current_manual_words_remain_amperes_without_shunt_or_milliamp_conversion() {
    let c = current();
    let d = descriptor(&c);
    let oracle = current_oracle();
    let block = Block {
        sample_index: 0,
        samples_per_channel: 6,
        bits_le_hex: oracle["two_channel_six_scan_sequence"]["all_words_le_hex"]
            .as_str()
            .unwrap()
            .into(),
        status_code: 0,
    };
    let reading = wire::block_reading(&c, &d, &block, &[]).unwrap();
    let restored: super::super::Reading =
        serde_json::from_slice(&serde_json::to_vec(&reading).unwrap()).unwrap();
    assert!(restored.source_time.is_none());
    assert_eq!(restored.value["event"], "analog_current_block");
    assert_eq!(
        restored.value["representation"],
        "driver_scaled_f64_current"
    );
    assert_eq!(restored.value["channel_measurements"][0]["units"], "amps");
    assert!(
        restored.value["channel_measurements"][0]["shunt"]
            .get("ohms")
            .is_none()
    );
    assert_eq!(
        restored.value["channel_measurements"][1]["shunt"]["ohms"],
        250.0
    );
    let words = restored.value["samples"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|scan| scan.as_array().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(words.len(), 12);
    for (actual, expected) in words
        .iter()
        .zip(oracle["manual_patterns"].as_array().unwrap())
    {
        assert!(actual.get("volts").is_none());
        assert!(actual.get("temperature").is_none());
        assert_eq!(actual["ieee754_le_hex"], expected["ieee754_le_hex"]);
        if let Some(decimal) = expected["finite_amperes_decimal"].as_str() {
            assert_eq!(
                actual["amperes"].as_f64().unwrap().to_bits(),
                decimal.parse::<f64>().unwrap().to_bits()
            );
        } else {
            assert!(actual.get("amperes").is_none());
        }
    }
    assert_eq!(words[0]["amperes"], 0.015_625);
    assert_eq!(words[1]["amperes"], 0.015_625);
    assert_eq!(words[2]["amperes"], 0.004);
    assert_eq!(words[3]["amperes"], 0.020);
    let mut c = c;
    current_channels(&mut c)[0].terminal_config = TerminalConfig::Default;
    let event = wire::event_reading(&c, &descriptor(&c), "configured", &[]).unwrap();
    assert_eq!(
        event.value["channel_measurements"][0]["terminal_config_provenance"],
        "driver_default_unqueried"
    );
}

#[test]
fn maximum_current_matrix_metadata_and_notices_fit_the_same_row_budget() {
    let mut c = current();
    let template = current_channels(&mut c)[1].clone();
    *current_channels(&mut c) = (0..64)
        .map(|n| {
            let mut channel = template.clone();
            channel.physical = format!("{}D/ai{n}", "D".repeat(248));
            channel.min_amperes = -f64::MAX;
            channel.max_amperes = f64::MAX;
            channel.shunt = CurrentShunt::External { ohms: f64::MAX };
            channel.terminal_config = TerminalConfig::Pseudodifferential;
            channel
        })
        .collect();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    n.diagnostic_source = "extended_error_info".into();
    n.diagnostic_lookup_status = i32::MIN;
    let notices = vec![n; wire::MAX_NOTICES];
    let block = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let reading = wire::block_reading(&c, &descriptor(&c), &block, &notices).unwrap();
    assert!(serde_json::to_vec(&reading).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &descriptor(&c), "configured", &notices).unwrap();
}

fn rtd() -> Config {
    serde_json::from_value(
        json!({"signal":"rtd_temperature","measurement":"rtd","rate_hz":1000.0,
        "max_samples_per_channel":4096,"host_buffer_samples_per_channel":8192,"channels":[
            {"physical":"Dev1/ai0","rtd_type":"pt3851","units":"deg_c","min_temperature":-50.0,
             "max_temperature":500.0,"resistance_config":"three_wire","r0_ohms":100.0,
             "excitation":{"source":"internal","amperes":0.000_976_562_5}},
            {"physical":"Dev1/ai1","rtd_type":"pt3928","units":"deg_f","min_temperature":-40.0,
             "max_temperature":300.0,"resistance_config":"four_wire","r0_ohms":1000.0,
             "excitation":{"source":"external","amperes":0.001_953_125}}
        ]}),
    )
    .unwrap()
}
fn rtd_channels(c: &mut Config) -> &mut Vec<super::config::RtdChannel> {
    let Measurement::Rtd { channels } = &mut c.measurement else {
        panic!("expected RTD")
    };
    channels
}
fn rtd_oracle() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/nidaqmx/rtd.json")).unwrap()
}

#[test]
fn rtd_requires_closed_explicit_sensor_wiring_reference_and_excitation_requests() {
    let good = serde_json::to_value(rtd()).unwrap();
    for key in [
        "physical",
        "rtd_type",
        "units",
        "min_temperature",
        "max_temperature",
        "resistance_config",
        "r0_ohms",
        "excitation",
    ] {
        let mut missing = good.clone();
        missing["channels"][0].as_object_mut().unwrap().remove(key);
        serde_json::from_value::<Config>(missing).unwrap_err();
        let mut null = good.clone();
        null["channels"][0][key] = Value::Null;
        serde_json::from_value::<Config>(null).unwrap_err();
    }
    for (key, value) in [
        ("rtd_type", json!("custom")),
        ("units", json!("ohms")),
        ("resistance_config", json!("automatic")),
        ("terminal_config", json!(null)),
        ("cjc", json!({"source":"built_in"})),
        ("shunt", json!(null)),
        ("coefficients", json!([])),
        ("teds", json!(true)),
        ("custom_scale", json!("scale")),
        ("excitation", json!({"source":"none","amperes":0.001})),
        ("excitation", json!({"source":"internal"})),
        ("excitation", json!({"source":"external","amperes":null})),
        (
            "excitation",
            json!({"source":"internal","amperes":0.001,"volts":1.0}),
        ),
    ] {
        let mut value_config = good.clone();
        value_config["channels"][0][key] = value;
        serde_json::from_value::<Config>(value_config).unwrap_err();
    }
    for key in ["terminal_config", "min_volts", "max_volts"] {
        let mut value = good.clone();
        value[key] = Value::Null;
        serde_json::from_value::<Config>(value).unwrap_err();
    }
    let mut mixed = good;
    mixed["channels"][1] = json!("Dev1/ai1");
    serde_json::from_value::<Config>(mixed).unwrap_err();
}

#[test]
fn rtd_ranges_reference_and_excitation_are_finite_without_inferred_device_defaults() {
    rtd().validate().unwrap();
    for value in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut c = rtd();
        rtd_channels(&mut c)[0].r0_ohms = value;
        c.validate().unwrap_err();
        for internal in [true, false] {
            let mut c = rtd();
            rtd_channels(&mut c)[0].excitation = if internal {
                CurrentExcitation::Internal { amperes: value }
            } else {
                CurrentExcitation::External { amperes: value }
            };
            c.validate().unwrap_err();
        }
    }
    for value in [500.0, f64::NAN, f64::NEG_INFINITY, f64::INFINITY] {
        let mut c = rtd();
        rtd_channels(&mut c)[0].min_temperature = value;
        c.validate().unwrap_err();
    }
    for names in [
        vec![],
        vec!["Dev1/ai0", "dev1/ai00"],
        vec!["Dev1/ai0:2"],
        vec!["MAXchannel"],
    ] {
        let mut c = rtd();
        let template = rtd_channels(&mut c)[0].clone();
        *rtd_channels(&mut c) = names
            .iter()
            .map(|name| {
                let mut channel = template.clone();
                channel.physical = (*name).into();
                channel
            })
            .collect();
        c.validate().unwrap_err();
    }
}

#[test]
fn rtd_decimal_requests_and_private_frames_preserve_float_bits_and_reject_duplicates() {
    let text = serde_json::to_string(&rtd())
        .unwrap()
        .replace("0.0009765625", "9.765625e-4")
        .replace("\"r0_ohms\":100.0", "\"r0_ohms\":1e2");
    let direct: Config = serde_json::from_str(&text).unwrap();
    direct.validate().unwrap();
    let buffered: Config = serde_json::from_value(serde_json::from_str(&text).unwrap()).unwrap();
    assert!(direct.measurement.same_request(&buffered.measurement));
    for duplicated in [
        text.replace(
            "\"amperes\":9.765625e-4",
            "\"amperes\":9.765625e-4,\"amperes\":1e-3",
        ),
        text.replace("\"r0_ohms\":1e2", "\"r0_ohms\":1e2,\"r0_ohms\":101"),
        text.replace(
            "\"source\":\"internal\"",
            "\"source\":\"internal\",\"source\":\"external\"",
        ),
    ] {
        serde_json::from_str::<Config>(&duplicated).unwrap_err();
    }
    for c in [direct, software(rtd())] {
        let d = Descriptor::for_config(
            &c,
            c.physical_channels(),
            c.timing.poll_interval_ns().is_none().then_some(800.0),
        )
        .unwrap();
        let mut frame = Vec::new();
        wire::write(
            &mut frame,
            &Response::Ready {
                descriptor: d,
                observed_ns: 1,
                notices: Vec::new(),
            },
        )
        .unwrap();
        let Some(Response::Ready {
            descriptor: restored,
            ..
        }) = wire::read::<Response>(&mut frame.as_slice()).unwrap()
        else {
            panic!("wrong response")
        };
        restored.validate(&c).unwrap();
    }
}

#[test]
fn rtd_descriptors_bind_each_requested_field_and_exact_float_bits() {
    let mut original = rtd();
    rtd_channels(&mut original)[0].min_temperature = -0.0;
    let d = descriptor(&original);
    for field in 0..10 {
        let mut c = original.clone();
        let channel = &mut rtd_channels(&mut c)[0];
        match field {
            0 => channel.physical = "dev1/ai00".into(),
            1 => channel.rtd_type = RtdType::Pt3911,
            2 => channel.units = TemperatureUnits::Kelvin,
            3 => channel.min_temperature = 0.0,
            4 => channel.max_temperature = 501.0,
            5 => channel.resistance_config = ResistanceConfig::Two,
            6 => channel.r0_ohms = 100.5,
            7 => {
                channel.excitation = CurrentExcitation::External {
                    amperes: 1.0 / 1024.0,
                }
            }
            8 => {
                channel.excitation = CurrentExcitation::Internal {
                    amperes: 1.0 / 512.0,
                }
            }
            _ => {
                rtd_channels(&mut c).reverse();
            }
        }
        c.validate().unwrap();
        d.validate(&c).unwrap_err();
    }
    let mut d = descriptor(&original);
    d.channels.reverse();
    d.validate(&original).unwrap_err();
}

#[test]
fn rtd_manual_temperature_words_remain_in_requested_units_without_local_physics() {
    let c = rtd();
    let o = rtd_oracle();
    let b = Block {
        sample_index: 0,
        samples_per_channel: 6,
        bits_le_hex: o["two_channel_six_scan_sequence"]["all_words_le_hex"]
            .as_str()
            .unwrap()
            .into(),
        status_code: 0,
    };
    let row = wire::block_reading(&c, &descriptor(&c), &b, &[]).unwrap();
    assert_eq!(row.value["event"], "rtd_temperature_block");
    assert_eq!(
        row.value["representation"],
        "driver_scaled_f64_rtd_temperature"
    );
    assert!(row.source_time.is_none());
    let samples = row.value["samples"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|scan| scan.as_array().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(samples.len(), 12);
    for (sample, expected) in samples
        .iter()
        .zip(o["manual_temperature_patterns"].as_array().unwrap())
    {
        assert_eq!(sample["ieee754_le_hex"], expected["ieee754_le_hex"]);
        assert!(sample.get("volts").is_none());
        assert!(sample.get("amperes").is_none());
        if let Some(decimal) = expected["round_trip_temperature_decimal"].as_str() {
            assert_eq!(
                sample["temperature"].as_f64().unwrap().to_bits(),
                decimal.parse::<f64>().unwrap().to_bits()
            );
        } else {
            assert!(sample.get("temperature").is_none());
        }
    }
    assert_eq!(samples[0]["temperature"], 25.0);
    assert_eq!(samples[1]["temperature"], 25.0);
}

#[test]
fn rtd_metadata_retains_requests_and_actual_names_for_both_timing_modes() {
    for c in [rtd(), software(rtd())] {
        let d = Descriptor::for_config(
            &c,
            vec!["dev1/ai00".into(), "DEV1/ai01".into()],
            c.timing.poll_interval_ns().is_none().then_some(800.0),
        )
        .unwrap();
        let requested = serde_json::to_value(&c).unwrap();
        for event in ["configured", "finished"] {
            let row = wire::event_reading(&c, &d, event, &[]).unwrap();
            assert_eq!(row.value["measurement"], "rtd");
            assert_eq!(row.value["settings_provenance"], "requested_not_read_back");
            for (index, channel) in row.value["channel_measurements"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                assert_eq!(channel["physical"], d.channels[index]);
                for key in [
                    "rtd_type",
                    "units",
                    "min_temperature",
                    "max_temperature",
                    "resistance_config",
                    "r0_ohms",
                    "excitation",
                ] {
                    assert_eq!(channel[key], requested["channels"][index][key]);
                }
                assert!(channel.get("terminal_config").is_none());
                assert!(channel.get("cjc").is_none());
                assert!(channel.get("shunt").is_none());
            }
            assert!(row.source_time.is_none());
            if c.timing.poll_interval_ns().is_some() {
                assert_eq!(row.value["clock"], "software_on_demand_host_completion");
                for key in [
                    "requested_rate_hz",
                    "nominal_rate_hz",
                    "host_buffer_samples_per_channel",
                ] {
                    assert!(row.value.get(key).is_none());
                }
            }
        }
    }
}

#[test]
fn maximum_rtd_matrix_metadata_and_notices_fit_the_existing_row_budget() {
    let mut c = rtd();
    let template = rtd_channels(&mut c)[1].clone();
    *rtd_channels(&mut c) = (0..64)
        .map(|n| {
            let mut channel = template.clone();
            channel.physical = format!("{}D/ai{n}", "D".repeat(248));
            channel.min_temperature = -f64::MAX;
            channel.max_temperature = f64::MAX;
            channel.r0_ohms = f64::MAX;
            channel.excitation = CurrentExcitation::External { amperes: f64::MAX };
            channel.resistance_config = ResistanceConfig::Three;
            channel.units = TemperatureUnits::Kelvin;
            channel
        })
        .collect();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    n.diagnostic_source = "extended_error_info".into();
    n.diagnostic_lookup_status = i32::MIN;
    let notices = vec![n; wire::MAX_NOTICES];
    let b = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let d = descriptor(&c);
    let row = wire::block_reading(&c, &d, &b, &notices).unwrap();
    assert!(serde_json::to_vec(&row).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &d, "configured", &notices).unwrap();
    let mut frame = Vec::new();
    wire::write(
        &mut frame,
        &Response::Ready {
            descriptor: d,
            observed_ns: u64::MAX - 1,
            notices,
        },
    )
    .unwrap();
}

fn mixed() -> Config {
    let mut tc = serde_json::to_value(thermocouple()).unwrap()["channels"][0].clone();
    tc["physical"] = json!("Dev1/ai1");
    tc["measurement"] = json!("thermocouple");
    let mut current = serde_json::to_value(current()).unwrap()["channels"][1].clone();
    current["physical"] = json!("Dev1/ai2");
    current["measurement"] = json!("current");
    current["terminal_config"] = json!("nrse");
    current["min_amperes"] = json!(-0.031_25);
    current["max_amperes"] = json!(0.031_25);
    let mut rtd = serde_json::to_value(rtd()).unwrap()["channels"][1].clone();
    rtd["physical"] = json!("Dev1/ai3");
    rtd["measurement"] = json!("rtd");
    serde_json::from_value(json!({"signal":"mixed_ai","measurement":"mixed","rate_hz":1000.0,
        "max_samples_per_channel":4096,"host_buffer_samples_per_channel":8192,"channels":[
            {"measurement":"voltage","physical":"Dev1/ai0","terminal_config":"differential","min_volts":-10.0,"max_volts":10.0},
            tc,current,rtd]})).unwrap()
}
fn mixed_channels(c: &mut Config) -> &mut Vec<MixedChannel> {
    let Measurement::Mixed { channels } = &mut c.measurement else {
        panic!("expected mixed AI")
    };
    channels
}
fn mixed_oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/nidaqmx/mixed-ai.json"
    ))
    .unwrap()
}

#[test]
fn mixed_requires_explicit_individual_measurements_and_closed_family_fields() {
    let good = serde_json::to_value(mixed()).unwrap();
    for index in 0..4 {
        for key in ["measurement", "physical"] {
            let mut missing = good.clone();
            missing["channels"][index]
                .as_object_mut()
                .unwrap()
                .remove(key);
            serde_json::from_value::<Config>(missing).unwrap_err();
            let mut null = good.clone();
            null["channels"][index][key] = Value::Null;
            serde_json::from_value::<Config>(null).unwrap_err();
        }
        let mut nested = good.clone();
        nested["channels"][index]["measurement"] = json!("mixed");
        serde_json::from_value::<Config>(nested).unwrap_err();
        let mut unknown = good.clone();
        unknown["channels"][index]["unknown"] = Value::Null;
        serde_json::from_value::<Config>(unknown).unwrap_err();
    }
    for key in ["terminal_config", "min_volts", "max_volts"] {
        let mut value = good.clone();
        value[key] = Value::Null;
        serde_json::from_value::<Config>(value).unwrap_err();
    }
    for (index, key) in [
        (0, "terminal_config"),
        (0, "min_volts"),
        (1, "cjc"),
        (2, "shunt"),
        (3, "excitation"),
        (3, "r0_ohms"),
    ] {
        let mut value = good.clone();
        value["channels"][index]
            .as_object_mut()
            .unwrap()
            .remove(key);
        serde_json::from_value::<Config>(value).unwrap_err();
    }
}

#[test]
fn mixed_validates_every_family_and_allows_same_family_overrides_without_defaults() {
    let mut value = serde_json::to_value(mixed()).unwrap();
    value["channels"] = json!([
        {"measurement":"voltage","physical":"Dev1/ai0","terminal_config":"differential","min_volts":-10.0,"max_volts":10.0},
        {"measurement":"voltage","physical":"Dev1/ai1","terminal_config":"rse","min_volts":-2.5,"max_volts":2.5}
    ]);
    let overrides: Config = serde_json::from_value(value).unwrap();
    overrides.validate().unwrap();
    let mut single = overrides;
    mixed_channels(&mut single).truncate(1);
    single.validate().unwrap();
    for mode in 0..8 {
        let mut c = mixed();
        match mode {
            0 => {
                let MixedChannel::Voltage(v) = &mut mixed_channels(&mut c)[0] else {
                    unreachable!()
                };
                v.min_volts = v.max_volts;
            }
            1 => {
                let MixedChannel::Thermocouple(v) = &mut mixed_channels(&mut c)[1] else {
                    unreachable!()
                };
                v.cjc = ColdJunction::Constant { value: f64::NAN };
            }
            2 => {
                let MixedChannel::Current(v) = &mut mixed_channels(&mut c)[2] else {
                    unreachable!()
                };
                v.shunt = CurrentShunt::External { ohms: 0.0 };
            }
            3 => {
                let MixedChannel::Rtd(v) = &mut mixed_channels(&mut c)[3] else {
                    unreachable!()
                };
                v.r0_ohms = -0.0;
            }
            4 => {
                let MixedChannel::Rtd(v) = &mut mixed_channels(&mut c)[3] else {
                    unreachable!()
                };
                v.excitation = CurrentExcitation::Internal {
                    amperes: f64::INFINITY,
                };
            }
            5 => {
                let MixedChannel::Thermocouple(v) = &mut mixed_channels(&mut c)[1] else {
                    unreachable!()
                };
                v.physical = "dev1/ai00".into();
            }
            6 => mixed_channels(&mut c).clear(),
            _ => {
                let first = mixed_channels(&mut c)[0].clone();
                *mixed_channels(&mut c) = vec![first; 65];
            }
        }
        c.validate().unwrap_err();
    }
}

#[test]
fn mixed_decimal_tags_and_private_frames_preserve_bits_and_reject_duplicate_keys() {
    let text = serde_json::to_string(&mixed())
        .unwrap()
        .replace("0.001953125", "1.953125e-3")
        .replace("\"ohms\":250.0", "\"ohms\":2.5e2");
    let direct: Config = serde_json::from_str(&text).unwrap();
    let buffered: Config = serde_json::from_value(serde_json::from_str(&text).unwrap()).unwrap();
    direct.validate().unwrap();
    assert!(direct.measurement.same_request(&buffered.measurement));
    for duplicate in [
        text.replace(
            "\"measurement\":\"voltage\"",
            "\"measurement\":\"voltage\",\"measurement\":\"current\"",
        ),
        text.replace("\"ohms\":2.5e2", "\"ohms\":2.5e2,\"ohms\":251"),
        text.replace(
            "\"amperes\":1.953125e-3",
            "\"amperes\":1.953125e-3,\"amperes\":0.001",
        ),
    ] {
        serde_json::from_str::<Config>(&duplicate).unwrap_err();
    }
    for c in [direct, software(mixed())] {
        let d = Descriptor::for_config(
            &c,
            c.physical_channels(),
            c.timing.poll_interval_ns().is_none().then_some(800.0),
        )
        .unwrap();
        let mut frame = Vec::new();
        wire::write(
            &mut frame,
            &Response::Ready {
                descriptor: d,
                observed_ns: 1,
                notices: Vec::new(),
            },
        )
        .unwrap();
        let Some(Response::Ready { descriptor, .. }) =
            wire::read::<Response>(&mut frame.as_slice()).unwrap()
        else {
            panic!("wrong response")
        };
        descriptor.validate(&c).unwrap();
    }
}

#[test]
fn mixed_descriptors_bind_column_kinds_requests_float_bits_and_actual_order() {
    let mut original = mixed();
    let MixedChannel::Voltage(v) = &mut mixed_channels(&mut original)[0] else {
        unreachable!()
    };
    v.min_volts = -0.0;
    let d = descriptor(&original);
    for mode in 0..8 {
        let mut c = original.clone();
        match mode {
            0 => {
                let MixedChannel::Voltage(v) = &mut mixed_channels(&mut c)[0] else {
                    unreachable!()
                };
                v.min_volts = 0.0;
            }
            1 => {
                let MixedChannel::Thermocouple(v) = &mut mixed_channels(&mut c)[1] else {
                    unreachable!()
                };
                v.units = TemperatureUnits::DegF;
            }
            2 => {
                let MixedChannel::Current(v) = &mut mixed_channels(&mut c)[2] else {
                    unreachable!()
                };
                v.shunt = CurrentShunt::BuiltIn;
            }
            3 => {
                let MixedChannel::Rtd(v) = &mut mixed_channels(&mut c)[3] else {
                    unreachable!()
                };
                v.r0_ohms = 100.0;
            }
            4 => {
                let MixedChannel::Rtd(v) = &mut mixed_channels(&mut c)[3] else {
                    unreachable!()
                };
                v.excitation = CurrentExcitation::Internal {
                    amperes: 1.0 / 512.0,
                };
            }
            5 => mixed_channels(&mut c).swap(1, 3),
            6 => {
                let replacement = mixed_channels(&mut c)[0].clone();
                mixed_channels(&mut c)[1] = replacement;
                let MixedChannel::Voltage(v) = &mut mixed_channels(&mut c)[1] else {
                    unreachable!()
                };
                v.physical = "Dev1/ai1".into();
            }
            _ => {
                let MixedChannel::Current(v) = &mut mixed_channels(&mut c)[2] else {
                    unreachable!()
                };
                v.max_amperes = 0.04;
            }
        }
        c.validate().unwrap();
        d.validate(&c).unwrap_err();
    }
    let mut d = descriptor(&original);
    d.channels.swap(0, 3);
    d.validate(&original).unwrap_err();
}

#[test]
fn mixed_manual_scan_words_keep_each_column_quantity_and_units_in_both_timings() {
    let oracle = mixed_oracle();
    let all = oracle["two_scan_four_column_control"]["all_words_le_hex"]
        .as_str()
        .unwrap();
    for c in [mixed(), software(mixed())] {
        let d = Descriptor::for_config(
            &c,
            c.physical_channels(),
            c.timing.poll_interval_ns().is_none().then_some(800.0),
        )
        .unwrap();
        let cap = c.max_samples_per_channel();
        let parts = if cap == 1 {
            let (first, second) = all.split_at_checked(64).unwrap();
            vec![(0, first), (1, second)]
        } else {
            vec![(0, all)]
        };
        let mut actual_words = Vec::new();
        for (index, bits) in parts {
            let count = u32::try_from(bits.len() / 64).unwrap();
            let row = wire::block_reading(
                &c,
                &d,
                &Block {
                    sample_index: index,
                    samples_per_channel: count,
                    bits_le_hex: bits.into(),
                    status_code: 0,
                },
                &[],
            )
            .unwrap();
            assert_eq!(row.value["event"], "mixed_analog_block");
            assert_eq!(
                row.value["representation"],
                "driver_scaled_f64_mixed_analog"
            );
            assert!(row.source_time.is_none());
            for scan in row.value["samples"].as_array().unwrap() {
                for (column, sample) in scan.as_array().unwrap().iter().enumerate() {
                    let expected = &oracle["manual_scan_major_patterns"][actual_words.len()];
                    let key = ["volts", "temperature", "amperes", "temperature"][column];
                    assert_eq!(sample["ieee754_le_hex"], expected["ieee754_le_hex"]);
                    if let Some(decimal) = expected["finite_decimal"].as_str() {
                        assert_eq!(
                            sample[key].as_f64().unwrap().to_bits(),
                            decimal.parse::<f64>().unwrap().to_bits()
                        );
                    } else {
                        assert!(sample.get(key).is_none());
                    }
                    for other in ["volts", "amperes", "temperature"] {
                        if other != key {
                            assert!(sample.get(other).is_none());
                        }
                    }
                    for field in ["measurement", "channel_index", "quantity", "units"] {
                        assert!(sample.get(field).is_none());
                    }
                    actual_words.push(sample["ieee754_le_hex"].as_str().unwrap().to_owned());
                }
            }
        }
        assert_eq!(actual_words.join(""), all);
    }
}

#[test]
fn mixed_metadata_binds_actual_zero_based_columns_without_global_unit_or_readback_claims() {
    for c in [mixed(), software(mixed())] {
        let names = vec![
            "dev1/ai00".into(),
            "DEV1/ai01".into(),
            "Dev1/ai02".into(),
            "dev1/ai03".into(),
        ];
        let d = Descriptor::for_config(
            &c,
            names,
            c.timing.poll_interval_ns().is_none().then_some(800.0),
        )
        .unwrap();
        for event in ["configured", "finished"] {
            let row = wire::event_reading(&c, &d, event, &[]).unwrap();
            assert_eq!(row.value["measurement"], "mixed");
            assert_eq!(row.value["settings_provenance"], "requested_not_read_back");
            assert!(row.value.get("units").is_none());
            for (index, channel) in row.value["channel_measurements"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                assert_eq!(channel["channel_index"], index);
                assert_eq!(channel["physical"], d.channels[index]);
                assert_eq!(
                    channel["measurement"],
                    ["voltage", "thermocouple", "current", "rtd"][index]
                );
                assert_eq!(
                    channel["quantity"],
                    ["voltage", "temperature", "current", "temperature"][index]
                );
                assert_eq!(channel["units"], ["volts", "deg_c", "amps", "deg_f"][index]);
            }
            assert_eq!(
                row.value["channel_measurements"][0]["terminal_config_request"],
                "differential"
            );
            assert_eq!(
                row.value["channel_measurements"][0]["terminal_config_provenance"],
                "explicit_request_not_read_back"
            );
            assert!(
                row.value["channel_measurements"][0]
                    .get("terminal_config")
                    .is_none()
            );
            assert_eq!(row.value["channel_measurements"][3]["r0_ohms"], 1000.0);
            if c.timing.poll_interval_ns().is_some() {
                for key in [
                    "requested_rate_hz",
                    "nominal_rate_hz",
                    "host_buffer_samples_per_channel",
                ] {
                    assert!(row.value.get(key).is_none());
                }
            }
        }
    }
}

#[test]
fn maximum_mixed_metadata_and_scan_matrix_fit_the_existing_row_and_frame_budget() {
    let mut c = mixed();
    let MixedChannel::Thermocouple(mut template) = mixed_channels(&mut c)[1].clone() else {
        unreachable!()
    };
    template.units = TemperatureUnits::Kelvin;
    template.min_temperature = -f64::MAX;
    template.max_temperature = f64::MAX;
    template.cjc = ColdJunction::Constant { value: f64::MAX };
    template.terminal_config = Some(TerminalConfig::Pseudodifferential);
    *mixed_channels(&mut c) = (0..64)
        .map(|index| {
            let mut channel = template.clone();
            channel.physical = format!("{}D/ai{index}", "D".repeat(248));
            MixedChannel::Thermocouple(channel)
        })
        .collect();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    n.diagnostic_source = "extended_error_info".into();
    n.diagnostic_lookup_status = i32::MIN;
    let notices = vec![n; wire::MAX_NOTICES];
    let d = descriptor(&c);
    let b = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let row = wire::block_reading(&c, &d, &b, &notices).unwrap();
    assert!(serde_json::to_vec(&row).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &d, "configured", &notices).unwrap();
    let mut frame = Vec::new();
    wire::write(
        &mut frame,
        &Response::Ready {
            descriptor: d,
            observed_ns: u64::MAX - 1,
            notices,
        },
    )
    .unwrap();
}

fn digital_ports() -> Config {
    serde_json::from_value(json!({"signal":"ports","measurement":"digital_port",
        "channels":["Dev1/port3","Dev1/port1","Dev1/port12"],"timing":"on_demand","poll_interval_ms":250})).unwrap()
}
fn counter_count() -> Config {
    serde_json::from_value(
        json!({"signal":"edges","measurement":"counter_count","channels":[{
        "physical":"Dev1/ctr0","edge":"falling","initial_count":9,"direction":"up"}],
        "timing":"on_demand","poll_interval_ms":250}),
    )
    .unwrap()
}
fn counter_frequency() -> Config {
    serde_json::from_value(json!({"signal":"frequency","measurement":"counter_frequency","channels":[{
        "physical":"Dev1/ctr1","edge":"rising","min_hz":0.0,"max_hz":1000.0,"method":"dynamic_averaging",
        "measurement_time_seconds":0.125,"divisor":7}],"timing":"on_demand","poll_interval_ms":250})).unwrap()
}
fn input_descriptor(c: &Config) -> Descriptor {
    if matches!(c.measurement, Measurement::DigitalPort { .. }) {
        Descriptor::for_digital_ports(
            c,
            (0..c.measurement.channel_count())
                .map(|i| format!("cfdp{i:04}"))
                .collect(),
            vec![8; c.measurement.channel_count()],
        )
        .unwrap()
    } else {
        Descriptor::for_config(c, c.physical_channels(), None).unwrap()
    }
}

#[test]
fn digital_ports_require_closed_whole_ports_distinct_names_and_explicit_software_timing() {
    let good = serde_json::to_value(digital_ports()).unwrap();
    for names in [
        json!([]),
        json!(["Dev1/port0", "dev1/port00"]),
        json!(["/Dev1/port0"]),
        json!(["Dev1/port0/line0"]),
        json!(["Dev1/port0:3"]),
        json!(["Dev1/port0,Dev1/port1"]),
        json!(["Dev1/port4294967296"]),
        json!(["Dev1/ai0"]),
        json!(["Alias"]),
        json!((0..65).map(|n| format!("Dev1/port{n}")).collect::<Vec<_>>()),
    ] {
        let mut v = good.clone();
        v["channels"] = names;
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap_err();
    }
    for field in [
        "terminal_config",
        "min_volts",
        "max_volts",
        "rate_hz",
        "max_samples_per_channel",
        "host_buffer_samples_per_channel",
    ] {
        let mut v = good.clone();
        v[field] = Value::Null;
        serde_json::from_value::<Config>(v).unwrap_err();
    }
    let mut v = good;
    v["channels"] = json!(
        (0..64)
            .map(|n| format!("cDAQ1Mod1/port{n}"))
            .collect::<Vec<_>>()
    );
    let c: Config = serde_json::from_value(v).unwrap();
    c.validate().unwrap();
    input_descriptor(&c).validate(&c).unwrap();
    let mut clocked = c;
    clocked.timing = Timing::Continuous {
        rate_hz: 1.0,
        max_samples_per_channel: 1,
        host_buffer_samples_per_channel: 1,
    };
    clocked.validate().unwrap_err();
}

#[test]
fn counters_require_one_closed_typed_channel_and_explicit_terminal_requests() {
    for c in [counter_count(), counter_frequency()] {
        c.validate().unwrap();
        let good = serde_json::to_value(&c).unwrap();
        let mut missing_timing = good.clone();
        missing_timing.as_object_mut().unwrap().remove("timing");
        serde_json::from_value::<Config>(missing_timing).unwrap_err();
        let mut continuous = c.clone();
        continuous.timing = Timing::Continuous {
            rate_hz: 1.0,
            max_samples_per_channel: 1,
            host_buffer_samples_per_channel: 1,
        };
        continuous.validate().unwrap_err();
        for field in good["channels"][0].as_object().unwrap().keys() {
            let mut missing = good.clone();
            missing["channels"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            serde_json::from_value::<Config>(missing).unwrap_err();
        }
        for field in [
            "terminal_config",
            "min_volts",
            "max_volts",
            "rate_hz",
            "max_samples_per_channel",
            "host_buffer_samples_per_channel",
        ] {
            let mut v = good.clone();
            v[field] = Value::Null;
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        for channels in [
            json!([]),
            json!([good["channels"][0].clone(), good["channels"][0].clone()]),
        ] {
            let mut v = good.clone();
            v["channels"] = channels;
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
        for physical in [
            "Dev1/ctr0:1",
            "/Dev1/ctr0",
            "Dev1/port0",
            "Dev1/ctr4294967296",
            "Dev1/ctr0/line0",
        ] {
            let mut v = good.clone();
            v["channels"][0]["physical"] = json!(physical);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
        for terminal in ["/Dev1/PFI0", "Dev1/PFI1", "/Dev1/ai/SampleClock"] {
            let mut v = good.clone();
            v["channels"][0]["input_terminal"] = json!(terminal);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap();
        }
        for terminal in [
            "",
            "/",
            "Dev1//PFI0",
            "/Dev1/PFI0,PFI1",
            "/Dev1/PFI0:1",
            "Dev1/PFI*",
            "Dev1/PFI\u{0}",
        ] {
            let mut v = good.clone();
            v["channels"][0]["input_terminal"] = json!(terminal);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
        let mut v = good.clone();
        v["channels"][0]["input_terminal"] = Value::Null;
        serde_json::from_value::<Config>(v).unwrap_err();
        let mut v = good;
        v["channels"][0]["unqualified_setting"] = json!(1);
        serde_json::from_value::<Config>(v).unwrap_err();
    }
    for method in [
        "low_frequency_one_counter",
        "high_frequency_two_counter",
        "large_range_two_counter",
        "dynamic_averaging",
    ] {
        let mut v = serde_json::to_value(counter_frequency()).unwrap();
        v["channels"][0]["method"] = json!(method);
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap();
    }
    for mode in 0..6 {
        let mut c = counter_frequency();
        let Measurement::CounterFrequency { channels } = &mut c.measurement else {
            unreachable!()
        };
        match mode {
            0 => channels[0].min_hz = -1.0,
            1 => channels[0].max_hz = channels[0].min_hz,
            2 => channels[0].max_hz = f64::INFINITY,
            3 => channels[0].min_hz = f64::NAN,
            4 => channels[0].measurement_time_seconds = -1.0,
            _ => channels[0].measurement_time_seconds = f64::NAN,
        }
        c.validate().unwrap_err();
    }
    for (mut v, field) in [
        (
            serde_json::to_value(counter_count()).unwrap(),
            "initial_count",
        ),
        (
            serde_json::to_value(counter_frequency()).unwrap(),
            "divisor",
        ),
    ] {
        for number in [json!(-1), json!(4_294_967_296u64), json!(0.5), Value::Null] {
            v["channels"][0][field] = number;
            serde_json::from_value::<Config>(v.clone()).unwrap_err();
        }
    }
}

#[test]
fn counter_numbers_and_private_requests_preserve_exact_integer_and_float_bits() {
    let text = serde_json::to_string(&counter_frequency())
        .unwrap()
        .replace("\"divisor\":7", "\"divisor\":4.294967295e9")
        .replace("\"min_hz\":0.0", "\"min_hz\":-0.0")
        .replace("0.125", "1.25e-1");
    let c: Config = serde_json::from_str(&text).unwrap();
    c.validate().unwrap();
    let copy: Config = serde_json::from_value(serde_json::from_str(&text).unwrap()).unwrap();
    assert!(c.measurement.same_request(&copy.measurement));
    let Measurement::CounterFrequency { channels } = &c.measurement else {
        unreachable!()
    };
    assert_eq!(channels[0].divisor, u32::MAX);
    assert_eq!(channels[0].min_hz.to_bits(), (-0.0f64).to_bits());
    let duplicate = text.replace(
        "\"divisor\":4.294967295e9",
        "\"divisor\":4.294967295e9,\"divisor\":1",
    );
    serde_json::from_str::<Config>(&duplicate).unwrap_err();
    let mut bytes = Vec::new();
    wire::write(
        &mut bytes,
        &Request::Open {
            config: c.clone(),
            deadline_ns: 9_007_199_254_740_999,
            cutoff_ns: u64::MAX,
        },
    )
    .unwrap();
    let Some(Request::Open { config: copy, .. }) =
        wire::read::<Request>(&mut bytes.as_slice()).unwrap()
    else {
        unreachable!()
    };
    assert!(c.measurement.same_request(&copy.measurement));
    let mut changed = copy;
    let Measurement::CounterFrequency { channels } = &mut changed.measurement else {
        unreachable!()
    };
    channels[0].min_hz = 0.0;
    input_descriptor(&c).validate(&changed).unwrap_err();
}

#[test]
fn input_descriptors_bind_requested_family_names_and_observed_port_widths() {
    let c = digital_ports();
    let d = input_descriptor(&c);
    for mode in 0..7 {
        let mut wrong = d.clone();
        match mode {
            0 => wrong.digital_port_line_counts = None,
            1 => wrong.digital_port_line_counts = Some(vec![8]),
            2 => wrong.digital_port_line_counts = Some(vec![0, 8, 32]),
            3 => wrong.digital_port_line_counts = Some(vec![1, 8, 33]),
            4 => wrong.channels.swap(0, 1),
            5 => wrong.channels[0] = "CFDP0000".into(),
            _ => wrong.measurement = counter_count().measurement,
        }
        wrong.validate(&c).unwrap_err();
    }
    for c in [counter_count(), counter_frequency(), software(config())] {
        let mut d = input_descriptor(&c);
        d.digital_port_line_counts = Some(vec![1]);
        d.validate(&c).unwrap_err();
        let mut v = serde_json::to_value(input_descriptor(&c)).unwrap();
        v["digital_port_line_counts"] = Value::Null;
        serde_json::from_value::<Descriptor>(v).unwrap_err();
    }
    let c = counter_count();
    let d = Descriptor::for_config(&c, vec!["DEV1/ctr00".into()], None).unwrap();
    d.validate(&c).unwrap();
    let mut changed = c;
    let Measurement::CounterCount { channels } = &mut changed.measurement else {
        unreachable!()
    };
    channels[0].initial_count += 1;
    d.validate(&changed).unwrap_err();
}

#[test]
fn input_block_geometry_is_selected_by_descriptor_and_never_guessed_from_hex_length() {
    for c in [digital_ports(), counter_count(), counter_frequency()] {
        let d = input_descriptor(&c);
        let hex_len = c.measurement.word_bytes() * 2 * c.measurement.channel_count();
        for (count, bits) in [
            (0, "0".repeat(hex_len)),
            (2, "0".repeat(hex_len * 2)),
            (1, "0".repeat(hex_len - 1)),
            (1, "G".repeat(hex_len)),
            (1, "A".repeat(hex_len)),
            (1, "0".repeat(if hex_len == 16 { 8 } else { hex_len * 2 })),
        ] {
            let b = Block {
                sample_index: 0,
                samples_per_channel: count,
                bits_le_hex: bits,
                status_code: 0,
            };
            wire::block_reading(&c, &d, &b, &[]).unwrap_err();
        }
        let b = Block {
            sample_index: 0,
            samples_per_channel: 1,
            bits_le_hex: "0".repeat(hex_len),
            status_code: 0,
        };
        let row = wire::block_reading(&c, &d, &b, &[]).unwrap();
        assert!(row.source_time.is_none());
        assert!(row.value.get("nominal_rate_hz").is_none());
        assert_eq!(row.value["samples_per_channel"], 1);
        if c.measurement.word_bytes() == 4 {
            assert!(row.value["samples"][0][0].get("ieee754_le_hex").is_none());
        } else {
            assert!(row.value["samples"][0][0].get("u32_le_hex").is_none());
        }
    }
}

#[test]
fn input_metadata_distinguishes_digital_properties_and_counter_requests() {
    for c in [digital_ports(), counter_count(), counter_frequency()] {
        let d = input_descriptor(&c);
        for event in ["configured", "finished"] {
            let row = wire::event_reading(&c, &d, event, &[]).unwrap();
            let item = &row.value["channel_measurements"][0];
            if matches!(c.measurement, Measurement::DigitalPort { .. }) {
                assert_eq!(item["physical"], "Dev1/port3");
                assert_eq!(item["read_channel"], "cfdp0000");
                assert_eq!(item["line_count"], 8);
                assert_eq!(
                    item["physical_provenance"],
                    "constructor_request_not_read_back"
                );
                assert_eq!(item["line_count_provenance"], "driver_channel_property");
                assert!(row.value.get("settings_provenance").is_none());
            } else {
                assert_eq!(row.value["settings_provenance"], "requested_not_read_back");
                assert_eq!(
                    item["input_terminal_provenance"],
                    "driver_default_unqueried"
                );
                assert!(item.get("input_terminal_request").is_none());
            }
            assert!(row.value.get("samples").is_none());
            assert!(row.value.get("source_time").is_none());
        }
    }
}

#[test]
fn maximum_digital_port_metadata_and_notices_fit_existing_bounded_frames() {
    let mut c = digital_ports();
    let Measurement::DigitalPort { channels } = &mut c.measurement else {
        unreachable!()
    };
    *channels = (0..64)
        .map(|n| format!("{}D/port{n}", "D".repeat(246)))
        .collect();
    c.validate().unwrap();
    let d = input_descriptor(&c);
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.diagnostic_hex = "ff".repeat(256);
    let notices = vec![n; wire::MAX_NOTICES];
    let b = Block {
        sample_index: u64::MAX - 1,
        samples_per_channel: 1,
        bits_le_hex: "ffffffff".repeat(64),
        status_code: i32::MIN,
    };
    let row = wire::block_reading(&c, &d, &b, &notices).unwrap();
    assert!(serde_json::to_vec(&row).unwrap().len() < MAX_ROW);
    let mut frame = Vec::new();
    wire::write(
        &mut frame,
        &Response::Ready {
            descriptor: d,
            observed_ns: 1,
            notices,
        },
    )
    .unwrap();
}

#[test]
fn digital_manual_unsigned_words_keep_full_native_states_and_repeats() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/nidaqmx/digital-port.json"
    ))
    .unwrap();
    let c = digital_ports();
    let d = Descriptor::for_digital_ports(
        &c,
        vec!["cfdp0000".into(), "cfdp0001".into(), "cfdp0002".into()],
        vec![1, 8, 32],
    )
    .unwrap();
    let all = oracle["raw_extent_control"]["all_words_le_hex"]
        .as_str()
        .unwrap();
    let mut seen = 0;
    for (index, scan) in all.as_bytes().as_chunks::<24>().0.iter().enumerate() {
        let b = Block {
            sample_index: u64::try_from(index).unwrap(),
            samples_per_channel: 1,
            bits_le_hex: std::str::from_utf8(scan).unwrap().into(),
            status_code: 0,
        };
        let row = wire::block_reading(&c, &d, &b, &[]).unwrap();
        assert_eq!(row.value["event"], "digital_port_block");
        assert_eq!(row.value["representation"], "sdk_u32_digital_port");
        for sample in row.value["samples"][0].as_array().unwrap() {
            let expected = &oracle["manual_scan_major_patterns"][seen];
            assert_eq!(sample["value"].as_u64(), expected["value"].as_u64());
            assert_eq!(sample["u32_le_hex"], expected["u32_le_hex"]);
            assert!(sample.get("volts").is_none());
            assert!(sample.get("ieee754_le_hex").is_none());
            seen += 1;
        }
    }
    assert_eq!(seen, 12);
}

#[test]
fn counter_manual_snapshot_and_frequency_words_never_unwrap_scale_clip_or_lose_nonfinite_bits() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/nidaqmx/counter-input.json"
    ))
    .unwrap();
    for (c, patterns, raw_key, numeric, event, representation) in [
        (
            counter_count(),
            "manual_count_snapshots",
            "u32_le_hex",
            "count",
            "counter_count_block",
            "driver_unscaled_u32_counter_count",
        ),
        (
            counter_frequency(),
            "manual_frequency_patterns",
            "ieee754_le_hex",
            "frequency_hz",
            "counter_frequency_block",
            "driver_scaled_f64_counter_frequency",
        ),
    ] {
        let d = input_descriptor(&c);
        let patterns = oracle[patterns].as_array().unwrap();
        for (index, expected) in patterns.iter().enumerate() {
            let b = Block {
                sample_index: u64::try_from(index).unwrap(),
                samples_per_channel: 1,
                bits_le_hex: expected[raw_key].as_str().unwrap().into(),
                status_code: 0,
            };
            let row = wire::block_reading(&c, &d, &b, &[]).unwrap();
            assert_eq!(row.value["event"], event);
            assert_eq!(row.value["representation"], representation);
            assert_eq!(row.value["sample_index"], u64::try_from(index).unwrap());
            let sample = &row.value["samples"][0][0];
            assert_eq!(sample[raw_key], expected[raw_key]);
            if numeric == "count" {
                assert_eq!(sample[numeric].as_u64(), expected[numeric].as_u64());
            } else if let Some(decimal) = expected["finite_decimal"].as_str() {
                assert_eq!(
                    sample[numeric].as_f64().unwrap().to_bits(),
                    decimal.parse::<f64>().unwrap().to_bits()
                );
            } else {
                assert!(sample.get(numeric).is_none());
            }
            for absent in ["volts", "amperes", "temperature", "delta", "rollovers"] {
                assert!(sample.get(absent).is_none());
            }
        }
        assert_eq!(patterns.len(), 8);
    }
}

fn engineering_cases() -> Vec<Config> {
    let channels = [
        (
            "strain",
            json!({"physical":"Dev1/ai0","min_strain":-0.015_625,"max_strain":0.023_437_5,
            "bridge_type":"half_bridge_i","excitation":{"source":"internal","volts":2.5},
            "gage_factor":2.0,"initial_bridge_volts":0.000_976_562_5,"nominal_gage_ohms":120.0,
            "poisson_ratio":0.25,"lead_wire_ohms":0.5,"coupling":"ac"}),
        ),
        (
            "bridge_ratio",
            json!({"physical":"Dev1/ai0","min_ratio":-0.031_25,"max_ratio":0.015_625,
            "units":"volts_per_volt","bridge_config":"full_bridge","excitation":{"source":"external","volts":5.0},
            "nominal_bridge_ohms":350.0,"coupling":"dc"}),
        ),
        (
            "acceleration",
            json!({"physical":"Dev1/ai0","terminal_config":"pseudodifferential",
            "min_acceleration":-64.0,"max_acceleration":128.0,"units":"g","sensitivity":125.0,
            "sensitivity_units":"mv_per_g","excitation":{"source":"none","amperes":0.000_976_562_5},"coupling":"ground"}),
        ),
        (
            "counter_period",
            json!({"physical":"Dev1/ctr2","min_seconds":0.000_976_562_5,"max_seconds":0.125,
            "edge":"falling","method":"high_frequency_two_counter","measurement_time_seconds":0.031_25,
            "divisor":4,"input_terminal":"/Dev1/PFI1"}),
        ),
        (
            "counter_pulse_width",
            json!({"physical":"Dev1/ctr3","min_seconds":0.000_488_281_25,"max_seconds":0.1875,
            "starting_edge":"rising","input_terminal":"/Dev1/PFI3"}),
        ),
        (
            "counter_angular_position",
            json!({"physical":"Dev1/ctr5","decoding":"x4","units":"degrees",
            "pulses_per_rev":4099,"initial_angle":-2.5,"z_index":{"enabled":true,"value":1.75,"phase":"a_low_b_high"},
            "a_input_terminal":"/Dev1/PFI1","b_input_terminal":"/Dev1/PFI3","z_input_terminal":"/Dev1/PFI9"}),
        ),
        (
            "counter_linear_position",
            json!({"physical":"Dev1/ctr6","decoding":"x2","units":"meters",
            "distance_per_pulse":0.000_976_562_5,"initial_position":1.25,
            "z_index":{"enabled":true,"value":1.375,"phase":"a_high_b_low"},
            "a_input_terminal":"Dev1/PFI2","b_input_terminal":"Dev1/PFI4","z_input_terminal":"Dev1/PFI8"}),
        ),
    ];
    channels
        .into_iter()
        .map(|(kind, channel)| {
            serde_json::from_value(json!({
        "signal":"engineering","measurement":kind,"channels":[channel],
        "timing":"on_demand","poll_interval_ms":250}))
            .unwrap()
        })
        .collect()
}

fn engineering_ai_oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/nidaqmx/engineering-ai.json"
    ))
    .unwrap()
}
fn engineering_counter_oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/nidaqmx/counter-time-position.json"
    ))
    .unwrap()
}
fn engineering_mixed() -> Config {
    let mut value = serde_json::to_value(mixed()).unwrap();
    for (index, c) in engineering_cases().into_iter().take(3).enumerate() {
        let mut channel = serde_json::to_value(c).unwrap()["channels"][0].clone();
        channel["physical"] = json!(format!("Dev1/ai{}", index + 4));
        channel["measurement"] = json!(["strain", "bridge_ratio", "acceleration"][index]);
        value["channels"].as_array_mut().unwrap().push(channel);
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn engineering_shapes_require_closed_requests_and_missing_only_optional_fields() {
    for c in engineering_cases() {
        c.validate().unwrap();
        let good = serde_json::to_value(&c).unwrap();
        for field in good["channels"][0].as_object().unwrap().keys() {
            let optional = field == "coupling" || field.ends_with("input_terminal");
            let mut v = good.clone();
            v["channels"][0].as_object_mut().unwrap().remove(field);
            if optional {
                serde_json::from_value::<Config>(v)
                    .unwrap()
                    .validate()
                    .unwrap();
            } else {
                serde_json::from_value::<Config>(v).unwrap_err();
            }
            let mut v = good.clone();
            v["channels"][0][field] = Value::Null;
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        for nested in ["excitation", "z_index"] {
            if let Some(fields) = good["channels"][0][nested].as_object() {
                for field in fields.keys() {
                    let mut v = good.clone();
                    v["channels"][0][nested]
                        .as_object_mut()
                        .unwrap()
                        .remove(field);
                    serde_json::from_value::<Config>(v).unwrap_err();
                }
                let mut v = good.clone();
                v["channels"][0][nested]["unknown"] = json!(1);
                serde_json::from_value::<Config>(v).unwrap_err();
            }
        }
        for field in ["terminal_config", "min_volts", "max_volts"] {
            let mut v = good.clone();
            v[field] = Value::Null;
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        let mut v = good;
        v["channels"][0]["unknown"] = json!(1);
        serde_json::from_value::<Config>(v).unwrap_err();
    }
}

#[test]
fn engineering_timing_and_channel_domains_are_explicit_before_effects() {
    for (index, c) in engineering_cases().into_iter().enumerate() {
        let mut v = serde_json::to_value(&c).unwrap();
        v.as_object_mut().unwrap().remove("timing");
        v.as_object_mut().unwrap().remove("poll_interval_ms");
        v["rate_hz"] = json!(1000.0);
        v["max_samples_per_channel"] = json!(4096);
        v["host_buffer_samples_per_channel"] = json!(8192);
        let clocked: Config = serde_json::from_value(v).unwrap();
        if index < 3 {
            clocked.validate().unwrap();
        } else {
            clocked.validate().unwrap_err();
        }
        for physical in ["Dev1/ai0:7", "Dev1/port0", "Dev1/ctr0/line0", "/Dev1/ai0"] {
            let mut v = serde_json::to_value(&c).unwrap();
            v["channels"][0]["physical"] = json!(physical);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
        let mut v = serde_json::to_value(&c).unwrap();
        v["channels"] = json!([]);
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap_err();
        if index >= 3 {
            let mut v = serde_json::to_value(c).unwrap();
            let mut second = v["channels"][0].clone();
            second["physical"] = json!("Dev1/ctr7");
            v["channels"].as_array_mut().unwrap().push(second);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
    }
    for mut c in engineering_cases().into_iter().skip(3) {
        let mut channel = serde_json::to_value(&c).unwrap()["channels"][0].clone();
        channel["measurement"] = serde_json::to_value(&c).unwrap()["measurement"].clone();
        c = engineering_mixed();
        let mut v = serde_json::to_value(c).unwrap();
        v["channels"].as_array_mut().unwrap().push(channel);
        serde_json::from_value::<Config>(v).unwrap_err();
    }
}

fn engineering_float_slots(measurement: &mut Measurement) -> Vec<&mut f64> {
    match measurement {
        Measurement::Strain { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_strain,
                &mut c.max_strain,
                &mut c.excitation.volts,
                &mut c.gage_factor,
                &mut c.initial_bridge_volts,
                &mut c.nominal_gage_ohms,
                &mut c.poisson_ratio,
                &mut c.lead_wire_ohms,
            ]
        }
        Measurement::BridgeRatio { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_ratio,
                &mut c.max_ratio,
                &mut c.excitation.volts,
                &mut c.nominal_bridge_ohms,
            ]
        }
        Measurement::Acceleration { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_acceleration,
                &mut c.max_acceleration,
                &mut c.sensitivity,
                &mut c.excitation.amperes,
            ]
        }
        Measurement::CounterPeriod { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_seconds,
                &mut c.max_seconds,
                &mut c.measurement_time_seconds,
            ]
        }
        Measurement::CounterPulseWidth { channels } => {
            let c = &mut channels[0];
            vec![&mut c.min_seconds, &mut c.max_seconds]
        }
        Measurement::CounterAngularPosition { channels } => {
            let c = &mut channels[0];
            vec![&mut c.initial_angle, &mut c.z_index.value]
        }
        Measurement::CounterLinearPosition { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.distance_per_pulse,
                &mut c.initial_position,
                &mut c.z_index.value,
            ]
        }
        _ => unreachable!(),
    }
}

#[test]
fn engineering_scientific_bounds_reject_nonfinite_and_invalid_signed_requests() {
    for mut c in engineering_cases() {
        let count = engineering_float_slots(&mut c.measurement).len();
        for index in 0..count {
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut invalid = c.clone();
                *engineering_float_slots(&mut invalid.measurement)[index] = bad;
                invalid.validate().unwrap_err();
            }
        }
        let good = serde_json::to_value(c).unwrap();
        for field in [
            "gage_factor",
            "nominal_gage_ohms",
            "nominal_bridge_ohms",
            "sensitivity",
            "distance_per_pulse",
        ] {
            if good["channels"][0].get(field).is_some() {
                for bad in [0.0, -1.0] {
                    let mut v = good.clone();
                    v["channels"][0][field] = json!(bad);
                    serde_json::from_value::<Config>(v)
                        .unwrap()
                        .validate()
                        .unwrap_err();
                }
            }
        }
        for field in ["lead_wire_ohms", "min_seconds", "measurement_time_seconds"] {
            if good["channels"][0].get(field).is_some() {
                let mut v = good.clone();
                v["channels"][0][field] = json!(-1.0);
                serde_json::from_value::<Config>(v)
                    .unwrap()
                    .validate()
                    .unwrap_err();
            }
        }
        for (min, max) in [
            ("min_strain", "max_strain"),
            ("min_ratio", "max_ratio"),
            ("min_acceleration", "max_acceleration"),
            ("min_seconds", "max_seconds"),
        ] {
            if good["channels"][0].get(min).is_some() {
                let mut v = good.clone();
                v["channels"][0][min] = v["channels"][0][max].clone();
                serde_json::from_value::<Config>(v)
                    .unwrap()
                    .validate()
                    .unwrap_err();
            }
        }
    }
}

#[test]
fn engineering_excitation_coupling_and_encoder_z_keep_explicit_request_semantics() {
    for c in engineering_cases().into_iter().take(3) {
        let good = serde_json::to_value(c).unwrap();
        let amount = if good["measurement"] == "acceleration" {
            "amperes"
        } else {
            "volts"
        };
        for source in ["internal", "external", "none"] {
            for value in [-1.0, -0.0, 0.0, 1.25] {
                let mut v = good.clone();
                v["channels"][0]["excitation"] = json!({"source":source,amount:value});
                let parsed: Config = serde_json::from_value(v).unwrap();
                if value > 0.0 || (source == "none" && value >= 0.0) {
                    parsed.validate().unwrap();
                } else {
                    parsed.validate().unwrap_err();
                }
            }
        }
        for coupling in ["ac", "dc", "ground"] {
            let mut v = good.clone();
            v["channels"][0]["coupling"] = json!(coupling);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap();
        }
    }
    for c in engineering_cases().into_iter().skip(5) {
        let good = serde_json::to_value(c).unwrap();
        for decode in ["x1", "x2", "x4", "two_pulse_counting"] {
            for enabled in [false, true] {
                let mut v = good.clone();
                v["channels"][0]["decoding"] = json!(decode);
                v["channels"][0]["z_index"]["enabled"] = json!(enabled);
                if !enabled {
                    serde_json::from_value::<Config>(v.clone())
                        .unwrap()
                        .validate()
                        .unwrap_err();
                    v["channels"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("z_input_terminal");
                }
                let c: Config = serde_json::from_value(v).unwrap();
                c.validate().unwrap();
                let row =
                    wire::event_reading(&c, &input_descriptor(&c), "configured", &[]).unwrap();
                let item = &row.value["channel_measurements"][0];
                assert_eq!(
                    item["z_index_phase_applicability"],
                    if !enabled {
                        "z_index_disabled"
                    } else if decode == "two_pulse_counting" {
                        "ignored_for_two_pulse_counting"
                    } else {
                        "quadrature_phase_request"
                    }
                );
                if !enabled {
                    assert_eq!(
                        item["z_input_terminal_provenance"],
                        "not_requested_z_disabled"
                    );
                    assert!(item.get("z_input_terminal_request").is_none());
                }
            }
        }
    }
}

#[test]
fn engineering_decimal_private_frames_and_descriptors_preserve_every_float_bit() {
    for mut c in engineering_cases() {
        let count = engineering_float_slots(&mut c.measurement).len();
        for index in 0..count {
            let mut zero = c.clone();
            *engineering_float_slots(&mut zero.measurement)[index] = -0.0;
            let mut changed = zero.clone();
            *engineering_float_slots(&mut changed.measurement)[index] = 0.0;
            assert!(!zero.measurement.same_request(&changed.measurement));
            if zero.validate().is_ok() {
                input_descriptor(&zero).validate(&changed).unwrap_err();
            }
            // Some zero replacements violate physical admission; equality remains a bit contract.
            let mut frame = Vec::new();
            wire::write(
                &mut frame,
                &Request::Open {
                    config: zero.clone(),
                    deadline_ns: u64::MAX - 1,
                    cutoff_ns: u64::MAX,
                },
            )
            .unwrap();
            let Some(Request::Open { config: copy, .. }) =
                wire::read::<Request>(&mut frame.as_slice()).unwrap()
            else {
                unreachable!()
            };
            assert!(zero.measurement.same_request(&copy.measurement));
        }
        let text = serde_json::to_string(&c)
            .unwrap()
            .replace("0.03125", "3.125e-2");
        let copy: Config = serde_json::from_str(&text).unwrap();
        assert!(c.measurement.same_request(&copy.measurement));
        let duplicate = text.replace("\"physical\":", "\"physical\":\"Dev1/ai9\",\"physical\":");
        serde_json::from_str::<Config>(&duplicate).unwrap_err();
    }
    for (index, field) in [(3, "divisor"), (5, "pulses_per_rev")] {
        let good = serde_json::to_value(&engineering_cases()[index]).unwrap();
        for bad in ["-1", "4294967296", "1.00000000000000001", "null"] {
            let mut v = good.clone();
            v["channels"][0][field] = serde_json::from_str(bad).unwrap();
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        let mut v = good;
        v["channels"][0][field] = serde_json::from_str("4.294967295e9").unwrap();
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap();
    }
}

#[test]
fn engineering_manual_words_keep_native_quantities_and_units_in_both_timings() {
    let ai = engineering_ai_oracle();
    let ci = engineering_counter_oracle();
    for (index, base) in engineering_cases().into_iter().enumerate() {
        let (o, rows, patterns) = if index < 3 {
            (&ai, "public_rows", "manual_homogeneous_patterns")
        } else {
            (&ci, "public_rows", "manual_native_patterns")
        };
        let expected = &o[rows][if index < 3 { index } else { index - 3 }];
        let key = expected["finite_key"].as_str().unwrap();
        let modes = if index < 3 { 2 } else { 1 };
        for mode in 0..modes {
            let mut c = base.clone();
            if mode == 1 {
                c.timing = Timing::Continuous {
                    rate_hz: 1000.0,
                    max_samples_per_channel: 4096,
                    host_buffer_samples_per_channel: 8192,
                };
            }
            let d = if mode == 1 {
                descriptor(&c)
            } else {
                input_descriptor(&c)
            };
            for (sample_index, word) in o[patterns].as_array().unwrap().iter().enumerate() {
                let b = Block {
                    sample_index: sample_index as u64,
                    samples_per_channel: 1,
                    bits_le_hex: word["ieee754_le_hex"].as_str().unwrap().into(),
                    status_code: -200_284,
                };
                let row = wire::block_reading(&c, &d, &b, &[]).unwrap();
                assert!(row.source_time.is_none());
                assert_eq!(row.value["event"], expected["event"]);
                assert_eq!(row.value["representation"], expected["representation"]);
                assert_eq!(row.value["settings_provenance"], "requested_not_read_back");
                let sample = &row.value["samples"][0][0];
                assert_eq!(sample["ieee754_le_hex"], word["ieee754_le_hex"]);
                if let Some(decimal) = word["finite_decimal"].as_str() {
                    assert_eq!(
                        sample[key].as_f64().unwrap().to_bits(),
                        decimal.parse::<f64>().unwrap().to_bits()
                    );
                } else {
                    assert!(sample.get(key).is_none());
                }
                for absent in [
                    "volts",
                    "amperes",
                    "temperature",
                    "source_time",
                    "unwrapped",
                    "rollovers",
                    "delta",
                ] {
                    assert!(sample.get(absent).is_none());
                }
                assert_eq!(row.value["channel_measurements"][0]["channel_index"], 0);
                if index == 2 {
                    let item = &row.value["channel_measurements"][0];
                    assert_eq!(item["terminal_config_request"], "pseudodifferential");
                    assert_eq!(
                        item["terminal_config_provenance"],
                        "explicit_request_not_read_back"
                    );
                    assert!(item.get("terminal_config").is_none());
                }
            }
        }
    }
}

#[test]
fn engineering_mixed_seven_columns_keep_one_ordered_request_vector_and_native_words() {
    let o = engineering_ai_oracle();
    let expected = &o["mixed_seven_channel_control"];
    for mode in 0..2 {
        let mut c = engineering_mixed();
        if mode == 1 {
            c = software(c);
        }
        c.validate().unwrap();
        let d = if mode == 1 {
            input_descriptor(&c)
        } else {
            descriptor(&c)
        };
        let b = Block {
            sample_index: 0,
            samples_per_channel: if mode == 1 { 1 } else { 2 },
            bits_le_hex: expected[if mode == 1 {
                "negative_one_scan_le_hex"
            } else {
                "all_words_le_hex"
            }]
            .as_str()
            .unwrap()
            .into(),
            status_code: 0,
        };
        let row = wire::block_reading(&c, &d, &b, &[]).unwrap();
        assert_eq!(row.value["event"], "mixed_analog_block");
        assert_eq!(
            row.value["representation"],
            "driver_scaled_f64_mixed_analog"
        );
        assert!(row.value.get("units").is_none());
        for (i, channel) in expected["channels"].as_array().unwrap().iter().enumerate() {
            let item = &row.value["channel_measurements"][i];
            for field in ["measurement", "quantity", "units"] {
                assert_eq!(item[field], channel[field]);
            }
            assert_eq!(item["channel_index"], i);
            assert_eq!(item["physical"], format!("Dev1/ai{i}"));
        }
        for (i, sample) in row.value["samples"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|scan| scan.as_array().unwrap())
            .enumerate()
        {
            let word = &expected["manual_scan_major_patterns"][i];
            let key = expected["channels"][i % 7]["finite_key"].as_str().unwrap();
            assert_eq!(sample["ieee754_le_hex"], word["ieee754_le_hex"]);
            if let Some(decimal) = word["finite_decimal"].as_str() {
                assert_eq!(
                    sample[key].as_f64().unwrap().to_bits(),
                    decimal.parse::<f64>().unwrap().to_bits()
                );
            } else {
                assert!(sample.get(key).is_none());
            }
            assert!(sample.get("units").is_none());
            assert!(sample.get("measurement").is_none());
        }
    }
}

#[test]
fn maximum_engineering_request_metadata_and_matrix_fit_existing_budgets() {
    let c = engineering_mixed();
    let mut value = serde_json::to_value(&c).unwrap();
    let serialized = value["channels"].as_array().unwrap().clone();
    value["channels"] = Value::Array(
        (0..64)
            .map(|index| {
                let mut channel = serialized[index % serialized.len()].clone();
                channel["physical"] = json!(format!("{}D/ai{index}", "D".repeat(248)));
                channel
            })
            .collect(),
    );
    let mut c: Config = serde_json::from_value(value).unwrap();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    let notices = vec![n; wire::MAX_NOTICES];
    let d = descriptor(&c);
    let b = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let row = wire::block_reading(&c, &d, &b, &notices).unwrap();
    assert!(serde_json::to_vec(&row).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &d, "configured", &notices).unwrap();
    let mut frame = Vec::new();
    wire::write(
        &mut frame,
        &Response::Ready {
            descriptor: d,
            observed_ns: u64::MAX - 1,
            notices,
        },
    )
    .unwrap();
}

#[test]
fn engineering_enum_units_and_optional_requests_are_bound_to_descriptors() {
    let cases = engineering_cases();
    let fields: &[(usize, &str, &[&str])] = &[
        (
            0,
            "bridge_type",
            &[
                "full_bridge_i",
                "full_bridge_ii",
                "full_bridge_iii",
                "half_bridge_i",
                "half_bridge_ii",
                "quarter_bridge_i",
                "quarter_bridge_ii",
            ],
        ),
        (
            1,
            "bridge_config",
            &["full_bridge", "half_bridge", "quarter_bridge"],
        ),
        (1, "units", &["volts_per_volt", "millivolts_per_volt"]),
        (2, "units", &["g", "m_s2", "in_s2"]),
        (2, "sensitivity_units", &["mv_per_g", "v_per_g"]),
        (
            2,
            "terminal_config",
            &[
                "default",
                "rse",
                "nrse",
                "differential",
                "pseudodifferential",
            ],
        ),
        (
            3,
            "method",
            &[
                "low_frequency_one_counter",
                "high_frequency_two_counter",
                "large_range_two_counter",
            ],
        ),
        (3, "edge", &["rising", "falling"]),
        (4, "starting_edge", &["rising", "falling"]),
        (5, "units", &["degrees", "radians"]),
        (6, "units", &["meters", "inches"]),
        (5, "decoding", &["x1", "x2", "x4", "two_pulse_counting"]),
        (6, "decoding", &["x1", "x2", "x4", "two_pulse_counting"]),
    ];
    for &(index, field, alternatives) in fields {
        let c = &cases[index];
        let d = input_descriptor(c);
        let good = serde_json::to_value(c).unwrap();
        for alternative in alternatives {
            let mut v = good.clone();
            v["channels"][0][field] = json!(alternative);
            let changed: Config = serde_json::from_value(v).unwrap();
            changed.validate().unwrap();
            if good["channels"][0][field] == *alternative {
                d.validate(&changed).unwrap();
            } else {
                d.validate(&changed).unwrap_err();
            }
        }
        let mut v = good;
        v["channels"][0][field] = json!("unknown");
        serde_json::from_value::<Config>(v).unwrap_err();
    }
    for c in cases {
        let d = input_descriptor(&c);
        let good = serde_json::to_value(c).unwrap();
        for field in [
            "coupling",
            "input_terminal",
            "a_input_terminal",
            "b_input_terminal",
            "z_input_terminal",
        ] {
            if good["channels"][0].get(field).is_some() {
                let mut v = good.clone();
                v["channels"][0].as_object_mut().unwrap().remove(field);
                d.validate(&serde_json::from_value(v).unwrap()).unwrap_err();
            }
        }
        if good["channels"][0].get("z_index").is_some() {
            for phase in [
                "a_high_b_high",
                "a_high_b_low",
                "a_low_b_high",
                "a_low_b_low",
            ] {
                let mut v = good.clone();
                v["channels"][0]["z_index"]["phase"] = json!(phase);
                let changed: Config = serde_json::from_value(v).unwrap();
                changed.validate().unwrap();
                if good["channels"][0]["z_index"]["phase"] != phase {
                    d.validate(&changed).unwrap_err();
                }
            }
        }
        let actual = good["channels"][0]["physical"]
            .as_str()
            .unwrap()
            .to_uppercase();
        let actual = actual.replace("/AI", "/ai").replace("/CTR", "/ctr");
        let d = Descriptor::for_config(
            &serde_json::from_value(good.clone()).unwrap(),
            vec![actual.clone()],
            None,
        )
        .unwrap();
        let row = wire::event_reading(
            &serde_json::from_value(good.clone()).unwrap(),
            &d,
            "configured",
            &[],
        )
        .unwrap();
        assert_eq!(row.value["channel_measurements"][0]["physical"], actual);
        assert_eq!(row.value["measurement"], good["measurement"]);
    }
}

fn resistive_cases() -> Vec<Config> {
    let coefficient = json!({"a":9.0/8192.0,"b":1.0/4096.0,"c":1.0/8_388_608.0});
    let channels = [
        (
            "resistance",
            json!({"physical":"Dev1/ai0","min_ohms":0.0,"max_ohms":512.0,"resistance_config":"three_wire",
            "excitation":{"source":"internal","amperes":1.0/1024.0},"coupling":"ac"}),
        ),
        (
            "thermistor_current",
            json!({"physical":"Dev1/ai0","min_temperature":-32.0,"max_temperature":256.0,"units":"deg_c",
            "resistance_config":"three_wire","excitation":{"source":"internal","amperes":1.0/1024.0},"coefficients":coefficient,"coupling":"dc"}),
        ),
        (
            "thermistor_voltage",
            json!({"physical":"Dev1/ai0","min_temperature":-16.0,"max_temperature":128.0,"units":"deg_c",
            "resistance_config":"four_wire","excitation":{"source":"external","volts":2.5},"coefficients":coefficient,"r1_ohms":10_000.0,"coupling":"ground"}),
        ),
        (
            "force_bridge",
            json!({"physical":"Dev1/ai0","min_force":-64.0,"max_force":1024.0,"units":"newtons","bridge_config":"full_bridge",
            "excitation":{"source":"internal","volts":2.5},"nominal_bridge_ohms":350.0,
            "calibration":{"electrical_units":"volts_per_volt","physical_units":"pounds",
                "first":{"electrical":-1.0/1024.0,"physical":-4.0},"second":{"electrical":3.0/512.0,"physical":128.0}},"coupling":"ac"}),
        ),
        (
            "pressure_bridge",
            json!({"physical":"Dev1/ai0","min_pressure":-131_072.0,"max_pressure":524_288.0,"units":"pascals","bridge_config":"half_bridge",
            "excitation":{"source":"external","volts":5.0},"nominal_bridge_ohms":1000.0,
            "calibration":{"electrical_units":"millivolts_per_volt","physical_units":"bar",
                "first":{"electrical":-1.0/2048.0,"physical":-0.5},"second":{"electrical":1.0/256.0,"physical":3.0}},"coupling":"dc"}),
        ),
        (
            "torque_bridge",
            json!({"physical":"Dev1/ai0","min_torque":-8.0,"max_torque":64.0,"units":"newton_meters","bridge_config":"quarter_bridge",
            "excitation":{"source":"internal","volts":1.25},"nominal_bridge_ohms":120.0,
            "calibration":{"electrical_units":"volts_per_volt","physical_units":"inch_ounces",
                "first":{"electrical":-1.0/4096.0,"physical":-16.0},"second":{"electrical":3.0/1024.0,"physical":512.0}},"coupling":"ground"}),
        ),
    ];
    channels
        .into_iter()
        .map(|(kind, channel)| {
            serde_json::from_value(
                json!({"signal":"resistive","measurement":kind,"channels":[channel],
        "timing":"on_demand","poll_interval_ms":250}),
            )
            .unwrap()
        })
        .collect()
}
fn resistive_oracle(bridge: bool) -> Value {
    serde_json::from_str(if bridge {
        include_str!("../../../tests/fixtures/nidaqmx/physical-bridge.json")
    } else {
        include_str!("../../../tests/fixtures/nidaqmx/resistive-ai.json")
    })
    .unwrap()
}
fn resistive_mixed() -> Config {
    let mut value = serde_json::to_value(engineering_mixed()).unwrap();
    for c in resistive_cases() {
        let v = serde_json::to_value(c).unwrap();
        let mut channel = v["channels"][0].clone();
        channel["measurement"] = v["measurement"].clone();
        channel["physical"] = json!(format!(
            "Dev1/ai{}",
            value["channels"].as_array().unwrap().len()
        ));
        value["channels"].as_array_mut().unwrap().push(channel);
    }
    serde_json::from_value(value).unwrap()
}
fn resistive_float_slots(measurement: &mut Measurement) -> Vec<&mut f64> {
    match measurement {
        Measurement::Resistance { channels } => {
            let c = &mut channels[0];
            vec![&mut c.min_ohms, &mut c.max_ohms, &mut c.excitation.amperes]
        }
        Measurement::ThermistorCurrent { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_temperature,
                &mut c.max_temperature,
                &mut c.excitation.amperes,
                &mut c.coefficients.a,
                &mut c.coefficients.b,
                &mut c.coefficients.c,
            ]
        }
        Measurement::ThermistorVoltage { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_temperature,
                &mut c.max_temperature,
                &mut c.excitation.volts,
                &mut c.coefficients.a,
                &mut c.coefficients.b,
                &mut c.coefficients.c,
                &mut c.r1_ohms,
            ]
        }
        Measurement::ForceBridge { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_force,
                &mut c.max_force,
                &mut c.excitation.volts,
                &mut c.nominal_bridge_ohms,
                &mut c.calibration.first.electrical,
                &mut c.calibration.second.electrical,
                &mut c.calibration.first.physical,
                &mut c.calibration.second.physical,
            ]
        }
        Measurement::PressureBridge { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_pressure,
                &mut c.max_pressure,
                &mut c.excitation.volts,
                &mut c.nominal_bridge_ohms,
                &mut c.calibration.first.electrical,
                &mut c.calibration.second.electrical,
                &mut c.calibration.first.physical,
                &mut c.calibration.second.physical,
            ]
        }
        Measurement::TorqueBridge { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_torque,
                &mut c.max_torque,
                &mut c.excitation.volts,
                &mut c.nominal_bridge_ohms,
                &mut c.calibration.first.electrical,
                &mut c.calibration.second.electrical,
                &mut c.calibration.first.physical,
                &mut c.calibration.second.physical,
            ]
        }
        _ => unreachable!(),
    }
}
fn required_object_fields(good: &Value, path: &[&str]) {
    let mut object = &good["channels"][0];
    for key in path {
        object = &object[*key];
    }
    for field in object.as_object().unwrap().keys() {
        let optional = path.is_empty() && field == "coupling";
        for null in [false, true] {
            let mut v = good.clone();
            let mut target = &mut v["channels"][0];
            for key in path {
                target = &mut target[*key];
            }
            if null {
                target[field] = Value::Null;
            } else {
                target.as_object_mut().unwrap().remove(field);
            }
            if optional && !null {
                serde_json::from_value::<Config>(v)
                    .unwrap()
                    .validate()
                    .unwrap();
            } else {
                serde_json::from_value::<Config>(v).unwrap_err();
            }
        }
    }
    let mut v = good.clone();
    let mut target = &mut v["channels"][0];
    for key in path {
        target = &mut target[*key];
    }
    target["unknown"] = json!(true);
    serde_json::from_value::<Config>(v).unwrap_err();
}

#[test]
fn resistive_and_physical_bridge_requests_are_closed_without_sensor_or_calibration_defaults() {
    for (index, c) in resistive_cases().into_iter().enumerate() {
        c.validate().unwrap();
        let good = serde_json::to_value(c).unwrap();
        required_object_fields(&good, &[]);
        required_object_fields(&good, &["excitation"]);
        if matches!(index, 1 | 2) {
            required_object_fields(&good, &["coefficients"]);
        }
        if index >= 3 {
            required_object_fields(&good, &["calibration"]);
            required_object_fields(&good, &["calibration", "first"]);
            required_object_fields(&good, &["calibration", "second"]);
        }
        for field in ["terminal_config", "min_volts", "max_volts"] {
            let mut v = good.clone();
            v[field] = Value::Null;
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        for field in [
            "rtd_type",
            "r0_ohms",
            "custom_scale",
            "terminal_config",
            "cjc",
        ] {
            let mut v = good.clone();
            v["channels"][0][field] = json!(1);
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        if index != 2 {
            let mut v = good.clone();
            v["channels"][0]["r1_ohms"] = json!(10_000.0);
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        let mut clocked = good.clone();
        clocked.as_object_mut().unwrap().remove("timing");
        clocked.as_object_mut().unwrap().remove("poll_interval_ms");
        clocked["rate_hz"] = json!(1000.0);
        clocked["max_samples_per_channel"] = json!(4096);
        clocked["host_buffer_samples_per_channel"] = json!(8192);
        serde_json::from_value::<Config>(clocked)
            .unwrap()
            .validate()
            .unwrap();
        for physical in ["Dev1/ctr0", "Dev1/port0", "Dev1/ai0:3", "/Dev1/ai0"] {
            let mut v = good.clone();
            v["channels"][0]["physical"] = json!(physical);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
    }
}

#[test]
fn resistive_bounds_and_coefficients_are_explicit_finite_requests_without_host_curve_rules() {
    for (index, mut c) in resistive_cases().into_iter().enumerate() {
        let count = resistive_float_slots(&mut c.measurement).len();
        for slot in 0..count {
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut invalid = c.clone();
                *resistive_float_slots(&mut invalid.measurement)[slot] = bad;
                invalid.validate().unwrap_err();
            }
        }
        let good = serde_json::to_value(c).unwrap();
        for field in ["r1_ohms", "nominal_bridge_ohms"] {
            if good["channels"][0].get(field).is_some() {
                for bad in [0.0, -1.0] {
                    let mut v = good.clone();
                    v["channels"][0][field] = json!(bad);
                    serde_json::from_value::<Config>(v)
                        .unwrap()
                        .validate()
                        .unwrap_err();
                }
            }
        }
        for (min, max) in [
            ("min_ohms", "max_ohms"),
            ("min_temperature", "max_temperature"),
            ("min_force", "max_force"),
            ("min_pressure", "max_pressure"),
            ("min_torque", "max_torque"),
        ] {
            if good["channels"][0].get(min).is_some() {
                let mut v = good.clone();
                v["channels"][0][min] = v["channels"][0][max].clone();
                serde_json::from_value::<Config>(v)
                    .unwrap()
                    .validate()
                    .unwrap_err();
            }
        }
        if matches!(index, 1 | 2) {
            for coefficient in ["a", "b", "c"] {
                for value in [-1.0, -0.0, 0.0, 1.0] {
                    let mut v = good.clone();
                    v["channels"][0]["coefficients"][coefficient] = json!(value);
                    serde_json::from_value::<Config>(v)
                        .unwrap()
                        .validate()
                        .unwrap();
                }
            }
        }
        let amount = if index < 2 { "amperes" } else { "volts" };
        for source in ["internal", "external", "none"] {
            for amount_value in [-1.0, -0.0, 0.0, 1.25] {
                let mut v = good.clone();
                v["channels"][0]["excitation"] = json!({"source":source,amount:amount_value});
                let parsed: Config = serde_json::from_value(v).unwrap();
                if amount_value > 0.0 || (source == "none" && amount_value >= 0.0) {
                    parsed.validate().unwrap();
                } else {
                    parsed.validate().unwrap_err();
                }
            }
        }
    }
}

#[test]
fn bridge_calibration_point_admission_keeps_pair_order_and_three_separate_unit_roles() {
    for c in resistive_cases().into_iter().skip(3) {
        let good = serde_json::to_value(c).unwrap();
        for (first, second) in [(0.0, -0.0), (1.0, 1.0)] {
            let mut v = good.clone();
            v["channels"][0]["calibration"]["first"]["electrical"] = json!(first);
            v["channels"][0]["calibration"]["second"]["electrical"] = json!(second);
            serde_json::from_value::<Config>(v)
                .unwrap()
                .validate()
                .unwrap_err();
        }
        for (first, second) in [(2.0, -1.0), (0.0, 0.0)] {
            let mut v = good.clone();
            v["channels"][0]["calibration"]["first"]["physical"] = json!(first);
            v["channels"][0]["calibration"]["second"]["physical"] = json!(second);
            let a = v["channels"][0]["calibration"]["first"].clone();
            let b = v["channels"][0]["calibration"]["second"].clone();
            for reverse in [false, true] {
                if reverse {
                    v["channels"][0]["calibration"]["first"] = b.clone();
                    v["channels"][0]["calibration"]["second"] = a.clone();
                }
                let c: Config = serde_json::from_value(v.clone()).unwrap();
                c.validate().unwrap();
                let row =
                    wire::event_reading(&c, &input_descriptor(&c), "configured", &[]).unwrap();
                assert_eq!(
                    row.value["channel_measurements"][0]["calibration"],
                    v["channels"][0]["calibration"]
                );
                assert_eq!(
                    row.value["channel_measurements"][0]["calibration_provenance"],
                    "declared_two_point_linear_request"
                );
                assert_ne!(
                    row.value["channel_measurements"][0]["units"],
                    row.value["channel_measurements"][0]["calibration"]["physical_units"]
                );
            }
        }
    }
}

#[test]
fn resistive_private_frames_and_descriptors_bind_every_nested_float_bit() {
    for mut c in resistive_cases() {
        let count = resistive_float_slots(&mut c.measurement).len();
        for slot in 0..count {
            let mut zero = c.clone();
            *resistive_float_slots(&mut zero.measurement)[slot] = -0.0;
            let mut changed = zero.clone();
            *resistive_float_slots(&mut changed.measurement)[slot] = 0.0;
            assert!(!zero.measurement.same_request(&changed.measurement));
            if zero.validate().is_ok() {
                input_descriptor(&zero).validate(&changed).unwrap_err();
            }
            let mut bytes = Vec::new();
            wire::write(
                &mut bytes,
                &Request::Open {
                    config: zero.clone(),
                    deadline_ns: u64::MAX - 1,
                    cutoff_ns: u64::MAX,
                },
            )
            .unwrap();
            let Some(Request::Open { config: copy, .. }) =
                wire::read::<Request>(&mut bytes.as_slice()).unwrap()
            else {
                unreachable!()
            };
            assert!(zero.measurement.same_request(&copy.measurement));
        }
        let text = serde_json::to_string(&c)
            .unwrap()
            .replace("512.0", "5.12e2");
        let copy: Config = serde_json::from_str(&text).unwrap();
        assert!(c.measurement.same_request(&copy.measurement));
        let duplicate = text.replace("\"physical\":", "\"physical\":\"Dev1/ai9\",\"physical\":");
        serde_json::from_str::<Config>(&duplicate).unwrap_err();
        for nested in ["coefficients", "calibration", "excitation"] {
            if let Some(object) =
                serde_json::to_value(&c).unwrap()["channels"][0][nested].as_object()
            {
                let field = object.keys().next().unwrap();
                let token = format!("\"{field}\":");
                let duplicate = text.replace(&token, &format!("\"{field}\":null,{token}"));
                serde_json::from_str::<Config>(&duplicate).unwrap_err();
            }
        }
    }
}

#[test]
fn resistive_unit_and_wiring_domains_remain_separate_from_bridge_calibration_units() {
    let cases = resistive_cases();
    let domains: &[(usize, &[&str])] = &[
        (1, &["deg_c", "deg_f", "kelvin", "deg_r"]),
        (2, &["deg_c", "deg_f", "kelvin", "deg_r"]),
        (3, &["newtons", "pounds", "kilogram_force"]),
        (4, &["pascals", "psi", "bar"]),
        (
            5,
            &["newton_meters", "inch_ounces", "inch_pounds", "foot_pounds"],
        ),
    ];
    for &(index, units) in domains {
        let good = serde_json::to_value(&cases[index]).unwrap();
        let d = input_descriptor(&cases[index]);
        for unit in units {
            let mut v = good.clone();
            v["channels"][0]["units"] = json!(unit);
            let changed: Config = serde_json::from_value(v.clone()).unwrap();
            changed.validate().unwrap();
            if good["channels"][0]["units"] != *unit {
                d.validate(&changed).unwrap_err();
            }
            if index >= 3 {
                for calibration_unit in units {
                    v["channels"][0]["calibration"]["physical_units"] = json!(calibration_unit);
                    let c: Config = serde_json::from_value(v.clone()).unwrap();
                    c.validate().unwrap();
                    if good["channels"][0]["units"] != *unit
                        || good["channels"][0]["calibration"]["physical_units"] != *calibration_unit
                    {
                        d.validate(&c).unwrap_err();
                    } else {
                        d.validate(&c).unwrap();
                    }
                    let row =
                        wire::event_reading(&c, &input_descriptor(&c), "configured", &[]).unwrap();
                    assert_eq!(row.value["channel_measurements"][0]["units"], *unit);
                    assert_eq!(
                        row.value["channel_measurements"][0]["calibration"]["physical_units"],
                        *calibration_unit
                    );
                }
            }
        }
        for unit in ["ohms", "volts_per_volt", "degrees", "custom"] {
            let mut v = good.clone();
            v["channels"][0]["units"] = json!(unit);
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        if index >= 3 {
            let wrong = if index == 3 { "pascals" } else { "pounds" };
            let mut v = good;
            v["channels"][0]["calibration"]["physical_units"] = json!(wrong);
            serde_json::from_value::<Config>(v).unwrap_err();
        }
    }
    for (index, c) in cases.into_iter().enumerate() {
        let good = serde_json::to_value(&c).unwrap();
        let d = input_descriptor(&c);
        for coupling in ["ac", "dc", "ground"] {
            let mut v = good.clone();
            v["channels"][0]["coupling"] = json!(coupling);
            let changed: Config = serde_json::from_value(v).unwrap();
            changed.validate().unwrap();
            if good["channels"][0]["coupling"] != coupling {
                d.validate(&changed).unwrap_err();
            }
        }
        let mut v = good.clone();
        v["channels"][0].as_object_mut().unwrap().remove("coupling");
        d.validate(&serde_json::from_value(v).unwrap()).unwrap_err();
        if index < 3 {
            for wiring in ["two_wire", "three_wire", "four_wire"] {
                let mut v = good.clone();
                v["channels"][0]["resistance_config"] = json!(wiring);
                let changed: Config = serde_json::from_value(v).unwrap();
                changed.validate().unwrap();
                if good["channels"][0]["resistance_config"] != wiring {
                    d.validate(&changed).unwrap_err();
                }
            }
        } else {
            for bridge in ["full_bridge", "half_bridge", "quarter_bridge"] {
                let mut v = good.clone();
                v["channels"][0]["bridge_config"] = json!(bridge);
                let changed: Config = serde_json::from_value(v).unwrap();
                changed.validate().unwrap();
                if good["channels"][0]["bridge_config"] != bridge {
                    d.validate(&changed).unwrap_err();
                }
            }
            for electrical in ["volts_per_volt", "millivolts_per_volt"] {
                let mut v = good.clone();
                v["channels"][0]["calibration"]["electrical_units"] = json!(electrical);
                let changed: Config = serde_json::from_value(v).unwrap();
                changed.validate().unwrap();
                if good["channels"][0]["calibration"]["electrical_units"] != electrical {
                    d.validate(&changed).unwrap_err();
                }
            }
        }
    }
}

#[test]
fn resistive_manual_ieee_words_keep_native_units_without_curve_or_calibration_math() {
    for (index, base) in resistive_cases().into_iter().enumerate() {
        let o = resistive_oracle(index >= 3);
        let expected = serde_json::to_value(&base).unwrap();
        let kind = expected["measurement"].as_str().unwrap();
        let key = [
            "ohms",
            "temperature",
            "temperature",
            "force",
            "pressure",
            "torque",
        ][index];
        for software in [false, true] {
            let mut c = base.clone();
            if !software {
                c.timing = Timing::Continuous {
                    rate_hz: 1000.0,
                    max_samples_per_channel: 4096,
                    host_buffer_samples_per_channel: 8192,
                };
            }
            let d = if software {
                input_descriptor(&c)
            } else {
                descriptor(&c)
            };
            for (sample_index, word) in o["manual_native_patterns"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                let block = Block {
                    sample_index: sample_index as u64,
                    samples_per_channel: 1,
                    bits_le_hex: word["ieee754_le_hex"].as_str().unwrap().into(),
                    status_code: -200_284,
                };
                let row = wire::block_reading(&c, &d, &block, &[]).unwrap();
                assert!(row.source_time.is_none());
                assert_eq!(row.value["event"], format!("{kind}_block"));
                assert_eq!(
                    row.value["representation"],
                    format!("driver_scaled_f64_{kind}")
                );
                let sample = &row.value["samples"][0][0];
                assert_eq!(sample["ieee754_le_hex"], word["ieee754_le_hex"]);
                if let Some(decimal) = word["finite_decimal"].as_str() {
                    assert_eq!(
                        sample[key].as_f64().unwrap().to_bits(),
                        decimal.parse::<f64>().unwrap().to_bits()
                    );
                } else {
                    assert!(sample.get(key).is_none());
                }
                for other in [
                    "ohms",
                    "temperature",
                    "force",
                    "pressure",
                    "torque",
                    "volts",
                    "amperes",
                    "strain",
                    "ratio",
                    "acceleration",
                ] {
                    if other != key {
                        assert!(sample.get(other).is_none());
                    }
                }
                let item = &row.value["channel_measurements"][0];
                assert_eq!(item["channel_index"], 0);
                assert_eq!(item["physical"], "Dev1/ai0");
                assert_eq!(
                    item["units"],
                    if index == 0 {
                        json!("ohms")
                    } else {
                        expected["channels"][0]["units"].clone()
                    }
                );
                assert_eq!(
                    item["coupling_request"],
                    expected["channels"][0]["coupling"]
                );
                assert_eq!(row.value["settings_provenance"], "requested_not_read_back");
                for absent in [
                    "calibration_readback",
                    "actual_coefficients",
                    "power_off",
                    "source_time",
                ] {
                    assert!(item.get(absent).is_none());
                }
            }
        }
    }
}

#[test]
fn mixed_thirteen_columns_preserve_old_seven_metadata_and_all_native_finite_keys() {
    let old = engineering_mixed();
    let mut c = resistive_mixed();
    c.validate().unwrap();
    assert_eq!(c.measurement.channel_count(), 13);
    let old_row = wire::event_reading(&old, &descriptor(&old), "configured", &[]).unwrap();
    let o = resistive_oracle(false);
    let expected = &o["mixed_thirteen_channel_control"];
    let bits = expected["literal_bits_scan0"]
        .as_array()
        .unwrap()
        .iter()
        .chain(expected["literal_bits_scan1"].as_array().unwrap())
        .collect::<Vec<_>>();
    for on_demand_mode in [false, true] {
        if on_demand_mode {
            c = software(c);
        }
        let d = if on_demand_mode {
            input_descriptor(&c)
        } else {
            descriptor(&c)
        };
        let block = Block {
            sample_index: 0,
            samples_per_channel: if on_demand_mode { 1 } else { 2 },
            bits_le_hex: expected[if on_demand_mode {
                "negative_one_scan_le_hex"
            } else {
                "all_words_le_hex"
            }]
            .as_str()
            .unwrap()
            .into(),
            status_code: 0,
        };
        let row = wire::block_reading(&c, &d, &block, &[]).unwrap();
        assert_eq!(row.value["event"], "mixed_analog_block");
        assert!(row.value.get("units").is_none());
        assert_eq!(
            &row.value["channel_measurements"].as_array().unwrap()[..7],
            old_row.value["channel_measurements"].as_array().unwrap()
        );
        let raw = block.bits_le_hex.as_bytes().as_chunks::<16>().0;
        for (index, sample) in row.value["samples"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| s.as_array().unwrap())
            .enumerate()
        {
            let column = index % 13;
            let key = [
                "volts",
                "temperature",
                "amperes",
                "temperature",
                "strain",
                "ratio",
                "acceleration",
                "ohms",
                "temperature",
                "temperature",
                "force",
                "pressure",
                "torque",
            ][column];
            let word = u64::from_str_radix(bits[index].as_str().unwrap(), 16).unwrap();
            assert_eq!(
                sample["ieee754_le_hex"],
                std::str::from_utf8(&raw[index]).unwrap()
            );
            if f64::from_bits(word).is_finite() {
                assert_eq!(sample[key].as_f64().unwrap().to_bits(), word);
            } else {
                assert!(sample.get(key).is_none());
            }
            assert!(sample.get("units").is_none());
            assert!(sample.get("measurement").is_none());
            assert_eq!(
                row.value["channel_measurements"][column]["channel_index"],
                column
            );
            if column >= 7 {
                let control = &expected["columns"][column];
                let item = &row.value["channel_measurements"][column];
                for field in ["measurement", "quantity", "units"] {
                    assert_eq!(item[field], control[field]);
                }
            }
        }
    }
}

#[test]
fn maximum_thirteen_family_requests_matrix_and_notices_fit_existing_row_and_frame_limits() {
    let mut value = serde_json::to_value(resistive_mixed()).unwrap();
    let templates = value["channels"].as_array().unwrap().clone();
    value["channels"] = Value::Array(
        (0..64)
            .map(|index| {
                let mut c = templates[index % 13].clone();
                c["physical"] = json!(format!("{}D/ai{index}", "D".repeat(248)));
                c
            })
            .collect(),
    );
    let mut c: Config = serde_json::from_value(value).unwrap();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    let notices = vec![n; wire::MAX_NOTICES];
    let d = descriptor(&c);
    let b = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let row = wire::block_reading(&c, &d, &b, &notices).unwrap();
    assert!(serde_json::to_vec(&row).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &d, "configured", &notices).unwrap();
    let mut frame = Vec::new();
    wire::write(
        &mut frame,
        &Response::Ready {
            descriptor: d,
            observed_ns: u64::MAX - 1,
            notices,
        },
    )
    .unwrap();
}

fn five_oracle(position: bool) -> Value {
    serde_json::from_str(if position {
        include_str!("../../../tests/fixtures/nidaqmx/analog-position.json")
    } else {
        include_str!("../../../tests/fixtures/nidaqmx/dynamic-ai.json")
    })
    .unwrap()
}

fn five_cases() -> Vec<Config> {
    let kinds = [
        "velocity_iepe",
        "force_iepe",
        "charge_acceleration",
        "lvdt",
        "rvdt",
    ];
    kinds.into_iter().enumerate().map(|(index, kind)| {
        let oracle = five_oracle(index >= 3);
        let channels = oracle["ordered_constructor_pairs"][kind].as_array().unwrap().iter().map(|control| {
            let words = &control["float_argument_words"];
            let number = |role: &str| -> f64 {
                oracle["manual_argument_words"][words[role].as_str().unwrap()]["decimal"]
                    .as_str().unwrap().parse().unwrap()
            };
            let (min, max) = [
                ("min_velocity", "max_velocity"), ("min_force", "max_force"),
                ("min_acceleration", "max_acceleration"), ("min_position", "max_position"),
                ("min_angle", "max_angle"),
            ][index];
            let mut channel = json!({"physical":control["physical"],"units":control["units"],
                "sensitivity":number("sensitivity"),"sensitivity_units":control["sensitivity_units"]});
            channel[min] = json!(number("min"));
            channel[max] = json!(number("max"));
            if index < 3 { channel["terminal_config"] = control["terminal_config"].clone(); }
            if index < 2 {
                channel["excitation"] = json!({"source":control["excitation_source"],"amperes":number("amperes")});
            } else if index >= 3 {
                channel["excitation"] = json!({"source":control["excitation_source"],"volts":number("volts"),"frequency_hz":number("frequency_hz")});
                channel["ac_wire_mode"] = control["ac_wire_mode"].clone();
            }
            if control["coupling"] != "omitted" { channel["coupling"] = control["coupling"].clone(); }
            channel
        }).collect::<Vec<_>>();
        serde_json::from_value(json!({"signal":"five","measurement":kind,"channels":channels,
            "timing":"on_demand","poll_interval_ms":125})).unwrap()
    }).collect()
}

fn five_mixed() -> Config {
    let mut value = serde_json::to_value(resistive_mixed()).unwrap();
    for c in five_cases() {
        let v = serde_json::to_value(c).unwrap();
        let mut channel = v["channels"][0].clone();
        channel["measurement"] = v["measurement"].clone();
        channel["physical"] = json!(format!(
            "Dev1/ai{}",
            value["channels"].as_array().unwrap().len()
        ));
        value["channels"].as_array_mut().unwrap().push(channel);
    }
    serde_json::from_value(value).unwrap()
}

fn five_float_slots(measurement: &mut Measurement) -> Vec<&mut f64> {
    match measurement {
        Measurement::VelocityIepe { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_velocity,
                &mut c.max_velocity,
                &mut c.sensitivity,
                &mut c.excitation.amperes,
            ]
        }
        Measurement::ForceIepe { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_force,
                &mut c.max_force,
                &mut c.sensitivity,
                &mut c.excitation.amperes,
            ]
        }
        Measurement::ChargeAcceleration { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_acceleration,
                &mut c.max_acceleration,
                &mut c.sensitivity,
            ]
        }
        Measurement::Lvdt { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_position,
                &mut c.max_position,
                &mut c.sensitivity,
                &mut c.excitation.volts,
                &mut c.excitation.frequency_hz,
            ]
        }
        Measurement::Rvdt { channels } => {
            let c = &mut channels[0];
            vec![
                &mut c.min_angle,
                &mut c.max_angle,
                &mut c.sensitivity,
                &mut c.excitation.volts,
                &mut c.excitation.frequency_hz,
            ]
        }
        _ => unreachable!(),
    }
}

#[test]
fn five_new_families_require_closed_objects_and_explicit_scientific_requests() {
    use super::config::{
        AcVoltageExcitation, ChargeAccelerationChannel, ForceIepeChannel, LvdtChannel, RvdtChannel,
        VelocityIepeChannel,
    };
    for (index, c) in five_cases().into_iter().enumerate() {
        c.validate().unwrap();
        let good = serde_json::to_value(c).unwrap();
        required_object_fields(&good, &[]);
        if index != 2 {
            required_object_fields(&good, &["excitation"]);
        }
        let direct = good["channels"][0].clone();
        let array = Value::Array(direct.as_object().unwrap().values().cloned().collect());
        match index {
            0 => {
                serde_json::from_value::<VelocityIepeChannel>(array.clone()).unwrap_err();
            }
            1 => {
                serde_json::from_value::<ForceIepeChannel>(array.clone()).unwrap_err();
            }
            2 => {
                serde_json::from_value::<ChargeAccelerationChannel>(array.clone()).unwrap_err();
            }
            3 => {
                serde_json::from_value::<LvdtChannel>(array.clone()).unwrap_err();
            }
            4 => {
                serde_json::from_value::<RvdtChannel>(array.clone()).unwrap_err();
            }
            _ => unreachable!(),
        }
        let mut v = good.clone();
        v["channels"][0] = array;
        serde_json::from_value::<Config>(v).unwrap_err();
        if index != 2 {
            let mut v = good.clone();
            v["channels"][0]["excitation"] = json!(["internal", 1.0]);
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        for field in ["custom_scale", "rtd_type", "cjc", "calibration"] {
            let mut v = good.clone();
            v["channels"][0][field] = Value::Null;
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        let mut v = good.clone();
        v["channels"][0][if index >= 3 {
            "terminal_config"
        } else {
            "ac_wire_mode"
        }] = Value::Null;
        serde_json::from_value::<Config>(v).unwrap_err();
        if index == 2 {
            let mut v = good.clone();
            v["channels"][0]["excitation"] = Value::Null;
            serde_json::from_value::<Config>(v).unwrap_err();
        }
        let mut v = good.clone();
        v["channels"] = json!([]);
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap_err();
        let mut v = good.clone();
        v["channels"][1]["physical"] = good["channels"][0]["physical"].clone();
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap_err();
        let mut v = good;
        v["channels"][0]["physical"] = json!("Dev1/ai0:100");
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap_err();
    }
    serde_json::from_value::<AcVoltageExcitation>(json!(["none", 0.0, 1000.0])).unwrap_err();
    serde_json::from_value::<AcVoltageExcitation>(
        json!({"source":"none","volts":0.0,"frequency_hz":1000.0,"extra":1}),
    )
    .unwrap_err();
}

#[test]
fn new_sensitivity_sign_and_none_excitation_policies_do_not_change_old_acceleration() {
    for (index, mut c) in five_cases().into_iter().enumerate() {
        let count = five_float_slots(&mut c.measurement).len();
        for slot in 0..count {
            for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut invalid = c.clone();
                *five_float_slots(&mut invalid.measurement)[slot] = bad;
                invalid.validate().unwrap_err();
            }
        }
        for sensitivity in [-1.0, -0.0, 0.0, f64::from_bits(1), 1.0] {
            let mut admitted = c.clone();
            *five_float_slots(&mut admitted.measurement)[2] = sensitivity;
            admitted.validate().unwrap();
        }
        let good = serde_json::to_value(&c).unwrap();
        if index != 2 {
            let amount = if index < 2 { "amperes" } else { "volts" };
            for source in ["internal", "external", "none"] {
                for value in [-1.0, -0.0, 0.0, 1.0] {
                    let mut v = good.clone();
                    v["channels"][0]["excitation"]["source"] = json!(source);
                    v["channels"][0]["excitation"][amount] = json!(value);
                    let request: Config = serde_json::from_value(v).unwrap();
                    if (source == "none" && value >= 0.0) || value > 0.0 {
                        request.validate().unwrap();
                    } else {
                        request.validate().unwrap_err();
                    }
                }
                if index >= 3 {
                    for hz in [-1.0, -0.0, 0.0] {
                        let mut v = good.clone();
                        v["channels"][0]["excitation"]["source"] = json!(source);
                        v["channels"][0]["excitation"]["frequency_hz"] = json!(hz);
                        serde_json::from_value::<Config>(v)
                            .unwrap()
                            .validate()
                            .unwrap_err();
                    }
                }
            }
        }
        let mut reversed = c.clone();
        let slots = five_float_slots(&mut reversed.measurement);
        *slots.into_iter().next().unwrap() = f64::MAX;
        reversed.validate().unwrap_err();
    }
    let acceleration = &engineering_cases()[2];
    for sensitivity in [-1.0, -0.0, 0.0] {
        let mut v = serde_json::to_value(acceleration).unwrap();
        v["channels"][0]["sensitivity"] = json!(sensitivity);
        serde_json::from_value::<Config>(v)
            .unwrap()
            .validate()
            .unwrap_err();
    }
}

#[test]
fn five_family_private_roundtrips_and_descriptors_preserve_all_float_bits() {
    for mut c in five_cases() {
        let count = five_float_slots(&mut c.measurement).len();
        for slot in 0..count {
            let mut original = c.clone();
            *five_float_slots(&mut original.measurement)[slot] = -0.0;
            let mut changed = original.clone();
            *five_float_slots(&mut changed.measurement)[slot] = 0.0;
            assert!(!original.measurement.same_request(&changed.measurement));
            if original.validate().is_ok() {
                input_descriptor(&original).validate(&changed).unwrap_err();
            }
            let mut bytes = Vec::new();
            wire::write(
                &mut bytes,
                &Request::Open {
                    config: original.clone(),
                    deadline_ns: u64::MAX - 1,
                    cutoff_ns: u64::MAX,
                },
            )
            .unwrap();
            let Some(Request::Open { config: copy, .. }) =
                wire::read::<Request>(&mut bytes.as_slice()).unwrap()
            else {
                unreachable!()
            };
            assert!(original.measurement.same_request(&copy.measurement));
            let mut changed = c.clone();
            let number = five_float_slots(&mut changed.measurement)
                .into_iter()
                .nth(slot)
                .unwrap();
            *number = f64::from_bits(number.to_bits() + 1);
            input_descriptor(&c).validate(&changed).unwrap_err();
        }
        let d = input_descriptor(&c);
        let mut wrong = d.clone();
        wrong.channels.swap(0, 1);
        wrong.validate(&c).unwrap_err();
        let mut wrong = d.clone();
        let _ = wrong.channels.pop();
        wrong.validate(&c).unwrap_err();
        let mut wrong = d;
        wrong.channels.push("Dev1/ai9".into());
        wrong.validate(&c).unwrap_err();
    }
}

#[test]
fn five_family_decimal_objects_reject_duplicate_keys_without_number_coercion() {
    use super::config::{AcVoltageExcitation, VelocityIepeChannel};
    for c in five_cases() {
        let text = serde_json::to_string(&c).unwrap();
        let decimal = text
            .replace("0.3125", "3.125e-1")
            .replace("0.234375", "2.34375e-1");
        let copy: Config = serde_json::from_str(&decimal).unwrap();
        assert!(c.measurement.same_request(&copy.measurement));
        let duplicate = text.replace("\"physical\":", "\"physical\":\"Other/ai9\",\"physical\":");
        serde_json::from_str::<Config>(&duplicate).unwrap_err();
        let duplicate = text.replace("\"sensitivity\":", "\"sensitivity\":1.0,\"sensitivity\":");
        serde_json::from_str::<Config>(&duplicate).unwrap_err();
        if text.contains("\"excitation\"") {
            let duplicate = text.replace("\"source\":", "\"source\":\"none\",\"source\":");
            serde_json::from_str::<Config>(&duplicate).unwrap_err();
        }
    }
    serde_json::from_str::<AcVoltageExcitation>(
        r#"{"source":"none","source":"external","volts":1.0,"frequency_hz":1.0}"#,
    )
    .unwrap_err();
    let c = five_cases().remove(0);
    let v = serde_json::to_value(c).unwrap();
    let direct = serde_json::to_string(&v["channels"][0]).unwrap();
    let duplicate = direct.replace("\"physical\":", "\"physical\":\"Other/ai9\",\"physical\":");
    serde_json::from_str::<VelocityIepeChannel>(&duplicate).unwrap_err();
    let duplicate = direct.replace("\"amperes\":", "\"amperes\":0.0,\"amperes\":");
    serde_json::from_str::<VelocityIepeChannel>(&duplicate).unwrap_err();
}

#[test]
fn five_family_unit_terminal_lead_and_coupling_requests_bind_descriptors() {
    let cases = five_cases();
    let domains: &[(usize, &str, &[&str])] = &[
        (0, "units", &["m_s", "in_s"]),
        (0, "sensitivity_units", &["mv_per_mm_s", "mv_per_in_s"]),
        (1, "units", &["newtons", "pounds", "kilogram_force"]),
        (1, "sensitivity_units", &["mv_per_newton", "mv_per_pound"]),
        (2, "units", &["g", "m_s2", "in_s2"]),
        (
            2,
            "sensitivity_units",
            &["pc_per_g", "pc_per_m_s2", "pc_per_in_s2"],
        ),
        (3, "units", &["meters", "inches"]),
        (
            3,
            "sensitivity_units",
            &["mv_per_v_per_mm", "mv_per_v_per_milli_inch"],
        ),
        (4, "units", &["degrees", "radians"]),
        (
            4,
            "sensitivity_units",
            &["mv_per_v_per_degree", "mv_per_v_per_radian"],
        ),
    ];
    for &(index, field, alternatives) in domains {
        let c = &cases[index];
        let d = input_descriptor(c);
        let good = serde_json::to_value(c).unwrap();
        for alternative in alternatives {
            let mut v = good.clone();
            v["channels"][0][field] = json!(alternative);
            let changed: Config = serde_json::from_value(v).unwrap();
            changed.validate().unwrap();
            if good["channels"][0][field] == *alternative {
                d.validate(&changed).unwrap();
            } else {
                d.validate(&changed).unwrap_err();
            }
        }
        let mut v = good;
        v["channels"][0][field] = json!("unknown");
        serde_json::from_value::<Config>(v).unwrap_err();
    }
    for (index, c) in cases.into_iter().enumerate() {
        let d = input_descriptor(&c);
        let good = serde_json::to_value(&c).unwrap();
        for coupling in [None, Some("ac"), Some("dc"), Some("ground")] {
            let mut v = good.clone();
            if let Some(coupling) = coupling {
                v["channels"][0]["coupling"] = json!(coupling);
            } else {
                v["channels"][0].as_object_mut().unwrap().remove("coupling");
            }
            let changed: Config = serde_json::from_value(v).unwrap();
            changed.validate().unwrap();
            if c.measurement.same_request(&changed.measurement) {
                d.validate(&changed).unwrap();
            } else {
                d.validate(&changed).unwrap_err();
            }
            let row = wire::event_reading(&changed, &input_descriptor(&changed), "configured", &[])
                .unwrap();
            let item = &row.value["channel_measurements"][0];
            assert_eq!(
                item["coupling_provenance"],
                if coupling.is_some() {
                    "explicit_request_not_read_back"
                } else {
                    "driver_default_unqueried"
                }
            );
            assert_eq!(item.get("coupling_request").is_some(), coupling.is_some());
            assert!(item.get("measurement").is_none());
            assert!(item.get("quantity").is_none());
            assert!(item.get("terminal_config").is_none());
            if index < 3 {
                assert_eq!(
                    item["terminal_config_request"],
                    good["channels"][0]["terminal_config"]
                );
            } else {
                assert!(item.get("terminal_config_request").is_none());
                assert!(item.get("terminal_config_provenance").is_none());
            }
        }
        let (field, alternatives): (&str, &[&str]) = if index < 3 {
            (
                "terminal_config",
                &[
                    "default",
                    "rse",
                    "nrse",
                    "differential",
                    "pseudodifferential",
                ],
            )
        } else {
            ("ac_wire_mode", &["four_wire", "five_wire", "six_wire"])
        };
        for alternative in alternatives {
            let mut v = good.clone();
            v["channels"][0][field] = json!(alternative);
            let changed: Config = serde_json::from_value(v).unwrap();
            changed.validate().unwrap();
            if good["channels"][0][field] != *alternative {
                d.validate(&changed).unwrap_err();
            }
        }
    }
}

#[test]
fn five_family_manual_words_preserve_native_quantity_and_original_unit_requests() {
    for (index, base) in five_cases().into_iter().enumerate() {
        let o = five_oracle(index >= 3);
        let expected = serde_json::to_value(&base).unwrap();
        let kind = expected["measurement"].as_str().unwrap();
        let mapping = &o["public_rows"][kind];
        for on_demand in [false, true] {
            let mut c = base.clone();
            if !on_demand {
                c.timing = Timing::Continuous {
                    rate_hz: 1000.0,
                    max_samples_per_channel: 4096,
                    host_buffer_samples_per_channel: 8192,
                };
            }
            let d = if on_demand {
                input_descriptor(&c)
            } else {
                descriptor(&c)
            };
            let block = Block {
                sample_index: 0,
                samples_per_channel: if on_demand { 1 } else { 6 },
                bits_le_hex: o["raw_extent"][if on_demand {
                    "negative_one_scan_le_hex"
                } else {
                    "all_words_le_hex"
                }]
                .as_str()
                .unwrap()
                .into(),
                status_code: -200_284,
            };
            let row = wire::block_reading(&c, &d, &block, &[]).unwrap();
            assert_eq!(row.value["event"], mapping["event"]);
            assert_eq!(row.value["representation"], mapping["representation"]);
            assert!(row.source_time.is_none());
            assert_eq!(row.value["settings_provenance"], "requested_not_read_back");
            for (i, sample) in row.value["samples"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|scan| scan.as_array().unwrap())
                .enumerate()
            {
                let bits = o["raw_extent"]["literal_bits_scan_major"][i]
                    .as_str()
                    .unwrap();
                let word = u64::from_str_radix(bits, 16).unwrap();
                let number = f64::from_bits(word);
                assert_eq!(
                    sample["ieee754_le_hex"],
                    crate::types::hex(word.to_le_bytes())
                );
                let key = mapping["finite_key"].as_str().unwrap();
                if number.is_finite() {
                    assert_eq!(sample[key].as_f64().unwrap().to_bits(), word);
                } else {
                    assert!(sample.get(key).is_none());
                }
                for other in [
                    "volts",
                    "amperes",
                    "temperature",
                    "velocity",
                    "force",
                    "acceleration",
                    "position",
                    "angle",
                ] {
                    if other != key {
                        assert!(sample.get(other).is_none());
                    }
                }
                assert!(sample.get("units").is_none());
                assert!(sample.get("measurement").is_none());
            }
            for column in 0..2 {
                let item = &row.value["channel_measurements"][column];
                assert_eq!(item["channel_index"], column);
                assert_eq!(item["physical"], expected["channels"][column]["physical"]);
                for key in [
                    "units",
                    "sensitivity",
                    "sensitivity_units",
                    "excitation",
                    "ac_wire_mode",
                ] {
                    assert_eq!(item.get(key), expected["channels"][column].get(key));
                }
            }
        }
    }
}

#[test]
fn mixed_eighteen_columns_preserve_old_thirteen_and_exact_scan_major_words() {
    let old = resistive_mixed();
    let oldrow = wire::event_reading(&old, &descriptor(&old), "configured", &[]).unwrap();
    let mut c = five_mixed();
    c.validate().unwrap();
    assert_eq!(c.measurement.channel_count(), 18);
    let oracle = five_oracle(true);
    let expected = &oracle["mixed_eighteen_channel_control"];
    let all = expected["literal_bits_scan0"]
        .as_array()
        .unwrap()
        .iter()
        .chain(expected["literal_bits_scan1"].as_array().unwrap())
        .collect::<Vec<_>>();
    for on_demand in [false, true] {
        if on_demand {
            c = software(c);
        }
        let d = if on_demand {
            input_descriptor(&c)
        } else {
            descriptor(&c)
        };
        let block = Block {
            sample_index: 0,
            samples_per_channel: if on_demand { 1 } else { 2 },
            bits_le_hex: expected[if on_demand {
                "negative_one_scan_le_hex"
            } else {
                "all_words_le_hex"
            }]
            .as_str()
            .unwrap()
            .into(),
            status_code: 0,
        };
        assert_eq!(
            block.bits_le_hex.len() / 2,
            if on_demand { 144 } else { 288 }
        );
        let row = wire::block_reading(&c, &d, &block, &[]).unwrap();
        assert_eq!(row.value["event"], "mixed_analog_block");
        assert_eq!(
            &row.value["channel_measurements"].as_array().unwrap()[..13],
            oldrow.value["channel_measurements"].as_array().unwrap()
        );
        assert!(row.value.get("units").is_none());
        assert!(row.source_time.is_none());
        for (i, sample) in row.value["samples"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|scan| scan.as_array().unwrap())
            .enumerate()
        {
            let col = i % 18;
            let control = &expected["columns"][col];
            let key = control["finite_key"].as_str().unwrap();
            let word = u64::from_str_radix(all[i].as_str().unwrap(), 16).unwrap();
            assert_eq!(
                sample["ieee754_le_hex"],
                crate::types::hex(word.to_le_bytes())
            );
            if f64::from_bits(word).is_finite() {
                assert_eq!(sample[key].as_f64().unwrap().to_bits(), word);
            } else {
                assert!(sample.get(key).is_none());
            }
            assert!(sample.get("measurement").is_none());
            assert!(sample.get("units").is_none());
            let item = &row.value["channel_measurements"][col];
            for key in ["measurement", "quantity", "units"] {
                assert_eq!(item[key], control[key]);
            }
            assert_eq!(item["channel_index"], col);
        }
    }
}

#[test]
fn maximum_eighteen_family_descriptor_and_blocks_fit_unchanged_caps() {
    let mut value = serde_json::to_value(five_mixed()).unwrap();
    let templates = value["channels"].as_array().unwrap().clone();
    value["channels"] = Value::Array(
        (0..64)
            .map(|index| {
                let mut c = templates[index % 18].clone();
                c["physical"] = json!(format!("{}D/ai{index}", "D".repeat(248)));
                c
            })
            .collect(),
    );
    let mut c: Config = serde_json::from_value(value).unwrap();
    *clocked(&mut c).2 = 65_536;
    c.validate().unwrap();
    let mut n = notice();
    n.operation = "O".repeat(64);
    n.status_code = i32::MIN;
    n.diagnostic_hex = "ff".repeat(256);
    let notices = vec![n; wire::MAX_NOTICES];
    let d = descriptor(&c);
    let block = Block {
        sample_index: u64::MAX - 64,
        samples_per_channel: 64,
        bits_le_hex: "ffffffffffffefff".repeat(4096),
        status_code: i32::MIN,
    };
    let row = wire::block_reading(&c, &d, &block, &notices).unwrap();
    assert!(serde_json::to_vec(&row).unwrap().len() < MAX_ROW);
    wire::event_reading(&c, &d, "configured", &notices).unwrap();
    let mut frame = Vec::new();
    wire::write(
        &mut frame,
        &Response::Ready {
            descriptor: d,
            observed_ns: u64::MAX - 1,
            notices,
        },
    )
    .unwrap();
}
