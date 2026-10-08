//! The single-camera stage: the live image edge to edge with its controls,
//! scopes and status floating over it. Also what the stage shares with the
//! overview's tiles: the zoom, liveness and error badges, the capture flash
//! and the fade-in of a camera's first frame.
use super::*;
use crate::session::CameraSnapshot;
use iced::widget::canvas::{self, Path, Stroke};
use iced::widget::column;
use iced::{Rectangle, Renderer, Vector, mouse};
use motion::Flashes;
use serde_json::json;
use std::collections::HashSet;

/// How long a zoom change eases from one scale to the next.
const ZOOM: Duration = Duration::from_millis(180);
/// How long the focus region's lock-on plays when it comes into focus.
const LOCK: Duration = Duration::from_millis(260);
/// How long a picture takes to fade in on its camera's first frame.
const REVEAL: Duration = Duration::from_millis(220);
/// How long a camera's error must stand with no frame since before its badge
/// shows, so errors that frames flow straight past never flash up.
const ERROR_SETTLE: Duration = Duration::from_millis(250);
/// The scales `zoom_by` keeps to.
pub(super) const ZOOM_MIN: f32 = 0.1;
pub(super) const ZOOM_MAX: f32 = 8.0;
/// The most room an error badge's text gets.
const ERROR_ROOM: f32 = 420.0;
/// What an error badge takes besides its text: the mark before it, and
/// Details and the dismiss after it.
const ERROR_CHROME: f32 = 102.0;
/// Below this much room an error badge's text, and its Details, give way
/// to the mark alone, which tells the error on hover.
const MARK_ONLY: f32 = 60.0;
/// What the "Last frame" badge takes, spacing included.
const LAST_FRAME_ROOM: f32 = 86.0;
/// What an error badge takes as its mark alone: padding, the mark and the
/// dismiss.
const MARK_BADGE: f32 = 42.0;
/// What the stall badge takes with its text, spacing included.
const STALL_ROOM: f32 = 170.0;
/// The least room an error's text keeps beside the stall badge, which
/// gives way to it below this.
const ERROR_BESIDE_STALL: f32 = 120.0;
/// How far the badges sit in from the stage's edges.
const BADGE_INSET: f32 = 12.0;
/// The error a screenshot scene shows.
const SCENE_ERROR: &str = "Frame timeout: the camera sent no frame within 2000 ms";

/// State for the single-camera stage, the overview grid and the scopes.
pub(super) struct StageState {
    zoom: Zoom,
    /// The focus region locking on, replayed from 1 to 0 each time.
    pub(super) lock: Motion,
    /// The focus meter's lock count when last looked at.
    locks_seen: u64,
    /// The controls of the tile the pointer just left, fading out.
    hover_out: Motion,
    /// That tile.
    hover_last: Option<String>,
    /// The tile under the pointer when last looked at.
    hover_seen: Option<String>,
    /// Pictures fading in on their camera's first frame, by camera ID, or
    /// `MAIN_VIEW` for the single view.
    reveal: Flashes<String>,
    /// Connected cameras, and those that have not shown a frame yet.
    known: HashSet<String>,
    fresh: HashSet<String>,
    /// The camera whose frame the single view holds, when last looked at.
    main_seen: Option<String>,
    /// Each camera's reported error, by camera ID.
    errors: HashMap<String, CameraError>,
    /// The latest error of a command this window sent to each camera. Its
    /// notice already says it, so the camera's badge does not.
    answered: HashMap<String, String>,
    /// The errors each camera's badge was dismissed on. A reported one
    /// shows again when it is logged anew.
    dismissed: HashMap<String, HashSet<String>>,
    /// What a screenshot scene holds in place.
    scene: StageScene,
}

/// The main picture's scale easing from one zoom to the next.
struct Zoom {
    /// From 0 at `from` to 1 at `to` while a change plays.
    motion: Motion,
    from: f32,
    /// The scale it heads for or rests at; `None` fits.
    to: Option<f32>,
}

/// An error a camera reports, as its badge shows it.
#[derive(Debug)]
struct CameraError {
    text: String,
    /// When the worker logged it (see `CameraSnapshot::error_logged`).
    logged: Option<String>,
    /// When it was first reported.
    at: Instant,
    /// Frames flowed since, or it was answered by a notice.
    cleared: bool,
    /// Whether its badge shows.
    shown: bool,
}

impl CameraError {
    fn new(text: &str, logged: Option<&String>, at: Instant) -> Self {
        Self {
            text: text.to_owned(),
            logged: logged.cloned(),
            at,
            cleared: false,
            shown: false,
        }
    }

    /// Whether `text`, logged at `logged`, is this occurrence: the same
    /// text, not logged again since. A repeat is a new error, even of one
    /// that frames retired or that was dismissed.
    fn is(&self, text: &str, logged: Option<&String>) -> bool {
        self.text == text && (logged.is_none() || logged == self.logged.as_ref())
    }
}

/// The error a camera's badge shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct BadgeError<'a> {
    pub(super) text: &'a str,
    /// The camera reported it, so Activity has it in the camera's log;
    /// otherwise this window failed to decode the camera's frames.
    pub(super) reported: bool,
}

/// How a picture's badges share the room over its top left: whether
/// "Last frame" shows, whether the stall badge does, and whole, and the
/// room the error's text gets.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BadgePlan {
    last_frame: bool,
    stall: Option<bool>,
    error: f32,
}

