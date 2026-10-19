//! Reflect work from CLI and API clients without treating it as a UI edit.
use super::*;
use crate::session::{ActivityJob, ActivityOrigin, ActivityState, SessionActivity};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct State {
    seen: HashMap<u64, ActivityState>,
    seen_jobs: HashMap<u64, ActivityJob>,
    job_frames: HashMap<(String, u32, u64), u32>,
    row_ids: RefCell<HashMap<u64, accessibility::NodeId>>,
    time_ids: RefCell<HashMap<u64, accessibility::NodeId>>,
    date_ids: RefCell<HashMap<u64, accessibility::NodeId>>,
    pub(super) raw_time_ids: RefCell<HashMap<time_format::RawKey, accessibility::NodeId>>,
    pub(super) raw_date_ids: RefCell<HashMap<time_format::RawKey, accessibility::NodeId>>,
    pub(super) unread: bool,
    pub(super) latest_changed: Option<u64>,
}
impl State {
    pub(super) fn from_snapshot(snapshot: &SessionSnapshot) -> Self {
        Self {
            seen: snapshot
                .activity
                .iter()
                .map(|entry| (entry.id, entry.state))
                .collect(),
            seen_jobs: snapshot
                .activity
                .iter()
                .filter_map(|entry| entry.job.as_ref().map(|job| (entry.id, job.clone())))
                .collect(),
            job_frames: snapshot
                .cameras
                .iter()
                .flat_map(|camera| {
                    camera.jobs.iter().map(|job| {
                        (
                            (camera.info.id.clone(), camera.worker_pid, job.id),
                            job.captured,
                        )
                    })
                })
                .collect(),
            row_ids: RefCell::default(),
            time_ids: RefCell::default(),
            date_ids: RefCell::default(),
            raw_time_ids: RefCell::default(),
            raw_date_ids: RefCell::default(),
            unread: false,
            latest_changed: None,
        }
    }
}

