//! The camera picker's state and the welcome screen's footer: discovery,
//! connect failures and the address field.
use super::*;
use iced::widget::column;
use motion::Flashes;
use std::collections::HashSet;

/// How many failed connect targets the picker remembers.
const FAILURES_KEPT: usize = 8;

/// The welcome footer's button that copies the udev rule a refused USB3
/// camera needs, and what it explains.
const USB_FIX: &str = "Copy USB access fix";
const USB_FIX_TIP: &str = "Linux needs a udev rule to let Capturefab open this camera. Paste the copied command in a terminal (it uses sudo), unplug and replug the camera, then choose it again.";

/// How long a newly found camera takes to grow onto the welcome screen.
pub(super) const ARRIVE: Duration = Duration::from_millis(220);
/// Between cameras found together, so they arrive one after another.
const STAGGER: Duration = Duration::from_millis(40);
/// Cameras after this many arrive together, so a long list is not slow.
const STAGGERED: usize = 4;
/// The address field's shake when connecting to it fails.
const SHAKE: Duration = Duration::from_millis(280);
/// How far the address field moves at most as it shakes.
const SHAKE_BY: f32 = 5.0;
/// How long the address must rest before the hint says it is not understood,
/// so a half-typed IP address is not called wrong.
const TYPING_PAUSE: Duration = Duration::from_millis(1000);
/// How long the address must rest before the disk is asked whether it is a
/// file, so a path being typed is not looked up at every key.
const FILE_PAUSE: Duration = Duration::from_millis(300);
/// How long a discovery runs before the welcome screen says it is
/// searching, so a quick one does not flash its placeholder.
const SEARCH_SHOWN: Duration = Duration::from_millis(200);
/// Key of the welcome screen's discovery callout in `SideState::arrived`.
pub(super) const ISSUES: &str = "\0issues";

/// Room between the footer's edges and its rows.
const EDGE: f32 = 12.0;
/// Inset of text and glyphs within a row, which headers and hints share, so
/// the footer has one left edge.
pub(super) const INSET: f32 = 10.0;

/// What discovery looks for, in its tooltips.
pub(super) const DISCOVERS: &str =
    "Find webcams, GigE Vision, USB3 Vision, ONVIF and saved hub cameras";
/// What discovery looks for, while it does.
pub(super) const DISCOVERING: &str =
    "Looking for webcams, GigE Vision, USB3 Vision, ONVIF and saved hub cameras…";
/// What the address field takes, in full.
pub(super) const ADDRESSES: &str = "IP address, stream URL (RTSP, SRT, RTMP, HTTP), onvif: address, hub:NAME/CAMERA, video file or sim:replay:PATH";
/// What the address field takes, in the room under it.
const ADDRESS_HINT: &str = "e.g. 192.168.1.20 or rtsp://host/stream";

/// State for the welcome screen.
pub(super) struct SideState {
    /// The last discovery's problems, each in full: its warnings, or the
    /// error it failed with. Replaced whenever a discovery finishes.
    pub(super) discovery_issues: Vec<String>,
    /// Connect targets (IDs or addresses) whose last attempt failed, with
    /// the whole error and when. Kept by `failed()`; a retry, a success,
    /// editing the address or the camera connecting any other way clears one.
    pub(super) connect_failures: HashMap<String, (String, Instant)>,
    /// Cameras discovery newly found, growing into the list and onto the
    /// welcome screen, by ID; `ISSUES` for the welcome's discovery callout.
    pub(super) arrived: Flashes<String>,
    /// What discovery had found at the last tick, to tell what is new.
    found: HashSet<String>,
    /// Whether the last tick had discovery issues, to tell when they appear.
    had_issues: bool,
    /// Whether any discovery has finished, so "not found" can be said.
    searched: bool,
    /// The address field shaking "no" after a connect to it failed, by target.
    shakes: Flashes<String>,
    /// The address the hint was read from, and what it will try, if anything.
    hint: (String, Option<String>),
    /// When the address last changed.
    typed_at: Option<Instant>,
    /// The disk answering whether the address, as asked, is a file.
    file_probe: Option<(String, Receiver<bool>)>,
    /// The disk's last answer, and the address it was about.
    file_answer: Option<(String, bool)>,
    /// Resolving whether whitespace around submitted input belongs to a
    /// real file name. Kept separate from the hint's trimmed-path probe.
    address_connect: Option<AddressConnect>,
    /// Whether a camera was connected at the last tick.
    had_camera: bool,
    /// The stage dimming from the welcome screen's surface to its own as the
    /// first camera connects.
    pub(super) stage_in: Motion,
    /// The welcome screen lists every camera found, not just its first
    /// few; until it is left.
    pub(super) welcome_all: bool,
    /// Quiet rediscovery while the welcome screen waits.
    pub(super) rediscover: rediscover::Rediscover,
}

struct AddressConnect {
    address: String,
    camera: Option<String>,
    answer: Receiver<bool>,
}

impl Default for SideState {
    fn default() -> Self {
        Self {
            discovery_issues: Vec::new(),
            connect_failures: HashMap::new(),
            arrived: Flashes::new(ARRIVE),
            found: HashSet::new(),
            had_issues: false,
            searched: false,
            shakes: Flashes::new(SHAKE),
            hint: (String::new(), None),
            typed_at: None,
            file_probe: None,
            file_answer: None,
            address_connect: None,
            had_camera: false,
            stage_in: Motion::new(1.0, motion::IMAGE, motion::IMAGE_OUT, Kind::Fade),
            welcome_all: false,
            rediscover: Default::default(),
        }
    }
}

impl SideState {
    super::motion::registry! {
        motions: [stage_in],
        flashes: [arrived, shakes],
    }

