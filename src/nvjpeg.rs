//! nvJPEG GPU encoding in an isolated helper process.
//!
//! The helper (`capturefab __nvjpeg`) loads the CUDA runtime and nvJPEG at run
//! time, so builds need no CUDA SDK and hosts without NVIDIA drivers simply keep
//! libjpeg-turbo. A driver fault, or a hang inside the driver (observed on a
//! Jetson whose GPU another process was saturating, where the stuck process
//! survived SIGKILL), cannot freeze or crash the camera worker: requests have a
//! deadline, after which the helper is abandoned and the process keeps using
//! the CPU encoder.
//!
//! CUDA start-up costs far more than one CPU encode, so the helper starts in
//! the background on first use; frames are encoded on the CPU until it reports
//! ready. `CAPTUREFAB_NVJPEG=0` disables it.
#![allow(unsafe_code)]

use crate::jpeg::{QUALITY, Raster, well_formed};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

/// One image to encode. Its `width * height * channels` pixels sit at offset 0
/// of the shared buffer; the helper writes the JPEG directly after them.
#[derive(Serialize, Deserialize)]
struct Request {
    width: u32,
    height: u32,
    channels: usize,
    /// Shared buffer file, mapped by the helper whenever it changes.
    buffer: std::path::PathBuf,
    capacity: usize,
}
/// Helper reply; on success the JPEG occupies `bytes` after the pixels.
#[derive(Serialize, Deserialize, Default)]
struct Reply {
    ok: bool,
    #[serde(default)]
    bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

const MAX_LINE: u64 = 4096;
const MAX_PIXELS: usize = 64 * 1024 * 1024;
const STARTUP_DEADLINE: Duration = Duration::from_secs(30);

fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    reader.take(MAX_LINE + 1).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    ensure!(
        line.len() as u64 <= MAX_LINE && line.ends_with(b"\n"),
        "invalid nvJPEG helper message"
    );
    Ok(Some(line))
}
fn write_message(out: &mut impl Write, message: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *out, message)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Parent side: a lazily started helper with a watchdog.

