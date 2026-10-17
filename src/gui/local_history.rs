//! History contains only user edits, never live frames or session snapshots.
use super::*;

#[derive(Clone)]
pub(super) enum Edit {
    Local(Box<LocalChange>),
    Camera(super::camera_history::Change),
    CameraBatch(Vec<super::camera_history::Change>),
    WindowMode { before: bool, after: bool },
    Destination(bool, Box<destinations::Change>),
}
impl Edit {
    pub(super) fn camera_changes(&self) -> &[super::camera_history::Change] {
        match self {
            Self::Camera(change) => std::slice::from_ref(change),
            Self::CameraBatch(changes) => changes,
            _ => &[],
        }
    }
}

macro_rules! fields {
    ($($name:ident: $ty:ty),* $(,)?) => {
        #[derive(Clone, PartialEq)]
        pub(super) struct Values { $( $name: $ty, )* }
        impl Values {
            fn capture(app: &Workbench) -> Self { Self { $( $name: app.$name.clone(), )* } }
            fn restore(&self, other: &Self, app: &mut Workbench) {
                $( if self.$name != other.$name { app.$name = self.$name.clone(); } )*
            }
        }
    }
}
fields! {
    include_simulator: bool, address: String, search: String, tab: Tab,
    output: String, count: u32, format: &'static str, timeout_ms: u64, scan_timeout_ms: u64,
    forward_output: String, stream_draft: bool, stashed_record: Option<(String, Option<String>)>,
    stashed_stream: String, forward_codec: &'static str, forward_encoder: String,
    forward_fps: f64, forward_bitrate: String, forward_file_mib: f64,
    quota_gib: f64, quota_files: u32, retention_enabled: bool, retention_days: u64,
    quota_action: &'static str, schedule_enabled: bool, schedule_delay_seconds: u64,
    schedule_interval_seconds: f64, storage_open: bool, auto_changes_open: bool,
    appearance: Appearance, ui_scale: UiScale, logs_open: bool, fit: bool, zoom: f32, exposure_open: bool,
    focus_camera: bool, sidebar_open: bool, inspector_open: bool, inspector_peek: bool,
    image_mode: bool, recent: Vec<Recent>, auto_start: bool, tags: CaptureTags,
}

#[derive(Clone)]
pub(super) struct LocalState {
    values: Values,
    drafts: HashMap<Num, String>,
    edits: HashMap<String, String>,
    camera: Option<String>,
    capture_selected: Option<String>,
    record_selected: Option<String>,
    focus: scopes::FocusMeter,
}

impl LocalState {
    pub fn capture(app: &Workbench) -> Self {
        let mut edits = app.edit_sources.clone();
        edits.extend(app.edits.clone());
        Self {
            values: Values::capture(app),
            drafts: app.drafts.clone(),
            edits,
            camera: app.snapshot.active_camera.clone(),
            capture_selected: app.capture_to.selected.clone(),
            record_selected: app.record_to.selected.clone(),
            focus: app.focus.clone(),
        }
    }
    fn differs(&self, other: &Self) -> bool {
        self.values != other.values
            || self.drafts != other.drafts
            || self.edits != other.edits
            || self.capture_selected != other.capture_selected
            || self.record_selected != other.record_selected
            || (self.focus.open, self.focus.region, self.focus.metric)
                != (other.focus.open, other.focus.region, other.focus.metric)
    }
}

#[derive(Clone)]
pub(super) struct LocalChange {
    before: LocalState,
    after: LocalState,
    calibration: bool,
    focus_id: Option<String>,
}

fn patch_map<K: Clone + Eq + std::hash::Hash, V: Clone + PartialEq>(
    wanted: &HashMap<K, V>,
    previous: &HashMap<K, V>,
    target: &mut HashMap<K, V>,
) {
    for key in wanted.keys().chain(previous.keys()) {
        if wanted.get(key) != previous.get(key) {
            if let Some(value) = wanted.get(key) {
                target.insert(key.clone(), value.clone());
            } else {
                target.remove(key);
            }
        }
    }
}

