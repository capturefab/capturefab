//! Capture checks use snapshots and plain context only: no workers, files,
//! network requests, FFmpeg processes or credential stores.
use crate::{
    naming::OnExists,
    session::SessionCommand,
    tools::{
        IssueCode, Severity, StepId, ToolCall,
        context::{Context, Zone},
        exec::{Checked, Op, PlanState},
        tests::{NOW, SCANNER, WEBCAM, feature, fixture_context, fixture_snapshot, job},
    },
};
use serde_json::{Value, json};

fn context() -> Context {
    fixture_context(fixture_snapshot())
}

fn args(tool: &str) -> Value {
    let schema = (crate::tools::spec(tool).unwrap().schema)();
    let mut args = Value::Object(
        schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|key| (key.clone(), Value::Null))
            .collect(),
    );
    args["camera"] = json!("sim:0");
    match tool {
        "schedule_captures" => {
            args["every"] = json!("1m");
            args["count"] = json!(3);
        }
        "stream_to_url" => args["url"] = json!("rtsp://live.example/video"),
        "cancel_job" => args["job_id"] = json!(7),
        _ => {}
    }
    args
}

fn call(tool: &str, args: Value) -> ToolCall {
    ToolCall {
        tool: tool.into(),
        args,
    }
}

fn run_in(ctx: &Context, plan: &mut PlanState, tool: &str, args: Value) -> Checked {
    crate::tools::check(ctx, plan, StepId(1), &call(tool, args), &|_, _| false)
}

fn run(ctx: &Context, tool: &str, args: Value) -> Checked {
    run_in(ctx, &mut PlanState::new(ctx), tool, args)
}

fn command(checked: &Checked) -> &SessionCommand {
    checked
        .ops
        .iter()
        .find_map(|op| match op {
            Op::Command { command, .. } | Op::Stream { command, .. } => Some(command),
            _ => None,
        })
        .expect("a camera command")
}

fn issue(checked: &Checked, field: &str, severity: Severity, contains: &str) -> bool {
    checked.issues.iter().any(|issue| {
        issue.field.as_deref() == Some(field)
            && issue.severity == severity
            && issue.message.contains(contains)
    })
}

fn ready(checked: &Checked) {
    assert!(checked.ready(), "{:#?}", checked.issues);
}

#[test]
fn photo_defaults_and_requested_policy_reach_a_pinned_command() {
    let mut ctx = context();
    ctx.storage.request.max_age_seconds = Some(90);
    let checked = run(&ctx, "capture_photos", args("capture_photos"));
    ready(&checked);
    assert_eq!(checked.call.args["count"], 1);
    assert_eq!(checked.call.args["format"], "png");
    assert_eq!(
        checked.call.args["file_name"],
        "{camera}-{datetime}-{seq}.png"
    );
    assert_eq!(checked.output_root.as_ref(), Some(&ctx.capture_root));
    let SessionCommand::Capture {
        count,
        timeout_ms,
        output,
        storage,
        on_exists,
        destination,
        ..
    } = command(&checked)
    else {
        panic!("capture command expected");
    };
    assert_eq!((*count, *timeout_ms), (1, 10_000));
    assert_eq!(storage.max_age_seconds, Some(90));
    assert_eq!(*on_exists, OnExists::Number);
    assert!(destination.is_none());
    assert!(std::path::Path::new(output).starts_with(&ctx.capture_root));
    assert!(matches!(&checked.ops[0], Op::Command { camera, .. } if camera.selector() == "sim:0"));
}

#[test]
fn captures_connect_discovered_cameras_and_number_static_sequences() {
    let ctx = context();
    let mut input = args("capture_photos");
    input["camera"] = json!("sim:2");
    input["count"] = json!(2);
    input["file_name"] = json!("photo");
    let checked = run(&ctx, "capture_photos", input);
    ready(&checked);
    assert_eq!(checked.call.args["file_name"], "photo-{seq}.png");
    assert!(matches!(&checked.ops[0], Op::Connect { selector } if selector == "sim:2"));
    assert!(matches!(&checked.ops[1], Op::Command { camera, .. } if camera.selector() == "sim:2"));
}

