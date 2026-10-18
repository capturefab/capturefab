//! The GUI timeline retains worker-issued checkpoints, so a camera edit stays
//! attached to its camera even after selection changes.
use super::*;
use anyhow::Context;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct State {
    batches: HashMap<(Job, Instant), Vec<Change>>,
    replay: Option<Replay>,
}

struct Replay {
    undo: bool,
    queue: VecDeque<Change>,
    current: Change,
    completed: Vec<Change>,
    rollback: bool,
    failure: Option<String>,
}

#[derive(Clone)]
pub(super) struct Change {
    camera: String,
    token: String,
    label: String,
    job: Job,
    changed_at_us: u64,
}

impl Change {
    pub(super) fn label(&self) -> String {
        self.label.clone()
    }
    pub(super) fn camera(&self) -> &str {
        &self.camera
    }
    pub(super) fn token(&self) -> &str {
        &self.token
    }
    pub(super) fn changed_at_us(&self) -> u64 {
        self.changed_at_us
    }
    pub(super) fn from_status(
        camera: &str,
        entry: &crate::session::CameraHistoryEntry,
    ) -> Option<Self> {
        let job = match entry.kind.as_str() {
            "set" => Job::Set(entry.feature.clone()?),
            "set_many" => Job::SetMany,
            "auto" => Job::AutoOn,
            "manual" => Job::AutoOff,
            "balance" => Job::Balance,
            "start" => Job::Start,
            "stop" => Job::Stop,
            _ => return None,
        };
        Some(Self {
            camera: camera.into(),
            token: entry.token.clone(),
            label: entry.label.clone(),
            job,
            changed_at_us: entry.last_changed_at_us,
        })
    }

    fn from_result(pending: &Pending, result: &serde_json::Value) -> Option<Self> {
        let checkpoint = result.get("camera_change")?;
        Some(Self {
            camera: checkpoint["camera"].as_str()?.to_owned(),
            token: checkpoint["token"].as_str()?.to_owned(),
            label: checkpoint["label"].as_str()?.to_owned(),
            job: pending.job.clone(),
            changed_at_us: checkpoint["last_changed_at_us"]
                .as_u64()
                .unwrap_or_default(),
        })
    }
}

impl Workbench {
    pub(super) fn replay_camera_change(&mut self, change: Change, undo: bool) -> Result<()> {
        self.enqueue_camera_replay(change, undo)
    }

    pub(super) fn replay_camera_batch(&mut self, changes: Vec<Change>, undo: bool) -> Result<()> {
        let mut queue = VecDeque::from(changes);
        if undo {
            queue.make_contiguous().reverse();
        }
        let current = queue.pop_front().context("camera batch is empty")?;
        self.enqueue_camera_replay(current.clone(), undo)?;
        self.camera_history_state.replay = Some(Replay {
            undo,
            queue,
            current,
            completed: Vec::new(),
            rollback: false,
            failure: None,
        });
        Ok(())
    }

    fn enqueue_camera_replay(&mut self, change: Change, undo: bool) -> Result<()> {
        let job = match (&change.job, undo) {
            (Job::Start, true) => Job::Stop,
            (Job::Stop, true) => Job::Start,
            (Job::AutoOn, true) => Job::AutoOff,
            (Job::AutoOff, true) => Job::AutoOn,
            _ => change.job.clone(),
        };
        let draft = job.feature().and_then(|feature| {
            if self.snapshot.active_camera.as_deref() == Some(change.camera()) {
                self.edits.get(feature).cloned()
            } else {
                self.camera_drafts
                    .get(&change.camera)
                    .and_then(|(edits, _)| edits.get(feature).cloned())
            }
        });
        let token = change.token.clone();
        let pending = self.try_submit(
            Some(&change.camera),
            job,
            SessionCommand::ReplayCameraChange {
                token: change.token,
                undo,
            },
        )?;
        pending.history_replay = Some(undo);
        pending.history_token = Some(token);
        pending.value = draft;
        Ok(())
    }

