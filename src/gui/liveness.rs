//! Liveness: whether each streaming camera's frames keep coming, and whether
//! it lost any lately. Kept by `tick()` and frame arrivals, never by views,
//! which only read it.
use super::*;
use crate::session::CameraSnapshot;

/// The shortest silence that counts as stalled, however fast the camera runs.
const STALL_FLOOR: Duration = Duration::from_secs(2);
/// The shortest silence that counts as stalled while the camera's pace is
/// not known yet, early in a stream.
const STALL_START: Duration = Duration::from_secs(5);
/// The worker times its rate over windows of a second or more, so readings
/// in a stream's first second can describe the stream before.
const RATE_SETTLE: Duration = Duration::from_secs(1);
/// How long a stream's chart says its frame rate is on its way; past it,
/// with no rate yet, the chart's slot is plainly empty.
const RATE_WAIT: Duration = STALL_START;
/// How long after the last lost frame a loss still counts as recent.
pub(super) const LOSS_RECENT: Duration = Duration::from_secs(10);

/// Frames a camera lost at the camera or on the way: the transport's count
/// where it keeps statistics, otherwise the worker's. Frames this window
/// skipped because a newer one was already waiting are not losses.
pub(super) fn camera_loss(camera: &CameraSnapshot) -> u64 {
    camera.transport.as_ref().map_or(
        camera.dropped.saturating_sub(camera.ring_dropped),
        transport_loss,
    )
}

/// What a camera's status dot says. The camera list, the tiles, the
/// inspector and the title pill all take it from here, so one camera reads
/// the same everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CameraState {
    /// Not connected.
    Offline,
    /// Not connected: the last connect to it failed.
    Failed,
    /// Connected, not streaming.
    Idle,
    /// Streaming, with frames coming.
    Live,
    /// Streaming, but no new frame for longer than its pace allows.
    Stalled,
}

impl CameraState {
    /// The dot's color in `p`: gray or red rings for cameras not connected;
    /// accent while idle, `live` while frames come, `warn` once they stop.
    pub(super) fn color(self, p: &Palette) -> Color {
        match self {
            CameraState::Offline => p.tertiary,
            CameraState::Failed => p.danger,
            CameraState::Idle => p.accent,
            CameraState::Live => p.live,
            CameraState::Stalled => p.warn,
        }
    }

    /// Whether the dot is a ring: the camera is not connected.
    pub(super) fn hollow(self) -> bool {
        matches!(self, CameraState::Offline | CameraState::Failed)
    }

    /// The state in a few words, for tooltips: "waiting for frames".
    pub(super) fn about(self) -> &'static str {
        match self {
            CameraState::Offline => "not connected",
            CameraState::Failed => "could not connect",
            CameraState::Idle => "connected",
            CameraState::Live => "streaming",
            CameraState::Stalled => "waiting for frames",
        }
    }
}

/// What a camera's features say about how often frames should come.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Pace {
    /// The exposure time, when the camera reports one.
    exposure: Option<Duration>,
    /// The camera waits for triggers, so silence is expected.
    triggered: bool,
}

impl Pace {
    fn of(features: &[FeatureInfo]) -> Self {
        let mut pace = Pace::default();
        for feature in features {
            match feature.name.as_str() {
                "ExposureTime" | "ExposureTimeAbs" if pace.exposure.is_none() => {
                    let scale = match feature.unit.as_deref() {
                        Some("s") => 1.0,
                        Some("ms") => 1e-3,
                        // GenICam's standard unit.
                        _ => 1e-6,
                    };
                    pace.exposure = feature
                        .value
                        .as_ref()
                        .and_then(serde_json::Value::as_f64)
                        .map(|value| value * scale)
                        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                        .map(|seconds| Duration::from_secs_f64(seconds.min(3600.0)));
                }
                "TriggerMode" => {
                    pace.triggered =
                        feature.value.as_ref().and_then(serde_json::Value::as_str) == Some("On");
                }
                _ => {}
            }
        }
        pace
    }
}