    /// Keep `error` for `target`, forgetting the oldest beyond `FAILURES_KEPT`.
    pub(super) fn connect_failed(&mut self, target: &str, error: String, now: Instant) {
        self.connect_failures
            .insert(target.to_owned(), (error, now));
        while self.connect_failures.len() > FAILURES_KEPT {
            let Some(oldest) = self
                .connect_failures
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(target, _)| target.clone())
            else {
                break;
            };
            self.connect_failures.remove(&oldest);
        }
    }

    /// Whether the disk said `typed` is a file; `None` until it answers.
    fn is_file(&self, typed: &str) -> Option<bool> {
        self.file_answer
            .as_ref()
            .filter(|(asked, _)| asked == typed)
            .map(|(_, file)| *file)
    }

    /// A submitted address can go straight to the camera worker unless
    /// trimming changes it. Then only the disk can tell whether those edge
    /// spaces are part of a file name, and that check must leave the UI free.
    fn request_address_connect(
        &mut self,
        address: &str,
        camera: Option<&str>,
        ask: impl FnOnce(String) -> Receiver<bool>,
    ) -> Option<String> {
        if address.is_empty() {
            return None;
        }
        if address.trim() == address {
            self.address_connect = None;
            return Some(address.to_owned());
        }
        if self.address_connect.as_ref().is_some_and(|pending| {
            pending.address == address && pending.camera.as_deref() == camera
        }) {
            return None;
        }
        self.address_connect = Some(AddressConnect {
            address: address.to_owned(),
            camera: camera.map(str::to_owned),
            answer: ask(address.to_owned()),
        });
        None
    }

    /// Editing or leaving the address screen retires its request, even if
    /// the filesystem worker answers after the user returns to the same text.
    fn retain_address_connect(&mut self, address: &str, camera: Option<&str>, available: bool) {
        if self.address_connect.as_ref().is_some_and(|pending| {
            !available || pending.address != address || pending.camera.as_deref() != camera
        }) {
            self.address_connect = None;
        }
    }

    fn poll_address_connect(&mut self) -> Result<Option<String>, TryRecvError> {
        let Some(pending) = &self.address_connect else {
            return Ok(None);
        };
        let result = pending.answer.try_recv();
        match result {
            Ok(file) => {
                let pending = self.address_connect.take().expect("just checked");
                let address = if file {
                    pending.address
                } else {
                    pending.address.trim().to_owned()
                };
                Ok((!address.is_empty()).then_some(address))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(error) => {
                self.address_connect = None;
                Err(error)
            }
        }
    }

    /// Take the disk's answer on whether an address is a file, if it came,
    /// and ask about `typed` once it has rested for `FILE_PAUSE`, nothing
    /// else reads it and no other answer is awaited; `ask` looks off the UI
    /// thread, since a network path can take seconds. Returns whether an
    /// answer about `typed` came. From `tick_side`.
    fn poll_file(
        &mut self,
        typed: &str,
        now: Instant,
        ask: impl FnOnce(String) -> Receiver<bool>,
    ) -> bool {
        let mut answered = false;
        if let Some((asked, answer)) = &self.file_probe {
            match answer.try_recv() {
                Ok(file) => {
                    answered = asked == typed;
                    self.file_answer = Some((asked.clone(), file));
                    self.file_probe = None;
                }
                Err(TryRecvError::Disconnected) => self.file_probe = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let rested = self
            .typed_at
            .is_some_and(|at| now.saturating_duration_since(at) >= FILE_PAUSE);
        let unread = self.hint.0.trim() == typed && self.hint.1.is_none();
        if rested
            && unread
            && !typed.is_empty()
            && self.file_probe.is_none()
            && self.is_file(typed).is_none()
        {
            self.file_probe = Some((typed.to_owned(), ask(typed.to_owned())));
        }
        answered
    }

    /// How far a camera that is arriving has come, from 0 to 1; 1 for any
    /// other. Free while nothing arrives.
    pub(super) fn arrival(&self, id: &str, now: Instant) -> f32 {
        1.0 - self.arrived.level(id, now)
    }

    /// How far the address field sits from its place while it shakes for
    /// `target`: a few quick swings that die away.
    fn shake(&self, target: &str, now: Instant) -> f32 {
        shake_offset(self.shakes.level(target, now))
    }
}

/// The address field's offset when its shake has `level` left (1 → 0): its
/// swings shrink with the time left, so it settles at rest.
fn shake_offset(level: f32) -> f32 {
    if level <= 0.0 {
        return 0.0;
    }
    // `Flashes` eases its level as (1 - progress)³.
    let left = level.cbrt();
    SHAKE_BY * left * left * ((1.0 - left) * 3.0 * std::f32::consts::TAU).sin()
}

/// Ask the disk whether `path` is a file, on a thread of its own.
fn probe_file(path: String) -> Receiver<bool> {
    let (sender, receiver) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("capturefab-address".into())
        .spawn(move || {
            let _ = sender.send(std::path::Path::new(&path).is_file());
        });
    receiver
}

/// The welcome screen's hooks into the shared update cycle.
impl Workbench {
    /// Point the welcome screen's motions at what they show; from
    /// `sync_animations`. Also reads a changed address, here rather than in
    /// the view; whether it is a file is asked on the tick (`poll_file`).
    pub(super) fn sync_side(&mut self) {
        self.retain_address_connect();
        if self.side.hint.0 != self.address {
            let typed = self.address.trim();
            let reading = read_address(typed, &self.snapshot.devices, self.side.is_file(typed));
            self.side.hint = (self.address.clone(), reading);
            self.side.typed_at = Some(self.now);
        }
    }

    fn retain_address_connect(&mut self) {
        let available =
            self.closing.is_none() && !self.image_mode && !self.settings_open && !self.sheet_open();
        self.side.retain_address_connect(
            &self.address,
            self.snapshot.active_camera.as_deref(),
            available,
        );
    }

    /// Return in the picker never reads the filesystem on the event loop.
    pub(super) fn connect_address(&mut self) {
        if self.connecting(&self.address) || self.connecting(self.address.trim()) {
            return;
        }
        if let Some(address) = self.side.request_address_connect(
            &self.address,
            self.snapshot.active_camera.as_deref(),
            probe_file,
        ) {
            self.connect(address);
        }
    }

    /// Focus the welcome screen's address field, from its "Enter an
    /// address" and its shortcut.
    pub(super) fn enter_address(&mut self) -> Task<Message> {
        // From the quick guide's first step: the field it leads to is
        // under the guide.
        if self.help_open {
            self.show_help(false);
        }
        self.image_mode = false;
        focus_address()
    }

    /// The welcome screen's bookkeeping on the slow tick,
    /// after the snapshot refresh; from `tick()`.
    pub(super) fn tick_side(&mut self) {
        self.retain_address_connect();
        match self.side.poll_address_connect() {
            Ok(Some(address)) if !self.connecting(&address) => self.connect(address),
            Err(_) => self.set_notice(
                "Could not check the camera file path",
                Some("The filesystem worker stopped. Submit the address again to retry.".into()),
                Level::Error,
            ),
            _ => {}
        }
        let now = self.now;
        let typed = self.address.trim();
        // Newly found cameras arrive one after another, in list order. One
        // a quiet round held is still listed, so it never arrives again.
        let listed: Vec<String> = self
            .listed_devices()
            .map(|device| device.id.clone())
            .collect();
        let side = &mut self.side;
        let mut fresh = 0;
        for id in &listed {
            if !side.found.contains(id) {
                let delay = STAGGER * fresh.min(STAGGERED) as u32;
                side.arrived.hit(id.clone(), now + delay);
                fresh += 1;
            }
        }
        if fresh > 0 || side.found.len() != listed.len() {
            side.found = listed.into_iter().collect();
            // A serial typed before discovery found it reads as known now.
            if !typed.is_empty() {
                side.hint.1 = read_address(typed, &self.snapshot.devices, side.is_file(typed));
            }
        }
        if side.poll_file(typed, now, probe_file) {
            side.hint.1 = read_address(typed, &self.snapshot.devices, side.is_file(typed));
        }
        // A camera connected since, however it was, retires its failures.
        let cameras = &self.snapshot.cameras;
        side.connect_failures.retain(|target, _| {
            !cameras.iter().any(|camera| {
                camera.info.id == *target
                    || camera.info.serial == *target
                    || camera.info.address.as_deref() == Some(target.as_str())
            })
        });
        let issues = !side.discovery_issues.is_empty();
        if issues && !side.had_issues {
            side.arrived.hit(ISSUES.to_owned(), now);
        }
        side.had_issues = issues;
        // Leaving the welcome screen folds its list of cameras again.
        if self.snapshot.connected.is_some() {
            side.welcome_all = false;
        }
        // The stage dims in as the first camera connects.
        let connected = self.observed_camera.is_some();
        if connected && !side.had_camera {
            side.stage_in.replay(0.0, 1.0, now);
        }
        side.had_camera = connected;
    }

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`. A failed connect to
    /// the address still in the field shakes it.
    pub(super) fn result_side(
        &mut self,
        pending: &Pending,
        result: &anyhow::Result<serde_json::Value>,
    ) {
        if pending.job == Job::Discover {
            self.side.searched = true;
            self.result_rediscover(pending, result);
        }
        if let (Err(_), Some(target)) = (result, &pending.target)
            && self.address.trim() == target
            && !motion::reduce_motion()
        {
            self.side.shakes.hit(target.clone(), self.now);
        }
    }

    /// Take a screenshot scene word for the welcome screen: `late` is
    /// false while the scene is set up and true once its cameras stream.
    /// Returns whether the word was taken; see `apply_scene`. Words, for
    /// `welcome` scenes:
    /// - `side-connecting`: connecting to the simulator and to a stream URL
    ///   never finishes
    /// - `side-card-failed`: connecting to the simulator failed
    /// - `side-recent`: two recent cameras, a GigE one and a stream
    /// - `side-address`: a GigE address typed in the address field
    /// - `side-unknown`: something the address field does not understand
    pub(super) fn scene_side(&mut self, word: &str, late: bool) -> bool {
        if late {
            return false;
        }
        match word {
            "side-connecting" => {
                for target in [
                    "sim:0",
                    "rtsp://operator:secret@192.168.1.20:8554/line-1?token=abc",
                ] {
                    self.scene_hold(Job::Connect).target = Some(target.into());
                }
            }
            "side-card-failed" => self.side.connect_failed(
                "sim:0",
                "the camera did not answer within 5 s".into(),
                self.now,
            ),
            "side-recent" => {
                self.recent = vec![
                    Recent {
                        target: "192.168.1.40".into(),
                        label: "acA1920-40gc".into(),
                        detail: "GigE · 192.168.1.40".into(),
                        transport: crate::types::Transport::GigE,
                    },
                    Recent {
                        target: "rtsp://operator:secret@10.0.0.7:8554/dock".into(),
                        label: "RTSP input".into(),
                        detail: format!(
                            "Media · {}",
                            redact_address("rtsp://operator:secret@10.0.0.7:8554/dock")
                        ),
                        transport: crate::types::Transport::Media,
                    },
                ]
            }
            "side-address" => self.address = "192.168.1.20".into(),
            "side-unknown" => self.address = "line-7 camera".into(),
            _ => return false,
        }
        true
    }
}

/// Just the host (and port) of `address`, as `short_address` gives it.
fn host(address: &str) -> String {
    let short = short_address(address);
    match short.split_once('/') {
        Some((host, _)) => host.to_owned(),
        None => short,
    }
}

/// Where a camera is, identifier first: its address or serial (see
/// `identity`), then its transport. Two cameras of one model differ here.
pub(super) fn place(camera: &CameraInfo) -> String {
    // A webcam or screen of this computer reads plainly, not as its
    // backend's selector ("avfoundation:… · Media").
    if host_device(camera).is_some()
        && let Some(place) = host_place(&camera.id, &camera.model)
    {
        return place;
    }
    // A hub camera's product says more than its transport ("Media").
    if crate::hubs::is_source(&camera.id) {
        return format!("{} · {}", identity(camera), camera.vendor);
    }
    format!("{} · {}", identity(camera), camera.transport)
}

/// A recent camera's detail, identifier first like `place`. Recents keep it
/// as "transport · address or serial".
pub(super) fn recent_place(recent: &Recent) -> String {
    if recent.transport == crate::types::Transport::Media
        && let Some(place) = host_place(&recent.target, &recent.label)
    {
        return place;
    }
    match recent.detail.split_once(" · ") {
        Some((transport, place)) if place.contains("://") || crate::hubs::is_source(place) => {
            format!("{} · {transport}", short_address(place))
        }
        Some((transport, place)) => format!("{place} · {transport}"),
        None => recent.detail.clone(),
    }
}

/// What connecting to `input` will try, following `Camera::open`'s order;
/// `None` when it is none of the things it accepts. `file` is the disk's
/// answer on whether `input` is a file, if it came. Never shows credentials.
fn read_address(input: &str, devices: &[CameraInfo], file: Option<bool>) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if crate::onvif::is_source(input) {
        return Some(format!("Will try ONVIF at {}", host(input)));
    }
    // Saved hubs are not read here: this runs on every edit, on the UI
    // thread. A missing hub is reported when connecting.
    if crate::hubs::is_source(input) {
        return Some(match crate::hubs::Selector::parse(input) {
            Ok(selector) => match devices.iter().find(|device| device.id == selector.id()) {
                Some(device) => format!(
                    "Will open {} from {} hub {}",
                    device.model, device.vendor, selector.hub
                ),
                None => format!("Will try {} on hub {}", selector.camera, selector.hub),
            },
            Err(_) => "Hub cameras look like hub:NAME/CAMERA".into(),
        });
    }
    if crate::transport::simulator::replay_source(input).is_some() {
        return Some("Will loop the test file or folder as a simulated camera".into());
    }
    if input.starts_with("sim:") {
        return Some("Will try a simulated camera".into());
    }
    if crate::scanner::is_source(input) {
        return Some("Will try a document scanner".into());
    }
    if crate::media::is_url_source(input) {
        return Some(match input.split_once(':') {
            Some((scheme, rest)) if rest.starts_with("//") => {
                let scheme = scheme.to_ascii_lowercase();
                match scheme.as_str() {
                    "file" => "Will try the video file".into(),
                    _ => format!(
                        "Will try {} at {}",
                        scheme.to_ascii_uppercase(),
                        short_address(input)
                    ),
                }
            }
            Some((scheme, _))
                if matches!(
                    scheme.to_ascii_lowercase().as_str(),
                    "avfoundation" | "v4l2" | "dshow"
                ) =>
            {
                "Will try a webcam".into()
            }
            _ => "Will try the video file".into(),
        });
    }
    if file == Some(true) {
        return Some("Will try the video file".into());
    }
    if let Ok(ip) = input
        .trim_start_matches("gige:")
        .parse::<std::net::Ipv4Addr>()
    {
        return Some(format!("Will try GigE Vision at {ip}"));
    }
    devices
        .iter()
        .find(|device| device.id == input || device.serial == input)
        .map(|device| format!("Will try {} · S/N {}", device.model, device.serial))
}

/// Whether connecting to `target` tries GigE Vision, as `read_address`
/// reads it; a found GigE camera's ID or serial counts too.
pub(super) fn reads_as_gige(target: &str, devices: &[CameraInfo]) -> bool {
    target.starts_with("gige:")
        || read_address(target, devices, None)
            .is_some_and(|reading| reading.starts_with("Will try GigE Vision"))
        || devices.iter().any(|device| {
            device.transport == crate::types::Transport::GigE
                && (device.id == target || device.serial == target)
        })
}

impl Workbench {
    /// Whether the welcome screen should say discovery is searching: from
    /// the start until the first one finishes, then once one has run for
    /// `SEARCH_SHOWN`. Before that its slot keeps what it said. Quiet
    /// rediscovery never says so.
    pub(super) fn searching_shown(&self) -> bool {
        let running = self
            .pending
            .iter()
            .filter(|pending| pending.job == Job::Discover && !pending.background)
            .map(|pending| self.now.saturating_duration_since(pending.at))
            .max();
        !self.side.searched || running.is_some_and(|running| running >= SEARCH_SHOWN)
    }

    /// Whether the address hint may say the typed text is not understood:
    /// it rested for `TYPING_PAUSE`, nothing reads it, and the disk said it
    /// is no file.
    fn address_unknown(&self) -> bool {
        let typed = self.address.trim();
        !typed.is_empty()
            && self.side.hint.1.is_none()
            && self.side.is_file(typed) == Some(false)
            && self
                .side
                .typed_at
                .is_some_and(|at| self.now.saturating_duration_since(at) >= TYPING_PAUSE)
    }

    /// Whether a connect to `camera`, by ID, serial or address, is on its way.
    pub(super) fn connecting_to(&self, camera: &CameraInfo) -> bool {
        self.connecting(&camera.id)
            || (!camera.serial.is_empty() && self.connecting(&camera.serial))
            || camera
                .address
                .as_deref()
                .is_some_and(|address| self.connecting(address))
    }

    /// Whether a webcam's last connect failed because the system refused
    /// access, so the footer offers the system's camera settings.
    pub(super) fn camera_permission_relevant(&self) -> bool {
        self.side
            .connect_failures
            .values()
            .any(|(error, _)| crate::media::is_camera_access_denied(error))
    }

    /// The fix the latest connect failure carries for a USB3 Vision camera
    /// this user may not open (a Linux udev rule; see
    /// `doctor::usb_access_error`), so the footer offers to copy it.
    pub(super) fn usb_access_fix(&self) -> Option<&str> {
        self.side
            .connect_failures
            .values()
            .filter(|(error, _)| error.contains(crate::doctor::USB_ACCESS_DENIED))
            .max_by_key(|(_, at)| *at)
            .and_then(|(error, _)| crate::doctor::fix_line(error))
    }

    /// Under the welcome screen's cards: the network check once GigE is in
    /// play, the address field and what it will try, and
    /// the window's settings.
    pub(super) fn picker_footer<'a>(
        &'a self,
        p: &'static Palette,
        search: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let typed = self.address.trim();
        let failure = self.side.connect_failures.get(typed);
        let shake = self.side.shake(typed, self.now);
        let field = text_input("Camera IP, stream URL or file", &self.address)
            .name("Camera address")
            .description("Enter a camera IP address, stream URL or device identifier, then press Return to connect.")
            .invalid(failure.map(|(error, _)| format!("Could not connect: {error}")))
            .id("connect-address")
            .icon(icon::input_icon(Icon::Plus))
            .on_input(Message::Address)
            .on_submit(Message::ConnectAddress)
            .size(style::BODY)
            .padding([7, 10])
            .style(if failure.is_some() {
                style::input_invalid
            } else {
                style::input
            });
        // One line, whatever it says, so the field above never moves.
        let hint: Element<'_, Message> = if self.side.address_connect.is_some() {
            one_line(
                "Checking file path…",
                style::CAPTION,
                style::SANS,
                p.secondary,
            )
            .width(Fill)
            .into()
        } else if let Some((error, _)) = failure {
            let ink = Level::Error.color(p);
            tip_above(
                row![
                    Level::Error.mark(11.0, p),
                    one_line(
                        capitalize(first_line(error)),
                        style::CAPTION,
                        style::SANS,
                        ink
                    )
                    .width(Fill),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
                format!("Could not connect: {error}"),
            )
        } else {
            let (reading, ink) = match (&self.side.hint, typed.is_empty()) {
                (_, true) => (ADDRESS_HINT, p.secondary),
                ((_, Some(reading)), _) => (reading.as_str(), p.secondary),
                ((_, None), _) if self.address_unknown() => {
                    ("Not an address, URL, file or serial", p.ink(p.warn))
                }
                ((_, None), _) => (ADDRESS_HINT, p.secondary),
            };
            one_line(reading, style::CAPTION, style::SANS, ink)
                .width(Fill)
                .into()
        };
        let command = self.session_command();
        let copied = self.just_copied(&command);
        // The session's name and a copy of the command that drives it, in
        // one control; a copy swaps only the glyph and tooltip, so nothing
        // moves from under the pointer.
        let session = tip_above(
            button(
                row![
                    if copied {
                        icon(Icon::Check, 13.0, p.ink(p.live))
                    } else {
                        icon(Icon::Copy, 13.0, p.secondary)
                    },
                    one_line(
                        format!("session {}", self.session),
                        style::CAPTION,
                        style::MONO,
                        p.secondary,
                    ),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
            )
            .name(format!("Copy command for session {}", self.session))
            .padding([5, 6])
            .style(style::plain)
            .on_press(Message::CopySessionCommand),
            if copied {
                "Copied".to_owned()
            } else {
                Action::CopySessionCommand.hint(&format!("Copy {command}"), Os::CURRENT)
            },
        );
        let settings = row![
            tip_above(
                button(icon(Icon::Gear, 15.0, p.secondary))
                    .name("Settings")
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::Settings(true)),
                Action::Settings.hint("Settings", Os::CURRENT),
            ),
            tip_above(
                button(icon(Icon::Help, 15.0, p.secondary))
                    .name("Keyboard shortcuts and quick guide")
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::Help(true)),
                Action::Help.hint("Keyboard shortcuts and quick guide", Os::CURRENT),
            ),
            // Other ways to work, after the camera's: quiet beside the
            // window's own controls.
            tip_above(
                button(
                    row![
                        icon(Icon::Sparkle, 13.0, p.secondary),
                        text("Ask the assistant")
                            .size(style::SMALL)
                            .color(p.secondary),
                    ]
                    .spacing(5)
                    .align_y(Alignment::Center),
                )
                .name("Ask the assistant")
                .description("Plan or run a capture task using your own provider or local model")
                .padding([5, 6])
                .style(style::plain)
                .on_press(Message::Assistant(assistant::Input::Quick(
                    assistant::quick::Input::Open(true)
                ))),
                Action::QuickCommand.hint(
                    "Plan or run a capture task using your own provider or local model",
                    Os::CURRENT
                ),
            ),
            tip_above(
                button(
                    text("Connect an agent")
                        .size(style::SMALL)
                        .color(p.secondary)
                )
                .name("Connect an agent")
                .description("Let an external harness control this same workbench through the CLI")
                .padding([5, 6])
                .style(style::plain)
                .on_press(Message::AgentSetup(true)),
                "Let an external harness control this same workbench through the CLI",
            ),
            space::horizontal(),
            session,
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        // Its report only once there is one, so it is never shown disabled.
        // The slot stays one container either way, so the widgets after it
        // keep their state as it comes and goes.
        let mut network = row![].spacing(8).align_y(Alignment::Center);
        // A webcam the system refused: one click to where access is given.
        if self.camera_permission_relevant()
            && let Some(label) = reveal::camera_privacy_label()
        {
            network = network.push(tip(
                button(
                    row![
                        icon(Icon::Lock, 14.0, p.secondary),
                        text(label).size(style::SMALL),
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                )
                .name(label)
                .padding([7, 10])
                .style(style::secondary)
                .on_press(Message::OpenCameraPrivacy),
                "Allow camera access for Capturefab in the system privacy settings, then choose the camera again",
            ));
        }
        // A USB3 camera Linux refused: its udev rule, to copy and run in a
        // terminal. It uses sudo, so it is never run from here.
        if let Some(fix) = self.usb_access_fix() {
            let copied = self.just_copied(fix);
            network = network.push(tip(
                button(
                    row![
                        if copied {
                            icon(Icon::Check, 14.0, p.ink(p.live))
                        } else {
                            icon(Icon::Lock, 14.0, p.secondary)
                        },
                        text(if copied {
                            "Copied: run it, then replug"
                        } else {
                            USB_FIX
                        })
                        .size(style::SMALL),
                    ]
                    .spacing(7)
                    .align_y(Alignment::Center),
                )
                .name(USB_FIX)
                .padding([7, 10])
                .style(style::secondary)
                .on_press(Message::Copy(fix.to_owned())),
                USB_FIX_TIP,
            ));
        }
        if self.network_check_relevant() {
            network = network
                .push(self.network_scan_button(p))
                .push(space::horizontal());
            if self.snapshot.network_scan.is_some() {
                network = network.push(
                    button(text("Report").size(style::SMALL))
                        .name("Open network scan results")
                        .padding([4, 5])
                        .style(style::plain)
                        .on_press(Message::NetworkReport(true)),
                );
            }
        }
        column![
            // Shifted both ways at once, so it moves without resizing.
            container(
                row![
                    tip(field, Action::ConnectAddress.hint(ADDRESSES, Os::CURRENT)),
                    search
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            )
            .padding(iced::Padding {
                left: EDGE + shake,
                right: EDGE - shake,
                ..iced::Padding::ZERO
            }),
            // Simulated cameras are a setting; the empty state offers one.
            container(hint).padding([0.0, EDGE + INSET]),
            container(network).padding([0.0, EDGE]),
            container(rule::horizontal(1).style(style::line)).padding([0.0, EDGE]),
            container(settings).padding(iced::Padding {
                left: EDGE + 4.0,
                right: EDGE,
                ..iced::Padding::ZERO
            }),
        ]
        .spacing(8)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Transport;
    use serde_json::json;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    fn device(id: &str, serial: &str) -> CameraInfo {
        CameraInfo {
            id: id.into(),
            transport: Transport::Simulator,
            vendor: "Capturefab".into(),
            model: "Pattern camera".into(),
            serial: serial.into(),
            address: None,
            profile: Default::default(),
        }
    }

    #[test]
    fn addresses_read_in_the_order_connecting_tries_them() {
        let devices = [device("sim:0", "SIM0")];
        // With the disk's answer, as `poll_file` brings it.
        let read = |input: &str| {
            read_address(input, &devices, Some(std::path::Path::new(input).is_file()))
        };
        let cases = [
            ("", None),
            (
                "onvif://admin:secret@192.168.1.30/onvif/device_service",
                Some("Will try ONVIF at 192.168.1.30"),
            ),
            (
                "onvif:http://camera.local/onvif/device_service",
                Some("Will try ONVIF at camera.local"),
            ),
            ("sim:3", Some("Will try a simulated camera")),
            (
                "rtsp://operator:secret@cam.local:554/line-1?token=abc",
                Some("Will try RTSP at cam.local:554/line-1"),
            ),
            ("srt://10.0.0.5:9000", Some("Will try SRT at 10.0.0.5:9000")),
            ("file:///videos/line.mkv", Some("Will try the video file")),
            ("avfoundation:0", Some("Will try a webcam")),
            ("scanner:ica:document", Some("Will try a document scanner")),
            // Tests run in the package root.
            ("Cargo.toml", Some("Will try the video file")),
            ("gige:10.0.0.2", Some("Will try GigE Vision at 10.0.0.2")),
            ("192.168.1.20", Some("Will try GigE Vision at 192.168.1.20")),
            ("SIM0", Some("Will try Pattern camera · S/N SIM0")),
            ("sim0", None),
            ("192.168.1", None),
            ("line-7 camera", None),
            (
                "hub:home/camera.porch?stream=snapshot",
                Some("Will try camera.porch on hub home"),
            ),
            ("hub:home", Some("Hub cameras look like hub:NAME/CAMERA")),
        ];
        for (input, expected) in cases {
            assert_eq!(read(input).as_deref(), expected, "{input:?}");
        }
        let mut hub_camera = device("hub:home/camera.front_door", "home/camera.front_door");
        hub_camera.vendor = "Home Assistant".into();
        hub_camera.model = "Front door".into();
        assert_eq!(
            read_address(
                "hub:home/camera.front_door",
                std::slice::from_ref(&hub_camera),
                None
            )
            .as_deref(),
            Some("Will open Front door from Home Assistant hub home")
        );
        hub_camera.address = Some(hub_camera.id.clone());
        assert_eq!(
            place(&hub_camera),
            "home/camera.front_door · Home Assistant"
        );
        // Reading never asks the disk itself.
        assert_eq!(read_address("Cargo.toml", &devices, None), None);
    }

    #[test]
    fn submitting_an_address_without_edge_spaces_never_checks_the_disk_on_the_ui_thread() {
        let mut side = SideState::default();
        for address in [
            "/tmp/a video file.mkv",
            "rtsp://camera.local/stream",
            "192.168.1.20",
            "sim:0",
        ] {
            assert_eq!(
                side.request_address_connect(address, None, |_| unreachable!("no disk access")),
                Some(address.to_owned())
            );
        }
        assert_eq!(
            side.request_address_connect("", None, |_| unreachable!()),
            None
        );
    }

    #[test]
    fn submitted_edge_spaces_wait_for_the_filesystem_and_preserve_real_file_names_once() {
        let mut side = SideState::default();
        for (address, file, expected) in [
            (" /tmp/video file.mkv ", true, Some(" /tmp/video file.mkv ")),
            (
                " rtsp://camera.local/stream ",
                false,
                Some("rtsp://camera.local/stream"),
            ),
            (" ", false, None),
            (" ", true, Some(" ")),
        ] {
            let (sender, receiver) = mpsc::channel();
            assert_eq!(
                side.request_address_connect(address, None, |asked| {
                    assert_eq!(asked, address);
                    receiver
                }),
                None
            );
            // Return and ticks stay responsive while a path is slow. A
            // repeated Return must not add a second filesystem request.
            assert_eq!(side.poll_address_connect().unwrap(), None);
            assert_eq!(
                side.request_address_connect(address, None, |_| unreachable!("already checking")),
                None
            );
            sender.send(file).unwrap();
            assert_eq!(side.poll_address_connect().unwrap().as_deref(), expected);
            assert_eq!(side.poll_address_connect().unwrap(), None, "consumed once");
        }
    }

    #[test]
    fn delayed_path_resolution_is_cancelled_after_edit_selection_settings_or_close() {
        for action in ["edit", "selection", "settings", "image", "close"] {
            let mut app = bench();
            app.address = " /tmp/slow-network-video.mkv ".into();
            let (sender, receiver) = mpsc::channel();
            app.side
                .request_address_connect(&app.address, None, |_| receiver);
            match action {
                "edit" => {
                    let _ = app.update(Message::Address("new address".into()));
                    // Returning to identical text cannot revive the old request.
                    let _ = app.update(Message::Address(" /tmp/slow-network-video.mkv ".into()));
                }
                "selection" => app.snapshot.active_camera = Some("sim:0".into()),
                "settings" => app.settings_open = true,
                "image" => app.image_mode = true,
                "close" => app.closing = Some(CloseTarget::Window),
                _ => unreachable!(),
            }
            app.sync_side();
            assert!(
                app.side.address_connect.is_none(),
                "cancelled after {action}"
            );
            assert!(
                sender.send(true).is_err(),
                "late reply retired after {action}"
            );
            assert_eq!(app.side.poll_address_connect().unwrap(), None);
        }
    }

    #[test]
    fn a_stopped_path_worker_retires_the_request_for_retry() {
        let mut side = SideState::default();
        let (sender, receiver) = mpsc::channel();
        side.request_address_connect(" /tmp/video.mkv ", None, |_| receiver);
        drop(sender);
        assert_eq!(side.poll_address_connect(), Err(TryRecvError::Disconnected));
        assert!(side.address_connect.is_none());
    }

    #[test]
    fn resubmitting_an_address_already_connecting_never_queues_a_second_request() {
        for (address, target) in [
            ("sim:0", "sim:0"),
            (" sim:0 ", "sim:0"),
            (" /tmp/video file.mkv ", " /tmp/video file.mkv "),
        ] {
            let mut app = bench();
            app.address = address.into();
            app.scene_hold(Job::Connect).target = Some(target.into());
            let pending = app.pending.len();
            app.connect_address();
            assert_eq!(app.pending.len(), pending);
            assert!(app.side.address_connect.is_none(), "{address:?}");
        }
    }

    #[test]
    fn the_disk_is_asked_about_files_once_typing_rests() {
        let mut bench = bench();
        let start = bench.now;
        bench.address = "Cargo.toml".into();
        bench.sync_side();
        assert_eq!(bench.side.hint.1, None, "unknown while typing");
        let answer = |file: bool| {
            let (sender, receiver) = mpsc::channel();
            let _ = sender.send(file);
            receiver
        };
        let mut asked = Vec::new();
        let typed = "Cargo.toml";
        bench
            .side
            .poll_file(typed, start + FILE_PAUSE - ms(1), |text| {
                asked.push(text);
                answer(true)
            });
        assert!(asked.is_empty(), "still typing");
        assert!(!bench.side.poll_file(typed, start + FILE_PAUSE, |text| {
            asked.push(text);
            answer(true)
        }));
        assert_eq!(asked, ["Cargo.toml"]);
        // The tick takes the answer into the hint.
        bench.tick_side();
        assert_eq!(bench.side.is_file(typed), Some(true));
        assert!(bench.side.file_probe.is_none());
        assert_eq!(
            bench.side.hint.1.as_deref(),
            Some("Will try the video file")
        );
        // An answer about text edited since reads nothing and asks again.
        bench.address = "Cargo".into();
        bench.sync_side();
        bench
            .side
            .poll_file("Cargo", start + FILE_PAUSE * 2, |_| answer(false));
        bench.address = "Cargo.lock".into();
        bench.sync_side();
        let later = bench.now + FILE_PAUSE;
        assert!(!bench.side.poll_file("Cargo.lock", later, |text| {
            assert_eq!(text, "Cargo.lock");
            answer(false)
        }));
        assert_eq!(bench.side.is_file("Cargo"), Some(false));
        assert!(
            bench
                .side
                .poll_file("Cargo.lock", later, |_| unreachable!())
        );
        // Not a file: the hint says so once typing has rested.
        assert!(!bench.address_unknown(), "rested only for the disk");
        bench.now = later + TYPING_PAUSE;
        assert!(bench.address_unknown());
        // Text something else reads asks nothing.
        bench.address = "192.168.1.20".into();
        bench.sync_side();
        bench
            .side
            .poll_file("192.168.1.20", bench.now + FILE_PAUSE, |_| unreachable!());
    }

    #[test]
    fn short_addresses_keep_where_and_drop_secrets() {
        assert_eq!(
            short_address("rtsp://user:pass@cam.local:554/stream?token=1#x"),
            "cam.local:554/stream"
        );
        assert_eq!(
            short_address("rtsp://[redacted]@cam.local/a/"),
            "cam.local/a"
        );
        assert_eq!(short_address("192.168.1.20"), "192.168.1.20");
        assert_eq!(
            short_address("onvif://admin:p@ss@10.0.0.9/onvif/device_service"),
            "10.0.0.9/onvif/device_service"
        );
        assert_eq!(host("onvif:http://cam/service"), "cam");
    }

    #[test]
    fn places_lead_with_what_tells_cameras_apart() {
        let mut camera = device("sim:0", "SIM0");
        assert_eq!(place(&camera), "S/N SIM0 · Simulator");
        camera.transport = Transport::GigE;
        camera.address = Some("192.168.1.20".into());
        assert_eq!(place(&camera), "192.168.1.20 · GigE");
        let recent = |detail: &str| Recent {
            target: "x".into(),
            label: "Camera".into(),
            detail: detail.into(),
            transport: Transport::Media,
        };
        assert_eq!(
            recent_place(&recent("Media · rtsp://[redacted]@cam.local/line")),
            "cam.local/line · Media"
        );
        assert_eq!(
            recent_place(&recent("Simulator · S/N SIM0")),
            "S/N SIM0 · Simulator"
        );
        assert_eq!(recent_place(&recent("GigE")), "GigE");
    }

    /// Webcams and screens say what they are, not their backend's selector,
    /// unless the selector is what tells two of them apart.
    #[test]
    fn host_inputs_read_as_webcams_and_screens() {
        let host = |id: &str, model: &str| CameraInfo {
            id: id.into(),
            transport: Transport::Media,
            vendor: "macos".into(),
            model: model.into(),
            serial: id.into(),
            address: Some(id.into()),
            profile: Default::default(),
        };
        let cases = [
            (
                "avfoundation:HD Pro Webcam C920",
                "HD Pro Webcam C920",
                "Webcam · this computer",
            ),
            (
                "avfoundation:Capture screen 0",
                "Capture screen 0",
                "Screen · this computer",
            ),
            ("avfoundation:4", "Cam", "#4 · Webcam"),
            ("v4l2:/dev/video0", "UVC Camera", "/dev/video0 · Webcam"),
            (
                "dshow:video=Integrated Webcam",
                "Integrated Webcam",
                "Webcam · this computer",
            ),
        ];
        for (id, model, expected) in cases {
            assert_eq!(place(&host(id, model)), expected, "{id}");
            let recent = Recent {
                target: id.into(),
                label: model.into(),
                detail: format!("Media · {id}"),
                transport: Transport::Media,
            };
            assert_eq!(recent_place(&recent), expected, "recent {id}");
        }
        // Streams and machine-vision cameras are unchanged.
        let stream = host("rtsp://cam.local/line", "RTSP input");
        assert_eq!(place(&stream), "cam.local/line · Media");
        let mut gige = host("gige:10.0.0.5", "acA1920");
        gige.transport = Transport::GigE;
        gige.address = Some("10.0.0.5".into());
        assert_eq!(place(&gige), "10.0.0.5 · GigE");
        assert_eq!(
            host_device(&host("avfoundation:Capture screen 1", "Capture screen 1")),
            Some(HostKind::Screen)
        );
        assert_eq!(host_device(&gige), None);
    }

    #[test]
    fn found_cameras_arrive_once_one_after_another() {
        let mut bench = bench();
        let start = bench.now;
        bench.tick_side();
        assert!(bench.side.arrived.is_empty(), "nothing found yet");
        bench.snapshot.devices = vec![device("sim:0", "SIM0"), device("sim:1", "SIM1")];
        bench.tick_side();
        assert_eq!(bench.side.arrival("sim:0", start), 0.0);
        assert_eq!(
            bench.side.arrival("sim:1", start + STAGGER),
            0.0,
            "staggered"
        );
        let mid = bench.side.arrival("sim:0", start + ARRIVE / 2);
        assert!(mid > 0.5 && mid < 1.0, "eases out: {mid}");
        assert!(bench.animating());
        bench.now = start + ARRIVE + STAGGER;
        assert!(!bench.animating(), "settles");
        assert_eq!(bench.side.arrival("sim:1", bench.now), 1.0);
        // Searching again and finding the same cameras replays nothing.
        bench.tick_side();
        assert!(!bench.animating());
        assert_eq!(bench.side.arrival("sim:0", bench.now), 1.0);
        // Losing one and finding it again does.
        bench.snapshot.devices.pop();
        bench.tick_side();
        bench.snapshot.devices.push(device("sim:1", "SIM1"));
        bench.tick_side();
        assert!(bench.animating());
    }

    #[test]
    fn discovery_issues_arrive_when_they_appear() {
        let mut bench = bench();
        bench.side.discovery_issues = vec!["USB access denied".into()];
        bench.tick_side();
        assert_eq!(bench.side.arrival(ISSUES, bench.now), 0.0);
        bench.now += ARRIVE;
        bench.tick_side();
        assert_eq!(bench.side.arrival(ISSUES, bench.now), 1.0, "only once");
    }

    #[test]
    fn the_stage_dims_in_on_the_first_connect_and_settles() {
        let mut bench = bench();
        let start = bench.now;
        bench.tick_side();
        assert_eq!(bench.side.stage_in.get(start), 1.0, "at rest");
        bench.observed_camera = Some("sim:0".into());
        bench.tick_side();
        assert_eq!(bench.side.stage_in.get(start), 0.0);
        assert!(bench.side.stage_in.animating(start + motion::IMAGE / 2));
        assert_eq!(bench.side.stage_in.get(start + motion::IMAGE), 1.0);
        // Another camera, or the same one again, does not dim it again.
        bench.now = start + motion::IMAGE;
        bench.observed_camera = Some("sim:1".into());
        bench.tick_side();
        assert!(!bench.side.stage_in.animating(bench.now));
    }

    #[test]
    fn a_failed_connect_shakes_only_the_address_it_came_from() {
        let mut bench = bench();
        let start = bench.now;
        bench.address = "192.168.10.50".into();
        let pending = |target: &str| Pending {
            target: Some(target.into()),
            at: start,
            ..Pending::unanswered(Job::Connect)
        };
        let failed: anyhow::Result<serde_json::Value> = Err(anyhow::anyhow!("no answer"));
        bench.settle(&pending("sim:4"), &failed);
        assert_eq!(bench.side.shake("sim:4", start), 0.0, "not in the field");
        bench.settle(&pending("192.168.10.50"), &failed);
        assert!(bench.side.connect_failures.contains_key("192.168.10.50"));
        if motion::reduce_motion() {
            assert!(bench.side.shakes.is_empty(), "no shake under Reduce Motion");
            return;
        }
        assert!(bench.animating());
        let swing = (1..28)
            .map(|step| {
                bench
                    .side
                    .shake("192.168.10.50", start + ms(step * 10))
                    .abs()
            })
            .fold(0.0, f32::max);
        assert!(swing > 1.0 && swing <= SHAKE_BY, "{swing}");
        assert_eq!(bench.side.shake("192.168.10.50", start + SHAKE), 0.0);
        bench.now = start + SHAKE;
        assert!(!bench.animating(), "settles");
        let ok: anyhow::Result<serde_json::Value> = Ok(json!({}));
        bench.settle(&pending("192.168.10.50"), &ok);
        assert!(!bench.side.connect_failures.contains_key("192.168.10.50"));
    }

    #[test]
    fn quick_searches_do_not_flash_the_placeholder() {
        let mut bench = bench();
        let start = bench.now;
        assert!(bench.searching_shown(), "until the first search finishes");
        let discovery = || Pending {
            at: start,
            ..Pending::unanswered(Job::Discover)
        };
        bench.settle(&discovery(), &Ok(json!({"devices": [], "warnings": []})));
        assert!(!bench.searching_shown());
        bench.pending.push(discovery());
        bench.now = start + SEARCH_SHOWN / 2;
        assert!(!bench.searching_shown(), "a quick search keeps the slot");
        bench.now = start + SEARCH_SHOWN;
        assert!(bench.searching_shown());
    }

    #[test]
    fn every_row_and_card_state_builds() {
        let mut bench = bench();
        let mut connected = super::super::liveness::streaming_camera(10, 0, 30.0);
        connected.forwarding = Some("rtsp://user:pw@10.0.0.9/out".into());
        bench.snapshot.cameras = vec![connected];
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.snapshot.devices = vec![device("sim:1", "SIM1"), device("sim:2", "SIM2")];
        bench.side.discovery_issues = vec!["USB access denied".into()];
        bench
            .side
            .connect_failed("sim:2", "no answer\nmore".into(), bench.now);
        bench.apply_scene(&["side-recent", "side-connecting"]);
        bench.address = "192.168.10.50".into();
        bench
            .side
            .connect_failed("192.168.10.50", "no answer".into(), bench.now);
        bench.tick_side();
        bench.sync_side();
        for p in [&style::LIGHT, &style::DARK] {
            let _ = bench.ready(p);
            let (_, extra) = bench.welcome_content(p);
            assert!(extra > 0.0, "the callout and recents add height");
        }
    }

    #[test]
    fn a_connect_by_serial_keeps_the_matching_camera_card_busy() {
        let mut app = bench();
        let mut camera = device("camera-id", "CAMERA-SERIAL");
        camera.address = Some("192.168.1.20".into());
        for target in [&camera.id, &camera.serial, camera.address.as_ref().unwrap()] {
            app.pending.clear();
            app.scene_hold(Job::Connect).target = Some(target.clone());
            assert!(app.connecting_to(&camera), "connecting by {target}");
        }
        app.pending.clear();
        app.scene_hold(Job::Connect).target = Some("OTHER-SERIAL".into());
        assert!(!app.connecting_to(&camera));
    }

    #[test]
    fn shake_starts_and_ends_at_rest() {
        assert_eq!(shake_offset(0.0), 0.0);
        assert!(shake_offset(1.0).abs() < 1e-5);
        assert!(shake_offset(0.5).abs() <= SHAKE_BY);
    }

    #[test]
    fn the_address_hint_waits_for_a_pause_before_saying_no() {
        let mut bench = bench();
        let start = bench.now;
        bench.address = "192.168".into();
        bench.sync_side();
        assert_eq!(bench.side.hint, ("192.168".into(), None));
        assert_eq!(bench.side.typed_at, Some(start));
        bench.now = start + TYPING_PAUSE;
        bench.sync_side();
        assert_eq!(
            bench.side.typed_at,
            Some(start),
            "unchanged text keeps its time"
        );
        bench.address = "192.168.1.20".into();
        bench.sync_side();
        assert_eq!(
            bench.side.hint.1.as_deref(),
            Some("Will try GigE Vision at 192.168.1.20")
        );
        assert_eq!(bench.side.typed_at, Some(bench.now));
    }

    #[test]
    fn failures_retire_once_their_camera_connects_any_way() {
        let mut bench = bench();
        let now = bench.now;
        let mut camera = super::super::liveness::streaming_camera(0, 0, 0.0);
        camera.info.id = "gige:00305311aabb".into();
        camera.info.serial = "40012345".into();
        camera.info.address = Some("192.168.1.40".into());
        for target in ["gige:00305311aabb", "40012345", "192.168.1.40", "sim:3"] {
            bench.side.connect_failed(target, "camera busy".into(), now);
        }
        bench.tick_side();
        assert_eq!(bench.side.connect_failures.len(), 4, "none connected yet");
        // Connected by IP, or by another client: its ID's failure goes too.
        bench.snapshot.cameras = vec![camera];
        bench.tick_side();
        let left: Vec<_> = bench.side.connect_failures.keys().collect();
        assert_eq!(left, ["sim:3"]);
    }

    #[test]
    fn entering_an_address_leaves_image_mode() {
        let mut bench = bench();
        bench.image_mode = true;
        let _ = bench.handle_message(Message::EnterAddress);
        assert!(!bench.image_mode);
    }

    /// The footer's accessible controls by name, and what pressing each
    /// one sends.
    fn footer_buttons(bench: &Workbench) -> Vec<(String, Option<Message>)> {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::futures::StreamExt;
        let renderer = super::super::destinations::layout_tests::renderer();
        let mut view = bench.picker_footer(Palette::of(false), space().into());
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(460.0, 700.0)),
        );
        let (snapshot, bridge, window) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            Layout::new(&node),
            &renderer,
        );
        snapshot
            .nodes
            .iter()
            .filter(|(_, node)| node.role() == accessibility::Role::Button)
            .map(|(id, node)| {
                let task = bridge.dispatch(accessibility::Request {
                    window,
                    replay: None,
                    action: accesskit::ActionRequest {
                        action: accesskit::Action::Click,
                        target: *id,
                        data: None,
                    },
                });
                let sent = iced_runtime::task::into_stream(task).and_then(|mut actions| {
                    match iced::futures::executor::block_on(actions.next()) {
                        Some(iced_runtime::Action::Output(message)) => Some(message),
                        _ => None,
                    }
                });
                (node.label().unwrap_or_default().to_owned(), sent)
            })
            .collect()
    }

    fn has(buttons: &[(String, Option<Message>)], name: &str) -> bool {
        buttons.iter().any(|(label, _)| label == name)
    }

    #[test]
    fn the_network_check_waits_until_gige_is_in_play() {
        let mut bench = bench();
        // Simulated cameras only: no network to check.
        bench.snapshot.devices = vec![device("sim:0", "SIM0"), device("sim:1", "SIM1")];
        assert!(!bench.network_check_relevant());
        let buttons = footer_buttons(&bench);
        assert!(!has(&buttons, "Scan network"));
        assert!(!has(&buttons, "Open network scan results"));
        // A GigE camera found: the check, but no report to open yet.
        let mut gige = device("gige:10.0.0.2", "GE1");
        gige.transport = Transport::GigE;
        gige.address = Some("10.0.0.2".into());
        bench.snapshot.devices.push(gige);
        let buttons = footer_buttons(&bench);
        assert!(has(&buttons, "Scan network"));
        assert!(
            !has(&buttons, "Open network scan results"),
            "never shown disabled"
        );
        // With a report, it opens it.
        bench.snapshot.network_scan = Some(crate::network::NetworkScan {
            interfaces: Vec::new(),
            cameras: Vec::new(),
            warnings: Vec::new(),
            scanned_at_ms: 0,
        });
        let buttons = footer_buttons(&bench);
        let report = buttons
            .iter()
            .find(|(label, _)| label == "Open network scan results")
            .expect("the report button");
        assert!(matches!(report.1, Some(Message::NetworkReport(true))));
    }

    #[test]
    fn a_gige_recent_or_failed_address_brings_the_network_check() {
        let mut bench = bench();
        assert!(!bench.network_check_relevant(), "a first run has none");
        bench.recent = vec![Recent {
            target: "192.168.1.40".into(),
            label: "acA1920-40gc".into(),
            detail: "GigE · 192.168.1.40".into(),
            transport: Transport::GigE,
        }];
        assert!(has(&footer_buttons(&bench), "Scan network"));
        bench.recent.clear();
        bench
            .side
            .connect_failed("rtsp://10.0.0.7/dock", "no answer".into(), bench.now);
        assert!(!bench.network_check_relevant(), "a stream is not GigE");
        bench
            .side
            .connect_failed("192.168.1.50", "no answer".into(), bench.now);
        assert!(bench.network_check_relevant(), "an IP address is");
        assert!(reads_as_gige("gige:10.0.0.9", &[]));
        // The report keeps it while open.
        let mut bench = self::bench();
        bench.network_open = true;
        assert!(bench.network_check_relevant());
    }

    /// A webcam the system refused brings one click to its privacy
    /// settings, where the system has such a page; nothing else does.
    #[test]
    fn a_refused_webcam_offers_the_camera_settings() {
        let label = "Open Camera Settings";
        let mut bench = bench();
        assert!(!bench.camera_permission_relevant());
        assert!(!has(&footer_buttons(&bench), label));
        bench.side.connect_failed(
            "192.168.1.50",
            "no GigE Vision camera answered at 192.168.1.50 within 5 s".into(),
            bench.now,
        );
        assert!(!bench.camera_permission_relevant(), "a GigE timeout");
        let denied = format!(
            "{}: allow this app in System Settings > Privacy & Security > Camera",
            crate::media::CAMERA_ACCESS_DENIED
        );
        bench
            .side
            .connect_failed("avfoundation:0", denied, bench.now);
        assert!(bench.camera_permission_relevant());
        let buttons = footer_buttons(&bench);
        let offered = buttons.iter().find(|(name, _)| name == label);
        if reveal::camera_privacy_label().is_some() {
            assert!(matches!(
                offered.expect("the settings button").1,
                Some(Message::OpenCameraPrivacy)
            ));
        } else {
            assert!(offered.is_none(), "no page to open on this system");
        }
        // Connecting again retires the failure, and the button with it.
        bench.side.connect_failures.remove("avfoundation:0");
        assert!(!has(&footer_buttons(&bench), label));
    }

    /// A USB3 camera Linux refused brings its udev rule to copy, exactly
    /// as `doctor` would print it; other failures bring none.
    #[test]
    fn a_refused_usb_camera_offers_its_fix_to_copy() {
        let mut bench = bench();
        bench
            .side
            .connect_failed("usb:2676:ba02:1", "camera busy".into(), bench.now);
        assert!(bench.usb_access_fix().is_none());
        assert!(!has(&footer_buttons(&bench), USB_FIX));
        let error = crate::doctor::usb_access_error(
            "acA1920-155um",
            std::path::Path::new("/dev/bus/usb/002/003"),
            "2676",
        );
        bench
            .side
            .connect_failed("usb:2676:ba02:2", format!("connect: {error:#}"), bench.now);
        let fix = crate::doctor::usb_access_fix(&["2676"]);
        assert_eq!(bench.usb_access_fix(), Some(fix.as_str()));
        let buttons = footer_buttons(&bench);
        let offered = buttons.iter().find(|(name, _)| name == USB_FIX);
        assert!(
            matches!(&offered.expect("the copy button").1, Some(Message::Copy(copied)) if *copied == fix),
            "{buttons:?}"
        );
        // Copying confirms in place; it never runs the command.
        let _ = bench.handle_message(Message::Copy(fix.clone()));
        assert!(bench.just_copied(&fix));
        assert!(has(&footer_buttons(&bench), USB_FIX), "the name stays");
        bench.side.connect_failures.remove("usb:2676:ba02:2");
        assert!(!has(&footer_buttons(&bench), USB_FIX));
    }

    #[test]
    fn the_address_hint_shows_examples_until_something_reads() {
        assert!(
            ADDRESS_HINT.chars().count() <= 40,
            "one line at the footer's width in caption size"
        );
        let mut bench = bench();
        bench.sync_side();
        assert_eq!(bench.side.hint, (String::new(), None));
        // Empty: the examples (see `picker_footer`); then what it will try.
        bench.address = "192.168.1.20".into();
        bench.sync_side();
        assert!(
            bench
                .side
                .hint
                .1
                .as_deref()
                .is_some_and(|reading| reading.starts_with("Will try GigE Vision"))
        );
    }

    #[test]
    fn the_footer_opens_settings_from_its_gear() {
        let buttons = footer_buttons(&bench());
        let settings = buttons
            .iter()
            .find(|(label, _)| label == "Settings")
            .expect("a Settings button");
        assert!(matches!(settings.1, Some(Message::Settings(true))));
        assert!(has(&buttons, "Keyboard shortcuts and quick guide"));
    }
}
