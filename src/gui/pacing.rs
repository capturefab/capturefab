//! Pacing the live image to what the window can show. The frame watcher
//! wakes the workbench when a camera it shows has a new frame: no faster than
//! the display, never while its last announcement waits, and rarely while no
//! frame is due. The draw log records the main window's draws, so frame work
//! pauses while nothing draws, and says which frames reached the screen.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{Event, Rectangle, Vector, mouse};
use std::{sync::atomic::AtomicBool, thread::Thread};

/// The finest wait between checks for a new frame. Finer checks at high
/// refresh rates keep frames on their vsync.
const FRAME_POLL: (Duration, Duration) = (Duration::from_millis(1), Duration::from_millis(2));
/// Efficiency mode announces at most 15 frames a second, so checking a few
/// milliseconds late costs nothing there.
const EFFICIENT_POLL: (Duration, Duration) = (Duration::from_millis(4), Duration::from_millis(8));
/// How long the watcher sleeps at a time while its announcement waits; the
/// workbench wakes it as soon as it takes the frame.
const PARKED: Duration = Duration::from_millis(250);
/// A main window that has not drawn this long after the view changed is
/// hidden, minimized or covered: frames wait until it draws again.
pub(super) const UNDRAWN: Duration = Duration::from_millis(250);

/// What the frame watcher reads from the workbench, live, so following the
/// display, a camera's rate or the view never restarts it.
#[derive(Default)]
pub(super) struct Cadence {
    /// Nanoseconds between announcements; see `Workbench::frame_gap`.
    gap: AtomicU64,
    /// Nanoseconds of the finest wait between checks.
    fine: AtomicU64,
    /// Nanoseconds between frames of the fastest camera shown; zero while
    /// unknown.
    interval: AtomicU64,
    /// The cameras whose frames the workbench shows.
    shown: Mutex<Vec<String>>,
    /// A frame was announced and the workbench has not taken it yet.
    announced: AtomicBool,
    /// The watcher thread, woken once it may announce again.
    watcher: Mutex<Option<Thread>>,
}

impl Cadence {
    /// Follow the workbench: announce frames of `shown` cameras at most
    /// every `gap`, expecting one every `interval`.
    pub(super) fn follow(
        &self,
        gap: Duration,
        interval: Option<Duration>,
        efficient: bool,
        shown: Vec<&str>,
    ) {
        let (finest, coarsest) = if efficient {
            EFFICIENT_POLL
        } else {
            FRAME_POLL
        };
        let nanos = |duration: Duration| duration.as_nanos() as u64;
        self.gap.store(nanos(gap), Ordering::Relaxed);
        self.fine
            .store(nanos((gap / 8).clamp(finest, coarsest)), Ordering::Relaxed);
        self.interval
            .store(interval.map_or(0, nanos), Ordering::Relaxed);
        let mut current = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        if !current.iter().map(String::as_str).eq(shown.iter().copied()) {
            *current = shown.into_iter().map(str::to_owned).collect();
        }
    }

    /// The workbench took the newest frames: the watcher may announce again.
    pub(super) fn taken(&self) {
        self.announced.store(false, Ordering::Release);
        if let Some(watcher) = &*self.watcher.lock().unwrap_or_else(|e| e.into_inner()) {
            watcher.unpark();
        }
    }

    /// The gap, finest wait and expected interval, as last followed.
    fn timing(&self) -> (Duration, Duration, Duration) {
        let read = |value: &AtomicU64| Duration::from_nanos(value.load(Ordering::Relaxed));
        (read(&self.gap), read(&self.fine), read(&self.interval))
    }

    /// The shown cameras' frame sequences together; it changes with each
    /// frame any of them offers. Lock-free in the session.
    fn sequence(&self, handle: &SessionHandle) -> u64 {
        let shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<&str> = shown.iter().map(String::as_str).collect();
        handle.frame_sequence_of(&ids)
    }
}

