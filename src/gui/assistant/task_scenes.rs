//! Task review fixtures never start a run. Their checked approval facts and
//! restore proposals come from the same pure tool checks as live work.
use super::*;
use crate::{
    assistant::{
        ProviderId, Usage,
        approval::ApprovalRequest,
        limits::Totals,
        run::{Leftover, Outcome, RunOutcome, RunRecord, RunSnapshot, RunState, SettingChange},
    },
    tools::{self, Checked, PlanState, StepId, ToolCall, context::Context},
};
use serde_json::json;

pub(in crate::gui) const WORDS: [&str; 6] = [
    "quick-command-task-setup",
    "quick-command-task-running",
    "quick-command-task-approval",
    "quick-command-task-paused-human",
    "quick-command-task-limit",
    "quick-command-task-done",
];
pub(in crate::gui) const DATA_WORDS: [&str; 10] = [
    "quick-command-task-data-setup",
    "quick-command-task-data-auth",
    "quick-command-task-data-running",
    "quick-command-task-mixed-running",
    "quick-command-task-data-approval",
    "quick-command-task-data-paused-human",
    "quick-command-task-data-error",
    "quick-command-task-data-done",
    "quick-command-task-separate-setup",
    "quick-command-task-separate-done",
];

impl Workbench {
    pub(in crate::gui) fn scene_assistant_task(&mut self, word: &str, _late: bool) -> bool {
        if !WORDS.contains(&word) && !DATA_WORDS.contains(&word) {
            return false;
        }
        self.assistant.quick.open = true;
        self.assistant.quick.synthetic = true;
        self.assistant.tasks = fixture(word, self.snapshot.clone());
        true
    }
}

pub(in crate::gui) fn fixture(word: &str, snapshot: SessionSnapshot) -> tasks::State {
    let mut state = tasks::State::default();
    populate(&mut state, word, snapshot);
    state
}

