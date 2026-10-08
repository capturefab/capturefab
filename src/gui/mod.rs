//! The native workbench. Camera I/O lives on the session worker, never the UI thread.
mod destinations;
mod format;
mod gpu;
mod grid;
mod help;
mod icon;
mod inspector;
mod keys;
mod liveness;
mod motion;
mod notice;
mod oneline;
mod prefs;
mod preview;
mod refresh;
mod scene;
mod scopes;
mod sidebar;
mod sparkline;
mod stage;
mod style;
mod titlebar;
mod toolbar;
mod welcome;
mod widgets;

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
use format::*;
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
use liveness::{CameraState, Liveness};
use motion::{Kind, Motion};
use notice::{Batch, Level, Notice, Saved, command_notice};
use prefs::{Prefs, Recent};
use preview::Shown;
use scene::{Scene, ScreenshotRequest};
use std::{
    cell::Cell,
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use style::Palette;
use widgets::*;

/// Height of the title bar row. On macOS it shares the window's top edge with
/// the traffic lights, which sit over the content.
const BAR: f32 = if cfg!(target_os = "macos") {
    40.0
} else {
    44.0
};
/// Room the macOS traffic lights need when no sidebar sits under them.
const LIGHTS: f32 = if cfg!(target_os = "macos") { 78.0 } else { 0.0 };
const SIDEBAR: f32 = 240.0;
/// Height of the toolbar under the stage, for folding it away.
const TOOLBAR: f32 = 45.0;
const INSPECTOR: f32 = 316.0;
const GUTTER: f32 = 18.0;
/// Longest wait between checks for a new camera frame.
const FRAME_POLL: Duration = Duration::from_millis(2);
/// Small overview tiles gain nothing from more than 30 redraws a second.
const OVERVIEW_INTERVAL: Duration = Duration::from_micros(33_333);

fn mac_default(domain: &str, key: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    std::process::Command::new("/usr/bin/defaults")
        .args(["read", domain, key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

fn double_click_action() -> Option<&'static str> {
    static ACTION: OnceLock<Option<String>> = OnceLock::new();
    ACTION
        .get_or_init(|| mac_default("-g", "AppleActionOnDoubleClick"))
        .as_deref()
}

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
    let saved = screenshot.is_none().then(prefs::load);
    let window_size = screenshot_size
        .or_else(|| {
            saved
                .as_ref()?
                .window
                .map(|[w, h]| Size::new(w.max(900.0), h.max(620.0)))
        })
        .unwrap_or(Size::new(1280.0, 840.0));
    let boot = move || {
        let capturing = screenshot.is_some();
        let mut app = Workbench::new(
            handle.clone(),
            session_label.clone(),
            simulated || capturing,
            saved.clone(),
        );
        app.width = window_size.width;
        app.height = window_size.height;
        if let Some(path) = &screenshot {
            let scene = std::env::var("CAPTUREFAB_SCREENSHOT_SCENE").unwrap_or_default();
            let scene: Vec<&str> = scene.split(',').map(str::trim).collect();
            let welcome = scene.contains(&"welcome");
            let count = if welcome {
                0
            } else {
                demo_cameras.clamp(1, 16)
            };
            app.apply_scene(&scene);
            if welcome && !scene.contains(&"searching") {
                app.discover();
            }
            app.screenshot = Some(ScreenshotRequest {
                path: path.clone(),
                cameras: count,
                started: Instant::now(),
                streams_started: None,
                staged: false,
                requested: false,
                save: None,
                outcome: app_outcome.clone(),
            });
            for index in 0..count {
                app.send(
                    Job::Connect,
                    SessionCommand::Connect {
                        camera: format!("sim:{index}"),
                        timeout_ms: 2000,
                    },
                );
            }
        } else {
            app.discover();
        }
        (
            app,
            Task::batch([
                system::theme().map(Message::SystemTheme),
                window::allow_automatic_tabbing(false),
                screen_refresh(),
            ]),
        )
    };
    iced::application(boot, Workbench::update, Workbench::view)
        .title(Workbench::title)
        .subscription(Workbench::subscription)
        .theme(Workbench::theme)
        .default_font(style::SANS)
        .font(icon::REGULAR_BYTES)
        .font(icon::FILL_BYTES)
        .antialiasing(true)
        .window(window::Settings {
            size: window_size,
            position: window::Position::Centered,
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

/// The session whose shared frame rings the frame watcher polls, and the
/// shortest gap between its signals. A window has one session, and the gap is
/// read live rather than hashed, so following the display never restarts the
/// watcher.
struct FrameWatch {
    handle: SessionHandle,
    /// Nanoseconds; see `Workbench::frame_gap`.
    gap: Arc<AtomicU64>,
}

impl std::hash::Hash for FrameWatch {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

/// Sends `FrameArrived` when any camera publishes a frame: at most once per
/// gap, and not again until the workbench has taken the last one.
fn watch_frames(watch: &FrameWatch) -> impl iced::futures::Stream<Item = Message> + use<> {
    let handle = watch.handle.clone();
    let gap = watch.gap.clone();
    iced::stream::channel(0, async move |mut output| {
        std::thread::spawn(move || {
            let mut seen = handle.frame_sequence();
            while !output.is_closed() {
                let gap = Duration::from_nanos(gap.load(Ordering::Relaxed));
                // Finer checks at high refresh rates keep frames on their vsync.
                let poll = (gap / 8).clamp(Duration::from_millis(1), FRAME_POLL);
                std::thread::sleep(poll);
                let sequence = handle.frame_sequence();
                if sequence != seen && output.try_send(Message::FrameArrived).is_ok() {
                    seen = sequence;
                    std::thread::sleep(gap.saturating_sub(poll));
                }
            }
        });
        std::future::pending::<()>().await;
    })
}

/// Ask the platform for the refresh interval of the window's screen.
fn screen_refresh() -> Task<Message> {
    window::latest()
        .and_then(|id| window::run(id, refresh::screen_period))
        .map(Message::ScreenRefresh)
}

/// A command on its way to the session, filled in once where it is sent.
struct Pending {
    /// What it does: views, notices and batches go by this, never by its
    /// `label`.
    job: Job,
    receiver: Receiver<anyhow::Result<serde_json::Value>>,
    /// The address or ID a connect asked for.
    target: Option<String>,
    /// The camera the command acts on, when known.
    camera: Option<String>,
    /// When it was sent.
    at: Instant,
    /// One of several sent together, reported as one; see `send_batch`.
    batch: bool,
}

impl Pending {
    /// A command doing `job` whose answer never comes, for scenes and tests;
    /// they, like `submit`, fill in the rest with struct update syntax.
    fn unanswered(job: Job) -> Self {
        Pending {
            job,
            receiver: mpsc::channel().1,
            target: None,
            camera: None,
            at: Instant::now(),
            batch: false,
        }
    }

    /// What the toolbar says while it is on its way, before its "…". For
    /// display only.
    fn label(&self) -> String {
        let label = match &self.job {
            Job::Discover => "Looking for cameras",
            Job::Connect => "Connecting camera",
            Job::Disconnect => "Disconnecting",
            Job::Select => "Selecting camera",
            Job::Start if self.batch => "Starting streams",
            Job::Start => "Starting stream",
            Job::Stop if self.batch => "Stopping streams",
            Job::Stop => "Stopping stream",
            Job::Capture => "Saving capture",
            Job::AutoOn => "Enabling auto mode",
            Job::AutoOff => "Switching to manual",
            Job::Balance => "Updating auto balance",
            Job::RefreshFeatures => "Refreshing features",
            Job::Schedule => "Scheduling capture",
            Job::RefreshJobs => "Refreshing capture jobs",
            Job::Cancel => "Cancelling capture job",
            Job::Forward { recording: true } => "Starting recording",
            Job::Forward { recording: false } => "Starting forwarding",
            Job::StopForward { recording: true } => "Stopping recording",
            Job::StopForward { recording: false } => "Stopping forwarding",
            Job::Set(feature) => return format!("Setting {}", words(feature)),
            Job::Execute(feature) => return format!("Executing {}", words(feature)),
        };
        label.into()
    }
}

/// What a command sent to the session does. Matches on it are exhaustive,
/// so a new kind of command cannot slip past the views and notices that
/// tell commands apart.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Job {
    Discover,
    Connect,
    Disconnect,
    Select,
    /// Start a camera's stream; with `Pending::batch`, part of Start all.
    Start,
    /// Stop a camera's stream; with `Pending::batch`, part of Stop all.
    Stop,
    Capture,
    AutoOn,
    AutoOff,
    /// A new balance for auto mode.
    Balance,
    RefreshFeatures,
    Schedule,
    RefreshJobs,
    /// Cancel a capture job.
    Cancel,
    /// Start forwarding, or recording to a file (see `Output`).
    Forward {
        recording: bool,
    },
    StopForward {
        recording: bool,
    },
    /// Write a feature, by name.
    Set(String),
    /// Run a command feature, by name.
    Execute(String),
}

impl Job {
    /// The feature a write or command button acts on.
    fn feature(&self) -> Option<&str> {
        match self {
            Job::Set(feature) | Job::Execute(feature) => Some(feature),
            _ => None,
        }
    }

    /// Whether it switches auto mode or tunes it.
    fn is_auto(&self) -> bool {
        matches!(self, Job::AutoOn | Job::AutoOff | Job::Balance)
    }
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
    /// A redraw paced by the display, at the given time.
    Frame(Instant),
    FrameArrived,
    /// The window opened, moved or changed scale, perhaps onto another display.
    Displaced,
    /// The refresh interval the window's screen reports, if any.
    ScreenRefresh(Option<Duration>),
    SystemTheme(theme::Mode),
    Key {
        chord: Chord,
        captured: bool,
    },
    FocusNext(bool),
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
    ToggleExposure,
    ToggleFocusRegion,
    /// The focus region, normalized to the frame.
    FocusRegion(iced::Rectangle),
    FocusMetric(scopes::Metric),
    ResetFocusPeak,
    ToggleActivity,
    /// Open the activity log; never closes it.
    ShowActivity,
    CopyLog,
    CopySessionCommand,
    Copy(String),
    Tab(Tab),
    /// Show the inspector on `Tab`, from anywhere.
    ShowTab(Tab),
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
    ToggleSidebar,
    ToggleInspector,
    ToggleImage,
    Resized(Size),
    WindowMode(window::Mode, Option<Size>),
    ZoomWindow,
    Pointer,
    OverControls(bool),
    HoverTile(Option<String>),
    FocusTile(String),
    CaptureCamera(String),
    DismissNotice,
    /// Hide a camera's error badge, by camera ID, until its error changes.
    DismissStageError(String),
    /// The guide's body scrolled away from its top, or back.
    HelpScrolled(bool),
    /// Open or close the guide's notes on connecting cameras.
    HelpMore(bool),
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
    exposure_open: bool,
    focus_camera: bool,
    exposure: Option<frame::Exposure>,
    /// The focus region and its scores.
    focus: scopes::FocusMeter,
    shown: Option<Shown>,
    gpu: GpuFrames,
    refresh: refresh::Refresh,
    /// Nanoseconds between new-frame announcements, read by the frame watcher.
    frame_gap: Arc<AtomicU64>,
    frame_meta: Option<(u64, u32, u32, u32, u64)>,
    frame_id: Option<u64>,
    display_error: Option<String>,
    previews: HashMap<String, CameraPreview>,
    /// Each streaming camera's recent frame rate, by camera id.
    throughput: HashMap<String, sparkline::History>,
    /// Whether each streaming camera's frames keep coming, by camera id.
    liveness: HashMap<String, Liveness>,
    pending: Vec<Pending>,
    /// Commands sent to several cameras at once, by what they do, until
    /// the last one finishes.
    batches: HashMap<Job, Batch>,
    notice: Option<Notice>,
    notice_shown: Motion,
    /// The last value put on the clipboard, and when.
    copied: Option<(String, Instant)>,
    /// The last capture that saved files.
    last_saved: Option<Saved>,
    recent: Vec<Recent>,
    saved: Option<Prefs>,
    prefs_changed: Option<Instant>,
    now: Instant,
    born: Instant,
    /// When the accessibility settings were last read.
    accessibility_read: Instant,
    /// What a screenshot scene holds in place.
    scene: Scene,
    activity_slide: Motion,
    exposure_slide: Motion,
    focus_slide: Motion,
    shutter: Motion,
    welcome: Motion,
    sidebar_open: bool,
    /// Whether the inspector is docked open in a wide window.
    inspector_open: bool,
    /// Whether the inspector floats over the stage in a narrow window.
    inspector_peek: bool,
    /// Only the image: no panes, title bar or toolbar.
    image_mode: bool,
    /// Window width at the last resize.
    width: f32,
    /// Window height at the last resize.
    height: f32,
    window: Option<[f32; 2]>,
    fullscreen: bool,
    pointer_moved: Option<Instant>,
    over_controls: bool,
    hovered_tile: Option<String>,
    sidebar_slide: Motion,
    inspector_slide: Motion,
    /// Title bar and toolbar, hidden in image mode.
    bars: Motion,
    controls: Motion,
    tile_hover: Motion,
    /// The selection drawing in after it changes: the ring around the
    /// selected tile; see `selection_level`.
    ring: Motion,
    /// The camera selected before the current one, while `ring` draws in.
    ring_from: Option<String>,
    // Each part of the window keeps its own state in its own file.
    chrome: titlebar::ChromeState,
    side: sidebar::SideState,
    stage_ui: stage::StageState,
    inspect: inspector::InspectorState,
    sheets: help::SheetState,
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
        motion::refresh_accessibility();
        let shown = |open: bool| if open { 1.0 } else { 0.0 };
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
            logs_open: prefs.logs_open,
            help_open: false,
            fit: true,
            zoom: 1.0,
            stage: Cell::new(Size::new(800.0, 600.0)),
            exposure_open: prefs.exposure_open,
            focus_camera: false,
            exposure: None,
            focus: scopes::FocusMeter::new(
                prefs.focus_open,
                prefs.focus_region,
                prefs.focus_metric,
            ),
            shown: None,
            gpu: GpuFrames::new(),
            refresh: refresh::Refresh::default(),
            frame_gap: Arc::default(),
            frame_meta: None,
            frame_id: None,
            display_error: None,
            previews: HashMap::new(),
            throughput: HashMap::new(),
            liveness: HashMap::new(),
            pending: Vec::new(),
            batches: HashMap::new(),
            notice: None,
            notice_shown: Motion::new(0.0, motion::NOTICE_IN, motion::NOTICE_OUT, Kind::Fade),
            copied: None,
            last_saved: None,
            recent: prefs.recent,
            saved,
            prefs_changed: None,
            now,
            born: now,
            accessibility_read: now,
            scene: Scene::default(),
            activity_slide: Motion::new(
                shown(prefs.logs_open),
                motion::SLIDE,
                motion::SLIDE_OUT,
                Kind::Move,
            ),
            exposure_slide: Motion::new(
                shown(prefs.exposure_open),
                motion::SCOPE,
                motion::SCOPE_OUT,
                Kind::Fade,
            ),
            focus_slide: Motion::new(
                shown(prefs.focus_open),
                motion::SCOPE,
                motion::SCOPE_OUT,
                Kind::Fade,
            ),
            shutter: Motion::new(0.0, motion::SHUTTER, motion::SHUTTER, Kind::Fade),
            // Replaced at once when a camera connects; it enters afresh each time.
            welcome: Motion::new(0.0, motion::WELCOME, Duration::ZERO, Kind::Fade),
            sidebar_open: prefs.sidebar_open,
            inspector_open: prefs.inspector_open,
            inspector_peek: false,
            image_mode: false,
            width: 1280.0,
            height: 840.0,
            window: prefs.window,
            fullscreen: false,
            pointer_moved: None,
            over_controls: false,
            hovered_tile: None,
            sidebar_slide: Motion::new(
                shown(prefs.sidebar_open),
                motion::PANEL,
                motion::PANEL_OUT,
                Kind::Move,
            ),
            inspector_slide: Motion::new(
                shown(prefs.inspector_open),
                motion::PANEL,
                motion::PANEL_OUT,
                Kind::Move,
            ),
            bars: Motion::new(1.0, motion::IMAGE, motion::IMAGE_OUT, Kind::Move),
            controls: Motion::new(0.0, motion::CONTROLS_IN, motion::CONTROLS_OUT, Kind::Fade),
            tile_hover: Motion::new(0.0, motion::HOVER, motion::HOVER_OUT, Kind::Fade),
            ring: Motion::new(1.0, motion::RING, motion::RING_OUT, Kind::Fade),
            ring_from: None,
            chrome: Default::default(),
            side: Default::default(),
            stage_ui: Default::default(),
            inspect: Default::default(),
            sheets: Default::default(),
        }
    }

    fn prefs(&self) -> Prefs {
        Prefs {
            appearance: self.appearance,
            include_simulator: self.include_simulator,
            tab: self.tab,
            exposure_open: self.exposure_open,
            focus_open: self.focus.open,
            focus_region: self.focus.saved_region(),
            focus_metric: self.focus.metric,
            sidebar_open: self.sidebar_open,
            inspector_open: self.inspector_open,
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
            window: self.window,
            logs_open: self.logs_open,
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

    /// A slow breath for transient pending indicators (connecting,
    /// searching); live states stay steady. It advances on the busy tick.
    fn pulse(&self) -> f32 {
        if self.screenshot.is_some() || motion::reduce_motion() {
            return 1.0;
        }
        let t = self.now.duration_since(self.born).as_secs_f32();
        0.7 + 0.3 * (t * std::f32::consts::TAU / 1.8).cos()
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
        // Pending commands (connecting, discovering) tick fast enough to
        // step spinners and pulses without per-frame redraws.
        let busy = !self.pending.is_empty()
            || self.screenshot.is_some()
            || self.capture_to.busy()
            || self.record_to.busy();
        let interval = if busy {
            Duration::from_millis(60)
        } else if streaming {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(300)
        };
        // Redraws also measure the display until its refresh rate is known.
        let frames = if self.animating() || !self.refresh.calibrated() {
            window::frames().map(Message::Frame)
        } else {
            Subscription::none()
        };
        let arrivals = if streaming {
            Subscription::run_with(
                FrameWatch {
                    handle: self.handle.clone(),
                    gap: self.frame_gap.clone(),
                },
                watch_frames,
            )
        } else {
            Subscription::none()
        };
        Subscription::batch([
            frames,
            arrivals,
            time::every(interval).map(|_| Message::Tick),
            event::listen_with(|event, status, _window| match event {
                iced::Event::Window(
                    window::Event::Opened { .. }
                    | window::Event::Moved(_)
                    | window::Event::Rescaled(_),
                ) => Some(Message::Displaced),
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Tab),
                    modifiers,
                    ..
                }) if status == event::Status::Ignored
                    && !modifiers.control()
                    && !modifiers.logo()
                    && !modifiers.alt() =>
                {
                    Some(Message::FocusNext(modifiers.shift()))
                }
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
            window::resize_events().map(|(_, size)| Message::Resized(size)),
        ])
    }

    /// Send `command` to the selected camera, or to the session for
    /// discovery, connecting and selection.
    fn send(&mut self, job: Job, command: SessionCommand) {
        self.submit(None, job, command);
    }

    fn send_to(&mut self, camera: &str, job: Job, command: SessionCommand) {
        self.submit(Some(camera), job, command);
    }

    /// Submit `command` and track it as pending, doing `job`, noting the
    /// camera it acts on; returns the entry, so a send site can add what only
    /// it knows. The session routes commands without a camera the same way:
    /// to the selected camera, except discovery, connecting and selection.
    /// When the session refuses it, says why in the notice.
    fn submit(
        &mut self,
        camera: Option<&str>,
        job: Job,
        command: SessionCommand,
    ) -> Option<&mut Pending> {
        let error = match self.try_submit(camera, job.clone(), command) {
            // Reborrowed, as the borrow checker cannot yet return `pending`.
            Ok(_) => return self.pending.last_mut(),
            Err(error) => error,
        };
        if let Some((text, detail, level)) = command_notice(&job, &Err(error)) {
            self.set_notice(text, detail, level);
        }
        None
    }

    /// `submit`, leaving a refusal for the caller to report.
    fn try_submit(
        &mut self,
        camera: Option<&str>,
        job: Job,
        command: SessionCommand,
    ) -> Result<&mut Pending> {
        let acts_on = match (camera, &command) {
            (Some(camera), _) => Some(camera.to_owned()),
            (
                None,
                SessionCommand::Connect { .. }
                | SessionCommand::Discover { .. }
                | SessionCommand::Status,
            ) => None,
            (None, SessionCommand::Select { camera }) => Some(camera.clone()),
            (None, _) => self.snapshot.active_camera.clone(),
        };
        let receiver = match camera {
            Some(camera) => self.handle.submit_to(camera, command),
            None => self.handle.submit(command),
        }?;
        self.give_way();
        self.pending.push(Pending {
            receiver,
            camera: acts_on,
            at: self.now,
            ..Pending::unanswered(job)
        });
        Ok(self.pending.last_mut().expect("just pushed"))
    }

    /// Whether a command doing `job` is on its way, to any camera.
    fn pending(&self, job: Job) -> bool {
        self.pending.iter().any(|p| p.job == job)
    }

    /// Whether a command doing `job` is on its way to `camera`.
    fn pending_for(&self, job: Job, camera: &str) -> bool {
        self.pending
            .iter()
            .any(|p| p.job == job && p.camera.as_deref() == Some(camera))
    }

    /// While a start or stop is on its way to `camera`, alone or as part of
    /// Start all or Stop all, whether it is a stop. The controls that start
    /// and stop a camera and `toggle_stream` all go by this, so a control
    /// that looks ready never ignores a press.
    fn stream_pending(&self, camera: &str) -> Option<bool> {
        self.pending
            .iter()
            .filter(|p| {
                p.camera.as_deref() == Some(camera) && matches!(p.job, Job::Start | Job::Stop)
            })
            .max_by_key(|p| p.at)
            .map(|p| p.job == Job::Stop)
    }

    /// While a forward or recording starts or stops, whether it stops.
    fn forward_pending(&self) -> Option<bool> {
        self.pending.iter().find_map(|p| match p.job {
            Job::Forward { .. } => Some(false),
            Job::StopForward { .. } => Some(true),
            _ => None,
        })
    }

    /// `command` in a code block whose copy button confirms in place.
    fn copyable<'a>(&self, command: String, p: &'static Palette) -> Element<'a, Message> {
        let copied = self.just_copied(&command);
        code_block(command, copied, p)
    }

    /// Whether auto mode is being switched or tuned on any camera; for the
    /// overview's Auto all.
    fn auto_busy(&self) -> bool {
        self.pending.iter().any(|p| p.job.is_auto())
    }

    /// Whether auto mode is being switched or tuned on the selected camera,
    /// which its Auto controls go by. Each camera has its own worker, so
    /// another camera's command never holds them up.
    fn auto_busy_here(&self) -> bool {
        let camera = self.snapshot.active_camera.as_deref();
        camera.is_some()
            && self
                .pending
                .iter()
                .any(|p| p.job.is_auto() && p.camera.as_deref() == camera)
    }

    fn title(&self) -> String {
        let shown = if self.overview() {
            Some("All cameras".to_owned())
        } else {
            self.snapshot.connected.as_ref().map(|c| c.model.clone())
        };
        match shown {
            None => "Capturefab".into(),
            Some(shown) if cfg!(target_os = "macos") => shown,
            Some(shown) => format!("{shown} — Capturefab"),
        }
    }

    fn lights(&self) -> f32 {
        if self.fullscreen { 0.0 } else { LIGHTS }
    }

    /// How long the frame watcher waits between announcing new frames: one
    /// refresh, or the overview's slower interval, less a quarter refresh.
    /// Sleep overshoot and polling would otherwise often land just after a
    /// vsync and hold the frame for a whole extra refresh.
    fn frame_gap(&self) -> Duration {
        let period = self.refresh.period();
        let interval = if self.overview() {
            period.max(OVERVIEW_INTERVAL)
        } else {
            period
        };
        interval - period / 4
    }

    fn overview(&self) -> bool {
        self.snapshot.cameras.len() > 1 && !self.focus_camera
    }

    fn poll(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            let result = match self.pending[i].receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("the session worker disconnected"))
                }
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
            };
            let pending = self.pending.swap_remove(i);
            self.settle(&pending, &result);
        }
    }

    /// Everything that follows a finished command: the shared bookkeeping,
    /// each area's, then its notice.
    fn settle(&mut self, pending: &Pending, result: &Result<serde_json::Value>) {
        match result {
            Ok(value) => self.finished(pending, value),
            Err(error) => self.failed(pending, error),
        }
        self.result_side(pending, result);
        self.result_stage(pending, result);
        self.result_inspector(pending, result);
        let notice = if pending.batch {
            self.batch_result(&pending.job, result)
        } else {
            command_notice(&pending.job, result)
        };
        if let Some((text, detail, level)) = notice {
            self.set_notice(text, detail, level);
        }
    }

    /// Shared bookkeeping for a command that worked, before its notice.
    fn finished(&mut self, pending: &Pending, result: &serde_json::Value) {
        if pending.job == Job::Capture {
            self.shutter.replay(1.0, 0.0, self.now);
            if let Some(saved) = Saved::from_result(pending.camera.clone(), result, self.now) {
                self.last_saved = Some(saved);
            }
        }
        if pending.job == Job::Discover {
            self.side.discovery_issues = result["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|warning| warning.as_str().map(str::to_owned))
                .collect();
        }
        if let Some(feature) = pending.job.feature() {
            self.inspect.write_errors.remove(feature);
        }
        let Some(target) = &pending.target else {
            return;
        };
        self.side.connect_failures.remove(target);
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

    /// Shared bookkeeping for a command that failed, before its notice: the
    /// failure kept where the control that sent it can show it.
    fn failed(&mut self, pending: &Pending, error: &anyhow::Error) {
        let full = format!("{error:#}");
        if pending.job == Job::Discover {
            self.side.discovery_issues = vec![full.clone()];
        }
        if let Some(feature) = pending.job.feature() {
            self.inspect
                .write_errors
                .insert(feature.to_owned(), full.clone());
        }
        if let Some(target) = &pending.target {
            self.side.connect_failed(target, full, self.now);
        }
    }

    fn tick(&mut self) -> Task<Message> {
        if self.shot_staged() {
            return self.screenshot_tick();
        }
        self.poll();
        self.age_notice();
        self.age_copied();
        self.prune_flashes();
        self.poll_accessibility();
        let selected = self.snapshot.active_camera.clone();
        self.snapshot = self.handle.snapshot();
        self.scene.patch(&mut self.snapshot);
        if self.snapshot.active_camera != selected {
            self.ring_from = selected;
            self.ring.replay(0.0, 1.0, self.now);
        }
        let camera_id = self
            .snapshot
            .connected
            .as_ref()
            .map(|camera| camera.id.clone());
        if camera_id != self.observed_camera {
            self.observed_camera = camera_id;
            self.edits.clear();
            self.edit_sources.clear();
            self.inspect.write_errors.clear();
            self.frame_id = None;
            self.shown = None;
            self.frame_meta = None;
            self.display_error = None;
            self.exposure = None;
            self.focus.reset();
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
            && !self.auto_busy_here()
        {
            self.balance = status.balance;
        }
        self.sample_throughput();
        self.observe_liveness();
        self.update_frames();
        self.capture_to.tick();
        self.record_to.tick();
        self.tick_chrome();
        self.tick_side();
        self.tick_stage();
        self.tick_inspector();
        self.save_prefs();
        self.screenshot_tick()
    }

    /// Add the newest frame rate of each streaming camera to its history,
    /// flagging samples where frames were lost; stopping clears it.
    fn sample_throughput(&mut self) {
        let now = self.now;
        let snapshot = &self.snapshot;
        self.throughput.retain(|id, _| {
            snapshot
                .cameras
                .iter()
                .any(|camera| camera.streaming && &camera.info.id == id)
        });
        for camera in snapshot.cameras.iter().filter(|camera| camera.streaming) {
            let lost = liveness::camera_loss(camera);
            // The rate reads zero until the worker has timed a few frames.
            if camera.fps <= 0.0 && !self.throughput.contains_key(&camera.info.id) {
                continue;
            }
            self.throughput
                .entry(camera.info.id.clone())
                .or_default()
                .offer(now, camera.fps, lost);
        }
    }

    fn update_frames(&mut self) {
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
            let Some(frame) = self.handle.latest_frame_for(&camera.info.id) else {
                continue;
            };
            let preview = self.previews.entry(camera.info.id.clone()).or_default();
            if preview.frame_id == Some(frame.id) {
                continue;
            }
            preview.frame_id = Some(frame.id);
            liveness::seen(
                &mut self.liveness,
                self.scene.frozen.as_ref(),
                &camera.info.id,
                self.now,
            );
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
        let Some(frame) = self.handle.latest_frame() else {
            return;
        };
        if Some(frame.id) == self.frame_id {
            return;
        }
        self.frame_id = Some(frame.id);
        if let Some(id) = &self.observed_camera {
            liveness::seen(&mut self.liveness, self.scene.frozen.as_ref(), id, self.now);
        }
        let meta = (
            frame.id,
            frame.width,
            frame.height,
            frame.pixel_format,
            frame.timestamp_ns,
        );
        // Scores from a frame of another size or format are not comparable.
        if self
            .frame_meta
            .is_some_and(|(_, width, height, format, _)| {
                (width, height, format) != (meta.1, meta.2, meta.3)
            })
        {
            self.focus.reset();
        }
        self.measure(&frame);
        let shown = preview::present(&self.gpu, &mut self.shown, MAIN_VIEW, frame, |frame| {
            // Decode straight into RGBA bytes: one pass, one allocation.
            let pixels = frame::convert(frame, |[r, g, b]| [r, g, b, 255])?;
            Ok((frame.width, frame.height, pixels.into_flattened()))
        });
        match shown {
            Ok(()) => {
                self.frame_meta = Some(meta);
                self.display_error = None;
            }
            Err(err) => self.display_error = Some(err.to_string()),
        }
    }

    /// Update the open scopes from `frame`. A frame they cannot measure
    /// still shows; presenting it reports any real decoding problem.
    fn measure(&mut self, frame: &crate::types::Frame) {
        if self.exposure_open {
            self.exposure = frame::exposure(frame).ok();
        }
        if self.focus.open
            && let Ok(score) = frame::sharpness(frame, self.focus.pixels(frame.width, frame.height))
        {
            self.focus.record(score);
        }
    }

    /// Measure the latest frame again after a scope changes, so a stopped
    /// stream still shows current values.
    fn remeasure(&mut self) {
        if let Some(frame) = self.handle.latest_frame() {
            self.measure(&frame);
        }
    }

    fn discover(&mut self) {
        self.send(
            Job::Discover,
            SessionCommand::Discover {
                timeout_ms: 700,
                simulated: self.include_simulator,
            },
        );
    }

    fn connect(&mut self, camera: String) {
        self.edits.clear();
        self.edit_sources.clear();
        self.side.connect_failures.remove(&camera);
        if let Some(pending) = self.submit(
            None,
            Job::Connect,
            SessionCommand::Connect {
                camera: camera.clone(),
                timeout_ms: 5000,
            },
        ) {
            pending.target = Some(camera);
        }
    }

    fn connecting(&self, id: &str) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.target.as_deref() == Some(id))
    }

    /// Start or stop the selected camera, unless a start or stop is already
    /// on its way to it; another camera's does not hold it up.
    fn toggle_stream(&mut self) {
        let Some(camera) = self.snapshot.active_camera.clone() else {
            return;
        };
        if self.stream_pending(&camera).is_some() {
            return;
        }
        if self.snapshot.streaming {
            self.send_to(&camera, Job::Stop, SessionCommand::Stop);
        } else {
            self.send_to(&camera, Job::Start, SessionCommand::Start);
        }
    }

    fn capture(&mut self) {
        if self.pending(Job::Capture) {
            return;
        }
        self.send(
            Job::Capture,
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
        if self.auto_busy_here() {
            return;
        }
        if self.snapshot.auto.is_some() {
            self.send(Job::AutoOff, SessionCommand::Manual { revert: false });
        } else {
            self.send(
                Job::AutoOn,
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
        self.send(Job::Select, SessionCommand::Select { camera });
    }

    fn toggle_exposure(&mut self) {
        self.exposure_open = !self.exposure_open;
        if self.exposure_open
            && let Some(frame) = self.handle.latest_frame()
        {
            self.exposure = frame::exposure(&frame).ok();
        }
    }

    fn toggle_focus_region(&mut self) {
        self.focus.open = !self.focus.open;
        self.focus.reset();
        if self.focus.open
            && let Some(frame) = self.handle.latest_frame()
            && let Ok(score) =
                frame::sharpness(&frame, self.focus.pixels(frame.width, frame.height))
        {
            self.focus.record(score);
        }
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

    /// Copy the session command. Copy buttons confirm in place; `announce`
    /// confirms in the toolbar instead, for the keyboard shortcut.
    fn copy_session_command(&mut self, announce: bool) -> Task<Message> {
        let command = self.session_command();
        if announce {
            // With the camera list hidden, its session control cannot
            // confirm in place, so the toolbar must, even over an error.
            if !self.sidebar_shown() {
                self.give_way();
            }
            self.set_notice("Session command copied", None, Level::Done);
        }
        self.note_copied(command.clone());
        iced::clipboard::write(command)
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
        self.frame_gap
            .store(self.frame_gap().as_nanos() as u64, Ordering::Relaxed);
        task
    }

    fn handle_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => return self.tick(),
            Message::Frame(at) => self.refresh.redraw(at),
            Message::Displaced => {
                self.refresh.recalibrate();
                return screen_refresh();
            }
            Message::ScreenRefresh(period) => self.refresh.report(period),
            Message::FrameArrived if !self.shot_staged() => self.update_frames(),
            Message::FrameArrived => {}
            Message::SystemTheme(mode) => self.system_dark = mode == theme::Mode::Dark,
            Message::Key { chord, captured } => return self.shortcut(chord, !captured),
            Message::FocusNext(back) => {
                use iced::advanced::widget::{
                    Id, Operation, operate,
                    operation::{focusable, scope},
                };
                fn within<T: Send + 'static>(
                    sheet: bool,
                    operation: impl Operation<T> + 'static,
                ) -> Task<T> {
                    if sheet {
                        operate(scope(Id::new("sheet"), operation))
                    } else {
                        operate(operation)
                    }
                }
                let sheet = self.sheet_open();
                let step = move || {
                    if back {
                        within(sheet, focusable::focus_previous())
                    } else {
                        within(sheet, focusable::focus_next())
                    }
                };
                return step().chain(within(sheet, focusable::count()).then(move |count| {
                    if count.focused.is_none() && count.total > 0 {
                        step()
                    } else {
                        Task::none()
                    }
                }));
            }
            Message::CycleAppearance => {
                self.appearance = match self.appearance {
                    Appearance::System => Appearance::Light,
                    Appearance::Light => Appearance::Dark,
                    Appearance::Dark => Appearance::System,
                }
            }
            Message::Help(open) => self.show_help(open),
            Message::DragWindow => return window::latest().and_then(window::drag),
            Message::ZoomWindow => {
                return window::latest().and_then(|id| match double_click_action() {
                    Some("Minimize") => window::minimize(id, true),
                    Some("None") => Task::none(),
                    _ => window::toggle_maximize(id),
                });
            }
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
                    self.send(Job::Select, SessionCommand::Select { camera: id });
                }
            }
            Message::Disconnect(id) => {
                self.send_to(&id, Job::Disconnect, SessionCommand::Disconnect)
            }
            Message::Address(value) => {
                // Editing the address retires its failure.
                self.side.connect_failures.remove(self.address.trim());
                self.address = value;
            }
            Message::ConnectAddress => {
                let address = self.address.trim().to_owned();
                if !address.is_empty() {
                    self.connect(address);
                }
            }
            Message::ToggleStream => self.toggle_stream(),
            Message::Capture => self.capture(),
            Message::Focus(focus) => self.focus_camera = focus,
            Message::Select(camera) => self.send(Job::Select, SessionCommand::Select { camera }),
            Message::StreamCamera(id, start) => {
                if start {
                    self.send_to(&id, Job::Start, SessionCommand::Start);
                } else {
                    self.send_to(&id, Job::Stop, SessionCommand::Stop);
                }
            }
            Message::AllStreams(start) => {
                let cameras = self
                    .snapshot
                    .cameras
                    .iter()
                    .filter(|camera| camera.streaming != start)
                    .map(|camera| camera.info.id.clone())
                    .collect();
                if start {
                    self.send_batch(cameras, Job::Start, || SessionCommand::Start);
                } else {
                    self.send_batch(cameras, Job::Stop, || SessionCommand::Stop);
                }
            }
            Message::AllAuto(auto) => {
                let cameras = self
                    .snapshot
                    .cameras
                    .iter()
                    .filter(|camera| camera.auto.is_none() == auto)
                    .map(|camera| camera.info.id.clone())
                    .collect();
                let balance = Some(self.balance);
                if auto {
                    self.send_batch(cameras, Job::AutoOn, || SessionCommand::Auto { balance });
                } else {
                    self.send_batch(cameras, Job::AutoOff, || SessionCommand::Manual {
                        revert: false,
                    });
                }
            }
            Message::Fit => self.fit = true,
            Message::Actual => {
                self.fit = false;
                self.zoom = 1.0;
            }
            Message::Zoom(factor) => self.zoom_by(factor),
            Message::ToggleExposure => self.toggle_exposure(),
            Message::ToggleFocusRegion => self.toggle_focus_region(),
            Message::FocusRegion(region) => {
                self.focus.region = region;
                self.focus.reset();
                self.remeasure();
            }
            Message::FocusMetric(metric) => self.focus.metric = metric,
            Message::ResetFocusPeak => {
                self.focus.reset();
                self.remeasure();
            }
            Message::ToggleActivity => self.logs_open = !self.logs_open,
            Message::ShowActivity => self.logs_open = true,
            Message::CopyLog => {
                self.note_copied(notice::COPIED_LOG);
                return iced::clipboard::write(
                    self.snapshot
                        .logs
                        .iter()
                        .map(|l| format!("{} [{}] {}", l.time, l.level, l.message))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            Message::CopySessionCommand => return self.copy_session_command(false),
            Message::Copy(value) => {
                self.note_copied(value.clone());
                return iced::clipboard::write(value);
            }
            Message::Tab(tab) => self.tab = tab,
            Message::ShowTab(tab) => {
                self.tab = tab;
                self.image_mode = false;
                if self.inspector_docked() {
                    self.inspector_open = true;
                } else {
                    self.inspector_peek = true;
                }
            }
            Message::Search(value) => self.search = value,
            Message::RefreshFeatures => {
                self.edits.clear();
                self.edit_sources.clear();
                self.inspect.write_errors.clear();
                self.send(Job::RefreshFeatures, SessionCommand::Features);
            }
            Message::Auto(on) => {
                if !self.auto_busy_here() && on != self.snapshot.auto.is_some() {
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
                        Job::Balance,
                        SessionCommand::Auto {
                            balance: Some(self.balance),
                        },
                    );
                }
            }
            Message::ToggleAutoChanges => self.auto_changes_open = !self.auto_changes_open,
            Message::Draft(feature, value) => {
                self.inspect.write_errors.remove(&feature);
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
                    Job::Set(feature.clone()),
                    SessionCommand::Set { feature, value },
                );
            }
            Message::Set(feature, value) => self.send(
                Job::Set(feature.clone()),
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
                self.send(Job::Execute(feature), command);
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
                Job::Schedule,
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
            Message::RefreshJobs => self.send(Job::RefreshJobs, SessionCommand::Jobs),
            Message::CancelJob(id) => self.send(Job::Cancel, SessionCommand::CancelJob { id }),
            Message::Codec(codec) => self.forward_codec = codec.value,
            Message::Encoder(value) => self.forward_encoder = value,
            Message::Bitrate(value) => self.forward_bitrate = value,
            Message::StartForward => {
                let output = Output::of(self.forward_output.trim());
                if self.forward_pending() != Some(false) {
                    self.send(
                        Job::Forward {
                            recording: output.recording(),
                        },
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
            Message::StopForward => {
                let recording = self
                    .snapshot
                    .forwarding
                    .as_deref()
                    .is_some_and(|output| Output::of(output).recording());
                self.send(Job::StopForward { recording }, SessionCommand::StopForward);
            }
            Message::CapturePicker(message) => return self.picker(false, message),
            Message::RecordPicker(message) => return self.picker(true, message),
            Message::ToggleSidebar => {
                self.image_mode = false;
                self.sidebar_open = !self.sidebar_open;
            }
            Message::ToggleInspector => {
                self.image_mode = false;
                if self.inspector_docked() {
                    self.inspector_open = !self.inspector_open;
                } else {
                    self.inspector_peek = !self.inspector_peek;
                }
            }
            Message::ToggleImage => self.image_mode = !self.image_mode,
            Message::Resized(size) => {
                self.width = size.width;
                self.height = size.height;
                if self.inspector_docked() {
                    self.inspector_peek = false;
                }
                return window::latest().and_then(move |id| {
                    window::is_maximized(id).then(move |maximized| {
                        window::mode(id).map(move |mode| {
                            Message::WindowMode(mode, (!maximized).then_some(size))
                        })
                    })
                });
            }
            Message::WindowMode(mode, size) => {
                self.fullscreen = mode == window::Mode::Fullscreen;
                if let (window::Mode::Windowed, Some(size)) = (mode, size) {
                    self.window = Some([size.width, size.height]);
                }
            }
            Message::Pointer => self.wake_controls(),
            Message::OverControls(over) => self.over_controls = over,
            Message::HoverTile(id) => {
                if id.is_some() && id != self.hovered_tile {
                    self.tile_hover.replay(0.0, 1.0, self.now);
                }
                self.hovered_tile = id;
            }
            Message::FocusTile(camera) => {
                self.focus_camera = true;
                if self.snapshot.active_camera.as_ref() != Some(&camera) {
                    self.send(Job::Select, SessionCommand::Select { camera });
                }
            }
            Message::CaptureCamera(id) => {
                if !self.pending(Job::Capture) {
                    self.send_to(
                        &id,
                        Job::Capture,
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
            }
            Message::Screenshot(shot) => {
                if let Some(request) = &mut self.screenshot {
                    let path = request.path.clone();
                    let (sender, receiver) = mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = sender.send(scene::save_screenshot(&shot, &path));
                    });
                    request.save = Some(receiver);
                }
            }
            Message::DismissNotice => self.dismiss_notice(),
            Message::DismissStageError(id) => self.dismiss_stage_error(&id),
            Message::HelpScrolled(scrolled) => self.sheets.scrolled = scrolled,
            Message::HelpMore(open) => self.sheets.more = open,
        }
        Task::none()
    }

    fn shortcut(&mut self, chord: Chord, keyboard_free: bool) -> Task<Message> {
        let modal = self.sheet_open();
        let Some(action) = Action::find(&chord, keyboard_free, Os::CURRENT) else {
            return Task::none();
        };
        let connected = self.snapshot.connected.is_some();
        match action {
            Action::Overview if self.help_open => self.help_open = false,
            Action::Overview if self.capture_to.editing() => {
                self.capture_to
                    .update(destinations::Message::Cancel, &mut self.output);
            }
            Action::Overview if self.record_to.editing() => {
                self.record_to
                    .update(destinations::Message::Cancel, &mut self.forward_output);
            }
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
            Action::Help => self.show_help(!self.help_open),
            Action::CopySessionCommand => return self.copy_session_command(true),
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
            Action::ToggleSidebar => return self.handle_message(Message::ToggleSidebar),
            Action::ToggleInspector => return self.handle_message(Message::ToggleInspector),
            // Sheets take the keyboard until they close.
            _ if modal => {}
            Action::Overview if self.image_mode => self.image_mode = false,
            Action::Overview if self.inspector_peek => self.inspector_peek = false,
            Action::ImageMode => self.image_mode = !self.image_mode,
            Action::ToggleExposure => self.toggle_exposure(),
            Action::ToggleFocusRegion => self.toggle_focus_region(),
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
                    operation::snap_to(inspector::SCROLL, operation::RelativeOffset::START),
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
}

// Views

impl Workbench {
    fn view(&self) -> Element<'_, Message> {
        let dark = self.dark();
        let p = Palette::of(dark);
        let sidebar = self.sidebar_slide.lerp(0.0, SIDEBAR, self.now);
        let inspector = self.inspector_slide.lerp(0.0, INSPECTOR, self.now);
        let docked = self.inspector_docked();
        let mut body = row![];
        if sidebar > 0.5 {
            // Anchored right, so the list slides out under the window's left edge.
            body = body.push(
                container(self.sidebar(p))
                    .width(sidebar)
                    .height(Fill)
                    .align_right(sidebar)
                    .clip(true),
            );
        }
        body = body.push(
            container(self.main(p))
                .width(Fill)
                .height(Fill)
                .style(style::base),
        );
        if docked && inspector > 0.5 {
            body = body.push(rule::vertical(1).style(style::line)).push(
                container(self.inspector(p))
                    .width(inspector)
                    .height(Fill)
                    .clip(true),
            );
        }
        let mut layers = stack![body];
        if !docked && inspector > 0.5 {
            // Between the title bar and the toolbar, so the button that
            // closes it and the notices stay in reach.
            layers = layers.push(
                container(opaque(
                    container(self.inspector(p))
                        .width(inspector)
                        .height(Fill)
                        .clip(true)
                        .style(style::floating),
                ))
                .align_right(Fill)
                .padding(iced::Padding {
                    top: BAR,
                    bottom: TOOLBAR * self.bars.get(self.now),
                    ..iced::Padding::ZERO
                }),
            );
        }
        // Building this 1×1 view is what turns on GPU frame decoding.
        let body: Element<'_, Message> = layers
            .push(shader(self.gpu.probe()).width(1).height(1))
            .into();
        self.sheets(body, p)
    }

    /// The title bar row at the top of a pane; on macOS it is also the
    /// window's drag handle.
    fn titlebar<'a>(&self, content: Element<'a, Message>) -> Element<'a, Message> {
        let top = container(content)
            .height(BAR)
            .width(Fill)
            .align_y(Alignment::Center);
        if cfg!(target_os = "macos") {
            mouse_area(top)
                .on_press(Message::DragWindow)
                .on_double_click(Message::ZoomWindow)
                .into()
        } else {
            top.into()
        }
    }

    fn main(&self, p: &'static Palette) -> Element<'_, Message> {
        let content = if self.overview() {
            self.overview_view(p)
        } else {
            self.single_view(p)
        };
        // Always a stack, so the hint coming and going keeps the views' state.
        let content = stack![content, self.image_hint()];
        let bars = self.bars.get(self.now);
        let mut main = column![content];
        let activity = self.activity_slide.lerp(0.0, 171.0, self.now) * bars;
        if activity > 0.5 {
            main = main.push(container(self.activity(p)).height(activity).clip(true));
        }
        if bars > 0.999 {
            main = main.push(self.toolbar(p));
        } else if bars > 0.001 {
            main = main.push(container(self.toolbar(p)).height(TOOLBAR * bars).clip(true));
        }
        main.into()
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

#[cfg(test)]
mod tests {
    use super::*;
    use iced::futures::{FutureExt, StreamExt};

    #[test]
    fn frame_watcher_stays_quiet_without_new_frames() {
        let watch = FrameWatch {
            handle: SessionHandle::new(),
            gap: Arc::new(AtomicU64::new(16_000_000)),
        };
        let mut frames = Box::pin(watch_frames(&watch));
        assert!(frames.next().now_or_never().is_none());
        std::thread::sleep(FRAME_POLL * 10);
        assert!(frames.next().now_or_never().is_none());
    }

    #[test]
    fn show_messages_open_without_toggling() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.logs_open = true;
        let _ = bench.handle_message(Message::ShowActivity);
        assert!(bench.logs_open, "stays open");
        bench.image_mode = true;
        bench.width = 1280.0;
        bench.inspector_open = false;
        let _ = bench.handle_message(Message::ShowTab(Tab::Forward));
        assert_eq!(bench.tab, Tab::Forward);
        assert!(bench.inspector_open && !bench.image_mode);
        bench.width = 960.0;
        let _ = bench.handle_message(Message::ShowTab(Tab::Capture));
        assert!(bench.inspector_peek, "floats in a narrow window");
    }

    #[test]
    fn a_start_on_its_way_holds_up_only_its_own_camera() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let mut other = liveness::streaming_camera(0, 0, 0.0);
        other.info.id = "sim:1".into();
        other.streaming = false;
        bench.snapshot.cameras = vec![liveness::streaming_camera(0, 0, 0.0), other];
        bench.pending.push(Pending {
            camera: Some("sim:0".into()),
            ..Pending::unanswered(Job::Start)
        });
        bench.snapshot.active_camera = Some("sim:1".into());
        assert_eq!(
            bench.stream_pending("sim:1"),
            None,
            "its controls look ready"
        );
        bench.toggle_stream();
        assert!(
            bench.pending_for(Job::Start, "sim:1"),
            "and do what they say"
        );
        let sent = bench.pending.len();
        bench.toggle_stream();
        assert_eq!(bench.pending.len(), sent, "its own start holds it up");
        // A Stop all on its way counts as the camera's own.
        bench.pending.clear();
        bench.pending.push(Pending {
            camera: Some("sim:1".into()),
            batch: true,
            ..Pending::unanswered(Job::Stop)
        });
        assert_eq!(bench.stream_pending("sim:1"), Some(true));
        assert_eq!(bench.stream_pending("sim:0"), None);
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
}
