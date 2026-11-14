use super::*;
use sha2::{Digest as _, Sha256};

fn literal(name: &str) -> Vec<u8> {
    let document: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/yaskawa_hses/packets.json"
    ))
    .unwrap();
    let entry = &document["packets"][name];
    let (pairs, remainder) = entry["hex"].as_str().unwrap().as_bytes().as_chunks::<2>();
    assert_eq!(remainder, b"");
    let bytes: Vec<_> = pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(
        bytes.len(),
        usize::try_from(entry["bytes"].as_u64().unwrap()).unwrap()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        entry["sha256"].as_str().unwrap()
    );
    bytes
}
fn selection(resource: Resource, index: u16) -> Selection {
    Selection {
        signal: "value".into(),
        resource,
        index: (!matches!(resource, Resource::Status | Resource::BasePosition)).then_some(index),
        group: (resource == Resource::BasePosition).then_some(1),
    }
}
fn config() -> Config {
    serde_json::from_value(json!({"profile":"yrc1000_hw1483358_rev12","address":"127.0.0.1:10040","reply_horizon_ms":1000,"reads":[{"signal":"state","resource":"status"}]})).unwrap()
}
fn local(socket: &UdpSocket) -> SocketAddrV4 {
    let SocketAddr::V4(address) = socket.local_addr().unwrap() else {
        panic!("IPv4 fixture required")
    };
    address
}
fn server() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
}
fn client(server: &UdpSocket, fixed: bool) -> Client {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(local(server)).unwrap();
    let bind = if fixed {
        local(&socket)
    } else {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)
    };
    Client {
        profile: Profile::Yrc1000Hw1483358Rev12,
        reads: vec![selection(Resource::Status, 0)],
        datagrams: Datagrams {
            address: local(server),
            bind,
            horizon: Duration::from_secs(1),
            socket: Some(socket),
            id: 0,
            binding: if fixed {
                Binding::Fixed(Box::new([None; 256]))
            } else {
                Binding::Ephemeral {
                    rotate: false,
                    retired: BTreeMap::new(),
                }
            },
        },
        failure: None,
        observed: None,
        ready: false,
    }
}

#[test]
fn published_request_reply_and_full_string_command_keep_the_frozen_bytes() {
    for (resource, index, id, name) in [
        (Resource::Status, 0, 0, "status_request"),
        (Resource::Byte, 7, 44, "byte7_request"),
        (Resource::BasePosition, 0, 43, "base_position_request"),
        (Resource::String32, 11, 48, "string32_request"),
    ] {
        assert_eq!(
            selection(resource, index)
                .request(id)
                .unwrap()
                .encode()
                .as_slice(),
            literal(name)
        );
    }
    let raw = literal("status_response");
    let reply = packet::reply(selection(Resource::Status, 0).request(0).unwrap(), &raw).unwrap();
    let value = packet::value(Resource::Status, 1, reply.body).unwrap();
    assert_eq!(value["words_raw"], json!([34, 0]));
    assert_eq!(value["flags"]["one_cycle"], true);
    assert_eq!(value["flags"]["teach"], true);
    assert_eq!(value["unqualified_bits"], json!([0, 0]));
}

#[test]
fn native_widths_signed_values_real_word_and_all_string_bytes_survive() {
    for (resource, name, expected) in [
        (Resource::Byte, "byte7_response", json!(165)),
        (Resource::Integer, "integer8_response", json!(-12345)),
        (
            Resource::DoubleInteger,
            "double9_response",
            json!(-123_456_789),
        ),
    ] {
        let raw = literal(name);
        let value = packet::value(resource, 7, &raw[32..]).unwrap();
        assert_eq!(value["value"], expected);
        assert_eq!(value["allocation_origin"], "configured_unverified");
    }
    let raw = literal("real10_raw_response");
    let value = packet::value(Resource::RealRaw, 10, &raw[32..]).unwrap();
    assert_eq!(value["word_raw"], 0x3fc0_0000_u32);
    assert_eq!(value["interpretation"], "unqualified_float_wire_encoding");
    assert!(value.get("value").is_none());
    let mut raw = literal("string32_response");
    raw[32] = 255;
    let value = packet::value(Resource::String32, 11, &raw[32..]).unwrap();
    let bytes = value["bytes_raw"].as_array().unwrap();
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0], 255);
    assert_eq!(bytes[2], 0);
    assert_eq!(bytes[16], b'q');
    assert_eq!(bytes[31], b'6');
    assert_eq!(value["encoding"], "unqualified");
    packet::value(Resource::Byte, 7, &[0, 1, 2, 3]).unwrap_err();
    packet::value(Resource::Integer, 8, &[0, 1, 2, 3]).unwrap_err();
    packet::value(Resource::String32, 11, &[0; 16]).unwrap_err();
}