#[test]
fn later_capture_steps_reuse_prior_connections_and_destination_overlays() {
    let ctx = context();
    let mut plan = PlanState::new(&ctx);
    plan.destinations
        .push(crate::tools::context::DestinationInfo {
            approval_identity: None,
            name: "new-folder".into(),
            location: "~/Captures/new".into(),
            ..ctx.destinations[0].clone()
        });
    for step in 0..2 {
        let mut input = args("capture_photos");
        input["camera"] = json!("sim:2");
        input["destination"] = json!("new-folder");
        let checked = run_in(&ctx, &mut plan, "capture_photos", input);
        ready(&checked);
        assert_eq!(
            checked
                .ops
                .iter()
                .filter(|op| matches!(op, Op::Connect { .. }))
                .count(),
            usize::from(step == 0)
        );
        assert!(
            matches!(command(&checked),SessionCommand::Capture {destination:Some(name),..} if name == "new-folder")
        );
        assert_eq!(checked.call.args["camera"], "sim:2");
    }
}

#[test]
fn recording_normalizes_compound_duration_codec_and_defaults() {
    let mut ctx = context();
    ctx.defaults.fps = 24.0;
    ctx.defaults.bitrate = "2M".into();
    let mut input = args("start_recording");
    input["duration"] = json!("1 minute 30 seconds");
    input["codec"] = json!("h265");
    let checked = run(&ctx, "start_recording", input);
    ready(&checked);
    assert_eq!(checked.call.args["duration"], "1m30s");
    assert_eq!(checked.call.args["codec"], "hevc");
    assert_eq!(checked.call.args["fps"], 24.0);
    assert_eq!(checked.call.args["bitrate"], "2M");
    assert!(matches!(command(&checked), SessionCommand::Forward {
        duration_ms: Some(90_000), codec, fps, bitrate, max_file_bytes, on_exists: OnExists::Number, ..
    } if codec == "hevc" && *fps == 24.0 && bitrate == "2M" && *max_file_bytes == crate::media::recording_reservation("2M",90_000).unwrap()));
}

#[test]
fn durationless_recording_has_a_clean_worker_stop_before_its_reservation() {
    let ctx = context();
    let checked = run(&ctx, "start_recording", args("start_recording"));
    ready(&checked);
    let SessionCommand::Forward {
        duration_ms,
        max_file_bytes,
        bitrate,
        ..
    } = command(&checked)
    else {
        panic!("forward expected");
    };
    let duration = duration_ms.expect("worker-enforced stop");
    assert!(duration >= 60_000);
    assert_eq!(*max_file_bytes, 512 * 1024 * 1024);
    assert_eq!(
        duration,
        crate::media::max_duration_for(bitrate, *max_file_bytes).unwrap()
    );
    assert!(crate::media::recording_reservation(bitrate, duration).unwrap() <= *max_file_bytes);
    assert!(issue(
        &checked,
        "duration",
        Severity::Warning,
        &format!("about {} minutes", duration / 60_000)
    ));
    let mut input = args("start_recording");
    input["bitrate"] = json!("1G");
    let checked = run(&ctx, "start_recording", input);
    assert!(!checked.ready());
    assert!(issue(
        &checked,
        "bitrate",
        Severity::Error,
        "at least one minute"
    ));
}

#[test]
fn schedule_duration_and_until_have_the_documented_frame_counts() {
    let ctx = context();
    let mut input = args("schedule_captures");
    input["count"] = Value::Null;
    input["duration"] = json!("5m");
    input["file_name"] = json!("scheduled.png");
    let checked = run(&ctx, "schedule_captures", input);
    ready(&checked);
    assert_eq!(checked.call.args["file_name"], "scheduled-{seq}.png");
    assert!(matches!(
        command(&checked),
        SessionCommand::Schedule {
            count: 5,
            first_at_ms: NOW,
            interval_ms: 60_000,
            ..
        }
    ));
    let mut input = args("schedule_captures");
    input["count"] = Value::Null;
    input["until"] = json!(ctx.zone.rfc3339(NOW + 5 * 60_000));
    let checked = run(&ctx, "schedule_captures", input);
    ready(&checked);
    assert!(matches!(
        command(&checked),
        SessionCommand::Schedule { count: 6, .. }
    ));
}