fn populate(state: &mut tasks::State, word: &str, snapshot: SessionSnapshot) {
    if DATA_WORDS.contains(&word) {
        populate_data(state, word, snapshot);
        return;
    }
    let context = context(snapshot);
    let camera = context.snapshot.cameras[0].info.id.clone();
    let now = context.now_ms;
    state.synthetic = true;
    state.now_ms = now;
    state.mode = tasks::Mode::Autonomous;
    state.goal = "Balance the exposure, then take a photo to verify the result.".into();
    state.cameras.insert(camera.clone());
    state.destinations.insert("inspection-archive".into());
    state.allow_images = true;
    state.settings.provider = Some(ProviderId::Anthropic);
    state.settings.providers.insert(
        ProviderId::Anthropic,
        crate::assistant::ProviderSettings {
            saved_key: true,
            ..Default::default()
        },
    );
    state.context = Some(context.clone());
    if word == "quick-command-task-setup" {
        return;
    }
    state.scene_active = true;
    state.snapshot = RunSnapshot {
        run_id: "r-20261018-143200-abcd".into(),
        actor_id: "assistant:scene:abcd".into(),
        state: RunState::Running,
        round: 12,
        elapsed_ms: 372_000,
        summary: "Comparing the current frame with the previous exposure.".into(),
        totals: Totals {
            rounds: 12,
            usage: Usage {
                input: 140_000,
                output: 10_000,
                estimated: true,
                ..Default::default()
            },
            cost_usd: None,
            images: 14,
            captures: 6,
            bytes: 32 * 1024 * 1024,
            recordings: 0,
            setting_changes: 3,
            ..Default::default()
        },
    };
    state.latest = Some(sheet::SentImage::new(scenes::image()));
    state.latest_at_ms = Some(now - 2_000);
    state.latest_camera = Some(camera.clone());
    state.metrics = "Brightness 46 % · clipped 0.1 % · sharpness 812".into();
    state.activity = vec![
        tasks::ActivityEntry {
            time_ms: now - 30_000,
            summary: "Inspected the camera: mean brightness 0.31, highlights 0.0 %.".into(),
            level: Level::Done,
        },
        tasks::ActivityEntry {
            time_ms: now - 20_000,
            summary: "Changed ExposureTime from 5000 to 8000 µs.".into(),
            level: Level::Done,
        },
        tasks::ActivityEntry {
            time_ms: now - 2_000,
            summary: "Captured a verification photo in the capture folder.".into(),
            level: Level::Done,
        },
    ];
    state.changes = vec![tasks::ChangeEntry {
        time_ms: now - 20_000,
        camera: camera.clone(),
        feature: "ExposureTime".into(),
        before: "5000".into(),
        after: "8000".into(),
        can_restore: true,
    }];
    match word {
        "quick-command-task-approval" => {
            state.mode = tasks::Mode::Supervised;
            state.snapshot.state = RunState::WaitingApproval;
            state.snapshot.summary =
                "Waiting for your approval before changing the exposure.".into();
            state.approval = Some(approval(&context));
        }
        "quick-command-task-paused-human" => {
            state.snapshot.state = RunState::Paused;
            state.snapshot.summary =
                "Paused — Desktop changed Gain to 2.0. Review the camera before continuing.".into();
            state.activity.push(tasks::ActivityEntry {
                time_ms: now,
                summary: "Desktop changed Gain to 2.0; the assistant paused.".into(),
                level: Level::Warning,
            });
        }
        "quick-command-task-limit" | "quick-command-task-done" => {
            state.scene_active = false;
            let limited = word.ends_with("-limit");
            state.snapshot.state = if limited {
                RunState::LimitReached
            } else {
                RunState::Finished
            };
            if limited {
                state.snapshot.round = state.limits.max_rounds;
                state.snapshot.totals.rounds = state.limits.max_rounds;
            }
            state.snapshot.summary = if limited {
                "The task reached its round limit.".into()
            } else {
                "The exposure is balanced. Saved a verification photo.".into()
            };
            state.outcome = Some(RunOutcome {
                run_id: state.snapshot.run_id.clone(),
                outcome: if limited {
                    Outcome::Limit
                } else {
                    Outcome::Finished
                },
                summary: state.snapshot.summary.clone(),
                totals: state.snapshot.totals.clone(),
                signal_reports: Vec::new(),
                leftovers: vec![
                    Leftover {
                        kind: "recording".into(),
                        camera: camera.clone(),
                        summary: "Recording on the workbench camera.".into(),
                        job_id: None,
                        until_ms: Some(now + 30_000),
                    },
                    Leftover {
                        kind: "schedule".into(),
                        camera: camera.clone(),
                        summary: "Schedule #3 is still taking photos.".into(),
                        job_id: Some(3),
                        until_ms: None,
                    },
                    Leftover {
                        kind: "auto".into(),
                        camera: camera.clone(),
                        summary: "Auto exposure remains on.".into(),
                        job_id: None,
                        until_ms: None,
                    },
                ],
                log: PathBuf::from(
                    "/tmp/capturefab-scene/assistant/runs/r-20261018-143200-abcd.jsonl",
                ),
                restore: RunRecord {
                    run_id: state.snapshot.run_id.clone(),
                    actor_id: state.snapshot.actor_id.clone(),
                    settings: vec![SettingChange {
                        camera: camera.clone(),
                        feature: "ExposureTime".into(),
                        before: "5000".into(),
                        last: "8000".into(),
                        needs_review: false,
                    }],
                    worker_pids: [(camera, context.snapshot.cameras[0].worker_pid)].into(),
                    forwarders: Default::default(),
                    ..Default::default()
                },
                task_data: None,
            });
        }
        _ => {}
    }
}

