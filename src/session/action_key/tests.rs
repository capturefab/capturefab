use super::*;
use crate::session::protocol::WorkerCommand;

#[test]
fn public_numbers_and_private_expiry_are_exact_and_cannot_reset_or_bypass_admission() {
    for number in ["0", "4294967295", "4.294967295e9"] {
        let text = format!(r#"{{"op":"write_action_device_key","key":{number},"timeout_ms":3e3}}"#);
        let command = SessionCommand::from_json_slice(text.as_bytes()).unwrap();
        command.validate().unwrap();
        WorkerCommand::from(command).validate().unwrap_err();
    }
    for number in ["-1", "4294967296", "1.0000000000000000001", "true", "\"1\""] {
        let text =
            format!(r#"{{"op":"write_action_device_key","key":{number},"timeout_ms":1000}}"#);
        SessionCommand::from_json_slice(text.as_bytes()).expect_err(number);
    }
    for text in [
        r#"{"op":"write_action_device_key","key":1,"timeout_ms":0}"#,
        r#"{"op":"write_action_device_key","key":1,"timeout_ms":30001}"#,
    ] {
        SessionCommand::from_json_slice(text.as_bytes())
            .unwrap()
            .validate()
            .unwrap_err();
    }
    SessionCommand::from_json_slice(
        br#"{"op":"write_action_device_key","key":1,"timeout_ms":1000,"deadline_ns":1}"#,
    )
    .unwrap_err();
    SessionCommand::from_json_slice(
        br#"{"op":"write_action_device_key_until","key":1,"deadline_ns":1}"#,
    )
    .unwrap_err();
    serde_json::from_str::<WorkerCommand>(
        r#"{"op":"write_action_device_key_until","key":1,"deadline_ns":1,"timeout_ms":30000}"#,
    )
    .unwrap_err();
    let future = now_ns().unwrap() + 1_000_000_000;
    let command = WorkerCommand::WriteActionDeviceKeyUntil {
        key: u32::MAX,
        deadline_ns: future,
    };
    command.validate().unwrap();
    assert!(command.session().is_none() && command.changes_settings());
    let wire = serde_json::to_string(&command).unwrap();
    let round: WorkerCommand = serde_json::from_str(&wire).unwrap();
    assert!(
        matches!(round,WorkerCommand::WriteActionDeviceKeyUntil {key:u32::MAX,deadline_ns} if deadline_ns==future)
    );
    let error = instant(1).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert_eq!(
        crate::types::classify(&error).details.unwrap()["write_may_have_applied"],
        false
    );
    instant(now_ns().unwrap() + MAX_NS + 1_000_000_000).unwrap_err();
    let anchor = Instant::now();
    let converted = instant(future).unwrap();
    assert!(converted > anchor && converted <= Instant::now() + Duration::from_secs(1));
}

#[test]
fn ambiguity_keeps_typed_cause_and_never_turns_dispatch_loss_into_no_write() {
    let no_write = no_write(
        Categorized::new(ErrorKind::FeatureLocked, "locked by acquisition")
            .details(json!({"locked_by":"AcquisitionStart"}))
            .into(),
    );
    let preserved = uncertain(no_write);
    let classified = crate::types::classify(&preserved);
    assert_eq!(classified.kind, ErrorKind::FeatureLocked);
    assert_eq!(
        classified.details.as_ref().unwrap()["write_may_have_applied"],
        false
    );
    let uncertain = dispatched(&preserved, false);
    let details = crate::types::classify(&uncertain).details.unwrap();
    assert_eq!(details["write_may_have_applied"], true);
    assert!(details.get("write_attempted").is_none());
    assert_eq!(details["locked_by"], "AcquisitionStart");
}

#[cfg(unix)]
fn held(handle: &SessionHandle) -> (Arc<Held>, Arc<Worker>) {
    let held = crate::session::tests::held_for_retirement_test();
    let worker = Arc::new(crate::session::tests::test_worker());
    *held.worker.write().unwrap() = Some(worker.clone());
    handle
        .inner
        .cameras
        .write()
        .unwrap()
        .insert("sim:0".into(), held.clone());
    handle.inner.slots.store(1, Ordering::Release);
    (held, worker)
}

#[test]
#[cfg(unix)]
fn queued_key_is_pinned_to_both_held_and_live_worker_without_automatic_reconnect() {
    for replace_held in [false, true] {
        let handle = SessionHandle::new();
        let (old, worker) = held(&handle);
        let barrier = handle
            .command_turn(Some("sim:0"), &SessionCommand::Info)
            .unwrap();
        let reply = handle
            .submit_to(
                "sim:0",
                SessionCommand::WriteActionDeviceKey {
                    key: 7,
                    timeout_ms: 3000,
                },
            )
            .unwrap();
        let replacement = Arc::new(crate::session::tests::test_worker());
        if replace_held {
            let fresh = crate::session::tests::held_for_retirement_test();
            *fresh.worker.write().unwrap() = Some(replacement.clone());
            handle
                .inner
                .cameras
                .write()
                .unwrap()
                .insert("sim:0".into(), fresh);
        } else {
            *old.worker.write().unwrap() = Some(replacement.clone());
        }
        drop(barrier);
        let error = reply
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Disconnected);
        assert_eq!(
            crate::types::classify(&error).details.unwrap()["write_attempted"],
            false
        );
        assert!(
            worker
                .status(Duration::from_secs(5))
                .unwrap()
                .connected
                .is_none()
        );
        assert!(
            replacement
                .status(Duration::from_secs(5))
                .unwrap()
                .connected
                .is_none()
        );
        worker.close();
        assert!(worker.wait_exit(Instant::now() + Duration::from_secs(5)));
        handle.shutdown();
    }
}

#[test]
#[cfg(unix)]
fn expired_order_wait_and_nonterminal_crash_never_reopen_or_submit_the_key() {
    for crash in [false, true] {
        let handle = SessionHandle::new();
        let (held, worker) = held(&handle);
        let mut barrier = Some(
            handle
                .command_turn(Some("sim:0"), &SessionCommand::Info)
                .unwrap(),
        );
        let reply = handle
            .submit_to(
                "sim:0",
                SessionCommand::WriteActionDeviceKey {
                    key: 8,
                    timeout_ms: if crash { 3000 } else { 20 },
                },
            )
            .unwrap();
        if crash {
            worker.kill();
            assert!(worker.wait_exit(Instant::now() + Duration::from_secs(5)));
            drop(barrier.take());
        }
        // For expiry the earlier ticket remains held: only a deadline-aware
        // wait can answer now, rather than first entering the camera gate.
        let error = reply
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            if crash {
                ErrorKind::Disconnected
            } else {
                ErrorKind::Timeout
            }
        );
        assert_eq!(
            crate::types::classify(&error).details.unwrap()["write_may_have_applied"],
            false
        );
        if crash {
            assert!(held.worker().is_none());
        } else {
            assert!(Arc::ptr_eq(&held.worker().unwrap(), &worker));
            assert!(
                worker
                    .status(Duration::from_secs(5))
                    .unwrap()
                    .connected
                    .is_none()
            );
        }
        drop(barrier);
        handle.shutdown();
    }
}