#[test]
fn schedule_times_are_pinned_across_clock_changes() {
    let mut ctx = context();
    ctx.zone = Zone::Fixed(0);
    let mut input = args("schedule_captures");
    input["count"] = Value::Null;
    input["start_at"] = json!("12:10");
    input["until"] = json!("12:20");
    let first = run(&ctx, "schedule_captures", input);
    ready(&first);
    assert_eq!(first.call.args["start_at"], "2026-10-18T12:10:00+00:00");
    assert_eq!(first.call.args["until"], "2026-10-18T12:20:00+00:00");
    ctx.now_ms += 10 * 60_000 + 30_000;
    let later = run(&ctx, "schedule_captures", first.call.args.clone());
    ready(&later);
    assert_eq!(first.call.args["start_at"], later.call.args["start_at"]);
    assert_eq!(first.call.args["until"], later.call.args["until"]);
    assert!(
        matches!(command(&later),SessionCommand::Schedule {count:11,first_at_ms,..} if *first_at_ms == NOW + 600_000)
    );
}

#[test]
fn stop_and_cancel_emit_explicit_commands_and_update_plan_state() {
    let ctx = context();
    let mut plan = PlanState::new(&ctx);
    let mut input = args("stop_recording");
    input["camera"] = json!("sim:1");
    let stopped = run_in(&ctx, &mut plan, "stop_recording", input);
    ready(&stopped);
    assert!(
        matches!(&stopped.ops[..], [Op::StopForward { forwarding_id: Some(id), .. }]
        if id == "11111111111111111111111111111111")
    );
    assert!(!plan.is_forwarding("sim:1"));
    let cancelled = run_in(&ctx, &mut plan, "cancel_job", args("cancel_job"));
    ready(&cancelled);
    assert!(matches!(
        command(&cancelled),
        SessionCommand::CancelJob { id: 7 }
    ));
    assert_eq!(plan.active_jobs("sim:0"), 0);
}

#[test]
fn stop_recording_pins_the_exact_forward_and_preserves_planned_start_references() {
    let mut ctx = context();
    let mut input = args("stop_recording");
    input["camera"] = json!("sim:1");
    let before = run(&ctx, "stop_recording", input.clone());
    ready(&before);
    let camera = ctx
        .snapshot
        .cameras
        .iter_mut()
        .find(|c| c.info.id == "sim:1")
        .unwrap();
    camera.forwarding_id = Some("33333333333333333333333333333333".into());
    let after = run(&ctx, "stop_recording", input.clone());
    ready(&after);
    assert!(
        crate::tools::exec::changed(&before, &after).is_some(),
        "same-time replacements require a new review"
    );
    ctx.snapshot
        .cameras
        .iter_mut()
        .find(|c| c.info.id == "sim:1")
        .unwrap()
        .forwarding_id = None;
    let legacy = run(&ctx, "stop_recording", input);
    assert!(
        !legacy.ready(),
        "an unidentified live recording cannot be stopped by a tool"
    );

    let mut plan = PlanState::new(&ctx);
    ready(&run_in(
        &ctx,
        &mut plan,
        "start_recording",
        args("start_recording"),
    ));
    let planned = run_in(&ctx, &mut plan, "stop_recording", args("stop_recording"));
    ready(&planned);
    assert!(
        matches!(
            &planned.ops[..],
            [Op::StopForward {
                forwarding_id: None,
                ..
            }]
        ),
        "only the earlier executor's Forward acknowledgement can resolve this reference"
    );
}

#[test]
fn streams_without_duration_have_no_recording_storage_check() {
    let mut ctx = context();
    ctx.storage.free_disk = Some(0);
    ctx.storage.effective.max_bytes = 1;
    ctx.storage.effective.max_files = 1;
    let checked = run(&ctx, "stream_to_url", args("stream_to_url"));
    ready(&checked);
    assert!(checked.leaves_computer);
    assert!(issue(&checked, "url", Severity::Warning, "live.example"));
    assert!(issue(&checked, "url", Severity::Warning, "Unencrypted."));
    assert!(!checked.facts.iter().any(|(key, _)| key == "Storage"));
    assert!(matches!(
        command(&checked),
        SessionCommand::Forward {
            duration_ms: None,
            destination: None,
            ..
        }
    ));
}

#[test]
fn forwarding_is_unique_in_live_state_and_across_earlier_plan_steps() {
    let ctx = context();
    let mut input = args("start_recording");
    input["duration"] = json!("5s");
    input["camera"] = json!("sim:1");
    let busy = run(&ctx, "start_recording", input);
    assert!(issue(
        &busy,
        "camera",
        Severity::Error,
        "already recording or streaming. Add a stop step first."
    ));
    let mut plan = PlanState::new(&ctx);
    let mut input = args("start_recording");
    input["duration"] = json!("5s");
    ready(&run_in(&ctx, &mut plan, "start_recording", input));
    let second = run_in(&ctx, &mut plan, "stream_to_url", args("stream_to_url"));
    assert!(issue(
        &second,
        "camera",
        Severity::Error,
        "already recording or streaming"
    ));
    ready(&run_in(
        &ctx,
        &mut plan,
        "stop_recording",
        args("stop_recording"),
    ));
    ready(&run_in(
        &ctx,
        &mut plan,
        "stream_to_url",
        args("stream_to_url"),
    ));
}