/// One streaming camera's frame and loss counters, and when each last rose.
#[derive(Clone, Debug)]
pub(super) struct Liveness {
    /// The frame counter at the last fresh report.
    pub(super) frames: u64,
    /// When a new frame was last counted or reached the screen.
    pub(super) frame_at: Instant,
    /// Frames lost since this stream started.
    pub(super) lost: u64,
    /// When `lost` last rose.
    pub(super) lost_at: Option<Instant>,
    /// The loss counter when the stream started: it counts per connection.
    lost_base: u64,
    started: Instant,
    /// When the frame counter last rose, and whether it rose since the start.
    counted_at: Instant,
    counted: bool,
    /// Frame interval from the latest rate reading above zero.
    rate: Option<Duration>,
    /// The last interval between two rises of the frame counter.
    gap: Option<Duration>,
    pace: Pace,
    /// The last report was a busy worker's repeat, so liveness is unknown.
    busy: bool,
}

impl Liveness {
    /// Counters as a stream starts, taken as the baseline: they restart only
    /// when the camera connects, so losses already counted are old.
    fn new(camera: &CameraSnapshot, now: Instant) -> Self {
        Self {
            frames: camera.frames,
            frame_at: now,
            lost: 0,
            lost_at: None,
            lost_base: camera_loss(camera),
            started: now,
            counted_at: now,
            counted: false,
            rate: None,
            gap: None,
            pace: Pace::of(&camera.features),
            busy: camera.stale,
        }
    }

    fn observe(&mut self, camera: &CameraSnapshot, now: Instant) {
        let was_busy = std::mem::replace(&mut self.busy, camera.stale);
        if camera.stale {
            return;
        }
        self.pace = Pace::of(&camera.features);
        // A reading of zero only says a window passed without a frame.
        if camera.fps > 0.0 && now.saturating_duration_since(self.started) >= RATE_SETTLE {
            self.rate = Some(Duration::from_secs_f64((1.0 / camera.fps).min(3600.0)));
        }
        if camera.frames != self.frames {
            // A gap spanning a busy spell measures the spell, not the camera.
            if self.counted && !was_busy {
                self.gap = Some(now.saturating_duration_since(self.counted_at));
            }
            self.counted = true;
            self.counted_at = now;
            self.frames = camera.frames;
            self.frame_at = now;
        }
        let total = camera_loss(camera);
        self.lost_base = self.lost_base.min(total);
        let lost = total - self.lost_base;
        if lost > self.lost {
            self.lost_at = Some(now);
        }
        self.lost = lost;
    }

    /// Whether a frame was counted or reached the screen after `at`, in
    /// this stream: a stream just started has shown none yet.
    pub(super) fn framed_since(&self, at: Instant) -> bool {
        self.frame_at > at && self.frame_at > self.started
    }

    /// Read as silent since `at` with fresh counters, for a screenshot
    /// scene that holds the camera still.
    pub(super) fn silent_since(&mut self, at: Instant) {
        self.frame_at = at;
        self.busy = false;
    }

    /// How long without a new frame counts as stalled: three frame
    /// intervals at the slowest pace seen lately and twice the exposure,
    /// never under `STALL_FLOOR`; `STALL_START` until the pace is known.
    fn limit(&self) -> Duration {
        let paced = match self.rate.max(self.gap) {
            Some(interval) => STALL_FLOOR.max(interval * 3),
            None => STALL_START,
        };
        paced.max(
            self.pace
                .exposure
                .map_or(Duration::ZERO, |exposure| exposure * 2),
        )
    }
}

/// A new frame from `id` reached the screen at `now`, unless a screenshot
/// scene holds that camera still. Takes fields rather than the workbench, so
/// frame updates can call it while they hold other fields.
pub(super) fn seen(
    liveness: &mut HashMap<String, Liveness>,
    frozen: Option<&String>,
    id: &str,
    now: Instant,
) {
    if frozen.is_some_and(|frozen| frozen == id) {
        return;
    }
    if let Some(live) = liveness.get_mut(id) {
        live.frame_at = now;
    }
}