fn populate_data(state: &mut tasks::State, word: &str, snapshot: SessionSnapshot) {
    populate(state, "quick-command-task-running", snapshot);
    let now = state.now_ms;
    let separate = word.contains("-separate-");
    let uri = if separate {
        "sim-signal://task-line1"
    } else if word.ends_with("-approval") {
        "mqtt://line1.example/line1"
    } else {
        "sim-signal://line1"
    };
    let binding = tools::context::DataSourceRef::from_uri(uri).expect("scene source");
    let slot = "a".repeat(32);
    let token = "b".repeat(32);
    state.goal = if word.contains("-mixed-") {"Capture product images alongside temperature and line speed, then export one analysis bundle."} else {"Log line temperature and speed, verify every sample was saved, then export the dataset."}.into();
    if !word.contains("-mixed-") {
        state.cameras.clear();
        state.latest = None;
        state.latest_camera = None;
        state.metrics.clear();
        state.changes.clear();
    }
    state.destinations.clear();
    state.allow_images = word.contains("-mixed-");
    state.allow_data = true;
    state.data_sources.insert(uri.into());
    state.dataset = Some(slot.clone());
    let context = state.context.as_mut().unwrap();
    context.allow_data = true;
    context.data_sources = vec![binding.clone()];
    context.data_dataset = Some(slot.clone());
    context.snapshot.signal_sink = Some(crate::signals::SignalSinkStatus {
        attached: true,
        dataset_slot_id: Some(slot.clone()),
        generation: 3,
        ..Default::default()
    });
    context.snapshot.dataset = Some(crate::dataset::DatasetStatus {
        slot_id: Some(slot),
        dataset_id: "line1-inspection".into(),
        name: "Line 1 inspection".into(),
        samples: 398,
        bytes: 16 * 1024 * 1024,
        max_bytes: 128 * 1024 * 1024,
        ..Default::default()
    });
    let source = serde_json::from_value(json!({"id":binding.reference,"capture_id":token,"protocol":if uri.starts_with("mqtt:"){"mqtt"}else{"sim-signal"},"uri":uri,"interval_ms":100,"state":"running","started_ms":now-20_000,"samples":400,"written":398,"last_sample_ms":now-100,"deadline_ms":now+280_000,"tags":[{"tag":"temperature","unit":"°C","value":42.3,"quality":"good","ts_ms":now-100},{"tag":"line_speed","unit":"m/min","value":12.4,"quality":"good","ts_ms":now-100}]})).expect("scene status");
    context.snapshot.signals = vec![source];
    state.snapshot.summary =
        "Capturing the approved source. Samples are arriving and the dataset writer is keeping up."
            .into();
    state.snapshot.totals.sources = 1;
    state.snapshot.totals.samples = 10_000;
    state.snapshot.totals.data_bytes = 8 * 1024 * 1024;
    state.snapshot.totals.signal_ms = 300_000;
    if state.cameras.is_empty() {
        state.snapshot.totals.images = 0;
        state.snapshot.totals.captures = 0;
        state.snapshot.totals.setting_changes = 0;
        state.snapshot.totals.bytes = 8 * 1024 * 1024;
    }
    context.snapshot.signals[0].payload_bytes = 48_000;
    context.snapshot.signals[0].written_bytes = 47_760;
    state.activity = vec![
        tasks::ActivityEntry {time_ms:now-20_000,summary:"Opened Line 1 inspection in the capture folder.".into(),level:Level::Done},
        tasks::ActivityEntry {time_ms:now-19_000,summary:"Started a finite source capture: 5 minutes, at most 10,000 values and 8 MiB.".into(),level:Level::Done},
        tasks::ActivityEntry {time_ms:now-100,summary:"400 sample values acquired; 398 written. Waiting for the remaining buffered samples.".into(),level:Level::Done},
    ];
    match word {
        "quick-command-task-data-setup"
        | "quick-command-task-data-auth"
        | "quick-command-task-separate-setup" => {
            state.snapshot = Default::default();
            state.scene_active = false;
            state.activity.clear();
            if word == "quick-command-task-data-auth" {
                state.source_setup.fixture();
            }
        }
        "quick-command-task-data-approval" => {
            state.mode = tasks::Mode::Supervised;
            state.snapshot.state = RunState::WaitingApproval;
            state.snapshot.summary =
                "Review the source connection before the assistant contacts it.".into();
            let call = ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":binding.reference,"timeout":"5s"}),
            };
            let checked = tools::check(
                context,
                &mut PlanState::assistant(context),
                StepId(7),
                &call,
                &|_, _| false,
            );
            state.approval = Some(ApprovalRequest::new(
                7,
                &state.snapshot.run_id,
                &checked,
                now + 120_000,
            ));
        }
        "quick-command-task-data-paused-human" => {
            state.snapshot.state = RunState::Paused;
            state.snapshot.summary = "Paused because the shared dataset was replaced. Stop this task and start a new task to approve the current dataset.".into();
            state.activity.push(tasks::ActivityEntry {time_ms:now,summary:"Desktop opened a different dataset. Previous approvals expired and the assistant paused.".into(),level:Level::Warning});
            context.snapshot.dataset.as_mut().unwrap().slot_id = Some("c".repeat(32));
            context.snapshot.dataset.as_mut().unwrap().name = "Manual review".into();
            context
                .snapshot
                .signal_sink
                .as_mut()
                .unwrap()
                .dataset_slot_id = Some("c".repeat(32));
        }
        "quick-command-task-data-error"
        | "quick-command-task-data-done"
        | "quick-command-task-separate-done" => {
            let failed = word.ends_with("-error");
            state.scene_active = false;
            state.snapshot.state = if failed {
                RunState::Failed
            } else {
                RunState::Finished
            };
            let source = &mut context.snapshot.signals[0];
            source.state = if failed {
                crate::signals::SourceState::Failed
            } else {
                crate::signals::SourceState::Stopped
            };
            source.completed_ms = Some(now);
            source.persistence_complete = !failed;
            if failed {
                source.persistence_error =
                    Some("Storage budget is full. 398 of 400 sample values were stored.".into());
                source.dropped = 2;
            } else {
                source.written = source.samples;
                source.written_bytes = source.payload_bytes;
                context.snapshot.dataset.as_mut().unwrap().samples = source.samples;
            }
            state.snapshot.summary = if failed {"Capture stopped with incomplete persistence. Review the two dropped sample values before using this dataset."} else {"All 400 sample values were saved and the analysis bundle was exported with verified checksums."}.into();
            state.outcome = Some(RunOutcome {
                run_id: state.snapshot.run_id.clone(),
                outcome: if failed {
                    Outcome::Failed
                } else {
                    Outcome::Finished
                },
                summary: state.snapshot.summary.clone(),
                totals: state.snapshot.totals.clone(),
                signal_reports: vec![crate::assistant::run::SignalReport::from_status(source)],
                leftovers: if failed {
                    vec![Leftover {
                        kind: "signal_persistence".into(),
                        camera: binding.reference.clone(),
                        summary: "398 of 400 values persisted; 2 dropped. Storage budget is full."
                            .into(),
                        job_id: None,
                        until_ms: None,
                    }]
                } else {
                    Vec::new()
                },
                log: PathBuf::from(
                    "/tmp/capturefab-scene/assistant/runs/r-20261018-143200-abcd.jsonl",
                ),
                restore: RunRecord {
                    signal_captures: [(binding.reference.clone(), token.clone())].into(),
                    ..Default::default()
                },
                task_data: None,
            });
            state.activity.push(tasks::ActivityEntry {
                time_ms: now,
                summary: state.snapshot.summary.clone(),
                level: if failed { Level::Error } else { Level::Done },
            });
        }
        _ => {}
    }
    if separate {
        state.separate_dataset = true;
        state.dataset = None;
        state.goal = "Log temperature and line speed to a separate dataset for this sensor-only task, then summarize the results.".into();
        let cap = state.task_dataset_cap().expect("scene limits");
        let context = state.context.as_mut().unwrap();
        context.task_data_available = cfg!(all(unix, feature = "parquet"));
        context.data_dataset = None;
        // These are fresh source bindings, never adoption of a running source
        // or the unrelated shared dataset shown as an unchecked alternative.
        context.snapshot.signals.clear();
        if let Some(outcome) = &mut state.outcome {
            use crate::{
                dataset::task::{ExtentUsage, TaskCatalogIdentity, TaskCatalogStatus},
                session::task_data::{TaskDataPhase, TaskDataStatus},
            };
            let identity = TaskCatalogIdentity {
                dataset_id: "c0123456789abcde".into(),
                slot_id: "d".repeat(32),
            };
            let summary = "All 400 sample values were saved. The sensor-only task dataset was closed and retained.";
            state.snapshot.summary = summary.into();
            state.snapshot.totals.bytes = cap + 48_000;
            outcome.summary = summary.into();
            outcome.totals.bytes = state.snapshot.totals.bytes;
            outcome.task_data = Some(TaskDataStatus {
                id: identity.slot_id.clone(),
                phase: TaskDataPhase::Retained,
                catalog: TaskCatalogStatus {
                    identity: identity.clone(),
                    dataset: crate::dataset::DatasetStatus {
                        path: "/captures/task-d0123456789abcdef0123456789abcdef/task.duckdb".into(),
                        dataset_id: identity.dataset_id,
                        slot_id: Some(identity.slot_id.clone()),
                        name: "Assistant task dataset".into(),
                        samples: 400,
                        signals: 2,
                        events: 4,
                        bytes: 4 * 1024 * 1024,
                        max_bytes: cap,
                        ..Default::default()
                    },
                    extents: ExtentUsage {
                        limit: cap,
                        used: 6 * 1024 * 1024,
                        peak: 7 * 1024 * 1024,
                        ..Default::default()
                    },
                    quarantined: false,
                    closed: true,
                },
                sink: crate::signals::SignalSinkStatus {
                    dataset_slot_id: Some(identity.slot_id),
                    ..Default::default()
                },
                captures: [(binding.reference, token)].into(),
                error: None,
            });
            state.activity = vec![
                tasks::ActivityEntry {
                    time_ms: now - 20_000,
                    summary: format!(
                        "Reserved {} for this task's separate dataset.",
                        crate::analytics::human_bytes(cap)
                    ),
                    level: Level::Done,
                },
                tasks::ActivityEntry {
                    time_ms: now - 19_000,
                    summary: "Started the approved sensor capture in the separate dataset.".into(),
                    level: Level::Done,
                },
                tasks::ActivityEntry {
                    time_ms: now,
                    summary: summary.into(),
                    level: Level::Done,
                },
            ];
        }
    }
}