enum State {
    Idle,
    Starting(mpsc::Receiver<Result<Helper, String>>),
    Ready(Helper),
    Disabled,
}
/// A helper executable and the state of its (single) helper process.
struct Accelerator {
    executable: std::path::PathBuf,
    /// Minimum per-request deadline; a healthy GPU needs milliseconds.
    deadline: Duration,
    state: Mutex<State>,
}
fn helper_executable() -> std::path::PathBuf {
    std::env::var_os("CAPTUREFAB_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| "capturefab".into())
}
static GLOBAL: std::sync::OnceLock<Accelerator> = std::sync::OnceLock::new();

/// Pixels in, JPEG out: a private mapped file shared with the helper so
/// frames never cross the pipe, which carries only one JSON line each way.
struct Shared {
    path: std::path::PathBuf,
    map: memmap2::MmapMut,
    /// The helper has mapped it, so the file name is no longer needed.
    unlinked: bool,
}
impl Shared {
    fn create(capacity: usize) -> Result<Self> {
        let dir = crate::ipc::session_dir();
        crate::ipc::ensure_private_dir(&dir)?;
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow!("random buffer name: {e}"))?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("nvjpeg-{name}.shm"));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        file.set_len(capacity as u64)?;
        // SAFETY: a new private file of fixed length, shared only with the
        // helper, which writes only the JPEG region while this side waits.
        let map = unsafe { memmap2::MmapMut::map_mut(&file) }?;
        Ok(Self {
            path,
            map,
            unlinked: false,
        })
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        if !self.unlinked {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
struct Helper {
    child: Child,
    jobs: mpsc::Sender<Arc<Raster>>,
    results: mpsc::Receiver<Result<Vec<u8>, String>>,
}
impl Helper {
    fn start(executable: &std::path::Path) -> Result<Self> {
        let mut child = Command::new(executable)
            .arg("__nvjpeg")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("launch nvJPEG helper")?;
        let mut input = child.stdin.take().context("helper stdin")?;
        let mut output = BufReader::new(child.stdout.take().context("helper stdout")?);
        let ready = (|| -> Result<()> {
            let line = read_line(&mut output)?.ok_or_else(|| anyhow!("helper exited"))?;
            let reply: Reply = serde_json::from_slice(&line)?;
            if !reply.ok {
                bail!("{}", reply.error.unwrap_or_else(|| "unavailable".into()));
            }
            Ok(())
        })();
        if let Err(error) = ready {
            abandon(child);
            return Err(error);
        }
        let (jobs, job_queue) = mpsc::channel::<Arc<Raster>>();
        let (result_sender, results) = mpsc::channel();
        // The I/O thread owns the pipes so a stalled helper blocks only it.
        thread::Builder::new()
            .name("capturefab-nvjpeg".into())
            .spawn(move || {
                let mut shared = None;
                for raster in job_queue {
                    let result = exchange(&mut input, &mut output, &mut shared, &raster);
                    if result_sender
                        .send(result.map_err(|e| format!("{e:#}")))
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            child,
            jobs,
            results,
        })
    }
}
fn exchange(
    input: &mut ChildStdin,
    output: &mut BufReader<ChildStdout>,
    shared: &mut Option<Shared>,
    raster: &Raster,
) -> Result<Vec<u8>> {
    let pixels = raster.data.len();
    // Room for the frame and a JPEG larger than it, which noise can produce.
    let capacity = pixels * 2 + (1 << 20);
    if shared.as_ref().is_none_or(|s| s.map.len() < capacity) {
        *shared = Some(Shared::create(capacity)?);
    }
    let buffer = shared.as_mut().expect("shared buffer");
    buffer.map[..pixels].copy_from_slice(&raster.data);
    let request = Request {
        width: raster.width,
        height: raster.height,
        channels: raster.channels,
        buffer: buffer.path.clone(),
        capacity: buffer.map.len(),
    };
    write_message(input, &request)?;
    let line = read_line(output)?.ok_or_else(|| anyhow!("nvJPEG helper exited"))?;
    let reply: Reply = serde_json::from_slice(&line)?;
    if !buffer.unlinked {
        // Mapped on both sides now; keeping the name only risks a stale file.
        buffer.unlinked = std::fs::remove_file(&buffer.path).is_ok();
    }
    if !reply.ok {
        bail!("{}", reply.error.unwrap_or_else(|| "encode failed".into()));
    }
    ensure!(
        reply.bytes <= buffer.map.len() - pixels,
        "nvJPEG helper reported an implausible size"
    );
    Ok(buffer.map[pixels..pixels + reply.bytes].to_vec())
}
/// Stop a helper without waiting on the caller's thread: a process stuck in
/// the GPU driver may not exit even after SIGKILL.
fn abandon(mut child: Child) {
    let _ = child.kill();
    let _ = thread::Builder::new()
        .name("capturefab-nvjpeg-reap".into())
        .spawn(move || child.wait());
}
fn disable(state: &mut State, reason: &str) {
    eprintln!("capturefab: nvJPEG disabled ({reason}); using libjpeg-turbo");
    if let State::Ready(helper) = std::mem::replace(state, State::Disabled) {
        abandon(helper.child);
    }
}
fn wanted() -> bool {
    !cfg!(target_os = "macos")
        && std::env::var_os("CAPTUREFAB_NVJPEG").is_none_or(|value| value != "0")
}

/// Encode on the GPU when the helper is ready; `None` means use the CPU.
pub fn encode(raster: &Arc<Raster>) -> Option<Vec<u8>> {
    if !wanted() {
        return None;
    }
    GLOBAL
        .get_or_init(|| Accelerator::new(helper_executable(), Duration::from_secs(2)))
        .encode(raster)
}
impl Accelerator {
    fn new(executable: std::path::PathBuf, deadline: Duration) -> Self {
        Self {
            executable,
            deadline,
            state: Mutex::new(State::Idle),
        }
    }
    fn encode(&self, raster: &Arc<Raster>) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Idle => {
                let (sender, receiver) = mpsc::channel();
                let executable = self.executable.clone();
                let started = thread::Builder::new()
                    .name("capturefab-nvjpeg-start".into())
                    .spawn(move || {
                        let _ =
                            sender.send(Helper::start(&executable).map_err(|e| format!("{e:#}")));
                    });
                *state = if started.is_ok() {
                    State::Starting(receiver)
                } else {
                    State::Disabled
                };
                return None;
            }
            State::Starting(receiver) => match receiver.try_recv() {
                Ok(Ok(helper)) => *state = State::Ready(helper),
                Ok(Err(error)) => {
                    // Unavailable hosts are the common case; say so once, quietly.
                    if !error.contains("not found") {
                        eprintln!("capturefab: nvJPEG unavailable ({error}); using libjpeg-turbo");
                    }
                    *state = State::Disabled;
                    return None;
                }
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    *state = State::Disabled;
                    return None;
                }
            },
            State::Ready(_) => {}
            State::Disabled => return None,
        }
        let State::Ready(helper) = &mut *state else {
            return None;
        };
        // Generous for multi-megapixel frames: a healthy GPU takes milliseconds.
        let deadline = self.deadline + Duration::from_millis(raster.data.len() as u64 / 50_000);
        if helper.jobs.send(raster.clone()).is_err() {
            disable(&mut state, "helper I/O stopped");
            return None;
        }
        match helper.results.recv_timeout(deadline) {
            Ok(Ok(jpeg)) if well_formed(&jpeg, raster.width, raster.height) => Some(jpeg),
            Ok(Ok(_)) => {
                disable(&mut state, "helper returned a malformed JPEG");
                None
            }
            Ok(Err(error)) => {
                disable(&mut state, &error);
                None
            }
            Err(_) => {
                disable(&mut state, "GPU encode exceeded its deadline");
                None
            }
        }
    }
}