#[test]
fn scanners_have_feeder_warnings_and_sixty_second_frame_timeouts() {
    let ctx = context();
    for tool in ["capture_photos", "schedule_captures"] {
        let mut input = args(tool);
        input["camera"] = json!(SCANNER);
        input["count"] = json!(2);
        let checked = run(&ctx, tool, input);
        ready(&checked);
        assert!(issue(
            &checked,
            "count",
            Severity::Warning,
            "Scans 2 pages from the feeder."
        ));
        assert!(matches!(
            command(&checked),
            SessionCommand::Capture {
                timeout_ms: 60_000,
                ..
            } | SessionCommand::Schedule {
                timeout_ms: 60_000,
                ..
            }
        ));
    }
    for tool in ["start_recording", "stream_to_url"] {
        let mut input = args(tool);
        input["camera"] = json!(SCANNER);
        let checked = run(&ctx, tool, input);
        assert!(
            checked
                .issues
                .iter()
                .any(|issue| issue.code == IssueCode::Unsupported)
        );
    }
}

#[test]
fn high_output_fps_warns_only_for_an_observed_streaming_camera() {
    let ctx = context();
    let mut input = args("start_recording");
    input["duration"] = json!("5s");
    input["fps"] = json!(60);
    let checked = run(&ctx, "start_recording", input.clone());
    ready(&checked);
    assert!(issue(
        &checked,
        "fps",
        Severity::Warning,
        "delivers about 30 fps; at 60 fps frames repeat and the video is not smoother."
    ));
    input["camera"] = json!(WEBCAM);
    let checked = run(&ctx, "start_recording", input);
    ready(&checked);
    assert!(!issue(&checked, "fps", Severity::Warning, "frames repeat"));
}

#[test]
fn photo_byte_estimates_warn_but_file_limits_block_with_stop_policy() {
    let mut ctx = context();
    ctx.storage.effective.max_bytes = 50_000_000;
    ctx.storage.allocated_bytes = 49_000_000;
    let checked = run(&ctx, "capture_photos", args("capture_photos"));
    ready(&checked);
    assert!(issue(
        &checked,
        "count",
        Severity::Warning,
        "budget is free"
    ));
    ctx.storage.effective.max_files = 122;
    let checked = run(&ctx, "schedule_captures", args("schedule_captures"));
    assert!(!checked.ready());
    assert!(issue(
        &checked,
        "count",
        Severity::Error,
        "This schedule saves 3 photos but only 2 more files fit in the storage budget."
    ));
    assert!(
        checked
            .issues
            .iter()
            .any(|issue| issue.code == IssueCode::StorageFull)
    );
}

#[test]
fn delete_oldest_warns_for_remaining_budget_and_files_but_not_total_recording_limit() {
    let mut ctx = context();
    ctx.storage.effective.on_full = "delete-oldest".into();
    ctx.storage.effective.max_bytes = 50_000_000;
    ctx.storage.effective.max_files = 122;
    ctx.storage.allocated_bytes = 49_000_000;
    let checked = run(&ctx, "schedule_captures", args("schedule_captures"));
    ready(&checked);
    assert!(issue(
        &checked,
        "count",
        Severity::Warning,
        "Makes room by permanently deleting the oldest captures."
    ));
    let mut input = args("start_recording");
    input["duration"] = json!("1h");
    let checked = run(&ctx, "start_recording", input);
    assert!(!checked.ready());
    assert!(issue(
        &checked,
        "duration",
        Severity::Error,
        "This recording needs up to"
    ));
}

