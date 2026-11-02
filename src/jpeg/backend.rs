//! Which encoder writes a JPEG still: libjpeg-turbo on the CPU, or a
//! hardware engine (`CAPTUREFAB_JPEG=auto|cpu|videotoolbox`).
//!
//! `auto` hands a hardware engine the frames it is good at (color frames of
//! at least `MIN_PIXELS` at the default quality) and keeps the rest on the
//! CPU, which reads 8-bit gray and RGB frames in place. Before an engine
//! writes a frame size and format for the first time it is checked against
//! the CPU: both encode the frame, both are decoded, and the engine's must
//! be accurate (`accurate`); `auto` then keeps whichever was faster. Every
//! engine output must be a well-formed JPEG of the frame's size. An engine
//! that fails, or does not answer within its deadline, is not used again
//! by this process, and a frame it held up is encoded on the CPU.
//!
//! Naming an engine makes it a requirement: if it is missing or fails its
//! check, the still fails with `unavailable` instead of quietly using the
//! CPU. Frames it cannot take (monochrome, a non-default quality) still go
//! to the CPU.
//!
//! An engine runs on a thread of its own, so a driver that hangs costs one
//! abandoned thread, not the camera worker; frames reach it shared, not
//! copied.
// Builds without a hardware engine for their platform use only the CPU.
#![cfg_attr(
    not(all(feature = "videotoolbox", target_os = "macos")),
    allow(dead_code)
)]
use super::QUALITY;
use crate::{
    frame::{Body, FrameRef},
    types::{Categorized, ErrorKind, Frame},
};
use anyhow::Result;
use std::{
    collections::HashMap,
    sync::{
        Mutex, PoisonError,
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
    VideoToolbox,
    /// Reserved: NVIDIA's nvJPEG, in a later version.
    Nvjpeg,
    /// Reserved: VA-API JPEG encoding, in a later version.
    Vaapi,
    #[cfg(test)]
    Fake,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::VideoToolbox => "videotoolbox",
            Kind::Nvjpeg => "nvjpeg",
            Kind::Vaapi => "vaapi",
            #[cfg(test)]
            Kind::Fake => "fake",
        }
    }
}

impl Choice {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        Ok(match value.map(str::trim).unwrap_or("") {
            "" | "auto" => Choice::Auto,
            "cpu" => Choice::Cpu,
            "videotoolbox" => Choice::Named(Kind::VideoToolbox),
            "nvjpeg" => Choice::Named(Kind::Nvjpeg),
            "vaapi" => Choice::Named(Kind::Vaapi),
            other => {
                return Err(Categorized::error(
                    ErrorKind::Usage,
                    format!("{ENV}={other:?} is not a JPEG encoder; use auto, cpu or videotoolbox"),
                ));
            }
        })
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Choice::Auto => "auto",
            Choice::Cpu => "cpu",
            Choice::Named(kind) => kind.name(),
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
    /// `view` as a JPEG, from the frame's own buffer.
    fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>>;
}

/// Opens a backend on its engine thread; an error says why it cannot.
type Opener = Box<dyn FnOnce() -> Result<Box<dyn Backend>> + Send>;

struct Request {
    frame: Frame,
    quality: i32,
    reply: mpsc::SyncSender<Result<Body<'static>>>,
}

/// What a frame size and format were found to be on an engine.
#[derive(Clone, Debug, PartialEq)]
enum Verdict {
    /// Accurate; `faster` when it took less time than the CPU.
    Accurate {
        faster: bool,
    },
    Rejected(String),
}

enum State {
    /// Not opened yet.
    Closed(Option<Opener>),
    Open(mpsc::Sender<Request>),
    /// Not used again, and why.
    Off(String),
}

/// A hardware engine shared by every encoder thread of the process.
pub(crate) struct Engine {
    kind: Kind,
    /// The backend's `accepts`.
    accepts: fn(FrameRef<'_>, i32) -> bool,
    state: Mutex<State>,
    /// Whether a frame is with the engine; others use the CPU meanwhile.
    busy: AtomicBool,
    verdicts: Mutex<HashMap<(u32, u32, u32), Verdict>>,
    /// Whether `auto` hands it small frames (tests).
    any_size: bool,
}

/// How a still is encoded when the engine cannot or should not.
fn cpu(frame: &Frame, quality: i32) -> Result<Body<'static>> {
    super::encode(frame, quality)
}

/// Without libjpeg-turbo nothing can check an engine.
#[cfg(not(feature = "jpeg"))]
fn accurate(_: &Frame, _: &[u8], _: &[u8]) -> Result<(), String> {
    Err("checking it needs libjpeg-turbo (the jpeg feature)".into())
}

