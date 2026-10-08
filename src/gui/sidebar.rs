//! The camera list: discovered, connected and recent cameras, and the window footer.
use super::*;
use iced::widget::{column, hover};
use motion::Flashes;
use std::collections::HashSet;

/// How many failed connect targets the camera list remembers.
const FAILURES_KEPT: usize = 8;
/// How long a newly found camera takes to grow into the list and fade in.
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
/// How long a discovery runs before the welcome screen says it is
/// searching, so a quick one does not flash its placeholder.
const SEARCH_SHOWN: Duration = Duration::from_millis(200);
/// Key of the welcome screen's discovery callout in `SideState::arrived`.
pub(super) const ISSUES: &str = "\0issues";

/// Room between the sidebar's edges and its rows.
const EDGE: f32 = 12.0;
/// Inset of text and glyphs within a row, which headers and hints share, so
/// the sidebar has one left edge.
const INSET: f32 = 10.0;
/// The slot leading glyphs sit in, so device and recent titles align.
const LEAD: f32 = 16.0;
/// The slot trailing status glyphs and the eject button share.
const TRAIL: f32 = 21.0;
/// A camera row's height while it grows in; a little over its real height.
const ROW_HEIGHT: f32 = 48.0;

/// What discovery looks for, in its tooltips.
pub(super) const DISCOVERS: &str = "Find webcams, GigE Vision, USB3 Vision and ONVIF cameras";
/// What discovery looks for, while it does.
pub(super) const DISCOVERING: &str =
    "Looking for webcams, GigE Vision, USB3 Vision and ONVIF cameras…";
/// What the address field takes, in full.
pub(super) const ADDRESSES: &str =
    "IP address, stream URL (RTSP, SRT, RTMP, HTTP), onvif: address or video file";
/// What the address field takes, in the room under it.
const ADDRESS_HINT: &str = "IP, stream URL, onvif: or video file";

/// The sidebar-welcome package's own state: the camera list and the welcome
/// screen. Add fields here, register their motions below and point them in
/// `sync_side`.
pub(super) struct SideState {
    /// The last discovery's problems, each in full: its warnings, or the
    /// error it failed with. Replaced whenever a discovery finishes.
    pub(super) discovery_issues: Vec<String>,
    /// Connect targets (IDs or addresses) whose last attempt failed, with
    /// the whole error and when. Kept by `failed()`; a retry, a success or
    /// editing the address clears one.
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
        }
    }
}