pub(in crate::gui) fn context(snapshot: SessionSnapshot) -> Context {
    let mut context = scenes::context(snapshot);
    for camera in &mut context.snapshot.cameras {
        let mut features = camera.features.to_vec();
        features.retain(|feature| feature.name != "ExposureTime");
        features.push(FeatureInfo {
            name: "ExposureTime".into(),
            display_name: "Exposure time".into(),
            kind: "Float".into(),
            value: Some(json!(8000.0)),
            writable: true,
            description: "Exposure time in microseconds.".into(),
            unit: Some("µs".into()),
            min: Some(10.0),
            max: Some(100_000.0),
            inc: Some(1.0),
            choices: Vec::new(),
            error: None,
        });
        camera.features = features.into();
    }
    context
}

pub(in crate::gui) fn checked_change(context: &Context) -> Checked {
    let camera = &context.snapshot.cameras[0].info.id;
    let mut state = PlanState::assistant(context);
    let call = ToolCall {
        tool: "set_camera_features".into(),
        args: json!({"camera":camera,"values":[{"feature":"ExposureTime","value":"10000","expect":"8000"}],"restart_stream":false}),
    };
    let checked = tools::check(context, &mut state, StepId(7), &call, &|_, _| false);
    assert!(checked.ready(), "{checked:?}");
    checked
}

