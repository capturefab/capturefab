//! Which encoder writes a JPEG still: libjpeg-turbo on the CPU, or a
//! hardware engine (`CAPTUREFAB_JPEG=auto|cpu|videotoolbox|nvjpeg|vaapi`):
//! Apple's JPEG engine on macOS, and on Linux NVIDIA's nvJPEG or a VA-API
//! driver's, which run in a helper process (`helper.rs`).
//!
//! `auto` hands a hardware engine the frames it is good at (frames of at
//! least `MIN_PIXELS` it takes: color at the default quality for Apple's,
//! any format and quality for the GPU engines) and keeps the rest on the
//! CPU, which reads 8-bit gray and RGB frames in place. Of the engines a
//! host has, the first that has not failed is used. Before an engine
//! writes a frame size, format and quality it is checked against the CPU:
//! both encode a frame, both are decoded, and the engine's must be accurate
//! (`accurate`); `auto` then keeps whichever was faster. Under `auto` the
//! first frame of a size and format is the CPU's, and the engine is checked
//! on it by a thread of its own meanwhile, so no capture waits for a check
//! (opening the engine and encoding twice more cost a one-off capture more
//! than the engine saves it). Every engine output must be a well-formed
//! JPEG of the frame's size. An engine that fails, or does not open or
//! answer within its deadline, is not used again by this process, which
//! says so on stderr, and a frame it held up is encoded on the CPU. One
//! found broken for a size and format (shifted colors, an output that does
//! not decode) is not used for it again; one found less faithful than
//! libjpeg-turbo, which may be down to the frame's content, is checked
//! again on a later frame; one that declines a frame (a size outside the
//! hardware's range) gets no more like it.
//!
//! Naming an engine makes it a requirement: it is checked on the very frame
//! it is given, and if it is missing or fails its check, the still fails
//! with `unavailable` instead of quietly using the CPU. Frames it cannot
//! take (monochrome or a non-default quality for Apple's, a frame it
//! declines) still go to the CPU.
//!
//! An engine runs on a thread of its own, so a driver that hangs costs one
//! abandoned thread, not the camera worker; frames reach it shared, not
//! copied. A GPU engine unused for a minute rests: its helper stops,
//! releasing the GPU, and under `auto` the next frame is the CPU's while it
//! starts again. Apple's engine keeps its sessions.
#![deny(clippy::missing_const_for_fn)]
// Builds without a hardware engine for their platform use only the CPU.
#![cfg_attr(
    not(any(
        all(feature = "videotoolbox", target_os = "macos"),
        all(jpeg_helper, target_os = "linux")
    )),
    allow(
        dead_code,
        reason = "hardware encoder interfaces are shared across platform and feature combinations for tests and doctor reporting"
    )
)]
#![deny(clippy::significant_drop_tightening)]
use super::QUALITY;
use crate::{
    frame::{Body, FrameRef},
    types::{Categorized, ErrorKind, Frame},
};
use anyhow::Result;
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub(crate) const ENV: &str = "CAPTUREFAB_JPEG";

/// Smaller frames encode faster on the CPU: a hardware engine has a fixed
/// cost per frame (about 0.25 ms on an M4 Max, where libjpeg-turbo was
/// faster at 640x480 and slower from 1280x720 up).
pub(crate) const MIN_PIXELS: usize = 512 * 1024;

/// What `CAPTUREFAB_JPEG` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Choice {
    Auto,
    Cpu,
    Named(Kind),
}

/// A hardware JPEG engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    /// Apple's JPEG engine through `VideoToolbox`, in the worker (macOS).
    VideoToolbox,
    /// NVIDIA's nvJPEG on a CUDA GPU, in a helper process (Linux).
    Nvjpeg,
    /// A VA-API driver's JPEG encoder, in a helper process (Linux).
    Vaapi,
    #[cfg(all(test, feature = "jpeg"))]
    Fake,
}

impl Kind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::VideoToolbox => "videotoolbox",
            Self::Nvjpeg => "nvjpeg",
            Self::Vaapi => "vaapi",
            #[cfg(all(test, feature = "jpeg"))]
            Self::Fake => "fake",
        }
    }
}

impl Choice {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        Ok(match value.map_or("", str::trim) {
            "" | "auto" => Self::Auto,
            "cpu" => Self::Cpu,
            "videotoolbox" => Self::Named(Kind::VideoToolbox),
            "nvjpeg" => Self::Named(Kind::Nvjpeg),
            "vaapi" => Self::Named(Kind::Vaapi),
            other => {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    format!(
                        "{ENV}={other:?} is not a JPEG encoder; use auto, cpu, videotoolbox, nvjpeg or vaapi"
                    ),
                ));
            }
        })
    }
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::Named(kind) => kind.name(),
        }
    }
}

/// The encoder this process's environment asks for.
pub(crate) fn choice() -> Result<Choice> {
    Choice::parse(std::env::var(ENV).ok().as_deref())
}

/// A JPEG encoder a still can be handed to instead of the CPU.
pub(crate) trait Backend: Send {
    /// Whether it encodes `view` at `quality` at all.
    fn accepts(view: FrameRef<'_>, quality: i32) -> bool
    where
        Self: Sized;
    /// `view` as a JPEG, from the frame's own buffer. An [`Unsupported`]
    /// error declines the frame: the CPU encodes it, and the engine stays.
    fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>>;
    /// What `doctor` shows of it once open: its device, driver, version.
    fn describe(&self) -> Map<String, Value> {
        Map::new()
    }
    /// Unused for a while: release what it holds until `wake`; whether it
    /// held anything, which `wake` must then take up again.
    fn rest(&mut self) -> bool {
        false
    }
    /// Take up again what `rest` released, before the next frame.
    fn wake(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A frame an engine cannot take after all, such as a size outside the
/// hardware's range: the CPU encodes it, and the engine stays in use.
#[derive(Debug)]
pub(crate) struct Unsupported(pub(crate) String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unsupported {}

/// A frame an engine cannot take now, such as when the memory it reads
/// frames from is all in use: the CPU encodes it, and nothing is learned
/// about the engine.
#[derive(Debug)]
pub(crate) struct Unready(pub(crate) String);
impl std::fmt::Display for Unready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unready {}

/// Opens a backend on its engine thread; an error says why it cannot.
type Opener = Box<dyn FnOnce() -> Result<Box<dyn Backend>> + Send>;

struct Request {
    frame: Frame,
    quality: i32,
    reply: mpsc::SyncSender<Result<Body<'static>>>,
}

/// What the engine thread is sent.
enum Message {
    Encode(Request),
    /// Wake a resting backend before a frame needs it.
    Wake,
}

/// Why an engine's JPEG of a frame is not as good as libjpeg-turbo's.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Fault {
    /// Wrong whatever the frame: an error, or malformed or shifted output.
    Broken(String),
    /// Less faithful than libjpeg-turbo's on this frame, which may be down
    /// to the frame's content.
    // Only libjpeg-turbo can find an engine unfaithful.
    #[cfg_attr(
        not(feature = "jpeg"),
        allow(
            dead_code,
            reason = "hardware encoder interfaces are shared across platform and feature combinations for tests and doctor reporting"
        )
    )]
    Unfaithful(String),
    /// It declined the frame.
    Declined(String),
    /// It could not take the frame now.
    Unready(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Broken(reason)
            | Self::Unfaithful(reason)
            | Self::Declined(reason)
            | Self::Unready(reason) => f.write_str(reason),
        }
    }
}

/// What a frame size, format and quality were found to be on an engine.
#[derive(Clone, Debug, PartialEq)]
enum Verdict {
    /// Accurate; `faster` when it took less time than the CPU.
    Accurate { faster: bool },
    /// Broken: not used for this size and format again.
    Rejected(String),
    /// Declined by the engine: such frames go to the CPU, named or not.
    Declined(String),
    /// Unfaithful in `failures` checks in a row: checked again on a later
    /// frame, under `auto` once 2^`failures` frames (`since` so far) have
    /// gone to the CPU, and on the next one when named.
    Doubtful { failures: u32, since: u32 },
    /// Being checked by a thread of its own, after `failures` doubtful
    /// checks.
    Checking { failures: u32 },
}

/// The most doublings of the frames a doubtful engine waits for: it is
/// checked again at least every 256 frames.
const MOST_DOUBT: u32 = 8;

/// Frames are told apart by size, format and quality.
type Key = (u32, u32, u32, i32);

