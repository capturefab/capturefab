use super::scopes::{DEFAULT_REGION, Metric};
use super::{Appearance, Tab};
use crate::types::Transport;
use anyhow::Result;
use iced::futures::channel::oneshot;
use iced::{Point, Size, Task, window};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt, fs,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

const RECENT_LIMIT: usize = 6;
const FILE: &str = "gui.prefs.json";

const MIN_WINDOW: Size = Size::new(900.0, 620.0);

#[cfg(any(target_os = "macos", test))]
mod work_area;
#[cfg(any(target_os = "windows", test))]
mod work_area_windows;
#[cfg(any(target_os = "linux", test))]
mod work_area_x11;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) enum DoubleClickAction {
    #[default]
    Zoom,
    Fill,
    Minimize,
    None,
}

#[cfg(any(target_os = "macos", test))]
impl DoubleClickAction {
    fn from_default(value: Option<&str>) -> Self {
        match value {
            Some("Fill") => Self::Fill,
            Some("Minimize") => Self::Minimize,
            Some("None") => Self::None,
            // Both the current Zoom value and older Maximize value use zoom.
            _ => Self::Zoom,
        }
    }
}

/// Read the preference at the gesture, so changes in System Settings apply
/// while the app remains open. NSUserDefaults updates cross-process changes;
/// no subprocess, cached application value or synchronize call is needed.
pub(super) fn double_click_action() -> DoubleClickAction {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSUserDefaults, ns_string};
        let value = NSUserDefaults::standardUserDefaults()
            .stringForKey(ns_string!("AppleActionOnDoubleClick"))
            .map(|value| value.to_string());
        DoubleClickAction::from_default(value.as_deref())
    }
    #[cfg(not(target_os = "macos"))]
    {
        DoubleClickAction::Zoom
    }
}

/// Fill the usable macOS screen, respecting the native menu bar and Dock.
/// iced calls this through window::run on the window's UI thread.
pub(super) fn fill_window(handle: &dyn window::Window) -> bool {
    #[cfg(target_os = "macos")]
    {
        use iced::window::raw_window_handle::RawWindowHandle;
        use objc2::MainThreadMarker;
        use objc2_app_kit::NSView;
        if MainThreadMarker::new().is_none() {
            return false;
        }
        let Ok(handle) = handle.window_handle() else {
            return false;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return false;
        };
        // SAFETY: the main-thread callback holds a live iced window; AppKit
        // handles identify its content NSView, which owns the queried window.
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let Some(window) = view.window() else {
            return false;
        };
        let Some(screen) = window.screen() else {
            return false;
        };
        window.setFrame_display(screen.visibleFrame(), true);
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = handle;
        false
    }
}

/// Scale the entire interface together so larger text retains usable controls,
/// spacing and focus rings. These choices cannot restore an invalid/nonfinite
/// scale, and unknown future choices fall back without discarding other prefs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum UiScale {
    Large,
    Larger,
    #[default]
    #[serde(other)]
    Standard,
}

impl UiScale {
    pub const ALL: [Self; 3] = [Self::Standard, Self::Large, Self::Larger];

    pub const fn factor(self) -> f32 {
        match self {
            Self::Standard => 1.0,
            Self::Large => 1.15,
            Self::Larger => 1.3,
        }
    }
}

impl fmt::Display for UiScale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Standard => "Standard",
            Self::Large => "Large",
            Self::Larger => "Larger",
        })
    }
}

/// iced scales the initial inner size, but its built-in Centered placement
/// receives the unscaled content size. Keep placement in native monitor units.
pub(super) fn centered_window(scale: UiScale) -> window::Position {
    match scale {
        UiScale::Standard => window::Position::Centered,
        UiScale::Large => window::Position::SpecificWith(|size, monitor| {
            native_center(size, monitor, UiScale::Large)
        }),
        UiScale::Larger => window::Position::SpecificWith(|size, monitor| {
            native_center(size, monitor, UiScale::Larger)
        }),
    }
}

fn native_center(size: Size, monitor: Size, scale: UiScale) -> Point {
    Point::new(
        (monitor.width - size.width * scale.factor()) / 2.0,
        (monitor.height - size.height * scale.factor()) / 2.0,
    )
}

/// Keep a restored/default window reachable on the monitor where it opens,
/// including high display scaling and moving preferences to a smaller screen.
/// macOS, Windows and EWMH X11 desktops use the native work area and window
/// chrome. Wayland retains compositor bounds; unsupported desktops reserve a
/// fallback margin for native chrome and taskbars.
/// Smaller screens lower the minimum instead of forcing overflow.
pub(super) fn fit_window<Message: Send + 'static>(wanted: Size, scale: UiScale) -> Task<Message> {
    window::latest().and_then(move |id| fit_main_window(id, wanted, scale))
}

