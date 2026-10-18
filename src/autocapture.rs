//! Session-local, human-supervised automatic capture. A session is a DuckDB
//! database plus a `<name>.frames` folder beside it: the database holds
//! decisions, settings, descriptors and the bounded background model, and each
//! raw sensor frame is a file written through the storage ledger. Copy the
//! database and its folder together. No network service or download is
//! needed. Frames are compared as embeddings from the bundled DINOv2 model (on
//! the GPU when one is usable), or as a native color/edge descriptor in builds
//! without it; each session records which one it learned.
#[cfg(feature = "dinov2")]
pub mod dinov2;

use crate::{frame, storage, types::Frame};
use anyhow::{Context, Result, anyhow, bail, ensure};
use duckdb::{Config, Connection, OptionalExt, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        mpsc::{self, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// 2 added the encoder; 3 keeps frames as files beside the database.
const SCHEMA_VERSION: u32 = 3;
const FEATURES: usize = 84;
/// A background embedding that finishes this long after its frame, and over
/// twice as long as embeddings usually take, is dropped: the scene has moved
/// on, as when a GPU missed its deadline. A computer whose every embedding is
/// slower still decides on each.
const STALE_SAMPLE: Duration = Duration::from_secs(2);
const MAX_PENDING: u64 = 200;
const MAX_STORED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_BACKGROUND: usize = 64;
const MAX_RECENT: usize = 32;
/// Frames live in files, so DuckDB only ever holds small rows.
const MEMORY_LIMIT: &str = "64MB";
/// Sessions from schema 1 and 2 keep raw pixels in the database, with its old budget.
const LEGACY_MEMORY_LIMIT: &str = "256MB";
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);
/// How long a storage refusal pauses suggestions before the next attempt.
const STORAGE_RETRY: Duration = Duration::from_secs(60);
/// How long suggestions wait after a frame failed to save for another
/// reason, such as a ledger busy with another process's bookkeeping.
const WRITE_RETRY: Duration = Duration::from_secs(5);
/// How long a note stays in the status.
const NOTE_LIFETIME: Duration = Duration::from_secs(60);
const BUSY_NOTE: &str = "Skipped a suggestion while the previous capture was still being saved";
/// The share of one core that embedding sampled frames on the CPU may use on
/// average, normally and in efficiency mode. A GPU embeds at the camera's pace.
const CPU_SHARE: f64 = 0.5;
const EFFICIENT_CPU_SHARE: f64 = 0.2;
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
    /// Frames embedded per second over the last few samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples_per_second: Option<f64>,
    /// Why the latest suggestion was skipped or a capture was dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
        // Three seconds of samples, but the latest two for up to a minute,
        // so sampling paced to the CPU still has a rate.
        while self.finished.front().is_some_and(|t| {
            let age = now.saturating_duration_since(*t);
            age > Duration::from_secs(60)
                || (age > Duration::from_secs(3) && self.finished.len() > 2)
        }) {
            self.finished.pop_front();
        }
    }
    fn rate(&self) -> Option<f64> {
        let (first, last) = (self.finished.front()?, self.finished.back()?);
        let span = last.saturating_duration_since(*first).as_secs_f64();
        let samples = self.finished.len().saturating_sub(1) as f64;
        // Sampling has stopped once it misses twice its usual spacing.
        let quiet = Duration::from_secs_f64((2.0 * span / samples.max(1.0)).max(1.0));
        (samples >= 1.0 && span > 0.0 && last.elapsed() < quiet).then(|| samples / span)
    }
}

