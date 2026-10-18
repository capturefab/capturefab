//! Session-local, human-supervised automatic capture. The database is the session:
//! raw sensor frames, decisions, settings and the bounded background model all
//! live in one DuckDB file. No network service or download is needed. Frames are
//! compared as embeddings from the bundled DINOv2 model (on the GPU when one is
//! usable), or as a native color/edge descriptor in builds without it; each
//! session records which one it learned.
#[cfg(feature = "dinov2")]
pub mod dinov2;

use crate::{frame, types::Frame};
use anyhow::{Context, Result, ensure};
use duckdb::{Config, Connection, OptionalExt, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: u32 = 2;
const FEATURES: usize = 84;
/// A background embedding that finishes this long after its frame is dropped:
/// the scene has moved on, and pausing must not leave a late capture behind.
const STALE_SAMPLE: Duration = Duration::from_secs(2);
const MAX_PENDING: u64 = 200;
const MAX_STORED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_BACKGROUND: usize = 64;
const MAX_RECENT: usize = 32;
const MODEL: &str = "Session exemplar kNN + background novelty";
type Descriptor = Vec<f32>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub path: String,
    pub camera: String,
    pub enabled: bool,
    pub sensitivity: f64,
    pub cooldown_ms: u64,
    pub manual_count: u64,
    pub kept_count: u64,
    pub discarded_count: u64,
    pub pending_count: u64,
    pub blocked_storage: bool,
    pub state: String,
    pub model: String,
    /// `dinov2` or `native`; sessions from older versions are `native`.
    #[serde(default = "native")]
    pub encoder: String,
    /// Where embeddings run, for example "GPU: Apple M4 Max (Metal)" or "CPU".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<String>,
    /// Frames embedded per second over the last few seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples_per_second: Option<f64>,
}

fn native() -> String {
    "native".into()
}

/// Which image representation a new capture session learns from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EncoderChoice {
    /// The bundled DINOv2 model when this build includes it, otherwise native
    #[default]
    Auto,
    /// Bundled DINOv2 ViT-S/14 embeddings, on the GPU when possible
    Dinov2,
    /// Native color/edge descriptor
    Native,
}

/// Distances are only comparable within one encoder, so each has its own
/// calibration. DINOv2 descriptors are unit vectors and use half the chord
/// length (0 identical, 0.5 orthogonal, 1 opposite); the native descriptor
/// uses the RMS difference of its bounded features.
struct Metric {
    chord: bool,
    /// Width of the similarity kernel around each positive example.
    sigma: f64,
    /// Closer than this to a recent capture is the same moment again.
    duplicate: f64,
    /// Background prototypes are at least this far apart.
    distinct: f64,
    /// Distance from the background at which novelty saturates.
    novelty: f64,
    /// At most one sample per interval.
    interval: Duration,
}

const NATIVE_METRIC: Metric = Metric {
    chord: false,
    sigma: 0.18,
    duplicate: 0.018,
    distinct: 0.035,
    novelty: 0.30,
    interval: Duration::from_millis(250),
};
/// Measured on ViT-S/14 CLS embeddings: sensor noise and exposure drift stay
/// under ~0.05, the same target moved in view is ~0.14-0.20, the target
/// against its empty background ~0.55-0.61 and unrelated scenes ~0.7.
/// Sampling follows the camera at up to 30 frames per second.
#[cfg(feature = "dinov2")]
const DINOV2_METRIC: Metric = Metric {
    chord: true,
    sigma: 0.30,
    duplicate: 0.08,
    distinct: 0.08,
    novelty: 0.40,
    interval: Duration::from_nanos(1_000_000_000 / 30),
};

#[derive(Clone)]
enum Encoder {
    Native,
    #[cfg(feature = "dinov2")]
    Dinov2(Arc<dinov2::Model>),
}

impl Encoder {
    fn name(&self) -> &'static str {
        match self {
            Self::Native => "native",
            #[cfg(feature = "dinov2")]
            Self::Dinov2(_) => "dinov2",
        }
    }
    /// Bumped whenever preprocessing or pooling changes the descriptor.
    fn version(&self) -> u32 {
        1
    }
    fn dims(&self) -> usize {
        match self {
            Self::Native => FEATURES,
            #[cfg(feature = "dinov2")]
            Self::Dinov2(model) => model.dims(),
        }
    }
    fn metric(&self) -> &'static Metric {
        match self {
            Self::Native => &NATIVE_METRIC,
            #[cfg(feature = "dinov2")]
            Self::Dinov2(_) => &DINOV2_METRIC,
        }
    }
    fn label(&self) -> String {
        match self {
            Self::Native => format!("{MODEL} (spatial color/edge descriptor v1)"),
            #[cfg(feature = "dinov2")]
            Self::Dinov2(model) => format!("{MODEL} ({})", model.label()),
        }
    }
    fn fingerprint(&self) -> Option<String> {
        match self {
            Self::Native => None,
            #[cfg(feature = "dinov2")]
            Self::Dinov2(model) => Some(model.fingerprint().into()),
        }
    }
    /// The native descriptor takes about a millisecond; a ViT pass must not
    /// stall acquisition on the camera worker thread.
    fn background(&self) -> bool {
        !matches!(self, Self::Native)
    }
    fn describe(&self, frame: &Frame) -> Result<Descriptor> {
        match self {
            Self::Native => descriptor(frame),
            #[cfg(feature = "dinov2")]
            Self::Dinov2(model) => {
                ensure!(
                    frame.data.len() <= MAX_FRAME_BYTES,
                    "frame exceeds 256 MiB session capture limit"
                );
                model.embed_rgb(&dinov2::prepare(frame, model.input())?)
            }
        }
    }
    fn decode(&self, bytes: &[u8]) -> Result<Descriptor> {
        ensure!(
            bytes.len() == self.dims() * 4,
            "corrupt session descriptor length"
        );
        let feature: Vec<_> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let valid = match self {
            Self::Native => feature
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
            #[cfg(feature = "dinov2")]
            Self::Dinov2(_) => {
                let length: f64 = feature.iter().map(|v| f64::from(*v).powi(2)).sum();
                feature.iter().all(|v| v.is_finite()) && (length - 1.0).abs() < 1e-3
            }
        };
        ensure!(valid, "corrupt session descriptor values");
        Ok(feature)
    }
    fn distance(&self, a: &Descriptor, b: &Descriptor) -> f64 {
        let squares = a
            .iter()
            .zip(b)
            .map(|(x, y)| f64::from(x - y).powi(2))
            .sum::<f64>();
        if self.metric().chord {
            squares.sqrt() / 2.0
        } else {
            (squares / a.len().max(1) as f64).sqrt()
        }
    }
    fn nearest(&self, examples: &[Descriptor], feature: &Descriptor) -> Option<f64> {
        // An exemplar nearest-neighbor model can learn immediately from a single
        // manual capture; unlike a fitted classifier it needs no balanced batch.
        examples
            .iter()
            .map(|example| self.distance(example, feature))
            .reduce(f64::min)
    }
    fn similarity(&self, distance: f64) -> f64 {
        (-distance.powi(2) / (2.0 * self.metric().sigma.powi(2))).exp()
    }
}

