//! The native workbench. Camera I/O lives on the session worker, never the UI thread.
mod destinations;
mod gpu;
mod icon;
mod keys;
mod prefs;
mod preview;
mod style;

use crate::{
    auto::{AutoChange, AutoStatus},
    frame,
    genicam::FeatureInfo,
    session::{SessionCommand, SessionHandle, SessionSnapshot},
    storage::StoragePolicy,
    types::{CameraInfo, MONO8, RGB8, TransportStats},
};
use anyhow::Result;
use destinations::Picker;
use gpu::GpuFrames;
use iced::widget::{
    button, center, column, container, mouse_area, opaque, operation, pick_list, progress_bar,
    responsive, row, rule, scrollable, shader, slider, space, stack, text, text_input, tooltip,
};
use iced::{
    Alignment, Animation, Color, Element, Fill, Length, Size, Subscription, Task, Theme, event,
    keyboard, system, theme, time, window,
};
use icon::{Icon, icon};
use keys::{Action, Chord, Os};
use prefs::{Prefs, Recent};
use preview::Shown;
use std::{
    cell::Cell,
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use style::Palette;

/// Room for the macOS traffic lights, which sit over the content.
const TOP: f32 = if cfg!(target_os = "macos") {
    38.0
} else {
    18.0
};
const SIDEBAR: f32 = 240.0;
const INSPECTOR: f32 = 316.0;
const GUTTER: f32 = 28.0;
const NOTICE_LIFE: Duration = Duration::from_secs(5);
const NOTICE_FADE: Duration = Duration::from_millis(180);

pub fn run(handle: SessionHandle, session_label: String, simulated: bool) -> Result<()> {
    run_capture(handle, session_label, simulated, None, 0)
}

pub fn run_capture(
    handle: SessionHandle,
    session_label: String,
    simulated: bool,
    screenshot: Option<PathBuf>,
    demo_cameras: u32,
) -> Result<()> {
    let outcome: Arc<Mutex<Option<String>>> =
        Arc::new(Mutex::new(screenshot.as_ref().map(|_| {
            "Window closed before the renderer screenshot was saved".into()
        })));
    let app_outcome = outcome.clone();
    let screenshot_size = if screenshot.is_some() {
        std::env::var("CAPTUREFAB_SCREENSHOT_SIZE")
            .ok()
            .map(|size| -> Result<Size> {
                let (width, height) = size.split_once('x').ok_or_else(|| {
                    anyhow::anyhow!("CAPTUREFAB_SCREENSHOT_SIZE must be WIDTHxHEIGHT")
                })?;
                let width: u32 = width.parse()?;
                let height: u32 = height.parse()?;
                anyhow::ensure!(
                    (900..=8192).contains(&width) && (620..=8192).contains(&height),
                    "screenshot size must be at least 900x620 and at most 8192x8192"
                );
                Ok(Size::new(width as f32, height as f32))
            })
            .transpose()?
    } else {
        None
    };
    let boot = move || {
        let capturing = screenshot.is_some();
        let mut app = Workbench::new(
            handle.clone(),
            session_label.clone(),
            simulated || capturing,
            (!capturing).then(prefs::load),
        );
        if let Some(path) = &screenshot {
            let count = demo_cameras.clamp(1, 16);
            app.screenshot = Some(ScreenshotRequest {
                path: path.clone(),
                cameras: count,
                started: Instant::now(),
                streams_started: None,
                requested: false,
                save: None,
                outcome: app_outcome.clone(),
            });
            for index in 0..count {
                app.send(
                    "Connecting demo camera",
                    SessionCommand::Connect {
                        camera: format!("sim:{index}"),
                        timeout_ms: 2000,
                    },
                );
            }
        } else {
            app.discover();
        }
        (app, system::theme().map(Message::SystemTheme))
    };
    iced::application(boot, Workbench::update, Workbench::view)
        .title("Capturefab")
        .subscription(Workbench::subscription)
        .theme(Workbench::theme)
        .default_font(style::SANS)
        .font(icon::REGULAR_BYTES)
        .font(icon::FILL_BYTES)
        .antialiasing(true)
        .window(window::Settings {
            size: screenshot_size.unwrap_or(Size::new(1280.0, 840.0)),
            min_size: Some(Size::new(900.0, 620.0)),
            #[cfg(target_os = "macos")]
            platform_specific: window::settings::PlatformSpecific {
                title_hidden: true,
                titlebar_transparent: true,
                fullsize_content_view: true,
            },
            ..window::Settings::default()
        })
        .run()
        .map_err(|err| anyhow::anyhow!("Could not start the native window: {err}"))?;
    if let Some(error) = outcome
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        anyhow::bail!(error);
    }
    Ok(())
}

struct ScreenshotRequest {
    path: PathBuf,
    cameras: u32,
    started: Instant,
    streams_started: Option<Instant>,
    requested: bool,
    save: Option<Receiver<Result<()>>>,
    outcome: Arc<Mutex<Option<String>>>,
}

struct Pending {
    label: String,
    receiver: Receiver<anyhow::Result<serde_json::Value>>,
    target: Option<String>,
}

/// GPU slot of the single-camera view; camera tiles use their camera IDs.
const MAIN_VIEW: &str = "\0main";

#[derive(Default)]
struct CameraPreview {
    frame_id: Option<u64>,
    shown: Option<Shown>,
    meta: Option<(u64, u32, u32, u32)>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Tab {
    Features,
    Capture,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Appearance {
    System,
    Light,
    Dark,
}

/// A pick-list entry: the value sent to the session and its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Choice {
    value: &'static str,
    label: &'static str,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label)
    }
}

const fn choice(value: &'static str, label: &'static str) -> Choice {
    Choice { value, label }
}

const FORMATS: [Choice; 5] = [
    choice("png", "PNG image"),
    choice("jpeg", "JPEG image"),
    choice("raw", "Raw pixels"),
    choice("pgm", "PGM monochrome"),
    choice("ppm", "PPM color"),
];
const CODECS: [Choice; 2] = [choice("h264", "H.264"), choice("h265", "H.265 / HEVC")];
const ON_FULL: [Choice; 2] = [
    choice("stop", "Stop when full"),
    choice("delete-oldest", "Delete oldest"),
];

fn find(choices: &[Choice], value: &str) -> Option<Choice> {
    choices.iter().copied().find(|c| c.value == value)
}

/// Numeric settings, edited as text and applied once they parse within range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Num {
    Count,
    Timeout,
    Delay,
    Interval,
    Fps,
    FileMib,
    QuotaGib,
    QuotaFiles,
    RetentionDays,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    Frame,
    SystemTheme(theme::Mode),
    Key { chord: Chord, captured: bool },
    CycleAppearance,
    Help(bool),
    DragWindow,
    Discover,
    IncludeSimulator(bool),
    TrySimulator,
    EnterAddress,
    CameraRow(String),
    Forget(String),
    Disconnect(String),
    Address(String),
    ConnectAddress,
    ToggleStream,
    Capture,
    Focus(bool),
    Select(String),
    StreamCamera(String, bool),
    AllStreams(bool),
    AllAuto(bool),
    Fit,
    Actual,
    Zoom(f32),
    ToggleHistogram,
    ToggleActivity,
    CopyLog,
    CopySessionCommand,
    Copy(String),
    Tab(Tab),
    Search(String),
    RefreshFeatures,
    Auto(bool),
    Balance(f64),
    BalanceReleased,
    ToggleAutoChanges,
    Draft(String, String),
    Commit(String),
    Set(String, String),
    Execute(String),
    Num(Num, String),
    Format(Choice),
    ScheduleEnabled(bool),
    Schedule,
    ToggleStorage,
    OnFull(Choice),
    RetentionEnabled(bool),
    RefreshJobs,
    CancelJob(u64),
    Codec(Choice),
    Encoder(String),
    Bitrate(String),
    StartForward,
    StopForward,
    CapturePicker(destinations::Message),
    RecordPicker(destinations::Message),
    Screenshot(window::Screenshot),
}

struct Workbench {
    handle: SessionHandle,
    session: String,
    snapshot: SessionSnapshot,
    observed_camera: Option<String>,
    include_simulator: bool,
    address: String,
    search: String,
    tab: Tab,
    /// Drafts of text feature values, and the camera value each draft started from.
    edits: HashMap<String, String>,
    edit_sources: HashMap<String, String>,
    output: String,
    /// Where stills and schedules are saved.
    capture_to: Picker,
    /// Where local recordings are saved.
    record_to: Picker,
    count: u32,
    format: &'static str,
    timeout_ms: u64,
    forward_output: String,
    forward_codec: &'static str,
    forward_encoder: String,
    forward_fps: f64,
    forward_bitrate: String,
    forward_file_mib: f64,
    quota_gib: f64,
    quota_files: u32,
    retention_enabled: bool,
    retention_days: u64,
    quota_action: &'static str,
    schedule_enabled: bool,
    schedule_delay_seconds: u64,
    schedule_interval_seconds: f64,
    /// Number fields as typed, until they hold a valid value again.
    drafts: HashMap<Num, String>,
    balance: f64,
    balance_dragging: bool,
    storage_open: bool,
    auto_changes_open: bool,
    system_dark: bool,
    appearance: Appearance,
    screenshot: Option<ScreenshotRequest>,
    logs_open: bool,
    help_open: bool,
    fit: bool,
    zoom: f32,
    /// Size of the preview stage at the last layout, so zooming from Fit
    /// starts where the image already is.
    stage: Cell<Size>,
    histogram_open: bool,
    focus_camera: bool,
    histogram: [u32; 64],
    shown: Option<Shown>,
    gpu: GpuFrames,
    frame_meta: Option<(u64, u32, u32, u32, u64)>,
    frame_id: Option<u64>,
    display_error: Option<String>,
    previews: HashMap<String, CameraPreview>,
    pending: Vec<Pending>,
    notice: Option<(String, bool, Instant)>,
    recent: Vec<Recent>,
    saved: Option<Prefs>,
    prefs_changed: Option<Instant>,
    now: Instant,
    born: Instant,
    sheet: Animation<bool>,
    activity_slide: Animation<bool>,
    histogram_slide: Animation<bool>,
    shutter: Animation<bool>,
    welcome: Animation<bool>,
}

impl Workbench {
    fn new(handle: SessionHandle, session: String, simulated: bool, saved: Option<Prefs>) -> Self {
        let prefs = saved.clone().unwrap_or_default();
        let defaults = Prefs::default();
        let pick = |choices: &[Choice], value: &str, fallback: &str| {
            find(choices, value)
                .or_else(|| find(choices, fallback))
                .map_or("", |choice| choice.value)
        };
        let now = Instant::now();
        Self {
            handle,
            session,
            snapshot: SessionSnapshot::default(),
            observed_camera: None,
            include_simulator: simulated || prefs.include_simulator,
            address: String::new(),
            search: String::new(),
            tab: prefs.tab,
            edits: HashMap::new(),
            edit_sources: HashMap::new(),
            capture_to: Picker::new(true, false),
            record_to: Picker::new(false, true),
            count: prefs.count,
            format: pick(&FORMATS, &prefs.format, &defaults.format),
            timeout_ms: prefs.timeout_ms,
            forward_codec: pick(&CODECS, &prefs.forward_codec, &defaults.forward_codec),
            forward_encoder: prefs.forward_encoder,
            forward_fps: prefs.forward_fps,
            forward_bitrate: prefs.forward_bitrate,
            forward_file_mib: prefs.forward_file_mib,
            quota_gib: prefs.quota_gib,
            quota_files: prefs.quota_files,
            retention_enabled: prefs.retention_enabled,
            retention_days: prefs.retention_days,
            quota_action: pick(&ON_FULL, &prefs.quota_action, &defaults.quota_action),
            schedule_enabled: false,
            schedule_delay_seconds: prefs.schedule_delay_seconds,
            schedule_interval_seconds: prefs.schedule_interval_seconds,
            output: prefs.output,
            forward_output: prefs.forward_output,
            drafts: HashMap::new(),
            balance: crate::auto::DEFAULT_BALANCE,
            balance_dragging: false,
            storage_open: false,
            auto_changes_open: false,
            system_dark: false,
            // CAPTUREFAB_APPEARANCE=light|dark overrides the system, e.g. for screenshots.
            appearance: match std::env::var("CAPTUREFAB_APPEARANCE").as_deref() {
                Ok("light") => Appearance::Light,
                Ok("dark") => Appearance::Dark,
                _ => prefs.appearance,
            },
            screenshot: None,
            logs_open: false,
            help_open: false,
            fit: true,
            zoom: 1.0,
            stage: Cell::new(Size::new(800.0, 600.0)),
            histogram_open: prefs.histogram_open,
            focus_camera: false,
            histogram: [0; 64],
            shown: None,
            gpu: GpuFrames::new(),
            frame_meta: None,
            frame_id: None,
            display_error: None,
            previews: HashMap::new(),
            pending: Vec::new(),
            notice: None,
            recent: prefs.recent,
            saved,
            prefs_changed: None,
            now,
            born: now,
            sheet: Animation::new(false).quick(),
            activity_slide: Animation::new(false).quick(),
            histogram_slide: Animation::new(prefs.histogram_open).quick(),
            shutter: Animation::new(false).duration(Duration::from_millis(320)),
            welcome: Animation::new(false).slow(),
        }
    }

    fn prefs(&self) -> Prefs {
        Prefs {
            appearance: self.appearance,
            include_simulator: self.include_simulator,
            tab: self.tab,
            histogram_open: self.histogram_open,
            output: self.output.clone(),
            format: self.format.into(),
            count: self.count,
            timeout_ms: self.timeout_ms,
            schedule_delay_seconds: self.schedule_delay_seconds,
            schedule_interval_seconds: self.schedule_interval_seconds,
            forward_output: self.forward_output.clone(),
            forward_codec: self.forward_codec.into(),
            forward_encoder: self.forward_encoder.clone(),
            forward_fps: self.forward_fps,
            forward_bitrate: self.forward_bitrate.clone(),
            forward_file_mib: self.forward_file_mib,
            quota_gib: self.quota_gib,
            quota_files: self.quota_files,
            retention_enabled: self.retention_enabled,
            retention_days: self.retention_days,
            quota_action: self.quota_action.into(),
            recent: self.recent.clone(),
        }
    }

