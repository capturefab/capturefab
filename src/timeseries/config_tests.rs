//! Exercise the complete flattened adapter dispatch, both from JSON text and
//! from an already parsed Value. No transport is opened by parsing these maps.
use super::{Adapter, Config, Selection, ads, ethernet_ip, fins, mqtt, pccc, s7, scpi};
use crate::types::Usage;
use serde_json::Value;

fn both(input: &str) -> [Config; 2] {
    [
        serde_json::from_str(input).expect("raw configuration must parse"),
        serde_json::from_value(serde_json::from_str::<Value>(input).unwrap())
            .expect("Value configuration must parse"),
    ]
}

fn document(source: &str) -> String {
    format!(r#"{{"version":1.0,"sources":[{{"id":"source",{source}}}]}}"#)
}

fn reject_both(input: &str) {
    serde_json::from_str::<Config>(input).expect_err(input);
    serde_json::from_value::<Config>(serde_json::from_str::<Value>(input).unwrap())
        .expect_err(input);
}

#[test]
fn exact_notation_reaches_native_plc_fields_through_flattened_dispatch() {
    let input = r#"{"version":1e0,"sources":[
        {"id":"modbus","protocol":"modbus_tcp","address":"127.0.0.1:502",
         "unit":2.55e2,"signals":[{"name":"value","address":6.5535e4,"scale":1.25}]},
        {"id":"rtu","protocol":"modbus_rtu","device":"unused",
         "baud":1.92e4,"stop_bits":2.0,"unit":2e0,
         "signals":[{"name":"value","unit":2.47e2,"address":1e1}]},
        {"id":"s7","protocol":"s7","address":"127.0.0.1:102",
         "rack":7e0,"slot":3.1e1,"local_tsap":6.5535e4,"remote_tsap":2.58e2,
         "signals":[{"name":"value","db":1.0,"address":1.6e1,"bit":0e4}]},
        {"id":"ads","protocol":"ads","address":"127.0.0.1:48898",
         "target_net_id":"1.2.3.4.5.6","source_net_id":"6.5.4.3.2.1",
         "target_port":8.51e2,"source_port":3.2905e4,
         "signals":[{"name":"value","index_group":4.294967295e9,
                     "index_offset":4.294967295e9,"elements":2.0,"length":null},
                    {"name":"bytes","symbol":"MAIN.Bytes","data_type":"bytes","length":6.5536e4}]},
        {"id":"pccc","protocol":"pccc","address":"127.0.0.1:44818",
         "slot":0.0,"vendor_id":6.5535e4,"serial_number":4.294967295e9,
         "max_read_bytes":2.24e2,"signals":[{"name":"value","address":"N7:0","elements":4.096e3}]},
        {"id":"fins","protocol":"fins_tcp","address":"127.0.0.1:9600",
         "gateway_count":7e0,"source_network":1.0,"source_node":2.0,"source_unit":3.0,
         "target_network":4.0,"target_node":2.54e2,"target_unit":0.0,
         "signals":[{"name":"value","address":6.5535e4,"bit":1.5e1,"data_type":"bool","elements":1e0}]},
        {"id":"slmp","protocol":"slmp","address":"127.0.0.1:5000",
         "network":2e0,"station":2.55e2,"module_io":1.023e3,"multidrop":0e2,
         "monitoring_timer":4.0,"signals":[{"name":"value","device":"D","address":1e2,"elements":2e0}]},
        {"id":"cip","protocol":"ethernet_ip","address":"127.0.0.1:44818",
         "slot":2.55e2,"signals":[{"name":"value","tag":"Counts[3]","elements":2.56e2}]}
    ]}"#;
    for config in both(input) {
        config.validate().unwrap();
        let [tcp, rtu, s7, ads, pccc, fins, slmp, cip] = config.sources.as_slice() else {
            panic!("all PLC adapters must survive dispatch");
        };
        let Adapter::ModbusTcp(tcp) = &tcp.adapter else {
            panic!("Modbus TCP")
        };
        assert_eq!((tcp.unit, tcp.signals[0].signal.address), (255, 65_535));
        assert_eq!(tcp.signals[0].signal.scale, 1.25);
        let Adapter::ModbusRtu(rtu) = &rtu.adapter else {
            panic!("Modbus RTU")
        };
        assert_eq!((rtu.baud, rtu.stop_bits, rtu.unit), (19_200, 2, 2));
        assert_eq!(
            (rtu.signals[0].unit, rtu.signals[0].signal.address),
            (Some(247), 10)
        );
        let Adapter::S7(s7) = &s7.adapter else {
            panic!("S7")
        };
        assert_eq!((s7.rack, s7.slot), (7, 31));
        assert_eq!((s7.local_tsap, s7.remote_tsap), (Some(65_535), Some(258)));
        assert_eq!(
            (s7.signals[0].db, s7.signals[0].address, s7.signals[0].bit),
            (1, 16, 0)
        );
        let Adapter::Ads(ads) = &ads.adapter else {
            panic!("ADS")
        };
        assert_eq!((ads.target_port, ads.source_port), (851, 32_905));
        assert_eq!(ads.signals[0].index_group, Some(u32::MAX));
        assert_eq!(ads.signals[0].index_offset, Some(u32::MAX));
        assert_eq!(ads.signals[0].elements, 2);
        assert_eq!(ads.signals[0].length, None);
        assert_eq!(ads.signals[1].length, Some(65_536));
        let Adapter::Pccc(pccc) = &pccc.adapter else {
            panic!("PCCC")
        };
        assert_eq!(
            (pccc.slot, pccc.vendor_id, pccc.serial_number),
            (Some(0), u16::MAX, Some(u32::MAX))
        );
        assert_eq!((pccc.max_read_bytes, pccc.signals[0].elements), (224, 4096));
        let Adapter::FinsTcp(fins) = &fins.adapter else {
            panic!("FINS")
        };
        assert_eq!(
            (
                fins.gateway_count,
                fins.source_network,
                fins.source_node,
                fins.source_unit
            ),
            (7, 1, 2, 3)
        );
        assert_eq!(
            (fins.target_network, fins.target_node, fins.target_unit),
            (4, Some(254), 0)
        );
        assert_eq!(
            (
                fins.signals[0].address,
                fins.signals[0].bit,
                fins.signals[0].elements
            ),
            (65_535, Some(15), 1)
        );
        let Adapter::Slmp(slmp) = &slmp.adapter else {
            panic!("SLMP")
        };
        assert_eq!(
            (
                slmp.network,
                slmp.station,
                slmp.module_io,
                slmp.multidrop,
                slmp.monitoring_timer
            ),
            (2, 255, 1023, 0, 4)
        );
        assert_eq!(
            (slmp.signals[0].address, slmp.signals[0].elements),
            (100, 2)
        );
        let Adapter::EthernetIp(Selection::BuiltIn(cip)) = &cip.adapter else {
            panic!("native EtherNet/IP")
        };
        assert_eq!((cip.slot, cip.signals[0].elements), (Some(255), 256));
    }
}