#[test]
fn coordinates_have_exact_integer_units_only_for_the_qualified_base_type() {
    let raw = literal("base_position_response");
    let value = packet::value(Resource::BasePosition, 101, &raw[32..]).unwrap();
    assert_eq!(value["position_um"], json!([-100_000, 250_000, 0]));
    assert_eq!(
        value["orientation_1e4_degree"],
        json!([-900_000, 450_000, 1_800_000])
    );
    assert_eq!(value["remaining_slots_raw"], json!([7, -8]));
    assert_eq!(value["figure_raw"], 0x1234);
    let mut body = raw[32..].to_vec();
    body[..4].copy_from_slice(&18_u32.to_le_bytes());
    let unknown = packet::value(Resource::BasePosition, 101, &body).unwrap();
    assert_eq!(unknown["coordinate_type_raw"], 18);
    assert_eq!(unknown["units_status"], "unqualified_returned_type");
    assert!(unknown.get("position_um").is_none());
    assert_eq!(unknown["coordinates_raw"], value["coordinates_raw"]);
}

#[test]
fn correlated_header_extents_ids_services_blocks_and_added_words_are_strict() {
    let request = selection(Resource::Status, 0).request(0).unwrap();
    let raw = literal("status_response");
    for length in 0..32 {
        packet::reply(request, &raw[..length]).unwrap_err();
    }
    for offset in [0, 4, 6, 8, 9, 10, 11, 12, 16, 24] {
        let mut bad = raw.clone();
        bad[offset] ^= 1;
        packet::reply(request, &bad).unwrap_err();
    }
    let mut bad = raw.clone();
    bad[26] = 3;
    packet::reply(request, &bad).unwrap_err();
    let mut oversized = raw.clone();
    oversized.resize(512, 0);
    oversized[6..8].copy_from_slice(&480_u16.to_le_bytes());
    packet::reply(request, &oversized).unwrap_err();
    let mut padded = raw;
    padded[26] = 1;
    padded[30..32].copy_from_slice(&0xfedc_u16.to_le_bytes());
    let parsed = packet::reply(request, &padded).unwrap();
    assert_eq!(parsed.added, 0);
    assert_eq!(parsed.added_bytes, [0, 0, 0xdc, 0xfe]);
    let mut error = literal("attribute_error_response");
    let request = selection(Resource::Byte, 7).request(49).unwrap();
    let failure = packet::reply(request, &error).unwrap_err();
    let classified = crate::types::classify(&failure);
    assert_eq!(classified.kind, ErrorKind::OperationFailed);
    assert_eq!(classified.details.unwrap()["hses_added_status_raw"], 0xa002);
    error[26] = 2;
    error[28..32].copy_from_slice(&0x1234_5678_u32.to_le_bytes());
    assert_eq!(
        crate::types::classify(&packet::reply(request, &error).unwrap_err())
            .details
            .unwrap()["hses_added_status_raw"],
        0x1234_5678_u32
    );
}

#[test]
fn source_configuration_rejects_ambiguous_resources_and_short_operator_horizons() {
    config().validate_for(1000).unwrap();
    config().validate_for(1001).unwrap_err();
    for address in [
        "0.0.0.0:10040",
        "224.0.0.1:10040",
        "255.255.255.255:10040",
        "127.0.0.1:0",
    ] {
        let mut cfg = config();
        cfg.address = address.parse().unwrap();
        cfg.validate().unwrap_err();
    }
    for horizon in [0, 30_001] {
        let mut cfg = config();
        cfg.reply_horizon_ms = horizon;
        cfg.validate().unwrap_err();
    }
    let mut cfg = config();
    cfg.reads.push(cfg.reads[0].clone());
    cfg.validate().unwrap_err();
    for resource in [
        Resource::Byte,
        Resource::Integer,
        Resource::DoubleInteger,
        Resource::RealRaw,
        Resource::String32,
    ] {
        let mut read = selection(resource, u16::MAX);
        read.instance().unwrap();
        read.index = None;
        read.instance().unwrap_err();
    }
    for group in [0, 9] {
        let mut read = selection(Resource::BasePosition, 0);
        read.group = Some(group);
        read.instance().unwrap_err();
    }
    let mut read = selection(Resource::Status, 0);
    read.index = Some(1);
    read.instance().unwrap_err();
    let mut cfg = config();
    cfg.reads = vec![selection(Resource::Status, 0); 33];
    cfg.validate().unwrap_err();
}