    /// True when this is a history replay and its completion was handled here.
    pub(super) fn camera_history_result(
        &mut self,
        pending: &Pending,
        result: &Result<serde_json::Value>,
    ) -> bool {
        if pending.history_replay.is_some() {
            // An own replay has already been positioned by the user's gesture.
            // Remember its worker timestamp before polling shared status, so
            // only later external transitions reposition that entry again.
            if let (Some(token), Ok(result)) = (&pending.history_token, result)
                && let Some(entries) = result["history"]["entries"].as_array()
                && let Some(entry) = entries.iter().find(|entry| entry["token"] == *token)
                && let (Some(applied), Some(at)) = (
                    entry["applied"].as_bool(),
                    entry["last_changed_at_us"].as_u64(),
                )
            {
                self.known_camera_history
                    .insert(token.clone(), (applied, at));
            }
            if self.camera_history_state.replay.is_some() {
                self.camera_batch_replied(pending, result);
                return true;
            }
            if let Err(error) = result {
                self.failed(pending, error);
                self.set_notice(
                    "Could not restore camera change",
                    Some(format!("{error:#}")),
                    Level::Error,
                );
            } else if let Ok(result) = result {
                self.refresh_replayed_draft(pending, result);
            }
            self.history_replayed(result.is_ok());
            return true;
        }
        let change = result
            .as_ref()
            .ok()
            .and_then(|result| Change::from_result(pending, result));
        if pending.batch {
            let key = (pending.job.clone(), pending.at);
            if let Some(change) = change {
                self.camera_history_state
                    .batches
                    .entry(key.clone())
                    .or_default()
                    .push(change);
            }
            if !self
                .pending
                .iter()
                .any(|other| other.batch && other.job == pending.job && other.at == pending.at)
                && let Some(changes) = self.camera_history_state.batches.remove(&key)
            {
                self.record_camera_batch_at(changes, pending.at);
            }
        } else if let Some(change) = change {
            self.record_camera_change_at(change, pending.at);
        }
        false
    }

    fn camera_batch_replied(&mut self, pending: &Pending, result: &Result<serde_json::Value>) {
        let mut replay = self
            .camera_history_state
            .replay
            .take()
            .expect("batch replay");
        if let Ok(result) = result {
            self.refresh_replayed_draft(pending, result);
            if !replay.rollback {
                replay.completed.push(replay.current.clone());
            }
        } else if let Err(error) = result {
            self.failed(pending, error);
            if replay.rollback {
                replay
                    .failure
                    .get_or_insert_with(String::new)
                    .push_str(&format!("; rollback failed: {error:#}"));
            } else {
                replay.failure = Some(format!("{error:#}"));
                replay.rollback = true;
                replay.queue = replay.completed.drain(..).rev().collect();
            }
        }
        loop {
            let Some(change) = replay.queue.pop_front() else {
                self.history_replayed(!replay.rollback);
                if let Some(error) = replay.failure {
                    self.set_notice("Could not restore camera batch", Some(error), Level::Error);
                }
                return;
            };
            let direction = if replay.rollback {
                !replay.undo
            } else {
                replay.undo
            };
            match self.enqueue_camera_replay(change.clone(), direction) {
                Ok(()) => {
                    replay.current = change;
                    self.camera_history_state.replay = Some(replay);
                    return;
                }
                Err(error) if replay.rollback => {
                    replay
                        .failure
                        .get_or_insert_with(String::new)
                        .push_str(&format!("; rollback could not start: {error:#}"));
                }
                Err(error) => {
                    replay.failure = Some(format!("{error:#}"));
                    replay.rollback = true;
                    replay.queue = replay.completed.drain(..).rev().collect();
                }
            }
        }
    }