#[test]
fn nested_serial_camera_and_passive_maps_keep_optional_and_required_integers() {
    let input = r#"{"version":1.0,"sources":[
        {"id":"scpi","protocol":"scpi","connection":{"transport":"serial","device":"unused",
          "baud":9.6e3,"data_bits":8e0,"stop_bits":1.0},
         "signals":[{"name":"value","query":"MEAS:VOLT:DC?","max_bytes":1.048576e6},
                    {"name":"trace","query":"CURVe?","format":"binary_block","output":"trace-{seq}.bin","max_bytes":1.073741824e9}]},
        {"id":"jsonl","protocol":"serial_jsonl","device":"unused","baud":1.152e5,"data_bits":8.0,"stop_bits":2e0},
        {"id":"camera","protocol":"camera","camera":"sim:0","output":"frame-{seq}.raw","quality":1e2,"warmup_ms":3e4},
        {"id":"can","protocol":"can","interface":"unused","filters":[{"can_id":5.36870911e8,"mask":5.36870911e8,"extended":true}],
         "signals":[{"name":"value","can_id":5.36870911e8,"extended":true,"offset":6.3e1,"data_type":"bool","bit":7e0}]},
        {"id":"ifm","protocol":"ifm_iot","address":"127.0.0.1:80","port":8e0,
         "signals":[{"name":"value","offset":2.55e2,"bit":null,"right_shift":1.0}]},
        {"id":"opc","protocol":"opc_ua","endpoint":"opc.tcp://127.0.0.1:4840",
         "sampling_ms":1e2,"request_timeout_ms":3e4,"nodes":[{"name":"value","node_id":"ns=2;i=1"}]}
    ]}"#;
    for config in both(input) {
        let [scpi, jsonl, camera, can, ifm, opc] = config.sources.as_slice() else {
            panic!("all nested and passive adapters must survive dispatch");
        };
        let Adapter::Scpi(scpi) = &scpi.adapter else {
            panic!("SCPI")
        };
        let scpi::ConnectionConfig::Serial {
            baud,
            data_bits,
            stop_bits,
            ..
        } = &scpi.connection
        else {
            panic!("serial SCPI")
        };
        assert_eq!((*baud, *data_bits, *stop_bits), (9600, 8, 1));
        assert_eq!(scpi.signals[0].max_bytes, Some(1_048_576));
        assert_eq!(scpi.signals[1].max_bytes, Some(1_073_741_824));
        let Adapter::JsonlSerial(jsonl) = &jsonl.adapter else {
            panic!("serial JSONL")
        };
        assert_eq!(
            (jsonl.baud, jsonl.data_bits, jsonl.stop_bits),
            (115_200, 8, 2)
        );
        let Adapter::Camera(camera) = &camera.adapter else {
            panic!("camera")
        };
        assert_eq!((camera.quality, camera.warmup_ms), (Some(100), 30_000));
        let Adapter::Can(can) = &can.adapter else {
            panic!("CAN")
        };
        assert_eq!(
            (can.filters[0].can_id, can.filters[0].mask),
            (0x1fff_ffff, 0x1fff_ffff)
        );
        assert_eq!(
            (
                can.signals[0].can_id,
                can.signals[0].offset,
                can.signals[0].bit
            ),
            (0x1fff_ffff, 63, Some(7))
        );
        let Adapter::IfmIot(ifm) = &ifm.adapter else {
            panic!("ifm")
        };
        assert_eq!(ifm.port, 8);
        assert_eq!(
            (
                ifm.signals[0].offset,
                ifm.signals[0].bit,
                ifm.signals[0].right_shift
            ),
            (255, None, 1)
        );
        let Adapter::OpcUa(Selection::BuiltIn(opc)) = &opc.adapter else {
            panic!("native OPC-UA")
        };
        assert_eq!((opc.sampling_ms, opc.request_timeout_ms), (100, 30_000));
    }
}

