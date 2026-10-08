//! Liveness: whether each streaming camera's frames keep coming, and whether
//! it lost any lately. Kept by `tick()` and frame arrivals, never by views,
//! which only read it.
use super::*;

/// The shortest silence that counts as stalled, however fast the camera ran.
const STALL_FLOOR: Duration = Duration::from_secs(1);
/// How long after the last lost frame a loss still counts as recent.
pub(super) const LOSS_RECENT: Duration = Duration::from_secs(10);

/// One streaming camera's frame and loss counters, and when each last rose.
#[derive(Clone, Copy, Debug)]
pub(super) struct Liveness {
    pub(super) frames: u64,
    pub(super) frame_at: Instant,
    pub(super) lost: u64,
    pub(super) lost_at: Option<Instant>,
    /// How long without a new frame counts as stalled at the camera's rate.
    limit: Duration,
}

impl Liveness {
    fn new(frames: u64, lost: u64, fps: f64, now: Instant) -> Self {
        Self {
            frames,
            frame_at: now,
            lost,
            // Counters restart with each stream, so losses already counted are new.
            lost_at: (lost > 0).then_some(now),
            limit: stall_limit(fps),
        }
    }

    fn observe(&mut self, frames: u64, lost: u64, fps: f64, now: Instant) {
        if frames != self.frames {
            self.frames = frames;
            self.frame_at = now;
        }
        if lost > self.lost {
            self.lost_at = Some(now);
        }
        self.lost = lost;
        self.limit = stall_limit(fps);
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

/// Three frame intervals at `fps`, and never less than `STALL_FLOOR`.
fn stall_limit(fps: f64) -> Duration {
    let intervals = if fps > 0.0 { 3.0 / fps } else { 0.0 };
    Duration::from_secs_f64(intervals.min(3600.0)).max(STALL_FLOOR)
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
            let lost = camera
                .transport
                .as_ref()
                .map_or(camera.dropped, transport_loss);
            match self.liveness.get_mut(&camera.info.id) {
                // A screenshot scene holds a stalled camera still.
                Some(_) if self.scene.frozen.as_ref() == Some(&camera.info.id) => {}
                Some(live) => live.observe(camera.frames, lost, camera.fps, now),
                None => {
                    self.liveness.insert(
                        camera.info.id.clone(),
                        Liveness::new(camera.frames, lost, camera.fps, now),
                    );
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

    /// How long a streaming camera has gone without a new frame, once that is
    /// longer than three frame intervals and a second. `None` while a
    /// command for it is pending, since a busy worker reports late.
    #[allow(dead_code)] // adopted by the chrome, stage and sidebar packages
    pub(super) fn stalled(&self, id: &str) -> Option<Duration> {
        let live = self.liveness.get(id)?;
        if self
            .pending
            .iter()
            .any(|pending| pending.camera.as_deref() == Some(id))
        {
            return None;
        }
        let silent = self.now.saturating_duration_since(live.frame_at);
        (silent > live.limit).then_some(silent)
    }

    /// Whether a streaming camera lost frames within `LOSS_RECENT`.
    #[allow(dead_code)] // adopted by the stage and sidebar packages
    pub(super) fn recent_loss(&self, id: &str) -> bool {
        self.liveness
            .get(id)
            .and_then(|live| live.lost_at)
            .is_some_and(|at| self.now.saturating_duration_since(at) < LOSS_RECENT)
    }
}

/// A streaming simulated camera with these counters, for tests.
#[cfg(test)]
pub(super) fn streaming_camera(
    frames: u64,
    dropped: u64,
    fps: f64,
) -> crate::session::CameraSnapshot {
    crate::session::CameraSnapshot {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_limit_follows_the_frame_rate() {
        assert_eq!(stall_limit(30.0), STALL_FLOOR);
        assert_eq!(stall_limit(0.0), STALL_FLOOR);
        assert_eq!(stall_limit(0.5), Duration::from_secs(6));
    }

    #[test]
    fn liveness_tracks_frames_and_recent_loss() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let start = bench.now;
        let id = "sim:0";
        let ms = Duration::from_millis;
        bench.snapshot.cameras = vec![streaming_camera(10, 0, 30.0)];
        bench.observe_liveness();
        assert_eq!(bench.stalled(id), None);
        assert!(!bench.recent_loss(id));

        bench.now = start + ms(800);
        bench.snapshot.cameras = vec![streaming_camera(34, 0, 30.0)];
        bench.observe_liveness();
        bench.now = start + ms(1500);
        bench.observe_liveness();
        assert_eq!(bench.stalled(id), None, "within a second of the last frame");
        bench.now = start + ms(1900);
        bench.observe_liveness();
        assert_eq!(bench.stalled(id), Some(ms(1100)));
        bench.frame_seen(id);
        assert_eq!(bench.stalled(id), None, "a frame reached the screen");

        bench.snapshot.cameras = vec![streaming_camera(60, 2, 30.0)];
        bench.observe_liveness();
        assert!(bench.recent_loss(id));
        bench.now += LOSS_RECENT;
        bench.snapshot.cameras = vec![streaming_camera(400, 2, 30.0)];
        bench.observe_liveness();
        assert!(!bench.recent_loss(id), "no new loss for a while");

        bench.snapshot.cameras[0].streaming = false;
        bench.observe_liveness();
        assert!(bench.liveness.is_empty(), "stopping forgets the stream");
    }
}
