//! Screenshots of named workbench states, for reviewing the interface:
//! `gui --screenshot` with CAPTUREFAB_SCREENSHOT_SCENE set to comma-separated
//! words, e.g. `help,light` or `saved,dark`.
use super::*;
use notice::{Level, Saved, command_notice};
use serde_json::json;

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
    /// The selected camera reports that it stopped streaming.
    stopped: bool,
    /// Turn on auto mode on every camera as the streams start.
    pub(super) auto: bool,
}

impl Scene {
    /// Make the snapshot say what the scene holds in place; from `tick()`
    /// after every refresh, so it never reverts.
    pub(super) fn patch(&self, snapshot: &mut SessionSnapshot) {
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
            }
            camera.streaming &= !self.stopped;
        }
        if self.forwarding.is_some() {
            snapshot.forwarding = self.forwarding.clone();
        }
        snapshot.streaming &= !self.stopped;
    }
}

/// An error long enough to test that notices stay on one line.
const LONG_ERROR: &str = "Native camera discovery: this development build has no embedded \
FFmpeg; rebuild with CAPTUREFAB_FFMPEG_BINARY pointing to a target-compatible static FFmpeg, \
or set CAPTUREFAB_FFMPEG to an absolute path";

impl Workbench {
    /// Put the workbench in a named state for a screenshot. Words:
    /// - layout: `help`, `capture`, `forward`, `features`, `destinations`,
    ///   `logs`, `image`, `focus`, `exposure`, `noscopes`, `inspector`,
    ///   `noinspector`, `sidebar`, `nosidebar`, `peek` (floating inspector
    ///   open; use a narrow size), `light`, `dark`
    /// - notices, which stay put: `notice` (done), `warning` (discovery
    ///   warnings), `error`, `notice-long` (an error too long for the bar)
    /// - activity: `searching` (with `welcome`: discovery never finishes),
    ///   `pending` (stopping the selected camera's stream never finishes),
    ///   `starting` (the selected camera stopped, and starting it never
    ///   finishes), `saved` (the selected camera just saved a capture),
    ///   `copied` (the session command was just copied), `stalled` (the
    ///   selected camera's frames stopped arriving), `auto` (every camera in
    ///   auto mode), `forwarding` (the selected camera forwards to a URL),
    ///   `recording` (the selected camera records to a file)
    /// - failures: `write-error` (setting the selected camera's exposure
    ///   time was refused) and `connect-failed` (connecting to a typed
    ///   address failed), each with its error notice; `editor` (the S3
    ///   bucket editor open, its save refused for a missing field)
    ///
    /// Other words go to each area's `scene_*` hook, first as the scene is
    /// set up and, if no area took them, again once the cameras stream.
    pub(super) fn apply_scene(&mut self, scene: &[&str]) {
        for word in scene {
            match *word {
                // Taken by `run_capture`, which then connects no cameras.
                "welcome" => {}
                "help" => self.help_open = true,
                "capture" => self.tab = Tab::Capture,
                "forward" => self.tab = Tab::Forward,
                "features" => self.tab = Tab::Features,
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
                "inspector" => self.inspector_open = true,
                "nosidebar" => self.sidebar_open = false,
                "sidebar" => self.sidebar_open = true,
                "peek" => self.inspector_peek = true,
                "light" => self.appearance = Appearance::Light,
                "dark" => self.appearance = Appearance::Dark,
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
                "searching" => self.hold(Job::Discover, None),
                "stalled" => self.scene.stalled = true,
                "auto" => self.scene.auto = true,
                "forwarding" => {
                    self.scene.forwarding =
                        Some("rtsp://operator:secret@192.168.1.20:8554/line-1".into())
                }
                "recording" => self.scene.forwarding = Some("recordings/line-1.mkv".into()),
                "connect-failed" => {
                    let target = "192.168.10.50";
                    self.address = target.into();
                    self.scene_settle(
                        Pending {
                            target: Some(target.into()),
                            ..Pending::unanswered(Job::Connect)
                        },
                        Err(anyhow::anyhow!(
                            "no GigE Vision camera answered at 192.168.10.50 within 5 s"
                        )),
                    );
                }
                "editor" => {
                    self.capture_to
                        .update(destinations::Message::OpenManager, &mut self.output);
                    self.capture_to
                        .update(destinations::Message::AddBucket, &mut self.output);
                    self.capture_to
                        .update(destinations::Message::Save, &mut self.output);
                }
                "saved" | "copied" | "pending" | "starting" | "write-error" => {
                    self.scene.late.push((*word).into())
                }
                word if self.area_scene(word, false) => {}
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
            || self.scene_sheets(word, late)
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
                "saved" => {
                    let result = json!({"files": ["capture-0001.png"], "count": 1});
                    self.last_saved = Saved::from_result(camera.clone(), &result, self.now);
                    self.scene_notice(command_notice(&Job::Capture, &Ok(result)));
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

    pub(super) fn screenshot_tick(&mut self) -> Task<Message> {
        let Some(mut request) = self.screenshot.take() else {
            return Task::none();
        };
        let mut task = Task::none();
        let finish = |request: &ScreenshotRequest, error: Option<String>| {
            *request
                .outcome
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = error;
            iced::exit()
        };
        if let Some(receiver) = &request.save {
            match receiver.try_recv() {
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
        } else if request.staged && !request.requested {
            // One tick after staging, so the staged state is what gets drawn.
            task = window::latest()
                .and_then(window::screenshot)
                .map(Message::Screenshot);
            request.requested = true;
        } else if !request.requested
            && self.snapshot.cameras.len() == request.cameras as usize
            && self.pending.len() == self.scene.held.len()
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
                && self
                    .snapshot
                    .cameras
                    .iter()
                    .all(|camera| camera.streaming && camera.frames >= 3)
            {
                self.stage_scene();
                request.staged = true;
            }
        }
        if request.started.elapsed() > Duration::from_secs(30) {
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
}

pub(super) fn save_screenshot(shot: &window::Screenshot, path: &std::path::Path) -> Result<()> {
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
    encoder.write_header()?.write_image_data(&shot.rgba)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use liveness::streaming_camera;

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
