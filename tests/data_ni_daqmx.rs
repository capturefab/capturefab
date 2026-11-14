//! NI configuration/preflight and independent C ABI acquisition through the CLI.
#[cfg(all(target_os = "linux", feature = "ni-daqmx"))]
#[path = "support/nidaqmx_clock.rs"]
mod native_clock;
mod support;
use serde_json::{Value, json};
use std::fs;
use support::cli::Home;

fn source(device: &str) -> Value {
    json!({"id":"daq","protocol":"ni_daqmx","signal":"voltage",
        "channels":[format!("{device}/ai0"),format!("{device}/ai1")],
        "terminal_config":"differential","min_volts":-10.0,"max_volts":10.0,
        "rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,
        "timeout_ms":1000,"cleanup_timeout_ms":500})
}
fn thermocouple_source(device: &str) -> Value {
    json!({"id":"daq","protocol":"ni_daqmx","measurement":"thermocouple","signal":"temperature",
    "rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,
    "timeout_ms":1000,"cleanup_timeout_ms":500,"channels":[
        {"physical":format!("{device}/ai0"),"thermocouple_type":"k","units":"deg_c",
         "min_temperature":-50.0,"max_temperature":500.0,"cjc":{"source":"built_in"}},
        {"physical":format!("{device}/ai1"),"thermocouple_type":"t","units":"deg_f",
         "min_temperature":-50.0,"max_temperature":300.0,"cjc":{"source":"constant","value":77.0}}
    ]})
}
fn current_source(device: &str) -> Value {
    json!({"id":"daq","protocol":"ni_daqmx","measurement":"current","signal":"current",
    "rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,
    "timeout_ms":1000,"cleanup_timeout_ms":500,"channels":[
        {"physical":format!("{device}/ai0"),"terminal_config":"differential",
         "min_amperes":0.0,"max_amperes":0.020,"shunt":{"source":"built_in"}},
        {"physical":format!("{device}/ai1"),"terminal_config":"rse",
         "min_amperes":-0.020,"max_amperes":0.020,"shunt":{"source":"external","ohms":250.0}}
    ]})
}
fn rtd_source(device: &str) -> Value {
    json!({"id":"daq","protocol":"ni_daqmx","measurement":"rtd","signal":"temperature",
    "rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,
    "timeout_ms":1000,"cleanup_timeout_ms":500,"channels":[
        {"physical":format!("{device}/ai0"),"rtd_type":"pt3851","units":"deg_c",
         "min_temperature":-50.0,"max_temperature":500.0,"resistance_config":"three_wire",
         "r0_ohms":100.0,"excitation":{"source":"internal","amperes":1.0/1024.0}},
        {"physical":format!("{device}/ai1"),"rtd_type":"pt3928","units":"deg_f",
         "min_temperature":-40.0,"max_temperature":300.0,"resistance_config":"four_wire",
         "r0_ohms":1000.0,"excitation":{"source":"external","amperes":1.0/512.0}}
    ]})
}