/// A warm camera can change geometry, format and quality indefinitely.
/// Evicted combinations are checked again before using the engine.
const MAX_VERDICTS: usize = 256;

enum State {
    /// Not opened yet.
    Closed(Option<Opener>),
    Open(mpsc::Sender<Message>),
    /// Not used again, and why.
    Off(String),
}

/// Why the engine did not encode a frame.
enum Miss {
    /// It is waking from rest, and the frame should not wait for it, or it
    /// cannot take the frame now.
    Unready(String),
    /// It declined the frame.
    Declined(String),
    /// It failed, and is turned off.
    Failed(String),
}

/// What the encoder threads of a process, and the threads checking the
/// engine for them, share of it.
struct Shared {
    kind: Kind,
    state: Mutex<State>,
    /// Whether a frame, or a check, is with the engine; under `auto` other
    /// frames use the CPU meanwhile.
    busy: AtomicBool,
    verdicts: Mutex<HashMap<Key, Verdict>>,
    /// How long the engine may take to open.
    open_wait: Duration,
    /// How long it may go unused before it rests.
    idle: Duration,
    /// It has rested and must wake before its next frame (which, under
    /// `auto`, the CPU encodes meanwhile).
    resting: AtomicBool,
    /// It has been turned off: the state says why. Read without the state
    /// lock, which an engine opening holds for as long as that takes.
    off: AtomicBool,
    /// What the backend said of itself when it opened.
    about: Mutex<Map<String, Value>>,
    /// Why this host cannot have it at all (no device), checked once before
    /// anything is opened; None when it may.
    absent: fn() -> Option<String>,
    absence: std::sync::OnceLock<Option<String>>,
}

/// A hardware engine shared by every encoder thread of the process.
pub(crate) struct Engine {
    /// The backend's `accepts`.
    accepts: fn(FrameRef<'_>, i32) -> bool,
    shared: Arc<Shared>,
    /// Whether `auto` hands it small frames (tests).
    any_size: bool,
}

/// The engine held for one frame or check, until dropped.
struct Held(Arc<Shared>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

/// How a still is encoded when the engine cannot or should not.
fn cpu(frame: &Frame, quality: i32) -> Result<Body<'static>> {
    super::encode(frame, quality)
}

/// Without libjpeg-turbo nothing can check an engine.
#[cfg(not(feature = "jpeg"))]
fn accurate(_: &Frame, _: &[u8], _: &[u8]) -> Result<(), Fault> {
    Err(Fault::Broken(
        "checking it needs libjpeg-turbo (the jpeg feature)".into(),
    ))
}

/// A named engine's frame cannot be encoded; in a session that started
/// with another `CAPTUREFAB_JPEG`, a later command's value is not seen.
fn unavailable(kind: Kind, reason: &str) -> anyhow::Error {
    let error = Categorized::new(
        ErrorKind::Unavailable,
        format!(
            "the {} JPEG encoder {ENV} names cannot be used: {reason}; set {ENV}=auto or cpu",
            kind.name()
        ),
    );
    match crate::session::environment_hint(ENV) {
        Some(hint) => error.hint(hint),
        None => error,
    }
    .into()
}

/// What the engine thread does: open the backend, then encode what it is
/// sent, resting after `idle` without a frame.
fn run(
    shared: &Weak<Shared>,
    open: Opener,
    opened: mpsc::SyncSender<Result<(), String>>,
    requests: &mpsc::Receiver<Message>,
    idle: Duration,
) {
    let mut backend = match open() {
        Ok(backend) => backend,
        Err(error) => {
            let _ = opened.send(Err(format!("{error:#}")));
            return;
        }
    };
    if let Some(shared) = shared.upgrade() {
        *shared.about.lock().unwrap_or_else(PoisonError::into_inner) = backend.describe();
    }
    let _ = opened.send(Ok(()));
    drop(opened);
    let mut resting = false;
    // Why waking failed: every later frame fails with it.
    let mut woken: Result<(), String> = Ok(());
    loop {
        let message = if resting {
            requests.recv().ok()
        } else {
            match requests.recv_timeout(idle) {
                Ok(message) => Some(message),
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let Some(shared) = shared.upgrade() else {
                        return;
                    };
                    // Callers send under the state lock, so with it held a
                    // frame either is already queued or will see `resting`.
                    let state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
                    match requests.try_recv() {
                        Ok(message) => Some(message),
                        Err(mpsc::TryRecvError::Disconnected) => None,
                        Err(mpsc::TryRecvError::Empty) => {
                            shared.resting.store(true, Ordering::Release);
                            drop(state);
                            resting = backend.rest();
                            if !resting {
                                shared.resting.store(false, Ordering::Release);
                            }
                            continue;
                        }
                    }
                }
            }
        };
        let Some(message) = message else {
            return;
        };
        if resting {
            woken = backend.wake().map_err(|error| format!("{error:#}"));
            if let Some(shared) = shared.upgrade() {
                *shared.about.lock().unwrap_or_else(PoisonError::into_inner) = backend.describe();
                shared.resting.store(false, Ordering::Release);
            }
            resting = false;
        }
        if let Message::Encode(request) = message {
            let result = match &woken {
                Ok(()) => backend.encode(FrameRef::whole(&request.frame), request.quality),
                Err(reason) => Err(anyhow::anyhow!("it did not start again: {reason}")),
            };
            let _ = request.reply.send(result);
        }
    }
}

impl Shared {
    fn remember(&self, key: Key, verdict: Verdict) {
        let mut verdicts = self.verdicts.lock().unwrap_or_else(PoisonError::into_inner);
        if verdicts.len() >= MAX_VERDICTS && !verdicts.contains_key(&key) {
            // Any entry may be evicted: missing verdicts always require a
            // fresh check, including previously rejected combinations.
            if let Some(old) = verdicts.keys().next().copied() {
                verdicts.remove(&old);
            }
        }
        verdicts.insert(key, verdict);
    }

