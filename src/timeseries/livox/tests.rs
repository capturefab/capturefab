use super::*;

fn config() -> Config {
    serde_json::from_value(json!({
        "listen":"127.0.0.1:56301", "source_ip":"127.0.0.1",
        "profile":"sdk2_mid360_v0", "stream":"points",
        "output":"raw.livox", "max_bytes":10_000_000, "min_free_bytes":0
    }))
    .unwrap()
}

fn stream(folder: &std::path::Path) -> Stream {
    let config = config();
    let format = config.format().unwrap();
    Stream {
        socket: Some(UdpSocket::bind("127.0.0.1:0").unwrap()),
        source_ip: config.source_ip,
        source_port: None,
        signal: config.signal.clone(),
        packets_per_group: config.packets_per_group,
        format,
        journal: Journal::new(
            folder.join("raw.livox"),
            &config,
            Context::read(None).unwrap(),
            format,
            crate::host::monotonic_ns().unwrap(),
        )
        .unwrap(),
        pending: None,
        observed: None,
        cutoff: None,
        terminal: VecDeque::new(),
        failed: false,
        finished: false,
        foreign_packets: 0,
        uncommitted_packets: 0,
    }
}

#[test]
fn explicit_generation_stream_and_multicast_are_validated_without_io() {
    let base = json!({
        "listen":"127.0.0.1:56301", "source_ip":"127.0.0.1",
        "profile":"sdk2_mid360_v0", "stream":"points",
        "output":"raw.livox", "max_bytes":10_000_000
    });
    for profile in [
        "sdk2_mid360_v0",
        "sdk2_mid360s_v0",
        "sdk2_mid360l_v0",
        "sdk2_hap_tx_v0",
        "sdk2_hap_t1_v0",
        "legacy_horizon_v5",
        "legacy_avia_v5",
    ] {
        for kind in ["points", "imu"] {
            let mut value = base.clone();
            value["profile"] = json!(profile);
            value["stream"] = json!(kind);
            let config: Config = serde_json::from_value(value).unwrap();
            let result = config.validate();
            if profile == "sdk2_hap_t1_v0" && kind == "imu" {
                assert_eq!(
                    crate::types::classify(&result.unwrap_err()).kind,
                    ErrorKind::Unsupported
                );
            } else {
                result.unwrap();
            }
        }
    }
    for (key, invalid) in [
        ("listen", json!("127.0.0.1:0")),
        ("listen", json!("239.1.2.3:56301")),
        ("source_ip", json!("0.0.0.0")),
        ("source_ip", json!("239.1.2.3")),
        ("source_port", json!(0)),
        ("max_bytes", json!(0)),
        ("packets_per_group", json!(0)),
        ("packets_per_group", json!(33)),
        ("output", json!("-")),
        ("output", json!("https://example.test/raw")),
        ("output", json!("{date}.livox")),
        ("signal", json!("")),
    ] {
        let mut value = base.clone();
        value[key] = invalid;
        let config: Config = serde_json::from_value(value).unwrap();
        assert_eq!(
            crate::types::classify(&config.validate().unwrap_err()).kind,
            ErrorKind::Usage
        );
    }
    let mut join = config();
    join.listen = "0.0.0.0:56301".parse().unwrap();
    join.multicast = Some(Multicast {
        group: "239.1.2.3".parse().unwrap(),
        interface: Ipv4Addr::LOCALHOST,
    });
    join.validate().unwrap();
    join.profile = "sdk2_mid360l_v0".into();
    assert_eq!(
        crate::types::classify(&join.validate().unwrap_err()).kind,
        ErrorKind::Unsupported
    );
    join.profile = "sdk2_mid360_v0".into();
    join.listen = "127.0.0.1:56301".parse().unwrap();
    assert_eq!(
        crate::types::classify(&join.validate().unwrap_err()).kind,
        ErrorKind::Usage
    );
    join.profile = "sdk2_avia2_v0".into();
    join.multicast = None;
    assert_eq!(
        crate::types::classify(&join.validate().unwrap_err()).kind,
        ErrorKind::Unsupported
    );
    let numeric: Config = serde_json::from_str(r#"{"listen":"127.0.0.1:56301","source_ip":"127.0.0.1","profile":"sdk2_mid360_v0","stream":"points","output":"raw.livox","max_bytes":1e7,"source_port":5.63e4,"packets_per_group":3.2e1}"#).unwrap();
    numeric.validate().unwrap();
    assert_eq!(numeric.source_port, Some(56_300));
    assert_eq!(numeric.packets_per_group, 32);
    let mut missing_stream = base;
    missing_stream.as_object_mut().unwrap().remove("stream");
    serde_json::from_value::<Config>(missing_stream).unwrap_err();
}

#[test]
fn expired_read_preserves_queued_datagram_and_cutoff_precedes_decoding() {
    let folder = tempfile::tempdir().unwrap();
    let mut source = stream(folder.path());
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&sender)
        .set_send_buffer_size(128 * 1024)
        .unwrap();
    let target = source.socket.as_ref().unwrap().local_addr().unwrap();
    sender
        .send_to(b"queued-without-read-budget", target)
        .unwrap();
    let mut raw = [0; 64];
    let socket = source.socket.as_ref().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(socket.peek_from(&mut raw).unwrap().0, 26);
    assert_eq!(source.read(Duration::ZERO).unwrap().len(), 0);
    let socket = source.socket.as_ref().unwrap();
    socket.set_nonblocking(true).unwrap();
    assert_eq!(socket.recv_from(&mut raw).unwrap().0, 26);
    assert_eq!(&raw[..26], b"queued-without-read-budget");
    socket.set_nonblocking(false).unwrap();
    source.set_acquisition_end(Some(0));
    sender.send_to(b"invalid-but-after-cutoff", target).unwrap();
    assert_eq!(source.read(Duration::from_millis(100)).unwrap().len(), 0);
    assert_eq!(source.uncommitted_packets, 0);
    assert_eq!(source.journal.groups, 0);
    assert_eq!(source.observed_ns(), None);
    source.finish(Duration::ZERO).unwrap();
    assert!(source.drain().unwrap().is_none());
    assert!(!folder.path().join("raw.livox").exists());
}