#[cfg(not(feature = "dinov2"))]
const DINOV2_UNSUPPORTED: &str = "DINOv2 auto capture is not supported by this build";

/// The bundled DINOv2 encoder. Weights that are present but unreadable are
/// an error, never a silent fallback to the native descriptor.
#[cfg(feature = "dinov2")]
fn dinov2_encoder() -> Result<Option<Encoder>> {
    Ok(dinov2::bundled()?.map(Encoder::Dinov2))
}
#[cfg(not(feature = "dinov2"))]
fn dinov2_encoder() -> Result<Option<Encoder>> {
    Ok(None)
}

fn new_encoder(choice: Option<EncoderChoice>) -> Result<Encoder> {
    match choice.unwrap_or_default() {
        EncoderChoice::Native => Ok(Encoder::Native),
        #[cfg(feature = "dinov2")]
        EncoderChoice::Dinov2 => dinov2_encoder()?.context(dinov2::MISSING),
        #[cfg(not(feature = "dinov2"))]
        EncoderChoice::Dinov2 => anyhow::bail!(DINOV2_UNSUPPORTED),
        EncoderChoice::Auto => Ok(dinov2_encoder()?.unwrap_or(Encoder::Native)),
    }
}

/// Reopen the exact encoder a session learned with: descriptors from another
/// model live in a different space and would silently corrupt every score.
fn stored_encoder(name: &str, dims: usize, fingerprint: Option<&str>) -> Result<Encoder> {
    let encoder = match name {
        "native" => Encoder::Native,
        #[cfg(feature = "dinov2")]
        "dinov2" => {
            let stored =
                fingerprint.context("capture session is missing its DINOv2 model fingerprint")?;
            let model = dinov2::bundled()?.context(dinov2::MISSING)?;
            ensure!(
                model.fingerprint() == stored,
                "capture session was learned with DINOv2 model {}, but this Capturefab bundles model {}; resume it with the version that created it or create a new session",
                &stored[..stored.len().min(12)],
                &model.fingerprint()[..12]
            );
            Encoder::Dinov2(model)
        }
        #[cfg(not(feature = "dinov2"))]
        "dinov2" => {
            let _ = fingerprint;
            anyhow::bail!("this capture session uses DINOv2 embeddings; {DINOV2_UNSUPPORTED}")
        }
        other => anyhow::bail!("unsupported capture session encoder {other}"),
    };
    ensure!(
        encoder.dims() == dims,
        "capture session descriptors have {dims} dimensions, but the {} encoder produces {}",
        encoder.name(),
        encoder.dims()
    );
    Ok(encoder)
}

/// What `doctor` reports: the encoder a new `auto` session uses, whether the
/// model is bundled, and the GPU its helper would use.
pub fn doctor() -> serde_json::Value {
    #[cfg(feature = "dinov2")]
    let found = match dinov2::bundled() {
        Ok(Some(model)) => serde_json::json!({
            "default_encoder": "dinov2",
            "dinov2": {
                "bundled": true,
                "model": model.label(),
                "fingerprint": model.fingerprint(),
                "license": "Apache-2.0",
                "gpu": match dinov2::helper::probe(&model) {
                    Ok(adapter) => serde_json::json!({"available": true, "adapter": adapter}),
                    Err(error) => serde_json::json!({"available": false, "reason": format!("{error:#}")}),
                },
            },
        }),
        Ok(None) => serde_json::json!({
            "default_encoder": "native",
            "dinov2": {"bundled": false, "hint": dinov2::MISSING},
        }),
        Err(error) => serde_json::json!({
            "default_encoder": null,
            "dinov2": {"bundled": true, "error": format!("{error:#}")},
        }),
    };
    #[cfg(not(feature = "dinov2"))]
    let found = serde_json::json!({
        "default_encoder": "native",
        "dinov2": {"bundled": false, "hint": DINOV2_UNSUPPORTED},
    });
    found
}

/// A sampled frame, when it was sampled, when its embedding finished, and the
/// embedding.
type Embedded = (Frame, Instant, Instant, Result<Descriptor>);