/// Start a helper and wait for it, for `doctor`: the nvJPEG version or why
/// the GPU path is unavailable.
pub fn probe() -> Result<String> {
    if !wanted() {
        bail!("not available on this platform or disabled by CAPTUREFAB_NVJPEG=0");
    }
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ =
            sender.send(Helper::start(&helper_executable()).map(|helper| abandon(helper.child)));
    });
    match receiver.recv_timeout(STARTUP_DEADLINE) {
        Ok(Ok(())) => Ok("ready".into()),
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("helper did not start within {STARTUP_DEADLINE:?}"),
    }
}

// ---------------------------------------------------------------------------
// Helper side: CUDA + nvJPEG through runtime-loaded symbols.

type Status = i32;
type Opaque = *mut c_void;
const STREAM: Opaque = std::ptr::null_mut();
const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
const NVJPEG_CSS_420: i32 = 2;
const NVJPEG_CSS_GRAY: i32 = 6;
const NVJPEG_INPUT_RGBI: i32 = 5;
const MAJOR_VERSION: i32 = 0;
const MINOR_VERSION: i32 = 1;

/// nvjpegImage_t: up to four planes and their pitches.
#[repr(C)]
struct NvImage {
    channel: [*mut u8; 4],
    pitch: [usize; 4],
}

struct Api {
    malloc: unsafe extern "C" fn(*mut Opaque, usize) -> i32,
    free: unsafe extern "C" fn(Opaque) -> i32,
    memcpy: unsafe extern "C" fn(Opaque, *const c_void, usize, i32) -> i32,
    synchronize: unsafe extern "C" fn(Opaque) -> i32,
    get_property: unsafe extern "C" fn(i32, *mut i32) -> Status,
    create: unsafe extern "C" fn(*mut Opaque) -> Status,
    destroy: unsafe extern "C" fn(Opaque) -> Status,
    state_create: unsafe extern "C" fn(Opaque, *mut Opaque, Opaque) -> Status,
    state_destroy: unsafe extern "C" fn(Opaque) -> Status,
    params_create: unsafe extern "C" fn(Opaque, *mut Opaque, Opaque) -> Status,
    params_destroy: unsafe extern "C" fn(Opaque) -> Status,
    set_quality: unsafe extern "C" fn(Opaque, i32, Opaque) -> Status,
    set_sampling: unsafe extern "C" fn(Opaque, i32, Opaque) -> Status,
    encode_image: unsafe extern "C" fn(
        Opaque,
        Opaque,
        Opaque,
        *const NvImage,
        i32,
        i32,
        i32,
        Opaque,
    ) -> Status,
    encode_yuv: unsafe extern "C" fn(
        Opaque,
        Opaque,
        Opaque,
        *const NvImage,
        i32,
        i32,
        i32,
        Opaque,
    ) -> Status,
    retrieve: unsafe extern "C" fn(Opaque, Opaque, *mut u8, *mut usize, Opaque) -> Status,
    // Keep the libraries loaded for as long as the copied function pointers live.
    _libraries: (libloading::Library, libloading::Library),
}