#[test]
fn omitted_and_null_optional_fields_keep_protocol_defaults() {
    let input = r#"{"version":1,"sources":[
        {"id":"modbus","protocol":"modbus_rtu","device":"unused","signals":[{"name":"value","address":0.0}]},
        {"id":"s7","protocol":"s7","address":"127.0.0.1:102","local_tsap":null,"signals":[{"name":"value","db":1,"address":0}]},
        {"id":"ads","protocol":"ads","address":"127.0.0.1:48898","target_net_id":"1.2.3.4.5.6","source_net_id":"6.5.4.3.2.1",
         "signals":[{"name":"value","symbol":"MAIN.Value","index_group":null,"length":null}]},
        {"id":"pccc","protocol":"pccc","address":"127.0.0.1:44818","slot":null,"serial_number":null,"signals":[{"name":"value","address":"N7:0"}]},
        {"id":"fins","protocol":"fins_tcp","address":"127.0.0.1:9600","target_node":null,"signals":[{"name":"value","address":0}]},
        {"id":"scpi","protocol":"scpi","connection":{"transport":"serial","device":"unused"},"signals":[{"name":"value","query":"READ?","max_bytes":null}]}
    ]}"#;
    for config in both(input) {
        for source in &config.sources {
            assert_eq!((source.every_ms, source.timeout_ms), (100, 1000));
        }
        let [rtu, s7, ads, pccc, fins, scpi] = config.sources.as_slice() else {
            panic!("defaults")
        };
        let Adapter::ModbusRtu(rtu) = &rtu.adapter else {
            panic!("Modbus RTU")
        };
        assert_eq!((rtu.baud, rtu.stop_bits, rtu.unit), (19_200, 1, 1));
        let Adapter::S7(s7) = &s7.adapter else {
            panic!("S7")
        };
        assert_eq!(
            (s7.rack, s7.slot, s7.local_tsap, s7.remote_tsap),
            (0, 2, None, None)
        );
        let Adapter::Ads(ads) = &ads.adapter else {
            panic!("ADS")
        };
        assert_eq!((ads.target_port, ads.source_port), (851, 32_905));
        assert_eq!(
            (
                ads.signals[0].index_group,
                ads.signals[0].index_offset,
                ads.signals[0].length
            ),
            (None, None, None)
        );
        assert_eq!(ads.signals[0].elements, 1);
        let Adapter::Pccc(pccc) = &pccc.adapter else {
            panic!("PCCC")
        };
        assert_eq!(
            (
                pccc.slot,
                pccc.serial_number,
                pccc.vendor_id,
                pccc.max_read_bytes
            ),
            (None, None, 0, 220)
        );
        assert_eq!(pccc.signals[0].elements, 1);
        let Adapter::FinsTcp(fins) = &fins.adapter else {
            panic!("FINS")
        };
        assert_eq!(
            (fins.gateway_count, fins.target_node, fins.signals[0].bit),
            (2, None, None)
        );
        let Adapter::Scpi(scpi) = &scpi.adapter else {
            panic!("SCPI")
        };
        let scpi::ConnectionConfig::Serial {
            baud,
            data_bits,
            stop_bits,
            ..
        } = &scpi.connection
        else {
            panic!("serial SCPI")
        };
        assert_eq!((*baud, *data_bits, *stop_bits), (9600, 8, 1));
        assert_eq!(scpi.signals[0].max_bytes, None);
    }
}