impl Workbench {
    /// Update each streaming camera's liveness from the snapshot's counters,
    /// forgetting cameras that stopped; from `tick()`.
    pub(super) fn observe_liveness(&mut self) {
        let now = self.now;
        let snapshot = &self.snapshot;
        self.liveness.retain(|id, _| {
            snapshot
                .cameras
                .iter()
                .any(|camera| camera.streaming && &camera.info.id == id)
        });
        for camera in snapshot.cameras.iter().filter(|camera| camera.streaming) {
            match self.liveness.get_mut(&camera.info.id) {
                // A screenshot scene holds a stalled camera still.
                Some(_) if self.scene.frozen.as_ref() == Some(&camera.info.id) => {}
                Some(live) => live.observe(camera, now),
                None => {
                    self.liveness
                        .insert(camera.info.id.clone(), Liveness::new(camera, now));
                }
            }
        }
    }

    /// A new frame from `id` reached the screen: it is alive even while its
    /// worker is too busy to report counters.
    #[cfg(test)]
    pub(super) fn frame_seen(&mut self, id: &str) {
        seen(&mut self.liveness, self.scene.frozen.as_ref(), id, self.now);
    }

    /// How long a streaming camera has gone without a new frame, once that
    /// passes its limit (see `Liveness::limit`). `None` while that cannot be
    /// told: its worker is busy with a command from here or from another
    /// client, so its counters are old, or it waits for triggers.
    pub(super) fn stalled(&self, id: &str) -> Option<Duration> {
        let live = self.liveness.get(id)?;
        if live.busy
            || live.pace.triggered
            || self
                .pending
                .iter()
                .any(|pending| pending.camera.as_deref() == Some(id))
        {
            return None;
        }
        let silent = self.now.saturating_duration_since(live.frame_at);
        (silent > live.limit()).then_some(silent)
    }

    /// Whether streaming camera `id`'s frame rate may still be on its way:
    /// its stream started within `RATE_WAIT` and its frames have not
    /// stopped. Its chart says so until the rate shows.
    pub(super) fn rate_coming(&self, id: &str) -> bool {
        self.liveness
            .get(id)
            .is_some_and(|live| self.now.saturating_duration_since(live.started) < RATE_WAIT)
            && self.stalled(id).is_none()
    }

    /// The state of a connected camera, as its status dot shows it.
    pub(super) fn camera_state(&self, camera: &CameraSnapshot) -> CameraState {
        if !camera.streaming {
            CameraState::Idle
        } else if self.stalled(&camera.info.id).is_some() {
            CameraState::Stalled
        } else {
            CameraState::Live
        }
    }

    /// Whether a streaming camera lost frames within `LOSS_RECENT`, counting
    /// only losses since its stream started (see `camera_loss`).
    pub(super) fn recent_loss(&self, id: &str) -> bool {
        self.liveness
            .get(id)
            .and_then(|live| live.lost_at)
            .is_some_and(|at| self.now.saturating_duration_since(at) < LOSS_RECENT)
    }
}