/// One encoder thread per DINOv2 session. At most one sampled frame is in
/// flight, so the worker holds one extra frame copy and sampling slows to the
/// encoder's speed instead of queueing. On the CPU, sampling also rests after
/// each frame, so embedding never takes over the computer.
struct Embedder {
    jobs: Option<mpsc::SyncSender<Job>>,
    done: mpsc::Receiver<(Embedded, Duration)>,
    busy: bool,
    /// When the CPU has rested long enough after the last sample.
    rested: Option<Instant>,
    /// How long samples usually take to embed, as a moving average.
    usual: Option<Duration>,
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
                            let (started, before) = (Instant::now(), process_cpu());
                            let feature = backend.describe(&encoder, &frame);
                            let took = started.elapsed();
                            let used = before
                                .zip(process_cpu())
                                .map(|(before, after)| after.saturating_sub(before));
                            let cpu = backend.cpu_time(took, used);
                            let rest = rest_after(cpu, took, cpu_share());
                            publish(&backend, feature.is_ok());
                            let embedded = (frame, at, Instant::now(), feature);
                            if outbox.send((embedded, rest)).is_err() {
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
            rested: None,
            usual: None,
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
    /// Whether a new sample must wait for the one in flight, or for the CPU
    /// to rest after the last one.
    fn waiting(&self, now: Instant) -> bool {
        self.busy || self.rested.is_some_and(|rested| now < rested)
    }
    fn describe(&self, frame: &Frame) -> Result<Descriptor> {
        let (reply, result) = mpsc::channel();
        self.send(Job::Manual(frame.clone(), reply))?;
        result.recv().context("auto capture encoder stopped")?
    }
    fn finished(&mut self) -> Result<Option<Embedded>> {
        match self.done.try_recv() {
            Ok(result) => Ok(Some(self.landed(result))),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = false;
                anyhow::bail!("auto capture encoder stopped")
            }
        }
    }
    fn landed(&mut self, (embedded, rest): (Embedded, Duration)) -> Embedded {
        self.busy = false;
        self.rested = Some(embedded.2 + rest);
        let took = embedded.2.saturating_duration_since(embedded.1);
        self.usual = Some(self.usual.map_or(took, |usual| (usual * 3 + took) / 4));
        embedded
    }
    /// How long after its frame a result is stale.
    fn stale_after(&self) -> Duration {
        self.usual
            .map_or(STALE_SAMPLE, |usual| STALE_SAMPLE.max(usual * 2))
    }
    #[cfg(all(test, feature = "dinov2"))]
    fn wait(&mut self) -> Option<Embedded> {
        let result = self.busy.then(|| self.done.recv().ok()).flatten();
        self.busy = false;
        result.map(|result| self.landed(result))
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

/// The share of one core CPU embeddings may use now.
fn cpu_share() -> f64 {
    if crate::efficiency::efficient() {
        EFFICIENT_CPU_SHARE
    } else {
        CPU_SHARE
    }
}

/// How long sampling rests after an embedding that took `took` and used
/// `cpu` of processor time, so that embedding averages `share` of one core.
fn rest_after(cpu: Duration, took: Duration, share: f64) -> Duration {
    cpu.div_f64(share).saturating_sub(took)
}

/// Threads a CPU forward pass may keep busy: matrixmultiply's pool takes
/// MATMUL_NUM_THREADS, or one thread per physical core, at most four.
/// Counting logical cores can only overstate the cost.
fn cpu_threads() -> u32 {
    static THREADS: OnceLock<u32> = OnceLock::new();
    *THREADS.get_or_init(|| {
        std::env::var("MATMUL_NUM_THREADS")
            .ok()
            .and_then(|threads| threads.trim().parse().ok())
            .unwrap_or(crate::efficiency::host().cpu_cores as u32)
            .clamp(1, 4)
    })
}

/// Processor time this process has used.
#[cfg(unix)]
#[allow(unsafe_code)]
fn process_cpu() -> Option<Duration> {
    let mut used = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec it is given.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut used) };
    (status == 0).then(|| Duration::new(used.tv_sec as u64, used.tv_nsec as u32))
}
#[cfg(not(unix))]
fn process_cpu() -> Option<Duration> {
    None
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

    /// Processor time of an embedding that just took `took`, while the
    /// worker `used` this much: none on the GPU. On the CPU that counts every
    /// thread of the forward pass, and other worker threads can only
    /// overstate it; without a measurement every thread is assumed busy.
    fn cpu_time(&self, took: Duration, used: Option<Duration>) -> Duration {
        match self {
            #[cfg(feature = "dinov2")]
            Self::Gpu(_) => Duration::ZERO,
            _ => {
                let most = took.saturating_mul(cpu_threads());
                used.map_or(most, |used| used.min(most))
            }
        }
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
    writer: Writer,
    shared: Arc<Mutex<Shared>>,
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
    /// When storage last refused a suggested frame.
    refused: Option<Instant>,
    /// When a suggested frame last failed to save for another reason.
    unsaved: Option<Instant>,
}

/// State shared by the camera thread and the writer. Counts are reserved when
/// a capture is queued and released if its write fails, so the limits also
/// cover frames still being written, and status never queries DuckDB.
#[derive(Default)]
struct Shared {
    tally: Tally,
    /// Tasks waiting for the writer; it runs one more while one waits.
    queued: usize,
    /// A queued write's failure, reported by the next sample.
    failure: Option<anyhow::Error>,
    /// The latest note and when it was made; it fades after NOTE_LIFETIME.
    note: Option<(Instant, String)>,
}

impl Shared {
    fn note(&mut self, note: String) {
        self.note = Some((Instant::now(), note));
    }
}

/// Context for a frame file storage could not save. Storage can recover,
/// so a suggestion that fails this way is retried instead of pausing.
#[derive(Debug)]
struct FrameNotSaved;

impl std::fmt::Display for FrameNotSaved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("save capture frame file")
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    manual: u64,
    kept: u64,
    discarded: u64,
    pending: u64,
    bytes: u64,
}

impl Tally {
    fn count(&mut self, label: &str) -> &mut u64 {
        match label {
            "manual" => &mut self.manual,
            "kept" => &mut self.kept,
            "discarded" => &mut self.discarded,
            _ => &mut self.pending,
        }
    }
    fn add(&mut self, label: &str, bytes: u64) {
        *self.count(label) += 1;
        self.bytes = self.bytes.saturating_add(bytes);
    }
    fn remove(&mut self, label: &str, bytes: u64) {
        let count = self.count(label);
        *count = count.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(bytes);
    }
}