#[test]
fn rtd_configuration_requires_explicit_scientific_requests_before_any_source_effect() {
    let home = Home::new("ni-rtd-preflight");
    let mut invalid = Vec::new();
    for key in [
        "rtd_type",
        "units",
        "min_temperature",
        "max_temperature",
        "resistance_config",
        "r0_ohms",
        "excitation",
    ] {
        let mut config = rtd_source("Dev1");
        config["channels"][0].as_object_mut().unwrap().remove(key);
        invalid.push(config);
        let mut config = rtd_source("Dev1");
        config["channels"][0][key] = Value::Null;
        invalid.push(config);
    }
    for (key, value) in [
        ("rtd_type", json!("custom")),
        ("units", json!("ohms")),
        ("resistance_config", json!("auto")),
        ("r0_ohms", json!(0.0)),
        ("r0_ohms", json!(-100.0)),
        ("min_temperature", json!(500.0)),
        ("excitation", json!({"source":"none","amperes":1.0/1024.0})),
        ("excitation", json!({"source":"internal"})),
        ("excitation", json!({"source":"external","amperes":0.0})),
        ("excitation", json!({"source":"external","amperes":-1.0})),
        ("excitation", json!({"source":"internal","amperes":null})),
        (
            "excitation",
            json!({"source":"internal","amperes":1.0/1024.0,"ohms":100.0}),
        ),
        ("terminal_config", json!("differential")),
        ("cjc", json!({"source":"built_in"})),
        ("shunt", json!({"source":"built_in"})),
        ("coefficients", json!([1.0, 2.0, 3.0])),
        ("custom_scale", json!("MAX-scale")),
        ("teds", json!(true)),
    ] {
        let mut config = rtd_source("Dev1");
        config["channels"][0][key] = value;
        invalid.push(config);
    }
    let mut duplicate = rtd_source("Dev1");
    duplicate["channels"][1]["physical"] = json!("dev1/ai00");
    invalid.push(duplicate);
    let mut mixed = rtd_source("Dev1");
    mixed["channels"][1] = json!("Dev1/ai1");
    invalid.push(mixed);
    for config in invalid {
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                config,
            ],
        );
        assert_eq!(
            home.fails(
                2,
                &[
                    "data",
                    "record",
                    "--config",
                    "sources.json",
                    "--for",
                    "10ms",
                    "-o",
                    "run.jsonl"
                ]
            )["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

fn digital_source(device: &str) -> Value {
    json!({"id":"daq","protocol":"ni_daqmx","measurement":"digital_port","signal":"digital",
        "timing":"on_demand","poll_interval_ms":10,"timeout_ms":1000,"cleanup_timeout_ms":500,
        "channels":[format!("{device}/port3"),format!("{device}/port1"),format!("{device}/port12")]})
}
fn counter_source(device: &str, frequency: bool) -> Value {
    let channel = if frequency {
        json!({"physical":format!("{device}/ctr3"),"min_hz":1.0/8.0,"max_hz":8192.0,"edge":"falling",
            "method":"large_range_two_counter","measurement_time_seconds":1.0/16.0,"divisor":7})
    } else if device == "CountUp" {
        json!({"physical":format!("{device}/ctr2"),"edge":"rising","initial_count":17,"direction":"up"})
    } else {
        json!({"physical":format!("{device}/ctr7"),"edge":"falling","initial_count":2_147_483_649_u32,"direction":"down"})
    };
    json!({"id":"daq","protocol":"ni_daqmx","measurement":if frequency {"counter_frequency"} else {"counter_count"},
        "signal":"counter","timing":"on_demand","poll_interval_ms":10,"timeout_ms":1000,"cleanup_timeout_ms":500,"channels":[channel]})
}
fn invalid_input_before_effects(home: &Home, configurations: Vec<Value>) {
    for config in configurations {
        write_config(
            home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                config,
            ],
        );
        assert_eq!(
            home.fails(
                2,
                &[
                    "data",
                    "record",
                    "--config",
                    "sources.json",
                    "--for",
                    "10ms",
                    "-o",
                    "run.jsonl"
                ]
            )["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}
#[test]
fn digital_port_configuration_requires_individual_ports_and_explicit_software_timing_before_effects()
 {
    let home = Home::new("ni-digital-preflight");
    let good = digital_source("Dev1");
    let mut invalid = Vec::new();
    for channels in [
        json!([]),
        json!(["Dev1/port0:3"]),
        json!(["Dev1/port0/line0"]),
        json!(["Dev1/ai0"]),
        json!(["Dev1/port0,Dev1/port1"]),
        json!(["MAXalias"]),
        json!(["Dev1/port1", "dev1/port01"]),
        json!((0..65).map(|i| format!("Dev1/port{i}")).collect::<Vec<_>>()),
    ] {
        let mut c = good.clone();
        c["channels"] = channels;
        invalid.push(c);
    }
    for key in ["timing", "poll_interval_ms"] {
        let mut c = good.clone();
        c.as_object_mut().unwrap().remove(key);
        invalid.push(c);
    }
    for (key, value) in [
        ("timing", json!("continuous")),
        ("poll_interval_ms", json!(0)),
        ("poll_interval_ms", json!(3_600_001)),
        ("poll_interval_ms", Value::Null),
        ("rate_hz", Value::Null),
        ("max_samples_per_channel", Value::Null),
        ("host_buffer_samples_per_channel", Value::Null),
        ("terminal_config", Value::Null),
        ("line_mask", json!(255)),
        ("clock_source", json!("PFI0")),
    ] {
        let mut c = good.clone();
        c[key] = value;
        invalid.push(c);
    }
    invalid_input_before_effects(&home, invalid);
}
#[test]
fn counter_configuration_requires_one_typed_counter_exact_scalars_and_explicit_input_terminal() {
    let home = Home::new("ni-counter-preflight");
    let mut invalid = Vec::new();
    for frequency in [false, true] {
        let good = counter_source("Dev1", frequency);
        for channels in [
            json!([]),
            json!(["Dev1/ctr0"]),
            json!([good["channels"][0], good["channels"][0]]),
        ] {
            let mut c = good.clone();
            c["channels"] = channels;
            invalid.push(c);
        }
        for physical in [
            "Dev1/ctr0:2",
            "Dev1/port0",
            "Dev1/ctr0,Dev1/ctr1",
            "MAXcounter",
        ] {
            let mut c = good.clone();
            c["channels"][0]["physical"] = json!(physical);
            invalid.push(c);
        }
        for terminal in [
            Value::Null,
            json!(""),
            json!("/Dev1/PFI0,PFI1"),
            json!("/Dev1/PFI0\0"),
            json!("/Dev1/*"),
        ] {
            let mut c = good.clone();
            c["channels"][0]["input_terminal"] = terminal;
            invalid.push(c);
        }
        for key in ["timing", "poll_interval_ms"] {
            let mut c = good.clone();
            c.as_object_mut().unwrap().remove(key);
            invalid.push(c);
        }
        for key in [
            "rate_hz",
            "max_samples_per_channel",
            "host_buffer_samples_per_channel",
            "min_volts",
        ] {
            let mut c = good.clone();
            c[key] = Value::Null;
            invalid.push(c);
        }
        let mut continuous = good.clone();
        continuous["timing"] = json!("continuous");
        invalid.push(continuous);
        let controls = if frequency {
            vec![
                ("min_hz", json!(-1.0)),
                ("max_hz", json!(0.125)),
                ("method", json!("automatic")),
                ("measurement_time_seconds", json!(-1.0)),
                ("divisor", json!(-1)),
                ("divisor", json!(4_294_967_296_u64)),
                ("divisor", json!(1.5)),
                ("units", json!("ticks")),
            ]
        } else {
            vec![
                ("initial_count", json!(-1)),
                ("initial_count", json!(4_294_967_296_u64)),
                ("initial_count", json!(0.5)),
                ("edge", json!("both")),
                ("direction", json!("external")),
            ]
        };
        for (key, value) in controls {
            let mut c = good.clone();
            c["channels"][0][key] = value;
            invalid.push(c);
        }
    }
    invalid_input_before_effects(&home, invalid);
}

fn mixed_source(device: &str) -> Value {
    json!({"id":"daq","protocol":"ni_daqmx","measurement":"mixed","signal":"mixed_ai",
    "rate_hz":1000.0,"max_samples_per_channel":2,"host_buffer_samples_per_channel":8192,
    "timeout_ms":1000,"cleanup_timeout_ms":500,"channels":[
      {"measurement":"voltage","physical":format!("{device}/ai0"),"terminal_config":"differential","min_volts":-10.0,"max_volts":10.0},
      {"measurement":"thermocouple","physical":format!("{device}/ai1"),"thermocouple_type":"k","units":"deg_c","min_temperature":-50.0,"max_temperature":500.0,"cjc":{"source":"built_in"}},
      {"measurement":"current","physical":format!("{device}/ai2"),"terminal_config":"nrse","min_amperes":-1.0/32.0,"max_amperes":1.0/32.0,"shunt":{"source":"external","ohms":250.0}},
      {"measurement":"rtd","physical":format!("{device}/ai3"),"rtd_type":"pt3928","units":"deg_f","min_temperature":-40.0,"max_temperature":300.0,"resistance_config":"four_wire","r0_ohms":1000.0,"excitation":{"source":"external","amperes":1.0/512.0}}
    ]})
}

#[test]
fn mixed_configuration_requires_per_column_types_and_rejects_effectful_global_guessing() {
    let home = Home::new("ni-mixed-preflight");
    let mut invalid = Vec::new();
    for tag in [Value::Null, json!("counter"), json!("mixed")] {
        let mut c = mixed_source("Dev1");
        c["channels"][0]["measurement"] = tag;
        invalid.push(c);
    }
    let mut missing = mixed_source("Dev1");
    missing["channels"][0]
        .as_object_mut()
        .unwrap()
        .remove("measurement");
    invalid.push(missing);
    for (column, key, value) in [
        (0, "terminal_config", Value::Null),
        (0, "max_volts", json!(-10.0)),
        (1, "units", json!("volts")),
        (1, "cjc", json!({"source":"built_in","value":0.0})),
        (2, "shunt", json!({"source":"external","ohms":0.0})),
        (3, "r0_ohms", json!(0.0)),
        (3, "excitation", json!({"source":"internal"})),
    ] {
        let mut c = mixed_source("Dev1");
        c["channels"][column][key] = value;
        invalid.push(c);
    }
    for key in ["terminal_config", "min_volts", "max_volts"] {
        let mut c = mixed_source("Dev1");
        c[key] = Value::Null;
        invalid.push(c);
    }
    let mut duplicate = mixed_source("Dev1");
    duplicate["channels"][1]["physical"] = json!("dev1/ai00");
    invalid.push(duplicate);
    let mut untyped = mixed_source("Dev1");
    untyped["channels"] = json!(["Dev1/ai0"]);
    invalid.push(untyped);
    let mut empty = mixed_source("Dev1");
    empty["channels"] = json!([]);
    invalid.push(empty);
    for c in invalid {
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                c,
            ],
        );
        assert_eq!(
            home.fails(
                2,
                &[
                    "data",
                    "record",
                    "--config",
                    "sources.json",
                    "--for",
                    "10ms",
                    "-o",
                    "run.jsonl"
                ]
            )["code"],
            "usage"
        );
        assert!(!home.path("run.jsonl").exists());
    }
}

fn on_demand(mut config: Value, interval: u64) -> Value {
    let fields = config.as_object_mut().unwrap();
    for key in [
        "rate_hz",
        "max_samples_per_channel",
        "host_buffer_samples_per_channel",
    ] {
        fields.remove(key);
    }
    config["timing"] = json!("on_demand");
    config["poll_interval_ms"] = json!(interval);
    config
}

#[test]
fn on_demand_configuration_rejects_continuous_fields_before_any_source_effect() {
    let home = Home::new("ni-software-preflight");
    let mut invalid = Vec::new();
    for key in [
        "rate_hz",
        "max_samples_per_channel",
        "host_buffer_samples_per_channel",
    ] {
        for value in [Value::Null, json!(1)] {
            let mut config = on_demand(source("Dev1"), 250);
            config[key] = value;
            invalid.push(config);
        }
    }
    for value in [
        json!(0),
        json!(3_600_001),
        json!(1.5),
        Value::Null,
        json!("250"),
    ] {
        let mut config = on_demand(source("Dev1"), 250);
        config["poll_interval_ms"] = value;
        invalid.push(config);
    }
    let mut missing = on_demand(source("Dev1"), 250);
    missing.as_object_mut().unwrap().remove("poll_interval_ms");
    invalid.push(missing);
    let mut unknown = on_demand(source("Dev1"), 250);
    unknown["timing"] = json!("automatic_fallback");
    invalid.push(unknown);
    let mut continuous = source("Dev1");
    continuous["poll_interval_ms"] = json!(250);
    invalid.push(continuous);
    for config in invalid {
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                config,
            ],
        );
        let error = home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

// Hand-declared configurations bind frozen889c/1a3e argument controls, not native code.
fn engineering_source(family: usize, device: &str) -> Value {
    let physical = |index| format!("{device}/ai{index}");
    let strain = |index: usize| {
        let b = index % 2 == 1;
        let mut channel = json!({"physical":physical(index),"min_strain":if b {-1.0/128.0}else{-1.0/64.0},
            "max_strain":if b {1.0/32.0}else{3.0/128.0},"bridge_type":if b {"full_bridge_iii"}else{"half_bridge_i"},
            "excitation":{"source":if b {"external"}else{"internal"},"volts":if b {5.0}else{2.5}},
            "gage_factor":if b {2.25}else{2.0},"initial_bridge_volts":if b {-1.0/512.0}else{1.0/1024.0},
            "nominal_gage_ohms":if b {350.0}else{120.0},"poisson_ratio":if b {0.375}else{0.25},"lead_wire_ohms":if b {0.75}else{0.5}});
        if !b {
            channel["coupling"] = json!("ac");
        }
        channel
    };
    let ratio = |index: usize| {
        let b = index % 2 == 1;
        json!({"physical":physical(index),"min_ratio":if b {-2.5}else{-1.0/32.0},"max_ratio":if b {3.75}else{1.0/64.0},
            "units":if b {"millivolts_per_volt"}else{"volts_per_volt"},"bridge_config":if b {"quarter_bridge"}else{"full_bridge"},
            "excitation":{"source":if b {"external"}else{"internal"},"volts":if b {5.0}else{2.5}},
            "nominal_bridge_ohms":if b {1000.0}else{350.0},"coupling":if b {"ground"}else{"dc"}})
    };
    let accel = |index: usize| {
        json!({"physical":physical(index),"terminal_config":(["pseudodifferential","default","differential"][index]),
        "min_acceleration":([-64.0,-32.0,-128.0][index]),"max_acceleration":([128.0,256.0,512.0][index]),"units":(["g","m_s2","in_s2"][index]),
        "sensitivity":([125.0,0.125,250.0][index]),"sensitivity_units":(["mv_per_g","v_per_g","mv_per_g"][index]),
        "excitation":{"source":(["internal","external","none"][index]),"amperes":([1.0/512.0,1.0/256.0,1.0/1024.0][index])},"coupling":(["ac","dc","ground"][index])})
    };
    let channels = match family {
        0 => json!([strain(0), strain(1)]),
        1 => json!([ratio(0), ratio(1)]),
        2 => json!([accel(0), accel(1), accel(2)]),
        3 => {
            let mut channels = mixed_source(device)["channels"].as_array().unwrap().clone();
            for (mut channel, measurement) in [
                (strain(0), "strain"),
                (ratio(0), "bridge_ratio"),
                (accel(0), "acceleration"),
            ] {
                channel["physical"] = json!(physical(channels.len()));
                channel["measurement"] = json!(measurement);
                channels.push(channel);
            }
            Value::Array(channels)
        }
        4 => {
            json!([{"physical":format!("{device}/ctr2"),"min_seconds":1.0/1024.0,"max_seconds":0.125,"edge":"falling",
            "method":"high_frequency_two_counter","measurement_time_seconds":1.0/32.0,"divisor":4}])
        }
        5 => {
            json!([{"physical":format!("{device}/ctr3"),"min_seconds":1.0/2048.0,"max_seconds":0.1875,"starting_edge":"rising"}])
        }
        6 => {
            json!([{"physical":format!("{device}/ctr5"),"decoding":"x4","units":"degrees","pulses_per_rev":4099,"initial_angle":-2.5,
            "z_index":{"enabled":true,"value":1.75,"phase":"a_low_b_high"},"a_input_terminal":format!("/{device}/PFI1"),
            "b_input_terminal":format!("/{device}/PFI3"),"z_input_terminal":format!("/{device}/PFI9")}])
        }
        7 => {
            json!([{"physical":format!("{device}/ctr6"),"decoding":"x2","units":"meters","distance_per_pulse":1.0/1024.0,"initial_position":1.25,
            "z_index":{"enabled":true,"value":1.375,"phase":"a_high_b_low"}}])
        }
        _ => unreachable!(),
    };
    let config = json!({"id":"daq","protocol":"ni_daqmx","measurement":(["strain","bridge_ratio","acceleration","mixed","counter_period","counter_pulse_width","counter_angular_position","counter_linear_position"][family]),
        "signal":"engineering","channels":channels,"rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,"timeout_ms":1000,"cleanup_timeout_ms":500});
    if family >= 4 {
        on_demand(config, 10)
    } else {
        config
    }
}

#[test]
fn engineering_configuration_is_closed_and_scientific_before_any_source_effect() {
    let home = Home::new("ni-engineering-preflight");
    let mut invalid = Vec::new();
    for family in 0..8 {
        let mut config = engineering_source(family, "Dev1");
        config["channels"][0]["unobserved_calibration"] = json!(true);
        invalid.push(config);
        let mut config = engineering_source(family, "Dev1");
        config["channels"][0]["physical"] = json!("Dev1/ai0:3");
        invalid.push(config);
        if family < 3 {
            let mut config = engineering_source(family, "Dev1");
            config["channels"][0]["coupling"] = Value::Null;
            invalid.push(config);
            let mut config = engineering_source(family, "Dev1");
            config["channels"][0]["excitation"]["source"] = json!("none");
            config["channels"][0]["excitation"][if family == 2 { "amperes" } else { "volts" }] =
                json!(-1.0);
            invalid.push(config);
        }
        if family >= 4 {
            let mut config = engineering_source(family, "Dev1");
            config["rate_hz"] = Value::Null;
            invalid.push(config);
        }
    }
    for field in ["enabled", "value", "phase"] {
        let mut config = engineering_source(6, "Dev1");
        config["channels"][0]["z_index"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        invalid.push(config);
    }
    let mut disabled = engineering_source(6, "Dev1");
    disabled["channels"][0]["z_index"]["enabled"] = json!(false);
    invalid.push(disabled);
    let mut dynamic = engineering_source(4, "Dev1");
    dynamic["channels"][0]["method"] = json!("dynamic_averaging");
    invalid.push(dynamic);
    invalid_input_before_effects(&home, invalid);
}

// Literal argument roles were frozen in1b53/ddea before any new native/C code.
fn resistive_source(family: usize, device: &str) -> Value {
    let channel = |kind: usize, index: usize| {
        let b = index % 2 == 1;
        let physical = format!("{device}/ai{index}");
        if kind == 0 {
            let mut row = json!({"physical":physical,"min_ohms":if b{32.0}else{0.0},"max_ohms":if b{2048.0}else{512.0},
            "resistance_config":if b{"four_wire"}else{"three_wire"},"excitation":{"source":if b{"external"}else{"internal"},"amperes":if b{3.0/2048.0}else{1.0/1024.0}}});
            if !b {
                row["coupling"] = json!("ac");
            }
            return row;
        }
        if kind <= 2 {
            let current = kind == 1;
            let mut row = json!({"physical":physical,"min_temperature":if current{if b{-64.0}else{-32.0}}else if b{0.0}else{-16.0},
                "max_temperature":if b{512.0}else if current{256.0}else{128.0},"units":if b{if current{"deg_f"}else{"kelvin"}}else{"deg_c"},
                "resistance_config":if b{"two_wire"}else if current{"three_wire"}else{"four_wire"},
                "coefficients":{"a":if b{5.0/4096.0}else{9.0/8192.0},"b":if b{3.0/16384.0}else{1.0/4096.0},"c":if b{3.0/33_554_432.0}else{1.0/8_388_608.0}}});
            row["excitation"] = if current {
                json!({"source":if b{"external"}else{"internal"},"amperes":if b{3.0/2048.0}else{1.0/1024.0}})
            } else {
                json!({"source":if b{"internal"}else{"external"},"volts":if b{1.25}else{2.5}})
            };
            if current {
                row["coupling"] = json!(if b { "ground" } else { "dc" });
            } else {
                row["r1_ohms"] = json!(if b { 12_000.0 } else { 10_000.0 });
                if b {
                    row["coupling"] = json!("ac");
                }
            }
            return row;
        }
        let k = kind - 3;
        let min = [[-64.0, -128.0], [-131_072.0, -0.125], [-8.0, -16.0]][k][usize::from(b)];
        let max = [[1024.0, 512.0], [524_288.0, 64.0], [64.0, 64.0]][k][usize::from(b)];
        let units = [
            ["newtons", "kilogram_force"],
            ["pascals", "psi"],
            ["newton_meters", "foot_pounds"],
        ][k][usize::from(b)];
        let bridge = [
            ["full_bridge", "half_bridge"],
            ["half_bridge", "full_bridge"],
            ["quarter_bridge", "full_bridge"],
        ][k][usize::from(b)];
        let source = [
            ["internal", "external"],
            ["external", "internal"],
            ["internal", "external"],
        ][k][usize::from(b)];
        let volts = [[2.5, 5.0], [5.0, 1.25], [1.25, 2.5]][k][usize::from(b)];
        let ohms = [[350.0, 1000.0], [1000.0, 350.0], [120.0, 350.0]][k][usize::from(b)];
        let electrical_units = if (k == 1) == b {
            "volts_per_volt"
        } else {
            "millivolts_per_volt"
        };
        let first_e = [
            [-1.0 / 1024.0, -1.0 / 1024.0],
            [-1.0 / 2048.0, 0.0],
            [-1.0 / 4096.0, -3.0 / 2048.0],
        ][k][usize::from(b)];
        let second_e = [
            [3.0 / 512.0, 3.0 / 512.0],
            [1.0 / 256.0, 1.0 / 512.0],
            [3.0 / 1024.0, 1.0 / 1024.0],
        ][k][usize::from(b)];
        let first_p = [[-4.0, 64.0], [-0.5, 16.0], [-16.0, 1.5]][k][usize::from(b)];
        let second_p = [[128.0, -32.0], [3.0, 2048.0], [512.0, -1.75]][k][usize::from(b)];
        let physical_units = [
            ["pounds", "newtons"],
            ["bar", "pascals"],
            ["inch_ounces", "newton_meters"],
        ][k][usize::from(b)];
        let mut row = json!({"physical":physical,"units":units,"bridge_config":bridge,"excitation":{"source":source,"volts":volts},"nominal_bridge_ohms":ohms,
            "calibration":{"electrical_units":electrical_units,"physical_units":physical_units,"first":{"electrical":first_e,"physical":first_p},"second":{"electrical":second_e,"physical":second_p}}});
        row[if k == 0 {
            "min_force"
        } else if k == 1 {
            "min_pressure"
        } else {
            "min_torque"
        }] = json!(min);
        row[if k == 0 {
            "max_force"
        } else if k == 1 {
            "max_pressure"
        } else {
            "max_torque"
        }] = json!(max);
        if k == 0 && !b {
            row["coupling"] = json!("dc");
        }
        if k == 1 {
            row["coupling"] = json!(if b { "ac" } else { "ground" });
        }
        if k == 2 && b {
            row["coupling"] = json!("dc");
        }
        row
    };
    let channels = if family < 6 {
        json!([channel(family, 0), channel(family, 1)])
    } else {
        let mut rows = engineering_source(3, device)["channels"]
            .as_array()
            .unwrap()
            .clone();
        for kind in 0..6 {
            let mut row = channel(kind, 0);
            row["physical"] = json!(format!("{device}/ai{}", rows.len()));
            row["measurement"] = json!(
                [
                    "resistance",
                    "thermistor_current",
                    "thermistor_voltage",
                    "force_bridge",
                    "pressure_bridge",
                    "torque_bridge"
                ][kind]
            );
            rows.push(row);
        }
        json!(rows)
    };
    json!({"id":"daq","protocol":"ni_daqmx","measurement":(["resistance","thermistor_current","thermistor_voltage","force_bridge","pressure_bridge","torque_bridge","mixed"][family]),"signal":"resistive",
        "channels":channels,"rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,"timeout_ms":1000,"cleanup_timeout_ms":500})
}

#[test]
fn resistive_and_physical_bridge_configuration_is_closed_before_any_effect() {
    let home = Home::new("ni-resistive-preflight");
    let mut invalid = Vec::new();
    for family in 0..7 {
        let mut c = resistive_source(family, "Dev1");
        c["channels"][0]["unobserved_calibration"] = json!(true);
        invalid.push(c);
        let mut c = resistive_source(family, "Dev1");
        c["channels"][0]["coupling"] = Value::Null;
        invalid.push(c);
        if family < 3 {
            let mut c = resistive_source(family, "Dev1");
            c["channels"][0]["rtd_type"] = json!("pt3851");
            invalid.push(c);
        }
        if matches!(family, 1 | 2) {
            let mut c = resistive_source(family, "Dev1");
            c["channels"][0]["coefficients"]
                .as_object_mut()
                .unwrap()
                .remove("b");
            invalid.push(c);
            let mut c = resistive_source(family, "Dev1");
            c["channels"][0]["coefficients"]["a"] = json!("NaN");
            invalid.push(c);
        }
        if (3..6).contains(&family) {
            for physical in [false, true] {
                let mut c = resistive_source(family, "Dev1");
                if physical {
                    c["channels"][0]["calibration"]["physical_units"] =
                        json!(if family == 3 { "pascals" } else { "newtons" });
                } else {
                    c["channels"][0]["calibration"]["first"]["electrical"] = json!(0.0);
                    c["channels"][0]["calibration"]["second"]["electrical"] = json!(-0.0);
                }
                invalid.push(c);
            }
        }
    }
    for value in [json!(0.0), json!(-1.0), Value::Null] {
        let mut c = resistive_source(2, "Dev1");
        c["channels"][0]["r1_ohms"] = value;
        invalid.push(c);
    }
    let mut c = resistive_source(1, "Dev1");
    c["channels"][0]["r1_ohms"] = json!(10000);
    invalid.push(c);
    let mut c = resistive_source(0, "Dev1");
    c["channels"][0]["units"] = json!("ohms");
    invalid.push(c);
    invalid_input_before_effects(&home, invalid);
}

// Independent five-profile literals879f/0032 predate all new native/C authoring.
const FIVE_TAGS: [&str; 6] = [
    "velocity_iepe",
    "force_iepe",
    "charge_acceleration",
    "lvdt",
    "rvdt",
    "mixed",
];
#[cfg(all(target_os = "linux", feature = "ni-daqmx"))]
const FIVE_DEVICES: [&str; 6] = [
    "Velocity",
    "IepeForce",
    "Charge",
    "Lvdt",
    "Rvdt",
    "FiveMixed",
];
fn five_oracle(position: bool) -> Value {
    serde_json::from_str(if position {
        include_str!("fixtures/nidaqmx/analog-position.json")
    } else {
        include_str!("fixtures/nidaqmx/dynamic-ai.json")
    })
    .unwrap()
}
fn five_source(family: usize, device: &str) -> Value {
    let channel = |kind: usize, ordinal: usize, pair: usize| {
        let oracle = five_oracle(kind >= 3);
        let case = &oracle["ordered_constructor_pairs"][FIVE_TAGS[kind]][pair];
        let number = |role: &str| {
            oracle["manual_argument_words"][case["float_argument_words"][role].as_str().unwrap()]["decimal"].as_str().unwrap().parse::<f64>().unwrap()
        };
        let mut row = json!({"physical":format!("{device}/ai{ordinal}"), "units":case["units"],
            "sensitivity":number("sensitivity"),"sensitivity_units":case["sensitivity_units"]});
        let (min, max) = [
            ("min_velocity", "max_velocity"),
            ("min_force", "max_force"),
            ("min_acceleration", "max_acceleration"),
            ("min_position", "max_position"),
            ("min_angle", "max_angle"),
        ][kind];
        row[min] = json!(number("min"));
        row[max] = json!(number("max"));
        if kind < 3 {
            row["terminal_config"] = case["terminal_config"].clone();
        }
        if kind != 2 {
            let amount = if kind < 2 { "amperes" } else { "volts" };
            row["excitation"] = json!({"source":case["excitation_source"]});
            row["excitation"][amount] = json!(number(amount));
        }
        if kind >= 3 {
            row["excitation"]["frequency_hz"] = json!(number("frequency_hz"));
            row["ac_wire_mode"] = case["ac_wire_mode"].clone();
        }
        if case["coupling"] != "omitted" {
            row["coupling"] = case["coupling"].clone();
        }
        row
    };
    let channels = if family < 5 {
        json!([channel(family, 0, 0), channel(family, 1, 1)])
    } else {
        let mut rows = resistive_source(6, device)["channels"]
            .as_array()
            .unwrap()
            .clone();
        for (kind, tag) in FIVE_TAGS.iter().take(5).enumerate() {
            let mut row = channel(kind, rows.len(), 0);
            row["measurement"] = json!(tag);
            rows.push(row);
        }
        json!(rows)
    };
    json!({"id":"daq","protocol":"ni_daqmx","measurement":FIVE_TAGS[family],"signal":"dynamic",
        "channels":channels,"rate_hz":1000.0,"max_samples_per_channel":3,"host_buffer_samples_per_channel":8192,
        "timeout_ms":1000,"cleanup_timeout_ms":500})
}
#[test]
fn five_analog_profiles_reject_missing_null_or_unqualified_science_before_any_source_effect() {
    let home = Home::new("ni-five-preflight");
    let mut invalid = Vec::new();
    for family in 0..5 {
        let source = five_source(family, "Dev1");
        for field in source["channels"][0].as_object().unwrap().keys() {
            if field == "coupling" {
                continue;
            }
            let mut c = source.clone();
            c["channels"][0].as_object_mut().unwrap().remove(field);
            invalid.push(c);
            let mut c = source.clone();
            c["channels"][0][field] = Value::Null;
            invalid.push(c);
        }
        for (field, value) in [
            ("coupling", Value::Null),
            ("calibration", json!({})),
            ("sensitivity", json!("NaN")),
            ("units", json!("volts")),
        ] {
            let mut c = source.clone();
            c["channels"][0][field] = value;
            invalid.push(c);
        }
        let mut c = source.clone();
        c["channels"][1]["physical"] = json!("dev1/ai00");
        invalid.push(c);
        let mut c = source.clone();
        c["channels"][0][if family < 3 {
            "excitation"
        } else {
            "terminal_config"
        }] = Value::Null;
        if family == 2 || family >= 3 {
            invalid.push(c);
        }
        if family != 2 {
            let amount = if family < 2 { "amperes" } else { "volts" };
            for (source, value) in [("internal", 0.0), ("external", -1.0), ("none", -0.125)] {
                let mut c = five_source(family, "Dev1");
                c["channels"][0]["excitation"]["source"] = json!(source);
                c["channels"][0]["excitation"][amount] = json!(value);
                invalid.push(c);
            }
        }
        if family >= 3 {
            for source in ["internal", "external", "none"] {
                for hz in [0.0, -0.0, -0.125] {
                    let mut c = five_source(family, "Dev1");
                    c["channels"][0]["excitation"]["source"] = json!(source);
                    c["channels"][0]["excitation"]["frequency_hz"] = json!(hz);
                    invalid.push(c);
                }
            }
        }
    }
    let mut c = five_source(5, "Dev1");
    c["channels"][17]
        .as_object_mut()
        .unwrap()
        .remove("measurement");
    invalid.push(c);
    invalid_input_before_effects(&home, invalid);
    #[cfg(not(feature = "ni-daqmx"))]
    for family in 0..6 {
        write_config(&home, &[five_source(family, "NeverLoaded")]);
        let error = home.fails(
            5,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "unsupported.jsonl",
            ],
        );
        assert_eq!(error["code"], "unsupported");
        assert!(!home.path("unsupported.jsonl").exists());
    }
}

fn write_config(home: &Home, sources: &[Value]) {
    fs::write(
        home.path("sources.json"),
        serde_json::to_vec(&json!({"version":1,"sources":sources})).unwrap(),
    )
    .unwrap();
}
#[cfg(all(target_os = "linux", feature = "ni-daqmx"))]
fn rows(home: &Home) -> Vec<Value> {
    fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[cfg(all(target_os = "linux", feature = "ni-daqmx"))]
fn blocks(rows: &[Value]) -> Vec<&Value> {
    rows.iter()
        .filter(|row| row["kind"] == "sample" && row["value"]["event"] == "analog_voltage_block")
        .collect()
}
#[cfg(all(target_os = "linux", feature = "ni-daqmx"))]
fn footer(rows: &[Value], status: &str) {
    let end = rows.last().unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["status"], status);
    assert_eq!(
        end["samples"].as_u64().unwrap(),
        u64::try_from(rows.iter().filter(|r| r["kind"] == "sample").count()).unwrap()
    );
}

#[test]
fn ni_catalogue_and_archives_remain_readable_without_an_installed_driver() {
    let home = Home::new("ni-catalogue");
    let catalogue = home.ok(&["data", "protocols"]);
    assert!(
        catalogue["protocols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["protocol"] == "ni_daqmx" && p["implementation"] == "native")
    );
    let header = json!({"kind":"header","version":1,"run_id":"00000000000000000000000000000000",
        "started_at":"2026-01-01T00:00:00Z","clock":"host_monotonic_receive","sources":[{"id":"daq","protocol":"ni_daqmx"}]});
    let end = json!({"kind":"end","samples":0,"status":"complete","elapsed_ns":0});
    fs::write(home.path("native.jsonl"), format!("{header}\n{end}\n")).unwrap();
    assert_eq!(
        home.ok(&["data", "export", "native.jsonl", "-o", "native.csv"])["complete"],
        true
    );
}

#[test]
fn invalid_ni_configuration_fails_before_an_earlier_serial_source_opens() {
    let home = Home::new("ni-preflight");
    for invalid in [
        json!(["Dev1/ai0:63"]),
        json!(["Dev1/ai0", "dev1/ai00"]),
        json!([]),
        json!(["Dev1/ao0"]),
    ] {
        let mut ni = source("Dev1");
        ni["channels"] = invalid;
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                ni,
            ],
        );
        let error = home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert!(error["message"].as_str().unwrap().contains("NI-DAQmx"));
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn thermocouple_configuration_is_closed_before_output_or_earlier_source_effects() {
    let home = Home::new("ni-tc-preflight");
    let mut invalid = Vec::new();
    for (field, value) in [
        ("thermocouple_type", json!("unknown")),
        ("units", json!("volts")),
        ("cjc", json!({"source":"channel","channel":"MAX-CJC"})),
        ("cjc", json!({"source":"constant"})),
        ("cjc", json!({"source":"built_in","value":25.0})),
        ("cjc", json!({"source":"constant","value":null})),
        ("min_temperature", json!(500.0)),
        ("terminal_config", json!("arbitrary")),
    ] {
        let mut config = thermocouple_source("Dev1");
        config["channels"][0][field] = value;
        invalid.push(config);
    }
    let mut mixed = thermocouple_source("Dev1");
    mixed["channels"][1] = json!("Dev1/ai1");
    invalid.push(mixed);
    let mut duplicate = thermocouple_source("Dev1");
    duplicate["channels"][1]["physical"] = json!("dev1/ai00");
    invalid.push(duplicate);
    let mut voltage_range = thermocouple_source("Dev1");
    voltage_range["min_volts"] = json!(-10.0);
    invalid.push(voltage_range);
    for config in invalid {
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                config,
            ],
        );
        let error = home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[test]
fn current_configuration_is_closed_before_output_or_earlier_source_effects() {
    let home = Home::new("ni-current-preflight");
    let mut invalid = Vec::new();
    for shunt in [
        json!({"source":"driver_choice"}),
        json!({"source":"external"}),
        json!({"source":"external","ohms":0.0}),
        json!({"source":"external","ohms":-1.0}),
        json!({"source":"external","ohms":null}),
        json!({"source":"built_in","ohms":249.0}),
    ] {
        let mut config = current_source("Dev1");
        config["channels"][0]["shunt"] = shunt;
        invalid.push(config);
    }
    for (key, value) in [
        ("units", json!("milliamps")),
        ("min_amperes", json!(0.020)),
        ("terminal_config", json!("arbitrary")),
        ("max_amperes", Value::Null),
    ] {
        let mut config = current_source("Dev1");
        config["channels"][0][key] = value;
        invalid.push(config);
    }
    let mut missing_terminal = current_source("Dev1");
    missing_terminal["channels"][0]
        .as_object_mut()
        .unwrap()
        .remove("terminal_config");
    invalid.push(missing_terminal);
    let mut duplicate = current_source("Dev1");
    duplicate["channels"][1]["physical"] = json!("dev1/ai00");
    invalid.push(duplicate);
    let mut voltage_range = current_source("Dev1");
    voltage_range["min_volts"] = json!(-10.0);
    invalid.push(voltage_range);
    let mut empty = current_source("Dev1");
    empty["channels"] = json!([]);
    invalid.push(empty);
    for config in invalid {
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                config,
            ],
        );
        let error = home.fails(
            2,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert_eq!(error["code"], "usage");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[cfg(not(all(feature = "ni-daqmx", any(target_os = "linux", windows))))]
#[test]
fn unavailable_ni_feature_or_host_is_preflighted_before_earlier_source_effects() {
    let home = Home::new("ni-host-preflight");
    for ni in [
        source("Dev1"),
        thermocouple_source("Dev1"),
        current_source("Dev1"),
        rtd_source("Dev1"),
        mixed_source("Dev1"),
        digital_source("Dev1"),
        counter_source("Dev1", false),
        counter_source("Dev1", true),
        on_demand(source("Dev1"), 250),
    ] {
        write_config(
            &home,
            &[
                json!({"id":"earlier","protocol":"serial_jsonl","device":"/capturefab-fixture/nonexistent-serial"}),
                ni,
            ],
        );
        let error = home.fails(
            5,
            &[
                "data",
                "record",
                "--config",
                "sources.json",
                "--for",
                "10ms",
                "-o",
                "run.jsonl",
            ],
        );
        assert_eq!(error["code"], "unsupported");
        assert!(!home.path("run.jsonl").exists());
    }
}

#[cfg(all(target_os = "linux", feature = "ni-daqmx"))]
mod linux {
    use super::*;
    use std::{
        collections::BTreeSet,
        process::{Command, Stdio},
        sync::OnceLock,
        time::{Duration, Instant},
    };
    use support::cli::Run;

    pub(super) fn libraries() -> &'static [Vec<u8>; 3] {
        static LIBS: OnceLock<[Vec<u8>; 3]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("fixture.c");
                let so = temp.path().join("fixture.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant == 1 {
                    cc.arg("-DNIDAQMX_FIXTURE_CREATE_FAIL_NONNULL=1");
                }
                if variant == 2 {
                    cc.arg("-DNIDAQMX_FIXTURE_OMIT_START=1");
                }
                let result = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn thermocouple_libraries() -> &'static [Vec<u8>; 3] {
        static LIBS: OnceLock<[Vec<u8>; 3]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("thermocouple.c");
                let so = temp.path().join("thermocouple.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let definition = [
                    "-DNIDAQMX_FIXTURE_OMIT_TC=1",
                    "-DNIDAQMX_FIXTURE_OMIT_VOLTAGE=1",
                    "-DNIDAQMX_FIXTURE_OMIT_TC_TERMINAL=1",
                ][variant];
                let output = Command::new("cc")
                    .args([
                        "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                        definition,
                    ])
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn current_libraries() -> &'static [Vec<u8>; 2] {
        static LIBS: OnceLock<[Vec<u8>; 2]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("current.c");
                let so = temp.path().join("current.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant == 0 {
                    cc.arg("-DNIDAQMX_FIXTURE_OMIT_CURRENT=1");
                } else {
                    cc.args([
                        "-DNIDAQMX_FIXTURE_OMIT_VOLTAGE=1",
                        "-DNIDAQMX_FIXTURE_OMIT_TC=1",
                        "-DNIDAQMX_FIXTURE_OMIT_TC_TERMINAL=1",
                    ]);
                }
                let output = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    pub(super) fn on_demand_libraries() -> &'static [Vec<u8>; 2] {
        static LIBS: OnceLock<[Vec<u8>; 2]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("software.c");
                let so = temp.path().join("software.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    "-shared",
                    "-fPIC",
                    "-O0",
                    "-DNIDAQMX_FIXTURE_ON_DEMAND=1",
                ]);
                if variant == 1 {
                    cc.arg("-DNIDAQMX_FIXTURE_OMIT_START=1");
                }
                let output = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn software_calls(log: &[Value]) {
        software_calls_with_channels(log, 2);
    }
    fn software_calls_with_channels(log: &[Value], expected_channels: usize) {
        for function in [
            "CfgSampClkTiming",
            "CfgInputBuffer",
            "SetReadOverWrite",
            "GetReadAvailSampPerChan",
            "GetSampClkRate",
            "SetSampTimingType",
            "GetSampTimingType",
        ] {
            assert_eq!(calls(log, function, "enter").len(), 0, "{function}");
        }
        for call in calls(log, "ReadAnalogF64", "arguments") {
            assert_eq!(call["requested"], 1);
            assert_eq!(call["capacity"], expected_channels);
            assert_eq!(call["fill_mode"], 1);
            assert_eq!(call["reserved_null"], true);
            let bits = u64::from_str_radix(call["timeout_bits"].as_str().unwrap(), 16).unwrap();
            let timeout = f64::from_bits(bits);
            assert!(
                timeout.is_finite() && timeout > 0.0 && timeout <= 1.0,
                "{call}"
            );
        }
    }
    fn software_metadata(value: &Value, interval: u64) {
        assert_eq!(
            value["timing"],
            json!({"mode":"on_demand", "poll_interval_ms":interval, "basis":"host_software_request_spacing"})
        );
        for key in [
            "rate_hz",
            "requested_rate_hz",
            "nominal_rate_hz",
            "max_samples_per_channel",
            "host_buffer_samples_per_channel",
        ] {
            assert!(value.get(key).is_none(), "{key}: {value}");
        }
    }
    fn rtd_oracle() -> Value {
        serde_json::from_str(include_str!("fixtures/nidaqmx/rtd.json")).unwrap()
    }
    fn rtd_libraries() -> &'static [Vec<u8>; 3] {
        static LIBS: OnceLock<[Vec<u8>; 3]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("rtd.c");
                let so = temp.path().join("rtd.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant == 0 {
                    cc.arg("-DNIDAQMX_FIXTURE_OMIT_RTD=1");
                } else {
                    cc.args([
                        "-DNIDAQMX_FIXTURE_OMIT_VOLTAGE=1",
                        "-DNIDAQMX_FIXTURE_OMIT_TC=1",
                        "-DNIDAQMX_FIXTURE_OMIT_CURRENT=1",
                        "-DNIDAQMX_FIXTURE_OMIT_TC_TERMINAL=1",
                    ]);
                    if variant == 2 {
                        cc.arg("-DNIDAQMX_FIXTURE_ON_DEMAND=1");
                    }
                }
                let output = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn rtd_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|row| {
                row["kind"] == "sample" && row["value"]["event"] == "rtd_temperature_block"
            })
            .collect()
    }
    fn rtd_bits(data: &[Value]) -> Vec<String> {
        rtd_blocks(data)
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn expected_rtd_bits() -> Vec<String> {
        rtd_oracle()["manual_temperature_patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn rtd_metadata(value: &Value, config: &Value) {
        assert_eq!(value["measurement"], "rtd");
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        let requested = config["channels"].as_array().unwrap();
        let channels = value["channel_measurements"].as_array().unwrap();
        assert_eq!(channels.len(), requested.len());
        assert_eq!(
            value["channels"],
            Value::Array(
                requested
                    .iter()
                    .map(|row| row["physical"].clone())
                    .collect()
            )
        );
        for (actual, request) in channels.iter().zip(requested) {
            assert_eq!(actual, request);
            for key in [
                "terminal_config",
                "cjc",
                "shunt",
                "coefficients",
                "custom_scale",
                "excitation_readback",
                "sensor_identity",
            ] {
                assert!(actual.get(key).is_none(), "{key}: {actual}");
            }
        }
    }
    fn rtd_arguments(log: &[Value], controls: &[Value]) {
        let oracle = rtd_oracle();
        let words = oracle["manual_argument_words"].as_array().unwrap();
        let word = |name: &Value| {
            words.iter().find(|row| row["name"] == *name).unwrap()["bits_hex"].clone()
        };
        let calls = calls(log, "CreateAIRTDChan", "arguments");
        assert_eq!(calls.len(), controls.len());
        for (actual, expected) in calls.iter().zip(controls) {
            for (field, oracle_field) in [
                ("units", "units_code"),
                ("rtd_type", "rtd_code"),
                ("resistance_config", "resistance_code"),
                ("excitation_source", "excitation_source_code"),
            ] {
                assert_eq!(actual[field], expected[oracle_field]);
            }
            for (field, oracle_field) in [
                ("minimum_bits", "min_word"),
                ("maximum_bits", "max_word"),
                ("excitation_amperes_bits", "excitation_word"),
                ("r0_ohms_bits", "r0_word"),
            ] {
                assert_eq!(actual[field], word(&expected[oracle_field]));
            }
            assert_eq!(actual["virtual_empty"], true);
        }
    }

    fn input_oracle(frequency_or_count: bool) -> Value {
        serde_json::from_str(if frequency_or_count {
            include_str!("fixtures/nidaqmx/counter-input.json")
        } else {
            include_str!("fixtures/nidaqmx/digital-port.json")
        })
        .unwrap()
    }
    pub(super) fn input_libraries() -> &'static [Vec<u8>; 13] {
        static LIBS: OnceLock<[Vec<u8>; 13]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("input.c");
                let so = temp.path().join("input.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    "-shared",
                    "-fPIC",
                    "-O0",
                    "-DNIDAQMX_FIXTURE_ON_DEMAND=1",
                ]);
                if variant == 12 {
                    cc.arg("-DNIDAQMX_FIXTURE_AI_ONLY=1");
                } else {
                    cc.arg("-DNIDAQMX_FIXTURE_INPUT_ONLY=1");
                    let family = [0, 1, 2, 0, 0, 0, 1, 1, 2, 2, 1, 2][variant];
                    cc.arg(
                        [
                            "-DNIDAQMX_FIXTURE_DIGITAL_ONLY=1",
                            "-DNIDAQMX_FIXTURE_COUNT_ONLY=1",
                            "-DNIDAQMX_FIXTURE_FREQUENCY_ONLY=1",
                        ][family],
                    );
                    if variant >= 3 {
                        cc.arg(
                            [
                                "-DNIDAQMX_FIXTURE_OMIT_DI=1",
                                "-DNIDAQMX_FIXTURE_OMIT_DI_READ=1",
                                "-DNIDAQMX_FIXTURE_OMIT_DI_WIDTH=1",
                                "-DNIDAQMX_FIXTURE_OMIT_COUNT=1",
                                "-DNIDAQMX_FIXTURE_OMIT_COUNT_READ=1",
                                "-DNIDAQMX_FIXTURE_OMIT_FREQUENCY=1",
                                "-DNIDAQMX_FIXTURE_OMIT_FREQUENCY_READ=1",
                                "-DNIDAQMX_FIXTURE_OMIT_COUNT_TERMINAL=1",
                                "-DNIDAQMX_FIXTURE_OMIT_FREQUENCY_TERMINAL=1",
                            ][variant - 3],
                        );
                    }
                }
                let out = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn input_config(family: usize, suffix: &str) -> Value {
        match family {
            0 => digital_source(&format!("Digital{suffix}")),
            1 => counter_source(&format!("Count{suffix}"), false),
            2 => counter_source(&format!("Frequency{suffix}"), true),
            _ => panic!("unknown fixture family"),
        }
    }
    fn input_event(family: usize) -> &'static str {
        [
            "digital_port_block",
            "counter_count_block",
            "counter_frequency_block",
        ][family]
    }
    fn input_read_name(family: usize) -> &'static str {
        ["ReadDigitalU32", "ReadCounterU32", "ReadCounterF64"][family]
    }
    fn input_blocks(data: &[Value], family: usize) -> Vec<&Value> {
        data.iter()
            .filter(|r| r["kind"] == "sample" && r["value"]["event"] == input_event(family))
            .collect()
    }
    fn input_bits(data: &[Value], family: usize) -> Vec<String> {
        let key = if family == 2 {
            "ieee754_le_hex"
        } else {
            "u32_le_hex"
        };
        input_blocks(data, family)
            .iter()
            .flat_map(|r| r["value"]["samples"].as_array().unwrap())
            .flat_map(|r| r.as_array().unwrap())
            .map(|r| r[key].as_str().unwrap().to_owned())
            .collect()
    }
    fn expected_input_bits(family: usize) -> Vec<String> {
        let o = input_oracle(family != 0);
        let (array, key) = match family {
            0 => ("manual_scan_major_patterns", "u32_le_hex"),
            1 => ("manual_count_snapshots", "u32_le_hex"),
            2 => ("manual_frequency_patterns", "ieee754_le_hex"),
            _ => panic!("unknown fixture family"),
        };
        o[array]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[key].as_str().unwrap().to_owned())
            .collect()
    }
    fn input_metadata(value: &Value, config: &Value, family: usize) {
        assert_eq!(value["measurement"], config["measurement"]);
        software_metadata(value, 10);
        let actual = value["channels"].as_array().unwrap();
        let meta = value["channel_measurements"].as_array().unwrap();
        assert_eq!(meta.len(), if family == 0 { 3 } else { 1 });
        assert_eq!(actual.len(), meta.len());
        for (i, item) in meta.iter().enumerate() {
            assert_eq!(item["channel_index"], i);
            if family == 0 {
                assert_eq!(item["physical"], config["channels"][i]);
                assert_eq!(
                    item["physical_provenance"],
                    "constructor_request_not_read_back"
                );
                assert_eq!(item["read_channel"], format!("cfdp{i:04}"));
                assert_eq!(actual[i], item["read_channel"]);
                assert_eq!(item["line_count"], [1, 8, 32][i]);
                assert_eq!(item["line_count_provenance"], "driver_channel_property");
                assert_eq!(item["line_grouping"], "chan_for_all_lines");
                assert!(item.get("line_mask").is_none());
            } else {
                let request = &config["channels"][i];
                assert_eq!(value["settings_provenance"], "requested_not_read_back");
                assert_eq!(item["physical"], actual[i]);
                assert_eq!(item["requested_physical"], request["physical"]);
                let keys = if family == 1 {
                    vec!["edge", "initial_count", "direction"]
                } else {
                    vec![
                        "min_hz",
                        "max_hz",
                        "edge",
                        "method",
                        "measurement_time_seconds",
                        "divisor",
                    ]
                };
                for key in keys {
                    assert_eq!(item[key], request[key]);
                }
                if family == 1 {
                    assert_eq!(item["api_word_bits"], 32);
                } else {
                    assert_eq!(item["units"], "hz");
                }
                if let Some(terminal) = request.get("input_terminal") {
                    assert_eq!(item["input_terminal_request"], *terminal);
                    assert_eq!(
                        item["input_terminal_provenance"],
                        "explicit_request_not_read_back"
                    );
                } else {
                    assert!(item.get("input_terminal_request").is_none());
                    assert_eq!(
                        item["input_terminal_provenance"],
                        "driver_default_unqueried"
                    );
                }
            }
        }
    }
    fn input_cleaned(log: &[Value], family: usize) {
        assert_eq!(calls(log, "StopTask", "enter").len(), 1);
        assert_eq!(calls(log, "ClearTask", "enter").len(), 1);
        let returned = calls(log, input_read_name(family), "return");
        let last_read = returned.last().unwrap()["seq"].as_u64().unwrap();
        assert!(calls(log, "StopTask", "enter")[0]["seq"].as_u64().unwrap() > last_read);
    }
    fn input_calls(log: &[Value], family: usize) {
        for name in [
            "ReadAnalogF64",
            "CreateAIVoltageChan",
            "CreateAIThrmcplChan",
            "CreateAICurrentChan",
            "CreateAIRTDChan",
            "SetAITermCfg",
            "CfgSampClkTiming",
            "CfgInputBuffer",
            "SetReadOverWrite",
            "GetReadAvailSampPerChan",
            "GetSampClkRate",
            "SetSampTimingType",
            "GetSampTimingType",
        ] {
            assert_eq!(calls(log, name, "enter").len(), 0, "{name}");
        }
        for other in 0..3 {
            if other != family {
                assert_eq!(calls(log, input_read_name(other), "enter").len(), 0);
            }
        }
        for row in calls(log, input_read_name(family), "arguments") {
            assert_eq!(row["requested"], 1);
            assert_eq!(row["capacity"], if family == 0 { 3 } else { 1 });
            assert_eq!(row["reserved_null"], true);
            assert!(row.get("fill_mode").is_none());
            let timeout = f64::from_bits(
                u64::from_str_radix(row["timeout_bits"].as_str().unwrap(), 16).unwrap(),
            );
            assert!(
                timeout.is_finite() && timeout > 0.0 && timeout <= 1.0,
                "{row}"
            );
        }
        if family == 0 {
            for row in calls(log, "ReadDigitalU32", "fill") {
                assert_eq!(row["fill_mode"], 1);
            }
        }
    }
    fn input_words(data: &[Value], family: usize) {
        let o = input_oracle(family != 0);
        let patterns = o[if family == 0 {
            "manual_scan_major_patterns"
        } else if family == 1 {
            "manual_count_snapshots"
        } else {
            "manual_frequency_patterns"
        }]
        .as_array()
        .unwrap();
        let mut word_index = 0;
        for (scan_index, row) in input_blocks(data, family).iter().enumerate() {
            let value = &row["value"];
            assert_eq!(
                value["sample_index"].as_u64().unwrap(),
                u64::try_from(scan_index).unwrap()
            );
            assert_eq!(value["samples_per_channel"], 1);
            assert_eq!(value["layout"], "scan_major");
            assert!(row.get("source_time").is_none());
            assert_eq!(
                value["representation"],
                [
                    "sdk_u32_digital_port",
                    "driver_unscaled_u32_counter_count",
                    "driver_scaled_f64_counter_frequency"
                ][family]
            );
            let scans = value["samples"].as_array().unwrap();
            assert_eq!(scans.len(), 1);
            assert_eq!(
                scans[0].as_array().unwrap().len(),
                if family == 0 { 3 } else { 1 }
            );
            for word in scans[0].as_array().unwrap() {
                let expected = &patterns[word_index % patterns.len()];
                if family == 2 {
                    assert_eq!(word["ieee754_le_hex"], expected["ieee754_le_hex"]);
                    if let Some(decimal) = expected["finite_decimal"].as_str() {
                        assert_eq!(
                            word["frequency_hz"].as_f64().unwrap().to_bits(),
                            decimal.parse::<f64>().unwrap().to_bits()
                        );
                    } else {
                        assert!(word.get("frequency_hz").is_none());
                    }
                    assert!(word.get("u32_le_hex").is_none());
                } else {
                    let key = if family == 0 { "value" } else { "count" };
                    assert_eq!(word[key].as_u64().unwrap(), expected[key].as_u64().unwrap());
                    assert_eq!(word["u32_le_hex"], expected["u32_le_hex"]);
                    assert!(word.get("ieee754_le_hex").is_none());
                }
                for key in ["volts", "amperes", "temperature", "units", "measurement"] {
                    assert!(word.get(key).is_none());
                }
                word_index += 1;
            }
        }
    }

    fn mixed_oracle() -> Value {
        serde_json::from_str(include_str!("fixtures/nidaqmx/mixed-ai.json")).unwrap()
    }
    fn mixed_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|row| row["kind"] == "sample" && row["value"]["event"] == "mixed_analog_block")
            .collect()
    }
    fn mixed_bits(data: &[Value]) -> Vec<String> {
        mixed_blocks(data)
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn expected_mixed_bits() -> Vec<String> {
        mixed_oracle()["manual_scan_major_patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn mixed_libraries() -> &'static [Vec<u8>; 7] {
        static LIBS: OnceLock<[Vec<u8>; 7]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("mixed.c");
                let so = temp.path().join("mixed.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant < 5 {
                    cc.arg(
                        [
                            "-DNIDAQMX_FIXTURE_OMIT_VOLTAGE=1",
                            "-DNIDAQMX_FIXTURE_OMIT_TC=1",
                            "-DNIDAQMX_FIXTURE_OMIT_CURRENT=1",
                            "-DNIDAQMX_FIXTURE_OMIT_RTD=1",
                            "-DNIDAQMX_FIXTURE_OMIT_TC_TERMINAL=1",
                        ][variant],
                    );
                } else {
                    cc.args([
                        "-DNIDAQMX_FIXTURE_OMIT_TC=1",
                        "-DNIDAQMX_FIXTURE_OMIT_CURRENT=1",
                        "-DNIDAQMX_FIXTURE_OMIT_RTD=1",
                        "-DNIDAQMX_FIXTURE_OMIT_TC_TERMINAL=1",
                    ]);
                    if variant == 6 {
                        cc.arg("-DNIDAQMX_FIXTURE_ON_DEMAND=1");
                    }
                }
                let output = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn mixed_metadata(value: &Value, config: &Value) {
        assert_eq!(value["measurement"], "mixed");
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        assert!(value.get("units").is_none());
        let requests = config["channels"].as_array().unwrap();
        let actual = value["channel_measurements"].as_array().unwrap();
        assert_eq!(actual.len(), requests.len());
        assert_eq!(
            value["channels"],
            Value::Array(requests.iter().map(|row| row["physical"].clone()).collect())
        );
        for (index, (actual, request)) in actual.iter().zip(requests).enumerate() {
            let family = request["measurement"].as_str().unwrap();
            assert_eq!(
                actual["channel_index"].as_u64().unwrap(),
                u64::try_from(index).unwrap()
            );
            assert_eq!(actual["physical"], request["physical"]);
            assert_eq!(actual["measurement"], family);
            assert!(actual.get("kind").is_none());
            let (quantity, units) = match family {
                "voltage" => ("voltage", json!("volts")),
                "current" => ("current", json!("amps")),
                _ => ("temperature", request["units"].clone()),
            };
            assert_eq!(actual["quantity"], quantity);
            assert_eq!(actual["units"], units);
            let keys: &[&str] = match family {
                "voltage" => &["min_volts", "max_volts"],
                "current" => &["min_amperes", "max_amperes", "shunt"],
                "thermocouple" => &[
                    "thermocouple_type",
                    "min_temperature",
                    "max_temperature",
                    "cjc",
                ],
                "rtd" => &[
                    "rtd_type",
                    "min_temperature",
                    "max_temperature",
                    "resistance_config",
                    "r0_ohms",
                    "excitation",
                ],
                _ => panic!("unknown fixture family"),
            };
            for key in keys {
                assert_eq!(actual[*key], request[*key], "{family}.{key}");
            }
            if family == "rtd" {
                assert!(actual.get("terminal_config_request").is_none());
                assert!(actual.get("terminal_config_provenance").is_none());
            } else {
                if let Some(terminal) = request.get("terminal_config") {
                    assert_eq!(actual["terminal_config_request"], *terminal);
                } else {
                    assert!(actual.get("terminal_config_request").is_none());
                }
                assert_eq!(
                    actual["terminal_config_provenance"],
                    if request
                        .get("terminal_config")
                        .is_some_and(|value| value != "default")
                    {
                        "explicit_request_not_read_back"
                    } else {
                        "driver_default_unqueried"
                    }
                );
                assert!(actual.get("terminal_config").is_none());
            }
        }
    }
    fn mixed_arguments(log: &[Value]) {
        let oracle = mixed_oracle();
        let words = oracle["manual_argument_words"].as_array().unwrap();
        let word = |name: &Value| {
            words.iter().find(|row| row["name"] == *name).unwrap()["bits_hex"].clone()
        };
        let controls = oracle["ordered_four_channel_constructor_controls"]
            .as_array()
            .unwrap();
        let calls = log
            .iter()
            .filter(|row| row["phase"] == "arguments" && row.get("constructor_kind").is_some())
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 4);
        for (index, (actual, expected)) in calls.iter().zip(controls).enumerate() {
            assert_eq!(actual["fn"], expected["constructor"]);
            assert_eq!(
                actual["column_index"].as_u64().unwrap(),
                u64::try_from(index).unwrap()
            );
            assert_eq!(
                actual["constructor_kind"].as_u64().unwrap(),
                u64::try_from(index + 1).unwrap()
            );
            assert!(
                actual["physical"]
                    .as_str()
                    .unwrap()
                    .ends_with(&format!("/ai{index}"))
            );
            assert_eq!(actual["minimum_bits"], word(&expected["min_word"]));
            assert_eq!(actual["maximum_bits"], word(&expected["max_word"]));
            assert_eq!(actual["units"], expected["units_code"]);
            assert_eq!(actual["virtual_empty"], true);
            match index {
                0 => {
                    assert_eq!(actual["terminal"], expected["terminal_config_code"]);
                    assert_eq!(actual["custom_scale_null"], true);
                }
                1 => {
                    assert_eq!(actual["type"], expected["thermocouple_type_code"]);
                    assert_eq!(actual["source"], expected["cjc_source_code"]);
                    assert_eq!(actual["arg_a_bits"], word(&expected["cjc_value_word"]));
                    assert_eq!(actual["aux_empty"], true);
                }
                2 => {
                    assert_eq!(actual["terminal"], expected["terminal_config_code"]);
                    assert_eq!(actual["source"], expected["shunt_source_code"]);
                    assert_eq!(actual["arg_a_bits"], word(&expected["external_ohms_word"]));
                    assert_eq!(actual["custom_scale_null"], true);
                }
                _ => {
                    assert_eq!(actual["type"], expected["rtd_type_code"]);
                    assert_eq!(actual["wire"], expected["resistance_config_code"]);
                    assert_eq!(actual["source"], expected["excitation_source_code"]);
                    assert_eq!(
                        actual["arg_a_bits"],
                        word(&expected["excitation_amperes_word"])
                    );
                    assert_eq!(actual["arg_b_bits"], word(&expected["r0_word"]));
                }
            }
        }
    }
    fn current_oracle() -> Value {
        serde_json::from_str(include_str!("fixtures/nidaqmx/current.json")).unwrap()
    }
    fn current_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|row| {
                row["kind"] == "sample" && row["value"]["event"] == "analog_current_block"
            })
            .collect()
    }
    fn current_bits(data: &[Value]) -> Vec<String> {
        current_blocks(data)
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn expected_current_bits() -> Vec<String> {
        current_oracle()["manual_patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn current_metadata(value: &Value, config: &Value) {
        assert_eq!(value["measurement"], "current");
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        let requested = config["channels"].as_array().unwrap();
        let actual = value["channel_measurements"].as_array().unwrap();
        assert_eq!(actual.len(), requested.len());
        assert_eq!(
            value["channels"],
            Value::Array(
                requested
                    .iter()
                    .map(|row| row["physical"].clone())
                    .collect()
            )
        );
        for (actual, request) in actual.iter().zip(requested) {
            let keys = actual
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            assert_eq!(
                keys,
                BTreeSet::from([
                    "physical",
                    "units",
                    "min_amperes",
                    "max_amperes",
                    "shunt",
                    "terminal_config_request",
                    "terminal_config_provenance"
                ])
            );
            for key in ["physical", "min_amperes", "max_amperes", "shunt"] {
                assert_eq!(actual[key], request[key]);
            }
            assert_eq!(actual["units"], "amps");
            assert_eq!(
                actual["terminal_config_request"],
                request["terminal_config"]
            );
            assert_eq!(
                actual["terminal_config_provenance"],
                if request["terminal_config"] == "default" {
                    "driver_default_unqueried"
                } else {
                    "explicit_request_not_read_back"
                }
            );
            if request["shunt"]["source"] == "built_in" {
                assert!(actual["shunt"].get("ohms").is_none());
            }
        }
    }
    fn tc_oracle() -> Value {
        serde_json::from_str(include_str!("fixtures/nidaqmx/thermocouple.json")).unwrap()
    }
    fn temperature_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|row| {
                row["kind"] == "sample" && row["value"]["event"] == "thermocouple_temperature_block"
            })
            .collect()
    }
    fn temperature_bits(data: &[Value]) -> Vec<String> {
        temperature_blocks(data)
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn expected_temperature_bits() -> Vec<String> {
        tc_oracle()["manual_sample_patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn temperature_metadata(value: &Value, config: &Value) {
        assert_eq!(value["measurement"], "thermocouple");
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        let requests = config["channels"].as_array().unwrap();
        let actual = value["channel_measurements"].as_array().unwrap();
        assert_eq!(actual.len(), requests.len());
        assert_eq!(
            value["channels"],
            Value::Array(requests.iter().map(|row| row["physical"].clone()).collect())
        );
        for (actual, request) in actual.iter().zip(requests) {
            for key in [
                "physical",
                "thermocouple_type",
                "units",
                "min_temperature",
                "max_temperature",
                "cjc",
            ] {
                assert_eq!(actual[key], request[key], "{key}");
            }
            if request
                .get("terminal_config")
                .is_some_and(|value| value != "default")
            {
                assert_eq!(
                    actual["terminal_config_provenance"],
                    "explicit_request_not_read_back"
                );
            } else {
                assert_eq!(
                    actual["terminal_config_provenance"],
                    "driver_default_unqueried"
                );
            }
            if let Some(terminal) = request.get("terminal_config") {
                assert_eq!(actual["terminal_config_request"], *terminal);
            } else {
                assert!(actual.get("terminal_config_request").is_none());
            }
        }
    }
    fn run_config_ni(
        home: &Home,
        config: &Value,
        library_bytes: &[u8],
        duration: &str,
    ) -> (Run, u32, Duration) {
        let library = home.path("selected-fixture.so");
        fs::write(&library, library_bytes).unwrap();
        let mut ni = config.clone();
        ni["library"] = json!(library);
        write_config(home, &[ni]);
        let args = [
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            duration,
            "-o",
            "run.jsonl",
        ];
        let began = Instant::now();
        let child = home
            .command()
            .env("CAPTUREFAB_EXECUTABLE", env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_NIDAQMX_FIXTURE_LOG", home.path("driver.jsonl"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        (Run::of_output(output, &args), pid, began.elapsed())
    }
    fn csv_cell(text: &str) -> String {
        format!("\"{}\"", text.replace('"', "\"\""))
    }
    fn temperature_exports(home: &Home, data: &[Value], incomplete: bool) {
        if incomplete {
            home.fails(1, &["data", "export", "run.jsonl", "-o", "strict-tc.csv"]);
        }
        let mut command = vec!["data", "export", "run.jsonl", "-o", "temperature.csv"];
        if incomplete {
            command.push("--allow-incomplete");
        }
        home.ok(&command);
        let samples = data
            .iter()
            .filter(|row| row["kind"] == "sample")
            .map(|row| {
                let mut sample = row.clone();
                sample.as_object_mut().unwrap().remove("kind");
                sample
            })
            .collect::<Vec<_>>();
        let mut expected = "seq,source,signal,time,elapsed_ns,read_duration_ns,delivery_latency_ns,source_time,value_json\n".to_owned();
        for sample in &samples {
            let mut fields = [
                "seq",
                "source",
                "signal",
                "time",
                "elapsed_ns",
                "read_duration_ns",
                "delivery_latency_ns",
                "source_time",
            ]
            .iter()
            .map(|key| match sample.get(*key) {
                Some(Value::String(value)) => csv_cell(value),
                Some(Value::Number(value)) => value.to_string(),
                None if *key == "source_time" => csv_cell(""),
                _ => panic!("invalid recorded CSV column {key}"),
            })
            .collect::<Vec<_>>();
            fields.push(csv_cell(&serde_json::to_string(&sample["value"]).unwrap()));
            expected.push_str(&fields.join(","));
            expected.push('\n');
        }
        assert_eq!(
            fs::read_to_string(home.path("temperature.csv")).unwrap(),
            expected
        );
        let mut command = vec![
            "data",
            "export",
            "run.jsonl",
            "--format",
            "jsonl",
            "-o",
            "temperature.jsonl",
        ];
        if incomplete {
            command.push("--allow-incomplete");
        }
        home.ok(&command);
        let exported = fs::read_to_string(home.path("temperature.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(exported, samples);
        let mut previous = 0;
        let origin =
            chrono::DateTime::parse_from_rfc3339(data[0]["started_at"].as_str().unwrap()).unwrap();
        for sample in samples {
            let elapsed = sample["elapsed_ns"].as_u64().unwrap();
            assert!(elapsed >= previous);
            previous = elapsed;
            assert!(sample.get("source_time").is_none());
            let time =
                chrono::DateTime::parse_from_rfc3339(sample["time"].as_str().unwrap()).unwrap();
            assert_eq!(
                u64::try_from((time - origin).num_nanoseconds().unwrap()).unwrap(),
                elapsed
            );
            assert!(sample["read_duration_ns"].as_u64().unwrap() <= elapsed);
        }
    }
    fn run_ni(
        home: &Home,
        scenario: &str,
        variant: usize,
        timeout: u64,
        duration: &str,
    ) -> (Run, u32, Duration) {
        let library = home.path("fixture.so");
        fs::write(&library, &libraries()[variant]).unwrap();
        let mut ni = source(scenario);
        ni["library"] = json!(library);
        ni["timeout_ms"] = json!(timeout);
        if scenario == "Max" {
            ni["max_samples_per_channel"] = json!(4096);
        }
        write_config(home, &[ni]);
        let args = [
            "--json",
            "data",
            "record",
            "--config",
            "sources.json",
            "--for",
            duration,
            "-o",
            "run.jsonl",
        ];
        let began = Instant::now();
        let child = home
            .command()
            .env("CAPTUREFAB_EXECUTABLE", env!("CARGO_BIN_EXE_capturefab"))
            .env("CAPTUREFAB_NIDAQMX_FIXTURE_LOG", home.path("driver.jsonl"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let output = child.wait_with_output().unwrap();
        (Run::of_output(output, &args), pid, began.elapsed())
    }
    fn trace(home: &Home) -> Vec<Value> {
        fs::read_to_string(home.path("driver.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn calls<'a>(log: &'a [Value], function: &str, phase: &str) -> Vec<&'a Value> {
        log.iter()
            .filter(|r| r["fn"].as_str().unwrap().ends_with(function) && r["phase"] == phase)
            .collect()
    }
    fn isolated(log: &[Value], cli: u32) {
        assert!(!log.iter().any(|r| r["phase"] == "contract"), "{log:?}");
        let pids = log
            .iter()
            .map(|r| r["pid"].as_u64().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(pids.len(), 1);
        assert!(!pids.contains(&u64::from(cli)));
        assert!(!pids.contains(&u64::from(std::process::id())));
    }
    fn has_code(value: &Value, code: i64) -> bool {
        match value {
            Value::Object(o) => o.iter().any(|(k, v)| {
                (matches!(
                    k.as_str(),
                    "status_code" | "native_status" | "ni_daqmx_status"
                ) && v.as_i64() == Some(code))
                    || has_code(v, code)
            }),
            Value::Array(a) => a.iter().any(|v| has_code(v, code)),
            _ => false,
        }
    }
    fn all_bits(rows: &[Value]) -> Vec<String> {
        blocks(rows)
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn cleaned(log: &[Value]) {
        assert_eq!(calls(log, "StopTask", "enter").len(), 1);
        assert_eq!(calls(log, "ClearTask", "enter").len(), 1);
        let last_read = calls(log, "ReadAnalogF64", "enter").last().unwrap()["seq"]
            .as_u64()
            .unwrap();
        let stop = calls(log, "StopTask", "enter")[0]["seq"].as_u64().unwrap();
        assert!(stop > last_read);
    }

    #[test]
    fn actual_c_abi_preserves_manual_ieee_words_owned_calls_and_archive_exports() {
        let home = Home::new("ni-native-success");
        let (result, pid, _) = run_ni(&home, "Success", 0, 1000, "160ms");
        result.success();
        let rows = rows(&home);
        footer(&rows, "complete");
        let blocks = blocks(&rows);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["value"]["sample_index"], 0);
        assert_eq!(blocks[1]["value"]["sample_index"], 3);
        let oracle: Value =
            serde_json::from_str(include_str!("fixtures/nidaqmx/abi.json")).unwrap();
        let expected = oracle["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(all_bits(&rows), expected);
        for block in &blocks {
            assert!(block.get("source_time").is_none());
            assert_eq!(
                block["value"]["channels"],
                json!(["Success/ai0", "Success/ai1"])
            );
        }
        assert!(blocks[0]["value"]["samples"][1][1].get("volts").is_none());
        assert_eq!(
            blocks[0]["value"]["samples"][1][0]["volts"]
                .as_f64()
                .unwrap()
                .to_bits(),
            (-0.0f64).to_bits()
        );
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        let scale_oracle: Value =
            serde_json::from_str(include_str!("fixtures/nidaqmx/voltage-scale-null.json")).unwrap();
        assert_eq!(
            scale_oracle["abi"]["slots_unchanged"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        assert_eq!(
            scale_oracle["fixed_voltage_arguments"]["custom_scale_pointer"],
            "NULL"
        );
        let voltage_calls = calls(&log, "CreateAIVoltageChan", "arguments");
        assert_eq!(voltage_calls.len(), 2);
        for call in voltage_calls {
            assert_eq!(call["custom_scale_null"], true);
            assert_eq!(call["virtual_empty"], true);
            assert_eq!(
                call["units"],
                scale_oracle["fixed_voltage_arguments"]["units_code"]
            );
        }
        for call in calls(&log, "ReadAnalogF64", "arguments") {
            assert_eq!(call["requested"], 3);
            assert_eq!(call["capacity"], 6);
            assert_eq!(call["fill_mode"], 1);
            assert_eq!(call["reserved_null"], true);
        }
        assert_eq!(
            calls(&log, "SetReadOverWrite", "arguments")[0]["value"],
            10_159
        );
        assert_eq!(
            calls(&log, "GetNthTaskReadChannel", "arguments")[0]["index"],
            1
        );
        home.ok(&["data", "export", "run.jsonl", "-o", "run.csv"]);
        let csv = fs::read_to_string(home.path("run.csv")).unwrap();
        assert!(expected.iter().all(|bits| csv.contains(bits)));
        let expected_samples = rows
            .iter()
            .filter(|row| row["kind"] == "sample")
            .map(|row| {
                let mut sample = row.clone();
                sample.as_object_mut().unwrap().remove("kind");
                sample
            })
            .collect::<Vec<_>>();
        let csv_values = csv
            .lines()
            .skip(1)
            .map(|line| {
                // These fixture source/signal/time fields contain no CSV separators.
                let field = line.splitn(9, ',').last().unwrap();
                let json = field
                    .strip_prefix('"')
                    .unwrap()
                    .strip_suffix('"')
                    .unwrap()
                    .replace("\"\"", "\"");
                serde_json::from_str::<Value>(&json).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            csv_values,
            expected_samples
                .iter()
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>()
        );
        home.ok(&[
            "data",
            "export",
            "run.jsonl",
            "--format",
            "jsonl",
            "-o",
            "export.jsonl",
        ]);
        let exported = fs::read_to_string(home.path("export.jsonl")).unwrap();
        assert!(expected.iter().all(|bits| exported.contains(bits)));
        let exported = exported
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(exported, expected_samples);
    }

    #[test]
    fn native_warning_and_coerced_nominal_rate_remain_visible() {
        for scenario in ["Warn", "Coerced", "SetupWarn", "StartWarn"] {
            let home = Home::new("ni-warnings");
            let (run, pid, _) = run_ni(&home, scenario, 0, 1000, "120ms");
            run.success();
            let rows = rows(&home);
            footer(&rows, "complete");
            if scenario == "Coerced" {
                assert_eq!(blocks(&rows)[0]["value"]["nominal_rate_hz"], 500.0);
            } else {
                assert!(rows.iter().any(|r| has_code(r, 12_345)));
            }
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
        }
    }

    #[test]
    fn partial_read_is_saved_before_original_failure_and_finish_cleanup_error() {
        for scenario in ["Partial", "PartialStopFail", "Overrun", "FifoOverflow"] {
            let home = Home::new("ni-partial");
            let (run, pid, _) = run_ni(&home, scenario, 0, 1000, "500ms");
            let status = if scenario == "Overrun" {
                -200_279
            } else if scenario == "FifoOverflow" {
                -200_361
            } else {
                -200_284
            };
            let error = run.failure(if status == -200_284 { 4 } else { 1 });
            assert!(has_code(&error, status));
            if scenario == "PartialStopFail" {
                assert!(has_code(&error, -990_201), "{error}");
            }
            let rows = rows(&home);
            footer(&rows, "failed");
            let data = blocks(&rows);
            assert_eq!(data.len(), 1);
            assert_eq!(data[0]["value"]["samples_per_channel"], 1);
            assert_eq!(all_bits(&rows), ["000000000000f03f", "00000000000004c0"]);
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
            home.fails(1, &["data", "export", "run.jsonl", "-o", "strict.csv"]);
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "--allow-incomplete",
                "-o",
                "partial.csv",
            ]);
        }
    }

    #[test]
    fn worker_read_clock_precedes_deferred_diagnostics_and_cleanup() {
        let home = Home::new("ni-native-clock");
        let (run, pid, _) = run_ni(&home, "DiagSlowPartial", 0, 1000, "700ms");
        run.failure(4);
        let rows = rows(&home);
        let data = blocks(&rows);
        assert_eq!(data.len(), 1);
        assert_eq!(
            data[0]["value"]["notices"][0]["diagnostic_source"],
            "deferred"
        );
        let finished = rows
            .iter()
            .find(|row| row["kind"] == "sample" && row["value"]["event"] == "finished")
            .unwrap();
        assert!(
            finished["elapsed_ns"].as_u64().unwrap() - data[0]["elapsed_ns"].as_u64().unwrap()
                >= 100_000_000
        );
        assert!(
            finished["value"]["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|notice| notice["operation"] == "ReadAnalogF64"
                    && notice["diagnostic_source"] == "extended_error_info")
        );
        assert_eq!(all_bits(&rows), ["000000000000f03f", "00000000000004c0"]);
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        let read = calls(&log, "ReadAnalogF64", "return")[0]["monotonic_ns"]
            .as_u64()
            .unwrap();
        let diag = calls(&log, "GetExtendedErrorInfo", "return")[0]["monotonic_ns"]
            .as_u64()
            .unwrap();
        assert!(diag - read >= 100_000_000);
    }

    #[test]
    fn malformed_native_count_and_driver_channel_geometry_never_publish_unqualified_samples() {
        for scenario in [
            "CorruptCountNegative",
            "CorruptCountTooLarge",
            "CountMismatch",
            "CountZero",
            "CountHuge",
            "Reverse",
            "NameDuplicate",
            "NameUnterminated",
            "NameInvalidUtf8",
            "NameProbeHuge",
            "NameProbeZero",
            "NameForeign",
        ] {
            let home = Home::new("ni-malformed");
            let (run, pid, _) = run_ni(&home, scenario, 0, 1000, "500ms");
            let error = run.failure(1);
            assert_eq!(error["code"], "incomplete");
            let rows = rows(&home);
            footer(&rows, "failed");
            assert_eq!(blocks(&rows), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            if !scenario.starts_with("CorruptCount") {
                assert_eq!(calls(&log, "StartTask", "enter"), [] as [&Value; 0]);
            }
        }
    }

    #[test]
    fn driver_missing_export_and_negative_unqualified_task_handle_have_no_acquisition_side_effects()
    {
        for variant in [1, 2] {
            let home = Home::new("ni-init-failure");
            let (run, pid, _) = run_ni(&home, "Success", variant, 1000, "500ms");
            let error = run.failure(if variant == 1 { 1 } else { 3 });
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "StartTask", "enter"), [] as [&Value; 0]);
            assert_eq!(calls(&log, "ClearTask", "enter"), [] as [&Value; 0]);
            assert_eq!(calls(&log, "StopTask", "enter"), [] as [&Value; 0]);
            if variant == 1 {
                assert!(has_code(&error, -50_103));
                assert_eq!(
                    error["details"]["task_ownership"],
                    "unconfirmed_after_failed_create"
                );
                assert_eq!(error["details"]["cleanup_confirmed"], false);
            } else {
                assert_eq!(calls(&log, "CreateTask", "enter"), [] as [&Value; 0]);
            }
        }
    }

    #[test]
    fn sdk_crash_and_hang_are_bounded_and_retire_the_owned_worker() {
        for scenario in ["ReadCrash", "ReadHang", "AvailHang"] {
            let home = Home::new("ni-worker-failure");
            let (run, pid, elapsed) = run_ni(&home, scenario, 0, 100, "1s");
            let error = run.failure(if scenario == "ReadCrash" { 3 } else { 4 });
            assert_eq!(
                error["code"],
                if scenario == "ReadCrash" {
                    "worker_crashed"
                } else {
                    "timeout"
                }
            );
            assert!(elapsed < Duration::from_secs(3), "{scenario}: {elapsed:?}");
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "StopTask", "enter"), [] as [&Value; 0]);
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
            footer(&rows(&home), "failed");
        }
    }

    #[test]
    fn bounded_maximum_read_capacity_and_raw_diagnostic_failures_are_preserved() {
        let home = Home::new("ni-max");
        let (run, pid, _) = run_ni(&home, "Max", 0, 1000, "500ms");
        run.success();
        let data = rows(&home);
        assert_ne!(blocks(&data), [] as [&Value; 0]);
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        assert!(
            calls(&log, "ReadAnalogF64", "arguments")
                .iter()
                .any(|r| r["capacity"] == 4096 && r["requested"] == 2048)
        );
        for row in blocks(&data) {
            assert!(serde_json::to_vec(row).unwrap().len() < 1024 * 1024);
        }
        for scenario in ["DiagError", "DiagInvalidUtf8", "DiagUnterminated"] {
            let home = Home::new("ni-diagnostics");
            let (run, pid, _) = run_ni(&home, scenario, 0, 1000, "500ms");
            let error = run.failure(4);
            assert!(has_code(&error, -200_284));
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            let data = rows(&home);
            assert_eq!(blocks(&data).len(), 1);
            assert_eq!(
                blocks(&data)[0]["value"]["notices"][0]["diagnostic_source"],
                "deferred"
            );
            let finished = data
                .iter()
                .find(|row| row["kind"] == "sample" && row["value"]["event"] == "finished")
                .unwrap();
            let notice = finished["value"]["notices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|notice| notice["operation"] == "ReadAnalogF64")
                .unwrap();
            if scenario == "DiagError" {
                assert_eq!(notice["diagnostic_lookup_status"], -990_301);
            }
            if scenario == "DiagInvalidUtf8" {
                assert!(notice["diagnostic_hex"].as_str().unwrap().ends_with("ff"));
            }
            if scenario == "DiagUnterminated" {
                assert_eq!(notice["diagnostic_truncated"], true);
                assert_eq!(notice["diagnostic_hex"].as_str().unwrap().len(), 512);
            }
        }
    }

    #[test]
    fn idle_cutoff_and_setup_failures_do_not_invent_analog_data_or_take_over_tasks() {
        let home = Home::new("ni-idle");
        let (run, pid, _) = run_ni(&home, "Idle", 0, 1000, "100ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        assert_eq!(blocks(&data), [] as [&Value; 0]);
        let log = trace(&home);
        isolated(&log, pid);
        assert_eq!(calls(&log, "ReadAnalogF64", "enter"), [] as [&Value; 0]);
        assert_eq!(calls(&log, "StopTask", "enter").len(), 1);
        assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
        for scenario in ["Busy", "SetupError", "StartBusy"] {
            let home = Home::new("ni-setup-failure");
            let (run, pid, _) = run_ni(&home, scenario, 0, 1000, "500ms");
            let error = run.failure(1);
            assert!(has_code(
                &error,
                if scenario == "SetupError" {
                    -990_102
                } else {
                    -50_103
                }
            ));
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ReadAnalogF64", "enter"), [] as [&Value; 0]);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            assert_eq!(
                calls(&log, "StopTask", "enter").len(),
                usize::from(scenario == "StartBusy")
            );
        }
    }

    #[test]
    fn stop_and_clear_failures_retain_all_received_data_and_first_cleanup_cause() {
        for scenario in ["StopError", "ClearError", "StopAndClearError"] {
            let home = Home::new("ni-cleanup-failure");
            let (run, pid, _) = run_ni(&home, scenario, 0, 1000, "120ms");
            let error = run.failure(1);
            assert!(has_code(
                &error,
                if scenario == "ClearError" {
                    -990_202
                } else {
                    -990_201
                }
            ));
            if scenario == "StopAndClearError" {
                assert!(has_code(&error, -990_202));
            }
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(blocks(&data).len(), 2);
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 2);
        }
    }

    #[test]
    fn hung_stop_and_clear_respect_one_cleanup_budget_and_report_uncertainty() {
        for scenario in ["StopHang", "ClearHang"] {
            let home = Home::new("ni-cleanup-hang");
            let (run, pid, elapsed) = run_ni(&home, scenario, 0, 1000, "100ms");
            let error = run.failure(4);
            assert_eq!(error["code"], "timeout");
            assert!(error.to_string().contains("unconfirmed"), "{error}");
            assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(blocks(&data).len(), 2);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "StopTask", "enter").len(), 1);
            if scenario == "ClearHang" {
                assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            }
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
        }
    }

    #[test]
    fn hung_optional_diagnostics_never_discard_completed_native_reads() {
        for scenario in ["DiagHangPartial", "WarnDiagHang"] {
            let home = Home::new("ni-diagnostic-hang");
            let (run, pid, elapsed) = run_ni(&home, scenario, 0, 1000, "300ms");
            let error = run.failure(4);
            assert_eq!(error["code"], "timeout");
            assert!(error.to_string().contains("unconfirmed"), "{error}");
            assert!(elapsed < Duration::from_secs(3), "{scenario}: {elapsed:?}");
            let data = rows(&home);
            footer(&data, "failed");
            let saved = blocks(&data);
            let log = trace(&home);
            isolated(&log, pid);
            if scenario == "DiagHangPartial" {
                assert_eq!(saved.len(), 1);
                assert!(has_code(&error, -200_284));
                assert_eq!(all_bits(&data), ["000000000000f03f", "00000000000004c0"]);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
                assert_eq!(calls(&log, "StopTask", "enter"), [] as [&Value; 0]);
            } else {
                assert_eq!(saved.len(), 2);
                assert!(data.iter().any(|row| has_code(row, 12_345)));
                let oracle: Value =
                    serde_json::from_str(include_str!("fixtures/nidaqmx/abi.json")).unwrap();
                let expected = oracle["patterns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["ieee754_le_hex"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(all_bits(&data), expected);
                cleaned(&log);
                let lookup = calls(&log, "GetErrorString", "enter")[0]["seq"]
                    .as_u64()
                    .unwrap();
                let cleared = calls(&log, "ClearTask", "return")[0]["seq"]
                    .as_u64()
                    .unwrap();
                assert!(lookup > cleared);
            }
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
        }
    }

    #[test]
    fn thermocouple_constructor_preserves_native_units_cjc_words_and_complete_exports() {
        let home = Home::new("ni-tc-native");
        let config = thermocouple_source("Success");
        let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "160ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        let saved = temperature_blocks(&data);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0]["value"]["sample_index"], 0);
        assert_eq!(saved[1]["value"]["sample_index"], 3);
        assert_eq!(temperature_bits(&data), expected_temperature_bits());
        let oracle = tc_oracle();
        let patterns = oracle["manual_sample_patterns"].as_array().unwrap();
        let words = saved
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .collect::<Vec<_>>();
        for (word, pattern) in words.iter().zip(patterns) {
            assert!(word.get("volts").is_none());
            if let Some(decimal) = pattern.get("finite_decimal") {
                assert_eq!(
                    word["temperature"].as_f64().unwrap().to_bits(),
                    decimal.as_str().unwrap().parse::<f64>().unwrap().to_bits()
                );
            } else {
                assert!(word.get("temperature").is_none());
            }
        }
        assert_eq!(words[0]["temperature"], 25.0);
        assert_eq!(words[1]["temperature"], 25.0);
        for block in &saved {
            assert_eq!(
                block["value"]["representation"],
                "driver_scaled_f64_temperature"
            );
            assert_eq!(block["value"]["layout"], "scan_major");
            temperature_metadata(&block["value"], &config);
            assert!(block.get("source_time").is_none());
        }
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        assert_eq!(
            calls(&log, "CreateAIVoltageChan", "enter"),
            [] as [&Value; 0]
        );
        assert_eq!(calls(&log, "SetAITermCfg", "enter"), [] as [&Value; 0]);
        let constructors = calls(&log, "CreateAIThrmcplChan", "arguments");
        assert_eq!(constructors.len(), 2);
        for (call, expected) in constructors
            .iter()
            .zip(oracle["two_channel_arguments"].as_array().unwrap())
        {
            for (actual_key, expected_key) in [
                ("units", "units_code"),
                ("thermocouple_type", "thermocouple_code"),
                ("minimum_bits", "min_bits_hex"),
                ("maximum_bits", "max_bits_hex"),
            ] {
                assert_eq!(call[actual_key], expected[expected_key]);
            }
            assert_eq!(call["cjc_source"], expected["cjc"]["code"]);
            let cjc_bits = expected["cjc"]
                .get("bits_hex")
                .or_else(|| expected["cjc"].get("abi_dummy_bits_hex"))
                .unwrap();
            assert_eq!(&call["cjc_value_bits"], cjc_bits);
            assert_eq!(call["virtual_empty"], true);
            assert_eq!(call["cjc_channel_empty"], true);
        }
        temperature_exports(&home, &data, false);
    }

    #[test]
    fn thermocouple_all_types_units_and_optional_terminal_pairing_are_explicit() {
        for scenario in ["TCAll", "TCExplicit", "TCDefaultEnum"] {
            let home = Home::new("ni-tc-types");
            let mut config = thermocouple_source(scenario);
            let oracle = tc_oracle();
            if scenario == "TCAll" {
                config["channels"] = Value::Array(oracle["all_types_unit_cycle_arguments"].as_array().unwrap().iter().map(|row| {
                    json!({"physical":format!("{scenario}/{}",row["physical_suffix"].as_str().unwrap()),
                        "thermocouple_type":row["thermocouple_type"],"units":row["units"],
                        "min_temperature":row["min_temperature"],"max_temperature":row["max_temperature"],
                        "cjc":{"source":"built_in"}})
                }).collect());
            } else if scenario == "TCExplicit" {
                config["channels"][1]["terminal_config"] = json!("differential");
            } else {
                for channel in config["channels"].as_array_mut().unwrap() {
                    channel["terminal_config"] = json!("default");
                }
            }
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "180ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            for block in temperature_blocks(&data) {
                temperature_metadata(&block["value"], &config);
            }
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            let constructors = calls(&log, "CreateAIThrmcplChan", "arguments");
            assert_eq!(
                constructors.len(),
                config["channels"].as_array().unwrap().len()
            );
            if scenario == "TCAll" {
                for (call, expected) in constructors
                    .iter()
                    .zip(oracle["all_types_unit_cycle_arguments"].as_array().unwrap())
                {
                    assert_eq!(call["units"], expected["units_code"]);
                    assert_eq!(call["thermocouple_type"], expected["thermocouple_code"]);
                    assert_eq!(call["cjc_source"], 10_200);
                    assert_eq!(call["cjc_value_bits"], "0000000000000000");
                }
                let expected = expected_temperature_bits()
                    .into_iter()
                    .cycle()
                    .take(60)
                    .collect::<Vec<_>>();
                assert_eq!(temperature_bits(&data), expected);
            } else {
                assert_eq!(temperature_bits(&data), expected_temperature_bits());
            }
            if scenario == "TCExplicit" {
                let setters = calls(&log, "SetAITermCfg", "arguments");
                assert_eq!(setters.len(), 1);
                assert_eq!(setters[0]["physical"], "TCExplicit/ai1");
                assert_eq!(setters[0]["terminal"], 10_106);
                assert_eq!(
                    setters[0]["seq"].as_u64().unwrap(),
                    constructors[1]["seq"].as_u64().unwrap() + 1
                );
            } else {
                assert_eq!(calls(&log, "SetAITermCfg", "enter"), [] as [&Value; 0]);
            }
        }
    }

    #[test]
    fn selected_measurement_exports_are_resolved_before_any_task_side_effect() {
        for (measurement, variant, terminal, succeeds) in [
            ("voltage", 0, None, true),
            ("thermocouple", 0, None, false),
            ("thermocouple", 1, None, true),
            ("voltage", 1, None, false),
            ("thermocouple", 2, None, true),
            ("thermocouple", 2, Some("default"), true),
            ("thermocouple", 2, Some("differential"), false),
        ] {
            let home = Home::new("ni-selected-export");
            let scenario = if terminal == Some("differential") {
                "TCExplicit"
            } else {
                "Success"
            };
            let mut config = if measurement == "voltage" {
                source(scenario)
            } else {
                thermocouple_source(scenario)
            };
            if let Some(terminal) = terminal {
                config["channels"][1]["terminal_config"] = json!(terminal);
            }
            let (run, pid, _) =
                run_config_ni(&home, &config, &thermocouple_libraries()[variant], "160ms");
            if succeeds {
                run.success();
                let data = rows(&home);
                footer(&data, "complete");
                if measurement == "thermocouple" {
                    assert_eq!(temperature_bits(&data), expected_temperature_bits());
                    for block in temperature_blocks(&data) {
                        temperature_metadata(&block["value"], &config);
                    }
                } else {
                    assert_eq!(all_bits(&data)[0], "000000000000f03f");
                }
            } else {
                let error = run.failure(3);
                assert_eq!(error["code"], "unavailable");
                let data = rows(&home);
                footer(&data, "failed");
                assert_eq!(temperature_blocks(&data), [] as [&Value; 0]);
            }
            let log = trace(&home);
            isolated(&log, pid);
            if succeeds {
                cleaned(&log);
            } else {
                for function in [
                    "CreateTask",
                    "CreateAIThrmcplChan",
                    "CreateAIVoltageChan",
                    "StartTask",
                    "StopTask",
                    "ClearTask",
                ] {
                    assert_eq!(
                        calls(&log, function, "enter"),
                        [] as [&Value; 0],
                        "{function}"
                    );
                }
            }
        }
    }

    #[test]
    fn thermocouple_partial_native_status_warning_and_cleanup_are_preserved() {
        for scenario in [
            "Partial",
            "PartialStopFail",
            "Overrun",
            "FifoOverflow",
            "Warn",
            "Coerced",
            "TCTermError",
        ] {
            let home = Home::new("ni-tc-status");
            let mut config = thermocouple_source(scenario);
            if scenario == "TCTermError" {
                config["channels"][1]["terminal_config"] = json!("differential");
            }
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let complete = matches!(scenario, "Warn" | "Coerced");
            if complete {
                run.success();
            } else {
                let code = match scenario {
                    "Overrun" => -200_279,
                    "FifoOverflow" => -200_361,
                    "TCTermError" => -990_203,
                    _ => -200_284,
                };
                let error = run.failure(if code == -200_284 { 4 } else { 1 });
                assert!(has_code(&error, code));
                if scenario == "PartialStopFail" {
                    assert!(has_code(&error, -990_201));
                }
            }
            let data = rows(&home);
            footer(&data, if complete { "complete" } else { "failed" });
            let saved = temperature_blocks(&data);
            if scenario == "TCTermError" {
                assert_eq!(saved, [] as [&Value; 0]);
            } else {
                assert_eq!(
                    temperature_bits(&data),
                    if complete {
                        expected_temperature_bits()
                    } else {
                        vec!["0000000000003940".to_owned(), "0000000000003940".to_owned()]
                    }
                );
                for block in saved {
                    temperature_metadata(&block["value"], &config);
                }
                if scenario == "Warn" {
                    assert!(data.iter().any(|row| has_code(row, 12_345)));
                }
                if scenario == "Coerced" {
                    assert_eq!(
                        temperature_blocks(&data)[0]["value"]["nominal_rate_hz"],
                        500.0
                    );
                }
                temperature_exports(&home, &data, !complete);
            }
            let log = trace(&home);
            isolated(&log, pid);
            if scenario == "TCTermError" {
                assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
                assert_eq!(calls(&log, "StartTask", "enter"), [] as [&Value; 0]);
            } else {
                cleaned(&log);
            }
        }
    }

    #[test]
    fn thermocouple_bad_geometry_and_hung_diagnostics_keep_only_qualified_prefix() {
        for scenario in ["CountMismatch", "Reverse", "CorruptCountTooLarge"] {
            let home = Home::new("ni-tc-geometry");
            let config = thermocouple_source(scenario);
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            assert_eq!(run.failure(1)["code"], "incomplete");
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(temperature_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
        }
        for scenario in ["DiagHangPartial", "WarnDiagHang"] {
            let home = Home::new("ni-tc-diagnostic-hang");
            let config = thermocouple_source(scenario);
            let (run, pid, elapsed) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let error = run.failure(4);
            assert_eq!(error["code"], "timeout");
            assert!(error.to_string().contains("unconfirmed"));
            assert!(elapsed < Duration::from_secs(3));
            let data = rows(&home);
            footer(&data, "failed");
            let log = trace(&home);
            isolated(&log, pid);
            if scenario == "DiagHangPartial" {
                assert!(has_code(&error, -200_284));
                assert_eq!(
                    temperature_bits(&data),
                    ["0000000000003940", "0000000000003940"]
                );
                assert_eq!(calls(&log, "StopTask", "enter"), [] as [&Value; 0]);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
            } else {
                assert_eq!(temperature_bits(&data), expected_temperature_bits());
                cleaned(&log);
                assert!(data.iter().any(|row| has_code(row, 12_345)));
            }
            temperature_exports(&home, &data, true);
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
        }
    }

    #[test]
    fn current_builtin_external_shunts_preserve_amperes_words_and_exact_exports() {
        let home = Home::new("ni-current-native");
        let config = current_source("Success");
        let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "180ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        let saved = current_blocks(&data);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0]["value"]["sample_index"], 0);
        assert_eq!(saved[1]["value"]["sample_index"], 3);
        assert_eq!(current_bits(&data), expected_current_bits());
        let oracle = current_oracle();
        let patterns = oracle["manual_patterns"].as_array().unwrap();
        let words = saved
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .collect::<Vec<_>>();
        for (word, pattern) in words.iter().zip(patterns) {
            assert!(word.get("volts").is_none());
            assert!(word.get("temperature").is_none());
            if let Some(decimal) = pattern.get("finite_amperes_decimal") {
                assert_eq!(
                    word["amperes"].as_f64().unwrap().to_bits(),
                    decimal.as_str().unwrap().parse::<f64>().unwrap().to_bits()
                );
            } else {
                assert!(word.get("amperes").is_none());
            }
        }
        assert_eq!(words[0]["amperes"], 0.015_625);
        assert_eq!(words[1]["amperes"], 0.015_625);
        assert_eq!(words[2]["amperes"], 0.004);
        assert_eq!(words[3]["amperes"], 0.020);
        for row in data.iter().filter(|row| row["kind"] == "sample") {
            current_metadata(&row["value"], &config);
        }
        for block in &saved {
            assert_eq!(
                block["value"]["representation"],
                "driver_scaled_f64_current"
            );
            assert_eq!(block["value"]["layout"], "scan_major");
            assert!(block.get("source_time").is_none());
            assert!(block["value"].get("min_volts").is_none());
            assert!(block["value"].get("max_volts").is_none());
        }
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        let constructors = calls(&log, "CreateAICurrentChan", "arguments");
        assert_eq!(constructors.len(), 2);
        for (call, expected) in constructors.iter().zip(
            oracle["two_channel_constructor_arguments"]
                .as_array()
                .unwrap(),
        ) {
            for (actual, expected_key) in [
                ("terminal", "terminal_config_code"),
                ("units", "units_code"),
                ("shunt_location", "shunt_location_code"),
                ("minimum_bits", "min_bits_hex"),
                ("maximum_bits", "max_bits_hex"),
                ("external_ohms_argument_bits", "external_argument_bits_hex"),
            ] {
                assert_eq!(call[actual], expected[expected_key]);
            }
            assert_eq!(call["virtual_empty"], true);
            assert_eq!(call["custom_scale_null"], true);
        }
        for function in ["CreateAIVoltageChan", "CreateAIThrmcplChan", "SetAITermCfg"] {
            assert_eq!(calls(&log, function, "enter"), [] as [&Value; 0]);
        }
        temperature_exports(&home, &data, false);
    }

    #[test]
    fn current_default_terminal_is_constructor_input_without_physical_readback_claim() {
        let home = Home::new("ni-current-default");
        let mut config = current_source("CurrentDefault");
        config["channels"][0]["terminal_config"] = json!("default");
        let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "180ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        assert_eq!(current_bits(&data), expected_current_bits());
        for block in current_blocks(&data) {
            current_metadata(&block["value"], &config);
        }
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        let constructors = calls(&log, "CreateAICurrentChan", "arguments");
        assert_eq!(constructors.len(), 2);
        assert_eq!(constructors[0]["terminal"], -1);
        assert_eq!(constructors[1]["terminal"], 10_083);
        assert_eq!(calls(&log, "SetAITermCfg", "enter"), [] as [&Value; 0]);
    }

    #[test]
    fn selected_current_export_is_required_without_requiring_unused_voltage_or_tc_exports() {
        for (measurement, variant, succeeds) in [
            ("voltage", 0, true),
            ("thermocouple", 0, true),
            ("current", 0, false),
            ("current", 1, true),
            ("voltage", 1, false),
            ("thermocouple", 1, false),
        ] {
            let home = Home::new("ni-current-selected-export");
            let config = match measurement {
                "voltage" => source("Success"),
                "thermocouple" => thermocouple_source("Success"),
                _ => current_source("Success"),
            };
            let (run, pid, _) =
                run_config_ni(&home, &config, &current_libraries()[variant], "180ms");
            if succeeds {
                run.success();
                let data = rows(&home);
                footer(&data, "complete");
                match measurement {
                    "current" => {
                        assert_eq!(current_bits(&data), expected_current_bits());
                        for block in current_blocks(&data) {
                            current_metadata(&block["value"], &config);
                        }
                    }
                    "thermocouple" => {
                        assert_eq!(temperature_bits(&data), expected_temperature_bits());
                    }
                    _ => {
                        let old: Value =
                            serde_json::from_str(include_str!("fixtures/nidaqmx/abi.json"))
                                .unwrap();
                        let expected = old["patterns"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
                            .collect::<Vec<_>>();
                        assert_eq!(all_bits(&data), expected);
                    }
                }
            } else {
                assert_eq!(run.failure(3)["code"], "unavailable");
                let data = rows(&home);
                footer(&data, "failed");
                assert_eq!(current_blocks(&data), [] as [&Value; 0]);
            }
            let log = trace(&home);
            isolated(&log, pid);
            if succeeds {
                cleaned(&log);
            } else {
                for function in [
                    "CreateTask",
                    "CreateAICurrentChan",
                    "CreateAIVoltageChan",
                    "CreateAIThrmcplChan",
                    "StartTask",
                    "StopTask",
                    "ClearTask",
                ] {
                    assert_eq!(
                        calls(&log, function, "enter"),
                        [] as [&Value; 0],
                        "{function}"
                    );
                }
            }
        }
    }

    #[test]
    fn current_partial_warning_and_cleanup_keep_native_status_and_requested_shunts() {
        for scenario in [
            "Partial",
            "PartialStopFail",
            "Overrun",
            "FifoOverflow",
            "Warn",
            "Coerced",
            "StopAndClearError",
        ] {
            let home = Home::new("ni-current-status");
            let config = current_source(scenario);
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let complete = matches!(scenario, "Warn" | "Coerced");
            let full = complete || scenario == "StopAndClearError";
            if complete {
                run.success();
            } else {
                let code = match scenario {
                    "Overrun" => -200_279,
                    "FifoOverflow" => -200_361,
                    "StopAndClearError" => -990_201,
                    _ => -200_284,
                };
                let error = run.failure(if code == -200_284 { 4 } else { 1 });
                assert!(has_code(&error, code));
                if scenario == "PartialStopFail" {
                    assert!(has_code(&error, -990_201));
                }
                if scenario == "StopAndClearError" {
                    assert!(has_code(&error, -990_202));
                }
            }
            let data = rows(&home);
            footer(&data, if complete { "complete" } else { "failed" });
            assert_eq!(
                current_bits(&data),
                if full {
                    expected_current_bits()
                } else {
                    vec!["000000000000903f".to_owned(), "000000000000903f".to_owned()]
                }
            );
            for block in current_blocks(&data) {
                current_metadata(&block["value"], &config);
            }
            if scenario == "Warn" {
                assert!(data.iter().any(|row| has_code(row, 12_345)));
            }
            if scenario == "Coerced" {
                assert_eq!(current_blocks(&data)[0]["value"]["nominal_rate_hz"], 500.0);
            }
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            temperature_exports(&home, &data, !complete);
        }
    }

    #[test]
    fn current_bad_geometry_and_hung_diagnostics_retain_only_completed_native_prefix() {
        for scenario in ["CountMismatch", "Reverse", "CorruptCountTooLarge", "Busy"] {
            let home = Home::new("ni-current-geometry");
            let config = current_source(scenario);
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let error = run.failure(1);
            if scenario == "Busy" {
                assert!(has_code(&error, -50_103));
            } else {
                assert_eq!(error["code"], "incomplete");
            }
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(current_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
        }
        for scenario in ["DiagHangPartial", "WarnDiagHang"] {
            let home = Home::new("ni-current-diagnostic-hang");
            let config = current_source(scenario);
            let (run, pid, elapsed) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let error = run.failure(4);
            assert_eq!(error["code"], "timeout");
            assert!(error.to_string().contains("unconfirmed"));
            assert!(elapsed < Duration::from_secs(3));
            let data = rows(&home);
            footer(&data, "failed");
            let log = trace(&home);
            isolated(&log, pid);
            if scenario == "DiagHangPartial" {
                assert!(has_code(&error, -200_284));
                assert_eq!(
                    current_bits(&data),
                    ["000000000000903f", "000000000000903f"]
                );
                assert_eq!(calls(&log, "StopTask", "enter"), [] as [&Value; 0]);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
            } else {
                assert_eq!(current_bits(&data), expected_current_bits());
                cleaned(&log);
                assert!(data.iter().any(|row| has_code(row, 12_345)));
            }
            assert_eq!(
                calls(&log, "ReadAnalogF64", "enter").len(),
                if scenario == "DiagHangPartial" { 1 } else { 2 }
            );
            temperature_exports(&home, &data, true);
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
        }
    }

    #[test]
    fn on_demand_all_measurements_preserve_one_scan_raw_words_and_exact_exports() {
        for (measurement, original, event, expected) in [
            ("voltage", source("Success"), "analog_voltage_block", {
                let oracle: Value =
                    serde_json::from_str(include_str!("fixtures/nidaqmx/on-demand.json")).unwrap();
                oracle["patterns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            }),
            (
                "thermocouple",
                thermocouple_source("Success"),
                "thermocouple_temperature_block",
                expected_temperature_bits(),
            ),
            (
                "current",
                current_source("Success"),
                "analog_current_block",
                expected_current_bits(),
            ),
            (
                "rtd",
                rtd_source("Success"),
                "rtd_temperature_block",
                expected_rtd_bits(),
            ),
        ] {
            let home = Home::new("ni-software-measurement");
            let config = on_demand(original, 10);
            let (run, pid, _) = run_config_ni(&home, &config, &on_demand_libraries()[0], "400ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let saved = data
                .iter()
                .filter(|row| row["kind"] == "sample" && row["value"]["event"] == event)
                .collect::<Vec<_>>();
            assert!(saved.len() >= 6, "{measurement}: {} scans", saved.len());
            let mut actual = Vec::new();
            for (index, row) in saved.iter().enumerate() {
                let value = &row["value"];
                assert_eq!(value["samples_per_channel"], 1);
                assert_eq!(
                    value["sample_index"].as_u64().unwrap(),
                    u64::try_from(index).unwrap()
                );
                assert_eq!(value["layout"], "scan_major");
                assert!(row.get("source_time").is_none());
                let scans = value["samples"].as_array().unwrap();
                assert_eq!(scans.len(), 1);
                assert_eq!(scans[0].as_array().unwrap().len(), 2);
                actual.extend(
                    scans[0]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|word| word["ieee754_le_hex"].as_str().unwrap().to_owned()),
                );
            }
            assert_eq!(
                actual,
                expected
                    .iter()
                    .cycle()
                    .take(actual.len())
                    .cloned()
                    .collect::<Vec<_>>()
            );
            for row in data.iter().filter(|row| row["kind"] == "sample") {
                software_metadata(&row["value"], 10);
                match measurement {
                    "thermocouple" => temperature_metadata(&row["value"], &config),
                    "current" => current_metadata(&row["value"], &config),
                    "rtd" => rtd_metadata(&row["value"], &config),
                    _ => {
                        assert!(row["value"].get("measurement").is_none());
                    }
                }
            }
            assert_eq!(
                data.iter()
                    .filter(|row| row["kind"] == "sample" && row["value"]["event"] == "configured")
                    .count(),
                1
            );
            let log = trace(&home);
            isolated(&log, pid);
            software_calls(&log);
            cleaned(&log);
            assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), saved.len());
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
            assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
            temperature_exports(&home, &data, false);
        }
    }

    #[test]
    fn on_demand_missing_selected_exports_and_continuous_library_conflicts_have_no_task_effect() {
        for (config, variant) in [
            (on_demand(source("Success"), 25), 1),
            (source("Success"), 0),
        ] {
            let home = Home::new("ni-software-missing");
            let (run, pid, _) =
                run_config_ni(&home, &config, &on_demand_libraries()[variant], "200ms");
            assert_eq!(run.failure(3)["code"], "unavailable");
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 0);
            assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 0);
            assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 0);
        }
    }

    #[test]
    fn on_demand_zero_invalid_and_negative_counts_never_qualify_unreceived_words() {
        for (scenario, exit, retained) in [
            ("ZeroRead", 1, false),
            ("WarnZeroRead", 1, false),
            ("CorruptCountNegative", 1, false),
            ("CorruptCountTooLarge", 1, false),
            ("NegativeNoData", 4, false),
            ("Partial", 4, true),
            ("PartialStopFail", 4, true),
        ] {
            let home = Home::new("ni-software-count");
            let config = on_demand(source(scenario), 25);
            let (run, pid, _) = run_config_ni(&home, &config, &on_demand_libraries()[0], "300ms");
            let error = run.failure(exit);
            assert_eq!(
                error["code"],
                if exit == 1 { "incomplete" } else { "timeout" }
            );
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(blocks(&data).len(), usize::from(retained));
            if retained {
                assert_eq!(all_bits(&data), ["000000000000f03f", "00000000000004c0"]);
                assert_eq!(blocks(&data)[0]["value"]["samples_per_channel"], 1);
                assert_eq!(blocks(&data)[0]["value"]["status_code"], -200_284);
            }
            if exit == 4 {
                assert!(has_code(&error, -200_284));
            }
            if exit == 1 {
                assert_eq!(error["details"]["requested_samples_per_channel"], 1);
                assert_eq!(
                    error["details"]["actual_samples_per_channel"],
                    match scenario {
                        "CorruptCountNegative" => -1,
                        "CorruptCountTooLarge" => 2,
                        _ => 0,
                    }
                );
                assert_eq!(
                    error["details"]["ni_daqmx_status"],
                    if scenario == "WarnZeroRead" {
                        12_345
                    } else {
                        0
                    }
                );
            }
            if scenario == "PartialStopFail" {
                assert!(has_code(&error, -990_201));
            }
            if scenario == "WarnZeroRead" {
                assert!(data.iter().any(|row| has_code(row, 12_345)) || has_code(&error, 12_345));
            }
            let log = trace(&home);
            isolated(&log, pid);
            software_calls(&log);
            cleaned(&log);
            assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
            temperature_exports(&home, &data, true);
        }
    }

    #[test]
    fn on_demand_original_deadline_and_hung_diagnostics_preserve_only_timely_native_prefix() {
        for scenario in ["ReadHang", "LateRead", "DiagHangPartial", "WarnDiagHang"] {
            let home = Home::new("ni-software-deadline");
            let mut config = on_demand(source(scenario), 10);
            config["timeout_ms"] = json!(250);
            let (run, pid, elapsed) =
                run_config_ni(&home, &config, &on_demand_libraries()[0], "300ms");
            let error = run.failure(4);
            assert_eq!(error["code"], "timeout");
            assert!(elapsed < Duration::from_secs(3), "{scenario}: {elapsed:?}");
            let data = rows(&home);
            footer(&data, "failed");
            let saved = blocks(&data);
            match scenario {
                "DiagHangPartial" => {
                    assert_eq!(saved.len(), 1);
                    assert_eq!(all_bits(&data), ["000000000000f03f", "00000000000004c0"]);
                    assert!(has_code(&error, -200_284));
                }
                "WarnDiagHang" => {
                    assert_ne!(saved, [] as [&Value; 0]);
                    assert!(data.iter().any(|row| has_code(row, 12_345)));
                    let oracle: Value =
                        serde_json::from_str(include_str!("fixtures/nidaqmx/on-demand.json"))
                            .unwrap();
                    let expected = oracle["patterns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
                        .collect::<Vec<_>>();
                    let actual = all_bits(&data);
                    assert_eq!(
                        actual,
                        expected
                            .iter()
                            .cycle()
                            .take(actual.len())
                            .cloned()
                            .collect::<Vec<_>>()
                    );
                }
                _ => assert_eq!(saved, [] as [&Value; 0]),
            }
            let log = trace(&home);
            isolated(&log, pid);
            software_calls(&log);
            if scenario == "DiagHangPartial" {
                assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
            }
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
            temperature_exports(&home, &data, true);
        }
    }

    #[test]
    fn rtd_fixed_constructor_preserves_asymmetric_scientific_args_raw_words_and_exact_exports() {
        let home = Home::new("ni-rtd-native");
        let config = rtd_source("RtdSuccess");
        let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "180ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        assert_eq!(rtd_bits(&data), expected_rtd_bits());
        let saved = rtd_blocks(&data);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0]["value"]["sample_index"], 0);
        assert_eq!(saved[1]["value"]["sample_index"], 3);
        let oracle = rtd_oracle();
        let patterns = oracle["manual_temperature_patterns"].as_array().unwrap();
        let words = saved
            .iter()
            .flat_map(|row| row["value"]["samples"].as_array().unwrap())
            .flat_map(|scan| scan.as_array().unwrap())
            .collect::<Vec<_>>();
        for (sample, pattern) in words.iter().zip(patterns) {
            assert!(sample.get("volts").is_none());
            assert!(sample.get("amperes").is_none());
            if pattern["finite"] == true {
                let expected = pattern["round_trip_temperature_decimal"]
                    .as_str()
                    .unwrap()
                    .parse::<f64>()
                    .unwrap();
                assert_eq!(
                    sample["temperature"].as_f64().unwrap().to_bits(),
                    expected.to_bits()
                );
            } else {
                assert!(sample.get("temperature").is_none());
            }
        }
        assert_eq!(words[0]["temperature"], 25.0);
        assert_eq!(words[1]["temperature"], 25.0);
        for row in data.iter().filter(|row| row["kind"] == "sample") {
            rtd_metadata(&row["value"], &config);
        }
        for row in saved {
            assert_eq!(
                row["value"]["representation"],
                "driver_scaled_f64_rtd_temperature"
            );
            assert_eq!(row["value"]["layout"], "scan_major");
            assert!(row.get("source_time").is_none());
        }
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        rtd_arguments(
            &log,
            oracle["asymmetric_two_channel_constructor_controls"]
                .as_array()
                .unwrap(),
        );
        for function in [
            "CreateAIVoltageChan",
            "CreateAIThrmcplChan",
            "CreateAICurrentChan",
            "SetAITermCfg",
        ] {
            assert_eq!(calls(&log, function, "enter").len(), 0);
        }
        temperature_exports(&home, &data, false);
    }

    #[test]
    fn rtd_all_standard_types_temperature_units_wire_counts_and_explicit_excitation_are_distinct() {
        let home = Home::new("ni-rtd-enums");
        let mut config = rtd_source("RtdAll");
        let oracle = rtd_oracle();
        let words = oracle["manual_argument_words"].as_array().unwrap();
        let number = |name: &Value| {
            let bits = words.iter().find(|word| word["name"] == *name).unwrap()["bits_hex"]
                .as_str()
                .unwrap();
            f64::from_bits(u64::from_str_radix(bits, 16).unwrap())
        };
        let controls = oracle["six_channel_enum_controls"].as_array().unwrap();
        config["channels"] = Value::Array(controls.iter().map(|row| json!({
            "physical":format!("RtdAll/ai{}",row["channel_index"].as_u64().unwrap()),
            "rtd_type":row["rtd_type"],"units":row["units"],"resistance_config":row["wire"],
            "min_temperature":number(&row["min_word"]),"max_temperature":number(&row["max_word"]),
            "r0_ohms":number(&row["r0_word"]),
            "excitation":{"source":row["excitation_source"],"amperes":number(&row["excitation_word"])}
        })).collect());
        let (run, pid, _) = run_config_ni(&home, &config, &rtd_libraries()[1], "180ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        let actual = rtd_bits(&data);
        let expected = expected_rtd_bits();
        assert_eq!(actual.len(), 36);
        assert_eq!(
            actual,
            expected
                .iter()
                .cycle()
                .take(36)
                .cloned()
                .collect::<Vec<_>>()
        );
        for row in data.iter().filter(|row| row["kind"] == "sample") {
            rtd_metadata(&row["value"], &config);
        }
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        rtd_arguments(&log, controls);
        temperature_exports(&home, &data, false);
    }

    #[test]
    fn rtd_symbol_selection_is_measurement_and_timing_specific_before_task_creation() {
        for config in [
            source("Success"),
            thermocouple_source("Success"),
            current_source("Success"),
        ] {
            let home = Home::new("ni-rtd-unused");
            let (run, pid, _) = run_config_ni(&home, &config, &rtd_libraries()[0], "180ms");
            run.success();
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            assert_eq!(calls(&log, "CreateAIRTDChan", "enter").len(), 0);
        }
        let home = Home::new("ni-rtd-missing");
        let (run, pid, _) = run_config_ni(
            &home,
            &rtd_source("RtdSuccess"),
            &rtd_libraries()[0],
            "180ms",
        );
        assert_eq!(run.failure(3)["code"], "unavailable");
        let log = trace(&home);
        isolated(&log, pid);
        for function in [
            "CreateTask",
            "CreateAIRTDChan",
            "ReadAnalogF64",
            "StopTask",
            "ClearTask",
        ] {
            assert_eq!(calls(&log, function, "enter").len(), 0);
        }
        for (variant, software) in [(1, false), (2, true)] {
            let home = Home::new("ni-rtd-only");
            let config = if software {
                on_demand(rtd_source("RtdSuccess"), 10)
            } else {
                rtd_source("RtdSuccess")
            };
            let (run, pid, _) = run_config_ni(&home, &config, &rtd_libraries()[variant], "200ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let saved = rtd_blocks(&data);
            assert_ne!(saved, [] as [&Value; 0]);
            let actual = rtd_bits(&data);
            let expected = expected_rtd_bits();
            assert_eq!(
                actual,
                expected
                    .iter()
                    .cycle()
                    .take(actual.len())
                    .cloned()
                    .collect::<Vec<_>>()
            );
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            if software {
                software_calls(&log);
                for row in data.iter().filter(|row| row["kind"] == "sample") {
                    software_metadata(&row["value"], 10);
                }
            }
            for function in [
                "CreateAIVoltageChan",
                "CreateAIThrmcplChan",
                "CreateAICurrentChan",
                "SetAITermCfg",
            ] {
                assert_eq!(calls(&log, function, "enter").len(), 0);
            }
            temperature_exports(&home, &data, false);
        }
    }

    #[test]
    fn rtd_partial_native_status_warning_and_cleanup_retain_requested_physics_without_conversion() {
        for scenario in ["Partial", "PartialStopFail", "Warn", "Busy"] {
            let home = Home::new("ni-rtd-native-status");
            let config = rtd_source(scenario);
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let error = if scenario == "Warn" {
                run.success();
                None
            } else {
                Some(run.failure(if scenario == "Busy" { 1 } else { 4 }))
            };
            let data = rows(&home);
            footer(
                &data,
                if error.is_none() {
                    "complete"
                } else {
                    "failed"
                },
            );
            let saved = rtd_blocks(&data);
            let log = trace(&home);
            isolated(&log, pid);
            if scenario == "Busy" {
                assert_eq!(saved, [] as [&Value; 0]);
                assert!(has_code(error.as_ref().unwrap(), -50_103));
                assert_eq!(calls(&log, "StartTask", "enter").len(), 0);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
                assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            } else {
                cleaned(&log);
                if scenario == "Warn" {
                    assert_eq!(rtd_bits(&data), expected_rtd_bits());
                    assert!(data.iter().any(|row| has_code(row, 12_345)));
                } else {
                    assert_eq!(saved.len(), 1);
                    assert_eq!(rtd_bits(&data), ["0000000000003940", "0000000000003940"]);
                    assert!(has_code(error.as_ref().unwrap(), -200_284));
                }
                if scenario == "PartialStopFail" {
                    assert!(has_code(error.as_ref().unwrap(), -990_201));
                }
            }
            for row in data.iter().filter(|row| row["kind"] == "sample") {
                rtd_metadata(&row["value"], &config);
            }
            temperature_exports(&home, &data, error.is_some());
        }
    }

    #[test]
    fn rtd_bad_geometry_and_hung_diagnostics_keep_only_qualified_native_prefix() {
        for scenario in ["CorruptCountNegative", "CorruptCountTooLarge", "Reverse"] {
            let home = Home::new("ni-rtd-geometry");
            let config = rtd_source(scenario);
            let (run, pid, _) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            assert_eq!(run.failure(1)["code"], "incomplete");
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(rtd_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            temperature_exports(&home, &data, true);
        }
        for scenario in ["DiagHangPartial", "WarnDiagHang"] {
            let home = Home::new("ni-rtd-diaghang");
            let config = rtd_source(scenario);
            let (run, pid, elapsed) = run_config_ni(&home, &config, &libraries()[0], "300ms");
            let error = run.failure(4);
            assert!(elapsed < Duration::from_secs(3), "{scenario}: {elapsed:?}");
            let data = rows(&home);
            footer(&data, "failed");
            if scenario == "DiagHangPartial" {
                assert_eq!(rtd_bits(&data), ["0000000000003940", "0000000000003940"]);
                assert!(has_code(&error, -200_284));
            } else {
                assert_eq!(rtd_bits(&data), expected_rtd_bits());
                assert!(data.iter().any(|row| has_code(row, 12_345)));
            }
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(
                calls(&log, "ReadAnalogF64", "enter").len(),
                if scenario == "DiagHangPartial" { 1 } else { 2 }
            );
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
            temperature_exports(&home, &data, true);
        }
    }

    #[test]
    fn mixed_one_owned_task_preserves_four_column_science_and_exports_for_both_timings() {
        let oracle = mixed_oracle();
        let expected = expected_mixed_bits();
        for software in [false, true] {
            let home = Home::new("ni-mixed-science");
            let config = if software {
                on_demand(mixed_source("MixedSuccess"), 10)
            } else {
                mixed_source("MixedSuccess")
            };
            let library = if software {
                &on_demand_libraries()[0]
            } else {
                &libraries()[0]
            };
            let (run, pid, _) = run_config_ni(&home, &config, library, "250ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let saved = mixed_blocks(&data);
            assert_ne!(saved, [] as [&Value; 0]);
            let actual = mixed_bits(&data);
            if software {
                assert!(saved.len() >= 2);
                assert_eq!(
                    actual,
                    expected
                        .iter()
                        .cycle()
                        .take(actual.len())
                        .cloned()
                        .collect::<Vec<_>>()
                );
            } else {
                assert_eq!(saved.len(), 1);
                assert_eq!(actual, expected);
            }
            for (index, row) in saved.iter().enumerate() {
                assert_eq!(
                    row["value"]["representation"],
                    "driver_scaled_f64_mixed_analog"
                );
                assert_eq!(row["value"]["layout"], "scan_major");
                assert_eq!(
                    row["value"]["sample_index"].as_u64().unwrap(),
                    if software {
                        u64::try_from(index).unwrap()
                    } else {
                        0
                    }
                );
                assert_eq!(
                    row["value"]["samples_per_channel"],
                    if software { 1 } else { 2 }
                );
            }
            let samples = saved
                .iter()
                .flat_map(|row| row["value"]["samples"].as_array().unwrap())
                .flat_map(|scan| scan.as_array().unwrap())
                .collect::<Vec<_>>();
            for (sample, pattern) in samples.iter().zip(
                oracle["manual_scan_major_patterns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cycle(),
            ) {
                let column = usize::try_from(pattern["column_index"].as_u64().unwrap()).unwrap();
                let key = ["volts", "temperature", "amperes", "temperature"][column];
                assert!(sample.get("measurement").is_none());
                assert!(sample.get("kind").is_none());
                assert!(sample.get("units").is_none());
                for other in ["volts", "temperature", "amperes"] {
                    if other != key {
                        assert!(sample.get(other).is_none());
                    }
                }
                if pattern["finite"] == true {
                    assert_eq!(
                        sample[key].as_f64().unwrap().to_bits(),
                        pattern["finite_decimal"]
                            .as_str()
                            .unwrap()
                            .parse::<f64>()
                            .unwrap()
                            .to_bits()
                    );
                } else {
                    assert!(sample.get(key).is_none());
                }
            }
            for row in data.iter().filter(|row| row["kind"] == "sample") {
                mixed_metadata(&row["value"], &config);
                if software {
                    software_metadata(&row["value"], 10);
                }
            }
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            mixed_arguments(&log);
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
            assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
            assert_eq!(calls(&log, "SetAITermCfg", "enter").len(), 0);
            if software {
                assert_eq!(calls(&log, "GetReadAvailSampPerChan", "enter").len(), 0);
                assert_eq!(calls(&log, "GetSampClkRate", "enter").len(), 0);
            }
            temperature_exports(&home, &data, false);
        }
    }

    #[test]
    fn mixed_same_family_overrides_need_only_the_selected_voltage_constructor() {
        for software in [false, true] {
            let home = Home::new("ni-mixed-voltage");
            let mut config = mixed_source("MixedVoltage");
            config["channels"] = json!([
                {"measurement":"voltage","physical":"MixedVoltage/ai0","terminal_config":"differential","min_volts":-10.0,"max_volts":10.0},
                {"measurement":"voltage","physical":"MixedVoltage/ai1","terminal_config":"rse","min_volts":-2.5,"max_volts":2.5}
            ]);
            config["max_samples_per_channel"] = json!(3);
            if software {
                config = on_demand(config, 10);
            }
            let (run, pid, _) = run_config_ni(
                &home,
                &config,
                &mixed_libraries()[if software { 6 } else { 5 }],
                "250ms",
            );
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let actual = mixed_bits(&data);
            let old: Value =
                serde_json::from_str(include_str!("fixtures/nidaqmx/abi.json")).unwrap();
            let expected = old["patterns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["ieee754_le_hex"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            if software {
                assert!(actual.len() >= 4);
                assert_eq!(
                    actual,
                    expected
                        .iter()
                        .cycle()
                        .take(actual.len())
                        .cloned()
                        .collect::<Vec<_>>()
                );
            } else {
                assert_eq!(actual, expected);
            }
            for row in data.iter().filter(|row| row["kind"] == "sample") {
                mixed_metadata(&row["value"], &config);
            }
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            let constructors = calls(&log, "CreateAIVoltageChan", "arguments");
            assert_eq!(constructors.len(), 2);
            let oracle = mixed_oracle();
            let controls = oracle["same_family_override_control"]["channels"]
                .as_array()
                .unwrap();
            let words = oracle["manual_argument_words"].as_array().unwrap();
            for (actual, expected) in constructors.iter().zip(controls) {
                assert_eq!(actual["terminal"], expected["terminal_config_code"]);
                for (field, reference) in
                    [("minimum_bits", "min_word"), ("maximum_bits", "max_word")]
                {
                    assert_eq!(
                        actual[field],
                        words
                            .iter()
                            .find(|row| row["name"] == expected[reference])
                            .unwrap()["bits_hex"]
                    );
                }
            }
            for function in [
                "CreateAIThrmcplChan",
                "CreateAICurrentChan",
                "CreateAIRTDChan",
                "SetAITermCfg",
            ] {
                assert_eq!(calls(&log, function, "enter").len(), 0);
            }
            temperature_exports(&home, &data, false);
        }
    }

    #[test]
    fn mixed_constructor_union_and_conditional_tc_setter_are_resolved_before_create() {
        for variant in 0..4 {
            let home = Home::new("ni-mixed-selected-missing");
            let (run, pid, _) = run_config_ni(
                &home,
                &mixed_source("MixedSuccess"),
                &mixed_libraries()[variant],
                "200ms",
            );
            assert_eq!(run.failure(3)["code"], "unavailable");
            let log = trace(&home);
            isolated(&log, pid);
            for function in ["CreateTask", "ReadAnalogF64", "StopTask", "ClearTask"] {
                assert_eq!(calls(&log, function, "enter").len(), 0);
            }
        }
        for scenario in ["MixedSuccess", "MixedTCDefaultEnum", "MixedTCExplicit"] {
            let home = Home::new("ni-mixed-tc-setter");
            let mut config = mixed_source(scenario);
            if scenario != "MixedSuccess" {
                config["channels"][1]["terminal_config"] =
                    json!(if scenario == "MixedTCExplicit" {
                        "differential"
                    } else {
                        "default"
                    });
            }
            let library = if scenario == "MixedTCExplicit" {
                &libraries()[0]
            } else {
                &mixed_libraries()[4]
            };
            let (run, pid, _) = run_config_ni(&home, &config, library, "200ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            assert_eq!(mixed_bits(&data), expected_mixed_bits());
            for row in data.iter().filter(|row| row["kind"] == "sample") {
                mixed_metadata(&row["value"], &config);
            }
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            let setter = calls(&log, "SetAITermCfg", "enter");
            assert_eq!(setter.len(), usize::from(scenario == "MixedTCExplicit"));
            if scenario == "MixedTCExplicit" {
                let seq = setter[0]["seq"].as_u64().unwrap();
                assert!(
                    calls(&log, "CreateAIThrmcplChan", "return")[0]["seq"]
                        .as_u64()
                        .unwrap()
                        < seq
                );
                assert!(
                    seq < calls(&log, "CreateAICurrentChan", "enter")[0]["seq"]
                        .as_u64()
                        .unwrap()
                );
            }
            temperature_exports(&home, &data, false);
        }
        let home = Home::new("ni-mixed-setter-missing");
        let mut config = mixed_source("MixedTCExplicit");
        config["channels"][1]["terminal_config"] = json!("differential");
        let (run, pid, _) = run_config_ni(&home, &config, &mixed_libraries()[4], "200ms");
        assert_eq!(run.failure(3)["code"], "unavailable");
        let log = trace(&home);
        isolated(&log, pid);
        assert_eq!(calls(&log, "CreateTask", "enter").len(), 0);
    }

    #[test]
    fn mixed_later_constructor_and_driver_geometry_never_start_or_publish_partial_setup() {
        for scenario in [
            "MixedLaterFail",
            "MixedCountMismatch",
            "MixedReverse",
            "MixedCorruptCountNegative",
            "MixedCorruptCountTooLarge",
        ] {
            let home = Home::new("ni-mixed-geometry");
            let (run, pid, _) =
                run_config_ni(&home, &mixed_source(scenario), &libraries()[0], "250ms");
            let error = run.failure(1);
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(mixed_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            if scenario == "MixedLaterFail" {
                assert!(has_code(&error, -990_104));
                assert_eq!(calls(&log, "CreateAIRTDChan", "enter").len(), 1);
                assert_eq!(calls(&log, "StartTask", "enter").len(), 0);
                assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 0);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
            } else {
                assert_eq!(error["code"], "incomplete");
            }
            temperature_exports(&home, &data, true);
        }
    }

    #[test]
    fn mixed_partial_warning_and_deferred_diagnostics_keep_each_original_column() {
        let expected = expected_mixed_bits();
        for software in [false, true] {
            for scenario in [
                "MixedPartial",
                "MixedPartialStopFail",
                "MixedWarn",
                "MixedDiagHangPartial",
                "MixedWarnDiagHang",
            ] {
                let home = Home::new("ni-mixed-native-status");
                let config = if software {
                    on_demand(mixed_source(scenario), 10)
                } else {
                    mixed_source(scenario)
                };
                let library = if software {
                    &on_demand_libraries()[0]
                } else {
                    &libraries()[0]
                };
                let (run, pid, elapsed) = run_config_ni(&home, &config, library, "250ms");
                let incomplete = scenario != "MixedWarn";
                let error = if incomplete {
                    Some(run.failure(4))
                } else {
                    run.success();
                    None
                };
                assert!(elapsed < Duration::from_secs(3), "{scenario}: {elapsed:?}");
                let data = rows(&home);
                footer(&data, if incomplete { "failed" } else { "complete" });
                let actual = mixed_bits(&data);
                if scenario == "MixedPartial"
                    || scenario == "MixedPartialStopFail"
                    || scenario == "MixedDiagHangPartial"
                {
                    assert_eq!(actual, expected[..4]);
                    assert!(has_code(error.as_ref().unwrap(), -200_284));
                } else {
                    if software {
                        assert!(actual.len() >= 4);
                        assert_eq!(
                            actual,
                            expected
                                .iter()
                                .cycle()
                                .take(actual.len())
                                .cloned()
                                .collect::<Vec<_>>()
                        );
                    } else {
                        assert_eq!(actual, expected);
                    }
                    assert!(data.iter().any(|row| has_code(row, 12_345)));
                }
                if scenario == "MixedPartialStopFail" {
                    assert!(has_code(error.as_ref().unwrap(), -990_201));
                }
                for row in data.iter().filter(|row| row["kind"] == "sample") {
                    mixed_metadata(&row["value"], &config);
                }
                let log = trace(&home);
                isolated(&log, pid);
                if scenario == "MixedPartial"
                    || scenario == "MixedPartialStopFail"
                    || scenario == "MixedDiagHangPartial"
                {
                    assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
                }
                if scenario == "MixedDiagHangPartial" {
                    assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
                    assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
                } else {
                    cleaned(&log);
                }
                let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
                assert!(!support::cli::process_lives(worker));
                temperature_exports(&home, &data, incomplete);
            }
        }
    }

    #[test]
    fn digital_ports_bind_assigned_order_widths_and_preserve_full_unsigned_states_and_exports() {
        let home = Home::new("ni-digital-words");
        let config = digital_source("DigitalSuccess");
        let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[0], "350ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        let actual = input_bits(&data, 0);
        let expected = expected_input_bits(0);
        assert!(actual.len() >= 12);
        assert_eq!(
            actual,
            expected
                .iter()
                .cycle()
                .take(actual.len())
                .cloned()
                .collect::<Vec<_>>()
        );
        input_words(&data, 0);
        for row in data.iter().filter(|r| r["kind"] == "sample") {
            input_metadata(&row["value"], &config, 0);
        }
        let log = trace(&home);
        isolated(&log, pid);
        input_cleaned(&log, 0);
        input_calls(&log, 0);
        let o = input_oracle(false);
        let constructors = calls(&log, "CreateDIChan", "arguments");
        assert_eq!(constructors.len(), 3);
        for (actual, expected) in constructors
            .iter()
            .zip(o["constructor_and_identity_controls"].as_array().unwrap())
        {
            assert_eq!(actual["physical"], expected["requested_physical"]);
            assert_eq!(actual["assigned"], expected["assigned_virtual_name"]);
            assert_eq!(actual["grouping"], 1);
            assert_eq!(actual["index"], expected["channel_index"]);
        }
        let widths = calls(&log, "GetDINumLines", "output");
        assert_eq!(widths.len(), 3);
        for (i, width) in widths.iter().enumerate() {
            assert_eq!(width["channel"], format!("cfdp{i:04}"));
            assert_eq!(width["width"], [1, 8, 32][i]);
        }
        assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
        assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
        temperature_exports(&home, &data, false);
    }
    #[test]
    fn counter_count_and_frequency_words_and_explicit_constructor_terminal_requests_are_exact() {
        for family in [1, 2] {
            let home = Home::new("ni-counter-words");
            let config = input_config(family, "Success");
            let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[family], "350ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let actual = input_bits(&data, family);
            let expected = expected_input_bits(family);
            assert!(actual.len() >= 8);
            assert_eq!(
                actual,
                expected
                    .iter()
                    .cycle()
                    .take(actual.len())
                    .cloned()
                    .collect::<Vec<_>>()
            );
            input_words(&data, family);
            for row in data.iter().filter(|r| r["kind"] == "sample") {
                input_metadata(&row["value"], &config, family);
            }
            let log = trace(&home);
            isolated(&log, pid);
            input_cleaned(&log, family);
            input_calls(&log, family);
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
            assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
            for setter in ["SetCICountEdgesTerm", "SetCIFreqTerm"] {
                assert_eq!(calls(&log, setter, "enter").len(), 0);
            }
            if family == 1 {
                let a = calls(&log, "CreateCICountEdgesChan", "arguments");
                assert_eq!(a.len(), 1);
                let a = a[0];
                assert_eq!(a["edge"], 10171);
                assert_eq!(a["initial_u32"], 2_147_483_649_u32);
                assert_eq!(a["direction"], 10124);
                assert_eq!(a["virtual_empty"], true);
            } else {
                let a = calls(&log, "CreateCIFreqChan", "arguments");
                assert_eq!(a.len(), 1);
                let a = a[0];
                let o = input_oracle(true);
                for (key, index) in [
                    ("minimum_bits", 0),
                    ("maximum_bits", 1),
                    ("measurement_time_bits", 2),
                ] {
                    assert_eq!(a[key], o["manual_argument_words"][index]["bits_hex"]);
                }
                assert_eq!(a["units"], 10373);
                assert_eq!(a["edge"], 10171);
                assert_eq!(a["method"], 10205);
                assert_eq!(a["divisor_u32"], 7);
                assert_eq!(a["custom_scale_null"], true);
            }
            temperature_exports(&home, &data, false);
        }
        for family in [1, 2] {
            let home = Home::new("ni-counter-terminal");
            let mut config = if family == 1 {
                counter_source("CountUp", false)
            } else {
                counter_source("FrequencyExplicit", true)
            };
            let terminal = if family == 1 {
                "/CountUp/PFI3"
            } else {
                "/FrequencyExplicit/PFI11"
            };
            config["channels"][0]["input_terminal"] = json!(terminal);
            let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[family], "200ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            input_words(&data, family);
            for row in data.iter().filter(|r| r["kind"] == "sample") {
                input_metadata(&row["value"], &config, family);
            }
            let log = trace(&home);
            isolated(&log, pid);
            input_cleaned(&log, family);
            input_calls(&log, family);
            let selected = if family == 1 {
                "SetCICountEdgesTerm"
            } else {
                "SetCIFreqTerm"
            };
            let a = calls(&log, selected, "arguments");
            assert_eq!(a.len(), 1);
            assert_eq!(a[0]["channel"], config["channels"][0]["physical"]);
            assert_eq!(a[0]["terminal"], terminal);
            assert!(
                a[0]["seq"].as_u64().unwrap()
                    < calls(&log, "StartTask", "enter")[0]["seq"]
                        .as_u64()
                        .unwrap()
            );
            if family == 1 {
                let a = calls(&log, "CreateCICountEdgesChan", "arguments")[0];
                assert_eq!(a["edge"], 10280);
                assert_eq!(a["initial_u32"], 17);
                assert_eq!(a["direction"], 10128);
            }
            temperature_exports(&home, &data, false);
        }
        let o = input_oracle(true);
        for (index, method) in o["frequency_method_controls"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let home = Home::new("ni-frequency-method");
            let mut config = counter_source(&format!("FrequencyMethod{index}"), true);
            config["channels"][0]["method"] = method["method"].clone();
            let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[2], "150ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            input_words(&data, 2);
            let log = trace(&home);
            isolated(&log, pid);
            input_cleaned(&log, 2);
            assert_eq!(
                calls(&log, "CreateCIFreqChan", "arguments")[0]["method"],
                method["code"]
            );
        }
    }
    #[test]
    fn digital_and_counter_selected_exports_fail_before_create_and_unused_ai_or_input_exports_are_absent()
     {
        for (variant, family) in [(3, 0), (4, 0), (5, 0), (6, 1), (7, 1), (8, 2), (9, 2)] {
            let home = Home::new("ni-input-symbol");
            let (run, pid, _) = run_config_ni(
                &home,
                &input_config(family, "Success"),
                &input_libraries()[variant],
                "150ms",
            );
            assert_eq!(run.failure(3)["code"], "unavailable");
            let log = trace(&home);
            isolated(&log, pid);
            for name in [
                "CreateTask",
                "StartTask",
                "StopTask",
                "ClearTask",
                "ReadDigitalU32",
                "ReadCounterU32",
                "ReadCounterF64",
            ] {
                assert_eq!(calls(&log, name, "enter").len(), 0);
            }
        }
        for (family, variant) in [(1, 10), (2, 11)] {
            let home = Home::new("ni-input-setter-symbol");
            let mut config = input_config(family, "Success");
            config["channels"][0]["input_terminal"] = json!(if family == 1 {
                "/CountSuccess/PFI3"
            } else {
                "/FrequencySuccess/PFI11"
            });
            let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[variant], "150ms");
            assert_eq!(run.failure(3)["code"], "unavailable");
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 0);
            let home = Home::new("ni-input-no-setter");
            let config = input_config(family, "Success");
            let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[variant], "150ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            assert_ne!(input_blocks(&data, family), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            input_cleaned(&log, family);
            input_calls(&log, family);
        }
        let home = Home::new("ni-ai-no-input-symbols");
        let config = on_demand(source("Success"), 10);
        let (run, pid, _) = run_config_ni(&home, &config, &input_libraries()[12], "200ms");
        run.success();
        let data = rows(&home);
        footer(&data, "complete");
        let actual = all_bits(&data);
        let o: Value = serde_json::from_str(include_str!("fixtures/nidaqmx/abi.json")).unwrap();
        let expected = o["patterns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_ne!(actual, [] as [String; 0]);
        assert_eq!(
            actual,
            expected
                .iter()
                .cycle()
                .take(actual.len())
                .cloned()
                .collect::<Vec<_>>()
        );
        let log = trace(&home);
        isolated(&log, pid);
        cleaned(&log);
        temperature_exports(&home, &data, false);
    }
    #[test]
    fn input_zero_value_is_not_zero_count_and_invalid_native_extents_never_publish_words() {
        for family in 0..3 {
            for (suffix, exit, retained) in [
                ("ZeroRead", 1, false),
                ("WarnZeroRead", 1, false),
                ("NegativeNoData", 4, false),
                ("CorruptCountNegative", 1, false),
                ("CorruptCountTooLarge", 1, false),
                ("Partial", 4, true),
                ("PartialStopFail", 4, true),
            ] {
                let home = Home::new("ni-input-extents");
                let config = input_config(family, suffix);
                let (run, pid, _) =
                    run_config_ni(&home, &config, &input_libraries()[family], "200ms");
                let error = run.failure(exit);
                let data = rows(&home);
                footer(&data, "failed");
                assert_eq!(input_blocks(&data, family).len(), usize::from(retained));
                if retained {
                    let count = if family == 0 { 3 } else { 1 };
                    assert_eq!(
                        input_bits(&data, family),
                        expected_input_bits(family)[..count]
                    );
                    assert!(has_code(&error, -200_284));
                    input_words(&data, family);
                }
                if exit == 1 {
                    assert_eq!(error["code"], "incomplete");
                    assert_eq!(error["details"]["requested_samples_per_channel"], 1);
                    assert_eq!(
                        error["details"]["actual_samples_per_channel"],
                        match suffix {
                            "CorruptCountNegative" => -1,
                            "CorruptCountTooLarge" => 2,
                            _ => 0,
                        }
                    );
                    assert_eq!(
                        error["details"]["ni_daqmx_status"],
                        if suffix == "WarnZeroRead" { 12_345 } else { 0 }
                    );
                }
                if suffix == "NegativeNoData" {
                    assert!(has_code(&error, -200_284));
                }
                if suffix == "WarnZeroRead" {
                    assert!(has_code(&error, 12_345) || data.iter().any(|r| has_code(r, 12_345)));
                }
                if suffix == "PartialStopFail" {
                    assert!(has_code(&error, -990_201));
                }
                let log = trace(&home);
                isolated(&log, pid);
                input_cleaned(&log, family);
                input_calls(&log, family);
                assert_eq!(calls(&log, input_read_name(family), "enter").len(), 1);
                temperature_exports(&home, &data, true);
            }
        }
    }
    #[test]
    fn input_channel_names_widths_and_terminal_failure_retire_setup_without_start_or_data() {
        for family in 0..3 {
            let suffixes = if family == 0 {
                vec![
                    "CountMismatch",
                    "Reverse",
                    "NameForeign",
                    "WidthZero",
                    "WidthHuge",
                    "WidthError",
                ]
            } else {
                vec!["CountMismatch", "NameForeign", "TermError"]
            };
            for suffix in suffixes {
                let home = Home::new("ni-input-identity");
                let mut config = input_config(family, suffix);
                if suffix == "TermError" {
                    config["channels"][0]["input_terminal"] = json!(format!(
                        "/{}{suffix}/PFI{}",
                        if family == 1 { "Count" } else { "Frequency" },
                        if family == 1 { 3 } else { 11 }
                    ));
                }
                let (run, pid, _) =
                    run_config_ni(&home, &config, &input_libraries()[family], "200ms");
                let error = run.failure(1);
                let data = rows(&home);
                footer(&data, "failed");
                assert_eq!(input_blocks(&data, family), [] as [&Value; 0]);
                let log = trace(&home);
                isolated(&log, pid);
                assert_eq!(calls(&log, "StartTask", "enter").len(), 0);
                assert_eq!(calls(&log, input_read_name(family), "enter").len(), 0);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
                assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
                if suffix == "TermError" {
                    assert!(has_code(&error, -990_401));
                } else if suffix == "WidthError" {
                    assert!(has_code(&error, -990_301));
                } else {
                    assert_eq!(error["code"], "incomplete");
                }
                temperature_exports(&home, &data, true);
            }
        }
    }
    #[test]
    fn input_deferred_diagnostic_hangs_preserve_native_words_and_original_status_with_bounded_cleanup()
     {
        for family in 0..3 {
            for suffix in ["Warn", "DiagHangPartial", "WarnDiagHang"] {
                let home = Home::new("ni-input-diagnostics");
                let config = input_config(family, suffix);
                let (run, pid, elapsed) =
                    run_config_ni(&home, &config, &input_libraries()[family], "250ms");
                let failed = suffix != "Warn";
                let error = if failed {
                    Some(run.failure(4))
                } else {
                    run.success();
                    None
                };
                assert!(elapsed < Duration::from_secs(3));
                let data = rows(&home);
                footer(&data, if failed { "failed" } else { "complete" });
                let actual = input_bits(&data, family);
                let expected = expected_input_bits(family);
                if suffix == "DiagHangPartial" {
                    assert_eq!(actual, expected[..if family == 0 { 3 } else { 1 }]);
                    assert!(has_code(error.as_ref().unwrap(), -200_284));
                } else {
                    assert_ne!(actual, [] as [String; 0]);
                    assert_eq!(
                        actual,
                        expected
                            .iter()
                            .cycle()
                            .take(actual.len())
                            .cloned()
                            .collect::<Vec<_>>()
                    );
                    assert!(data.iter().any(|r| has_code(r, 12_345)));
                }
                input_words(&data, family);
                for row in data.iter().filter(|r| r["kind"] == "sample") {
                    input_metadata(&row["value"], &config, family);
                }
                let log = trace(&home);
                isolated(&log, pid);
                input_calls(&log, family);
                if suffix == "DiagHangPartial" {
                    assert_eq!(calls(&log, input_read_name(family), "enter").len(), 1);
                    assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
                    assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
                } else {
                    input_cleaned(&log, family);
                }
                let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
                assert!(!support::cli::process_lives(worker));
                temperature_exports(&home, &data, failed);
            }
        }
    }
    fn engineering_oracle(counter: bool) -> Value {
        serde_json::from_str(if counter {
            include_str!("fixtures/nidaqmx/counter-time-position.json")
        } else {
            include_str!("fixtures/nidaqmx/engineering-ai.json")
        })
        .unwrap()
    }
    pub(super) fn engineering_libraries() -> &'static [Vec<u8>; 17] {
        static LIBS: OnceLock<[Vec<u8>; 17]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("engineering.c");
                let so = temp.path().join("engineering.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant != 12 {
                    cc.arg("-DNIDAQMX_FIXTURE_ENGINEERING_ONLY=1");
                }
                if variant <= 1 || (3..=6).contains(&variant) {
                    cc.arg("-DNIDAQMX_FIXTURE_ENGINEERING_AI_ONLY=1");
                }
                if variant != 0 && variant != 12 {
                    cc.arg("-DNIDAQMX_FIXTURE_ON_DEMAND=1");
                }
                if variant == 2 || (7..=11).contains(&variant) || variant >= 13 {
                    cc.arg("-DNIDAQMX_FIXTURE_ENGINEERING_CI_ONLY=1");
                }
                for flag in match variant {
                    3 => vec!["STRAIN"],
                    4 => vec!["BRIDGE"],
                    5 => vec!["ACCEL"],
                    6 => vec!["COUPLING"],
                    7 => vec!["PERIOD"],
                    8 => vec!["PULSE"],
                    9 => vec!["ANGULAR"],
                    10 => vec!["LINEAR"],
                    11 => vec!["ENCODER_Z"],
                    13 => vec!["PERIOD_TERMINAL"],
                    14 => vec!["PULSE_TERMINAL"],
                    15 => vec!["ENCODER_A"],
                    16 => vec!["ENCODER_B"],
                    12 => vec![
                        "STRAIN",
                        "BRIDGE",
                        "ACCEL",
                        "COUPLING",
                        "PERIOD",
                        "PULSE",
                        "ANGULAR",
                        "LINEAR",
                        "PERIOD_TERMINAL",
                        "PULSE_TERMINAL",
                        "ENCODER_A",
                        "ENCODER_B",
                        "ENCODER_Z",
                    ],
                    _ => Vec::new(),
                } {
                    cc.arg(format!("-DNIDAQMX_FIXTURE_OMIT_{flag}=1"));
                }
                let output = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "variant{variant}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn engineering_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|r| {
                r["kind"] == "sample"
                    && matches!(
                        r["value"]["event"].as_str(),
                        Some(
                            "strain_block"
                                | "bridge_ratio_block"
                                | "acceleration_block"
                                | "mixed_analog_block"
                                | "counter_period_block"
                                | "counter_pulse_width_block"
                                | "counter_angular_position_block"
                                | "counter_linear_position_block"
                        )
                    )
            })
            .collect()
    }
    fn engineering_bits(data: &[Value]) -> Vec<String> {
        engineering_blocks(data)
            .iter()
            .flat_map(|r| r["value"]["samples"].as_array().unwrap())
            .flat_map(|s| s.as_array().unwrap())
            .map(|w| w["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn engineering_expected(family: usize) -> Vec<String> {
        let oracle = engineering_oracle(family >= 4);
        let patterns = if family == 3 {
            &oracle["mixed_seven_channel_control"]["manual_scan_major_patterns"]
        } else if family >= 4 {
            &oracle["manual_native_patterns"]
        } else {
            &oracle["manual_homogeneous_patterns"]
        };
        patterns
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn engineering_metadata(value: &Value, config: &Value, family: usize) {
        assert_eq!(value["measurement"], config["measurement"]);
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        let requests = config["channels"].as_array().unwrap();
        let actual = value["channel_measurements"].as_array().unwrap();
        assert_eq!(actual.len(), requests.len());
        assert_eq!(
            value["channels"],
            Value::Array(requests.iter().map(|r| r["physical"].clone()).collect())
        );
        for (index, (item, request)) in actual.iter().zip(requests).enumerate() {
            assert_eq!(item["channel_index"], index);
            assert_eq!(item["physical"], request["physical"]);
            let is_new = family != 3 || index >= 4;
            if !is_new {
                assert_eq!(item["measurement"], request["measurement"]);
                assert_eq!(
                    item["quantity"],
                    ["voltage", "temperature", "current", "temperature"][index]
                );
                assert_eq!(item["units"], ["volts", "deg_c", "amps", "deg_f"][index]);
                for (key, wanted) in request.as_object().unwrap() {
                    if key == "terminal_config" {
                        assert_eq!(item["terminal_config_request"], *wanted);
                    } else {
                        assert_eq!(item[key], *wanted);
                    }
                }
                continue;
            }
            for (key, wanted) in request.as_object().unwrap() {
                if matches!(
                    key.as_str(),
                    "coupling"
                        | "terminal_config"
                        | "input_terminal"
                        | "a_input_terminal"
                        | "b_input_terminal"
                        | "z_input_terminal"
                ) {
                    continue;
                }
                assert_eq!(item[key], *wanted, "{key}: {item}");
            }
            if family >= 4 {
                assert_eq!(item["requested_physical"], request["physical"]);
            }
            if family != 3 {
                assert_eq!(
                    item["units"],
                    if family == 0 {
                        json!("strain")
                    } else if matches!(family, 4 | 5) {
                        json!("seconds")
                    } else {
                        request["units"].clone()
                    }
                );
            }
            let sources: &[&str] = if family >= 6 {
                &["a_input_terminal", "b_input_terminal", "z_input_terminal"]
            } else if family >= 4 {
                &["input_terminal"]
            } else {
                &["coupling"]
            };
            for name in sources {
                let key = format!("{name}_request");
                let provenance = format!("{name}_provenance");
                if let Some(wanted) = request.get(*name) {
                    assert_eq!(item[&key], *wanted);
                    assert_eq!(item[&provenance], "explicit_request_not_read_back");
                } else {
                    assert!(item.get(&key).is_none());
                    assert_eq!(
                        item[&provenance],
                        if *name == "z_input_terminal" && request["z_index"]["enabled"] == false {
                            "not_requested_z_disabled"
                        } else {
                            "driver_default_unqueried"
                        }
                    );
                }
            }
            if let Some(terminal) = request.get("terminal_config") {
                assert_eq!(item["terminal_config_request"], *terminal);
                assert!(item.get("terminal_config").is_none());
                assert_eq!(
                    item["terminal_config_provenance"],
                    if terminal == "default" {
                        "driver_default_unqueried"
                    } else {
                        "explicit_request_not_read_back"
                    }
                );
            }
            if family >= 6 {
                assert_eq!(
                    item["z_index_phase_applicability"],
                    if request["z_index"]["enabled"] == false {
                        "z_index_disabled"
                    } else if request["decoding"] == "two_pulse_counting" {
                        "ignored_for_two_pulse_counting"
                    } else {
                        "quadrature_phase_request"
                    }
                );
            }
            if family == 3 {
                assert_eq!(item["measurement"], request["measurement"]);
                assert_eq!(
                    item["quantity"],
                    ["strain", "bridge_ratio", "acceleration"][index - 4]
                );
                assert_eq!(
                    item["units"],
                    if index == 4 {
                        json!("strain")
                    } else {
                        request["units"].clone()
                    }
                );
            }
            for key in [
                "calibration_readback",
                "power_off",
                "excitation_readback",
                "source_time",
            ] {
                assert!(item.get(key).is_none());
            }
        }
        if config["timing"] == "on_demand" {
            software_metadata(value, 10);
        }
    }
    fn engineering_words(data: &[Value], config: &Value, family: usize) {
        let expected = engineering_expected(family);
        let actual = engineering_bits(data);
        assert_ne!(actual, [] as [String; 0]);
        assert_eq!(
            actual,
            expected
                .iter()
                .cycle()
                .take(actual.len())
                .cloned()
                .collect::<Vec<_>>()
        );
        let event = [
            "strain_block",
            "bridge_ratio_block",
            "acceleration_block",
            "mixed_analog_block",
            "counter_period_block",
            "counter_pulse_width_block",
            "counter_angular_position_block",
            "counter_linear_position_block",
        ][family];
        let representation = [
            "driver_scaled_f64_strain",
            "driver_scaled_f64_bridge_ratio",
            "driver_scaled_f64_acceleration",
            "driver_scaled_f64_mixed_analog",
            "driver_scaled_f64_counter_period",
            "driver_scaled_f64_counter_pulse_width",
            "driver_scaled_f64_counter_angular_position",
            "driver_scaled_f64_counter_linear_position",
        ][family];
        let channels = config["channels"].as_array().unwrap().len();
        let mut scans = 0;
        for row in engineering_blocks(data) {
            let value = &row["value"];
            assert_eq!(value["event"], event);
            assert_eq!(value["representation"], representation);
            assert_eq!(value["sample_index"], scans);
            assert!(row.get("source_time").is_none());
            let samples = value["samples"].as_array().unwrap();
            assert_eq!(value["samples_per_channel"], samples.len());
            for scan in samples {
                assert_eq!(scan.as_array().unwrap().len(), channels);
                for (index, word) in scan.as_array().unwrap().iter().enumerate() {
                    let key = if family == 3 {
                        [
                            "volts",
                            "temperature",
                            "amperes",
                            "temperature",
                            "strain",
                            "ratio",
                            "acceleration",
                        ][index]
                    } else {
                        [
                            "strain",
                            "ratio",
                            "acceleration",
                            "",
                            "period_seconds",
                            "pulse_width_seconds",
                            "angle",
                            "position",
                        ][family]
                    };
                    let bytes = word["ieee754_le_hex"].as_str().unwrap();
                    let mut raw = [0u8; 8];
                    for (i, pair) in bytes.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                        raw[i] =
                            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
                    }
                    let number = f64::from_bits(u64::from_le_bytes(raw));
                    if number.is_finite() {
                        assert_eq!(word[key].as_f64().unwrap().to_bits(), number.to_bits());
                    } else {
                        assert!(word.get(key).is_none());
                    }
                    for other in [
                        "volts",
                        "amperes",
                        "temperature",
                        "strain",
                        "ratio",
                        "acceleration",
                        "period_seconds",
                        "pulse_width_seconds",
                        "angle",
                        "position",
                    ] {
                        if other != key {
                            assert!(word.get(other).is_none());
                        }
                    }
                    for other in ["measurement", "units", "kind", "source_time"] {
                        assert!(word.get(other).is_none());
                    }
                }
            }
            scans += samples.len();
            engineering_metadata(value, config, family);
        }
    }
    fn engineering_cleaned(log: &[Value], read: &str) {
        assert_eq!(calls(log, "StopTask", "enter").len(), 1);
        assert_eq!(calls(log, "ClearTask", "enter").len(), 1);
        let returned = calls(log, read, "return");
        assert!(
            calls(log, "StopTask", "enter")[0]["seq"].as_u64().unwrap()
                > returned.last().unwrap()["seq"].as_u64().unwrap()
        );
    }
    fn engineering_arguments(log: &[Value], family: usize) {
        let oracle = engineering_oracle(false);
        let words = oracle["manual_argument_words"].as_array().unwrap();
        let groups = if family == 3 {
            vec![
                (0, "strain_pair"),
                (1, "bridge_ratio_pair"),
                (2, "acceleration_three_channel_cycle"),
            ]
        } else {
            vec![(
                family,
                [
                    "strain_pair",
                    "bridge_ratio_pair",
                    "acceleration_three_channel_cycle",
                ][family],
            )]
        };
        for (kind, group) in groups {
            let function = [
                "CreateAIStrainGageChan",
                "CreateAIBridgeChan",
                "CreateAIAccelChan",
            ][kind];
            let actual = calls(log, function, "arguments");
            let controls = oracle[group].as_array().unwrap();
            assert_eq!(actual.len(), if family == 3 { 1 } else { controls.len() });
            for (call, control) in actual.iter().zip(controls) {
                assert_eq!(call["units"], control["units_code"]);
                assert_eq!(call["excitation_source"], control["excitation_code"]);
                assert_eq!(call["custom_scale_null"], true);
                assert_eq!(call["virtual_empty"], true);
                let mut mappings = vec![
                    ("minimum_bits", "min_word"),
                    ("maximum_bits", "max_word"),
                    (
                        "excitation_bits",
                        if kind == 2 {
                            "amperes_word"
                        } else {
                            "volts_word"
                        },
                    ),
                ];
                if kind == 0 {
                    mappings.extend([
                        ("gage_bits", "gage_factor_word"),
                        ("initial_bits", "initial_bridge_volts_word"),
                        ("ohms_bits", "nominal_gage_ohms_word"),
                        ("poisson_bits", "poisson_ratio_word"),
                        ("lead_bits", "lead_wire_ohms_word"),
                    ]);
                    assert_eq!(call["bridge"], control["bridge_type_code"]);
                }
                if kind == 1 {
                    mappings.push(("ohms_bits", "nominal_bridge_ohms_word"));
                    assert_eq!(call["bridge"], control["bridge_config_code"]);
                }
                if kind == 2 {
                    mappings.push(("sensitivity_bits", "sensitivity_word"));
                    assert_eq!(call["terminal"], control["terminal_code"]);
                    assert_eq!(call["sensitivity_units"], control["sensitivity_units_code"]);
                }
                for (field, reference) in mappings {
                    assert_eq!(
                        call[field],
                        words
                            .iter()
                            .find(|w| w["name"] == control[reference])
                            .unwrap()["bits_hex"]
                    );
                }
            }
        }
    }
    #[test]
    fn engineering_ai_both_timings_preserve_three_families_and_mixed_seven_science() {
        for family in 0..4 {
            for software in [false, true] {
                let home = Home::new("ni-engineering-ai");
                let device = [
                    "StrainSuccess",
                    "RatioSuccess",
                    "AccelSuccess",
                    "EngineeringMixedSuccess",
                ][family];
                let config = engineering_source(family, device);
                let config = if software {
                    on_demand(config, 10)
                } else {
                    config
                };
                let library = if family == 3 {
                    if software {
                        &on_demand_libraries()[0]
                    } else {
                        &libraries()[0]
                    }
                } else {
                    &engineering_libraries()[usize::from(software)]
                };
                let (run, pid, _) = run_config_ni(&home, &config, library, "250ms");
                run.success();
                let data = rows(&home);
                footer(&data, "complete");
                engineering_words(&data, &config, family);
                let actual = engineering_bits(&data);
                if software {
                    assert!(actual.len() >= config["channels"].as_array().unwrap().len() * 2);
                } else {
                    assert_eq!(actual.len(), if family == 3 { 14 } else { 12 });
                }
                let log = trace(&home);
                isolated(&log, pid);
                engineering_cleaned(&log, "ReadAnalogF64");
                engineering_arguments(&log, family);
                assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
                assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
                assert_eq!(calls(&log, "SetAITermCfg", "enter").len(), 0);
                assert_eq!(
                    calls(&log, "SetAICoupling", "enter").len(),
                    [1, 2, 3, 3][family]
                );
                for call in calls(&log, "ReadAnalogF64", "arguments") {
                    assert_eq!(call["fill_mode"], 1);
                    assert_eq!(call["reserved_null"], true);
                    if software {
                        assert_eq!(call["requested"], 1);
                        assert_eq!(
                            call["capacity"],
                            config["channels"].as_array().unwrap().len()
                        );
                    }
                }
                if software {
                    for name in [
                        "CfgSampClkTiming",
                        "CfgInputBuffer",
                        "SetReadOverWrite",
                        "GetReadAvailSampPerChan",
                        "GetSampClkRate",
                    ] {
                        assert_eq!(calls(&log, name, "enter").len(), 0);
                    }
                }
                temperature_exports(&home, &data, false);
            }
        }
    }
    #[test]
    fn engineering_layouts_coupling_and_none_excitation_are_literal_requests() {
        let oracle = engineering_oracle(false);
        for (family, suffix) in [
            (0, "All"),
            (1, "All"),
            (0, "None"),
            (1, "None"),
            (2, "None"),
            (0, "NegativeZero"),
            (2, "Omitted"),
        ] {
            let prefix = ["Strain", "Ratio", "Accel"][family];
            let device = format!("{prefix}{suffix}");
            let mut config = engineering_source(family, &device);
            if suffix == "All" {
                let mut channels = Vec::new();
                let count = if family == 0 { 7 } else { 3 };
                for i in 0..count {
                    let mut channel = config["channels"][i % 2].clone();
                    channel["physical"] = json!(format!("{device}/ai{i}"));
                    channel[if family == 0 {
                        "bridge_type"
                    } else {
                        "bridge_config"
                    }] = oracle[if family == 0 {
                        "strain_layout_controls"
                    } else {
                        "bridge_configuration_controls"
                    }][i][if family == 0 {
                        "bridge_type"
                    } else {
                        "bridge_config"
                    }]
                    .clone();
                    channels.push(channel);
                }
                config["channels"] = json!(channels);
            }
            if matches!(suffix, "None" | "NegativeZero") {
                for channel in config["channels"].as_array_mut().unwrap() {
                    channel["excitation"]["source"] = json!("none");
                    channel["excitation"][if family == 2 { "amperes" } else { "volts" }] =
                        json!(if suffix == "NegativeZero" {
                            -0.0
                        } else if family == 0 {
                            1.25
                        } else if family == 1 {
                            0.0
                        } else {
                            1.0 / 1024.0
                        });
                }
            }
            if suffix == "Omitted" {
                for channel in config["channels"].as_array_mut().unwrap() {
                    channel.as_object_mut().unwrap().remove("coupling");
                }
            }
            let home = Home::new("ni-engineering-controls");
            let config = on_demand(config, 10);
            let (run, pid, _) = run_config_ni(
                &home,
                &config,
                &engineering_libraries()[if suffix == "Omitted" { 6 } else { 1 }],
                "180ms",
            );
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            engineering_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            let function = [
                "CreateAIStrainGageChan",
                "CreateAIBridgeChan",
                "CreateAIAccelChan",
            ][family];
            let arguments = calls(&log, function, "arguments");
            if suffix == "All" {
                for (i, arg) in arguments.iter().enumerate() {
                    assert_eq!(
                        arg["bridge"],
                        oracle[if family == 0 {
                            "strain_layout_controls"
                        } else {
                            "bridge_configuration_controls"
                        }][i]["code"]
                    );
                }
            }
            if matches!(suffix, "None" | "NegativeZero") {
                for arg in arguments {
                    assert_eq!(arg["excitation_source"], 10_230);
                    let wanted = if suffix == "NegativeZero" {
                        "8000000000000000"
                    } else if family == 0 {
                        "3ff4000000000000"
                    } else if family == 1 {
                        "0000000000000000"
                    } else {
                        "3f50000000000000"
                    };
                    assert_eq!(arg["excitation_bits"], wanted);
                }
            }
            if suffix == "Omitted" {
                assert_eq!(calls(&log, "SetAICoupling", "enter").len(), 0);
            }
            temperature_exports(&home, &data, false);
        }
    }
    #[test]
    fn engineering_selected_exports_and_highest_encoder_source_are_resolved_before_create() {
        for (family, variant) in [
            (0, 3),
            (1, 4),
            (2, 5),
            (0, 6),
            (4, 7),
            (5, 8),
            (6, 9),
            (7, 10),
            (6, 11),
            (4, 13),
            (5, 14),
            (6, 15),
            (6, 16),
        ] {
            let home = Home::new("ni-engineering-missing");
            let device = [
                "StrainSuccess",
                "RatioSuccess",
                "AccelSuccess",
                "EngineeringMixedSuccess",
                "PeriodSuccess",
                "PulseSuccess",
                "AngleSuccess",
                "LinearSuccess",
            ][family];
            let mut config = on_demand(engineering_source(family, device), 10);
            if matches!(variant, 13 | 14) {
                let pfi = if family == 4 { 5 } else { 7 };
                config["channels"][0]["input_terminal"] = json!(format!("/{device}/PFI{pfi}"));
            }
            let (run, pid, _) =
                run_config_ni(&home, &config, &engineering_libraries()[variant], "150ms");
            assert_eq!(run.failure(3)["code"], "unavailable");
            let log = trace(&home);
            isolated(&log, pid);
            for name in [
                "CreateTask",
                "StartTask",
                "StopTask",
                "ClearTask",
                "ReadAnalogF64",
                "ReadCounterF64",
            ] {
                assert_eq!(calls(&log, name, "enter").len(), 0);
            }
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(engineering_blocks(&data), [] as [&Value; 0]);
        }
        // Missing unselected new constructors must not prevent another new family.
        for (family, variant) in [(1, 3), (2, 4), (0, 5), (7, 9), (4, 10)] {
            let home = Home::new("ni-engineering-unused");
            let device = [
                "StrainSuccess",
                "RatioSuccess",
                "AccelSuccess",
                "EngineeringMixedSuccess",
                "PeriodSuccess",
                "PulseSuccess",
                "AngleSuccess",
                "LinearSuccess",
            ][family];
            let config = on_demand(engineering_source(family, device), 10);
            let (run, pid, _) =
                run_config_ni(&home, &config, &engineering_libraries()[variant], "150ms");
            run.success();
            let data = rows(&home);
            engineering_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(
                &log,
                if family < 4 {
                    "ReadAnalogF64"
                } else {
                    "ReadCounterF64"
                },
            );
        }
        let home = Home::new("ni-encoder-unused-z");
        let mut config = engineering_source(6, "AngleOmitted");
        for key in ["a_input_terminal", "b_input_terminal", "z_input_terminal"] {
            config["channels"][0].as_object_mut().unwrap().remove(key);
        }
        let (run, pid, _) = run_config_ni(&home, &config, &engineering_libraries()[11], "150ms");
        run.success();
        let log = trace(&home);
        isolated(&log, pid);
        engineering_cleaned(&log, "ReadCounterF64");
        assert_eq!(calls(&log, "SetCIEncoderZInputTerm", "enter").len(), 0);
        for (family, variant, omitted) in [
            (4, 13, "input_terminal"),
            (5, 14, "input_terminal"),
            (6, 15, "a_input_terminal"),
            (6, 16, "b_input_terminal"),
        ] {
            let home = Home::new("ni-engineering-omitted-source");
            let device = if family == 4 {
                "PeriodSuccess"
            } else if family == 5 {
                "PulseSuccess"
            } else {
                "AngleSuccess"
            };
            let mut config = engineering_source(family, device);
            config["channels"][0]
                .as_object_mut()
                .unwrap()
                .remove(omitted);
            let (run, pid, _) =
                run_config_ni(&home, &config, &engineering_libraries()[variant], "150ms");
            run.success();
            let data = rows(&home);
            engineering_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadCounterF64");
            assert_eq!(
                calls(
                    &log,
                    [
                        "SetCIPeriodTerm",
                        "SetCIPulseWidthTerm",
                        "SetCIEncoderAInputTerm",
                        "SetCIEncoderBInputTerm"
                    ][variant - 13],
                    "enter"
                )
                .len(),
                0
            );
        }
        // Old homogeneous and four-column MixedAI still work with every new constructor absent.
        for config in [
            source("Success"),
            thermocouple_source("Success"),
            current_source("Success"),
            rtd_source("RtdSuccess"),
            mixed_source("MixedSuccess"),
        ] {
            let home = Home::new("ni-engineering-old");
            let (run, pid, _) =
                run_config_ni(&home, &config, &engineering_libraries()[12], "150ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let log = trace(&home);
            isolated(&log, pid);
            cleaned(&log);
            temperature_exports(&home, &data, false);
        }
    }
    #[test]
    fn engineering_counter_period_pulse_and_encoder_units_preserve_all_native_words() {
        for (family, suffix) in [
            (4, "Success"),
            (4, "Low"),
            (4, "Large"),
            (4, "Explicit"),
            (5, "Success"),
            (5, "Explicit"),
            (5, "Falling"),
            (6, "Success"),
            (6, "Radians"),
            (7, "Success"),
            (7, "Inches"),
            (6, "X1"),
            (6, "X2"),
            (6, "X4"),
            (6, "TwoPulse"),
            (6, "Disabled"),
            (7, "Explicit"),
        ] {
            let prefix = ["Period", "Pulse", "Angle", "Linear"][family - 4];
            let device = format!("{prefix}{suffix}");
            let mut config = engineering_source(family, &device);
            let channel = &mut config["channels"][0];
            match suffix {
                "Low" => {
                    channel["method"] = json!("low_frequency_one_counter");
                    channel["measurement_time_seconds"] = json!(0.000_001);
                }
                "Large" => {
                    channel["method"] = json!("large_range_two_counter");
                    channel["divisor"] = json!(7);
                }
                "Falling" => channel["starting_edge"] = json!("falling"),
                "Radians" => channel["units"] = json!("radians"),
                "Inches" => {
                    channel["units"] = json!("inches");
                    channel["distance_per_pulse"] = json!(3.0 / 2048.0);
                    channel["initial_position"] = json!(-0.875);
                }
                "X1" | "X2" | "X4" | "TwoPulse" => {
                    let i = ["X1", "X2", "X4", "TwoPulse"]
                        .iter()
                        .position(|s| *s == suffix)
                        .unwrap();
                    channel["decoding"] = json!(["x1", "x2", "x4", "two_pulse_counting"][i]);
                    channel["z_index"]["phase"] = json!(
                        [
                            "a_high_b_high",
                            "a_high_b_low",
                            "a_low_b_high",
                            "a_low_b_low"
                        ][i]
                    );
                }
                "Disabled" => {
                    channel["z_index"] =
                        json!({"enabled":false,"value":0.375,"phase":"a_low_b_low"});
                    channel.as_object_mut().unwrap().remove("z_input_terminal");
                }
                "Explicit" => {
                    if family < 6 {
                        let pfi = if family == 4 { 5 } else { 7 };
                        channel["input_terminal"] = json!(format!("/{device}/PFI{pfi}"));
                    } else {
                        for (key, pfi) in [
                            ("a_input_terminal", 1),
                            ("b_input_terminal", 3),
                            ("z_input_terminal", 9),
                        ] {
                            channel[key] = json!(format!("/{device}/PFI{pfi}"));
                        }
                    }
                }
                _ => {}
            }
            let home = Home::new("ni-engineering-counter");
            let (run, pid, _) = run_config_ni(&home, &config, &engineering_libraries()[2], "220ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            engineering_words(&data, &config, family);
            assert!(engineering_bits(&data).len() >= 8);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadCounterF64");
            assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
            for name in [
                "ReadAnalogF64",
                "ReadCounterU32",
                "ReadDigitalU32",
                "CfgSampClkTiming",
                "CfgInputBuffer",
                "SetReadOverWrite",
                "GetReadAvailSampPerChan",
                "GetSampClkRate",
            ] {
                assert_eq!(calls(&log, name, "enter").len(), 0);
            }
            for read in calls(&log, "ReadCounterF64", "arguments") {
                assert_eq!(read["requested"], 1);
                assert_eq!(read["capacity"], 1);
                assert_eq!(read["reserved_null"], true);
                assert!(read.get("fill_mode").is_none());
            }
            let function = [
                "CreateCIPeriodChan",
                "CreateCIPulseWidthChan",
                "CreateCIAngEncoderChan",
                "CreateCILinEncoderChan",
            ][family - 4];
            let args = calls(&log, function, "arguments");
            assert_eq!(args.len(), 1);
            assert_eq!(args[0]["custom_scale_null"], true);
            assert_eq!(args[0]["virtual_empty"], true);
            for (field, function) in [
                (
                    "input_terminal",
                    if family == 4 {
                        "SetCIPeriodTerm"
                    } else {
                        "SetCIPulseWidthTerm"
                    },
                ),
                ("a_input_terminal", "SetCIEncoderAInputTerm"),
                ("b_input_terminal", "SetCIEncoderBInputTerm"),
                ("z_input_terminal", "SetCIEncoderZInputTerm"),
            ] {
                let wanted = config["channels"][0].get(field);
                let actual = calls(&log, function, "arguments");
                assert_eq!(actual.len(), usize::from(wanted.is_some()));
                if let Some(wanted) = wanted {
                    assert_eq!(actual[0]["terminal"], *wanted);
                    assert_eq!(actual[0]["channel"], config["channels"][0]["physical"]);
                }
            }
            if suffix == "Low" {
                assert_eq!(args[0]["measurement_time_bits"], "3eb0c6f7a0b5ed8d");
                assert_eq!(args[0]["divisor_u32"], 4);
            }
            if family == 6 {
                assert_eq!(args[0]["pulses_per_rev_u32"], 4099);
                assert_eq!(args[0]["initial_bits"], "c004000000000000");
            }
            temperature_exports(&home, &data, false);
        }
    }
    #[test]
    fn engineering_negative_prefix_status_and_deferred_diagnostic_hangs_retain_native_science() {
        let cases = [
            (0, "Partial"),
            (1, "Partial"),
            (2, "Partial"),
            (3, "Partial"),
            (4, "Partial"),
            (5, "Partial"),
            (6, "Partial"),
            (7, "Partial"),
            (0, "PartialStopFail"),
            (6, "PartialStopFail"),
            (1, "DiagHangPartial"),
            (7, "DiagHangPartial"),
            (2, "WarnDiagHang"),
            (4, "Warn"),
        ];
        for (family, suffix) in cases {
            let prefix = [
                "Strain",
                "Ratio",
                "Accel",
                "EngineeringMixed",
                "Period",
                "Pulse",
                "Angle",
                "Linear",
            ][family];
            let device = format!("{prefix}{suffix}");
            let config = engineering_source(family, &device);
            let home = Home::new("ni-engineering-prefix");
            let library = if family >= 4 {
                &engineering_libraries()[2]
            } else if family == 3 {
                &libraries()[0]
            } else {
                &engineering_libraries()[0]
            };
            let (run, pid, elapsed) = run_config_ni(&home, &config, library, "250ms");
            let failed = suffix != "Warn";
            let error = if failed {
                Some(run.failure(4))
            } else {
                run.success();
                None
            };
            assert!(elapsed < Duration::from_secs(3), "{device}: {elapsed:?}");
            let data = rows(&home);
            footer(&data, if failed { "failed" } else { "complete" });
            engineering_words(&data, &config, family);
            let channels = config["channels"].as_array().unwrap().len();
            if suffix.starts_with("Partial") || suffix == "DiagHangPartial" {
                assert_eq!(engineering_bits(&data).len(), channels);
                assert!(has_code(error.as_ref().unwrap(), -200_284));
            }
            if suffix == "PartialStopFail" {
                assert!(has_code(error.as_ref().unwrap(), -990_201));
            }
            if suffix.starts_with("Warn") {
                assert!(data.iter().any(|r| has_code(r, 12_345)));
            }
            let log = trace(&home);
            isolated(&log, pid);
            let read = if family >= 4 {
                "ReadCounterF64"
            } else {
                "ReadAnalogF64"
            };
            if suffix == "DiagHangPartial" {
                assert_eq!(calls(&log, read, "enter").len(), 1);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
            } else {
                engineering_cleaned(&log, read);
            }
            if suffix.ends_with("DiagHang") || suffix == "DiagHangPartial" {
                let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
                assert!(!support::cli::process_lives(worker));
            }
            temperature_exports(&home, &data, failed);
        }
    }
    #[test]
    fn engineering_geometry_and_later_constructor_failure_never_fabricate_samples() {
        for (family, suffix) in [
            (0, "Reverse"),
            (1, "CorruptCountNegative"),
            (4, "ZeroRead"),
            (5, "WarnZeroRead"),
            (6, "CorruptCountTooLarge"),
            (7, "NameForeign"),
            (3, "LaterFail"),
            (0, "CouplingError"),
            (4, "TermError"),
            (6, "TermError"),
        ] {
            let device = format!(
                "{}{suffix}",
                [
                    "Strain",
                    "Ratio",
                    "Accel",
                    "EngineeringMixed",
                    "Period",
                    "Pulse",
                    "Angle",
                    "Linear"
                ][family]
            );
            let mut config = engineering_source(family, &device);
            if family == 4 && suffix == "TermError" {
                config["channels"][0]["input_terminal"] = json!(format!("/{device}/PFI5"));
            }
            let home = Home::new("ni-engineering-geometry");
            let library = if family >= 4 {
                &engineering_libraries()[2]
            } else if family == 3 {
                &libraries()[0]
            } else {
                &engineering_libraries()[0]
            };
            let (run, pid, _) = run_config_ni(&home, &config, library, "250ms");
            let error = run.failure(1);
            assert_eq!(
                error["code"],
                if matches!(suffix, "LaterFail" | "CouplingError" | "TermError") {
                    "failed"
                } else {
                    "incomplete"
                }
            );
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(engineering_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            if matches!(suffix, "LaterFail" | "CouplingError" | "TermError") {
                assert!(has_code(
                    &error,
                    if suffix == "LaterFail" {
                        -990_104
                    } else if suffix == "CouplingError" {
                        -990_402
                    } else {
                        -990_401
                    }
                ));
                assert_eq!(calls(&log, "StartTask", "enter").len(), 0);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
            }
            if matches!(suffix, "ZeroRead" | "WarnZeroRead") {
                assert_eq!(error["details"]["actual_samples_per_channel"], 0);
                assert!(has_code(
                    &error,
                    if suffix == "WarnZeroRead" { 12_345 } else { 0 }
                ));
            }
            temperature_exports(&home, &data, true);
        }
    }
    fn resistive_oracle(bridge: bool) -> Value {
        serde_json::from_str(if bridge {
            include_str!("fixtures/nidaqmx/physical-bridge.json")
        } else {
            include_str!("fixtures/nidaqmx/resistive-ai.json")
        })
        .unwrap()
    }
    pub(super) fn resistive_libraries() -> &'static [Vec<u8>; 10] {
        static LIBS: OnceLock<[Vec<u8>; 10]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("resistive.c");
                let so = temp.path().join("resistive.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant <= 1 || variant == 9 {
                    cc.arg("-DNIDAQMX_FIXTURE_RESISTIVE_ONLY=1");
                }
                if variant == 2 {
                    cc.arg("-DNIDAQMX_FIXTURE_LEGACY_ONLY=1");
                }
                if variant == 1 || variant >= 3 {
                    cc.arg("-DNIDAQMX_FIXTURE_ON_DEMAND=1");
                }
                if variant >= 3 {
                    cc.arg(format!(
                        "-DNIDAQMX_FIXTURE_OMIT_{}=1",
                        [
                            "RESISTANCE",
                            "THERM_IEX",
                            "THERM_VEX",
                            "FORCE",
                            "PRESSURE",
                            "TORQUE",
                            "COUPLING"
                        ][variant - 3]
                    ));
                }
                let output = cc
                    .arg(&c)
                    .arg("-o")
                    .arg(&so)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "variant{variant}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn resistive_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|r| {
                r["kind"] == "sample"
                    && matches!(
                        r["value"]["event"].as_str(),
                        Some(
                            "resistance_block"
                                | "thermistor_current_block"
                                | "thermistor_voltage_block"
                                | "force_bridge_block"
                                | "pressure_bridge_block"
                                | "torque_bridge_block"
                                | "mixed_analog_block"
                        )
                    )
            })
            .collect()
    }
    fn resistive_bits(data: &[Value]) -> Vec<String> {
        resistive_blocks(data)
            .iter()
            .flat_map(|r| r["value"]["samples"].as_array().unwrap())
            .flat_map(|s| s.as_array().unwrap())
            .map(|w| w["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn resistive_expected(family: usize) -> Vec<String> {
        let oracle = resistive_oracle((3..6).contains(&family));
        if family == 6 {
            oracle["mixed_thirteen_channel_control"]["all_words_le_hex"]
                .as_str()
                .unwrap()
                .as_bytes()
                .as_chunks::<16>()
                .0
                .iter()
                .map(|word| std::str::from_utf8(word).unwrap().to_owned())
                .collect()
        } else {
            oracle["manual_native_patterns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w["ieee754_le_hex"].as_str().unwrap().to_owned())
                .collect()
        }
    }
    fn exact_request(actual: &Value, request: &Value) {
        match request {
            Value::Number(_) => assert_eq!(
                actual.as_f64().unwrap().to_bits(),
                request.as_f64().unwrap().to_bits()
            ),
            Value::Object(fields) => {
                assert_eq!(actual.as_object().unwrap().len(), fields.len());
                for (key, value) in fields {
                    exact_request(&actual[key], value);
                }
            }
            _ => assert_eq!(actual, request),
        }
    }
    fn resistive_metadata(value: &Value, config: &Value, family: usize) {
        assert_eq!(value["measurement"], config["measurement"]);
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        let requests = config["channels"].as_array().unwrap();
        let actual = value["channel_measurements"].as_array().unwrap();
        assert_eq!(actual.len(), requests.len());
        assert_eq!(
            value["channels"],
            Value::Array(requests.iter().map(|c| c["physical"].clone()).collect())
        );
        if family == 6 {
            let mut previous = value.clone();
            let mut oldconfig = config.clone();
            previous["channel_measurements"] = json!(&actual[..7]);
            previous["channels"] = json!(&value["channels"].as_array().unwrap()[..7]);
            oldconfig["channels"] = json!(&requests[..7]);
            engineering_metadata(&previous, &oldconfig, 3);
            assert!(value.get("units").is_none());
        }
        for (index, (item, request)) in actual.iter().zip(requests).enumerate() {
            if family == 6 && index < 7 {
                continue;
            }
            assert_eq!(item["channel_index"], index);
            let kind = if family == 6 { index - 7 } else { family };
            for (key, wanted) in request.as_object().unwrap() {
                if key != "coupling" {
                    exact_request(&item[key], wanted);
                }
            }
            let units = if kind == 0 {
                json!("ohms")
            } else {
                request["units"].clone()
            };
            assert_eq!(item["units"], units);
            if let Some(c) = request.get("coupling") {
                assert_eq!(item["coupling_request"], *c);
                assert_eq!(
                    item["coupling_provenance"],
                    "explicit_request_not_read_back"
                );
            } else {
                assert!(item.get("coupling_request").is_none());
                assert_eq!(item["coupling_provenance"], "driver_default_unqueried");
            }
            assert!(item.get("coupling").is_none());
            if kind != 2 {
                assert!(item.get("r1_ohms").is_none());
            }
            if kind == 0 || kind >= 3 {
                assert!(item.get("coefficients").is_none());
            }
            if kind < 3 {
                assert!(item.get("calibration").is_none());
            }
            for key in [
                "terminal_config",
                "terminal_config_request",
                "terminal_config_provenance",
                "cjc",
                "rtd_type",
                "r0_ohms",
                "observed_calibration",
                "excitation_readback",
                "source_time",
            ] {
                assert!(item.get(key).is_none(), "{key}: {item}");
            }
            if kind >= 3 {
                assert_eq!(
                    item["calibration_provenance"],
                    "declared_two_point_linear_request"
                );
            } else {
                assert!(item.get("calibration_provenance").is_none());
            }
            if family == 6 {
                assert_eq!(item["measurement"], request["measurement"]);
                assert_eq!(
                    item["quantity"],
                    [
                        "resistance",
                        "temperature",
                        "temperature",
                        "force",
                        "pressure",
                        "torque"
                    ][kind]
                );
            }
        }
        if config["timing"] == "on_demand" {
            software_metadata(value, 10);
        }
    }
    fn resistive_words(data: &[Value], config: &Value, family: usize) {
        let expected = resistive_expected(family);
        let actual = resistive_bits(data);
        assert_ne!(actual, [] as [String; 0]);
        assert_eq!(
            actual,
            expected
                .iter()
                .cycle()
                .take(actual.len())
                .cloned()
                .collect::<Vec<_>>()
        );
        let event = [
            "resistance_block",
            "thermistor_current_block",
            "thermistor_voltage_block",
            "force_bridge_block",
            "pressure_bridge_block",
            "torque_bridge_block",
            "mixed_analog_block",
        ][family];
        let representation = [
            "driver_scaled_f64_resistance",
            "driver_scaled_f64_thermistor_current",
            "driver_scaled_f64_thermistor_voltage",
            "driver_scaled_f64_force_bridge",
            "driver_scaled_f64_pressure_bridge",
            "driver_scaled_f64_torque_bridge",
            "driver_scaled_f64_mixed_analog",
        ][family];
        let count = config["channels"].as_array().unwrap().len();
        let mut index = 0;
        for row in resistive_blocks(data) {
            let value = &row["value"];
            assert_eq!(value["event"], event);
            assert_eq!(value["representation"], representation);
            assert_eq!(value["sample_index"], index);
            assert!(row.get("source_time").is_none());
            let scans = value["samples"].as_array().unwrap();
            assert_eq!(value["samples_per_channel"], scans.len());
            for scan in scans {
                let words = scan.as_array().unwrap();
                assert_eq!(words.len(), count);
                for (column, word) in words.iter().enumerate() {
                    let key = if family == 6 {
                        [
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
                        ][column]
                    } else {
                        [
                            "ohms",
                            "temperature",
                            "temperature",
                            "force",
                            "pressure",
                            "torque",
                        ][family]
                    };
                    let hex = word["ieee754_le_hex"].as_str().unwrap();
                    let mut bytes = [0u8; 8];
                    for (i, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                        bytes[i] =
                            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
                    }
                    let number = f64::from_bits(u64::from_le_bytes(bytes));
                    if number.is_finite() {
                        assert_eq!(word[key].as_f64().unwrap().to_bits(), number.to_bits());
                    } else {
                        assert!(word.get(key).is_none());
                    }
                    for other in [
                        "volts",
                        "temperature",
                        "amperes",
                        "strain",
                        "ratio",
                        "acceleration",
                        "ohms",
                        "force",
                        "pressure",
                        "torque",
                    ] {
                        if other != key {
                            assert!(word.get(other).is_none());
                        }
                    }
                    for name in ["units", "measurement", "kind", "source_time"] {
                        assert!(word.get(name).is_none());
                    }
                }
            }
            index += scans.len();
            resistive_metadata(value, config, family);
        }
    }
    fn resistive_arguments(log: &[Value], family: usize, mixed: bool) {
        let kind = family;
        let oracle = resistive_oracle(kind >= 3);
        let key = [
            "resistance",
            "thermistor_current",
            "thermistor_voltage",
            "force_bridge",
            "pressure_bridge",
            "torque_bridge",
        ][kind];
        let controls = oracle["ordered_constructor_pairs"][key].as_array().unwrap();
        let function = [
            "CreateAIResistanceChan",
            "CreateAIThrmstrChanIex",
            "CreateAIThrmstrChanVex",
            "CreateAIForceBridgeTwoPointLinChan",
            "CreateAIPressureBridgeTwoPointLinChan",
            "CreateAITorqueBridgeTwoPointLinChan",
        ][kind];
        let calls = calls(log, function, "arguments");
        assert_eq!(calls.len(), if mixed { 1 } else { 2 });
        let words = oracle["manual_argument_words"].as_array().unwrap();
        let bits =
            |name: &Value| words.iter().find(|w| w["name"] == *name).unwrap()["bits_hex"].clone();
        for (actual, wanted) in calls.iter().zip(controls) {
            assert_eq!(actual["virtual_empty"], true);
            assert_eq!(actual["minimum_bits"], bits(&wanted["min_word"]));
            assert_eq!(actual["maximum_bits"], bits(&wanted["max_word"]));
            assert_eq!(actual["excitation_source"], wanted["excitation_code"]);
            if kind < 3 {
                assert_eq!(actual["units"], wanted["units_code"]);
                assert_eq!(
                    actual["resistance_config"],
                    wanted["resistance_config_code"]
                );
                assert_eq!(actual["excitation_bits"], bits(&wanted["excitation_word"]));
                assert_eq!(actual["scale_slot_present"], kind == 0);
                if kind == 0 {
                    assert_eq!(actual["custom_scale_null"], true);
                } else {
                    for field in ["a", "b", "c"] {
                        assert_eq!(
                            actual[format!("{field}_bits")],
                            bits(&wanted["coefficient_words"][field])
                        );
                    }
                    if kind == 2 {
                        assert_eq!(actual["r1_bits"], bits(&wanted["r1_word"]));
                    }
                }
            } else {
                assert_eq!(actual["units"], wanted["output_units_code"]);
                assert_eq!(actual["bridge"], wanted["bridge_code"]);
                assert_eq!(actual["excitation_bits"], bits(&wanted["volts_word"]));
                assert_eq!(
                    actual["ohms_bits"],
                    bits(&wanted["nominal_bridge_ohms_word"])
                );
                let c = &wanted["calibration"];
                for (field, point, axis) in [
                    ("first_electrical_bits", "first", "electrical_word"),
                    ("second_electrical_bits", "second", "electrical_word"),
                    ("first_physical_bits", "first", "physical_word"),
                    ("second_physical_bits", "second", "physical_word"),
                ] {
                    assert_eq!(actual[field], bits(&c[point][axis]));
                }
                assert_eq!(actual["electrical_units"], c["electrical_units_code"]);
                assert_eq!(actual["physical_units"], c["physical_units_code"]);
                assert_ne!(actual["units"], actual["physical_units"]);
                assert_eq!(actual["custom_scale_null"], true);
            }
        }
    }
    #[test]
    fn resistive_native_science_and_mixed_thirteen_keep_raw_units_for_both_timings() {
        for family in 0..7 {
            for software in [false, true] {
                let home = Home::new("ni-resistive-science");
                let device = [
                    "ResistanceSuccess",
                    "ThermIexSuccess",
                    "ThermVexSuccess",
                    "ForceSuccess",
                    "PressureSuccess",
                    "TorqueSuccess",
                    "ResistiveMixedSuccess",
                ][family];
                let config = resistive_source(family, device);
                let config = if software {
                    on_demand(config, 10)
                } else {
                    config
                };
                let library = if family == 6 {
                    if software {
                        &on_demand_libraries()[0]
                    } else {
                        &libraries()[0]
                    }
                } else {
                    &resistive_libraries()[usize::from(software)]
                };
                let (run, pid, _) = run_config_ni(&home, &config, library, "250ms");
                run.success();
                let data = rows(&home);
                footer(&data, "complete");
                resistive_words(&data, &config, family);
                let actual = resistive_bits(&data);
                if software {
                    assert!(actual.len() >= config["channels"].as_array().unwrap().len() * 2);
                } else {
                    assert_eq!(actual.len(), if family == 6 { 26 } else { 12 });
                }
                let log = trace(&home);
                isolated(&log, pid);
                engineering_cleaned(&log, "ReadAnalogF64");
                assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
                assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
                assert_eq!(calls(&log, "ReadCounterF64", "enter").len(), 0);
                for kind in if family == 6 {
                    (0..6).collect::<Vec<_>>()
                } else {
                    vec![family]
                } {
                    resistive_arguments(&log, kind, family == 6);
                }
                if family == 6 {
                    for name in [
                        "CreateAIVoltageChan",
                        "CreateAIThrmcplChan",
                        "CreateAICurrentChan",
                        "CreateAIRTDChan",
                        "CreateAIStrainGageChan",
                        "CreateAIBridgeChan",
                        "CreateAIAccelChan",
                    ] {
                        assert_eq!(calls(&log, name, "enter").len(), 1);
                    }
                    assert_eq!(calls(&log, "SetAICoupling", "enter").len(), 7);
                }
                if software {
                    for name in [
                        "CfgSampClkTiming",
                        "CfgInputBuffer",
                        "SetReadOverWrite",
                        "GetReadAvailSampPerChan",
                        "GetSampClkRate",
                    ] {
                        assert_eq!(calls(&log, name, "enter").len(), 0);
                    }
                }
                for call in calls(&log, "ReadAnalogF64", "arguments") {
                    assert_eq!(call["fill_mode"], 1);
                    assert_eq!(call["reserved_null"], true);
                    if software {
                        assert_eq!(call["requested"], 1);
                        assert_eq!(
                            call["capacity"],
                            config["channels"].as_array().unwrap().len()
                        );
                    }
                }
                temperature_exports(&home, &data, false);
            }
        }
    }
    #[test]
    fn resistive_unit_wire_bridge_cycles_and_explicit_none_coefficients_preserve_requests() {
        for family in 0..6 {
            for suffix in ["All", "None"] {
                let device = format!(
                    "{}{suffix}",
                    [
                        "Resistance",
                        "ThermIex",
                        "ThermVex",
                        "Force",
                        "Pressure",
                        "Torque"
                    ][family]
                );
                let mut config = resistive_source(family, &device);
                if suffix == "All" {
                    let oracle = resistive_oracle(family >= 3);
                    let controls = if family == 0 {
                        vec![
                            json!({"base":0,"resistance_config":"three_wire"}),
                            json!({"base":1,"resistance_config":"four_wire"}),
                            json!({"base":0,"resistance_config":"two_wire"}),
                        ]
                    } else if family < 3 {
                        oracle["short_enum_cycles"]["explicit_modes"]
                            [["resistance", "thermistor_current", "thermistor_voltage"][family]]
                            .as_array()
                            .unwrap()
                            .clone()
                    } else {
                        oracle["short_enum_cycles"][["force", "pressure", "torque"][family - 3]]
                            .as_array()
                            .unwrap()
                            .clone()
                    };
                    let original = config["channels"].as_array().unwrap().clone();
                    let mut channels = Vec::new();
                    for (i, control) in controls.iter().enumerate() {
                        let mut row = original[i % 2].clone();
                        row["physical"] = json!(format!("{device}/ai{i}"));
                        if family < 3 {
                            row["resistance_config"] = control["resistance_config"].clone();
                            if family > 0 {
                                row["units"] = control["units"].clone();
                            }
                        } else {
                            row["units"] = control["units"].clone();
                            row["bridge_config"] = control["bridge"].clone();
                            row["calibration"]["physical_units"] =
                                control["physical_units"].clone();
                        }
                        channels.push(row);
                    }
                    config["channels"] = json!(channels);
                } else {
                    for row in config["channels"].as_array_mut().unwrap() {
                        row["excitation"]["source"] = json!("none");
                        row["excitation"][if family <= 1 { "amperes" } else { "volts" }] =
                            json!(match family {
                                0 => 1.0 / 2048.0,
                                1 | 4 => 0.0,
                                2 | 5 => -0.0,
                                _ => 1.25,
                            });
                    }
                }
                let home = Home::new("ni-resistive-enums");
                let config = on_demand(config, 10);
                let (run, pid, _) =
                    run_config_ni(&home, &config, &resistive_libraries()[1], "180ms");
                run.success();
                let data = rows(&home);
                footer(&data, "complete");
                resistive_words(&data, &config, family);
                let log = trace(&home);
                isolated(&log, pid);
                engineering_cleaned(&log, "ReadAnalogF64");
                temperature_exports(&home, &data, false);
            }
        }
        for family in [1, 2] {
            let device = if family == 1 {
                "ThermIexSignedCoefficients"
            } else {
                "ThermVexSignedCoefficients"
            };
            let mut config = resistive_source(family, device);
            config["channels"].as_array_mut().unwrap().truncate(1);
            let c = &mut config["channels"][0]["coefficients"];
            if family == 1 {
                c["a"] = json!(-9.0 / 8192.0);
            } else {
                c["b"] = json!(-3.0 / 16384.0);
            }
            c["c"] = json!(-0.0);
            let config = on_demand(config, 10);
            let home = Home::new("ni-signed-coefficients");
            let (run, pid, _) = run_config_ni(&home, &config, &resistive_libraries()[1], "160ms");
            run.success();
            let data = rows(&home);
            resistive_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            let function = if family == 1 {
                "CreateAIThrmstrChanIex"
            } else {
                "CreateAIThrmstrChanVex"
            };
            assert_eq!(
                calls(&log, function, "arguments")[0]["c_bits"],
                "8000000000000000"
            );
            temperature_exports(&home, &data, false);
        }
        let mut config = resistive_source(3, "ForceSwapPairs");
        for channel in config["channels"].as_array_mut().unwrap() {
            let first = channel["calibration"]["first"].clone();
            channel["calibration"]["first"] = channel["calibration"]["second"].clone();
            channel["calibration"]["second"] = first;
        }
        let config = on_demand(config, 10);
        let home = Home::new("ni-bridge-pair-swap");
        let (run, pid, _) = run_config_ni(&home, &config, &resistive_libraries()[1], "160ms");
        run.success();
        let data = rows(&home);
        resistive_words(&data, &config, 3);
        let log = trace(&home);
        isolated(&log, pid);
        engineering_cleaned(&log, "ReadAnalogF64");
        temperature_exports(&home, &data, false);
    }
    #[test]
    fn resistive_selected_constructor_union_highest_bit_and_coupling_are_precreate() {
        for family in 0..6 {
            for mixed in [false, true] {
                let home = Home::new("ni-resistive-selected-missing");
                let config = on_demand(
                    resistive_source(
                        if mixed { 6 } else { family },
                        if mixed {
                            "ResistiveMixedSuccess"
                        } else {
                            [
                                "ResistanceSuccess",
                                "ThermIexSuccess",
                                "ThermVexSuccess",
                                "ForceSuccess",
                                "PressureSuccess",
                                "TorqueSuccess",
                            ][family]
                        },
                    ),
                    10,
                );
                let (run, pid, _) =
                    run_config_ni(&home, &config, &resistive_libraries()[family + 3], "100ms");
                assert_eq!(run.failure(3)["code"], "unavailable");
                let log = trace(&home);
                isolated(&log, pid);
                for name in [
                    "CreateTask",
                    "StartTask",
                    "StopTask",
                    "ClearTask",
                    "ReadAnalogF64",
                ] {
                    assert_eq!(calls(&log, name, "enter").len(), 0);
                }
                assert_eq!(resistive_blocks(&rows(&home)), [] as [&Value; 0]);
            }
        }
        for family in 0..6 {
            let home = Home::new("ni-resistive-unused-symbol");
            let other = (family + 1) % 6;
            let config = on_demand(
                resistive_source(
                    other,
                    [
                        "ResistanceSuccess",
                        "ThermIexSuccess",
                        "ThermVexSuccess",
                        "ForceSuccess",
                        "PressureSuccess",
                        "TorqueSuccess",
                    ][other],
                ),
                10,
            );
            let (run, pid, _) =
                run_config_ni(&home, &config, &resistive_libraries()[family + 3], "140ms");
            run.success();
            let data = rows(&home);
            resistive_words(&data, &config, other);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
        }
        let home = Home::new("ni-resistive-coupling-missing");
        let config = on_demand(resistive_source(0, "ResistanceSuccess"), 10);
        let (run, pid, _) = run_config_ni(&home, &config, &resistive_libraries()[9], "100ms");
        assert_eq!(run.failure(3)["code"], "unavailable");
        let log = trace(&home);
        isolated(&log, pid);
        assert_eq!(calls(&log, "CreateTask", "enter").len(), 0);
        for family in 0..6 {
            let device = format!(
                "{}Omitted",
                [
                    "Resistance",
                    "ThermIex",
                    "ThermVex",
                    "Force",
                    "Pressure",
                    "Torque"
                ][family]
            );
            let mut config = resistive_source(family, &device);
            for c in config["channels"].as_array_mut().unwrap() {
                c.as_object_mut().unwrap().remove("coupling");
            }
            let config = on_demand(config, 10);
            let home = Home::new("ni-resistive-omitted-coupling");
            let (run, pid, _) = run_config_ni(&home, &config, &resistive_libraries()[9], "140ms");
            run.success();
            let data = rows(&home);
            resistive_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            assert_eq!(calls(&log, "SetAICoupling", "enter").len(), 0);
        }
        for config in [
            source("Success"),
            engineering_source(3, "EngineeringMixedSuccess"),
            counter_source("FrequencySuccess", true),
        ] {
            let home = Home::new("ni-resistive-old-symbols");
            let (run, pid, _) = run_config_ni(&home, &config, &resistive_libraries()[2], "150ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(
                &log,
                if config["measurement"] == "counter_frequency" {
                    "ReadCounterF64"
                } else {
                    "ReadAnalogF64"
                },
            );
            temperature_exports(&home, &data, false);
        }
    }
    #[test]
    fn resistive_negative_scan_and_cleanup_errors_keep_every_new_family_prefix() {
        for family in 0..7 {
            let device = format!(
                "{}Partial",
                [
                    "Resistance",
                    "ThermIex",
                    "ThermVex",
                    "Force",
                    "Pressure",
                    "Torque",
                    "ResistiveMixed"
                ][family]
            );
            let config = resistive_source(family, &device);
            let home = Home::new("ni-resistive-negative");
            let (run, pid, _) = run_config_ni(
                &home,
                &config,
                if family == 6 {
                    &libraries()[0]
                } else {
                    &resistive_libraries()[0]
                },
                "250ms",
            );
            let error = run.failure(4);
            assert!(has_code(&error, -200_284));
            let data = rows(&home);
            footer(&data, "failed");
            resistive_words(&data, &config, family);
            assert_eq!(
                resistive_bits(&data).len(),
                config["channels"].as_array().unwrap().len()
            );
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
            temperature_exports(&home, &data, true);
        }
        for family in [1, 6] {
            let device = if family == 1 {
                "ThermIexPartialStopFail"
            } else {
                "ResistiveMixedPartialStopFail"
            };
            let config = on_demand(resistive_source(family, device), 10);
            let home = Home::new("ni-resistive-software-negative");
            let (run, pid, _) = run_config_ni(
                &home,
                &config,
                if family == 6 {
                    &on_demand_libraries()[0]
                } else {
                    &resistive_libraries()[1]
                },
                "250ms",
            );
            let error = run.failure(4);
            assert!(has_code(&error, -200_284));
            assert!(has_code(&error, -990_201));
            let data = rows(&home);
            resistive_words(&data, &config, family);
            assert_eq!(
                resistive_bits(&data).len(),
                config["channels"].as_array().unwrap().len()
            );
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            temperature_exports(&home, &data, true);
        }
    }
    #[test]
    fn resistive_deferred_diagnostic_hangs_never_discard_completed_native_words() {
        for (family, suffix) in [
            (1, "DiagHangPartial"),
            (5, "WarnDiagHang"),
            (6, "DiagHangPartial"),
        ] {
            let device = format!(
                "{}{suffix}",
                [
                    "Resistance",
                    "ThermIex",
                    "ThermVex",
                    "Force",
                    "Pressure",
                    "Torque",
                    "ResistiveMixed"
                ][family]
            );
            let config = resistive_source(family, &device);
            let home = Home::new("ni-resistive-hung-diagnostic");
            let (run, pid, elapsed) = run_config_ni(
                &home,
                &config,
                if family == 6 {
                    &libraries()[0]
                } else {
                    &resistive_libraries()[0]
                },
                "250ms",
            );
            let error = run.failure(4);
            assert!(elapsed < Duration::from_secs(3));
            let data = rows(&home);
            footer(&data, "failed");
            resistive_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            if suffix == "DiagHangPartial" {
                assert!(has_code(&error, -200_284));
                assert_eq!(
                    resistive_bits(&data).len(),
                    config["channels"].as_array().unwrap().len()
                );
                assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
            } else {
                assert_eq!(resistive_bits(&data), resistive_expected(family));
                assert!(data.iter().any(|r| has_code(r, 12_345)));
                engineering_cleaned(&log, "ReadAnalogF64");
            }
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
            temperature_exports(&home, &data, true);
        }
    }
    #[test]
    fn resistive_geometry_and_original_calibration_or_setup_failure_do_not_fabricate_data() {
        for (family, suffix) in [
            (0, "Reverse"),
            (1, "CorruptCountNegative"),
            (2, "WarnZeroRead"),
            (3, "CorruptCountTooLarge"),
            (4, "EqualPhysicalReject"),
            (5, "NameForeign"),
            (6, "LaterFail"),
            (0, "CouplingError"),
        ] {
            let device = format!(
                "{}{suffix}",
                [
                    "Resistance",
                    "ThermIex",
                    "ThermVex",
                    "Force",
                    "Pressure",
                    "Torque",
                    "ResistiveMixed"
                ][family]
            );
            let mut config = resistive_source(family, &device);
            if suffix == "EqualPhysicalReject" {
                for c in config["channels"].as_array_mut().unwrap() {
                    c["calibration"]["second"]["physical"] =
                        c["calibration"]["first"]["physical"].clone();
                }
            }
            let software = family == 2;
            let config = if software {
                on_demand(config, 10)
            } else {
                config
            };
            let home = Home::new("ni-resistive-bad-geometry");
            let (run, pid, _) = run_config_ni(
                &home,
                &config,
                if family == 6 {
                    &libraries()[0]
                } else {
                    &resistive_libraries()[usize::from(software)]
                },
                "220ms",
            );
            let error = run.failure(1);
            let setup = matches!(
                suffix,
                "EqualPhysicalReject" | "LaterFail" | "CouplingError"
            );
            assert_eq!(error["code"], if setup { "failed" } else { "incomplete" });
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(resistive_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            if setup {
                assert!(has_code(
                    &error,
                    if suffix == "EqualPhysicalReject" {
                        -201_334
                    } else if suffix == "LaterFail" {
                        -990_104
                    } else {
                        -990_402
                    }
                ));
                for name in ["ReadAnalogF64", "StartTask", "StopTask"] {
                    assert_eq!(calls(&log, name, "enter").len(), 0);
                }
            }
            if suffix == "WarnZeroRead" {
                assert!(has_code(&error, 12_345));
                assert_eq!(error["details"]["actual_samples_per_channel"], 0);
            }
            temperature_exports(&home, &data, true);
        }
    }

    pub(super) fn five_libraries() -> &'static [Vec<u8>; 9] {
        static LIBS: OnceLock<[Vec<u8>; 9]> = OnceLock::new();
        LIBS.get_or_init(|| {
            std::array::from_fn(|variant| {
                let temp = tempfile::tempdir().unwrap();
                let c = temp.path().join("five.c");
                let so = temp.path().join("five.so");
                fs::write(&c, include_str!("support/nidaqmx.c")).unwrap();
                let mut cc = Command::new("cc");
                cc.args([
                    "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC", "-O0",
                ]);
                if variant <= 1 {
                    cc.args([
                        "-DNIDAQMX_FIXTURE_FIVE_ONLY=1",
                        "-DNIDAQMX_FIXTURE_OMIT_TC_TERMINAL=1",
                    ]);
                }
                if variant == 1 || variant >= 3 {
                    cc.arg("-DNIDAQMX_FIXTURE_ON_DEMAND=1");
                }
                if variant == 2 {
                    cc.arg("-DNIDAQMX_FIXTURE_OMIT_FIVE=1");
                }
                if variant >= 3 {
                    cc.arg(format!(
                        "-DNIDAQMX_FIXTURE_OMIT_{}=1",
                        [
                            "VELOCITY_IEPE",
                            "FORCE_IEPE",
                            "CHARGE_ACCEL",
                            "LVDT",
                            "RVDT",
                            "COUPLING"
                        ][variant - 3]
                    ));
                }
                let output = cc
                    .arg(c)
                    .arg("-o")
                    .arg(so.as_path())
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "variant{variant}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                fs::read(so).unwrap()
            })
        })
    }
    fn five_blocks(data: &[Value]) -> Vec<&Value> {
        data.iter()
            .filter(|r| {
                r["kind"] == "sample"
                    && matches!(
                        r["value"]["event"].as_str(),
                        Some(
                            "velocity_iepe_block"
                                | "force_iepe_block"
                                | "charge_acceleration_block"
                                | "lvdt_position_block"
                                | "rvdt_angle_block"
                                | "mixed_analog_block"
                        )
                    )
            })
            .collect()
    }
    fn five_bits(data: &[Value]) -> Vec<String> {
        five_blocks(data)
            .iter()
            .flat_map(|r| r["value"]["samples"].as_array().unwrap())
            .flat_map(|s| s.as_array().unwrap())
            .map(|w| w["ieee754_le_hex"].as_str().unwrap().to_owned())
            .collect()
    }
    fn five_expected(family: usize) -> Vec<String> {
        let oracle = five_oracle(family >= 3);
        let text = if family == 5 {
            &oracle["mixed_eighteen_channel_control"]["all_words_le_hex"]
        } else {
            &oracle["raw_extent"]["all_words_le_hex"]
        };
        text.as_str()
            .unwrap()
            .as_bytes()
            .as_chunks::<16>()
            .0
            .iter()
            .map(|w| std::str::from_utf8(w).unwrap().to_owned())
            .collect()
    }
    fn five_metadata(value: &Value, config: &Value, family: usize) {
        assert_eq!(value["measurement"], config["measurement"]);
        assert_eq!(value["settings_provenance"], "requested_not_read_back");
        let requests = config["channels"].as_array().unwrap();
        let items = value["channel_measurements"].as_array().unwrap();
        assert_eq!(items.len(), requests.len());
        assert_eq!(
            value["channels"],
            json!(
                requests
                    .iter()
                    .map(|r| r["physical"].clone())
                    .collect::<Vec<_>>()
            )
        );
        if family == 5 {
            let mut old = value.clone();
            let mut oldconfig = config.clone();
            old["channel_measurements"] = json!(&items[..13]);
            old["channels"] = json!(&value["channels"].as_array().unwrap()[..13]);
            oldconfig["channels"] = json!(&requests[..13]);
            resistive_metadata(&old, &oldconfig, 6);
            assert!(value.get("units").is_none());
        }
        for (index, (item, request)) in items.iter().zip(requests).enumerate() {
            if family == 5 && index < 13 {
                continue;
            }
            let kind = if family == 5 { index - 13 } else { family };
            let mut expected = request.as_object().unwrap().clone();
            expected.insert("channel_index".to_owned(), json!(index));
            if let Some(terminal) = expected.remove("terminal_config") {
                expected.insert(
                    "terminal_config_provenance".to_owned(),
                    json!(if terminal == "default" {
                        "driver_default_unqueried"
                    } else {
                        "explicit_request_not_read_back"
                    }),
                );
                expected.insert("terminal_config_request".to_owned(), terminal);
            }
            if let Some(coupling) = expected.remove("coupling") {
                expected.insert("coupling_request".to_owned(), coupling);
                expected.insert(
                    "coupling_provenance".to_owned(),
                    json!("explicit_request_not_read_back"),
                );
            } else {
                expected.insert(
                    "coupling_provenance".to_owned(),
                    json!("driver_default_unqueried"),
                );
            }
            if family == 5 {
                expected.insert(
                    "quantity".to_owned(),
                    json!(
                        [
                            "velocity",
                            "force",
                            "acceleration",
                            "linear_position",
                            "angular_position"
                        ][kind]
                    ),
                );
            }
            exact_request(item, &Value::Object(expected));
            for key in [
                "calibration",
                "calibration_provenance",
                "cjc",
                "r0_ohms",
                "rtd_type",
                "shunt",
                "observed_excitation",
                "source_time",
            ] {
                assert!(item.get(key).is_none());
            }
        }
        if config["timing"] == "on_demand" {
            software_metadata(value, 10);
        }
    }
    fn five_words(data: &[Value], config: &Value, family: usize) {
        let expected = five_expected(family);
        let actual = five_bits(data);
        assert_ne!(actual, [] as [String; 0]);
        assert_eq!(
            actual,
            expected
                .iter()
                .cycle()
                .take(actual.len())
                .cloned()
                .collect::<Vec<_>>()
        );
        let event = [
            "velocity_iepe_block",
            "force_iepe_block",
            "charge_acceleration_block",
            "lvdt_position_block",
            "rvdt_angle_block",
            "mixed_analog_block",
        ][family];
        let representation = [
            "driver_scaled_f64_velocity_iepe",
            "driver_scaled_f64_force_iepe",
            "driver_scaled_f64_charge_acceleration",
            "driver_scaled_f64_lvdt_position",
            "driver_scaled_f64_rvdt_angle",
            "driver_scaled_f64_mixed_analog",
        ][family];
        let keys = [
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
            "velocity",
            "force",
            "acceleration",
            "position",
            "angle",
        ];
        let mut index = 0;
        for row in five_blocks(data) {
            let v = &row["value"];
            assert_eq!(v["event"], event);
            assert_eq!(v["representation"], representation);
            assert_eq!(v["sample_index"], index);
            assert!(row.get("source_time").is_none());
            let scans = v["samples"].as_array().unwrap();
            assert_eq!(v["samples_per_channel"], scans.len());
            for scan in scans {
                let words = scan.as_array().unwrap();
                assert_eq!(words.len(), config["channels"].as_array().unwrap().len());
                for (column, word) in words.iter().enumerate() {
                    let key = if family == 5 {
                        keys[column]
                    } else {
                        ["velocity", "force", "acceleration", "position", "angle"][family]
                    };
                    let hex = word["ieee754_le_hex"].as_str().unwrap();
                    assert_eq!(hex.len(), 16);
                    let mut bytes = [0u8; 8];
                    for (i, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                        bytes[i] =
                            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
                    }
                    let number = f64::from_bits(u64::from_le_bytes(bytes));
                    if number.is_finite() {
                        assert_eq!(word[key].as_f64().unwrap().to_bits(), number.to_bits());
                        assert_eq!(word.as_object().unwrap().len(), 2);
                    } else {
                        assert!(word.get(key).is_none());
                        assert_eq!(word.as_object().unwrap().len(), 1);
                    }
                }
            }
            index += scans.len();
            five_metadata(v, config, family);
        }
    }
    fn five_arguments(log: &[Value], family: usize, mixed: bool) {
        let oracle = five_oracle(family >= 3);
        let key = FIVE_TAGS[family];
        let controls = oracle["ordered_constructor_pairs"][key].as_array().unwrap();
        let function = [
            "CreateAIVelocityIEPEChan",
            "CreateAIForceIEPEChan",
            "CreateAIAccelChargeChan",
            "CreateAIPosLVDTChan",
            "CreateAIPosRVDTChan",
        ][family];
        let actual = calls(log, function, "arguments");
        assert_eq!(actual.len(), if mixed { 1 } else { 2 });
        let abi = oracle["fixed_abis"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == format!("DAQmx{function}"))
            .unwrap();
        let roles = abi["arguments"].as_array().unwrap();
        for (actual, control) in actual.iter().zip(controls) {
            assert_eq!(actual["virtual_empty"], true);
            assert_eq!(actual["custom_scale_null"], true);
            assert_eq!(actual["terminal_slot_present"], family < 3);
            assert_eq!(actual["excitation_slot_present"], family != 2);
            for (role, field) in [
                ("min", "minimum_bits"),
                ("max", "maximum_bits"),
                ("sensitivity", "sensitivity_bits"),
                (
                    if family < 2 { "amperes" } else { "volts" },
                    "excitation_bits",
                ),
                ("frequency_hz", "frequency_bits"),
            ] {
                if let Some(name) = control["float_argument_words"].get(role) {
                    assert_eq!(
                        actual[field],
                        oracle["manual_argument_words"][name.as_str().unwrap()]["ieee754_hex"]
                    );
                } else {
                    assert_eq!(actual[field], "0000000000000000");
                }
            }
            for (role, field) in [
                ("terminal", "terminal"),
                ("units", "units"),
                ("sensitivity_units", "sensitivity_units"),
                ("source", "excitation_source"),
                ("ac_wire_mode", "ac_wire_mode"),
            ] {
                let wanted = roles
                    .iter()
                    .position(|r| r["role"] == role)
                    .map_or_else(|| json!(0), |i| control["ordered_arguments"][i].clone());
                assert_eq!(actual[field], wanted);
            }
        }
    }
    #[test]
    fn five_native_scientific_roles_and_mixed_eighteen_preserve_raw_words_for_both_timings() {
        for (family, &device_name) in FIVE_DEVICES.iter().take(6).enumerate() {
            for software in [false, true] {
                let home = Home::new("ni-five-science");
                let device = format!("{device_name}Success");
                let config = five_source(family, &device);
                let config = if software {
                    on_demand(config, 10)
                } else {
                    config
                };
                let library = if family == 5 {
                    if software {
                        &on_demand_libraries()[0]
                    } else {
                        &libraries()[0]
                    }
                } else {
                    &five_libraries()[usize::from(software)]
                };
                let (run, pid, _) = run_config_ni(&home, &config, library, "250ms");
                run.success();
                let data = rows(&home);
                footer(&data, "complete");
                five_words(&data, &config, family);
                let actual = five_bits(&data);
                if software {
                    assert!(actual.len() >= config["channels"].as_array().unwrap().len() * 2);
                } else {
                    assert_eq!(actual.len(), if family == 5 { 36 } else { 12 });
                }
                let log = trace(&home);
                isolated(&log, pid);
                engineering_cleaned(&log, "ReadAnalogF64");
                assert_eq!(calls(&log, "CreateTask", "enter").len(), 1);
                assert_eq!(calls(&log, "StartTask", "enter").len(), 1);
                for kind in if family == 5 {
                    (0..5).collect::<Vec<_>>()
                } else {
                    vec![family]
                } {
                    five_arguments(&log, kind, family == 5);
                }
                if family == 5 {
                    let oracle = five_oracle(true);
                    let constructors = log
                        .iter()
                        .filter(|r| {
                            r["phase"] == "enter"
                                && r["fn"].as_str().unwrap().starts_with("DAQmxCreateAI")
                        })
                        .map(|r| r["fn"].clone())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        json!(constructors),
                        oracle["mixed_eighteen_channel_control"]["constructor_order"]
                    );
                    assert_eq!(calls(&log, "SetAICoupling", "enter").len(), 10);
                }
                for call in calls(&log, "ReadAnalogF64", "arguments") {
                    assert_eq!(call["fill_mode"], 1);
                    assert_eq!(call["reserved_null"], true);
                    if software {
                        assert_eq!(call["requested"], 1);
                        assert_eq!(
                            call["capacity"],
                            config["channels"].as_array().unwrap().len()
                        );
                    }
                }
                if software {
                    software_calls_with_channels(&log, if family == 5 { 18 } else { 2 });
                }
                for name in [
                    "ReadDigitalU32",
                    "ReadCounterU32",
                    "ReadCounterF64",
                    "SetAITermCfg",
                ] {
                    assert_eq!(calls(&log, name, "enter").len(), 0);
                }
                temperature_exports(&home, &data, false);
            }
        }
    }
    #[test]
    fn five_finite_enum_cycles_signed_sensitivity_and_none_amounts_are_request_bits() {
        for family in 0..5 {
            let oracle = five_oracle(family >= 3);
            let device = format!("{}All", FIVE_DEVICES[family]);
            let mut config = five_source(family, &device);
            let base = config["channels"][0].clone();
            let mut channels = Vec::new();
            for (index, variant) in oracle["short_request_cycles"][FIVE_TAGS[family]]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                let o = &variant["overrides"];
                let mut row = base.clone();
                row["physical"] = json!(format!("{device}/ai{index}"));
                let number = |word: &Value| {
                    oracle["manual_argument_words"][word.as_str().unwrap()]["decimal"]
                        .as_str()
                        .unwrap()
                        .parse::<f64>()
                        .unwrap()
                };
                row["sensitivity"] = json!(number(&o["sensitivity_word"]));
                row["coupling"] = o["coupling"].clone();
                if family != 2 {
                    row["excitation"]["source"] = o["excitation_source"].clone();
                    row["excitation"][if family < 2 { "amperes" } else { "volts" }] =
                        json!(number(&o["excitation_amount_word"]));
                }
                if family >= 3 {
                    row["excitation"]["frequency_hz"] = json!(number(&o["frequency_word"]));
                    row["ac_wire_mode"] = o["ac_wire_mode"].clone();
                }
                for field in ["units", "sensitivity_units"] {
                    if let Some(v) = o.get(field) {
                        row[field] = v.clone();
                    }
                }
                channels.push(row);
            }
            config["channels"] = json!(channels);
            let config = on_demand(config, 10);
            let home = Home::new("ni-five-cycles");
            let (run, pid, _) = run_config_ni(&home, &config, &five_libraries()[1], "180ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            five_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            assert_eq!(calls(&log, "SetAICoupling", "enter").len(), 3);
            let function = [
                "CreateAIVelocityIEPEChan",
                "CreateAIForceIEPEChan",
                "CreateAIAccelChargeChan",
                "CreateAIPosLVDTChan",
                "CreateAIPosRVDTChan",
            ][family];
            let args = calls(&log, function, "arguments");
            assert_eq!(args.len(), 3);
            for (actual, word) in args
                .iter()
                .zip(["negative_sensitivity", "zero", "negative_zero"])
            {
                assert_eq!(
                    actual["sensitivity_bits"],
                    oracle["manual_argument_words"][word]["ieee754_hex"]
                );
            }
            temperature_exports(&home, &data, false);
        }
    }
    #[test]
    fn five_selected_high_bit_constructors_and_optional_coupling_resolve_before_create() {
        for (family, &device_name) in FIVE_DEVICES.iter().take(5).enumerate() {
            for mixed in [false, true] {
                let home = Home::new("ni-five-selected-missing");
                let config = on_demand(
                    five_source(
                        if mixed { 5 } else { family },
                        if mixed {
                            "FiveMixedSuccess"
                        } else {
                            device_name
                        },
                    ),
                    10,
                );
                let (run, pid, _) =
                    run_config_ni(&home, &config, &five_libraries()[family + 3], "100ms");
                assert_eq!(run.failure(3)["code"], "unavailable");
                let log = trace(&home);
                isolated(&log, pid);
                for name in [
                    "CreateTask",
                    "StartTask",
                    "StopTask",
                    "ClearTask",
                    "ReadAnalogF64",
                ] {
                    assert_eq!(calls(&log, name, "enter").len(), 0);
                }
            }
            let other = (family + 1) % 5;
            let home = Home::new("ni-five-unused-missing");
            let config = on_demand(five_source(other, FIVE_DEVICES[other]), 10);
            let (run, pid, _) =
                run_config_ni(&home, &config, &five_libraries()[family + 3], "140ms");
            run.success();
            let data = rows(&home);
            five_words(&data, &config, other);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            temperature_exports(&home, &data, false);
        }
        let home = Home::new("ni-five-missing-coupling");
        let config = on_demand(five_source(0, "VelocitySuccess"), 10);
        let (run, pid, _) = run_config_ni(&home, &config, &five_libraries()[8], "100ms");
        assert_eq!(run.failure(3)["code"], "unavailable");
        let log = trace(&home);
        isolated(&log, pid);
        assert_eq!(calls(&log, "CreateTask", "enter").len(), 0);
        for (family, &device_name) in FIVE_DEVICES.iter().take(5).enumerate() {
            let device = format!("{device_name}Omitted");
            let mut config = five_source(family, &device);
            for channel in config["channels"].as_array_mut().unwrap() {
                channel.as_object_mut().unwrap().remove("coupling");
            }
            let config = on_demand(config, 10);
            let home = Home::new("ni-five-omitted-coupling");
            let (run, pid, _) = run_config_ni(&home, &config, &five_libraries()[8], "140ms");
            run.success();
            let data = rows(&home);
            five_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            engineering_cleaned(&log, "ReadAnalogF64");
            assert_eq!(calls(&log, "SetAICoupling", "enter").len(), 0);
        }
        for config in [
            source("Success"),
            thermocouple_source("Success"),
            resistive_source(6, "ResistiveMixedSuccess"),
            counter_source("FrequencySuccess", true),
        ] {
            let home = Home::new("ni-old-without-new-five");
            let (run, pid, _) = run_config_ni(&home, &config, &five_libraries()[2], "180ms");
            run.success();
            let data = rows(&home);
            footer(&data, "complete");
            let log = trace(&home);
            isolated(&log, pid);
            let read = if config["measurement"] == "counter_frequency" {
                "ReadCounterF64"
            } else {
                "ReadAnalogF64"
            };
            engineering_cleaned(&log, read);
            temperature_exports(&home, &data, false);
        }
    }
    #[test]
    fn five_negative_complete_scan_prefix_and_cleanup_errors_keep_native_first_cause() {
        for (family, &device_name) in FIVE_DEVICES.iter().take(6).enumerate() {
            for software in [false, true] {
                let device = format!("{device_name}Partial");
                let config = five_source(family, &device);
                let config = if software {
                    on_demand(config, 10)
                } else {
                    config
                };
                let home = Home::new("ni-five-partial");
                let library = if family == 5 {
                    if software {
                        &on_demand_libraries()[0]
                    } else {
                        &libraries()[0]
                    }
                } else {
                    &five_libraries()[usize::from(software)]
                };
                let (run, pid, _) = run_config_ni(&home, &config, library, "250ms");
                let error = run.failure(4);
                assert!(has_code(&error, -200_284));
                let data = rows(&home);
                footer(&data, "failed");
                five_words(&data, &config, family);
                assert_eq!(
                    five_bits(&data).len(),
                    config["channels"].as_array().unwrap().len()
                );
                let log = trace(&home);
                isolated(&log, pid);
                engineering_cleaned(&log, "ReadAnalogF64");
                assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
                temperature_exports(&home, &data, true);
            }
        }
        let home = Home::new("ni-five-partial-stop");
        let config = on_demand(five_source(4, "RvdtPartialStopFail"), 10);
        let (run, pid, _) = run_config_ni(&home, &config, &five_libraries()[1], "250ms");
        let error = run.failure(4);
        assert!(has_code(&error, -200_284));
        assert!(has_code(&error, -990_201));
        let data = rows(&home);
        five_words(&data, &config, 4);
        assert_eq!(five_bits(&data).len(), 2);
        let log = trace(&home);
        isolated(&log, pid);
        engineering_cleaned(&log, "ReadAnalogF64");
        temperature_exports(&home, &data, true);
    }
    #[test]
    fn five_deferred_diagnostic_hangs_cannot_hide_completed_words_or_original_status() {
        for (family, suffix) in [
            (0, "DiagHangPartial"),
            (5, "DiagHangPartial"),
            (4, "WarnDiagHang"),
        ] {
            let home = Home::new("ni-five-diagnostic-hang");
            let device = format!("{}{suffix}", FIVE_DEVICES[family]);
            let config = five_source(family, &device);
            let library = if family == 5 {
                &libraries()[0]
            } else {
                &five_libraries()[0]
            };
            let (run, pid, elapsed) = run_config_ni(&home, &config, library, "250ms");
            let error = run.failure(4);
            assert!(elapsed < Duration::from_secs(3));
            let data = rows(&home);
            footer(&data, "failed");
            five_words(&data, &config, family);
            let log = trace(&home);
            isolated(&log, pid);
            if suffix == "DiagHangPartial" {
                assert!(has_code(&error, -200_284));
                assert_eq!(
                    five_bits(&data).len(),
                    config["channels"].as_array().unwrap().len()
                );
                assert_eq!(calls(&log, "ReadAnalogF64", "enter").len(), 1);
                assert_eq!(calls(&log, "GetExtendedErrorInfo", "enter").len(), 1);
                assert_eq!(calls(&log, "StopTask", "enter").len(), 0);
            } else {
                assert_eq!(five_bits(&data), five_expected(family));
                assert!(data.iter().any(|r| has_code(r, 12_345)));
                engineering_cleaned(&log, "ReadAnalogF64");
            }
            let worker = i32::try_from(log[0]["pid"].as_u64().unwrap()).unwrap();
            assert!(!support::cli::process_lives(worker));
            temperature_exports(&home, &data, true);
        }
    }
    #[test]
    fn five_invalid_geometry_and_later_setup_failure_never_invent_samples_or_start() {
        for (family, suffix, software) in [
            (0, "Reverse", false),
            (4, "CountMismatch", false),
            (0, "ZeroRead", true),
            (1, "CorruptCountNegative", false),
            (2, "CorruptCountTooLarge", false),
            (3, "WarnZeroRead", true),
            (4, "NameForeign", false),
            (5, "LaterFail", false),
            (0, "CouplingError", true),
            (1, "LaterFail", true),
        ] {
            let home = Home::new("ni-five-invalid-geometry");
            let device = format!("{}{suffix}", FIVE_DEVICES[family]);
            let config = five_source(family, &device);
            let config = if software {
                on_demand(config, 10)
            } else {
                config
            };
            let library = if family == 5 {
                &libraries()[0]
            } else {
                &five_libraries()[usize::from(software)]
            };
            let (run, pid, _) = run_config_ni(&home, &config, library, "220ms");
            let error = run.failure(1);
            let setup = matches!(suffix, "LaterFail" | "CouplingError");
            assert_eq!(error["code"], if setup { "failed" } else { "incomplete" });
            let data = rows(&home);
            footer(&data, "failed");
            assert_eq!(five_blocks(&data), [] as [&Value; 0]);
            let log = trace(&home);
            isolated(&log, pid);
            assert_eq!(calls(&log, "ClearTask", "enter").len(), 1);
            if setup {
                assert!(has_code(
                    &error,
                    if suffix == "LaterFail" {
                        -990_104
                    } else {
                        -990_402
                    }
                ));
                for name in ["StartTask", "StopTask", "ReadAnalogF64"] {
                    assert_eq!(calls(&log, name, "enter").len(), 0);
                }
            }
            if suffix == "WarnZeroRead" {
                assert!(has_code(&error, 12_345));
                assert_eq!(error["details"]["actual_samples_per_channel"], 0);
            }
            temperature_exports(&home, &data, true);
        }
    }
}