enum Job {
    Sample(Frame, Instant),
    /// A manual capture waits for its own embedding.
    Manual(Frame, mpsc::Sender<Result<Descriptor>>),
}

/// What the encoder thread reports for status.
#[derive(Default)]
struct Throughput {
    accelerator: Option<String>,
    finished: VecDeque<Instant>,
}

impl Throughput {
    fn record(&mut self, now: Instant) {
        self.finished.push_back(now);
        while self
            .finished
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_secs(3))
        {
            self.finished.pop_front();
        }
    }
    fn rate(&self) -> Option<f64> {
        let (first, last) = (self.finished.front()?, self.finished.back()?);
        let span = last.saturating_duration_since(*first).as_secs_f64();
        (self.finished.len() >= 2
            && span > 0.0
            && Instant::now().saturating_duration_since(*last) < Duration::from_secs(1))
        .then(|| (self.finished.len() - 1) as f64 / span)
    }
}

/// One encoder thread per DINOv2 session. At most one sampled frame is in
/// flight, so the worker holds one extra frame copy and sampling slows to the
/// encoder's speed instead of queueing.
struct Embedder {
    jobs: Option<mpsc::SyncSender<Job>>,
    done: mpsc::Receiver<Embedded>,
    busy: bool,
    thread: Option<std::thread::JoinHandle<()>>,
    throughput: Arc<Mutex<Throughput>>,
}