#[test]
fn recording_reservations_include_existing_reservations_and_actionable_maximum() {
    let mut ctx = context();
    ctx.storage.effective.max_bytes = 100_000_000;
    ctx.storage.allocated_bytes = 60_000_000;
    ctx.storage.reserved_bytes = 30_000_000;
    let mut input = args("start_recording");
    input["duration"] = json!("10s");
    let checked = run(&ctx, "start_recording", input.clone());
    assert!(issue(&checked, "duration", Severity::Error, "only 10.0 MB"));
    input["duration"] = json!("1h");
    let checked = run(&ctx, "start_recording", input);
    assert!(issue(
        &checked,
        "duration",
        Severity::Error,
        "Record at most"
    ));
    assert!(issue(
        &checked,
        "duration",
        Severity::Error,
        "lower the bitrate, or raise the budget in Settings."
    ));
}

#[test]
fn disk_checks_use_the_selected_destination_and_remote_steps_warn() {
    let mut ctx = context();
    ctx.storage.free_disk = Some(0);
    let local = run(&ctx, "capture_photos", args("capture_photos"));
    assert!(issue(
        &local,
        "count",
        Severity::Error,
        "Free space at the destination"
    ));
    let mut input = args("capture_photos");
    input["destination"] = json!("LINE-1");
    let saved = run(&ctx, "capture_photos", input.clone());
    ready(&saved);
    assert_eq!(saved.call.args["destination"], "line-1");
    assert!(!saved.leaves_computer);
    ctx.destinations[0].free_bytes = Some(0);
    assert!(!run(&ctx, "capture_photos", input).ready());
    let mut input = args("capture_photos");
    input["destination"] = json!("archive");
    ctx.destinations[1].insecure = true;
    let remote = run(&ctx, "capture_photos", input);
    ready(&remote);
    assert!(remote.leaves_computer);
    assert!(issue(
        &remote,
        "destination",
        Severity::Warning,
        "minio.example:9000"
    ));
    assert!(issue(
        &remote,
        "destination",
        Severity::Warning,
        "Unencrypted."
    ));
}

#[test]
fn capture_and_recording_fields_report_their_documented_issues() {
    let ctx = context();
    for bad in [0, 1001] {
        let mut input = args("capture_photos");
        input["count"] = json!(bad);
        assert!(issue(
            &run(&ctx, "capture_photos", input),
            "count",
            Severity::Error,
            "1 to 1,000"
        ));
    }
    for (key, value, fragment) in [
        (
            "file_name",
            json!("../escape.png"),
            "Choose a folder destination",
        ),
        ("file_name", json!("wrong.jpg"), "Use the .png extension"),
        (
            "destination",
            json!("missing"),
            "Choose a saved destination",
        ),
    ] {
        let mut input = args("capture_photos");
        input[key] = value;
        assert!(issue(
            &run(&ctx, "capture_photos", input),
            key,
            Severity::Error,
            fragment
        ));
    }
    for (key, value, fragment) in [
        ("duration", json!("2"), "Add a unit"),
        ("duration", json!("500ms"), "at least 1s"),
        ("fps", json!(241), "0.1 to 240"),
        ("bitrate", json!("nonsense"), "bitrate"),
        ("file_name", json!("recording.png"), "Use .mp4"),
    ] {
        let mut input = args("start_recording");
        input[key] = value;
        let checked = run(&ctx, "start_recording", input);
        assert!(
            issue(&checked, key, Severity::Error, fragment),
            "{key}: {:?}",
            checked.issues
        );
    }
}

#[test]
fn unavailable_media_encoders_and_destination_problems_are_typed() {
    let mut ctx = context();
    ctx.media.available = false;
    let checked = run(&ctx, "start_recording", args("start_recording"));
    assert!(issue(&checked, "codec", Severity::Error, "Install FFmpeg"));
    ctx.media.available = true;
    let mut input = args("start_recording");
    input["codec"] = json!("av1");
    let checked = run(&ctx, "start_recording", input);
    assert!(issue(&checked, "codec", Severity::Error, "av1 has none"));
    ctx.destinations[0].problem = Some("destination volume is not mounted".into());
    let mut input = args("capture_photos");
    input["destination"] = json!("line-1");
    let checked = run(&ctx, "capture_photos", input);
    assert!(
        checked
            .issues
            .iter()
            .any(|issue| issue.field.as_deref() == Some("destination")
                && issue.code == IssueCode::Unavailable)
    );
}