    /// The state, opened on first use within `open_wait`.
    fn opened(self: &Arc<Self>) -> MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let State::Closed(open) = &mut *state {
            if let Some(reason) = self.absence() {
                *state = State::Off(reason.to_owned());
                self.off.store(true, Ordering::Release);
                return state;
            }
            let open = open.take().expect("opened once");
            let (opened, ready) = mpsc::sync_channel(1);
            let (sender, requests) = mpsc::channel::<Message>();
            let (shared, idle) = (Arc::downgrade(self), self.idle);
            let spawned = std::thread::Builder::new()
                .name(format!("jpeg-{}", self.kind.name()))
                .spawn(move || run(&shared, open, opened, &requests, idle));
            // A driver that hangs while opening keeps only its own thread,
            // which ends once it opens: no request will reach it.
            *state = match spawned {
                Err(error) => State::Off(format!("cannot start its thread: {error}")),
                Ok(_) => match ready.recv_timeout(self.open_wait) {
                    Ok(Ok(())) => State::Open(sender),
                    Ok(Err(reason)) => State::Off(reason),
                    Err(mpsc::RecvTimeoutError::Timeout) => State::Off(format!(
                        "it did not open within {} ms",
                        self.open_wait.as_millis()
                    )),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        State::Off("its thread ended while opening".into())
                    }
                },
            };
            self.off
                .store(matches!(*state, State::Off(_)), Ordering::Release);
        }
        state
    }

    /// Why this host cannot have the engine, if it cannot.
    fn absence(&self) -> Option<&str> {
        self.absence.get_or_init(self.absent).as_deref()
    }

    /// Why the engine cannot be used, opening it if it has not been.
    fn unavailable(self: &Arc<Self>) -> Option<String> {
        match &*self.opened() {
            State::Off(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    /// Whether it has been turned off.
    fn is_off(&self) -> bool {
        self.off.load(Ordering::Acquire)
    }

    /// Stop using the engine for good. One that was open says so, as frames
    /// go to the CPU from now on.
    fn turn_off(&self, reason: String) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let message = matches!(*state, State::Open(_)).then(|| format!(
            "capturefab worker: the {} JPEG encoder stopped ({reason}); JPEGs are encoded on the CPU",
            self.kind.name()
        ));
        let previous = std::mem::replace(&mut *state, State::Off(reason));
        self.off.store(true, Ordering::Release);
        drop(state);
        drop(previous);
        if let Some(message) = message {
            crate::eprintln!("{message}");
        }
    }

    /// `frame` encoded on the engine within its deadline, checked to be a
    /// whole JPEG of the frame's size; an engine that misses the deadline
    /// or breaks is turned off. A resting engine is woken: unless `wait`,
    /// the frame does not wait for that.
    fn engine_encode(
        self: &Arc<Self>,
        frame: &Frame,
        quality: i32,
        wait: bool,
    ) -> Result<Body<'static>, Miss> {
        let (reply, answer) = mpsc::sync_channel(1);
        let mut deadline = deadline(frame);
        {
            let state = self.opened();
            let sender = match &*state {
                State::Open(sender) => sender,
                State::Off(reason) => return Err(Miss::Failed(reason.clone())),
                State::Closed(_) => unreachable!(),
            };
            if self.resting.load(Ordering::Acquire) {
                if !wait {
                    let _ = sender.send(Message::Wake);
                    return Err(Miss::Unready("it is starting again".into()));
                }
                deadline += self.open_wait;
            }
            let request = Request {
                frame: frame.clone(),
                quality,
                reply,
            };
            if sender.send(Message::Encode(request)).is_err() {
                drop(state);
                let reason = "its thread has ended".to_owned();
                self.turn_off(reason.clone());
                return Err(Miss::Failed(reason));
            }
        }
        let result = match answer.recv_timeout(deadline) {
            Ok(Ok(body)) if super::well_formed(&body, frame.width, frame.height) => {
                return Ok(body);
            }
            Ok(Ok(_)) => "it wrote a malformed JPEG".to_owned(),
            Ok(Err(error)) => match error.downcast::<Unsupported>() {
                Ok(declined) => return Err(Miss::Declined(declined.0)),
                Err(error) => match error.downcast::<Unready>() {
                    Ok(unready) => return Err(Miss::Unready(unready.0)),
                    Err(error) => format!("{error:#}"),
                },
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                format!("it took longer than {} ms", deadline.as_millis())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => "its thread has ended".to_owned(),
        };
        self.turn_off(result.clone());
        Err(Miss::Failed(result))
    }

    /// Check the engine on a frame: encoded on the engine and the CPU, both
    /// decoded and compared with the frame. Whether the engine was faster,
    /// and the output to keep: the engine's when it is named or faster and
    /// accurate, else the CPU's.
    fn verify(
        self: &Arc<Self>,
        frame: &Frame,
        quality: i32,
        named: bool,
    ) -> (Result<bool, Fault>, Option<Body<'static>>) {
        let refused = |miss: Miss| match miss {
            Miss::Declined(reason) => Fault::Declined(reason),
            Miss::Failed(reason) => Fault::Broken(reason),
            Miss::Unready(reason) => Fault::Unready(reason),
        };
        // The first encode of a size may open a session; it is checked, and
        // a second one timed.
        if let Err(miss) = self.engine_encode(frame, quality, true) {
            return (Err(refused(miss)), None);
        }
        let started = Instant::now();
        let timed = self.engine_encode(frame, quality, true);
        let engine_time = started.elapsed();
        let started = Instant::now();
        let reference = match cpu(frame, quality) {
            Ok(body) => body,
            Err(error) => return (Err(Fault::Broken(format!("{error:#}"))), None),
        };
        let cpu_time = started.elapsed();
        let engine = match timed {
            Ok(body) => body,
            Err(miss) => return (Err(refused(miss)), Some(reference)),
        };
        match accurate(frame, &engine, &reference) {
            Ok(()) => {
                let faster = engine_time < cpu_time;
                let keep = if faster || named { engine } else { reference };
                (Ok(faster), Some(keep))
            }
            Err(fault) => (Err(fault), Some(reference)),
        }
    }

    /// Keep what a check after `failures` doubtful ones found for `key`; a
    /// check that could not run finds nothing, and the next frame is
    /// checked.
    fn settle(&self, key: Key, checked: &Result<bool, Fault>, failures: u32) {
        let verdict = match checked {
            Ok(faster) => Verdict::Accurate { faster: *faster },
            Err(Fault::Broken(reason)) => Verdict::Rejected(reason.clone()),
            Err(Fault::Declined(reason)) => Verdict::Declined(reason.clone()),
            Err(Fault::Unfaithful(_)) => Verdict::Doubtful {
                failures: failures + 1,
                since: 0,
            },
            Err(Fault::Unready(_)) if failures == 0 => {
                self.verdicts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
                return;
            }
            Err(Fault::Unready(_)) => Verdict::Doubtful { failures, since: 0 },
        };
        self.remember(key, verdict);
    }
}

/// What an encoder thread does with a frame the engine takes.
enum Plan {
    Engine,
    Cpu,
    /// A named engine found broken for its size and format.
    Fail(String),
    /// Check the engine on it, after `failures` doubtful checks.
    Check {
        failures: u32,
    },
}

impl Engine {
    #[deny(trivial_casts)]
    pub(crate) fn new<B: Backend + 'static>(
        kind: Kind,
        open: impl FnOnce() -> Result<B> + Send + 'static,
    ) -> Self {
        let open: Opener =
            Box::new(move || open().map(|backend| -> Box<dyn Backend> { Box::new(backend) }));
        Self {
            accepts: B::accepts,
            shared: Arc::new(Shared {
                kind,
                state: Mutex::new(State::Closed(Some(open))),
                busy: AtomicBool::new(false),
                verdicts: Mutex::new(HashMap::new()),
                open_wait: OPEN_WAIT,
                idle: IDLE,
                resting: AtomicBool::new(false),
                off: AtomicBool::new(false),
                about: Mutex::new(Map::new()),
                absent: || None,
                absence: std::sync::OnceLock::new(),
            }),
            any_size: false,
        }
    }

    /// Not opened, and unavailable, on a host where `absent` says why.
    // The GPU engines run on Linux.
    #[cfg_attr(
        not(all(jpeg_helper, target_os = "linux")),
        allow(
            dead_code,
            reason = "hardware encoder interfaces are shared across platform and feature combinations for tests and doctor reporting"
        )
    )]
    pub(crate) fn absent_when(mut self, absent: fn() -> Option<String>) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("not shared yet")
            .absent = absent;
        self
    }

    /// Allowed `wait` to open, and to wake from rest.
    #[cfg_attr(
        not(all(jpeg_helper, target_os = "linux")),
        allow(
            dead_code,
            reason = "hardware encoder interfaces are shared across platform and feature combinations for tests and doctor reporting"
        )
    )]
    pub(crate) fn open_within(mut self, wait: Duration) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("not shared yet")
            .open_wait = wait;
        self
    }

    fn kind(&self) -> Kind {
        self.shared.kind
    }

    /// `auto` hands it frames of any size.
    #[cfg(all(test, feature = "jpeg", jpeg_helper))]
    pub(crate) const fn any_size(mut self) -> Self {
        self.any_size = true;
        self
    }

    /// Why the engine cannot be used, or None when it opens.
    pub(crate) fn unavailable(&self) -> Option<String> {
        self.shared.unavailable()
    }

    /// Whether `auto` should hand it `frame` at `quality`: it takes such
    /// frames, may be on this host and has not failed.
    fn wants(&self, frame: &Frame, quality: i32) -> bool {
        let view = FrameRef::whole(frame);
        (self.accepts)(view, quality)
            && (self.any_size || view.pixels() >= MIN_PIXELS)
            && !self.shared.is_off()
            && self.shared.absence().is_none()
    }

    /// Encode `frame` at `quality`: on the engine when `choice` and the
    /// frame call for it and it is accurate there, else on the CPU.
    pub(crate) fn encode(&self, frame: &Frame, quality: i32, named: bool) -> Result<Body<'static>> {
        let view = FrameRef::whole(frame);
        let takes = (self.accepts)(view, quality)
            && (named || self.any_size || view.pixels() >= MIN_PIXELS);
        // An invalid frame (a truncated one) says nothing of the engine: the
        // CPU says what is wrong with it, and the next frame of its size is
        // the engine's as before.
        if !takes || crate::frame::validate_source(frame).is_err() {
            return cpu(frame, quality);
        }
        // One frame at a time: under `auto` another thread's frame goes to
        // the CPU meanwhile, a named engine is waited for.
        let started = Instant::now();
        while self
            .shared
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if !named || started.elapsed() > deadline(frame) {
                return cpu(frame, quality);
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        self.encode_held(Held(self.shared.clone()), frame, quality, named)
    }

    fn encode_held(
        &self,
        held: Held,
        frame: &Frame,
        quality: i32,
        named: bool,
    ) -> Result<Body<'static>> {
        let shared = &self.shared;
        let key = (frame.width, frame.height, frame.pixel_format, quality);
        let plan = {
            let mut verdicts = shared
                .verdicts
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match verdicts.get_mut(&key) {
                Some(Verdict::Accurate { faster }) if *faster || named => Plan::Engine,
                Some(Verdict::Rejected(reason)) if named => Plan::Fail(reason.clone()),
                Some(Verdict::Accurate { .. } | Verdict::Rejected(_) | Verdict::Declined(_)) => {
                    Plan::Cpu
                }
                Some(Verdict::Doubtful { failures, since }) => {
                    *since += 1;
                    if named || *since >= 1 << (*failures).min(MOST_DOUBT) {
                        Plan::Check {
                            failures: *failures,
                        }
                    } else {
                        Plan::Cpu
                    }
                }
                // A check holds the engine until it has settled, so one
                // left checking ended without settling: check again.
                Some(Verdict::Checking { failures }) => Plan::Check {
                    failures: *failures,
                },
                None => Plan::Check { failures: 0 },
            }
        };
        match plan {
            Plan::Engine => match shared.engine_encode(frame, quality, named) {
                Ok(body) => Ok(body),
                Err(Miss::Failed(reason)) if named => Err(unavailable(self.kind(), &reason)),
                Err(miss) => {
                    if let Miss::Declined(reason) = miss {
                        shared.settle(key, &Err(Fault::Declined(reason)), 0);
                    }
                    drop(held);
                    cpu(frame, quality)
                }
            },
            Plan::Cpu => {
                drop(held);
                cpu(frame, quality)
            }
            Plan::Fail(reason) => Err(unavailable(self.kind(), &reason)),
            // A named engine is checked on the very frame it is given.
            Plan::Check { failures } if named => {
                let (checked, body) = shared.verify(frame, quality, true);
                shared.settle(key, &checked, failures);
                match (checked, body) {
                    (Err(Fault::Declined(_) | Fault::Unready(_)), _) => {
                        drop(held);
                        cpu(frame, quality)
                    }
                    (Err(fault), _) => Err(unavailable(self.kind(), &fault.to_string())),
                    (Ok(_), Some(body)) => Ok(body),
                    (Ok(_), None) => cpu(frame, quality),
                }
            }
            // Under `auto` the frame is the CPU's at once, and the engine is
            // checked on it by a thread of its own, which holds the engine
            // until it is done: no capture waits for a check.
            Plan::Check { failures } => {
                shared.remember(key, Verdict::Checking { failures });
                let (checking, checked) = (shared.clone(), frame.clone());
                let _ = std::thread::Builder::new()
                    .name(format!("jpeg-{}-check", self.kind().name()))
                    .spawn(move || {
                        let _held = held;
                        let (result, _) = checking.verify(&checked, quality, false);
                        checking.settle(key, &result, failures);
                    });
                cpu(frame, quality)
            }
        }
    }

    /// Wait until no frame or check is with the engine.
    #[cfg(all(test, feature = "jpeg"))]
    pub(crate) fn settled(&self) -> &Self {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.shared.busy.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the engine's check did not end");
            std::thread::sleep(Duration::from_millis(2));
        }
        self
    }

    /// Whether this engine has been checked on any frame yet, and passed.
    #[cfg(all(test, feature = "jpeg"))]
    pub(crate) fn verified(&self) -> Option<bool> {
        let verdicts = self
            .shared
            .verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if verdicts.is_empty() {
            return None;
        }
        Some(
            verdicts
                .values()
                .any(|verdict| matches!(verdict, Verdict::Accurate { .. })),
        )
    }

    /// Check the engine on a synthetic color frame, as its first frame of a
    /// size would be: for `doctor`.
    pub(crate) fn self_test(&self) -> Result<(), String> {
        let frame = gradient(1280, 720);
        let (checked, _) = self.shared.verify(&frame, QUALITY, false);
        self.shared.settle(
            (frame.width, frame.height, frame.pixel_format, QUALITY),
            &checked,
            0,
        );
        checked.map(|_| ()).map_err(|fault| fault.to_string())
    }

    /// What the backend said of itself when it opened.
    pub(crate) fn about(&self) -> Map<String, Value> {
        self.shared
            .about
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// How long an engine may take to open before it is given up on.
const OPEN_WAIT: Duration = Duration::from_secs(5);

/// How long an engine may go unused before it rests.
const IDLE: Duration = Duration::from_secs(60);

/// How long an engine may take for a frame before it is given up on: 2 s
/// plus 1 ms for every 50 kB of the frame.
pub(crate) fn deadline(frame: &Frame) -> Duration {
    Duration::from_secs(2) + Duration::from_millis(frame.data.len() as u64 / 50_000)
}

/// Frames Apple's JPEG engine takes: color frames at the default quality.
/// It writes three components, so libjpeg-turbo keeps gray frames, which it
/// reads in place; and its quality follows a calibrated setting.
#[cfg_attr(
    not(all(feature = "videotoolbox", target_os = "macos")),
    allow(
        dead_code,
        reason = "hardware encoder interfaces are shared across platform and feature combinations for tests and doctor reporting"
    )
)]
pub(crate) fn color_at_default_quality(view: FrameRef<'_>, quality: i32) -> bool {
    quality == QUALITY
        && crate::frame::convertible(view.pixel_format())
        && !crate::frame::monochrome(view.pixel_format())
}