impl Engine {
    pub(crate) fn new<B: Backend + 'static>(
        kind: Kind,
        open: impl FnOnce() -> Result<B> + Send + 'static,
    ) -> Self {
        let open: Opener = Box::new(move || open().map(|backend| Box::new(backend) as _));
        Self {
            kind,
            accepts: B::accepts,
            state: Mutex::new(State::Closed(Some(open))),
            busy: AtomicBool::new(false),
            verdicts: Mutex::new(HashMap::new()),
            any_size: false,
        }
    }

    /// The engine's request channel, opening it on first use; why not when
    /// it cannot be used.
    fn sender(&self) -> Result<mpsc::Sender<Request>, String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let State::Closed(open) = &mut *state {
            let open = open.take().expect("opened once");
            let (opened, ready) = mpsc::sync_channel(1);
            let (sender, requests) = mpsc::channel::<Request>();
            let spawned = std::thread::Builder::new()
                .name(format!("jpeg-{}", self.kind.name()))
                .spawn(move || {
                    let mut backend = match open() {
                        Ok(backend) => {
                            let _ = opened.send(Ok(()));
                            backend
                        }
                        Err(error) => {
                            let _ = opened.send(Err(format!("{error:#}")));
                            return;
                        }
                    };
                    for request in requests {
                        let view = FrameRef::whole(&request.frame);
                        let result = backend.encode(view, request.quality);
                        let _ = request.reply.send(result);
                    }
                });
            *state = match spawned {
                Err(error) => State::Off(format!("cannot start its thread: {error}")),
                Ok(_) => match ready.recv() {
                    Ok(Ok(())) => State::Open(sender),
                    Ok(Err(reason)) => State::Off(reason),
                    Err(_) => State::Off("its thread ended while opening".into()),
                },
            };
        }
        match &*state {
            State::Open(sender) => Ok(sender.clone()),
            State::Off(reason) => Err(reason.clone()),
            State::Closed(_) => unreachable!(),
        }
    }

    /// Stop using the engine for good.
    fn turn_off(&self, reason: String) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = State::Off(reason);
    }

    /// Why the engine cannot be used, or None when it opens.
    pub(crate) fn unavailable(&self) -> Option<String> {
        self.sender().err()
    }

    /// `frame` encoded on the engine within its deadline, checked to be a
    /// whole JPEG of the frame's size; an engine that misses the deadline
    /// or breaks is turned off.
    fn engine_encode(&self, frame: &Frame, quality: i32) -> Result<Body<'static>, String> {
        let sender = self.sender()?;
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Request {
            frame: frame.clone(),
            quality,
            reply,
        };
        if sender.send(request).is_err() {
            let reason = "its thread has ended".to_owned();
            self.turn_off(reason.clone());
            return Err(reason);
        }
        let result = match answer.recv_timeout(deadline(frame)) {
            Ok(Ok(body)) if super::well_formed(&body, frame.width, frame.height) => {
                return Ok(body);
            }
            Ok(Ok(_)) => "it wrote a malformed JPEG".to_owned(),
            Ok(Err(error)) => format!("{error:#}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                format!("it took longer than {} ms", deadline(frame).as_millis())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => "its thread has ended".to_owned(),
        };
        self.turn_off(result.clone());
        Err(result)
    }

    /// Encode `frame` at `quality`: on the engine when `choice` and the
    /// frame call for it and it is accurate there, else on the CPU.
    pub(crate) fn encode(&self, frame: &Frame, quality: i32, named: bool) -> Result<Body<'static>> {
        let view = FrameRef::whole(frame);
        let takes = (self.accepts)(view, quality)
            && (named || self.any_size || view.pixels() >= MIN_PIXELS);
        if !takes {
            return cpu(frame, quality);
        }
        // One frame at a time: under `auto` another thread's frame goes to
        // the CPU meanwhile, a named engine is waited for.
        let started = Instant::now();
        while self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if !named || started.elapsed() > deadline(frame) {
                return cpu(frame, quality);
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        let result = self.encode_held(frame, quality, named);
        self.busy.store(false, Ordering::Release);
        result
    }

    fn encode_held(&self, frame: &Frame, quality: i32, named: bool) -> Result<Body<'static>> {
        let unavailable = |reason: &str| {
            Categorized::error(
                ErrorKind::Unavailable,
                format!(
                    "the {} JPEG encoder {ENV} names cannot be used: {reason}; set {ENV}=auto or cpu",
                    self.kind.name()
                ),
            )
        };
        let key = (frame.width, frame.height, frame.pixel_format);
        let verdict = self
            .verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        match verdict {
            Some(Verdict::Accurate { faster }) if faster || named => {
                match self.engine_encode(frame, quality) {
                    Ok(body) => Ok(body),
                    Err(reason) if named => Err(unavailable(&reason)),
                    Err(_) => cpu(frame, quality),
                }
            }
            Some(Verdict::Rejected(reason)) if named => Err(unavailable(&reason)),
            Some(_) => cpu(frame, quality),
            None => {
                let (verdict, body) = self.verify(frame, quality, named);
                let reason = match &verdict {
                    Verdict::Rejected(reason) => Some(reason.clone()),
                    Verdict::Accurate { .. } => None,
                };
                self.verdicts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(key, verdict);
                match (reason, body) {
                    (Some(reason), _) if named => Err(unavailable(&reason)),
                    (_, Some(body)) => Ok(body),
                    (_, None) => cpu(frame, quality),
                }
            }
        }
    }

    /// Check the engine on its first frame of a size and format: encoded on
    /// the engine and the CPU, both decoded and compared with the frame.
    /// The verdict, and the output to keep: the engine's when it is named
    /// or faster and accurate, else the CPU's.
    fn verify(&self, frame: &Frame, quality: i32, named: bool) -> (Verdict, Option<Body<'static>>) {
        // The first encode of a size may open a session; it is checked, and
        // a second one timed.
        if let Err(reason) = self.engine_encode(frame, quality) {
            return (Verdict::Rejected(reason), None);
        }
        let started = Instant::now();
        let timed = self.engine_encode(frame, quality);
        let engine_time = started.elapsed();
        let started = Instant::now();
        let reference = match cpu(frame, quality) {
            Ok(body) => body,
            Err(error) => return (Verdict::Rejected(format!("{error:#}")), None),
        };
        let cpu_time = started.elapsed();
        let engine = match timed {
            Ok(body) => body,
            Err(reason) => return (Verdict::Rejected(reason), Some(reference)),
        };
        match accurate(frame, &engine, &reference) {
            Ok(()) => {
                let faster = engine_time < cpu_time;
                let keep = if faster || named { engine } else { reference };
                (Verdict::Accurate { faster }, Some(keep))
            }
            Err(reason) => (Verdict::Rejected(reason), Some(reference)),
        }
    }

    /// Whether this engine has been checked on any frame yet, and passed.
    #[cfg(test)]
    pub(crate) fn verified(&self) -> Option<bool> {
        let verdicts = self.verdicts.lock().unwrap_or_else(PoisonError::into_inner);
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
        let (verdict, _) = self.verify(&frame, QUALITY, false);
        self.verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (frame.width, frame.height, frame.pixel_format),
                verdict.clone(),
            );
        match verdict {
            Verdict::Accurate { .. } => Ok(()),
            Verdict::Rejected(reason) => Err(reason),
        }
    }
}

