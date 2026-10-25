//! Screenshots of named workbench states, for reviewing the interface:
//! `gui --screenshot` with CAPTUREFAB_SCREENSHOT_SCENE set to comma-separated
//! words, e.g. `help,light` or `saved,dark`.
use super::*;
use notice::{Level, Saved, command_notice};
use serde_json::json;

mod video;
use video::VideoSink;

/// An explicit zero renders a camera-free workbench; it never invents a
/// simulator just to satisfy the screenshot's startup condition.
pub(super) fn screenshot_camera_count(words: &[&str], existing: u32, requested: u32) -> u32 {
    if words.contains(&"welcome") {
        0
    } else if existing > 0 {
        existing
    } else {
        requested.min(16)
    }
}

pub(super) struct ScreenshotRequest {
    pub(super) path: PathBuf,
    pub(super) cameras: u32,
    pub(super) started: Instant,
    pub(super) streams_started: Option<Instant>,
    /// The scene's late words have been applied; the shot follows next tick.
    pub(super) staged: bool,
    pub(super) requested: bool,
    pub(super) save: Option<Receiver<Result<()>>>,
    pub(super) outcome: Arc<Mutex<Option<String>>>,
    pub(super) sequence: ScreenshotSequence,
    /// A video sequence's writers and timing sidecar; single shots use `save`.
    pub(super) video: Option<VideoSink>,
}

impl ScreenshotRequest {
    pub(super) fn new(
        path: PathBuf,
        cameras: u32,
        outcome: Arc<Mutex<Option<String>>>,
        sequence: ScreenshotSequence,
    ) -> Result<Self> {
        let video = sequence
            .is_video()
            .then(|| VideoSink::open(&path))
            .transpose()
            .map_err(|error| anyhow::anyhow!("Open renderer frame timing: {error:#}"))?;
        Ok(Self {
            path,
            cameras,
            started: Instant::now(),
            streams_started: None,
            staged: false,
            requested: false,
            save: None,
            outcome,
            sequence,
            video,
        })
    }
}

/// Optional bounded renderer frame sequence for documentation videos. Each
/// frame is painted by the normal workbench; no desktop pixels are captured.
#[derive(Clone, Copy)]
pub(super) struct ScreenshotSequence {
    frames: u32,
    index: u32,
    interval: Duration,
    next: Option<Instant>,
}

impl ScreenshotSequence {
    pub(super) fn from_env() -> Result<Self> {
        Self::parse(
            std::env::var("CAPTUREFAB_SCREENSHOT_FRAMES")
                .ok()
                .as_deref(),
            std::env::var("CAPTUREFAB_SCREENSHOT_FPS").ok().as_deref(),
        )
    }

    pub(super) fn parse(frames: Option<&str>, fps: Option<&str>) -> Result<Self> {
        let frames = frames.unwrap_or("1").parse::<u32>().map_err(|_| {
            anyhow::anyhow!("CAPTUREFAB_SCREENSHOT_FRAMES must be an integer from 1 to 900")
        })?;
        let fps = fps.unwrap_or("8").parse::<u32>().map_err(|_| {
            anyhow::anyhow!("CAPTUREFAB_SCREENSHOT_FPS must be an integer from 1 to 60")
        })?;
        anyhow::ensure!(
            (1..=900).contains(&frames),
            "CAPTUREFAB_SCREENSHOT_FRAMES must be an integer from 1 to 900"
        );
        anyhow::ensure!(
            (1..=60).contains(&fps),
            "CAPTUREFAB_SCREENSHOT_FPS must be an integer from 1 to 60"
        );
        Ok(Self {
            frames,
            index: 0,
            interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            next: None,
        })
    }

    /// More than one frame: written by `VideoSink`, with a timing sidecar.
    pub(super) fn is_video(&self) -> bool {
        self.frames > 1
    }

    pub(super) fn path(&self, base: &std::path::Path) -> PathBuf {
        if self.frames == 1 {
            return base.to_owned();
        }
        let stem = base.file_stem().unwrap_or_default().to_string_lossy();
        base.with_file_name(format!("{stem}-{:04}.png", self.index))
    }

    fn more(&mut self) -> bool {
        self.index += 1;
        self.index < self.frames
    }