impl BadgePlan {
    /// The badges in `room`: "Last frame" when `stopped`, the stall badge
    /// when `stalled` and the error when there is `error`. The error also
    /// keeps clear of `hover` at the end, where a tile's actions show, so
    /// its Details and dismiss stay in reach. It says more than the stall
    /// badge, which shows beside it only while the error's text keeps
    /// `ERROR_BESIDE_STALL`, and than "Last frame", which gives way when
    /// even the error's mark would reach the actions; the caption's dot
    /// and the Start action still say the camera stopped.
    fn new(room: f32, hover: f32, stopped: bool, stalled: bool, error: bool) -> Self {
        let text = |room: f32| (room - hover - ERROR_CHROME).min(ERROR_ROOM);
        let plan = |room: f32| {
            let stall = match (stalled, error) {
                (false, _) => None,
                (true, false) => Some(room >= STALL_ROOM),
                (true, true) => (text(room - STALL_ROOM) >= ERROR_BESIDE_STALL).then_some(true),
            };
            let error = text(if stall.is_some() {
                room - STALL_ROOM
            } else {
                room
            });
            (stall, error)
        };
        if stopped {
            let (stall, text) = plan(room - LAST_FRAME_ROOM);
            if !error || text >= MARK_ONLY || room - hover >= LAST_FRAME_ROOM + MARK_BADGE {
                return Self {
                    last_frame: true,
                    stall,
                    error: text,
                };
            }
        }
        let (stall, error) = plan(room);
        Self {
            last_frame: false,
            stall,
            error,
        }
    }
}

#[derive(Default)]
struct StageScene {
    /// Every camera reports lost frames.
    lost: bool,
    /// The selected camera reports `SCENE_ERROR`.
    error: bool,
}

impl Default for StageState {
    fn default() -> Self {
        Self {
            zoom: Zoom {
                motion: Motion::new(1.0, ZOOM, ZOOM, Kind::Move),
                from: 1.0,
                to: None,
            },
            lock: Motion::new(0.0, LOCK, LOCK, Kind::Fade),
            locks_seen: 0,
            hover_out: Motion::new(0.0, motion::HOVER, motion::HOVER_OUT, Kind::Fade),
            hover_last: None,
            hover_seen: None,
            reveal: Flashes::new(REVEAL),
            known: HashSet::new(),
            fresh: HashSet::new(),
            main_seen: None,
            errors: HashMap::new(),
            answered: HashMap::new(),
            dismissed: HashMap::new(),
            scene: StageScene::default(),
        }
    }
}

impl StageState {
    super::motion::registry! {
        motions: [zoom.motion, lock, hover_out],
        flashes: [reveal],
    }
}

/// The stage's hooks into the shared update cycle.
impl Workbench {
    /// Point the stage's motions at what they show; from
    /// `sync_animations`, after every message.
    pub(super) fn sync_stage(&mut self) {
        let now = self.now;
        // Screenshots show settled states.
        let still = self.screenshot.is_some();
        // Every way of zooming, buttons and shortcuts alike, lands here.
        let target = (!self.fit).then_some(self.zoom);
        if target != self.stage_ui.zoom.to {
            let from = self.shown_scale();
            let zoom = &mut self.stage_ui.zoom;
            zoom.to = target;
            match from {
                Some(from) if !still => {
                    zoom.from = from;
                    zoom.motion.replay(0.0, 1.0, now);
                }
                _ => zoom.motion.set(1.0),
            }
        }
        let locks = self.focus.locks();
        if locks != self.stage_ui.locks_seen {
            self.stage_ui.locks_seen = locks;
            if !still {
                self.stage_ui.lock.replay(1.0, 0.0, now);
            }
        }
        // The tile the pointer left fades its controls out; coming back
        // before they have gone picks them up where they are.
        if self.hovered_tile != self.stage_ui.hover_seen {
            if let Some(left) = self.stage_ui.hover_seen.take() {
                let shown = if self.hovered_tile.is_none() {
                    self.tile_hover.get(now)
                } else {
                    1.0
                };
                self.stage_ui.hover_out.set(shown);
                self.stage_ui.hover_out.go(0.0, now);
                self.stage_ui.hover_last = Some(left);
            } else if self.hovered_tile.is_some() && self.hovered_tile == self.stage_ui.hover_last {
                let shown = self.stage_ui.hover_out.get(now);
                self.tile_hover.set(shown);
                self.tile_hover.go(1.0, now);
                self.stage_ui.hover_out.set(0.0);
            }
            self.stage_ui.hover_seen = self.hovered_tile.clone();
        }
        self.reveal_first_frames(still);
    }

    /// Fade in each camera's first frame, in the single view or its tile,
    /// and introduce the stage controls with it. Later frames, and switching
    /// between the overview and a camera, show at once.
    fn reveal_first_frames(&mut self, still: bool) {
        let now = self.now;
        let main = self.shown.as_ref().and(self.observed_camera.as_ref());
        if main != self.stage_ui.main_seen.as_ref() {
            let main = main.cloned();
            if let Some(id) = &main
                && self.stage_ui.fresh.remove(id)
                && !still
            {
                self.stage_ui.reveal.hit(MAIN_VIEW.to_owned(), now);
                self.wake_controls();
            }
            self.stage_ui.main_seen = main;
        }
        if self.stage_ui.fresh.is_empty() || !self.overview() {
            return;
        }
        let shown: Vec<String> = self
            .stage_ui
            .fresh
            .iter()
            .filter(|id| {
                self.previews
                    .get(*id)
                    .is_some_and(|preview| preview.shown.is_some())
            })
            .cloned()
            .collect();
        for id in shown {
            self.stage_ui.fresh.remove(&id);
            if !still {
                self.stage_ui.reveal.hit(id, now);
            }
        }
    }

