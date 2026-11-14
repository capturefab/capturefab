use super::*;
use crate::types::classify;
use std::net::UdpSocket;

const LEADER: &str =
    "0000002a010000000000000100000000000004d2010800010000000400000002000000000000000000000000";
const DATA: &str = "0000002a030000011021324354657687";
const TRAILER: &str = "0000002a020000020000000100000002";
const PIXELS: [u8; 8] = [0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87];
fn raw(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn route() -> Route {
    Route {
        interface: Interface {
            name: "fixture".into(),
            index: 7,
            address: Ipv4Addr::LOCALHOST,
            netmask: Ipv4Addr::new(255, 0, 0, 0),
        },
        group: Ipv4Addr::new(239, 255, 42, 19),
        port: 49154,
        source: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 49155),
        scps: 1500,
        payload: 8,
        big_endian: false,
        mono16_big_endian: false,
        frequency: 0,
    }
}
#[test]
fn selector_is_closed_canonical_and_cannot_be_a_media_or_controller_alias() {
    let a = MonitorSpec::parse("gige-monitor://192.0.2.20?mac=02AABBCCDDEE&interface=192.0.2.10")
        .unwrap();
    assert_eq!(
        a.canonical(),
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=02aabbccddee"
    );
    assert_eq!(a.physical_id(), "gige:02aabbccddee");
    assert_eq!(MonitorSpec::parse(&a.canonical()).unwrap(), a);
    assert!(is_selector("gige-monitor:malformed"));
    for bad in [
        "gige:192.0.2.20",
        "gige-monitor://192.0.2.20",
        "gige-monitor://host?interface=192.0.2.10&mac=020000000001",
        "gige-monitor://192.0.2.20:3956?interface=192.0.2.10&mac=020000000001",
        "gige-monitor://u@192.0.2.20?interface=192.0.2.10&mac=020000000001",
        "gige-monitor://192.0.2.20/?interface=192.0.2.10&mac=020000000001",
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=020000000001&mac=020000000001",
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=000000000000",
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=010000000001",
        "gige-monitor://239.1.1.1?interface=192.0.2.10&mac=020000000001",
        "gige-monitor://192.0.2.20?interface=0.0.0.0&mac=020000000001",
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=020000000001#x",
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=02%000000001",
        "gige-monitor://192.0.2.20?interface=192.0.2.10&mac=020000000001&group=239.1.1.1",
    ] {
        assert_eq!(
            classify(&MonitorSpec::parse(bad).unwrap_err()).kind,
            ErrorKind::Usage,
            "{bad}"
        );
    }
}
#[test]
fn exact_peer_destination_and_interface_share_a_full_byte_foreign_budget() {
    let route = route();
    let selected = receive::Datagram {
        bytes: 65_507,
        source: route.source,
        destination: route.group,
        interface_index: 7,
    };
    let mut work = ForeignWork::default();
    assert!(work.selected(selected, &route).unwrap());
    assert_eq!((work.packets, work.bytes), (0, 0));
    for packet in [
        receive::Datagram {
            source: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 49156),
            ..selected
        },
        receive::Datagram {
            destination: Ipv4Addr::new(239, 255, 42, 20),
            ..selected
        },
        receive::Datagram {
            interface_index: 8,
            ..selected
        },
    ] {
        let mut work = ForeignWork::default();
        assert!(!work.selected(packet, &route).unwrap());
        assert_eq!(work.bytes, 65_507);
        let error = work
            .selected(
                receive::Datagram {
                    bytes: 30,
                    ..packet
                },
                &route,
            )
            .unwrap_err();
        assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    }
    let mut work = ForeignWork::default();
    let zero = receive::Datagram {
        bytes: 0,
        interface_index: 8,
        ..selected
    };
    for _ in 0..128 {
        assert!(!work.selected(zero, &route).unwrap());
    }
    assert_eq!(
        classify(&work.selected(zero, &route).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
}
fn completed() -> Frame {
    let mut assembler = assembler(&route(), false, None);
    let mut frame = None;
    for text in [DATA, LEADER, DATA, TRAILER] {
        let bytes = raw(text);
        let parsed = super::super::parse_gvsp(&bytes, crate::copies::Site::GvspAssemble).unwrap();
        if let Some(done) = assembler.take(parsed, Instant::now()).unwrap() {
            frame = Some(done);
        }
    }
    frame.unwrap()
}
#[test]
fn independent_block42_survives_reorder_duplicate_and_postassembly_expiry() {
    let frame = completed();
    assert_eq!(
        (frame.id, frame.width, frame.height, frame.pixel_format),
        (42, 4, 2, crate::types::MONO8)
    );
    assert_eq!(frame.data.to_vec(), PIXELS);
    assert_eq!(frame.device_timestamp.unwrap().ticks, 1234);
    assert!(frame.device_timestamp.unwrap().frequency_hz.is_none());
    assert!(frame.device_ts_ns.is_none());
    let mut ready = std::collections::VecDeque::new();
    let error = publish(frame, &mut ready, Instant::now()).unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert_eq!(ready.len(), 1);
    let retained = ready.pop_front().unwrap();
    assert_eq!(retained.data.to_vec(), PIXELS);
    assert_eq!(retained.id, 42);
    assert_eq!(retained.device_timestamp.unwrap().ticks, 1234);
}
#[test]
fn actual_read_only_control_rejects_writes_and_expired_reads_without_sending() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    server
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.connect(server.local_addr().unwrap()).unwrap();
    let peer = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let until = Instant::now() + Duration::from_secs(2);
        let mut quiet = None;
        while Instant::now() < until && quiet.is_none_or(|end| Instant::now() < end) {
            let mut bytes = [0_u8; 1024];
            let (count, from) = match server.recv_from(&mut bytes) {
                Ok(value) => value,
                Err(e) if super::super::is_receive_timeout(&e) => continue,
                Err(e) => panic!("fixture receive: {e}"),
            };
            requests.push(bytes[..count].to_vec());
            let response = match requests.len() {
                1 => "000000810004000112345678",
                2 => "000000850008000200000100a1b2c3d4",
                _ => {
                    quiet = Some(Instant::now() + Duration::from_millis(100));
                    continue;
                }
            };
            if requests.len() == 1 {
                server
                    .send_to(&raw("0000008100047fff00000000"), from)
                    .unwrap();
            }
            server.send_to(&raw(response), from).unwrap();
            if requests.len() == 2 {
                quiet = Some(Instant::now() + Duration::from_millis(100));
            }
        }
        requests
    });
    let mut control =
        ReadControl::test_socket(client, Instant::now() + Duration::from_secs(1)).unwrap();
    let register = control.register(0x934, control.deadline());
    let memory = control.memory(0x101, 2, control.deadline());
    let denied = control.write_memory(0xa00, &2_u32.to_be_bytes());
    let expired = control.read_memory_until(0, 4, Instant::now());
    let requests = peer.join().unwrap();
    assert_eq!(register.unwrap(), 0x1234_5678);
    assert_eq!(memory.unwrap(), [0xb2, 0xc3]);
    assert_eq!(classify(&denied.unwrap_err()).kind, ErrorKind::Unsupported);
    assert_eq!(classify(&expired.unwrap_err()).kind, ErrorKind::Timeout);
    assert_eq!(
        requests,
        [
            raw("420100800004000100000934"),
            raw("42010084000800020000010000000004")
        ]
    );
}

