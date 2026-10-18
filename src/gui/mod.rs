//! The native workbench. Camera I/O lives on the session worker, never the UI thread.
mod accessibility;
mod analytics;
mod autocapture;
mod brand;
mod button;
mod camera_history;
mod camera_view;
mod client_activity;
mod close_error;
mod dataset;
mod destinations;
mod format;
mod gpu;
mod grid;
mod help;
mod history;
mod icon;
mod inspector;
mod keyboard_history;
mod keys;
mod liveness;
mod local_history;
mod menus;
mod motion;
mod network;
mod notice;
mod oneline;
mod pick_list;
mod prefs;
mod preview;
mod refresh;
mod reveal;
mod scene;
mod scopes;
mod settings;
mod sidebar;
mod signals;
mod sparkline;
mod stage;
mod storage_policy;
mod style;
mod tags;
mod text_input;
mod time_format;
mod titlebar;
mod toolbar;
mod video_analytics;
mod welcome;
mod widgets;
#[cfg(test)]
mod window_lifecycle_tests;

use crate::{
    auto::{AutoChange, AutoStatus},
    camera_profile::CameraKind,
    frame,
    genicam::FeatureInfo,
    session::{ActivityOrigin, SessionActor, SessionCommand, SessionHandle, SessionSnapshot},
    storage::StoragePolicy,
    types::{CameraInfo, MONO8, RGB8, TransportStats},
};
use anyhow::Result;
use button::button;
use close_error::CloseError;
use destinations::Picker;
use format::*;
use gpu::GpuFrames;
use iced::widget::{
    center, column, container, mouse_area, opaque, operation, progress_bar, responsive, row, rule,
    scrollable, shader, space, stack, text, tooltip,
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
use pick_list::pick_list;
pub(crate) use prefs::migrate as migrate_prefs;
use prefs::{CaptureTags, Prefs, Recent, UiScale};
use preview::Shown;
use scene::{Scene, ScreenshotRequest};
use std::{
    cell::Cell,
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use style::Palette;
use text_input::text_input;
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
    let screenshot_sequence = screenshot
        .as_ref()
        .map(|_| scene::ScreenshotSequence::from_env())
        .transpose()?;
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
    let initial_scale = match std::env::var("CAPTUREFAB_UI_SCALE").as_deref() {
        Ok("large") => UiScale::Large,
        Ok("larger") => UiScale::Larger,
        _ => saved
            .as_ref()
            .map_or(UiScale::Standard, |prefs| prefs.ui_scale),
    };
    let boot = move || {
        let capturing = screenshot.is_some();
        let (main_window, opened) = window::open(window::Settings {
            icon: Some(brand::window_icon()),
            size: Size::new(
                window_size.width / initial_scale.factor(),
                window_size.height / initial_scale.factor(),
            ),
            position: prefs::centered_window(initial_scale),
            min_size: Some(Size::new(900.0, 620.0)),
            exit_on_close_request: false,
            visible: false,
            #[cfg(target_os = "macos")]
            platform_specific: window::settings::PlatformSpecific {
                title_hidden: true,
                titlebar_transparent: true,
                fullsize_content_view: true,
            },
            #[cfg(target_os = "linux")]
            platform_specific: window::settings::PlatformSpecific {
                application_id: "capturefab".into(),
                ..Default::default()
            },
            ..window::Settings::default()
        });
        let mut app = Workbench::new(
            handle.clone(),
            session_label.clone(),
            simulated || capturing,
            saved.clone(),
        );
        app.main_window = main_window;
        app.focused_window = main_window;
        app.key_window = Some(main_window);
        app.width = window_size.width / app.ui_scale.factor();
        app.height = window_size.height / app.ui_scale.factor();
        if let Some(path) = &screenshot {
            let scene = std::env::var("CAPTUREFAB_SCREENSHOT_SCENE").unwrap_or_default();
            let scene: Vec<&str> = scene.split(',').map(str::trim).collect();
            let welcome = scene.contains(&"welcome");
            let existing = handle.snapshot().cameras.len() as u32;
            let count = if welcome {
                0
            } else if existing > 0 {
                existing
            } else {
                demo_cameras.clamp(1, 16)
            };
            app.apply_scene(&scene);
            app.screenshot = Some(ScreenshotRequest {
                path: path.clone(),
                cameras: count,
                started: Instant::now(),
                streams_started: None,
                staged: false,
                requested: false,
                save: None,
                outcome: app_outcome.clone(),
                sequence: screenshot_sequence.expect("validated screenshot sequence"),
            });
            for index in 0..if existing == 0 { count } else { 0 } {
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
                opened.map(Message::WindowOpened).chain(Task::batch([
                    screen_refresh(main_window),
                    if capturing {
                        Task::none()
                    } else {
                        prefs::fit_window(window_size, initial_scale)
                    },
                ])),
            ]),
        )
    };
    #[cfg(target_os = "macos")]
    brand::set_dock_icon();
    style::initialize_fonts();
    iced::daemon(boot, Workbench::update, Workbench::window_view)
        .title(|app: &Workbench, id| {
            if app.settings_window == Some(id) {
                "Settings".into()
            } else {
                app.title()
            }
        })
        .subscription(Workbench::subscription)
        .theme(Workbench::window_theme)
        .scale_factor(|app: &Workbench, _| app.ui_scale.factor())
        .default_font(style::SANS)
        .font(icon::REGULAR_BYTES)
        .font(icon::FILL_BYTES)
        .antialiasing(true)
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

/// Session activity has its own condition variable, so even an idle window
/// sees another participant's action as soon as it is queued or completed.
struct ActivityWatch(SessionHandle);

impl std::hash::Hash for ActivityWatch {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

fn watch_activity(watch: &ActivityWatch) -> impl iced::futures::Stream<Item = Message> + use<> {
    let handle = watch.0.clone();
    iced::stream::channel(0, async move |mut output| {
        std::thread::spawn(move || {
            let mut seen = 0;
            while !output.is_closed() {
                let revision = handle.wait_for_activity(seen, Duration::from_secs(1));
                if revision > seen {
                    if output.try_send(Message::Tick).is_ok() {
                        seen = revision;
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        });
        std::future::pending::<()>().await;
    })
}

/// Ask the platform for the refresh interval of the window's screen.
fn screen_refresh(id: window::Id) -> Task<Message> {
    window::run(id, refresh::screen_period).map(Message::ScreenRefresh)
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
    /// The value a write asked for, which its control shows until the reply.
    value: Option<String>,
    /// When it was sent.
    at: Instant,
    /// One of several sent together, reported as one; see `send_batch`.
    batch: bool,
    /// A replay of an acknowledged history checkpoint. Its reply commits the
    /// shared timeline rather than recording another edit.
    history_replay: Option<bool>,
    history_token: Option<String>,
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
            value: None,
            at: Instant::now(),
            batch: false,
            history_replay: None,
            history_token: None,
        }
    }

    /// What the toolbar says while it is on its way, before its "…". For
    /// display only.
    fn label(&self) -> String {
        let label = match &self.job {
            Job::Discover => "Looking for cameras",
            Job::NetworkScan => "Scanning network and tuning cameras",
            Job::Connect => "Connecting camera",
            Job::Disconnect => "Disconnecting",
            Job::Select => "Selecting camera",
            Job::Start if self.batch => "Starting streams",
            Job::Start => "Starting stream",
            Job::Stop if self.batch => "Stopping streams",
            Job::Stop => "Stopping stream",
            Job::Capture if self.batch => "Capturing all cameras",
            Job::Capture => "Saving capture",
            Job::CaptureSessionOpen => "Opening capture session",
            Job::CaptureSessionClose => "Closing capture session",
            Job::AutoCapture => "Updating automatic capture",
            Job::CaptureExample => "Learning from capture",
            Job::CaptureReview => "Loading capture for review",
            Job::ReviewCapture => "Saving review",
            Job::AutoOn => "Enabling auto mode",
            Job::AutoOff => "Switching to manual",
            Job::Balance => "Updating auto balance",
            Job::RefreshFeatures => "Refreshing features",
            Job::Schedule => "Scheduling capture",
            Job::RefreshJobs => "Refreshing capture jobs",
            Job::Cancel => "Cancelling capture job",
            Job::Forward { recording: true } if self.batch => "Starting recordings",
            Job::Forward { recording: true } => "Starting recording",
            Job::Forward { recording: false } => "Starting forwarding",
            Job::StopForward { recording: true } if self.batch => "Stopping recordings",
            Job::StopForward { recording: true } => "Stopping recording",
            Job::StopForward { recording: false } => "Stopping forwarding",
            Job::DatasetOpen => "Opening dataset",
            Job::DatasetClose => "Closing dataset",
            Job::DatasetNote => "Adding note",
            Job::DatasetExport => "Exporting dataset",
            Job::SignalStart => "Starting data source",
            Job::SignalStop => "Stopping data source",
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
    NetworkScan,
    Connect,
    Disconnect,
    Select,
    /// Start a camera's stream; with `Pending::batch`, part of Start all.
    Start,
    /// Stop a camera's stream; with `Pending::batch`, part of Stop all.
    Stop,
    Capture,
    CaptureSessionOpen,
    CaptureSessionClose,
    AutoCapture,
    CaptureExample,
    CaptureReview,
    ReviewCapture,
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
    DatasetOpen,
    DatasetClose,
    #[allow(dead_code)] // Notes come from the CLI and RPC for now.
    DatasetNote,
    DatasetExport,
    SignalStart,
    SignalStop,
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

/// A scope the overview cannot draw over its tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Histogram,
    Focus,
}

#[derive(Debug, Clone)]
enum Message {
    Undo,
    Redo,
    FullscreenResult {
        before: bool,
        actual: bool,
        expected: bool,
        replay: Option<bool>,
    },
    Tick,
    /// A redraw paced by the display, at the given time.
    Frame(Instant),
    FrameArrived,
    /// The window opened, moved or changed scale, perhaps onto another display.
    Displaced,
    /// The refresh interval the window's screen reports, if any.
    ScreenRefresh(Option<Duration>),
    StoragePolicy(storage_policy::Reply),
    SystemTheme(theme::Mode),
    Key {
        chord: Chord,
        captured: bool,
    },
    FocusNext(bool),
    Menu(menus::Command),
    DesktopMenu(menus::DesktopInput),
    EditorFocused(bool),
    ContentFocused(keyboard_history::Area),
    PopupOpen(bool),
    Settings(bool),
    Appearance(Appearance),
    UiScale(UiScale),
    ScaledViewport(Size),
    CloseRequested(window::Id),
    WindowOpened(window::Id),
    WindowFocused(window::Id),
    WindowUnfocused(window::Id),
    InWindow(window::Id, Box<Message>),
    AccessibilityRequest(accessibility::Request),
    NativeInput {
        message: Box<Message>,
        acknowledgement: text_input::Acknowledgement,
    },
    AccessibilityAttached(window::Id, bool),
    AccessibilityRecoveryAttached(window::Id),
    CloseErrorMode(window::Id, window::Mode),
    AccessibilityChanged(window::Id),
    WindowScale(window::Id, f32),
    Quit,
    PrefsFlushed(std::result::Result<(), String>),
    RetryClose,
    KeepOpen,
    Help(bool),
    DragWindow,
    Discover,
    ScanNetwork,
    NetworkReport(bool),
    NetworkScrolled(bool),
    Analytics(bool),
    AnalyticsTab(analytics::Tab),
    AnalyticsScrolled(bool),
    AnalyticsHistory(analytics::HistoryInput),
    VideoAnalytics(video_analytics::Input),
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
    FocusRegionStart,
    FocusMetric(scopes::Metric),
    ResetFocusPeak,
    ToggleActivity,
    /// Open the activity log; never closes it.
    ShowActivity,
    /// Open the activity log on a camera's log, by camera ID: it shows only
    /// the selected camera's, so this selects the camera first.
    CameraActivity(String),
    CopyLog,
    CopySessionCommand,
    Copy(String),
    Tab(Tab),
    /// Show the inspector on `Tab`, from anywhere.
    ShowTab(Tab),
    Search(String),
    InspectorDisclosure(inspector::Section),
    /// Open every feature group, or close them all.
    ExpandFeatureGroups(bool),
    RefreshFeatures,
    Auto(bool),
    Balance(f64),
    BalanceReleased,
    BalanceCommit(f64),
    ToggleAutoChanges,
    Draft(String, String),
    Commit(String),
    /// A quick slider let go: applies its draft, if dragging made one.
    SliderReleased(String),
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
    /// Switch the Video tab to streaming to a URL, or back to recording.
    ForwardMode(bool),
    CapturePicker(destinations::Message),
    AutoCapture(autocapture::Input),
    RecordPicker(destinations::Message),
    Screenshot(window::Screenshot),
    ToggleSidebar,
    ToggleInspector,
    ToggleImage,
    ToggleCameraView,
    Resized(Size),
    WindowMode(window::Mode, Option<Size>),
    ZoomWindow,
    Pointer,
    OverControls(bool),
    HoverTile(Option<String>),
    FocusTile(String),
    /// Turn a scope on if it is off and open this camera, where it draws.
    ShowScopeOn(String, Scope),
    CaptureCamera(String),
    /// Capture from every connected camera at once.
    AllCapture,
    /// Start recording on every camera that can, or stop all recordings.
    AllRecord(bool),
    /// Start recording the selected camera, or stop it.
    Record,
    /// Show a saved file in the system file manager.
    Reveal(PathBuf),
    /// Start the live view when a camera connects.
    AutoStart(bool),
    Tag(tags::TagField, String),
    Dataset(dataset::Msg),
    Signals(signals::Msg),
    DismissNotice,
    /// Hide a camera's error badge, by camera ID, until the error recurs.
    DismissStageError(String),
    /// The guide's body scrolled away from its top, or back.
    HelpScrolled(bool),
    /// Open or close the guide's notes on connecting cameras.
    HelpMore(bool),
}

type FeatureDrafts = (HashMap<String, String>, HashMap<String, String>);

#[derive(Clone, Copy, Debug)]
enum CloseTarget {
    Window(window::Id),
    Quit,
}

struct Workbench {
    main_window: window::Id,
    /// Last window used for commands; retained when the app loses focus.
    focused_window: window::Id,
    /// Native windows retain their editors while another window is key.
    window_editor_focus: HashMap<window::Id, bool>,
    /// The window currently receiving keyboard input, if the app is active.
    key_window: Option<window::Id>,
    content_focus: keyboard_history::Area,
    settings_window: Option<window::Id>,
    accessibility: accessibility::Bridge,
    window_scales: HashMap<window::Id, f32>,
    history: history::History<local_history::Edit>,
    history_replay: Option<bool>,
    history_deferred: Vec<(String, Option<String>, local_history::Edit, Instant)>,
    history_deferred_apply: Option<Box<local_history::LocalChange>>,
    camera_drafts: HashMap<String, FeatureDrafts>,
    known_camera_history: HashMap<String, (bool, u64)>,
    camera_history_state: camera_history::State,
    handle: SessionHandle,
    session: String,
    actor: SessionActor,
    snapshot: SessionSnapshot,
    /// This window's camera focus. Other clients may select a different
    /// camera in the shared session without moving these editors or preview.
    local_camera: Option<String>,
    client_activity: client_activity::State,
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
    scan_timeout_ms: u64,
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
    storage_status: storage_policy::Cache,
    auto_changes_open: bool,
    system_dark: bool,
    appearance: Appearance,
    ui_scale: UiScale,
    screenshot: Option<ScreenshotRequest>,
    logs_open: bool,
    help_open: bool,
    network_open: bool,
    network_error: Option<String>,
    network_scrolled: bool,
    analytics: analytics::State,
    video_analytics: video_analytics::State,
    settings_open: bool,
    menu_editor: bool,
    popup_open: bool,
    menu_edit: Option<keyboard_history::EditRequest>,
    desktop_menu: menus::DesktopState,
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
    camera_views: HashMap<String, camera_view::CameraView>,
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
    /// The file the last recording started here writes, and its camera.
    last_recording: Option<(Option<String>, String)>,
    /// Start the live view when a camera connects.
    auto_start: bool,
    /// Capture context sent with captures and recordings.
    tags: CaptureTags,
    dataset: dataset::DatasetUi,
    signals: signals::SignalsUi,
    recent: Vec<Recent>,
    saved: Option<Prefs>,
    prefs_saver: prefs::Saver,
    closing: Option<CloseTarget>,
    close_error: Option<CloseError>,
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
    /// The inspector was asked for before the first camera, which otherwise
    /// keeps it out of the way; for this run only.
    inspector_summoned: bool,
    /// What the Video tab held on the side it last left, in memory only:
    /// the recording template and its destination, and the stream URL.
    stashed_record: Option<(String, Option<String>)>,
    stashed_stream: String,
    /// The Video tab was switched to streaming and its URL is not one yet:
    /// empty, or a scheme being typed. Every other output tells its own mode.
    stream_draft: bool,
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
    auto_capture: autocapture::State,
    sheets: help::SheetState,
}

impl Workbench {
    fn new(handle: SessionHandle, session: String, simulated: bool, saved: Option<Prefs>) -> Self {
        let prefs = saved.clone().unwrap_or_default();
        let open_sections = prefs.open_sections.clone();
        // A first run keeps the inspector out of the way until a camera
        // comes; it must not slide away on launch.
        let quiet = prefs.recent.is_empty();
        // A saved stream URL opens the Video tab in Stream mode, and keeps it
        // there while the URL is cleared and retyped.
        let stream_draft = !Output::of(&prefs.forward_output).recording();
        let defaults = Prefs::default();
        let pick = |choices: &[Choice], value: &str, fallback: &str| {
            find(choices, value)
                .or_else(|| find(choices, fallback))
                .map_or("", |choice| choice.value)
        };
        let now = Instant::now();
        motion::refresh_accessibility();
        let shown = |open: bool| if open { 1.0 } else { 0.0 };
        let main_window = window::Id::unique();
        let dataset = dataset::DatasetUi::start(&handle, saved.as_ref());
        let signals = signals::SignalsUi::start(saved.as_ref());
        let client_activity = client_activity::State::from_snapshot(&handle.snapshot());
        let video_analytics =
            video_analytics::State::new(handle.clone(), prefs.video_analytics.clone());
        let mut bench = Self {
            main_window,
            focused_window: main_window,
            window_editor_focus: HashMap::new(),
            key_window: Some(main_window),
            content_focus: keyboard_history::Area::Image,
            settings_window: None,
            accessibility: Default::default(),
            window_scales: HashMap::new(),
            history: Default::default(),
            history_replay: None,
            history_deferred: Vec::new(),
            history_deferred_apply: None,
            camera_drafts: HashMap::new(),
            known_camera_history: HashMap::new(),
            camera_history_state: Default::default(),
            handle,
            session,
            actor: SessionActor::process(ActivityOrigin::Gui),
            snapshot: SessionSnapshot::default(),
            local_camera: None,
            client_activity,
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
            scan_timeout_ms: prefs.scan_timeout_ms,
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
            storage_status: Default::default(),
            auto_changes_open: false,
            system_dark: false,
            // CAPTUREFAB_APPEARANCE=light|dark overrides the system, e.g. for screenshots.
            appearance: match std::env::var("CAPTUREFAB_APPEARANCE").as_deref() {
                Ok("light") => Appearance::Light,
                Ok("dark") => Appearance::Dark,
                _ => prefs.appearance,
            },
            ui_scale: match std::env::var("CAPTUREFAB_UI_SCALE").as_deref() {
                Ok("large") => UiScale::Large,
                Ok("larger") => UiScale::Larger,
                _ => prefs.ui_scale,
            },
            screenshot: None,
            logs_open: prefs.logs_open,
            help_open: false,
            network_open: false,
            network_error: None,
            network_scrolled: false,
            analytics: Default::default(),
            video_analytics,
            settings_open: false,
            menu_editor: false,
            popup_open: false,
            menu_edit: None,
            desktop_menu: Default::default(),
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
            camera_views: HashMap::new(),
            throughput: HashMap::new(),
            liveness: HashMap::new(),
            pending: Vec::new(),
            batches: HashMap::new(),
            notice: None,
            notice_shown: Motion::new(0.0, motion::NOTICE_IN, motion::NOTICE_OUT, Kind::Fade),
            copied: None,
            last_saved: None,
            last_recording: None,
            auto_start: prefs.auto_start,
            tags: prefs.tags,
            dataset,
            signals,
            recent: prefs.recent,
            saved,
            prefs_saver: prefs::Saver::new(),
            closing: None,
            close_error: None,
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
            inspector_summoned: false,
            stashed_record: None,
            stashed_stream: String::new(),
            stream_draft,
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
                shown(prefs.inspector_open && !quiet),
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
            auto_capture: Default::default(),
            sheets: Default::default(),
        };
        bench.restore_open_sections(&open_sections);
        bench
    }

    fn prefs(&self) -> Prefs {
        Prefs {
            appearance: self.appearance,
            ui_scale: self.ui_scale,
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
            scan_timeout_ms: self.scan_timeout_ms,
            schedule_delay_seconds: self.schedule_delay_seconds,
            schedule_interval_seconds: self.schedule_interval_seconds,
            forward_output: if self.forward_streams()
                && Output::of(&self.forward_output).recording()
            {
                // Stream mode with no URL yet (empty or no scheme): keep the
                // recording template it replaced, so the next launch opens
                // Record with it rather than an empty or half-typed path.
                self.stashed_record
                    .as_ref()
                    .map(|(output, _)| output.clone())
                    .unwrap_or_else(prefs::default_record_output)
            } else {
                self.forward_output.clone()
            },
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
            video_analytics: self.video_analytics.preferences.clone(),
            auto_start: self.auto_start,
            tags: self.tags.clone(),
            dataset_path: self.dataset.path.clone(),
            dataset_auto: self.dataset.auto,
            recent_signals: self.signals.recent.clone(),
            open_sections: self.open_section_keys(),
            // Not yet mirrored on the workbench: keep what was loaded.
            ..self.saved.clone().unwrap_or_default()
        }
    }

    fn save_prefs(&mut self) {
        if self.closing.is_some() {
            return;
        }
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
            if let Err(error) = self.prefs_saver.save(prefs) {
                self.set_notice("Could not save settings", Some(error), Level::Error);
            }
        }
    }

    fn close(&mut self, target: CloseTarget) -> Task<Message> {
        if self.closing.is_some() {
            return Task::none();
        }
        self.dismiss_close_error();
        self.closing = Some(target);
        if self.saved.is_none() {
            Task::done(Message::PrefsFlushed(Ok(())))
        } else {
            Task::perform(self.prefs_saver.flush(self.prefs()), Message::PrefsFlushed)
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

    fn window_palette(&self, id: window::Id) -> &'static Palette {
        Palette::for_window(self.dark(), self.key_window == Some(id))
    }

    fn known_window(&self, id: window::Id) -> bool {
        id == self.main_window || self.settings_window == Some(id)
    }

    fn present_window(&self, id: window::Id) -> Task<Message> {
        let fit = if self.settings_window == Some(id) {
            prefs::fit_aux_window(id, settings::window_settings().size, self.ui_scale)
        } else {
            Task::none()
        };
        // EWMH frame extents are supplied after the window manager maps/
        // reparents the client; query them after requesting show.
        let mapped_fit =
            if cfg!(target_os = "linux") && id == self.main_window && self.screenshot.is_none() {
                let [width, height] = self.window.unwrap_or([1280.0, 840.0]);
                prefs::fit_main_window(id, Size::new(width, height), self.ui_scale)
            } else {
                Task::none()
            };
        fit.chain(window::set_mode(id, window::Mode::Windowed))
            .chain(mapped_fit)
            .chain(window::gain_focus(id))
    }

    fn window_theme(&self, id: window::Id) -> Theme {
        style::window_theme(self.dark(), self.key_window == Some(id))
    }

    fn selection_emphasized(&self, area: keyboard_history::Area) -> bool {
        self.key_window == Some(self.main_window)
            && !self.sheet_open()
            && self.close_error.is_none()
            && self.closing.is_none()
            && self.content_focus == area
    }

    fn subscription(&self) -> Subscription<Message> {
        let streaming = self.snapshot.cameras.iter().any(|camera| camera.streaming);
        // Pending commands (connecting, discovering) tick fast enough to
        // step spinners and pulses without per-frame redraws.
        let busy = !self.pending.is_empty()
            || self
                .snapshot
                .activity
                .iter()
                .any(|entry| entry.state.active())
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
            Subscription::run_with(ActivityWatch(self.handle.clone()), watch_activity),
            frames,
            arrivals,
            time::every(interval).map(|_| Message::Tick),
            event::listen_with(|event, status, window| {
                let message = match event {
                iced::Event::Window(window::Event::Focused) => Some(Message::WindowFocused(window)),
                iced::Event::Window(window::Event::Unfocused) => Some(Message::WindowUnfocused(window)),
                iced::Event::Window(window::Event::CloseRequested) => {
                    Some(Message::CloseRequested(window))
                }
                iced::Event::Window(
                    window::Event::Opened { .. }
                    | window::Event::Moved(_)
                    | window::Event::Rescaled(_),
                ) => Some(Message::Displaced),
                // The widget guard routes Escape after inspecting the active
                // popup. A captured popup Escape closes only that popup.
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    modifiers,
                    ..
                }) if modifiers.is_empty() => None,
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
                };
                message.map(|message| Message::InWindow(window, Box::new(message)))
            }).with(self.desktop_menu.active()).filter_map(|(active, message)| {
                if active && matches!(&message, Message::InWindow(_, inner) if matches!(**inner, Message::Key { .. } | Message::FocusNext(_))) {
                    None
                } else {
                    Some(message)
                }
            }),
            system::theme_changes().map(Message::SystemTheme),
            window::resize_events().map(|(id, size)| Message::InWindow(id, Box::new(Message::Resized(size)))),
            motion::accessibility_subscription(),
            menus::subscription(),
            self.accessibility.subscription().map(Message::AccessibilityRequest),
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
        if let Job::Set(feature) = &job
            && camera.is_none_or(|id| self.snapshot.active_camera.as_deref() == Some(id))
        {
            self.inspect
                .write_errors
                .insert(feature.clone(), format!("{error:#}"));
        }
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
        self.validate_input(&command)?;
        if let SessionCommand::Set { feature, value } = &command
            && camera.is_none_or(|id| self.snapshot.active_camera.as_deref() == Some(id))
            && let Some(info) = self
                .snapshot
                .features
                .iter()
                .find(|info| info.name == *feature)
        {
            info.validate_input(value)?;
        }
        let acts_on = match (camera, &command) {
            (Some(camera), _) => Some(camera.to_owned()),
            (
                None,
                SessionCommand::Connect { .. }
                | SessionCommand::Discover { .. }
                | SessionCommand::NetworkScan { .. }
                | SessionCommand::Status,
            ) => None,
            (None, SessionCommand::Select { camera }) => Some(camera.clone()),
            // The session answers dataset commands itself.
            (None, command) if command.is_dataset() => None,
            (None, _) => self.snapshot.active_camera.clone(),
        };
        let value = match &command {
            SessionCommand::Set { value, .. } => Some(value.clone()),
            _ => None,
        };
        // Pin every camera operation at submission. An external selection
        // between rendering and execution must never redirect a human edit.
        let target = match &command {
            SessionCommand::Connect { .. }
            | SessionCommand::Select { .. }
            | SessionCommand::Discover { .. }
            | SessionCommand::NetworkScan { .. }
            | SessionCommand::Status => None,
            _ => acts_on.as_deref(),
        };
        let selected = match &command {
            SessionCommand::Select { camera } => Some(camera.clone()),
            _ => None,
        };
        let receiver = match target {
            Some(camera) => {
                self.handle
                    .submit_to_as(camera, command, ActivityOrigin::Gui, self.actor.clone())
            }
            None => self
                .handle
                .submit_as(command, ActivityOrigin::Gui, self.actor.clone()),
        }?;
        if let Some(camera) = selected {
            self.local_camera = Some(camera);
        }
        self.give_way();
        self.pending.push(Pending {
            receiver,
            camera: acts_on,
            value,
            at: self.now,
            ..Pending::unanswered(job)
        });
        Ok(self.pending.last_mut().expect("just pushed"))
    }

    /// A first run before any camera: nothing connected, connecting or
    /// remembered. The docked inspector stays out of the way until then.
    fn first_run_quiet(&self) -> bool {
        self.recent.is_empty() && self.snapshot.cameras.is_empty() && !self.pending(Job::Connect)
    }

    /// Whether a command doing `job` is on its way, to any camera.
    fn pending(&self, job: Job) -> bool {
        self.pending.iter().any(|p| p.job == job)
            || job != Job::Capture && self.client_pending(&job, None)
    }

    /// Whether a command doing `job` is on its way to `camera`.
    fn pending_for(&self, job: Job, camera: &str) -> bool {
        self.pending
            .iter()
            .any(|p| p.job == job && p.camera.as_deref() == Some(camera))
            || job != Job::Capture && self.client_pending(&job, Some(camera))
    }

    fn capturing_here(&self) -> bool {
        self.pending.iter().any(|pending| {
            pending.job == Job::Capture
                && (pending.camera.is_none() || pending.camera == self.snapshot.active_camera)
        })
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
            .or_else(|| {
                if self.client_pending(&Job::Stop, Some(camera)) {
                    Some(true)
                } else if self.client_pending(&Job::Start, Some(camera)) {
                    Some(false)
                } else {
                    None
                }
            })
    }

    /// While a forward or recording starts or stops, whether it stops.
    fn forward_pending(&self) -> Option<bool> {
        self.pending
            .iter()
            .find_map(|p| match p.job {
                Job::Forward { .. } => Some(false),
                Job::StopForward { .. } => Some(true),
                _ => None,
            })
            .or_else(|| {
                if self.client_pending(&Job::StopForward { recording: false }, None) {
                    Some(true)
                } else if self.client_pending(&Job::Forward { recording: false }, None) {
                    Some(false)
                } else {
                    None
                }
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
            || self.client_pending(&Job::AutoOn, None)
            || self.client_pending(&Job::AutoOff, None)
    }

    /// Whether auto mode is being switched or tuned on the selected camera,
    /// which its Auto controls go by. Each camera has its own worker, so
    /// another camera's command never holds them up.
    fn auto_busy_here(&self) -> bool {
        let camera = self.snapshot.active_camera.as_deref();
        camera.is_some()
            && (self
                .pending
                .iter()
                .any(|p| p.job.is_auto() && p.camera.as_deref() == camera)
                || self.client_pending(&Job::AutoOn, camera)
                || self.client_pending(&Job::AutoOff, camera))
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
            // In the order sent, so a newer write of a feature settles
            // after an older one (see `settles_row`).
            let pending = self.pending.remove(i);
            self.settle(&pending, &result);
        }
    }

    /// Everything that follows a finished command: the shared bookkeeping,
    /// each area's, then its notice.
    fn settle(&mut self, pending: &Pending, result: &Result<serde_json::Value>) {
        if self.camera_history_result(pending, result) {
            return;
        }
        match result {
            Ok(value) => self.finished(pending, value),
            Err(error) => self.failed(pending, error),
        }
        self.result_side(pending, result);
        self.result_stage(pending, result);
        self.result_inspector(pending, result);
        self.result_autocapture(pending, result);
        self.dataset_settled(pending, result);
        self.signals_settled(pending, result);
        let notice = if pending.batch {
            self.batch_result(&pending.job, result)
        } else {
            command_notice(&pending.job, result)
        };
        if let Some((text, detail, level)) = notice {
            self.set_notice(text, detail, level);
        }
        if let Ok(value) = result
            && let Some(reason) = value["history_unavailable"].as_str()
        {
            self.set_notice(
                "Change completed · undo unavailable",
                Some(reason.to_owned()),
                Level::Warning,
            );
        }
    }

    /// Shared bookkeeping for a command that worked, before its notice.
    fn finished(&mut self, pending: &Pending, result: &serde_json::Value) {
        if pending.job == (Job::Forward { recording: true })
            && let Some(path) = result["path"].as_str()
        {
            self.last_recording = Some((pending.camera.clone(), path.to_owned()));
        }
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
        if pending.job == Job::NetworkScan {
            self.network_error = None;
            if let Ok(report) = serde_json::from_value(result["network_scan"].clone()) {
                self.snapshot.network_scan = Some(report);
            }
        }
        if let Some(feature) = pending.job.feature()
            && self.settles_row(pending)
        {
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
        self.local_camera = Some(camera.id.clone());
        if pending.job == Job::Connect && self.starts_on_connect(&camera) {
            self.send_to(&camera.id, Job::Start, SessionCommand::Start);
        }
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
        if pending.job == Job::NetworkScan {
            self.network_error = Some(full.clone());
        }
        if let Some(feature) = pending.job.feature()
            && self.settles_row(pending)
        {
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
        self.project_local_camera();
        self.scene.patch(&mut self.snapshot);
        self.sync_camera_history();
        if self.snapshot.active_camera != selected {
            self.ring_from = selected;
            self.ring.replay(0.0, 1.0, self.now);
        }
        self.sync_shared_editors();
        if let Some(status) = &self.snapshot.auto
            && !self.balance_dragging
            && !self.auto_busy_here()
        {
            self.balance = status.balance;
        }
        self.sync_client_activity();
        self.sample_throughput();
        self.observe_liveness();
        self.update_frames();
        let destination_before = local_history::LocalState::capture(self);
        self.capture_to.tick(&mut self.output);
        self.record_to.tick(&mut self.forward_output);
        self.record_local_change(
            "Choose output folder".into(),
            None,
            destination_before,
            false,
        );
        let pages = self.pickers_to_top();
        self.tick_chrome();
        self.tick_side();
        self.tick_stage();
        self.tick_inspector();
        self.tick_analytics();
        self.tick_video_analytics();
        self.tick_autocapture();
        self.tick_signals();
        self.save_prefs();
        Task::batch([
            pages,
            self.screenshot_tick(),
            if self.screenshot.is_none() {
                self.storage_status
                    .refresh(self.now)
                    .map(Message::StoragePolicy)
            } else {
                Task::none()
            },
        ])
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
        self.camera_views.retain(|id, _| {
            self.snapshot
                .cameras
                .iter()
                .any(|camera| &camera.info.id == id)
        });
        if self.overview() {
            self.update_previews();
            if self.frame_id.take().is_some() {
                self.gpu.retire(MAIN_VIEW);
            }
        } else {
            self.update_frame(true);
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
            let state = self.camera_views.entry(camera.info.id.clone()).or_default();
            let shown = camera_view::present(
                gpu,
                &mut preview.shown,
                state,
                (camera.info.profile.kind, &camera.features),
                &camera.info.id,
                frame,
                (640, 480),
            );
            match shown {
                Ok(()) => {
                    preview.meta = Some(meta);
                    preview.error = None;
                }
                Err(error) => preview.error = Some(error.to_string()),
            }
        }
    }

    fn update_frame(&mut self, arrival: bool) {
        let Some(frame) = self.selected_frame() else {
            return;
        };
        if Some(frame.id) == self.frame_id {
            return;
        }
        self.frame_id = Some(frame.id);
        if arrival && let Some(id) = &self.observed_camera {
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
        let kind = self.camera_kind();
        let id = self.snapshot.active_camera.clone().unwrap_or_default();
        let state = self.camera_views.entry(id).or_default();
        let shown = camera_view::present(
            &self.gpu,
            &mut self.shown,
            state,
            (kind, &self.snapshot.features),
            MAIN_VIEW,
            frame,
            (u32::MAX, u32::MAX),
        );
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
        if !self.intensity_image() || self.camera_kind() == CameraKind::LineScan {
            self.exposure = None;
            self.focus.reset();
            return;
        }
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
        if let Some(frame) = self.selected_frame() {
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
        self.side.connect_failures.remove(&camera);
        let command = SessionCommand::Connect {
            camera: camera.clone(),
            timeout_ms: 5000,
        };
        if let Err(error) = command.validate() {
            self.side
                .connect_failed(&camera, format!("{error:#}"), self.now);
            return;
        }
        if let Some(pending) = self.submit(
            None,
            Job::Connect,
            SessionCommand::Connect {
                camera: camera.clone(),
                timeout_ms: 5000,
            },
        ) {
            pending.target = Some(camera);
            self.edits.clear();
            self.edit_sources.clear();
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

    fn scanner_selected(&self) -> bool {
        self.snapshot
            .connected
            .as_ref()
            .is_some_and(|camera| camera.transport == crate::types::Transport::Scanner)
    }

    fn camera_kind(&self) -> CameraKind {
        self.snapshot
            .connected
            .as_ref()
            .map_or(CameraKind::Unknown, |camera| camera.profile.kind)
    }

    fn intensity_image(&self) -> bool {
        !matches!(self.camera_kind(), CameraKind::Depth | CameraKind::Thermal)
    }

    fn capture_timeout(&self, target: Option<&str>) -> u64 {
        let scanner = match target {
            Some(id) => self.snapshot.cameras.iter().any(|camera| {
                camera.info.id == id && camera.info.transport == crate::types::Transport::Scanner
            }),
            None => self.scanner_selected(),
        };
        if scanner {
            self.scan_timeout_ms
        } else {
            self.timeout_ms
        }
    }

    fn capture(&mut self) {
        if self.capturing_here() {
            return;
        }
        self.send(Job::Capture, self.capture_command(false));
    }

    fn capture_command(&self, scheduled: bool) -> SessionCommand {
        self.capture_command_for(scheduled, None)
    }

    /// The capture or schedule the Capture settings describe, for `target`
    /// or the selected camera. Names never collide: taken ones are numbered.
    fn capture_command_for(&self, scheduled: bool, target: Option<&str>) -> SessionCommand {
        let context = tags::context(&self.tags);
        let on_exists = crate::naming::OnExists::Number;
        if scheduled {
            SessionCommand::Schedule {
                output: self.output.clone(),
                count: self.count,
                timeout_ms: self.capture_timeout(target),
                format: self.format.into(),
                first_at_ms: epoch_ms()
                    .saturating_add(self.schedule_delay_seconds.saturating_mul(1000)),
                interval_ms: (self.schedule_interval_seconds * 1000.0).round() as u64,
                storage: self.storage_policy(),
                destination: self.capture_to.selected.clone(),
                on_exists,
                context,
            }
        } else {
            SessionCommand::Capture {
                output: self.output.clone(),
                count: self.count,
                timeout_ms: self.capture_timeout(target),
                format: self.format.into(),
                storage: self.storage_policy(),
                destination: self.capture_to.selected.clone(),
                on_exists,
                context,
            }
        }
    }

    fn forward_command(&self) -> SessionCommand {
        SessionCommand::Forward {
            output: self.forward_output.clone(),
            codec: self.forward_codec.into(),
            encoder: self.forward_encoder.trim().into(),
            fps: self.forward_fps,
            bitrate: self.forward_bitrate.trim().into(),
            storage: self.storage_policy(),
            max_file_bytes: (self.forward_file_mib * 1024.0 * 1024.0).round() as u64,
            destination: self.record_to.selected.clone(),
            on_exists: crate::naming::OnExists::Number,
            context: tags::context(&self.tags),
            // The Video panel and Record button run until stopped.
            duration_ms: None,
        }
    }

    /// Record to a local file: the Video output when it is one, else the
    /// default recording folder. With `per_camera`, the output must name the
    /// camera, so that cameras recording at once never race for one name.
    fn record_command(&self, per_camera: bool) -> SessionCommand {
        let mut command = self.forward_command();
        // An unfinished stream URL is not a recording path either.
        let keep = !self.forward_streams()
            && Output::of(&self.forward_output).recording()
            && (!per_camera || self.forward_output.contains("{camera}"));
        if !keep
            && let SessionCommand::Forward {
                output,
                destination,
                ..
            } = &mut command
        {
            *output = prefs::default_record_output();
            *destination = None;
        }
        command
    }

    /// Whether the selected camera records to a local file now.
    fn recording_here(&self) -> bool {
        self.snapshot
            .forwarding
            .as_deref()
            .is_some_and(|output| Output::of(output).recording())
    }

    /// Start recording the selected camera, or stop its recording.
    fn record(&mut self) {
        if self.forward_pending().is_some() || self.scanner_selected() {
            return;
        }
        if self.recording_here() {
            self.send(
                Job::StopForward { recording: true },
                SessionCommand::StopForward,
            );
        } else if self.snapshot.forwarding.is_none() {
            self.send(Job::Forward { recording: true }, self.record_command(false));
        }
    }

    /// Whether to start `camera`'s live view as it connects.
    fn starts_on_connect(&self, camera: &CameraInfo) -> bool {
        self.auto_start
            && camera.transport != crate::types::Transport::Scanner
            && camera.profile.kind != CameraKind::DocumentScanner
            && !self
                .snapshot
                .cameras
                .iter()
                .any(|c| c.info.id == camera.id && c.streaming)
    }

    /// What the capture output saves to next, `{seq}` shown as `####`.
    fn output_preview(&self) -> String {
        self.preview_of(&self.output)
    }

    /// What `template` saves to next, `{seq}` shown as `####`, under `~`
    /// when it is in the home folder.
    fn preview_of(&self, template: &str) -> String {
        self.preview_at(template, chrono::Local::now())
    }

    /// `preview_of` at the time `now`.
    fn preview_at(&self, template: &str, now: chrono::DateTime<chrono::Local>) -> String {
        let camera = self
            .snapshot
            .connected
            .as_ref()
            .map_or_else(|| "camera".to_owned(), crate::naming::camera_label);
        let preview = crate::naming::preview(template, &camera, now);
        let home = prefs::capture_root();
        match home
            .parent()
            .and_then(|home| std::path::Path::new(&preview).strip_prefix(home).ok())
        {
            Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
            None => preview,
        }
    }

    /// Validate the visible draft, including shortcuts and per-camera actions.
    /// A stale accepted number must never replace invalid text on submission.
    fn validate_input(&self, command: &SessionCommand) -> Result<()> {
        let mut keys = Vec::new();
        match command {
            SessionCommand::Capture { .. } | SessionCommand::Schedule { .. } => {
                keys.extend([Num::Count, Num::Timeout, Num::QuotaGib, Num::QuotaFiles]);
                if matches!(command, SessionCommand::Schedule { .. }) {
                    keys.extend([Num::Delay, Num::Interval]);
                }
                if self.retention_enabled {
                    keys.push(Num::RetentionDays);
                }
            }
            SessionCommand::Forward { output, .. } => {
                keys.push(Num::Fps);
                if Output::of(output).recording() {
                    keys.extend([Num::FileMib, Num::QuotaGib, Num::QuotaFiles]);
                    if self.retention_enabled {
                        keys.push(Num::RetentionDays);
                    }
                }
            }
            _ => {}
        }
        for key in keys {
            let value = self
                .drafts
                .get(&key)
                .cloned()
                .unwrap_or_else(|| self.num_text(key));
            anyhow::ensure!(num_valid(key, &value), "{}", num_requirement(key));
        }
        command.validate()
    }

    fn toggle_auto(&mut self) {
        if self.auto_busy_here() {
            return;
        }
        if self.snapshot.auto.is_some() {
            self.send(Job::AutoOff, SessionCommand::Manual { revert: false });
        } else if self.camera_kind().supports_auto() {
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

    /// Open one camera from the overview, selecting it first if needed.
    fn focus_tile(&mut self, camera: String) {
        self.focus_camera = true;
        if self.snapshot.active_camera.as_ref() != Some(&camera) {
            self.send(Job::Select, SessionCommand::Select { camera });
        }
    }

    fn toggle_exposure(&mut self) {
        if !self.intensity_image() || self.camera_kind() == CameraKind::LineScan {
            return;
        }
        self.exposure_open = !self.exposure_open;
        if self.exposure_open
            && let Some(frame) = self.selected_frame()
        {
            self.exposure = frame::exposure(&frame).ok();
        }
    }

    fn toggle_focus_region(&mut self) {
        if !self.intensity_image() || self.camera_kind() == CameraKind::LineScan {
            return;
        }
        self.focus.open = !self.focus.open;
        self.focus.reset();
        if self.focus.open
            && let Some(frame) = self.selected_frame()
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
        self.zoom = (from * factor).clamp(stage::ZOOM_MIN, stage::ZOOM_MAX);
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
            // With the camera list closed, its session control cannot
            // confirm in place, so the toolbar must, even over an error.
            // Image mode hides the toolbar too, so the error stays.
            if !self.sidebar_open && !self.image_mode {
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
            Num::Timeout => self.capture_timeout(None).to_string(),
            Num::Delay => self.schedule_delay_seconds.to_string(),
            Num::Interval => self.schedule_interval_seconds.to_string(),
            Num::Fps => self.forward_fps.to_string(),
            Num::FileMib => self.forward_file_mib.to_string(),
            Num::QuotaGib => self.quota_gib.to_string(),
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
            (Num::Timeout, Some(v), _) => {
                if self.scanner_selected() {
                    self.scan_timeout_ms = v;
                } else {
                    self.timeout_ms = v;
                }
            }
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
        // A native edit must wait for the normal validation/history update,
        // even when that update rejects it while closing. Preserve its window
        // context and acknowledge before the next actual widget collection.
        let message = match message {
            Message::NativeInput {
                message,
                acknowledgement,
            } => {
                if !acknowledgement.is_current() {
                    return Task::none();
                }
                let task = self.update(*message);
                acknowledgement.acknowledge();
                return task;
            }
            Message::InWindow(id, message) => match *message {
                Message::NativeInput {
                    message,
                    acknowledgement,
                } => {
                    if !acknowledgement.is_current() {
                        return Task::none();
                    }
                    let task = self.update(Message::InWindow(id, message));
                    acknowledgement.acknowledge();
                    return task;
                }
                message => Message::InWindow(id, Box::new(message)),
            },
            message => message,
        };
        if self.closing.is_some()
            && !matches!(
                message,
                Message::PrefsFlushed(_) | Message::StoragePolicy(_)
            )
        {
            // Cleanup still matters while a preference flush is pending: it
            // can fail and keep the daemon alive after the window has retired.
            if matches!(&message,
                Message::WindowOpened(id)
                    | Message::AccessibilityAttached(id, _)
                    | Message::AccessibilityRecoveryAttached(id)
                    if !self.known_window(*id)
            ) {
                return self.handle_message(message);
            }
            return Task::none();
        }
        let message = match message {
            Message::InWindow(id, message) => {
                // Settings leaves the model before its asynchronous native
                // close finishes. Ignore queued widget/geometry reports from
                // that retired window, after acknowledging native edit work.
                if !self.known_window(id) {
                    return Task::none();
                }
                if let Message::EditorFocused(value) = *message {
                    self.window_editor_focus.insert(id, value);
                }
                match *message {
                    Message::EditorFocused(_) | Message::PopupOpen(_)
                        if id != self.focused_window =>
                    {
                        return Task::none();
                    }
                    Message::ContentFocused(_) if id != self.main_window => {
                        return Task::none();
                    }
                    Message::Resized(_) if id != self.main_window => {
                        return Task::none();
                    }
                    Message::Displaced if id != self.main_window => {
                        return window::scale_factor(id)
                            .map(move |scale| Message::WindowScale(id, scale));
                    }
                    _ => {}
                }
                *message
            }
            message => message,
        };
        self.now = Instant::now();
        let scale_before = self.ui_scale;
        let sheet_before = self.sheet_open();
        let menus_changed = !matches!(
            message,
            Message::Frame(_)
                | Message::FrameArrived
                | Message::Pointer
                | Message::OverControls(_)
                | Message::HoverTile(_)
        );
        // Apply ready completions before capturing this gesture. A destination
        // replay must never overwrite input before its history entry exists.
        self.poll_destination_history();
        let gesture = local_history::gesture(&message);
        if local_history::group_barrier(&message) {
            self.break_history_group();
        }
        let before = gesture
            .as_ref()
            .map(|_| local_history::LocalState::capture(self));
        let reset = matches!(message, Message::ResetFocusPeak);
        let mut task = self.handle_message(message);
        if let (Some((label, key)), Some(before)) = (gesture, before) {
            self.record_local_change(label, key, before, reset);
        }
        self.poll_destination_history();
        if !sheet_before && self.sheet_open() {
            self.desktop_menu = Default::default();
        }
        if sheet_before != self.sheet_open() {
            self.accessibility.invalidate(self.main_window);
        }
        if scale_before != self.ui_scale {
            let auxiliary = self.settings_window.map_or_else(Task::none, |id| {
                prefs::fit_aux_window(id, settings::window_settings().size, self.ui_scale)
            });
            task = Task::batch([
                task,
                window::size(self.main_window).map(Message::ScaledViewport),
                auxiliary,
            ]);
        }
        self.sync_animations();
        self.frame_gap
            .store(self.frame_gap().as_nanos() as u64, Ordering::Relaxed);
        if menus_changed {
            menus::sync(self);
        }
        task
    }

    fn handle_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::InWindow(_, _) | Message::NativeInput { .. } => {
                unreachable!("window and native edit messages are unwrapped before update")
            }
            Message::WindowOpened(id)
            | Message::AccessibilityAttached(id, _)
            | Message::AccessibilityRecoveryAttached(id)
                if !self.known_window(id) =>
            {
                // An earlier attach may finish after Settings(false) has
                // already detached/closed its native window. Run cleanup on
                // the live main UI callback: iced drops Run callbacks whose
                // target window no longer exists. Never show/focus this ID.
                return self
                    .accessibility
                    .detach_in_window(id, self.main_window)
                    .discard()
                    .chain(window::close(id));
            }
            Message::AccessibilityChanged(id)
            | Message::WindowScale(id, _)
            | Message::WindowFocused(id)
            | Message::WindowUnfocused(id)
            | Message::CloseRequested(id)
                if !self.known_window(id) =>
            {
                return Task::none();
            }
            Message::AccessibilityRequest(request) if !self.known_window(request.window) => {
                return Task::none();
            }
            Message::WindowOpened(id) => {
                return Task::batch([
                    self.accessibility
                        .attach(id)
                        .map(move |ok| Message::AccessibilityAttached(id, ok)),
                    window::scale_factor(id).map(move |scale| Message::WindowScale(id, scale)),
                ]);
            }
            Message::AccessibilityAttached(id, attached) => {
                if !attached {
                    self.set_notice("Accessibility connection unavailable", None, Level::Error);
                }
                return self.present_window(id);
            }
            Message::AccessibilityRecoveryAttached(id) => return self.recovery_attached(id),
            Message::CloseErrorMode(id, mode) => return self.present_close_error(id, mode),
            Message::AccessibilityChanged(id) => return self.accessibility.sync(id).discard(),
            Message::AccessibilityRequest(request) => {
                let id = request.window;
                return self
                    .accessibility
                    .dispatch(request)
                    .map(move |message| Message::InWindow(id, Box::new(message)));
            }
            Message::WindowScale(id, scale) => {
                self.window_scales.insert(id, scale);
            }
            Message::WindowFocused(id) => {
                if let Some(error) = &self.close_error
                    && id != error.window
                {
                    return window::gain_focus(error.window);
                }
                self.focused_window = id;
                self.key_window = Some(id);
                self.menu_editor = self.window_editor_focus.get(&id).copied().unwrap_or(false);
                self.popup_open = false;
                return self.accessibility.focused(id, true).discard();
            }
            Message::WindowUnfocused(id) => {
                if self.key_window == Some(id) {
                    self.key_window = None;
                }
                self.desktop_menu = Default::default();
                return self.accessibility.focused(id, false).discard();
            }
            Message::CloseRequested(_) if self.close_error.is_some() => {
                self.dismiss_close_error();
            }
            Message::CloseRequested(id) if self.settings_window == Some(id) => {
                return self.handle_message(Message::Settings(false));
            }
            Message::CloseRequested(id) if id == self.main_window && self.sheet_open() => {
                return self.dispatch_action(Action::Overview);
            }
            Message::CloseRequested(id) => return self.close(CloseTarget::Window(id)),
            Message::Quit => return self.close(CloseTarget::Quit),
            Message::PrefsFlushed(result) => {
                let target = self.closing.take();
                match result {
                    Ok(()) => {
                        return match target {
                            Some(target) => {
                                if let CloseTarget::Window(id) = target {
                                    debug_assert_eq!(id, self.main_window);
                                }
                                let mut detach =
                                    vec![self.accessibility.detach(self.main_window).discard()];
                                if let Some(id) = self.settings_window {
                                    detach.push(self.accessibility.detach(id).discard());
                                }
                                Task::batch(detach).chain(iced::exit())
                            }
                            None => Task::none(),
                        };
                    }
                    Err(error) => {
                        self.set_notice(
                            "Could not save settings",
                            Some(error.clone()),
                            Level::Error,
                        );
                        if let Some(target) = target {
                            return self.show_close_error(target, error);
                        }
                    }
                }
            }
            Message::RetryClose => {
                if let Some(error) = &self.close_error {
                    return self.close(error.target);
                }
            }
            Message::KeepOpen => self.dismiss_close_error(),
            Message::Menu(command) => return self.menu_command(command),
            Message::StoragePolicy(reply) => self.storage_status.update(reply, self.now),
            Message::DesktopMenu(input) => return self.desktop_menu_input(input),
            Message::EditorFocused(value) => {
                self.window_editor_focus.insert(self.focused_window, value);
                self.menu_editor = value;
            }
            Message::ContentFocused(area) => self.content_focus = area,
            Message::PopupOpen(value) => self.popup_open = value,
            Message::Settings(open) => {
                if open {
                    self.video_analytics.refresh_credentials();
                } else {
                    self.video_analytics.clear_secret();
                }
                #[cfg(target_os = "macos")]
                if self.screenshot.is_none() {
                    if !open {
                        self.settings_open = false;
                        self.accessibility.invalidate(self.main_window);
                        if let Some(id) = self.settings_window.take() {
                            self.window_editor_focus.remove(&id);
                            if self.key_window == Some(id) {
                                self.key_window = None;
                            }
                            self.focused_window = self.main_window;
                            self.menu_editor = self
                                .window_editor_focus
                                .get(&self.main_window)
                                .copied()
                                .unwrap_or(false);
                            self.window_scales.remove(&id);
                            return self
                                .accessibility
                                .detach(id)
                                .discard()
                                .chain(window::close(id))
                                .chain(window::gain_focus(self.main_window));
                        }
                    } else if let Some(id) = self.settings_window {
                        return window::gain_focus(id);
                    } else if !self.sheet_open() {
                        let mut settings = settings::window_settings();
                        settings.visible = false;
                        settings.position = prefs::centered_window(self.ui_scale);
                        let (id, opened) = window::open(settings);
                        self.settings_window = Some(id);
                        self.settings_open = true;
                        self.focused_window = id;
                        self.menu_editor =
                            self.window_editor_focus.get(&id).copied().unwrap_or(false);
                        self.accessibility.invalidate(self.main_window);
                        return opened.map(Message::WindowOpened);
                    }
                    return Task::none();
                }
                if !open || !self.sheet_open() {
                    self.settings_open = open;
                    if open {
                        return operation::focus("sheet-close");
                    }
                }
            }
            Message::Appearance(value) => self.appearance = value,
            Message::UiScale(value) => self.ui_scale = value,
            Message::ScaledViewport(size) => {
                self.width = size.width;
                self.height = size.height;
                if !self.inspector_docked() {
                    self.inspector_peek = false;
                }
            }
            Message::Undo => return self.replay_history(true),
            Message::Redo => return self.replay_history(false),
            Message::FullscreenResult {
                before,
                actual,
                expected,
                replay,
            } => {
                self.fullscreen = actual;
                if replay.is_some() {
                    self.history_replayed(actual == expected);
                    if actual != expected {
                        self.set_notice("Could not restore window mode", None, Level::Error);
                    }
                } else if before != actual {
                    self.record_window_change(before, actual);
                }
            }
            Message::Tick => return self.tick(),
            Message::Frame(at) => self.refresh.redraw(at),
            Message::Displaced => {
                self.refresh.recalibrate();
                let id = self.main_window;
                return Task::batch([
                    screen_refresh(id),
                    window::scale_factor(id).map(move |scale| Message::WindowScale(id, scale)),
                ]);
            }
            Message::ScreenRefresh(period) => self.refresh.report(period),
            Message::FrameArrived if !self.shot_staged() => self.update_frames(),
            Message::FrameArrived => {}
            Message::SystemTheme(mode) => self.system_dark = mode == theme::Mode::Dark,
            Message::Key { chord, captured } => {
                self.desktop_menu.cancel_alt();
                if self.review_shortcut(chord, captured) {
                    return Task::none();
                }
                return self.shortcut(chord, !captured);
            }
            Message::FocusNext(back) => {
                use iced::advanced::widget::{
                    Id, Operation, operate,
                    operation::{focusable, scope},
                };
                fn within<T: Send + 'static>(
                    scope_id: &'static str,
                    operation: impl Operation<T> + 'static,
                ) -> Task<T> {
                    operate(scope(Id::new(scope_id), operation))
                }
                let scope_id = if self.close_error.is_some() {
                    "close-error"
                } else if self.settings_window == Some(self.focused_window) {
                    "settings-root"
                } else if self.sheet_open() {
                    "sheet"
                } else {
                    "workbench-root"
                };
                let step = move || {
                    if back {
                        within(scope_id, focusable::focus_previous())
                    } else {
                        within(scope_id, focusable::focus_next())
                    }
                };
                return step()
                    .chain(within(scope_id, focusable::count()).then(move |count| {
                        if count.focused.is_none() && count.total > 0 {
                            step()
                        } else {
                            Task::none()
                        }
                    }))
                    .chain(accessibility::reveal_focus(self.focused_window));
            }
            Message::Help(open) => {
                self.show_help(open);
                if open && self.help_open {
                    return operation::focus("sheet-close");
                }
            }
            Message::DragWindow => return window::drag(self.main_window),
            Message::ZoomWindow => {
                return Task::done(self.main_window).then(|id| {
                    match prefs::double_click_action() {
                        prefs::DoubleClickAction::Minimize => window::minimize(id, true),
                        prefs::DoubleClickAction::None => Task::none(),
                        prefs::DoubleClickAction::Fill => {
                            window::run(id, prefs::fill_window).discard()
                        }
                        prefs::DoubleClickAction::Zoom => window::toggle_maximize(id),
                    }
                });
            }
            Message::Discover => self.discover(),
            Message::ScanNetwork => return self.scan_network(),
            Message::NetworkReport(open) => return self.show_network(open),
            Message::NetworkScrolled(scrolled) => self.network_scrolled = scrolled,
            Message::Analytics(open) => return self.show_analytics(open),
            Message::AnalyticsTab(tab) => return self.analytics_tab(tab),
            Message::AnalyticsScrolled(scrolled) => self.analytics.scrolled = scrolled,
            Message::AnalyticsHistory(input) => return self.analytics_history(input),
            Message::VideoAnalytics(input) => self.video_analytics_input(input),
            Message::IncludeSimulator(value) => {
                self.include_simulator = value;
                self.discover();
            }
            Message::TrySimulator => {
                self.include_simulator = true;
                self.connect("sim:0".into());
            }
            Message::EnterAddress => return self.enter_address(),
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
                let address = if std::path::Path::new(&self.address).is_file() {
                    self.address.clone()
                } else {
                    self.address.trim().to_owned()
                };
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
                    .filter(|camera| {
                        camera.streaming != start
                            && (!start || camera.info.profile.kind != CameraKind::DocumentScanner)
                    })
                    .map(|camera| camera.info.id.clone())
                    .collect();
                if start {
                    self.send_batch(cameras, Job::Start, |_| SessionCommand::Start);
                } else {
                    self.send_batch(cameras, Job::Stop, |_| SessionCommand::Stop);
                }
            }
            Message::AllAuto(auto) => {
                let cameras = self
                    .snapshot
                    .cameras
                    .iter()
                    .filter(|camera| {
                        camera.auto.is_none() == auto
                            && (!auto || camera.info.profile.kind.supports_auto())
                    })
                    .map(|camera| camera.info.id.clone())
                    .collect();
                let balance = Some(self.balance);
                if auto {
                    self.send_batch(cameras, Job::AutoOn, |_| SessionCommand::Auto { balance });
                } else {
                    self.send_batch(cameras, Job::AutoOff, |_| SessionCommand::Manual {
                        revert: false,
                    });
                }
            }
            Message::BalanceCommit(value) => {
                let _ = self.handle_message(Message::Balance(value));
                return self.handle_message(Message::BalanceReleased);
            }
            Message::Fit => self.fit = true,
            Message::Actual => {
                self.fit = false;
                self.zoom = 1.0;
            }
            Message::Zoom(factor) => self.zoom_by(factor),
            Message::ToggleCameraView => {
                if let Some(id) = &self.snapshot.active_camera {
                    self.camera_views.entry(id.clone()).or_default().toggle();
                    self.frame_id = None;
                    self.update_frame(false);
                }
            }
            Message::ToggleExposure => self.toggle_exposure(),
            Message::ToggleFocusRegion => self.toggle_focus_region(),
            Message::FocusRegion(region) => {
                self.focus.region = region;
                self.focus.reset();
                self.remeasure();
            }
            Message::FocusRegionStart => self.break_history_group(),
            Message::FocusMetric(metric) => self.focus.set_metric(metric),
            Message::ResetFocusPeak => {
                self.focus.reset();
                self.remeasure();
            }
            Message::ToggleActivity => self.logs_open = !self.logs_open,
            // Image mode hides the log, so showing it leaves image mode.
            Message::ShowActivity => {
                self.logs_open = true;
                self.image_mode = false;
            }
            Message::CameraActivity(camera) => {
                self.logs_open = true;
                self.image_mode = false;
                if self.snapshot.active_camera.as_ref() != Some(&camera) {
                    self.send(Job::Select, SessionCommand::Select { camera });
                }
            }
            Message::CopyLog => {
                self.note_copied(notice::COPIED_LOG);
                return iced::clipboard::write(self.activity_text());
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
                    self.inspector_summoned = true;
                } else {
                    self.inspector_peek = true;
                }
                return operation::focus(format!("inspector-tab-{}", tab as usize));
            }
            Message::Search(value) => self.search = value,
            Message::InspectorDisclosure(section) => self.toggle_inspector_section(section),
            Message::ExpandFeatureGroups(open) => self.expand_feature_groups(open),
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
            Message::SliderReleased(feature) => {
                if self.edits.contains_key(&feature) {
                    return self.handle_message(Message::Commit(feature));
                }
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
            Message::Schedule => self.send(Job::Schedule, self.capture_command(true)),
            Message::ToggleStorage => self.storage_open = !self.storage_open,
            Message::OnFull(choice) => self.quota_action = choice.value,
            Message::RetentionEnabled(value) => self.retention_enabled = value,
            Message::RefreshJobs => self.send(Job::RefreshJobs, SessionCommand::Jobs),
            Message::CancelJob(id) => self.send(Job::Cancel, SessionCommand::CancelJob { id }),
            Message::Codec(codec) => self.forward_codec = codec.value,
            Message::Encoder(value) => self.forward_encoder = value,
            Message::Bitrate(value) => self.forward_bitrate = value,
            Message::StartForward => {
                let output = Output::of(&self.forward_output);
                if self.forward_pending() != Some(false) {
                    self.send(
                        Job::Forward {
                            recording: output.recording(),
                        },
                        self.forward_command(),
                    );
                }
            }
            Message::ForwardMode(stream) => self.forward_mode(stream),
            Message::StopForward => {
                let recording = self
                    .snapshot
                    .forwarding
                    .as_deref()
                    .is_some_and(|output| Output::of(output).recording());
                self.send(Job::StopForward { recording }, SessionCommand::StopForward);
            }
            Message::CapturePicker(message) => return self.picker(false, message),
            Message::AutoCapture(input) => return self.update_autocapture(input),
            Message::RecordPicker(message) => {
                // Typing never switches between Record and Stream: once the
                // form streams, it keeps streaming until the switch says so.
                if matches!(message, destinations::Message::Output(_)) && self.forward_streams() {
                    self.stream_draft = true;
                }
                return self.picker(true, message);
            }
            Message::ToggleSidebar => {
                self.image_mode = false;
                self.sidebar_open = !self.sidebar_open;
            }
            Message::ToggleInspector => {
                self.image_mode = false;
                if self.inspector_docked() && self.first_run_quiet() && !self.inspector_summoned {
                    // Hidden only for the quiet first run: show it.
                    self.inspector_summoned = true;
                    self.inspector_open = true;
                } else if self.inspector_docked() {
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
                return Task::done(self.main_window).then(move |id| {
                    window::is_maximized(id).then(move |maximized| {
                        window::size(id).then(move |native_size| {
                            window::mode(id).map(move |mode| {
                                Message::WindowMode(mode, (!maximized).then_some(native_size))
                            })
                        })
                    })
                });
            }
            Message::WindowMode(mode, size) => {
                self.fullscreen = mode == window::Mode::Fullscreen;
                if let (window::Mode::Windowed, Some(size)) = (mode, size) {
                    self.window = Some([
                        size.width * self.ui_scale.factor(),
                        size.height * self.ui_scale.factor(),
                    ]);
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
            Message::FocusTile(camera) => self.focus_tile(camera),
            Message::ShowScopeOn(camera, scope) => {
                match scope {
                    Scope::Histogram if !self.exposure_open => self.toggle_exposure(),
                    Scope::Focus if !self.focus.open => self.toggle_focus_region(),
                    _ => {}
                }
                self.focus_tile(camera);
            }
            Message::CaptureCamera(id) => {
                if !self.pending_for(Job::Capture, &id) {
                    let command = self.capture_command_for(false, Some(&id));
                    self.send_to(&id, Job::Capture, command);
                }
            }
            Message::AllCapture => {
                let cameras = self
                    .snapshot
                    .cameras
                    .iter()
                    .map(|camera| camera.info.id.clone())
                    .filter(|id| !self.pending_for(Job::Capture, id))
                    .collect();
                let commands: HashMap<String, SessionCommand> = self
                    .snapshot
                    .cameras
                    .iter()
                    .map(|camera| {
                        let id = camera.info.id.clone();
                        let command = self.capture_command_for(false, Some(&id));
                        (id, command)
                    })
                    .collect();
                self.send_batch(cameras, Job::Capture, |id| commands[id].clone());
            }
            Message::AllRecord(start) => {
                let cameras = self
                    .snapshot
                    .cameras
                    .iter()
                    .filter(|camera| {
                        let recording = camera
                            .forwarding
                            .as_deref()
                            .is_some_and(|output| Output::of(output).recording());
                        if start {
                            camera.forwarding.is_none()
                                && camera.info.transport != crate::types::Transport::Scanner
                                && camera.info.profile.kind != CameraKind::DocumentScanner
                        } else {
                            recording
                        }
                    })
                    .map(|camera| camera.info.id.clone())
                    .collect();
                if start {
                    let command = self.record_command(true);
                    self.send_batch(cameras, Job::Forward { recording: true }, |_| {
                        command.clone()
                    });
                } else {
                    self.send_batch(cameras, Job::StopForward { recording: true }, |_| {
                        SessionCommand::StopForward
                    });
                }
            }
            Message::Record => self.record(),
            Message::Reveal(path) => {
                if std::fs::symlink_metadata(&path).is_err() {
                    self.set_notice(
                        "The file is no longer on this computer (it may have been uploaded)",
                        Some(path.to_string_lossy().into_owned()),
                        Level::Warning,
                    );
                } else if let Err(error) = reveal::reveal(&path) {
                    self.set_notice(
                        format!("Could not open the file manager: {error}"),
                        None,
                        Level::Error,
                    );
                }
            }
            Message::AutoStart(on) => self.auto_start = on,
            Message::Tag(field, value) => field.set(&mut self.tags, value),
            Message::Dataset(message) => return self.dataset_update(message),
            Message::Signals(message) => return self.signals_update(message),
            Message::Screenshot(shot) => {
                if let Some(request) = &mut self.screenshot {
                    let path = request.sequence.path(&request.path);
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
            Message::HelpMore(open) => return self.show_help_notes(open),
        }
        Task::none()
    }

    fn shortcut(&mut self, chord: Chord, keyboard_free: bool) -> Task<Message> {
        let modal = self.sheet_open() || self.close_error.is_some();
        // In a sheet Esc cancels or closes it, even from the field it
        // focused on opening, which took the key to leave itself.
        let keyboard_free = keyboard_free
            || (modal
                && !self.popup_open
                && chord.key == keys::Key::Escape
                && chord.mods == keys::Mods::NONE);
        let Some(action) = Action::find(&chord, keyboard_free, Os::CURRENT) else {
            return Task::none();
        };
        self.dispatch_action(action)
    }

    pub(super) fn dispatch_action(&mut self, action: Action) -> Task<Message> {
        if action == Action::Quit {
            return self.handle_message(Message::Quit);
        }
        if self.close_error.is_some() {
            if matches!(action, Action::Overview | Action::CloseWindow) {
                self.dismiss_close_error();
            }
            return Task::none();
        }
        if self.settings_window == Some(self.focused_window) {
            return match action {
                Action::Settings => window::gain_focus(self.focused_window),
                Action::CloseWindow | Action::Overview => {
                    self.handle_message(Message::Settings(false))
                }
                Action::Undo => self.replay_history(true),
                Action::Redo => self.replay_history(false),
                _ => Task::none(),
            };
        }
        let modal = self.sheet_open();
        let connected = self.snapshot.connected.is_some();
        match action {
            Action::Undo => return self.replay_history(true),
            Action::Redo => return self.replay_history(false),
            Action::Overview if self.help_open => self.help_open = false,
            Action::Overview if self.network_open => self.network_open = false,
            Action::Overview if self.analytics.open => self.analytics.open = false,
            Action::Overview if self.auto_capture.open => self.auto_capture.open = false,
            Action::Overview if self.settings_open && self.settings_window.is_none() => {
                self.settings_open = false
            }
            Action::Settings => return self.handle_message(Message::Settings(true)),
            Action::Overview if self.capture_to.editing() => {
                return self.picker(false, destinations::Message::Cancel);
            }
            Action::Overview if self.record_to.editing() => {
                return self.picker(true, destinations::Message::Cancel);
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
            Action::Help => return self.handle_message(Message::Help(!self.help_open)),
            Action::CopySessionCommand => return self.copy_session_command(true),
            Action::Fullscreen => {
                return self.fullscreen_task(None, None);
            }
            Action::CloseWindow if modal => return self.dispatch_action(Action::Overview),
            Action::CloseWindow => return Task::done(Message::CloseRequested(self.focused_window)),
            // Sheets take the keyboard until they close.
            _ if modal => {}
            Action::ToggleSidebar => return self.handle_message(Message::ToggleSidebar),
            Action::ToggleInspector => return self.handle_message(Message::ToggleInspector),
            Action::Overview if self.image_mode => self.image_mode = false,
            Action::Overview if self.inspector_peek => self.inspector_peek = false,
            Action::ImageMode => self.image_mode = !self.image_mode,
            Action::ToggleExposure => self.toggle_exposure(),
            Action::ToggleFocusRegion => self.toggle_focus_region(),
            Action::Discover => self.discover(),
            Action::ConnectAddress => return self.enter_address(),
            Action::NextCamera => self.select_relative_camera(1),
            Action::PreviousCamera => self.select_relative_camera(-1),
            Action::FocusCamera if self.overview() => self.focus_camera = true,
            Action::Overview if self.snapshot.cameras.len() > 1 => self.focus_camera = false,
            Action::ToggleStream if connected => self.toggle_stream(),
            Action::Capture if connected => self.capture(),
            Action::Record if connected => self.record(),
            Action::ToggleAuto if connected => self.toggle_auto(),
            Action::SearchFeatures => {
                let open = self.handle_message(Message::ShowTab(Tab::Features));
                return open
                    .chain(operation::snap_to(
                        inspector::SCROLL,
                        operation::RelativeOffset::START,
                    ))
                    .chain(operation::focus("feature-search"))
                    .chain(operation::select_all("feature-search"));
            }
            Action::FeaturesTab => return self.handle_message(Message::ShowTab(Tab::Features)),
            Action::CaptureTab => return self.handle_message(Message::ShowTab(Tab::Capture)),
            Action::ForwardTab => return self.handle_message(Message::ShowTab(Tab::Forward)),
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
    fn window_view(&self, id: window::Id) -> Element<'_, Message> {
        let content = if self.settings_window == Some(id) {
            keyboard_history::guard(
                container(self.settings_window(self.window_palette(id)))
                    .id("settings-root")
                    .into(),
                if self.focused_window == id {
                    self.menu_edit
                } else {
                    None
                },
                false,
            )
        } else {
            container(self.view()).id("workbench-root").into()
        };
        accessibility::wrap(
            self.close_error_view(
                keyboard_history::background(content, self.closing.is_some()),
                id,
            ),
            self.accessibility.clone(),
            id,
            if self.settings_window == Some(id) {
                "Settings".into()
            } else {
                self.title()
            },
            self.window_scales.get(&id).copied().unwrap_or(1.0) * self.ui_scale.factor(),
            Message::AccessibilityChanged,
        )
        .map(move |message| Message::InWindow(id, Box::new(message)))
    }

    fn view(&self) -> Element<'_, Message> {
        let p = self.window_palette(self.main_window);
        // iced omits zero-width row children. Keep opening panes in the tree
        // immediately so focus commands reach them before animation advances.
        let sidebar = self.sidebar_slide.lerp(0.0, SIDEBAR, self.now).max(
            if self.sidebar_slide.target() > 0.5 {
                1.0
            } else {
                0.0
            },
        );
        let inspector = self.inspector_slide.lerp(0.0, INSPECTOR, self.now).max(
            if self.inspector_slide.target() > 0.5 {
                1.0
            } else {
                0.0
            },
        );
        let inspector_visible = inspector > 0.5 || self.inspector_slide.target() > 0.5;
        let docked = self.inspector_docked();
        let mut body = row![];
        // Also while it heads open, so focusing its address field finds it.
        if sidebar > 0.5 || self.sidebar_slide.target() > 0.5 {
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
        if docked && inspector_visible {
            body = body.push(rule::vertical(1).style(style::line)).push(
                container(self.inspector(p))
                    .width(inspector)
                    .height(Fill)
                    .clip(true),
            );
        }
        let mut layers = stack![body];
        if !docked && inspector_visible {
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
        let body = self.desktop_menus(body, p);
        keyboard_history::main_guard(
            self.sheets(body, p),
            if self.focused_window == self.main_window {
                self.menu_edit
            } else {
                None
            },
            self.desktop_menu.active(),
            self.sheet_open() || self.close_error.is_some() || self.closing.is_some(),
        )
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
        let content = keyboard_history::content_scope(
            stack![content, self.image_hint()].into(),
            keyboard_history::Area::Image,
        );
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
        Num::Timeout => within(text, 1u64, 60_000),
        Num::Delay => within(text, 0u64, 315_576_000),
        Num::Interval => {
            let value = text.trim();
            (value.contains('0')
                && value.bytes().all(|b| matches!(b, b'0' | b'.'))
                && value.bytes().filter(|b| *b == b'.').count() <= 1)
                || (within(text, 0.001f64, 315_576_000.0)
                    && crate::scheduling::parse_duration(&format!("{}s", text.trim())).is_ok())
        }
        Num::Fps => within(text, 0.1f64, 240.0),
        Num::FileMib => within(text, 1.0f64, 65_536.0),
        Num::QuotaGib => within(text, 0.01f64, 102_400.0),
        Num::QuotaFiles => within(text, 1u64, 100_000),
        Num::RetentionDays => within(text, 1u64, 3650),
    }
}

fn num_requirement(key: Num) -> &'static str {
    match key {
        Num::Count => "Enter a whole frame or page count from 1 to 100,000.",
        Num::Timeout => "Enter a whole timeout from 1 to 60,000 ms.",
        Num::Delay => "Enter a whole delay from 0 to 315,576,000 seconds (10 years).",
        Num::Interval => {
            "Enter an interval from 0.001 to 315,576,000 seconds in whole milliseconds, or 0 for consecutive captures."
        }
        Num::Fps => "Enter a frame rate from 0.1 to 240 fps.",
        Num::FileMib => "Enter a maximum file size from 1 to 65,536 MiB.",
        Num::QuotaGib => "Enter a disk budget from 0.01 to 102,400 GiB.",
        Num::QuotaFiles => "Enter a whole file limit from 1 to 100,000.",
        Num::RetentionDays => "Enter a whole retention age from 1 to 3,650 days.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::futures::{FutureExt, StreamExt};

    #[test]
    fn opening_panels_build_their_editors_before_animation_frames() {
        use iced::advanced::widget::{Tree, tree::Tag};
        type Editor = iced::widget::text_input::State<
            <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
        >;
        fn editors(tree: &Tree) -> usize {
            usize::from(tree.tag == Tag::of::<Editor>())
                + tree.children.iter().map(editors).sum::<usize>()
        }

        for width in [1280.0, 900.0] {
            for action in [Action::SearchFeatures, Action::ConnectAddress] {
                let mut app = Workbench::new(
                    SessionHandle::new(),
                    "test".into(),
                    false,
                    Some(Prefs {
                        inspector_open: false,
                        sidebar_open: false,
                        ..Prefs::default()
                    }),
                );
                app.width = width;
                app.snapshot.connected = Some(crate::transport::simulator::info());
                let before = editors(&Tree::new(app.view().as_widget()));
                let _ = app.dispatch_action(action);
                app.sync_animations();
                let after = editors(&Tree::new(app.view().as_widget()));
                assert_eq!(
                    after,
                    before + 1,
                    "{action:?} is reachable at width {width}"
                );
            }
        }
    }

    #[test]
    fn native_window_focus_preserves_command_context_and_ignores_late_unfocus() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let main = app.main_window;
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.address = "unfinished camera draft".into();
        let _ = app.handle_message(Message::WindowFocused(settings));
        let _ = app.handle_message(Message::WindowUnfocused(main));
        assert_eq!(app.key_window, Some(settings));
        assert_eq!(app.focused_window, settings);
        let _ = app.handle_message(Message::WindowUnfocused(settings));
        assert_eq!(app.key_window, None);
        assert_eq!(app.focused_window, settings);
        let _ = app.handle_message(Message::WindowFocused(main));
        let _ = app.handle_message(Message::WindowUnfocused(settings));
        assert_eq!(app.key_window, Some(main));
        assert_eq!(app.focused_window, main);
        assert_eq!(app.address, "unfinished camera draft");
    }

    #[test]
    fn native_close_dismisses_the_active_sheet_before_closing_the_workbench() {
        for network in [false, true] {
            let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
            app.help_open = !network;
            app.network_open = network;
            app.address = "unfinished camera draft".into();
            let _ = app.update(Message::CloseRequested(app.main_window));
            assert!(!app.sheet_open());
            assert!(app.closing.is_none());
            assert_eq!(app.address, "unfinished camera draft");
            let _ = app.update(Message::CloseRequested(app.main_window));
            assert!(matches!(app.closing, Some(CloseTarget::Window(id)) if id == app.main_window));
        }
    }

    #[test]
    fn native_close_returns_from_a_destination_draft_before_closing_its_manager() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let output = app.output.clone();
        app.capture_to.manager_open = true;
        app.capture_to
            .update(destinations::Message::AddBucket, &mut app.output);
        app.capture_to.update(
            destinations::Message::Name("unfinished destination".into()),
            &mut app.output,
        );
        assert!(app.capture_to.editing());
        let _ = app.update(Message::CloseRequested(app.main_window));
        assert!(!app.capture_to.editing());
        assert!(app.capture_to.manager_open);
        assert!(app.closing.is_none());
        assert_eq!(app.output, output);
        let _ = app.update(Message::CloseRequested(app.main_window));
        assert!(!app.capture_to.manager_open);
        assert!(app.closing.is_none());
        assert_eq!(app.output, output);
    }

    #[test]
    fn auxiliary_window_resize_preserves_workbench_and_saved_window_size() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.window = Some([1280.0, 840.0]);
        let _ = app.update(Message::InWindow(
            settings,
            Box::new(Message::Resized(Size::new(520.0, 500.0))),
        ));
        assert_eq!((app.width, app.height), (1280.0, 840.0));
        assert_eq!(app.window, Some([1280.0, 840.0]));
    }

    #[test]
    fn settings_window_blocks_camera_shortcuts_but_preserves_main_window() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.focused_window = settings;
        let original = app.sidebar_open;
        let _ = app.dispatch_action(Action::ToggleSidebar);
        assert_eq!(app.sidebar_open, original);
        assert!(app.closing.is_none());
    }

    #[test]
    fn scale_adjustment_preserves_native_window_dimensions() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        app.ui_scale = UiScale::Larger;
        let _ = app.handle_message(Message::ScaledViewport(Size::new(800.0, 600.0)));
        assert_eq!((app.width, app.height), (800.0, 600.0));
        let _ = app.handle_message(Message::WindowMode(
            window::Mode::Windowed,
            Some(Size::new(800.0, 600.0)),
        ));
        assert_eq!(app.window, Some([1040.0, 780.0]));
    }

    #[test]
    fn close_freezes_changes_and_recovers_after_a_failed_preference_flush() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let _ = app.close(CloseTarget::Window(app.main_window));
        let original = app.include_simulator;
        let _ = app.update(Message::IncludeSimulator(!original));
        assert_eq!(app.include_simulator, original);
        let _ = app.update(Message::PrefsFlushed(Err("disk unavailable".into())));
        assert!(app.closing.is_none());
        assert!(app.notice.is_some());
        let error = app.close_error.as_ref().unwrap();
        assert_eq!(error.window, app.main_window);
        assert!(matches!(error.target, CloseTarget::Window(id) if id == app.main_window));
        let _ = app.update(Message::RetryClose);
        assert!(app.close_error.is_none());
        assert!(matches!(app.closing, Some(CloseTarget::Window(id)) if id == app.main_window));
    }

    #[test]
    fn failed_quit_is_visible_in_settings_and_keeps_drafts_when_cancelled() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.settings_open = true;
        app.focused_window = settings;
        app.address = "unfinished camera address".into();
        app.help_open = true;
        let _ = app.close(CloseTarget::Quit);
        let _ = app.update(Message::PrefsFlushed(Err("disk full".into())));
        let error = app.close_error.as_ref().unwrap();
        assert_eq!(error.window, settings);
        assert!(matches!(error.target, CloseTarget::Quit));
        assert!(
            app.command_menus()
                .iter()
                .flat_map(|menu| menu.items.iter().flatten())
                .filter(|item| item.enabled)
                .all(|item| matches!(
                    item.command,
                    menus::Command::Quit | menus::Command::Action(Action::CloseWindow)
                ))
        );
        let pending = app.pending.len();
        let _ = app.dispatch_action(Action::Discover);
        assert_eq!(app.pending.len(), pending);
        assert!(app.close_error.is_some());
        let _ = app.dispatch_action(Action::Overview);
        assert!(app.close_error.is_none());
        assert!(app.closing.is_none());
        assert_eq!(app.settings_window, Some(settings));
        assert!(app.settings_open && app.help_open);
        assert_eq!(app.address, "unfinished camera address");
    }

    #[test]
    fn retry_after_failed_quit_preserves_quit_target_from_settings() {
        let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let settings = window::Id::unique();
        app.settings_window = Some(settings);
        app.focused_window = settings;
        let _ = app.close(CloseTarget::Quit);
        let _ = app.update(Message::PrefsFlushed(Err("disk full".into())));
        let _ = app.update(Message::RetryClose);
        assert!(app.close_error.is_none());
        assert!(matches!(app.closing, Some(CloseTarget::Quit)));
        assert_eq!(app.settings_window, Some(settings));
    }

    #[test]
    fn quit_action_reaches_the_flush_path_from_sheets_and_settings() {
        for settings in [false, true] {
            let mut app = Workbench::new(SessionHandle::new(), "test".into(), false, None);
            app.help_open = true;
            if settings {
                let id = window::Id::unique();
                app.settings_window = Some(id);
                app.focused_window = id;
            }
            let _ = app.dispatch_action(Action::Quit);
            assert!(matches!(app.closing, Some(CloseTarget::Quit)));
            assert!(app.help_open);
        }
    }

    #[test]
    fn scanner_timeout_is_separate_from_camera_timeout() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        bench.timeout_ms = 5000;
        bench.scan_timeout_ms = 60_000;
        let mut info = crate::transport::simulator::info();
        bench.snapshot.connected = Some(info.clone());
        assert_eq!(bench.num_text(Num::Timeout), "5000");
        info.transport = crate::types::Transport::Scanner;
        info.id = "scanner:ica:test".into();
        bench.snapshot.connected = Some(info);
        assert_eq!(bench.num_text(Num::Timeout), "60000");
        assert!(bench.set_num(Num::Timeout, "45000"));
        assert_eq!(bench.capture_timeout(None), 45_000);
        assert_eq!(bench.timeout_ms, 5000);
        bench.snapshot.connected = None;
        assert_eq!(bench.capture_timeout(None), 5000);
        assert_eq!(bench.prefs().scan_timeout_ms, 45_000);
    }

    #[test]
    fn an_unfinished_stream_draft_saves_the_recording_it_replaced() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), false, None);
        let before = "/tmp/capturefab-test/clip-{seq}.mkv".to_owned();
        bench.forward_output = before.clone();
        bench.stream_draft = false;
        let _ = bench.update(Message::ForwardMode(true));
        assert!(bench.forward_streams());
        for draft in ["", "rts"] {
            bench.forward_output = draft.into();
            let saved = bench.prefs().forward_output;
            assert_eq!(saved, before, "{draft:?}");
            assert!(Output::of(&saved).recording());
        }
        bench.forward_output = "rtsp://127.0.0.1:8554/live".into();
        assert_eq!(bench.prefs().forward_output, "rtsp://127.0.0.1:8554/live");
    }

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
    fn a_first_run_keeps_the_inspector_quiet_until_a_camera_or_a_request() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.width = 1280.0;
        assert!(bench.inspector_open && bench.first_run_quiet());
        assert!(!bench.inspector_shown(), "nothing to inspect yet");
        assert_eq!(
            bench.inspector_slide.get(bench.now),
            0.0,
            "no slide on launch"
        );
        // ⌘I shows it on the first press, leaving the preference as it was.
        let _ = bench.handle_message(Message::ToggleInspector);
        assert!(bench.inspector_open && bench.inspector_shown());
        let _ = bench.handle_message(Message::ToggleInspector);
        assert!(!bench.inspector_open && !bench.inspector_shown());

        // So does a shortcut or menu item that shows one of its tabs.
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.width = 1280.0;
        let _ = bench.handle_message(Message::ShowTab(Tab::Capture));
        assert!(bench.inspector_shown() && bench.tab == Tab::Capture);
        // And a screenshot scene that names it.
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.width = 1280.0;
        bench.apply_scene(&["inspector"]);
        assert!(bench.inspector_shown(), "the inspector scene word shows it");

        // A camera on its way shows it.
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.width = 1280.0;
        bench.send(
            Job::Connect,
            SessionCommand::Connect {
                camera: "sim:0".into(),
                timeout_ms: 2000,
            },
        );
        assert!(bench.inspector_shown());

        // A returning user sees it from the start.
        let prefs = Prefs {
            recent: vec![Recent {
                target: "sim:0".into(),
                label: "Pattern camera".into(),
                detail: "Simulator".into(),
                transport: crate::types::Transport::Simulator,
            }],
            ..Prefs::default()
        };
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, Some(prefs));
        bench.width = 1280.0;
        assert!(bench.inspector_shown());
        assert_eq!(bench.inspector_slide.get(bench.now), 1.0);
    }

    #[test]
    fn show_messages_open_without_toggling() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.logs_open = true;
        let _ = bench.handle_message(Message::ShowActivity);
        assert!(bench.logs_open, "stays open");
        // The log has no room in image mode, so showing it leaves.
        bench.logs_open = false;
        bench.image_mode = true;
        let _ = bench.handle_message(Message::ShowActivity);
        assert!(bench.logs_open && !bench.image_mode);
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
        assert!(num_valid(Num::Timeout, "50"));
        assert!(!num_valid(Num::Timeout, "0"));
        assert!(num_valid(Num::Interval, " 0.5 "));
        assert!(num_valid(Num::Interval, "0.05"));
        assert!(num_valid(Num::Interval, "0.0"));
        assert!(!num_valid(Num::Interval, "0.0005"));
        assert!(!num_valid(Num::Interval, "0.0015"));
        assert!(!num_valid(Num::Interval, "1e-999"));
        assert!(!num_valid(Num::Interval, "-1e-999"));
        assert!(!num_valid(Num::QuotaFiles, "many"));
        assert!(!num_valid(Num::QuotaFiles, "100001"));
        for key in [Num::Fps, Num::QuotaGib, Num::FileMib, Num::Interval] {
            assert!(!num_valid(key, "NaN"));
            assert!(!num_valid(key, "inf"));
        }
    }

    #[test]
    fn invalid_visible_drafts_block_buttons_shortcuts_and_tile_captures() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.drafts.insert(Num::Count, "invalid".into());
        assert_eq!(bench.count, 1, "an old valid value is still stored");
        assert!(bench.validate_input(&bench.capture_command(false)).is_err());
        bench.capture();
        let _ = bench.handle_message(Message::CaptureCamera("sim:0".into()));
        let _ = bench.handle_message(Message::Schedule);
        assert!(
            bench.pending.is_empty(),
            "invalid drafts never reach the session"
        );
        assert!(
            bench.notice.is_some(),
            "shortcuts explain the refused submission"
        );
        assert!(
            bench.validate_input(&bench.forward_command()).is_ok(),
            "capture drafts do not block forwarding"
        );

        bench.drafts.remove(&Num::Count);
        bench.drafts.insert(Num::Interval, "NaN".into());
        assert!(bench.validate_input(&bench.capture_command(false)).is_ok());
        assert!(bench.validate_input(&bench.capture_command(true)).is_err());
        bench.drafts.insert(Num::Fps, "0".into());
        let _ = bench.handle_message(Message::StartForward);
        assert!(bench.pending.is_empty());

        bench.drafts.clear();
        bench.quota_files = 100001;
        assert!(
            bench.validate_input(&bench.capture_command(false)).is_err(),
            "invalid persisted values also need correction"
        );
    }

    #[test]
    fn unused_retention_and_recording_fields_do_not_block_streams() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.drafts.insert(Num::RetentionDays, "invalid".into());
        assert!(bench.validate_input(&bench.capture_command(false)).is_ok());
        bench.retention_enabled = true;
        assert!(bench.validate_input(&bench.capture_command(false)).is_err());
        bench.drafts.insert(Num::FileMib, "invalid".into());
        bench.forward_output = "rtsp://127.0.0.1:8554/capturefab".into();
        assert!(bench.validate_input(&bench.forward_command()).is_ok());
        bench.forward_output = "recording.mkv".into();
        assert!(bench.validate_input(&bench.forward_command()).is_err());
    }

    #[test]
    fn editable_numeric_text_preserves_the_value_sent() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        for key in [Num::Fps, Num::FileMib, Num::QuotaGib] {
            assert!(bench.set_num(key, "1.23456789"));
            assert_eq!(bench.num_text(key), "1.23456789");
        }
    }

    #[test]
    fn captures_and_recordings_carry_tags_and_number_taken_names() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        assert!(bench.output.ends_with("{seq}.png"), "{}", bench.output);
        bench.tags.line = " L1 ".into();
        bench.tags.note = "  ".into();
        let tags = crate::context::Context::from([("line".into(), "L1".into())]);
        for command in [
            bench.capture_command(false),
            bench.capture_command_for(true, Some("sim:0")),
            bench.forward_command(),
        ] {
            let (SessionCommand::Capture {
                on_exists, context, ..
            }
            | SessionCommand::Schedule {
                on_exists, context, ..
            }
            | SessionCommand::Forward {
                on_exists, context, ..
            }) = command
            else {
                panic!("expected a capture or recording");
            };
            assert_eq!(
                (on_exists, context),
                (crate::naming::OnExists::Number, tags.clone())
            );
        }
        let output_of = |command| match command {
            SessionCommand::Forward {
                output,
                destination,
                ..
            } => (output, destination),
            _ => panic!("expected a recording"),
        };
        let default = (prefs::default_record_output(), None);
        bench.forward_output = "rtsp://127.0.0.1:8554/capturefab".into();
        assert_eq!(output_of(bench.record_command(false)), default);
        bench.forward_output = "clips/run.mkv".into();
        assert_eq!(output_of(bench.record_command(false)).0, "clips/run.mkv");
        // Cameras recording at once each need their own name.
        assert_eq!(output_of(bench.record_command(true)), default);
        bench.forward_output = "clips/{camera}.mkv".into();
        assert_eq!(
            output_of(bench.record_command(true)).0,
            "clips/{camera}.mkv"
        );
    }

    #[test]
    fn connecting_starts_the_live_view_unless_off_or_a_scanner() {
        let connect = |bench: &mut Workbench, info: &CameraInfo| {
            bench.pending.clear();
            bench.settle(
                &Pending {
                    target: Some(info.id.clone()),
                    ..Pending::unanswered(Job::Connect)
                },
                &Ok(serde_json::json!({ "connected": info })),
            );
            bench.pending_for(Job::Start, &info.id)
        };
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let info = crate::transport::simulator::info();
        assert!(connect(&mut bench, &info));
        let mut scanner = info.clone();
        scanner.transport = crate::types::Transport::Scanner;
        assert!(!connect(&mut bench, &scanner));
        let mut streaming = liveness::streaming_camera(0, 0, 0.0);
        streaming.info.id = info.id.clone();
        bench.snapshot.cameras = vec![streaming];
        assert!(!connect(&mut bench, &info), "already streaming");
        bench.snapshot.cameras.clear();
        let _ = bench.handle_message(Message::AutoStart(false));
        assert!(!connect(&mut bench, &info));
        assert!(!bench.prefs().auto_start);
    }

    #[test]
    fn record_toggles_and_all_actions_reach_every_camera() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.snapshot.connected = Some(crate::transport::simulator::info());
        let _ = bench.dispatch_action(Action::Record);
        assert!(bench.pending(Job::Forward { recording: true }));
        bench.pending.clear();
        bench.snapshot.forwarding = Some("/tmp/a.mp4".into());
        let _ = bench.handle_message(Message::Record);
        assert!(bench.pending(Job::StopForward { recording: true }));
        bench.pending.clear();
        bench.snapshot.forwarding = None;

        let mut other = liveness::streaming_camera(0, 0, 0.0);
        other.info.id = "sim:1".into();
        bench.snapshot.cameras = vec![liveness::streaming_camera(0, 0, 0.0), other];
        let _ = bench.handle_message(Message::AllCapture);
        let _ = bench.handle_message(Message::AllRecord(true));
        for job in [Job::Capture, Job::Forward { recording: true }] {
            let cameras: Vec<_> = bench
                .pending
                .iter()
                .filter(|p| p.job == job && p.batch)
                .filter_map(|p| p.camera.clone())
                .collect();
            assert_eq!(cameras, ["sim:0", "sim:1"], "{job:?}");
        }
        assert_eq!(
            bench
                .pending
                .iter()
                .find(|p| p.batch && p.job == Job::Capture)
                .unwrap()
                .label(),
            "Capturing all cameras"
        );
    }

    #[test]
    fn recording_paths_preserve_meaningful_whitespace() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.forward_output = " recordings / recording.mkv".into();
        let command = bench.forward_command();
        let SessionCommand::Forward { output, .. } = &command else {
            panic!("expected forwarding command");
        };
        assert_eq!(output, " recordings / recording.mkv");
        assert!(bench.validate_input(&command).is_ok());
    }
}