impl SideState {
    super::motion::registry! {
        motions: [],
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

/// The sidebar-welcome package's hooks into the shared update cycle.
impl Workbench {
    /// Point the sidebar-welcome package's motions at what they show; from
    /// `sync_animations`. Also reads a changed address, here rather than in
    /// the view, since recognizing a file checks the disk.
    pub(super) fn sync_side(&mut self) {
        if self.side.hint.0 != self.address {
            let reading = read_address(self.address.trim(), &self.snapshot.devices);
            self.side.hint = (self.address.clone(), reading);
            self.side.typed_at = Some(self.now);
        }
    }

    /// The sidebar-welcome package's bookkeeping on the slow tick, after the
    /// snapshot refresh; from `tick()`.
    pub(super) fn tick_side(&mut self) {
        let now = self.now;
        // Newly found cameras arrive one after another, in list order.
        let side = &mut self.side;
        let mut fresh = 0;
        for device in &self.snapshot.devices {
            if !side.found.contains(&device.id) {
                let delay = STAGGER * fresh.min(STAGGERED) as u32;
                side.arrived.hit(device.id.clone(), now + delay);
                fresh += 1;
            }
        }
        if fresh > 0 || side.found.len() != self.snapshot.devices.len() {
            side.found = self
                .snapshot
                .devices
                .iter()
                .map(|device| device.id.clone())
                .collect();
            // A serial typed before discovery found it reads as known now.
            if !self.address.trim().is_empty() {
                side.hint.1 = read_address(self.address.trim(), &self.snapshot.devices);
            }
        }
        let issues = !side.discovery_issues.is_empty();
        if issues && !side.had_issues {
            side.arrived.hit(ISSUES.to_owned(), now);
        }
        side.had_issues = issues;
    }

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`. A failed connect to
    /// the address still in the field shakes it.
    pub(super) fn result_side(
        &mut self,
        pending: &Pending,
        result: &anyhow::Result<serde_json::Value>,
    ) {
        if pending.label == "Discovering cameras" {
            self.side.searched = true;
        }
        if let (Err(_), Some(target)) = (result, &pending.target)
            && self.address.trim() == target
            && !motion::reduce_motion()
        {
            self.side.shakes.hit(target.clone(), self.now);
        }
    }

    /// Take a screenshot scene word the sidebar-welcome package owns: `late` is
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
                    self.scene_hold("Connecting camera").target = Some(target.into());
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

/// `address` without its scheme, credentials, query or fragment: the host,
/// any port, and the path. Safe to show, as `redact_address` is.
pub(super) fn short_address(address: &str) -> String {
    let rest = address.strip_prefix("onvif:").unwrap_or(address);
    let rest = rest
        .strip_prefix("//")
        .or_else(|| rest.split_once("://").map(|(_, rest)| rest))
        .unwrap_or(rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{host}{}", path.trim_end_matches('/'))
}

/// Just the host (and port) of `address`, as `short_address` gives it.
fn host(address: &str) -> String {
    let short = short_address(address);
    match short.split_once('/') {
        Some((host, _)) => host.to_owned(),
        None => short,
    }
}

/// Where a camera is, identifier first: its address or serial, then its
/// transport. Two cameras of one model differ here.
pub(super) fn place(camera: &CameraInfo) -> String {
    match &camera.address {
        Some(address) => format!("{} · {}", short_address(address), camera.transport),
        None => format!("S/N {} · {}", camera.serial, camera.transport),
    }
}

/// A recent camera's detail, identifier first like `place`. Recents keep it
/// as "transport · address or serial".
pub(super) fn recent_place(recent: &Recent) -> String {
    match recent.detail.split_once(" · ") {
        Some((transport, place)) if place.contains("://") => {
            format!("{} · {transport}", short_address(place))
        }
        Some((transport, place)) => format!("{place} · {transport}"),
        None => recent.detail.clone(),
    }
}

/// The first line of an error, for a one-line slot; the tooltip has it all.
pub(super) fn first_line(error: &str) -> &str {
    error.lines().next().unwrap_or(error)
}

/// What connecting to `input` will try, following `Camera::open`'s order;
/// `None` when it is none of the things it accepts. Never shows credentials.
fn read_address(input: &str, devices: &[CameraInfo]) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if crate::onvif::is_source(input) {
        return Some(format!("Will try ONVIF at {}", host(input)));
    }
    if input.starts_with("sim:") {
        return Some("Will try a simulated camera".into());
    }
    if crate::media::is_source(input) {
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

/// A hollow status dot: a camera that is not connected.
fn ring<'a>(color: Color, size: f32) -> Element<'a, Message> {
    container(space().width(size).height(size))
        .style(move |_| container::Style {
            border: iced::Border {
                color,
                width: 1.5,
                radius: (size / 2.0).into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// `glyph` centered in the leading slot.
fn lead<'a>(glyph: Element<'a, Message>) -> Element<'a, Message> {
    container(glyph).center_x(LEAD).into()
}

/// A small button that reveals itself over a row's trailing end on hover,
/// on the row's hovered fill so it covers what sits there.
fn reveal<'a>(button: Element<'a, Message>, p: &'static Palette) -> Element<'a, Message> {
    container(container(button).style(move |_| container::Style {
        background: Some(p.hover.into()),
        border: iced::border::rounded(style::RADIUS_SMALL),
        ..container::Style::default()
    }))
    .align_right(Fill)
    .center_y(Fill)
    .padding(iced::Padding {
        right: INSET,
        ..iced::Padding::ZERO
    })
    .into()
}

/// A quiet checkbox for a developer option: small, with a secondary label.
fn quiet_checkbox<'a>(
    label: &'a str,
    checked: bool,
    on: impl Fn(bool) -> Message + 'a,
    p: &'static Palette,
) -> Element<'a, Message> {
    iced::widget::checkbox(checked)
        .label(label)
        .on_toggle(on)
        .size(14)
        .spacing(7)
        .text_size(style::SMALL)
        .style(move |theme, status| iced::widget::checkbox::Style {
            text_color: Some(p.secondary),
            ..style::check(theme, status)
        })
        .into()
}

/// What a connected camera's trailing slot shows, if anything: a glyph, its
/// color and what it means.
fn camera_status(
    camera: &crate::session::CameraSnapshot,
    stalled: Option<Duration>,
    p: &'static Palette,
) -> Option<(Element<'static, Message>, String)> {
    if let Some(silent) = stalled {
        return Some((
            icon(Icon::WarningTriangle, 12.0, p.ink(p.warn)),
            format!("No new frames for {} s", silent.as_secs()),
        ));
    }
    // An old error says little while frames flow.
    if let Some(error) = camera.last_error.as_ref().filter(|_| !camera.streaming) {
        return Some((
            icon(Icon::Warning, 12.0, p.ink(p.danger)),
            format!("Last error: {error}"),
        ));
    }
    let target = camera.forwarding.as_ref()?;
    Some(if target.contains("://") {
        (
            icon(Icon::Broadcast, 13.0, p.secondary),
            format!("Forwarding to {}", redact_address(target)),
        )
    } else {
        (dot(p.danger, 7.0), format!("Recording to {target}"))
    })
}

impl Workbench {
    /// Whether the welcome screen should say discovery is searching: from
    /// the start until the first one finishes, then once one has run for
    /// `SEARCH_SHOWN`. Before that its slot keeps what it said.
    pub(super) fn searching_shown(&self) -> bool {
        let running = self
            .pending
            .iter()
            .filter(|pending| pending.label == "Discovering cameras")
            .map(|pending| self.now.saturating_duration_since(pending.at))
            .max();
        !self.side.searched || running.is_some_and(|running| running >= SEARCH_SHOWN)
    }

    /// Whether a connect to `camera`, by ID or address, is on its way.
    fn connecting_to(&self, camera: &CameraInfo) -> bool {
        self.connecting(&camera.id)
            || camera
                .address
                .as_deref()
                .is_some_and(|address| self.connecting(address))
    }

    pub(super) fn sidebar(&self, p: &'static Palette) -> Element<'_, Message> {
        let mut devices: Vec<&CameraInfo> = self.snapshot.devices.iter().collect();
        for camera in &self.snapshot.cameras {
            if !devices.iter().any(|device| device.id == camera.info.id) {
                devices.push(&camera.info);
            }
        }
        let discovering = self.pending("Discovering cameras");
        let spin = self.spin();
        let brand = row![
            icon(Icon::Mark, 16.0, p.accent),
            text("Capturefab")
                .size(style::HEADING)
                .font(style::SEMIBOLD)
                .color(p.text),
        ]
        .spacing(7)
        .align_y(Alignment::Center);
        let mut header = row![
            text("Cameras")
                .size(style::CAPTION)
                .font(style::SEMIBOLD)
                .color(p.secondary),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if !devices.is_empty() {
            header = header.push(
                text(devices.len().to_string())
                    .size(style::CAPTION)
                    .color(p.secondary),
            );
        }
        if !self.side.discovery_issues.is_empty() {
            header = header.push(tip(
                icon(Icon::WarningTriangle, 12.0, p.ink(p.warn)),
                self.side.discovery_issues.join("\n"),
            ));
        }
        let refresh: Element<'_, Message> = if discovering {
            icon::spinner(14.0, p.secondary, spin)
        } else {
            icon(Icon::Refresh, 14.0, p.secondary)
        };
        let header = header.push(space::horizontal()).push(tip(
            button(refresh)
                .padding(5)
                .style(style::plain)
                .on_press_maybe((!discovering).then_some(Message::Discover)),
            Action::Discover.hint(DISCOVERS, Os::CURRENT),
        ));
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
                .padding([8.0, INSET]),
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
        for camera in &devices {
            list = list.push(self.camera_row(camera, spin, p));
        }
        // A connect to something not listed yet shows where it will land.
        for target in self.pending.iter().filter_map(|p| p.target.as_deref()) {
            let listed = devices
                .iter()
                .any(|device| device.id == target || device.address.as_deref() == Some(target))
                || recent.iter().any(|recent| recent.target == target);
            if !listed {
                list = list.push(self.provisional_row(target, spin, p));
            }
        }
        if !recent.is_empty() {
            list = list.push(
                container(
                    text("Recent")
                        .size(style::CAPTION)
                        .font(style::SEMIBOLD)
                        .color(p.secondary),
                )
                .padding(iced::Padding {
                    top: 14.0,
                    right: INSET,
                    bottom: 4.0,
                    left: INSET,
                }),
            );
        }
        for entry in recent {
            list = list.push(self.recent_row(entry, spin, p));
        }
        container(
            column![
                // Beside the macOS traffic lights, which sit at the left.
                self.titlebar(
                    container(brand)
                        .padding(iced::Padding {
                            left: self.lights().max(EDGE + INSET),
                            ..iced::Padding::ZERO
                        })
                        .into()
                ),
                space().height(10),
                container(header).padding(iced::Padding {
                    right: EDGE + 1.0,
                    left: EDGE + INSET,
                    ..iced::Padding::ZERO
                }),
                space().height(4),
                container(scrollable(list).height(Fill).style(style::scroll))
                    .height(Fill)
                    .padding([0.0, EDGE]),
                self.sidebar_footer(p),
            ]
            .padding(iced::Padding {
                bottom: EDGE,
                ..iced::Padding::ZERO
            }),
        )
        .width(SIDEBAR)
        .height(Fill)
        .style(style::sidebar)
        .into()
    }

    /// A discovered or connected camera: a status dot, its model, where it
    /// is, and at the end what needs attention or the eject button.
    fn camera_row<'a>(
        &'a self,
        camera: &'a CameraInfo,
        spin: usize,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let id = camera.id.as_str();
        let active = self.snapshot.active_camera.as_deref() == Some(id);
        let state = self.snapshot.cameras.iter().find(|c| c.info.id == id);
        let connecting = state.is_none() && self.connecting_to(camera);
        let failure = (state.is_none() && !connecting)
            .then(|| self.side.connect_failures.get(id))
            .flatten();
        let stalled = state
            .filter(|state| state.streaming)
            .and_then(|_| self.stalled(id));
        let glyph: Element<'a, Message> = match state {
            _ if connecting => icon::spinner(12.0, p.secondary, spin),
            Some(state) if state.streaming && stalled.is_some() => dot(p.warn, 8.0),
            Some(state) if state.streaming => dot(p.live, 8.0),
            Some(_) => dot(p.accent, 8.0),
            None if failure.is_some() => ring(p.danger, 8.0),
            None => ring(p.tertiary, 8.0),
        };
        let (detail, ink) = if connecting {
            ("Connecting…".to_owned(), p.secondary)
        } else if let Some((error, _)) = failure {
            (first_line(error).to_owned(), p.ink(p.danger))
        } else {
            (place(camera), p.secondary)
        };
        let status = state.and_then(|state| camera_status(state, stalled, p));
        let mut hint = match state {
            Some(state) => format!(
                "{} {} · {}",
                camera.vendor,
                camera.model,
                if stalled.is_some() {
                    "waiting for frames"
                } else if state.streaming {
                    "streaming"
                } else {
                    "connected"
                }
            ),
            None => format!("Connect to {} {}", camera.vendor, camera.model),
        };
        if let Some((_, about)) = &status {
            hint = format!("{hint}\n{about}");
        } else if let Some((error, _)) = failure {
            hint = format!("{hint}\nCould not connect: {error}");
        }
        let mut line = row![
            lead(glyph),
            column![
                one_line(
                    camera.model.as_str(),
                    style::BODY,
                    if active {
                        style::SEMIBOLD
                    } else {
                        style::MEDIUM
                    },
                    p.text,
                )
                .width(Fill),
                one_line(detail, style::CAPTION, style::SANS, ink).width(Fill),
            ]
            .spacing(1)
            .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let eject = || -> Element<'a, Message> {
            tip(
                button(icon(Icon::Eject, 13.0, p.secondary))
                    .padding(4)
                    .style(style::plain)
                    .on_press(Message::Disconnect(camera.id.clone())),
                "Disconnect",
            )
        };
        if state.is_some() {
            // Connected rows keep the trailing slot, so their text never
            // shifts; eject takes it on hover, or for good on the selected row.
            match status {
                Some((glyph, about)) => {
                    line = line.push(tip(container(glyph).center_x(TRAIL), about));
                }
                None if !active => line = line.push(space().width(TRAIL)),
                None => {}
            }
            if active {
                line = line.push(eject());
            }
        }
        let base = tip(
            button(line)
                .width(Fill)
                .padding([7.0, INSET])
                .style(style::row_mix(self.selection_level(id)))
                .on_press_maybe((!connecting).then(|| Message::CameraRow(camera.id.clone()))),
            hint,
        );
        let row = if state.is_some() && !active {
            hover(base, reveal(eject(), p))
        } else {
            base
        };
        self.arriving(id, row, p)
    }

    /// A connect to a target no row stands for yet, such as a typed address,
    /// shown where the camera will appear.
    fn provisional_row<'a>(
        &self,
        target: &str,
        spin: usize,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let line = row![
            lead(icon::spinner(12.0, p.secondary, spin)),
            column![
                one_line(short_address(target), style::BODY, style::MEDIUM, p.text).width(Fill),
                one_line("Connecting…", style::CAPTION, style::SANS, p.secondary).width(Fill),
            ]
            .spacing(1)
            .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        button(line)
            .width(Fill)
            .padding([7.0, INSET])
            .style(style::row(false))
            .into()
    }

    /// A camera connected before and not found now, to reconnect or forget.
    fn recent_row<'a>(
        &self,
        entry: &'a Recent,
        spin: usize,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let connecting = self.connecting(&entry.target);
        let failure = (!connecting)
            .then(|| self.side.connect_failures.get(&entry.target))
            .flatten();
        let (glyph, detail, ink): (Element<'a, Message>, String, Color) = if connecting {
            (
                icon::spinner(12.0, p.secondary, spin),
                "Connecting…".into(),
                p.secondary,
            )
        } else if let Some((error, _)) = failure {
            (
                ring(p.danger, 8.0),
                first_line(error).to_owned(),
                p.ink(p.danger),
            )
        } else {
            (
                icon(Icon::Recent, 13.0, p.tertiary),
                recent_place(entry),
                p.secondary,
            )
        };
        let line = row![
            lead(glyph),
            column![
                one_line(entry.label.as_str(), style::BODY, style::MEDIUM, p.text).width(Fill),
                one_line(detail, style::CAPTION, style::SANS, ink).width(Fill),
            ]
            .spacing(1)
            .width(Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut hint = format!("Reconnect {}", entry.label);
        if let Some((error, _)) = failure {
            hint = format!("{hint}\nCould not connect: {error}");
        }
        let forget = tip(
            button(icon(Icon::Close, 11.0, p.secondary))
                .padding(5)
                .style(style::plain)
                .on_press(Message::Forget(entry.target.clone())),
            "Forget",
        );
        hover(
            tip(
                button(line)
                    .width(Fill)
                    .padding([7.0, INSET])
                    .style(style::row(false))
                    .on_press_maybe(
                        (!connecting).then(|| Message::CameraRow(entry.target.clone())),
                    ),
                hint,
            ),
            reveal(forget, p),
        )
    }

    /// `row` growing in and fading up while camera `id` arrives; as it is
    /// otherwise. Under Reduce Motion it only fades.
    fn arriving<'a>(
        &self,
        id: &str,
        row: Element<'a, Message>,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let t = self.side.arrival(id, self.now);
        if t >= 1.0 {
            return row;
        }
        let row = veil(row, p.sidebar, style::RADIUS, t);
        if motion::reduce_motion() {
            return row;
        }
        container(row).max_height(ROW_HEIGHT * t).clip(true).into()
    }

    /// The address field and what it will try, the simulator option, and
    /// the window's settings.
    fn sidebar_footer(&self, p: &'static Palette) -> Element<'_, Message> {
        let typed = self.address.trim();
        let failure = self.side.connect_failures.get(typed);
        let shake = self.side.shake(typed, self.now);
        let field = text_input("Add camera, IP or stream URL", &self.address)
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
        let hint: Element<'_, Message> = if let Some((error, _)) = failure {
            let ink = p.ink(p.danger);
            tip_above(
                row![
                    icon(Icon::Warning, 11.0, ink),
                    one_line(first_line(error), style::CAPTION, style::SANS, ink).width(Fill),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
                format!("Could not connect: {error}"),
            )
        } else {
            let (reading, ink) = match (&self.side.hint, typed.is_empty()) {
                (_, true) => (ADDRESS_HINT, p.secondary),
                ((_, Some(reading)), _) => (reading.as_str(), p.secondary),
                ((_, None), _)
                    if self.side.typed_at.is_some_and(|at| {
                        self.now.saturating_duration_since(at) >= TYPING_PAUSE
                    }) =>
                {
                    ("Not an address, URL, file or serial", p.ink(p.warn))
                }
                ((_, None), _) => (ADDRESS_HINT, p.secondary),
            };
            one_line(reading, style::CAPTION, style::SANS, ink)
                .width(Fill)
                .into()
        };
        let appearance = match self.appearance {
            Appearance::System => (Icon::Contrast, "Appearance: match system"),
            Appearance::Light => (Icon::Sun, "Appearance: light"),
            Appearance::Dark => (Icon::Moon, "Appearance: dark"),
        };
        let command = self.session_command();
        let copied = self.copied.is_some() && self.just_copied(&command);
        // The session's name and a copy of the command that drives it, in
        // one control that confirms in place.
        let session = tip_above(
            button(
                row![
                    if copied {
                        icon(Icon::Check, 13.0, p.live)
                    } else {
                        icon(Icon::Copy, 13.0, p.secondary)
                    },
                    one_line(
                        if copied {
                            "Copied".to_owned()
                        } else {
                            format!("session {}", self.session)
                        },
                        style::CAPTION,
                        style::MONO,
                        p.secondary,
                    ),
                ]
                .spacing(5)
                .align_y(Alignment::Center),
            )
            .padding([5, 6])
            .style(style::plain)
            .on_press(Message::CopySessionCommand),
            Action::CopySessionCommand.hint(&format!("Copy {command}"), Os::CURRENT),
        );
        let settings = row![
            tip_above(
                button(icon(appearance.0, 15.0, p.secondary))
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::CycleAppearance),
                appearance.1,
            ),
            tip_above(
                button(icon(Icon::Help, 15.0, p.secondary))
                    .padding(6)
                    .style(style::plain)
                    .on_press(Message::Help(true)),
                Action::Help.hint("Keyboard shortcuts and quick guide", Os::CURRENT),
            ),
            space::horizontal(),
            session,
        ]
        .spacing(2)
        .align_y(Alignment::Center);
        column![
            // Shifted both ways at once, so it moves without resizing.
            container(field).padding(iced::Padding {
                left: EDGE + shake,
                right: EDGE - shake,
                ..iced::Padding::ZERO
            }),
            container(
                column![
                    hint,
                    quiet_checkbox(
                        "Include simulated cameras",
                        self.include_simulator,
                        Message::IncludeSimulator,
                        p,
                    ),
                ]
                .spacing(10)
            )
            .padding([0.0, EDGE + INSET]),
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
        }
    }

    #[test]
    fn addresses_read_in_the_order_connecting_tries_them() {
        let devices = [device("sim:0", "SIM0")];
        let read = |input: &str| read_address(input, &devices);
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
            // Tests run in the package root.
            ("Cargo.toml", Some("Will try the video file")),
            ("gige:10.0.0.2", Some("Will try GigE Vision at 10.0.0.2")),
            ("192.168.1.20", Some("Will try GigE Vision at 192.168.1.20")),
            ("SIM0", Some("Will try Pattern camera · S/N SIM0")),
            ("sim0", None),
            ("192.168.1", None),
            ("line-7 camera", None),
        ];
        for (input, expected) in cases {
            assert_eq!(read(input).as_deref(), expected, "{input:?}");
        }
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
    fn a_failed_connect_shakes_only_the_address_it_came_from() {
        let mut bench = bench();
        let start = bench.now;
        bench.address = "192.168.10.50".into();
        let pending = |target: &str| Pending {
            label: "Connecting camera".into(),
            receiver: mpsc::channel().1,
            target: Some(target.into()),
            camera: None,
            feature: None,
            at: start,
            batch: false,
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
    fn the_selection_cross_fades_between_rows() {
        use iced::widget::button::Status;
        for dark in [false, true] {
            let theme = style::theme(dark);
            let p = Palette::of(dark);
            let fill = |amount: f32, status: Status| {
                style::row_mix(amount)(&theme, status)
                    .background
                    .map(|background| match background {
                        iced::Background::Color(color) => color,
                        _ => unreachable!(),
                    })
            };
            let row = |selected: bool, status: Status| {
                style::row(selected)(&theme, status)
                    .background
                    .map(|background| match background {
                        iced::Background::Color(color) => color,
                        _ => unreachable!(),
                    })
            };
            for status in [Status::Active, Status::Hovered, Status::Pressed] {
                assert_eq!(fill(1.0, status), row(true, status));
                assert_eq!(fill(0.0, status), row(false, status));
            }
            let half = fill(0.5, Status::Active).expect("a fill");
            assert_eq!(
                (half.r, half.g, half.b),
                (p.selected.r, p.selected.g, p.selected.b)
            );
            assert!((half.a - 0.5).abs() < 1e-6, "fades in place");
        }
    }

    #[test]
    fn quick_searches_do_not_flash_the_placeholder() {
        let mut bench = bench();
        let start = bench.now;
        assert!(bench.searching_shown(), "until the first search finishes");
        let discovery = || Pending {
            label: "Discovering cameras".into(),
            receiver: mpsc::channel().1,
            target: None,
            camera: None,
            feature: None,
            at: start,
            batch: false,
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
}