/// Refresh the main window's bounds after an interface-scale change, even when
/// an auxiliary window is the latest native window.
pub(super) fn fit_main_window<Message: Send + 'static>(
    id: window::Id,
    wanted: Size,
    scale: UiScale,
) -> Task<Message> {
    #[cfg(target_os = "macos")]
    {
        window::run(id, move |handle| {
            work_area::fit(handle, wanted, Some(MIN_WINDOW))
        })
        .then(move |fitted| {
            if fitted {
                Task::none()
            } else {
                fit_main_fallback(id, wanted, scale)
            }
        })
    }
    #[cfg(target_os = "windows")]
    {
        fit_windows(id, wanted, Some(MIN_WINDOW), scale, false)
    }
    #[cfg(target_os = "linux")]
    {
        window::run(id, is_wayland).then(move |wayland| {
            if wayland {
                // winit has already constrained the initial size to the
                // compositor's configure_bounds. Its later resize requests
                // bypass those bounds, so retain that size and placement.
                window::size(id)
                    .then(move |current| window::set_min_size(id, wayland_minimum(current, scale)))
            } else {
                fit_x11(id, wanted, Some(MIN_WINDOW), scale, false, 3)
            }
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        fit_main_fallback(id, wanted, scale)
    }
}

fn fit_main_fallback<Message: Send + 'static>(
    id: window::Id,
    wanted: Size,
    scale: UiScale,
) -> Task<Message> {
    window::monitor_size(id).then(move |monitor| {
        let Some(monitor) = monitor.filter(|size| valid_size(*size)) else {
            return Task::none();
        };
        let (size, minimum) = fit_main_size(wanted, monitor, scale);
        resize_centered(id, size, Some(minimum), scale)
    })
}

/// Auxiliary windows use content dimensions, without the main window's large
/// minimum. iced applies the interface scale to these dimensions itself.
pub(super) fn fit_aux_window<Message: Send + 'static>(
    id: window::Id,
    wanted: Size,
    scale: UiScale,
) -> Task<Message> {
    #[cfg(target_os = "macos")]
    {
        window::run(id, move |handle| {
            work_area::fit(handle, wanted * scale.factor(), None)
        })
        .then(move |fitted| {
            if fitted {
                Task::none()
            } else {
                fit_aux_fallback(id, wanted, scale)
            }
        })
    }
    #[cfg(target_os = "windows")]
    {
        fit_windows(id, wanted * scale.factor(), None, scale, true)
    }
    #[cfg(target_os = "linux")]
    {
        window::run(id, is_wayland).then(move |wayland| {
            if wayland {
                Task::none()
            } else {
                fit_x11(id, wanted * scale.factor(), None, scale, true, 3)
            }
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        fit_aux_fallback(id, wanted, scale)
    }
}

#[cfg(target_os = "linux")]
fn is_wayland(handle: &dyn window::Window) -> bool {
    use iced::window::raw_window_handle::RawWindowHandle;
    matches!(
        handle.window_handle().map(|handle| handle.as_raw()),
        Ok(RawWindowHandle::Wayland(_))
    )
}

#[cfg(any(target_os = "linux", test))]
fn wayland_minimum(current_ui: Size, scale: UiScale) -> Option<Size> {
    if !valid_size(current_ui) {
        return None;
    }
    // Current size already uses iced UI units, including both display DPI and
    // interface scale. Converting the native preference minimum once keeps
    // constraints at or below the compositor-configured content viewport.
    let requested = MIN_WINDOW / scale.factor();
    Some(Size::new(
        requested.width.min(current_ui.width),
        requested.height.min(current_ui.height),
    ))
}

#[cfg(target_os = "linux")]
fn fit_x11<Message: Send + 'static>(
    id: window::Id,
    wanted_native: Size,
    minimum_native: Option<Size>,
    scale: UiScale,
    auxiliary: bool,
    retries: u8,
) -> Task<Message> {
    window::scale_factor(id)
        .then(move |display_scale| {
            window::run(id, move |handle| {
                work_area_x11::query(handle, wanted_native, minimum_native, display_scale, scale)
            })
        })
        .then(move |reply| match reply {
            work_area_x11::Query::Fit(fit) => window::set_min_size(id, fit.minimum_ui)
                .chain(window::resize(id, fit.content_ui))
                .chain(window::move_to(id, fit.origin_native)),
            work_area_x11::Query::FramePending if retries > 0 => {
                // A hidden/new window can precede its WM frame property. Let
                // mapping/reparenting proceed without waiting on the UI thread.
                Task::perform(
                    async {
                        let (send, receive) = oneshot::channel();
                        let _ = std::thread::Builder::new()
                            .name("capturefab-window-fit".into())
                            .spawn(move || {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                let _ = send.send(());
                            });
                        let _ = receive.await;
                    },
                    |()| (),
                )
                .then(move |()| {
                    fit_x11(
                        id,
                        wanted_native,
                        minimum_native,
                        scale,
                        auxiliary,
                        retries - 1,
                    )
                })
            }
            _ if auxiliary => fit_aux_fallback(id, wanted_native / scale.factor(), scale),
            _ => fit_main_fallback(id, wanted_native, scale),
        })
}

#[cfg(target_os = "windows")]
fn fit_windows<Message: Send + 'static>(
    id: window::Id,
    wanted_native: Size,
    minimum_native: Option<Size>,
    scale: UiScale,
    auxiliary: bool,
) -> Task<Message> {
    window::run(id, move |handle| {
        work_area_windows::query(handle, wanted_native, minimum_native)
    })
    .then(move |fit| {
        let fallback = move || {
            if auxiliary {
                fit_aux_fallback(id, wanted_native / scale.factor(), scale)
            } else {
                fit_main_fallback(id, wanted_native, scale)
            }
        };
        let Some(fit) = fit else {
            return fallback();
        };
        // iced/winit retain physical min-size constraints for WM_GETMINMAXINFO.
        // Lower those before SetWindowPos, or Windows can undo the fitted size.
        window::set_min_size(id, fit.minimum_ui(scale)).chain(
            window::run(id, move |handle| work_area_windows::apply(handle, fit)).then(
                move |fitted| {
                    if fitted { Task::none() } else { fallback() }
                },
            ),
        )
    })
}

