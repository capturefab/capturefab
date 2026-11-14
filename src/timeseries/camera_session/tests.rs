use super::*;
use crate::types::classify;

#[test]
fn monitor_source_mutations_fail_during_pure_whole_config_validation() {
    let selector = "gige-monitor://192.0.2.10?interface=192.0.2.20&mac=020000000001";
    for protocol in ["camera", "video"] {
        for (field, value) in [
            ("settings", json!(["Width=4"])),
            ("action_device_key", json!(0)),
            ("warmup_ms", json!(1)),
        ] {
            let mut declared = source("monitor", protocol, selector, None);
            declared[field] = value;
            let error = configuration(&[declared]).validate().unwrap_err();
            assert_eq!(classify(&error).kind, ErrorKind::Unsupported);
        }
        configuration(&[source("monitor", protocol, selector, None)])
            .validate()
            .unwrap();
    }
}

#[test]
fn alternate_monitor_uri_spellings_share_one_industrial_camera_owner() {
    let first = "gige-monitor://192.0.2.10?interface=192.0.2.20&mac=0200000000AB";
    let second = "gige-monitor://192.0.2.10?mac=0200000000ab&interface=192.0.2.20";
    let mut cameras = Cameras::default();
    let a = cameras.get(first, &[], None).unwrap();
    let b = cameras.get(second, &[], None).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(cameras.sessions.len(), 1);
    assert!(cameras.get(first, &[], Some(1)).is_err());
    assert_eq!(cameras.sessions.len(), 1);
}

fn configuration(sources: &[Value]) -> super::super::Config {
    serde_json::from_value(json!({"version":1,"sources":sources})).unwrap()
}

fn source(id: &str, protocol: &str, selector: &str, key: Option<u32>) -> Value {
    let output = if protocol == "video" {
        "clip.mkv"
    } else {
        "frame.png"
    };
    let mut value = json!({"id":id,"protocol":protocol,"camera":selector,"output":output});
    if let Some(key) = key {
        value["action_device_key"] = json!(key);
    }
    value
}

fn setup() -> Setup {
    Setup {
        initialized: false,
        settings: Vec::new(),
        action_device_key: None,
        key_state: KeyState::Pending,
    }
}

#[test]
fn exact_optional_key_numbers_reach_both_source_configs_without_float_rounding() {
    for protocol in ["camera", "video"] {
        let output = if protocol == "video" {
            "clip.mkv"
        } else {
            "frame.png"
        };
        for (number, expected) in [
            ("0", Some(0)),
            ("4.294967295e9", Some(u32::MAX)),
            ("null", None),
        ] {
            let input = format!(
                r#"{{"version":1,"sources":[{{"id":"source","protocol":"{protocol}","camera":"sim:0","output":"{output}","action_device_key":{number}}}]}}"#
            );
            for config in [
                serde_json::from_str::<super::super::Config>(&input).unwrap(),
                serde_json::from_value(serde_json::from_str::<Value>(&input).unwrap()).unwrap(),
            ] {
                config.validate().unwrap();
                assert_eq!(declaration(&config.sources[0].adapter).unwrap().1, expected);
            }
        }
        for number in [
            "-1",
            "4294967296",
            "1.00000000000000000001",
            "true",
            "\"1\"",
        ] {
            let input = format!(
                r#"{{"version":1,"sources":[{{"id":"source","protocol":"{protocol}","camera":"sim:0","output":"{output}","action_device_key":{number}}}]}}"#
            );
            serde_json::from_str::<super::super::Config>(&input).unwrap_err();
            serde_json::from_value::<super::super::Config>(
                serde_json::from_str::<Value>(&input).unwrap(),
            )
            .unwrap_err();
        }
    }
}