/// Matching CUDA runtime and nvJPEG library names, newest major first.
fn library_pairs() -> Vec<(String, String)> {
    let mut directories = vec![String::new()];
    if let Some(dir) = std::env::var_os("CAPTUREFAB_CUDA_LIB_DIR") {
        directories.push(format!("{}/", dir.to_string_lossy()));
    }
    let mut pairs = Vec::new();
    if cfg!(windows) {
        if let Some(cuda) = std::env::var_os("CUDA_PATH") {
            let cuda = cuda.to_string_lossy().into_owned();
            directories.extend([format!("{cuda}\\bin\\x64\\"), format!("{cuda}\\bin\\")]);
        }
        for (cudart, nvjpeg) in [
            ("cudart64_13.dll", "nvjpeg64_13.dll"),
            ("cudart64_12.dll", "nvjpeg64_12.dll"),
            ("cudart64_110.dll", "nvjpeg64_11.dll"),
        ] {
            for dir in &directories {
                pairs.push((format!("{dir}{cudart}"), format!("{dir}{nvjpeg}")));
            }
        }
    } else {
        directories.extend(
            [
                "/usr/local/cuda/lib64/",
                "/usr/local/cuda/targets/sbsa-linux/lib/",
                "/usr/local/cuda/targets/aarch64-linux/lib/",
                "/usr/local/cuda/targets/x86_64-linux/lib/",
            ]
            .map(String::from),
        );
        for (cudart, nvjpeg) in [
            ("libcudart.so.13", "libnvjpeg.so.13"),
            ("libcudart.so.12", "libnvjpeg.so.12"),
            ("libcudart.so.11.0", "libnvjpeg.so.11"),
            ("libcudart.so", "libnvjpeg.so"),
        ] {
            for dir in &directories {
                pairs.push((format!("{dir}{cudart}"), format!("{dir}{nvjpeg}")));
            }
        }
    }
    pairs
}

impl Api {
    fn load() -> Result<Self> {
        let mut last = None;
        for (cudart, nvjpeg) in library_pairs() {
            // SAFETY: loading the vendor's CUDA runtime and nvJPEG runs their
            // initializers, which is their documented use.
            match unsafe {
                (
                    libloading::Library::new(&cudart),
                    libloading::Library::new(&nvjpeg),
                )
            } {
                (Ok(cudart), Ok(nvjpeg)) => return Self::bind(cudart, nvjpeg),
                (Err(e), _) | (_, Err(e)) => last = Some(e),
            }
        }
        bail!(
            "CUDA runtime and nvJPEG libraries not found{}",
            last.map(|e| format!(" ({e})")).unwrap_or_default()
        )
    }
    fn bind(cudart: libloading::Library, nvjpeg: libloading::Library) -> Result<Self> {
        // SAFETY: each signature mirrors the CUDA runtime / nvjpeg.h C prototype
        // (all handles are opaque pointers, enums are C ints, size_t is usize).
        unsafe {
            macro_rules! symbol {
                ($library:expr, $name:literal) => {
                    *$library
                        .get(concat!($name, "\0").as_bytes())
                        .with_context(|| concat!("missing symbol ", $name))?
                };
            }
            Ok(Self {
                malloc: symbol!(cudart, "cudaMalloc"),
                free: symbol!(cudart, "cudaFree"),
                memcpy: symbol!(cudart, "cudaMemcpy"),
                synchronize: symbol!(cudart, "cudaStreamSynchronize"),
                get_property: symbol!(nvjpeg, "nvjpegGetProperty"),
                create: symbol!(nvjpeg, "nvjpegCreateSimple"),
                destroy: symbol!(nvjpeg, "nvjpegDestroy"),
                state_create: symbol!(nvjpeg, "nvjpegEncoderStateCreate"),
                state_destroy: symbol!(nvjpeg, "nvjpegEncoderStateDestroy"),
                params_create: symbol!(nvjpeg, "nvjpegEncoderParamsCreate"),
                params_destroy: symbol!(nvjpeg, "nvjpegEncoderParamsDestroy"),
                set_quality: symbol!(nvjpeg, "nvjpegEncoderParamsSetQuality"),
                set_sampling: symbol!(nvjpeg, "nvjpegEncoderParamsSetSamplingFactors"),
                encode_image: symbol!(nvjpeg, "nvjpegEncodeImage"),
                encode_yuv: symbol!(nvjpeg, "nvjpegEncodeYUV"),
                retrieve: symbol!(nvjpeg, "nvjpegEncodeRetrieveBitstream"),
                _libraries: (cudart, nvjpeg),
            })
        }
    }
}

fn status(what: &str, code: i32) -> Result<()> {
    ensure!(code == 0, "{what} failed with status {code}");
    Ok(())
}

