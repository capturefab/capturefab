//! Screenshots of named workbench states, for reviewing the interface:
//! `gui --screenshot` with CAPTUREFAB_SCREENSHOT_SCENE set to comma-separated
//! words, e.g. `help,light`.
use super::*;

pub(super) struct ScreenshotRequest {
    pub(super) path: PathBuf,
    pub(super) cameras: u32,
    pub(super) started: Instant,
    pub(super) streams_started: Option<Instant>,
    pub(super) requested: bool,
    pub(super) save: Option<Receiver<Result<()>>>,
    pub(super) outcome: Arc<Mutex<Option<String>>>,
}

impl Workbench {
    /// Put the workbench in a named state for a screenshot. Set through
    /// CAPTUREFAB_SCREENSHOT_SCENE as comma-separated words, e.g. `help,light`.
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
                "light" => self.appearance = Appearance::Light,
                "dark" => self.appearance = Appearance::Dark,
                "notice" => {
                    self.notice = Some(("Saved capture-0001.png".into(), false, self.now));
                }
                "error" => {
                    self.notice = Some((
                        "Capture failed: the camera stopped responding".into(),
                        true,
                        self.now,
                    ));
                }
                _ => {}
            }
        }
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
        } else if !request.requested
            && self.snapshot.cameras.len() == request.cameras as usize
            && self.pending.is_empty()
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
                task = window::latest()
                    .and_then(window::screenshot)
                    .map(Message::Screenshot);
                request.requested = true;
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