#[test]
fn declarations_merge_by_exact_selector_in_any_source_order_before_preparation() {
    let still = source("still", "camera", "sim:0", Some(0));
    let video = source("video", "video", "sim:0", None);
    for sources in [[still.clone(), video.clone()], [video, still]] {
        configuration(&sources).validate().unwrap();
    }
    configuration(&[
        source("still", "camera", "sim:0", Some(u32::MAX)),
        source("video", "video", "sim:0", Some(u32::MAX)),
    ])
    .validate()
    .unwrap();
    configuration(&[
        source("still", "camera", "sim:0", Some(0)),
        source("video", "video", "sim:1", Some(1)),
    ])
    .validate()
    .unwrap();
    let config = configuration(&[
        source("still", "camera", "sim:0", Some(0)),
        source("video", "video", "sim:0", Some(1)),
    ]);
    assert_eq!(
        classify(&config.validate().unwrap_err()).kind,
        ErrorKind::Usage
    );
    let mut merged = setup();
    merged.merge_key(None, &[]).unwrap();
    merged.merge_key(Some(0), &[]).unwrap();
    merged.merge_key(Some(0), &[]).unwrap();
    merged.merge_key(None, &[]).unwrap();
    assert_eq!(merged.action_device_key, Some(0));
    merged.merge_key(Some(1), &[]).unwrap_err();
    assert_eq!(merged.action_device_key, Some(0));
}

#[test]
fn only_key_bearing_shared_cameras_reject_the_rw_key_path() {
    for setting in ["ActionDeviceKey=0", "ActionDeviceKey[1]=0"] {
        let still = source("still", "camera", "sim:0", Some(1));
        let mut video = source("video", "video", "sim:0", None);
        video["settings"] = json!([setting]);
        for sources in [[still.clone(), video.clone()], [video, still]] {
            assert_eq!(
                classify(&configuration(&sources).validate().unwrap_err()).kind,
                ErrorKind::Usage
            );
        }
        let mut legacy = source("legacy", "camera", "sim:0", None);
        legacy["settings"] = json!([setting]);
        configuration(&[legacy]).validate().unwrap();
    }
}

#[test]
fn first_typed_failure_is_sticky_and_late_declarations_do_not_revive_it() {
    let mut setup = setup();
    setup.merge_key(Some(17), &[]).unwrap();
    let first = crate::types::with_details(
        Categorized::new(ErrorKind::PermissionDenied, "producer denied key")
            .hint("first hint")
            .details(json!({"native_code":-1005,"write_may_have_applied":true}))
            .into(),
        json!({"initialization_stage":"write_key"}),
    );
    let a = classify(&setup.fail(first));
    let b = classify(&setup.fail(Categorized::error(ErrorKind::Timeout, "later expiry")));
    assert_eq!(a.kind, ErrorKind::PermissionDenied);
    assert_eq!(a.kind, b.kind);
    assert_eq!(a.hint, b.hint);
    assert_eq!(a.details, b.details);
    assert_eq!(a.details.unwrap()["native_code"], -1005);
    setup.merge_key(None, &[]).unwrap_err();
    setup.merge_key(Some(17), &[]).unwrap_err();
    assert!(matches!(setup.key_state, KeyState::Failed(_)));
    let mut legacy = self::setup();
    legacy.initialized = true;
    legacy.merge_key(None, &[]).unwrap();
    legacy.merge_key(Some(17), &[]).unwrap_err();
}

#[test]
fn an_expired_setup_waiter_cannot_restart_or_mutate_the_active_initialization() {
    let mut cameras = Cameras::default();
    let shared = cameras.get("sim:0", &[], Some(17)).unwrap();
    let setup = shared.setup.lock().unwrap();
    let error = std::thread::scope(|scope| {
        scope
            .spawn(|| shared.initialize(Instant::now() + Duration::from_millis(20)))
            .join()
            .unwrap()
            .unwrap_err()
    });
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert!(matches!(setup.key_state, KeyState::Pending));
    assert!(!setup.initialized);
    assert_eq!(setup.action_device_key, Some(17));
    assert_eq!(shared.handle.snapshot().cameras.len(), 0);
    drop(setup);
    // The active owner subsequently fails its own cutoff before any Connect.
    // This first owner failure sticks; a later caller cannot restart setup.
    let first = shared.initialize(Instant::now()).unwrap_err();
    let later = shared
        .initialize(Instant::now() + Duration::from_secs(2))
        .unwrap_err();
    assert_eq!(classify(&first).kind, ErrorKind::Timeout);
    assert_eq!(classify(&first).details, classify(&later).details);
    assert_eq!(shared.handle.snapshot().cameras.len(), 0);
}