struct Encoder {
    api: Api,
    handle: Opaque,
    state: Opaque,
    params: Opaque,
    device: Opaque,
    capacity: usize,
}
impl Encoder {
    fn new() -> Result<(Self, String)> {
        let api = Api::load()?;
        let mut encoder = Self {
            api,
            handle: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            params: std::ptr::null_mut(),
            device: std::ptr::null_mut(),
            capacity: 0,
        };
        // SAFETY: out-pointers are valid locals; created objects are owned by
        // `encoder` and released in Drop even if a later step fails.
        let version = unsafe {
            let api = &encoder.api;
            let (mut major, mut minor) = (0, 0);
            (api.get_property)(MAJOR_VERSION, &mut major);
            (api.get_property)(MINOR_VERSION, &mut minor);
            status("nvjpegCreateSimple", (api.create)(&mut encoder.handle))?;
            status(
                "nvjpegEncoderStateCreate",
                (api.state_create)(encoder.handle, &mut encoder.state, STREAM),
            )?;
            status(
                "nvjpegEncoderParamsCreate",
                (api.params_create)(encoder.handle, &mut encoder.params, STREAM),
            )?;
            status(
                "nvjpegEncoderParamsSetQuality",
                (api.set_quality)(encoder.params, QUALITY, STREAM),
            )?;
            format!("{major}.{minor}")
        };
        Ok((encoder, version))
    }
    /// Encode `pixels` and write the JPEG into `out`, returning its length.
    fn encode(&mut self, request: &Request, pixels: &[u8], out: &mut [u8]) -> Result<usize> {
        let api = &self.api;
        let (width, height) = (request.width as i32, request.height as i32);
        // SAFETY: `device` holds at least `pixels.len()` bytes after the
        // (re)allocation below; the image descriptor points only into it; all
        // calls use the legacy default stream, synchronized before retrieval.
        unsafe {
            if pixels.len() > self.capacity {
                if !self.device.is_null() {
                    (api.free)(self.device);
                    self.device = std::ptr::null_mut();
                    self.capacity = 0;
                }
                status("cudaMalloc", (api.malloc)(&mut self.device, pixels.len()))?;
                self.capacity = pixels.len();
            }
            status(
                "cudaMemcpy",
                (api.memcpy)(
                    self.device,
                    pixels.as_ptr().cast(),
                    pixels.len(),
                    CUDA_MEMCPY_HOST_TO_DEVICE,
                ),
            )?;
            let mut image = NvImage {
                channel: [std::ptr::null_mut(); 4],
                pitch: [0; 4],
            };
            image.channel[0] = self.device.cast();
            image.pitch[0] = request.width as usize * request.channels;
            if request.channels == 1 {
                status(
                    "nvjpegEncoderParamsSetSamplingFactors",
                    (api.set_sampling)(self.params, NVJPEG_CSS_GRAY, STREAM),
                )?;
                status(
                    "nvjpegEncodeYUV",
                    (api.encode_yuv)(
                        self.handle,
                        self.state,
                        self.params,
                        &image,
                        NVJPEG_CSS_GRAY,
                        width,
                        height,
                        STREAM,
                    ),
                )?;
            } else {
                status(
                    "nvjpegEncoderParamsSetSamplingFactors",
                    (api.set_sampling)(self.params, NVJPEG_CSS_420, STREAM),
                )?;
                status(
                    "nvjpegEncodeImage",
                    (api.encode_image)(
                        self.handle,
                        self.state,
                        self.params,
                        &image,
                        NVJPEG_INPUT_RGBI,
                        width,
                        height,
                        STREAM,
                    ),
                )?;
            }
            status("cudaStreamSynchronize", (api.synchronize)(STREAM))?;
            let mut length = 0usize;
            status(
                "nvjpegEncodeRetrieveBitstream",
                (api.retrieve)(
                    self.handle,
                    self.state,
                    std::ptr::null_mut(),
                    &mut length,
                    STREAM,
                ),
            )?;
            ensure!(length <= out.len(), "JPEG exceeds the shared buffer");
            status(
                "nvjpegEncodeRetrieveBitstream",
                (api.retrieve)(
                    self.handle,
                    self.state,
                    out.as_mut_ptr(),
                    &mut length,
                    STREAM,
                ),
            )?;
            status("cudaStreamSynchronize", (api.synchronize)(STREAM))?;
            Ok(length)
        }
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        let api = &self.api;
        // SAFETY: each object was created by this encoder and is freed once.
        unsafe {
            if !self.device.is_null() {
                (api.free)(self.device);
            }
            if !self.params.is_null() {
                (api.params_destroy)(self.params);
            }
            if !self.state.is_null() {
                (api.state_destroy)(self.state);
            }
            if !self.handle.is_null() {
                (api.destroy)(self.handle);
            }
        }
    }
}