/// A smooth, saturated color frame: a matrix or range mismatch shows as a
/// mean shift of several levels per channel.
fn gradient(width: u32, height: u32) -> Frame {
    let (w, h) = (width as usize, height as usize);
    let data: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            [
                u8::try_from(x * 255 / w).expect("gradient is within 0..255"),
                u8::try_from(y * 255 / h).expect("gradient is within 0..255"),
                200,
            ]
        })
        .collect();
    Frame {
        width,
        height,
        pixel_format: crate::types::RGB8,
        data: data.into(),
        ..Frame::default()
    }
}

/// Whether an engine's JPEG of `frame` is as faithful as the CPU's: decoded,
/// no channel is shifted by a level or more on average against the frame's
/// RGB conversion (else it is broken), and its PSNR is no more than 3 dB
/// below the CPU's, and at least 30 dB where the CPU's is (else it is
/// unfaithful). A detailed frame can be below 30 dB from any JPEG of it.
#[cfg(feature = "jpeg")]
#[deny(clippy::cast_precision_loss)]
pub(crate) fn accurate(frame: &Frame, engine: &[u8], cpu: &[u8]) -> Result<(), Fault> {
    let source =
        crate::frame::rgb_samples(frame).map_err(|error| Fault::Broken(format!("{error:#}")))?;
    let dimensions = (frame.width as usize, frame.height as usize);
    let failed = |error| Fault::Broken(format!("its JPEG does not decode: {error}"));
    let prepare = |jpeg: &[u8], mismatch: &str| {
        let mut decoder = turbojpeg::Decompressor::new().map_err(failed)?;
        let header = decoder.read_header(jpeg).map_err(failed)?;
        if (header.width, header.height) != dimensions {
            return Err(Fault::Broken(mismatch.into()));
        }
        Ok(decoder)
    };
    // Validate both headers before allocating either decoded image, each
    // with its own decoder state as independent JPEG streams.
    let engine_decoder = prepare(engine, "its JPEG has another size")?;
    let cpu_decoder = prepare(cpu, "libjpeg-turbo's JPEG has another size")?;
    let decode = |mut decoder: turbojpeg::Decompressor, jpeg: &[u8]| {
        let mut image = turbojpeg::Image {
            pixels: vec![0; source.len()],
            width: dimensions.0,
            pitch: dimensions.0 * 3,
            height: dimensions.1,
            format: turbojpeg::PixelFormat::RGB,
        };
        decoder
            .decompress(jpeg, image.as_deref_mut())
            .map_err(failed)?;
        Ok::<_, Fault>(image)
    };
    let (engine, cpu) = (decode(engine_decoder, engine)?, decode(cpu_decoder, cpu)?);
    let samples = 3.0 * f64::from(frame.width) * f64::from(frame.height);
    let compare = |image: &turbojpeg::Image<Vec<u8>>| {
        let mut squared = 0f64;
        let mut bias = [0f64; 3];
        let width = image.width;
        for y in 0..image.height {
            let row = &image.pixels[y * image.pitch..][..3 * width];
            let source = &source[3 * y * width..][..3 * width];
            for (i, (&a, &b)) in row.iter().zip(source).enumerate() {
                let difference = f64::from(a) - f64::from(b);
                squared += difference * difference;
                bias[i % 3] += difference;
            }
        }
        let mse = squared / samples;
        let psnr = if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        };
        (psnr, bias.map(|total| total / (samples / 3.0)))
    };
    let (psnr, bias) = compare(&engine);
    let (cpu_psnr, _) = compare(&cpu);
    if let Some((channel, shift)) = bias
        .iter()
        .enumerate()
        .find(|(_, shift)| shift.abs() >= 1.0)
    {
        return Err(Fault::Broken(format!(
            "its colors are shifted ({} by {shift:.2} levels)",
            ["red", "green", "blue"][channel]
        )));
    }
    if psnr < cpu_psnr - 3.0 || (psnr < 30.0 && cpu_psnr >= 30.0) {
        return Err(Fault::Unfaithful(format!(
            "its JPEG is {psnr:.1} dB PSNR from the frame, libjpeg-turbo's {cpu_psnr:.1} dB"
        )));
    }
    Ok(())
}