/// How long an engine may take for a frame before it is given up on: 2 s
/// plus 1 ms for every 50 kB of the frame.
fn deadline(frame: &Frame) -> Duration {
    Duration::from_secs(2) + Duration::from_millis(frame.data.len() as u64 / 50_000)
}

/// Frames the hardware engines take: color frames at the default quality.
/// They write three components, so libjpeg-turbo keeps gray frames, which
/// it reads in place; and their quality follows a calibrated setting.
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
            [(x * 255 / w) as u8, (y * 255 / h) as u8, 200]
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
/// its PSNR against the frame's RGB conversion is at least 30 dB and no
/// more than 3 dB below the CPU's, and no channel is shifted by a level or
/// more on average.
#[cfg(feature = "jpeg")]
pub(crate) fn accurate(frame: &Frame, engine: &[u8], cpu: &[u8]) -> Result<(), String> {
    let source = crate::frame::rgb_samples(frame).map_err(|error| format!("{error:#}"))?;
    let decode = |jpeg: &[u8]| {
        turbojpeg::decompress(jpeg, turbojpeg::PixelFormat::RGB)
            .map_err(|error| format!("its JPEG does not decode: {error}"))
    };
    let (engine, cpu) = (decode(engine)?, decode(cpu)?);
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
        let samples = (3 * width * image.height) as f64;
        let mse = squared / samples;
        let psnr = if mse == 0.0 {
            f64::INFINITY
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        };
        (psnr, bias.map(|total| total / (samples / 3.0)))
    };
    if (engine.width, engine.height) != (frame.width as usize, frame.height as usize) {
        return Err("its JPEG has another size".into());
    }
    let (psnr, bias) = compare(&engine);
    let (cpu_psnr, _) = compare(&cpu);
    if psnr < 30.0 {
        return Err(format!(
            "its JPEG is too far from the frame ({psnr:.1} dB PSNR)"
        ));
    }
    if let Some((channel, shift)) = bias
        .iter()
        .enumerate()
        .find(|(_, shift)| shift.abs() >= 1.0)
    {
        return Err(format!(
            "its colors are shifted ({} by {shift:.2} levels)",
            ["red", "green", "blue"][channel]
        ));
    }
    if psnr < cpu_psnr - 3.0 {
        return Err(format!(
            "its JPEG is {:.1} dB worse than libjpeg-turbo's",
            cpu_psnr - psnr
        ));
    }
    Ok(())
}