#[test]
fn schedules_collect_end_interval_tag_and_file_errors_together() {
    let ctx = context();
    for ends in [
        vec![],
        vec!["duration", "count"],
        vec!["duration", "until"],
        vec!["count", "until"],
    ] {
        let mut input = args("schedule_captures");
        input["count"] = Value::Null;
        for key in ends {
            input[key] = match key {
                "duration" => json!("5m"),
                "count" => json!(2),
                _ => json!(ctx.zone.rfc3339(NOW + 600_000)),
            };
        }
        let checked = run(&ctx, "schedule_captures", input);
        assert!(issue(
            &checked,
            "duration",
            Severity::Error,
            "Choose only one way to end this schedule."
        ));
    }
    let mut input = args("schedule_captures");
    input["count"] = Value::Null;
    input["every"] = json!("2");
    input["file_name"] = json!("../escape");
    input["tags"] = json!([{"key":"Bad","value":"x"}]);
    let checked = run(&ctx, "schedule_captures", input);
    for field in ["duration", "every", "file_name", "tags"] {
        assert!(
            checked
                .issues
                .iter()
                .any(|issue| issue.field.as_deref() == Some(field) && issue.is_error()),
            "{field}: {:?}",
            checked.issues
        );
    }
}

#[test]
fn schedule_counts_horizon_and_job_limit_are_checked_before_execution() {
    let mut ctx = context();
    for bad in [0, 100001] {
        let mut input = args("schedule_captures");
        input["count"] = json!(bad);
        assert!(issue(
            &run(&ctx, "schedule_captures", input),
            "count",
            Severity::Error,
            "1 to 100,000"
        ));
    }
    let mut input = args("schedule_captures");
    input["every"] = json!("10d");
    input["count"] = json!(1000);
    assert!(issue(
        &run(&ctx, "schedule_captures", input),
        "count",
        Severity::Error,
        "within the next 10 years"
    ));
    ctx.snapshot.cameras[0].jobs = (1..=32).map(|id| job(id, "running")).collect();
    assert!(issue(
        &run(&ctx, "schedule_captures", args("schedule_captures")),
        "camera",
        Severity::Error,
        "at most 32 may be active"
    ));
    let mut plan = PlanState::new(&context());
    plan.jobs.insert("sim:0".into(), 31);
    ready(&run_in(
        &context(),
        &mut plan,
        "schedule_captures",
        args("schedule_captures"),
    ));
    assert!(
        !run_in(
            &context(),
            &mut plan,
            "schedule_captures",
            args("schedule_captures")
        )
        .ready()
    );
}

#[test]
fn invalid_schedules_do_not_change_later_plan_budget_connections_or_jobs() {
    let ctx = context();
    let mut plan = PlanState::new(&ctx);
    let mut input = args("schedule_captures");
    input["camera"] = json!("sim:2");
    input["every"] = json!("0s");
    assert!(!run_in(&ctx, &mut plan, "schedule_captures", input).ready());
    assert!(!plan.connected.contains("sim:2"));
    assert_eq!(plan.active_jobs("sim:2"), 0);
    assert_eq!((plan.planned_files, plan.planned_bytes), (0, 0));
}

#[test]
fn cancelling_completed_missing_or_wrong_camera_jobs_is_unavailable() {
    let ctx = context();
    for id in [3, 999] {
        let mut input = args("cancel_job");
        input["job_id"] = json!(id);
        let checked = run(&ctx, "cancel_job", input);
        assert!(issue(
            &checked,
            "job_id",
            Severity::Error,
            "Choose an active schedule from list_jobs."
        ));
    }
    for tool in ["stop_recording", "cancel_job", "stream_to_url"] {
        let mut input = args(tool);
        input["camera"] = json!("sim:2");
        assert!(issue(
            &run(&ctx, tool, input),
            "camera",
            Severity::Error,
            "Connect the camera first; propose connect_camera."
        ));
    }
}

#[test]
fn canceling_a_job_twice_does_not_free_a_second_schedule_slot() {
    let mut ctx = context();
    ctx.snapshot.cameras[0].jobs = (1..=32).map(|id| job(id, "running")).collect();
    let mut plan = PlanState::new(&ctx);
    ready(&run_in(&ctx, &mut plan, "cancel_job", args("cancel_job")));
    assert_eq!(plan.active_jobs("sim:0"), 31);
    let repeated = run_in(&ctx, &mut plan, "cancel_job", args("cancel_job"));
    assert!(issue(
        &repeated,
        "job_id",
        Severity::Error,
        "already canceled by an earlier step"
    ));
    assert_eq!(plan.active_jobs("sim:0"), 31);
    ready(&run_in(
        &ctx,
        &mut plan,
        "schedule_captures",
        args("schedule_captures"),
    ));
    assert_eq!(plan.active_jobs("sim:0"), 32);
    let extra = run_in(
        &ctx,
        &mut plan,
        "schedule_captures",
        args("schedule_captures"),
    );
    assert!(issue(&extra, "camera", Severity::Error, "at most 32"));
}