    fn ready_at(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    /// Schedule the next frame on a fixed grid, so frames do not drift; a
    /// late frame restarts the grid rather than bursting to catch up.
    fn requested(&mut self) {
        self.requested_at(Instant::now());
    }

    fn requested_at(&mut self, now: Instant) {
        self.next = Some(match self.next {
            Some(next) if next + self.interval > now => next + self.interval,
            _ => now,
        });
    }

    /// 30 s, plus max(interval, 250 ms) per frame for a sequence; the
    /// website media script mirrors this for its own deadline.
    pub(super) fn timeout(&self) -> Duration {
        // Allow startup and painting time, while keeping even a one-frame/s
        // documentation sequence finite. This does not affect a single shot.
        Duration::from_secs(30)
            + if self.frames > 1 {
                self.interval.max(Duration::from_millis(250)) * self.frames
            } else {
                Duration::ZERO
            }
    }
}

/// What a screenshot scene holds in place while the cameras start.
#[derive(Default)]
pub(super) struct Scene {
    /// The scene's notice stays, whatever commands finish.
    pub(super) notice: bool,
    /// The selected camera is to show no new frames.
    stalled: bool,
    /// The camera held without new frames, once staged.
    pub(super) frozen: Option<String>,
    /// Words that need streaming cameras, applied just before the shot.
    late: Vec<String>,
    /// Keep the scene's pending commands from ever finishing.
    held: Vec<mpsc::Sender<Result<serde_json::Value>>>,
    /// What the selected camera reports as forwarding to, whatever it does.
    forwarding: Option<String>,
    /// When that forward started, UTC Unix milliseconds.
    forwarding_since_ms: Option<u64>,
    /// When the session stops that forward by itself, if it does.
    forwarding_until_ms: Option<u64>,
    /// The file that recording writes, and how far it has got.
    recording_path: Option<String>,
    forwarding_progress: Option<crate::session::ForwardingProgress>,
    /// The selected camera reports that it stopped streaming.
    stopped: bool,
    /// Turn on auto mode on every camera as the streams start.
    pub(super) auto: bool,
    /// Scroll a normal widget after its first staged layout, before capturing.
    scroll_end: Option<&'static str>,
    /// Review the next portion of a sheet after focus has revealed its control.
    scroll_down: bool,
    /// Auth fixtures reveal the save row, then focus the real masked input.
    reveal_control: Option<&'static str>,
    focus_control: Option<&'static str>,
    /// Welcome discovery is display-only and never enumerates real devices.
    welcome: bool,
    searching: bool,
    /// Cameras on saved hubs are on the welcome screen, as discovery finds
    /// them (synthetic: no hub is contacted).
    hubs: bool,
    /// What the welcome screen's discovery finds instead of three
    /// simulated cameras (synthetic: nothing is enumerated).
    found: Option<Found>,
    /// The selected camera streams, but no frame ever reaches the screen.
    pub(super) no_picture: bool,
    /// The selected camera reads as GigE Vision (display only).
    gige: bool,
}

/// What a welcome scene's discovery finds; see `Scene::found`.
#[derive(Clone, Copy)]
enum Found {
    /// One simulated camera.
    One,
    /// More cameras than fit, of every kind, in discovery's ID order.
    Mixed,
}

/// `Found::Mixed`: screens and webcams sort first by ID, as on a Mac.
fn mixed_devices() -> Vec<crate::types::CameraInfo> {
    use crate::types::{CameraInfo, Transport};
    let device =
        |id: &str, transport: Transport, vendor: &str, model: &str, address: Option<&str>| {
            CameraInfo {
                id: id.into(),
                transport,
                vendor: vendor.into(),
                model: model.into(),
                serial: id.rsplit(':').next().unwrap_or(id).into(),
                address: address.map(str::to_owned),
                profile: Default::default(),
            }
        };
    let host = |id: &str, model: &str| device(id, Transport::Media, "macos", model, Some(id));
    vec![
        host("avfoundation:Capture screen 0", "Capture screen 0"),
        host("avfoundation:FaceTime HD Camera", "FaceTime HD Camera"),
        host("avfoundation:HD Pro Webcam C920", "HD Pro Webcam C920"),
        device(
            "gige:192.168.10.21",
            Transport::GigE,
            "Example Vision",
            "Area camera (display fixture)",
            Some("192.168.10.21"),
        ),
        device(
            "scanner:flatbed",
            Transport::Scanner,
            "Example Imaging",
            "Flatbed",
            None,
        ),
        device(
            "usb:EX0042",
            Transport::Usb3,
            "Example Vision",
            "USB3 camera (display fixture)",
            None,
        ),
    ]
}

impl Scene {
    /// Make the snapshot say what the scene holds in place; from `tick()`
    /// after every refresh, so it never reverts.
    pub(super) fn patch(&self, snapshot: &mut SessionSnapshot) {
        if self.welcome {
            snapshot.devices = match (self.searching, self.found) {
                (true, _) => Vec::new(),
                (false, Some(Found::One)) => vec![crate::transport::simulator::info_named("sim:0")],
                (false, Some(Found::Mixed)) => mixed_devices(),
                (false, None) => ["sim:0", "sim:line", "sim:thermal"]
                    .into_iter()
                    .map(crate::transport::simulator::info_named)
                    .collect(),
            };
        }
        if self.gige
            && let Some(connected) = &mut snapshot.connected
        {
            connected.transport = crate::types::Transport::GigE;
        }
        if self.hubs
            && !snapshot
                .devices
                .iter()
                .any(|d| crate::hubs::is_source(&d.id))
        {
            for (hub, camera, vendor, model) in [
                ("home", "camera.front_door", "Home Assistant", "Front door"),
                ("home", "camera.garage", "Home Assistant", "Garage"),
                ("nvr", "driveway", "Frigate", "Driveway"),
            ] {
                let id = format!("hub:{hub}/{camera}");
                snapshot.devices.push(crate::types::CameraInfo {
                    id: id.clone(),
                    transport: crate::types::Transport::Media,
                    vendor: vendor.into(),
                    model: model.into(),
                    serial: format!("{hub}/{camera}"),
                    address: Some(id),
                    profile: Default::default(),
                });
            }
        }
        if self.forwarding.is_none() && !self.stopped {
            return;
        }
        let active = snapshot.active_camera.clone();
        let selected = snapshot
            .cameras
            .iter_mut()
            .filter(|camera| Some(&camera.info.id) == active.as_ref());
        for camera in selected {
            if self.forwarding.is_some() {
                camera.forwarding = self.forwarding.clone();
                camera.forwarding_id = self.forwarding.as_ref().map(|_| SCENE_FORWARDING_ID.into());
                camera.forwarding_since_ms = self.forwarding_since_ms;
                camera.forwarding_until_ms = self.forwarding_until_ms;
                camera.recording_path = self.recording_path.clone();
                camera.forwarding_progress = self.forwarding_progress.clone();
            }
            camera.streaming &= !self.stopped;
        }
        if self.forwarding.is_some() {
            snapshot.forwarding = self.forwarding.clone();
            snapshot.forwarding_id = self.forwarding.as_ref().map(|_| SCENE_FORWARDING_ID.into());
            snapshot.forwarding_since_ms = self.forwarding_since_ms;
            snapshot.forwarding_until_ms = self.forwarding_until_ms;
            snapshot.recording_path = self.recording_path.clone();
            snapshot.forwarding_progress = self.forwarding_progress.clone();
        }
        snapshot.streaming &= !self.stopped;
    }
}

/// The identity of the forward or recording a scene shows.
const SCENE_FORWARDING_ID: &str = "00000000000000000000000000000001";

/// An error long enough to test that notices stay on one line.
const LONG_ERROR: &str = "Native camera discovery: this development build has no embedded \
FFmpeg; rebuild with CAPTUREFAB_FFMPEG_BINARY pointing to a target-compatible static FFmpeg, \
or set CAPTUREFAB_FFMPEG to an absolute path";

impl Workbench {
    /// Put the workbench in a named state for a screenshot. Words:
    /// - layout: `help`, `capture` and `forward` or `video` (the sidebar on
    ///   that tab, floating where it cannot dock), `features`,
    ///   `destinations`, `logs`, `image`, `focus`, `exposure`, `noscopes`,
    ///   `inspector`, `noinspector`, `nosidebar`, `peek` (the floating
    ///   inspector, unless a tab word floats the sidebar; use a narrow
    ///   size), `light`, `dark`
    /// - notices, which stay put: `notice` (done), `warning` (discovery
    ///   warnings), `error`, `notice-long` (an error too long for the bar)
    /// - activity: `searching` (with `welcome`: discovery never finishes),
    ///   `pending` (stopping the selected camera's stream never finishes),
    ///   `starting` (the selected camera stopped, and starting it never
    ///   finishes), `saved` (the selected camera just saved a capture, its
    ///   24th this session),
    ///   `copied` (the session command was just copied), `stalled` (the
    ///   selected camera's frames stopped arriving), `auto` (every camera in
    ///   auto mode), `forwarding` (the selected camera forwards to a URL),
    ///   `recording` (the selected camera records to a file, for 1:23),
    ///   `timed` (with `recording`: the session stops it after 1 h),
    ///   `dropped` (with `recording`: the encoder dropped frames)
    /// - `tags`: the capture tags filled in and open; `dataset`: the Dataset
    ///   section open; `catalog`: Catalog captures automatically ticked;
    ///   `by-label`: "One folder per label" file names, one folder per Label tag;
    ///   `side-hub`: cameras from saved hubs in the camera
    ///   list (with `welcome`, on the welcome cards)
    /// - review fixtures: `network`, `network-empty`, `network-error`,
    ///   `destinations-remove`, `folder-error`, `settings-credentials`,
    ///   `settings-credential-error`, `settings-assistant`,
    ///   `settings-assistant-error`, `analytics-measurements`,
    ///   `quick-command`, `quick-command-plan`, `quick-command-invalid`,
    ///   `quick-command-running`, `quick-command-done`, `quick-command-setup`,
    ///   `quick-command-thinking`, `quick-command-error`,
    ///   `quick-command-task-setup`, `quick-command-task-running`,
    ///   `quick-command-task-approval`, `quick-command-task-paused-human`,
    ///   `quick-command-task-limit`, `quick-command-task-done`,
    ///   `analytics-measurements-populated`, `analytics-invalid`,
    ///   `close-error`;
    ///   `sheet-bottom` and `inspector-bottom` expose their lower content
    /// - welcome: `side-one` (with `welcome`: discovery finds one simulated
    ///   camera), `side-mixed` (finds more cameras than fit, of every kind),
    ///   `camera-denied` (the system refused a webcam), `usb-denied` (with
    ///   `side-mixed`: Linux refused the USB3 camera, whose udev rule the
    ///   footer offers to copy)
    /// - first frame: `no-first-frame` (the selected camera streams, but its
    ///   first frame is late), `side-gige` (it reads as GigE Vision)
    /// - failures: `write-error` (setting the selected camera's exposure
    ///   time was refused) and `connect-failed` (connecting to a typed
    ///   address failed), each with its error notice; `editor` (the folder
    ///   editor open, its save refused for a missing field)
    ///
    /// Other words go to each area's `scene_*` hook, first as the scene is
    /// set up and, if no area took them, again once the cameras stream.
    pub(super) fn apply_scene(&mut self, scene: &[&str]) {
        for word in scene {
            match *word {
                // run_capture connects no cameras. The snapshot supplies only
                // built-in simulator cards, without real device discovery.
                "welcome" => self.scene.welcome = true,
                "side-one" => self.scene.found = Some(Found::One),
                "side-mixed" => self.scene.found = Some(Found::Mixed),
                "side-gige" => self.scene.gige = true,
                "camera-denied" => {
                    // As connecting to a webcam the system refused would.
                    let target = "avfoundation:0";
                    self.scene_settle(
                        Pending {
                            target: Some(target.into()),
                            ..Pending::unanswered(Job::Connect)
                        },
                        Err(anyhow::anyhow!(
                            "{}: allow this app in System Settings > Privacy & Security > Camera",
                            crate::media::CAMERA_ACCESS_DENIED
                        )),
                    );
                }
                "usb-denied" => {
                    // As connecting to a USB3 camera Linux refused would.
                    self.scene_settle(
                        Pending {
                            target: Some("usb:EX0042".into()),
                            ..Pending::unanswered(Job::Connect)
                        },
                        Err(crate::doctor::usb_access_error(
                            "USB3 camera (display fixture)",
                            std::path::Path::new("/dev/bus/usb/002/003"),
                        )),
                    );
                }
                "side-hub" => self.scene.hubs = true,
                "help" => self.help_open = true,
                "agents" => self.show_agent_setup(true),
                "from-disk-image" | "translocated" => {
                    let location = if *word == "translocated" {
                        crate::volumes::InstallLocation::Translocated
                    } else {
                        crate::volumes::InstallLocation::DiskImage
                    };
                    self.assistant.commands.install = Some(location);
                    let (notice, detail) = agents::install_notice(location);
                    self.scene_notice(Some((notice.into(), Some(detail.into()), Level::Warning)));
                }
                "analytics" => self.analytics.open = true,
                "capture" | "forward" | "video" => {
                    self.tab = if *word == "capture" {
                        Tab::Capture
                    } else {
                        Tab::Forward
                    };
                    self.sidebar_open = true;
                    self.sidebar_peek = true;
                }
                "features" => self.inspector_open = true,
                "destinations" => self.capture_to.manager_open = true,
                "logs" => self.logs_open = true,
                "image" => self.image_mode = true,
                "focus" => self.focus.open = true,
                "exposure" => self.exposure_open = true,
                "noscopes" => {
                    self.exposure_open = false;
                    self.focus.open = false;
                }
                "noinspector" => self.inspector_open = false,
                "nosidebar" => self.sidebar_open = false,
                "inspector" => self.inspector_open = true,
                "peek" => {
                    if !self.sidebar_peek {
                        self.inspector_peek = true;
                    }
                }
                "light" => self.appearance = Appearance::Light,
                "dark" => self.appearance = Appearance::Dark,
                "sheet-bottom" => self.scene.scroll_end = Some(SHEET_BODY),
                "sheet-down" => self.scene.scroll_down = true,
                "inspector-bottom" => self.scene.scroll_end = Some(inspector::SCROLL),
                "close-error" => {
                    // A display fixture only: no failed write, close request,
                    // preference destination or native credential is involved.
                    let _ = self.show_close_error(
                        CloseTarget::Quit,
                        "Could not save settings: Permission denied while replacing the preferences file. Check that the session folder is writable, then retry.".into(),
                    );
                }
                "notice" => self.scene_notice(command_notice(
                    &Job::Capture,
                    &Ok(json!({"files": ["capture-0001.png"], "count": 1})),
                )),
                // Settled, so the discovery's issues are kept as well.
                "warning" => self.scene_settle(
                    Pending::unanswered(Job::Discover),
                    Ok(json!({
                        "devices": [{"id": "sim:0"}],
                        "warnings": [
                            "USB discovery: access denied; check the device permissions",
                            "Native camera discovery worker stopped",
                        ],
                    })),
                ),
                "error" => self.scene_notice(command_notice(
                    &Job::Capture,
                    &Err(anyhow::anyhow!("the camera stopped responding")),
                )),
                "notice-long" => self.scene_notice(command_notice(
                    &Job::Connect,
                    &Err(anyhow::anyhow!(LONG_ERROR)),
                )),
                "searching" => {
                    self.scene.searching = true;
                    self.hold(Job::Discover, None);
                }
                "stalled" => self.scene.stalled = true,
                "auto" => self.scene.auto = true,
                "forwarding" => {
                    self.scene.forwarding =
                        Some("rtsp://operator:secret@192.168.1.20:8554/line-1".into());
                    self.scene.forwarding_since_ms = Some(epoch_ms().saturating_sub(83_000));
                }
                "recording" => {
                    let output = prefs::default_record_output();
                    // The session reports the file it named, so the card
                    // shows it even for a recording started elsewhere.
                    self.scene.recording_path = Some(crate::naming::preview_numbered(
                        &output,
                        "Sim-SIM0",
                        &Default::default(),
                        chrono::Local::now(),
                        Some(1),
                    ));
                    self.scene.forwarding = Some(output);
                    self.scene.forwarding_since_ms = Some(epoch_ms().saturating_sub(83_000));
                    self.scene.forwarding_progress = Some(crate::session::ForwardingProgress {
                        id: SCENE_FORWARDING_ID.into(),
                        accepted: 2_490,
                        written: 2_490,
                        ..Default::default()
                    });
                    self.scene.late.push("recording-file".into());
                }
                "dropped" => {
                    if let Some(progress) = &mut self.scene.forwarding_progress {
                        progress.dropped = 12;
                        progress.late = 12;
                    }
                }
                "timed" => {
                    let since = epoch_ms().saturating_sub(83_000);
                    self.scene.forwarding_until_ms = Some(since + 3_600_000);
                }
                "tags" => {
                    self.tags = prefs::CaptureTags {
                        label: "good".into(),
                        line: "L1".into(),
                        station: "Inspection 2".into(),
                        part: "A-100".into(),
                        lot: "2026-10-17-A".into(),
                        note: String::new(),
                    };
                    let _ = self.handle_message(Message::InspectorDisclosure(
                        inspector::Section::CaptureTags,
                    ));
                }
                // The "One folder per label" file names, one folder per Label tag.
                "by-label" => {
                    self.output = prefs::capture_root()
                        .join("{tag:label}")
                        .join("{camera}-{datetime}-{seq}.png")
                        .to_string_lossy()
                        .into_owned();
                }
                "dataset" => {
                    let _ = self
                        .handle_message(Message::InspectorDisclosure(inspector::Section::Dataset));
                }
                "catalog" => {
                    let _ = self.handle_message(Message::Dataset(dataset::Msg::Auto(true)));
                }
                "connect-failed" => {
                    let target = "192.168.10.50";
                    self.address = target.into();
                    self.scene_settle(
                        Pending {
                            target: Some(target.into()),
                            ..Pending::unanswered(Job::Connect)
                        },
                        // The copy a real unanswered probe shows, from a
                        // computer whose only adapter is on another subnet.
                        Err(crate::types::Categorized::error(
                            crate::types::ErrorKind::Timeout,
                            format!(
                                "{} (GVCP command 0x0084 went unanswered)",
                                crate::network::explain_gige_unreachable(
                                    std::net::Ipv4Addr::new(192, 168, 10, 50),
                                    &[crate::network::NetworkInterface {
                                        name: "en0".into(),
                                        address: std::net::Ipv4Addr::new(192, 168, 1, 23),
                                        netmask: std::net::Ipv4Addr::new(255, 255, 255, 0),
                                        mtu: Some(1500),
                                        speed_mbps: None,
                                        kind: crate::network::InterfaceKind::Wireless,
                                        up: Some(true),
                                    }],
                                    false,
                                    std::time::Duration::from_secs(5),
                                )
                            ),
                        )),
                    );
                }
                "editor" => {
                    self.capture_to
                        .update(destinations::Message::OpenManager, &mut self.output);
                    self.capture_to
                        .update(destinations::Message::AddFolder, &mut self.output);
                    self.capture_to
                        .update(destinations::Message::Save, &mut self.output);
                }
                "saved" | "copied" | "pending" | "starting" | "write-error" | "kind-scanner"
                | "kind-unknown" | "no-first-frame" => self.scene.late.push((*word).into()),
                word if self.area_scene(word, false) => {
                    let controls = match word {
                        "quick-command-exported" => Some((
                            "quick-command-export-preview",
                            "quick-command-export-preview",
                        )),
                        _ => None,
                    };
                    if let Some((reveal, focus)) = controls {
                        self.scene.reveal_control = Some(reveal);
                        self.scene.focus_control = Some(focus);
                    }
                }
                word => self.scene.late.push(word.into()),
            }
        }
    }