#[test]
fn foreign_datagram_full_lengths_are_bounded_and_retire_without_sending_commands() {
    let folder = tempfile::tempdir().unwrap();
    let mut source = stream(folder.path());
    let allowed = UdpSocket::bind("127.0.0.1:0").unwrap();
    source.source_port = Some(allowed.local_addr().unwrap().port());
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket2::SockRef::from(&sender)
        .set_send_buffer_size(128 * 1024)
        .unwrap();
    let socket = source.socket.as_ref().unwrap();
    socket2::SockRef::from(socket)
        .set_recv_buffer_size(256 * 1024)
        .unwrap();
    let target = socket.local_addr().unwrap();
    let raw = vec![0; MAX_FOREIGN_BYTES / 2 + 1];
    sender.send_to(&raw, target).unwrap();
    sender.send_to(&raw, target).unwrap();
    let error = source.read(Duration::from_secs(2)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert!(error.to_string().contains("foreign-source"));
    assert_eq!(source.foreign_packets, 2);
    assert_eq!(source.uncommitted_packets, 0);
    assert!(source.socket.is_none());
    assert!(!source.ready());
    assert_eq!(source.journal.groups, 0);
    assert!(
        source
            .read(Duration::ZERO)
            .unwrap_err()
            .to_string()
            .contains("retired")
    );
    source.finish(Duration::ZERO).unwrap();
    assert!(source.drain().unwrap().is_none());
    assert!(!folder.path().join("raw.livox").exists());
    sender.set_nonblocking(true).unwrap();
    assert_eq!(
        sender.recv_from(&mut [0; 64]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    allowed.set_nonblocking(true).unwrap();
    assert_eq!(
        allowed.recv_from(&mut [0; 64]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
