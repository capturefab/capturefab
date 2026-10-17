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
    pub(super) sequence: ScreenshotSequence,
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

    fn parse(frames: Option<&str>, fps: Option<&str>) -> Result<Self> {
        let frames = frames.unwrap_or("1").parse::<u32>().map_err(|_| {
            anyhow::anyhow!("CAPTUREFAB_SCREENSHOT_FRAMES must be an integer from 1 to 240")
        })?;
        let fps = fps.unwrap_or("8").parse::<u32>().map_err(|_| {
            anyhow::anyhow!("CAPTUREFAB_SCREENSHOT_FPS must be an integer from 1 to 15")
        })?;
        anyhow::ensure!(
            (1..=240).contains(&frames),
            "CAPTUREFAB_SCREENSHOT_FRAMES must be an integer from 1 to 240"
        );
        anyhow::ensure!(
            (1..=15).contains(&fps),
            "CAPTUREFAB_SCREENSHOT_FPS must be an integer from 1 to 15"
        );
        Ok(Self {
            frames,
            index: 0,
            interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            next: None,
        })
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

    fn ready(&self) -> bool {
        self.ready_at(Instant::now())
    }

    fn ready_at(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }

    fn requested(&mut self) {
        self.next = Some(Instant::now() + self.interval);
    }

    fn timeout(&self) -> Duration {
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
    /// The selected camera reports that it stopped streaming.
    stopped: bool,
    /// Turn on auto mode on every camera as the streams start.
    pub(super) auto: bool,
    /// Scroll a normal widget after its first staged layout, before capturing.
    scroll_end: Option<&'static str>,
    /// Welcome discovery is display-only and never enumerates real devices.
    welcome: bool,
    searching: bool,
}

impl Scene {
    /// Make the snapshot say what the scene holds in place; from `tick()`
    /// after every refresh, so it never reverts.
    pub(super) fn patch(&self, snapshot: &mut SessionSnapshot) {
        if self.welcome {
            snapshot.devices = if self.searching {
                Vec::new()
            } else {
                ["sim:0", "sim:line", "sim:thermal"]
                    .into_iter()
                    .map(crate::transport::simulator::info_named)
                    .collect()
            };
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
                camera.forwarding_since_ms = self.forwarding_since_ms;
            }
            camera.streaming &= !self.stopped;
        }
        if self.forwarding.is_some() {
            snapshot.forwarding = self.forwarding.clone();
            snapshot.forwarding_since_ms = self.forwarding_since_ms;
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
    ///   `recording` (the selected camera records to a file, for 1:23)
    /// - `tags`: the capture tags filled in and open; `dataset`: the Dataset
    ///   section open
    /// - review fixtures: `network`, `network-empty`, `network-error`,
    ///   `destinations-remove`, `bucket-editor-advanced`, `settings-credentials`,
    ///   `settings-credential-error`, `analytics-measurements`,
    ///   `analytics-measurements-populated`, `analytics-invalid`,
    ///   `close-error`;
    ///   `sheet-bottom` and `inspector-bottom` expose their lower content
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
                // run_capture connects no cameras. The snapshot supplies only
                // built-in simulator cards, without real device discovery.
                "welcome" => self.scene.welcome = true,
                "help" => self.help_open = true,
                "analytics" => self.analytics.open = true,
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
                "inspector" => {
                    // Asked for by name: show it even on a quiet first run.
                    self.inspector_open = true;
                    self.inspector_summoned = true;
                }
                "nosidebar" => self.sidebar_open = false,
                "sidebar" => self.sidebar_open = true,
                "peek" => self.inspector_peek = true,
                "light" => self.appearance = Appearance::Light,
                "dark" => self.appearance = Appearance::Dark,
                "sheet-bottom" => self.scene.scroll_end = Some(SHEET_BODY),
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
                    self.scene.forwarding = Some(prefs::default_record_output());
                    self.scene.forwarding_since_ms = Some(epoch_ms().saturating_sub(83_000));
                    self.scene.late.push("recording-file".into());
                }
                "tags" => {
                    self.tags = prefs::CaptureTags {
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
                "dataset" => {
                    let _ = self
                        .handle_message(Message::InspectorDisclosure(inspector::Section::Dataset));
                }
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
                "saved" | "copied" | "pending" | "starting" | "write-error" | "kind-scanner"
                | "kind-unknown" => self.scene.late.push((*word).into()),
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
            || self.scene_autocapture(word, late)
            || self.scene_network(word, late)
            || self.scene_video_analytics(word, late)
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
                "kind-scanner" | "kind-unknown" => self.scene_display_profile(&word),
                "saved" => {
                    let result = json!({"files": ["capture-0001.png"], "count": 1});
                    self.last_saved = Saved::from_result(camera.clone(), &result, self.now);
                    self.scene_notice(command_notice(&Job::Capture, &Ok(result)));
                }
                "recording-file" => {
                    let path = crate::naming::preview(
                        &prefs::default_record_output(),
                        "Sim-SIM0",
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
            self.previews.remove(&id);
            // A scanner starts without a document image. Drop the simulator's
            // single-view frame as well as its tile preview so this fixture
            // exercises the actual Ready to scan guidance.
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
                camera.features = self.snapshot.features.clone();
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
        } else if request.staged && self.scene.scroll_end.is_some() {
            // The first staged frame establishes the scrollable's real
            // extent. Give the operation another paint before the screenshot.
            task = operation::snap_to_end(self.scene.scroll_end.take().unwrap());
        } else if request.staged && !request.requested {
            // One tick after staging, so the staged state is what gets drawn.
            task = window::latest()
                .and_then(window::screenshot)
                .map(Message::Screenshot);
            request.requested = true;
            request.sequence.requested();
        } else if !request.requested
            && request.sequence.ready()
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
                && self.analytics_staged()
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
        for value in ["0", "241", "-1", "2.5", "invalid"] {
            assert!(ScreenshotSequence::parse(Some(value), None).is_err());
        }
        for value in ["0", "16", "-1", "2.5", "invalid"] {
            assert!(ScreenshotSequence::parse(None, Some(value)).is_err());
        }
    }

    #[test]
    fn screenshot_sequence_names_every_frame_and_finishes() {
        let base = std::path::Path::new("/tmp/workbench.png");
        let mut sequence = ScreenshotSequence::parse(Some("3"), Some("5")).unwrap();
        assert_eq!(
            sequence.path(base),
            std::path::Path::new("/tmp/workbench-0000.png")
        );
        assert!(sequence.ready());
        let before_request = Instant::now();
        sequence.requested();
        assert!(
            sequence
                .next
                .is_some_and(|next| next >= before_request + sequence.interval)
        );
        let next = sequence.next.unwrap();
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
