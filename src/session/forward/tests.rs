use super::*;
#[cfg(unix)]
use crate::session::protocol::JobStatus;
use crate::types::classify;

fn command(dir: &Path, job: Option<u64>) -> SessionCommand {
    serde_json::from_value(json!({"op":"forward","output":dir.join("clip.mkv"),
        "codec":"ffv1","encoder":"cpu","fps":5,"bitrate":"5M","warmup_ms":0,
        "job":job,"timeline":{"path":dir.join("clip.frames.jsonl"),"acquisition_end_ns":1}}))
    .unwrap()
}
#[cfg(unix)]
fn ownership(observation: &ForwardObservation, job: u64) -> Output {
    Output::IndexOwned {
        job,
        forwarding_id: "0123456789abcdef0123456789abcdef".into(),
        index_path: observation.index.clone(),
        output_path: observation.output.clone(),
    }
}
#[cfg(unix)]
fn ending(observation: &ForwardObservation, job: u64, error: Option<Value>) -> Output {
    Output::JobEnd {
        job,
        status: if error.is_some() {
            JobStatus::Failed
        } else {
            JobStatus::Cancelled
        },
        error,
        summary: json!({"written":1,"accepted":1,"dropped":0,"finalized":true,
            "codec":"ffv1","encoder":"cpu","recording":{"path":observation.output,"bytes":5000}}),
    }
}

#[test]
fn observed_command_is_private_finite_and_parses_exact_expiry() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let command = command(dir.path(), Some(1));
    let deadline_ns =
        super::super::pinned::before(Instant::now() + Duration::from_secs(2)).unwrap();
    let wrapped = WorkerCommand::ForwardObservedUntil {
        command: command.clone(),
        deadline_ns,
    };
    wrapped.validate().unwrap();
    assert!(wrapped.session().is_some());
    assert!(!wrapped.changes_settings());
    let encoded = serde_json::to_vec(&wrapped).unwrap();
    SessionCommand::from_json_slice(&encoded).unwrap_err();
    let round: WorkerCommand = serde_json::from_slice(&encoded).unwrap();
    round.validate().unwrap();
    let mut value = serde_json::to_value(wrapped).unwrap();
    value["deadline_ns"] = json!(1);
    let expired: WorkerCommand = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        classify(&expired.validate().unwrap_err()).kind,
        ErrorKind::Timeout
    );
    value["unexpected"] = true.into();
    serde_json::from_value::<WorkerCommand>(value).unwrap_err();
    let mut public = serde_json::to_value(command).unwrap();
    for change in [
        json!({"timeline":null}),
        json!({"output":"rtmp://localhost/live"}),
        json!({"output":"clip-{seq}.mkv"}),
        json!({"on_exists":"number"}),
    ] {
        let mut invalid = public.clone();
        for (key, value) in change.as_object().unwrap() {
            invalid[key].clone_from(value);
        }
        let command: SessionCommand = serde_json::from_value(invalid).unwrap();
        assert_eq!(
            classify(&validate(&command).unwrap_err()).kind,
            ErrorKind::Usage
        );
    }
    public["job"] = Value::Null;
    let unassigned: SessionCommand = serde_json::from_value(public).unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &unassigned,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    assert!(observation.owned().unwrap().is_none());
    assert!(observation.poll_end().unwrap().is_none());
    let command = WorkerCommand::ForwardObservedUntil {
        command: unassigned,
        deadline_ns,
    };
    assert_eq!(
        classify(&command.validate().unwrap_err()).kind,
        ErrorKind::Usage
    );
    handle.shutdown();
}

#[cfg(unix)]
fn worker() -> Arc<Worker> {
    Arc::new(super::super::tests::test_worker())
}
#[cfg(unix)]
fn close(worker: &Worker) {
    worker.close();
    assert!(worker.wait_exit(Instant::now() + Duration::from_secs(5)));
}