/// The session whose shared frame rings the frame watcher polls, and how. A
/// window has one session, and the cadence is read live rather than hashed.
pub(super) struct FrameWatch {
    pub(super) handle: SessionHandle,
    pub(super) cadence: Arc<Cadence>,
}

impl std::hash::Hash for FrameWatch {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

/// Sends `FrameArrived` when a shown camera publishes a frame: at most once
/// per gap, and not again until the workbench has taken the last one, so
/// announcements never queue up behind a slow update.
pub(super) fn watch_frames(
    watch: &FrameWatch,
) -> impl iced::futures::Stream<Item = Message> + use<> {
    let handle = watch.handle.clone();
    let cadence = watch.cadence.clone();
    iced::stream::channel(0, async move |mut output| {
        std::thread::spawn(move || {
            *cadence.watcher.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::thread::current());
            let mut seen = cadence.sequence(&handle);
            let mut changed = Instant::now();
            while !output.is_closed() {
                let (gap, fine, interval) = cadence.timing();
                std::thread::sleep(poll_after(changed.elapsed(), interval, gap, fine));
                let sequence = cadence.sequence(&handle);
                if sequence == seen {
                    continue;
                }
                cadence.announced.store(true, Ordering::Release);
                if output.try_send(Message::FrameArrived).is_err() {
                    cadence.announced.store(false, Ordering::Release);
                    continue;
                }
                seen = sequence;
                changed = Instant::now();
                while cadence.announced.load(Ordering::Acquire) && !output.is_closed() {
                    std::thread::park_timeout(PARKED);
                }
                std::thread::sleep(gap.saturating_sub(changed.elapsed()));
            }
        });
        std::future::pending::<()>().await;
    })
}

/// How long the watcher sleeps before looking again, `since` the last new
/// frame: until a quarter gap before the next is due at `interval` (zero
/// when unknown), then `fine` steps until a gap past it, and a gap at a time
/// while it is late or the camera slow. A burst after a lull waits a gap at
/// most.
fn poll_after(since: Duration, interval: Duration, gap: Duration, fine: Duration) -> Duration {
    let due = interval.saturating_sub(since);
    let wait = if due > gap / 4 {
        (due - gap / 4).min(gap)
    } else if since < interval.max(gap) + gap {
        fine
    } else {
        gap
    };
    wait.max(fine)
}

/// Ask whether main window `id` is out of sight. On macOS that is hidden,
/// minimized, covered or on another Space, for winit draws such a window
/// all the same; elsewhere minimized, which X11 draws too, while Wayland and
/// Windows stop drawing on their own.
pub(super) fn out_of_sight(id: window::Id) -> Task<Message> {
    #[cfg(target_os = "macos")]
    return window::run(id, occluded).map(Message::Hidden);
    #[cfg(not(target_os = "macos"))]
    window::is_minimized(id).map(Message::Hidden)
}

/// Whether no part of `window` can be seen, as AppKit tracks it.
#[cfg(target_os = "macos")]
fn occluded(window: &dyn iced::window::Window) -> Option<bool> {
    use iced::window::raw_window_handle::RawWindowHandle;
    use objc2_app_kit::{NSView, NSWindowOcclusionState};
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: iced runs this on the main thread with the window alive, and the
    // handle's view is that window's content view.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let state = view.window()?.occlusionState();
    Some(!state.contains(NSWindowOcclusionState::Visible))
}

/// The main window's draws, as `observe` records them on the UI thread.
#[derive(Default)]
pub(super) struct Draws(Mutex<DrawLog>);

#[derive(Default)]
struct DrawLog {
    /// When the main window last drew.
    at: Option<Instant>,
    /// How many times it drew.
    count: u64,
    /// The workbench stopped taking frames because nothing drew.
    paused: bool,
    /// The frame each camera's view last drew, and how many new frames it
    /// drew since the workbench last counted, by camera ID.
    presented: HashMap<String, (u64, u32)>,
}