    fn refresh_replayed_draft(&mut self, pending: &Pending, result: &serde_json::Value) {
        let (Some(camera), Some(feature)) = (&pending.camera, pending.job.feature()) else {
            return;
        };
        let stored = result["features"]
            .as_array()
            .and_then(|features| {
                features
                    .iter()
                    .find(|item| item["name"].as_str() == Some(feature))
            })
            .and_then(|item| item.get("value"))
            .filter(|value| !value.is_null())
            .map(value_text);
        let Some(stored) = stored else {
            return;
        };
        let (edits, sources) = if self.snapshot.active_camera.as_ref() == Some(camera) {
            (&mut self.edits, &mut self.edit_sources)
        } else {
            let (edits, sources) = self.camera_drafts.entry(camera.clone()).or_default();
            (edits, sources)
        };
        if edits.get(feature) == pending.value.as_ref() && edits.contains_key(feature) {
            edits.insert(feature.into(), stored.clone());
        }
        sources.insert(feature.into(), stored);
        self.inspect.write_errors.remove(feature);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_acknowledged_checkpoints_are_history_and_keep_the_original_camera() {
        let pending = Pending {
            camera: Some("sim:1".into()),
            ..Pending::unanswered(Job::Set("Gain".into()))
        };
        assert!(Change::from_result(&pending, &serde_json::json!({"value":3})).is_none());
        let change = Change::from_result(
            &pending,
            &serde_json::json!({"camera_change":{
                "camera":"sim:1", "token":"worker-4", "label":"Set Gain"
            }}),
        )
        .unwrap();
        assert_eq!(change.camera, "sim:1");
        assert_eq!(change.token, "worker-4");
        assert_eq!(change.label(), "Set Gain");
    }

    #[test]
    fn status_entries_of_every_reversible_kind_become_changes() {
        let entry = |kind: &str, feature: Option<&str>| crate::session::CameraHistoryEntry {
            token: format!("worker-{kind}"),
            label: "Set Gain, Width".into(),
            applied: true,
            created_at_ms: 0,
            last_changed_at_us: 5,
            feature: feature.map(Into::into),
            kind: kind.into(),
        };
        let change = Change::from_status("sim:0", &entry("set_many", None)).unwrap();
        assert_eq!(change.job, Job::SetMany);
        assert_eq!(
            (change.camera(), change.token(), change.label().as_str()),
            ("sim:0", "worker-set_many", "Set Gain, Width")
        );
        assert_eq!(
            Change::from_status("sim:0", &entry("set", Some("Gain")))
                .unwrap()
                .job,
            Job::Set("Gain".into())
        );
        assert!(Change::from_status("sim:0", &entry("set", None)).is_none());
        assert!(Change::from_status("sim:0", &entry("capture", None)).is_none());
    }

    #[test]
    fn asynchronous_replay_commits_only_success_and_refreshes_the_original_camera_draft() {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "camera-history-test".into(),
            true,
            None,
        );
        app.snapshot.active_camera = Some("sim:0".into());
        app.edits.insert("Gain".into(), "7".into());
        app.camera_drafts.insert(
            "sim:1".into(),
            (
                HashMap::from([("Gain".into(), "2".into())]),
                HashMap::from([("Gain".into(), "2".into())]),
            ),
        );
        let change = Change {
            camera: "sim:1".into(),
            token: "worker-1".into(),
            label: "Set Gain".into(),
            job: Job::Set("Gain".into()),
            changed_at_us: 0,
        };
        app.record_camera_change_at(change, app.now);
        let replay = |receiver| Pending {
            camera: Some("sim:1".into()),
            value: Some("2".into()),
            history_replay: Some(true),
            receiver,
            ..Pending::unanswered(Job::Set("Gain".into()))
        };
        let (sender, receiver) = mpsc::channel();
        app.history_replay = Some(true);
        app.pending.push(replay(receiver));
        app.poll();
        assert!(app.history.undo().is_some());
        assert!(app.history.redo().is_none());
        sender
            .send(Err(anyhow::anyhow!("camera write rejected")))
            .unwrap();
        app.poll();
        assert!(app.history.undo().is_some());
        assert!(app.history.redo().is_none());
        assert_eq!(app.camera_drafts["sim:1"].0["Gain"], "2");
        let (sender, receiver) = mpsc::channel();
        app.history_replay = Some(true);
        app.pending.push(replay(receiver));
        sender
            .send(Ok(
                serde_json::json!({"features":[{"name":"Gain","value":0.0}]}),
            ))
            .unwrap();
        app.poll();
        assert!(app.history.undo().is_none());
        assert!(app.history.redo().is_some());
        assert_eq!(app.camera_drafts["sim:1"].0["Gain"], "0");
        assert_eq!(app.camera_drafts["sim:1"].1["Gain"], "0");
        assert_eq!(app.edits["Gain"], "7");
        // A newer draft typed during a reply is retained; its source still
        // tracks the acknowledged camera value.
        app.camera_drafts
            .get_mut("sim:1")
            .unwrap()
            .0
            .insert("Gain".into(), "9".into());
        let pending = replay(mpsc::channel().1);
        app.refresh_replayed_draft(
            &pending,
            &serde_json::json!({"features":[{"name":"Gain","value":1.0}]}),
        );
        assert_eq!(app.camera_drafts["sim:1"].0["Gain"], "9");
        assert_eq!(app.camera_drafts["sim:1"].1["Gain"], "1");
    }

