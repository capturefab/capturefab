//! Quiet rediscovery: while the welcome screen waits for a choice in a
//! focused window, discovery runs again now and then, so cameras that boot
//! or are plugged in later arrive by themselves, with the cards' usual
//! grow-in. A quiet round shows no placeholder, spinner or notice, unless
//! what discovery warns about changed.
use super::*;

/// The wait after a round before the next, while what discovery finds keeps
/// changing.
pub(super) const QUICK: Duration = Duration::from_secs(8);
/// The wait once `SETTLED` rounds in a row found the same cameras.
pub(super) const SLOW: Duration = Duration::from_secs(30);
/// Rounds in a row that found the same cameras before rounds slow down.
const SETTLED: u8 = 3;

/// When the next quiet round is due, and what the last ones found.
#[derive(Default)]
pub(super) struct Rediscover {
    /// When the next quiet round may start; `None` until a discovery has
    /// finished, so the one at startup goes first.
    next: Option<Instant>,
    /// Rounds in a row that found the same cameras as the one before.
    unchanged: u8,
    /// The IDs the last round found, sorted, to tell whether it changed.
    ids: Option<Vec<String>>,
    /// What the last round found, to hold any one quiet round misses.
    last: Vec<CameraInfo>,
    /// Cameras the last quiet round missed but the round before found:
    /// still listed for one round, so a single lost reply does not take a
    /// card away and bring it back.
    pub(super) held: Vec<CameraInfo>,
    /// Whether the window lost focus since the last round was scheduled.
    away: bool,
}

impl Rediscover {
    fn interval(&self) -> Duration {
        if self.unchanged >= SETTLED {
            SLOW
        } else {
            QUICK
        }
    }

    /// A discovery finished at `now`, finding `devices`; `quiet` when it was
    /// a quiet round. One asked for lists exactly what it found.
    pub(super) fn finished(&mut self, devices: &[CameraInfo], quiet: bool, now: Instant) {
        let mut ids: Vec<String> = devices.iter().map(|device| device.id.clone()).collect();
        ids.sort();
        if quiet && self.ids.as_ref() == Some(&ids) {
            self.unchanged = self.unchanged.saturating_add(1);
        } else {
            self.unchanged = 0;
        }
        self.held = if quiet {
            self.last
                .iter()
                .filter(|seen| !devices.iter().any(|device| device.id == seen.id))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        self.last = devices.to_vec();
        self.ids = Some(ids);
        self.next = Some(now + self.interval());
    }

    /// A discovery failed at `now`: try again after the usual wait, keeping
    /// what is listed.
    pub(super) fn failed(&mut self, now: Instant) {
        self.next = Some(now + self.interval());
    }

    /// Whether a quiet round may start at `now`.
    fn due(&self, now: Instant) -> bool {
        self.next.is_some_and(|next| now >= next)
    }
}

impl Workbench {
    /// What discovery lists: the session's devices, and any the last quiet
    /// round missed once (`Rediscover::held`).
    pub(super) fn listed_devices(&self) -> impl Iterator<Item = &CameraInfo> + Clone {
        let devices = &self.snapshot.devices;
        devices.iter().chain(
            self.side
                .rediscover
                .held
                .iter()
                .filter(move |held| !devices.iter().any(|device| device.id == held.id)),
        )
    }

    /// Whether the welcome screen is waiting for a camera to be chosen in a
    /// focused window, with nothing on its way that a discovery would
    /// disturb or that would make one pointless.
    fn rediscovery_wanted(&self) -> bool {
        self.screenshot.is_none()
            && self.key_window.is_some()
            && self.shown.is_none()
            && self.snapshot.connected.is_none()
            && self.snapshot.cameras.is_empty()
            && !self.sheet_open()
            && !self.pending(Job::Discover)
            && !self.pending(Job::Connect)
            && !self.pending(Job::NetworkScan)
    }

