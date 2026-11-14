//! Direct BACnet/IP through the CLI, using Annex J and literal `ReadProperty` APDUs.

use super::{Home, assert_unpublished, support::cli::Run};
use serde_json::{Value, json};
use std::{
    fs,
    net::{SocketAddr, UdpSocket},
    thread,
    time::Duration,
};

const ANALOG: [u8; 17] = [
    0x81, 0x0a, 0, 17, 1, 4, 0, 5, 0, 12, 0x0c, 0, 0x80, 0, 17, 0x19, 85,
];
const ANALOG_ACK: [u8; 23] = [
    0x81, 0x0a, 0, 23, 1, 0, 0x30, 0, 12, 0x0c, 0, 0x80, 0, 17, 0x19, 85, 0x3e, 0x44, 0x42, 0xf7,
    0, 0, 0x3f,
];
const UNITS: [u8; 17] = [
    0x81, 0x0a, 0, 17, 1, 4, 0, 5, 1, 12, 0x0c, 0, 0x80, 0, 17, 0x19, 117,
];
const UNITS_ACK: [u8; 20] = [
    0x81, 0x0a, 0, 20, 1, 0, 0x30, 1, 12, 0x0c, 0, 0x80, 0, 17, 0x19, 117, 0x3e, 0x91, 47, 0x3f,
];
const FLAGS: [u8; 17] = [
    0x81, 0x0a, 0, 17, 1, 4, 0, 5, 2, 12, 0x0c, 0, 0x80, 0, 17, 0x19, 111,
];
const FLAGS_ACK: [u8; 21] = [
    0x81, 0x0a, 0, 21, 1, 0, 0x30, 2, 12, 0x0c, 0, 0x80, 0, 17, 0x19, 111, 0x3e, 0x82, 4, 0x50,
    0x3f,
];
const BINARY: [u8; 17] = [
    0x81, 0x0a, 0, 17, 1, 4, 0, 5, 3, 12, 0x0c, 1, 0x40, 0, 3, 0x19, 85,
];
const BINARY_ACK: [u8; 20] = [
    0x81, 0x0a, 0, 20, 1, 0, 0x30, 3, 12, 0x0c, 1, 0x40, 0, 3, 0x19, 85, 0x3e, 0x91, 1, 0x3f,
];
const STATE: [u8; 17] = [
    0x81, 0x0a, 0, 17, 1, 4, 0, 5, 4, 12, 0x0c, 3, 0x40, 0, 4, 0x19, 85,
];
const STATE_ACK: [u8; 21] = [
    0x81, 0x0a, 0, 21, 1, 0, 0x30, 4, 12, 0x0c, 3, 0x40, 0, 4, 0x19, 85, 0x3e, 0x22, 1, 2, 0x3f,
];

fn udp_socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
}

fn request(socket: &UdpSocket, expected: &[u8]) -> SocketAddr {
    let mut bytes = [0; 64];
    let (length, client) = socket.recv_from(&mut bytes).unwrap();
    assert_eq!(&bytes[..length], expected);
    client
}

fn reply(socket: &UdpSocket, client: SocketAddr, bytes: &[u8]) {
    assert_eq!(socket.send_to(bytes, client).unwrap(), bytes.len());
}

fn source(socket: &UdpSocket, signals: &Value) -> Value {
    json!({
        "id":"meter","protocol":"bacnet_ip","address":socket.local_addr().unwrap().to_string(),
        "every_ms":3_600_000,"timeout_ms":1000,"signals":signals
    })
}

fn record(home: &Home, source: &Value) -> Run {
    fs::write(
        home.path("sources.json"),
        json!({"version":1,"sources":[source]}).to_string(),
    )
    .unwrap();
    home.run(&[
        "--json",
        "data",
        "record",
        "--config",
        "sources.json",
        "--for",
        "1400ms",
        "-o",
        "run.jsonl",
    ])
}