#[test]
fn native_cached_cause_survives_arc_standard_sources_and_preserves_typed_metadata() {
    let Err(parse_error) = super::super::parse_gvsp(&[0, 0, 0], crate::copies::Site::GvspAssemble)
    else {
        panic!("selected short GVSP header unexpectedly parsed");
    };
    let malformed = decode_error(parse_error);
    assert_eq!(classify(&malformed).kind, ErrorKind::Incomplete);
    let typed = Categorized::new(ErrorKind::Unsupported, "first unsupported native layout")
        .hint("keep the original native payload")
        .details(json!({"native_layout":37}));
    let non_object = Categorized::new(ErrorKind::Unavailable, "first port failure")
        .hint("keep the original monitor epoch stopped")
        .details(json!([17, 23]));
    for original in [
        malformed,
        decode_error(typed.into()),
        decode_error(non_object.into()),
    ] {
        let expected = classify(&original);
        let message = format!("{original:#}");
        // Exactly the factory called by Monitor::remember, and exactly the
        // native Arc/SharedFailure path returned by Monitor::failure.
        let stored = retired_cause(original);
        assert_eq!(stored.to_string(), message);
        for _ in 0..2 {
            let shared: anyhow::Error = SharedFailure(stored.clone()).into();
            let classified = classify(&shared);
            assert_eq!(classified.kind, expected.kind);
            assert_eq!(classified.hint, expected.hint);
            let details = classified.details.unwrap();
            assert_eq!(details["monitor_connection"]["state"], "retired");
            if let Some(original) = &expected.details {
                if let Some(object) = original.as_object() {
                    for (key, value) in object {
                        assert_eq!(&details[key], value);
                    }
                } else {
                    assert_eq!(&details["cause"], original);
                }
            }
            assert!(format!("{shared:#}").contains(&message));
            let wire = crate::session::protocol::wire_error(&shared);
            assert_eq!(wire["code"], expected.kind.code().0);
            assert!(crate::engine::retirement::wire_is_retired(&wire));
        }
    }
}