pub(in crate::gui) fn approval(context: &Context) -> ApprovalRequest {
    ApprovalRequest::new(
        7,
        "r-20261018-143200-abcd",
        &checked_change(context),
        context.now_ms + 120_000,
    )
}

#[cfg(all(test, feature = "s3"))]
pub(in crate::gui) fn secret_approval(context: &Context) -> ApprovalRequest {
    let call = ToolCall {
        tool: "create_s3_destination".into(),
        args: json!({"name":"inspection-remote-new","bucket":"inspection-review","region":"us-east-1","endpoint":"https://s3.example.invalid","prefix":"camera","path_style":true,"credentials":"keychain","profile":null,"access_key_id":"EXAMPLE-ACCESS","keep_local":false}),
    };
    let mut state = PlanState::assistant(context);
    let checked = tools::check(context, &mut state, StepId(8), &call, &|_, _| true);
    assert!(checked.ready(), "{checked:?}");
    ApprovalRequest::new(
        8,
        "r-20261018-143200-abcd",
        &checked,
        context.now_ms + 120_000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_scenes_are_display_only_on_both_scene_passes() {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "assistant-task-scenes".into(),
            true,
            None,
        );
        for word in WORDS {
            for late in [false, true] {
                assert!(app.scene_assistant_task(word, late));
                assert!(app.assistant.quick.open);
                assert!(app.assistant.tasks.synthetic);
                assert!(app.handle.snapshot().cameras.is_empty());
                assert!(app.handle.snapshot().activity.is_empty());
            }
        }
        assert!(!app.scene_assistant_task("unrelated", false));
    }

    #[test]
    fn approval_scene_facts_come_from_typed_checked_camera_operations() {
        let state = fixture("quick-command-task-approval", SessionSnapshot::default());
        let request = state.approval.unwrap();
        assert_eq!(request.tool, "set_camera_features");
        assert!(!request.facts.is_empty());
        assert!(
            request.facts.iter().any(
                |(name, value)| name.contains("ExposureTime") || value.contains("ExposureTime")
            )
        );
        assert_eq!(request.expires_at_ms, state.now_ms + 120_000);
    }
}