impl LocalChange {
    fn restore(&self, app: &mut Workbench, undo: bool) -> Result<()> {
        let (wanted, previous) = if undo {
            (&self.before, &self.after)
        } else {
            (&self.after, &self.before)
        };
        // Validate persisted selection first; a failure must not partly restore other fields.
        if wanted.capture_selected != previous.capture_selected {
            app.capture_to
                .restore_selection(wanted.capture_selected.clone(), &mut app.output)
                .map_err(anyhow::Error::msg)?;
        }
        if wanted.record_selected != previous.record_selected {
            app.record_to
                .restore_selection(wanted.record_selected.clone(), &mut app.forward_output)
                .map_err(anyhow::Error::msg)?;
        }
        wanted.values.restore(&previous.values, app);
        if wanted.values.include_simulator != previous.values.include_simulator {
            app.discover();
        }
        patch_map(&wanted.drafts, &previous.drafts, &mut app.drafts);
        if wanted.edits != previous.edits {
            if app.snapshot.active_camera == wanted.camera {
                patch_map(&wanted.edits, &previous.edits, &mut app.edits);
                for feature in wanted.edits.keys() {
                    app.inspect.write_errors.remove(feature);
                }
            } else if let Some(camera) = &wanted.camera {
                let (edits, _) = app.camera_drafts.entry(camera.clone()).or_default();
                patch_map(&wanted.edits, &previous.edits, edits);
                if app
                    .snapshot
                    .cameras
                    .iter()
                    .any(|item| &item.info.id == camera)
                {
                    app.send(
                        Job::Select,
                        SessionCommand::Select {
                            camera: camera.clone(),
                        },
                    );
                }
            }
        }
        if wanted.focus.open != previous.focus.open {
            app.focus.open = wanted.focus.open;
            app.focus.reset();
        }
        if wanted.focus.metric != previous.focus.metric {
            app.focus.set_metric(wanted.focus.metric);
        }
        if wanted.focus.region != previous.focus.region {
            app.focus.region = wanted.focus.region;
            app.focus.reset();
            app.remeasure();
        }
        if (wanted.focus.open && wanted.focus.open != previous.focus.open)
            || (wanted.values.exposure_open
                && wanted.values.exposure_open != previous.values.exposure_open)
        {
            app.remeasure();
        }
        if self.calibration && wanted.camera == app.snapshot.active_camera {
            app.focus.restore_calibration(&wanted.focus);
        }
        Ok(())
    }
}

pub(super) fn gesture(message: &Message) -> Option<(String, Option<String>)> {
    let (label, key) = match message {
        Message::Menu(menus::Command::Action(_)) | Message::Key { .. } => {
            let action = match message {
                Message::Menu(menus::Command::Action(action)) => *action,
                Message::Key { chord, captured } => Action::find(chord, !captured, Os::CURRENT)?,
                _ => unreachable!(),
            };
            match action {
                Action::ZoomIn | Action::ZoomOut => ("Zoom preview".into(), Some("zoom".into())),
                Action::FeaturesTab
                | Action::CaptureTab
                | Action::ForwardTab
                | Action::ImageMode
                | Action::ToggleExposure
                | Action::ToggleFocusRegion
                | Action::ToggleActivity
                | Action::ToggleSidebar
                | Action::ToggleInspector
                | Action::FocusCamera
                | Action::Overview
                | Action::ZoomFit
                | Action::ZoomActual
                | Action::SearchFeatures => (action.label().into(), None),
                _ => return None,
            }
        }
        Message::Address(_) => ("Edit camera address".into(), Some("address".into())),
        Message::Search(_) => ("Search features".into(), Some("search".into())),
        Message::Num(key, _) => (
            format!("Edit {}", num_label(*key)),
            Some(format!("number:{key:?}")),
        ),
        Message::Draft(feature, _) => (
            format!("Edit {feature}"),
            Some(format!("feature:{feature}")),
        ),
        Message::Encoder(_) => ("Edit encoder".into(), Some("encoder".into())),
        Message::Tag(field, _) => (
            format!("Edit {} tag", field.label().to_lowercase()),
            Some(format!("tag:{}", field.key())),
        ),
        Message::AutoStart(_) => ("Change live view on connect".into(), None),
        Message::Bitrate(_) => ("Edit bitrate".into(), Some("bitrate".into())),
        Message::FocusRegion(_) => ("Move focus region".into(), Some("focus-region".into())),
        Message::CapturePicker(destinations::Message::Output(_)) => {
            ("Edit capture output".into(), Some("capture-output".into()))
        }
        Message::RecordPicker(destinations::Message::Output(_)) => {
            ("Edit recording output".into(), Some("record-output".into()))
        }
        Message::CapturePicker(
            destinations::Message::Select(_)
            | destinations::Message::UseVolume(_)
            | destinations::Message::BrowseOutput,
        ) => ("Choose capture destination".into(), None),
        Message::RecordPicker(
            destinations::Message::Select(_)
            | destinations::Message::UseVolume(_)
            | destinations::Message::BrowseOutput,
        ) => ("Choose recording destination".into(), None),
        Message::Format(_) => ("Change capture format".into(), None),
        Message::Codec(_) => ("Change video codec".into(), None),
        Message::ForwardMode(_) => ("Change video output".into(), None),
        Message::ScheduleEnabled(_) => ("Change scheduling".into(), None),
        Message::RetentionEnabled(_) => ("Change retention".into(), None),
        Message::OnFull(_) => ("Change storage limit action".into(), None),
        Message::FocusMetric(_) => ("Change focus metric".into(), None),
        Message::ResetFocusPeak => ("Reset focus peak".into(), None),
        Message::Forget(_) => ("Forget recent camera".into(), None),
        Message::Zoom(_) => ("Zoom preview".into(), Some("zoom".into())),
        Message::Fit | Message::Actual => ("Change preview scale".into(), None),
        Message::Appearance(_) => ("Change appearance".into(), None),
        Message::UiScale(_) => ("Change interface size".into(), None),
        Message::BalanceCommit(_) => ("Change auto exposure balance".into(), None),
        Message::Menu(menus::Command::Schedule) => ("Change scheduling".into(), None),
        Message::IncludeSimulator(_) | Message::TrySimulator => ("Show simulator".into(), None),
        Message::Tab(_) | Message::ShowTab(_) => ("Change settings panel".into(), None),
        Message::ToggleSidebar
        | Message::ToggleInspector
        | Message::ToggleImage
        | Message::ToggleExposure
        | Message::ToggleFocusRegion
        | Message::ToggleActivity
        | Message::ShowActivity
        | Message::CameraActivity(_)
        | Message::Focus(_)
        | Message::FocusTile(_)
        | Message::ToggleStorage
        | Message::ToggleAutoChanges => ("Change workbench view".into(), None),
        _ => return None,
    };
    Some((label, key))
}