    /// Offer a scene word to each area's hook; whether one took it.
    fn area_scene(&mut self, word: &str, late: bool) -> bool {
        self.scene_chrome(word, late)
            || self.scene_side(word, late)
            || self.scene_stage(word, late)
            || self.scene_inspector(word, late)
            || self.scene_network(word, late)
            || self.scene_video_analytics(word, late)
            || self.scene_assistant_settings(word, late)
            || self.scene_assistant_quick(word, late)
            || self.scene_assistant_task(word, late)
            || self.scene_performance(word, late)
            || self.scene_resources(word, late)
            || self.scene_updates(word, late)
            || self.scene_sheets(word, late)
            || self.scene_sign_in(word, late)
    }

    /// Settle `pending` with `result`, as if it had just finished, and keep
    /// its notice for the scene.
    fn scene_settle(&mut self, pending: Pending, result: Result<serde_json::Value>) {
        self.scene.notice = false;
        self.settle(&pending, &result);
        if self.notice.is_some() {
            self.notice_shown.set(1.0);
            self.scene.notice = true;
        }
    }

    /// Apply the words that need connected, streaming cameras.
    fn stage_scene(&mut self) {
        let camera = self.snapshot.active_camera.clone();
        for word in std::mem::take(&mut self.scene.late) {
            match word.as_str() {
                "kind-scanner" | "kind-unknown" => self.scene_display_profile(&word),
                "no-first-frame" => {
                    // Streaming with no frame on screen, silent past its
                    // limit: the first frame is late.
                    self.scene.no_picture = true;
                    self.shown = None;
                    self.scene.stalled = true;
                }
                "saved" => {
                    let result = json!({
                        "files": ["capture-0001.png"],
                        "frames": [{"width": 640, "height": 480}],
                        "count": 1,
                    });
                    if let Some(id) = &camera {
                        self.session_saved.insert(id.clone(), 23);
                    }
                    if let Some(saved) = Saved::from_result(camera.clone(), &result, self.now) {
                        self.note_saved(saved);
                    }
                    // Made here, not on a worker: the shot is of the next frame.
                    if let Some((path, frame)) = self.thumb_wanted.take()
                        && let Ok((width, height, rgba)) = crate::frame::preview_rgba(
                            &frame,
                            super::notice::THUMB_BOUNDS.0,
                            super::notice::THUMB_BOUNDS.1,
                        )
                    {
                        let thumb = iced::widget::image::Handle::from_rgba(width, height, rgba);
                        self.saved_thumb(&path, Some(thumb));
                    }
                    self.scene_notice(command_notice(&Job::Capture, &Ok(result)));
                }
                "recording-file" => {
                    let path = crate::naming::preview(
                        &prefs::default_record_output(),
                        "Sim-SIM0",
                        &Default::default(),
                        chrono::Local::now(),
                    )
                    .replace("####", "0001");
                    self.last_recording = Some((camera.clone(), path));
                }
                // Copy buttons confirm in place, with no notice.
                "copied" => self.note_copied(self.session_command()),
                "pending" | "starting" => {
                    if !self.scene.notice {
                        self.notice = None;
                        self.notice_shown.set(0.0);
                    }
                    if word == "starting" {
                        self.scene.stopped = true;
                        self.scene.patch(&mut self.snapshot);
                        self.hold(Job::Start, camera.clone());
                    } else {
                        self.hold(Job::Stop, camera.clone());
                    }
                }
                "write-error" => {
                    let feature = "ExposureTime";
                    self.edits.insert(feature.into(), "5".into());
                    self.scene_settle(
                        Pending {
                            camera: camera.clone(),
                            ..Pending::unanswered(Job::Set(feature.into()))
                        },
                        Err(anyhow::anyhow!("5 is below the minimum of 10")),
                    );
                }
                word => {
                    if !self.area_scene(word, true) {
                        eprintln!("screenshot scene: no area takes {word:?}");
                    }
                }
            }
        }
        if self.scene.stalled
            && let Some(id) = camera
            && let Some(live) = self.liveness.get_mut(&id)
        {
            live.silent_since(
                self.now
                    .checked_sub(Duration::from_secs(8))
                    .unwrap_or(self.born),
            );
            self.scene.frozen = Some(id);
        }
    }

