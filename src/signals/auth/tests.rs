use super::*;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[derive(Default)]
struct Memory {
    passwords: Mutex<BTreeMap<String, String>>,
    environment: Mutex<BTreeMap<String, String>>,
    calls: Mutex<Vec<&'static str>>,
    break_metadata: Mutex<Option<PathBuf>>,
    fail_delete: AtomicBool,
    fail_write_after: AtomicBool,
    delay_ms: AtomicU64,
}
impl Memory {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
    fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}
impl SecretStore for Memory {
    fn environment(&self, name: &str) -> anyhow::Result<Option<Secret>> {
        self.calls.lock().unwrap().push("environment");
        std::thread::sleep(std::time::Duration::from_millis(
            self.delay_ms.load(Ordering::SeqCst),
        ));
        self.environment
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .map(|value| Secret::new(value.into()))
            .transpose()
    }
    fn read_keychain(&self, account: &str) -> anyhow::Result<Option<Secret>> {
        self.calls.lock().unwrap().push("read");
        self.passwords
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .map(|value| Secret::new(value.into()))
            .transpose()
    }
    fn write_keychain(&self, account: &str, password: &Secret) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("write");
        self.passwords
            .lock()
            .unwrap()
            .insert(account.into(), password.expose().into());
        if let Some(path) = self.break_metadata.lock().unwrap().take() {
            fs::create_dir(path)?;
        }
        anyhow::ensure!(
            !self.fail_write_after.load(Ordering::SeqCst),
            "injected uncertain write CANARY-password"
        );
        Ok(())
    }
    fn delete_keychain(&self, account: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("delete");
        anyhow::ensure!(
            !self.fail_delete.load(Ordering::SeqCst),
            "injected credential cleanup failure"
        );
        self.passwords.lock().unwrap().remove(account);
        Ok(())
    }
}
struct Fixture {
    dir: PathBuf,
    store: Store,
    secrets: Arc<Memory>,
}
impl Fixture {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("capturefab-signal-auth-{}", random_id().unwrap()));
        let secrets = Arc::new(Memory::default());
        let store = Store::at(&dir).with_secret_store(secrets.clone());
        Self {
            dir,
            store,
            secrets,
        }
    }
    fn environment(&self, name: &str) -> ProfileInfo {
        self.store
            .save(
                None,
                ProfileDraft {
                    name: name.into(),
                    endpoint: "mqtts://broker.invalid/line/#".into(),
                    username: Some("CANARY-private-user".into()),
                    secret: SecretSource::Environment {
                        variable: "CAPTUREFAB_SIGNAL_TEST_PASSWORD".into(),
                    },
                },
                None,
            )
            .unwrap()
    }
    fn password(&self, value: &str) {
        self.secrets
            .environment
            .lock()
            .unwrap()
            .insert("CAPTUREFAB_SIGNAL_TEST_PASSWORD".into(), value.into());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn password(value: &str) -> Secret {
    Secret::new(value.to_owned().into()).unwrap()
}

#[test]
fn metadata_and_validation_never_resolve_credentials_or_serialize_usernames() {
    let fixture = Fixture::new();
    let profile = fixture.environment("line-one");
    assert!(fixture.secrets.calls().is_empty());
    for _ in 0..2 {
        assert_eq!(fixture.store.list().unwrap(), vec![profile.clone()]);
        assert_eq!(fixture.store.get(&profile.profile).unwrap(), profile);
        let draft = fixture.store.load_draft(&profile.profile).unwrap();
        draft.validate().unwrap();
        assert_eq!(draft.username.as_deref(), Some("CANARY-private-user"));
        assert!(!format!("{draft:?}").contains("CANARY-private-user"));
        assert!(!format!("{draft:?}").contains("CAPTUREFAB_SIGNAL_TEST_PASSWORD"));
    }
    assert!(fixture.secrets.calls().is_empty());
    let safe = serde_json::to_string(&profile).unwrap();
    assert!(
        !safe.contains("CANARY-private-user") && !safe.contains("CAPTUREFAB_SIGNAL_TEST_PASSWORD")
    );
    assert!(!profile.insecure);
    let mut draft = fixture.store.load_draft(&profile.profile).unwrap();
    draft.endpoint = "opc.tcp://server.invalid:4840/?node=i=2258".into();
    let opc = fixture
        .store
        .save(Some(&profile.profile), draft, None)
        .unwrap();
    assert!(opc.insecure);
    assert_eq!(opc.profile.id, profile.profile.id);
    assert_ne!(opc.profile.revision, profile.profile.revision);
    assert!(fixture.secrets.calls().is_empty());
}

#[test]
fn explicit_bind_pins_this_coordinator_and_credentials_until_rebound() {
    let fixture = Fixture::new();
    fixture.password("CANARY-first-password");
    let profile = fixture.environment("line-one");
    let session = Session::new(fixture.store.clone());
    let receipt = session.bind(&profile.profile).unwrap();
    assert_eq!(fixture.secrets.calls(), ["environment"]);
    fixture.secrets.clear_calls();
    assert_eq!(session.cached_bindings(), std::slice::from_ref(&receipt));
    session
        .validate_cached(&receipt.auth, &receipt.endpoint)
        .unwrap();
    assert!(fixture.secrets.calls().is_empty());
    let other = Session::new(fixture.store.clone());
    assert!(
        other
            .with_credentials(&receipt.auth, &receipt.endpoint, |_| Ok(()))
            .is_err()
    );
    assert!(
        session
            .with_credentials(&receipt.auth, "mqtts://other.invalid/line/#", |_| Ok(()))
            .is_err()
    );
    assert!(fixture.secrets.calls().is_empty());
    session
        .with_credentials(&receipt.auth, &receipt.endpoint, |credentials| {
            let credentials = credentials.unwrap();
            let wire = serde_json::to_value(&credentials)?;
            assert_eq!(wire["username"], "CANARY-private-user");
            assert_eq!(wire["password"], "CANARY-first-password");
            assert!(!format!("{credentials:?}").contains("CANARY"));
            let mut parsed = crate::signals::parse(&receipt.endpoint)?;
            credentials.apply(&mut parsed)?;
            assert_eq!(parsed.user.as_deref(), Some("CANARY-private-user"));
            assert_eq!(parsed.password.as_deref(), Some("CANARY-first-password"));
            Ok(())
        })
        .unwrap();
    let safe = serde_json::to_string(&receipt).unwrap();
    assert!(!safe.contains("CANARY") && !safe.contains("CAPTUREFAB_SIGNAL_TEST_PASSWORD"));
    fixture.password("CANARY-rotated-password");
    let launched = AtomicBool::new(false);
    let changed = session
        .with_credentials(&receipt.auth, &receipt.endpoint, |_| {
            launched.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap_err();
    assert!(!launched.load(Ordering::SeqCst));
    assert!(changed.to_string().contains("credentials changed"));
    assert!(!format!("{changed:#}").contains("CANARY"));
    assert!(session.cached_bindings().is_empty());
    let replacement = session.bind(&profile.profile).unwrap();
    assert_ne!(replacement.auth.epoch, receipt.auth.epoch);
    fixture.secrets.clear_calls();
    assert!(
        session
            .with_credentials(&receipt.auth, &receipt.endpoint, |_| Ok(()))
            .is_err()
    );
    assert!(fixture.secrets.calls().is_empty());
}

#[test]
fn profile_replacement_and_same_endpoint_rebind_revoke_old_helper_epochs() {
    let fixture = Fixture::new();
    fixture.password("CANARY-password");
    let first = fixture.environment("line-one");
    let second = fixture.environment("line-two");
    let session = Session::new(fixture.store.clone());
    let old = session.bind(&first.profile).unwrap();
    let current = session.bind(&second.profile).unwrap();
    assert_eq!(session.cached_bindings(), std::slice::from_ref(&current));
    fixture.secrets.clear_calls();
    assert!(
        session
            .with_credentials(&old.auth, &old.endpoint, |_| Ok(()))
            .is_err()
    );
    assert!(fixture.secrets.calls().is_empty());
    let draft = fixture.store.load_draft(&second.profile).unwrap();
    let updated = fixture
        .store
        .save(Some(&second.profile), draft, None)
        .unwrap();
    assert_ne!(updated.profile.revision, second.profile.revision);
    assert!(
        session
            .with_credentials(&current.auth, &current.endpoint, |_| Ok(()))
            .is_err()
    );
    assert!(fixture.secrets.calls().is_empty());
    assert!(session.cached_bindings().is_empty());
    let fresh = session.bind(&updated.profile).unwrap();
    fixture.store.delete(&updated.profile).unwrap();
    fixture.secrets.clear_calls();
    assert!(
        session
            .with_credentials(&fresh.auth, &fresh.endpoint, |_| Ok(()))
            .is_err()
    );
    assert!(fixture.secrets.calls().is_empty());
}

#[test]
fn a_slow_credential_lookup_never_starts_an_expired_operation() {
    let fixture = Fixture::new();
    fixture.password("CANARY-password");
    let profile = fixture.environment("line-one");
    let session = Session::new(fixture.store.clone());
    let receipt = session.bind(&profile.profile).unwrap();
    fixture.secrets.clear_calls();
    fixture.secrets.delay_ms.store(40, Ordering::SeqCst);
    let spec = crate::signals::SignalSpec {
        uri: receipt.endpoint.clone(),
        credentials: None,
        mode: crate::signals::Mode::Read,
        interval_ms: 100,
        tags: Vec::new(),
        timeout_ms: 1,
        pki_dir: None,
    };
    let error = crate::signals::process::SignalProcess::spawn_bound(
        &spec,
        &session,
        Some(&receipt.auth),
        Some(std::time::Instant::now() + std::time::Duration::from_millis(1)),
    )
    .err()
    .expect("expired lookup must not launch a helper");
    assert_eq!(fixture.secrets.calls(), ["environment"]);
    assert!(error.to_string().contains("no connection was started"));
    assert_eq!(session.cached_bindings(), [receipt]);
}

#[test]
fn keychain_saves_are_compare_and_save_and_failed_metadata_removes_new_secret() {
    let fixture = Fixture::new();
    let draft = ProfileDraft {
        name: "line-keychain".into(),
        endpoint: "mqtts://broker.invalid/line".into(),
        username: Some("CANARY-user".into()),
        secret: SecretSource::Keychain,
    };
    let saved = fixture
        .store
        .save(None, draft.clone(), Some(password("CANARY-password")))
        .unwrap();
    assert_eq!(fixture.secrets.passwords.lock().unwrap().len(), 1);
    assert!(
        !fs::read_to_string(fixture.dir.join("signal-profiles.json"))
            .unwrap()
            .contains("CANARY-password")
    );
    let updated = fixture
        .store
        .save(Some(&saved.profile), draft.clone(), None)
        .unwrap();
    assert_eq!(updated.profile.id, saved.profile.id);
    assert_ne!(updated.profile.revision, saved.profile.revision);
    assert_eq!(fixture.secrets.passwords.lock().unwrap().len(), 1);
    fixture.secrets.clear_calls();
    assert!(
        fixture
            .store
            .save(Some(&saved.profile), draft, Some(password("CANARY-stale")))
            .is_err()
    );
    assert!(fixture.secrets.calls().is_empty());
    assert_eq!(fixture.store.get(&updated.profile).unwrap(), updated);
    fixture.store.delete(&updated.profile).unwrap();
    assert!(fixture.secrets.passwords.lock().unwrap().is_empty());

    let broken = Fixture::new();
    *broken.secrets.break_metadata.lock().unwrap() = Some(broken.dir.join("signal-profiles.json"));
    let failed = broken.store.save(
        None,
        ProfileDraft {
            name: "broken".into(),
            endpoint: "mqtt://broker.invalid/line".into(),
            username: Some("CANARY-user".into()),
            secret: SecretSource::Keychain,
        },
        Some(password("CANARY-password")),
    );
    assert!(failed.is_err());
    assert!(broken.secrets.passwords.lock().unwrap().is_empty());
    assert_eq!(broken.secrets.calls(), ["write", "delete"]);
}

#[test]
fn uncertain_keychain_writes_remove_new_accounts_or_track_bounded_cleanup() {
    let fixture = Fixture::new();
    let draft = ProfileDraft {
        name: "keychain".into(),
        endpoint: "mqtts://broker.invalid/line".into(),
        username: Some("CANARY-user".into()),
        secret: SecretSource::Keychain,
    };
    let previous = fixture
        .store
        .save(None, draft.clone(), Some(password("CANARY-old-password")))
        .unwrap();
    fixture.secrets.clear_calls();
    fixture
        .secrets
        .fail_write_after
        .store(true, Ordering::SeqCst);
    let failed = fixture
        .store
        .save(
            Some(&previous.profile),
            draft.clone(),
            Some(password("CANARY-password")),
        )
        .unwrap_err();
    assert!(failed.to_string().contains("unused credential was removed"));
    assert!(!format!("{failed:#}").contains("CANARY"));
    assert_eq!(fixture.secrets.calls(), ["write", "delete"]);
    assert_eq!(
        fixture.store.list().unwrap(),
        std::slice::from_ref(&previous)
    );
    assert_eq!(fixture.secrets.passwords.lock().unwrap().len(), 1);

    fixture.secrets.fail_delete.store(true, Ordering::SeqCst);
    let failed = fixture
        .store
        .save(
            Some(&previous.profile),
            draft,
            Some(password("CANARY-password")),
        )
        .unwrap_err();
    assert!(failed.to_string().contains("cleanup was recorded"));
    assert!(!format!("{failed:#}").contains("CANARY"));
    assert_eq!(fixture.store.list().unwrap(), [previous]);
    assert_eq!(fixture.secrets.passwords.lock().unwrap().len(), 2);
    let path = fixture.dir.join("signal-profiles.json");
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(metadata["retired_accounts"].as_array().unwrap().len(), 1);
    assert!(!metadata.to_string().contains("CANARY-password"));

    // Later explicit local mutations retry cleanup without changing the
    // still-valid previous profile or probing credentials during list.
    fixture.secrets.clear_calls();
    fixture.store.list().unwrap();
    assert!(fixture.secrets.calls().is_empty());
    fixture.secrets.fail_delete.store(false, Ordering::SeqCst);
    fixture
        .secrets
        .fail_write_after
        .store(false, Ordering::SeqCst);
    fixture
        .store
        .save(
            None,
            ProfileDraft {
                name: "simulated".into(),
                endpoint: "sim-signal://line-one".into(),
                username: None,
                secret: SecretSource::None,
            },
            None,
        )
        .unwrap();
    assert_eq!(fixture.secrets.calls(), ["delete"]);
    assert_eq!(fixture.secrets.passwords.lock().unwrap().len(), 1);
    let metadata: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(metadata["retired_accounts"].as_array().unwrap().is_empty());

    let broken = Fixture::new();
    broken
        .secrets
        .fail_write_after
        .store(true, Ordering::SeqCst);
    broken.secrets.fail_delete.store(true, Ordering::SeqCst);
    *broken.secrets.break_metadata.lock().unwrap() = Some(broken.dir.join("signal-profiles.json"));
    let failed = broken
        .store
        .save(
            None,
            ProfileDraft {
                name: "broken".into(),
                endpoint: "mqtt://broker.invalid/line".into(),
                username: Some("CANARY-user".into()),
                secret: SecretSource::Keychain,
            },
            Some(password("CANARY-password")),
        )
        .unwrap_err();
    assert!(failed.to_string().contains("cleanup could not be recorded"));
    assert!(!format!("{failed:#}").contains("CANARY"));
    assert_eq!(broken.secrets.calls(), ["write", "delete"]);
}

#[test]
fn anonymous_profile_transport_metadata_still_marks_unencrypted_connections() {
    let fixture = Fixture::new();
    for (index, (endpoint, insecure)) in [
        ("mqtt://broker.invalid/line", true),
        ("mqtts://broker.invalid/line", false),
        ("opc.tcp://server.invalid:4840/?node=i=2258", true),
        ("modbus-tcp://plc.invalid/1/holding/0?count=1", true),
        ("mtconnect://agent.invalid", true),
        ("mtconnects://agent.invalid", false),
        ("sim-signal://line-one", false),
    ]
    .into_iter()
    .enumerate()
    {
        let profile = fixture
            .store
            .save(
                None,
                ProfileDraft {
                    name: format!("transport-{index}"),
                    endpoint: endpoint.into(),
                    username: None,
                    secret: SecretSource::None,
                },
                None,
            )
            .unwrap();
        assert_eq!(profile.insecure, insecure);
    }
    assert!(fixture.secrets.calls().is_empty());
}

#[cfg(not(feature = "keychain"))]
#[test]
fn unavailable_os_store_never_creates_a_profile_or_phantom_cleanup_record() {
    let fixture = Fixture::new();
    let native = Store::at(&fixture.dir);
    let error = native
        .save(
            None,
            ProfileDraft {
                name: "unavailable".into(),
                endpoint: "mqtts://broker.invalid/line".into(),
                username: Some("CANARY-user".into()),
                secret: SecretSource::Keychain,
            },
            Some(password("CANARY-password")),
        )
        .unwrap_err();
    assert_eq!(
        crate::types::error_transport_code(&error),
        Some("unsupported")
    );
    assert!(!format!("{error:#}").contains("CANARY"));
    assert!(!fixture.dir.exists());
}

#[test]
fn helper_wire_credentials_scrub_text_metadata_and_never_override_embedded_auth() {
    let credentials: SignalCredentials = serde_json::from_value(
        serde_json::json!({"username":"CANARY-user","password":"CANARY-password"}),
    )
    .unwrap();
    let mut line = crate::signals::Line::Browse {
        items: vec![crate::signals::BrowseItem {
            id: "CANARY-user/id".into(),
            name: "CANARY-password".into(),
            kind: "text".into(),
            data_type: None,
            unit: None,
            value: Some(crate::signals::Value::Text(
                "CANARY-user CANARY-password".into(),
            )),
        }],
    };
    credentials.scrub_line(&mut line);
    let public = serde_json::to_string(&line).unwrap();
    assert!(!public.contains("CANARY"));
    assert_eq!(
        credentials.redact("failed CANARY-user / CANARY-password"),
        "failed <redacted> / <redacted>"
    );
    let mut embedded =
        crate::signals::parse("mqtt://existing:existing@broker.invalid/line").unwrap();
    assert!(credentials.apply(&mut embedded).is_err());
    assert_eq!(embedded.user.as_deref(), Some("existing"));
    let mut unsupported = crate::signals::parse("sim-signal://line-one").unwrap();
    assert!(credentials.apply(&mut unsupported).is_err());
    for endpoint in [
        "mqtt://user:password@broker.invalid/line",
        "mqtt://user@broker.invalid/line",
        "mqtt://broker.invalid/line?password=CANARY-password",
        "mqtt://broker.invalid/line?token=CANARY-password",
    ] {
        assert!(validate_endpoint(endpoint).is_err());
        assert!(
            ProfileDraft {
                name: "bad-endpoint".into(),
                endpoint: endpoint.into(),
                username: None,
                secret: SecretSource::None
            }
            .validate()
            .is_err()
        );
    }
    let mut draft = ProfileDraft {
        name: "valid".into(),
        endpoint: "mqtt://broker.invalid/line".into(),
        username: Some("user".into()),
        secret: SecretSource::Environment {
            variable: "OPENAI_API_KEY".into(),
        },
    };
    assert!(draft.validate().is_err());
    draft.secret = SecretSource::None;
    assert!(draft.validate().is_err());
    assert!(Secret::new("contains\nnewline".to_owned().into()).is_err());
    assert!(Secret::new(String::new().into()).is_err());

    let credentials: SignalCredentials = serde_json::from_value(
        serde_json::json!({"username":"connected","password":"CANARY-password"}),
    )
    .unwrap();
    let mut state = crate::signals::Line::State {
        state: "connected".into(),
        error: Some("connected rejected CANARY-password".into()),
    };
    credentials.scrub_line(&mut state);
    assert!(
        matches!(state, crate::signals::Line::State { state, error: Some(error) }
        if state == "connected" && error == "<redacted> rejected <redacted>")
    );
}

#[test]
fn anonymous_profiles_need_no_secret_and_corrupt_or_public_metadata_is_refused() {
    let fixture = Fixture::new();
    let profile = fixture
        .store
        .save(
            None,
            ProfileDraft {
                name: "simulated".into(),
                endpoint: "sim-signal://line-one".into(),
                username: None,
                secret: SecretSource::None,
            },
            None,
        )
        .unwrap();
    let session = Session::new(fixture.store.clone());
    let receipt = session.bind(&profile.profile).unwrap();
    session
        .with_credentials(&receipt.auth, &receipt.endpoint, |credentials| {
            assert!(credentials.is_none());
            Ok(())
        })
        .unwrap();
    assert!(fixture.secrets.calls().is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = fixture.dir.join("signal-profiles.json");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();
        assert!(fixture.store.list().is_err());
    }
}