/// The process's hardware engine, if this build has one for this platform.
pub(crate) fn hardware() -> Option<&'static Engine> {
    #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
    {
        static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();
        Some(
            ENGINE.get_or_init(|| {
                Engine::new(Kind::VideoToolbox, super::videotoolbox::Encoder::open)
            }),
        )
    }
    #[cfg(not(all(feature = "videotoolbox", target_os = "macos")))]
    None
}

/// Why a named engine is not in this build or on this platform.
fn missing(kind: Kind) -> String {
    match kind {
        Kind::VideoToolbox if cfg!(target_os = "macos") => {
            "this build has no VideoToolbox support (the videotoolbox feature)".into()
        }
        Kind::VideoToolbox => "VideoToolbox exists only on macOS".into(),
        _ => format!("{} JPEG encoding is not in this version", kind.name()),
    }
}

/// A still at `quality` (the default when None) by the encoder
/// `CAPTUREFAB_JPEG` asks for.
pub(crate) fn encode(frame: &Frame, quality: Option<u8>) -> Result<Body<'static>> {
    let quality = quality.map_or(QUALITY, i32::from);
    let engine = hardware();
    match choice()? {
        Choice::Cpu => cpu(frame, quality),
        Choice::Auto => match engine {
            Some(engine) => engine.encode(frame, quality, false),
            None => cpu(frame, quality),
        },
        Choice::Named(kind) => match engine.filter(|engine| engine.kind == kind) {
            Some(engine) => engine.encode(frame, quality, true),
            None => Err(Categorized::error(
                ErrorKind::Unavailable,
                format!(
                    "the {} JPEG encoder {ENV} names cannot be used: {}; set {ENV}=auto or cpu",
                    kind.name(),
                    missing(kind)
                ),
            )),
        },
    }
}

/// `doctor`'s `jpeg` object: what `CAPTUREFAB_JPEG` selects and every
/// encoder this build knows, checking a hardware engine on a test frame.
pub(crate) fn report() -> serde_json::Value {
    let selected = match choice() {
        Ok(choice) => serde_json::json!(choice.name()),
        Err(error) => serde_json::json!(format!("{error:#}")),
    };
    let mut videotoolbox = serde_json::json!({
        "compiled": cfg!(all(feature = "videotoolbox", target_os = "macos")),
        "available": false,
    });
    match hardware().filter(|engine| engine.kind == Kind::VideoToolbox) {
        Some(engine) => match engine.unavailable() {
            Some(reason) => videotoolbox["reason"] = reason.into(),
            None => {
                videotoolbox["available"] = true.into();
                match engine.self_test() {
                    Ok(()) => videotoolbox["verified"] = true.into(),
                    Err(reason) => {
                        videotoolbox["verified"] = false.into();
                        videotoolbox["reason"] = reason.into();
                    }
                }
            }
        },
        None => videotoolbox["reason"] = missing(Kind::VideoToolbox).into(),
    }
    serde_json::json!({
        "selected": selected,
        "backends": {
            "libjpeg_turbo": {"compiled": cfg!(feature = "jpeg"), "simd": cfg!(feature = "jpeg")},
            "videotoolbox": videotoolbox,
        },
    })
}

#[cfg(all(test, feature = "jpeg"))]
mod tests {
    use super::*;
    use crate::jpeg::Raster;
    use anyhow::bail;
    use std::sync::{Arc, atomic::AtomicUsize};

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
                    seen.lock().unwrap().push(view.frame.data.as_ptr() as usize);
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
                .map(|i| ((i % 96 + i / 96) * 255 / 160) as u8)
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
        assert_eq!(
            Choice::parse(Some("videotoolbox")).unwrap(),
            Choice::Named(Kind::VideoToolbox)
        );
        let error = Choice::parse(Some("gpu")).unwrap_err();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::Usage);
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
            assert_eq!(engine.verified(), Some(false));
            let (engine, _) = super::tests::engine(scenario);
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
        let started = Instant::now();
        let cpu = cpu(&frame, QUALITY).unwrap();
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        assert!(started.elapsed() < Duration::from_millis(2900));
        assert!(engine.unavailable().unwrap().contains("longer than"));
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
        let error = Choice::parse(Some("nvjpeg")).unwrap();
        assert_eq!(error, Choice::Named(Kind::Nvjpeg));
        assert!(missing(Kind::Nvjpeg).contains("not in this version"));
        assert!(missing(Kind::Vaapi).contains("not in this version"));
    }
}
