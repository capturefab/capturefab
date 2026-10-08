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
    ///   `saved` (the selected camera just saved a capture), `copied`
    ///   (the session command was just copied), `stalled` (the selected
    ///   camera's frames stopped arriving)
    pub(super) fn apply_scene(&mut self, scene: &[&str]) {
        for word in scene {
            match *word {
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
                    "Saving capture",
                    &Ok(json!({"files": ["capture-0001.png"], "count": 1})),
                )),
                "warning" => self.scene_notice(command_notice(
                    "Discovering cameras",
                    &Ok(json!({
                        "devices": [{"id": "sim:0"}],
                        "warnings": [
                            "USB discovery: access denied; check the device permissions",
                            "Native camera discovery worker stopped",
                        ],
                    })),
                )),
                "error" => self.scene_notice(command_notice(
                    "Saving capture",
                    &Err(anyhow::anyhow!("the camera stopped responding")),
                )),
                "notice-long" => self.scene_notice(command_notice(
                    "Connecting camera",
                    &Err(anyhow::anyhow!(LONG_ERROR)),
                )),
                "searching" => self.hold("Discovering cameras", None),
                "stalled" => self.scene.stalled = true,
                "saved" | "copied" | "pending" => self.scene.late.push((*word).into()),
                _ => {}
            }
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
                    self.scene_notice(command_notice("Saving capture", &Ok(result)));
                }
                "copied" => {
                    self.note_copied(self.session_command());
                    self.scene_notice(Some(("Session command copied".into(), None, Level::Done)));
                }
                "pending" => {
                    if !self.scene.notice {
                        self.notice = None;
                        self.notice_shown.set(0.0);
                    }
                    self.hold("Stopping stream", camera.clone());
                }
                _ => {}
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
    fn hold(&mut self, label: &str, camera: Option<String>) {
        let (sender, receiver) = mpsc::channel();
        self.scene.held.push(sender);
        self.pending.push(Pending {
            label: label.into(),
            receiver,
            target: None,
            camera,
            feature: None,
        });
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
                    self.send_to(&id, "Starting demo stream", SessionCommand::Start);
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
        assert!(bench.pending("Discovering cameras"));
        assert!(bench.pending("Stopping stream"));
        assert_eq!(bench.pending.len(), bench.scene.held.len());
        assert!(
            bench
                .pending
                .iter()
                .any(|p| p.label == "Stopping stream" && p.camera.as_deref() == Some("sim:0"))
        );
    }
}