#[test]
fn camera_part_selection_uses_native_capture_format_names() {
    use crate::transport::multipart::GvspPartFormat;

    let camera = r#""protocol":"camera","camera":"sim:0","output":"frame-{seq}.raw""#;
    for (field, expected) in [
        ("", None),
        (r#", "part_format":null"#, None),
        (
            r#", "part_format":"Coord3D_C16""#,
            Some(GvspPartFormat::Coord3dC16),
        ),
        (
            r#", "part_format":"Confidence8""#,
            Some(GvspPartFormat::Confidence8),
        ),
        (
            r#", "part_format":"Confidence16""#,
            Some(GvspPartFormat::Confidence16),
        ),
        (r#", "part_format":"Mono16""#, Some(GvspPartFormat::Mono16)),
        (
            r#", "part_format":"Coord3D_ABC32f""#,
            Some(GvspPartFormat::Coord3dAbc32f),
        ),
    ] {
        for config in both(&document(&format!("{camera}{field}"))) {
            let Adapter::Camera(camera) = &config.sources[0].adapter else {
                panic!("camera");
            };
            assert_eq!(camera.part_format, expected);
        }
    }
    reject_both(&document(&format!(
        r#"{camera},"part_format":"confidence8""#
    )));
    reject_both(&document(
        r#""protocol":"video","camera":"sim:0","output":"clip.mkv","part_format":"Confidence8""#,
    ));
}

#[test]
fn source_timing_keeps_values_above_f64_precision_before_semantic_validation() {
    for (token, expected) in [
        ("9.007199254740993e15", 9_007_199_254_740_993),
        ("18446744073709551615.0", u64::MAX),
    ] {
        let input = document(&format!(
            r#""protocol":"simulator","signal":"sample","every_ms":{token},"timeout_ms":{token}"#
        ));
        for config in both(&input) {
            assert_eq!(config.sources[0].every_ms, expected);
            assert_eq!(config.sources[0].timeout_ms, expected);
            // Exact decoding never relaxes the recorder's timing limits.
            config.validate().unwrap_err();
        }
    }
}

#[test]
fn fractions_objects_and_narrow_overflow_remain_invalid_across_dispatch_paths() {
    for token in [
        "1.00000000000000000001",
        "-1e0",
        "2.56e2",
        "true",
        "null",
        "\"1\"",
        "{}",
        "[]",
    ] {
        reject_both(&document(&format!(
            r#""protocol":"modbus_tcp","address":"127.0.0.1:502","unit":{token},"signals":[{{"name":"value","address":0}}]"#
        )));
    }
    for source in [
        r#""protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"value","address":6.5536e4}]"#,
        r#""protocol":"s7","address":"127.0.0.1:102","local_tsap":6.5536e4,"signals":[]"#,
        r#""protocol":"ads","address":"127.0.0.1:48898","target_net_id":"1.2.3.4.5.6","source_net_id":"6.5.4.3.2.1","signals":[{"name":"value","index_group":4.294967296e9}]"#,
        r#""protocol":"pccc","address":"127.0.0.1:44818","serial_number":4.294967296e9,"signals":[]"#,
        r#""protocol":"fins_tcp","address":"127.0.0.1:9600","gateway_count":7.0000000000000001,"signals":[]"#,
        r#""protocol":"ethernet_ip","address":"127.0.0.1:44818","slot":2.56e2,"signals":[]"#,
        r#""protocol":"scpi","connection":{"transport":"serial","device":"unused","baud":4.294967296e9},"signals":[]"#,
        r#""protocol":"scpi","connection":{"transport":"serial","device":"unused","data_bits":8.000000000000001},"signals":[]"#,
        r#""protocol":"scpi","connection":{"transport":"tcp","address":"127.0.0.1:5025"},"signals":[{"name":"value","query":"READ?","max_bytes":18446744073709551616.0}]"#,
        r#""protocol":"simulator","signal":"sample","every_ms":18446744073709551616.0"#,
        r#""protocol":"simulator","signal":"sample","timeout_ms":1e-1000"#,
    ] {
        reject_both(&document(source));
    }
}

#[test]
fn signal_unit_overrides_are_exact_and_adapter_scoped() {
    let input = document(
        r#""protocol":"omron_host_link","device":"unused","mode":"c_mode_rd",
           "signals":[{"name":"value","unit":3.1e1,"address":1e2}]"#,
    );
    for config in both(&input) {
        let Adapter::OmronHostLink(fins::host_link::Config::Legacy(config)) =
            &config.sources[0].adapter
        else {
            panic!("Omron Host Link")
        };
        assert_eq!(config.unit, 0);
        assert_eq!(config.signals[0].unit, Some(31));
        assert_eq!(config.signals[0].signal.address, 100);
    }
    let input = document(
        r#""protocol":"modbus_tcp","address":"127.0.0.1:502","unit":5e0,
           "signals":[{"name":"default","address":0},
                      {"name":"gateway","unit":2.55e2,"address":1e2},
                      {"name":"local","unit":0,"address":1}]"#,
    );
    for config in both(&input) {
        let Adapter::ModbusTcp(config) = &config.sources[0].adapter else {
            panic!("Modbus TCP")
        };
        let [default, gateway, local] = config.signals.as_slice() else {
            panic!("three TCP signals")
        };
        assert_eq!(default.unit(config.unit), 5);
        assert_eq!(gateway.unit(config.unit), 255);
        assert_eq!(gateway.signal.address, 100);
        assert_eq!(local.unit(config.unit), 0);
    }
    for token in ["1.00000000000000000001", "-1", "256", "null", "\"1\""] {
        reject_both(&document(&format!(
            r#""protocol":"modbus_rtu","device":"unused","signals":[{{"name":"value","unit":{token},"address":0}}]"#
        )));
        reject_both(&document(&format!(
            r#""protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{{"name":"value","unit":{token},"address":0}}]"#
        )));
    }
    for source in [
        r#""protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"value","address":0,"unit":7,"unexpected":1}]"#,
        r#""protocol":"fins_tcp","address":"127.0.0.1:9600","signals":[{"name":"value","address":0,"unit":7}]"#,
        r#""protocol":"fins_udp","address":"127.0.0.1:9600","source_node":1,"target_node":2,"signals":[{"name":"value","address":0,"unit":7}]"#,
        r#""protocol":"modbus_rtu","device":"unused","signals":[{"name":"value","address":0,"unit":7,"unexpected":1}]"#,
    ] {
        reject_both(&document(source));
    }
}

#[test]
fn duplicates_and_unknown_fields_still_fail_at_source_adapter_and_nested_levels() {
    // A Value has already discarded duplicate keys; exercise those at the raw
    // text boundary where the parser can still reject them.
    for input in [
        r#"{"version":1,"version":1.0,"sources":[]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"simulator","signal":"sample","every_ms":1e2,"every_ms":101}]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[]}]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","unit":1e0,"unit":2,"signals":[]}]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"value","address":1.0,"address":2}]}]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"value","address":0,"unit":1e0,"unit":2}]}]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"scpi","connection":{"transport":"serial","transport":"serial","device":"unused"},"signals":[]}]}"#,
        r#"{"version":1,"sources":[{"id":"x","protocol":"scpi","connection":{"transport":"serial","device":"unused","baud":9.6e3,"baud":9601},"signals":[]}]}"#,
    ] {
        serde_json::from_str::<Config>(input).expect_err(input);
    }
    for source in [
        r#""protocol":"simulator","signal":"sample","unknown":1"#,
        r#""protocol":"modbus_tcp","address":"127.0.0.1:502","signals":[{"name":"value","address":1.0,"unknown":1}]"#,
        r#""protocol":"scpi","connection":{"transport":"serial","device":"unused","baud":9.6e3,"unknown":1},"signals":[]"#,
        r#""protocol":"scpi","connection":{"transport":"tcp","address":"127.0.0.1:5025","baud":9.6e3},"signals":[]"#,
        r#""protocol":"scpi","connection":{"transport":"serial","device":"unused","address":"127.0.0.1:5025"},"signals":[]"#,
    ] {
        reject_both(&document(source));
    }
    reject_both(r#"{"version":1.0,"sources":[],"unknown":1}"#);
}

#[test]
fn exact_integral_notation_does_not_relax_protocol_semantic_bounds() {
    for source in [
        r#""protocol":"s7","address":"127.0.0.1:102","rack":8e0,"signals":[{"name":"value","db":1,"address":0}]"#,
        r#""protocol":"ads","address":"127.0.0.1:48898","target_net_id":"1.2.3.4.5.6","source_net_id":"6.5.4.3.2.1","signals":[{"name":"value","symbol":"MAIN.Bytes","data_type":"bytes","length":6.5537e4}]"#,
        r#""protocol":"pccc","address":"127.0.0.1:44818","max_read_bytes":2.25e2,"signals":[{"name":"value","address":"N7:0"}]"#,
        r#""protocol":"fins_tcp","address":"127.0.0.1:9600","source_network":1.28e2,"signals":[{"name":"value","address":0}]"#,
        r#""protocol":"ethernet_ip","address":"127.0.0.1:44818","signals":[{"name":"value","tag":"Counts","elements":2.57e2}]"#,
        r#""protocol":"mqtt","broker":"127.0.0.1:1883","topics":["values/#"],"qos":3e0"#,
        r#""protocol":"scpi","connection":{"transport":"tcp","address":"127.0.0.1:5025"},"signals":[{"name":"value","query":"READ?","max_bytes":1.048577e6}]"#,
        r#""protocol":"scpi","connection":{"transport":"serial","device":"unused","data_bits":9e0},"signals":[{"name":"value","query":"READ?"}]"#,
    ] {
        for config in both(&document(source)) {
            let result = match &config.sources[0].adapter {
                Adapter::S7(adapter) => s7::open(adapter),
                Adapter::Ads(adapter) => ads::open(adapter),
                Adapter::Pccc(adapter) => pccc::open(adapter),
                Adapter::FinsTcp(adapter) => fins::open(adapter),
                Adapter::EthernetIp(Selection::BuiltIn(adapter)) => ethernet_ip::open(adapter),
                Adapter::Mqtt(adapter) => mqtt::open(adapter),
                Adapter::Scpi(adapter) => scpi::open(adapter),
                other => panic!("unexpected adapter: {other:?}"),
            };
            let error = result
                .err()
                .expect("invalid protocol settings must fail before transport I/O");
            assert!(error.is::<Usage>(), "{source}: {error:#}");
        }
    }
}

#[test]
fn dataman_cip_result_integral_limits_survive_both_config_paths() {
    let input = document(
        r#""protocol":"cognex_dataman","address":"127.0.0.1:44818","profile":"cip_result","signal":"decode","max_result_bytes":8.164e3"#,
    );
    for config in both(&input) {
        let Adapter::CognexDataMan(adapter) = &config.sources[0].adapter else {
            panic!("DataMan protocol was not retained");
        };
        drop(ethernet_ip::dataman::open(adapter).unwrap());
    }
}

#[test]
fn native_subscription_and_all_helper_overrides_validate_before_open() {
    for protocol in ["helper", "opc_ua", "ethercat", "profinet", "ethernet_ip"] {
        let source = format!(r#""protocol":"{protocol}","executable":"""#);
        for config in both(&document(&source)) {
            assert!(config.validate().is_err(), "{protocol} empty executable");
        }
    }
    for extra in [
        r#", "mode":"subscription""#,
        r#", "mode":"read", "subscription":{}"#,
        r#", "mode":"subscription", "subscription":{"queue_size":1}"#,
    ] {
        let source = format!(
            r#""protocol":"opc_ua","endpoint":"opc.tcp://127.0.0.1:4840","nodes":[{{"name":"value","node_id":"i=1"}}]{extra}"#
        );
        for config in both(&document(&source)) {
            assert!(config.validate().is_err(), "{extra}");
        }
    }
}