/// A capture handed to the writer; its counts are already reserved.
struct Row {
    id: u64,
    created_ms: u64,
    source: &'static str,
    label: &'static str,
    frame: Frame,
    feature: Descriptor,
    score: f64,
    reason: String,
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
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if let Some(parent) = path.parent() {
            crate::volumes::ensure_present(parent)?;
        }
        let frames = path.with_extension("frames");
        ensure!(
            frames != path,
            "capture session file name must not end in .frames"
        );
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
        // Resolve a new session's encoder before creating its file, so a
        // missing or broken model leaves nothing behind.
        let created = create.then(|| new_encoder(encoder)).transpose()?;
        if create {
            // Claim the folder before the database, so sessions whose names
            // differ only in extension never share one.
            claim(&frames)?;
        }
        let session = Self::load(path, &frames, camera, created);
        // A failed `new` leaves no folder behind to block the next attempt.
        if create && session.is_err() {
            let _ = std::fs::remove_dir(&frames);
        }
        session
    }

    fn load(path: PathBuf, frames: &Path, camera: &str, created: Option<Encoder>) -> Result<Self> {
        let config = Config::default()
            .max_memory(MEMORY_LIMIT)?
            .threads(1)?
            .enable_autoload_extension(false)?;
        let db =
            Connection::open_with_flags(&path, config).context("open capture session DuckDB")?;
        // Never ask DuckDB to load extensions, read external data, or spill a
        // second session data file beside the database.
        db.execute_batch("SET temp_directory = ''; SET enable_external_access = false;")?;
        if let Some(encoder) = &created {
            // `data` holds pixels only in sessions from schema 1 and 2; newer
            // rows name a file in the frames folder and its SHA-256.
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
                    reason VARCHAR NOT NULL, reviewed_ms UBIGINT, frame_path VARCHAR, frame_sha256 VARCHAR
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
                    encoder.fingerprint()
                ],
            )?;
            db.execute_batch("CHECKPOINT")?;
        }
        let (version, descriptor_version, stored_camera, enabled, sensitivity, cooldown_ms): (u32, u32, String, bool, f64, u64) = db.query_row(
            "SELECT schema_version, descriptor_version, camera, enabled, sensitivity, cooldown_ms FROM capture_session WHERE singleton = 1", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
            .context("not a CaptureFab capture session or missing session metadata")?;
        ensure!(
            (1..=SCHEMA_VERSION).contains(&version),
            "unsupported capture session schema version {version}"
        );
        ensure!(
            stored_camera == camera,
            "capture session belongs to camera {stored_camera}, not {camera}"
        );
        // Everything is checked before an older session is upgraded, so one
        // this build refuses still opens in the version that made it.
        let (name, dims, fingerprint): (String, u32, Option<String>) = if version == 1 {
            // Version 1 sessions predate encoder choice: all are native.
            ("native".into(), FEATURES as u32, None)
        } else {
            db.query_row(
                "SELECT encoder, descriptor_dims, model_fingerprint FROM capture_session WHERE singleton = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .context("not a CaptureFab capture session or missing session metadata")?
        };
        let encoder = match created {
            Some(encoder) => encoder,
            None => stored_encoder(&name, dims as usize, fingerprint.as_deref())?,
        };
        ensure!(
            descriptor_version == encoder.version(),
            "unsupported capture session descriptor version {descriptor_version}"
        );
        validate_settings(sensitivity, cooldown_ms)?;
        let (positive, negative) = examples(&db, &encoder)?;
        let background = load_descriptors(
            &db,
            &encoder,
            "SELECT descriptor FROM background ORDER BY id LIMIT 64",
        )?;
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
        if version < SCHEMA_VERSION {
            db.execute_batch(&format!("SET memory_limit = '{LEGACY_MEMORY_LIMIT}'"))?;
            // Earlier sessions had no folder, so one by this name is another
            // session's. The folder is created only once the upgrade is
            // committed: an upgraded session that names no frame files claims
            // a missing folder when resumed, so an interruption between the
            // two leaves nothing that blocks it.
            ensure!(
                std::fs::symlink_metadata(frames).is_err(),
                "capture session frames folder already exists: {}; it belongs to another session, so move it or rename this session",
                frames.display()
            );
            upgrade(&db)?;
            claim(frames)?;
        } else {
            let (files, blobs): (u64, u64) = db.query_row(
                "SELECT COUNT(frame_path), COUNT(*) FILTER (WHERE frame_path IS NULL AND data_bytes > 0) FROM captures",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if blobs > 0 {
                db.execute_batch(&format!("SET memory_limit = '{LEGACY_MEMORY_LIMIT}'"))?;
            }
            if !frames.is_dir() {
                // Saving a frame would recreate the folder, and the next review
                // would then take every saved frame for deleted.
                ensure!(
                    files == 0,
                    "capture session frames folder {} is missing; copy it beside the database, or create an empty folder there to drop the frames it held",
                    frames.display()
                );
                claim(frames)?;
            }
        }
        let shared = Arc::new(Mutex::new(Shared {
            tally: counts(&db)?,
            ..Shared::default()
        }));
        let writer = Writer::spawn(Store {
            db,
            encoder: encoder.clone(),
            frames: frames.to_owned(),
            dirty_since: None,
            shared: shared.clone(),
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
            writer,
            shared,
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
            refused: None,
            unsaved: None,
        })
    }

    pub fn configure(&mut self, enabled: bool, sensitivity: f64, cooldown_ms: u64) -> Result<()> {
        validate_settings(sensitivity, cooldown_ms)?;
        self.writer.call(move |store| {
            store.db.execute("UPDATE capture_session SET enabled = ?, sensitivity = ?, cooldown_ms = ? WHERE singleton = 1", params![enabled, sensitivity, cooldown_ms])?;
            store.wrote();
            Ok(())
        })?;
        if !enabled || !self.enabled {
            self.armed_at = Instant::now();
        }
        self.enabled = enabled;
        self.sensitivity = sensitivity;
        self.cooldown_ms = cooldown_ms;
        // Changing settings is also how a user retries after freeing space.
        self.retry_storage();
        lock(&self.shared).note = None;
        Ok(())
    }

    /// Let the next suggestion try storage again. Every queued write has
    /// landed by now, so a failure one left behind is stale.
    fn retry_storage(&mut self) {
        lock(&self.shared).failure = None;
        self.refused = None;
        self.unsaved = None;
        self.blocked_storage = false;
    }

    pub fn status(&self) -> Result<SessionStatus> {
        let shared = lock(&self.shared);
        let tally = shared.tally;
        let blocked_storage = self.blocked_storage || tally.bytes >= MAX_STORED_BYTES;
        let state = if !self.enabled {
            "paused"
        } else if tally.pending >= MAX_PENDING {
            "review_queue_full"
        } else if blocked_storage {
            "storage_full"
        } else if self.positive.is_empty() {
            "learning"
        } else {
            "watching"
        };
        let throughput = self
            .embedder
            .as_ref()
            .map(|e| e.throughput.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(SessionStatus {
            path: self.path.to_string_lossy().into_owned(),
            camera: self.camera.clone(),
            enabled: self.enabled,
            sensitivity: self.sensitivity,
            cooldown_ms: self.cooldown_ms,
            manual_count: tally.manual,
            kept_count: tally.kept,
            discarded_count: tally.discarded,
            pending_count: tally.pending,
            blocked_storage,
            state: state.into(),
            model: self.encoder.label(),
            encoder: self.encoder.name().into(),
            accelerator: throughput.as_ref().and_then(|t| t.accelerator.clone()),
            samples_per_second: throughput.as_ref().and_then(|t| t.rate()).map(|rate| {
                // Paced CPU sampling below one per second keeps two decimals.
                let scale = if rate < 1.0 { 100.0 } else { 10.0 };
                (rate * scale).round() / scale
            }),
            note: shared
                .note
                .as_ref()
                .filter(|(at, _)| at.elapsed() < NOTE_LIFETIME)
                .map(|(_, note)| note.clone()),
        })
    }

    /// Sample frames on the encoder's grid. Automatic suggestions do not
    /// become training examples until the user reviews them. Scoring stays
    /// in memory; a suggestion is written by the session's writer, so
    /// acquisition never waits for the disk.
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
            || self.embedder.as_ref().is_some_and(|e| e.waiting(now))
        {
            return Ok(captured);
        }
        self.last_sample = Some(match self.last_sample {
            Some(last) if now.saturating_duration_since(last) < 2 * interval => last + interval,
            _ => now,
        });
        if !self.has_room(frame, now)? {
            return Ok(captured);
        }
        match &mut self.embedder {
            Some(embedder) => embedder.submit(frame.clone(), now)?,
            None => {
                let feature = self.encoder.describe(frame)?;
                if let Some(id) = self.decide(Cow::Borrowed(frame), feature, now)? {
                    captured = Some(id);
                }
            }
        }
        Ok(captured)
    }

    /// Whether another pending capture fits the review queue and pixel
    /// budget, and storage is not holding suggestions back after a failed save.
    fn has_room(&mut self, frame: &Frame, now: Instant) -> Result<bool> {
        let tally = self.settle(now)?;
        self.refused = self
            .refused
            .filter(|at| now.saturating_duration_since(*at) < STORAGE_RETRY);
        self.unsaved = self
            .unsaved
            .filter(|at| now.saturating_duration_since(*at) < WRITE_RETRY);
        self.blocked_storage = self.refused.is_some()
            || tally.bytes.saturating_add(frame.data.len() as u64) > MAX_STORED_BYTES;
        Ok(tally.pending < MAX_PENDING && !self.blocked_storage && self.unsaved.is_none())
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
                self.decide_late(frame, feature?, sampled, finished)
            }
            None => Ok(None),
        }
    }

    /// An embedding that took too long belongs to a scene that has moved on.
    fn decide_late(
        &mut self,
        frame: Frame,
        feature: Descriptor,
        sampled: Instant,
        finished: Instant,
    ) -> Result<Option<u64>> {
        let stale_after = self
            .embedder
            .as_ref()
            .map_or(STALE_SAMPLE, Embedder::stale_after);
        if sampled < self.armed_at
            || finished.saturating_duration_since(sampled) > stale_after
            || !self.has_room(&frame, finished)?
        {
            return Ok(None);
        }
        self.decide(Cow::Owned(frame), feature, sampled)
    }

    fn decide(
        &mut self,
        frame: Cow<'_, Frame>,
        feature: Descriptor,
        now: Instant,
    ) -> Result<Option<u64>> {
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
            // Copy the frame only when the writer can take it.
            if !self.writer.ready() {
                lock(&self.shared).note(BUSY_NOTE.into());
                return Ok(None);
            }
            let row = self.row(
                frame,
                feature.clone(),
                "automatic",
                "pending",
                score,
                reason,
            )?;
            let id = row.id;
            self.writer.post(move |store| store.insert(row))?;
            lock(&self.shared).note = None;
            self.last_capture = Some(now);
            push_bounded(&mut self.recent, (feature, now_ms), MAX_RECENT);
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
            // A busy writer skips the prototype; a later sample offers it again.
            if distinct && self.writer.ready() {
                let (id, bytes) = (self.background.len() as u32, encode(&feature));
                self.writer
                    .post(move |store| store.add_background(id, bytes))?;
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
        let row = self.row(
            Cow::Borrowed(frame),
            feature.clone(),
            "manual",
            "manual",
            1.0,
            "User-initiated capture: positive training example".into(),
        )?;
        let id = row.id;
        let (positive, negative) = self.writer.call(move |store| {
            store.insert(row)?;
            examples(&store.db, &store.encoder)
        })?;
        self.positive = positive;
        self.negative = negative;
        push_bounded(&mut self.recent, (feature, epoch_ms()), MAX_RECENT);
        self.last_capture = Some(Instant::now());
        Ok(id)
    }

    /// Keeping changes only the label. Discarding deletes the frame file
    /// through the storage ledger and keeps the descriptor and negative
    /// decision for future learning.
    pub fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let (positive, negative) = self.writer.call(move |store| {
            store.review(id, keep)?;
            examples(&store.db, &store.encoder)
        })?;
        self.positive = positive;
        self.negative = negative;
        if !keep {
            self.retry_storage();
        }
        Ok(())
    }

    pub fn next_review(&self) -> Result<Option<ReviewCapture>> {
        self.writer.call(|store| store.load_review(None))
    }

    pub fn review_capture(&self, id: u64) -> Result<Option<ReviewCapture>> {
        self.writer.call(move |store| store.load_review(Some(id)))
    }

    /// Finish queued writes and checkpoint, so the database file is complete
    /// without its WAL. The session closes even if this fails: DuckDB replays
    /// the WAL when the session is resumed.
    pub fn close(mut self) -> Result<()> {
        self.writer.close()
    }

    /// Apply the outcome of queued writes. A frame storage refused or could
    /// not save holds suggestions back for a while; any other failure is
    /// returned so the caller can pause.
    fn settle(&mut self, now: Instant) -> Result<Tally> {
        let mut shared = lock(&self.shared);
        if let Some(error) = shared.failure.take() {
            if crate::cli::error_code(&error).0 == "storage_full" {
                self.refused = Some(now);
            } else if error.downcast_ref::<FrameNotSaved>().is_some() {
                self.unsaved = Some(now);
            } else {
                return Err(error);
            }
            shared.note(format!("{error:#}"));
        }
        Ok(shared.tally)
    }

    /// Assign the next identifier and reserve the capture's counts.
    fn row(
        &mut self,
        frame: Cow<'_, Frame>,
        feature: Descriptor,
        source: &'static str,
        label: &'static str,
        score: f64,
        reason: String,
    ) -> Result<Row> {
        ensure!(
            frame.data.len() <= MAX_FRAME_BYTES,
            "frame exceeds 256 MiB session capture limit"
        );
        let bytes = frame.data.len() as u64;
        let next_id = self
            .next_id
            .checked_add(1)
            .context("capture session identifier overflow")?;
        {
            let mut shared = lock(&self.shared);
            self.blocked_storage = shared.tally.bytes.saturating_add(bytes) > MAX_STORED_BYTES;
            ensure!(
                !self.blocked_storage,
                "Storage full: capture session reached its 2 GiB pixel storage limit; start another session or discard pending captures"
            );
            shared.tally.add(label, bytes);
        }
        let id = self.next_id;
        self.next_id = next_id;
        Ok(Row {
            id,
            created_ms: epoch_ms(),
            source,
            label,
            frame: frame.into_owned(),
            feature,
            score,
            reason,
        })
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

type Task = Box<dyn FnOnce(&mut Store) + Send>;

/// The 'capturefab-autocapture' thread. It owns the DuckDB connection and runs
/// every statement and frame write in order, off the camera thread.
struct Writer {
    tasks: Option<SyncSender<Task>>,
    thread: Option<JoinHandle<Result<()>>>,
    shared: Arc<Mutex<Shared>>,
}

impl Writer {
    fn spawn(store: Store) -> Result<Self> {
        let shared = store.shared.clone();
        let (tasks, queue) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("capturefab-autocapture".into())
            .spawn(move || store.run(queue))
            .context("start capture session writer")?;
        Ok(Self {
            tasks: Some(tasks),
            thread: Some(thread),
            shared,
        })
    }

    fn ready(&self) -> bool {
        lock(&self.shared).queued == 0
    }

    /// Queue a task without waiting for it; its failure is reported by the
    /// next sample. Only the camera thread queues tasks, so after `ready`
    /// there is room.
    fn post(&self, task: impl FnOnce(&mut Store) -> Result<()> + Send + 'static) -> Result<()> {
        let tasks = self.tasks.as_ref().ok_or_else(stopped)?;
        lock(&self.shared).queued += 1;
        let task = Box::new(move |store: &mut Store| {
            if let Err(error) = task(store) {
                lock(&store.shared).failure.get_or_insert(error);
            }
        });
        tasks.try_send(task).map_err(|error| {
            lock(&self.shared).queued -= 1;
            match error {
                TrySendError::Full(_) => anyhow!("capture session writer is busy"),
                TrySendError::Disconnected(_) => stopped(),
            }
        })
    }

    /// Run a task after those already queued and wait for its result.
    fn call<T: Send + 'static>(
        &self,
        task: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let tasks = self.tasks.as_ref().ok_or_else(stopped)?;
        let (reply, result) = mpsc::channel();
        lock(&self.shared).queued += 1;
        if tasks
            .send(Box::new(move |store: &mut Store| {
                let _ = reply.send(task(store));
            }))
            .is_err()
        {
            lock(&self.shared).queued -= 1;
            return Err(stopped());
        }
        result.recv().map_err(|_| stopped())?
    }

    fn close(&mut self) -> Result<()> {
        self.tasks.take();
        match self.thread.take() {
            Some(thread) => thread.join().map_err(|_| stopped())?,
            None => Ok(()),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn stopped() -> anyhow::Error {
    anyhow!("capture session writer stopped")
}

/// The writer's side of a session: its database connection and frames folder.
struct Store {
    db: Connection,
    encoder: Encoder,
    frames: PathBuf,
    dirty_since: Option<Instant>,
    shared: Arc<Mutex<Shared>>,
}

impl Store {
    /// Run tasks until the session closes. The WAL makes each write durable;
    /// checkpoints fold it into the database at most once a minute, and on close.
    fn run(mut self, queue: mpsc::Receiver<Task>) -> Result<()> {
        loop {
            let task = match self.dirty_since {
                None => queue.recv().ok(),
                Some(since) => {
                    match queue.recv_timeout(CHECKPOINT_INTERVAL.saturating_sub(since.elapsed())) {
                        Ok(task) => Some(task),
                        Err(RecvTimeoutError::Timeout) => {
                            if self.checkpoint().is_err() {
                                self.dirty_since = Some(Instant::now());
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => None,
                    }
                }
            };
            let Some(task) = task else {
                return self.checkpoint();
            };
            lock(&self.shared).queued -= 1;
            task(&mut self);
        }
    }

    fn wrote(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.db
            .execute_batch("CHECKPOINT")
            .context("checkpoint capture session database")?;
        self.dirty_since = None;
        Ok(())
    }

    /// Save the frame file, then its row, so a row never names a partial file.
    /// A failure releases the counts reserved for the capture.
    fn insert(&mut self, row: Row) -> Result<()> {
        let (label, bytes) = (row.label, row.frame.data.len() as u64);
        let result = self.save(&row);
        if result.is_err() {
            lock(&self.shared).tally.remove(label, bytes);
        }
        result
    }

    fn save(&mut self, row: &Row) -> Result<()> {
        let (name, sha256) = self
            .write_frame(row.id, &row.frame.data)
            .context(FrameNotSaved)?;
        let inserted = self.db.execute(
            "INSERT INTO captures (id, created_ms, source, label, frame_id, width, height, pixel_format,
                timestamp_ns, data_bytes, descriptor, score, reason, frame_path, frame_sha256)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                row.id,
                row.created_ms,
                row.source,
                row.label,
                row.frame.id,
                row.frame.width,
                row.frame.height,
                row.frame.pixel_format,
                row.frame.timestamp_ns,
                row.frame.data.len() as u64,
                encode(&row.feature),
                row.score,
                row.reason,
                name,
                sha256
            ],
        );
        self.wrote();
        if let Err(error) = inserted {
            let _ = storage::remove(&self.frames.join(&name));
            return Err(error.into());
        }
        Ok(())
    }

    fn add_background(&mut self, id: u32, descriptor: Vec<u8>) -> Result<()> {
        self.db.execute(
            "INSERT INTO background VALUES (?, ?)",
            params![id, descriptor],
        )?;
        self.wrote();
        Ok(())
    }

    /// Write a raw frame through the storage ledger, which applies the global
    /// budget, volume checks and never-overwrite. Frames have no limits of
    /// their own: any other request policy could only tighten the one the
    /// user configured, and delete-oldest would then remove captures that
    /// budget keeps. A name left by an interrupted session is skipped, not
    /// replaced.
    fn write_frame(&self, id: u64, data: &[u8]) -> Result<(String, String)> {
        self.check_folder()?;
        let policy = storage::configured_policy()?;
        for attempt in 0..8 {
            let name = match attempt {
                0 => format!("{id:08}.raw"),
                n => format!("{id:08}-{n}.raw"),
            };
            let mut writer = match storage::create_writer(
                &self.frames.join(&name),
                &policy,
                data.len() as u64,
            ) {
                Ok(writer) => writer,
                Err(error) if error.downcast_ref::<storage::OutputExists>().is_some() => continue,
                Err(error) => return Err(error),
            };
            writer.write_all(data)?;
            let sha256 = writer.sha256();
            writer.finish()?;
            return Ok((name, sha256));
        }
        bail!(
            "no free frame file name for capture {id} in {}",
            self.frames.display()
        )
    }

    /// The folder must already exist. Writing never recreates a folder that
    /// was moved away, so a review never takes its frames for deleted.
    fn check_folder(&self) -> Result<()> {
        crate::volumes::ensure_present(&self.frames)?;
        ensure!(
            self.frames.is_dir(),
            "capture session frames folder {} is missing; keep it beside the database",
            self.frames.display()
        );
        Ok(())
    }

    /// A pending-only transaction prevents a double key press or stale review
    /// card from relabeling a capture.
    fn review(&mut self, id: u64, keep: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        let row: Option<(Vec<u8>, u64, Option<String>)> = tx
            .query_row(
                "SELECT descriptor, data_bytes, frame_path FROM captures WHERE id = ? AND label = 'pending'",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (descriptor, bytes, frame_path) = row.context("capture is no longer pending review")?;
        self.encoder.decode(&descriptor)?;
        let file = frame_path
            .map(|name| frame_file(&self.frames, &name))
            .transpose()?;
        let now = epoch_ms();
        let changed = if keep {
            tx.execute(
                "UPDATE captures SET label = 'kept', reviewed_ms = ? WHERE id = ? AND label = 'pending'",
                params![now, id],
            )?
        } else {
            // Release the pixels: older rows keep them in `data`, newer rows
            // in the file deleted below.
            tx.execute(
                "UPDATE captures SET label = 'discarded', reviewed_ms = ?, data = NULL, data_bytes = 0, frame_path = NULL, frame_sha256 = NULL WHERE id = ? AND label = 'pending'",
                params![now, id],
            )?
        };
        ensure!(changed == 1, "capture is no longer pending review");
        tx.commit()?;
        self.wrote();
        {
            let mut shared = lock(&self.shared);
            shared.tally.remove("pending", if keep { 0 } else { bytes });
            shared.tally.add(if keep { "kept" } else { "discarded" }, 0);
        }
        // Delete only once the decision is saved: a failed delete leaves a
        // file the ledger still tracks, never a lost decision.
        if let (false, Some(path)) = (keep, file) {
            self.delete_frame(id, &path);
        }
        Ok(())
    }

    /// Delete a discarded capture's frame through the ledger. A file that
    /// changed after saving, or cannot be deleted now, stays with a note.
    fn delete_frame(&self, id: u64, path: &Path) {
        let left = match storage::remove(path) {
            Ok(removed) => (!removed && path.exists())
                .then(|| "it changed after saving or was saved on another computer".into()),
            Err(error) => Some(format!("{error:#}")),
        };
        if let Some(reason) = left {
            lock(&self.shared).note(format!(
                "Discarded capture {id} but left {} in place: {reason}",
                path.display()
            ));
        }
    }

    fn load_review(&mut self, id: Option<u64>) -> Result<Option<ReviewCapture>> {
        loop {
            let row = self.db.query_row(
                "SELECT id, frame_id, width, height, pixel_format, timestamp_ns, data, score, reason, data_bytes, frame_path, frame_sha256
                 FROM captures WHERE label = 'pending' AND (?::UBIGINT IS NULL OR id = ?::UBIGINT) ORDER BY id LIMIT 1",
                params![id, id],
                |r| {
                    let capture = ReviewCapture { id: r.get(0)?, frame: Frame { id:r.get(1)?,width:r.get(2)?,height:r.get(3)?,pixel_format:r.get(4)?,timestamp_ns:r.get(5)?,data:r.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default() },score:r.get(7)?,reason:r.get(8)? };
                    Ok((capture, r.get::<_, u64>(9)?, r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(11)?))
                },
            ).optional()?;
            let Some((mut capture, bytes, frame_path, sha256)) = row else {
                return Ok(None);
            };
            if let Some(name) = frame_path {
                let path = frame_file(&self.frames, &name)?;
                match self.read_frame(&path, bytes, sha256.as_deref())? {
                    Ok(data) => capture.frame.data = data,
                    Err(lost) => {
                        self.forget(capture.id, bytes, &path, lost)?;
                        continue;
                    }
                }
            }
            // Validate metadata before callers allocate a UI image for a file
            // which may have been modified outside CaptureFab.
            ensure!(
                capture.frame.data.len() <= MAX_FRAME_BYTES,
                "stored frame exceeds capture size limit"
            );
            descriptor(&capture.frame).context("corrupt capture frame in session")?;
            ensure!(capture.score.is_finite(), "invalid stored capture score");
            return Ok(Some(capture));
        }
    }

    /// A pending frame's bytes, or why they are lost: storage retention or a
    /// user deleted the file, or it no longer holds what was saved. A missing
    /// folder or volume is an error, so it never empties the review queue.
    fn read_frame(
        &self,
        path: &Path,
        bytes: u64,
        sha256: Option<&str>,
    ) -> Result<Result<Vec<u8>, &'static str>> {
        ensure!(
            bytes <= MAX_FRAME_BYTES as u64,
            "stored frame exceeds capture size limit"
        );
        self.check_folder()?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Err("was deleted")),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            }
        };
        let mut data = Vec::with_capacity(bytes as usize);
        file.take(bytes + 1).read_to_end(&mut data)?;
        if data.len() as u64 != bytes
            || sha256 != Some(format!("{:x}", Sha256::digest(&data)).as_str())
        {
            return Ok(Err("changed after saving and was left in place"));
        }
        Ok(Ok(data))
    }

    /// Drop a pending capture whose frame is lost, so it cannot block the
    /// review queue. Pending captures never train the model, so nothing
    /// learned is lost.
    fn forget(&mut self, id: u64, bytes: u64, path: &Path, lost: &str) -> Result<()> {
        let removed = self.db.execute(
            "DELETE FROM captures WHERE id = ? AND label = 'pending'",
            params![id],
        )?;
        self.wrote();
        ensure!(removed == 1, "capture is no longer pending review");
        let mut shared = lock(&self.shared);
        shared.tally.remove("pending", bytes);
        shared.note(format!(
            "Removed pending capture {id}: its frame file {} {lost}",
            path.display()
        ));
        Ok(())
    }
}