/// Hidden `__nvjpeg` mode: encode requests from stdin until it closes.
pub fn run_helper() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut encoder = match Encoder::new() {
        Ok((encoder, version)) => {
            write_message(
                &mut output,
                &Reply {
                    ok: true,
                    version: Some(version),
                    ..Reply::default()
                },
            )?;
            encoder
        }
        Err(error) => {
            return write_message(
                &mut output,
                &Reply {
                    error: Some(format!("{error:#}")),
                    ..Reply::default()
                },
            );
        }
    };
    let mut mapping: Option<(std::path::PathBuf, memmap2::MmapMut)> = None;
    while let Some(line) = read_line(&mut input)? {
        let request: Request = serde_json::from_slice(&line)?;
        let size = (request.width as usize)
            .checked_mul(request.height as usize)
            .filter(|&n| {
                n > 0 && n <= MAX_PIXELS && request.width <= 65_535 && request.height <= 65_535
            })
            .and_then(|n| n.checked_mul(request.channels))
            .filter(|_| matches!(request.channels, 1 | 3))
            .filter(|&n| n < request.capacity)
            .context("invalid nvJPEG request")?;
        if mapping
            .as_ref()
            .is_none_or(|(path, map)| *path != request.buffer || map.len() != request.capacity)
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&request.buffer)?;
            ensure!(
                file.metadata()?.len() == request.capacity as u64,
                "shared buffer size mismatch"
            );
            // SAFETY: the parent created this fixed-size private file and does
            // not touch it while a request is outstanding.
            let map = unsafe { memmap2::MmapMut::map_mut(&file) }?;
            mapping = Some((request.buffer.clone(), map));
        }
        let (_, map) = mapping.as_mut().expect("mapped buffer");
        let (pixels, out) = map.split_at_mut(size);
        let started = Instant::now();
        let reply = match encoder.encode(&request, pixels, out) {
            Ok(bytes) => Reply {
                ok: true,
                bytes,
                ..Reply::default()
            },
            Err(error) => Reply {
                error: Some(format!("{error:#} after {:?}", started.elapsed())),
                ..Reply::default()
            },
        };
        write_message(&mut output, &reply)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    /// A fake helper: a shell script standing in for `capturefab __nvjpeg`.
    fn script(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "capturefab-nvjpeg-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn raster() -> Arc<Raster> {
        Arc::new(Raster {
            width: 8,
            height: 8,
            channels: 1,
            data: vec![0; 64],
        })
    }
    /// Drive the accelerator until it leaves the start-up phase.
    fn settle(accelerator: &Accelerator) -> Option<Vec<u8>> {
        let start = Instant::now();
        loop {
            let result = accelerator.encode(&raster());
            let state = accelerator.state.lock().unwrap();
            if !matches!(*state, State::Idle | State::Starting(_))
                || start.elapsed() > Duration::from_secs(10)
            {
                return result;
            }
            drop(state);
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn hung_helper_is_abandoned_at_the_deadline() {
        // Reports ready, then never answers: the driver-hang case.
        let path = script("hang", "echo '{\"ok\":true}'; exec sleep 30");
        let accelerator = Accelerator::new(path.clone(), Duration::from_millis(200));
        let started = Instant::now();
        assert!(settle(&accelerator).is_none());
        assert!(matches!(
            *accelerator.state.lock().unwrap(),
            State::Disabled
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // Once disabled, requests return immediately for the CPU encoder.
        let again = Instant::now();
        assert!(accelerator.encode(&raster()).is_none());
        assert!(again.elapsed() < Duration::from_millis(50));
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn unavailable_or_misbehaving_helpers_fall_back() {
        for (name, body) in [
            (
                "refuses",
                "echo '{\"ok\":false,\"error\":\"no CUDA device\"}'",
            ),
            ("exits", "exit 3"),
            (
                "garbage",
                "echo '{\"ok\":true}'; read line; echo '{\"ok\":true,\"bytes\":4}'; sleep 1",
            ),
        ] {
            let path = script(name, body);
            let accelerator = Accelerator::new(path.clone(), Duration::from_millis(500));
            assert!(settle(&accelerator).is_none(), "{name}");
            assert!(
                matches!(*accelerator.state.lock().unwrap(), State::Disabled),
                "{name}"
            );
            let _ = std::fs::remove_file(path);
        }
    }
}