    /// The stage's bookkeeping on the slow tick, after the snapshot
    /// refresh: which cameras are new, and which errors their badges show.
    pub(super) fn tick_stage(&mut self) {
        let now = self.now;
        self.patch_stage_scene();
        let cameras = &self.snapshot.cameras;
        let ui = &mut self.stage_ui;
        let connected = |id: &String| cameras.iter().any(|camera| &camera.info.id == id);
        ui.known.retain(connected);
        ui.fresh.retain(connected);
        ui.errors.retain(|id, _| connected(id));
        ui.answered.retain(|id, _| connected(id));
        ui.dismissed.retain(|id, _| connected(id));
        for camera in cameras {
            let id = &camera.info.id;
            if !ui.known.contains(id) {
                ui.known.insert(id.clone());
                ui.fresh.insert(id.clone());
            }
            let Some(text) = &camera.last_error else {
                ui.errors.remove(id);
                continue;
            };
            let logged = camera.error_logged.as_ref();
            let old = ui.errors.get(id);
            if old.is_none_or(|error| !error.is(text, logged)) {
                // The same error again while its badge shows keeps it up;
                // one that frames retired or that was dismissed settles anew.
                let dismissed = ui.dismissed.get_mut(id);
                let showing = old.is_some_and(|old| old.text == *text && old.shown)
                    && dismissed.as_ref().is_none_or(|texts| !texts.contains(text));
                if let Some(dismissed) = dismissed {
                    dismissed.remove(text);
                }
                let mut error = CameraError::new(text, logged, now);
                error.shown = showing;
                ui.errors.insert(id.clone(), error);
            }
            let Some(error) = ui.errors.get_mut(id) else {
                continue;
            };
            // Frames flowing since, or a notice that already said it, retire it.
            let flowing = self
                .liveness
                .get(id)
                .is_some_and(|live| live.framed_since(error.at));
            if flowing || ui.answered.get(id) == Some(text) {
                error.cleared = true;
            }
            error.shown = !error.cleared
                && (error.shown || now.saturating_duration_since(error.at) >= ERROR_SETTLE);
        }
    }

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_stage(
        &mut self,
        pending: &Pending,
        result: &anyhow::Result<serde_json::Value>,
    ) {
        if let (Err(error), Some(camera)) = (result, &pending.camera) {
            self.stage_ui
                .answered
                .insert(camera.clone(), format!("{error:#}"));
        }
    }

    /// Take a screenshot scene word for the stage: `late` is false
    /// while the scene is set up and true once its cameras stream. Words:
    /// `stage-controls` (the stage controls stay up), `stage-zoom` (zoomed
    /// to 150%), `stage-lost` (every camera lost frames, the first and every
    /// other one lately), `stage-error` (the selected camera reports an
    /// error; pair it with `stalled` so no frame retires it), `stage-flash`
    /// (the selected camera's capture flash, held part way) and `stage-hover`
    /// (the pointer over the selected camera's tile).
    pub(super) fn scene_stage(&mut self, word: &str, late: bool) -> bool {
        let camera = self.snapshot.active_camera.clone();
        match (word, late) {
            ("stage-controls", false) => self.over_controls = true,
            ("stage-zoom", false) => {
                self.fit = false;
                self.zoom = 1.5;
            }
            ("stage-lost", true) => {
                self.stage_ui.scene.lost = true;
                let ids: Vec<String> = self
                    .snapshot
                    .cameras
                    .iter()
                    .map(|camera| camera.info.id.clone())
                    .collect();
                for (index, id) in ids.iter().enumerate() {
                    if let Some(live) = self.liveness.get_mut(id) {
                        let ago = Duration::from_secs(if index % 2 == 0 { 2 } else { 40 });
                        live.lost_at = Some(self.now.checked_sub(ago).unwrap_or(self.born));
                    }
                }
                self.patch_stage_scene();
            }
            ("stage-error", true) => {
                self.stage_ui.scene.error = true;
                self.patch_stage_scene();
                // Shown at once: the shot is of a frame drawn before the next tick.
                if let Some(id) = camera {
                    let at = self
                        .now
                        .checked_sub(Duration::from_secs(1))
                        .unwrap_or(self.born);
                    let mut error = CameraError::new(SCENE_ERROR, None, at);
                    error.shown = true;
                    self.stage_ui.errors.insert(id, error);
                }
            }
            ("stage-flash", true) => {
                let result = json!({"files": ["capture-0001.png"], "count": 1});
                self.last_saved = Saved::from_result(camera, &result, self.now);
                self.shutter.set(0.7);
            }
            ("stage-hover", true) => {
                self.hovered_tile = camera.clone();
                self.stage_ui.hover_seen = camera;
                self.tile_hover.set(1.0);
            }
            _ => return false,
        }
        true
    }

    /// Make the snapshot say what a stage scene holds in place; every tick.
    fn patch_stage_scene(&mut self) {
        let scene = &self.stage_ui.scene;
        if !scene.lost && !scene.error {
            return;
        }
        let active = self.snapshot.active_camera.clone();
        for (index, camera) in self.snapshot.cameras.iter_mut().enumerate() {
            if scene.lost {
                camera.dropped += 2 + index as u64;
            }
            if scene.error && active.as_ref() == Some(&camera.info.id) {
                camera.last_error = Some(SCENE_ERROR.into());
                camera.error_logged = None;
            }
        }
    }

    /// Hide camera `id`'s error badge, until the camera logs the error anew.
    pub(super) fn dismiss_stage_error(&mut self, id: &str) {
        if let Some(error) = self.camera_error(id).map(str::to_owned) {
            self.stage_ui
                .dismissed
                .entry(id.to_owned())
                .or_default()
                .insert(error);
        }
    }
}

// What the stage and the tiles share.
impl Workbench {
    /// The selected camera's report.
    pub(super) fn active_camera(&self) -> Option<&CameraSnapshot> {
        let id = self.snapshot.active_camera.as_ref()?;
        self.snapshot
            .cameras
            .iter()
            .find(|camera| &camera.info.id == id)
    }

    /// The scale the main picture shows at: on its way while a zoom change
    /// eases, otherwise the zoom, or `None` to fit. The picture, the focus
    /// region and the capture flash all read this, so they stay together.
    pub(super) fn view_scale(&self) -> Option<f32> {
        if self.stage_ui.zoom.motion.animating(self.now) {
            self.shown_scale()
        } else {
            self.stage_ui.zoom.to
        }
    }

    /// The scale the main picture shows at as a number, fitting included.
    fn shown_scale(&self) -> Option<f32> {
        let zoom = &self.stage_ui.zoom;
        let to = self.resting_scale()?;
        Some(if zoom.motion.animating(self.now) {
            ease_scale(zoom.from, to, zoom.motion.get(self.now))
        } else {
            to
        })
    }

    /// The scale the main picture rests at once any zoom change settles.
    fn resting_scale(&self) -> Option<f32> {
        let native = self.shown.as_ref()?.size();
        Some(
            self.stage_ui
                .zoom
                .to
                .unwrap_or_else(|| preview::fit_scale(self.stage.get(), native)),
        )
    }