/// The hardware engines of this build for this platform, in the order
/// `auto` prefers them.
pub(crate) fn engines() -> &'static [Engine] {
    static ENGINES: std::sync::OnceLock<Vec<Engine>> = std::sync::OnceLock::new();
    ENGINES.get_or_init(|| {
        vec![
            #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
            Engine::new(Kind::VideoToolbox, super::videotoolbox::Encoder::open),
            #[cfg(all(jpeg_helper, feature = "nvjpeg", target_os = "linux"))]
            super::helper::engine(Kind::Nvjpeg),
            #[cfg(all(jpeg_helper, feature = "vaapi", target_os = "linux"))]
            super::helper::engine(Kind::Vaapi),
        ]
    })
}

/// The engine `auto` gives `frame` at `quality`: the first that wants it.
fn choose<'a>(engines: &'a [Engine], frame: &Frame, quality: i32) -> Option<&'a Engine> {
    engines.iter().find(|engine| engine.wants(frame, quality))
}

/// Whether this build has the engine for this platform.
const fn compiled(kind: Kind) -> bool {
    match kind {
        Kind::VideoToolbox => cfg!(all(feature = "videotoolbox", target_os = "macos")),
        Kind::Nvjpeg => cfg!(all(jpeg_helper, feature = "nvjpeg", target_os = "linux")),
        Kind::Vaapi => cfg!(all(jpeg_helper, feature = "vaapi", target_os = "linux")),
        #[cfg(all(test, feature = "jpeg"))]
        Kind::Fake => false,
    }
}

/// Why a named engine is not in this build or on this platform.
fn missing(kind: Kind) -> String {
    match kind {
        Kind::VideoToolbox if cfg!(target_os = "macos") => {
            "this build has no VideoToolbox support (the videotoolbox feature)".into()
        }
        Kind::VideoToolbox => "VideoToolbox exists only on macOS".into(),
        Kind::Nvjpeg | Kind::Vaapi if !cfg!(target_os = "linux") => {
            format!("{} JPEG encoding is used only on Linux", kind.name())
        }
        Kind::Nvjpeg => "this build has no nvJPEG support (the nvjpeg feature)".into(),
        Kind::Vaapi => "this build has no VA-API support (the vaapi feature)".into(),
        #[cfg(all(test, feature = "jpeg"))]
        Kind::Fake => "no fake engine in this build".into(),
    }
}

/// A still at `quality` (the default when None) by the encoder
/// `CAPTUREFAB_JPEG` asks for.
pub(crate) fn encode(frame: &Frame, quality: Option<u8>) -> Result<Body<'static>> {
    let quality = quality.map_or(QUALITY, i32::from);
    match choice()? {
        Choice::Cpu => cpu(frame, quality),
        Choice::Auto => choose(engines(), frame, quality).map_or_else(
            || cpu(frame, quality),
            |engine| engine.encode(frame, quality, false),
        ),
        Choice::Named(kind) => engines()
            .iter()
            .find(|engine| engine.kind() == kind)
            .map_or_else(
                || Err(unavailable(kind, &missing(kind))),
                |engine| engine.encode(frame, quality, true),
            ),
    }
}

/// `doctor`'s `jpeg` object: what `CAPTUREFAB_JPEG` selects and every
/// encoder this build knows, checking each hardware engine this host has
/// on a test frame once a boot (`checked`).
pub(crate) fn report() -> Value {
    let selected = match choice() {
        Ok(choice) => serde_json::json!(choice.name()),
        Err(error) => serde_json::json!(format!("{error:#}")),
    };
    let mut backends = serde_json::json!({
        "libjpeg_turbo": {"compiled": cfg!(feature = "jpeg"), "simd": cfg!(feature = "jpeg")},
    });
    for kind in [Kind::VideoToolbox, Kind::Nvjpeg, Kind::Vaapi] {
        let mut entry = serde_json::json!({
            "compiled": compiled(kind),
            "available": false,
        });
        match engines().iter().find(|engine| engine.kind() == kind) {
            Some(engine) => {
                let found = checked::recall(kind.name()).unwrap_or_else(|| {
                    let found = check(engine);
                    let ok = found["verified"] == true;
                    checked::remember(kind.name(), &found, ok);
                    found
                });
                if let Value::Object(found) = found {
                    for (key, value) in found {
                        entry[key] = value;
                    }
                }
            }
            None => entry["reason"] = missing(kind).into(),
        }
        backends[kind.name()] = entry;
    }
    serde_json::json!({
        "selected": selected,
        "backends": backends,
    })
}

/// Open `engine` and encode the test frame on it: whether it is available
/// and verified, why not, and what it says of itself.
fn check(engine: &Engine) -> Value {
    let mut entry = serde_json::json!({"available": false});
    if let Some(reason) = engine.unavailable() {
        entry["reason"] = reason.into();
    } else {
        entry["available"] = true.into();
        match engine.self_test() {
            Ok(()) => entry["verified"] = true.into(),
            Err(reason) => {
                entry["verified"] = false.into();
                entry["reason"] = reason.into();
            }
        }
        for (key, value) in engine.about() {
            entry[key] = value;
        }
    }
    entry
}

/// What `doctor` found of each engine, kept in the cache for the boot, so
/// a later `doctor` neither opens the hardware nor encodes its test frame
/// again: under this build and the variables that choose a device, and a
/// failure for five minutes only, as `FFmpeg`'s probes are.
mod checked {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::{collections::BTreeMap, path::PathBuf};

    #[derive(Default, Serialize, Deserialize)]
    struct Checks {
        /// The boot, build and settings the checks hold for.
        key: String,
        engines: BTreeMap<String, Check>,
    }
    #[derive(Clone, Serialize, Deserialize)]
    struct Check {
        found: Value,
        ok: bool,
        at_ms: u64,
    }

    /// How long a check that failed is believed: a busy GPU may refuse
    /// once and work soon after.
    const FAILURE_KEPT_MS: u64 = 300_000;
    /// What else chooses an engine's device or helper.
    const SETTINGS: [&str; 3] = [
        "CAPTUREFAB_CUDA_LIB_DIR",
        "CAPTUREFAB_VAAPI_DEVICE",
        "CAPTUREFAB_EXECUTABLE",
    ];

