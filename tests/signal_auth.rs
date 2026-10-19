//! Real helper transport with an entirely local, finite MQTT peer. The
//! credential fixture runs in a fresh process with an isolated environment.
#[cfg(all(feature = "mqtt", unix))]
mod mqtt {
    use capturefab::{
        session::{SessionCommand, SessionSnapshot},
        signals::{
            auth::{ProfileDraft, SecretSource, Store},
            hub::SignalHub,
        },
        tools::{
            Origin, ToolCall,
            context::{Context, DataSourceRef, RecordingDefaults, ToolHost},
        },
    };
    use serde_json::{Value, json};
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const USER: &str = "CANARY-signal-private-user";
    const PASSWORD: &str = "CANARY-signal-initial-password";
    const ROTATED: &str = "CANARY-signal-rotated-password";
    const VARIABLE: &str = "CAPTUREFAB_SIGNAL_HELPER_PASSWORD";

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Broker {
        address: std::net::SocketAddr,
        logins: Arc<Mutex<Vec<(String, String)>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl Broker {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let logins = Arc::new(Mutex::new(Vec::new()));
            let seen = logins.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        Err(_) => break,
                    };
                    // BSD accept may inherit the listener's nonblocking flag.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let (kind, connect_packet) = packet(&mut stream).expect("local MQTT CONNECT");
                    assert_eq!(kind, 0x10);
                    let mut cursor = 0;
                    assert_eq!(string(&connect_packet, &mut cursor), "MQTT");
                    assert_eq!(connect_packet[cursor], 4);
                    cursor += 1;
                    let flags = connect_packet[cursor];
                    cursor += 3; // flags and keep-alive
                    let _client_id = string(&connect_packet, &mut cursor);
                    assert_eq!(
                        flags & 0xc4,
                        0xc0,
                        "fixture expects user/password and no will"
                    );
                    let user = string(&connect_packet, &mut cursor);
                    let password = string(&connect_packet, &mut cursor);
                    assert_eq!(cursor, connect_packet.len());
                    seen.lock().unwrap().push((user, password));
                    stream.write_all(&[0x20, 2, 0, 0]).unwrap();
                    let (kind, subscribe) = packet(&mut stream).expect("local MQTT SUBSCRIBE");
                    assert_eq!(kind & 0xf0, 0x80);
                    stream
                        .write_all(&[0x90, 3, subscribe[0], subscribe[1], 0])
                        .unwrap();
                    publish(&mut stream, "line/echo", &format!("{USER} {PASSWORD}"));
                    publish(&mut stream, "line/number", "42");
                    stream
                        .set_read_timeout(Some(Duration::from_millis(250)))
                        .unwrap();
                    while !stopped.load(Ordering::SeqCst) {
                        match packet(&mut stream) {
                            Ok((0xc0, _)) => {
                                let _ = stream.write_all(&[0xd0, 0]);
                            }
                            Ok((0xe0, _)) => break,
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => break,
                        }
                    }
                }
            });
            Self {
                address,
                logins,
                stop,
                worker: Some(worker),
            }
        }
    }
    impl Drop for Broker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !thread::panicking() {
                    result.unwrap();
                }
            }
        }
    }
    fn packet(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
        let mut kind = [0u8; 1];
        stream.read_exact(&mut kind)?;
        let mut length = 0usize;
        let mut multiplier = 1;
        for _ in 0..4 {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte)?;
            length += usize::from(byte[0] & 127) * multiplier;
            if byte[0] & 128 == 0 {
                assert!(length <= 1 << 20);
                let mut body = vec![0; length];
                stream.read_exact(&mut body)?;
                return Ok((kind[0], body));
            }
            multiplier *= 128;
        }
        Err(std::io::Error::other("invalid MQTT fixture frame"))
    }
    fn string(bytes: &[u8], cursor: &mut usize) -> String {
        let length = usize::from(u16::from_be_bytes([bytes[*cursor], bytes[*cursor + 1]]));
        *cursor += 2;
        let value = String::from_utf8(bytes[*cursor..*cursor + length].to_vec()).unwrap();
        *cursor += length;
        value
    }
    fn publish(stream: &mut TcpStream, topic: &str, text: &str) {
        let length = 2 + topic.len() + text.len();
        assert!(length < 128);
        let mut packet = vec![0x30, length as u8];
        packet.extend((topic.len() as u16).to_be_bytes());
        packet.extend(topic.as_bytes());
        packet.extend(text.as_bytes());
        stream.write_all(&packet).unwrap();
    }
    struct Host(Arc<SignalHub>);
    impl ToolHost for Host {
        fn snapshot(&self) -> anyhow::Result<SessionSnapshot> {
            Ok(SessionSnapshot {
                signals: self.0.status(true),
                signal_bindings: self.0.cached_bindings(),
                ..SessionSnapshot::default()
            })
        }
        fn execute(&self, _: Option<&str>, command: SessionCommand) -> anyhow::Result<Value> {
            capturefab::signals::hub::run(&self.0, command)
        }
        fn has_session(&self) -> bool {
            true
        }
    }
    fn no_secrets(text: &str) {
        for value in [USER, PASSWORD, ROTATED, VARIABLE] {
            assert!(
                !text.contains(value),
                "protected signal credentials appeared outside the private fixture"
            );
        }
    }

    #[test]
    fn protected_mqtt_credentials_reach_stdin_only_and_rotation_refuses_helper_restart() {
        if std::env::var_os("CAPTUREFAB_AUTH_MQTT_CHILD").is_none() {
            let dir = Directory(std::env::temp_dir().join(format!(
                    "capturefab-signal-helper-auth-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )));
            std::fs::create_dir_all(&dir.0).unwrap();
            let output=Command::new(std::env::current_exe().unwrap())
                .env_clear().env("PATH","/usr/bin:/bin").env("HOME",&dir.0)
                .env("CAPTUREFAB_SESSION_DIR",dir.0.join("state"))
                .env("CAPTUREFAB_CACHE_DIR",dir.0.join("cache"))
                .env("CAPTUREFAB_EXECUTABLE",env!("CARGO_BIN_EXE_capturefab"))
                .env("CAPTUREFAB_AUTH_MQTT_CHILD","1").env(VARIABLE,PASSWORD)
                .args(["--exact","mqtt::protected_mqtt_credentials_reach_stdin_only_and_rotation_refuses_helper_restart","--nocapture"])
                .output().unwrap();
            let logs = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            no_secrets(&logs);
            assert!(
                output.status.success(),
                "isolated authenticated helper regression failed: {logs}"
            );
            return;
        }
        let broker = Broker::new();
        let endpoint = format!("mqtt://{}/line/#", broker.address);
        let profile = Store::session()
            .save(
                None,
                ProfileDraft {
                    name: "protected-local-broker".into(),
                    endpoint: endpoint.clone(),
                    username: Some(USER.into()),
                    secret: SecretSource::Environment {
                        variable: VARIABLE.into(),
                    },
                },
                None,
            )
            .unwrap();
        assert!(profile.insecure, "plain MQTT remains explicitly insecure");
        let hub = Arc::new(SignalHub::new());
        let receipt = hub.bind(&profile.profile).unwrap();
        no_secrets(&serde_json::to_string(&receipt).unwrap());
        let started = hub
            .start_bound(&endpoint, 100, Vec::new(), 3000, Some(receipt.auth.clone()))
            .unwrap();
        assert_eq!(started.auth.as_ref(), Some(&receipt.auth));
        let deadline = Instant::now() + Duration::from_secs(5);
        while hub.status(true)[0].samples < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let status = hub.status(true);
        assert!(
            status[0].samples >= 2,
            "authenticated helper must receive both fixture samples"
        );
        assert_eq!(
            *broker.logins.lock().unwrap(),
            [(USER.into(), PASSWORD.into())]
        );
        no_secrets(&serde_json::to_string(&status).unwrap());
        let pid = status[0].helper_pid.unwrap();
        let argv = Command::new("/bin/ps")
            .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        assert!(argv.status.success());
        no_secrets(&String::from_utf8_lossy(&argv.stdout));
        let environment = Command::new("/bin/ps")
            .args(["eww", "-p", &pid.to_string(), "-o", "command="])
            .output()
            .unwrap();
        assert!(environment.status.success());
        no_secrets(&String::from_utf8_lossy(&environment.stdout));
        let host = Host(hub.clone());
        let root = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let mut context = Context::gather(
            &host,
            root,
            Default::default(),
            RecordingDefaults::default(),
        )
        .unwrap();
        context.allow_data = true;
        context.data_sources = vec![DataSourceRef::from_binding(&receipt).unwrap()];
        let model = capturefab::tools::data::read(
            &host,
            &context,
            &ToolCall {
                tool: "list_data_sources".into(),
                args: json!({}),
            },
            Origin::Assistant,
            Duration::from_secs(1),
        )
        .unwrap();
        no_secrets(&serde_json::to_string(&model).unwrap());
        assert_eq!(model["sources"].as_array().unwrap().len(), 1);
        assert!(model["sources"][0].get("auth").is_none());
        assert!(
            model["sources"][0]["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tag| tag["value"] == "<redacted> <redacted>")
        );
        // This fresh child runs one test; its other threads only perform
        // socket IO or sleep here. No environment-dependent code is active.
        unsafe {
            std::env::set_var(VARIABLE, ROTATED);
        }
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let status = hub.status(true);
            if status[0].state == capturefab::signals::SourceState::Failed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "rotated environment credentials must prevent helper restart"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            broker.logins.lock().unwrap().len(),
            1,
            "no replacement helper may connect with rotated or fallback credentials"
        );
        assert!(hub.cached_bindings().is_empty());
        let final_status = hub.status(true);
        assert_eq!(final_status[0].capture_id, started.capture_id);
        assert!(
            final_status[0]
                .error
                .as_deref()
                .unwrap()
                .contains("credentials changed")
        );
        no_secrets(&serde_json::to_string(&final_status).unwrap());
        no_secrets(&serde_json::to_string(&hub.logs()).unwrap());
        hub.stop_if(&started.id, &started.capture_id, Duration::from_secs(3))
            .unwrap();
    }
}