    fn save_prefs(&mut self) {
        let Some(saved) = &self.saved else {
            return;
        };
        let prefs = self.prefs();
        if &prefs == saved {
            self.prefs_changed = None;
            return;
        }
        let changed = *self.prefs_changed.get_or_insert(self.now);
        if self.now.duration_since(changed) >= Duration::from_millis(600) {
            self.prefs_changed = None;
            self.saved = Some(prefs.clone());
            std::thread::spawn(move || {
                let _ = prefs::save(&prefs);
            });
        }
    }

    fn sheet_open(&self) -> bool {
        self.help_open || self.capture_to.manager_open || self.record_to.manager_open
    }

    fn animating(&self) -> bool {
        let fading = self.notice.as_ref().is_some_and(|(_, error, at)| {
            let age = self.now.duration_since(*at);
            age < NOTICE_FADE
                || (!error && age > NOTICE_LIFE - NOTICE_FADE * 2 && age < NOTICE_LIFE)
        });
        fading
            || [
                &self.sheet,
                &self.activity_slide,
                &self.histogram_slide,
                &self.shutter,
                &self.welcome,
            ]
            .iter()
            .any(|animation| animation.is_animating(self.now))
    }

    fn sync_animations(&mut self) {
        let now = self.now;
        let open = self.sheet_open();
        if open != self.sheet.value() {
            if open {
                self.sheet.go_mut(true, now);
            } else {
                self.sheet = Animation::new(false).quick();
            }
        }
        if self.logs_open != self.activity_slide.value() {
            self.activity_slide.go_mut(self.logs_open, now);
        }
        if self.histogram_open != self.histogram_slide.value() {
            self.histogram_slide.go_mut(self.histogram_open, now);
        }
        let welcome = self.snapshot.connected.is_none() && !self.overview();
        if welcome != self.welcome.value() {
            if welcome {
                self.welcome.go_mut(true, now);
            } else {
                self.welcome = Animation::new(false).slow();
            }
        }
    }

    fn pulse(&self) -> f32 {
        if self.screenshot.is_some() {
            return 1.0;
        }
        let t = self.now.duration_since(self.born).as_secs_f32();
        0.7 + 0.3 * (t * std::f32::consts::TAU / 1.8).cos()
    }

    fn notice_alpha(&self, at: Instant, error: bool) -> f32 {
        let age = self.now.duration_since(at).as_secs_f32();
        let fade = NOTICE_FADE.as_secs_f32();
        let rise = (age / fade).min(1.0);
        if error {
            return rise;
        }
        rise.min(((NOTICE_LIFE.as_secs_f32() - age) / (fade * 2.0)).clamp(0.0, 1.0))
    }

    fn dark(&self) -> bool {
        match self.appearance {
            Appearance::System => self.system_dark,
            Appearance::Light => false,
            Appearance::Dark => true,
        }
    }

    fn theme(&self) -> Theme {
        style::theme(self.dark())
    }

