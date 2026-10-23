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
impl Workbench {
    pub(in crate::gui) fn scene_assistant_task(&mut self, word: &str, _late: bool) -> bool {
        if !WORDS.contains(&word) {
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
            });
        }
        _ => {}
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