impl Draws {
    fn log(&self) -> std::sync::MutexGuard<'_, DrawLog> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The window drew at `at`, showing `frames` (camera ID, frame ID).
    /// Whether that ends a pause, which the workbench must hear about.
    pub(super) fn drew(&self, at: Instant, frames: &[(String, u64)]) -> bool {
        let mut log = self.log();
        log.at = Some(at);
        log.count += 1;
        for (camera, frame) in frames {
            match log.presented.get_mut(camera) {
                Some((last, _)) if last == frame => {}
                Some((last, count)) => {
                    *last = *frame;
                    *count += 1;
                }
                None => {
                    log.presented.insert(camera.clone(), (*frame, 1));
                }
            }
        }
        std::mem::take(&mut log.paused)
    }

    /// When the main window last drew, and how many times it has.
    pub(super) fn last(&self) -> (Option<Instant>, u64) {
        let log = self.log();
        (log.at, log.count)
    }

    /// Frame work stopped while nothing drew; the next draw resumes it.
    pub(super) fn pause(&self) {
        self.log().paused = true;
    }

    /// New frames each camera's view drew since the last call, by camera.
    pub(super) fn take_presented(&self) -> HashMap<String, u32> {
        let mut log = self.log();
        log.presented
            .iter_mut()
            .map(|(camera, (_, count))| (camera.clone(), std::mem::take(count)))
            .collect()
    }

    /// Forget cameras no longer shown, so one shown again counts afresh.
    pub(super) fn retain(&self, shown: impl Fn(&str) -> bool) {
        self.log().presented.retain(|camera, _| shown(camera));
    }
}

/// Wraps the main window's content to record its draws in `draws`: the
/// draw itself, and `frames`, the (camera ID, frame ID) pairs this view
/// shows. Publishes `Redrawn` when a draw ends a pause.
pub(super) fn observe<'a>(
    content: Element<'a, Message>,
    draws: Arc<Draws>,
    frames: Vec<(String, u64)>,
) -> Element<'a, Message> {
    Element::new(Observer {
        content,
        draws,
        frames,
    })
}

struct Observer<'a> {
    content: Element<'a, Message>,
    draws: Arc<Draws>,
    frames: Vec<(String, u64)>,
}

impl Widget<Message, Theme, iced::Renderer> for Observer<'_> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }
    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }
    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
        // iced sends this to the window's widgets just before it draws them.
        if let Event::Window(window::Event::RedrawRequested(at)) = event
            && self.draws.drew(*at, &self.frames)
        {
            shell.publish(Message::Redrawn);
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}

/// The frame path's side of the draw log.
impl Workbench {
    /// Whether frame work waits: the main window is out of sight, or the
    /// view changed `UNDRAWN` ago and it has not drawn since. Screenshots
    /// render a hidden window that never draws, so they never wait.
    pub(super) fn frames_paused(&self) -> bool {
        self.screenshot.is_none()
            && (self.hidden
                || self
                    .undrawn
                    .is_some_and(|since| self.now.saturating_duration_since(since) >= UNDRAWN))
    }

    /// Note that this update changes the view, which iced draws next,
    /// unless an earlier undrawn change is still waiting.
    pub(super) fn note_undrawn(&mut self) {
        let (drawn, _) = self.draws.last();
        if self
            .undrawn
            .is_some_and(|since| drawn.is_some_and(|drawn| drawn >= since))
        {
            self.undrawn = None;
        }
        self.undrawn.get_or_insert(self.now);
    }

    /// The (camera ID, frame ID) pairs the main view shows now.
    pub(super) fn frames_shown(&self) -> Vec<(String, u64)> {
        if self.overview() {
            self.previews
                .iter()
                .filter(|(_, preview)| preview.shown.is_some())
                .filter_map(|(id, preview)| Some((id.clone(), preview.frame_id?)))
                .collect()
        } else {
            self.shown
                .as_ref()
                .and(self.snapshot.active_camera.clone())
                .zip(self.frame_id)
                .into_iter()
                .collect()
        }
    }