#[test]
fn prior_feature_changes_make_photo_estimates_use_available_sensor_maxima() {
    let mut ctx = context();
    ctx.snapshot.cameras[0].streaming = false;
    ctx.snapshot.cameras[0].jobs.clear();
    ctx.snapshot.cameras[0].features.extend([
        feature("WidthMax", "Integer", json!(3840)),
        feature("HeightMax", "Integer", json!(2160)),
    ]);
    let mut plan = PlanState::new(&ctx);
    let before = run_in(&ctx, &mut plan, "capture_photos", args("capture_photos"));
    ready(&before);
    let previous = plan.planned_bytes;
    assert_eq!(previous, 640 * 480 * 4 + 65_536);
    let set = run_in(
        &ctx,
        &mut plan,
        "set_camera_features",
        json!({"camera":"sim:0","values":[{"feature":"Width","value":"1920","expect":null}],"restart_stream":false}),
    );
    ready(&set);
    let after = run_in(&ctx, &mut plan, "capture_photos", args("capture_photos"));
    ready(&after);
    assert_eq!(plan.planned_bytes - previous, 3840 * 2160 * 4 + 65_536);
    assert!(issue(
        &after,
        "camera",
        Severity::Warning,
        "storage estimate uses maximum sensor dimensions"
    ));
}

#[test]
fn changed_dimension_estimates_use_feature_bounds_or_conservative_fallbacks() {
    for sensor_max in [Some((2048.0, 1024.0)), None] {
        let mut ctx = context();
        let features = &mut ctx.snapshot.cameras[0].features;
        features.iter_mut().find(|f| f.name == "Width").unwrap().max = sensor_max.map(|(w, _)| w);
        features
            .iter_mut()
            .find(|f| f.name == "Height")
            .unwrap()
            .max = sensor_max.map(|(_, h)| h);
        let mut plan = PlanState::new(&ctx);
        plan.changed_features
            .entry("sim:0".into())
            .or_default()
            .insert("Width".into());
        let checked = run_in(&ctx, &mut plan, "capture_photos", args("capture_photos"));
        ready(&checked);
        let (width, height) = sensor_max.unwrap_or((4096.0, 4096.0));
        assert_eq!(
            plan.planned_bytes,
            width as u64 * height as u64 * 4 + 65_536
        );
    }
    let mut ctx = context();
    let width = ctx.snapshot.cameras[0]
        .features
        .iter_mut()
        .find(|f| f.name == "Width")
        .unwrap();
    width.value = Some(json!(9000));
    width.max = None;
    let mut plan = PlanState::new(&ctx);
    plan.changed_features
        .entry("sim:0".into())
        .or_default()
        .insert("Width".into());
    ready(&run_in(
        &ctx,
        &mut plan,
        "capture_photos",
        args("capture_photos"),
    ));
    assert_eq!(plan.planned_bytes, 9000 * 4096 * 4 + 65_536);
}

#[test]
fn jpeg_capture_and_schedule_require_jpeg_support() {
    let mut ctx = context();
    ctx.compiled.jpeg = false;
    for tool in ["capture_photos", "schedule_captures"] {
        let mut input = args(tool);
        input["format"] = json!("jpeg");
        let checked = run(&ctx, tool, input.clone());
        assert!(!checked.ready());
        assert!(checked.issues.iter().any(|issue| {
            issue.field.as_deref() == Some("format") && issue.code == IssueCode::Unsupported
        }));
        ctx.compiled.jpeg = true;
        ready(&run(&ctx, tool, input));
        ctx.compiled.jpeg = false;
    }
}