    fn subscription(&self) -> Subscription<Message> {
        let streaming = self.snapshot.cameras.iter().any(|camera| camera.streaming);
        let busy = !self.pending.is_empty()
            || self.screenshot.is_some()
            || self.capture_to.busy()
            || self.record_to.busy();
        let interval = if streaming {
            Duration::from_millis(33)
        } else if busy {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(300)
        };
        let frames = if self.animating() {
            window::frames().map(|_| Message::Frame)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            frames,
            time::every(interval).map(|_| Message::Tick),
            event::listen_with(|event, status, _window| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key,
                    physical_key,
                    modifiers,
                    ..
                }) => keys::chord(&key, physical_key, modifiers, Os::CURRENT).map(|chord| {
                    Message::Key {
                        chord,
                        captured: status == event::Status::Captured,
                    }
                }),
                _ => None,
            }),
            system::theme_changes().map(Message::SystemTheme),
        ])
    }

    fn send(&mut self, label: impl Into<String>, command: SessionCommand) {
        let label = label.into();
        match self.handle.submit(command) {
            Ok(receiver) => {
                self.notice = None;
                self.pending.push(Pending {
                    label,
                    receiver,
                    target: None,
                });
            }
            Err(err) => self.notice = Some((err.to_string(), true, Instant::now())),
        }
    }

    fn send_to(&mut self, camera: &str, label: impl Into<String>, command: SessionCommand) {
        match self.handle.submit_to(camera, command) {
            Ok(receiver) => {
                self.notice = None;
                self.pending.push(Pending {
                    label: label.into(),
                    receiver,
                    target: None,
                });
            }
            Err(error) => self.notice = Some((error.to_string(), true, Instant::now())),
        }
    }

    fn pending(&self, label: &str) -> bool {
        self.pending.iter().any(|p| p.label == label)
    }

    fn auto_busy(&self) -> bool {
        self.pending.iter().any(|p| {
            matches!(
                p.label.as_str(),
                "Enabling auto mode" | "Switching to manual" | "Updating auto balance"
            )
        })
    }

    fn overview(&self) -> bool {
        self.snapshot.cameras.len() > 1 && !self.focus_camera
    }

    fn poll(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            match self.pending[i].receiver.try_recv() {
                Ok(result) => {
                    let pending = self.pending.swap_remove(i);
                    if let Ok(value) = &result {
                        self.finished(&pending, value);
                    }
                    self.notice = Some(match result {
                        Ok(_) => (format!("{} · done", pending.label), false, Instant::now()),
                        Err(err) => (format!("{}: {err:#}", pending.label), true, Instant::now()),
                    });
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending.swap_remove(i);
                    self.notice =
                        Some(("Session worker disconnected".into(), true, Instant::now()));
                }
                Err(TryRecvError::Empty) => i += 1,
            }
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, error, at)| !*error && at.elapsed() > NOTICE_LIFE)
        {
            self.notice = None;
        }
    }

    fn finished(&mut self, pending: &Pending, result: &serde_json::Value) {
        if pending.label == "Saving capture" {
            self.shutter = Animation::new(true)
                .duration(Duration::from_millis(320))
                .go(false, self.now);
        }
        let Some(target) = &pending.target else {
            return;
        };
        if self.address.trim() == target {
            self.address.clear();
        }
        let Ok(camera) = serde_json::from_value::<CameraInfo>(result["connected"].clone()) else {
            return;
        };
        let detail = match &camera.address {
            Some(address) => format!("{} · {}", camera.transport, redact_address(address)),
            None => format!("{} · S/N {}", camera.transport, camera.serial),
        };
        let mut prefs = Prefs {
            recent: std::mem::take(&mut self.recent),
            ..Prefs::default()
        };
        prefs.remember(Recent {
            target: target.clone(),
            label: camera.model,
            detail,
            transport: camera.transport,
        });
        self.recent = prefs.recent;
    }

    fn tick(&mut self) -> Task<Message> {
        self.poll();
        self.snapshot = self.handle.snapshot();
        let camera_id = self
            .snapshot
            .connected
            .as_ref()
            .map(|camera| camera.id.clone());
        if camera_id != self.observed_camera {
            self.observed_camera = camera_id;
            self.edits.clear();
            self.edit_sources.clear();
            self.frame_id = None;
            self.shown = None;
            self.frame_meta = None;
            self.display_error = None;
            self.gpu.retire(MAIN_VIEW);
        }
        // Keep untouched editors in sync with CLI/agent changes without destroying drafts.
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
        if let Some(status) = &self.snapshot.auto
            && !self.balance_dragging
            && !self.auto_busy()
        {
            self.balance = status.balance;
        }
        if self.overview() {
            self.update_previews();
            if self.frame_id.take().is_some() {
                self.gpu.retire(MAIN_VIEW);
            }
        } else {
            self.update_frame();
            for key in self.previews.drain().map(|(key, _)| key) {
                self.gpu.retire(&key);
            }
        }
        self.capture_to.tick();
        self.record_to.tick();
        self.save_prefs();
        self.screenshot_tick()
    }

    fn update_previews(&mut self) {
        let snapshot = &self.snapshot;
        let gpu = &self.gpu;
        self.previews.retain(|id, _| {
            let kept = snapshot.cameras.iter().any(|camera| &camera.info.id == id);
            if !kept {
                gpu.retire(id);
            }
            kept
        });
        for camera in &snapshot.cameras {
            let Some(id) = self.handle.latest_frame_id_for(&camera.info.id) else {
                continue;
            };
            let preview = self.previews.entry(camera.info.id.clone()).or_default();
            if preview.frame_id == Some(id) {
                continue;
            }
            let Some(frame) = self.handle.latest_frame_for(&camera.info.id) else {
                continue;
            };
            preview.frame_id = Some(frame.id);
            let meta = (frame.id, frame.width, frame.height, frame.pixel_format);
            let shown =
                preview::present(gpu, &mut preview.shown, &camera.info.id, frame, |frame| {
                    frame::preview_rgba(frame, 640, 480)
                });
            match shown {
                Ok(()) => {
                    preview.meta = Some(meta);
                    preview.error = None;
                }
                Err(error) => preview.error = Some(error.to_string()),
            }
        }
    }

    fn update_frame(&mut self) {
        let Some(id) = self.handle.latest_frame_id() else {
            return;
        };
        if Some(id) == self.frame_id {
            return;
        }
        let Some(frame) = self.handle.latest_frame() else {
            return;
        };
        self.frame_id = Some(frame.id);
        let meta = (
            frame.id,
            frame.width,
            frame.height,
            frame.pixel_format,
            frame.timestamp_ns,
        );
        let shown = frame::sampled_histogram(&frame).and_then(|histogram| {
            preview::present(&self.gpu, &mut self.shown, MAIN_VIEW, frame, |frame| {
                // Decode straight into RGBA bytes: one pass, one allocation.
                let pixels = frame::convert(frame, |[r, g, b]| [r, g, b, 255])?;
                Ok((frame.width, frame.height, pixels.into_flattened()))
            })?;
            Ok(histogram)
        });
        match shown {
            Ok(histogram) => {
                self.histogram = histogram;
                self.frame_meta = Some(meta);
                self.display_error = None;
            }
            Err(err) => self.display_error = Some(err.to_string()),
        }
    }

    fn discover(&mut self) {
        self.send(
            "Discovering cameras",
            SessionCommand::Discover {
                timeout_ms: 700,
                simulated: self.include_simulator,
            },
        );
    }

    fn connect(&mut self, camera: String) {
        self.edits.clear();
        self.edit_sources.clear();
        self.send(
            "Connecting camera",
            SessionCommand::Connect {
                camera: camera.clone(),
                timeout_ms: 5000,
            },
        );
        if let Some(pending) = self.pending.last_mut() {
            pending.target = Some(camera);
        }
    }

    fn connecting(&self, id: &str) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.target.as_deref() == Some(id))
    }

    fn toggle_stream(&mut self) {
        if self.pending("Starting stream") || self.pending("Stopping stream") {
            return;
        }
        let streaming = self.snapshot.streaming;
        self.send(
            if streaming {
                "Stopping stream"
            } else {
                "Starting stream"
            },
            if streaming {
                SessionCommand::Stop
            } else {
                SessionCommand::Start
            },
        );
    }

    fn capture(&mut self) {
        if self.pending("Saving capture") {
            return;
        }
        self.send(
            "Saving capture",
            SessionCommand::Capture {
                output: self.output.clone(),
                count: self.count,
                timeout_ms: self.timeout_ms,
                format: self.format.into(),
                storage: self.storage_policy(),
                destination: self.capture_to.selected.clone(),
            },
        );
    }

    fn toggle_auto(&mut self) {
        if self.auto_busy() {
            return;
        }
        if self.snapshot.auto.is_some() {
            self.send(
                "Switching to manual",
                SessionCommand::Manual { revert: false },
            );
        } else {
            self.send(
                "Enabling auto mode",
                SessionCommand::Auto {
                    balance: Some(self.balance),
                },
            );
        }
    }

    fn select_relative_camera(&mut self, step: isize) {
        let count = self.snapshot.cameras.len();
        if count < 2 {
            return;
        }
        let current = self
            .snapshot
            .cameras
            .iter()
            .position(|camera| self.snapshot.active_camera.as_ref() == Some(&camera.info.id))
            .unwrap_or(0);
        let next = (current as isize + step).rem_euclid(count as isize) as usize;
        let camera = self.snapshot.cameras[next].info.id.clone();
        self.send("Selecting camera", SessionCommand::Select { camera });
    }

    fn zoom_by(&mut self, factor: f32) {
        let from = if self.fit {
            self.shown.as_ref().map_or(1.0, |shown| {
                preview::fit_scale(self.stage.get(), shown.size())
            })
        } else {
            self.zoom
        };
        self.fit = false;
        self.zoom = (from * factor).clamp(0.1, 8.0);
    }

    fn storage_policy(&self) -> StoragePolicy {
        StoragePolicy {
            max_bytes: (self.quota_gib * 1024.0 * 1024.0 * 1024.0).round() as u64,
            max_files: self.quota_files,
            max_age_seconds: self
                .retention_enabled
                .then(|| self.retention_days.saturating_mul(86400)),
            on_full: self.quota_action.into(),
        }
    }

    fn session_command(&self) -> String {
        format!("capturefab --session {} status", shell_quote(&self.session))
    }

    fn copy_session_command(&mut self) -> Task<Message> {
        self.notice = Some(("Session command copied".into(), false, Instant::now()));
        iced::clipboard::write(self.session_command())
    }

    fn num_text(&self, key: Num) -> String {
        match key {
            Num::Count => self.count.to_string(),
            Num::Timeout => self.timeout_ms.to_string(),
            Num::Delay => self.schedule_delay_seconds.to_string(),
            Num::Interval => number(self.schedule_interval_seconds),
            Num::Fps => number(self.forward_fps),
            Num::FileMib => number(self.forward_file_mib),
            Num::QuotaGib => number(self.quota_gib),
            Num::QuotaFiles => self.quota_files.to_string(),
            Num::RetentionDays => self.retention_days.to_string(),
        }
    }

    /// Apply a typed number when it parses within range; report whether it did.
    fn set_num(&mut self, key: Num, text: &str) -> bool {
        if !num_valid(key, text) {
            return false;
        }
        let text = text.trim();
        let (int, float) = (text.parse::<u64>().ok(), text.parse::<f64>().ok());
        match (key, int, float) {
            (Num::Count, Some(v), _) => {
                let previous = self.count;
                self.count = v as u32;
                if previous == 1 && self.count > 1 && self.output == "capture.png" {
                    self.output = "captures".into();
                } else if previous > 1 && self.count == 1 && self.output == "captures" {
                    self.output = "capture.png".into();
                }
            }
            (Num::Timeout, Some(v), _) => self.timeout_ms = v,
            (Num::Delay, Some(v), _) => self.schedule_delay_seconds = v,
            (Num::QuotaFiles, Some(v), _) => self.quota_files = v as u32,
            (Num::RetentionDays, Some(v), _) => self.retention_days = v,
            (Num::Interval, _, Some(v)) => self.schedule_interval_seconds = v,
            (Num::Fps, _, Some(v)) => self.forward_fps = v,
            (Num::FileMib, _, Some(v)) => self.forward_file_mib = v,
            (Num::QuotaGib, _, Some(v)) => self.quota_gib = v,
            _ => return false,
        }
        true
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        self.now = Instant::now();
        let task = self.handle_message(message);
        self.sync_animations();
        task
    }

    fn handle_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => return self.tick(),
            Message::Frame => {}
            Message::SystemTheme(mode) => self.system_dark = mode == theme::Mode::Dark,
            Message::Key { chord, captured } => return self.shortcut(chord, !captured),
            Message::CycleAppearance => {
                self.appearance = match self.appearance {
                    Appearance::System => Appearance::Light,
                    Appearance::Light => Appearance::Dark,
                    Appearance::Dark => Appearance::System,
                }
            }
            Message::Help(open) => self.help_open = open,
            Message::DragWindow => return window::latest().and_then(window::drag),
            Message::Discover => self.discover(),
            Message::IncludeSimulator(value) => {
                self.include_simulator = value;
                self.discover();
            }
            Message::TrySimulator => {
                self.include_simulator = true;
                self.connect("sim:0".into());
            }
            Message::EnterAddress => return focus_address(),
            Message::Forget(target) => self.recent.retain(|recent| recent.target != target),
            Message::CameraRow(id) => {
                if !self.snapshot.cameras.iter().any(|c| c.info.id == id) {
                    self.connect(id);
                } else if self.snapshot.active_camera.as_ref() != Some(&id) {
                    self.send("Selecting camera", SessionCommand::Select { camera: id });
                }
            }
            Message::Disconnect(id) => {
                self.send_to(&id, "Disconnecting", SessionCommand::Disconnect)
            }
            Message::Address(value) => self.address = value,
            Message::ConnectAddress => {
                let address = self.address.trim().to_owned();
                if !address.is_empty() {
                    self.connect(address);
                }
            }
            Message::ToggleStream => self.toggle_stream(),
            Message::Capture => self.capture(),
            Message::Focus(focus) => self.focus_camera = focus,
            Message::Select(camera) => {
                self.send("Selecting camera", SessionCommand::Select { camera })
            }
            Message::StreamCamera(id, start) => self.send_to(
                &id,
                if start {
                    "Starting stream"
                } else {
                    "Stopping stream"
                },
                if start {
                    SessionCommand::Start
                } else {
                    SessionCommand::Stop
                },
            ),
            Message::AllStreams(start) => {
                for id in self
                    .snapshot
                    .cameras
                    .iter()
                    .filter(|camera| camera.streaming != start)
                    .map(|camera| camera.info.id.clone())
                    .collect::<Vec<_>>()
                {
                    self.send_to(
                        &id,
                        if start {
                            "Starting streams"
                        } else {
                            "Stopping streams"
                        },
                        if start {
                            SessionCommand::Start
                        } else {
                            SessionCommand::Stop
                        },
                    );
                }
            }
            Message::AllAuto(auto) => {
                for id in self
                    .snapshot
                    .cameras
                    .iter()
                    .filter(|camera| camera.auto.is_none() == auto)
                    .map(|camera| camera.info.id.clone())
                    .collect::<Vec<_>>()
                {
                    self.send_to(
                        &id,
                        if auto {
                            "Enabling auto mode"
                        } else {
                            "Switching to manual"
                        },
                        if auto {
                            SessionCommand::Auto {
                                balance: Some(self.balance),
                            }
                        } else {
                            SessionCommand::Manual { revert: false }
                        },
                    );
                }
            }
            Message::Fit => self.fit = true,
            Message::Actual => {
                self.fit = false;
                self.zoom = 1.0;
            }
            Message::Zoom(factor) => self.zoom_by(factor),
            Message::ToggleHistogram => self.histogram_open = !self.histogram_open,
            Message::ToggleActivity => self.logs_open = !self.logs_open,
            Message::CopyLog => {
                return iced::clipboard::write(
                    self.snapshot
                        .logs
                        .iter()
                        .map(|l| format!("{} [{}] {}", l.time, l.level, l.message))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            Message::CopySessionCommand => return self.copy_session_command(),
            Message::Copy(value) => {
                self.notice = Some(("Copied".into(), false, Instant::now()));
                return iced::clipboard::write(value);
            }
            Message::Tab(tab) => self.tab = tab,
            Message::Search(value) => self.search = value,
            Message::RefreshFeatures => {
                self.edits.clear();
                self.edit_sources.clear();
                self.send("Refreshing features", SessionCommand::Features);
            }
            Message::Auto(on) => {
                if !self.auto_busy() && on != self.snapshot.auto.is_some() {
                    self.toggle_auto();
                }
            }
            Message::Balance(value) => {
                self.balance = value;
                self.balance_dragging = true;
            }
            Message::BalanceReleased => {
                self.balance_dragging = false;
                if self
                    .snapshot
                    .auto
                    .as_ref()
                    .is_some_and(|status| status.balance != self.balance)
                {
                    self.send(
                        "Updating auto balance",
                        SessionCommand::Auto {
                            balance: Some(self.balance),
                        },
                    );
                }
            }
            Message::ToggleAutoChanges => self.auto_changes_open = !self.auto_changes_open,
            Message::Draft(feature, value) => {
                self.edits.insert(feature, value);
            }
            Message::Commit(feature) => {
                let value = self
                    .edits
                    .get(&feature)
                    .cloned()
                    .or_else(|| self.edit_sources.get(&feature).cloned())
                    .unwrap_or_default();
                self.send(
                    format!("Setting {feature}"),
                    SessionCommand::Set { feature, value },
                );
            }
            Message::Set(feature, value) => self.send(
                format!("Setting {feature}"),
                SessionCommand::Set { feature, value },
            ),
            Message::Execute(feature) => {
                let command = match feature.as_str() {
                    "AcquisitionStart" => SessionCommand::Start,
                    "AcquisitionStop" => SessionCommand::Stop,
                    _ => SessionCommand::Execute {
                        feature: feature.clone(),
                    },
                };
                self.send(format!("Executing {feature}"), command);
            }
            Message::Num(key, value) => {
                if self.set_num(key, &value) {
                    self.drafts.remove(&key);
                    // Keep what was typed (e.g. "1." on the way to "1.5") visible.
                    if self.num_text(key) != value.trim() {
                        self.drafts.insert(key, value);
                    }
                } else {
                    self.drafts.insert(key, value);
                }
            }
            Message::Format(format) => {
                let previous = std::mem::replace(&mut self.format, format.value);
                let mut path = PathBuf::from(&self.output);
                if path.extension().and_then(|extension| extension.to_str()) == Some(previous) {
                    path.set_extension(self.format);
                    self.output = path.to_string_lossy().into_owned();
                }
            }
            Message::ScheduleEnabled(value) => self.schedule_enabled = value,
            Message::Schedule => self.send(
                "Scheduling capture",
                SessionCommand::Schedule {
                    output: self.output.clone(),
                    count: self.count,
                    timeout_ms: self.timeout_ms,
                    format: self.format.into(),
                    first_at_ms: epoch_ms()
                        .saturating_add(self.schedule_delay_seconds.saturating_mul(1000)),
                    interval_ms: (self.schedule_interval_seconds * 1000.0).round() as u64,
                    storage: self.storage_policy(),
                    destination: self.capture_to.selected.clone(),
                },
            ),
            Message::ToggleStorage => self.storage_open = !self.storage_open,
            Message::OnFull(choice) => self.quota_action = choice.value,
            Message::RetentionEnabled(value) => self.retention_enabled = value,
            Message::RefreshJobs => self.send("Refreshing capture jobs", SessionCommand::Jobs),
            Message::CancelJob(id) => {
                self.send("Cancelling capture job", SessionCommand::CancelJob { id })
            }
            Message::Codec(codec) => self.forward_codec = codec.value,
            Message::Encoder(value) => self.forward_encoder = value,
            Message::Bitrate(value) => self.forward_bitrate = value,
            Message::StartForward => {
                if !self.pending("Starting forwarding") {
                    self.send(
                        "Starting forwarding",
                        SessionCommand::Forward {
                            output: self.forward_output.trim().into(),
                            codec: self.forward_codec.into(),
                            encoder: self.forward_encoder.trim().into(),
                            fps: self.forward_fps,
                            bitrate: self.forward_bitrate.trim().into(),
                            storage: self.storage_policy(),
                            max_file_bytes: (self.forward_file_mib * 1024.0 * 1024.0).round()
                                as u64,
                            destination: self.record_to.selected.clone(),
                        },
                    );
                }
            }
            Message::StopForward => self.send("Stopping forwarding", SessionCommand::StopForward),
            Message::CapturePicker(message) => self.capture_to.update(message, &mut self.output),
            Message::RecordPicker(message) => {
                self.record_to.update(message, &mut self.forward_output)
            }
            Message::Screenshot(shot) => {
                if let Some(request) = &mut self.screenshot {
                    let path = request.path.clone();
                    let (sender, receiver) = mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = sender.send(save_screenshot(&shot, &path));
                    });
                    request.save = Some(receiver);
                }
            }
        }
        Task::none()
    }

    fn shortcut(&mut self, chord: Chord, keyboard_free: bool) -> Task<Message> {
        let modal = self.help_open || self.capture_to.manager_open || self.record_to.manager_open;
        let Some(action) = Action::find(&chord, keyboard_free, Os::CURRENT) else {
            return Task::none();
        };
        let connected = self.snapshot.connected.is_some();
        match action {
            Action::Overview if self.help_open => self.help_open = false,
            Action::Overview if self.capture_to.manager_open => {
                self.capture_to
                    .update(destinations::Message::CloseManager, &mut self.output);
            }
            Action::Overview if self.record_to.manager_open => {
                self.record_to.update(
                    destinations::Message::CloseManager,
                    &mut self.forward_output,
                );
            }
            Action::Help => self.help_open = !self.help_open,
            Action::CopySessionCommand => return self.copy_session_command(),
            Action::Fullscreen => {
                return window::latest().and_then(|id| {
                    window::mode(id).then(move |mode| {
                        window::set_mode(
                            id,
                            if mode == window::Mode::Fullscreen {
                                window::Mode::Windowed
                            } else {
                                window::Mode::Fullscreen
                            },
                        )
                    })
                });
            }
            Action::CloseWindow => return window::latest().and_then(window::close),
            // Sheets take the keyboard until they close.
            _ if modal => {}
            Action::Discover => self.discover(),
            Action::ConnectAddress => return focus_address(),
            Action::NextCamera => self.select_relative_camera(1),
            Action::PreviousCamera => self.select_relative_camera(-1),
            Action::FocusCamera if self.overview() => self.focus_camera = true,
            Action::Overview if self.snapshot.cameras.len() > 1 => self.focus_camera = false,
            Action::ToggleStream if connected => self.toggle_stream(),
            Action::Capture if connected => self.capture(),
            Action::ToggleAuto if connected => self.toggle_auto(),
            Action::SearchFeatures => {
                self.tab = Tab::Features;
                return Task::batch([
                    operation::focus("feature-search"),
                    operation::select_all("feature-search"),
                ]);
            }
            Action::FeaturesTab => self.tab = Tab::Features,
            Action::CaptureTab => self.tab = Tab::Capture,
            Action::ForwardTab => self.tab = Tab::Forward,
            Action::ZoomIn => self.zoom_by(1.25),
            Action::ZoomOut => self.zoom_by(1.0 / 1.25),
            Action::ZoomFit => self.fit = true,
            Action::ZoomActual => {
                self.fit = false;
                self.zoom = 1.0;
            }
            Action::ToggleActivity => self.logs_open = !self.logs_open,
            // Pressed where it does not apply, e.g. Space with no camera connected.
            _ => {}
        }
        Task::none()
    }

    fn screenshot_tick(&mut self) -> Task<Message> {
        let Some(mut request) = self.screenshot.take() else {
            return Task::none();
        };
        let mut task = Task::none();
        let finish = |request: &ScreenshotRequest, error: Option<String>| {
            *request
                .outcome
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = error;
            iced::exit()
        };
        if let Some(receiver) = &request.save {
            match receiver.try_recv() {
                Ok(result) => {
                    return finish(
                        &request,
                        result
                            .err()
                            .map(|error| format!("Save renderer screenshot: {error:#}")),
                    );
                }
                Err(TryRecvError::Disconnected) => {
                    return finish(&request, Some("Screenshot writer stopped".into()));
                }
                Err(TryRecvError::Empty) => {}
            }
        } else if !request.requested
            && self.snapshot.cameras.len() == request.cameras as usize
            && self.pending.is_empty()
        {
            if request.streams_started.is_none() {
                for id in self
                    .snapshot
                    .cameras
                    .iter()
                    .map(|camera| camera.info.id.clone())
                    .collect::<Vec<_>>()
                {
                    self.send_to(&id, "Starting demo stream", SessionCommand::Start);
                }
                request.streams_started = Some(Instant::now());
            } else if request
                .streams_started
                .is_some_and(|started| started.elapsed() > Duration::from_millis(1600))
                && self
                    .snapshot
                    .cameras
                    .iter()
                    .all(|camera| camera.streaming && camera.frames >= 3)
            {
                task = window::latest()
                    .and_then(window::screenshot)
                    .map(Message::Screenshot);
                request.requested = true;
            }
        }
        if request.started.elapsed() > Duration::from_secs(30) {
            return finish(
                &request,
                Some(format!(
                    "Renderer screenshot timed out waiting for {} camera(s) and a complete painted frame",
                    request.cameras
                )),
            );
        }
        self.screenshot = Some(request);
        task
    }
}

