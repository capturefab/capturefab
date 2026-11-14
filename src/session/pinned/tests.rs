use super::*;
use serde_json::json;

fn settings() -> SessionCommand {
    serde_json::from_value(json!({"op":"set_values","values":[{"name":"Width","value":"64"}]}))
        .unwrap()
}

#[test]
fn absolute_wrapper_is_finite_exact_and_cannot_be_a_public_command() {
    let deadline_ns = before(Instant::now() + Duration::from_secs(2)).unwrap();
    let command = WorkerCommand::SessionUntil {
        command: settings(),
        deadline_ns,
    };
    command.validate().unwrap();
    assert!(command.changes_settings());
    assert!(matches!(
        command.session(),
        Some(SessionCommand::SetValues { .. })
    ));
    let encoded = serde_json::to_vec(&command).unwrap();
    SessionCommand::from_json_slice(&encoded).unwrap_err();
    let restored: WorkerCommand = serde_json::from_slice(&encoded).unwrap();
    restored.validate().unwrap();
    assert!(
        matches!(restored, WorkerCommand::SessionUntil { deadline_ns: value, .. } if value == deadline_ns)
    );
    for inner in [
        json!({"op":"disconnect"}),
        json!({"op":"connect","camera":"sim:0","timeout_ms":1000}),
        json!({"op":"set_values","values":[],"dry_run":true}),
        json!({"op":"write_action_device_key","key":1,"timeout_ms":1000}),
    ] {
        let command: WorkerCommand = serde_json::from_value(json!({"op":"session_until",
            "command":inner,"deadline_ns":deadline_ns}))
        .unwrap();
        command.validate().unwrap_err();
    }
    for value in ["-1", "1.1", "18446744073709551616", "true", "\"1\""] {
        let text = format!(
            r#"{{"op":"session_until","command":{{"op":"set_values","values":[]}},"deadline_ns":{value}}}"#
        );
        serde_json::from_str::<WorkerCommand>(&text).unwrap_err();
    }
    serde_json::from_value::<WorkerCommand>(json!({"op":"session_until","command":{
        "op":"set_values","values":[]},"deadline_ns":deadline_ns,"timeout_ms":30000}))
    .unwrap_err();
    let exact: WorkerCommand = serde_json::from_str(
        r#"{"op":"session_until","command":{"op":"set_values","values":[{"name":"Width","value":"64"}]},"deadline_ns":1e0}"#,
    )
    .unwrap();
    assert!(matches!(
        exact,
        WorkerCommand::SessionUntil { deadline_ns: 1, .. }
    ));
    let error = exact.validate().unwrap_err();
    assert_eq!(crate::types::classify(&error).kind, ErrorKind::Timeout);
    assert!(crate::types::classify(&error).details.is_none());
    let invalid: WorkerCommand = serde_json::from_value(json!({"op":"session_until",
        "command":{"op":"set_values","values":[]},"deadline_ns":1}))
    .unwrap();
    assert_eq!(
        crate::types::classify(&invalid.validate().unwrap_err()).kind,
        ErrorKind::Usage
    );
    assert_eq!(
        crate::types::classify(&before(Instant::now()).unwrap_err()).kind,
        ErrorKind::Timeout
    );
    before(Instant::now() + Duration::from_secs(31)).unwrap_err();
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
fn pin_has_no_recovery_and_rejects_foreign_coordinator_or_both_replacement_kinds() {
    for replace_held in [false, true] {
        let handle = SessionHandle::new();
        let (old, worker) = held(&handle);
        let pin = handle.pin_existing_camera("sim:0").unwrap();
        let foreign = SessionHandle::new();
        assert_eq!(
            crate::types::classify(
                &foreign
                    .check_pinned(&pin, Instant::now() + Duration::from_secs(2))
                    .unwrap_err()
            )
            .kind,
            ErrorKind::Usage
        );
        foreign.shutdown();
        let barrier = handle.command_turn(Some("sim:0"), &settings()).unwrap();
        let reply = handle
            .submit_pinned(&pin, settings(), Instant::now() + Duration::from_secs(2))
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
        assert!(
            worker
                .status(Duration::from_secs(2))
                .unwrap()
                .connected
                .is_none()
        );
        assert!(
            replacement
                .status(Duration::from_secs(2))
                .unwrap()
                .connected
                .is_none()
        );
        worker.close();
        assert!(worker.wait_exit(Instant::now() + Duration::from_secs(5)));
        handle.shutdown();
    }
    let handle = SessionHandle::new();
    let (_, worker) = held(&handle);
    worker.kill();
    assert!(worker.wait_exit(Instant::now() + Duration::from_secs(5)));
    assert_eq!(
        crate::types::classify(&handle.pin_existing_camera("sim:0").err().unwrap()).kind,
        ErrorKind::Disconnected
    );
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn ordering_wait_uses_original_cutoff_for_rw_and_key_without_submission_or_renewal() {
    for key in [false, true] {
        let handle = SessionHandle::new();
        let (_, worker) = held(&handle);
        let pin = handle.pin_existing_camera("sim:0").unwrap();
        let barrier = handle.command_turn(Some("sim:0"), &settings()).unwrap();
        let cutoff = Instant::now() + Duration::from_millis(25);
        let reply = if key {
            handle.submit_action_key_before(&pin, 7, cutoff)
        } else {
            handle.submit_pinned(&pin, settings(), cutoff)
        }
        .unwrap();
        // Keep the earlier ticket held: the reply must not need that ticket
        // to be released, and no new relative timeout can start afterward.
        let error = reply
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap_err();
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, ErrorKind::Timeout);
        if key {
            assert_eq!(classified.details.unwrap()["write_may_have_applied"], false);
        }
        assert!(
            worker
                .status(Duration::from_secs(2))
                .unwrap()
                .connected
                .is_none()
        );
        drop(barrier);
        handle.shutdown();
    }
}

#[test]
#[cfg(unix)]
fn pinned_retirement_cause_is_sticky_and_does_not_clear_unknown_configuration() {
    let handle = SessionHandle::new();
    let (held, worker) = held(&handle);
    let pin = handle.pin_existing_camera("sim:0").unwrap();
    held.config_unknown.store(true, Ordering::Release);
    held.remember_retirement(
        json!({"code":"disconnected","message":"original ownership cause",
        "details":{"controller_ownership":"retired","reason":"lease expired"}}),
    );
    for key in [false, true] {
        let result = if key {
            handle.submit_action_key_before(&pin, 3, Instant::now() + Duration::from_secs(2))
        } else {
            handle.submit_pinned(&pin, settings(), Instant::now() + Duration::from_secs(2))
        };
        let error = result.unwrap_err();
        assert_eq!(
            crate::types::classify(&error).details.unwrap()["reason"],
            "lease expired"
        );
    }
    assert!(held.config_unknown.load(Ordering::Acquire));
    assert!(
        worker
            .status(Duration::from_secs(2))
            .unwrap()
            .connected
            .is_none()
    );
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn pinned_still_uses_normal_job_book_observation_and_keeps_job_only_drain_after_retirement() {
    use std::{
        os::unix::process::CommandExt as _,
        process::{Command, Stdio},
    };
    let handle = SessionHandle::new();
    let weak = Arc::downgrade(&handle.inner);
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session::tests::serve_as_test_worker",
            "--nocapture",
        ])
        .env("CAPTUREFAB_TEST_WORKER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let worker = Arc::new(
        Worker::attach(
            child,
            Arc::new(move |shared, signal| {
                if let Some(inner) = weak.upgrade() {
                    SessionHandle { inner }.on_signal(shared, signal);
                }
            }),
        )
        .unwrap(),
    );
    *locked(&worker.shared().label) = "sim:0".into();
    worker
        .call(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 2000,
            login: None,
            saved_login: true,
        })
        .unwrap();
    let held = crate::session::tests::held_for_retirement_test();
    *held.worker.write().unwrap() = Some(worker.clone());
    handle
        .inner
        .cameras
        .write()
        .unwrap()
        .insert("sim:0".into(), held.clone());
    handle.inner.slots.store(1, Ordering::Release);
    let pin = handle.pin_existing_camera("sim:0").unwrap();
    let settings: SessionCommand = serde_json::from_value(json!({"op":"set_values","values":[
        {"name":"Width","value":"64"},{"name":"Height","value":"64"},
        {"name":"PixelFormat","value":"Mono8"}]}))
    .unwrap();
    handle
        .submit_pinned(&pin, settings, Instant::now() + Duration::from_secs(3))
        .unwrap()
        .recv_timeout(Duration::from_secs(4))
        .unwrap()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("selected.raw");
    let schedule: SessionCommand = serde_json::from_value(json!({"op":"schedule",
        "output":output,"count":1,"timeout_ms":2000,"format":"raw",
        "first_at_ms":0,"interval_ms":0,"warmup_ms":0,"min_free":0,"lease":true}))
    .unwrap();
    let (reply, receipt) = handle
        .submit_observed_capture_pinned(&pin, schedule, Instant::now() + Duration::from_secs(3))
        .unwrap();
    let result = reply.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
    assert_eq!(result["worker_pid"], worker.pid());
    let job = result["job"]["id"].as_str().unwrap().to_owned();
    let mut seq = 0;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < until, "scheduled still did not complete");
        let result = handle
            .submit(SessionCommand::Wait {
                job: job.clone(),
                after_seq: seq,
                timeout_ms: 2000,
            })
            .unwrap()
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        seq = result["seq"].as_u64().unwrap();
        if result["job"]["state"] == "done" {
            break;
        }
        assert!(result["job"]["error"].is_null(), "{result}");
    }
    held.remember_retirement(json!({"code":"disconnected","message":"after save",
        "details":{"controller_ownership":"retired","reason":"after owned frame"}}));
    handle
        .check_pinned(&pin, Instant::now() + Duration::from_secs(1))
        .unwrap_err();
    let saved = receipt.try_take().unwrap().unwrap();
    assert_eq!(saved.path, output.to_str().unwrap());
    assert_eq!(saved.bytes, 4096);
    assert_eq!(std::fs::metadata(&output).unwrap().len(), saved.bytes);
    let stopped = handle
        .submit(SessionCommand::StopJob { job })
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(stopped["job"]["saved"], 1);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn pinned_job_stop_reports_lost_original_worker_instead_of_rebinding_cleanup() {
    let handle = SessionHandle::new();
    let (old, worker) = held(&handle);
    let number = handle
        .inner
        .jobs
        .start(super::super::JobKind::Capture, "sim:0", Some(1), true)
        .unwrap();
    handle.inner.jobs.pin_owner(number, &worker);
    worker.kill();
    assert!(worker.wait_exit(Instant::now() + Duration::from_secs(5)));
    let replacement = Arc::new(crate::session::tests::test_worker());
    *old.worker.write().unwrap() = Some(replacement.clone());
    let result = handle
        .submit(SessionCommand::StopJob {
            job: format!("{}-{number}", handle.inner.name),
        })
        .unwrap()
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap();
    assert_eq!(result["job"]["state"], "failed");
    assert_eq!(result["job"]["error"]["code"], "disconnected");
    assert!(
        result["job"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("original worker")
    );
    assert!(
        replacement
            .status(Duration::from_secs(2))
            .unwrap()
            .connected
            .is_none()
    );
    handle.shutdown();
}