    /// The simulator has no scanner or unclassified backend. Supply those
    /// display profiles only after acquisition has completed, without changing
    /// the session, opening a scanner or submitting any camera operation.
    fn scene_display_profile(&mut self, word: &str) {
        use crate::camera_profile::{CameraCapability, CameraKind, DetectionSource};
        let Some(id) = self.snapshot.active_camera.clone() else {
            return;
        };
        let Some(mut info) = self
            .snapshot
            .cameras
            .iter()
            .find(|camera| camera.info.id == id)
            .map(|camera| camera.info.clone())
        else {
            return;
        };
        let scanner = word == "kind-scanner";
        info.vendor = "Example Imaging".into();
        info.model = if scanner {
            "Document scanner (display fixture)"
        } else {
            "Unclassified camera (display fixture)"
        }
        .into();
        info.profile.kind = if scanner {
            CameraKind::DocumentScanner
        } else {
            CameraKind::Unknown
        };
        info.profile.source = if scanner {
            DetectionSource::Transport
        } else {
            DetectionSource::Unknown
        };
        info.profile.evidence = vec![
            "Synthetic display profile for UI review; no device of this kind was opened.".into(),
        ];
        if scanner {
            info.transport = crate::types::Transport::Scanner;
            info.profile.capabilities = vec![
                CameraCapability::ScanResolution,
                CameraCapability::ScanSource,
            ];
            let feature = |name: &str,
                           kind: &str,
                           value: serde_json::Value,
                           choices: &[&str],
                           unit: Option<&str>| FeatureInfo {
                name: name.into(),
                display_name: name.into(),
                kind: kind.into(),
                value: Some(value),
                writable: true,
                description: "Display-only scanner control; no scanner operation is performed."
                    .into(),
                unit: unit.map(str::to_owned),
                min: (name == "ScanResolution").then_some(75.0),
                max: (name == "ScanResolution").then_some(600.0),
                inc: (name == "ScanResolution").then_some(1.0),
                choices: choices.iter().map(|value| (*value).into()).collect(),
                error: None,
            };
            self.snapshot.features = vec![
                feature(
                    "ScanSource",
                    "Enumeration",
                    json!("Flatbed"),
                    &["Default", "Flatbed", "Feeder"],
                    None,
                ),
                feature("ScanResolution", "Integer", json!(300), &[], Some("dpi")),
                feature(
                    "ScanMode",
                    "Enumeration",
                    json!("Color"),
                    &["Color", "Gray", "Lineart"],
                    None,
                ),
            ];
            self.snapshot.streaming = false;
            // A scanner starts without a document image. Drop the simulator's
            // frame so this fixture exercises the actual Ready to scan
            // guidance.
            self.shown = None;
        }
        for camera in self
            .snapshot
            .cameras
            .iter_mut()
            .filter(|camera| camera.info.id == id)
        {
            camera.info = info.clone();
            if scanner {
                camera.streaming = false;
                camera.features = self.snapshot.features.clone().into();
            }
        }
        self.snapshot.connected = Some(info);
        self.tick_inspector();
    }

    /// Show a notice that stays for the whole scene, fully faded in.
    fn scene_notice(&mut self, notice: Option<(String, Option<String>, Level)>) {
        if let Some((text, detail, level)) = notice {
            self.put_notice(text, detail, level);
            self.notice_shown.set(1.0);
            self.scene.notice = true;
        }
    }

    /// A pending command that never finishes.
    fn hold(&mut self, job: Job, camera: Option<String>) {
        let (sender, receiver) = mpsc::channel();
        self.scene.held.push(sender);
        self.pending.push(Pending {
            camera,
            at: self.now,
            receiver,
            ..Pending::unanswered(job)
        });
    }

    /// A pending command doing `job` that never finishes, for an area's
    /// scene words; returned so the area can say what it acts on.
    pub(super) fn scene_hold(&mut self, job: Job) -> &mut Pending {
        self.hold(job, None);
        self.pending.last_mut().expect("just held")
    }

    /// Whether a screenshot scene is staged: the workbench then holds still
    /// until the shot. The capture renders the layers last drawn again, and
    /// they keep their text only weakly, so text laid out anew since that
    /// draw (a frame count, say) would come out blank.
    pub(super) fn shot_staged(&self) -> bool {
        self.screenshot
            .as_ref()
            .is_some_and(|request| request.staged)
    }

    /// A video frame after the first may be staged as soon as a new camera
    /// frame arrives (`FrameArrived`).
    pub(super) fn video_awaits_camera(&self) -> bool {
        self.screenshot.as_ref().is_some_and(|request| {
            request.video.as_ref().is_some_and(|video| {
                !video.draining && video.first_staged.is_some() && video.backlog.is_none()
            }) && !request.staged
                && !request.requested
        })
    }