/// Schema 3 keeps frames as files beside the database; rows saved before
/// keep their pixels in `data` and review exactly as before. Schema 1
/// sessions predate encoder choice, so all of them are native.
fn upgrade(db: &Connection) -> Result<()> {
    db.execute_batch(&format!(
        "BEGIN;
         ALTER TABLE capture_session ADD COLUMN IF NOT EXISTS encoder VARCHAR DEFAULT 'native';
         ALTER TABLE capture_session ADD COLUMN IF NOT EXISTS descriptor_dims UINTEGER DEFAULT {FEATURES};
         ALTER TABLE capture_session ADD COLUMN IF NOT EXISTS model_fingerprint VARCHAR;
         ALTER TABLE captures ADD COLUMN IF NOT EXISTS frame_path VARCHAR;
         ALTER TABLE captures ADD COLUMN IF NOT EXISTS frame_sha256 VARCHAR;
         UPDATE capture_session SET schema_version = {SCHEMA_VERSION} WHERE singleton = 1;
         COMMIT;
         CHECKPOINT;"
    ))
    .context("upgrade capture session to frame files")
}

/// Create a session's frames folder. It must not exist yet, which reserves
/// the name, so a session never mixes another's frames into its own.
fn claim(frames: &Path) -> Result<()> {
    match std::fs::create_dir(frames) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => bail!(
            "capture session frames folder already exists: {}; choose another session name",
            frames.display()
        ),
        Err(error) => Err(error)
            .with_context(|| format!("create capture session frames folder {}", frames.display())),
    }
}