fn fit_aux_fallback<Message: Send + 'static>(
    id: window::Id,
    wanted: Size,
    scale: UiScale,
) -> Task<Message> {
    window::monitor_size(id).then(move |monitor| {
        let Some(monitor) = monitor.filter(|size| valid_size(*size)) else {
            return Task::none();
        };
        resize_centered(id, fit_aux_size(wanted, monitor, scale), None, scale)
    })
}

fn resize_centered<Message: Send + 'static>(
    id: window::Id,
    size: Size,
    minimum: Option<Size>,
    scale: UiScale,
) -> Task<Message> {
    window::size(id).then(move |current| {
        window::position(id).then(move |position| {
            // Size uses iced UI units; position/move use native logical units.
            let center = position.map(|position| centered_position(position, current, size, scale));
            window::set_min_size(id, minimum)
                .chain(window::resize(id, size))
                .chain(center.map_or_else(Task::none, |position| window::move_to(id, position)))
        })
    })
}

fn centered_position(position: Point, current: Size, size: Size, scale: UiScale) -> Point {
    Point::new(
        position.x + (current.width - size.width) * scale.factor() / 2.0,
        position.y + (current.height - size.height) * scale.factor() / 2.0,
    )
}

fn available_size(monitor: Size, scale: UiScale) -> Size {
    Size::new(
        (monitor.width - 32.0 / scale.factor()).max(1.0),
        (monitor.height - 96.0 / scale.factor()).max(1.0),
    )
}

fn fit_main_size(wanted: Size, monitor: Size, scale: UiScale) -> (Size, Size) {
    let factor = scale.factor();
    let available = available_size(monitor, scale);
    let (size, minimum) = fit_size(wanted, available * factor);
    (size / factor, minimum / factor)
}

fn fit_aux_size(wanted: Size, monitor: Size, scale: UiScale) -> Size {
    let available = available_size(monitor, scale);
    let wanted = if valid_size(wanted) {
        wanted
    } else {
        available
    };
    Size::new(
        wanted.width.min(available.width),
        wanted.height.min(available.height),
    )
}

fn valid_size(size: Size) -> bool {
    size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0
}