pub(super) fn group_barrier(message: &Message) -> bool {
    match message {
        Message::Key { chord, captured } => {
            gesture(message).is_none() && Action::find(chord, !captured, Os::CURRENT).is_some()
        }
        Message::Menu(_) => gesture(message).is_none(),
        Message::FocusNext(_)
        | Message::Commit(_)
        | Message::SliderReleased(_)
        | Message::Set(_, _)
        | Message::Execute(_)
        | Message::EnterAddress
        | Message::ConnectAddress
        | Message::CameraRow(_)
        | Message::Select(_)
        | Message::Disconnect(_)
        | Message::ToggleStream
        | Message::StreamCamera(_, _)
        | Message::AllStreams(_)
        | Message::Auto(_)
        | Message::AllAuto(_)
        | Message::BalanceReleased
        | Message::Capture
        | Message::CaptureCamera(_)
        | Message::Schedule
        | Message::CancelJob(_)
        | Message::StartForward
        | Message::StopForward
        | Message::RefreshFeatures
        | Message::ScanNetwork
        | Message::NetworkReport(_)
        | Message::Help(_)
        | Message::CapturePicker(_)
        | Message::RecordPicker(_) => gesture(message).is_none(),
        _ => false,
    }
}

fn num_label(key: Num) -> &'static str {
    match key {
        Num::Count => "capture count",
        Num::Timeout => "capture timeout",
        Num::Delay => "schedule delay",
        Num::Interval => "schedule interval",
        Num::Fps => "video frame rate",
        Num::FileMib => "recording file limit",
        Num::QuotaGib => "storage space limit",
        Num::QuotaFiles => "storage file limit",
        Num::RetentionDays => "retention age",
    }
}

impl Workbench {
    pub(super) fn break_history_group(&mut self) {
        self.history.break_group();
        // Deferred entries must obey the same explicit gesture boundaries.
        if let Some((_, key, _, _)) = self.history_deferred.last_mut() {
            *key = None;
        }
    }

    pub(super) fn record_window_change(&mut self, before: bool, after: bool) {
        self.record_history(
            "Change full screen".into(),
            None,
            Edit::WindowMode { before, after },
        );
    }
    pub(super) fn fullscreen_task(
        &self,
        desired: Option<bool>,
        replay: Option<bool>,
    ) -> Task<Message> {
        Task::done(self.main_window).then(move |id| {
            window::mode(id).then(move |mode| {
                let before = mode == window::Mode::Fullscreen;
                let expected = desired.unwrap_or(!before);
                window::set_mode(
                    id,
                    if expected {
                        window::Mode::Fullscreen
                    } else {
                        window::Mode::Windowed
                    },
                )
                .chain(window::mode(id).map(move |actual| {
                    Message::FullscreenResult {
                        before,
                        actual: actual == window::Mode::Fullscreen,
                        expected,
                        replay,
                    }
                }))
            })
        })
    }
    pub(super) fn record_local_change(
        &mut self,
        label: String,
        mut key: Option<String>,
        before: LocalState,
        calibration: bool,
    ) {
        if self.drain_destination_changes() {
            return;
        }
        let after = LocalState::capture(self);
        if !before.differs(&after) && (!calibration || before.focus == after.focus) {
            return;
        }
        let focus_id = key.as_ref().and_then(|key| match key.as_str() {
            "address" => Some("connect-address".into()),
            "search" => Some("feature-search".into()),
            "encoder" => Some("forward-encoder".into()),
            "bitrate" => Some("forward-bitrate".into()),
            "capture-output" | "record-output" => Some(key.clone()),
            _ if key.starts_with("number:") => Some(key.clone()),
            _ if key.starts_with("feature:") => Some(key.clone()),
            _ => None,
        });
        if key.as_ref().is_some_and(|key| key.starts_with("feature:")) {
            key = key.map(|key| format!("{key}:{:?}", before.camera));
        }
        if key.as_deref() == Some("number:Timeout") {
            key = Some(format!("number:Timeout:{}", self.scanner_selected()));
        }
        self.record_history(
            label,
            key,
            Edit::Local(Box::new(LocalChange {
                before,
                after,
                calibration,
                focus_id,
            })),
        );
    }