/// A stored frame name resolved inside the frames folder; a modified
/// database cannot point outside it.
fn frame_file(frames: &Path, name: &str) -> Result<PathBuf> {
    let mut components = Path::new(name).components();
    ensure!(
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
        "invalid frame file name in capture session"
    );
    Ok(frames.join(name))
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn counts(db: &Connection) -> Result<Tally> {
    Ok(db.query_row(
        "SELECT COUNT(*) FILTER (WHERE label = 'manual'), COUNT(*) FILTER (WHERE label = 'kept'),
                COUNT(*) FILTER (WHERE label = 'discarded'), COUNT(*) FILTER (WHERE label = 'pending'),
                COALESCE(SUM(data_bytes), 0)::UBIGINT FROM captures", [],
        |r| Ok(Tally { manual: r.get(0)?, kept: r.get(1)?, discarded: r.get(2)?, pending: r.get(3)?, bytes: r.get(4)? }))?)
}

fn examples(db: &Connection, encoder: &Encoder) -> Result<(Vec<Descriptor>, Vec<Descriptor>)> {
    let mut positive = load_descriptors(
        db,
        encoder,
        "SELECT descriptor FROM captures WHERE label IN ('manual', 'kept') ORDER BY id DESC LIMIT 256",
    )?;
    let mut negative = load_descriptors(
        db,
        encoder,
        "SELECT descriptor FROM captures WHERE label = 'discarded' ORDER BY id DESC LIMIT 256",
    )?;
    positive.reverse();
    negative.reverse();
    Ok((positive, negative))
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
impl CaptureSession {
    /// Run SQL on the writer's connection after every queued write lands.
    fn sql<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> duckdb::Result<T> + Send + 'static,
    ) -> T {
        self.writer
            .call(move |store| Ok(query(&store.db)?))
            .unwrap()
    }
    /// Wait until every queued write has landed.
    fn flush(&self) {
        self.writer.call(|_| Ok(())).unwrap();
    }
    /// Recount after tests edit rows behind the session's back.
    fn recount(&self) {
        let tally = self.writer.call(|store| counts(&store.db)).unwrap();
        lock(&self.shared).tally = tally;
    }
    /// Hold a write transaction open on another connection, which makes
    /// DuckDB refuse to checkpoint until it is dropped.
    pub(crate) fn block_checkpoints(&self) -> Connection {
        self.sql(|db| {
            let other = db.try_clone()?;
            other.execute_batch(
                "BEGIN; UPDATE capture_session SET cooldown_ms = cooldown_ms WHERE singleton = 1;",
            )?;
            Ok(other)
        })
    }
}

#[cfg(test)]
mod tests;