    fn batch_app() -> Workbench {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "batch-history-test".into(),
            true,
            None,
        );
        let changes = ["sim:0", "sim:1"]
            .into_iter()
            .map(|camera| Change {
                camera: camera.into(),
                token: format!("worker-{camera}"),
                label: "Enable auto mode".into(),
                job: Job::AutoOn,
                changed_at_us: 0,
            })
            .collect::<Vec<_>>();
        app.record_camera_batch_at(changes, app.now);
        app
    }

    #[test]
    fn composite_batch_moves_history_only_when_all_camera_replies_succeed() {
        let mut app = batch_app();
        let _ = app.replay_history(true);
        let first = app.pending.remove(0);
        assert_eq!(first.camera.as_deref(), Some("sim:1"));
        app.camera_history_result(&first, &Ok(serde_json::json!({"features":[]})));
        assert!(app.history.undo().is_some());
        assert!(app.history.redo().is_none());
        let second = app.pending.remove(0);
        assert_eq!(second.camera.as_deref(), Some("sim:0"));
        app.camera_history_result(&second, &Ok(serde_json::json!({"features":[]})));
        assert!(app.history.undo().is_none());
        assert!(app.history.redo().is_some());
        assert!(app.history_replay.is_none());
        assert!(app.camera_history_state.replay.is_none());
    }

    #[test]
    fn composite_failure_rolls_back_completed_cameras_and_keeps_history_available() {
        let mut app = batch_app();
        let _ = app.replay_history(true);
        let first = app.pending.remove(0);
        app.camera_history_result(&first, &Ok(serde_json::json!({"features":[]})));
        let failed = app.pending.remove(0);
        app.camera_history_result(&failed, &Err(anyhow::anyhow!("auto mode conflict")));
        let rollback = app.pending.remove(0);
        assert_eq!(rollback.camera.as_deref(), Some("sim:1"));
        assert_eq!(rollback.history_replay, Some(false));
        assert!(app.history.undo().is_some());
        app.camera_history_result(&rollback, &Ok(serde_json::json!({"features":[]})));
        assert!(app.history.undo().is_some());
        assert!(app.history.redo().is_none());
        assert!(app.history_replay.is_none());
        assert!(app.camera_history_state.replay.is_none());
    }

    #[test]
    fn batch_records_only_successful_original_commands_as_one_gesture() {
        let mut app = Workbench::new(
            SessionHandle::new(),
            "batch-history-test".into(),
            true,
            None,
        );
        let pending = |camera: &str| Pending {
            camera: Some(camera.into()),
            batch: true,
            at: app.now,
            ..Pending::unanswered(Job::AutoOn)
        };
        let first = pending("sim:0");
        let second = pending("sim:1");
        app.pending.push(second);
        let result = |camera: &str| {
            Ok(serde_json::json!({"camera_change":{
                "camera":camera,"token":format!("worker-{camera}"),"label":"Enable auto mode"
            }}))
        };
        app.camera_history_result(&first, &result("sim:0"));
        assert!(app.history.undo().is_none());
        let second = app.pending.remove(0);
        app.camera_history_result(&second, &result("sim:1"));
        assert!(app.history.undo().is_some());
        assert!(app.camera_history_state.batches.is_empty());
        app.history.committed(true);
        assert!(app.history.undo().is_none());
    }
}