    fn record_history(&mut self, label: String, key: Option<String>, change: Edit) {
        self.record_history_at(label, key, change, self.now);
    }

    fn record_history_at(&mut self, label: String, key: Option<String>, change: Edit, at: Instant) {
        if self.history_replay.is_some() {
            if let Edit::Local(local) = &change {
                if let Some(aggregate) = &mut self.history_deferred_apply {
                    aggregate.after = local.after.clone();
                } else {
                    self.history_deferred_apply = Some(local.clone());
                }
            }
            if key.is_some()
                && let Some((_, last_key, Edit::Local(last), last_at)) =
                    self.history_deferred.last_mut()
                && *last_key == key
                && (key.as_deref() == Some("focus-region")
                    || at.saturating_duration_since(*last_at) <= Duration::from_millis(800))
                && let Edit::Local(local) = &change
            {
                last.after = local.after.clone();
                *last_at = at;
                return;
            }
            self.history_deferred.push((label, key, change, at));
            if self.history_deferred.len() > 200 {
                self.history_deferred.remove(0);
            }
            return;
        }
        self.history.record(label, key, change, at, |old, new| {
            if let (Edit::Local(old), Edit::Local(new)) = (old, new) {
                old.after = new.after.clone();
                true
            } else {
                false
            }
        });
        self.history.retain(|change| !matches!(change, Edit::Local(local) if !local.calibration && !local.before.differs(&local.after)));
    }

    pub(super) fn record_camera_change_at(
        &mut self,
        change: super::camera_history::Change,
        at: Instant,
    ) {
        if self.history.contains(|edit| {
            edit.camera_changes()
                .iter()
                .any(|existing| existing.token() == change.token())
        }) {
            return;
        }
        self.known_camera_history
            .insert(change.token().to_owned(), (true, change.changed_at_us()));
        self.record_history_at(change.label(), None, Edit::Camera(change), at);
    }

    pub(super) fn record_camera_batch_at(
        &mut self,
        changes: Vec<super::camera_history::Change>,
        at: Instant,
    ) {
        if changes.len() == 1 {
            self.record_camera_change_at(changes.into_iter().next().unwrap(), at);
            return;
        }
        let Some(first) = changes.first() else {
            return;
        };
        let label = format!("{} on {} cameras", first.label(), changes.len());
        for change in &changes {
            self.known_camera_history
                .insert(change.token().to_owned(), (true, change.changed_at_us()));
        }
        self.record_history_at(label, None, Edit::CameraBatch(changes), at);
    }

    pub(super) fn sync_camera_history(&mut self) {
        if self.history_replay.is_some() || self.pending.iter().any(|pending| pending.batch) {
            return;
        }
        let mut entries: Vec<_> = self
            .snapshot
            .cameras
            .iter()
            .flat_map(|camera| {
                camera
                    .history
                    .entries
                    .iter()
                    .map(|entry| (camera.info.id.clone(), entry.clone()))
            })
            .collect();
        entries.sort_by_key(|(_, entry)| entry.last_changed_at_us);
        self.known_camera_history
            .retain(|token, _| entries.iter().any(|(_, entry)| &entry.token == token));
        self.history.expand(|edit| {
            let Edit::CameraBatch(changes) = edit else {
                return None;
            };
            let statuses: Vec<_> = changes
                .iter()
                .map(|change| {
                    entries
                        .iter()
                        .find(|(_, entry)| entry.token == change.token())
                        .map(|(_, entry)| entry.applied)
                })
                .collect();
            if !statuses.is_empty()
                && statuses
                    .iter()
                    .all(|status| status.is_some() && status == &statuses[0])
            {
                return None;
            }
            Some(
                changes
                    .iter()
                    .filter(|change| {
                        entries
                            .iter()
                            .any(|(_, entry)| entry.token == change.token())
                    })
                    .map(|change| (change.label(), Edit::Camera(change.clone())))
                    .collect(),
            )
        });
        self.history.retain(|edit| {
            edit.camera_changes().iter().all(|change| {
                entries
                    .iter()
                    .any(|(_, entry)| entry.token == change.token())
            })
        });
        let current_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            .min(u64::MAX as u128) as u64;
        for (camera, entry) in entries {
            if self.pending.iter().any(|pending| {
                pending.camera.as_ref() == Some(&camera)
                    && matches!(
                        pending.job,
                        Job::Set(_)
                            | Job::AutoOn
                            | Job::AutoOff
                            | Job::Balance
                            | Job::Start
                            | Job::Stop
                    )
            }) {
                continue;
            }
            let matches = |edit: &Edit| {
                edit.camera_changes()
                    .iter()
                    .any(|change| change.token() == entry.token)
            };
            if let Some(Edit::CameraBatch(changes)) = self.history.find(matches)
                && changes.iter().any(|change| {
                    self.snapshot
                        .cameras
                        .iter()
                        .flat_map(|camera| &camera.history.entries)
                        .any(|candidate| {
                            candidate.token == change.token()
                                && candidate.last_changed_at_us > entry.last_changed_at_us
                        })
                })
            {
                self.known_camera_history
                    .insert(entry.token, (entry.applied, entry.last_changed_at_us));
                continue;
            }
            let state = (entry.applied, entry.last_changed_at_us);
            let elapsed =
                Duration::from_micros(current_epoch.saturating_sub(entry.last_changed_at_us));
            let at = self.now.checked_sub(elapsed).unwrap_or(self.born);
            if self.history.contains(matches) {
                if self.known_camera_history.get(&entry.token) != Some(&state) {
                    self.history.synchronize(matches, entry.applied, at);
                }
            } else if self.known_camera_history.get(&entry.token) != Some(&state)
                && let Some(change) = super::camera_history::Change::from_status(&camera, &entry)
            {
                if entry.applied {
                    self.record_camera_change_at(change, at);
                } else {
                    self.history
                        .import_redo(entry.label, Edit::Camera(change), at);
                }
            }
            self.known_camera_history.insert(entry.token, state);
        }
    }