    /// Start a quiet round when one is due; from `tick()`. Focus coming back
    /// brings the next round forward to `QUICK`.
    pub(super) fn tick_rediscover(&mut self) {
        let now = self.now;
        let focused = self.key_window.is_some();
        let rediscover = &mut self.side.rediscover;
        if !focused {
            rediscover.away = true;
        } else if std::mem::take(&mut rediscover.away) && rediscover.next.is_some() {
            rediscover.unchanged = 0;
            rediscover.next = Some(now + QUICK);
        }
        if !self.side.rediscover.due(now) || !self.rediscovery_wanted() {
            return;
        }
        match self.try_submit(None, Job::Discover, self.discover_command()) {
            Ok(pending) => pending.background = true,
            // A busy session is asked again after the usual wait.
            Err(_) => self.side.rediscover.failed(now),
        }
    }

    /// A discovery finished; from `result_side`.
    pub(super) fn result_rediscover(
        &mut self,
        pending: &Pending,
        result: &anyhow::Result<serde_json::Value>,
    ) {
        let now = self.now;
        match result {
            Ok(value) => {
                let devices: Vec<CameraInfo> =
                    serde_json::from_value(value["devices"].clone()).unwrap_or_default();
                self.side
                    .rediscover
                    .finished(&devices, pending.background, now);
            }
            Err(_) => self.side.rediscover.failed(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    fn device(id: &str) -> CameraInfo {
        crate::transport::simulator::info_named(id)
    }

    fn quiet_rounds(bench: &Workbench) -> usize {
        bench
            .pending
            .iter()
            .filter(|pending| pending.job == Job::Discover && pending.background)
            .count()
    }

    /// Finish the pending discovery with `devices`, as `poll` would.
    fn answer(bench: &mut Workbench, devices: &[&str]) {
        let index = bench
            .pending
            .iter()
            .position(|pending| pending.job == Job::Discover)
            .expect("a discovery");
        let pending = bench.pending.remove(index);
        let devices: Vec<CameraInfo> = devices.iter().map(|id| device(id)).collect();
        bench.snapshot.devices = devices.clone();
        bench.settle(&pending, &Ok(json!({"devices": devices, "warnings": []})));
        bench.tick_side();
    }

    /// The startup discovery, asked for and answered.
    fn started(devices: &[&str]) -> Workbench {
        let mut bench = bench();
        bench.pending.clear();
        bench.pending.push(Pending::unanswered(Job::Discover));
        answer(&mut bench, devices);
        bench.notice = None;
        bench
    }

    #[test]
    fn a_quiet_round_starts_only_while_the_welcome_waits_in_a_focused_window() {
        let mut bench = started(&[]);
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 0, "not due yet");
        bench.now += QUICK;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 1);
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 1, "one at a time");

        // Not while connecting, unfocused, or with a camera connected.
        let mut bench = started(&[]);
        bench.now += QUICK;
        bench.pending.push(Pending::unanswered(Job::Connect));
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 0, "connecting");
        bench.pending.clear();
        bench.key_window = None;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 0, "unfocused");
        bench.key_window = Some(bench.main_window);
        bench.snapshot.cameras = vec![liveness::streaming_camera(0, 0, 0.0)];
        bench.now += QUICK;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 0, "a camera is connected");
    }

    #[test]
    fn rounds_slow_down_while_nothing_changes_and_speed_up_when_it_does() {
        let mut bench = started(&["sim:0"]);
        for round in 0..SETTLED {
            bench.now += QUICK;
            bench.tick_rediscover();
            assert_eq!(quiet_rounds(&bench), 1, "round {round} after {QUICK:?}");
            answer(&mut bench, &["sim:0"]);
        }
        bench.now += QUICK;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 0, "settled: slower");
        bench.now += SLOW - QUICK;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 1, "after {SLOW:?}");
        answer(&mut bench, &["sim:0", "sim:1"]);
        bench.now += QUICK;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 1, "changed: quick again");
    }

    #[test]
    fn a_quiet_round_says_nothing_and_shows_no_search() {
        let mut bench = started(&["sim:0"]);
        bench.now += QUICK;
        bench.tick_rediscover();
        bench.now += Duration::from_secs(1);
        assert!(!bench.searching_shown(), "no placeholder");
        assert!(!bench.discovering_aloud(), "no spinner");
        answer(&mut bench, &["sim:0", "sim:1"]);
        assert!(bench.notice.is_none(), "no notice for a quiet round");

        // Asked for while a quiet round runs, the round answers aloud.
        bench.now += QUICK;
        bench.tick_rediscover();
        let _ = bench.handle_message(Message::Discover);
        assert_eq!(quiet_rounds(&bench), 0);
        assert!(bench.discovering_aloud());
        answer(&mut bench, &["sim:0", "sim:1"]);
        assert!(bench.notice.is_some(), "a search asked for reports");
    }

    #[test]
    fn a_quiet_round_keeps_the_idle_tick() {
        let mut bench = started(&["sim:0"]);
        let idle = bench.tick_interval();
        bench.now += QUICK;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 1);
        // The session lists the round's own discovery as running.
        bench.snapshot.activity = vec![crate::session::SessionActivity {
            id: 1,
            origin: crate::session::ActivityOrigin::Gui,
            actor: bench.actor.clone(),
            operation: "discover".into(),
            camera: None,
            feature: None,
            label: "Discover cameras".into(),
            state: crate::session::ActivityState::Running,
            submitted_at_ms: 0,
            started_at_ms: Some(0),
            finished_at_ms: None,
            detail: None,
            file_count: 0,
            files: Vec::new(),
            job: None,
        }];
        assert_eq!(
            bench.tick_interval(),
            idle,
            "no fast tick for a quiet round"
        );

        // Asked for aloud, the same round ticks fast for its spinner.
        let _ = bench.handle_message(Message::Discover);
        assert!(bench.tick_interval() < idle);
    }

    #[test]
    fn a_camera_one_quiet_round_misses_stays_listed_for_that_round() {
        let mut bench = started(&["sim:0", "sim:1"]);
        let start = bench.now;
        bench.now += QUICK;
        bench.tick_rediscover();
        answer(&mut bench, &["sim:0"]);
        let listed: Vec<&str> = bench.listed_devices().map(|d| d.id.as_str()).collect();
        assert_eq!(listed, ["sim:0", "sim:1"], "held for one round");
        assert!(
            bench.side.arrival("sim:1", bench.now) >= 1.0,
            "a held card does not arrive again"
        );
        bench.now += QUICK;
        bench.tick_rediscover();
        answer(&mut bench, &["sim:0"]);
        let listed: Vec<&str> = bench.listed_devices().map(|d| d.id.as_str()).collect();
        assert_eq!(listed, ["sim:0"], "missed twice: gone");

        // A new camera grows in.
        bench.now += QUICK;
        bench.tick_rediscover();
        answer(&mut bench, &["sim:0", "sim:2"]);
        assert_eq!(bench.side.arrival("sim:2", bench.now), 0.0);
        assert!(bench.now > start);

        // A search asked for lists exactly what it found.
        bench.pending.push(Pending::unanswered(Job::Discover));
        answer(&mut bench, &[]);
        assert_eq!(bench.listed_devices().count(), 0);
    }

    #[test]
    fn screenshots_never_rediscover() {
        let mut bench = started(&[]);
        bench.apply_scene(&["welcome"]);
        bench.screenshot = Some(scene::ScreenshotRequest {
            path: "shot.png".into(),
            cameras: 0,
            started: Instant::now(),
            streams_started: None,
            staged: false,
            requested: false,
            save: None,
            outcome: Default::default(),
            sequence: scene::ScreenshotSequence::parse(None, None).unwrap(),
            video: None,
        });
        bench.now += SLOW;
        bench.tick_rediscover();
        assert_eq!(quiet_rounds(&bench), 0);
    }
}