/// A streaming simulated camera with these counters, for tests.
#[cfg(test)]
pub(super) fn streaming_camera(frames: u64, dropped: u64, fps: f64) -> CameraSnapshot {
    CameraSnapshot {
        info: crate::transport::simulator::info(),
        features: Vec::new(),
        streaming: true,
        frames,
        dropped,
        fps,
        last_error: None,
        worker_pid: 0,
        forwarding: None,
        jobs: Vec::new(),
        auto: None,
        transport: None,
        ring_dropped: 0,
        stale: false,
        error_logged: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    /// Report `camera` at `at` past the start.
    fn report(bench: &mut Workbench, start: Instant, at: Duration, camera: CameraSnapshot) {
        bench.now = start + at;
        bench.snapshot.cameras = vec![camera];
        bench.observe_liveness();
    }

    fn feature(name: &str, value: serde_json::Value) -> FeatureInfo {
        FeatureInfo {
            name: name.into(),
            display_name: name.into(),
            kind: "Float".into(),
            value: Some(value),
            writable: true,
            description: String::new(),
            unit: None,
            min: None,
            max: None,
            inc: None,
            choices: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn stall_limit_follows_the_slowest_recent_pace() {
        let start = Instant::now();
        let mut camera = streaming_camera(10, 0, 30.0);
        let mut live = Liveness::new(&camera, start);
        assert_eq!(live.limit(), STALL_START, "pace not known yet");
        camera.frames = 40;
        live.observe(&camera, start + ms(1000));
        assert_eq!(live.limit(), STALL_FLOOR);
        live.rate = Some(ms(2000));
        assert_eq!(live.limit(), ms(6000));
        live.gap = Some(ms(4000));
        assert_eq!(live.limit(), ms(12_000));
        live.pace.exposure = Some(ms(8000));
        assert_eq!(live.limit(), ms(16_000));
    }

    #[test]
    fn slow_streams_do_not_flicker_stalled() {
        // 0.5 fps: the worker's one-second windows alternate one frame and none.
        let mut bench = bench();
        let start = bench.now;
        let id = "sim:0";
        report(&mut bench, start, ms(0), streaming_camera(0, 0, 0.0));
        let mut frames = 0;
        for step in 1..=80u64 {
            let at = ms(step * 250);
            if step % 8 == 0 {
                frames += 1;
            }
            let fps = if (step / 4) % 2 == 0 { 1.0 } else { 0.0 };
            report(&mut bench, start, at, streaming_camera(frames, 0, fps));
            assert_eq!(bench.stalled(id), None, "at {at:?}");
        }
    }

    #[test]
    fn fast_streams_that_stop_read_as_stalled() {
        let mut bench = bench();
        let start = bench.now;
        let id = "sim:0";
        for step in 0..=8u64 {
            let camera = streaming_camera(step * 8, 0, if step >= 4 { 30.0 } else { 0.0 });
            report(&mut bench, start, ms(step * 250), camera);
        }
        let last = ms(8 * 250);
        report(
            &mut bench,
            start,
            last + ms(1900),
            streaming_camera(64, 0, 0.0),
        );
        assert_eq!(
            bench.stalled(id),
            None,
            "within two seconds of the last frame"
        );
        report(
            &mut bench,
            start,
            last + ms(2100),
            streaming_camera(64, 0, 0.0),
        );
        assert_eq!(bench.stalled(id), Some(ms(2100)));
        bench.frame_seen(id);
        assert_eq!(bench.stalled(id), None, "a frame reached the screen");
    }

    #[test]
    fn busy_workers_and_triggered_cameras_are_not_stalled() {
        let mut bench = bench();
        let start = bench.now;
        let id = "sim:0";
        for step in 0..=4u64 {
            report(
                &mut bench,
                start,
                ms(step * 250),
                streaming_camera(step * 8, 0, 30.0),
            );
        }
        // Another client's long capture holds the worker: its report repeats.
        let mut busy = streaming_camera(32, 0, 30.0);
        busy.stale = true;
        report(&mut bench, start, ms(6000), busy.clone());
        assert_eq!(bench.stalled(id), None, "unknown while busy");
        // Fresh again and moving: alive, and the busy spell is no frame gap.
        report(&mut bench, start, ms(6250), streaming_camera(200, 0, 30.0));
        assert_eq!(bench.stalled(id), None);
        assert_eq!(bench.liveness[id].limit(), STALL_FLOOR);
        // Fresh again but still: stalled since the last count.
        report(&mut bench, start, ms(9000), busy);
        report(&mut bench, start, ms(9250), streaming_camera(200, 0, 30.0));
        assert_eq!(bench.stalled(id), Some(ms(3000)));

        let mut triggered = streaming_camera(200, 0, 0.0);
        triggered.features = vec![feature("TriggerMode", json!("On"))];
        report(&mut bench, start, ms(20_000), triggered);
        assert_eq!(bench.stalled(id), None, "waits for triggers");
    }

    #[test]
    fn every_view_reads_a_camera_state_the_same() {
        let mut bench = bench();
        let start = bench.now;
        let mut idle = streaming_camera(0, 0, 0.0);
        idle.streaming = false;
        assert_eq!(bench.camera_state(&idle), CameraState::Idle);
        for step in 0..=4u64 {
            report(
                &mut bench,
                start,
                ms(step * 250),
                streaming_camera(step * 8, 0, 30.0),
            );
        }
        let camera = bench.snapshot.cameras[0].clone();
        assert_eq!(bench.camera_state(&camera), CameraState::Live);
        report(&mut bench, start, ms(4000), camera.clone());
        assert_eq!(bench.camera_state(&camera), CameraState::Stalled);
        for p in [&style::LIGHT, &style::DARK, style::STAGE] {
            assert_eq!(CameraState::Idle.color(p), p.accent);
            assert_eq!(CameraState::Live.color(p), p.live);
            assert_eq!(CameraState::Stalled.color(p), p.warn);
            assert_eq!(CameraState::Failed.color(p), p.danger);
        }
        assert!(CameraState::Offline.hollow() && !CameraState::Idle.hollow());
    }

    #[test]
    fn a_frame_rate_is_only_coming_for_a_while() {
        let mut bench = bench();
        let start = bench.now;
        assert!(!bench.rate_coming("sim:0"), "not streaming");
        report(&mut bench, start, ms(0), streaming_camera(0, 0, 0.0));
        assert!(bench.rate_coming("sim:0"));
        // No frame ever comes: the chart stops promising one.
        report(&mut bench, start, RATE_WAIT, streaming_camera(0, 0, 0.0));
        assert!(!bench.rate_coming("sim:0"));
    }

    #[test]
    fn exposure_and_its_unit_set_the_pace() {
        let pace = Pace::of(&[
            feature("Gain", json!(2.0)),
            feature("ExposureTime", json!(1_500_000.0)),
        ]);
        assert_eq!(pace.exposure, Some(ms(1500)));
        assert!(!pace.triggered);
        let mut exposure = feature("ExposureTimeAbs", json!(40.0));
        exposure.unit = Some("ms".into());
        assert_eq!(Pace::of(&[exposure]).exposure, Some(ms(40)));
        assert_eq!(
            Pace::of(&[feature("ExposureTime", json!("x"))]),
            Pace::default()
        );
    }

    #[test]
    fn only_new_losses_at_the_camera_are_recent() {
        let mut bench = bench();
        let start = bench.now;
        let id = "sim:0";
        // Losses from an earlier stream on this connection are not recent.
        report(&mut bench, start, ms(0), streaming_camera(10, 5, 30.0));
        assert!(!bench.recent_loss(id));
        assert_eq!(bench.liveness[id].lost, 0);
        // Frames this window skipped are not losses.
        let mut skipped = streaming_camera(40, 25, 30.0);
        skipped.ring_dropped = 20;
        report(&mut bench, start, ms(1000), skipped);
        assert!(!bench.recent_loss(id));
        report(&mut bench, start, ms(2000), streaming_camera(70, 7, 30.0));
        assert!(bench.recent_loss(id));
        assert_eq!(bench.liveness[id].lost, 2);
        report(
            &mut bench,
            start,
            ms(2000) + LOSS_RECENT,
            streaming_camera(400, 7, 30.0),
        );
        assert!(!bench.recent_loss(id), "no new loss for a while");

        bench.snapshot.cameras[0].streaming = false;
        bench.observe_liveness();
        assert!(bench.liveness.is_empty(), "stopping forgets the stream");
    }
}