fn fit_size(wanted: Size, available: Size) -> (Size, Size) {
    let minimum = Size::new(
        MIN_WINDOW.width.min(available.width),
        MIN_WINDOW.height.min(available.height),
    );
    let wanted = if valid_size(wanted) {
        wanted
    } else {
        MIN_WINDOW
    };
    (
        Size::new(
            wanted.width.clamp(minimum.width, available.width),
            wanted.height.clamp(minimum.height, available.height),
        ),
        minimum,
    )
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub appearance: Appearance,
    pub ui_scale: UiScale,
    /// Settings › Performance; a choice from a newer version reads as
    /// Automatic.
    pub performance: crate::efficiency::Mode,
    pub include_simulator: bool,
    pub tab: Tab,
    pub exposure_open: bool,
    pub focus_open: bool,
    pub focus_region: [f32; 4],
    pub focus_metric: Metric,
    pub inspector_open: bool,
    pub sidebar_open: bool,
    pub output: String,
    pub format: String,
    pub count: u32,
    pub timeout_ms: u64,
    pub scan_timeout_ms: u64,
    pub schedule_delay_seconds: u64,
    pub schedule_interval_seconds: f64,
    pub forward_output: String,
    pub forward_codec: String,
    pub forward_encoder: String,
    pub forward_fps: f64,
    pub forward_bitrate: String,
    pub forward_file_mib: f64,
    pub quota_gib: f64,
    pub quota_files: u32,
    pub retention_enabled: bool,
    pub retention_days: u64,
    pub quota_action: String,
    pub recent: Vec<Recent>,
    pub window: Option<[f32; 2]>,
    pub logs_open: bool,
    pub video_analytics: super::video_analytics::Preferences,
    /// Start the live view when a camera connects.
    pub auto_start: bool,
    /// Capture context sent with every capture and recording.
    pub tags: CaptureTags,
    /// The dataset reopened at startup.
    pub dataset_path: Option<PathBuf>,
    /// Catalog captures in a dataset under the capture folder when none is open.
    pub dataset_auto: bool,
    /// Copy the captured images into dataset exports.
    pub dataset_export_images: bool,
    /// The settings panel's disclosures the user left open, by stable key
    /// ("features:Image", "storage"); unknown keys are ignored on load.
    pub open_sections: Vec<String>,
}
/// Capture context fields; empty ones are not sent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureTags {
    /// A general class or condition, such as `good` or `defect`.
    pub label: String,
    pub line: String,
    pub station: String,
    pub part: String,
    pub lot: String,
    pub note: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recent {
    pub target: String,
    pub label: String,
    pub detail: String,
    pub transport: Transport,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            ui_scale: UiScale::Standard,
            performance: crate::efficiency::Mode::Auto,
            include_simulator: false,
            tab: Tab::Capture,
            exposure_open: true,
            focus_open: false,
            focus_region: DEFAULT_REGION,
            focus_metric: Metric::default(),
            inspector_open: true,
            sidebar_open: true,
            output: default_capture_output(),
            format: "png".into(),
            count: 1,
            timeout_ms: 5000,
            scan_timeout_ms: 60_000,
            schedule_delay_seconds: 5,
            schedule_interval_seconds: 60.0,
            forward_output: default_record_output(),
            forward_codec: "h264".into(),
            forward_encoder: "auto".into(),
            forward_fps: 30.0,
            forward_bitrate: "4M".into(),
            forward_file_mib: 512.0,
            quota_gib: 10.0,
            quota_files: 10_000,
            retention_enabled: false,
            retention_days: 7,
            quota_action: "stop".into(),
            recent: Vec::new(),
            window: None,
            logs_open: false,
            video_analytics: Default::default(),
            auto_start: true,
            tags: CaptureTags::default(),
            dataset_path: None,
            dataset_auto: true,
            dataset_export_images: true,
            open_sections: Vec::new(),
        }
    }
}
/// Where the GUI saves by default: `Capturefab` in the user's home folder.
pub fn capture_root() -> PathBuf {
    crate::storage::capture_root()
}
/// A dated folder of uniquely named stills under `capture_root`.
pub fn default_capture_output() -> String {
    capture_root()
        .join("{date}")
        .join("{camera}-{datetime}-{seq}.png")
        .to_string_lossy()
        .into_owned()
}
/// Like `default_capture_output`, for local recordings.
pub fn default_record_output() -> String {
    capture_root()
        .join("{date}")
        .join("{camera}-{datetime}-{seq}.mp4")
        .to_string_lossy()
        .into_owned()
}
pub fn storable(address: &str) -> bool {
    crate::media::redact_url(address) == address
}

impl Prefs {
    pub fn remember(&mut self, entry: Recent) {
        if !storable(&entry.target) {
            return;
        }
        self.recent.retain(|r| r.target != entry.target);
        self.recent.insert(0, entry);
        self.recent.truncate(RECENT_LIMIT);
    }
    pub fn shared(&self) -> Self {
        self.clone().with_layout(&Self::default())
    }
    pub fn with_layout(self, layout: &Self) -> Self {
        Self {
            tab: layout.tab,
            exposure_open: layout.exposure_open,
            focus_open: layout.focus_open,
            focus_region: layout.focus_region,
            focus_metric: layout.focus_metric,
            inspector_open: layout.inspector_open,
            sidebar_open: layout.sidebar_open,
            window: layout.window,
            logs_open: layout.logs_open,
            open_sections: layout.open_sections.clone(),
            ..self
        }
    }
    fn sanitized(&self) -> Self {
        let mut prefs = self.clone();
        if !storable(&prefs.forward_output) {
            prefs.forward_output = Self::default().forward_output;
        }
        prefs.recent.retain(|r| storable(&r.target));
        prefs
    }
}

fn path() -> PathBuf {
    crate::ipc::state_dir().join(FILE)
}

pub fn load() -> Prefs {
    load_from(&path())
}

/// Settings › Performance as last saved, unless Automatic or never saved,
/// for `doctor`: only the desktop window applies it.
pub(crate) fn saved_performance() -> Option<crate::efficiency::Mode> {
    let path = path();
    path.is_file()
        .then(|| load_from(&path).performance)
        .filter(|mode| *mode != crate::efficiency::Mode::Auto)
}

/// One application's preference destination. All instances share a serialized
/// writer, so independent snapshots for the same path cannot race a rename.
#[derive(Clone)]
pub(super) struct Saver {
    path: PathBuf,
}

impl Saver {
    pub(super) fn new() -> Self {
        Self { path: path() }
    }