#[test]
fn stream_urls_are_normalized_and_credentials_never_enter_operations_or_debug() {
    let ctx = context();
    let mut input = args("stream_to_url");
    input["url"] = json!("  RTMPS://live.example/live/  ");
    input["duration"] = json!("90s");
    let checked = run(&ctx, "stream_to_url", input);
    ready(&checked);
    assert_eq!(checked.call.args["url"], "rtmps://live.example/live/");
    assert_eq!(checked.call.args["duration"], "1m30s");
    assert!(!issue(&checked, "url", Severity::Warning, "Unencrypted"));
    let mut input = args("stream_to_url");
    input["url"] = json!("rtsp://alice@host/video");
    let checked = run(&ctx, "stream_to_url", input);
    ready(&checked);
    assert_eq!(checked.call.args["url"], "rtsp://alice@host/video");
    assert!(
        matches!(command(&checked), SessionCommand::Forward { output, .. } if output == "rtsp://alice@host/video")
    );
    assert_eq!(checked.summary, "Stream sim:0 to host.");
    for (url, secret) in [
        (
            "rtsp://alice:sensitive-password@host/video",
            "sensitive-password",
        ),
        (
            "srt://host:9000?passphrase=supersecretpass",
            "supersecretpass",
        ),
        ("rtmp://host/live/STREAM-SECRET-KEY", "STREAM-SECRET-KEY"),
        (
            "rtsp://host/video?api_key=private-api-key",
            "private-api-key",
        ),
        (
            "rtsp://host/video?%61pi%2Dkey=encoded-api-key",
            "encoded-api-key",
        ),
        (
            "rtsp://host/video?client_secret=private-client-secret",
            "private-client-secret",
        ),
        (
            "rtsp://host/video?X-Amz-Signature=private-signature",
            "private-signature",
        ),
        (
            "rtsp://host/video?access_token=secret-token",
            "secret-token",
        ),
        (
            "rtsp://alice@host/video?access_token=secret-token",
            "secret-token",
        ),
        (
            "rtmp://alice@host/live/STREAM-SECRET-KEY",
            "STREAM-SECRET-KEY",
        ),
    ] {
        let mut input = args("stream_to_url");
        input["url"] = json!(url);
        let checked = run(&ctx, "stream_to_url", input);
        assert!(issue(
            &checked,
            "url",
            Severity::Error,
            "Remove the password from the URL and enter it in Stream password."
        ));
        assert!(!checked.value().to_string().contains(secret));
        assert!(!format!("{checked:?}").contains(secret));
        assert!(checked.ops.is_empty());
    }
    for url in [
        "http://host/video",
        "rtsp:///missing",
        "rtsp://host:99999/video",
        "rtsp://host/%xx",
    ] {
        let mut input = args("stream_to_url");
        input["url"] = json!(url);
        assert!(issue(
            &run(&ctx, "stream_to_url", input),
            "url",
            Severity::Error,
            ""
        ));
    }
}

#[test]
fn tags_reject_duplicates_invalid_values_and_more_than_sixteen_entries() {
    let ctx = context();
    for tags in [
        json!([{"key":"line","value":"one"},{"key":"line","value":"two"}]),
        json!([{"key":"Line","value":"one"}]),
        json!([{"key":"line","value":"bad\nvalue"}]),
        json!(
            (0..17)
                .map(|n| json!({"key":format!("tag{n}"),"value":"x"}))
                .collect::<Vec<_>>()
        ),
    ] {
        let mut input = args("capture_photos");
        input["tags"] = tags;
        assert!(issue(
            &run(&ctx, "capture_photos", input),
            "tags",
            Severity::Error,
            ""
        ));
    }
    let mut input = args("capture_photos");
    input["tags"] = json!([{"key":"line","value":"assembly"}]);
    let checked = run(&ctx, "capture_photos", input);
    ready(&checked);
    assert!(
        matches!(command(&checked),SessionCommand::Capture {context,..} if context.get("line").map(String::as_str)==Some("assembly"))
    );
}

#[test]
fn capture_schemas_are_strict_and_secret_fields_are_never_provider_arguments() {
    let ctx = context();
    for tool in [
        "capture_photos",
        "start_recording",
        "schedule_captures",
        "stop_recording",
        "cancel_job",
        "stream_to_url",
    ] {
        let mut input = args(tool);
        input["extra"] = json!(true);
        assert!(!run(&ctx, tool, input).ready(), "{tool}");
        let mut input = args(tool);
        input.as_object_mut().unwrap().remove("camera");
        assert!(issue(
            &run(&ctx, tool, input),
            "camera",
            Severity::Error,
            "Provide `camera`"
        ));
    }
    let spec = crate::tools::spec("stream_to_url").unwrap();
    assert!(
        spec.fields
            .iter()
            .any(|field| field.key == "password" && field.ui_only)
    );
    assert!((spec.schema)()["properties"].get("password").is_none());
}