    /// This boot, this executable (its path, size and time, which a new
    /// build changes) and the settings; None where there is no boot ID.
    fn key() -> Option<String> {
        let boot = crate::media::boot_id()?;
        let exe = std::env::current_exe().ok()?;
        let meta = std::fs::metadata(&exe).ok()?;
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let mut key = format!(
            "{}\n{}\n{}\n{modified}",
            boot.trim(),
            exe.display(),
            meta.len()
        );
        use std::fmt::Write as _;
        for name in SETTINGS {
            let value = std::env::var_os(name).unwrap_or_default();
            let _ = write!(key, "\n{name}={}", value.to_string_lossy());
        }
        Some(key)
    }
    fn path() -> Option<PathBuf> {
        Some(crate::dirs::cache()?.join("jpeg-engines.json"))
    }
    /// The checks kept for `key`.
    fn read(path: &PathBuf, key: &str) -> Checks {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Checks>(&bytes).ok())
            .filter(|checks| checks.key == key)
            .unwrap_or_default()
    }
    /// What a check of engine `name` found this boot, while it holds.
    pub(super) fn recall(name: &str) -> Option<Value> {
        let checks = read(&path()?, &key()?);
        let check = checks.engines.get(name)?;
        let age = crate::scheduling::now_ms().checked_sub(check.at_ms)?;
        (check.ok || age < FAILURE_KEPT_MS).then(|| check.found.clone())
    }
    /// Keep what a check of engine `name` found; `ok` when it passed.
    pub(super) fn remember(name: &str, found: &Value, ok: bool) {
        let (Some(path), Some(key)) = (path(), key()) else {
            return;
        };
        let mut checks = read(&path, &key);
        checks.key = key;
        checks.engines.insert(
            name.to_owned(),
            Check {
                found: found.clone(),
                ok,
                at_ms: crate::scheduling::now_ms(),
            },
        );
        let Ok(bytes) = serde_json::to_vec(&checks) else {
            return;
        };
        if let Some(dir) = path.parent()
            && crate::dirs::ensure_private_dir(dir).is_err()
        {
            return;
        }
        let _ = crate::dirs::write_private(&path, &bytes, false);
    }
}

#[cfg(all(test, feature = "jpeg"))]
mod tests {
    use super::*;
    use crate::jpeg::Raster;
    use anyhow::bail;
    use std::sync::atomic::AtomicUsize;

    /// What a fake engine does with each frame.
    #[derive(Clone)]
    enum Scenario {
        /// Encode on the CPU, recording the frame buffer it was handed.
        Echo(Arc<Mutex<Vec<usize>>>),
        Fails,
        Malformed,
        /// Darken every frame, as an engine with the wrong matrix would.
        Biased,
        Sleeps(Duration),
        /// Encode at a low quality while the flag is set, as an engine
        /// would on content it handles worse than libjpeg-turbo.
        Coarse(Arc<AtomicBool>),
    }

    struct Fake(Scenario, Arc<AtomicUsize>);

    impl Backend for Fake {
        fn accepts(view: FrameRef<'_>, quality: i32) -> bool {
            color_at_default_quality(view, quality)
        }
        fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>> {
            self.1.fetch_add(1, Ordering::Relaxed);
            match &self.0 {
                Scenario::Echo(seen) => {
                    seen.lock()
                        .map_err(|_| anyhow::anyhow!("test fixture lock is poisoned"))?
                        .push(view.frame.data.as_ptr() as usize);
                    super::super::encode(view.frame, quality)
                }
                Scenario::Fails => bail!("engine error"),
                Scenario::Malformed => Ok(Body::Vec(vec![0xFF, 0xD8, 0xFF, 0xD9])),
                Scenario::Biased => {
                    let rgb = crate::frame::rgb(view.frame)?;
                    let darker: Vec<u8> = rgb.iter().map(|v| v.saturating_sub(6)).collect();
                    let raster = Raster::of(view.frame.width, view.frame.height, 3, &darker);
                    super::super::software_quality(&raster, quality)
                }
                Scenario::Sleeps(time) => {
                    std::thread::sleep(*time);
                    super::super::encode(view.frame, quality)
                }
                Scenario::Coarse(coarse) => {
                    let quality = if coarse.load(Ordering::Relaxed) {
                        50
                    } else {
                        quality
                    };
                    super::super::encode(view.frame, quality)
                }
            }
        }
    }

    fn engine(scenario: Scenario) -> (Engine, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mut engine = Engine::new(Kind::Fake, move || Ok(Fake(scenario, counter)));
        engine.any_size = true;
        (engine, calls)
    }

    #[test]
    fn changing_geometry_keeps_validation_memory_bounded() {
        let (engine, calls) = engine(Scenario::Echo(Arc::default()));
        let shared = &engine.shared;
        for width in 1..=1024 {
            let key = (width, 64, crate::types::RGB8, QUALITY);
            shared.remember(key, Verdict::Checking { failures: 0 });
            shared.settle(key, &Ok(true), 0);
            let verdicts = shared.verdicts.lock().unwrap();
            assert!(verdicts.len() <= MAX_VERDICTS);
            assert!(matches!(
                verdicts.get(&key),
                Some(Verdict::Accurate { faster: true })
            ));
            drop(verdicts);
        }
        // Updating an existing combination must not evict another one.
        let key = (1024, 64, crate::types::RGB8, QUALITY);
        shared.settle(key, &Err(Fault::Declined("unsupported size".into())), 0);
        assert_eq!(shared.verdicts.lock().unwrap().len(), MAX_VERDICTS);
        let width = (128..1024)
            .find(|&width| {
                !shared.verdicts.lock().unwrap().contains_key(&(
                    width,
                    64,
                    crate::types::RGB8,
                    QUALITY,
                ))
            })
            .unwrap();
        let frame = gradient(width, 64);
        let jpeg = engine.encode(&frame, QUALITY, true).unwrap();
        assert!(super::super::well_formed(&jpeg, width, 64));
        // A fresh check encodes twice; trusting an old verdict encodes once.
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(engine.verified(), Some(true));
        assert_eq!(shared.verdicts.lock().unwrap().len(), MAX_VERDICTS);
    }

    /// A smooth frame in `format`: the RGB gradient, or a mosaic or gray
    /// ramp of the same size.
    fn frame(format: u32) -> Frame {
        let frame = gradient(96, 64);
        if format == crate::types::RGB8 {
            return frame;
        }
        let ramp = Frame {
            pixel_format: crate::types::MONO8,
            data: (0..96 * 64)
                .map(|i| u8::try_from((i % 96 + i / 96) * 255 / 160).unwrap())
                .collect(),
            ..frame
        };
        match format {
            crate::types::MONO8 | 0x0108_0009 => Frame {
                pixel_format: format,
                ..ramp
            },
            _ => crate::frame::noise_frame(format, 96, 64),
        }
    }

    #[test]
    fn choices_are_parsed_and_unknown_ones_are_usage_errors() {
        assert_eq!(Choice::parse(None).unwrap(), Choice::Auto);
        assert_eq!(Choice::parse(Some(" cpu ")).unwrap(), Choice::Cpu);
        for kind in [Kind::VideoToolbox, Kind::Nvjpeg, Kind::Vaapi] {
            assert_eq!(
                Choice::parse(Some(kind.name())).unwrap(),
                Choice::Named(kind)
            );
        }
        let error = Choice::parse(Some("gpu")).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
        assert!(format!("{error:#}").contains("nvjpeg or vaapi"));
    }

    #[test]
    fn doctor_keeps_what_it_found_of_an_engine_for_the_boot() {
        // Without a boot to key them to, checks are not kept.
        if crate::media::boot_id().is_none() {
            return;
        }
        let name = format!("test-engine-{}", std::process::id());
        assert_eq!(checked::recall(&name), None);
        let found = serde_json::json!({"available": true, "verified": true});
        checked::remember(&name, &found, true);
        assert_eq!(checked::recall(&name), Some(found));
        // A failure is believed for a while too, not opened again at once.
        let failed = serde_json::json!({"available": false, "reason": "busy"});
        checked::remember(&name, &failed, false);
        assert_eq!(checked::recall(&name), Some(failed));
    }