fn analog() -> Value {
    json!({"name":"power","object_type":"analog_value","instance":17})
}

#[test]
fn reads_meter_values_and_typed_metadata_on_one_explicit_local_port() {
    let home = Home::new("bacnet-values");
    let socket = udp_socket();
    let mut power = analog();
    power["units"] = json!(true);
    power["status_flags"] = json!(true);
    let mut config = source(
        &socket,
        &json!([
            power,
            {"name":"running","object_type":"binary_value","instance":3},
            {"name":"mode","object_type":"multi_state_input","instance":4}
        ]),
    );
    // A fixture-selected port permits the same run on shared CI hosts.
    let bind = udp_socket().local_addr().unwrap();
    config["bind"] = json!(bind.to_string());
    let server = thread::spawn(move || {
        let client = request(&socket, &ANALOG);
        assert_eq!(client, bind);
        let mut foreign = ANALOG_ACK;
        foreign[19] = 0;
        reply(&udp_socket(), client, &foreign);
        let mut old = foreign;
        old[7] = 255;
        reply(&socket, client, &old);
        reply(&socket, client, &ANALOG_ACK);
        for (query, value) in [
            (&UNITS[..], &UNITS_ACK[..]),
            (&FLAGS[..], &FLAGS_ACK[..]),
            (&BINARY[..], &BINARY_ACK[..]),
            (&STATE[..], &STATE_ACK[..]),
        ] {
            assert_eq!(request(&socket, query), client);
            reply(&socket, client, value);
        }
    });
    assert_eq!(record(&home, &config).success()["samples"], 3);
    server.join().unwrap();
    let rows = fs::read_to_string(home.path("run.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows[0]["sources"][0]["protocol"], "bacnet_ip");
    assert_eq!(
        rows[1]["value"],
        json!({"value":123.5,"units":47,"status_flags":{
            "in_alarm":false,"fault":true,"overridden":false,"out_of_service":true
        }})
    );
    assert_eq!(rows[2]["value"], true);
    assert_eq!(rows[3]["value"], 258);
    for format in ["csv", "jsonl"] {
        assert_eq!(
            home.ok(&[
                "data",
                "export",
                "run.jsonl",
                "-o",
                format,
                "--format",
                format,
            ])["samples"],
            3
        );
        let exported = fs::read_to_string(home.path(format)).unwrap();
        assert!(exported.contains("123.5"));
        assert!(exported.contains("status_flags"));
    }
}

#[test]
fn rejects_mismatched_or_malformed_property_replies_atomically() {
    let error_ack = [0x81, 0x0a, 0, 13, 1, 0, 0x50, 1, 12, 0x91, 2, 0x91, 32];
    let reject_ack = [0x81, 0x0a, 0, 9, 1, 0, 0x60, 1, 9];
    let abort_ack = [0x81, 0x0a, 0, 9, 1, 0, 0x71, 1, 4];
    for mode in [
        "length",
        "object",
        "property",
        "service",
        "segmented",
        "tag",
        "oversized",
        "error",
        "reject",
        "abort",
    ] {
        let home = Home::new("bacnet-invalid");
        let socket = udp_socket();
        let mut signal = analog();
        signal["units"] = json!(true);
        let config = source(&socket, &json!([signal]));
        let server = thread::spawn(move || {
            let client = request(&socket, &ANALOG);
            reply(&socket, client, &ANALOG_ACK);
            assert_eq!(request(&socket, &UNITS), client);
            let mut frame = UNITS_ACK.to_vec();
            match mode {
                "length" => frame[3] -= 1,
                "object" => frame[13] ^= 1,
                "property" => frame[15] = 111,
                "service" => frame[8] = 14,
                "segmented" => frame[6] = 0x38,
                "tag" => frame[17] = 0x21,
                "oversized" => frame.resize(8192, 0),
                "error" => frame = error_ack.to_vec(),
                "reject" => frame = reject_ack.to_vec(),
                "abort" => frame = abort_ack.to_vec(),
                _ => unreachable!(),
            }
            reply(&socket, client, &frame);
        });
        let error = record(&home, &config).failure(1);
        assert_eq!(error["code"], "failed", "{mode}: {error}");
        match mode {
            "error" => {
                assert_eq!(error["details"]["bacnet_error_class"], 2);
                assert_eq!(error["details"]["bacnet_error_code"], 32);
            }
            "reject" => assert_eq!(error["details"]["bacnet_reject_reason"], 9),
            "abort" => assert_eq!(error["details"]["bacnet_abort_reason"], 4),
            _ => {}
        }
        server.join().unwrap();
        assert_unpublished(&home);
    }
}

#[test]
fn status_and_value_share_a_poll_deadline_even_with_stale_responses() {
    let home = Home::new("bacnet-deadline");
    let socket = udp_socket();
    let mut signal = analog();
    signal["units"] = json!(true);
    let mut config = source(&socket, &json!([signal]));
    config["timeout_ms"] = json!(320);
    let server = thread::spawn(move || {
        let client = request(&socket, &ANALOG);
        thread::sleep(Duration::from_millis(160));
        reply(&socket, client, &ANALOG_ACK);
        assert_eq!(request(&socket, &UNITS), client);
        thread::sleep(Duration::from_millis(80));
        reply(&socket, client, &ANALOG_ACK);
        thread::sleep(Duration::from_millis(120));
        let _ = socket.send_to(&UNITS_ACK, client);
    });
    assert_eq!(record(&home, &config).failure(4)["code"], "timeout");
    server.join().unwrap();
    assert_unpublished(&home);
}

#[test]
fn invoke_wrap_rotates_ephemeral_ports_or_guards_a_fixed_port() {
    let unknown_object = [0x81, 0x0a, 0, 13, 1, 0, 0x50, 0, 12, 0x91, 1, 0x91, 31];
    for fixed in [false, true] {
        let home = Home::new("bacnet-invoke-wrap");
        let socket = udp_socket();
        let mut config = source(
            &socket,
            &json!((0..=u8::MAX)
                .map(|invoke| json!({"name":format!("power_{invoke}"),"object_type":"analog_value","instance":17}))
                .collect::<Vec<_>>()),
        );
        config["every_ms"] = json!(200);
        config["reply_horizon_ms"] = json!(5000);
        let binding = fixed.then(|| {
            let address = udp_socket().local_addr().unwrap();
            config["bind"] = json!(address.to_string());
            address
        });
        let server = thread::spawn(move || {
            let first = request(&socket, &ANALOG);
            if let Some(address) = binding {
                assert_eq!(first, address);
            }
            reply(&socket, first, &ANALOG_ACK);
            for invoke in 1..=u8::MAX {
                let mut query = ANALOG;
                query[8] = invoke;
                assert_eq!(request(&socket, &query), first);
                let mut ack = ANALOG_ACK;
                ack[7] = invoke;
                reply(&socket, first, &ack);
            }
            reply(&socket, first, &unknown_object);
            if fixed {
                socket
                    .set_read_timeout(Some(Duration::from_millis(450)))
                    .unwrap();
                let error = socket.recv_from(&mut [0; 32]).unwrap_err();
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ));
            } else {
                let second = request(&socket, &ANALOG);
                assert_ne!(first, second);
                reply(&socket, second, &unknown_object);
            }
        });
        let error = record(&home, &config).failure(if fixed { 4 } else { 1 });
        if fixed {
            assert_eq!(error["details"]["invoke_id"], 0);
            assert_eq!(error["details"]["reply_horizon_ms"], 5000);
        } else {
            assert_eq!(error["details"]["bacnet_error_code"], 31);
        }
        server.join().unwrap();
        let rows = fs::read_to_string(home.path("run.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 258);
        assert_eq!(rows.matches(r#""value":123.5"#).count(), 256);
    }
}