#[test]
#[cfg(unix)]
fn retirement_and_known_cancel_preserve_the_exact_cause_and_existing_unknown_config() {
    let handle = SessionHandle::new();
    let (held, worker) = held(&handle);
    held.config_unknown.store(true, Ordering::Release);
    let command = SessionCommand::WriteActionDeviceKey {
        key: 9,
        timeout_ms: 3000,
    };
    let action = Admission::capture(&handle, Some("sim:0"), &command)
        .unwrap()
        .unwrap();
    let cancel = Cancel::default();
    cancel.cancel();
    let error = action.execute(&handle, Some(&cancel)).unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Interrupted);
    assert_eq!(
        crate::types::classify(&error).details.unwrap()["write_attempted"],
        false
    );
    let barrier = handle
        .command_turn(Some("sim:0"), &SessionCommand::Info)
        .unwrap();
    let reply = handle.submit_to("sim:0", command).unwrap();
    held.remember_retirement(
        json!({"code":"disconnected","message":"first ownership cause",
        "details":{"controller_ownership":"retired","reason":"heartbeat lease expired"}}),
    );
    held.remember_retirement(json!({"code":"disconnected","message":"later cause",
        "details":{"controller_ownership":"retired","reason":"replacement"}}));
    drop(barrier);
    let error = reply
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    let classified = crate::types::classify(&error);
    assert_eq!(classified.kind, ErrorKind::Disconnected);
    let details = classified.details.unwrap();
    assert_eq!(details["reason"], "heartbeat lease expired");
    assert_eq!(details["write_may_have_applied"], false);
    assert!(held.config_unknown.load(Ordering::Acquire));
    assert!(
        worker
            .status(Duration::from_secs(5))
            .unwrap()
            .connected
            .is_none()
    );
    handle.shutdown();
}