    pub(super) fn history_replayed(&mut self, success: bool) {
        if let Some(undo) = self.history_replay.take()
            && success
        {
            let label = if undo {
                self.history.undo()
            } else {
                self.history.redo()
            }
            .map_or("change", |entry| entry.label.as_str())
            .to_owned();
            self.history.committed(undo);
            self.set_notice(
                format!("{} {label}", if undo { "Undid" } else { "Redid" }),
                None,
                Level::Done,
            );
        }
        let deferred = std::mem::take(&mut self.history_deferred);
        if let Some(local) = self.history_deferred_apply.take()
            && let Err(error) = local.restore(self, false)
        {
            self.set_notice(
                "Could not reapply newer edits",
                Some(format!("{error:#}")),
                Level::Error,
            );
        }
        self.break_history_group();
        for (label, key, change, at) in deferred {
            if let Edit::Local(local) = &change
                && let Err(error) = local.restore(self, false)
            {
                self.set_notice(
                    "Could not reapply a newer edit",
                    Some(format!("{error:#}")),
                    Level::Error,
                );
                continue;
            }
            self.record_history_at(label, key, change, at);
        }
    }

    pub(super) fn history_label(&self, undo: bool) -> Option<&str> {
        let picker = if self.capture_to.editor_open() {
            Some(&self.capture_to)
        } else if self.record_to.editor_open() {
            Some(&self.record_to)
        } else {
            None
        };
        if let Some(picker) = picker {
            return if undo {
                picker.undo_label()
            } else {
                picker.redo_label()
            };
        }
        if undo {
            self.history.undo()
        } else {
            self.history.redo()
        }
        .map(|entry| entry.label.as_str())
    }

    pub(super) fn history_busy(&self) -> bool {
        self.history_replay.is_some()
            || self.capture_to.mutation_busy()
            || self.record_to.mutation_busy()
            || self.pending.iter().any(|pending| {
                matches!(
                    pending.job,
                    Job::Set(_)
                        | Job::NetworkScan
                        | Job::AutoOn
                        | Job::AutoOff
                        | Job::Balance
                        | Job::Start
                        | Job::Stop
                )
            })
    }

    pub(super) fn replay_history(&mut self, undo: bool) -> Task<Message> {
        self.break_history_group();
        if self.history_busy() {
            return Task::none();
        }
        if self.capture_to.editor_open() || self.record_to.editor_open() {
            let picker = if self.capture_to.editor_open() {
                &mut self.capture_to
            } else {
                &mut self.record_to
            };
            if undo {
                picker.undo();
            } else {
                picker.redo();
            }
            return Task::none();
        }
        if self.help_open || self.network_open || self.analytics.open {
            return Task::none();
        }
        let entry = if undo {
            self.history.undo()
        } else {
            self.history.redo()
        }
        .cloned();
        let Some(entry) = entry else {
            return Task::none();
        };
        self.history_replay = Some(undo);
        if let Edit::WindowMode { before, after } = &entry.change {
            return self.fullscreen_task(Some(if undo { *before } else { *after }), Some(undo));
        }
        let result = match &entry.change {
            Edit::WindowMode { .. } => unreachable!("handled above"),
            Edit::Local(change) => change.restore(self, undo).map(|()| true),
            Edit::Camera(change) => self
                .replay_camera_change(change.clone(), undo)
                .map(|()| false),
            Edit::CameraBatch(changes) => self
                .replay_camera_batch(changes.clone(), undo)
                .map(|()| false),
            Edit::Destination(recording, change) => {
                let (picker, output) = if *recording {
                    (&mut self.record_to, &mut self.forward_output)
                } else {
                    (&mut self.capture_to, &mut self.output)
                };
                picker
                    .replay(change, undo, output)
                    .map_err(anyhow::Error::msg)
            }
        };
        let focus = match &entry.change {
            Edit::Local(change) => change.focus_id.clone(),
            _ => None,
        };
        match result {
            Ok(true) => self.history_replayed(true),
            Ok(false) => {}
            Err(error) => {
                self.history_replayed(false);
                self.set_notice(
                    format!(
                        "Could not {} {}",
                        if undo { "undo" } else { "redo" },
                        entry.label
                    ),
                    Some(format!("{error:#}")),
                    Level::Error,
                );
            }
        }
        focus.map_or_else(Task::none, |id| {
            Task::batch([
                operation::focus(id.clone()),
                operation::move_cursor_to_end(id),
            ])
        })
    }