    /// The error camera `id`'s badge shows: one decoding its frames here,
    /// else one it reported (see `tick_stage`), unless dismissed.
    pub(super) fn camera_error(&self, id: &str) -> Option<&str> {
        self.badge_error(id).map(|error| error.text)
    }

    /// `camera_error`, and where it comes from.
    pub(super) fn badge_error(&self, id: &str) -> Option<BadgeError<'_>> {
        let local = if self.overview() {
            self.previews
                .get(id)
                .and_then(|preview| preview.error.as_deref())
        } else if self.snapshot.active_camera.as_deref() == Some(id) {
            self.display_error.as_deref()
        } else {
            None
        };
        let reported = self
            .stage_ui
            .errors
            .get(id)
            .filter(|error| error.shown)
            .map(|error| error.text.as_str());
        // Each checked on its own, so a dismissed one never hides the other.
        let dismissed = self.stage_ui.dismissed.get(id);
        let open = |error: &&str| dismissed.is_none_or(|texts| !texts.contains(*error));
        let badge = |reported| move |text| BadgeError { text, reported };
        local
            .filter(open)
            .map(badge(false))
            .or_else(|| reported.filter(open).map(badge(true)))
    }

    /// The badges over the top left of camera `id`'s picture, laid out in
    /// `room` as `BadgePlan` says: "Last frame" when `stopped`, how long
    /// frames have stopped, and its error. Clipped to the room, so nothing
    /// reaches under a neighbouring tile.
    pub(super) fn badges(
        &self,
        id: &str,
        stopped: bool,
        room: f32,
        hover: f32,
        padding: f32,
    ) -> Element<'_, Message> {
        let stalled = self.stalled(id);
        let error = self.badge_error(id);
        let plan = BadgePlan::new(room, hover, stopped, stalled.is_some(), error.is_some());
        let mut badges = row![].spacing(6).align_y(Alignment::Center);
        if plan.last_frame {
            badges = badges.push(last_frame());
        }
        if let (Some(silent), Some(whole)) = (stalled, plan.stall) {
            badges = badges.push(stall_badge(silent, whole));
        }
        if let Some(error) = error {
            badges = badges.push(self.error_badge(id, error, plan.error));
        }
        container(badges)
            .padding(padding)
            .width(Fill)
            .clip(true)
            .into()
    }

    /// A camera's error over its picture: the first line, with a dismiss
    /// and, for an error the camera reported, the way to the whole story in
    /// its log in Activity. `room` is what the text may take; too little
    /// leaves just the mark, explained on hover.
    fn error_badge<'a>(&self, id: &str, error: BadgeError<'a>, room: f32) -> Element<'a, Message> {
        let whole = room >= MARK_ONLY;
        let mut message = row![Level::Error.mark(12.0, style::STAGE)]
            .spacing(6)
            .align_y(Alignment::Center);
        if whole {
            message = message.push(
                container(one_line(
                    error.text,
                    style::SMALL,
                    style::SANS,
                    style::STAGE.text,
                ))
                .max_width(room),
            );
        }
        let mut badge = row![tip(message, error.text)]
            .spacing(4)
            .align_y(Alignment::Center);
        // Only a reported error is in a log, and Activity shows only the
        // selected camera's, so Details selects the camera first.
        if whole && error.reported {
            let details = button(
                text("Details")
                    .size(style::SMALL)
                    .font(style::MEDIUM)
                    .color(style::STAGE.text),
            )
            .padding([0, 5])
            .style(style::on_glass(false, 1.0))
            .on_press(Message::CameraActivity(id.to_owned()));
            let about = if self.snapshot.active_camera.as_deref() == Some(id) {
                "Show the full error in Activity"
            } else {
                "Select the camera and show the full error in Activity"
            };
            badge = badge.push(tip(details, about));
        }
        let close = button(icon(Icon::Close, 10.0, style::STAGE.secondary))
            .padding(3)
            .style(style::on_glass(false, 1.0))
            .on_press(Message::DismissStageError(id.to_owned()));
        container(badge.push(tip(close, "Dismiss")))
            .padding(iced::Padding {
                top: 2.0,
                right: 2.0,
                bottom: 2.0,
                left: 8.0,
            })
            .style(style::badge)
            .into()
    }

    /// "N lost": amber while losses are recent, neutral once they are not,
    /// with the whole story on hover.
    pub(super) fn loss_label<'a>(
        &self,
        camera: &CameraSnapshot,
        lost: u64,
        shown: f32,
    ) -> Element<'a, Message> {
        let color = if self.recent_loss(&camera.info.id) {
            style::STAGE.warn
        } else {
            style::STAGE.secondary
        };
        tip_above(
            text(format!("{} lost", grouped(lost)))
                .size(style::SMALL)
                .color(fade(color, shown))
                .wrapping(text::Wrapping::None),
            self.loss_about(camera, lost),
        )
    }

    fn loss_about(&self, camera: &CameraSnapshot, lost: u64) -> String {
        let mut about = format!(
            "{} lost since the camera connected",
            plural(lost, "frame", "frames")
        );
        if let Some(at) = self
            .liveness
            .get(&camera.info.id)
            .and_then(|live| live.lost_at)
        {
            about.push_str(&format!(
                ", the last {} ago",
                span(self.now.saturating_duration_since(at))
            ));
        }
        if let Some(stats) = &camera.transport {
            about.push('\n');
            about.push_str(&transport_text(stats));
        }
        about
    }

    /// How bright the capture flash over camera `id` is: 0 unless its
    /// capture just saved. `unknown` also flashes a capture whose camera is
    /// not known.
    pub(super) fn flash(&self, id: &str, unknown: bool) -> f32 {
        let level = self.shutter.get(self.now);
        if level <= 0.001 {
            return 0.0;
        }
        match self
            .last_saved
            .as_ref()
            .map(|saved| saved.camera.as_deref())
        {
            Some(Some(camera)) if camera == id => level,
            Some(None) if unknown => level,
            _ => 0.0,
        }
    }

    /// How far camera `key`'s picture has faded in on its first frame.
    pub(super) fn revealed(&self, key: &str) -> f32 {
        1.0 - self.stage_ui.reveal.level(key, self.now)
    }

    /// How much the overview tile of camera `id` shows its controls: while
    /// the pointer is over it, and fading out after it leaves.
    pub(super) fn tile_controls(&self, id: &str) -> f32 {
        if self.hovered_tile.as_deref() == Some(id) {
            self.tile_hover.get(self.now)
        } else if self.stage_ui.hover_last.as_deref() == Some(id) {
            self.stage_ui.hover_out.get(self.now)
        } else {
            0.0
        }
    }
}