#[test]
#[cfg(unix)]
fn only_exact_worker_job_receipt_adopts_the_created_index() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let owner = worker();
    let stranger = worker();
    observation.assign(&handle, 1, owner.clone()).unwrap();
    let existing = b"unrelated preexisting index bytes";
    std::fs::write(&observation.index, existing).unwrap();
    assert!(
        observation.owned().unwrap().is_none(),
        "allocation/path is not ownership"
    );
    observation.event(stranger.shared(), &ownership(&observation, 1));
    observation.event(owner.shared(), &ownership(&observation, 2));
    assert!(observation.owned().unwrap().is_none());
    assert_eq!(std::fs::read(&observation.index).unwrap(), existing);
    std::fs::remove_file(&observation.index).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&observation.index)
        .unwrap();
    observation.event(owner.shared(), &ownership(&observation, 1));
    let owned = observation.owned().unwrap().unwrap();
    assert_eq!(owned.job, format!("{}-1", handle.inner.name));
    assert_eq!(owned.index_path, observation.index);
    assert_eq!(owned.output_path, observation.output);
    assert_eq!(owned.forwarding_id.len(), 32);
    observation.event(owner.shared(), &ownership(&observation, 1));
    assert_eq!(
        classify(&observation.owned().unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    close(&owner);
    close(&stranger);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn late_actual_job_end_is_observed_after_public_admission_failure_without_erasing_it() {
    for failed_end in [false, true] {
        let handle = SessionHandle::new();
        let dir = tempfile::tempdir().unwrap();
        let observation = ForwardObservation::new(
            &handle,
            &command(dir.path(), None),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let owner = worker();
        let number = handle
            .inner
            .jobs
            .start(super::super::JobKind::Record, "fixture", None, false)
            .unwrap();
        observation.assign(&handle, number, owner.clone()).unwrap();
        handle.inner.jobs.pin_owner(number, &owner);
        owner
            .shared()
            .observe_forward_output(&ownership(&observation, number));
        // The outer source wait can expire before the coordinator wakes.
        observation.stop().unwrap();
        {
            let state = locked(&observation.state);
            assert!(state.stop_requested);
            assert!(!state.stop_sent);
            assert!(!state.admission_done);
        }
        let failure = Categorized::error(ErrorKind::Timeout, "original startup deadline");
        handle.inner.jobs.admission_failed(number, &failure);
        let original = observation.admission(Err(failure)).unwrap_err();
        assert_eq!(classify(&original).kind, ErrorKind::Timeout);
        assert!(locked(&observation.state).stop_sent);
        let error = failed_end
            .then(|| json!({"code":"permission_denied","message":"actual finish failure"}));
        let output = ending(&observation, number, error);
        owner.shared().observe_forward_output(&output);
        handle.on_signal(
            owner.shared(),
            super::super::Signal::Output(Box::new(output)),
        );
        let end = observation.poll_end().unwrap().unwrap();
        assert_eq!(end["detail"]["written"], 1);
        if failed_end {
            assert_eq!(end["detail"]["error"]["code"], "permission_denied");
        }
        assert!(observation.owned().unwrap().is_some());
        let public = handle.inner.jobs.get(number).unwrap();
        assert_eq!(public.state, super::super::jobs::JobState::Failed);
        assert_eq!(public.error.unwrap()["code"], "timeout");
        assert_eq!(
            observation
                .stop_and_wait(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
            end
        );
        close(&owner);
        handle.shutdown();
    }
}

#[test]
#[cfg(unix)]
fn bad_final_metadata_or_worker_loss_does_not_erase_valid_index_ownership() {
    for lost in [false, true] {
        let handle = SessionHandle::new();
        let dir = tempfile::tempdir().unwrap();
        let observation = ForwardObservation::new(
            &handle,
            &command(dir.path(), None),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let owner = worker();
        observation.assign(&handle, 1, owner.clone()).unwrap();
        observation.event(owner.shared(), &ownership(&observation, 1));
        if lost {
            owner.kill();
            assert!(owner.wait_exit(Instant::now() + Duration::from_secs(5)));
        } else {
            observation.event(
                owner.shared(),
                &Output::JobEnd {
                    job: 1,
                    status: JobStatus::Failed,
                    error: None,
                    summary: json!({}),
                },
            );
        }
        observation.poll_end().unwrap_err();
        assert!(observation.owned().unwrap().is_some());
        close(&owner);
        handle.shutdown();
    }
}

#[test]
#[cfg(unix)]
fn stop_before_assignment_prevents_later_dispatch_and_none_requires_definitive_refusal() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    observation.stop().unwrap();
    let owner = worker();
    observation.assign(&handle, 1, owner.clone()).unwrap();
    let error = observation
        .call(&handle, &owner, command(dir.path(), Some(1)))
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Interrupted);
    observation.admission(Err(error)).unwrap_err();
    assert!(
        observation
            .stop_and_wait(Duration::from_secs(1))
            .unwrap()
            .is_none()
    );
    assert!(
        owner
            .status(Duration::from_secs(2))
            .unwrap()
            .connected
            .is_none()
    );
    assert!(observation.owned().unwrap().is_none());
    close(&owner);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn late_correlated_worker_refusal_is_retained_after_the_reply_waiter_is_gone() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let owner = worker();
    observation.assign(&handle, 1, owner.clone()).unwrap();
    let mut invalid = command(dir.path(), Some(1));
    if let SessionCommand::Forward { timeline, .. } = &mut invalid {
        *timeline = None;
    }
    let (_, reply) = owner
        .submit_forward(
            WorkerCommand::ForwardObservedUntil {
                command: invalid,
                deadline_ns: observation.deadline_ns,
            },
            Arc::downgrade(&observation),
        )
        .unwrap();
    drop(reply);
    observation
        .admission(Err(Categorized::error(
            ErrorKind::Timeout,
            "original local reply cutoff",
        )))
        .unwrap_err();
    assert!(
        observation
            .stop_and_wait(Duration::from_secs(2))
            .unwrap()
            .is_none()
    );
    assert!(observation.owned().unwrap().is_none());
    assert!(
        owner
            .status(Duration::from_secs(2))
            .unwrap()
            .connected
            .is_none()
    );
    close(&owner);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn uncertain_assignment_never_returns_none_and_settle_kills_only_its_old_worker() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let owner = worker();
    let stranger = worker();
    observation.assign(&handle, 1, owner.clone()).unwrap();
    observation
        .admission(Err(Categorized::error(
            ErrorKind::Timeout,
            "uncertain dispatched startup",
        )))
        .unwrap_err();
    let error = observation
        .stop_and_wait(Duration::from_millis(30))
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert!(owner.wait_exit(Instant::now() + Duration::from_secs(5)));
    assert!(
        stranger
            .status(Duration::from_secs(2))
            .unwrap()
            .connected
            .is_none()
    );
    close(&stranger);
    handle.shutdown();
}

#[test]
fn observed_queue_expires_before_setup_or_worker_selection() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let command = command(dir.path(), None);
    let first = handle.command_turn(Some("sim:0"), &command).unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command,
        Instant::now() + Duration::from_millis(50),
    )
    .unwrap();
    let reply = handle
        .submit_target_observed(
            Some("sim:0".into()),
            command,
            None,
            None,
            Some(observation.clone()),
        )
        .unwrap();
    let error = reply
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Timeout);
    assert!(locked(&observation.state).assigned.is_none());
    assert!(
        observation
            .stop_and_wait(Duration::from_secs(1))
            .unwrap()
            .is_none()
    );
    drop(first);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn pipe_loss_is_not_a_correlated_worker_refusal() {
    use std::os::unix::process::CommandExt as _;
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let hello = serde_json::to_string(&super::super::protocol::Line {
        seq: 0,
        output: Output::Hello {
            protocol: super::super::protocol::PROTOCOL,
            build: "reply-loss".into(),
        },
    })
    .unwrap();
    let child = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "printf '%s\n' \"$1\"; IFS= read -r line; exit 9",
            "worker",
            &hello,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let owner = Arc::new(Worker::attach(child, Arc::new(|_, _| {})).unwrap());
    observation.assign(&handle, 1, owner.clone()).unwrap();
    let error = observation
        .call(&handle, &owner, command(dir.path(), Some(1)))
        .unwrap_err();
    observation.admission(Err(error)).unwrap_err();
    assert!(!locked(&observation.state).refused);
    assert!(owner.wait_exit(Instant::now() + Duration::from_secs(5)));
    let error = observation
        .stop_and_wait(Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::WorkerCrashed);
    assert!(observation.owned().unwrap().is_none());
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn malformed_receipt_still_settles_only_the_original_worker() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let owner = worker();
    let stranger = worker();
    observation.assign(&handle, 1, owner.clone()).unwrap();
    let mut invalid = ownership(&observation, 1);
    if let Output::IndexOwned { index_path, .. } = &mut invalid {
        index_path.push_str(".foreign");
    }
    observation.event(owner.shared(), &invalid);
    let error = observation
        .stop_and_wait(Duration::from_millis(30))
        .unwrap_err();
    assert_eq!(classify(&error).kind, ErrorKind::Incomplete);
    assert!(owner.wait_exit(Instant::now() + Duration::from_secs(5)));
    assert!(
        stranger
            .status(Duration::from_secs(2))
            .unwrap()
            .connected
            .is_none()
    );
    close(&stranger);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn malformed_terminal_errors_and_metadata_are_not_successful_finalization() {
    for error in [None, Some(json!(false)), Some(json!({"code":"incomplete"}))] {
        let handle = SessionHandle::new();
        let dir = tempfile::tempdir().unwrap();
        let observation = ForwardObservation::new(
            &handle,
            &command(dir.path(), None),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let owner = worker();
        observation.assign(&handle, 1, owner.clone()).unwrap();
        observation.event(owner.shared(), &ownership(&observation, 1));
        observation.event(
            owner.shared(),
            &Output::JobEnd {
                job: 1,
                status: JobStatus::Failed,
                error,
                summary: json!({"written":1,"finalized":true}),
            },
        );
        assert_eq!(
            classify(&observation.poll_end().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        assert_eq!(
            classify(
                &observation
                    .stop_and_wait(Duration::from_secs(1))
                    .unwrap_err()
            )
            .kind,
            ErrorKind::Incomplete
        );
        assert!(observation.owned().unwrap().is_some());
        close(&owner);
        handle.shutdown();
    }
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let owner = worker();
    observation.assign(&handle, 1, owner.clone()).unwrap();
    observation.event(owner.shared(), &ownership(&observation, 1));
    let mut oversized = ending(&observation, 1, None);
    if let Output::JobEnd { summary, .. } = &mut oversized {
        summary["oversized"] = json!("x".repeat(MAX_METADATA));
    }
    observation.event(owner.shared(), &oversized);
    assert_eq!(
        classify(&observation.poll_end().unwrap_err()).kind,
        ErrorKind::Incomplete
    );
    assert!(observation.owned().unwrap().is_some());
    close(&owner);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn finalized_outcome_requires_actual_matching_container_metadata() {
    for patch in [
        json!({"recording":null}),
        json!({"recording":{"path":1,"bytes":5000}}),
        json!({"recording":{"path":"foreign.mkv","bytes":5000}}),
        json!({"recording":{"bytes":5000}}),
        json!({"recording":{"path":"$expected","bytes":5000,"truncated":true}}),
        json!({"codec":null}),
        json!({"encoder":null}),
        json!({"accepted":null}),
        json!({"dropped":null}),
    ] {
        let handle = SessionHandle::new();
        let dir = tempfile::tempdir().unwrap();
        let observation = ForwardObservation::new(
            &handle,
            &command(dir.path(), None),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let owner = worker();
        observation.assign(&handle, 1, owner.clone()).unwrap();
        observation.event(owner.shared(), &ownership(&observation, 1));
        let mut output = ending(&observation, 1, None);
        if let Output::JobEnd { summary, .. } = &mut output {
            for (key, value) in patch.as_object().unwrap() {
                summary[key].clone_from(value);
            }
            if summary["recording"]["path"] == "$expected" {
                summary["recording"]["path"] = json!(observation.output);
            }
        }
        observation.event(owner.shared(), &output);
        assert_eq!(
            classify(&observation.poll_end().unwrap_err()).kind,
            ErrorKind::Incomplete
        );
        assert!(observation.owned().unwrap().is_some());
        close(&owner);
        handle.shutdown();
    }
}

#[test]
#[cfg(unix)]
fn actual_reader_routes_late_outcome_after_public_history_prunes_the_job() {
    use std::os::unix::process::CommandExt as _;
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let number = handle
        .inner
        .jobs
        .start(super::super::JobKind::Record, "fixture", None, false)
        .unwrap();
    let lines = [
        Output::Hello {
            protocol: super::super::protocol::PROTOCOL,
            build: "late-outcome".into(),
        },
        ownership(&observation, number),
        ending(&observation, number, None),
        Output::Reply {
            id: 1,
            ok: true,
            result: Some(json!({})),
            error: None,
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(seq, output)| {
        serde_json::to_string(&super::super::protocol::Line {
            seq: u64::try_from(seq).unwrap(),
            output,
        })
        .unwrap()
    })
    .collect::<Vec<_>>();
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "printf '%s\n' \"$1\"; IFS= read -r line; printf '%s\n' \"$2\" \"$3\" \"$4\"; while IFS= read -r line; do :; done", "worker",
            &lines[0], &lines[1], &lines[2], &lines[3]])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()).process_group(0).spawn().unwrap();
    let owner = Arc::new(Worker::attach(child, Arc::new(|_, _| {})).unwrap());
    observation.assign(&handle, number, owner.clone()).unwrap();
    let failure = Categorized::error(ErrorKind::Timeout, "original startup cutoff");
    handle.inner.jobs.admission_failed(number, &failure);
    observation.admission(Err(failure)).unwrap_err();
    for _ in 0..257 {
        let newer = handle
            .inner
            .jobs
            .start(super::super::JobKind::Record, "newer", None, false)
            .unwrap();
        handle.inner.jobs.admission_failed(
            newer,
            &Categorized::error(ErrorKind::Timeout, "newer cutoff"),
        );
    }
    assert!(handle.inner.jobs.get(number).is_none());
    owner.call(WorkerCommand::CheckConnection).unwrap();
    assert!(observation.owned().unwrap().is_some());
    let end = observation.poll_end().unwrap().unwrap();
    assert_eq!(end["detail"]["written"], 1);
    assert_eq!(end["detail"]["finalized"], true);
    assert!(handle.inner.jobs.get(number).is_none());
    close(&owner);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn fifo_publication_delivers_ownership_before_a_correlated_error_reply() {
    use std::os::unix::process::CommandExt as _;
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let observation = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    let events = crate::engine::events::Events::attached(false);
    for output in [
        Output::Hello {
            protocol: super::super::protocol::PROTOCOL,
            build: "fifo-receipt".into(),
        },
        ownership(&observation, 1),
        Output::Reply {
            id: 1,
            ok: false,
            result: None,
            error: Some(json!({"code":"operation_failed","message":"after ownership"})),
        },
    ] {
        events.emit(output);
    }
    let lines: [String; 3] = std::array::from_fn(|seq| {
        let crate::engine::events::Next::Line(output) = events.next(Duration::ZERO) else {
            panic!("missing ordered event")
        };
        serde_json::to_string(&super::super::protocol::Line {
            seq: u64::try_from(seq).unwrap(),
            output: *output,
        })
        .unwrap()
    });
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "printf '%s\n' \"$1\"; IFS= read -r line; printf '%s\n' \"$2\" \"$3\"; while IFS= read -r line; do :; done", "worker",
            &lines[0], &lines[1], &lines[2]])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()).process_group(0).spawn().unwrap();
    let owner = Arc::new(Worker::attach(child, Arc::new(|_, _| {})).unwrap());
    observation.assign(&handle, 1, owner.clone()).unwrap();
    let error = observation
        .call(&handle, &owner, command(dir.path(), Some(1)))
        .unwrap_err();
    assert!(format!("{error:#}").contains("after ownership"));
    assert!(observation.owned().unwrap().is_some());
    assert!(!locked(&observation.state).refused);
    close(&owner);
    handle.shutdown();
}

#[test]
#[cfg(unix)]
fn exact_worker_observer_map_is_bounded_and_purges_dead_receipts() {
    let handle = SessionHandle::new();
    let dir = tempfile::tempdir().unwrap();
    let owner = worker();
    let observations = (1..=64)
        .map(|number| {
            let observation = ForwardObservation::new(
                &handle,
                &command(dir.path(), None),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            observation.assign(&handle, number, owner.clone()).unwrap();
            observation
        })
        .collect::<Vec<_>>();
    let extra = ForwardObservation::new(
        &handle,
        &command(dir.path(), None),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(
        classify(&extra.assign(&handle, 65, owner.clone()).unwrap_err()).kind,
        ErrorKind::Usage
    );
    assert!(locked(&extra.state).assigned.is_none());
    drop(observations);
    extra.assign(&handle, 65, owner.clone()).unwrap();
    owner
        .shared()
        .observe_forward_output(&ownership(&extra, 65));
    assert!(extra.owned().unwrap().is_some());
    close(&owner);
    handle.shutdown();
}