    /// The cameras whose frames the main view shows.
    pub(super) fn cameras_shown(&self) -> Vec<&str> {
        if self.overview() {
            self.snapshot
                .cameras
                .iter()
                .map(|camera| camera.info.id.as_str())
                .collect()
        } else {
            self.snapshot.active_camera.as_deref().into_iter().collect()
        }
    }

    /// The time between frames of the fastest shown camera, once its
    /// worker has timed it.
    pub(super) fn shown_interval(&self) -> Option<Duration> {
        let shown = self.cameras_shown();
        self.snapshot
            .cameras
            .iter()
            .filter(|camera| camera.streaming && shown.contains(&camera.info.id.as_str()))
            .map(|camera| camera.fps)
            .filter(|fps| fps.is_finite() && *fps > 0.0)
            .reduce(f64::max)
            .map(|fps| Duration::from_secs_f64(1.0 / fps.min(1000.0)))
    }

    /// Whether any shown camera streams, so frames may come.
    pub(super) fn shown_streaming(&self) -> bool {
        let shown = self.cameras_shown();
        self.snapshot
            .cameras
            .iter()
            .any(|camera| camera.streaming && shown.contains(&camera.info.id.as_str()))
    }

    /// Tell the frame watcher what the view now shows and how fast.
    pub(super) fn follow_frames(&self) {
        self.cadence.follow(
            self.frame_gap(),
            self.shown_interval(),
            performance::efficient(),
            self.cameras_shown(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::futures::{FutureExt, StreamExt};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn watch() -> FrameWatch {
        let cadence = Arc::new(Cadence::default());
        cadence.follow(ms(16), None, false, vec!["sim:0"]);
        FrameWatch {
            handle: SessionHandle::new(),
            cadence,
        }
    }

    #[test]
    fn frame_watcher_stays_quiet_without_new_frames() {
        let watch = watch();
        let mut frames = Box::pin(watch_frames(&watch));
        assert!(frames.next().now_or_never().is_none());
        std::thread::sleep(FRAME_POLL.1 * 10);
        assert!(frames.next().now_or_never().is_none());
    }

    #[test]
    fn polling_backs_off_between_frames_and_looks_closely_when_one_is_due() {
        let (gap, fine) = (ms(12), ms(2));
        // A 30 fps camera on a 60 Hz display: sleep until just before the
        // next frame, then look finely.
        let interval = ms(33);
        assert_eq!(poll_after(ms(12), interval, gap, fine), ms(12));
        assert_eq!(poll_after(ms(24), interval, gap, fine), ms(6));
        assert_eq!(poll_after(ms(31), interval, gap, fine), fine);
        assert_eq!(poll_after(ms(40), interval, gap, fine), fine);
        // A slow camera never waits more than a gap at a time, so a burst
        // shows within one.
        assert_eq!(poll_after(ms(12), ms(1000), gap, fine), gap);
        assert_eq!(poll_after(ms(900), ms(1000), gap, fine), gap);
        // Late, stalled or of unknown pace: a gap at a time, not 500 looks
        // a second.
        assert_eq!(poll_after(ms(1100), ms(1000), gap, fine), gap);
        assert_eq!(poll_after(ms(5), Duration::ZERO, gap, fine), fine);
        assert_eq!(poll_after(ms(30), Duration::ZERO, gap, fine), gap);
        // A camera faster than the display is always due.
        assert_eq!(poll_after(ms(12), ms(8), gap, fine), fine);
    }

    #[test]
    fn efficiency_mode_looks_less_often() {
        let cadence = Cadence::default();
        cadence.follow(ms(16), None, false, Vec::new());
        assert_eq!(cadence.timing().1, ms(2));
        cadence.follow(Duration::from_micros(4_167), None, false, Vec::new());
        assert_eq!(cadence.timing().1, ms(1));
        cadence.follow(ms(66), Some(ms(33)), true, Vec::new());
        assert_eq!(cadence.timing(), (ms(66), ms(8), ms(33)));
        cadence.follow(ms(16), None, true, Vec::new());
        assert_eq!(cadence.timing().1, ms(4));
    }

    #[test]
    fn the_watcher_announces_nothing_more_until_the_frame_is_taken() {
        let cadence = Cadence::default();
        cadence.announced.store(true, Ordering::Release);
        let waiting = Arc::new(cadence);
        let parked = waiting.clone();
        let watcher = std::thread::spawn(move || {
            *parked.watcher.lock().unwrap() = Some(std::thread::current());
            let started = Instant::now();
            while parked.announced.load(Ordering::Acquire) {
                std::thread::park_timeout(PARKED);
            }
            started.elapsed()
        });
        std::thread::sleep(ms(30));
        assert!(!watcher.is_finished(), "parked while the frame waits");
        while waiting.watcher.lock().unwrap().is_none() {
            std::thread::yield_now();
        }
        waiting.taken();
        let waited = watcher.join().unwrap();
        assert!(waited < PARKED, "woken at once, after {waited:?}");
    }

    #[test]
    fn the_watcher_follows_the_cameras_shown() {
        let cadence = Cadence::default();
        cadence.follow(ms(16), None, false, vec!["sim:0", "sim:1"]);
        assert_eq!(*cadence.shown.lock().unwrap(), ["sim:0", "sim:1"]);
        cadence.follow(ms(16), None, false, vec!["sim:1"]);
        assert_eq!(*cadence.shown.lock().unwrap(), ["sim:1"]);
        let handle = SessionHandle::new();
        assert_eq!(cadence.sequence(&handle), 0, "no feed, no frames");
    }

    #[test]
    fn draws_count_each_new_frame_once_and_end_a_pause() {
        let draws = Draws::default();
        let start = Instant::now();
        let shown = |frame| vec![("sim:0".to_owned(), frame)];
        assert!(!draws.drew(start, &shown(1)));
        // Redraws of the same frame, for a hover or an animation, are not
        // new frames.
        assert!(!draws.drew(start + ms(16), &shown(1)));
        assert!(!draws.drew(start + ms(33), &shown(2)));
        assert_eq!(draws.take_presented()["sim:0"], 2);
        assert_eq!(draws.take_presented()["sim:0"], 0, "taken");
        assert_eq!(draws.last(), (Some(start + ms(33)), 3));
        draws.pause();
        assert!(draws.drew(start + ms(500), &shown(9)), "a draw resumes");
        assert!(!draws.drew(start + ms(516), &shown(9)), "once");
        draws.retain(|camera| camera != "sim:0");
        assert!(draws.take_presented().is_empty());
    }

    #[test]
    fn frame_work_pauses_while_the_window_does_not_draw() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let start = bench.now;
        bench.note_undrawn();
        bench.now = start + UNDRAWN / 2;
        assert!(!bench.frames_paused(), "a draw is on its way");
        bench.now = start + UNDRAWN;
        assert!(bench.frames_paused(), "nothing drew");
        // It draws again: the change is drawn, and the next one counts anew.
        bench.draws.drew(start + UNDRAWN + ms(1), &[]);
        bench.now = start + UNDRAWN + ms(2);
        bench.note_undrawn();
        assert!(!bench.frames_paused());
        bench.now += UNDRAWN / 2;
        bench.draws.drew(bench.now, &[]);
        bench.now += UNDRAWN;
        bench.note_undrawn();
        assert!(!bench.frames_paused(), "drawn since");
        // Out of sight, it waits however recently it drew.
        bench.hidden = true;
        assert!(bench.frames_paused());
    }
}