/// A frame that should have come by now, on the stage or a tile: its mark
/// and how long, or unless `whole`, the mark alone, saying it on hover.
fn stall_badge<'a>(silent: Duration, whole: bool) -> Element<'a, Message> {
    let mark = Level::Warning.mark(12.0, style::STAGE);
    if !whole {
        return tip(
            container(mark).padding([4, 6]).style(style::badge),
            silence(silent),
        );
    }
    container(
        row![
            mark,
            text(silence(silent))
                .size(style::SMALL)
                .wrapping(text::Wrapping::None),
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .padding([2, 8])
    .style(style::badge)
    .into()
}

/// `from` to `to` at `t`, evenly in ratio, so each step of the way zooms by
/// the same factor.
fn ease_scale(from: f32, to: f32, t: f32) -> f32 {
    if from <= 0.0 || to <= 0.0 {
        return to;
    }
    from * (to / from).powf(t)
}

/// The capture flash over the picture itself, not the stage around it, as
/// `style::capture_flash` draws it over a tile.
struct Flash {
    native: Size,
    scale: Option<f32>,
    level: f32,
    wash: bool,
}

impl canvas::Program<Message> for Flash {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let local = Rectangle::with_size(bounds.size());
        let Some(picture) = preview::placement(local, self.native, self.scale).intersection(&local)
        else {
            return Vec::new();
        };
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let white = |a: f32| Color { a, ..Color::WHITE };
        if self.wash {
            frame.fill_rectangle(
                picture.position(),
                picture.size(),
                white(style::FLASH_WASH * self.level),
            );
        }
        // The edge inside the picture, as a tile's border is.
        let inset = style::FLASH_WIDTH / 2.0;
        frame.stroke(
            &Path::rectangle(
                picture.position() + Vector::new(inset, inset),
                Size::new(
                    picture.width - style::FLASH_WIDTH,
                    picture.height - style::FLASH_WIDTH,
                ),
            ),
            Stroke::default()
                .with_width(style::FLASH_WIDTH)
                .with_color(white(style::FLASH_EDGE * self.level)),
        );
        vec![frame.into_geometry()]
    }
}

impl Workbench {
    pub(super) fn single_view(&self, p: &'static Palette) -> Element<'_, Message> {
        let body = match &self.shown {
            Some(shown) => self.stage(shown, p),
            None if self.snapshot.connected.is_none() => self.welcome(p),
            None => self.ready(p),
        };
        column![self.single_bar(p), body].into()
    }

    /// The live image filling the stage, with what floats over it.
    fn stage<'a>(&'a self, shown: &'a Shown, p: &'static Palette) -> Element<'a, Message> {
        let snapshot = &self.snapshot;
        let id = snapshot.active_camera.as_deref().unwrap_or_default();
        let scale = self.view_scale();
        let image = responsive(move |size| {
            self.stage.set(size);
            shown.view(&self.gpu, scale)
        });
        let image = container(image)
            .width(Fill)
            .height(Fill)
            .style(style::stage);
        let mut layers = stack![veil(image, p.stage, 0.0, self.revealed(MAIN_VIEW))];
        if let Some(region) = self.focus_overlay(shown) {
            layers = layers.push(region);
        }
        let flash = self.flash(id, true);
        if flash > 0.0 {
            layers = layers.push(
                iced::widget::canvas(Flash {
                    native: shown.size(),
                    scale,
                    level: flash,
                    wash: style::flash_wash(),
                })
                .width(Fill)
                .height(Fill),
            );
        }
        // The badges keep clear of the scope cards in the other corner.
        let stage = self.stage.get();
        let room = stage.width - 2.0 * BADGE_INSET - self.scope_room(stage);
        layers = layers.push(self.badges(id, !snapshot.streaming, room, 0.0, BADGE_INSET));
        if let Some(scopes) = self.scopes() {
            layers = layers.push(scopes);
        }
        let controls = self.controls.get(self.now);
        if controls > 0.01 {
            layers = layers.push(
                container(
                    mouse_area(self.stage_controls(controls))
                        .on_enter(Message::OverControls(true))
                        .on_exit(Message::OverControls(false)),
                )
                .center_x(Fill)
                .align_bottom(Fill)
                .padding(iced::Padding {
                    bottom: 16.0 + motion::rise(6.0, controls),
                    ..iced::Padding::new(16.0)
                }),
            );
        }
        mouse_area(layers)
            .on_move(|_| Message::Pointer)
            .on_double_click(Message::ToggleImage)
            .into()
    }

    /// Zoom, scope and image mode controls with the frame's details, on
    /// dark glass at the bottom of the stage; `shown` fades them.
    fn stage_controls(&self, shown: f32) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let snapshot = &self.snapshot;
        let stage = style::STAGE;
        let zoom_actual = !self.fit && (self.zoom - 1.0).abs() < 0.01;
        let ink = |active: bool, enabled: bool| {
            let color = if active {
                stage.accent_text
            } else {
                stage.secondary
            };
            fade(color, if enabled { shown } else { 0.4 * shown })
        };
        let segment = |label: &'static str, selected: bool, on: Message| {
            button(text(label).size(style::SMALL).font(if selected {
                style::SEMIBOLD
            } else {
                style::SANS
            }))
            .padding([3, 10])
            .style(style::glass_segment(selected, shown))
            .on_press(on)
        };
        let glass = |kind: Icon, active: bool, on: Option<Message>| {
            button(icon(kind, 14.0, ink(active, on.is_some())))
                .padding(5)
                .style(style::on_glass(active, shown))
                .on_press_maybe(on)
        };
        let resting = self.resting_scale();
        let zoom_out = resting.is_none_or(|scale| scale > ZOOM_MIN + 1e-3);
        let zoom_in = resting.is_none_or(|scale| scale < ZOOM_MAX - 1e-3);
        let readout = text(
            self.shown_scale()
                .map_or_else(|| "–".to_owned(), |scale| format!("{:.0}%", scale * 100.0)),
        )
        .size(style::SMALL)
        .color(fade(stage.secondary, shown))
        .width(44)
        .align_x(Alignment::Center)
        .wrapping(text::Wrapping::None);
        let camera = self.active_camera();
        let mut details = vec![];
        // With the title bar hidden, say whose picture this is.
        if self.image_mode {
            if let Some(connected) = &snapshot.connected {
                details.push(connected.model.clone());
            }
            if snapshot.streaming {
                details.push(format!("{:.1} fps", snapshot.fps));
            }
        }
        if let Some((_, width, height, format, _)) = self.frame_meta {
            details.push(format!("{width} × {height}"));
            details.push(pixel_name(format));
        }
        details.push(format!("{} frames", grouped(snapshot.frames)));
        let mut summary = row![
            one_line(
                details.join("  ·  "),
                style::SMALL,
                style::SANS,
                fade(stage.secondary, shown),
            )
            .width(Fill)
            .align_right()
        ]
        .align_y(Alignment::Center);
        // Only the loss takes a status color.
        let lost = camera.map_or(0, liveness::camera_loss);
        if let Some(camera) = camera.filter(|_| lost > 0) {
            summary = summary
                .push(
                    text("  ·  ")
                        .size(style::SMALL)
                        .color(fade(stage.secondary, shown)),
                )
                .push(self.loss_label(camera, lost, shown));
        }
        let bar = row![
            row![
                tip_above(
                    segment("Fit", self.fit, Message::Fit),
                    Action::ZoomFit.hint("Zoom to fit", os)
                ),
                tip_above(
                    segment("1:1", zoom_actual, Message::Actual),
                    Action::ZoomActual.hint("Actual pixels", os)
                ),
            ]
            .spacing(2),
            row![
                tip_above(
                    glass(
                        Icon::Minus,
                        false,
                        zoom_out.then_some(Message::Zoom(1.0 / 1.25))
                    ),
                    Action::ZoomOut.hint("Zoom out", os)
                ),
                readout,
                tip_above(
                    glass(Icon::Plus, false, zoom_in.then_some(Message::Zoom(1.25))),
                    Action::ZoomIn.hint("Zoom in", os)
                ),
            ]
            .align_y(Alignment::Center),
            tip_above(
                glass(
                    Icon::Chart,
                    self.exposure_open,
                    Some(Message::ToggleExposure)
                ),
                Action::ToggleExposure.hint("Histogram", os)
            ),
            tip_above(
                glass(
                    Icon::Scan,
                    self.focus.open,
                    Some(Message::ToggleFocusRegion)
                ),
                Action::ToggleFocusRegion.hint("Focus region", os)
            ),
            summary,
            tip_above(
                glass(
                    if self.image_mode {
                        Icon::CornersIn
                    } else {
                        Icon::CornersOut
                    },
                    false,
                    Some(Message::ToggleImage)
                ),
                Action::ImageMode.hint(
                    if self.image_mode {
                        "Leave image only"
                    } else {
                        "Image only"
                    },
                    os
                )
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        container(bar)
            .max_width(if self.image_mode { 760 } else { 640 })
            .width(Fill)
            .padding([6, 8])
            .style(style::overlay(shown))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liveness::streaming_camera;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn bench() -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.snapshot.cameras = vec![streaming_camera(10, 0, 30.0)];
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.observed_camera = Some("sim:0".into());
        bench.snapshot.connected = Some(crate::transport::simulator::info());
        bench
    }

    fn shown(width: f32, height: f32) -> Shown {
        Shown::Gpu {
            key: MAIN_VIEW.into(),
            size: Size::new(width, height),
        }
    }

    #[test]
    fn zoom_eases_between_scales_and_settles_where_it_rests() {
        let mut bench = bench();
        bench.shown = Some(shown(800.0, 600.0));
        bench.stage.set(Size::new(400.0, 300.0));
        bench.sync_animations();
        assert_eq!(bench.view_scale(), None, "fits at rest");
        let start = bench.now;
        // 1:1 from Fit at 50%.
        bench.fit = false;
        bench.zoom = 1.0;
        bench.sync_animations();
        assert!(bench.animating());
        assert_eq!(
            bench.view_scale(),
            Some(0.5),
            "starts where the picture was"
        );
        bench.now = start + ZOOM / 2;
        let mid = bench.view_scale().unwrap();
        assert!(mid > 0.5 && mid < 1.0, "on its way: {mid}");
        // Interrupted: back to Fit from wherever it is.
        bench.fit = true;
        bench.sync_animations();
        assert!((bench.view_scale().unwrap() - mid).abs() < 1e-4, "no jump");
        bench.now += ZOOM;
        assert!(!bench.stage_ui.zoom.motion.animating(bench.now));
        assert_eq!(
            bench.view_scale(),
            None,
            "fitting again, so it follows resizes"
        );
        assert_eq!(bench.shown_scale(), Some(0.5));
    }

    #[test]
    fn zoom_snaps_without_a_picture_and_in_screenshots() {
        let mut bench = bench();
        bench.fit = false;
        bench.zoom = 2.0;
        bench.sync_animations();
        assert!(!bench.stage_ui.zoom.motion.animating(bench.now));
        assert_eq!(bench.view_scale(), Some(2.0));
        assert_eq!(ease_scale(0.5, 2.0, 0.5), 1.0, "even in ratio");
        assert_eq!(ease_scale(0.5, 2.0, 1.0), 2.0);
    }

    #[test]
    fn focus_lock_plays_once_and_settles() {
        let mut bench = bench();
        let start = bench.now;
        bench.focus.open = true;
        let sharpness = |tenengrad| frame::Sharpness {
            tenengrad,
            ..Default::default()
        };
        bench.focus.metric = scopes::Metric::Tenengrad;
        // Settling at its own peak reads in focus, but is no lock-on.
        for _ in 0..12 {
            bench.focus.record(sharpness(40.0));
        }
        bench.sync_animations();
        assert!(bench.focus.in_focus());
        assert!(!bench.stage_ui.lock.animating(start));
        // Out of focus, then back: it locks on.
        bench.focus.record(sharpness(30.0));
        bench.focus.record(sharpness(39.5));
        bench.sync_animations();
        assert!(bench.stage_ui.lock.animating(start + ms(1)));
        let peak = (bench.stage_ui.lock.get(start + LOCK / 5) * std::f32::consts::PI).sin();
        assert!(peak > 0.9, "bumps early: {peak}");
        bench.now = start + LOCK;
        assert!(!bench.animating(), "settles");
        assert_eq!(bench.stage_ui.lock.get(bench.now), 0.0);
    }

    #[test]
    fn first_frames_fade_in_once_per_camera() {
        let mut bench = bench();
        bench.tick_stage();
        assert!(bench.stage_ui.fresh.contains("sim:0"));
        bench.shown = Some(shown(640.0, 480.0));
        bench.sync_animations();
        assert_eq!(bench.revealed(MAIN_VIEW), 0.0, "starts hidden");
        assert!(bench.pointer_moved.is_some(), "introduces the controls");
        let start = bench.now;
        bench.now = start + REVEAL / 2;
        let half = bench.revealed(MAIN_VIEW);
        assert!(half > 0.5 && half < 1.0, "eases out: {half}");
        bench.now = start + REVEAL;
        assert_eq!(bench.revealed(MAIN_VIEW), 1.0);
        assert!(!bench.animating());
        // Another frame of the same camera, or the same camera again after
        // the overview, shows at once.
        bench.shown = None;
        bench.sync_animations();
        bench.shown = Some(shown(640.0, 480.0));
        bench.pointer_moved = None;
        bench.sync_animations();
        assert_eq!(bench.revealed(MAIN_VIEW), 1.0);
        assert!(bench.pointer_moved.is_none());
    }

    #[test]
    fn tile_controls_fade_out_after_the_pointer_leaves() {
        let mut bench = bench();
        let start = bench.now;
        let _ = bench.handle_message(Message::HoverTile(Some("sim:0".into())));
        bench.sync_animations();
        bench.now = start + motion::HOVER;
        assert_eq!(bench.tile_controls("sim:0"), 1.0);
        let left = bench.now;
        let _ = bench.handle_message(Message::HoverTile(None));
        bench.sync_animations();
        assert_eq!(
            bench.tile_controls("sim:0"),
            1.0,
            "still there as it leaves"
        );
        bench.now = left + motion::HOVER_OUT / 2;
        let fading = bench.tile_controls("sim:0");
        assert!(fading > 0.0 && fading < 1.0);
        // Back before they have gone: they come back from where they are.
        let _ = bench.handle_message(Message::HoverTile(Some("sim:0".into())));
        bench.sync_animations();
        assert!((bench.tile_controls("sim:0") - fading).abs() < 1e-4);
        let back = bench.now;
        bench.now = back + motion::HOVER;
        assert_eq!(bench.tile_controls("sim:0"), 1.0);
        let left = bench.now;
        let _ = bench.handle_message(Message::HoverTile(None));
        bench.sync_animations();
        bench.now = left + motion::HOVER_OUT;
        assert_eq!(bench.tile_controls("sim:0"), 0.0);
        assert!(!bench.animating());
    }

    #[test]
    fn camera_errors_show_until_frames_flow_or_they_are_dismissed() {
        let mut bench = bench();
        let start = bench.now;
        bench.observe_liveness();
        bench.snapshot.cameras[0].last_error = Some("Frame timeout".into());
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None, "not before it settles");
        bench.now = start + ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some("Frame timeout"));
        // Dismissed: gone until the error changes.
        bench.dismiss_stage_error("sim:0");
        assert_eq!(bench.camera_error("sim:0"), None);
        bench.snapshot.cameras[0].last_error = Some("Link down".into());
        bench.tick_stage();
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some("Link down"));
        // A frame reaches the screen: the error is history.
        bench.now += ms(10);
        bench.frame_seen("sim:0");
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None);
        bench.snapshot.cameras[0].last_error = None;
        bench.tick_stage();
        assert!(bench.stage_ui.errors.is_empty());
    }

    #[test]
    fn the_same_error_logged_again_shows_again() {
        let mut bench = bench();
        bench.observe_liveness();
        fn camera(bench: &mut Workbench) -> &mut CameraSnapshot {
            &mut bench.snapshot.cameras[0]
        }
        let stopped = "Acquisition stopped after repeated transport errors";
        camera(&mut bench).last_error = Some(stopped.into());
        camera(&mut bench).error_logged = Some("10:00:00".into());
        bench.tick_stage();
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some(stopped));
        // Logged again while its badge shows: it stays up, no blink.
        camera(&mut bench).error_logged = Some("10:00:05".into());
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some(stopped));
        // Restarted: frames retire it, however long it stays the last error.
        bench.now += ms(10);
        bench.frame_seen("sim:0");
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None);
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None, "the same occurrence");
        // The same failure later on.
        camera(&mut bench).error_logged = Some("10:10:00".into());
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None, "not before it settles");
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some(stopped));
        // Dismissed, it stays so until it is logged once more.
        bench.dismiss_stage_error("sim:0");
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None);
        camera(&mut bench).error_logged = Some("10:20:00".into());
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None, "settles anew");
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some(stopped));
        // Its line scrolled out of the worker's log: no new occurrence.
        camera(&mut bench).error_logged = None;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some(stopped));
    }

    #[test]
    fn a_stream_starting_is_no_frame_flowing() {
        let mut bench = bench();
        bench.snapshot.cameras[0].streaming = false;
        bench.snapshot.cameras[0].last_error = Some("Link down".into());
        bench.tick_stage();
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some("Link down"));
        // Started again: no frame yet, so the error stands.
        bench.now += ms(100);
        bench.snapshot.cameras[0].streaming = true;
        bench.observe_liveness();
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), Some("Link down"));
        // The first frame is counted.
        bench.now += ms(100);
        bench.snapshot.cameras[0].frames += 1;
        bench.observe_liveness();
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None);
    }

    #[test]
    fn dismissing_one_error_never_hides_another() {
        let mut bench = bench();
        bench.display_error = Some("Unsupported pixel format".into());
        let error = bench.badge_error("sim:0").unwrap();
        assert_eq!(error.text, "Unsupported pixel format");
        assert!(!error.reported, "not in any log");
        bench.dismiss_stage_error("sim:0");
        assert_eq!(bench.camera_error("sim:0"), None);
        bench.snapshot.cameras[0].last_error = Some("Link down".into());
        bench.tick_stage();
        bench.now += ERROR_SETTLE;
        bench.tick_stage();
        let error = bench.badge_error("sim:0").unwrap();
        assert_eq!((error.text, error.reported), ("Link down", true));
        // Dismissed too: neither comes back in turn.
        bench.dismiss_stage_error("sim:0");
        for _ in 0..3 {
            bench.now += ERROR_SETTLE;
            bench.tick_stage();
            assert_eq!(bench.camera_error("sim:0"), None);
        }
    }

    #[test]
    fn details_select_the_camera_whose_log_tells_the_error() {
        let mut bench = bench();
        let mut other = streaming_camera(10, 0, 30.0);
        other.info.id = "sim:1".into();
        bench.snapshot.cameras.push(other);
        bench.image_mode = true;
        let _ = bench.handle_message(Message::CameraActivity("sim:0".into()));
        assert!(bench.logs_open && !bench.image_mode);
        assert!(!bench.pending(Job::Select), "already selected");
        bench.logs_open = false;
        let _ = bench.handle_message(Message::CameraActivity("sim:1".into()));
        assert!(bench.logs_open);
        assert!(bench.pending_for(Job::Select, "sim:1"));
    }

    #[test]
    fn badges_give_the_error_its_room_before_the_stall() {
        let error = |plan: BadgePlan| plan.error;
        // A 4-camera tile: the error's text and controls clear the actions,
        // and the stall gives way to it.
        let tile = BadgePlan::new(408.0, 88.0, false, true, true);
        assert_eq!(tile.stall, None);
        assert_eq!(error(tile), 218.0);
        assert_eq!(
            BadgePlan::new(408.0, 88.0, false, true, false).stall,
            Some(true)
        );
        // A wide tile has room for both; a compact stage beside its scope
        // cards would leave the error too little.
        let wide = BadgePlan::new(710.0, 88.0, false, true, true);
        assert_eq!(wide.stall, Some(true));
        assert_eq!(error(wide), 350.0);
        let compact = BadgePlan::new(355.0, 0.0, false, true, true);
        assert_eq!(compact.stall, None);
        assert_eq!(error(compact), 253.0);
        // A small one: the stall's mark alone, or the error's.
        assert_eq!(
            BadgePlan::new(160.0, 88.0, false, true, false).stall,
            Some(false)
        );
        assert!(error(BadgePlan::new(240.0, 88.0, false, false, true)) < MARK_ONLY);
        // "Last frame" takes its room; the text never takes more than its cap.
        let stopped = BadgePlan::new(408.0, 88.0, true, false, true);
        assert!(stopped.last_frame);
        assert_eq!(error(stopped), 218.0 - LAST_FRAME_ROOM);
        // A stopped tile's error, down to its mark: "Last frame" gives way
        // before the mark and dismiss would reach the actions.
        let narrow = BadgePlan::new(190.0 - 20.0, 88.0, true, false, true);
        assert!(!narrow.last_frame);
        assert!(error(narrow) < MARK_ONLY);
        assert!(BadgePlan::new(388.0, 88.0, true, false, true).last_frame);
        let tight = BadgePlan::new(88.0 + LAST_FRAME_ROOM + MARK_BADGE, 88.0, true, false, true);
        assert!(tight.last_frame && error(tight) < MARK_ONLY);
        // Without an error, or with no actions to keep clear of, it stays.
        assert!(BadgePlan::new(120.0, 88.0, true, false, false).last_frame);
        assert!(BadgePlan::new(130.0, 0.0, true, false, true).last_frame);
        assert_eq!(
            error(BadgePlan::new(2000.0, 0.0, false, true, true)),
            ERROR_ROOM
        );
    }

    #[test]
    fn errors_a_notice_already_told_get_no_badge() {
        let mut bench = bench();
        let start = bench.now;
        let pending = Pending {
            camera: Some("sim:0".into()),
            at: start,
            ..Pending::unanswered(Job::Set("Gain".into()))
        };
        bench.result_stage(
            &pending,
            &Err(anyhow::anyhow!("30 is above the maximum of 24")),
        );
        bench.snapshot.cameras[0].last_error = Some("30 is above the maximum of 24".into());
        bench.tick_stage();
        bench.now = start + ERROR_SETTLE * 2;
        bench.tick_stage();
        assert_eq!(bench.camera_error("sim:0"), None);
    }

    #[test]
    fn capture_flash_lands_on_the_captured_camera_and_settles() {
        let mut bench = bench();
        let start = bench.now;
        assert_eq!(bench.flash("sim:0", true), 0.0);
        bench.shutter.replay(1.0, 0.0, start);
        bench.last_saved = Some(Saved {
            camera: Some("sim:1".into()),
            path: "capture.png".into(),
            count: 1,
            at: start,
        });
        assert_eq!(bench.flash("sim:1", false), 1.0);
        assert_eq!(bench.flash("sim:0", true), 0.0, "another camera's");
        bench.now = start + motion::SHUTTER;
        assert_eq!(bench.flash("sim:1", false), 0.0);
        assert!(!bench.animating());
    }
}