#[test]
fn fixed_id_quarantine_failure_retires_the_source_before_any_send() {
    let server = server();
    let mut client = client(&server, true);
    let Binding::Fixed(available) = &mut client.datagrams.binding else {
        panic!("fixed fixture")
    };
    available[0] = Some(Instant::now() + Duration::from_secs(1));
    let error = client.read(Duration::from_millis(10)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert!(client.datagrams.socket.is_none());
    let again = client.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&again).kind, ErrorKind::Timeout);
    server.set_nonblocking(true).unwrap();
    assert_eq!(
        server.recv(&mut [0; 32]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(client.observed_ns().is_none());
    assert!(!client.ready());
}

#[test]
fn expired_fixed_id_drains_queued_stale_reply_before_sending_the_next_request() {
    let server = server();
    let mut client = client(&server, true);
    client.datagrams.id = 9;
    let Binding::Fixed(available) = &mut client.datagrams.binding else {
        panic!("fixed fixture")
    };
    available[9] = Some(Instant::now());
    let socket = client.datagrams.socket.as_ref().unwrap();
    let mut stale = literal("status_response");
    stale[11] = 9;
    server.send_to(&stale, local(socket)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    socket.peek(&mut [0; 40]).unwrap();
    let handle = std::thread::spawn(move || {
        let mut request = [0; 32];
        let (bytes, peer) = server.recv_from(&mut request).unwrap();
        assert_eq!(bytes, 32);
        assert_eq!(request[11], 9);
        let mut fresh = literal("status_response");
        fresh[11] = 9;
        fresh[32..36].copy_from_slice(&12_u32.to_le_bytes());
        server.send_to(&fresh, peer).unwrap();
    });
    let mut work = Work::default();
    let received = client
        .datagrams
        .read(
            &selection(Resource::Status, 0),
            Instant::now() + Duration::from_secs(1),
            &mut work,
        )
        .unwrap_or_else(|error| panic!("{error:#}"));
    handle.join().unwrap();
    assert_eq!(received.value["words_raw"], json!([12, 0]));
    assert_eq!(work.received, 2);
    assert_eq!(work.ignored, 1);
    assert_eq!(work.raw, 80);
}

#[test]
fn ephemeral_id_rotation_excludes_previous_ports_and_bounds_the_epoch_history() {
    let server = server();
    let mut client = client(&server, false);
    let old = port(client.datagrams.socket.as_ref().unwrap()).unwrap();
    let Binding::Ephemeral { rotate, .. } = &mut client.datagrams.binding else {
        panic!("ephemeral fixture")
    };
    *rotate = true;
    client
        .datagrams
        .prepare(Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert_ne!(
        port(client.datagrams.socket.as_ref().unwrap()).unwrap(),
        old
    );
    assert_eq!(client.datagrams.id, 0);
    let Binding::Ephemeral { rotate, retired } = &mut client.datagrams.binding else {
        panic!("ephemeral fixture")
    };
    assert!(!*rotate);
    assert!(retired.contains_key(&old));
    *rotate = true;
    retired.clear();
    for p in 1..=1024 {
        retired.insert(p, Instant::now() + Duration::from_secs(1));
    }
    let error = client
        .datagrams
        .prepare(Instant::now() + Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
}

#[test]
fn oversized_foreign_id_datagram_is_fully_accounted_before_the_valid_reply() {
    let server = server();
    let mut client = client(&server, false);
    let socket = client.datagrams.socket.as_ref().unwrap();
    let mut foreign = vec![0; 4096];
    foreign[11] = 7;
    server.send_to(&foreign, local(socket)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    socket.peek(&mut [0; 4096]).unwrap();
    let handle = std::thread::spawn(move || {
        let (bytes, peer) = server.recv_from(&mut [0; 32]).unwrap();
        assert_eq!(bytes, 32);
        server.send_to(&literal("status_response"), peer).unwrap();
    });
    let mut work = Work::default();
    let response = client
        .datagrams
        .read(
            &selection(Resource::Status, 0),
            Instant::now() + Duration::from_secs(1),
            &mut work,
        )
        .unwrap_or_else(|error| panic!("{error:#}"));
    handle.join().unwrap();
    assert_eq!(response.value["words_raw"], json!([34, 0]));
    assert_eq!(work.received, 2);
    assert_eq!(work.ignored, 1);
    assert_eq!(work.raw, 4136);
}