impl Workbench {
    /// Command IDs retain row identity as clients submit, finish, or reorder
    /// work. Each row owns its timestamp, attribution, outcome, and detail.
    pub(super) fn client_activity_rows(&self, p: &'static Palette) -> Element<'_, Message> {
        let current: HashSet<_> = self
            .snapshot
            .activity
            .iter()
            .map(|entry| entry.id)
            .collect();
        let mut row_ids = self.client_activity.row_ids.borrow_mut();
        row_ids.retain(|id, _| current.contains(id));
        let mut time_ids = self.client_activity.time_ids.borrow_mut();
        time_ids.retain(|id, _| current.contains(id));
        let mut date_ids = self.client_activity.date_ids.borrow_mut();
        date_ids.retain(|id, _| current.contains(id));
        let mut previous_day = None;
        iced::widget::keyed_column(self.snapshot.activity.iter().map(|entry| {
            let row_id = *row_ids
                .entry(entry.id)
                .or_insert_with(accessibility::new_node_id);
            let time_id = *time_ids
                .entry(entry.id)
                .or_insert_with(accessibility::new_node_id);
            let date_id = *date_ids
                .entry(entry.id)
                .or_insert_with(accessibility::new_node_id);
            let labels = time_format::activity(entry);
            let day = labels.day.clone();
            let boundary = previous_day != Some(day.key);
            previous_day = Some(day.key);
            let (mark, ink): (Element<'_, Message>, _) = match effective_state(entry) {
                ActivityState::Failed => (Level::Error.mark(11.0, p), Level::Error.color(p)),
                ActivityState::Succeeded => (Level::Done.mark(11.0, p), p.text),
                ActivityState::Running => (
                    icon::spinner(11.0, p.accent_text, self.spin()),
                    p.accent_text,
                ),
                ActivityState::Queued => (dot(p.secondary, 5.0), p.secondary),
            };
            // The row reads plainly; who exactly, and the raw camera ID, are
            // in the status mark's tooltip and the copied log.
            let (line, full) = self.activity_texts(entry);
            let mut body = iced::widget::column![
                text(line)
                    .size(style::CAPTION)
                    .color(ink)
                    .width(Fill)
                    .wrapping(text::Wrapping::WordOrGlyph)
            ]
            .spacing(2)
            .width(Fill);
            if let Some(detail) = activity_detail(entry) {
                body = body.push(
                    text(detail)
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(p.secondary)
                        .width(Fill)
                        .wrapping(text::Wrapping::WordOrGlyph),
                );
            }
            (
                entry.id,
                time_format::dated_row(
                    accessibility::group_with_id(
                        row![
                            time_format::view_with_id(labels, p, Some(time_id)),
                            super::tip(
                                container(mark)
                                    .width(11)
                                    .height(style::line_height(style::CAPTION))
                                    .align_y(Alignment::Center),
                                full,
                            ),
                            body,
                        ]
                        .spacing(6)
                        .width(Fill)
                        .into(),
                        format!("Command {}", entry.id),
                        row_id,
                    ),
                    boundary.then_some(day),
                    "Command updates",
                    date_id,
                    p,
                ),
            )
        }))
        .spacing(3)
        .width(Fill)
        .into()
    }

    /// Project this window's focus from the shared cameras. Focus is local;
    /// settings, jobs, and activity continue to be shared with every client.
    pub(super) fn project_local_camera(&mut self) {
        let available = |id: &str| {
            self.snapshot
                .cameras
                .iter()
                .any(|camera| camera.info.id == id)
        };
        let camera = self
            .local_camera
            .as_deref()
            .filter(|id| available(id))
            .or_else(|| {
                self.snapshot
                    .active_camera
                    .as_deref()
                    .filter(|id| available(id))
            })
            .or_else(|| {
                self.snapshot
                    .cameras
                    .first()
                    .map(|camera| camera.info.id.as_str())
            })
            .map(str::to_owned);
        self.local_camera = camera.clone();
        self.snapshot.select_view(camera.as_deref());
    }

    pub(super) fn selected_frame(&self) -> Option<Arc<crate::types::Frame>> {
        self.handle
            .latest_frame_for(self.snapshot.active_camera.as_deref()?)
    }

    /// Preserve each camera's human drafts. Only untouched fields adopt
    /// values written by another participant.
    pub(super) fn sync_shared_editors(&mut self) {
        let camera_id = self
            .snapshot
            .connected
            .as_ref()
            .map(|camera| camera.id.clone());
        if camera_id != self.observed_camera {
            if let Some(previous) = &self.observed_camera {
                self.camera_views
                    .entry(previous.clone())
                    .or_default()
                    .focus_open = Some(self.focus.open);
                self.camera_drafts.insert(
                    previous.clone(),
                    (self.edits.clone(), self.edit_sources.clone()),
                );
            }
            self.observed_camera = camera_id;
            let (edits, sources) = self
                .observed_camera
                .as_ref()
                .and_then(|id| self.camera_drafts.remove(id))
                .unwrap_or_default();
            self.edits = edits;
            self.edit_sources = sources;
            self.inspect.write_errors.clear();
            self.frame_id = None;
            self.shown = None;
            self.frame_meta = None;
            self.frame_source = None;
            self.display_error = None;
            self.exposure = None;
            self.focus.reset();
            let kind = self.camera_kind();
            if let Some(id) = &self.observed_camera {
                let state = self.camera_views.entry(id.clone()).or_default();
                self.focus.open = state
                    .focus_open
                    .unwrap_or(kind == CameraKind::Microscope || self.focus.open);
                state.focus_open = Some(self.focus.open);
            }
            self.gpu.retire(MAIN_VIEW);
        }
        for feature in &self.snapshot.features {
            let current = feature_value(feature);
            if let Some(source) = self.edit_sources.get(&feature.name)
                && source != &current
                && self.edits.get(&feature.name) == Some(source)
            {
                self.edits.insert(feature.name.clone(), current.clone());
            }
            self.edit_sources.insert(feature.name.clone(), current);
        }
    }

    pub(super) fn client_pending(&self, job: &Job, camera: Option<&str>) -> bool {
        self.snapshot.activity.iter().any(|entry| {
            self.other_activity(entry)
                && entry.state.active()
                && camera.is_none_or(|camera| entry.camera.as_deref() == Some(camera))
                && matches_job(entry, job)
        })
    }

    /// Process transitions by command ID, rather than a high-water mark:
    /// commands on separate cameras may finish in the opposite order.
    pub(super) fn sync_client_activity(&mut self) {
        let mut completed = Vec::new();
        let mut new_work = false;
        let mut kept = HashSet::new();
        for entry in &self.snapshot.activity {
            kept.insert(entry.id);
            let previous = self.client_activity.seen.insert(entry.id, entry.state);
            let previous_job = entry
                .job
                .as_ref()
                .and_then(|job| self.client_activity.seen_jobs.insert(entry.id, job.clone()));
            if !self.other_activity(entry) {
                continue;
            }
            let changed_job = entry
                .job
                .as_ref()
                .is_some_and(|job| previous_job.as_ref() != Some(job));
            if previous != Some(entry.state) || changed_job {
                self.client_activity.latest_changed = Some(entry.id);
            }
            new_work |= previous.is_none() || changed_job;
            let finished_job = changed_job && entry.job.as_ref().is_some_and(|job| !job.active());
            if !entry.state.active() && previous != Some(entry.state) || finished_job {
                completed.push(entry.clone());
            }
        }
        self.client_activity.seen.retain(|id, _| kept.contains(id));
        self.client_activity
            .seen_jobs
            .retain(|id, _| kept.contains(id));
        self.client_activity
            .row_ids
            .get_mut()
            .retain(|id, _| kept.contains(id));
        self.client_activity
            .time_ids
            .get_mut()
            .retain(|id, _| kept.contains(id));
        self.client_activity
            .date_ids
            .get_mut()
            .retain(|id, _| kept.contains(id));
        let raw: HashSet<_> = time_format::log_keys(&self.snapshot.logs)
            .into_iter()
            .collect();
        self.client_activity
            .raw_time_ids
            .get_mut()
            .retain(|key, _| raw.contains(key));
        self.client_activity
            .raw_date_ids
            .get_mut()
            .retain(|key, _| raw.contains(key));
        if new_work {
            // Keep background work visible without moving a human's focus or
            // opening a panel while they are editing camera settings.
            self.client_activity.unread = true;
        }
        if self.logs_open && !self.image_mode {
            self.client_activity.unread = false;
        }
        completed.sort_by_key(|entry| (entry.finished_at_ms, entry.id));
        for entry in completed {
            self.client_completed(&entry);
        }
        self.sync_scheduled_captures();
    }

    fn client_completed(&mut self, entry: &SessionActivity) {
        let job = match entry.operation.as_str() {
            "capture" => Some(Job::Capture),
            "set" => entry.feature.clone().map(Job::Set),
            "execute" => entry.feature.clone().map(Job::Execute),
            _ => None,
        };
        if let Some(job) = job {
            let pending = Pending {
                camera: entry.camera.clone(),
                ..Pending::unanswered(job)
            };
            let result = if entry.state == ActivityState::Succeeded {
                Ok(serde_json::json!({"files": entry.files}))
            } else {
                Err(anyhow::anyhow!(
                    entry
                        .detail
                        .clone()
                        .unwrap_or_else(|| "command failed".into())
                ))
            };
            match &result {
                Ok(value) => self.finished(&pending, value),
                Err(error) => self.failed(&pending, error),
            }
            self.result_inspector(&pending, &result);
            self.result_stage(&pending, &result);
            if entry.operation == "capture"
                && entry.state == ActivityState::Succeeded
                && let Some(saved) = &mut self.last_saved
            {
                saved.count = entry.file_count;
            }
        }
        let level = if effective_state(entry) == ActivityState::Failed {
            Level::Error
        } else {
            Level::Done
        };
        let target = entry
            .camera
            .as_ref()
            .map(|camera| format!(" · {camera}"))
            .unwrap_or_default();
        let text = format!(
            "{}{} · {}: {}",
            actor_display(entry),
            target,
            activity_state(entry),
            entry.label
        );
        // The toolbar may clip a long attribution. Its tooltip retains the
        // actor and progress alongside the outcome; announcements include the
        // same summary once, through Notice::announcement's shared handling.
        let attribution = entry
            .actor
            .as_ref()
            .map(|actor| format!("Participant: {} · {}", actor_kind(actor.kind), actor.id));
        let detail = attribution
            .iter()
            .chain(entry.detail.iter())
            .cloned()
            .collect::<Vec<_>>();
        let detail = (!detail.is_empty()).then(|| format!("{text}\n{}", detail.join("\n")));
        self.set_notice(text, detail, level);
    }

    fn sync_scheduled_captures(&mut self) {
        let mut saved = None;
        let mut kept = HashSet::new();
        for camera in &self.snapshot.cameras {
            for job in &camera.jobs {
                let key = (camera.info.id.clone(), camera.worker_pid, job.id);
                kept.insert(key.clone());
                let previous = self
                    .client_activity
                    .job_frames
                    .insert(key, job.captured)
                    .unwrap_or(0);
                if job.captured > previous
                    && let Some(path) = &job.last_file
                {
                    saved = Some(Saved {
                        camera: Some(camera.info.id.clone()),
                        path: path.clone(),
                        count: (job.captured - previous) as usize,
                        at: self.now,
                    });
                }
            }
        }
        self.client_activity
            .job_frames
            .retain(|key, _| kept.contains(key));
        if let Some(saved) = saved {
            self.shutter.replay(1.0, 0.0, self.now);
            self.last_saved = Some(saved);
        }
    }

    pub(super) fn activity_text(&self) -> String {
        let commands = self.snapshot.activity.iter().map(|entry| {
            let target = entry
                .camera
                .as_ref()
                .map(|camera| format!(" [{camera}]"))
                .unwrap_or_default();
            let detail = entry
                .detail
                .as_ref()
                .map(|detail| format!("\n{detail}"))
                .unwrap_or_default();
            format!(
                "{} [{} / {}]{} {}{}",
                time_format::activity(entry).copy,
                actor_text(entry),
                activity_state(entry),
                target,
                entry.label,
                detail
            )
        });
        self.snapshot
            .logs
            .iter()
            .map(|entry| {
                format!(
                    "{} [{}] {}",
                    time_format::log(entry).copy,
                    entry.level,
                    entry.message
                )
            })
            .chain(commands)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `activity_line` for `entry`, naming its camera as the session does.
    pub(super) fn activity_texts(&self, entry: &SessionActivity) -> (String, String) {
        let name = entry.camera.as_ref().and_then(|id| {
            self.snapshot
                .cameras
                .iter()
                .find(|camera| &camera.info.id == id)
                .map(|camera| {
                    format!(
                        "{} {}",
                        camera.info.model,
                        super::format::identity(&camera.info)
                    )
                })
        });
        activity_line(entry, self.other_activity(entry), name.as_deref())
    }

    pub(super) fn other_activity(&self, entry: &SessionActivity) -> bool {
        entry
            .actor
            .as_ref()
            .map_or(entry.origin == ActivityOrigin::Client, |actor| {
                actor.id != self.actor.id && !self.assistant_actor_is_local(&actor.id)
            })
    }
}

/// A command as its activity row reads, and in full. The row says what was
/// done, then to which camera (by `camera_name` when the session knows it),
/// then how it went unless it simply succeeded. Who did it leads only when it
/// was someone else (`other`), shortened as in the toolbar, or an AI agent.
/// The full text, for the tooltip and the row's accessible name, keeps the
/// whole actor identity, raw camera ID, state and command label.
pub(super) fn activity_line(
    entry: &SessionActivity,
    other: bool,
    camera_name: Option<&str>,
) -> (String, String) {
    let mut line = entry.label.clone();
    if let Some(camera) = &entry.camera {
        let shown = camera_name.unwrap_or(camera);
        let tokens: Vec<_> = entry.label.split_whitespace().collect();
        if tokens.last() == Some(&camera.as_str()) {
            // "Connect sim:0" reads like every other row: "Connect · sim:0".
            let head =
                entry.label.trim_end()[..entry.label.trim_end().len() - camera.len()].trim_end();
            line = format!("{head} · {shown}");
        } else if !tokens.contains(&camera.as_str()) {
            line.push_str(&format!(" · {shown}"));
        }
    }
    if effective_state(entry) != ActivityState::Succeeded || entry.job.is_some() {
        line.push_str(&format!(" · {}", activity_state(entry)));
    }
    let agent = entry
        .actor
        .as_ref()
        .is_some_and(|actor| actor.kind == crate::session::ActorKind::Agent);
    if agent {
        // An agent's whole ID: the row wraps rather than leave two agents
        // looking alike.
        let actor = entry.actor.as_ref().expect("agent");
        line = format!("AI agent {} [{}] · {line}", actor.name, actor.id);
    } else if other {
        line = format!("{} · {line}", actor_display(entry));
    }
    let target = entry
        .camera
        .as_ref()
        .map(|camera| format!(" · {camera}"))
        .unwrap_or_default();
    let full = format!(
        "{} · {}{target} · {}",
        actor_text(entry),
        activity_state(entry),
        entry.label
    );
    (line, full)
}

/// Expanded activity and accessible status retain full participant identity.
pub(super) fn actor_text(entry: &SessionActivity) -> String {
    entry.actor.as_ref().map_or_else(
        || entry.origin.label().into(),
        |actor| format!("{} [{}] · {}", actor.name, actor.id, actor_kind(actor.kind)),
    )
}

fn actor_kind(kind: crate::session::ActorKind) -> &'static str {
    match kind {
        crate::session::ActorKind::Human => "Human",
        crate::session::ActorKind::Agent => "AI agent",
    }
}

/// The command label already names some one-file outputs. Keep an additional
/// path when resolution changed it, or the detail carries any other outcome.
pub(super) fn activity_detail(entry: &SessionActivity) -> Option<&str> {
    let detail = entry.detail.as_deref()?;
    if entry.operation == "capture"
        && entry.file_count == 1
        && entry.files.len() == 1
        && detail == entry.files[0]
        && entry.label.ends_with(detail)
    {
        None
    } else {
        Some(detail)
    }
}

pub(super) fn actor_display(entry: &SessionActivity) -> String {
    entry.actor.as_ref().map_or_else(
        || entry.origin.label().into(),
        |actor| {
            let name = if actor.name.chars().count() > 24 {
                format!("{}…", actor.name.chars().take(24).collect::<String>())
            } else {
                actor.name.clone()
            };
            let id = if actor.id.len() > 24 {
                format!("…{}", &actor.id[actor.id.len() - 8..])
            } else {
                actor.id.clone()
            };
            format!("{name} [{id}]")
        },
    )
}

pub(super) fn active(entry: &SessionActivity) -> bool {
    entry.state.active() || entry.job.as_ref().is_some_and(ActivityJob::active)
}

pub(super) fn effective_state(entry: &SessionActivity) -> ActivityState {
    entry
        .job
        .as_ref()
        .map_or(entry.state, |job| match job.status.as_str() {
            "pending" => ActivityState::Queued,
            "running" => ActivityState::Running,
            "failed" => ActivityState::Failed,
            _ => ActivityState::Succeeded,
        })
}

pub(super) fn activity_state(entry: &SessionActivity) -> String {
    entry.job.as_ref().map_or_else(
        || entry.state.label().into(),
        |job| {
            format!(
                "{} · {}/{} frames",
                capitalize(&job.status),
                job.captured,
                job.count
            )
        },
    )
}

#[cfg(test)]
pub(super) fn activity_time(entry: &SessionActivity) -> String {
    time_format::activity(entry).compact
}

fn matches_job(entry: &SessionActivity, job: &Job) -> bool {
    match job {
        Job::Discover => entry.operation == "discover",
        Job::NetworkScan => entry.operation == "network_scan",
        Job::Connect => entry.operation == "connect",
        Job::Disconnect => entry.operation == "disconnect",
        Job::Select => entry.operation == "select",
        Job::Start => {
            entry.operation == "start"
                || entry.operation == "execute"
                    && entry.feature.as_deref() == Some("AcquisitionStart")
        }
        Job::Stop => {
            entry.operation == "stop"
                || entry.operation == "execute"
                    && entry.feature.as_deref() == Some("AcquisitionStop")
        }
        Job::Capture => entry.operation == "capture",
        Job::CaptureSessionOpen => entry.operation == "capture_session_open",
        Job::CaptureSessionClose => entry.operation == "capture_session_close",
        Job::AutoCapture => entry.operation == "auto_capture",
        Job::CaptureExample => entry.operation == "capture_example",
        Job::CaptureReview => entry.operation == "capture_review",
        Job::ReviewCapture => entry.operation == "review_capture",
        Job::AutoOn | Job::Balance => entry.operation == "auto",
        Job::AutoOff => entry.operation == "manual",
        Job::RefreshFeatures => entry.operation == "features",
        Job::Schedule => entry.operation == "schedule",
        Job::RefreshJobs => entry.operation == "jobs",
        Job::Cancel => entry.operation == "cancel_job",
        Job::Forward { .. } => entry.operation == "forward",
        Job::StopForward { .. } => entry.operation == "stop_forward",
        Job::DatasetOpen => entry.operation == "dataset_open",
        Job::DatasetClose => entry.operation == "dataset_close",
        Job::DatasetNote => entry.operation == "dataset_note",
        Job::DatasetExport => entry.operation == "dataset_export",
        Job::SignalStart => entry.operation == "signal_start",
        Job::SignalStop => entry.operation == "signal_stop",
        Job::Set(feature) => entry.operation == "set" && entry.feature.as_ref() == Some(feature),
        Job::SetMany => entry.operation == "set_many",
        Job::Execute(feature) => {
            entry.operation == "execute" && entry.feature.as_ref() == Some(feature)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn select(bench: &mut Workbench, camera: &str, width: u64) {
        bench.snapshot.connected = Some(crate::transport::simulator::info_named(camera));
        bench.snapshot.active_camera = Some(camera.into());
        bench.snapshot.features = vec![
            serde_json::from_value(json!({
                "name": "Width", "display_name": "Width", "kind": "Integer",
                "value": width, "writable": true, "description": "", "choices": []
            }))
            .unwrap(),
        ];
        bench.sync_shared_editors();
    }
    #[test]
    fn client_changes_refresh_pristine_values_and_preserve_drafts_across_selection() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        select(&mut bench, "sim:0", 50);
        bench.edits.insert("Width".into(), "50".into());
        select(&mut bench, "sim:0", 60);
        assert_eq!(
            bench.edits["Width"], "60",
            "untouched field adopts the client value"
        );
        bench.edits.insert("Width".into(), "75".into());
        select(&mut bench, "sim:1", 100);
        assert!(bench.edits.is_empty());
        bench.edits.insert("Width".into(), "150".into());
        select(&mut bench, "sim:0", 80);
        assert_eq!(
            bench.edits["Width"], "75",
            "sim:0 human draft survives external selection and write"
        );
        assert_eq!(bench.edit_sources["Width"], "80");
        select(&mut bench, "sim:1", 110);
        assert_eq!(bench.edits["Width"], "150", "sim:1 keeps its own draft");
    }
    fn entry(id: u64, state: ActivityState) -> SessionActivity {
        SessionActivity {
            id,
            origin: ActivityOrigin::Client,
            actor: Some(
                crate::session::SessionActor::new(
                    "agent-a",
                    "Agent A",
                    crate::session::ActorKind::Agent,
                )
                .unwrap(),
            ),
            operation: "capture".into(),
            camera: Some("sim:0".into()),
            feature: None,
            label: "Capture 1 png frame(s) → frame.png".into(),
            state,
            submitted_at_ms: 1,
            started_at_ms: Some(1),
            finished_at_ms: (!state.active()).then_some(id),
            detail: Some("frame.png".into()),
            file_count: 1,
            files: vec![format!("frame-{id}.png")],
            job: None,
        }
    }
    #[test]
    fn activity_rows_read_plainly_and_name_who_only_when_it_matters() {
        let human = |id: &str, name: &str| {
            Some(
                crate::session::SessionActor::new(id, name, crate::session::ActorKind::Human)
                    .unwrap(),
            )
        };
        let line = |entry: &SessionActivity, other: bool| activity_line(entry, other, None).0;
        let mut connect = entry(1, ActivityState::Succeeded);
        connect.actor = human("desktop:1", "Desktop 1");
        connect.operation = "connect".into();
        connect.label = "Connect sim:0".into();
        connect.detail = None;
        // Our own success: what was done and to which camera, in the one form
        // every row uses.
        assert_eq!(line(&connect, false), "Connect · sim:0");
        // A known camera reads by name; the raw ID stays in the full text.
        let (named, full) = activity_line(&connect, false, Some("Pattern camera S/N SIM0"));
        assert_eq!(named, "Connect · Pattern camera S/N SIM0");
        assert_eq!(
            full,
            "Desktop 1 [desktop:1] · Human · Done · sim:0 · Connect sim:0"
        );
        // Anything but a plain success says how it went.
        connect.state = ActivityState::Failed;
        assert_eq!(line(&connect, false), "Connect · sim:0 · Failed");
        connect.state = ActivityState::Succeeded;
        // Someone else's says who, shortened as in the toolbar.
        connect.actor = human("desktop:7200", "Desktop 7200");
        assert_eq!(
            line(&connect, true),
            "Desktop 7200 [desktop:7200] · Connect · sim:0"
        );
        // A camera the label leaves out is added.
        connect.label = "Start acquisition".into();
        assert_eq!(line(&connect, false), "Start acquisition · sim:0");
        // Only a whole word names the camera: sim:1 is not in "sim:10".
        connect.camera = Some("sim:1".into());
        connect.label = "Connect sim:10".into();
        assert_eq!(line(&connect, false), "Connect sim:10 · sim:1");
        // Agents always say so, with their whole ID.
        let mut agent = entry(2, ActivityState::Running);
        let id = format!("cli-agent-{}", "b".repeat(64));
        agent.actor = Some(
            crate::session::SessionActor::new(
                id.clone(),
                "Workflow agent",
                crate::session::ActorKind::Agent,
            )
            .unwrap(),
        );
        let (row, full) = activity_line(&agent, false, None);
        assert_eq!(
            row,
            format!(
                "AI agent Workflow agent [{id}] · Capture 1 png frame(s) → frame.png · sim:0 · Running"
            )
        );
        assert!(full.starts_with(&format!("Workflow agent [{id}] · AI agent · Running")));
        // A job reports its progress even once it is done.
        agent.job = Some(ActivityJob {
            id: 1,
            worker_pid: 0,
            status: "done".into(),
            count: 3,
            captured: 3,
            last_file: None,
            error: None,
        });
        assert!(line(&agent, false).ends_with(" · Done · 3/3 frames"));
    }

    #[test]
    fn compact_actor_labels_leave_full_identity_in_expanded_activity() {
        let mut command = entry(1, ActivityState::Succeeded);
        let id = format!("cli-agent-{}", "a".repeat(64));
        command.actor = Some(
            crate::session::SessionActor::new(
                id.clone(),
                "Workflow agent",
                crate::session::ActorKind::Agent,
            )
            .unwrap(),
        );
        assert_eq!(
            actor_text(&command),
            format!("Workflow agent [{id}] · AI agent")
        );
        assert_eq!(actor_display(&command), "Workflow agent […aaaaaaaa]");
        let machine = serde_json::to_value(&command).unwrap();
        assert_eq!(machine["actor"]["id"], id);
        assert_eq!(machine["actor"]["kind"], "agent");
    }

    #[test]
    fn actual_activity_rows_own_details_once_and_follow_command_ids_across_reordering() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "activity-tree".into(), true, None);
        let mut first = entry(1, ActivityState::Running);
        first.submitted_at_ms = 1_000;
        first.actor = Some(
            crate::session::SessionActor::new(
                "cli-agent-with-a-long-distinct-identity",
                "Workflow agent",
                crate::session::ActorKind::Agent,
            )
            .unwrap(),
        );
        first.detail = Some("Waiting for the acquisition trigger".into());
        let mut second = entry(2, ActivityState::Succeeded);
        second.finished_at_ms = Some(2_000);
        let mut third = entry(3, ActivityState::Queued);
        third.submitted_at_ms = 3_000;
        let mut tree = Tree::empty();
        let mut ids = HashMap::new();
        let mut text_ids = HashMap::new();
        for stage in 0..3 {
            bench.snapshot.activity = match stage {
                0 => vec![first.clone(), second.clone()],
                1 => {
                    first.state = ActivityState::Failed;
                    first.detail = Some("Acquisition failed\n/isolated/capture.png".into());
                    vec![third.clone(), second.clone(), first.clone()]
                }
                _ => vec![first.clone(), second.clone()],
            };
            let mut view = bench.client_activity_rows(Palette::of(false));
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(1000.0, 700.0)),
            );
            let (snapshot, bridge, window) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                Layout::new(&node),
                &renderer,
            );
            let nodes: HashMap<_, _> = snapshot
                .nodes
                .iter()
                .map(|(id, node)| (*id, node))
                .collect();
            let root = snapshot
                .nodes
                .iter()
                .find(|(_, node)| node.role() == accessibility::Role::Window)
                .unwrap()
                .1
                .clone();
            let reading_order: Vec<_> = root
                .children()
                .iter()
                .filter(|id| nodes[id].role() == accessibility::Role::Group)
                .map(|id| nodes[id].label().unwrap())
                .collect();
            let expected: Vec<_> = bench
                .snapshot
                .activity
                .iter()
                .map(|entry| format!("Command {}", entry.id))
                .collect();
            assert_eq!(reading_order, expected);
            for entry in &bench.snapshot.activity {
                let name = format!("Command {}", entry.id);
                let (id, group) = snapshot
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(name.as_str()))
                    .unwrap();
                assert_eq!(group.role(), accessibility::Role::Group);
                assert!(!group.supports_action(accesskit::Action::Click));
                assert!(!group.supports_action(accesskit::Action::Focus));
                assert!(
                    group.live().is_none(),
                    "the expanded history does not repeat toolbar announcements"
                );
                if let Some(previous) = ids.insert(entry.id, *id) {
                    assert_eq!(
                        *id, previous,
                        "retained command IDs retain the same native row after state changes and reordering"
                    );
                }
                let children: Vec<_> = group
                    .children()
                    .iter()
                    .map(|id| nodes[id].label().unwrap())
                    .collect();
                assert_eq!(children[0], activity_time(entry));
                assert_eq!(children[1], bench.activity_texts(entry).0);
                assert_eq!(children.get(2).copied(), activity_detail(entry));
                for child in group.children() {
                    assert_eq!(
                        snapshot
                            .nodes
                            .iter()
                            .filter(|(_, node)| node.children().contains(child))
                            .count(),
                        1
                    );
                    let key = (entry.id, nodes[child].label().unwrap().to_owned());
                    if let Some(previous) = text_ids.insert(key, *child) {
                        assert_eq!(
                            *child, previous,
                            "unchanged row text retains its native ID after reordering"
                        );
                    }
                }
                for action in [accesskit::Action::Focus, accesskit::Action::Click] {
                    assert!(
                        iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                            window,
                            replay: None,
                            action: accesskit::ActionRequest {
                                action,
                                target: *id,
                                data: None
                            },
                        }))
                        .is_none(),
                        "a journal group cannot acquire an action from a reused widget state"
                    );
                }
            }
            assert_eq!(
                bench.client_activity.row_ids.borrow().len(),
                bench.snapshot.activity.len()
            );
            if stage == 2 {
                let removed = ids[&third.id];
                assert!(!nodes.contains_key(&removed));
                assert!(
                    iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                        window,
                        replay: None,
                        action: accesskit::ActionRequest {
                            action: accesskit::Action::Focus,
                            target: removed,
                            data: None
                        },
                    }))
                    .is_none()
                );
            }
        }
    }

    #[test]
    fn native_activity_identity_cache_is_pruned_even_when_the_panel_is_closed() {
        let mut bench = Workbench::new(SessionHandle::new(), "activity-prune".into(), true, None);
        let mut first = entry(1, ActivityState::Running);
        first.actor = Some(bench.actor.clone());
        let mut second = entry(2, ActivityState::Running);
        second.actor = Some(bench.actor.clone());
        bench.snapshot.activity = vec![first, second.clone()];
        drop(bench.client_activity_rows(Palette::of(false)));
        let retained = bench.client_activity.row_ids.borrow()[&2];
        let retained_time = bench.client_activity.time_ids.borrow()[&2];
        let retained_date = bench.client_activity.date_ids.borrow()[&2];
        assert_eq!(bench.client_activity.row_ids.borrow().len(), 2);
        assert!(!bench.logs_open);
        bench.snapshot.activity = vec![second];
        bench.sync_client_activity();
        assert_eq!(
            *bench.client_activity.time_ids.borrow(),
            HashMap::from([(2, retained_time)])
        );
        assert_eq!(
            *bench.client_activity.row_ids.borrow(),
            HashMap::from([(2, retained)])
        );
        assert_eq!(
            *bench.client_activity.date_ids.borrow(),
            HashMap::from([(2, retained_date)])
        );
        bench.snapshot.activity.clear();
        bench.sync_client_activity();
        assert!(bench.client_activity.row_ids.borrow().is_empty());
        assert!(bench.client_activity.time_ids.borrow().is_empty());
        assert!(bench.client_activity.date_ids.borrow().is_empty());
    }

    #[test]
    fn grouped_client_activity_keeps_the_actual_panel_copy_action() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "activity-copy".into(), true, None);
        let mut command = entry(1, ActivityState::Failed);
        command.detail = Some("Output folder is read-only".into());
        bench.snapshot.activity = vec![command];
        let mut view = bench.activity(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1000.0, 180.0)),
        );
        let (snapshot, bridge, window) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        assert_eq!(
            snapshot
                .nodes
                .iter()
                .filter(|(_, node)| node.label() == Some("Output folder is read-only"))
                .count(),
            1
        );
        let (copy_id, copy) = snapshot
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Copy activity log"))
            .unwrap();
        assert!(copy.supports_action(accesskit::Action::Click));
        let mut actions =
            iced_runtime::task::into_stream(bridge.dispatch(accessibility::Request {
                window,
                replay: None,
                action: accesskit::ActionRequest {
                    action: accesskit::Action::Click,
                    target: *copy_id,
                    data: None,
                },
            }))
            .unwrap();
        assert!(matches!(
            iced::futures::executor::block_on(actions.next()),
            Some(iced_runtime::Action::Output(Message::CopyLog))
        ));
        assert!(iced::futures::executor::block_on(actions.next()).is_none());
    }

    #[test]
    fn activity_actor_outcomes_and_unbroken_paths_wrap_inside_the_remaining_row_width() {
        use iced::advanced::{Layout, layout, text::Paragraph, widget::Tree};
        type TextState = iced::widget::text::State<
            <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
        >;
        let mut renderer = super::super::destinations::layout_tests::renderer();
        let mut bench = Workbench::new(SessionHandle::new(), "activity-wrap".into(), true, None);
        let mut desktop = entry(1, ActivityState::Succeeded);
        desktop.actor = Some(
            crate::session::SessionActor::new(
                "desktop:27591",
                "Desktop 27591",
                crate::session::ActorKind::Human,
            )
            .unwrap(),
        );
        desktop.operation = "start".into();
        desktop.label = "Start acquisition".into();
        desktop.detail = None;
        let mut long = entry(2, ActivityState::Failed);
        long.actor = Some(
            crate::session::SessionActor::new(
                "a".repeat(128),
                "Workflow agent",
                crate::session::ActorKind::Agent,
            )
            .unwrap(),
        );
        long.label = "Capture failed: output folder is read-only".into();
        long.detail = Some(format!("/isolated/{}/capture.png", "unbroken".repeat(48)));
        bench.snapshot.activity = vec![desktop, long];
        let mut tree = Tree::empty();
        let mut group_ids = HashMap::new();
        for scale in [1.0, 1.15, 1.3] {
            for native_width in [480.0, 640.0, 900.0] {
                let size = Size::new(native_width / scale, 1100.0);
                let mut view = bench.client_activity_rows(Palette::of(false));
                tree.diff(view.as_widget());
                let node = view.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, size),
                );
                let (snapshot, _, _) = accessibility::snapshot_widget(
                    view.as_widget_mut(),
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                );
                for (index, entry) in bench.snapshot.activity.iter().enumerate() {
                    // The keyed row keeps date-caption and command slots;
                    // Group delegates its layout to the Row → body Column.
                    let row = &node.children()[index].children()[1];
                    let body = &row.children()[2];
                    let body_tree = &tree.children[index].children[1].children[0].children[2];
                    let body_left = row.bounds().x + body.bounds().x;
                    let room = size.width - body_left;
                    assert!(
                        body.bounds().width <= room + 0.01,
                        "the activity body exceeds the remaining row width at {native_width}/{scale}: {:?}, room {room}",
                        body.bounds()
                    );
                    for (text_node, text_tree) in body.children().iter().zip(&body_tree.children) {
                        let state = text_tree.state.downcast_ref::<TextState>();
                        let measured = state.raw().min_bounds();
                        assert!(
                            measured.width <= room + 0.01,
                            "rendered activity text overflows at {native_width}/{scale}: {measured:?}, room {room}"
                        );
                        assert!(
                            text_node.bounds().x + measured.width <= body.bounds().width + 0.01
                        );
                        assert!(
                            measured.height <= text_node.bounds().height + 0.01,
                            "the real paragraph must retain every wrapped line"
                        );
                    }
                    if index == 1 {
                        assert!(
                            body.children()[0].bounds().height
                                > style::line_height(style::CAPTION) * 1.5,
                            "the full unbroken agent ID retains its readable wrapped lines"
                        );
                        assert!(
                            body.children()[1].bounds().height
                                > style::line_height(style::CAPTION) * 2.0,
                            "the full unbroken output path needs several readable lines"
                        );
                    }
                    let name = format!("Command {}", entry.id);
                    let (id, group) = snapshot
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(name.as_str()))
                        .unwrap();
                    if let Some(previous) = group_ids.insert(entry.id, *id) {
                        assert_eq!(
                            *id, previous,
                            "wrapping and viewport changes retain the command's native identity"
                        );
                    }
                    let expected = bench.activity_texts(entry).0;
                    assert!(
                        group.children().iter().any(|id| snapshot
                            .nodes
                            .iter()
                            .any(|(node_id, node)| node_id == id
                                && node.label() == Some(expected.as_str()))),
                        "native text reads the plain line"
                    );
                    if let Some(detail) = activity_detail(entry) {
                        assert!(group.children().iter().any(|id| {
                            snapshot.nodes.iter().any(|(node_id, node)| {
                                node_id == id && node.label() == Some(detail)
                            })
                        }));
                    }
                }
                super::super::destinations::layout_tests::render_artifact_scaled(
                    &format!("activity-wrap-{native_width}-{scale}"),
                    &view,
                    &tree,
                    &node,
                    &mut renderer,
                    size,
                    scale,
                );
            }
        }
    }
    #[test]
    fn activity_avoids_duplicate_paths_and_preserves_resolved_or_multi_file_details() {
        let mut capture = entry(1, ActivityState::Succeeded);
        capture.files = vec!["frame.png".into()];
        assert!(activity_detail(&capture).is_none());
        capture.detail = Some("/resolved/frame.png".into());
        capture.files = vec!["/resolved/frame.png".into()];
        assert_eq!(activity_detail(&capture), Some("/resolved/frame.png"));
        capture.detail = Some("frame.png\nframe-2.png".into());
        capture.files = vec!["frame.png".into(), "frame-2.png".into()];
        capture.file_count = 2;
        assert_eq!(activity_detail(&capture), Some("frame.png\nframe-2.png"));
    }
    #[test]
    fn client_capture_progress_and_out_of_order_completion_reach_the_gui_once() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        bench.snapshot.activity = vec![
            entry(1, ActivityState::Running),
            entry(2, ActivityState::Succeeded),
        ];
        bench.sync_client_activity();
        assert!(!bench.logs_open);
        assert!(bench.client_activity.unread);
        assert!(bench.client_pending(&Job::Capture, Some("sim:0")));
        assert!(
            !bench.capturing_here(),
            "other participants do not disable this user's capture"
        );
        assert_eq!(bench.last_saved.as_ref().unwrap().path, "frame-2.png");
        bench.snapshot.activity[0] = entry(1, ActivityState::Succeeded);
        bench.sync_client_activity();
        assert!(!bench.pending(Job::Capture));
        assert_eq!(bench.last_saved.as_ref().unwrap().path, "frame-1.png");
        bench.last_saved = None;
        bench.logs_open = false;
        bench.sync_client_activity();
        assert!(bench.last_saved.is_none());
        assert!(!bench.logs_open);
        let activity = bench.activity_text();
        assert!(activity.contains("Agent A"));
        assert!(activity.contains("Done"));
        assert!(activity.contains("Capture 1 png frame(s)"));
    }
    #[test]
    fn gui_commands_are_not_announced_twice_and_client_failures_stay_visible() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let mut gui = entry(1, ActivityState::Succeeded);
        gui.origin = ActivityOrigin::Gui;
        gui.actor = Some(bench.actor.clone());
        bench.snapshot.activity = vec![gui];
        bench.sync_client_activity();
        assert!(bench.notice.is_none() && bench.last_saved.is_none());
        let mut failure = entry(2, ActivityState::Failed);
        failure.detail = Some("storage full".into());
        failure.files.clear();
        bench.snapshot.activity.push(failure);
        bench.sync_client_activity();
        assert_eq!(bench.notice.as_ref().unwrap().level, Level::Error);
        assert!(
            bench
                .notice
                .as_ref()
                .unwrap()
                .about()
                .contains("storage full")
        );
    }

    #[test]
    fn another_humans_gui_actions_are_visible_and_attributed() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let mut peer = entry(1, ActivityState::Succeeded);
        peer.origin = ActivityOrigin::Gui;
        peer.actor = Some(
            crate::session::SessionActor::new(
                "desktop-peer",
                "Morgan",
                crate::session::ActorKind::Human,
            )
            .unwrap(),
        );
        bench.snapshot.activity = vec![peer];
        bench.sync_client_activity();
        assert!(bench.client_activity.unread);
        assert!(bench.notice.as_ref().unwrap().text.contains("Morgan"));
        assert!(bench.activity_text().contains("Morgan"));
        assert_eq!(bench.last_saved.as_ref().unwrap().path, "frame-1.png");
    }

    #[test]
    fn shared_selection_cannot_move_local_editors_or_redirect_capture() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let first = super::super::liveness::streaming_camera(1, 0, 10.0);
        let mut second = first.clone();
        second.info.id = "sim:1".into();
        second.frames = 20;
        bench.snapshot.cameras = vec![first, second];
        bench.local_camera = Some("sim:0".into());
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.project_local_camera();
        assert_eq!(bench.snapshot.active_camera.as_deref(), Some("sim:0"));
        assert_eq!(bench.snapshot.connected.as_ref().unwrap().id, "sim:0");
        assert_eq!(bench.snapshot.frames, 1);
        bench.capture();
        assert_eq!(
            bench.pending.last().unwrap().camera.as_deref(),
            Some("sim:0")
        );
        let activity = bench.handle.snapshot().activity;
        assert_eq!(activity.last().unwrap().camera.as_deref(), Some("sim:0"));
        assert_eq!(
            activity.last().unwrap().actor.as_ref().unwrap(),
            &bench.actor
        );
    }

    #[test]
    fn local_focus_falls_back_after_disconnect_and_keeps_shared_activity() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let mut camera = super::super::liveness::streaming_camera(1, 0, 10.0);
        camera.info.id = "sim:1".into();
        bench.snapshot.cameras = vec![camera];
        bench.snapshot.activity = vec![entry(1, ActivityState::Running)];
        bench.local_camera = Some("sim:0".into());
        bench.project_local_camera();
        assert_eq!(bench.local_camera.as_deref(), Some("sim:1"));
        assert_eq!(bench.snapshot.activity.len(), 1);
        bench.snapshot.cameras.clear();
        bench.project_local_camera();
        assert!(bench.local_camera.is_none() && bench.snapshot.connected.is_none());
    }

    #[test]
    fn own_captures_limit_only_the_camera_they_were_sent_to() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.pending.push(Pending {
            camera: Some("sim:1".into()),
            ..Pending::unanswered(Job::Capture)
        });
        assert!(!bench.capturing_here());
        bench.snapshot.activity = vec![entry(1, ActivityState::Running)];
        assert!(
            !bench.capturing_here(),
            "same camera remains available while an agent captures"
        );
        bench.pending.push(Pending {
            camera: Some("sim:0".into()),
            ..Pending::unanswered(Job::Capture)
        });
        assert!(bench.capturing_here());
    }

    #[test]
    fn scheduled_work_stays_visible_after_acknowledgment_and_announces_failure() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let mut scheduled = entry(1, ActivityState::Succeeded);
        scheduled.operation = "schedule".into();
        scheduled.job = Some(ActivityJob {
            id: 2,
            worker_pid: 123,
            status: "running".into(),
            count: 10,
            captured: 2,
            last_file: Some("frame-2.png".into()),
            error: None,
        });
        bench.snapshot.activity = vec![scheduled];
        bench.sync_client_activity();
        assert!(active(&bench.snapshot.activity[0]));
        assert_eq!(
            effective_state(&bench.snapshot.activity[0]),
            ActivityState::Running
        );
        assert!(bench.activity_text().contains("Running · 2/10 frames"));
        bench.client_activity.unread = false;
        bench.snapshot.activity[0].job.as_mut().unwrap().captured = 3;
        bench.sync_client_activity();
        assert!(bench.client_activity.unread, "progress is shared work");
        let scheduled = &mut bench.snapshot.activity[0];
        scheduled.job.as_mut().unwrap().status = "failed".into();
        scheduled.job.as_mut().unwrap().error = Some("storage full".into());
        scheduled.detail = Some("storage full".into());
        bench.sync_client_activity();
        assert!(!active(&bench.snapshot.activity[0]));
        assert_eq!(bench.notice.as_ref().unwrap().level, Level::Error);
        assert!(bench.notice.as_ref().unwrap().text.contains("Agent A"));
        assert!(
            bench
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("Failed · 3/10 frames")
        );
        bench.notice = None;
        bench.sync_client_activity();
        assert!(
            bench.notice.is_none(),
            "each final transition is announced once"
        );
    }

    #[test]
    fn session_actions_wake_an_idle_native_subscription() {
        use iced::futures::{FutureExt, StreamExt};
        let handle = SessionHandle::new();
        let mut changes = Box::pin(watch_activity(&ActivityWatch(handle.clone())));
        assert!(changes.next().now_or_never().is_none());
        let reply = handle
            .submit_to_as(
                "sim:0",
                SessionCommand::Set {
                    feature: "Gain".into(),
                    value: "2".into(),
                },
                ActivityOrigin::Client,
                crate::session::SessionActor::new(
                    "agent-wakeup",
                    "Capture agent",
                    crate::session::ActorKind::Agent,
                )
                .unwrap(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let message = loop {
            if let Some(message) = changes.next().now_or_never() {
                break message;
            }
            assert!(
                Instant::now() < deadline,
                "native subscriber was not notified"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(message, Some(Message::Tick)));
        assert!(reply.recv_timeout(Duration::from_secs(1)).is_ok());
    }
}