    pub(super) fn poll_destination_history(&mut self) -> bool {
        let changed = self.drain_destination_changes();
        for recording in [false, true] {
            let replay = if recording {
                self.record_to.take_replay_result(&mut self.forward_output)
            } else {
                self.capture_to.take_replay_result(&mut self.output)
            };
            if let Some(result) = replay {
                self.history_replayed(result.is_ok());
                if let Err(error) = result {
                    self.set_notice("Could not restore destination", Some(error), Level::Error);
                }
            }
        }
        changed
    }

    fn drain_destination_changes(&mut self) -> bool {
        let mut changed = false;
        for recording in [false, true] {
            let changes = if recording {
                self.record_to.take_changes()
            } else {
                self.capture_to.take_changes()
            };
            for change in changes {
                changed = true;
                let at = change.created_at();
                self.record_history_at(
                    change.label().to_owned(),
                    None,
                    Edit::Destination(recording, Box::new(change)),
                    at,
                );
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bench() -> Workbench {
        let mut app = Workbench::new(SessionHandle::new(), "history-test".into(), true, None);
        // The single-file output whose name follows the count and format.
        app.output = "capture.png".into();
        app
    }
    fn update(app: &mut Workbench, message: Message) {
        let _ = app.update(message);
    }
    fn key(app: &mut Workbench, action: Action) {
        update(
            app,
            Message::Key {
                chord: action.bindings(Os::CURRENT)[0],
                captured: true,
            },
        );
    }

    #[test]
    fn typing_undo_restores_numeric_drafts_and_dependent_output_together() {
        let mut app = bench();
        update(&mut app, Message::Num(Num::Count, "12".into()));
        update(&mut app, Message::Num(Num::Count, "12x".into()));
        assert_eq!(app.count, 12);
        assert_eq!(app.output, "captures");
        assert_eq!(app.drafts[&Num::Count], "12x");
        key(&mut app, Action::Undo);
        assert_eq!(app.count, 1);
        assert_eq!(app.output, "capture.png");
        assert!(!app.drafts.contains_key(&Num::Count));
        key(&mut app, Action::Redo);
        assert_eq!(app.count, 12);
        assert_eq!(app.output, "captures");
        assert_eq!(app.drafts[&Num::Count], "12x");
        update(&mut app, Message::Format(FORMATS[1]));
        update(&mut app, Message::Undo);
        assert_eq!(app.format, "png");
        update(&mut app, Message::Encoder("custom".into()));
        assert!(app.history.redo().is_none());
    }

    #[test]
    fn format_extension_and_keyboard_view_changes_share_the_timeline() {
        let mut app = bench();
        update(&mut app, Message::Format(FORMATS[1]));
        assert_eq!(app.output, "capture.jpeg");
        key(&mut app, Action::ToggleSidebar);
        assert!(!app.sidebar_open);
        key(&mut app, Action::Undo);
        assert!(app.sidebar_open);
        assert_eq!(app.format, "jpeg");
        key(&mut app, Action::Undo);
        assert_eq!(app.format, "png");
        assert_eq!(app.output, "capture.png");
        key(&mut app, Action::Redo);
        assert_eq!(app.format, "jpeg");
        assert_eq!(app.output, "capture.jpeg");
    }

    #[test]
    fn user_gestures_and_scanner_context_separate_typing_groups() {
        let mut app = bench();
        update(&mut app, Message::Num(Num::Timeout, "4000".into()));
        update(&mut app, Message::FocusNext(false));
        let mut scanner = crate::transport::simulator::info();
        scanner.transport = crate::types::Transport::Scanner;
        scanner.id = "scanner:ica:test".into();
        app.snapshot.connected = Some(scanner);
        update(&mut app, Message::Num(Num::Timeout, "45000".into()));
        update(&mut app, Message::Undo);
        assert_eq!(app.scan_timeout_ms, 60000);
        assert_eq!(app.timeout_ms, 4000);
        update(&mut app, Message::Undo);
        assert_eq!(app.timeout_ms, 5000);
        update(&mut app, Message::Address("a".into()));
        update(&mut app, Message::FocusNext(false));
        update(&mut app, Message::Address("ab".into()));
        update(&mut app, Message::Undo);
        assert_eq!(app.address, "a");
    }

    #[test]
    fn each_focus_drag_is_one_separate_reversible_gesture() {
        let mut app = bench();
        let original = app.focus.region;
        let first = iced::Rectangle {
            x: 0.1,
            y: 0.1,
            ..original
        };
        let second = iced::Rectangle {
            x: 0.2,
            y: 0.2,
            ..original
        };
        let third = iced::Rectangle {
            x: 0.3,
            y: 0.3,
            ..original
        };
        update(&mut app, Message::FocusRegionStart);
        update(&mut app, Message::FocusRegion(first));
        update(&mut app, Message::FocusRegion(second));
        update(&mut app, Message::FocusRegionStart);
        update(&mut app, Message::FocusRegion(third));
        update(&mut app, Message::Undo);
        assert_eq!(app.focus.region, second);
        update(&mut app, Message::Undo);
        assert_eq!(app.focus.region, original);
        update(&mut app, Message::Redo);
        assert_eq!(app.focus.region, second);
        update(&mut app, Message::Redo);
        assert_eq!(app.focus.region, third);
    }

    #[test]
    fn focus_peak_reset_restores_calibration_and_empty_reset_is_a_noop() {
        let mut app = bench();
        update(&mut app, Message::ResetFocusPeak);
        assert!(app.history.undo().is_none());
        app.focus.record(crate::frame::Sharpness {
            laplacian: 12.0,
            tenengrad: 8.0,
            brenner: 4.0,
            variance: 2.0,
        });
        let calibrated = app.focus.clone();
        update(&mut app, Message::ResetFocusPeak);
        assert_eq!(app.focus.latest(), None);
        update(&mut app, Message::Undo);
        assert!(app.focus == calibrated);
        update(&mut app, Message::Redo);
        assert_eq!(app.focus.latest(), None);
        update(&mut app, Message::ResetFocusPeak);
        assert_eq!(app.history_label(true), Some("Reset focus peak"));
        update(&mut app, Message::Undo);
        assert!(app.focus == calibrated);
    }

    #[test]
    fn restores_only_changed_fields_and_keeps_other_cameras_drafts() {
        let mut app = bench();
        app.snapshot.active_camera = Some("sim:0".into());
        app.edit_sources.insert("Gain".into(), "0".into());
        update(&mut app, Message::Draft("Gain".into(), "3".into()));
        app.camera_drafts.insert(
            "sim:0".into(),
            (app.edits.clone(), app.edit_sources.clone()),
        );
        app.snapshot.active_camera = Some("sim:1".into());
        app.edits = HashMap::from([("Gain".into(), "7".into())]);
        app.output = "external-path.png".into();
        update(&mut app, Message::Undo);
        assert_eq!(app.edits["Gain"], "7");
        assert_eq!(app.camera_drafts["sim:0"].0["Gain"], "0");
        assert_eq!(app.output, "external-path.png");
    }

    #[test]
    fn pending_replay_keeps_newer_local_edits_and_branches_redo() {
        let mut app = bench();
        update(&mut app, Message::Encoder("first".into()));
        app.history_replay = Some(true);
        update(&mut app, Message::Encoder("newer".into()));
        // An asynchronous restore may have just restored this field.
        app.forward_encoder = "auto".into();
        app.history_replayed(true);
        assert_eq!(app.forward_encoder, "newer");
        assert!(app.history.redo().is_none());
        update(&mut app, Message::Undo);
        assert_eq!(app.forward_encoder, "first");
    }

    #[test]
    fn deferred_edits_keep_explicit_gesture_boundaries() {
        let mut app = bench();
        update(&mut app, Message::Encoder("first".into()));
        app.history_replay = Some(true);
        update(&mut app, Message::Encoder("a".into()));
        update(&mut app, Message::FocusNext(false));
        update(&mut app, Message::Encoder("ab".into()));
        let region = app.focus.region;
        let first = iced::Rectangle { x: 0.1, ..region };
        let second = iced::Rectangle { x: 0.2, ..region };
        update(&mut app, Message::FocusRegionStart);
        update(&mut app, Message::FocusRegion(first));
        update(&mut app, Message::FocusRegionStart);
        update(&mut app, Message::FocusRegion(second));
        app.forward_encoder = "auto".into();
        app.history_replayed(true);
        assert_eq!(app.forward_encoder, "ab");
        update(&mut app, Message::Undo);
        assert_eq!(app.focus.region, first);
        update(&mut app, Message::Undo);
        assert_eq!(app.focus.region, region);
        update(&mut app, Message::Undo);
        assert_eq!(app.forward_encoder, "a");
        update(&mut app, Message::Undo);
        assert_eq!(app.forward_encoder, "first");
    }

    #[test]
    fn destination_editor_undo_never_changes_underlying_workbench() {
        let mut app = bench();
        update(&mut app, Message::Encoder("custom".into()));
        update(
            &mut app,
            Message::CapturePicker(destinations::Message::AddBucket),
        );
        update(
            &mut app,
            Message::CapturePicker(destinations::Message::Bucket("first".into())),
        );
        key(&mut app, Action::Undo);
        assert_eq!(app.forward_encoder, "custom");
        assert!(app.capture_to.can_redo());
        update(
            &mut app,
            Message::CapturePicker(destinations::Message::Cancel),
        );
        key(&mut app, Action::Undo);
        assert_eq!(app.forward_encoder, "auto");
    }

    #[test]
    fn shared_session_camera_edits_and_external_replays_are_visible_in_the_window() {
        let worker = crate::engine::WorkerHandle::new();
        let command = |command| worker.request(command).unwrap();
        command(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 1000,
        });
        let mut app = bench();
        let sync = |app: &mut Workbench| {
            app.now = Instant::now();
            let snapshot = worker.snapshot();
            app.snapshot = snapshot.clone();
            if let Some(info) = snapshot.connected {
                let mut camera = liveness::streaming_camera(0, 0, 0.0);
                camera.info = info;
                camera.history = snapshot.history;
                app.snapshot.cameras = vec![camera];
            }
            app.sync_camera_history();
        };
        command(SessionCommand::Set {
            feature: "Gain".into(),
            value: "3".into(),
        });
        sync(&mut app);
        assert_eq!(app.history_label(true), Some("Set Gain"));
        command(SessionCommand::Undo);
        sync(&mut app);
        assert_eq!(app.history_label(false), Some("Set Gain"));
        assert_eq!(
            command(SessionCommand::Get {
                feature: "Gain".into()
            })["value"],
            serde_json::json!(0.0)
        );
        command(SessionCommand::Redo);
        sync(&mut app);
        assert_eq!(app.history_label(true), Some("Set Gain"));
        assert!(app.history_label(false).is_none());
        command(SessionCommand::Undo);
        sync(&mut app);
        assert_eq!(app.history_label(false), Some("Set Gain"));
        command(SessionCommand::Set {
            feature: "Gain".into(),
            value: "5".into(),
        });
        sync(&mut app);
        assert!(app.history_label(false).is_none());
        assert_eq!(app.history_label(true), Some("Set Gain"));
        command(SessionCommand::Disconnect);
        sync(&mut app);
        assert!(app.history_label(true).is_none());
    }

    #[test]
    fn external_undo_and_redo_between_polls_reposition_the_shared_entry() {
        let worker = crate::engine::WorkerHandle::new();
        worker
            .request(SessionCommand::Connect {
                camera: "sim:0".into(),
                timeout_ms: 1000,
            })
            .unwrap();
        worker
            .request(SessionCommand::Set {
                feature: "Gain".into(),
                value: "3".into(),
            })
            .unwrap();
        let mut app = bench();
        let sync = |app: &mut Workbench| {
            app.now = Instant::now();
            let snapshot = worker.snapshot();
            let mut camera = liveness::streaming_camera(0, 0, 0.0);
            camera.info = snapshot.connected.unwrap();
            camera.history = snapshot.history;
            app.snapshot.cameras = vec![camera];
            app.sync_camera_history();
        };
        sync(&mut app);
        update(&mut app, Message::Encoder("later-local-edit".into()));
        assert_eq!(app.history_label(true), Some("Edit encoder"));
        worker.request(SessionCommand::Undo).unwrap();
        worker.request(SessionCommand::Redo).unwrap();
        sync(&mut app);
        assert_eq!(app.history_label(true), Some("Set Gain"));
        worker.request(SessionCommand::Undo).unwrap();
        sync(&mut app);
        assert_eq!(app.history_label(true), Some("Edit encoder"));
        assert_eq!(app.history_label(false), Some("Set Gain"));
        // Polling unchanged snapshots must not move camera history repeatedly.
        sync(&mut app);
        assert_eq!(app.history_label(true), Some("Edit encoder"));
    }

    #[test]
    fn evicted_worker_entries_are_not_reimported_and_typing_net_zero_is_removed() {
        let mut app = bench();
        let mut camera = liveness::streaming_camera(0, 0, 0.0);
        camera.history.entries = (0..210)
            .map(|i| crate::session::CameraHistoryEntry {
                token: format!("token-{i}"),
                label: format!("Set Gain {i}"),
                applied: true,
                created_at_ms: epoch_ms(),
                last_changed_at_us: i,
                feature: Some("Gain".into()),
                kind: "set".into(),
            })
            .collect();
        app.snapshot.cameras = vec![camera];
        app.sync_camera_history();
        update(&mut app, Message::Encoder("newer".into()));
        for _ in 0..3 {
            app.sync_camera_history();
        }
        assert_eq!(app.history_label(true), Some("Edit encoder"));
        update(&mut app, Message::Undo);
        update(&mut app, Message::Address("abc".into()));
        update(&mut app, Message::Address(String::new()));
        assert_eq!(app.history_label(true), Some("Set Gain 209"));
    }
}