// Views

impl Workbench {
    fn view(&self) -> Element<'_, Message> {
        let dark = self.dark();
        let p = Palette::of(dark);
        let body: Element<'_, Message> = row![
            self.sidebar(p),
            rule::vertical(1).style(style::line),
            container(self.main(p))
                .width(Fill)
                .height(Fill)
                .style(style::base),
            rule::vertical(1).style(style::line),
            self.inspector(p),
        ]
        .into();
        let t = self.sheet.interpolate(0.0f32, 1.0, self.now);
        let body = if self.help_open {
            modal(body, self.help(p), Message::Help(false), t)
        } else {
            body
        };
        let body = if self.capture_to.manager_open {
            modal(
                body,
                self.capture_to.manager(dark).map(Message::CapturePicker),
                Message::CapturePicker(destinations::Message::CloseManager),
                t,
            )
        } else {
            body
        };
        if self.record_to.manager_open {
            modal(
                body,
                self.record_to.manager(dark).map(Message::RecordPicker),
                Message::RecordPicker(destinations::Message::CloseManager),
                t,
            )
        } else {
            body
        }
    }

    /// Space at the top of a column; on macOS it is also the window's drag handle.
    fn titlebar<'a>(&self, content: Element<'a, Message>) -> Element<'a, Message> {
        let top = column![space().height(TOP), content];
        if cfg!(target_os = "macos") {
            mouse_area(top).on_press(Message::DragWindow).into()
        } else {
            top.into()
        }
    }

    fn sidebar(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut devices = self.snapshot.devices.clone();
        for camera in &self.snapshot.cameras {
            if !devices.iter().any(|device| device.id == camera.info.id) {
                devices.push(camera.info.clone());
            }
        }
        let discovering = self.pending("Discovering cameras");
        let brand = row![
            icon(Icon::Mark, 20.0, p.accent),
            text("Capturefab").size(15).font(style::BOLD),
        ]
        .spacing(9)
        .align_y(Alignment::Center);
        let header = row![
            text("Cameras")
                .size(style::SMALL)
                .font(style::SEMIBOLD)
                .color(p.secondary),
            text(if discovering {
                "Searching…".to_string()
            } else {
                devices.len().to_string()
            })
            .size(style::SMALL)
            .color(p.tertiary),
            space::horizontal(),
            tip(
                button(icon(
                    Icon::Refresh,
                    14.0,
                    if discovering {
                        fade(p.accent, self.pulse())
                    } else {
                        p.secondary
                    },
                ))
                .padding(5)
                .style(style::plain)
                .on_press_maybe((!discovering).then_some(Message::Discover)),
                Action::Discover.hint("Find GigE Vision and USB3 Vision devices", Os::CURRENT),
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let mut list = column![].spacing(2);
        if devices.is_empty() {
            list = list.push(
                container(
                    text(if discovering {
                        "Looking for cameras…"
                    } else {
                        "No cameras found"
                    })
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .padding([8, 10]),
            );
        }
        let recent: Vec<&Recent> = self
            .recent
            .iter()
            .filter(|recent| {
                !devices.iter().any(|device| {
                    device.id == recent.target
                        || device.address.as_deref() == Some(recent.target.as_str())
                })
            })
            .collect();
        for camera in devices {
            let active = self.snapshot.active_camera.as_ref() == Some(&camera.id);
            let state = self
                .snapshot
                .cameras
                .iter()
                .find(|c| c.info.id == camera.id);
            let connecting = self.connecting(&camera.id);
            let color = match state {
                Some(state) if state.streaming => fade(p.live, self.pulse()),
                Some(_) => p.accent,
                None if connecting => fade(p.accent, self.pulse()),
                None => p.tertiary,
            };
            let detail = if connecting {
                "Connecting…".to_string()
            } else {
                match &camera.address {
                    Some(address) => {
                        format!("{} · {}", camera.transport, redact_address(address))
                    }
                    None => format!("{} · S/N {}", camera.transport, camera.serial),
                }
            };
            let mut line = row![
                dot(color, 8.0),
                column![
                    clipped(
                        text(camera.model.clone())
                            .size(style::BODY)
                            .font(if active {
                                style::SEMIBOLD
                            } else {
                                style::MEDIUM
                            })
                    ),
                    clipped(
                        text(detail)
                            .size(style::CAPTION)
                            .color(p.secondary)
                            .wrapping(text::Wrapping::None)
                    ),
                ]
                .spacing(1)
                .width(Fill),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            let hint = if state.is_some() {
                format!("{} {} · {}", camera.vendor, camera.model, "connected")
            } else {
                format!("Connect to {} {}", camera.vendor, camera.model)
            };
            if state.is_some() {
                line = line.push(tip(
                    button(icon(Icon::Eject, 13.0, p.tertiary))
                        .padding(4)
                        .style(style::plain)
                        .on_press(Message::Disconnect(camera.id.clone())),
                    "Disconnect",
                ));
            } else if !connecting {
                line = line.push(icon(Icon::Plug, 13.0, p.tertiary));
            }
            list = list.push(tip(
                button(line)
                    .width(Fill)
                    .padding([7, 10])
                    .style(style::row(active))
                    .on_press(Message::CameraRow(camera.id.clone())),
                hint,
            ));
        }
        if !recent.is_empty() {
            list = list.push(
                container(
                    text("Recent")
                        .size(style::SMALL)
                        .font(style::SEMIBOLD)
                        .color(p.secondary),
                )
                .padding(iced::Padding {
                    top: 14.0,
                    right: 4.0,
                    bottom: 4.0,
                    left: 4.0,
                }),
            );
        }
        for entry in recent {
            let connecting = self.connecting(&entry.target);
            let line = row![
                icon(
                    Icon::Recent,
                    13.0,
                    if connecting {
                        fade(p.accent, self.pulse())
                    } else {
                        p.tertiary
                    }
                ),
                column![
                    clipped(text(entry.label.clone()).size(style::BODY)),
                    clipped(
                        text(if connecting {
                            "Connecting…".to_string()
                        } else {
                            entry.detail.clone()
                        })
                        .size(style::CAPTION)
                        .color(p.secondary)
                        .wrapping(text::Wrapping::None)
                    ),
                ]
                .spacing(1)
                .width(Fill),
                tip(
                    button(icon(Icon::Close, 11.0, p.tertiary))
                        .padding(5)
                        .style(style::plain)
                        .on_press(Message::Forget(entry.target.clone())),
                    "Forget",
                ),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            list = list.push(tip(
                button(line)
                    .width(Fill)
                    .padding([7, 10])
                    .style(style::row(false))
                    .on_press(Message::CameraRow(entry.target.clone())),
                format!("Reconnect {}", entry.label),
            ));
        }
        let appearance = match self.appearance {
            Appearance::System => (Icon::Contrast, "Appearance: match system"),
            Appearance::Light => (Icon::Sun, "Appearance: light"),
            Appearance::Dark => (Icon::Moon, "Appearance: dark"),
        };
        let footer = column![
            text_input("Add camera, IP or stream URL", &self.address)
                .id("connect-address")
                .icon(icon::input_icon(Icon::Plus))
                .on_input(Message::Address)
                .on_submit(Message::ConnectAddress)
                .size(style::BODY)
                .padding([7, 10])
                .style(style::input),
            text("GigE · USB3 · RTSP · HTTP · RTMP · native")
                .size(style::CAPTION)
                .color(p.tertiary),
            checkbox(
                "Include simulated cameras",
                self.include_simulator,
                Message::IncludeSimulator,
            ),
            row![
                tip(
                    button(icon(appearance.0, 15.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::CycleAppearance),
                    appearance.1,
                ),
                tip(
                    button(icon(Icon::Copy, 15.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::CopySessionCommand),
                    Action::CopySessionCommand.hint(
                        "Copy a command that controls this visible session",
                        Os::CURRENT,
                    ),
                ),
                tip(
                    button(icon(Icon::Help, 15.0, p.secondary))
                        .padding(6)
                        .style(style::plain)
                        .on_press(Message::Help(true)),
                    Action::Help.hint("Keyboard shortcuts and quick guide", Os::CURRENT),
                ),
                space::horizontal(),
                clipped(
                    text(format!("session {}", self.session))
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(p.tertiary)
                ),
            ]
            .spacing(2)
            .align_y(Alignment::Center),
        ]
        .spacing(9);
        container(
            column![
                self.titlebar(container(brand).padding([0, 6]).into()),
                space().height(22),
                container(header).padding([0, 4]),
                space().height(4),
                scrollable(list).height(Fill).style(style::scroll),
                footer,
            ]
            .padding(iced::Padding {
                top: 0.0,
                right: 12.0,
                bottom: 12.0,
                left: 12.0,
            }),
        )
        .width(SIDEBAR)
        .height(Fill)
        .style(style::sidebar)
        .into()
    }

    fn main(&self, p: &'static Palette) -> Element<'_, Message> {
        let content = if self.overview() {
            self.overview_view(p)
        } else {
            self.single_view(p)
        };
        let mut main = column![content];
        let activity = self.activity_slide.interpolate(0.0f32, 171.0, self.now);
        if activity > 0.5 {
            main = main.push(container(self.activity(p)).height(activity).clip(true));
        }
        main.push(self.toolbar(p)).into()
    }

    fn header<'a>(
        &self,
        marker: Element<'a, Message>,
        title: String,
        subtitle: Element<'a, Message>,
        actions: Element<'a, Message>,
    ) -> Element<'a, Message> {
        self.titlebar(
            container(
                row![
                    column![
                        row![
                            marker,
                            clipped(text(title).size(style::TITLE).font(style::BOLD))
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                        subtitle,
                    ]
                    .spacing(4)
                    .width(Fill),
                    actions,
                ]
                .spacing(16)
                .align_y(Alignment::Center),
            )
            .padding([0.0, GUTTER])
            .into(),
        )
    }

    fn single_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let (status, color) = if snapshot.streaming {
            ("Streaming", p.live)
        } else if connected {
            ("Ready", p.accent_text)
        } else {
            ("No camera connected", p.secondary)
        };
        let mut subtitle = row![
            text(status)
                .size(style::BODY)
                .font(style::MEDIUM)
                .color(color)
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if connected {
            let (label, lost) = snapshot
                .transport
                .as_ref()
                .map_or(("dropped", snapshot.dropped), |stats| {
                    ("lost", transport_loss(stats))
                });
            let mut parts = vec![
                format!("{:.1} fps", snapshot.fps),
                format!("{} frames", grouped(snapshot.frames)),
            ];
            parts.push(format!("{lost} {label}"));
            for part in parts {
                subtitle = subtitle
                    .push(text("·").size(style::BODY).color(p.tertiary))
                    .push(text(part).size(style::BODY).color(p.secondary));
            }
            if lost > 0 {
                subtitle = subtitle.push(dot(p.warn, 6.0));
            }
            if snapshot.auto.is_some() {
                subtitle = subtitle
                    .push(text("·").size(style::BODY).color(p.tertiary))
                    .push(text("Auto").size(style::BODY).color(p.accent_text));
            }
        }
        let mut actions = row![].spacing(8).align_y(Alignment::Center);
        if snapshot.cameras.len() > 1 {
            actions = actions.push(tip(
                button(
                    row![
                        icon(Icon::Grid, 14.0, p.text),
                        text("All cameras").size(style::BODY)
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                )
                .padding([7, 12])
                .style(style::secondary)
                .on_press(Message::Focus(false)),
                Action::Overview.hint("Back to all cameras", Os::CURRENT),
            ));
        }
        if connected {
            actions = actions.push(tip(
                stream_button(
                    snapshot.streaming,
                    if snapshot.streaming { "Stop" } else { "Start" },
                    Some(Message::ToggleStream),
                    p,
                ),
                Action::ToggleStream.hint("Start or stop acquisition", Os::CURRENT),
            ));
        }
        let title = snapshot
            .connected
            .as_ref()
            .map_or("Live Preview".to_string(), |camera| camera.model.clone());
        let marker = icon(
            Icon::Camera,
            24.0,
            if connected { p.accent } else { p.tertiary },
        );
        let header = self.header(marker, title, subtitle.into(), actions.into());
        let stage: Element<'_, Message> = match &self.shown {
            Some(shown) => {
                let mut layers = stack![responsive(move |size| {
                    self.stage.set(size);
                    shown.view(&self.gpu, (!self.fit).then_some(self.zoom))
                })];
                if !snapshot.streaming {
                    layers = layers.push(container(last_frame()).padding(12));
                }
                layers.into()
            }
            None if !connected => self.welcome(p),
            None => {
                let mut ready = column![
                    icon(Icon::Camera, 46.0, p.tertiary),
                    space().height(6),
                    text(if snapshot.streaming {
                        "Waiting for the first frame…"
                    } else {
                        "Ready for your first frame"
                    })
                    .size(18)
                    .font(style::SEMIBOLD),
                ]
                .spacing(6)
                .align_x(Alignment::Center);
                if !snapshot.streaming {
                    ready = ready.push(space().height(8)).push(
                        row![
                            stream_button(false, "Start stream", Some(Message::ToggleStream), p),
                            button(text("Capture one frame").size(style::BODY))
                                .padding([7, 12])
                                .style(style::link)
                                .on_press_maybe(
                                    (!self.pending("Saving capture")).then_some(Message::Capture)
                                ),
                        ]
                        .spacing(10)
                        .align_y(Alignment::Center),
                    );
                }
                center(ready).into()
            }
        };
        let flash = self.shutter.interpolate(0.0f32, 0.55, self.now);
        let stage: Element<'_, Message> = if flash > 0.0 {
            stack![
                stage,
                container(space().width(Fill).height(Fill)).style(move |_| {
                    container::Style::default().background(Color {
                        a: flash,
                        ..Color::WHITE
                    })
                }),
            ]
            .into()
        } else {
            stage
        };
        let os = Os::CURRENT;
        let zoom_actual = !self.fit && (self.zoom - 1.0).abs() < 0.01;
        let mut controls = row![
            container(
                row![
                    tip(
                        segment("Fit", self.fit, Message::Fit),
                        Action::ZoomFit.hint("Zoom to fit", os)
                    ),
                    tip(
                        segment("1:1", zoom_actual, Message::Actual),
                        Action::ZoomActual.hint("Actual pixels", os)
                    ),
                ]
                .spacing(2)
            )
            .padding(2)
            .style(style::segment_track),
            tip(
                icon_button(Icon::Minus, 13.0, p.secondary, Message::Zoom(1.0 / 1.25)),
                Action::ZoomOut.hint("Zoom out", os)
            ),
            tip(
                icon_button(Icon::Plus, 13.0, p.secondary, Message::Zoom(1.25)),
                Action::ZoomIn.hint("Zoom in", os)
            ),
            tip(
                icon_button(
                    Icon::Chart,
                    14.0,
                    if self.histogram_open {
                        p.accent
                    } else {
                        p.secondary
                    },
                    Message::ToggleHistogram,
                ),
                "Luminance histogram"
            ),
            space::horizontal(),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if let Some((id, width, height, format, _)) = self.frame_meta {
            controls = controls.push(
                text(format!(
                    "{width} × {height}  ·  {}  ·  #{id}",
                    pixel_name(format)
                ))
                .size(style::CAPTION)
                .font(style::MONO)
                .color(p.secondary),
            );
        }
        let mut content = column![
            header,
            space().height(18),
            container(stage)
                .width(Fill)
                .height(Fill)
                .clip(true)
                .style(style::stage),
        ];
        if self.shown.is_some() {
            content = content.push(space().height(10)).push(controls);
        }
        let histogram = self.histogram_slide.interpolate(0.0f32, 52.0, self.now);
        if histogram > 0.5 && self.shown.is_some() {
            content = content.push(
                container(
                    column![
                        space().height(8),
                        iced::widget::canvas(preview::Histogram {
                            bins: self.histogram,
                            color: Color {
                                a: 0.55,
                                ..p.accent
                            },
                        })
                        .width(Fill)
                        .height(44),
                    ]
                    .height(52),
                )
                .height(histogram)
                .clip(true),
            );
        }
        if let Some(error) = self.display_error.as_ref().or(snapshot.last_error.as_ref()) {
            content = content.push(space().height(6)).push(tip(
                clipped(text(error.clone()).size(style::SMALL).color(p.danger)),
                error.clone(),
            ));
        }
        content
            .push(space().height(12))
            .padding(iced::Padding {
                top: 0.0,
                right: GUTTER,
                bottom: 0.0,
                left: GUTTER,
            })
            .height(Fill)
            .into()
    }

    fn welcome(&self, p: &'static Palette) -> Element<'_, Message> {
        let t = self.welcome.interpolate(0.0f32, 1.0, self.now);
        let discovering = self.pending("Discovering cameras");
        let found: Vec<_> = self
            .snapshot
            .devices
            .iter()
            .filter(|device| {
                !self
                    .snapshot
                    .cameras
                    .iter()
                    .any(|camera| camera.info.id == device.id)
            })
            .take(4)
            .collect();
        let recent: Vec<_> = self
            .recent
            .iter()
            .filter(|recent| {
                !self.snapshot.devices.iter().any(|device| {
                    device.id == recent.target
                        || device.address.as_deref() == Some(recent.target.as_str())
                })
            })
            .take(4 - found.len())
            .collect();
        let subtitle = if discovering {
            "Looking for GigE Vision and USB3 Vision cameras…".to_string()
        } else if found.is_empty() {
            "No cameras found yet. Try the simulator, or enter an address or stream URL.".into()
        } else if found.len() == 1 {
            "Found 1 camera. Click it to connect.".into()
        } else {
            format!("Found {} cameras. Click one to connect.", found.len())
        };
        let card = |kind: Icon, title: String, detail: String, target: String| {
            let connecting = self.connecting(&target);
            button(
                row![
                    icon(
                        kind,
                        20.0,
                        fade(p.accent, t * if connecting { self.pulse() } else { 1.0 })
                    ),
                    column![
                        clipped(
                            text(title)
                                .size(style::BODY)
                                .font(style::MEDIUM)
                                .color(fade(p.text, t))
                        ),
                        clipped(
                            text(if connecting {
                                "Connecting…".to_string()
                            } else {
                                detail
                            })
                            .size(style::CAPTION)
                            .color(fade(p.secondary, t))
                        ),
                    ]
                    .spacing(2)
                    .width(Fill),
                    icon(Icon::ChevronRight, 13.0, fade(p.tertiary, t)),
                ]
                .spacing(12)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([10, 14])
            .style(style::card)
            .on_press(Message::CameraRow(target))
        };
        let mut cards = column![].spacing(8).width(Fill);
        for device in &found {
            let detail = match &device.address {
                Some(address) => format!("{} · {}", device.transport, redact_address(address)),
                None => format!("{} · S/N {}", device.transport, device.serial),
            };
            cards = cards.push(card(
                Icon::transport(device.transport),
                device.model.clone(),
                detail,
                device.id.clone(),
            ));
        }
        if !recent.is_empty() {
            cards = cards.push(
                container(
                    text("Recent")
                        .size(style::SMALL)
                        .font(style::SEMIBOLD)
                        .color(fade(p.secondary, t)),
                )
                .padding(iced::Padding {
                    top: if found.is_empty() { 0.0 } else { 8.0 },
                    left: 2.0,
                    ..iced::Padding::ZERO
                }),
            );
        }
        for entry in &recent {
            cards = cards.push(card(
                Icon::transport(entry.transport),
                entry.label.clone(),
                entry.detail.clone(),
                entry.target.clone(),
            ));
        }
        let simulator_first = found.is_empty() && recent.is_empty() && !discovering;
        let action = |kind: Icon, label: &'static str, on: Option<Message>, primary: bool| {
            button(
                row![
                    icon(kind, 15.0, if primary { Color::WHITE } else { p.secondary }),
                    text(label).size(style::BODY).font(style::MEDIUM),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .padding([7, 12])
            .style(if primary {
                style::primary
            } else {
                style::secondary
            })
            .on_press_maybe(on)
        };
        let os = Os::CURRENT;
        let actions = row![
            tip(
                action(
                    Icon::Refresh,
                    "Search again",
                    (!discovering).then_some(Message::Discover),
                    false
                ),
                Action::Discover.hint("Find GigE Vision and USB3 Vision devices", os),
            ),
            action(
                Icon::Cube,
                "Try a simulated camera",
                (!self.connecting("sim:0")).then_some(Message::TrySimulator),
                simulator_first,
            ),
            tip(
                action(
                    Icon::Plus,
                    "Enter an address",
                    Some(Message::EnterAddress),
                    false
                ),
                Action::ConnectAddress.hint("IP address, RTSP, SRT or HTTP stream URL", os),
            ),
        ]
        .spacing(8)
        .wrap()
        .vertical_spacing(8);
        let mut content = column![
            icon(Icon::Mark, 44.0, fade(p.accent, t)),
            space().height(10),
            text("Connect a camera")
                .size(22)
                .font(style::BOLD)
                .color(fade(p.text, t)),
            text(subtitle)
                .size(style::BODY)
                .color(fade(p.secondary, t))
                .align_x(Alignment::Center),
            space().height(14),
        ]
        .spacing(4)
        .align_x(Alignment::Center)
        .max_width(460);
        if !found.is_empty() || !recent.is_empty() {
            content = content.push(cards).push(space().height(14));
        }
        content = content.push(actions).push(space().height(18)).push(
            text(format!(
                "GigE cameras need an address on this computer's subnet. USB3 cameras need operating system access. {} opens the quick guide.",
                Action::Help.shortcut(os)
            ))
            .size(style::CAPTION)
            .color(fade(p.tertiary, t))
            .align_x(Alignment::Center),
        );
        center(container(content).padding(iced::Padding {
            top: 24.0 * (1.0 - t),
            right: 24.0,
            bottom: 0.0,
            left: 24.0,
        }))
        .clip(true)
        .into()
    }

    fn overview_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let streaming = snapshot.cameras.iter().filter(|c| c.streaming).count();
        let subtitle = text(format!(
            "{} connected · {streaming} streaming",
            snapshot.cameras.len()
        ))
        .size(style::BODY)
        .color(p.secondary);
        let start = snapshot.cameras.iter().any(|camera| !camera.streaming);
        let manual = snapshot.cameras.iter().any(|camera| camera.auto.is_none());
        let actions = row![
            button(text(if manual { "Auto all" } else { "Manual all" }).size(style::BODY))
                .padding([7, 12])
                .style(style::secondary)
                .on_press(Message::AllAuto(manual)),
            stream_button(
                !start,
                if start { "Start all" } else { "Stop all" },
                Some(Message::AllStreams(start)),
                p,
            )
            .width(Length::Shrink),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let header = self.header(
            icon(Icon::Grid, 24.0, p.accent),
            "All Cameras".into(),
            subtitle.into(),
            actions.into(),
        );
        let grid = responsive(move |size| self.grid(size, p));
        column![
            header,
            space().height(18),
            container(grid).width(Fill).height(Fill),
            space().height(6),
            text(format!(
                "Click a preview to inspect its settings · {} / {} switches camera · {} focuses",
                Action::PreviousCamera.shortcut(Os::CURRENT),
                Action::NextCamera.shortcut(Os::CURRENT),
                Action::FocusCamera.shortcut(Os::CURRENT),
            ))
            .size(style::CAPTION)
            .color(p.tertiary),
            space().height(12),
        ]
        .padding(iced::Padding {
            top: 0.0,
            right: GUTTER,
            bottom: 0.0,
            left: GUTTER,
        })
        .height(Fill)
        .into()
    }

    fn grid(&self, size: Size, p: &'static Palette) -> Element<'_, Message> {
        let cameras = &self.snapshot.cameras;
        let count = cameras.len();
        let gap = 14.0;
        let columns = if count == 2 {
            if size.width >= 460.0 { 2 } else { 1 }
        } else {
            ((size.width / 240.0).floor() as usize)
                .max(1)
                .min((count as f32).sqrt().ceil() as usize)
        };
        let rows = count.div_ceil(columns);
        let tile_height = ((size.height - gap * (rows - 1) as f32) / rows as f32).max(230.0);
        let mut grid = column![].spacing(gap);
        for chunk in cameras.chunks(columns) {
            let mut line = row![].spacing(gap).height(tile_height);
            for camera in chunk {
                line = line.push(self.tile(camera, p));
            }
            for _ in chunk.len()..columns {
                line = line.push(space().width(Fill));
            }
            grid = grid.push(line);
        }
        scrollable(grid).style(style::scroll).into()
    }

    fn tile<'a>(
        &'a self,
        camera: &'a crate::session::CameraSnapshot,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let id = &camera.info.id;
        let active = self.snapshot.active_camera.as_ref() == Some(id);
        let preview = self.previews.get(id);
        let picture: Element<'_, Message> = match preview.and_then(|preview| preview.shown.as_ref())
        {
            Some(shown) => {
                let mut layers = stack![shown.view(&self.gpu, None)];
                if !camera.streaming {
                    layers = layers.push(container(last_frame()).padding(9));
                }
                layers.into()
            }
            None => center(
                text(if camera.streaming {
                    "Waiting for a frame…"
                } else {
                    "Ready to stream"
                })
                .size(style::BODY)
                .color(p.secondary),
            )
            .into(),
        };
        let picture = tip(
            button(
                container(picture)
                    .width(Fill)
                    .height(Fill)
                    .clip(true)
                    .style(style::stage),
            )
            .padding(0)
            .width(Fill)
            .height(Fill)
            .style(style::bare)
            .on_press(Message::Select(id.clone())),
            format!(
                "Select {} for settings and capture\nAcquisition worker PID {}",
                camera.info.serial, camera.worker_pid
            ),
        );
        let mut title = row![
            dot(
                if camera.streaming {
                    fade(p.live, self.pulse())
                } else {
                    p.tertiary
                },
                8.0
            ),
            clipped(
                text(camera.info.model.clone())
                    .size(14)
                    .font(style::SEMIBOLD)
            ),
            space::horizontal(),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if active {
            title = title.push(
                text("Selected")
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(p.accent_text),
            );
        }
        let mut stats = row![
            text(format!("{:.1} fps", camera.fps))
                .size(style::SMALL)
                .font(style::MONO)
                .color(if camera.streaming {
                    p.text
                } else {
                    p.secondary
                }),
            text(format!("{} frames", grouped(camera.frames)))
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if let Some(destination) = &camera.forwarding {
            stats = stats.push(tip(
                text("Out")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.accent_text),
                format!("Forwarding to {}", redact_address(destination)),
            ));
        }
        if let Some(auto) = &camera.auto {
            stats = stats.push(tip(
                text("Auto")
                    .size(style::CAPTION)
                    .font(style::SEMIBOLD)
                    .color(p.accent_text),
                format!("Auto mode · {} · balance {:.2}", auto.state, auto.balance),
            ));
        }
        let lost = camera
            .transport
            .as_ref()
            .map_or(camera.dropped, transport_loss);
        if lost > 0 {
            stats = stats.push(
                text(format!("{lost} lost"))
                    .size(style::SMALL)
                    .color(p.warn),
            );
        }
        let mut controls = row![
            button(text(if camera.streaming { "Stop" } else { "Start" }).size(style::SMALL))
                .padding([3, 8])
                .style(style::link)
                .on_press(Message::StreamCamera(id.clone(), !camera.streaming)),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        if !active {
            controls = controls.push(
                button(text("Select").size(style::SMALL))
                    .padding([3, 8])
                    .style(style::plain)
                    .on_press(Message::Select(id.clone())),
            );
        }
        controls = controls.push(space::horizontal());
        if let Some((_, width, height, format)) = preview.and_then(|preview| preview.meta) {
            controls = controls.push(
                text(format!("{width}×{height} {}", pixel_name(format)))
                    .size(style::CAPTION)
                    .color(p.tertiary),
            );
        }
        let mut body = column![picture, title, stats, controls].spacing(8);
        if let Some(error) = preview
            .and_then(|preview| preview.error.as_ref())
            .or(camera.last_error.as_ref())
        {
            body = body.push(tip(
                clipped(text(error.clone()).size(style::CAPTION).color(p.danger)),
                error.clone(),
            ));
        }
        container(body)
            .padding(10)
            .width(Fill)
            .height(Fill)
            .style(style::tile(active))
            .into()
    }

    fn toolbar(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let capturing = self.pending("Saving capture");
        let os = Os::CURRENT;
        let mut left = row![tip(
            button(
                row![
                    icon(
                        Icon::Camera,
                        15.0,
                        if connected { p.text } else { p.tertiary }
                    ),
                    text(if capturing {
                        "Saving…"
                    } else if self.overview() {
                        "Capture selected"
                    } else {
                        "Capture"
                    })
                    .size(style::BODY),
                ]
                .spacing(7)
                .align_y(Alignment::Center),
            )
            .padding([6, 12])
            .style(style::secondary)
            .on_press_maybe((connected && !capturing).then_some(Message::Capture)),
            Action::Capture.hint("Save using the settings in Capture", os),
        ),]
        .spacing(8)
        .align_y(Alignment::Center);
        if self.overview() {
            left = left.push(tip(
                button(text("Focus selected").size(style::BODY))
                    .padding([6, 12])
                    .style(style::plain)
                    .on_press(Message::Focus(true)),
                Action::FocusCamera.hint("Focus selected camera", os),
            ));
        }
        left = left.push(tip(
            button(clipped(
                text(self.capture_to.label(&self.output))
                    .size(style::SMALL)
                    .font(style::MONO)
                    .color(p.secondary),
            ))
            .padding([4, 6])
            .style(style::plain)
            .on_press(Message::Tab(Tab::Capture)),
            Action::CaptureTab.hint("Where captures are saved", os),
        ));
        let status: Element<'_, Message> = if let Some((message, error, at)) = &self.notice {
            let alpha = self.notice_alpha(*at, *error);
            let color = fade(if *error { p.danger } else { p.secondary }, alpha);
            row![
                icon(
                    if *error { Icon::Warning } else { Icon::Check },
                    13.0,
                    fade(if *error { p.danger } else { p.live }, alpha),
                ),
                clipped(text(message.clone()).size(style::SMALL).color(color)),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into()
        } else if let Some(pending) = self.pending.first() {
            row![
                dot(fade(p.accent, self.pulse()), 6.0),
                text(format!("{}…", pending.label))
                    .size(style::SMALL)
                    .color(p.secondary)
            ]
            .spacing(7)
            .align_y(Alignment::Center)
            .into()
        } else if let Some(camera) = &snapshot.connected {
            let mut parts = row![
                text(format!("{} · {}", camera.transport, camera.serial))
                    .size(style::SMALL)
                    .color(p.tertiary)
            ]
            .spacing(10);
            if let Some(stats) = &snapshot.transport {
                parts = parts.push(tip(
                    text(transport_text(stats)).size(style::SMALL).color(
                        if stats.notes.is_empty() {
                            p.tertiary
                        } else {
                            p.warn
                        },
                    ),
                    if stats.notes.is_empty() {
                        "Packet size · resent packets recovered/requested · incomplete or missing frames".into()
                    } else {
                        stats.notes.join("\n")
                    },
                ));
            }
            parts.into()
        } else {
            text("Ready").size(style::SMALL).color(p.tertiary).into()
        };
        column![
            rule::horizontal(1).style(style::line),
            container(
                row![
                    container(left).width(Fill).clip(true),
                    container(status).max_width(420).clip(true),
                    tip(
                        icon_button(
                            Icon::Activity,
                            15.0,
                            if self.logs_open {
                                p.accent
                            } else {
                                p.secondary
                            },
                            Message::ToggleActivity,
                        ),
                        Action::ToggleActivity.hint("Session activity", os),
                    ),
                    // Building this 1×1 view is what turns on GPU frame decoding.
                    shader(self.gpu.probe()).width(1).height(1),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([8.0, GUTTER - 6.0]),
        ]
        .into()
    }

    fn activity(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut lines = column![].spacing(3);
        if self.snapshot.logs.is_empty() {
            lines = lines.push(
                text("Session events will appear here.")
                    .size(style::SMALL)
                    .color(p.secondary),
            );
        }
        for entry in &self.snapshot.logs {
            lines = lines.push(
                row![
                    text(entry.time.clone())
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(p.tertiary),
                    text(entry.message.clone())
                        .size(style::CAPTION)
                        .font(style::MONO)
                        .color(if entry.level.eq_ignore_ascii_case("error") {
                            p.danger
                        } else {
                            p.secondary
                        }),
                ]
                .spacing(12),
            );
        }
        column![
            rule::horizontal(1).style(style::line),
            container(
                column![
                    row![
                        text("Activity").size(style::BODY).font(style::SEMIBOLD),
                        space::horizontal(),
                        button(text("Copy log").size(style::SMALL))
                            .padding([3, 8])
                            .style(style::link)
                            .on_press(Message::CopyLog),
                    ]
                    .align_y(Alignment::Center),
                    scrollable(lines)
                        .anchor_bottom()
                        .width(Fill)
                        .height(Fill)
                        .style(style::scroll),
                ]
                .spacing(6),
            )
            .height(170)
            .padding([10.0, GUTTER]),
        ]
        .into()
    }

    fn inspector(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let tabs = container(
            row![
                tip(
                    segment(
                        "Features",
                        self.tab == Tab::Features,
                        Message::Tab(Tab::Features)
                    ),
                    Action::FeaturesTab.hint("Features panel", os),
                ),
                tip(
                    segment(
                        "Capture",
                        self.tab == Tab::Capture,
                        Message::Tab(Tab::Capture)
                    ),
                    Action::CaptureTab.hint("Capture panel", os),
                ),
                tip(
                    segment(
                        "Forward",
                        self.tab == Tab::Forward,
                        Message::Tab(Tab::Forward)
                    ),
                    Action::ForwardTab.hint("Forward panel", os),
                ),
            ]
            .spacing(2),
        )
        .padding(2)
        .style(style::segment_track);
        let content = match self.tab {
            Tab::Features => self.features(p),
            Tab::Capture => self.capture_settings(p),
            Tab::Forward => self.forward_settings(p),
        };
        container(column![
            self.titlebar(container(tabs).padding([0, 18]).into()),
            space().height(14),
            scrollable(container(content).padding(iced::Padding {
                top: 4.0,
                right: 20.0,
                bottom: 24.0,
                left: 18.0,
            }))
            .height(Fill)
            .style(style::scroll),
        ])
        .width(INSPECTOR)
        .height(Fill)
        .style(style::base)
        .into()
    }

    fn features(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let connected = snapshot.connected.is_some();
        let mut content = column![].spacing(12);
        if connected {
            content = content.push(self.auto_card(p));
        }
        content = content.push(
            text_input("Search features", &self.search)
                .id("feature-search")
                .icon(icon::input_icon(Icon::Search))
                .on_input(Message::Search)
                .size(style::BODY)
                .padding([7, 10])
                .style(style::input),
        );
        content = content.push(
            row![
                text(format!("{} features", snapshot.features.len()))
                    .size(style::SMALL)
                    .color(p.secondary),
                space::horizontal(),
                button(text("Refresh").size(style::SMALL))
                    .padding([3, 8])
                    .style(style::link)
                    .on_press_maybe(connected.then_some(Message::RefreshFeatures)),
            ]
            .align_y(Alignment::Center),
        );
        if !connected {
            return content
                .push(space().height(16))
                .push(text("Camera settings").size(style::HEADING).font(style::SEMIBOLD))
                .push(
                    text("Connect a camera to inspect its GenICam features, configure acquisition, and run commands.")
                        .size(style::BODY)
                        .color(p.secondary),
                )
                .into();
        }
        let query = self.search.to_lowercase();
        let managed = |name: &str| {
            snapshot
                .auto
                .as_ref()
                .is_some_and(|auto| auto.managed.iter().any(|m| m == name))
        };
        let mut visible = 0;
        for group in ["Image", "Acquisition", "Device", "Transport", "Other"] {
            let features: Vec<_> = snapshot
                .features
                .iter()
                .filter(|f| {
                    feature_group(&f.name) == group
                        && (query.is_empty()
                            || f.name.to_lowercase().contains(&query)
                            || f.display_name.to_lowercase().contains(&query))
                })
                .collect();
            if features.is_empty() {
                continue;
            }
            visible += features.len();
            let mut section = column![heading(group, p)].spacing(12);
            for feature in features {
                section = section.push(self.feature(
                    feature,
                    snapshot.streaming,
                    managed(&feature.name),
                    p,
                ));
            }
            content = content.push(space().height(6)).push(section);
        }
        if visible == 0 {
            content = content.push(
                text("No matching features")
                    .size(style::BODY)
                    .color(p.secondary),
            );
        }
        content.into()
    }

    fn auto_card(&self, p: &'static Palette) -> Element<'_, Message> {
        let auto = self.snapshot.auto.as_ref();
        let busy = self.auto_busy();
        let os = Os::CURRENT;
        let mut head = row![
            text("Exposure").size(style::BODY).font(style::SEMIBOLD),
            space::horizontal(),
        ]
        .align_y(Alignment::Center);
        if let Some(status) = auto {
            head = head.push(
                text(capitalize(&status.state))
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(match status.state.as_str() {
                        "stable" => p.live,
                        "limited" => p.warn,
                        _ => p.secondary,
                    }),
            );
        }
        let modes = container(
            row![
                tip(
                    segment(
                        "Manual",
                        auto.is_none(),
                        if busy {
                            None
                        } else {
                            Some(Message::Auto(false))
                        },
                    ),
                    Action::ToggleAuto.hint("Toggle auto / manual", os),
                ),
                tip(
                    segment(
                        "Auto",
                        auto.is_some(),
                        if busy {
                            None
                        } else {
                            Some(Message::Auto(true))
                        },
                    ),
                    Action::ToggleAuto.hint("Toggle auto / manual", os),
                ),
            ]
            .spacing(2),
        )
        .padding(2)
        .style(style::segment_track);
        let mut card = column![head, modes].spacing(10);
        match auto {
            None => {
                card = card.push(
                    text("Auto tunes exposure, gain and frame rate for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            Some(status) => {
                card = card
                    .push(
                        column![
                            slider(0.0..=1.0, self.balance, Message::Balance)
                                .step(0.01)
                                .on_release(Message::BalanceReleased)
                                .style(style::slide),
                            row![
                                text("Quality").size(style::CAPTION).color(p.secondary),
                                space::horizontal(),
                                text("Frame rate").size(style::CAPTION).color(p.secondary),
                            ],
                        ]
                        .spacing(4),
                    )
                    .push(
                        text(auto_summary(status))
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
                for note in &status.notes {
                    card = card.push(text(note.clone()).size(style::SMALL).color(p.warn));
                }
                if !status.changes.is_empty() {
                    card = card.push(disclosure(
                        format!("Auto changes ({})", status.changes.len()),
                        self.auto_changes_open,
                        Message::ToggleAutoChanges,
                        p,
                    ));
                    if self.auto_changes_open {
                        let mut changes = column![].spacing(3);
                        for change in status.changes.iter().rev() {
                            let unit = self
                                .snapshot
                                .features
                                .iter()
                                .find(|f| f.name == change.feature)
                                .and_then(|f| f.unit.as_deref());
                            changes = changes.push(tip(
                                text(change_text(change, unit))
                                    .size(style::CAPTION)
                                    .font(style::MONO)
                                    .color(p.secondary),
                                change.reason.clone(),
                            ));
                        }
                        card = card.push(
                            scrollable(changes)
                                .height(Length::Shrink)
                                .style(style::scroll),
                        );
                    }
                }
            }
        }
        container(card)
            .padding(14)
            .width(Fill)
            .style(style::well)
            .into()
    }

    fn feature<'a>(
        &'a self,
        feature: &'a FeatureInfo,
        streaming: bool,
        managed: bool,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let locked = streaming
            && matches!(
                feature.name.as_str(),
                "Width"
                    | "Height"
                    | "OffsetX"
                    | "OffsetY"
                    | "PixelFormat"
                    | "BinningHorizontal"
                    | "BinningVertical"
                    | "DecimationHorizontal"
                    | "DecimationVertical"
                    | "VideoMode"
            );
        let writable = (feature.writable || managed) && !locked;
        let name = if feature.display_name.is_empty() {
            &feature.name
        } else {
            &feature.display_name
        };
        let kind = feature.kind.to_lowercase();
        let label = tip(
            text(name.clone()).size(style::BODY),
            format!("{}\n{}", feature.name, feature.description),
        );
        let current = feature_value(feature);
        let mut notes: Vec<(String, Color)> = Vec::new();
        if managed && !locked {
            notes.push(("Managed by auto".into(), p.accent_text));
        } else if !writable && kind != "command" {
            notes.push((
                if locked {
                    "Stop stream to edit"
                } else {
                    "Read only"
                }
                .into(),
                p.tertiary,
            ));
        }
        let control: Element<'_, Message> = if let Some(error) = &feature.error
            && kind != "command"
        {
            notes.push((error.clone(), p.warn));
            space().into()
        } else if kind == "command" {
            let enabled = feature.writable
                && match feature.name.as_str() {
                    "AcquisitionStart" => !streaming,
                    "AcquisitionStop" => streaming,
                    _ => true,
                };
            let action = match feature.name.as_str() {
                "AcquisitionStart" => "Start stream",
                "AcquisitionStop" => "Stop stream",
                _ => "Execute",
            };
            button(text(action).size(style::SMALL))
                .padding([4, 10])
                .style(style::secondary)
                .on_press_maybe(enabled.then(|| Message::Execute(feature.name.clone())))
                .into()
        } else if !writable {
            text(format!(
                "{current}{}",
                feature
                    .unit
                    .as_ref()
                    .map(|u| format!(" {u}"))
                    .unwrap_or_default()
            ))
            .size(style::SMALL)
            .font(style::MONO)
            .color(p.secondary)
            .into()
        } else if kind == "boolean" || kind == "bool" {
            let checked = feature
                .value
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(current == "true" || current == "1");
            let name = feature.name.clone();
            iced::widget::checkbox(checked)
                .on_toggle(move |value| Message::Set(name.clone(), value.to_string()))
                .size(16)
                .style(style::check)
                .into()
        } else if !feature.choices.is_empty() {
            let name = feature.name.clone();
            pick_list(
                &feature.choices[..],
                feature.choices.iter().find(|c| **c == current).cloned(),
                move |value| Message::Set(name.clone(), value),
            )
            .placeholder(current.clone())
            .width(150)
            .text_size(style::SMALL)
            .padding([5, 8])
            .style(style::pick)
            .menu_style(style::menu)
            .into()
        } else {
            let draft = self.edits.get(&feature.name).unwrap_or(&current);
            let draft_name = feature.name.clone();
            let mut editor = row![
                text_input(&current, draft)
                    .on_input(move |value| Message::Draft(draft_name.clone(), value))
                    .on_submit(Message::Commit(feature.name.clone()))
                    .font(style::MONO)
                    .size(style::SMALL)
                    .padding([5, 8])
                    .width(110)
                    .style(style::input),
            ]
            .spacing(4)
            .align_y(Alignment::Center);
            if *draft != current {
                editor = editor.push(tip(
                    button(text("Set").size(style::SMALL))
                        .padding([4, 6])
                        .style(style::link)
                        .on_press(Message::Commit(feature.name.clone())),
                    "Apply · Enter",
                ));
            }
            editor.into()
        };
        if feature.min.is_some() || feature.max.is_some() || feature.unit.is_some() {
            let range = match (feature.min, feature.max) {
                (Some(min), Some(max)) => format!("{} – {}", number(min), number(max)),
                (Some(min), None) => format!("min {}", number(min)),
                (None, Some(max)) => format!("max {}", number(max)),
                _ => String::new(),
            };
            let text = format!("{range} {}", feature.unit.as_deref().unwrap_or_default());
            if !text.trim().is_empty() && writable {
                notes.push((text.trim().to_owned(), p.tertiary));
            }
        }
        let mut item = column![
            row![container(label).width(Fill), control]
                .spacing(10)
                .align_y(Alignment::Center),
        ]
        .spacing(2);
        if !notes.is_empty() {
            let mut line = row![].spacing(8);
            for (note, color) in notes {
                line = line.push(text(note).size(style::CAPTION).color(color));
            }
            item = item.push(line.wrap());
        }
        item.into()
    }

    fn num(&self, key: Num, width: f32) -> Element<'_, Message> {
        let value = self
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_else(|| self.num_text(key));
        let valid = !self.drafts.contains_key(&key) || num_valid(key, &value);
        text_input("", &value)
            .on_input(move |value| Message::Num(key, value))
            .size(style::BODY)
            .padding([5, 8])
            .width(width)
            .style(if valid {
                style::input
            } else {
                style::input_invalid
            })
            .into()
    }

    fn capture_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let connected = self.snapshot.connected.is_some();
        let capturing = self.pending("Saving capture");
        let os = Os::CURRENT;
        let mut content = column![
            heading("Save frames", p),
            text("Capture to a file or numbered sequence on this computer, an external or network drive, or an S3 bucket.")
                .size(style::SMALL)
                .color(p.secondary),
            self.capture_to
                .view(&self.output, "Output path", self.dark())
                .map(Message::CapturePicker),
            field("Frames", unit(self.num(Num::Count, 80.0), "", p), p),
            field(
                "Format",
                pick_list(&FORMATS[..], find(&FORMATS, self.format), Message::Format)
                    .width(150)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
                p,
            ),
            field("Timeout", unit(self.num(Num::Timeout, 80.0), "ms", p), p),
            space().height(2),
            button(
                center(
                    row![
                        icon(Icon::Camera, 15.0, Color::WHITE),
                        text(if capturing { "Saving…" } else { "Capture & save" })
                            .size(style::BODY)
                            .font(style::MEDIUM),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .height(Length::Shrink),
            )
            .width(Fill)
            .padding([9, 14])
            .style(style::primary)
            .on_press_maybe(
                (connected && !self.output.trim().is_empty() && !capturing)
                    .then_some(Message::Capture),
            ),
            text(format!("{} · Capture from this session", Action::Capture.shortcut(os)))
                .size(style::CAPTION)
                .color(p.tertiary),
            space().height(6),
            checkbox(
                "Capture on a schedule",
                self.schedule_enabled,
                Message::ScheduleEnabled,
            ),
        ]
        .spacing(12);
        if self.schedule_enabled {
            content = content
                .push(field(
                    "Start after",
                    unit(self.num(Num::Delay, 80.0), "s", p),
                    p,
                ))
                .push(field(
                    "Frame interval",
                    unit(self.num(Num::Interval, 80.0), "s", p),
                    p,
                ))
                .push(
                    text(format!(
                        "{} frames, one every {} seconds",
                        self.count,
                        number(self.schedule_interval_seconds)
                    ))
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .push(
                    button(
                        center(text("Schedule capture").size(style::BODY)).height(Length::Shrink),
                    )
                    .width(Fill)
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press_maybe(
                        (connected && !self.output.trim().is_empty()).then_some(Message::Schedule),
                    ),
                );
        }
        content = content
            .push(space().height(4))
            .push(self.storage_settings(p))
            .push(space().height(4))
            .push(self.jobs(p))
            .push(space().height(8))
            .push(heading("Automation", p))
            .push(
                text("Control this same camera from your shell or coding agent. Camera ownership stays in this session.")
                    .size(style::SMALL)
                    .color(p.secondary),
            )
            .push(code_block(self.session_command(), p));
        content.into()
    }

    fn forward_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let pending = self.pending("Starting forwarding") || self.pending("Stopping forwarding");
        let mut content = column![
            heading("Forward to a recorder", p),
            text("Publish the selected camera to MediaMTX, an NVR, or another compatible destination.")
                .size(style::SMALL)
                .color(p.secondary),
            self.record_to
                .view(
                    &self.forward_output,
                    "Stream URL or recording file (rtsp://, srt://, capture.mkv)",
                    self.dark(),
                )
                .map(Message::RecordPicker),
            field(
                "Codec",
                pick_list(&CODECS[..], find(&CODECS, self.forward_codec), Message::Codec)
                    .width(150)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
                p,
            ),
            field(
                "Encoder",
                tip(
                    text_input("auto", &self.forward_encoder)
                        .on_input(Message::Encoder)
                        .size(style::BODY)
                        .padding([5, 8])
                        .width(150)
                        .style(style::input),
                    "auto selects an available encoder. Enter an FFmpeg encoder name to override.",
                ),
                p,
            ),
            field("Frame rate", unit(self.num(Num::Fps, 80.0), "fps", p), p),
            field(
                "Bitrate",
                text_input("4M", &self.forward_bitrate)
                    .on_input(Message::Bitrate)
                    .size(style::BODY)
                    .padding([5, 8])
                    .width(150)
                    .style(style::input)
                    .into(),
                p,
            ),
            field(
                "Maximum file",
                tip(
                    unit(self.num(Num::FileMib, 80.0), "MiB", p),
                    "For file destinations, recording stops when this file size limit is reached.",
                ),
                p,
            ),
            space().height(2),
        ]
        .spacing(12);
        if let Some(destination) = &snapshot.forwarding {
            content = content
                .push(
                    row![
                        dot(p.live, 7.0),
                        text("Forwarding")
                            .size(style::SMALL)
                            .font(style::SEMIBOLD)
                            .color(p.live),
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                )
                .push(
                    text(redact_address(destination))
                        .size(style::SMALL)
                        .font(style::MONO)
                        .color(p.secondary),
                )
                .push(
                    button(
                        center(text("Stop forwarding").size(style::BODY)).height(Length::Shrink),
                    )
                    .width(Fill)
                    .padding([9, 14])
                    .style(style::danger)
                    .on_press_maybe((!pending).then_some(Message::StopForward)),
                );
        } else {
            content = content.push(
                button(
                    center(
                        row![
                            icon(Icon::Broadcast, 15.0, Color::WHITE),
                            text(if pending {
                                "Starting…"
                            } else {
                                "Start forwarding"
                            })
                            .size(style::BODY)
                            .font(style::MEDIUM),
                        ]
                        .spacing(8)
                        .align_y(Alignment::Center),
                    )
                    .height(Length::Shrink),
                )
                .width(Fill)
                .padding([9, 14])
                .style(style::primary)
                .on_press_maybe(
                    (snapshot.connected.is_some()
                        && !pending
                        && !self.forward_output.trim().is_empty())
                    .then_some(Message::StartForward),
                ),
            );
        }
        content
            .push(space().height(4))
            .push(self.storage_settings(p))
            .push(space().height(8))
            .push(heading("Encoder availability", p))
            .push(
                text("Automatic selection uses the available hardware or software encoder. Run capturefab doctor to inspect media support and available encoders.")
                    .size(style::SMALL)
                    .color(p.secondary),
            )
            .push(code_block("capturefab doctor".into(), p))
            .into()
    }

    fn storage_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let summary = format!(
            "{} GiB · {} files · {}",
            number(self.quota_gib),
            grouped(self.quota_files.into()),
            if self.quota_action == "stop" {
                "stop at capacity"
            } else {
                "delete oldest at capacity"
            }
        );
        let mut content = column![disclosure(
            "Storage & retention".into(),
            self.storage_open,
            Message::ToggleStorage,
            p
        ),]
        .spacing(10);
        if self.storage_open {
            content = content
                .push(field(
                    "Disk budget",
                    unit(self.num(Num::QuotaGib, 80.0), "GiB", p),
                    p,
                ))
                .push(field(
                    "File limit",
                    unit(self.num(Num::QuotaFiles, 80.0), "", p),
                    p,
                ))
                .push(field(
                    "At capacity",
                    pick_list(
                        &ON_FULL[..],
                        find(&ON_FULL, self.quota_action),
                        Message::OnFull,
                    )
                    .width(150)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu)
                    .into(),
                    p,
                ))
                .push(checkbox(
                    "Limit capture age",
                    self.retention_enabled,
                    Message::RetentionEnabled,
                ));
            if self.retention_enabled {
                content = content.push(field(
                    "Keep for",
                    unit(self.num(Num::RetentionDays, 80.0), "days", p),
                    p,
                ));
            }
            if self.quota_action == "delete-oldest" {
                content = content.push(
                    text("At capacity, removes the oldest Capturefab managed files in the output location.")
                        .size(style::SMALL)
                        .color(p.warn),
                );
            }
        }
        content
            .push(text(summary).size(style::CAPTION).color(p.tertiary))
            .into()
    }

    fn jobs(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let mut content = column![
            row![
                text("Scheduled jobs")
                    .size(style::BODY)
                    .font(style::SEMIBOLD),
                space::horizontal(),
                button(text("Refresh").size(style::SMALL))
                    .padding([3, 8])
                    .style(style::link)
                    .on_press_maybe(snapshot.connected.is_some().then_some(Message::RefreshJobs)),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(10);
        if snapshot.jobs.is_empty() {
            return content
                .push(
                    text("No scheduled captures for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                )
                .into();
        }
        let jobs = serde_json::to_value(&snapshot.jobs).unwrap_or_default();
        for job in jobs.as_array().into_iter().flatten().rev() {
            let id = job["id"].as_u64().unwrap_or_default();
            let status = job["status"].as_str().unwrap_or("unknown");
            let captured = job["captured"].as_u64().unwrap_or_default();
            let count = job["count"].as_u64().unwrap_or(1);
            let mut card = column![
                row![
                    text(format!("Job {id}"))
                        .size(style::BODY)
                        .font(style::MEDIUM),
                    space::horizontal(),
                    text(capitalize(status))
                        .size(style::CAPTION)
                        .color(match status {
                            "failed" => p.danger,
                            "running" | "pending" => p.accent_text,
                            _ => p.secondary,
                        }),
                ]
                .align_y(Alignment::Center),
                progress_bar(0.0..=1.0, captured as f32 / count.max(1) as f32)
                    .girth(4)
                    .style(style::progress),
                text(format!("{captured} / {count} frames"))
                    .size(style::CAPTION)
                    .color(p.secondary),
                text(job["output"].as_str().unwrap_or("").to_owned())
                    .size(style::CAPTION)
                    .font(style::MONO)
                    .color(p.secondary),
            ]
            .spacing(6);
            if matches!(status, "pending" | "running") {
                let wait = job["next_at_ms"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_sub(epoch_ms());
                card = card.push(
                    row![
                        text(format!("Next frame in {:.1} s", wait as f64 / 1000.0))
                            .size(style::CAPTION)
                            .color(p.secondary),
                        space::horizontal(),
                        button(text("Cancel").size(style::SMALL))
                            .padding([3, 8])
                            .style(style::plain)
                            .on_press(Message::CancelJob(id)),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            if let Some(error) = job["error"].as_str() {
                card = card.push(text(error.to_owned()).size(style::CAPTION).color(p.danger));
            }
            content = content.push(container(card).padding(12).width(Fill).style(style::well));
        }
        content.into()
    }

    fn help(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let section = |(name, actions): (&'static str, &'static [Action])| {
            let mut list = column![heading(name, p)].spacing(7);
            for action in actions
                .iter()
                .filter(|action| !action.bindings(os).is_empty())
            {
                list = list.push(
                    row![
                        text(action.label()).size(style::BODY).width(Fill),
                        text(action.shortcut(os))
                            .size(style::SMALL)
                            .font(style::MEDIUM)
                            .color(p.secondary),
                    ]
                    .spacing(10),
                );
            }
            list
        };
        let [cameras, acquisition, view, window] = Action::SECTIONS;
        let content = column![
            row![
                icon(Icon::Keyboard, 24.0, p.accent),
                text("Quick guide").size(22).font(style::BOLD),
                space::horizontal(),
                button(icon(Icon::Close, 14.0, p.secondary))
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::Help(false)),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
            text("Discover a camera, connect, then start a stream. The simulator is available without camera hardware.")
                .size(style::BODY)
                .color(p.secondary),
            row![
                column![section(cameras), section(acquisition)].spacing(18).width(Fill),
                column![section(view), section(window)].spacing(18).width(Fill),
            ]
            .spacing(32),
            text("Space, Enter and Esc act on the workbench when no field has keyboard focus. Press Esc or click elsewhere to leave a field.")
                .size(style::SMALL)
                .color(p.secondary),
            heading("One visible session, many ways to control it", p),
            text("Copy the session command to attach your terminal or coding agent. Session commands update this window and share its camera connection.")
                .size(style::BODY),
            code_block(self.session_command(), p),
            code_block("capturefab --help".into(), p),
            text("Add stream URLs directly. Native camera sources use avfoundation:// on macOS, v4l2:// on Linux, and dshow:// on Windows. GigE: use a reachable address on the camera's subnet. USB3: the operating system must allow access to the camera. Activity shows connection and capture errors.")
                .size(style::SMALL)
                .color(p.secondary),
        ]
        .spacing(14)
        .padding(28);
        container(scrollable(content).style(style::scroll))
            .width(700)
            .max_height(680)
            .style(style::sheet)
            .into()
    }
}

/// Whether `text` is a value `key` accepts.
fn num_valid(key: Num, text: &str) -> bool {
    fn within<T: std::str::FromStr + PartialOrd>(text: &str, min: T, max: T) -> bool {
        text.trim()
            .parse::<T>()
            .ok()
            .is_some_and(|v| v >= min && v <= max)
    }
    match key {
        Num::Count => within(text, 1u64, 100_000),
        Num::Timeout => within(text, 100u64, 60_000),
        Num::Delay => within(text, 0u64, 31_536_000),
        Num::Interval => within(text, 0.1f64, 86_400.0),
        Num::Fps => within(text, 1.0f64, 240.0),
        Num::FileMib => within(text, 1.0f64, 65_536.0),
        Num::QuotaGib => within(text, 0.01f64, 102_400.0),
        Num::QuotaFiles => within(text, 1u64, 1_000_000),
        Num::RetentionDays => within(text, 1u64, 3650),
    }
}

// Small building blocks

/// Tooltip in the workbench style.
fn focus_address() -> Task<Message> {
    Task::batch([
        operation::focus("connect-address"),
        operation::select_all("connect-address"),
    ])
}

fn tip<'a, M: 'a>(content: impl Into<Element<'a, M>>, tip: impl ToString) -> Element<'a, M> {
    tooltip(
        content,
        container(text(tip.to_string()).size(style::SMALL))
            .padding([5, 9])
            .style(style::tooltip),
        tooltip::Position::Bottom,
    )
    .gap(6)
    .delay(Duration::from_millis(500))
    .into()
}

fn checkbox<'a, M: 'a>(
    label: &'a str,
    checked: bool,
    on: impl Fn(bool) -> M + 'a,
) -> Element<'a, M> {
    iced::widget::checkbox(checked)
        .label(label)
        .on_toggle(on)
        .size(16)
        .spacing(8)
        .text_size(style::BODY)
        .style(style::check)
        .into()
}

/// Single-line text that is cut off at its container's edge.
fn clipped<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> Element<'a, M> {
    container(content).clip(true).into()
}

fn fade(color: Color, amount: f32) -> Color {
    Color {
        a: color.a * amount,
        ..color
    }
}

fn dot<'a, M: 'a>(color: Color, size: f32) -> Element<'a, M> {
    container(space().width(size).height(size))
        .style(move |_| container::Style {
            background: Some(color.into()),
            border: iced::border::rounded(size / 2.0),
            ..container::Style::default()
        })
        .into()
}

/// A section title: semibold accent text over a hairline, as in Things.
fn heading<'a, M: 'a>(title: &'a str, p: &'static Palette) -> Element<'a, M> {
    column![
        text(title)
            .size(style::BODY)
            .font(style::SEMIBOLD)
            .color(p.accent_text),
        rule::horizontal(1).style(style::line),
    ]
    .spacing(6)
    .into()
}

fn disclosure<'a>(
    title: String,
    open: bool,
    on: Message,
    p: &'static Palette,
) -> Element<'a, Message> {
    button(
        row![
            icon(
                if open {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                },
                11.0,
                p.secondary,
            ),
            text(title)
                .size(style::BODY)
                .font(style::MEDIUM)
                .color(p.text),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding([3, 4])
    .style(style::plain)
    .on_press(on)
    .into()
}

/// A label on the left and its control on the right.
fn field<'a>(
    label: &'a str,
    control: Element<'a, Message>,
    p: &'static Palette,
) -> Element<'a, Message> {
    row![
        text(label).size(style::BODY).color(p.secondary).width(Fill),
        control,
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn unit<'a>(
    control: Element<'a, Message>,
    unit: &'a str,
    p: &'static Palette,
) -> Element<'a, Message> {
    row![
        control,
        text(unit)
            .size(style::SMALL)
            .color(p.secondary)
            .width(Length::Fixed(28.0)),
    ]
    .spacing(6)
    .align_y(Alignment::Center)
    .into()
}

fn code_block<'a>(command: String, p: &'static Palette) -> Element<'a, Message> {
    container(
        row![
            clipped(
                text(command.clone())
                    .size(style::SMALL)
                    .font(style::MONO)
                    .color(p.text)
            ),
            space::horizontal(),
            tip(
                button(icon(Icon::Copy, 13.0, p.secondary))
                    .padding(4)
                    .style(style::plain)
                    .on_press(Message::Copy(command)),
                "Copy",
            ),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(iced::Padding {
        top: 4.0,
        right: 4.0,
        bottom: 4.0,
        left: 10.0,
    })
    .width(Fill)
    .style(style::code)
    .into()
}

fn segment<'a>(
    label: &'a str,
    selected: bool,
    on: impl Into<Option<Message>>,
) -> Element<'a, Message> {
    button(
        text(label)
            .size(style::SMALL)
            .font(if selected {
                style::SEMIBOLD
            } else {
                style::SANS
            })
            .width(Fill)
            .align_x(Alignment::Center),
    )
    .width(Fill)
    .padding([4, 12])
    .style(style::segment(selected))
    .on_press_maybe(on.into())
    .into()
}

fn icon_button<'a>(kind: Icon, size: f32, color: Color, on: Message) -> Element<'a, Message> {
    button(icon(kind, size, color))
        .padding(6)
        .style(style::plain)
        .on_press(on)
        .into()
}

fn stream_button<'a>(
    streaming: bool,
    label: &'a str,
    on: Option<Message>,
    p: &'static Palette,
) -> button::Button<'a, Message> {
    let kind = if streaming { Icon::Stop } else { Icon::Play };
    button(
        row![
            icon(kind, 12.0, if streaming { p.danger } else { Color::WHITE }),
            text(label).size(style::BODY).font(style::MEDIUM)
        ]
        .spacing(7)
        .align_y(Alignment::Center),
    )
    .padding([7, 16])
    .style(if streaming {
        style::danger
    } else {
        style::primary
    })
    .on_press_maybe(on)
}

fn last_frame<'a, M: 'a>() -> Element<'a, M> {
    container(text("Last frame").size(style::CAPTION).font(style::MEDIUM))
        .padding([3, 8])
        .style(style::badge)
        .into()
}

/// `content` centered over a dimmed copy of `base`, rising in as `shown` goes
/// from 0 to 1; clicking outside sends `on_blur`.
fn modal<'a>(
    base: Element<'a, Message>,
    content: Element<'a, Message>,
    on_blur: Message,
    shown: f32,
) -> Element<'a, Message> {
    let sheet = container(opaque(content)).padding(iced::Padding {
        top: 24.0 * (1.0 - shown),
        ..iced::Padding::ZERO
    });
    stack![
        base,
        opaque(
            mouse_area(center(sheet).style(move |theme| style::scrim(theme, shown)))
                .on_press(on_blur)
        ),
    ]
    .into()
}

fn save_screenshot(shot: &window::Screenshot, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, shot.size.width, shot.size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&shot.rgba)?;
    Ok(())
}

// Formatting

fn feature_value(feature: &FeatureInfo) -> String {
    feature
        .value
        .as_ref()
        .map(value_text)
        .unwrap_or_else(|| "—".into())
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// 1204 → "1,204".
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn feature_group(name: &str) -> &'static str {
    if [
        "Width",
        "Height",
        "Offset",
        "Pixel",
        "Binning",
        "Decimation",
        "Reverse",
        "Sensor",
        "TestPattern",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        "Image"
    } else if [
        "Acquisition",
        "Exposure",
        "Gain",
        "Trigger",
        "Balance",
        "Black",
        "Gamma",
        "Analog",
        "Digital",
        "Auto",
    ]
    .iter()
    .any(|p| name.starts_with(p))
    {
        "Acquisition"
    } else if ["Device", "UserSet", "Camera", "Temperature"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "Device"
    } else if ["Gev", "U3V", "TL", "Stream", "Payload", "Packet"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        "Transport"
    } else {
        "Other"
    }
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => "—".into(),
        _ => value.to_string(),
    }
}

fn auto_summary(status: &AutoStatus) -> String {
    let strategy = match status.strategy.as_str() {
        "firmware" => "Camera auto exposure",
        "software" => "Capturefab exposure",
        "video-mode" => "Video mode",
        "none" => "Frame rate only",
        other => other,
    };
    std::iter::once(strategy.to_owned())
        .chain(status.exposure_us.map(|us| {
            if us < 1000.0 {
                format!("{us:.0} µs")
            } else {
                format!("{:.1} ms", us / 1000.0)
            }
        }))
        .chain(status.gain_db.map(|db| format!("{db:.1} dB")))
        .chain(status.target_fps.map(|fps| format!("{fps:.0} fps")))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn change_text(change: &AutoChange, unit: Option<&str>) -> String {
    format!(
        "{} {} {} → {}{}",
        change.time,
        change.feature,
        value_text(change.from.as_ref().unwrap_or(&serde_json::Value::Null)),
        value_text(&change.to),
        unit.map(|u| format!(" {u}")).unwrap_or_default()
    )
}

fn transport_loss(stats: &TransportStats) -> u64 {
    stats.incomplete_frames + stats.lost_frames
}

fn transport_text(stats: &TransportStats) -> String {
    format!(
        "{}resend {}/{} · {} lost",
        stats
            .packet_size
            .map(|size| format!("{size} B packets · "))
            .unwrap_or_default(),
        stats.resend_recovered,
        stats.resend_requested,
        transport_loss(stats)
    )
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn pixel_name(pixel_format: u32) -> String {
    match pixel_format {
        MONO8 => "Mono8".into(),
        RGB8 => "RGB8".into(),
        0x0218_0015 => "BGR8".into(),
        0x0110_0003 => "Mono10".into(),
        0x0110_0005 => "Mono12".into(),
        0x0110_0007 => "Mono16".into(),
        0x0108_0008 => "BayerGR8".into(),
        0x0108_0009 => "BayerRG8".into(),
        0x0108_000a => "BayerGB8".into(),
        0x0108_000b => "BayerBG8".into(),
        _ => format!("0x{pixel_format:08x}"),
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        && !value.is_empty()
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn redact_address(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return value.into();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let safe_tail = tail.split(['?', '#']).next().unwrap_or(tail);
    let suffix = if safe_tail.len() != tail.len() {
        "?[redacted]"
    } else {
        ""
    };
    if let Some((_, host)) = authority.rsplit_once('@') {
        format!("{scheme}://[redacted]@{host}{safe_tail}{suffix}")
    } else {
        format!("{scheme}://{authority}{safe_tail}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn auto_feature_grouping_and_text() {
        assert_eq!(feature_group("AutoExposureTimeUpperLimit"), "Acquisition");
        assert_eq!(feature_group("AutoFunctionProfile"), "Acquisition");
        let status = AutoStatus {
            strategy: "firmware".into(),
            exposure_us: Some(8200.0),
            gain_db: Some(3.06),
            target_fps: Some(45.2),
            ..Default::default()
        };
        assert_eq!(
            auto_summary(&status),
            "Camera auto exposure · 8.2 ms · 3.1 dB · 45 fps"
        );
        let status = AutoStatus {
            strategy: "software".into(),
            exposure_us: Some(250.0),
            ..Default::default()
        };
        assert_eq!(auto_summary(&status), "Capturefab exposure · 250 µs");
        let change = AutoChange {
            time: "12:00:01".into(),
            feature: "AutoExposureTimeUpperLimit".into(),
            from: Some(json!(5000)),
            to: json!(19800.5),
            reason: "limits".into(),
        };
        assert_eq!(
            change_text(&change, Some("us")),
            "12:00:01 AutoExposureTimeUpperLimit 5000 → 19800.5 us"
        );
        let change = AutoChange {
            from: None,
            feature: "ExposureAuto".into(),
            to: json!("Continuous"),
            ..change
        };
        assert_eq!(
            change_text(&change, None),
            "12:00:01 ExposureAuto — → Continuous"
        );
    }

    #[test]
    fn transport_line_and_loss() {
        let stats = TransportStats {
            packet_size: Some(1500),
            resend_requested: 14,
            resend_recovered: 12,
            incomplete_frames: 2,
            lost_frames: 1,
            ..Default::default()
        };
        assert_eq!(transport_loss(&stats), 3);
        assert_eq!(
            transport_text(&stats),
            "1500 B packets · resend 12/14 · 3 lost"
        );
        assert_eq!(
            transport_text(&TransportStats::default()),
            "resend 0/0 · 0 lost"
        );
    }

    #[test]
    fn numbers_group_and_capitalize() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1204), "1,204");
        assert_eq!(grouped(10_000_000), "10,000,000");
        assert_eq!(capitalize("stable"), "Stable");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn number_fields_apply_only_valid_values() {
        assert!(num_valid(Num::Count, "12"));
        assert!(!num_valid(Num::Count, "0"));
        assert!(!num_valid(Num::Count, "1.5"));
        assert!(!num_valid(Num::Timeout, "50"));
        assert!(num_valid(Num::Interval, " 0.5 "));
        assert!(!num_valid(Num::Interval, "0.05"));
        assert!(!num_valid(Num::QuotaFiles, "many"));
    }

    #[test]
    fn addresses_hide_credentials_and_queries() {
        assert_eq!(
            redact_address("rtsp://user:pass@cam.local:554/stream?token=1"),
            "rtsp://[redacted]@cam.local:554/stream?[redacted]"
        );
        assert_eq!(redact_address("192.168.1.20"), "192.168.1.20");
    }
}
