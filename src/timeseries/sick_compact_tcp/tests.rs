use super::*;
use std::{
    io::Write as _,
    net::{Shutdown, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
fn raw_empty() -> Vec<u8> {
    // Independent 4773e8ee empty_modules_v3, frozen before either decoder.
    let hex = b"02020202010000000100000000000000080706050403020103000000000000006b934408";
    let (pairs, tail) = hex.as_chunks::<2>();
    assert_eq!(tail, b"");
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn settings(address: SocketAddrV4, directory: &std::path::Path) -> Config {
    serde_json::from_value(json!({"address":address,"profile":"lrs4000","stream":"points","output_directory":directory,"max_bytes":20_000_000,"min_free_bytes":0})).unwrap()
}
fn receiver(config: &Config) -> Receiver {
    Receiver {
        socket: None,
        address: config.address,
        format: config.format(),
        signal: config.signal.clone(),
        journal: Journal::new(
            std::path::absolute(&config.output_directory).unwrap(),
            config,
            Context {
                raw: vec![0, 255, 17],
                supplied: true,
            },
            crate::host::monotonic_ns().unwrap(),
        )
        .unwrap(),
        pending: None,
        failed: None,
        finished: false,
        observed: None,
        cutoff: None,
        stream_bytes: 0,
        part_readings_released: 0,
        terminal: None,
    }
}
struct Peer {
    address: SocketAddrV4,
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Peer {
    fn new(body: impl FnOnce(TcpStream, Arc<AtomicBool>) + Send + 'static) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            panic!("fixture IPv4")
        };
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let stopping = Arc::clone(&stop);
        let active = Arc::clone(&socket);
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        *active.lock().unwrap() = Some(stream.try_clone().unwrap());
                        body(stream, stopping);
                        drop(active.lock().unwrap().take());
                        return;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "bounded TCP accept");
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("TCP fixture accept: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            socket,
            thread: Some(thread),
        }
    }
    fn finish(mut self) {
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        {
            let active = self
                .socket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(socket) = active.as_ref() {
                drop(socket.shutdown(Shutdown::Both));
            }
        }
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}
fn eof(mut stream: TcpStream, stop: &AtomicBool) {
    match stream.read(&mut [0; 1]) {
        Ok(0) => (),
        Err(_) if stop.load(Ordering::Acquire) => (),
        other => panic!("unexpected application bytes/peer state: {other:?}"),
    }
}
#[test]
fn profiles_streams_and_reserve_are_pure_closed_admission() {
    let folder = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:2115".parse().unwrap();
    for profile in ["lrs4000", "multiscan200"] {
        for stream in ["points", "imu", "ambient"] {
            let mut value = json!({"address":address,"profile":profile,"stream":stream,"output_directory":folder.path().join("new"),"max_bytes":20_000_000});
            let config: Config = serde_json::from_value(value.take()).unwrap();
            if profile == "lrs4000" && stream != "points" {
                assert_eq!(
                    crate::types::classify(&config.validate().unwrap_err()).kind,
                    ErrorKind::Unsupported
                );
            } else {
                config.validate().unwrap();
            }
        }
    }
    let base = settings(address, &folder.path().join("new"));
    for (key, value) in [
        ("address", json!("example.test:2115")),
        ("address", json!("127.0.0.1:0")),
        ("address", json!("239.1.1.1:2115")),
        ("profile", json!("unknown")),
        ("stream", json!("any")),
        ("max_bytes", json!(4_210_891)),
        ("max_bytes", json!(1.5)),
        ("output_directory", json!("-")),
        ("output_directory", json!("{date}")),
        ("signal", json!("")),
    ] {
        let mut raw = json!({"address":base.address,"profile":"lrs4000","stream":"points","output_directory":base.output_directory,"max_bytes":base.max_bytes});
        raw[key] = value;
        if let Ok(config) = serde_json::from_value::<Config>(raw) {
            assert_eq!(
                crate::types::classify(&config.validate().unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
    }
    assert!(!folder.path().join("new").exists());
}
#[test]
fn zero_original_budget_never_connects_or_creates_output_and_first_failure_is_sticky() {
    let folder = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
        panic!("IPv4")
    };
    let config = settings(address, &folder.path().join("idle"));
    let mut owner = receiver(&config);
    assert_eq!(
        crate::types::classify(&owner.read(Duration::ZERO).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(
        crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert!(owner.socket.is_none());
    assert!(!owner.ready());
    assert!(!owner.end_timeout_is_idle());
    assert!(owner.buffered_timestamps());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    owner.finish(Duration::from_secs(1)).unwrap();
    assert!(!folder.path().join("idle").exists());
}
#[test]
fn fragmented_and_coalesced_frames_keep_exact_boundaries_one_epoch_and_no_application_bytes() {
    let folder = tempfile::tempdir().unwrap();
    let raw = raw_empty();
    let sent = raw.clone();
    let peer = Peer::new(move |mut stream, stop| {
        stream.write_all(&sent[..7]).unwrap();
        let mut tail = sent[7..].to_vec();
        tail.extend_from_slice(&sent);
        stream.write_all(&tail).unwrap();
        eof(stream, &stop);
    });
    let config = settings(peer.address, &folder.path().join("frames"));
    let mut owner = receiver(&config);
    let first = owner.read(Duration::from_secs(1)).unwrap();
    let clock = owner.observed_ns().unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].value["telegram_index"], 1);
    assert_eq!(first[0].value["stream_offset"], 0);
    assert_eq!(first[0].value["archive"]["raw_length"], 36);
    assert_eq!(owner.stream_bytes, 36);
    let next = owner.read(Duration::from_secs(1)).unwrap();
    assert_eq!(next[0].value["telegram_index"], 2);
    assert_eq!(next[0].value["stream_offset"], 36);
    assert_eq!(owner.stream_bytes, 72);
    assert!(owner.observed_ns().unwrap() >= clock);
    owner.finish(Duration::from_secs(1)).unwrap();
    let file = owner.drain().unwrap().unwrap();
    assert_eq!(file.value["telegrams"], 2);
    assert_eq!(file.value["part_readings_released"], 2);
    assert_eq!(file.value["pending_parts"], 0);
    assert!(owner.drain().unwrap().is_none());
    peer.finish();
    let archive = std::fs::read(folder.path().join("frames/telegrams.sickcompacttcp")).unwrap();
    assert_eq!(&archive[..8], b"CFSCTP01");
    assert_eq!(&archive[80..83], &[0, 255, 17]);
    let mut offset = 83;
    for index in 1..=2_u64 {
        assert_eq!(&archive[offset..offset + 4], b"TGM1");
        assert_eq!(
            u64::from_le_bytes(archive[offset + 8..offset + 16].try_into().unwrap()),
            index
        );
        assert_eq!(&archive[offset + 32..offset + 68], raw);
        assert_eq!(&archive[offset + 68..offset + 76], b"CFSCTC1\0");
        offset += 124;
    }
    assert_eq!(offset, archive.len());
}
#[test]
fn a_progressing_partial_frame_times_out_at_original_budget_then_exact_prefix_retires() {
    let folder = tempfile::tempdir().unwrap();
    let raw = raw_empty();
    let (ready, accepted) = std::sync::mpsc::channel();
    let peer = Peer::new(move |mut stream, stop| {
        stream.write_all(&raw[..1]).unwrap();
        ready.send(()).unwrap();
        for byte in &raw[1..] {
            thread::sleep(Duration::from_millis(15));
            if stream.write_all(&[*byte]).is_err() {
                return;
            }
        }
        eof(stream, &stop);
    });
    let config = settings(peer.address, &folder.path().join("partial"));
    let mut owner = receiver(&config);
    owner.socket = Some(TcpStream::connect(peer.address).unwrap());
    accepted.recv_timeout(Duration::from_secs(2)).unwrap();
    let error = owner.read(Duration::from_millis(70)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert!(!owner.end_timeout_is_idle());
    let prefix = std::fs::read(folder.path().join("partial/rejected-stream.raw")).unwrap();
    assert!(!prefix.is_empty() && prefix.len() < 36);
    assert_eq!(prefix, &raw_empty()[..prefix.len()]);
    assert_eq!(
        owner.prefix()["rejected_stream_prefix"]["scope"],
        "stream_prefix"
    );
    assert_eq!(owner.prefix()["telegrams"], 0);
    assert!(owner.socket.is_none());
    assert_eq!(
        crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    owner.finish(Duration::from_secs(1)).unwrap();
    assert!(owner.drain().unwrap().is_none());
    peer.finish();
}
#[test]
fn partial_eof_and_boundary_eof_have_distinct_typed_causes_and_no_reconnect() {
    for bytes in [0, 14] {
        let folder = tempfile::tempdir().unwrap();
        let raw = raw_empty();
        let sent = raw[..bytes].to_vec();
        let peer = Peer::new(move |mut stream, _| {
            stream.write_all(&sent).unwrap();
        });
        let config = settings(peer.address, &folder.path().join("eof"));
        let mut owner = receiver(&config);
        let kind = if bytes == 0 {
            ErrorKind::Disconnected
        } else {
            ErrorKind::Incomplete
        };
        assert_eq!(
            crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
            kind
        );
        assert_eq!(
            std::fs::read(folder.path().join("eof/rejected-stream.raw")).unwrap(),
            &raw[..bytes]
        );
        assert_eq!(owner.prefix()["telegrams"], 0);
        assert!(owner.socket.is_none());
        assert_eq!(
            crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
            kind
        );
        owner.finish(Duration::from_secs(1)).unwrap();
        assert!(owner.drain().unwrap().is_none());
        peer.finish();
    }
}
#[test]
fn later_crc_failure_preserves_synced_prefix_and_exact_rejected_frame() {
    let folder = tempfile::tempdir().unwrap();
    let good = raw_empty();
    let mut bad = good.clone();
    bad[8] ^= 1;
    let sent_bad = bad.clone();
    let peer = Peer::new(move |mut stream, stop| {
        let mut raw = good;
        raw.extend_from_slice(&sent_bad);
        stream.write_all(&raw).unwrap();
        eof(stream, &stop);
    });
    let config = settings(peer.address, &folder.path().join("crc"));
    let mut owner = receiver(&config);
    assert_eq!(owner.read(Duration::from_secs(1)).unwrap().len(), 1);
    let error = owner.read(Duration::from_secs(1)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Incomplete);
    assert_eq!(owner.prefix()["telegrams"], 1);
    assert_eq!(
        std::fs::read(folder.path().join("crc/rejected-stream.raw")).unwrap(),
        bad
    );
    assert_eq!(
        owner.prefix()["rejected_stream_prefix"]["scope"],
        "framed_telegram"
    );
    assert_eq!(owner.prefix()["rejected_stream_prefix"]["complete"], true);
    owner.finish(Duration::from_secs(1)).unwrap();
    assert_eq!(owner.drain().unwrap().unwrap().value["telegrams"], 1);
    peer.finish();
}
#[test]
fn cutoff_rejects_late_complete_frame_without_admitting_or_hiding_timeout() {
    let folder = tempfile::tempdir().unwrap();
    let peer = Peer::new(|mut stream, stop| {
        stream.write_all(&raw_empty()).unwrap();
        eof(stream, &stop);
    });
    let config = settings(peer.address, &folder.path().join("cutoff"));
    let mut owner = receiver(&config);
    owner.set_acquisition_end(Some(0));
    assert_eq!(
        crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert_eq!(owner.journal.telegrams, 0);
    assert!(owner.pending.is_none());
    assert!(!owner.end_timeout_is_idle());
    owner.finish(Duration::from_secs(1)).unwrap();
    peer.finish();
}
#[test]
fn expired_postcommit_publication_keeps_all_admitted_parts_for_clocked_drain() {
    let folder = tempfile::tempdir().unwrap();
    let config = settings(
        "127.0.0.1:2115".parse().unwrap(),
        &folder.path().join("late"),
    );
    let mut owner = receiver(&config);
    let raw = raw_empty();
    let deadline = Instant::now() + Duration::from_secs(1);
    let decoded = packet::validate(&raw, owner.format, deadline).unwrap();
    let clock = crate::host::monotonic_ns().unwrap();
    let archive = owner.journal.descriptor(&raw, clock, 0).unwrap();
    owner
        .admission(&decoded, &archive, clock, 0, deadline)
        .unwrap();
    owner
        .journal
        .commit(&raw, clock, 0, decoded.parts, None)
        .unwrap();
    let pending = Pending {
        raw,
        decoded,
        archive,
        received: clock,
        stream_offset: 0,
        next: 0,
    };
    let error = owner.committed(pending, Instant::now()).unwrap_err();
    let error = owner.remember(error);
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert_eq!(owner.journal.telegrams, 1);
    assert_eq!(owner.part_readings_released, 0);
    owner.finish(Duration::from_secs(1)).unwrap();
    let part = owner.drain().unwrap().unwrap();
    assert_eq!(part.value["telegram_index"], 1);
    assert_eq!(part.value["received_monotonic_ns"], clock);
    assert_eq!(owner.observed_ns(), Some(clock));
    assert!(part.source_time.is_none());
    assert_eq!(
        owner.drain().unwrap().unwrap().value["part_readings_released"],
        1
    );
    assert!(owner.drain().unwrap().is_none());
    assert_eq!(
        crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
}
#[test]
fn expired_cleanup_render_latches_first_timeout_without_popping_the_committed_plan() {
    let folder = tempfile::tempdir().unwrap();
    let config = settings(
        "127.0.0.1:2115".parse().unwrap(),
        &folder.path().join("drain"),
    );
    let mut owner = receiver(&config);
    let raw = raw_empty();
    let deadline = Instant::now() + Duration::from_secs(1);
    let decoded = packet::validate(&raw, owner.format, deadline).unwrap();
    let clock = crate::host::monotonic_ns().unwrap();
    let archive = owner.journal.descriptor(&raw, clock, 0).unwrap();
    owner
        .admission(&decoded, &archive, clock, 0, deadline)
        .unwrap();
    owner
        .journal
        .commit(&raw, clock, 0, decoded.parts, None)
        .unwrap();
    owner.pending = Some(Pending {
        raw,
        decoded,
        archive,
        received: clock,
        stream_offset: 0,
        next: 0,
    });
    owner.finish(Duration::from_secs(1)).unwrap();
    let error = owner.drain_until(Instant::now()).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert!(
        crate::types::classify(&error)
            .details
            .unwrap()
            .get("sick_compact_tcp")
            .is_some()
    );
    assert_eq!(owner.pending.as_ref().unwrap().next, 0);
    assert_eq!(owner.part_readings_released, 0);
    assert_eq!(
        crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    assert!(
        owner
            .drain_until(Instant::now() + Duration::from_secs(1))
            .unwrap()
            .is_some()
    );
    assert_eq!(owner.observed_ns(), Some(clock));
    assert!(owner.pending.is_none());
    assert_eq!(
        owner.drain().unwrap().unwrap().value["part_readings_released"],
        1
    );
    assert_eq!(
        crate::types::classify(&owner.read(Duration::from_secs(1)).unwrap_err()).kind,
        ErrorKind::Timeout
    );
}