    /// A video frame is staged and waits for a draw of it.
    pub(super) fn video_awaits_draw(&self) -> bool {
        self.screenshot
            .as_ref()
            .is_some_and(|request| request.video.is_some() && request.staged && !request.requested)
    }

    pub(super) fn screenshot_tick(&mut self) -> Task<Message> {
        let Some(mut request) = self.screenshot.take() else {
            return Task::none();
        };
        let mut task = Task::none();
        if let Some(video) = &mut request.video {
            // A video frame is released on readback (`video_frame_read`);
            // here the writers report, and a refused frame is retried.
            let frames = request.sequence.frames;
            let retried = video.poll().and_then(|_| match video.backlog {
                Some(_) => video.retry(),
                None => Ok(false),
            });
            match retried {
                Err(error) => {
                    return finish(&request, Some(format!("Save renderer screenshot: {error}")));
                }
                // The frame the full queue refused is now queued.
                Ok(true) => release_video(&mut request),
                Ok(false) => {}
            }
            if video_drained(&request) {
                let video = request.video.take().expect("video sequence");
                return finish(
                    &request,
                    match video.close() {
                        Ok(written) if written == frames => None,
                        Ok(written) => Some(format!(
                            "Save renderer screenshot: wrote {written} of {frames} frames"
                        )),
                        Err(error) => Some(format!("Save renderer screenshot: {error}")),
                    },
                );
            }
        }
        if let Some(receiver) = &request.save {
            match receiver.try_recv() {
                Ok(Ok(())) if request.sequence.more() => {
                    // Release the snapshot freeze between frames. Acquisition,
                    // real CLI commands and normal UI updates keep running.
                    request.staged = false;
                    request.requested = false;
                    request.save = None;
                }
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
        } else if request.staged && self.scene.reveal_control.is_some() {
            task = accessibility::focus_in_window(
                self.scene.reveal_control.take().unwrap(),
                self.main_window,
            );
        } else if request.staged && self.scene.focus_control.is_some() {
            task = operation::focus(self.scene.focus_control.take().unwrap());
        } else if request.staged && self.scene.scroll_end.is_some() {
            // The first staged frame establishes the scrollable's real
            // extent. Give the operation another paint before the screenshot.
            let id = self.scene.scroll_end.take().unwrap();
            task = operation::snap_to_end(id);
            if id == inspector::SCROLL {
                task = task.chain(operation::snap_to_end(inspector::SIDEBAR_SCROLL));
            }
        } else if request.staged && self.scene.scroll_down {
            self.scene.scroll_down = false;
            task = operation::scroll_by(SHEET_BODY, operation::AbsoluteOffset { x: 0.0, y: 150.0 });
        } else if request.staged && !request.requested {
            // A single shot is taken one tick after staging, so the staged
            // state is what gets drawn. A video frame waits for an observed
            // draw instead: the screenshot renders the layers last drawn, so
            // a draw since staging proves they hold the staged state.
            match &request.video {
                None => {
                    task = window::latest()
                        .and_then(window::screenshot)
                        .map(Message::Screenshot);
                    request.requested = true;
                    request.sequence.requested();
                }
                Some(video) if self.draws.last().1 > video.staged_draws => {
                    // Addressed directly, the screenshot runs as this
                    // update ends, before the next draw, rather than after
                    // a round trip to look the window up.
                    task = window::screenshot(self.main_window).map(Message::Screenshot);
                    request.requested = true;
                    request.sequence.requested();
                }
                Some(_) => {}
            }
        } else if !request.requested
            && !request.staged
            && !request.video.as_ref().is_some_and(|video| video.draining)
            && stage_due(&request, self.frame_id, Instant::now())
            && self.snapshot.cameras.len() == request.cameras as usize
            // Pending commands hold back a single shot and a video's first
            // frame; later video frames record their spinners as they are.
            && (self.pending.len() == self.scene.held.len()
                || request
                    .video
                    .as_ref()
                    .is_some_and(|video| video.first_staged.is_some()))
        {
            if request.streams_started.is_none() {
                for id in self
                    .snapshot
                    .cameras
                    .iter()
                    .map(|camera| camera.info.id.clone())
                    .collect::<Vec<_>>()
                {
                    self.send_to(&id, Job::Start, SessionCommand::Start);
                    if self.scene.auto {
                        self.send_to(
                            &id,
                            Job::AutoOn,
                            SessionCommand::Auto {
                                balance: Some(self.balance),
                            },
                        );
                    }
                }
                request.streams_started = Some(Instant::now());
            } else if request
                .streams_started
                .is_some_and(|started| started.elapsed() > Duration::from_millis(1600))
                && self.analytics_staged()
                && self
                    .snapshot
                    .cameras
                    .iter()
                    .all(|camera| camera.streaming && camera.frames >= 3)
            {
                self.stage_scene();
                request.staged = true;
                if let Some(video) = &mut request.video {
                    let held = self.pending.len() != self.scene.held.len();
                    let cameras = self
                        .snapshot
                        .cameras
                        .iter()
                        .map(|camera| (camera.info.id.clone(), camera.frames))
                        .collect();
                    video.stage(
                        Instant::now(),
                        self.draws.last().1,
                        self.frame_id,
                        cameras,
                        held,
                    );
                }
            }
        }
        if let Some(video) = &mut request.video
            && !request.staged
            && !request.requested
        {
            // Stage the next frame when it is due, without ticking for it;
            // a new camera frame arriving stages it too (`FrameArrived`).
            let now = Instant::now();
            let due = match video.staged_at.filter(|_| video.first_staged.is_some()) {
                Some(at) => camera_wait(request.sequence.interval, at)
                    .into_iter()
                    .find(|due| *due > now),
                None => request.sequence.next.filter(|next| *next > now),
            };
            if let Some(due) = due
                && video.wake != Some(due)
            {
                video.wake = Some(due);
                task = Task::batch([task, wake_at(due)]);
            }
        }
        if request.started.elapsed() > request.sequence.timeout() {
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

    /// A video frame's pixels arrived: note its timing, queue its write and
    /// release the scene for the next frame, which a full queue holds back.
    pub(super) fn video_frame_read(&mut self, shot: window::Screenshot) -> Task<Message> {
        let Some(request) = &mut self.screenshot else {
            return Task::none();
        };
        let Some(video) = &mut request.video else {
            return Task::none();
        };
        let path = request.sequence.path(&request.path);
        if let Err(error) = video.record(request.sequence.index, &path) {
            let request = self.screenshot.take().expect("screenshot request");
            return finish(
                &request,
                Some(format!("Write renderer frame timing: {error}")),
            );
        }
        match video.send((request.sequence.index, shot, path)) {
            Ok(true) => {
                release_video(request);
                if !request.staged {
                    // Tick now rather than up to a tick later: the frames
                    // held back meanwhile are taken, freeing the frame
                    // watcher, the snapshot catches up and the next frame is
                    // staged when due, in time for the coming draw.
                    self.tick_frames();
                    return self.tick();
                }
            }
            Ok(false) => {}
            Err(error) => {
                let request = self.screenshot.take().expect("screenshot request");
                return finish(&request, Some(format!("Save renderer screenshot: {error}")));
            }
        }
        Task::none()
    }
}

/// Whether the next frame may be staged. A single shot and a video's first
/// frame follow the sequence's schedule. Later video frames follow the
/// camera instead: each is staged once the preview shows a camera frame the
/// last did not, at least 0.9 of an interval after it, or 1.5 intervals
/// after it without one. A camera sampled on an unrelated clock at its own
/// rate would otherwise show some frames twice and skip others.
fn stage_due(request: &ScreenshotRequest, frame_id: Option<u64>, now: Instant) -> bool {
    match &request.video {
        Some(video) if video.first_staged.is_some() => {
            let Some(at) = video.staged_at else {
                return true;
            };
            let [spaced, ends] = camera_wait(request.sequence.interval, at);
            now >= ends || now >= spaced && frame_id != video.preview_frame
        }
        _ => request.sequence.ready_at(now),
    }
}

/// When a video frame staged at `at` may be followed by one showing a new
/// camera frame, and when by one regardless.
fn camera_wait(interval: Duration, at: Instant) -> [Instant; 2] {
    [at + interval * 9 / 10, at + interval * 3 / 2]
}

/// A tick at `at`.
fn wake_at(at: Instant) -> Task<Message> {
    Task::perform(
        async move {
            let (send, receive) = iced::futures::channel::oneshot::channel();
            let started = std::thread::Builder::new()
                .name("capturefab-screenshot-wake".into())
                .spawn(move || {
                    std::thread::sleep(at.saturating_duration_since(Instant::now()));
                    let _ = send.send(());
                });
            if started.is_ok() {
                let _ = receive.await;
            }
        },
        |()| Message::Tick,
    )
}

/// Record the screenshot's outcome and close the app.
fn finish(request: &ScreenshotRequest, error: Option<String>) -> Task<Message> {
    *request
        .outcome
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = error;
    iced::exit()
}

/// The writers took the frame just read: stage the next, or drain after the
/// last.
fn release_video(request: &mut ScreenshotRequest) {
    if request.sequence.more() {
        // Release the snapshot freeze between frames. Acquisition, real CLI
        // commands and normal UI updates keep running.
        request.staged = false;
        request.requested = false;
    } else if let Some(video) = &mut request.video {
        video.draining = true;
    }
}

/// Every frame of a drained video sequence is written.
fn video_drained(request: &ScreenshotRequest) -> bool {
    request.video.as_ref().is_some_and(|video| {
        video.draining && video.backlog.is_none() && video.written() == request.sequence.frames
    })
}

pub(super) fn save_screenshot(shot: &window::Screenshot, path: &std::path::Path) -> Result<()> {
    save_screenshot_with(shot, path, png::Compression::Default)
}

pub(super) fn save_screenshot_with(
    shot: &window::Screenshot,
    path: &std::path::Path,
    compression: png::Compression,
) -> Result<()> {
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
    encoder.set_compression(compression);
    encoder.write_header()?.write_image_data(&shot.rgba)?;
    Ok(())
}

/// A fresh directory for a test's frames.
#[cfg(test)]
pub(super) fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "capturefab-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_panel_peek_scenes_consume_the_modifier_for_either_panel() {
        for tab in ["capture", "forward", "video"] {
            let mut app = bench();
            app.snapshot.connected = Some(app.snapshot.cameras[0].info.clone());
            app.apply_scene(&[tab, "peek"]);
            assert!(app.sidebar_peek);
            assert!(!app.scene.late.iter().any(|word| word == "peek"));
            app.stage_scene();
            assert!(app.sidebar_shown());
        }
        let mut app = bench();
        app.apply_scene(&["features", "peek"]);
        assert!(app.inspector_peek);
        assert!(!app.scene.late.iter().any(|word| word == "peek"));
    }
    use liveness::streaming_camera;

    #[test]
    fn camera_kind_review_fixtures_change_only_the_display_snapshot() {
        for (word, kind) in [
            (
                "kind-scanner",
                crate::camera_profile::CameraKind::DocumentScanner,
            ),
            ("kind-unknown", crate::camera_profile::CameraKind::Unknown),
        ] {
            let mut app = bench();
            app.shown = Some(Shown::Gpu {
                key: MAIN_VIEW.into(),
                size: Size::new(8.0, 8.0),
            });
            app.apply_scene(&[word]);
            assert!(app.pending.is_empty());
            app.stage_scene();
            assert_eq!(app.camera_kind(), kind);
            assert!(app.pending.is_empty());
            assert!(app.handle.snapshot().cameras.is_empty());
            if word == "kind-scanner" {
                assert!(app.scanner_selected());
                assert!(!app.snapshot.streaming);
                assert!(
                    app.shown.is_none(),
                    "scanner readiness replaces the simulator frame"
                );
                assert_eq!(
                    app.snapshot
                        .features
                        .iter()
                        .map(|feature| feature.name.as_str())
                        .collect::<Vec<_>>(),
                    ["ScanSource", "ScanResolution", "ScanMode"]
                );
            } else {
                assert!(
                    app.shown.is_some(),
                    "the unknown profile retains its simulator image"
                );
            }
        }
    }

    #[test]
    fn scanner_review_fixture_routes_the_actual_stage_to_document_ready_guidance() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = destinations::layout_tests::renderer();
        let mut app = bench();
        app.shown = Some(Shown::Gpu {
            key: MAIN_VIEW.into(),
            size: Size::new(8.0, 8.0),
        });
        app.apply_scene(&["kind-scanner"]);
        app.stage_scene();
        let size = Size::new(600.0, 560.0);
        let mut view = app.single_view(Palette::of(false));
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        destinations::layout_tests::prepare_artifact_frame(
            &mut view, &mut tree, &node, &renderer, size,
        );
        let (snapshot, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        assert!(
            snapshot
                .nodes
                .iter()
                .any(|(_, control)| { control.label() == Some("Ready to scan a document") })
        );
        assert!(app.pending.is_empty());
        assert!(app.handle.snapshot().cameras.is_empty());
    }

    #[test]
    fn ui_review_welcome_discovery_uses_only_synthetic_simulator_cards() {
        let mut bench = Workbench::new(SessionHandle::new(), "ui-review-test".into(), true, None);
        bench.apply_scene(&["welcome"]);
        bench.scene.patch(&mut bench.snapshot);
        assert_eq!(bench.snapshot.devices.len(), 3);
        assert!(
            bench
                .snapshot
                .devices
                .iter()
                .all(|camera| camera.id.starts_with("sim:"))
        );
        assert!(!bench.pending(Job::Discover));
        assert!(bench.handle.snapshot().devices.is_empty());
        bench.apply_scene(&["searching"]);
        bench.scene.patch(&mut bench.snapshot);
        assert!(bench.snapshot.devices.is_empty());
        assert!(bench.pending(Job::Discover));
        assert_eq!(bench.pending.len(), bench.scene.held.len());
    }

    #[test]
    fn ui_review_scene_routes_tabs_and_scroll_without_submitting_camera_commands() {
        let mut bench = Workbench::new(SessionHandle::new(), "ui-review-test".into(), true, None);
        bench.apply_scene(&["analytics-measurements", "sheet-bottom"]);
        assert!(bench.analytics.open);
        assert_eq!(bench.analytics.tab, analytics::Tab::Measurements);
        assert_eq!(bench.scene.scroll_end, Some(SHEET_BODY));
        bench.apply_scene(&["sheet-down"]);
        assert!(bench.scene.scroll_down);
        assert!(bench.scene.late.is_empty());
        assert!(bench.pending.is_empty());
        bench.apply_scene(&["close-error"]);
        assert!(bench.close_error.is_some());
        assert!(bench.closing.is_none());
        assert!(bench.pending.is_empty());
    }

    #[test]
    fn screenshot_sequence_preserves_single_shot_and_bounds_work() {
        let base = std::path::Path::new("/tmp/workbench.png");
        let mut single = ScreenshotSequence::parse(None, None).unwrap();
        assert_eq!(single.path(base), base);
        assert_eq!(single.timeout(), Duration::from_secs(30));
        assert!(!single.more());
        assert!(!single.is_video());
        for value in ["0", "901", "-1", "2.5", "invalid"] {
            let error = ScreenshotSequence::parse(Some(value), None).err().unwrap();
            assert_eq!(
                error.to_string(),
                "CAPTUREFAB_SCREENSHOT_FRAMES must be an integer from 1 to 900"
            );
        }
        for value in ["0", "61", "-1", "2.5", "invalid"] {
            let error = ScreenshotSequence::parse(None, Some(value)).err().unwrap();
            assert_eq!(
                error.to_string(),
                "CAPTUREFAB_SCREENSHOT_FPS must be an integer from 1 to 60"
            );
        }
        let most = ScreenshotSequence::parse(Some("900"), Some("60")).unwrap();
        assert!(most.is_video());
        assert_eq!(
            most.timeout(),
            Duration::from_secs(30) + Duration::from_millis(250) * 900
        );
    }

    #[test]
    fn screenshot_sequence_names_every_frame_and_finishes() {
        let base = std::path::Path::new("/tmp/workbench.png");
        let mut sequence = ScreenshotSequence::parse(Some("3"), Some("30")).unwrap();
        assert_eq!(
            sequence.path(base),
            std::path::Path::new("/tmp/workbench-0000.png")
        );
        assert!(sequence.ready_at(Instant::now()));
        let start = Instant::now();
        sequence.requested_at(start);
        sequence.requested_at(start + Duration::from_millis(5));
        let next = sequence.next.unwrap();
        assert_eq!(next, start + sequence.interval);
        assert!(!sequence.ready_at(next - Duration::from_nanos(1)));
        assert!(sequence.ready_at(next));
        assert!(sequence.more());
        assert_eq!(
            sequence.path(base),
            std::path::Path::new("/tmp/workbench-0001.png")
        );
        assert!(sequence.more());
        assert_eq!(
            sequence.path(base),
            std::path::Path::new("/tmp/workbench-0002.png")
        );
        assert!(!sequence.more());
        assert!(sequence.timeout() < Duration::from_secs(31));
    }

    #[test]
    fn requested_schedules_on_a_fixed_grid() {
        let mut sequence = ScreenshotSequence::parse(Some("10"), Some("30")).unwrap();
        let interval = sequence.interval;
        let start = Instant::now();
        sequence.requested_at(start);
        assert_eq!(sequence.next, Some(start));
        // On time, however late within the frame: the grid holds.
        sequence.requested_at(start + Duration::from_millis(3));
        assert_eq!(sequence.next, Some(start + interval));
        sequence.requested_at(start + interval + Duration::from_millis(20));
        assert_eq!(sequence.next, Some(start + interval * 2));
        // A frame later than a whole interval restarts the grid, no burst.
        let late = start + interval * 5;
        sequence.requested_at(late);
        assert_eq!(sequence.next, Some(late));
        sequence.requested_at(late + Duration::from_millis(1));
        assert_eq!(sequence.next, Some(late + interval));
    }

    fn shot_bench(frames: &str, dir: &std::path::Path) -> Workbench {
        let mut bench = bench();
        let request = ScreenshotRequest::new(
            dir.join("frame.png"),
            1,
            Arc::new(Mutex::new(Some("unfinished".into()))),
            ScreenshotSequence::parse(Some(frames), Some("30")).unwrap(),
        )
        .unwrap();
        bench.screenshot = Some(request);
        bench
    }

    fn stage_for_video(bench: &mut Workbench) {
        let draws = bench.draws.last().1;
        let request = bench.screenshot.as_mut().unwrap();
        request.staged = true;
        let video = request.video.as_mut().unwrap();
        video.stage(
            Instant::now(),
            draws,
            Some(7),
            vec![("sim:0".into(), 10)],
            false,
        );
    }

    fn shot() -> window::Screenshot {
        window::Screenshot::new(vec![128; 64], Size::new(4, 4), 1.0)
    }

    fn outcome(bench: &Workbench) -> Arc<Mutex<Option<String>>> {
        bench.screenshot.as_ref().unwrap().outcome.clone()
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < until, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn video_sequence_ticks_slowly_and_single_shot_keeps_60ms() {
        let dir = test_dir("tick-interval");
        let mut bench = shot_bench("30", &dir);
        // Draws, readbacks and wakes move a video on, not ticks.
        assert_eq!(bench.tick_interval(), Duration::from_millis(60));
        stage_for_video(&mut bench);
        assert!(bench.video_awaits_draw());
        assert_eq!(bench.tick_interval(), Duration::from_millis(60));
        let (index, path) = (
            bench.screenshot.as_ref().unwrap().sequence.index,
            dir.join("x.png"),
        );
        let video = bench.screenshot.as_mut().unwrap().video.as_mut().unwrap();
        video.backlog = Some((index, shot(), path));
        assert_eq!(
            bench.tick_interval(),
            Duration::from_millis(5),
            "a held frame retries fast"
        );
        let bench = shot_bench("1", &dir);
        assert!(bench.screenshot.as_ref().unwrap().video.is_none());
        assert_eq!(bench.tick_interval(), Duration::from_millis(60));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn next_video_frame_wakes_when_due() {
        let dir = test_dir("video-wake");
        let mut bench = shot_bench("3", &dir);
        let now = Instant::now();
        let sequence = &mut bench.screenshot.as_mut().unwrap().sequence;
        sequence.requested_at(now);
        sequence.requested_at(now);
        let next = sequence.next.unwrap();
        assert!(next > Instant::now());
        let _ = bench.screenshot_tick();
        let request = bench.screenshot.as_ref().unwrap();
        assert!(!request.staged, "not due yet");
        assert_eq!(request.video.as_ref().unwrap().wake, Some(next));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn video_request_waits_for_a_draw_after_staging() {
        let dir = test_dir("video-draw");
        let mut bench = shot_bench("3", &dir);
        stage_for_video(&mut bench);
        let _ = bench.screenshot_tick();
        assert!(
            !bench.screenshot.as_ref().unwrap().requested,
            "not drawn yet"
        );
        assert!(bench.shot_staged(), "still staged, not staged again");
        bench.draws.drawn();
        let _ = bench.screenshot_tick();
        assert!(bench.screenshot.as_ref().unwrap().requested);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn single_shot_still_requests_one_tick_after_staging() {
        let dir = test_dir("single-shot");
        let mut bench = shot_bench("1", &dir);
        bench.screenshot.as_mut().unwrap().staged = true;
        let _ = bench.screenshot_tick();
        assert!(
            bench.screenshot.as_ref().unwrap().requested,
            "no draw needed"
        );
        assert!(!dir.join("frame-timing.jsonl").exists(), "no sidecar");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sequence_unstages_on_readback_and_queues_the_write() {
        let dir = test_dir("video-readback");
        let mut bench = shot_bench("3", &dir);
        stage_for_video(&mut bench);
        bench.screenshot.as_mut().unwrap().requested = true;
        let _ = bench.update(Message::Screenshot(shot()));
        assert!(!bench.shot_staged());
        let request = bench.screenshot.as_ref().unwrap();
        assert!(!request.requested);
        assert_eq!(request.sequence.index, 1);
        wait_for(|| {
            let _ = bench.screenshot_tick();
            bench
                .screenshot
                .as_ref()
                .unwrap()
                .video
                .as_ref()
                .unwrap()
                .written()
                == 1
        });
        let png = std::fs::read(dir.join("frame-0000.png")).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let timing = std::fs::read_to_string(dir.join("frame-timing.jsonl")).unwrap();
        assert_eq!(timing.lines().count(), 1);
        let line: serde_json::Value = serde_json::from_str(timing.lines().next().unwrap()).unwrap();
        assert_eq!(line["index"], 0);
        assert_eq!(line["file"], "frame-0000.png");
        assert_eq!(line["preview_frame"], 7);
        assert_eq!(line["camera_frames"]["sim:0"], 10);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn last_video_frame_drains_then_finishes_cleanly() {
        let dir = test_dir("video-drain");
        let mut bench = shot_bench("2", &dir);
        let outcome = outcome(&bench);
        for _ in 0..2 {
            stage_for_video(&mut bench);
            bench.screenshot.as_mut().unwrap().requested = true;
            let _ = bench.update(Message::Screenshot(shot()));
        }
        assert!(
            bench
                .screenshot
                .as_ref()
                .unwrap()
                .video
                .as_ref()
                .unwrap()
                .draining
        );
        wait_for(|| {
            let _ = bench.screenshot_tick();
            bench.screenshot.is_none()
        });
        assert_eq!(*outcome.lock().unwrap(), None);
        assert!(dir.join("frame-0001.png").is_file());
        let timing = std::fs::read_to_string(dir.join("frame-timing.jsonl")).unwrap();
        assert_eq!(timing.lines().count(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn full_writer_queue_holds_the_next_request() {
        let dir = test_dir("video-backlog");
        let mut bench = shot_bench("10", &dir);
        // No writers: the queue fills and stays full.
        std::fs::remove_file(dir.join("frame-timing.jsonl")).unwrap();
        bench.screenshot.as_mut().unwrap().video =
            Some(VideoSink::with_writers(&dir.join("frame.png"), 0).unwrap());
        for index in 0..4 {
            stage_for_video(&mut bench);
            bench.screenshot.as_mut().unwrap().requested = true;
            let _ = bench.update(Message::Screenshot(shot()));
            let request = bench.screenshot.as_ref().unwrap();
            if index < 3 {
                assert!(!request.staged, "frame {index} queued");
            }
        }
        let held = |bench: &Workbench| {
            let request = bench.screenshot.as_ref().unwrap();
            request.staged
                && request.requested
                && request.video.as_ref().unwrap().backlog.is_some()
                && request.sequence.index == 3
        };
        assert!(held(&bench), "the fourth frame waits as the backlog");
        // Each tick retries it; the full queue keeps it waiting.
        let _ = bench.tick();
        assert!(held(&bench));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn timing_line_has_relative_file_and_staged_ms() {
        let text = video::line(
            4,
            "frame-0004.png",
            133.3336,
            150.0,
            None,
            &[("sim:0".into(), 12)],
            true,
        );
        let line: serde_json::Value = serde_json::from_str(&text).unwrap();
        for key in [
            "index",
            "file",
            "staged_ms",
            "read_ms",
            "preview_frame",
            "camera_frames",
            "held_by_pending",
        ] {
            assert!(line.get(key).is_some(), "{key}");
        }
        let file = line["file"].as_str().unwrap();
        assert!(!file.contains('/') && !file.contains('\\'));
        assert_eq!(line["staged_ms"], 133.334);
        assert_eq!(line["held_by_pending"], 1);
        assert!(line["preview_frame"].is_null());
    }

    #[test]
    fn writer_error_finishes_with_save_message() {
        let dir = test_dir("video-error");
        let mut bench = shot_bench("3", &dir);
        let outcome = outcome(&bench);
        // A frame under a regular file cannot be written.
        std::fs::write(dir.join("blocker"), b"").unwrap();
        bench.screenshot.as_mut().unwrap().path = dir.join("blocker/frame.png");
        stage_for_video(&mut bench);
        bench.screenshot.as_mut().unwrap().requested = true;
        let _ = bench.update(Message::Screenshot(shot()));
        wait_for(|| {
            let _ = bench.screenshot_tick();
            bench.screenshot.is_none()
        });
        let error = outcome.lock().unwrap().clone().unwrap();
        assert!(error.starts_with("Save renderer screenshot:"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn video_staging_ignores_pending_jobs_after_the_first_frame() {
        let dir = test_dir("video-pending");
        let mut bench = shot_bench("3", &dir);
        bench.screenshot.as_mut().unwrap().streams_started =
            Some(Instant::now() - Duration::from_secs(2));
        // A pending command of the workbench's own, not the scene's.
        bench.hold(Job::Discover, None);
        drop(bench.scene.held.pop());
        let _ = bench.screenshot_tick();
        assert!(!bench.shot_staged(), "the first frame waits for it");
        bench
            .screenshot
            .as_mut()
            .unwrap()
            .video
            .as_mut()
            .unwrap()
            .first_staged = Some(Instant::now());
        let _ = bench.screenshot_tick();
        assert!(bench.shot_staged(), "later frames do not");
        let video = bench.screenshot.as_ref().unwrap().video.as_ref().unwrap();
        assert!(video.held_by_pending);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn later_video_frames_follow_the_camera() {
        let dir = test_dir("video-camera");
        let mut bench = shot_bench("5", &dir);
        let interval = bench.screenshot.as_ref().unwrap().sequence.interval;
        let staged = |bench: &mut Workbench, ago: Duration, preview: u64| {
            let request = bench.screenshot.as_mut().unwrap();
            request.staged = false;
            request.streams_started = Some(Instant::now() - Duration::from_secs(2));
            let video = request.video.as_mut().unwrap();
            video.first_staged = Some(Instant::now() - Duration::from_secs(1));
            video.staged_at = Some(Instant::now() - ago);
            video.preview_frame = Some(preview);
        };
        bench.frame_id = Some(7);
        staged(&mut bench, interval, 7);
        assert!(bench.video_awaits_camera());
        let _ = bench.screenshot_tick();
        assert!(!bench.shot_staged(), "waits for a new camera frame");
        let video = bench.screenshot.as_ref().unwrap().video.as_ref().unwrap();
        let ends = video.staged_at.unwrap() + interval * 3 / 2;
        assert_eq!(video.wake, Some(ends), "wakes when the wait ends");
        bench.frame_id = Some(8);
        let _ = bench.screenshot_tick();
        assert!(bench.shot_staged(), "a new camera frame stages it");
        // Too soon after the last frame, even a new camera frame waits.
        staged(&mut bench, interval / 2, 7);
        let _ = bench.screenshot_tick();
        assert!(!bench.shot_staged(), "at most about one frame an interval");
        // Without a new camera frame, the end of the wait stages it.
        staged(&mut bench, interval * 2, 8);
        let _ = bench.screenshot_tick();
        assert!(
            bench.shot_staged(),
            "stops waiting 1.5 intervals after the last"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn bench() -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.snapshot.cameras = vec![streaming_camera(10, 0, 30.0)];
        bench.snapshot.active_camera = Some("sim:0".into());
        bench
    }

    #[test]
    fn late_words_apply_once_cameras_stream() {
        let mut bench = bench();
        bench.apply_scene(&["saved", "copied", "stalled"]);
        assert!(!bench.just_saved(None), "not before staging");
        bench.observe_liveness();
        bench.stage_scene();
        assert!(bench.just_saved(Some("sim:0")));
        assert!(bench.just_copied(&bench.session_command()));
        assert!(bench.stalled("sim:0").is_some());
        bench.now += Duration::from_millis(300);
        bench.snapshot.cameras = vec![streaming_camera(20, 0, 30.0)];
        bench.observe_liveness();
        bench.frame_seen("sim:0");
        assert!(bench.stalled("sim:0").is_some(), "frames do not revive it");
    }

    #[test]
    fn failure_and_state_words_set_up_what_they_name() {
        let mut bench = bench();
        bench.apply_scene(&["connect-failed", "warning", "forwarding", "starting"]);
        assert!(bench.side.connect_failures.contains_key("192.168.10.50"));
        assert_eq!(bench.address, "192.168.10.50");
        assert_eq!(bench.side.discovery_issues.len(), 2);
        assert_eq!(
            bench.notice.as_ref().map(|n| n.level),
            Some(Level::Error),
            "the error stands over the later warning"
        );
        bench.snapshot.forwarding = None;
        bench.scene.patch(&mut bench.snapshot);
        assert!(bench.snapshot.forwarding.is_some());
        assert!(bench.snapshot.cameras[0].forwarding.is_some());

        bench.apply_scene(&["write-error"]);
        bench.observe_liveness();
        bench.stage_scene();
        assert_eq!(bench.stream_pending("sim:0"), Some(false));
        assert!(!bench.snapshot.cameras[0].streaming && !bench.snapshot.streaming);
        assert!(bench.inspect.write_errors.contains_key("ExposureTime"));
        assert_eq!(
            bench.notice.as_ref().map(|n| n.text.as_str()),
            Some("Could not set Exposure Time: 5 is below the minimum of 10")
        );
    }

    #[test]
    fn disk_image_scene_warns_and_marks_the_agent_commands() {
        let mut bench = bench();
        bench.apply_scene(&["from-disk-image", "agents"]);
        let notice = bench.notice.clone().expect("a notice");
        assert_eq!(notice.level, Level::Warning);
        assert_eq!(notice.text, "Capturefab is running from the disk image");
        assert_eq!(
            bench.assistant.commands.install,
            Some(crate::volumes::InstallLocation::DiskImage)
        );
        assert!(bench.sheets.agents);
    }

    #[test]
    fn scene_notices_and_pending_commands_stay() {
        let mut bench = bench();
        bench.apply_scene(&["error", "searching", "pending"]);
        let notice = bench.notice.clone().expect("a notice");
        assert_eq!(notice.level, Level::Error);
        assert_eq!(notice.text, "Capture failed: the camera stopped responding");
        bench.set_notice("Stream started", None, Level::Done);
        bench.now += motion::NOTICE_LIFE * 2;
        bench.age_notice();
        assert_eq!(
            bench.notice.as_ref().map(|n| n.text.as_str()),
            Some(notice.text.as_str())
        );
        bench.stage_scene();
        bench.poll();
        assert!(bench.pending(Job::Discover));
        assert_eq!(bench.pending.len(), bench.scene.held.len());
        assert!(bench.pending_for(Job::Stop, "sim:0"));
    }
}