    /// Queue a snapshot off the UI thread. The generation is allocated before
    /// enqueueing; the writer rejects a stale arrival after a newer snapshot.
    pub(super) fn save(&self, prefs: Prefs) -> std::result::Result<u64, String> {
        writer()?.enqueue(self.path.clone(), prefs, None)
    }

    /// Queue the current snapshot immediately and await its persisted result.
    /// Close/quit must await this before allowing the native window to close.
    pub(super) fn flush(
        &self,
        prefs: Prefs,
    ) -> impl Future<Output = std::result::Result<(), String>> + Send + 'static {
        let (completed, result) = oneshot::channel();
        let queued =
            writer().and_then(|writer| writer.enqueue(self.path.clone(), prefs, Some(completed)));
        async move {
            queued?;
            result
                .await
                .map_err(|_| "The settings writer stopped before saving.".to_owned())?
        }
    }
}

struct Snapshot {
    path: PathBuf,
    generation: u64,
    prefs: Prefs,
    completed: Option<oneshot::Sender<std::result::Result<(), String>>>,
}

struct Writer {
    next_generation: AtomicU64,
    snapshots: Sender<Snapshot>,
}

impl Writer {
    fn start() -> std::result::Result<Self, String> {
        let (snapshots, pending) = mpsc::channel();
        std::thread::Builder::new()
            .name("capturefab-settings".into())
            .spawn(move || write_snapshots(pending, write_prefs))
            .map_err(|error| format!("Could not start the settings writer: {error}"))?;
        Ok(Self {
            next_generation: AtomicU64::new(0),
            snapshots,
        })
    }

    fn enqueue(
        &self,
        path: PathBuf,
        prefs: Prefs,
        completed: Option<oneshot::Sender<std::result::Result<(), String>>>,
    ) -> std::result::Result<u64, String> {
        let mut previous = self.next_generation.load(Ordering::Relaxed);
        let generation = loop {
            let next = previous
                .checked_add(1)
                .ok_or_else(|| "The settings save generation is exhausted.".to_owned())?;
            match self.next_generation.compare_exchange_weak(
                previous,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break next,
                Err(current) => previous = current,
            }
        };
        self.snapshots
            .send(Snapshot {
                path,
                generation,
                prefs,
                completed,
            })
            .map_err(|_| "The settings writer is unavailable.".to_owned())?;
        Ok(generation)
    }
}

fn writer() -> std::result::Result<&'static Writer, String> {
    static WRITER: OnceLock<std::result::Result<Writer, String>> = OnceLock::new();
    WRITER
        .get_or_init(Writer::start)
        .as_ref()
        .map_err(Clone::clone)
}

fn write_prefs(path: &Path, prefs: &Prefs) -> std::result::Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "The settings destination has no parent folder.".to_owned())?;
    crate::ipc::ensure_private_dir(dir)
        .and_then(|()| save_to(path, prefs))
        .map_err(|error| format!("Could not save settings: {error:#}"))
}

fn write_snapshots(
    pending: Receiver<Snapshot>,
    mut write: impl FnMut(&Path, &Prefs) -> std::result::Result<(), String>,
) {
    let mut latest: HashMap<PathBuf, (u64, std::result::Result<(), String>)> = HashMap::new();
    for snapshot in pending {
        let current = latest.entry(snapshot.path.clone()).or_insert((0, Ok(())));
        // A failed newer write still rejects old snapshots. A fresh generation
        // may retry; reporting success for an older write would hide the failure.
        if snapshot.generation > current.0 {
            *current = (snapshot.generation, write(&snapshot.path, &snapshot.prefs));
        }
        if let Some(completed) = snapshot.completed {
            let _ = completed.send(current.1.clone());
        }
    }
}

