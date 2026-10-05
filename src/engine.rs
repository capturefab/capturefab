//! A single camera owner shared by the UI and authenticated local RPC clients.
use crate::{
    auto::AutoController,
    camera::Camera,
    media::{ForwardConfig, Forwarder},
    types::Frame,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

use crate::session::{LogEntry, SessionCommand, SessionSnapshot};
type Reply = Receiver<Result<Value>>;
struct Envelope {
    command: SessionCommand,
    reply: mpsc::Sender<Result<Value>>,
}
/// Per-frame statistics, updated lock-free on the acquisition hot path and
/// merged into snapshots on read.
#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    dropped: AtomicU64,
    fps_bits: AtomicU64,
}
impl Counters {
    fn reset(&self) {
        self.frames.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.set_fps(0.0);
    }
    fn set_fps(&self, fps: f64) {
        self.fps_bits.store(fps.to_bits(), Ordering::Relaxed);
    }
}
#[derive(Clone)]
pub struct WorkerHandle {
    sender: SyncSender<Envelope>,
    state: Arc<RwLock<SessionSnapshot>>,
    counters: Arc<Counters>,
    frame: Arc<RwLock<Option<Frame>>>,
    quitting: Arc<AtomicBool>,
    thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    ring: Option<Arc<Mutex<crate::shared_memory::SharedRing>>>,
}
impl Default for WorkerHandle {
    fn default() -> Self {
        Self::new()
    }
}
impl WorkerHandle {
    pub fn new() -> Self {
        Self::create(None)
    }
    pub fn with_ring(ring: Arc<Mutex<crate::shared_memory::SharedRing>>) -> Self {
        Self::create(Some(ring))
    }
    fn create(ring: Option<Arc<Mutex<crate::shared_memory::SharedRing>>>) -> Self {
        let (sender, receiver) = mpsc::sync_channel(32);
        let handle = Self {
            sender,
            state: Arc::new(RwLock::new(SessionSnapshot::default())),
            counters: Arc::default(),
            frame: Arc::new(RwLock::new(None)),
            quitting: Arc::new(AtomicBool::new(false)),
            thread: Arc::new(Mutex::new(None)),
            ring,
        };
        let worker_handle = handle.clone();
        let thread = std::thread::Builder::new()
            .name("capturefab-camera".into())
            .spawn(move || worker(worker_handle, receiver))
            .expect("spawn camera worker");
        *handle.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
        handle
    }
    pub fn snapshot(&self) -> SessionSnapshot {
        let mut s = self.state.read().unwrap_or_else(|e| e.into_inner()).clone();
        s.frames = self.counters.frames.load(Ordering::Relaxed);
        s.dropped = self.counters.dropped.load(Ordering::Relaxed);
        s.fps = f64::from_bits(self.counters.fps_bits.load(Ordering::Relaxed));
        s
    }
    #[cfg(test)]
    pub fn latest_frame(&self) -> Option<Frame> {
        self.frame.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn submit(&self, command: SessionCommand) -> Result<Reply> {
        ensure!(
            !self.quitting.load(Ordering::Relaxed),
            "session is shutting down"
        );
        let (reply, rx) = mpsc::channel();
        self.sender
            .try_send(Envelope { command, reply })
            .context("camera command queue is full or closed")?;
        Ok(rx)
    }
    pub fn request(&self, command: SessionCommand) -> Result<Value> {
        self.submit(command)?
            .recv()
            .context("camera worker stopped")?
    }
    pub fn shutdown(&self) {
        self.quitting.store(true, Ordering::Relaxed);
        let thread = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
    fn cancelled(&self) -> bool {
        self.quitting.load(Ordering::Relaxed)
            || self
                .ring
                .as_ref()
                .is_some_and(|r| r.lock().unwrap_or_else(|e| e.into_inner()).cancelled())
    }
    fn log(&self, level: &str, message: String) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        if level == "error" {
            s.last_error = Some(message.clone());
        }
        s.logs.push(LogEntry {
            time: crate::auto::clock(),
            level: level.into(),
            message,
        });
        if s.logs.len() > 200 {
            s.logs.remove(0);
        }
    }
    fn record(&self, frame: Frame) {
        if let Some(ring) = &self.ring
            && let Err(e) = ring.lock().unwrap_or_else(|e| e.into_inner()).write(&frame)
        {
            self.log("error", format!("Shared frame buffer: {e:#}"));
        }
        self.counters.frames.fetch_add(1, Ordering::Relaxed);
        if self.ring.is_none() {
            *self.frame.write().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        }
    }
}
fn timeout(ms: u64) -> Result<Duration> {
    ensure!(
        (1..=60_000).contains(&ms),
        "timeout must be 1..60000 milliseconds"
    );
    Ok(Duration::from_millis(ms))
}
fn refresh(h: &WorkerHandle, camera: &mut Option<Camera>, features: bool) {
    let list = if features {
        camera.as_mut().map(|c| c.features())
    } else {
        None
    };
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    s.connected = camera.as_ref().map(|c| c.info.clone());
    s.streaming = camera.as_ref().is_some_and(Camera::is_streaming);
    s.transport = camera.as_ref().and_then(Camera::stats);
    if let Some(list) = list {
        s.features = list;
    } else if camera.is_none() {
        s.features.clear();
    }
}
fn release_auto(
    h: &WorkerHandle,
    auto: &mut Option<AutoController>,
    camera: &mut Option<Camera>,
    revert: bool,
) -> Result<()> {
    let result = match (auto.take(), camera.as_mut()) {
        (Some(controller), Some(c)) => controller.release(c, revert),
        _ => Ok(()),
    };
    if let Err(e) = &result {
        h.log("warn", format!("Auto mode cleanup: {e:#}"));
    }
    h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
    result
}
fn publish_auto(h: &WorkerHandle, controller: &AutoController) {
    let mut s = h.state.write().unwrap_or_else(|e| e.into_inner());
    s.auto = Some(controller.status().clone());
    for f in s
        .features
        .iter_mut()
        .filter(|f| controller.manages(&f.name))
    {
        if let Some(v) = controller.values().get(&f.name) {
            f.value = Some(v.clone());
        }
    }
}
fn camera_mut(camera: &mut Option<Camera>) -> Result<&mut Camera> {
    camera
        .as_mut()
        .context("no camera connected; select --camera or use connect in a session")
}
fn timed_out(e: &anyhow::Error) -> bool {
    let message = format!("{e:#}").to_ascii_lowercase();
    message.contains("timeout") || message.contains("timed out")
}
fn next_frame(
    h: &WorkerHandle,
    c: &mut Camera,
    auto: &mut Option<AutoController>,
    deadline: Instant,
) -> Result<Option<Frame>> {
    loop {
        ensure!(!h.cancelled(), "capture interrupted");
        if let Some(a) = auto.as_mut()
            && a.tick(c, Instant::now())
        {
            publish_auto(h, a);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        match c.next_frame(remaining.min(Duration::from_millis(100))) {
            Ok(frame) => {
                if let Some(a) = auto.as_mut() {
                    a.observe(&frame, Instant::now());
                }
                return Ok(Some(frame));
            }
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e),
        }
    }
}

fn stop_forward(
    h: &WorkerHandle,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
) -> Result<()> {
    h.state
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .forwarding = None;
    if let Some(a) = auto.as_mut() {
        a.set_forward_fps(None);
    }
    if let Some((mut f, _)) = forwarder.take() {
        f.stop()
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn process(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    forwarder: &mut Option<(Forwarder, f64)>,
    auto: &mut Option<AutoController>,
    jobs: &mut Vec<Scheduled>,
    next_job: &mut u64,
    scheduled_stream: &mut bool,
    command: SessionCommand,
) -> Result<Value> {
    match command {
        SessionCommand::Status => Ok(serde_json::to_value(h.snapshot())?),
        SessionCommand::Select { .. } => anyhow::bail!("selection belongs to the coordinator"),
        SessionCommand::Forward {
            output,
            codec,
            encoder,
            fps,
            bitrate,
            storage,
            max_file_bytes,
        } => {
            ensure!(
                forwarder.is_none(),
                "forwarding is already active; stop-forward first"
            );
            let f = Forwarder::new(ForwardConfig {
                output: output.clone(),
                codec,
                encoder,
                fps,
                bitrate,
                storage,
                max_file_bytes,
            })?;
            camera_mut(camera)?.start()?;
            *forwarder = Some((f, fps));
            if let Some(a) = auto.as_mut() {
                a.set_forward_fps(Some(fps));
            }
            refresh(h, camera, false);
            h.state
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .forwarding = Some(crate::media::redact_url(&output));
            Ok(json!({"forwarding":crate::media::redact_url(&output)}))
        }
        SessionCommand::StopForward => {
            stop_forward(h, forwarder, auto)?;
            Ok(json!({"forwarding":null}))
        }
        SessionCommand::Discover {
            timeout_ms,
            simulated,
        } => {
            let (devices, warnings) = crate::camera::discover(timeout(timeout_ms)?, simulated)?;
            h.state.write().unwrap_or_else(|e| e.into_inner()).devices = devices.clone();
            for w in &warnings {
                h.log("warn", w.clone());
            }
            h.log("info", format!("Discovered {} camera(s)", devices.len()));
            Ok(json!({"devices":devices,"warnings":warnings}))
        }
        SessionCommand::Connect {
            camera: selector,
            timeout_ms,
        } => {
            ensure!(
                camera.is_none(),
                "a camera is already connected; disconnect first"
            );
            let duration = timeout(timeout_ms)?;
            let mut known = h.snapshot().devices;
            if !known
                .iter()
                .any(|d| d.id == selector || d.serial == selector)
                && !selector.starts_with("sim:")
                && !crate::media::is_source(&selector)
                && !crate::onvif::is_source(&selector)
                && selector
                    .trim_start_matches("gige:")
                    .parse::<std::net::Ipv4Addr>()
                    .is_err()
            {
                let (devices, warnings) = crate::camera::discover(duration, false)?;
                known = devices;
                for w in warnings {
                    h.log("warn", w);
                }
                h.state.write().unwrap_or_else(|e| e.into_inner()).devices = known.clone();
            }
            *camera = Some(Camera::open(&selector, &known, duration)?);
            *h.frame.write().unwrap_or_else(|e| e.into_inner()) = None;
            {
                h.counters.reset();
                h.state
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_error = None;
            }
            refresh(h, camera, true);
            h.log(
                "info",
                format!("Connected to {}", crate::media::redact_url(&selector)),
            );
            Ok(json!({"connected":h.snapshot().connected}))
        }
        SessionCommand::Disconnect => {
            cancel_jobs(jobs);
            sync_jobs(h, jobs);
            *scheduled_stream = false;
            let auto_result = release_auto(h, auto, camera, true);
            let forward_result = stop_forward(h, forwarder, auto);
            let camera_result = if let Some(c) = camera.as_mut() {
                c.stop()
            } else {
                Ok(())
            };
            *camera = None;
            *h.frame.write().unwrap_or_else(|e| e.into_inner()) = None;
            refresh(h, camera, false);
            auto_result?;
            forward_result?;
            camera_result?;
            h.log("info", "Disconnected".into());
            Ok(json!({"connected":null}))
        }
        SessionCommand::Get { feature } => {
            Ok(json!({"name":feature,"value":camera_mut(camera)?.get(&feature)?}))
        }
        SessionCommand::Set { feature, value } => {
            let switched = auto.as_ref().is_some_and(|a| a.manages(&feature));
            if switched {
                release_auto(h, auto, camera, false)?;
                h.log(
                    "info",
                    format!("Switched to manual mode because {feature} was set"),
                );
            } else if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            camera_mut(camera)?.set(&feature, &value)?;
            let result = camera_mut(camera)?.get(&feature)?;
            refresh(h, camera, true);
            h.log("info", format!("Set {feature} = {value}"));
            let mut reply = json!({"name":feature,"value":result});
            if switched {
                reply["auto"] = Value::Null;
            }
            Ok(reply)
        }
        SessionCommand::Execute { feature } => {
            if auto.is_some() && matches!(feature.as_str(), "UserSetLoad" | "UserSetDefault") {
                release_auto(h, auto, camera, false)?;
                h.log(
                    "info",
                    format!("Switched to manual mode because {feature} was executed"),
                );
            } else if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            camera_mut(camera)?.execute(&feature)?;
            refresh(h, camera, true);
            Ok(json!({"executed":feature}))
        }
        SessionCommand::Features => {
            refresh(h, camera, true);
            Ok(json!({"features":h.snapshot().features}))
        }
        SessionCommand::Xml => Ok(json!({"xml":camera_mut(camera)?.xml()})),
        SessionCommand::ReadMemory { address, length } => {
            ensure!(
                (1..=65536).contains(&length),
                "memory length must be 1..65536"
            );
            Ok(json!({"address":address,"data":camera_mut(camera)?.read_memory(address,length)?}))
        }
        SessionCommand::WriteMemory { address, data } => {
            ensure!(
                !data.is_empty() && data.len() <= 65536,
                "memory length must be 1..65536"
            );
            if let Some(a) = auto.as_mut() {
                a.invalidate();
            }
            camera_mut(camera)?.write_memory(address, &data)?;
            Ok(json!({"address":address,"written":data.len()}))
        }
        SessionCommand::Jobs => {
            Ok(json!({"jobs":jobs.iter().map(|j| &j.info).collect::<Vec<_>>()}))
        }
        SessionCommand::CancelJob { id } => {
            let job = jobs
                .iter_mut()
                .find(|j| j.info.id == id)
                .context("capture job not found")?;
            ensure!(job.info.active(), "capture job is already finished");
            job.info.status = "cancelled".into();
            sync_jobs(h, jobs);
            Ok(json!({"cancelled":id}))
        }
        SessionCommand::Schedule {
            output,
            count,
            timeout_ms,
            format,
            first_at_ms,
            interval_ms,
            storage,
        } => {
            validate_capture(&output, count, timeout_ms, &format)?;
            storage.validate()?;
            ensure!(
                output != "-",
                "scheduled capture requires a file or directory"
            );
            ensure!(
                jobs.iter().filter(|j| j.info.active()).count() < 32,
                "capture schedule queue is full (32 active jobs)"
            );
            ensure!(
                interval_ms <= 315_576_000_000,
                "interval must be at most 10 years"
            );
            ensure!(
                first_at_ms <= crate::scheduling::now_ms().saturating_add(315_576_000_000),
                "capture time must be within 10 years"
            );
            camera_mut(camera)?;
            let first_at_ms = first_at_ms.max(crate::scheduling::now_ms());
            let info = crate::scheduling::CaptureJob {
                id: *next_job,
                status: "pending".into(),
                output,
                count,
                captured: 0,
                first_at_ms,
                interval_ms,
                next_at_ms: first_at_ms,
                last_file: None,
                error: None,
            };
            *next_job = next_job.checked_add(1).context("job ID exhausted")?;
            jobs.push(Scheduled {
                info: info.clone(),
                timeout_ms,
                format,
                storage,
                deadline: None,
                warm: None,
            });
            trim_jobs(jobs);
            sync_jobs(h, jobs);
            h.log("info", format!("Capture job {} scheduled", info.id));
            Ok(json!({"job":info}))
        }
        SessionCommand::Auto { balance } => {
            let c = camera_mut(camera)?;
            let explicit = match auto.as_mut() {
                Some(controller) => {
                    if let Some(balance) = balance {
                        controller.set_balance(c, balance)?;
                    }
                    balance.is_some()
                }
                None => {
                    let balance = balance.unwrap_or(crate::auto::DEFAULT_BALANCE);
                    let mut controller = AutoController::enter(c, balance)?;
                    controller.set_forward_fps(forwarder.as_ref().map(|(_, fps)| *fps));
                    *auto = Some(controller);
                    true
                }
            };
            let controller = auto.as_mut().context("auto controller")?;
            if explicit && forwarder.is_none() && !jobs.iter().any(|j| j.info.active()) {
                controller.choose_video_mode(c);
            }
            controller.tick(c, Instant::now());
            let status = controller.status().clone();
            refresh(h, camera, true);
            publish_auto(h, controller);
            h.log("info", format!("Auto mode, balance {:.2}", status.balance));
            Ok(json!({"auto":status}))
        }
        SessionCommand::Manual { revert } => {
            let c = camera_mut(camera)?;
            let released = match auto.take() {
                Some(controller) => controller.release(c, revert),
                None if revert => Ok(()),
                None => crate::auto::hold(c),
            };
            h.state.write().unwrap_or_else(|e| e.into_inner()).auto = None;
            refresh(h, camera, true);
            released?;
            h.log("info", "Manual mode".into());
            Ok(json!({"auto":null}))
        }
        SessionCommand::Start => {
            *scheduled_stream = false;
            camera_mut(camera)?.start()?;
            refresh(h, camera, false);
            h.log("info", "Acquisition started".into());
            Ok(json!({"streaming":true}))
        }
        SessionCommand::Stop => {
            cancel_jobs(jobs);
            sync_jobs(h, jobs);
            *scheduled_stream = false;
            let forward_result = stop_forward(h, forwarder, auto);
            let camera_result = camera_mut(camera)?.stop();
            refresh(h, camera, false);
            forward_result?;
            camera_result?;
            h.log("info", "Acquisition stopped".into());
            Ok(json!({"streaming":false}))
        }
        SessionCommand::Capture {
            output,
            count,
            timeout_ms,
            format,
            storage,
        } => {
            validate_capture(&output, count, timeout_ms, &format)?;
            storage.validate()?;
            let duration = timeout(timeout_ms)?;
            ensure!((1..=100_000).contains(&count), "count must be 1..100000");
            ensure!(
                ["png", "raw", "ppm", "pgm"].contains(&format.as_str()),
                "format must be png, raw, pgm or ppm"
            );
            ensure!(!output.is_empty(), "provide an output path");
            ensure!(
                output != "-" || count == 1,
                "stdout supports one frame only"
            );
            let c = camera_mut(camera)?;
            let was_streaming = c.is_streaming();
            c.start()?;
            let result = (|| -> Result<Value> {
                let mut files = Vec::new();
                let mut frames = Vec::new();
                let mut status = None;
                if !was_streaming && auto.is_some() {
                    // Refresh the controller's streaming state before testing convergence:
                    // a previously stable acquisition must settle again after a restart.
                    if let Some(a) = auto.as_mut() {
                        a.tick(c, Instant::now());
                        publish_auto(h, a);
                    }
                    let until = Instant::now() + (duration / 2).min(Duration::from_secs(3));
                    while auto.as_ref().is_some_and(|a| !a.converged()) {
                        let Some(frame) = next_frame(h, c, auto, until)? else {
                            h.log(
                                "warn",
                                "Auto exposure had not settled; capturing anyway".into(),
                            );
                            break;
                        };
                        h.record(frame);
                    }
                }
                for index in 0..count {
                    let frame = next_frame(h, c, auto, Instant::now() + duration)?
                        .context("frame timeout")?;
                    if index == 0 {
                        status = auto.as_ref().map(|a| a.status().clone());
                    }
                    let mut metadata = serde_json::to_value(&frame)?;
                    metadata["bytes"] = json!(frame.data.len());
                    if output != "-" {
                        let path = output_path(&output, count, index, &format);
                        crate::storage::save(&frame, &path, &format, &storage)
                            .with_context(|| format!("save {}", path.display()))?;
                        files.push(path.to_string_lossy().to_string());
                    }
                    frames.push(metadata);
                    h.record(frame);
                }
                let mut value = json!({"files":files,"frames":frames,"count":count});
                if let Some(status) = status {
                    value["auto"] = json!(status);
                }
                Ok(value)
            })();
            let stopped = if !was_streaming { c.stop() } else { Ok(()) };
            refresh(h, camera, false);
            let value = result?;
            stopped?;
            h.log("info", format!("Captured {count} frame(s)"));
            Ok(value)
        }
    }
}

struct Scheduled {
    info: crate::scheduling::CaptureJob,
    timeout_ms: u64,
    format: String,
    storage: crate::storage::StoragePolicy,
    deadline: Option<Instant>,
    warm: Option<Instant>,
}
fn validate_capture(output: &str, count: u32, timeout_ms: u64, format: &str) -> Result<()> {
    timeout(timeout_ms)?;
    ensure!((1..=100_000).contains(&count), "count must be 1..100000");
    ensure!(
        ["png", "raw", "ppm", "pgm"].contains(&format),
        "format must be png, raw, pgm or ppm"
    );
    ensure!(!output.is_empty(), "provide an output path");
    Ok(())
}
fn cancel_jobs(jobs: &mut [Scheduled]) {
    for j in jobs.iter_mut().filter(|j| j.info.active()) {
        j.info.status = "cancelled".into();
    }
}
fn trim_jobs(jobs: &mut Vec<Scheduled>) {
    while jobs.len() > 132 {
        if let Some(i) = jobs.iter().position(|j| !j.info.active()) {
            jobs.remove(i);
        } else {
            break;
        }
    }
}
fn sync_jobs(h: &WorkerHandle, jobs: &[Scheduled]) {
    h.state.write().unwrap_or_else(|e| e.into_inner()).jobs =
        jobs.iter().map(|j| j.info.clone()).collect();
}
fn tick_jobs(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    jobs: &mut Vec<Scheduled>,
    started: &mut bool,
    forwarding: bool,
    auto: bool,
) {
    let now = crate::scheduling::now_ms();
    for job in jobs.iter_mut().filter(|j| j.info.active()) {
        if job.info.next_at_ms > now {
            continue;
        }
        if job.deadline.is_none() {
            let result = (|| -> Result<bool> {
                let c = camera_mut(camera)?;
                let stopped = !c.is_streaming();
                if stopped {
                    c.start()?;
                    *started = true;
                }
                Ok(stopped)
            })();
            let stopped = match result {
                Ok(stopped) => stopped,
                Err(e) => {
                    job.info.status = "failed".into();
                    job.info.error = Some(format!("{e:#}"));
                    h.log(
                        "error",
                        format!("Capture job {} failed: {e:#}", job.info.id),
                    );
                    continue;
                }
            };
            job.info.status = "running".into();
            let timeout = Duration::from_millis(job.timeout_ms);
            job.deadline = Some(Instant::now() + timeout);
            job.warm = (auto && stopped)
                .then(|| Instant::now() + (timeout / 2).min(Duration::from_secs(3)));
        }
        if job.deadline.is_some_and(|d| Instant::now() >= d) {
            job.info.status = "failed".into();
            job.info.error = Some("frame timeout".into());
            h.log(
                "error",
                format!("Capture job {}: frame timeout", job.info.id),
            );
        }
    }
    if *started && !forwarding && !jobs.iter().any(|j| j.info.active() && j.deadline.is_some()) {
        if let Some(c) = camera.as_mut() {
            let _ = c.stop();
        }
        *started = false;
    }
    trim_jobs(jobs);
    sync_jobs(h, jobs);
    refresh(h, camera, false);
}
fn capture_jobs(h: &WorkerHandle, frame: &Frame, jobs: &mut [Scheduled], settled: bool) {
    let now = crate::scheduling::now_ms();
    for job in jobs.iter_mut().filter(|j| {
        j.info.active()
            && j.deadline.is_some()
            && j.info.next_at_ms <= now
            && (settled || j.warm.is_none_or(|w| Instant::now() >= w))
    }) {
        let path = output_path(
            &job.info.output,
            job.info.count,
            job.info.captured,
            &job.format,
        );
        match crate::storage::save(frame, &path, &job.format, &job.storage) {
            Ok(()) => {
                job.info.captured += 1;
                job.info.last_file = Some(path.to_string_lossy().into_owned());
                job.deadline = None;
                job.warm = None;
                if job.info.captured >= job.info.count {
                    job.info.status = "complete".into();
                    h.log(
                        "info",
                        format!(
                            "Capture job {} complete: {} frame(s)",
                            job.info.id, job.info.captured
                        ),
                    );
                } else {
                    job.info.status = "pending".into();
                    job.info.next_at_ms = job.info.first_at_ms.saturating_add(
                        job.info
                            .interval_ms
                            .saturating_mul(job.info.captured as u64),
                    );
                }
            }
            Err(e) => {
                job.info.status = "failed".into();
                job.info.error = Some(format!("{e:#}"));
                h.log(
                    "error",
                    format!("Capture job {} stopped: {e:#}", job.info.id),
                );
            }
        }
    }
    sync_jobs(h, jobs);
}

pub fn output_path(output: &str, count: u32, index: u32, format: &str) -> PathBuf {
    if output.contains("{frame}") {
        PathBuf::from(output.replace("{frame}", &format!("{:06}", index + 1)))
    } else if count > 1 || PathBuf::from(output).is_dir() {
        PathBuf::from(output).join(format!("frame-{:06}.{format}", index + 1))
    } else {
        PathBuf::from(output)
    }
}

fn worker(h: WorkerHandle, rx: Receiver<Envelope>) {
    let mut camera = None;
    let mut forwarder: Option<(Forwarder, f64)> = None;
    let mut auto: Option<AutoController> = None;
    let mut jobs: Vec<Scheduled> = Vec::new();
    let mut next_job = 1;
    let mut scheduled_stream = false;
    let mut rate_epoch = Instant::now();
    let mut rate_count = 0;
    let mut consecutive_errors = 0u32;
    while !h.cancelled() {
        let envelope = if camera.as_ref().is_some_and(Camera::is_streaming) {
            rx.try_recv().ok()
        } else {
            rx.recv_timeout(Duration::from_millis(100)).ok()
        };
        if let Some(e) = envelope {
            let result = process(
                &h,
                &mut camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next_job,
                &mut scheduled_stream,
                e.command,
            );
            if let Err(err) = &result {
                h.log("error", format!("{err:#}"));
            }
            let _ = e.reply.send(result);
            rate_epoch = Instant::now();
            rate_count = 0;
            consecutive_errors = 0;
        }
        tick_jobs(
            &h,
            &mut camera,
            &mut jobs,
            &mut scheduled_stream,
            forwarder.is_some(),
            auto.is_some(),
        );
        if let (Some(controller), Some(c)) = (auto.as_mut(), camera.as_mut())
            && controller.tick(c, Instant::now())
        {
            publish_auto(&h, controller);
        }
        if let Some(c) = camera.as_mut().filter(|c| c.is_streaming()) {
            match c.next_frame(Duration::from_millis(100)) {
                Ok(frame) => {
                    if let Some(controller) = auto.as_mut() {
                        controller.observe(&frame, Instant::now());
                    }
                    let settled = auto.as_ref().is_none_or(AutoController::converged);
                    capture_jobs(&h, &frame, &mut jobs, settled);
                    if let Some((f, _)) = forwarder.as_mut()
                        && let Err(e) = f.push(&frame)
                    {
                        h.log("error", format!("Forwarding stopped: {e:#}"));
                        let _ = stop_forward(&h, &mut forwarder, &mut auto);
                    }
                    h.record(frame);
                    rate_count += 1;
                    consecutive_errors = 0;
                }
                Err(e) => {
                    if !timed_out(&e) {
                        consecutive_errors += 1;
                        h.counters.dropped.fetch_add(1, Ordering::Relaxed);
                        if consecutive_errors == 1 {
                            h.log("warn", format!("{e:#}"));
                        }
                        if consecutive_errors >= 10 {
                            let _ = c.stop();
                            h.log(
                                "error",
                                "Acquisition stopped after repeated transport errors".into(),
                            );
                            refresh(&h, &mut camera, false);
                        }
                    }
                }
            }
            if rate_epoch.elapsed() >= Duration::from_secs(1) {
                h.counters
                    .set_fps(rate_count as f64 / rate_epoch.elapsed().as_secs_f64());
                rate_epoch = Instant::now();
                rate_count = 0;
            }
        }
    }
    if let Some((mut f, _)) = forwarder {
        let _ = f.stop();
    }
    let _ = release_auto(&h, &mut auto, &mut camera, true);
    if let Some(mut c) = camera
        && let Err(e) = c.stop()
    {
        h.log("warn", format!("Acquisition cleanup: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_simulator_configuration_and_acquisition() {
        let h = WorkerHandle::new();
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
        .unwrap();
        h.request(SessionCommand::Set {
            feature: "Width".into(),
            value: "128".into(),
        })
        .unwrap();
        assert_eq!(
            h.request(SessionCommand::Get {
                feature: "Width".into()
            })
            .unwrap()["value"],
            128
        );
        assert!(
            h.request(SessionCommand::Set {
                feature: "Width".into(),
                value: "1".into()
            })
            .is_err()
        );
        let v = h
            .request(SessionCommand::Capture {
                output: "-".into(),
                count: 1,
                timeout_ms: 1000,
                format: "raw".into(),
                storage: Default::default(),
            })
            .unwrap();
        assert_eq!(v["frames"][0]["width"], 128);
        assert!(!h.snapshot().streaming);
        assert!(h.latest_frame().is_some());
        h.request(SessionCommand::Disconnect).unwrap();
        assert!(h.snapshot().connected.is_none());
        h.shutdown();
    }
    #[test]
    fn capture_paths() {
        assert_eq!(
            output_path("frames", 2, 0, "png"),
            PathBuf::from("frames/frame-000001.png")
        );
        assert_eq!(
            output_path("shot-{frame}.png", 5, 3, "png"),
            PathBuf::from("shot-000004.png")
        );
    }

    fn connect(h: &WorkerHandle, exposure: &str) {
        h.request(SessionCommand::Connect {
            camera: "sim:0".into(),
            timeout_ms: 100,
        })
        .unwrap();
        for (feature, value) in [
            ("Width", "128"),
            ("Height", "128"),
            ("ExposureTime", exposure),
        ] {
            h.request(SessionCommand::Set {
                feature: feature.into(),
                value: value.into(),
            })
            .unwrap();
        }
    }
    fn auto(h: &WorkerHandle, balance: Option<f64>) -> Result<Value> {
        h.request(SessionCommand::Auto { balance })
    }

    #[test]
    fn software_auto_exposure_converges_inside_the_balance_caps() {
        for (balance, exposure, fps) in [(1.0, "2500", 120.0f64), (0.0, "40000", 30.0)] {
            let h = WorkerHandle::new();
            connect(&h, exposure);
            h.request(SessionCommand::Start).unwrap();
            assert_eq!(
                auto(&h, Some(balance)).unwrap()["auto"]["strategy"],
                "software"
            );
            let deadline = Instant::now() + Duration::from_secs(20);
            let status = loop {
                let status = h.snapshot().auto.unwrap();
                if status.state == "stable" || Instant::now() > deadline {
                    break status;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(status.state, "stable");
            let cap = 1e6 / fps - (0.02e6 / fps).max(100.0);
            assert!((status.exposure_limits_us.unwrap()[1] - cap).abs() < 1e-6);
            assert!(status.exposure_us.unwrap() <= cap + 1e-6);
            assert!((status.brightness.unwrap() / 0.45).log2().abs() < 0.2);
            assert_eq!(status.gain_db.unwrap() > 0.5, balance == 1.0);
            h.shutdown();
        }
    }

    #[test]
    fn auto_mode_commands_follow_the_contract() {
        let h = WorkerHandle::new();
        assert!(auto(&h, None).is_err());
        connect(&h, "10000");
        assert!(auto(&h, Some(2.0)).is_err());
        assert!(h.snapshot().auto.is_none());
        let entered = auto(&h, Some(0.25)).unwrap();
        assert_eq!(entered["auto"]["balance"], 0.25);
        assert!(
            entered["auto"]["managed"]
                .as_array()
                .unwrap()
                .contains(&json!("ExposureTime"))
        );
        assert_eq!(auto(&h, None).unwrap()["auto"]["balance"], 0.25);
        assert!(auto(&h, Some(f64::NAN)).is_err());
        assert_eq!(h.snapshot().auto.unwrap().balance, 0.25);
        let set = |feature: &str, value: &str| {
            h.request(SessionCommand::Set {
                feature: feature.into(),
                value: value.into(),
            })
            .unwrap()
        };
        assert!(set("Width", "128").get("auto").is_none());
        assert!(h.snapshot().auto.is_some());
        let manual = set("ExposureTime", "5000");
        assert!(manual.get("auto").is_some_and(Value::is_null));
        assert_eq!(manual["value"], 5000.0);
        assert!(h.snapshot().auto.is_none());
        assert_eq!(
            h.request(SessionCommand::Manual { revert: false }).unwrap(),
            json!({"auto": null})
        );
        h.shutdown();
    }

    #[test]
    fn capture_warms_up_auto_exposure_and_reports_auto_status() {
        let h = WorkerHandle::new();
        connect(&h, "2500");
        auto(&h, Some(0.5)).unwrap();
        let v = h
            .request(SessionCommand::Capture {
                output: "-".into(),
                count: 1,
                timeout_ms: 6000,
                format: "raw".into(),
                storage: Default::default(),
            })
            .unwrap();
        assert_eq!(v["frames"][0]["width"], 128);
        assert_eq!(v["auto"]["strategy"], "software");
        assert!(v["auto"]["brightness"].as_f64().unwrap() > 0.2);
        assert!(!h.snapshot().streaming);
        h.shutdown();
    }

    struct Shared(Arc<Mutex<crate::transport::simulator::Simulator>>);
    impl crate::types::RegisterIo for Shared {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.0.lock().unwrap().read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.0.lock().unwrap().write_memory(address, data)
        }
    }
    impl crate::types::Backend for Shared {
        fn xml(&mut self) -> Result<String> {
            self.0.lock().unwrap().xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.0.lock().unwrap().start(payload)
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.0.lock().unwrap().next_frame(timeout)
        }
        fn stop(&mut self) -> Result<()> {
            self.0.lock().unwrap().stop()
        }
    }

    #[test]
    fn disconnect_reverts_auto_mode_changes() {
        use crate::types::{Backend, RegisterIo};
        let sim = Arc::new(Mutex::new(crate::transport::simulator::Simulator::default()));
        let read = |address: u64| {
            let bytes = sim.lock().unwrap().read_memory(address, 8).unwrap();
            f64::from_le_bytes(bytes.try_into().unwrap())
        };
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Shared(sim.clone())),
            )
            .unwrap(),
        );
        camera
            .as_mut()
            .unwrap()
            .set("ExposureTime", "20000")
            .unwrap();
        let h = WorkerHandle::new();
        let (mut forwarder, mut auto, mut jobs) = (None, None, Vec::new());
        let (mut next_job, mut stream) = (1, false);
        let mut run = |camera: &mut Option<Camera>, command| {
            process(
                &h,
                camera,
                &mut forwarder,
                &mut auto,
                &mut jobs,
                &mut next_job,
                &mut stream,
                command,
            )
        };
        run(&mut camera, SessionCommand::Auto { balance: Some(1.0) }).unwrap();
        assert!((read(0x110) - (1e6 / 120.0 - 1e6 / 120.0 * 0.02)).abs() < 1e-6);
        assert!(read(0x118) > 7.0);
        assert_eq!(read(0x120), 120.0);
        run(&mut camera, SessionCommand::Disconnect).unwrap();
        assert!(camera.is_none());
        assert_eq!(
            [read(0x110), read(0x118), read(0x120)],
            [20_000.0, 0.0, 30.0]
        );
        assert!(sim.lock().unwrap().xml().is_ok());
        h.shutdown();
    }
}
