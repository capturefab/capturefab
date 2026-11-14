use super::*;
use sha2::{Digest as _, Sha256};
use std::path::Path;

fn config() -> Config {
    serde_json::from_value(json!({"listen":"127.0.0.1:56301","source_ip":"127.0.0.1",
        "profile":"multiscan100","stream":"points","output_directory":"sick-output",
        "max_bytes":10_000_000,"min_free_bytes":0}))
    .unwrap()
}
fn receiver(directory: &Path) -> Receiver {
    configured_receiver(directory, &config())
}
fn configured_receiver(directory: &Path, settings: &Config) -> Receiver {
    Receiver {
        socket: Some(UdpSocket::bind("127.0.0.1:0").unwrap()),
        source_ip: Ipv4Addr::LOCALHOST,
        source_port: None,
        signal: settings.signal.clone(),
        packets_per_group: 32,
        stream: settings.stream,
        profile: settings.profile,
        journal: Journal::new(
            directory.to_path_buf(),
            settings,
            Context {
                raw: b"{\"original\":true}".to_vec(),
                supplied: true,
            },
            crate::host::monotonic_ns().unwrap(),
        )
        .unwrap(),
        pending: None,
        drain_pending_after_failure: false,
        observed: None,
        cutoff: None,
        terminal: VecDeque::new(),
        failed: None,
        finished: false,
        foreign_packets: 0,
        other_stream_packets: 0,
        uncommitted_packets: 0,
    }
}
fn packet(received: u64, padding: usize) -> Received {
    Received {
        raw: vec![u8::try_from(received).unwrap(); 36],
        value: json!({"kind":"fixture","padding":"x".repeat(padding)}),
        received,
        source: "127.0.0.1:7502".parse().unwrap(),
    }
}
fn raw_empty() -> Vec<u8> {
    // Independent frozen empty_modules_v3 literal (4773e8ee), not a product encoder.
    let hex = b"02020202010000000100000000000000080706050403020103000000000000006b934408";
    let (pairs, tail) = hex.as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect()
}
fn send(receiver: &Receiver, raw: &[u8]) {
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        peer.send_to(raw, receiver.socket.as_ref().unwrap().local_addr().unwrap())
            .unwrap(),
        raw.len()
    );
}
#[test]
fn explicit_profiles_streams_and_limits_are_pure_preflight() {
    let base = json!({"listen":"127.0.0.1:56301","source_ip":"127.0.0.1",
        "profile":"multiscan100","stream":"points","output_directory":"sick-output","max_bytes":10_000_000});
    for profile in ["multiscan100", "picoscan100", "picoscan150"] {
        for stream in ["points", "imu"] {
            let mut value = base.clone();
            value["profile"] = json!(profile);
            value["stream"] = json!(stream);
            serde_json::from_value::<Config>(value)
                .unwrap()
                .validate()
                .unwrap();
        }
    }
    for (key, value) in [
        ("listen", json!("127.0.0.1:0")),
        ("listen", json!("239.1.2.3:56301")),
        ("source_ip", json!("0.0.0.0")),
        ("source_ip", json!("239.1.2.3")),
        ("source_port", json!(0)),
        ("max_bytes", json!(82_126)),
        ("packets_per_group", json!(0)),
        ("packets_per_group", json!(33)),
        ("output_directory", json!("-")),
        ("output_directory", json!("https://example.test/raw")),
        ("output_directory", json!("{date}")),
        ("signal", json!("")),
    ] {
        let mut invalid = base.clone();
        invalid[key] = value;
        let settings: Config = serde_json::from_value(invalid).unwrap();
        assert_eq!(
            crate::types::classify(&settings.validate().unwrap_err()).kind,
            ErrorKind::Usage
        );
    }
    for (key, value) in [
        ("profile", json!("multiscan200")),
        ("stream", json!("ambient")),
        ("source_port", json!(65_536)),
    ] {
        let mut invalid = base.clone();
        invalid[key] = value;
        let _error = serde_json::from_value::<Config>(invalid).unwrap_err();
    }
    let mut exact = config();
    exact.max_bytes = 82_127;
    exact.validate().unwrap();
    exact.stream = Stream::Imu;
    assert_eq!(
        crate::types::classify(&exact.validate().unwrap_err()).kind,
        ErrorKind::Usage
    );
    exact.max_bytes = 82_155;
    exact.validate().unwrap();
}
#[test]
fn dynamic_json_prefix_pending_cutoff_and_archive_readback_keep_original_clocks() {
    let folder = tempfile::tempdir().unwrap();
    let directory = folder.path().join("output");
    let mut source = receiver(&directory);
    let mut group = Group::new();
    assert!(
        group
            .offer(
                packet(1, 600_000),
                &source.journal,
                &source.signal,
                32,
                None
            )
            .unwrap()
            .is_none()
    );
    let before = group.raw.clone();
    source.pending = group
        .offer(
            packet(2, 600_000),
            &source.journal,
            &source.signal,
            32,
            None,
        )
        .unwrap();
    assert_eq!(group.values.len(), 1);
    assert_eq!(group.raw, before);
    let first = source.finish_group(group, None).unwrap();
    assert_eq!(first[0].value["packet_count"], 1);
    source.set_acquisition_end(Some(2));
    send(
        &source,
        b"queued malformed data must not be received while offering pending",
    );
    let second = source.read(Duration::ZERO).unwrap();
    assert_eq!(second[0].value["first_received_monotonic_ns"], 2);
    assert_eq!(second[0].value["datagrams"][0]["received_monotonic_ns"], 2);
    assert_eq!(second[0].value["datagrams"][0]["host_packet_index"], 2);
    assert_eq!(source.observed_ns(), Some(2));
    source.finish(Duration::from_secs(1)).unwrap();
    let raw = std::fs::read(directory.join("datagrams.sickcompact")).unwrap();
    assert_eq!(&raw[..8], b"CFSCMP01");
    assert_eq!(u16::from_le_bytes(raw[20..22].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(raw[72..74].try_into().unwrap()), 1);
    assert_eq!(&raw[80..97], b"{\"original\":true}");
    for reading in [&first[0], &second[0]] {
        let offset = usize::try_from(reading.value["offset"].as_u64().unwrap()).unwrap();
        let length = usize::try_from(reading.value["length"].as_u64().unwrap()).unwrap();
        assert_eq!(
            crate::types::hex(Sha256::digest(&raw[offset..offset + length])),
            reading.value["sha256"].as_str().unwrap()
        );
        assert_eq!(&raw[offset..offset + 8], b"CFSCGR1\0");
        assert_eq!(
            &raw[offset + length - 56..offset + length - 48],
            b"CFSCCM1\0"
        );
    }
    let summary = source.drain().unwrap().unwrap();
    assert_eq!(summary.value["groups"], 2);
    assert_eq!(summary.value["storage_finalized"], true);
    assert_eq!(
        summary.value["committed_bytes"],
        u64::try_from(raw.len()).unwrap()
    );
    assert_eq!(
        summary.value["sha256_committed_prefix"],
        crate::types::hex(Sha256::digest(&raw))
    );
    assert!(source.drain().unwrap().is_none());
}
#[test]
fn selected_rejection_preserves_exact_raw_context_and_first_typed_failure_without_sample() {
    let folder = tempfile::tempdir().unwrap();
    let directory = folder.path().join("output");
    let mut source = receiver(&directory);
    send(&source, b"\x02\x02\xff");
    let first = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&first).kind, ErrorKind::Incomplete);
    let first_tag = crate::types::error_tag(&first);
    let repeated = source.read(Duration::from_secs(30)).unwrap_err();
    assert_eq!(crate::types::error_tag(&repeated), first_tag);
    assert!(source.socket.is_none());
    assert_eq!(source.journal.groups, 0);
    source.finish(Duration::from_secs(1)).unwrap();
    assert!(source.drain().unwrap().is_none());
    assert_eq!(
        std::fs::read(directory.join("rejected-datagram.raw")).unwrap(),
        [2, 2, 255]
    );
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(directory.join("rejected-datagram.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["raw_length"], 3);
    assert_eq!(metadata["cause"]["code"], "incomplete");
    assert_eq!(
        metadata["sha256"],
        crate::types::hex(Sha256::digest([2, 2, 255]))
    );
    assert_eq!(metadata["context_evidence_archive"]["valid_group_count"], 0);
    let archive = std::fs::read(directory.join("datagrams.sickcompact")).unwrap();
    assert_eq!(archive.len(), 97);
    assert_eq!(&archive[80..], b"{\"original\":true}");
    assert_eq!(source.prefix()["committed_bytes"], 0);
    assert_eq!(source.prefix()["packets"], 0);
    assert_eq!(source.prefix()["context_evidence_archive"]["length"], 97);
    assert_eq!(source.prefix()["rejected_datagram"]["complete"], true);
}
#[test]
fn good_datagram_before_bad_packet_is_synced_and_drained_once_with_original_cause() {
    let folder = tempfile::tempdir().unwrap();
    let directory = folder.path().join("output");
    let mut source = receiver(&directory);
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let target = source.socket.as_ref().unwrap().local_addr().unwrap();
    let good = raw_empty();
    assert_eq!(peer.send_to(&good, target).unwrap(), good.len());
    assert_eq!(peer.send_to(b"bad", target).unwrap(), 3);
    let error = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert_eq!(source.journal.groups, 1);
    source.finish(Duration::from_secs(1)).unwrap();
    let prefix = source.drain().unwrap().unwrap();
    assert_eq!(prefix.value["packet_count"], 1);
    assert_eq!(prefix.value["datagrams"][0]["modules"], json!([]));
    let offset =
        usize::try_from(prefix.value["datagrams"][0]["raw_offset"].as_u64().unwrap()).unwrap();
    let raw = std::fs::read(directory.join("datagrams.sickcompact")).unwrap();
    assert_eq!(&raw[offset..offset + good.len()], good);
    let summary = source.drain().unwrap().unwrap();
    assert_eq!(summary.value["packets"], 1);
    assert_eq!(summary.value["rejected_datagram"]["complete"], true);
    assert!(source.drain().unwrap().is_none());
}
#[test]
fn maximum_and_collision_checks_preserve_foreign_files_and_reserved_evidence() {
    let folder = tempfile::tempdir().unwrap();
    let directory = folder.path().join("foreign");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("keep"), b"foreign").unwrap();
    let mut settings = config();
    settings.output_directory = directory.to_str().unwrap().into();
    let Err(error) = open(&settings) else {
        panic!("preexisting directory accepted");
    };
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Exists);
    assert_eq!(std::fs::read(directory.join("keep")).unwrap(), b"foreign");
    let output = folder.path().join("exact");
    settings.max_bytes = 82_127;
    let mut journal =
        Journal::new(output.clone(), &settings, Context::read(None).unwrap(), 0).unwrap();
    let mut group = Group::new();
    assert!(
        group
            .offer(packet(1, 0), &journal, "datagrams", 32, None)
            .unwrap()
            .is_none()
    );
    journal.commit(group, "datagrams", None).unwrap();
    let mut next = Group::new();
    assert_eq!(
        crate::types::classify(
            &next
                .offer(packet(2, 0), &journal, "datagrams", 32, None)
                .unwrap_err()
        )
        .kind,
        ErrorKind::StorageFull
    );
    let bad = vec![255; MAX_PACKET];
    journal
        .reject(
            &bad,
            2,
            "127.0.0.1:7502".parse().unwrap(),
            &incomplete("injected malformed maximum UDP packet"),
        )
        .unwrap();
    journal.finish().unwrap();
    assert_eq!(
        std::fs::read(output.join("rejected-datagram.raw")).unwrap(),
        bad
    );
    let total: u64 = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= settings.max_bytes);
}
#[test]
fn expired_commit_retains_only_committed_reading_and_foreign_budget_counts_actual_bytes() {
    let folder = tempfile::tempdir().unwrap();
    let mut source = receiver(&folder.path().join("late"));
    let mut group = Group::new();
    assert!(
        group
            .offer(packet(1, 0), &source.journal, "datagrams", 32, None)
            .unwrap()
            .is_none()
    );
    source.pending = Some(packet(2, 0));
    let error = source
        .finish_group(group, Some(Instant::now()))
        .unwrap_err();
    source.failed = Some(Failure::capture(&error));
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert_eq!(source.journal.groups, 1);
    assert_eq!(source.drain().unwrap().unwrap().value["packet_count"], 1);
    source.finish(Duration::from_secs(1)).unwrap();
    let pending = source.drain().unwrap().unwrap();
    assert_eq!(pending.value["datagrams"][0]["received_monotonic_ns"], 2);
    assert_eq!(source.drain().unwrap().unwrap().value["packets"], 2);
    assert!(source.drain().unwrap().is_none());
    assert_eq!(
        crate::types::classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    let mut foreign = receiver(&folder.path().join("foreign-budget"));
    foreign.source_port = Some(1);
    let raw = vec![255; 8_193];
    let crossing = MAX_FOREIGN_BYTES / raw.len() + 1;
    assert!((crossing - 1) * raw.len() <= MAX_FOREIGN_BYTES);
    assert!(crossing * raw.len() > MAX_FOREIGN_BYTES);
    assert!(crossing < MAX_FOREIGN_PACKETS);
    for _ in 0..=crossing {
        send(&foreign, &raw);
    }
    let error = foreign.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert_eq!(foreign.foreign_packets, u64::try_from(crossing).unwrap());
    assert!(foreign.socket.is_none());
    let later = foreign.read(Duration::ZERO).unwrap_err();
    assert_eq!(crate::types::classify(&later).kind, ErrorKind::Incomplete);
    assert_eq!(foreign.foreign_packets, u64::try_from(crossing).unwrap());
    assert!(!folder.path().join("foreign-budget").exists());
}

#[test]
fn opposite_known_stream_is_bounded_ignore_while_unknown_selected_bytes_retire() {
    let folder = tempfile::tempdir().unwrap();
    let mut source = receiver(&folder.path().join("filtered"));
    source.packets_per_group = 1;
    send(&source, b"\x02\x02\x02\x02\x02\x00\x00\x00");
    send(&source, b"\x02\x02\x02\x02\x02\x00\x00\x00");
    send(&source, &raw_empty());
    let reading = source.read(Duration::from_secs(1)).unwrap();
    assert_eq!(reading.len(), 1);
    assert_eq!(reading[0].value["packet_count"], 1);
    assert_eq!(source.other_stream_packets, 2);
    assert_eq!(source.foreign_packets, 0);
    let folder2 = folder.path().join("unknown");
    let mut unknown = receiver(&folder2);
    send(&unknown, b"\x02\x02\x02\x02\x03\x00\x00\x00");
    let error = unknown.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert_eq!(unknown.other_stream_packets, 0);
    assert_eq!(
        std::fs::read(folder2.join("rejected-datagram.raw")).unwrap(),
        b"\x02\x02\x02\x02\x03\x00\x00\x00"
    );
}
#[test]
fn evidence_floor_failure_preserves_decoder_cause_and_makes_no_persistence_claim() {
    let folder = tempfile::tempdir().unwrap();
    let directory = folder.path().join("no-space");
    let mut settings = config();
    settings.min_free_bytes = Some(u64::MAX);
    let mut source = configured_receiver(&directory, &settings);
    send(&source, b"bad");
    let error = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert!(source.prefix().get("rejected_datagram").is_none());
    assert_eq!(
        source.prefix()["evidence_write_error"]["code"],
        "storage_full"
    );
    assert_ne!(
        source.prefix()["evidence_write_error"]["message"]
            .as_str()
            .unwrap(),
        ""
    );
    assert_eq!(source.prefix()["diagnostic_bytes"], 0);
    let details = crate::types::classify(&error).details.unwrap();
    assert_eq!(
        details["sick_compact_udp"]["evidence_write_error"]["code"],
        "storage_full"
    );
    source.finish(Duration::from_secs(1)).unwrap();
    assert!(!directory.exists());
    assert_eq!(
        crate::types::classify(&source.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    assert_eq!(
        FAILURE_RESERVE,
        u64::try_from(MAX_PACKET + MAX_EVIDENCE_JSON).unwrap()
    );
}

#[test]
fn terminal_receive_failure_path_retains_already_accepted_group_and_original_typed_cause() {
    let folder = tempfile::tempdir().unwrap();
    let mut source = receiver(&folder.path().join("output"));
    let mut group = Group::new();
    assert!(
        group
            .offer(packet(1, 0), &source.journal, "datagrams", 32, None)
            .unwrap()
            .is_none()
    );
    let error = source.retain_group(
        Categorized::error(ErrorKind::Disconnected, "injected terminal receive failure"),
        group,
    );
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
    assert_eq!(source.journal.groups, 1);
    assert_eq!(source.journal.packets, 1);
    let reading = source.drain().unwrap().unwrap();
    assert_eq!(reading.value["datagrams"][0]["received_monotonic_ns"], 1);
    assert!(source.drain().unwrap().is_none());
}

fn encoder_config(stream: Stream) -> Config {
    let mut settings = config();
    settings.profile = Profile::Picoscan150;
    settings.stream = stream;
    settings
}
fn hand_packet(hex: &str) -> Vec<u8> {
    let (pairs, tail) = hex.as_bytes().as_chunks::<2>();
    assert_eq!(tail, &[] as &[u8]);
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn raw_encoder() -> Vec<u8> {
    // Immutable plan literal: v1_signed_zero_unsigned_edges, SHA622e9ce0.
    hand_packet(
        "0202020204000000080706050403020118171615141312110100000034000000d4c3b2a12827262524232221ffffffff0000008004030201000000800000000000000000ffffffffffffffff3837363534333231bb408086",
    )
}
fn raw_imu() -> Vec<u8> {
    // Immutable predecoder4773e8ee plan_type2_imu_v1 (not a decoder-generated frame).
    hand_packet(
        "0202020202000000010000000000803f000000c000000000000000800000807f3412c07f0000803f000000000000000000000000640000000000000019538588",
    )
}

#[test]
fn encoder_profile_gates_precede_directory_or_socket_io_and_minima_append_ids() {
    let folder = tempfile::tempdir().unwrap();
    let occupied = UdpSocket::bind("127.0.0.1:0").unwrap();
    for profile in [Profile::Multiscan100, Profile::Picoscan100] {
        for stream in [Stream::Encoder, Stream::Mixed] {
            let mut settings = encoder_config(stream);
            settings.profile = profile;
            let SocketAddr::V4(listen) = occupied.local_addr().unwrap() else {
                panic!("IPv4 fixture socket expected");
            };
            settings.listen = listen;
            let output = folder
                .path()
                .join(format!("{}-{}", profile.name(), stream.name()));
            settings.output_directory = output.to_str().unwrap().into();
            assert_eq!(
                crate::types::classify(&settings.validate().unwrap_err()).kind,
                ErrorKind::Unsupported
            );
            let Err(error) = open(&settings) else {
                panic!("unqualified encoder profile accepted");
            };
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
            assert!(!output.exists());
        }
    }
    for (stream, id, minimum, max_bytes) in [
        (Stream::Points, 1, 36, 82_127),
        (Stream::Imu, 2, 64, 82_155),
        (Stream::Encoder, 3, 88, 82_179),
        (Stream::Mixed, 4, 36, 82_127),
    ] {
        assert_eq!(stream.id(), id);
        assert_eq!(stream.minimum_packet(), minimum);
        let mut settings = encoder_config(stream);
        settings.max_bytes = max_bytes;
        settings.validate().unwrap();
        settings.max_bytes -= 1;
        assert_eq!(
            crate::types::classify(&settings.validate().unwrap_err()).kind,
            ErrorKind::Usage
        );
    }
    assert_eq!(Profile::Multiscan100.id(), 1);
    assert_eq!(Profile::Picoscan100.id(), 2);
    assert_eq!(Profile::Picoscan150.id(), 3);
}

#[test]
fn known_other_stream_marker_filter_is_profile_scoped_and_never_general_ignore() {
    let marker = |kind| [2, 2, 2, 2, kind, 0, 0, 0];
    // Independent closed expectations for types1/2/4/5, in that order.
    for (profile, expected_rows) in [
        (
            Profile::Multiscan100,
            [
                [false, true, false, false],
                [true, false, false, false],
                [true, true, false, false],
                [false, false, false, false],
            ],
        ),
        (
            Profile::Picoscan100,
            [
                [false, true, false, false],
                [true, false, false, false],
                [true, true, false, false],
                [false, false, false, false],
            ],
        ),
        (
            Profile::Picoscan150,
            [
                [false, true, true, false],
                [true, false, true, false],
                [true, true, false, false],
                [false, false, false, false],
            ],
        ),
    ] {
        for (stream, expected) in [Stream::Points, Stream::Imu, Stream::Encoder, Stream::Mixed]
            .into_iter()
            .zip(expected_rows)
        {
            for (kind, expected) in [1, 2, 4, 5].into_iter().zip(expected) {
                assert_eq!(other_stream(&marker(kind), profile, stream), expected);
            }
            assert!(!other_stream(&marker(4)[..7], profile, stream));
            assert!(!other_stream(&[2, 2, 2, 3, 4, 0, 0, 0], profile, stream));
            assert!(!other_stream(&[2, 2, 2, 2, 4, 1, 0, 0], profile, stream));
        }
    }
    let folder = tempfile::tempdir().unwrap();
    for profile in [Profile::Multiscan100, Profile::Picoscan100] {
        for stream in [Stream::Points, Stream::Imu] {
            let mut settings = config();
            settings.profile = profile;
            settings.stream = stream;
            let directory = folder
                .path()
                .join(format!("{}-{}", profile.name(), stream.name()));
            let mut source = configured_receiver(&directory, &settings);
            let encoder = raw_encoder();
            send(&source, &encoder);
            let error = source.read(Duration::from_secs(1)).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unsupported);
            assert_eq!(source.other_stream_packets, 0);
            assert_eq!(source.journal.groups, 0);
            assert!(source.socket.is_none());
            assert_eq!(
                std::fs::read(directory.join("rejected-datagram.raw")).unwrap(),
                encoder
            );
        }
    }
}

#[test]
fn picoscan150_mixed_keeps_three_sender_ports_order_raw_and_original_clocks() {
    let folder = tempfile::tempdir().unwrap();
    let directory = folder.path().join("mixed");
    let mut source = configured_receiver(&directory, &encoder_config(Stream::Mixed));
    source.packets_per_group = 4;
    let target = source.socket.as_ref().unwrap().local_addr().unwrap();
    let peers: Vec<_> = std::iter::repeat_with(|| UdpSocket::bind("127.0.0.1:0").unwrap())
        .take(3)
        .collect();
    let packets = [raw_empty(), raw_encoder(), raw_imu(), raw_encoder()];
    for (raw, peer_index) in packets.iter().zip([0, 1, 2, 1]) {
        assert_eq!(peers[peer_index].send_to(raw, target).unwrap(), raw.len());
    }
    let readings = source.read(Duration::from_secs(1)).unwrap();
    assert_eq!(readings.len(), 1);
    assert!(readings[0].source_time.is_none());
    assert_eq!(readings[0].value["packet_count"], 4);
    assert_eq!(readings[0].value["sensor"]["profile"], "picoscan150");
    assert_eq!(readings[0].value["sensor"]["stream"], "mixed");
    let datagrams = readings[0].value["datagrams"].as_array().unwrap();
    let mut previous_clock = 0;
    for (index, ((datagram, expected_type), peer_index)) in datagrams
        .iter()
        .zip([1, 4, 2, 4])
        .zip([0, 1, 2, 1])
        .enumerate()
    {
        assert_eq!(datagram["telegram_type"], expected_type);
        assert_eq!(
            datagram["source"],
            peers[peer_index].local_addr().unwrap().to_string()
        );
        assert_eq!(datagram["host_packet_index"], index + 1);
        assert_eq!(datagram["raw_length"], packets[index].len());
        let received = datagram["received_monotonic_ns"].as_u64().unwrap();
        assert!(received >= previous_clock);
        previous_clock = received;
    }
    assert_eq!(source.observed_ns(), Some(previous_clock));
    assert_eq!(source.other_stream_packets, 0);
    assert_eq!(source.foreign_packets, 0);
    source.finish(Duration::from_secs(1)).unwrap();
    let archive = std::fs::read(directory.join("datagrams.sickcompact")).unwrap();
    assert_eq!(&archive[..8], b"CFSCMP01");
    assert_eq!(u16::from_le_bytes(archive[8..10].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(archive[20..22].try_into().unwrap()), 3);
    assert_eq!(u16::from_le_bytes(archive[72..74].try_into().unwrap()), 4);
    assert_eq!(u16::from_le_bytes(archive[74..76].try_into().unwrap()), 36);
    for (datagram, raw) in datagrams.iter().zip(&packets) {
        let offset = usize::try_from(datagram["raw_offset"].as_u64().unwrap()).unwrap();
        assert_eq!(&archive[offset..offset + raw.len()], raw);
    }
    assert_eq!(source.drain().unwrap().unwrap().value["packets"], 4);
    assert!(source.drain().unwrap().is_none());
}

#[test]
fn qualified_single_streams_ignore_known_markers_and_share_actual_byte_budget() {
    let folder = tempfile::tempdir().unwrap();
    for (stream, selected, ignored) in [
        (Stream::Points, raw_empty(), vec![4]),
        (Stream::Imu, raw_imu(), vec![4]),
        (Stream::Encoder, raw_encoder(), vec![1, 2]),
    ] {
        let directory = folder.path().join(stream.name());
        let mut source = configured_receiver(&directory, &encoder_config(stream));
        source.packets_per_group = 1;
        for kind in &ignored {
            // Eight bytes are sufficient for ignored classification; no CRC qualification.
            send(&source, &[2, 2, 2, 2, *kind, 0, 0, 0]);
        }
        send(&source, &selected);
        let readings = source.read(Duration::from_secs(1)).unwrap();
        assert_eq!(readings[0].value["packet_count"], 1);
        assert_eq!(
            source.other_stream_packets,
            u64::try_from(ignored.len()).unwrap()
        );
        source.finish(Duration::from_secs(1)).unwrap();
        let archive = std::fs::read(directory.join("datagrams.sickcompact")).unwrap();
        assert_eq!(u16::from_le_bytes(archive[20..22].try_into().unwrap()), 3);
        assert_eq!(
            u16::from_le_bytes(archive[72..74].try_into().unwrap()),
            stream.id()
        );
        assert_eq!(
            usize::from(u16::from_le_bytes(archive[74..76].try_into().unwrap())),
            stream.minimum_packet()
        );
    }
    let mut source = configured_receiver(
        &folder.path().join("shared-budget"),
        &encoder_config(Stream::Points),
    );
    let selected_peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    let wrong_peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    for peer in [&selected_peer, &wrong_peer] {
        socket2::SockRef::from(peer)
            .set_send_buffer_size(128 * 1024)
            .unwrap();
    }
    source.source_port = Some(selected_peer.local_addr().unwrap().port());
    let target = source.socket.as_ref().unwrap().local_addr().unwrap();
    let mut ignored = vec![255; 16_385];
    ignored[..8].copy_from_slice(&[2, 2, 2, 2, 4, 0, 0, 0]);
    for peer in [&selected_peer, &wrong_peer, &selected_peer, &wrong_peer] {
        assert_eq!(peer.send_to(&ignored, target).unwrap(), ignored.len());
    }
    let error = source.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert_eq!(source.other_stream_packets, 2);
    assert_eq!(source.foreign_packets, 2);
    assert_eq!(source.journal.groups, 0);
    assert!(source.socket.is_none());
    let first = crate::types::error_tag(&error);
    assert_eq!(
        crate::types::error_tag(&source.read(Duration::ZERO).unwrap_err()),
        first
    );
}