    #[test]
    fn engines_read_the_frame_buffer_itself() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (engine, _) = engine(Scenario::Echo(seen.clone()));
        let frame = frame(0x0108_0009);
        for _ in 0..3 {
            let jpeg = engine.encode(&frame, QUALITY, true).unwrap();
            assert!(super::super::well_formed(&jpeg, 96, 64));
        }
        let seen = seen.lock().unwrap();
        assert!(seen.len() >= 3);
        assert!(
            seen.iter()
                .all(|&address| address == frame.data.as_ptr() as usize)
        );
        drop(seen);
        assert_eq!(engine.verified(), Some(true));
    }

    #[test]
    fn auto_keeps_monochrome_other_qualities_and_small_frames_on_the_cpu() {
        let (mut engine, calls) = engine(Scenario::Fails);
        engine
            .encode(&frame(crate::types::MONO8), QUALITY, false)
            .unwrap();
        engine.encode(&frame(0x010c_0047), QUALITY, false).unwrap();
        engine.encode(&frame(0x0108_0009), 75, false).unwrap();
        engine.any_size = false;
        engine.encode(&frame(0x0108_0009), QUALITY, false).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn engine_errors_and_malformed_output_fall_back_under_auto_and_fail_when_named() {
        for scenario in [Scenario::Fails, Scenario::Malformed] {
            let (engine, _) = engine(scenario.clone());
            let frame = frame(0x0108_0009);
            let cpu = cpu(&frame, QUALITY).unwrap();
            assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
            assert_eq!(engine.settled().verified(), Some(false));
            let (engine, _) = tests::engine(scenario);
            let error = engine.encode(&frame, QUALITY, true).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unavailable);
        }
    }

    #[test]
    fn inaccurate_engines_are_rejected_on_their_first_frame() {
        let (engine, calls) = engine(Scenario::Biased);
        let frame = frame(crate::types::RGB8);
        let cpu = cpu(&frame, QUALITY).unwrap();
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        let checked = engine.settled().shared.verdicts.lock().unwrap().len();
        assert_eq!(checked, 1);
        let checked = calls.load(Ordering::Relaxed);
        // Rejected for good: no more frames go to it.
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        assert_eq!(calls.load(Ordering::Relaxed), checked);
        let error = engine.encode(&frame, QUALITY, true).unwrap_err();
        assert!(format!("{error:#}").contains("shifted"), "{error:#}");
    }

    #[test]
    fn engines_past_their_deadline_are_turned_off() {
        let (engine, _) = engine(Scenario::Sleeps(Duration::from_secs(3)));
        let frame = frame(0x0108_0009);
        let cpu = cpu(&frame, QUALITY).unwrap();
        let started = Instant::now();
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        engine.settled();
        assert!(started.elapsed() < Duration::from_millis(2900));
        assert!(engine.unavailable().unwrap().contains("longer than"));
        let error = engine.encode(&frame, QUALITY, true).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unavailable);
    }

    /// Under `auto` a frame never waits for the engine's check, which runs
    /// on a thread of its own; later frames use what it found.
    #[test]
    fn auto_checks_the_engine_beside_the_frame() {
        let (engine, calls) = engine(Scenario::Sleeps(Duration::from_millis(400)));
        let frame = frame(0x0108_0009);
        let cpu = cpu(&frame, QUALITY).unwrap();
        let started = Instant::now();
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        assert!(
            started.elapsed() < Duration::from_millis(350),
            "waited for the check"
        );
        // Checked: accurate, and slower than the CPU, so frames stay there.
        assert_eq!(engine.settled().verified(), Some(true));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// An engine less faithful than libjpeg-turbo on a frame is checked
    /// again on later ones, as the content may be to blame: on the next
    /// frame when named, after 2, 4, 8... frames under `auto`.
    #[test]
    fn unfaithful_engines_are_checked_again() {
        let coarse = Arc::new(AtomicBool::new(true));
        let (engine, calls) = engine(Scenario::Coarse(coarse.clone()));
        let frame = frame(crate::types::RGB8);
        for checks in [2, 4] {
            let error = engine.encode(&frame, QUALITY, true).unwrap_err();
            assert_eq!(crate::types::classify(&error).kind, ErrorKind::Unavailable);
            assert!(format!("{error:#}").contains("dB PSNR"), "{error:#}");
            assert_eq!(calls.load(Ordering::Relaxed), checks);
        }
        coarse.store(false, Ordering::Relaxed);
        let jpeg = engine.encode(&frame, QUALITY, true).unwrap();
        assert_eq!(jpeg, cpu(&frame, QUALITY).unwrap());
        assert_eq!(engine.verified(), Some(true));

        let (engine, calls) = tests::engine(Scenario::Coarse(coarse.clone()));
        coarse.store(true, Ordering::Relaxed);
        let mut checked = Vec::new();
        for _ in 0..8 {
            let before = calls.load(Ordering::Relaxed);
            engine.encode(&frame, QUALITY, false).unwrap();
            engine.settled();
            checked.push(calls.load(Ordering::Relaxed) > before);
        }
        // Frames 1, 3 and 7: after one, two and four more frames.
        let expected = [true, false, true, false, false, false, true, false];
        assert_eq!(checked, expected);
        assert_eq!(engine.verified(), Some(false));
    }

    #[test]
    fn engines_that_do_not_open_in_time_are_given_up() {
        let mut engine = Engine::new(Kind::Fake, || {
            std::thread::sleep(Duration::from_secs(2));
            Ok(Fake(Scenario::Fails, Arc::default()))
        });
        Arc::get_mut(&mut engine.shared).unwrap().open_wait = Duration::from_millis(100);
        let started = Instant::now();
        let reason = engine.unavailable().unwrap();
        assert!(reason.contains("did not open within 100 ms"), "{reason}");
        assert!(started.elapsed() < Duration::from_secs(1));
        let frame = frame(0x0108_0009);
        let error = engine.encode(&frame, QUALITY, true).unwrap_err();
        assert!(format!("{error:#}").contains("did not open"), "{error:#}");
        engine.any_size = true;
        let cpu = cpu(&frame, QUALITY).unwrap();
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
    }

    /// A detailed frame is far below 30 dB from any JPEG of it (noise:
    /// about 13 dB): an engine as faithful as libjpeg-turbo there passes;
    /// one much coarser on a smooth frame does not.
    #[test]
    fn engines_are_judged_against_libjpeg_turbo_on_the_same_frame() {
        let noise = crate::frame::noise_frame(crate::types::RGB8, 96, 64);
        let cpu = cpu(&noise, QUALITY).unwrap();
        assert_eq!(accurate(&noise, &cpu, &cpu), Ok(()));
        let smooth = gradient(96, 64);
        let reference = super::cpu(&smooth, QUALITY).unwrap();
        let coarse = super::cpu(&smooth, 50).unwrap();
        assert_eq!(accurate(&smooth, &reference, &reference), Ok(()));
        assert!(matches!(
            accurate(&smooth, &coarse, &reference),
            Err(Fault::Unfaithful(_))
        ));
    }

    #[test]
    fn quality_comparison_requires_both_jpegs_to_have_the_frame_dimensions() {
        let frame = gradient(96, 64);
        let reference = cpu(&frame, QUALITY).unwrap();
        for (width, height) in [(128, 64), (16, 16), (64, 96)] {
            let wrong = cpu(&gradient(width, height), QUALITY).unwrap();
            assert!(matches!(
                accurate(&frame, &reference, &wrong),
                Err(Fault::Broken(_))
            ));
            assert!(matches!(
                accurate(&frame, &wrong, &reference),
                Err(Fault::Broken(_))
            ));
        }
    }

    #[test]
    fn quality_comparison_checks_dimensions_before_decoding_pixels() {
        let frame = gradient(96, 64);
        let reference = cpu(&frame, QUALITY).unwrap();
        let wrong = cpu(&gradient(128, 64), QUALITY).unwrap();
        let scan = wrong
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xda])
            .unwrap();
        let length = usize::from(u16::from_be_bytes([wrong[scan + 2], wrong[scan + 3]]));
        let header = &wrong[..scan + 2 + length];
        assert_eq!(
            accurate(&frame, header, &reference),
            Err(Fault::Broken("its JPEG has another size".into()))
        );
        assert_eq!(
            accurate(&frame, &reference, header),
            Err(Fault::Broken(
                "libjpeg-turbo's JPEG has another size".into()
            ))
        );
    }

    #[test]
    fn engines_that_cannot_open_say_why() {
        let engine = Engine::new(Kind::Fake, || -> Result<Fake> { bail!("no such device") });
        assert_eq!(engine.unavailable().as_deref(), Some("no such device"));
        let error = engine
            .encode(&frame(0x0108_0009), QUALITY, true)
            .unwrap_err();
        assert!(format!("{error:#}").contains("no such device"));
    }

    #[test]
    fn a_busy_engine_sends_other_frames_to_the_cpu_under_auto() {
        let (engine, calls) = engine(Scenario::Sleeps(Duration::from_millis(300)));
        let engine = Arc::new(engine);
        let frame = frame(0x0108_0009);
        // Verified once, so later frames go straight to the engine.
        engine.encode(&frame, QUALITY, true).unwrap();
        let before = calls.load(Ordering::Relaxed);
        let held = {
            let (engine, frame) = (engine.clone(), frame.clone());
            std::thread::spawn(move || engine.encode(&frame, QUALITY, true).unwrap())
        };
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        engine.encode(&frame, QUALITY, false).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "waited for the engine"
        );
        held.join().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), before + 1);
    }

    #[test]
    fn named_engines_this_build_lacks_are_unavailable() {
        if cfg!(target_os = "linux") {
            assert!(missing(Kind::Nvjpeg).contains("the nvjpeg feature"));
            assert!(missing(Kind::Vaapi).contains("the vaapi feature"));
            assert!(missing(Kind::VideoToolbox).contains("only on macOS"));
        } else {
            assert!(missing(Kind::Nvjpeg).contains("only on Linux"));
        }
        // Each engine of this build is named in doctor's report.
        let report = report();
        for kind in [Kind::VideoToolbox, Kind::Nvjpeg, Kind::Vaapi] {
            let entry = &report["backends"][kind.name()];
            assert_eq!(entry["compiled"], compiled(kind), "{kind:?}");
            if !compiled(kind) {
                assert_eq!(entry["available"], false);
                assert!(entry["reason"].is_string(), "{kind:?}");
            }
        }
    }

    /// An engine that declines a frame (`Unsupported`) leaves it to the CPU,
    /// named or not, and stays in use for others.
    #[test]
    fn declined_frames_go_to_the_cpu_and_the_engine_stays() {
        struct Declines;
        impl Backend for Declines {
            fn accepts(_: FrameRef<'_>, _: i32) -> bool {
                true
            }
            fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>> {
                if view.width == 96 {
                    return Err(Unsupported("too wide".into()).into());
                }
                super::super::encode(view.frame, quality)
            }
        }
        let mut engine = Engine::new(Kind::Fake, || Ok(Declines));
        engine.any_size = true;
        let wide = frame(crate::types::RGB8);
        let cpu = cpu(&wide, QUALITY).unwrap();
        assert_eq!(engine.encode(&wide, QUALITY, true).unwrap(), cpu);
        assert_eq!(engine.encode(&wide, QUALITY, false).unwrap(), cpu);
        assert!(engine.unavailable().is_none(), "still in use");
        let narrow = Frame {
            width: 48,
            height: 64,
            data: wide.data[..48 * 64 * 3].to_vec().into(),
            ..wide.clone()
        };
        engine.encode(&narrow, QUALITY, true).unwrap();
        assert_eq!(engine.verified(), Some(true));
    }

    /// A frame an engine cannot take now goes to the CPU, named or not, and
    /// leaves no verdict: the next frame of its kind is checked again.
    #[test]
    fn frames_an_engine_cannot_take_now_go_to_the_cpu() {
        struct Short(Arc<AtomicUsize>);
        impl Backend for Short {
            fn accepts(_: FrameRef<'_>, _: i32) -> bool {
                true
            }
            fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>> {
                if self.0.fetch_add(1, Ordering::Relaxed) < 2 {
                    return Err(Unready("no memory now".into()).into());
                }
                super::super::encode(view.frame, quality)
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls;
        let mut engine = Engine::new(Kind::Fake, move || Ok(Short(counter)));
        engine.any_size = true;
        let frame = frame(crate::types::RGB8);
        let cpu = cpu(&frame, QUALITY).unwrap();
        assert_eq!(engine.encode(&frame, QUALITY, true).unwrap(), cpu);
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        assert_eq!(engine.settled().verified(), None, "nothing learned");
        assert!(engine.unavailable().is_none());
        // Once it can, the next frame is checked and passes.
        engine.encode(&frame, QUALITY, true).unwrap();
        assert_eq!(engine.verified(), Some(true));
    }

    /// A truncated frame is the CPU's, which says what is wrong with it; the
    /// engine never sees it and keeps the next valid frame of its size,
    /// named or not.
    #[test]
    fn invalid_frames_leave_the_engine_as_it_was() {
        for named in [true, false] {
            let (engine, calls) = engine(Scenario::Echo(Arc::default()));
            let frame = frame(0x0108_0009);
            let truncated = Frame {
                data: frame.data[..frame.data.len() / 2].to_vec().into(),
                ..frame.clone()
            };
            let error = engine.encode(&truncated, QUALITY, named).unwrap_err();
            assert!(format!("{error:#}").contains("truncated"), "{error:#}");
            assert_ne!(
                crate::types::classify(&error).kind,
                ErrorKind::Unavailable,
                "{error:#}"
            );
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(engine.verified(), None, "nothing learned");
            engine.encode(&frame, QUALITY, named).unwrap();
            assert_eq!(engine.settled().verified(), Some(true));
            assert!(calls.load(Ordering::Relaxed) > 0);
            assert!(engine.unavailable().is_none());
        }
    }

    /// An engine unused for its idle time rests; under `auto` the next
    /// frame is the CPU's while it wakes, and a named engine waits for it.
    #[test]
    fn idle_engines_rest_and_wake() {
        #[derive(Default)]
        struct Counts {
            rests: AtomicUsize,
            wakes: AtomicUsize,
            encodes: AtomicUsize,
        }
        struct Rests(Arc<Counts>);
        impl Backend for Rests {
            fn accepts(_: FrameRef<'_>, _: i32) -> bool {
                true
            }
            fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>> {
                self.0.encodes.fetch_add(1, Ordering::Relaxed);
                super::super::encode(view.frame, quality)
            }
            fn rest(&mut self) -> bool {
                self.0.rests.fetch_add(1, Ordering::Relaxed);
                true
            }
            fn wake(&mut self) -> Result<()> {
                self.0.wakes.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
        let counts = Arc::new(Counts::default());
        let backend = counts.clone();
        let mut engine = Engine::new(Kind::Fake, move || Ok(Rests(backend)));
        engine.any_size = true;
        Arc::get_mut(&mut engine.shared).unwrap().idle = Duration::from_millis(100);
        let frame = frame(0x0108_0009);
        // Checked, and then used (named, so whether faster or not).
        engine.encode(&frame, QUALITY, true).unwrap();
        let encoded = counts.encodes.load(Ordering::Relaxed);
        let rest = |n| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while counts.rests.load(Ordering::Relaxed) < n {
                assert!(Instant::now() < deadline, "never rested");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        rest(1);
        engine.encode(&frame, QUALITY, true).unwrap();
        assert_eq!(counts.wakes.load(Ordering::Relaxed), 1);
        assert_eq!(counts.encodes.load(Ordering::Relaxed), encoded + 1);
        // Under `auto` an accurate, faster engine's frame goes to the CPU
        // while it wakes.
        rest(2);
        engine.shared.verdicts.lock().unwrap().insert(
            (frame.width, frame.height, frame.pixel_format, QUALITY),
            Verdict::Accurate { faster: true },
        );
        let cpu = cpu(&frame, QUALITY).unwrap();
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        assert_eq!(counts.encodes.load(Ordering::Relaxed), encoded + 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while counts.wakes.load(Ordering::Relaxed) < 2 {
            assert!(Instant::now() < deadline, "never woke");
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.encode(&frame, QUALITY, false).unwrap();
        assert_eq!(counts.encodes.load(Ordering::Relaxed), encoded + 2);
    }

    /// `auto` takes the first engine that wants a frame: one that has
    /// failed, or whose device the host lacks, is passed over.
    #[test]
    fn auto_passes_over_failed_and_absent_engines() {
        let (first, first_calls) = engine(Scenario::Fails);
        let (second, second_calls) = engine(Scenario::Echo(Arc::default()));
        let frame = frame(0x0108_0009);
        let engines = [first, second];
        let chosen = choose(&engines, &frame, QUALITY).unwrap();
        assert!(std::ptr::eq(chosen, &raw const engines[0]));
        // The first fails its check and is turned off.
        chosen.encode(&frame, QUALITY, false).unwrap();
        chosen.settled();
        assert!(first_calls.load(Ordering::Relaxed) > 0);
        let chosen = choose(&engines, &frame, QUALITY).unwrap();
        assert!(std::ptr::eq(chosen, &raw const engines[1]));
        chosen.encode(&frame, QUALITY, false).unwrap();
        chosen.settled();
        assert!(second_calls.load(Ordering::Relaxed) > 0);
        let mut absent = Engine::new(Kind::Fake, || Ok(Fake(Scenario::Fails, Arc::default())))
            .absent_when(|| Some("no such device".into()));
        absent.any_size = true;
        assert!(!absent.wants(&frame, QUALITY));
        assert_eq!(absent.unavailable().as_deref(), Some("no such device"));
    }
}