fn load_from(path: &Path) -> Prefs {
    fs::read(path)
        .ok()
        .filter(|bytes| bytes.len() <= 1 << 20)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, prefs: &Prefs) -> Result<()> {
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    output.write_all(&serde_json::to_vec_pretty(&prefs.sanitized())?)?;
    output.sync_all()?;
    fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("capturefab-prefs-{name}-{}", std::process::id()));
        crate::ipc::ensure_private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn prefs_without_open_sections_open_nothing() {
        let prefs: Prefs = serde_json::from_str(r#"{"tab": "capture"}"#).unwrap();
        assert!(prefs.open_sections.is_empty());
        let prefs = Prefs {
            open_sections: vec!["features:Image".into(), "storage".into()],
            ..Prefs::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: Prefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.open_sections, prefs.open_sections);
    }

    #[test]
    fn titlebar_actions_preserve_each_system_choice_and_fall_back_to_zoom() {
        for (value, action) in [
            (Some("Zoom"), DoubleClickAction::Zoom),
            (Some("Maximize"), DoubleClickAction::Zoom),
            (Some("Fill"), DoubleClickAction::Fill),
            (Some("Minimize"), DoubleClickAction::Minimize),
            (Some("None"), DoubleClickAction::None),
            (None, DoubleClickAction::Zoom),
            (Some("future-action"), DoubleClickAction::Zoom),
        ] {
            assert_eq!(DoubleClickAction::from_default(value), action);
        }
    }

    #[test]
    fn native_window_dimensions_and_chrome_margins_survive_interface_scaling() {
        let native_monitor = Size::new(1093.0, 614.0);
        let native_available = Size::new(1061.0, 518.0);
        for scale in UiScale::ALL {
            let factor = scale.factor();
            let monitor = native_monitor / factor;
            let (size, minimum) = fit_main_size(Size::new(1280.0, 840.0), monitor, scale);
            let native_size = size * factor;
            let native_minimum = minimum * factor;
            assert!((native_size.width - native_available.width).abs() < 0.001);
            assert!((native_size.height - native_available.height).abs() < 0.001);
            assert!((native_minimum.width - 900.0).abs() < 0.001);
            assert!((native_minimum.height - native_available.height).abs() < 0.001);
            let auxiliary = fit_aux_size(Size::new(520.0, 380.0), monitor, scale);
            assert_eq!(auxiliary, Size::new(520.0, 380.0));
            let settings = fit_aux_size(Size::new(520.0, 500.0), monitor, scale) * factor;
            assert!((settings.width - 520.0 * factor).abs() < 0.001);
            assert!((settings.height - (500.0 * factor).min(518.0)).abs() < 0.001);
        }
        let scale = UiScale::Larger;
        let monitor = Size::new(640.0, 480.0) / scale.factor();
        let size = fit_aux_size(Size::new(520.0, 380.0), monitor, scale) * scale.factor();
        assert!((size.width - 608.0).abs() < 0.001);
        assert!((size.height - 384.0).abs() < 0.001);
        assert!(size.width < MIN_WINDOW.width && size.height < MIN_WINDOW.height);
    }

    #[test]
    fn wayland_minimum_never_grows_the_compositor_configured_viewport() {
        for scale in UiScale::ALL {
            let requested = MIN_WINDOW / scale.factor();
            for current in [
                Size::new(500.0, 300.0),
                Size::new(1200.0, 300.0),
                Size::new(500.0, 1000.0),
                Size::new(1200.0, 1000.0),
            ] {
                let minimum = wayland_minimum(current, scale).unwrap();
                assert!(minimum.width <= current.width && minimum.height <= current.height);
                assert!(minimum.width <= requested.width && minimum.height <= requested.height);
            }
            assert_eq!(
                wayland_minimum(Size::new(500.0, 300.0), scale),
                Some(Size::new(500.0, 300.0))
            );
            assert_eq!(
                wayland_minimum(Size::new(1200.0, 1000.0), scale),
                Some(requested)
            );
        }
        assert_eq!(wayland_minimum(Size::ZERO, UiScale::Standard), None);
        assert_eq!(
            wayland_minimum(Size::new(f32::NAN, 300.0), UiScale::Standard),
            None
        );
    }

    #[test]
    fn resize_keeps_the_window_center_in_native_coordinates() {
        let position = Point::new(-100.0, 45.0);
        let current = Size::new(1280.0, 840.0);
        let size = Size::new(608.0, 384.0);
        for scale in UiScale::ALL {
            let factor = scale.factor();
            let moved = centered_position(position, current / factor, size / factor, scale);
            assert!((moved.x - 236.0).abs() < 0.001);
            assert!((moved.y - 273.0).abs() < 0.001);
        }
    }

    #[test]
    fn opening_a_scaled_window_centers_the_actual_native_dimensions() {
        let monitor = Size::new(1093.0, 614.0);
        let actual = Size::new(1280.0, 840.0);
        for scale in UiScale::ALL {
            let content = actual / scale.factor();
            let centered = match centered_window(scale) {
                window::Position::Centered => native_center(content, monitor, scale),
                window::Position::SpecificWith(center) => center(content, monitor),
                _ => panic!("scaled placement must use the monitor"),
            };
            assert!((centered.x - -93.5).abs() < 0.001);
            assert!((centered.y - -113.0).abs() < 0.001);
            let (fitted, _) = fit_main_size(actual, monitor / scale.factor(), scale);
            let moved = centered_position(centered, content, fitted, scale);
            assert!((moved.x - 16.0).abs() < 0.001);
            assert!((moved.y - 48.0).abs() < 0.001);
        }
    }

    #[test]
    fn interface_scale_round_trips_defaults_and_preserves_other_preferences() {
        assert_eq!(Prefs::default().ui_scale, UiScale::Standard);
        for (scale, factor, label) in [
            (UiScale::Standard, 1.0, "Standard"),
            (UiScale::Large, 1.15, "Large"),
            (UiScale::Larger, 1.3, "Larger"),
        ] {
            let prefs = Prefs {
                ui_scale: scale,
                count: 17,
                ..Prefs::default()
            };
            let json = serde_json::to_vec(&prefs).unwrap();
            assert_eq!(serde_json::from_slice::<Prefs>(&json).unwrap(), prefs);
            assert_eq!(prefs.sanitized().ui_scale, scale);
            assert_eq!(scale.factor(), factor);
            assert_eq!(scale.to_string(), label);
        }
        let partial: Prefs = serde_json::from_str(r#"{"count":17}"#).unwrap();
        assert_eq!(partial.count, 17);
        assert_eq!(partial.ui_scale, UiScale::Standard);
        let future: Prefs = serde_json::from_str(r#"{"ui_scale":"huge","count":17}"#).unwrap();
        assert_eq!(
            future.count, 17,
            "a future scale choice does not discard existing preferences"
        );
        assert_eq!(future.ui_scale, UiScale::Standard);
        assert_eq!(
            UiScale::ALL,
            [UiScale::Standard, UiScale::Large, UiScale::Larger]
        );
    }

    #[test]
    fn the_performance_setting_round_trips_and_unknown_choices_read_as_automatic() {
        use crate::efficiency::Mode;
        assert_eq!(Prefs::default().performance, Mode::Auto);
        for mode in Mode::ALL {
            let prefs = Prefs {
                performance: mode,
                count: 17,
                ..Prefs::default()
            };
            let json = serde_json::to_vec(&prefs).unwrap();
            assert_eq!(serde_json::from_slice::<Prefs>(&json).unwrap(), prefs);
        }
        let dir = test_dir("performance");
        let file = dir.join(FILE);
        fs::write(&file, br#"{"performance":"turbo","count":17}"#).unwrap();
        let future = load_from(&file);
        assert_eq!(
            (future.performance, future.count),
            (Mode::Auto, 17),
            "a newer choice keeps the other preferences"
        );
        fs::write(&file, br#"{"count":17}"#).unwrap();
        assert_eq!(load_from(&file).performance, Mode::Auto);
        fs::remove_dir_all(dir).unwrap();
    }

    fn snapshot(
        path: &Path,
        generation: u64,
        count: u32,
    ) -> (Snapshot, oneshot::Receiver<std::result::Result<(), String>>) {
        let (completed, result) = oneshot::channel();
        (
            Snapshot {
                path: path.into(),
                generation,
                prefs: Prefs {
                    count,
                    ..Prefs::default()
                },
                completed: Some(completed),
            },
            result,
        )
    }

    fn recent(target: &str) -> Recent {
        Recent {
            target: target.into(),
            label: "Camera".into(),
            detail: "GigE".into(),
            transport: Transport::GigE,
        }
    }

    #[test]
    fn flush_persists_the_latest_snapshot_across_saver_instances() {
        let dir = test_dir("flush");
        let path = dir.join(FILE);
        let first = Saver { path: path.clone() };
        let second = Saver { path: path.clone() };
        let mut previous = 0;
        for count in 1..=40 {
            let generation = first
                .save(Prefs {
                    count,
                    ..Prefs::default()
                })
                .unwrap();
            assert!(generation > previous);
            previous = generation;
        }
        let latest = Prefs {
            count: 99,
            appearance: Appearance::Dark,
            ..Prefs::default()
        };
        let finished = second.flush(latest.clone());
        // The save does not depend on either Saver or on the UI polling its
        // returned future before quitting.
        drop(first);
        drop(second);
        futures::executor::block_on(finished).unwrap();
        assert_eq!(load_from(&path), latest);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_flush_waits_behind_an_inflight_save_before_acknowledging() {
        let dir = test_dir("inflight");
        let path = dir.join(FILE);
        let (pending, receiver) = mpsc::channel();
        let (started, waiting) = mpsc::channel();
        let (release, paused) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            write_snapshots(receiver, |path, prefs| {
                if prefs.count == 1 {
                    started.send(()).unwrap();
                    paused.recv().unwrap();
                }
                write_prefs(path, prefs)
            });
        });
        let (first, first_finished) = snapshot(&path, 1, 1);
        pending.send(first).unwrap();
        waiting
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let (last, mut last_finished) = snapshot(&path, 2, 2);
        pending.send(last).unwrap();
        assert!(
            last_finished.try_recv().unwrap().is_none(),
            "the UI cannot close while an older write is still running"
        );
        release.send(()).unwrap();
        futures::executor::block_on(first_finished)
            .unwrap()
            .unwrap();
        futures::executor::block_on(last_finished).unwrap().unwrap();
        drop(pending);
        worker.join().unwrap();
        assert_eq!(load_from(&path).count, 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_generations_cannot_overwrite_a_newer_snapshot_of_the_same_path() {
        let dir = test_dir("generations");
        let path = dir.join(FILE);
        let other = dir.join("other.prefs.json");
        let (pending, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || write_snapshots(receiver, write_prefs));
        let mut completed = Vec::new();
        // Simulate an older caller delayed before enqueueing. Generation
        // comparison is local to the destination, so another path still saves.
        for (path, generation, count) in [(&path, 5, 5), (&other, 1, 1), (&path, 4, 4)] {
            let (save, done) = snapshot(path, generation, count);
            pending.send(save).unwrap();
            completed.push(done);
        }
        drop(pending);
        for done in completed {
            futures::executor::block_on(done).unwrap().unwrap();
        }
        worker.join().unwrap();
        assert_eq!(load_from(&path).count, 5);
        assert_eq!(load_from(&other).count, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_newer_write_reports_failure_and_rejects_stale_fallbacks() {
        let dir = test_dir("failure");
        let path = dir.join(FILE);
        let (pending, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut writes = Vec::new();
            write_snapshots(receiver, |path, prefs| {
                writes.push(prefs.count);
                if prefs.count == 2 {
                    Err("disk unavailable".into())
                } else {
                    write_prefs(path, prefs)
                }
            });
            writes
        });
        for (generation, count, expected) in [
            (2, 2, Err("disk unavailable".to_owned())),
            (1, 1, Err("disk unavailable".to_owned())),
            (3, 3, Ok(())),
        ] {
            let (save, done) = snapshot(&path, generation, count);
            pending.send(save).unwrap();
            assert_eq!(futures::executor::block_on(done).unwrap(), expected);
        }
        drop(pending);
        assert_eq!(
            worker.join().unwrap(),
            [2, 3],
            "old generations never rewrite after a newer failure"
        );
        assert_eq!(load_from(&path).count, 3, "a new snapshot retries normally");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn flush_reports_real_write_failures_and_a_later_flush_can_retry() {
        let dir = test_dir("flush-io");
        let path = dir.join(FILE);
        fs::create_dir(&path).unwrap();
        let saver = Saver { path: path.clone() };
        let error = futures::executor::block_on(saver.flush(Prefs::default())).unwrap_err();
        assert!(error.starts_with("Could not save settings:"));
        fs::remove_dir(&path).unwrap();
        let latest = Prefs {
            include_simulator: true,
            ..Prefs::default()
        };
        futures::executor::block_on(saver.flush(latest.clone())).unwrap();
        assert_eq!(load_from(&path), latest);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn restored_windows_fit_smaller_and_scaled_displays() {
        let wanted = Size::new(2560.0, 1600.0);
        let available = Size::new(1061.0, 518.0);
        let (size, minimum) = fit_size(wanted, available);
        assert_eq!(size, available);
        assert_eq!(minimum, Size::new(900.0, 518.0));
        assert_eq!(fit_size(Size::new(640.0, 400.0), available).0, minimum);
        assert_eq!(
            fit_size(wanted, Size::new(740.0, 480.0)),
            (Size::new(740.0, 480.0), Size::new(740.0, 480.0))
        );
        let roomy = Size::new(1900.0, 1000.0);
        assert_eq!(
            fit_size(Size::new(1280.0, 840.0), roomy),
            (Size::new(1280.0, 840.0), MIN_WINDOW)
        );
        assert_eq!(
            fit_size(Size::new(f32::NAN, f32::INFINITY), roomy).0,
            MIN_WINDOW
        );
    }

    #[test]
    fn round_trips_and_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("capturefab-prefs-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE);
        assert_eq!(load_from(&file), Prefs::default());
        let mut prefs = Prefs {
            appearance: Appearance::Dark,
            tab: Tab::Forward,
            count: 12,
            format: "jpeg".into(),
            ..Prefs::default()
        };
        prefs.remember(recent("rtsp://camera.local/stream"));
        save_to(&file, &prefs).unwrap();
        assert_eq!(load_from(&file), prefs);
        fs::write(&file, br#"{"count":3,"tab":"capture","unknown":true}"#).unwrap();
        assert_eq!(
            load_from(&file),
            Prefs {
                count: 3,
                ..Prefs::default()
            }
        );
        fs::write(&file, b"not json").unwrap();
        assert_eq!(load_from(&file), Prefs::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn secrets_are_never_written() {
        let mut prefs = Prefs {
            forward_output: "rtsp://user:pass@nvr.local/live".into(),
            ..Prefs::default()
        };
        prefs.remember(recent("rtsp://admin:secret@10.0.0.5/stream"));
        prefs.remember(recent("http://10.0.0.5/video?token=abc"));
        assert!(prefs.recent.is_empty());
        let written = prefs.sanitized();
        assert_eq!(written.forward_output, Prefs::default().forward_output);
        let json = serde_json::to_string(&written).unwrap();
        assert!(!json.contains("pass") && !json.contains("secret"));
    }

    #[test]
    fn recent_is_newest_first_unique_and_capped() {
        let mut prefs = Prefs::default();
        for index in 0..8 {
            prefs.remember(recent(&format!("192.168.1.{index}")));
        }
        prefs.remember(recent("192.168.1.4"));
        let targets: Vec<_> = prefs.recent.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "192.168.1.4",
                "192.168.1.7",
                "192.168.1.6",
                "192.168.1.5",
                "192.168.1.3",
                "192.168.1.2"
            ]
        );
    }
}