impl Embedder {
    fn spawn(encoder: Encoder) -> Result<Self> {
        // Room for a manual capture while a sample is in flight.
        let (jobs, inbox) = mpsc::sync_channel::<Job>(2);
        let (outbox, done) = mpsc::sync_channel(1);
        let throughput = Arc::new(Mutex::new(Throughput::default()));
        let stats = throughput.clone();
        let thread = std::thread::Builder::new()
            .name("autocapture-encoder".into())
            .spawn(move || {
                let mut backend = Backend::start(&encoder);
                let publish = |backend: &Backend, finished: bool| {
                    let mut stats = stats.lock().unwrap_or_else(|e| e.into_inner());
                    stats.accelerator = backend.accelerator();
                    if finished {
                        stats.record(Instant::now());
                    }
                };
                publish(&backend, false);
                for job in inbox {
                    match job {
                        Job::Sample(frame, at) => {
                            let feature = backend.describe(&encoder, &frame);
                            publish(&backend, feature.is_ok());
                            if outbox.send((frame, at, Instant::now(), feature)).is_err() {
                                break;
                            }
                        }
                        Job::Manual(frame, reply) => {
                            let feature = backend.describe(&encoder, &frame);
                            publish(&backend, false);
                            let _ = reply.send(feature);
                        }
                    }
                }
            })
            .context("start auto capture encoder thread")?;
        Ok(Self {
            jobs: Some(jobs),
            done,
            busy: false,
            thread: Some(thread),
            throughput,
        })
    }
    fn send(&self, job: Job) -> Result<()> {
        self.jobs
            .as_ref()
            .context("auto capture encoder stopped")?
            .send(job)
            .ok()
            .context("auto capture encoder stopped")
    }
    fn submit(&mut self, frame: Frame, at: Instant) -> Result<()> {
        self.send(Job::Sample(frame, at))?;
        self.busy = true;
        Ok(())
    }
    fn describe(&self, frame: &Frame) -> Result<Descriptor> {
        let (reply, result) = mpsc::channel();
        self.send(Job::Manual(frame.clone(), reply))?;
        result.recv().context("auto capture encoder stopped")?
    }
    fn finished(&mut self) -> Result<Option<Embedded>> {
        match self.done.try_recv() {
            Ok(result) => {
                self.busy = false;
                Ok(Some(result))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = false;
                anyhow::bail!("auto capture encoder stopped")
            }
        }
    }
    #[cfg(all(test, feature = "dinov2"))]
    fn wait(&mut self) -> Option<Embedded> {
        let result = self.busy.then(|| self.done.recv().ok()).flatten();
        self.busy = false;
        result
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        self.jobs.take();
        // Finish (at most) the frame in flight; its result is discarded.
        while self.done.try_recv().is_ok() {}
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Where the encoder thread computes embeddings. The GPU helper starts in the
/// background; frames use the CPU until it is ready, and for the rest of the
/// session if it is unavailable or misses a deadline.
enum Backend {
    Cpu,
    #[cfg(feature = "dinov2")]
    Starting(mpsc::Receiver<Result<dinov2::helper::Client>>),
    #[cfg(feature = "dinov2")]
    Gpu(dinov2::helper::Client),
}

impl Backend {
    fn start(encoder: &Encoder) -> Self {
        match encoder {
            // Unit tests cannot re-execute their own binary as a helper.
            #[cfg(feature = "dinov2")]
            Encoder::Dinov2(model) if !cfg!(test) && dinov2::helper::wanted() => {
                let (sender, receiver) = mpsc::channel();
                let model = model.clone();
                let started = std::thread::Builder::new()
                    .name("autocapture-gpu-start".into())
                    .spawn(move || {
                        let _ = sender.send(dinov2::helper::Client::start(&model));
                    });
                if started.is_ok() {
                    Self::Starting(receiver)
                } else {
                    Self::Cpu
                }
            }
            _ => Self::Cpu,
        }
    }

    fn describe(&mut self, encoder: &Encoder, frame: &Frame) -> Result<Descriptor> {
        #[cfg(feature = "dinov2")]
        if let Encoder::Dinov2(model) = encoder {
            if let Self::Starting(receiver) = self {
                match receiver.try_recv() {
                    Ok(Ok(client)) => *self = Self::Gpu(client),
                    Ok(Err(error)) => {
                        eprintln!("capturefab: DINOv2 GPU unavailable ({error:#}); using the CPU");
                        *self = Self::Cpu;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => *self = Self::Cpu,
                }
            }
            ensure!(
                frame.data.len() <= MAX_FRAME_BYTES,
                "frame exceeds 256 MiB session capture limit"
            );
            let rgb = dinov2::prepare(frame, model.input())?;
            if let Self::Gpu(client) = self {
                match client.embed(rgb.clone(), Duration::from_secs(2)) {
                    Ok(embedding) => return Ok(embedding),
                    Err(error) => {
                        // A GPU that faults or hangs is not trusted again.
                        eprintln!("capturefab: DINOv2 GPU disabled ({error:#}); using the CPU");
                        *self = Self::Cpu;
                    }
                }
            }
            return model.embed_rgb(&rgb);
        }
        encoder.describe(frame)
    }

    fn accelerator(&self) -> Option<String> {
        match self {
            Self::Cpu => Some("CPU".into()),
            #[cfg(feature = "dinov2")]
            Self::Starting(_) => Some("CPU (starting GPU)".into()),
            #[cfg(feature = "dinov2")]
            Self::Gpu(client) => Some(format!("GPU: {}", client.adapter())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewCapture {
    pub id: u64,
    pub frame: Frame,
    pub score: f64,
    pub reason: String,
}

pub struct CaptureSession {
    db: Connection,
    path: PathBuf,
    camera: String,
    encoder: Encoder,
    embedder: Option<Embedder>,
    enabled: bool,
    sensitivity: f64,
    cooldown_ms: u64,
    next_id: u64,
    positive: Vec<Descriptor>,
    negative: Vec<Descriptor>,
    background: Vec<Descriptor>,
    recent: Vec<(Descriptor, u64)>,
    last_sample: Option<Instant>,
    last_capture: Option<Instant>,
    started_at: Instant,
    started_ms: u64,
    blocked_storage: bool,
    /// When auto capture was last paused or re-armed. A background embedding
    /// sampled before then belongs to a scene the user stopped watching.
    armed_at: Instant,
}

impl CaptureSession {
    /// Create a new session or reopen one without changing its settings or labels.
    /// A session is bound to one camera identity to avoid mixing unrelated scenes.
    /// `encoder` chooses the encoder of a new session; a resumed session
    /// always keeps its own.
    pub fn open(
        path: &Path,
        camera: &str,
        create: bool,
        encoder: Option<EncoderChoice>,
    ) -> Result<Self> {
        ensure!(
            create || encoder.is_none(),
            "the encoder is fixed when a capture session is created"
        );
        ensure!(
            !camera.trim().is_empty(),
            "capture session requires a camera identity"
        );
        ensure!(camera.len() <= 1024, "camera identity is too long");
        if create {
            ensure!(
                !path.exists(),
                "capture session already exists; resume it instead"
            );
        } else {
            ensure!(
                path.is_file(),
                "capture session does not exist: {}",
                path.display()
            );
        }
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        // Resolve a new session's encoder before creating its file, so a
        // missing or broken model leaves nothing behind.
        let created = create.then(|| new_encoder(encoder)).transpose()?;
        let config = Config::default()
            .max_memory("256MB")?
            .threads(1)?
            .enable_autoload_extension(false)?;
        let db =
            Connection::open_with_flags(&path, config).context("open capture session DuckDB")?;
        // Frames are BLOBs; never ask DuckDB to load extensions, read external
        // data, or spill a second session data file beside the database.
        db.execute_batch("SET temp_directory = ''; SET enable_external_access = false;")?;
        if let Some(encoder) = &created {
            let fingerprint = encoder.fingerprint();
            db.execute_batch(
                "CREATE TABLE capture_session (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    schema_version UINTEGER NOT NULL, descriptor_version UINTEGER NOT NULL,
                    camera VARCHAR NOT NULL, created_ms UBIGINT NOT NULL,
                    enabled BOOLEAN NOT NULL, sensitivity DOUBLE NOT NULL, cooldown_ms UBIGINT NOT NULL,
                    encoder VARCHAR NOT NULL, descriptor_dims UINTEGER NOT NULL,
                    model_fingerprint VARCHAR
                );
                CREATE TABLE captures (
                    id UBIGINT PRIMARY KEY, created_ms UBIGINT NOT NULL,
                    source VARCHAR NOT NULL CHECK (source IN ('manual', 'automatic')),
                    label VARCHAR NOT NULL CHECK (label IN ('manual', 'pending', 'kept', 'discarded')),
                    frame_id UBIGINT NOT NULL, width UINTEGER NOT NULL, height UINTEGER NOT NULL,
                    pixel_format UINTEGER NOT NULL, timestamp_ns UBIGINT NOT NULL,
                    data BLOB, data_bytes UBIGINT NOT NULL, descriptor BLOB NOT NULL, score DOUBLE NOT NULL,
                    reason VARCHAR NOT NULL, reviewed_ms UBIGINT
                );
                CREATE TABLE background (id UINTEGER PRIMARY KEY, descriptor BLOB NOT NULL);"
            )?;
            db.execute(
                "INSERT INTO capture_session VALUES (1, ?, ?, ?, ?, false, 0.5, 2000, ?, ?, ?)",
                params![
                    SCHEMA_VERSION,
                    encoder.version(),
                    camera,
                    epoch_ms(),
                    encoder.name(),
                    encoder.dims() as u32,
                    fingerprint
                ],
            )?;
            db.execute_batch("CHECKPOINT")?;
        }
        let version: u32 = db
            .query_row(
                "SELECT schema_version FROM capture_session WHERE singleton = 1",
                [],
                |r| r.get(0),
            )
            .context("not a CaptureFab capture session or missing session metadata")?;
        ensure!(
            version == 1 || version == SCHEMA_VERSION,
            "unsupported capture session schema version {version}"
        );
        if version == 1 {
            // Version 1 sessions predate encoder choice: all are native.
            db.execute_batch(
                "BEGIN TRANSACTION;
                ALTER TABLE capture_session ADD COLUMN encoder VARCHAR DEFAULT 'native';
                ALTER TABLE capture_session ADD COLUMN descriptor_dims UINTEGER DEFAULT 84;
                ALTER TABLE capture_session ADD COLUMN model_fingerprint VARCHAR;
                UPDATE capture_session SET schema_version = 2;
                COMMIT;",
            )
            .context("upgrade capture session to schema version 2")?;
        }
        #[allow(clippy::type_complexity)]
        let (descriptor_version, stored_camera, enabled, sensitivity, cooldown_ms, name, dims, fingerprint): (u32, String, bool, f64, u64, String, u32, Option<String>) = db.query_row(
            "SELECT descriptor_version, camera, enabled, sensitivity, cooldown_ms, encoder, descriptor_dims, model_fingerprint FROM capture_session WHERE singleton = 1", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))
            .context("not a CaptureFab capture session or missing session metadata")?;
        ensure!(
            stored_camera == camera,
            "capture session belongs to camera {stored_camera}, not {camera}"
        );
        let encoder = match created {
            Some(encoder) => encoder,
            None => stored_encoder(&name, dims as usize, fingerprint.as_deref())?,
        };
        ensure!(
            descriptor_version == encoder.version(),
            "unsupported capture session descriptor version {descriptor_version}"
        );
        validate_settings(sensitivity, cooldown_ms)?;
        let mut positive = load_descriptors(
            &db,
            &encoder,
            "SELECT descriptor FROM captures WHERE label IN ('manual', 'kept') ORDER BY id DESC LIMIT 256",
        )?;
        let mut negative = load_descriptors(
            &db,
            &encoder,
            "SELECT descriptor FROM captures WHERE label = 'discarded' ORDER BY id DESC LIMIT 256",
        )?;
        let background = load_descriptors(
            &db,
            &encoder,
            "SELECT descriptor FROM background ORDER BY id LIMIT 64",
        )?;
        positive.reverse();
        negative.reverse();
        let mut recent = Vec::new();
        {
            let mut statement = db.prepare("SELECT descriptor, created_ms FROM captures WHERE label <> 'discarded' ORDER BY id DESC LIMIT 32")?;
            let rows =
                statement.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u64>(1)?)))?;
            for row in rows {
                let (bytes, created_ms) = row?;
                recent.push((encoder.decode(&bytes)?, created_ms));
            }
            recent.reverse();
        }
        let (last_id, last_ms): (Option<u64>, Option<u64>) =
            db.query_row("SELECT MAX(id), MAX(created_ms) FROM captures", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let now = Instant::now();
        let started_ms = epoch_ms();
        let last_capture = last_ms
            .and_then(|ms| now.checked_sub(Duration::from_millis(epoch_ms().saturating_sub(ms))));
        let embedder = encoder
            .background()
            .then(|| Embedder::spawn(encoder.clone()))
            .transpose()?;
        Ok(Self {
            db,
            path,
            camera: stored_camera,
            encoder,
            embedder,
            enabled,
            sensitivity,
            cooldown_ms,
            next_id: last_id
                .unwrap_or(0)
                .checked_add(1)
                .context("capture session identifier overflow")?,
            positive,
            negative,
            background,
            recent,
            last_sample: None,
            last_capture,
            started_at: now,
            started_ms,
            blocked_storage: false,
            armed_at: now,
        })
    }

    pub fn configure(&mut self, enabled: bool, sensitivity: f64, cooldown_ms: u64) -> Result<()> {
        validate_settings(sensitivity, cooldown_ms)?;
        self.db.execute("UPDATE capture_session SET enabled = ?, sensitivity = ?, cooldown_ms = ? WHERE singleton = 1", params![enabled, sensitivity, cooldown_ms])?;
        if !enabled || !self.enabled {
            self.armed_at = Instant::now();
        }
        self.enabled = enabled;
        self.sensitivity = sensitivity;
        self.cooldown_ms = cooldown_ms;
        self.checkpoint()?;
        Ok(())
    }

    pub fn status(&self) -> Result<SessionStatus> {
        let (manual_count, kept_count, discarded_count, pending_count, bytes) = self.counts()?;
        let state = if !self.enabled {
            "paused"
        } else if pending_count >= MAX_PENDING {
            "review_queue_full"
        } else if self.blocked_storage || bytes >= MAX_STORED_BYTES {
            "storage_full"
        } else if self.positive.is_empty() {
            "learning"
        } else {
            "watching"
        };
        Ok(SessionStatus {
            path: self.path.to_string_lossy().into_owned(),
            camera: self.camera.clone(),
            enabled: self.enabled,
            sensitivity: self.sensitivity,
            cooldown_ms: self.cooldown_ms,
            manual_count,
            kept_count,
            discarded_count,
            pending_count,
            blocked_storage: self.blocked_storage || bytes >= MAX_STORED_BYTES,
            state: state.into(),
            model: self.encoder.label(),
            encoder: self.encoder.name().into(),
            accelerator: self.embedder.as_ref().and_then(|e| {
                e.throughput
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .accelerator
                    .clone()
            }),
            samples_per_second: self.embedder.as_ref().and_then(|e| {
                e.throughput
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rate()
                    .map(|rate| (rate * 10.0).round() / 10.0)
            }),
        })
    }

    /// Evaluate at most four small previews per second. Automatic suggestions do
    /// not become training examples until the user reviews them.
    pub fn observe(&mut self, frame: &Frame) -> Result<Option<u64>> {
        self.observe_at(frame, Instant::now())
    }

    fn observe_at(&mut self, frame: &Frame, now: Instant) -> Result<Option<u64>> {
        if !self.enabled {
            return Ok(None);
        }
        let mut captured = self.poll()?;
        // Samples sit on a fixed grid (30 Hz for DINOv2): a frame that arrives
        // up to a quarter interval early still counts, so camera jitter does not
        // cost every other frame, while the long-run rate never exceeds the grid.
        let interval = self.encoder.metric().interval;
        if self
            .last_sample
            .is_some_and(|last| now + interval / 4 < last + interval)
            || self.embedder.as_ref().is_some_and(|e| e.busy)
        {
            return Ok(captured);
        }
        self.last_sample = Some(match self.last_sample {
            Some(last) if now.saturating_duration_since(last) < 2 * interval => last + interval,
            _ => now,
        });
        if !self.has_room(frame)? {
            return Ok(captured);
        }
        match &mut self.embedder {
            Some(embedder) => embedder.submit(frame.clone(), now)?,
            None => {
                let feature = self.encoder.describe(frame)?;
                if let Some(id) = self.decide(frame, feature, now)? {
                    captured = Some(id);
                }
            }
        }
        Ok(captured)
    }

    /// Whether another pending capture fits the review queue and pixel budget.
    fn has_room(&mut self, frame: &Frame) -> Result<bool> {
        let (_, _, _, pending, bytes) = self.counts()?;
        self.blocked_storage = bytes.saturating_add(frame.data.len() as u64) > MAX_STORED_BYTES;
        Ok(pending < MAX_PENDING && !self.blocked_storage)
    }

    /// Decide on a background embedding that has finished, on the frame it
    /// was computed from. Called for every frame and from the worker's idle
    /// loop, so slow or triggered cameras need no later frame to capture.
    pub fn poll(&mut self) -> Result<Option<u64>> {
        if !self.enabled {
            return Ok(None);
        }
        match self
            .embedder
            .as_mut()
            .map(Embedder::finished)
            .transpose()?
            .flatten()
        {
            Some((frame, sampled, finished, feature)) => {
                self.decide_late(&frame, feature?, sampled, finished)
            }
            None => Ok(None),
        }
    }

    /// An embedding that took too long belongs to a scene that has moved on.
    fn decide_late(
        &mut self,
        frame: &Frame,
        feature: Descriptor,
        sampled: Instant,
        finished: Instant,
    ) -> Result<Option<u64>> {
        if sampled < self.armed_at
            || finished.saturating_duration_since(sampled) > STALE_SAMPLE
            || !self.has_room(frame)?
        {
            return Ok(None);
        }
        self.decide(frame, feature, sampled)
    }

    fn decide(&mut self, frame: &Frame, feature: Descriptor, now: Instant) -> Result<Option<u64>> {
        let metric = self.encoder.metric();
        let (score, reason) = self.score(&feature);
        let now_ms = self.started_ms.saturating_add(
            now.saturating_duration_since(self.started_at)
                .as_millis()
                .min(u64::MAX as u128) as u64,
        );
        let duplicate_window_ms = self.cooldown_ms.saturating_mul(3).clamp(1000, 30_000);
        self.recent
            .retain(|(_, created_ms)| now_ms.saturating_sub(*created_ms) < duplicate_window_ms);
        let cooling = self.last_capture.is_some_and(|last| {
            now.saturating_duration_since(last) < Duration::from_millis(self.cooldown_ms)
        });
        let duplicate = self
            .recent
            .iter()
            .any(|(previous, _)| self.encoder.distance(previous, &feature) < metric.duplicate);
        // Even the strict endpoint admits an exact positive with no observed
        // background (minimum exact-match score is 0.90).
        let threshold = 0.90 - 0.35 * self.sensitivity;
        if !self.positive.is_empty() && !cooling && !duplicate && score >= threshold {
            let id = self.insert(frame, &feature, "automatic", "pending", score, &reason)?;
            self.last_capture = Some(now);
            push_bounded(&mut self.recent, (feature, now_ms), MAX_RECENT);
            self.checkpoint()?;
            return Ok(Some(id));
        }
        // Do not absorb a likely target into normal background. Persist only
        // distinct prototypes, keeping both work and on-disk model size bounded.
        if self.background.len() < MAX_BACKGROUND && (self.positive.is_empty() || score < threshold)
        {
            let distinct = self
                .background
                .iter()
                .all(|previous| self.encoder.distance(previous, &feature) >= metric.distinct);
            if distinct {
                self.db.execute(
                    "INSERT INTO background VALUES (?, ?)",
                    params![self.background.len() as u32, encode(&feature)],
                )?;
                self.background.push(feature);
            }
        }
        Ok(None)
    }

    /// Store the exact user-selected sensor frame and immediately teach the
    /// session that this is an interesting example, even while auto mode is off.
    pub fn manual(&mut self, frame: &Frame) -> Result<u64> {
        let feature = match &self.embedder {
            Some(embedder) => embedder.describe(frame)?,
            None => self.encoder.describe(frame)?,
        };
        let id = self.insert(
            frame,
            &feature,
            "manual",
            "manual",
            1.0,
            "User-initiated capture: positive training example",
        )?;
        self.reload_examples()?;
        push_bounded(&mut self.recent, (feature, epoch_ms()), MAX_RECENT);
        self.last_capture = Some(Instant::now());
        self.checkpoint()?;
        Ok(id)
    }

    /// A pending-only transaction prevents a double key press or stale review
    /// card from relabeling a capture. Discarding releases raw pixels while
    /// keeping the descriptor and negative decision for future learning.
    pub fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let bytes: Option<Vec<u8>> = tx
            .query_row(
                "SELECT descriptor FROM captures WHERE id = ? AND label = 'pending'",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        self.encoder
            .decode(&bytes.context("capture is no longer pending review")?)?;
        let changed = tx.execute("UPDATE captures SET label = ?, reviewed_ms = ?, data = CASE WHEN ? THEN data ELSE NULL END, data_bytes = CASE WHEN ? THEN data_bytes ELSE 0 END WHERE id = ? AND label = 'pending'",
            params![if keep { "kept" } else { "discarded" }, epoch_ms(), keep, keep, id])?;
        ensure!(changed == 1, "capture is no longer pending review");
        tx.commit()?;
        self.reload_examples()?;
        if !keep {
            self.blocked_storage = false;
        }
        self.checkpoint()?;
        Ok(())
    }

    pub fn next_review(&self) -> Result<Option<ReviewCapture>> {
        self.load_review(None)
    }

    pub fn review_capture(&self, id: u64) -> Result<Option<ReviewCapture>> {
        self.load_review(Some(id))
    }

    /// Flush DuckDB's recovery log into the session file before transferring it
    /// or explicitly closing it. Callers can retain the live session on error.
    pub fn checkpoint(&self) -> Result<()> {
        self.db
            .execute_batch("CHECKPOINT")
            .context("checkpoint capture session into its portable DuckDB file")
    }

    fn reload_examples(&mut self) -> Result<()> {
        let mut positive = load_descriptors(
            &self.db,
            &self.encoder,
            "SELECT descriptor FROM captures WHERE label IN ('manual', 'kept') ORDER BY id DESC LIMIT 256",
        )?;
        let mut negative = load_descriptors(
            &self.db,
            &self.encoder,
            "SELECT descriptor FROM captures WHERE label = 'discarded' ORDER BY id DESC LIMIT 256",
        )?;
        positive.reverse();
        negative.reverse();
        self.positive = positive;
        self.negative = negative;
        Ok(())
    }

    fn load_review(&self, id: Option<u64>) -> Result<Option<ReviewCapture>> {
        let result = self.db.query_row(
            "SELECT id, frame_id, width, height, pixel_format, timestamp_ns, data, score, reason FROM captures
             WHERE label = 'pending' AND (?::UBIGINT IS NULL OR id = ?::UBIGINT) ORDER BY id LIMIT 1",
            params![id, id], |r| Ok(ReviewCapture { id: r.get(0)?, frame: Frame { id:r.get(1)?,width:r.get(2)?,height:r.get(3)?,pixel_format:r.get(4)?,timestamp_ns:r.get(5)?,data:r.get(6)? },score:r.get(7)?,reason:r.get(8)? })).optional()?;
        if let Some(capture) = &result {
            // Validate metadata before callers allocate a UI image for a file
            // which may have been modified outside CaptureFab.
            ensure!(
                capture.frame.data.len() <= MAX_FRAME_BYTES,
                "stored frame exceeds capture size limit"
            );
            descriptor(&capture.frame).context("corrupt capture frame in session")?;
            ensure!(capture.score.is_finite(), "invalid stored capture score");
        }
        Ok(result)
    }

    fn counts(&self) -> Result<(u64, u64, u64, u64, u64)> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FILTER (WHERE label = 'manual'), COUNT(*) FILTER (WHERE label = 'kept'),
                    COUNT(*) FILTER (WHERE label = 'discarded'), COUNT(*) FILTER (WHERE label = 'pending'),
                    COALESCE(SUM(data_bytes), 0)::UBIGINT FROM captures", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?)
    }

    fn insert(
        &mut self,
        frame: &Frame,
        feature: &Descriptor,
        source: &str,
        label: &str,
        score: f64,
        reason: &str,
    ) -> Result<u64> {
        ensure!(
            frame.data.len() <= MAX_FRAME_BYTES,
            "frame exceeds 256 MiB session capture limit"
        );
        let (_, _, _, _, bytes) = self.counts()?;
        self.blocked_storage = bytes.saturating_add(frame.data.len() as u64) > MAX_STORED_BYTES;
        ensure!(
            !self.blocked_storage,
            "capture session reached its 2 GiB pixel storage limit; start another session or discard pending captures"
        );
        let next_id = self
            .next_id
            .checked_add(1)
            .context("capture session identifier overflow")?;
        self.db.execute(
            "INSERT INTO captures VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
            params![
                self.next_id,
                epoch_ms(),
                source,
                label,
                frame.id,
                frame.width,
                frame.height,
                frame.pixel_format,
                frame.timestamp_ns,
                &frame.data,
                frame.data.len() as u64,
                encode(feature),
                score,
                reason
            ],
        )?;
        let id = self.next_id;
        self.next_id = next_id;
        Ok(id)
    }

    fn score(&self, feature: &Descriptor) -> (f64, String) {
        let Some(positive_distance) = self.encoder.nearest(&self.positive, feature) else {
            return (0.0, "Waiting for a user-initiated capture".into());
        };
        let affinity = self.encoder.similarity(positive_distance);
        let preference = match self.encoder.nearest(&self.negative, feature) {
            Some(negative_distance) => {
                // Distance-ratio preference makes an exact rejection dominate
                // nearby positive examples without suppressing an exact human
                // positive just because a rejected variant was nearby.
                let total = positive_distance + negative_distance;
                let ratio = if total <= f64::EPSILON {
                    0.5
                } else {
                    negative_distance / total
                };
                affinity * ratio
            }
            None => affinity,
        };
        let novelty = self
            .encoder
            .nearest(&self.background, feature)
            .map(|d| (d / self.encoder.metric().novelty).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        // Human preference dominates. Novelty can promote an unusual variant
        // of something the user showed, never an unrelated frame on its own.
        let score = (preference * (0.9 + 0.1 * novelty)).clamp(0.0, 1.0);
        (
            score,
            format!(
                "{:.0}% match to your captures; {:.0}% background novelty{}",
                affinity * 100.0,
                novelty * 100.0,
                if self.negative.is_empty() {
                    ""
                } else {
                    "; adjusted by discarded examples"
                }
            ),
        )
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // A cleanly closed session is one portable file, including all labels
        // and raw pixels. DuckDB's WAL protects interrupted writes while open.
        let _ = self.checkpoint();
    }
}

fn validate_settings(sensitivity: f64, cooldown_ms: u64) -> Result<()> {
    ensure!(
        sensitivity.is_finite() && (0.0..=1.0).contains(&sensitivity),
        "auto capture sensitivity must be between 0 and 1"
    );
    ensure!(
        (250..=3_600_000).contains(&cooldown_ms),
        "auto capture cooldown must be between 250 and 3600000 ms"
    );
    Ok(())
}
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn push_bounded<T>(items: &mut Vec<T>, feature: T, limit: usize) {
    if items.len() >= limit {
        items.remove(0);
    }
    items.push(feature);
}
fn encode(feature: &Descriptor) -> Vec<u8> {
    feature
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}
fn load_descriptors(db: &Connection, encoder: &Encoder, query: &str) -> Result<Vec<Descriptor>> {
    let mut statement = db.prepare(query)?;
    let rows = statement.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
    rows.map(|row| encoder.decode(&row?)).collect()
}

/// Bounded, CPU-native appearance descriptor: 4x4 spatial RGB means, smooth
/// channel histograms, edge directions, contrast, brightness and aspect ratio.
/// Features retain absolute color/brightness since those can be the anomaly.
fn descriptor(frame: &Frame) -> Result<Descriptor> {
    ensure!(
        frame.data.len() <= MAX_FRAME_BYTES,
        "frame exceeds 256 MiB session capture limit"
    );
    let (w, h, rgba) = frame::preview_rgba(frame, 64, 64)?;
    ensure!(
        w > 0 && h > 0 && w <= 64 && h <= 64,
        "invalid descriptor preview dimensions"
    );
    let (w, h) = (w as usize, h as usize);
    let mut feature = vec![0.0_f32; FEATURES];
    let mut cells = [0_u32; 16];
    let mut luma = vec![0.0_f32; w * h];
    let (mut sum, mut squares) = (0.0, 0.0);
    for (i, pixel) in rgba.as_chunks::<4>().0.iter().enumerate() {
        let cell = ((i / w) * 4 / h) * 4 + (i % w) * 4 / w;
        cells[cell] += 1;
        for (channel, &byte) in pixel[..3].iter().enumerate() {
            let value = f32::from(byte) / 255.0;
            feature[cell * 3 + channel] += value;
            let position = value * 7.0;
            let lower = position.floor() as usize;
            let fraction = position - lower as f32;
            feature[48 + channel * 8 + lower] += 1.0 - fraction;
            if lower < 7 {
                feature[48 + channel * 8 + lower + 1] += fraction;
            }
        }
        let y = (0.2126 * f32::from(pixel[0])
            + 0.7152 * f32::from(pixel[1])
            + 0.0722 * f32::from(pixel[2]))
            / 255.0;
        luma[i] = y;
        sum += y;
        squares += y * y;
    }
    for (cell, &count) in cells.iter().enumerate() {
        if count > 0 {
            for channel in 0..3 {
                feature[cell * 3 + channel] /= count as f32;
            }
        }
    }
    let n = (w * h) as f32;
    for value in &mut feature[48..72] {
        // Histograms complement spatial means. Full-strength bins would make
        // a small global exposure change outweigh a strong spatial match.
        *value *= 0.4 / n;
    }
    let mut gradient_sum = 0.0;
    for y in 0..h.saturating_sub(1) {
        for x in 0..w.saturating_sub(1) {
            let i = y * w + x;
            let gx = luma[i + 1] - luma[i];
            let gy = luma[i + w] - luma[i];
            let magnitude = (gx * gx + gy * gy).sqrt();
            let direction = ((gy.atan2(gx) + std::f32::consts::PI) / (2.0 * std::f32::consts::PI)
                * 8.0) as usize
                % 8;
            feature[72 + direction] += magnitude;
            gradient_sum += magnitude;
        }
    }
    if gradient_sum > 0.0 {
        for value in &mut feature[72..80] {
            *value /= gradient_sum;
        }
    }
    let mean = sum / n;
    feature[80] = mean.clamp(0.0, 1.0);
    feature[81] = ((squares / n - mean * mean).max(0.0).sqrt() * 2.0).clamp(0.0, 1.0);
    feature[82] = (gradient_sum / n).clamp(0.0, 1.0);
    feature[83] = (frame.width as f64 / (f64::from(frame.width) + f64::from(frame.height))) as f32;
    Ok(feature)
}

#[cfg(test)]
mod tests;
