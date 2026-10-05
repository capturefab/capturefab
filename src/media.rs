//! FFmpeg-backed media inputs and bounded frame forwarding. The optional FFmpeg
//! executable is embedded at build time and extracted into a private directory.
//! No shell is involved in launching decoders or encoders.
use crate::types::{Backend, CameraInfo, Frame, RGB8, RegisterIo, Transport};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cmp::Reverse,
    collections::{HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const EMBEDDED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/capturefab-ffmpeg.bin"));
const LICENSE: &str = include_str!(concat!(env!("OUT_DIR"), "/capturefab-ffmpeg-license.txt"));
const MAX_FRAME: usize = 256 * 1024 * 1024;
const MAX_LOG: usize = 16 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
static HWACCEL: OnceLock<String> = OnceLock::new();
pub fn configure_hwaccel(value: &str) -> Result<()> {
    ensure!(
        [
            "auto",
            "none",
            "off",
            "videotoolbox",
            "cuda",
            "qsv",
            "vaapi",
            "d3d11va",
            "dxva2",
            "vulkan",
            "drm",
            "opencl"
        ]
        .contains(&value),
        "unsupported hardware acceleration method"
    );
    let _ = HWACCEL.set(value.to_owned());
    Ok(())
}
pub fn hwaccel() -> String {
    HWACCEL
        .get()
        .cloned()
        .or_else(|| std::env::var("CAPTUREFAB_HWACCEL").ok())
        .unwrap_or_else(|| "auto".into())
}

pub fn redact_url(url: &str) -> String {
    let mut result = url.to_string();
    if let Some(scheme_end) = result.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = result[authority_start..]
            .find(['/', '?', '#'])
            .map(|n| authority_start + n)
            .unwrap_or(result.len());
        if let Some(at) = result[authority_start..authority_end].rfind('@') {
            result.replace_range(authority_start..authority_start + at, "<redacted>");
        }
    }
    if let Some(query) = result.find('?') {
        let path = result[..query].to_string();
        let params = result[query + 1..]
            .split('&')
            .map(|item| {
                let (key, value) = item.split_once('=').unwrap_or((item, ""));
                if matches!(
                    key.to_ascii_lowercase().as_str(),
                    "password"
                        | "passphrase"
                        | "passwd"
                        | "token"
                        | "key"
                        | "auth"
                        | "secret"
                        | "access_token"
                        | "streamid"
                ) {
                    format!("{key}=<redacted>")
                } else if value.is_empty() {
                    key.to_string()
                } else {
                    format!("{key}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        result = format!("{path}?{params}");
    }
    result
}
fn redact_log(message: &str, source: &str) -> String {
    message.replace(source, &redact_url(source))
}
fn scheme(url: &str) -> Option<String> {
    if url.len() >= 3 && url.as_bytes()[1] == b':' && matches!(url.as_bytes()[2], b'\\' | b'/') {
        return None;
    }
    let (name, _) = url.split_once(':')?;
    if !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        Some(name.to_ascii_lowercase())
    } else {
        None
    }
}
fn validate_source(url: &str) -> Result<()> {
    ensure!(
        !url.is_empty()
            && url.len() <= 16384
            && !url.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n'),
        "invalid media source URL"
    );
    if let Some(protocol) = scheme(url) {
        ensure!(
            matches!(
                protocol.as_str(),
                "rtsp"
                    | "rtsps"
                    | "srt"
                    | "rtmp"
                    | "rtmps"
                    | "http"
                    | "https"
                    | "udp"
                    | "tcp"
                    | "rtp"
                    | "file"
                    | "avfoundation"
                    | "v4l2"
                    | "dshow"
            ),
            "unsupported media protocol {protocol}"
        );
    } else {
        ensure!(
            std::path::Path::new(url).is_file(),
            "media file does not exist"
        );
    }
    Ok(())
}
pub fn is_source(url: &str) -> bool {
    scheme(url).is_some_and(|s| {
        matches!(
            s.as_str(),
            "rtsp"
                | "rtsps"
                | "srt"
                | "rtmp"
                | "rtmps"
                | "http"
                | "https"
                | "udp"
                | "tcp"
                | "rtp"
                | "file"
                | "avfoundation"
                | "v4l2"
                | "dshow"
        )
    }) || std::path::Path::new(url).is_file()
}
pub fn info(url: &str) -> Result<CameraInfo> {
    validate_source(url)?;
    let safe_url = redact_url(url);
    let hash = safe_url.bytes().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    Ok(CameraInfo {
        id: format!("media:{hash:016x}"),
        transport: Transport::Media,
        vendor: "Media".into(),
        model: format!(
            "{} input",
            scheme(url).unwrap_or_else(|| "file".into()).to_uppercase()
        ),
        serial: format!("{hash:016x}"),
        address: Some(safe_url),
    })
}
struct Executable {
    path: PathBuf,
    directory: Option<PathBuf>,
}
impl Drop for Executable {
    fn drop(&mut self) {
        if let Some(dir) = &self.directory {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(dir);
        }
    }
}
static EXECUTABLE: OnceLock<Mutex<Weak<Executable>>> = OnceLock::new();
fn executable() -> Result<Arc<Executable>> {
    if let Some(path) = std::env::var_os("CAPTUREFAB_FFMPEG") {
        ensure!(
            !path.is_empty(),
            "CAPTUREFAB_FFMPEG must be an executable path"
        );
        return Ok(Arc::new(Executable {
            path: PathBuf::from(path),
            directory: None,
        }));
    }
    if let Some(path) = option_env!("CAPTUREFAB_LOCAL_FFMPEG")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Ok(Arc::new(Executable {
            path,
            directory: None,
        }));
    }
    ensure!(
        !EMBEDDED.is_empty(),
        "this development build has no embedded FFmpeg; rebuild with CAPTUREFAB_FFMPEG_BINARY pointing to a target-compatible static FFmpeg, or set CAPTUREFAB_FFMPEG to an executable path"
    );
    let mut cached = EXECUTABLE
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = cached.upgrade() {
        return Ok(existing);
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|e| anyhow!("cannot generate FFmpeg extraction nonce: {e}"))?;
    let suffix = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let directory = std::env::temp_dir().join(format!("capturefab-ffmpeg-{suffix}"));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&directory)
        .context("cannot create private FFmpeg directory")?;
    let path = directory.join(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    });
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        let mut file = options.open(&path)?;
        file.write_all(EMBEDDED)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&directory);
        return Err(error.context("cannot extract bundled FFmpeg"));
    }
    let executable = Arc::new(Executable {
        path,
        directory: Some(directory),
    });
    *cached = Arc::downgrade(&executable);
    Ok(executable)
}
fn command(exe: &Executable) -> Command {
    let mut cmd = Command::new(&exe.path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.stdin(Stdio::null());
    cmd
}
fn bounded_output(exe: &Executable, args: &[String], timeout: Duration) -> Result<(bool, String)> {
    let mut child = command(exe)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot launch FFmpeg")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let reader = |mut input: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = input.by_ref().take(1024 * 1024).read_to_end(&mut bytes);
            bytes
        })
    };
    let out = reader(Box::new(stdout));
    let err = reader(Box::new(stderr));
    let deadline = Instant::now() + timeout;
    let success = loop {
        if let Some(status) = child.try_wait()? {
            break status.success();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut bytes = out.join().unwrap_or_default();
    bytes.extend(err.join().unwrap_or_default());
    Ok((success, String::from_utf8_lossy(&bytes).to_string()))
}
pub fn ffmpeg_info() -> Result<Value> {
    let exe = executable()?;
    let (ok, version) = bounded_output(&exe, &["-version".into()], Duration::from_secs(5))?;
    ensure!(ok, "FFmpeg version query failed");
    let (_, protocols) = bounded_output(
        &exe,
        &["-hide_banner".into(), "-protocols".into()],
        Duration::from_secs(5),
    )?;
    let (_, encoders) = bounded_output(
        &exe,
        &["-hide_banner".into(), "-encoders".into()],
        Duration::from_secs(5),
    )?;
    Ok(
        json!({"embedded":!EMBEDDED.is_empty(),"embedded_bytes":EMBEDDED.len(),"version":version,"protocols":protocols,"encoders":encoders,"license_notice":LICENSE}),
    )
}
pub fn native_devices() -> Result<Value> {
    native_devices_with_timeout(Duration::from_secs(8))
}
pub fn native_devices_with_timeout(timeout: Duration) -> Result<Value> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = timeout;
    #[cfg(target_os = "linux")]
    {
        let mut devices = Vec::new();
        if let Ok(entries) = fs::read_dir("/sys/class/video4linux") {
            for entry in entries.flatten() {
                let node = entry.file_name().to_string_lossy().to_string();
                let name = fs::read_to_string(entry.path().join("name"))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                devices
                    .push(json!({"id":format!("v4l2:/dev/{node}"),"name":name,"backend":"v4l2"}));
            }
        }
        return Ok(json!({"platform":"linux","devices":devices}));
    }
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let exe = executable()?;
        let backend = if cfg!(target_os = "macos") {
            "avfoundation"
        } else {
            "dshow"
        };
        let args = [
            "-hide_banner",
            "-f",
            backend,
            "-list_devices",
            "true",
            "-i",
            if cfg!(target_os = "macos") {
                ""
            } else {
                "dummy"
            },
        ]
        .map(str::to_string);
        let (_, listing) = bounded_output(&exe, &args, timeout)?;
        let devices = if backend == "avfoundation" {
            avfoundation_devices(&listing)
        } else {
            listing
                .lines()
                .filter(|line| line.contains("(video)"))
                .filter_map(|line| {
                    let (_, remaining) = line.split_once('"')?;
                    let (name, _) = remaining.split_once('"')?;
                    Some(json!({"id":format!("dshow:video={name}"),"name":name,"backend":backend}))
                })
                .collect()
        };
        return Ok(json!({"platform":std::env::consts::OS,"devices":devices,"diagnostic":listing}));
    }
    #[allow(unreachable_code)]
    Err(anyhow!(
        "native camera enumeration is supported on macOS, Linux and Windows"
    ))
}
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn avfoundation_devices(listing: &str) -> Vec<Value> {
    let mut video = true;
    let mut found = Vec::new();
    for line in listing.lines() {
        if line.contains("audio devices:") {
            video = false;
        } else if line.contains("video devices:") {
            video = true;
        } else if video
            && let Some(device) = line.match_indices('[').find_map(|(start, _)| {
                let (index, name) = line[start + 1..].split_once("] ")?;
                index
                    .parse::<u32>()
                    .is_ok()
                    .then_some((index, name.trim_end()))
            })
        {
            found.push(device);
        }
    }
    let selectable = |name: &str| {
        !name.is_empty()
            && !name.contains([':', '?'])
            && !name.starts_with(|c: char| {
                c.is_ascii_digit() || c.is_whitespace() || matches!(c, '+' | '-')
            })
            && !["none", "default"]
                .iter()
                .any(|word| name.starts_with(word))
            && found
                .iter()
                .filter(|(_, other)| other.starts_with(name))
                .count()
                == 1
    };
    found
        .iter()
        .map(|&(index, name)| {
            let id = if selectable(name) { name } else { index };
            json!({"id":format!("avfoundation:{id}"),"name":name,"backend":"avfoundation"})
        })
        .collect()
}

struct Log {
    bytes: Mutex<VecDeque<u8>>,
}
impl Log {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(VecDeque::new()),
        })
    }
    fn text(&self, source: &str) -> String {
        let bytes = self
            .bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect::<Vec<_>>();
        redact_log(&String::from_utf8_lossy(&bytes), source)
    }
    fn collect(self: &Arc<Self>, mut input: impl Read + Send + 'static) -> JoinHandle<()> {
        let log = self.clone();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut stored = log.bytes.lock().unwrap_or_else(|e| e.into_inner());
                stored.extend(&buffer[..count]);
                while stored.len() > MAX_LOG {
                    stored.pop_front();
                }
            }
        })
    }
}
struct Decoded {
    frames: VecDeque<Frame>,
    error: Option<String>,
}
struct DecoderQueue {
    state: Mutex<Decoded>,
    changed: Condvar,
    stopped: AtomicBool,
}
struct Decoder {
    child: Child,
    thread: Option<JoinHandle<()>>,
    log_thread: Option<JoinHandle<()>>,
    queue: Arc<DecoderQueue>,
    log: Arc<Log>,
    _exe: Arc<Executable>,
    source: String,
}
fn ppm_token(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut token = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        ensure!(!available.is_empty(), "media stream ended");
        let byte = available[0];
        reader.consume(1);
        if token.is_empty() && byte == b'#' {
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line)?;
            ensure!(line.len() <= 4096, "PPM comment is too long");
            continue;
        }
        if byte.is_ascii_whitespace() {
            if token.is_empty() { continue } else { break }
        }
        token.push(byte);
        ensure!(token.len() <= 32, "PPM header token is too long");
    }
    Ok(token)
}
fn read_ppm(reader: &mut impl BufRead, id: u64, timestamp_ns: u64) -> Result<Frame> {
    ensure!(
        ppm_token(reader)? == b"P6",
        "FFmpeg emitted an unsupported image format"
    );
    let integer = |bytes: Vec<u8>| -> Result<u32> {
        std::str::from_utf8(&bytes)?
            .parse()
            .context("invalid PPM dimensions")
    };
    let width = integer(ppm_token(reader)?)?;
    let height = integer(ppm_token(reader)?)?;
    ensure!(
        integer(ppm_token(reader)?)? == 255,
        "unsupported PPM sample depth"
    );
    let size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .context("media frame dimension overflow")?;
    ensure!(
        width > 0 && height > 0 && size <= MAX_FRAME,
        "media frame exceeds 256 MiB limit"
    );
    let mut data = vec![0; size];
    reader
        .read_exact(&mut data)
        .context("truncated FFmpeg RGB frame")?;
    Ok(Frame {
        id,
        width,
        height,
        pixel_format: RGB8,
        timestamp_ns,
        data,
    })
}
impl Decoder {
    #[cfg(test)]
    fn spawn(source: &str, timeout: Duration) -> Result<Self> {
        Self::spawn_configured(source, timeout, &[])
    }
    fn spawn_configured(source: &str, timeout: Duration, options: &[String]) -> Result<Self> {
        validate_source(source)?;
        let exe = executable()?;
        let mut cmd = command(&exe);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error"]);
        let acceleration = hwaccel();
        if !matches!(acceleration.as_str(), "none" | "off") {
            cmd.args(["-hwaccel", &acceleration]);
        }
        let input = match native_input(source)? {
            Some((format, input)) => {
                cmd.args(["-f", format]);
                input
            }
            None => source.into(),
        };
        cmd.args(options);
        if matches!(scheme(source).as_deref(), Some("rtsp" | "rtsps")) {
            cmd.args([
                "-rtsp_transport",
                "tcp",
                "-timeout",
                &timeout.as_micros().max(1).to_string(),
            ]);
        } else if scheme(source)
            .is_some_and(|s| !matches!(s.as_str(), "file" | "avfoundation" | "v4l2" | "dshow"))
        {
            cmd.args(["-rw_timeout", &timeout.as_micros().max(1).to_string()]);
        }
        cmd.args([
            "-i",
            &input,
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-fps_mode",
            "passthrough",
            "-f",
            "image2pipe",
            "-c:v",
            "ppm",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("cannot start FFmpeg decoder")?;
        let stdout = child.stdout.take().unwrap();
        let log = Log::new();
        let log_thread = log.collect(child.stderr.take().unwrap());
        let queue = Arc::new(DecoderQueue {
            state: Mutex::new(Decoded {
                frames: VecDeque::new(),
                error: None,
            }),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let shared = queue.clone();
        let epoch = Instant::now();
        let live = scheme(source).is_some_and(|s| s != "file");
        let thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut id = 0;
            while !shared.stopped.load(Ordering::Relaxed) {
                id += 1;
                let result = read_ppm(
                    &mut reader,
                    id,
                    epoch.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                );
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(frame) => {
                        if live {
                            while state.frames.len() >= 2 {
                                state.frames.pop_front();
                            }
                        } else {
                            while state.frames.len() >= 2 && !shared.stopped.load(Ordering::Relaxed)
                            {
                                state = shared
                                    .changed
                                    .wait(state)
                                    .unwrap_or_else(|e| e.into_inner());
                            }
                        }
                        if shared.stopped.load(Ordering::Relaxed) {
                            break;
                        }
                        state.frames.push_back(frame);
                    }
                    Err(e) => {
                        state.error = Some(format!("{e:#}"));
                        shared.changed.notify_all();
                        break;
                    }
                }
                shared.changed.notify_all();
            }
        });
        Ok(Self {
            child,
            thread: Some(thread),
            log_thread: Some(log_thread),
            queue,
            log,
            _exe: exe,
            source: source.into(),
        })
    }
    fn next(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let mut state = self.queue.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(frame) = state.frames.pop_front() {
                self.queue.changed.notify_all();
                return Ok(frame);
            }
            if let Some(error) = &state.error {
                bail!(
                    "{}: {}",
                    redact_log(error, &self.source),
                    self.log.text(&self.source).trim()
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "media frame capture timed out: {}",
                redact_url(&self.source)
            );
            state = self
                .queue
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn stop(&mut self) {
        self.queue.stopped.store(true, Ordering::Relaxed);
        self.queue.changed.notify_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.log_thread.take() {
            let _ = thread.join();
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}
fn native_input(source: &str) -> Result<Option<(&'static str, String)>> {
    Ok(Some(
        if let Some(device) = source.strip_prefix("avfoundation:") {
            ensure!(
                cfg!(target_os = "macos"),
                "AVFoundation capture is available on macOS"
            );
            (
                "avfoundation",
                format!("{}:none", device.split(':').next().unwrap_or(device)),
            )
        } else if let Some(path) = source.strip_prefix("v4l2:") {
            ensure!(
                cfg!(target_os = "linux"),
                "V4L2 capture is available on Linux"
            );
            ensure!(
                path.starts_with("/dev/video"),
                "V4L2 URI must point to /dev/videoN"
            );
            ("v4l2", path.into())
        } else if let Some(name) = source.strip_prefix("dshow:") {
            ensure!(
                cfg!(target_os = "windows"),
                "DirectShow capture is available on Windows"
            );
            ensure!(
                name.starts_with("video="),
                "DirectShow URI must begin dshow:video="
            );
            ("dshow", name.into())
        } else {
            return Ok(None);
        },
    ))
}
fn device_name(source: &str) -> &str {
    let name = source.split_once(':').map_or(source, |(_, name)| name);
    let name = name.strip_prefix("video=").unwrap_or(name);
    if name.parse::<u32>().is_ok() {
        source
    } else {
        name
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mode {
    width: u32,
    height: u32,
    fps: Option<(f64, f64)>,
}
impl Mode {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn honors(&self, fps: f64) -> bool {
        self.fps
            .is_none_or(|(low, high)| fps > low - 0.01 && fps < high + 0.01)
    }
    fn label(&self) -> String {
        match self.fps {
            Some((_, high)) => format!("{}x{}@{high}", self.width, self.height),
            None => format!("{}x{}", self.width, self.height),
        }
    }
    fn rank(&self) -> (bool, u64, u64, u64, u32) {
        let fps = self
            .fps
            .map_or(0, |(_, high)| (high * 100.0).round() as u64);
        let area = self.width as u64 * self.height as u64;
        let smooth = fps >= 2500;
        (smooth, if smooth { area } else { 0 }, fps, area, self.width)
    }
}
fn best(modes: &[Mode], keep: impl Fn(&Mode) -> bool) -> Option<usize> {
    modes
        .iter()
        .enumerate()
        .filter(|(_, mode)| keep(mode))
        .max_by_key(|(_, mode)| mode.rank())
        .map(|(index, _)| index)
}
fn rounded(fps: f64) -> f64 {
    (fps * 100.0).round() / 100.0
}
fn dimensions(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    let size = (width.parse().ok()?, height.parse().ok()?);
    ((16..=8192).contains(&size.0) && (16..=8192).contains(&size.1)).then_some(size)
}
fn rate(text: &str) -> Option<f64> {
    let fps = rounded(text.parse().ok()?);
    (fps.is_finite() && fps > 0.0).then_some(fps)
}
fn merge(mut modes: Vec<Mode>, mode: Mode) -> Vec<Mode> {
    match modes.iter_mut().find(|m| m.label() == mode.label()) {
        Some(m) => {
            m.fps = m
                .fps
                .zip(mode.fps)
                .map(|((a, high), (b, _))| (a.min(b), high))
        }
        None => modes.push(mode),
    }
    modes
}
fn avfoundation_modes(log: &str) -> Vec<Mode> {
    let mut modes: Vec<Mode> = Vec::new();
    for mode in log.lines().filter_map(|line| {
        let (head, tail) = line.trim_end().split_once("@[")?;
        let (width, height) = dimensions(head.rsplit(' ').next()?)?;
        let high = rate(tail.strip_suffix("]fps")?.split_once(' ')?.1)?;
        Some(Mode {
            width,
            height,
            fps: Some((high, high)),
        })
    }) {
        if modes
            .last()
            .is_none_or(|last| last.size() != mode.size() || last.fps <= mode.fps)
        {
            modes.retain(|m| m.size() != mode.size());
        }
        modes.push(mode);
    }
    modes
}
fn dshow_modes(log: &str) -> Vec<Mode> {
    log.lines()
        .filter_map(|line| {
            let (min, max) = line.split_once(" min s=")?.1.split_once(" max s=")?;
            let caps = |text: &str| {
                let (size, fps) = text.split_once(" fps=")?;
                Some((dimensions(size), rate(fps.split([' ', '(']).next()?)?))
            };
            let ((_, low), (size, high)) = (caps(min)?, caps(max)?);
            let (width, height) = size?;
            Some(Mode {
                width,
                height,
                fps: Some((low.min(high), high)),
            })
        })
        .fold(Vec::new(), merge)
}
fn v4l2_modes(log: &str) -> (Vec<Mode>, Option<String>) {
    let mut formats = log
        .lines()
        .filter_map(|line| {
            let (kind, fields) = line.split_once("] ")?.1.split_once(':')?;
            let compressed = match kind.trim_end() {
                "Raw" => false,
                "Compressed" => true,
                _ => return None,
            };
            let mut fields = fields.splitn(3, " : ");
            let name = fields.next()?.trim();
            let modes = fields
                .nth(1)?
                .split_whitespace()
                .filter_map(dimensions)
                .map(|(width, height)| Mode {
                    width,
                    height,
                    fps: None,
                })
                .fold(Vec::new(), merge);
            (name != "Unsupported" && !modes.is_empty()).then_some((compressed, name, modes))
        })
        .collect::<Vec<_>>();
    formats.sort_by_key(|format| format.0);
    formats
        .into_iter()
        .next()
        .map(|(_, name, modes)| (modes, Some(name.into())))
        .unwrap_or_default()
}
fn probe(source: &str) -> (Vec<Mode>, Option<String>) {
    let Ok(Some((format, input))) = native_input(source) else {
        return Default::default();
    };
    let options = match format {
        "avfoundation" => ["-framerate", "1234", "-video_size", "16x16"].as_slice(),
        "v4l2" => &["-list_formats", "all"],
        _ => &["-list_options", "true"],
    };
    let args = ["-hide_banner", "-f", format]
        .iter()
        .chain(options)
        .map(|arg| arg.to_string())
        .chain(["-i".into(), input])
        .collect::<Vec<_>>();
    let Ok((_, log)) = executable().and_then(|exe| bounded_output(&exe, &args, PROBE_TIMEOUT))
    else {
        return Default::default();
    };
    match format {
        "avfoundation" => (avfoundation_modes(&log), None),
        "v4l2" => v4l2_modes(&log),
        _ => (dshow_modes(&log), None),
    }
}
fn native_failure(log: &str, stalled: bool, name: &str, timeout: Duration) -> Option<String> {
    if log.contains("Cannot use ") {
        Some(
            "camera access denied: allow this app in System Settings > Privacy & Security > Camera"
                .into(),
        )
    } else if ["Cannot Use ", "Device or resource busy", "already in use"]
        .iter()
        .any(|text| log.contains(text))
    {
        Some("camera is in use by another application".into())
    } else {
        stalled.then(|| format!("no frames from {name} (timed out after {} s): the camera may be suspended (for example a closed MacBook lid), in use, or blocked by permissions", timeout.as_secs_f64()))
    }
}
#[derive(Clone, Copy, Default)]
struct NativeOptions {
    size: Option<(u32, u32)>,
    fps: Option<f64>,
}
fn native_source(source: &str) -> Result<(String, Option<NativeOptions>)> {
    if !matches!(
        scheme(source).as_deref(),
        Some("avfoundation" | "v4l2" | "dshow")
    ) {
        return Ok((source.into(), None));
    }
    let (base, query) = source.split_once('?').unwrap_or((source, ""));
    let mut options = NativeOptions::default();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .context("native camera options need key=value")?;
        match key {
            "fps" => {
                let fps: f64 = value
                    .parse()
                    .context("native camera fps must be a number")?;
                ensure!(
                    fps.is_finite() && (0.1..=240.0).contains(&fps),
                    "native camera fps must be 0.1..240"
                );
                options.fps = Some(fps);
            }
            "size" => {
                let (w, h) = value
                    .split_once('x')
                    .context("native camera size must be WIDTHxHEIGHT")?;
                let w = w.parse::<u32>()?;
                let h = h.parse::<u32>()?;
                ensure!(
                    (16..=8192).contains(&w)
                        && (16..=8192).contains(&h)
                        && w as usize * h as usize <= MAX_FRAME / 3,
                    "native camera size is outside 16..8192 or frame memory bound"
                );
                options.size = Some((w, h));
            }
            _ => bail!("unsupported native camera option {key}; use size=1280x720 and fps=30"),
        }
    }
    Ok((base.into(), Some(options)))
}
struct MediaBackend {
    source: String,
    timeout: Duration,
    decoder: Option<Decoder>,
    first: Option<Frame>,
    width: u32,
    height: u32,
    native: bool,
    requested_size: bool,
    fps: f64,
    running: bool,
    modes: Vec<Mode>,
    mode: Option<usize>,
    format: Option<String>,
}
impl MediaBackend {
    fn configure(&mut self, options: NativeOptions) -> Result<()> {
        self.fps = options.fps.unwrap_or(30.0);
        if let Some((width, height)) = options.size {
            (self.width, self.height, self.requested_size) = (width, height, true);
        }
        if self.modes.is_empty()
            || options.size.is_none() && self.modes.iter().all(|m| m.fps.is_none())
        {
            return Ok(());
        }
        let index = best(&self.modes, |m| {
            options.size.is_none_or(|size| m.size() == size)
                && options.fps.is_none_or(|fps| m.honors(fps))
        })
        .ok_or_else(|| {
            let (width, height) = options.size.unwrap_or_else(|| {
                best(&self.modes, |_| true).map_or((0, 0), |i| self.modes[i].size())
            });
            self.unsupported(width, height, self.fps)
        })?;
        self.select(index, options.fps);
        Ok(())
    }
    fn select(&mut self, index: usize, fps: Option<f64>) {
        let mode = self.modes[index];
        self.mode = Some(index);
        (self.width, self.height) = mode.size();
        self.fps = mode.fps.map_or(fps.unwrap_or(self.fps), |(low, high)| {
            fps.map_or(high, |fps| fps.clamp(low, high))
        });
    }
    fn unsupported(&self, width: u32, height: u32, fps: f64) -> anyhow::Error {
        let mut listed = self.modes.clone();
        listed.sort_by_key(|m| (m.size() != (width, height), Reverse(m.rank())));
        let mut seen = HashSet::new();
        listed.retain(|m| m.size() == (width, height) || seen.insert(m.size()));
        let listed = listed.iter().take(8).map(Mode::label).collect::<Vec<_>>();
        anyhow!(
            "{width}x{height}@{} is not supported by {}; supported: {}",
            rounded(fps),
            device_name(&self.source),
            listed.join(", ")
        )
    }
    fn input_options(&self) -> Vec<String> {
        if !self.native {
            return Vec::new();
        }
        let mut options = vec!["-framerate".to_string(), self.fps.to_string()];
        if let Some((width, height)) = self
            .mode
            .map(|i| self.modes[i].size())
            .or(self.requested_size.then_some((self.width, self.height)))
        {
            options.extend(["-video_size".into(), format!("{width}x{height}")]);
        }
        if let Some(format) = &self.format {
            options.extend(["-input_format".into(), format.clone()]);
        }
        if self.source.starts_with("avfoundation:") {
            options.extend(["-pixel_format".into(), "uyvy422".into()]);
        }
        options
    }
    fn launch(&mut self) -> Result<()> {
        let mut decoder =
            Decoder::spawn_configured(&self.source, self.timeout, &self.input_options())?;
        let first = if self.native {
            let timeout = self.timeout.max(FIRST_FRAME_TIMEOUT);
            decoder.next(timeout).map_err(|error| {
                let stalled = decoder
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error
                    .is_none();
                decoder.stop();
                let log = decoder.log.text(&self.source);
                native_failure(&log, stalled, device_name(&self.source), timeout)
                    .map_or(error, |message| anyhow!(message))
            })?
        } else {
            decoder.next(self.timeout).context("open media input")?
        };
        if self.native && (self.requested_size || self.mode.is_some()) {
            ensure!(
                (first.width, first.height) == (self.width, self.height),
                "requested native video size {}x{} is not supported by {}; device returned {}x{}",
                self.width,
                self.height,
                device_name(&self.source),
                first.width,
                first.height
            );
        }
        (self.width, self.height) = (first.width, first.height);
        self.mode = self.mode.or_else(|| {
            self.modes
                .iter()
                .position(|m| m.size() == (first.width, first.height) && m.honors(self.fps))
        });
        self.first = Some(first);
        self.decoder = Some(decoder);
        Ok(())
    }
}
pub fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
    ensure!(
        info.transport == Transport::Media,
        "camera is not a media input"
    );
    let source = info
        .address
        .clone()
        .context("media camera has no source address")?;
    ensure!(
        !source.contains("<redacted>"),
        "credential-bearing media sources must be opened through open_url"
    );
    open_url(&source, timeout)
}
pub fn open_url(source: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    let (source, options) = native_source(source)?;
    let mut backend = MediaBackend {
        source,
        timeout,
        decoder: None,
        first: None,
        width: 0,
        height: 0,
        native: options.is_some(),
        requested_size: false,
        fps: 0.0,
        running: false,
        modes: Vec::new(),
        mode: None,
        format: None,
    };
    if let Some(options) = options {
        (backend.modes, backend.format) = probe(&backend.source);
        backend.configure(options)?;
    }
    backend.launch()?;
    Ok(Box::new(backend))
}
impl RegisterIo for MediaBackend {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let mut registers = [0u8; 32];
        registers[0..4].copy_from_slice(&self.width.to_le_bytes());
        registers[4..8].copy_from_slice(&self.height.to_le_bytes());
        registers[8..12].copy_from_slice(&RGB8.to_le_bytes());
        registers[12..16].copy_from_slice(&(self.width * self.height * 3).to_le_bytes());
        registers[16..20].copy_from_slice(&(self.running as u32).to_le_bytes());
        registers[20..28].copy_from_slice(&self.fps.to_le_bytes());
        registers[28..32].copy_from_slice(&self.mode.map_or(u32::MAX, |i| i as u32).to_le_bytes());
        let start = usize::try_from(address)?;
        let end = start
            .checked_add(length)
            .context("media register range overflow")?;
        Ok(registers
            .get(start..end)
            .context("unknown media register")?
            .to_vec())
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if address == 16 && data.len() == 4 {
            return Ok(());
        }
        ensure!(
            self.native,
            "media format features are read-only; configure the source camera through ONVIF or its administration interface"
        );
        ensure!(
            !self.running,
            "stop native acquisition before changing video mode"
        );
        match (address, data.len()) {
            (0 | 4, 4) => {
                let value = u32::from_le_bytes(data.try_into()?);
                ensure!(
                    (16..=8192).contains(&value),
                    "native dimensions must be 16..8192"
                );
                let (w, h) = if address == 0 {
                    (value, self.height)
                } else {
                    (self.width, value)
                };
                ensure!(
                    w as usize * h as usize <= MAX_FRAME / 3,
                    "native video mode exceeds frame memory bound"
                );
                if self.modes.is_empty() {
                    (self.width, self.height, self.requested_size) = (w, h, true);
                } else {
                    let index = self
                        .modes
                        .iter()
                        .enumerate()
                        .filter(|(_, m)| {
                            if address == 0 {
                                m.width == w
                            } else {
                                m.height == h
                            }
                        })
                        .max_by_key(|(_, m)| (m.size() == (w, h), m.honors(self.fps), m.rank()))
                        .map(|(index, _)| index)
                        .ok_or_else(|| self.unsupported(w, h, self.fps))?;
                    self.select(index, Some(self.fps));
                }
            }
            (20, 8) => {
                let value = f64::from_le_bytes(data.try_into()?);
                ensure!(
                    value.is_finite() && (0.1..=240.0).contains(&value),
                    "native frame rate must be 0.1..240"
                );
                if self.modes.is_empty() {
                    self.fps = value;
                } else {
                    let size = (self.width, self.height);
                    let index = best(&self.modes, |m| m.size() == size && m.honors(value))
                        .ok_or_else(|| self.unsupported(size.0, size.1, value))?;
                    self.select(index, Some(value));
                }
            }
            (28, 4) => {
                let index = u32::from_le_bytes(data.try_into()?) as usize;
                ensure!(index < self.modes.len(), "unknown native video mode");
                self.select(index, None);
            }
            _ => bail!("unsupported native camera register"),
        }
        self.decoder.take();
        self.first = None;
        Ok(())
    }
}
impl Backend for MediaBackend {
    fn xml(&mut self) -> Result<String> {
        let video_mode = if self.modes.is_empty() {
            ""
        } else {
            "<pFeature>VideoMode</pFeature>"
        };
        let mut xml = format!(
            "<RegisterDescription><Category Name='Root'><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature><pFeature>PayloadSize</pFeature><pFeature>AcquisitionFrameRate</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature>{video_mode}</Category>"
        );
        for (name, address) in [
            ("Width", 0),
            ("Height", 4),
            ("PixelFormat", 8),
            ("PayloadSize", 12),
        ] {
            let writable = self.native && matches!(name, "Width" | "Height");
            let access = if writable { "RW" } else { "RO" };
            let bounds = if writable {
                "<Min>16</Min><Max>8192</Max><pIsLocked>IsAcquiring</pIsLocked>"
            } else {
                ""
            };
            xml.push_str(&format!("<Integer Name='{name}'><pValue>{name}Reg</pValue>{bounds}</Integer><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>{access}</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"));
        }
        let access = if self.native { "RW" } else { "RO" };
        xml.push_str(&format!("<Float Name='AcquisitionFrameRate'><pValue>FrameRateReg</pValue><Min>0.1</Min><Max>240</Max><Unit>Hz</Unit><pIsLocked>IsAcquiring</pIsLocked></Float><FloatReg Name='FrameRateReg'><Address>20</Address><Length>8</Length><AccessMode>{access}</AccessMode><Endianess>LittleEndian</Endianess></FloatReg><Integer Name='IsAcquiring'><pValue>AcquiringReg</pValue></Integer><IntReg Name='AcquiringReg'><Address>16</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"));
        if !self.modes.is_empty() {
            let entries = self
                .modes
                .iter()
                .enumerate()
                .map(|(index, mode)| {
                    format!(
                        "<EnumEntry Name='{}'><Value>{index}</Value></EnumEntry>",
                        mode.label()
                    )
                })
                .collect::<String>();
            xml.push_str(&format!("<Enumeration Name='VideoMode'><pValue>VideoModeReg</pValue>{entries}<pIsLocked>IsAcquiring</pIsLocked></Enumeration><IntReg Name='VideoModeReg'><Address>28</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"));
        }
        xml.push_str("<Command Name='AcquisitionStart'><pValue>Control</pValue><CommandValue>1</CommandValue></Command><Command Name='AcquisitionStop'><pValue>Control</pValue><CommandValue>0</CommandValue></Command><IntReg Name='Control'><Address>16</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg></RegisterDescription>");
        Ok(xml)
    }
    fn start(&mut self, _: usize) -> Result<()> {
        if self.decoder.is_none() {
            self.launch()?;
        }
        self.running = true;
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.running, "media acquisition has not started");
        let frame = if let Some(frame) = self.first.take() {
            frame
        } else {
            self.decoder
                .as_mut()
                .context("media decoder stopped")?
                .next(timeout)?
        };
        self.width = frame.width;
        self.height = frame.height;
        Ok(frame)
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.decoder.take();
        self.first = None;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardConfig {
    pub output: String,
    pub codec: String,
    pub encoder: String,
    pub fps: f64,
    pub bitrate: String,
    #[serde(default)]
    pub storage: crate::storage::StoragePolicy,
    #[serde(default = "default_recording_cap")]
    pub max_file_bytes: u64,
}
pub fn default_recording_cap() -> u64 {
    512 * 1024 * 1024
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForwardStats {
    pub accepted: u64,
    pub written: u64,
    pub dropped: u64,
    pub encoder: String,
    pub error: Option<String>,
}
struct WriterState {
    latest: Option<Vec<u8>>,
    error: Option<String>,
}
struct WriterQueue {
    state: Mutex<WriterState>,
    changed: Condvar,
    running: AtomicBool,
    accepted: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
}
struct EncoderProcess {
    child: Child,
    writer: Option<JoinHandle<()>>,
    output_thread: Option<JoinHandle<EncodedOutput>>,
    log_thread: Option<JoinHandle<()>>,
    queue: Arc<WriterQueue>,
    log: Arc<Log>,
    _exe: Arc<Executable>,
    width: u32,
    height: u32,
    raw: RawInput,
    /// Set when a GStreamer Jetson encoder process precedes FFmpeg.
    jetson: Option<JetsonInput>,
    stage: Option<Child>,
    stage_log: Option<JoinHandle<()>>,
    encoder: String,
}
struct EncodedOutput {
    storage: crate::storage::StorageWriter,
    error: Option<String>,
}
pub struct Forwarder {
    config: ForwardConfig,
    process: Option<EncoderProcess>,
    final_stats: ForwardStats,
}
fn codec_family(codec: &str) -> Result<&'static str> {
    match codec {
        "h264" | "avc" => Ok("h264"),
        "h265" | "hevc" => Ok("hevc"),
        "av1" => Ok("av1"),
        _ => bail!("unsupported forwarding codec {codec}; use h264, hevc or av1"),
    }
}
fn select_encoder(exe: &Executable, config: &ForwardConfig) -> Result<String> {
    let family = codec_family(&config.codec)?;
    if config.encoder != "auto" {
        ensure!(
            !config.encoder.is_empty()
                && config.encoder.len() <= 64
                && config
                    .encoder
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid FFmpeg encoder name"
        );
        return Ok(config.encoder.clone());
    }
    // Vendor encoders first, then cross-vendor Vulkan Video, then software.
    // Every candidate must be present in the FFmpeg build and pass a real
    // test encode on this machine before it is chosen.
    let candidates: Vec<&str> = match (family, std::env::consts::OS) {
        ("h264", "macos") => vec!["h264_videotoolbox", "libx264"],
        ("hevc", "macos") => vec!["hevc_videotoolbox", "libx265"],
        ("av1", "macos") => vec!["libsvtav1", "libaom-av1"],
        ("h264", "linux") => vec![
            "h264_nvenc",
            "nvv4l2h264enc",
            "h264_amf",
            "h264_qsv",
            "h264_vaapi",
            "h264_vulkan",
            "h264_v4l2m2m",
            "libx264",
        ],
        ("hevc", "linux") => vec![
            "hevc_nvenc",
            "nvv4l2h265enc",
            "hevc_amf",
            "hevc_qsv",
            "hevc_vaapi",
            "hevc_vulkan",
            "hevc_v4l2m2m",
            "libx265",
        ],
        ("av1", "linux") => vec![
            "av1_nvenc",
            "nvv4l2av1enc",
            "av1_amf",
            "av1_qsv",
            "av1_vaapi",
            "av1_vulkan",
            "libsvtav1",
            "libaom-av1",
        ],
        ("h264", "windows") => vec![
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_vulkan",
            "h264_mf",
            "libx264",
        ],
        ("hevc", "windows") => vec![
            "hevc_nvenc",
            "hevc_qsv",
            "hevc_amf",
            "hevc_vulkan",
            "hevc_mf",
            "libx265",
        ],
        ("av1", "windows") => vec![
            "av1_nvenc",
            "av1_qsv",
            "av1_amf",
            "av1_vulkan",
            "av1_mf",
            "libsvtav1",
            "libaom-av1",
        ],
        ("h264", _) => vec!["libx264"],
        ("hevc", _) => vec!["libx265"],
        _ => vec!["av1_nvenc", "libsvtav1", "libaom-av1"],
    };
    let (_, available) = bounded_output(
        exe,
        &["-hide_banner".into(), "-encoders".into()],
        Duration::from_secs(5),
    )?;
    for candidate in candidates {
        if jetson_encoder(candidate) {
            if probe_jetson(candidate) {
                return Ok(candidate.into());
            }
            continue;
        }
        if !available
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(candidate))
        {
            continue;
        }
        if probe_encoder(exe, candidate, EncoderInput::Yuv)? {
            return Ok(candidate.into());
        }
    }
    bail!(
        "no usable {family} encoder found in bundled FFmpeg; choose --encoder explicitly or provide a build with a software encoder"
    )
}
/// NVIDIA Jetson hardware encoders, reached through NVIDIA's GStreamer
/// elements: on Jetson the video engine is a V4L2 device that FFmpeg's NVENC
/// and generic V4L2 wrappers cannot drive. GStreamer runs as a separate
/// process that encodes to MPEG-TS for FFmpeg to remux and deliver.
fn jetson_encoder(encoder: &str) -> bool {
    matches!(encoder, "nvv4l2h264enc" | "nvv4l2h265enc" | "nvv4l2av1enc")
}
fn jetson_parser(encoder: &str) -> &'static str {
    match encoder {
        "nvv4l2h264enc" => "h264parse",
        "nvv4l2h265enc" => "h265parse",
        _ => "av1parse",
    }
}
/// Run a helper tool briefly; true when it exits successfully in time.
fn tool_succeeds(program: &str, args: &[&str], timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}
/// The element exists and encodes a short test clip on this device; Orin,
/// for example, lists the AV1 element but has no AV1 encoder.
fn probe_jetson(encoder: &str) -> bool {
    cfg!(target_os = "linux")
        && tool_succeeds("gst-inspect-1.0", &[encoder], Duration::from_secs(5))
        && tool_succeeds(
            "gst-launch-1.0",
            &[
                "-q",
                "videotestsrc",
                "num-buffers=3",
                "!",
                "video/x-raw,width=640,height=480,format=I420",
                "!",
                "nvvidconv",
                "!",
                "video/x-raw(memory:NVMM),format=NV12",
                "!",
                encoder,
                "!",
                jetson_parser(encoder),
                "!",
                "fakesink",
            ],
            Duration::from_secs(10),
        )
}
/// Bits per second from a bitrate like `4M`, `2500k` or `800000`.
fn bits_per_second(bitrate: &str) -> Result<u64> {
    let (number, scale) = match bitrate.chars().last() {
        Some('k' | 'K') => (&bitrate[..bitrate.len() - 1], 1e3),
        Some('M') => (&bitrate[..bitrate.len() - 1], 1e6),
        Some('G') => (&bitrate[..bitrate.len() - 1], 1e9),
        _ => (bitrate, 1.0),
    };
    let value: f64 = number.parse().context("invalid forwarding bitrate")?;
    ensure!(
        value.is_finite() && value > 0.0 && value * scale <= u32::MAX as f64,
        "forwarding bitrate out of range"
    );
    Ok((value * scale) as u64)
}
/// Frames handed to a Jetson encoder: formats `nvvidconv` accepts in system
/// memory and converts on the VIC engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JetsonInput {
    /// Gray as the luma plane with neutral chroma.
    I420,
    Rgba,
}
impl JetsonInput {
    fn caps(self) -> &'static str {
        match self {
            Self::I420 => "i420",
            Self::Rgba => "rgba",
        }
    }
    fn bytes(self, frame: &Frame) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Rgba => crate::frame::rgba(frame)?,
            Self::I420 => {
                let mut data = crate::frame::convert(frame, |[gray, _, _]| gray)?;
                let chroma =
                    (frame.width as usize).div_ceil(2) * (frame.height as usize).div_ceil(2);
                data.resize(data.len() + 2 * chroma, 128);
                data
            }
        })
    }
}
/// Pixel layout handed to the encoder after FFmpeg's input conversion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncoderInput {
    /// Planar or semi-planar 4:2:0 converted by FFmpeg's SIMD swscale.
    Yuv,
    /// Packed RGB; the encoder's own color-conversion hardware makes YUV.
    Rgb,
}
/// NVENC and AMF accept packed RGB and convert it to YUV on the GPU, which
/// keeps that matrix multiply off the CPU for color sources.
fn hardware_rgb_input(encoder: &str) -> bool {
    encoder.ends_with("_nvenc") || encoder.ends_with("_amf")
}
/// Choose the encoder input for a source. RGB input is used only when the
/// encoder accepts it on this machine, so a driver without it keeps the YUV
/// path instead of falling back to software encoding.
fn encoder_input(exe: &Executable, encoder: &str, color: bool) -> Result<EncoderInput> {
    if color && hardware_rgb_input(encoder) && probe_encoder(exe, encoder, EncoderInput::Rgb)? {
        Ok(EncoderInput::Rgb)
    } else {
        Ok(EncoderInput::Yuv)
    }
}
fn probe_encoder(exe: &Executable, candidate: &str, input: EncoderInput) -> Result<bool> {
    {
        let mut args = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=size=640x480:rate=1",
            "-frames:v",
            "1",
            "-c:v",
            candidate,
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        args.extend(encoder_device_options(candidate));
        args.extend(encoder_output_options(candidate, input));
        args.extend(["-f".into(), "null".into(), "-".into()]);
        Ok(bounded_output(exe, &args, Duration::from_secs(3))?.0)
    }
}
fn encoder_device_options(encoder: &str) -> Vec<String> {
    if encoder.ends_with("_vaapi") {
        vec![
            "-vaapi_device".into(),
            std::env::var("CAPTUREFAB_VAAPI_DEVICE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        ]
    } else if encoder.ends_with("_vulkan") {
        // The first Vulkan device with video encode; FFmpeg loads the system
        // Vulkan loader at run time.
        ["-init_hw_device", "vulkan=vk", "-filter_hw_device", "vk"]
            .map(str::to_owned)
            .to_vec()
    } else {
        Vec::new()
    }
}
fn encoder_output_options(encoder: &str, input: EncoderInput) -> Vec<String> {
    let mut options = if encoder.ends_with("_vaapi") || encoder.ends_with("_vulkan") {
        // Convert and pad on the CPU, then upload into the device's frames.
        vec![
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2,format=nv12,hwupload",
            "-pix_fmt",
            if encoder.ends_with("_vaapi") {
                "vaapi"
            } else {
                "vulkan"
            },
        ]
    } else {
        vec![
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-pix_fmt",
            if input == EncoderInput::Rgb {
                "bgr0"
            } else if encoder.ends_with("_qsv")
                || encoder.ends_with("_mf")
                || encoder.ends_with("_videotoolbox")
            {
                // The native surface layout of these encoders: no repacking copy.
                "nv12"
            } else {
                "yuv420p"
            },
        ]
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if encoder.ends_with("_videotoolbox") {
        options.extend(["-allow_sw", "0"].map(str::to_owned));
    }
    if encoder.ends_with("_mf") {
        options.extend(["-hw_encoding", "1"].map(str::to_owned));
    }
    options
}
fn output_args(output: &str) -> Result<Vec<String>> {
    ensure!(
        !output.is_empty()
            && output.len() <= 16384
            && !output.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r'),
        "invalid forwarding destination"
    );
    let mut args = Vec::new();
    match scheme(output).as_deref() {
        Some("rtsp" | "rtsps") => {
            args.extend(["-f", "rtsp", "-rtsp_transport", "tcp"].map(str::to_string))
        }
        Some("rtmp" | "rtmps") => args.extend(["-f", "flv"].map(str::to_string)),
        Some("srt" | "udp" | "tcp") => args.extend(["-f", "mpegts"].map(str::to_string)),
        Some("http" | "https") => args.extend(["-f", "mpegts"].map(str::to_string)),
        Some("file") | None => {
            ensure!(
                !output.to_ascii_lowercase().ends_with(".m3u8"),
                "unsupported local HLS output: use MediaMTX for HLS or record a bounded .mkv/.mp4 file"
            );
        }
        Some(other) => bail!("unsupported forwarding destination protocol {other}"),
    }
    let destination = if scheme(output).is_none() && output.starts_with('-') {
        format!("file:{output}")
    } else {
        output.to_string()
    };
    args.push(destination);
    Ok(args)
}
fn recording_output(output: &str) -> Result<Option<(PathBuf, &'static str)>> {
    if !matches!(scheme(output).as_deref(), None | Some("file")) {
        return Ok(None);
    }
    let path = if scheme(output).as_deref() == Some("file") {
        output.split_once(':').unwrap().1
    } else {
        output
    };
    ensure!(
        !path.is_empty() && path != "-",
        "recording needs an output file"
    );
    let path = PathBuf::from(path);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mkv")
        .to_ascii_lowercase();
    let container = match extension.as_str() {
        "mp4" | "mov" => "mp4",
        "ts" | "mpegts" => "mpegts",
        "webm" => "webm",
        "mkv" | "matroska" => "matroska",
        _ => bail!("unsupported recording container: use .mkv, .mp4, .mov, .ts or .webm"),
    };
    Ok(Some((path, container)))
}
/// How a camera frame is handed to FFmpeg unchanged: its rawvideo pixel format,
/// bytes per pixel and whether it carries color. Sending sensor-native bytes
/// avoids a CPU conversion on the camera thread and, for mono and Bayer, moves
/// a third of the bytes RGB24 would through the pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawInput {
    pixel_format: u32,
    ffmpeg: &'static str,
    bytes_per_pixel: usize,
    color: bool,
}
impl RawInput {
    fn for_format(pixel_format: u32) -> Result<Self> {
        let (ffmpeg, bytes_per_pixel, color) = match pixel_format {
            crate::types::MONO8 => ("gray", 1, false),
            0x0110_0003 => ("gray10le", 2, false),
            0x0110_0005 => ("gray12le", 2, false),
            0x0110_0007 => ("gray16le", 2, false),
            crate::types::RGB8 => ("rgb24", 3, true),
            0x0218_0015 => ("bgr24", 3, true),
            0x0108_0008 => ("bayer_grbg8", 1, true),
            0x0108_0009 => ("bayer_rggb8", 1, true),
            0x0108_000a => ("bayer_gbrg8", 1, true),
            0x0108_000b => ("bayer_bggr8", 1, true),
            v => bail!(
                "unsupported PFNC format 0x{v:08x}; capture with --format raw to preserve bytes"
            ),
        };
        Ok(Self {
            pixel_format,
            ffmpeg,
            bytes_per_pixel,
            color,
        })
    }
}
impl Forwarder {
    pub fn new(config: ForwardConfig) -> Result<Self> {
        ensure!(
            config.fps.is_finite() && (0.1..=240.0).contains(&config.fps),
            "forwarding FPS must be within 0.1..240"
        );
        codec_family(&config.codec)?;
        output_args(&config.output)?;
        if recording_output(&config.output)?.is_some() {
            config.storage.validate()?;
            ensure!(
                config.max_file_bytes > 0 && config.max_file_bytes <= config.storage.max_bytes,
                "Storage full: recording reservation must be positive and no greater than the storage byte budget"
            );
        }
        ensure!(
            !config.bitrate.is_empty()
                && config.bitrate.len() <= 32
                && config
                    .bitrate
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'k' | b'K' | b'M' | b'G')),
            "invalid forwarding bitrate (for example 4M)"
        );
        Ok(Self {
            config,
            process: None,
            final_stats: ForwardStats::default(),
        })
    }
    fn spawn(&mut self, width: u32, height: u32, raw: RawInput) -> Result<()> {
        ensure!(
            width > 0 && height > 0 && width as usize * height as usize <= MAX_FRAME / 3,
            "invalid forwarding dimensions"
        );
        let recording = recording_output(&self.config.output)?;
        let local = recording.is_some();
        let exe = executable()?;
        let encoder = select_encoder(&exe, &self.config)?;
        let jetson = jetson_encoder(&encoder).then_some(if raw.color {
            JetsonInput::Rgba
        } else {
            JetsonInput::I420
        });
        let mut cmd = command(&exe);
        let mut stage = None;
        if let Some(input) = jetson {
            let gop = ((self.config.fps * 2.0).round().max(1.0) as u32).to_string();
            let mut gst = Command::new("gst-launch-1.0");
            gst.args([
                "-q",
                "fdsrc",
                "fd=0",
                "!",
                "rawvideoparse",
                &format!("width={width}"),
                &format!("height={height}"),
                &format!("format={}", input.caps()),
                &format!(
                    "framerate={}/1000",
                    (self.config.fps * 1000.0).round() as u64
                ),
                "!",
                "nvvidconv",
                "!",
                "video/x-raw(memory:NVMM),format=NV12",
                "!",
                &encoder,
                &format!("bitrate={}", bits_per_second(&self.config.bitrate)?),
                &format!("iframeinterval={gop}"),
                &format!("idrinterval={gop}"),
                "insert-sps-pps=true",
                "!",
                jetson_parser(&encoder),
                "config-interval=-1",
                "!",
                "mpegtsmux",
                "!",
                "fdsink",
                "fd=1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            let mut child = gst
                .spawn()
                .context("cannot launch GStreamer Jetson encoder")?;
            let encoded = child.stdout.take().context("GStreamer stdout")?;
            cmd.stdin(Stdio::from(encoded));
            stage = Some(child);
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "mpegts",
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                "copy",
            ]);
        } else {
            let input = encoder_input(&exe, &encoder, raw.color)?;
            cmd.args(encoder_device_options(&encoder));
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "rawvideo",
                "-pix_fmt",
                raw.ffmpeg,
                "-video_size",
                &format!("{width}x{height}"),
                "-framerate",
                &self.config.fps.to_string(),
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                &encoder,
                "-b:v",
                &self.config.bitrate,
                "-g",
                &((self.config.fps * 2.0).round().max(1.0) as u32).to_string(),
            ]);
            cmd.args(encoder_output_options(&encoder, input));
            if encoder == "libx264" {
                cmd.args(["-preset", "veryfast", "-tune", "zerolatency"]);
            } else if encoder == "libx265" {
                cmd.args(["-preset", "veryfast"]);
            } else if encoder.ends_with("_videotoolbox") {
                cmd.args(["-realtime", "1"]);
            }
        }
        if let Some((_, container)) = &recording {
            if *container == "mp4" {
                cmd.args(["-movflags", "frag_keyframe+empty_moov"]);
            }
            cmd.args(["-f", container, "pipe:1"]);
        } else {
            cmd.args(output_args(&self.config.output)?);
        }
        if stage.is_none() {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(if local { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::piped());
        // Validate/probe before creating a file, then reserve the global budget.
        let storage = recording
            .map(|(path, _)| {
                crate::storage::create_writer(
                    &path,
                    &self.config.storage,
                    self.config.max_file_bytes,
                )
            })
            .transpose()?;
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let Some(mut stage) = stage {
                    let _ = stage.kill();
                    let _ = stage.wait();
                }
                if let Some(storage) = storage {
                    storage.abort().context("remove failed recording output")?;
                }
                return Err(error).context("cannot launch FFmpeg forwarder");
            }
        };
        let log = Log::new();
        // Frames go to the first process: GStreamer when it encodes, else FFmpeg.
        let (mut input, stage_log) = match stage.as_mut() {
            Some(stage) => (
                stage.stdin.take().context("GStreamer stdin")?,
                Some(log.collect(stage.stderr.take().context("GStreamer stderr")?)),
            ),
            None => (child.stdin.take().context("FFmpeg stdin")?, None),
        };
        let log_thread = log.collect(child.stderr.take().unwrap());
        let queue = Arc::new(WriterQueue {
            state: Mutex::new(WriterState {
                latest: None,
                error: None,
            }),
            changed: Condvar::new(),
            running: AtomicBool::new(true),
            accepted: AtomicU64::new(0),
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let output_thread = storage.map(|mut storage| {
            let mut output = child.stdout.take().expect("piped encoded output");
            let shared = queue.clone();
            thread::spawn(move || {
                let error = std::io::copy(&mut output, &mut storage)
                    .err()
                    .map(|error| format!("write bounded recording: {error}"));
                if let Some(error) = &error {
                    shared
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .error
                        .get_or_insert_with(|| error.clone());
                    shared.running.store(false, Ordering::Release);
                    shared.changed.notify_all();
                }
                // Only the owner observing the child's exit status may commit
                // a recording. EOF alone can also mean an encoder failure.
                EncodedOutput { storage, error }
            })
        });
        let shared = queue.clone();
        let interval = Duration::from_secs_f64(1.0 / self.config.fps);
        let writer = thread::spawn(move || {
            let mut last = None;
            let mut next = Instant::now();
            while shared.running.load(Ordering::Acquire) {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                while shared.running.load(Ordering::Acquire)
                    && (last.is_none() && state.latest.is_none() || Instant::now() < next)
                {
                    let timeout = if last.is_none() {
                        Duration::from_secs(1)
                    } else {
                        next.saturating_duration_since(Instant::now())
                    };
                    state = shared
                        .changed
                        .wait_timeout(state, timeout)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                if !shared.running.load(Ordering::Acquire) {
                    break;
                }
                if let Some(latest) = state.latest.take() {
                    last = Some(latest)
                }
                drop(state);
                if let Some(bytes) = &last {
                    if let Err(e) = input.write_all(bytes) {
                        shared
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .error
                            .get_or_insert_with(|| format!("FFmpeg input failed: {e}"));
                        break;
                    }
                    shared.written.fetch_add(1, Ordering::Relaxed);
                }
                next = Instant::now() + interval;
            }
            // Closing stdin lets FFmpeg flush its muxer and finalize files.
            drop(input);
            shared.running.store(false, Ordering::Release);
            shared.changed.notify_all();
        });
        self.process = Some(EncoderProcess {
            child,
            writer: Some(writer),
            output_thread,
            log_thread: Some(log_thread),
            queue,
            log,
            _exe: exe,
            width,
            height,
            raw,
            jetson,
            stage,
            stage_log,
            encoder,
        });
        Ok(())
    }
    pub fn push(&mut self, frame: &Frame) -> Result<bool> {
        if self.process.is_none() {
            self.spawn(
                frame.width,
                frame.height,
                RawInput::for_format(frame.pixel_format)?,
            )?
        }
        let process = self.process.as_mut().unwrap();
        ensure!(
            frame.width == process.width && frame.height == process.height,
            "forwarding source dimensions changed; restart forwarding"
        );
        ensure!(
            frame.pixel_format == process.raw.pixel_format,
            "forwarding source pixel format changed; restart forwarding"
        );
        let stored_error = process
            .queue
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone();
        if let Some(error) = stored_error {
            bail!("{error}: {}", process.log.text(&self.config.output).trim());
        }
        if let Some(status) = process.child.try_wait()? {
            let stored_error = process
                .queue
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .error
                .clone();
            if let Some(error) = stored_error {
                bail!("{error}: {}", process.log.text(&self.config.output).trim());
            }
            bail!(
                "FFmpeg forwarding exited ({status}): {}",
                process.log.text(&self.config.output).trim()
            );
        }
        let length = frame.width as usize * frame.height as usize * process.raw.bytes_per_pixel;
        ensure!(
            frame.data.len() >= length,
            "truncated {} frame",
            crate::frame::pixel_format_name(frame.pixel_format)
        );
        // FFmpeg converts on its own threads with SIMD swscale (or the encoder
        // hardware), so the camera thread only copies sensor bytes. Jetson's
        // converter takes RGBA or I420, built here in one pass.
        let bytes = match process.jetson {
            Some(input) => input.bytes(frame)?,
            None => frame.data[..length].to_vec(),
        };
        let mut state = process
            .queue
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.error {
            bail!("{error}: {}", process.log.text(&self.config.output).trim());
        }
        ensure!(
            process.queue.running.load(Ordering::Acquire),
            "FFmpeg forwarding writer stopped"
        );
        let replaced = state.latest.replace(bytes).is_some();
        process.queue.accepted.fetch_add(1, Ordering::Relaxed);
        if replaced {
            process.queue.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(state);
        process.queue.changed.notify_one();
        Ok(!replaced)
    }
    pub fn stats(&self) -> ForwardStats {
        if let Some(p) = &self.process {
            ForwardStats {
                accepted: p.queue.accepted.load(Ordering::Relaxed),
                written: p.queue.written.load(Ordering::Relaxed),
                dropped: p.queue.dropped.load(Ordering::Relaxed),
                encoder: p.encoder.clone(),
                error: p
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .error
                    .clone(),
            }
        } else {
            self.final_stats.clone()
        }
    }
    pub fn stop(&mut self) -> Result<()> {
        self.final_stats = self.stats();
        let Some(mut process) = self.process.take() else {
            return Ok(());
        };
        process.queue.running.store(false, Ordering::Release);
        process.queue.changed.notify_all();
        // A GStreamer stage must drain the hardware encoder before FFmpeg sees EOF.
        let grace = if process.stage.is_some() { 6 } else { 3 };
        let deadline = Instant::now() + Duration::from_secs(grace);
        let mut shutdown_error = None;
        let status = loop {
            match process.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(error) => {
                    shutdown_error =
                        Some(format!("cannot inspect FFmpeg forwarding process: {error}"));
                    let _ = process.child.kill();
                    let _ = process.child.wait();
                    break None;
                }
            }
            if Instant::now() >= deadline {
                let _ = process.child.kill();
                let _ = process.child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        if let Some(mut stage) = process.stage.take() {
            // FFmpeg has exited; the encoder stage normally already has too.
            if !matches!(stage.try_wait(), Ok(Some(_))) {
                let _ = stage.kill();
            }
            let _ = stage.wait();
        }
        if let Some(thread) = process.stage_log.take() {
            let _ = thread.join();
        }
        if let Some(thread) = process.writer.take()
            && thread.join().is_err()
        {
            shutdown_error
                .get_or_insert_with(|| "FFmpeg input writer terminated unexpectedly".to_owned());
        }
        let mut recording = None;
        if let Some(thread) = process.output_thread.take() {
            match thread.join() {
                Ok(output) => recording = Some(output),
                Err(_) => {
                    shutdown_error.get_or_insert_with(|| {
                        "FFmpeg recording writer terminated unexpectedly".to_owned()
                    });
                }
            }
        }
        let mut error = process
            .queue
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone();
        if error.is_none() {
            error = shutdown_error;
        }
        if let Some(thread) = process.log_thread.take() {
            let _ = thread.join();
        }
        self.final_stats.written = process.queue.written.load(Ordering::Relaxed);
        if let Some(output) = recording {
            if error.is_none() {
                error = output.error;
            }
            let valid = status.as_ref().is_some_and(|status| status.success())
                && error.is_none()
                && output.storage.bytes_written() > 0;
            let result = if valid {
                output.storage.finish()
            } else {
                output.storage.abort()
            };
            if let Err(failure) = result {
                error.get_or_insert_with(|| format!("recording finalization failed: {failure:#}"));
            }
        }
        if error.is_none() {
            error = match status {
                Some(status) if status.success() => None,
                Some(status) => Some(format!(
                    "FFmpeg forwarding failed ({status}): {}",
                    process.log.text(&self.config.output).trim()
                )),
                None => {
                    Some("FFmpeg forwarding shutdown timed out; subprocess was terminated".into())
                }
            };
        }
        self.final_stats.error = error.clone();
        if let Some(error) = error {
            let log = process.log.text(&self.config.output);
            if log.trim().is_empty() {
                bail!("{error}");
            }
            bail!("{error}: {}", log.trim());
        }
        Ok(())
    }
}
impl Drop for Forwarder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genicam::NodeMap;
    #[test]
    fn ppm_preserves_whitespace_pixels_and_multiple_frames() {
        let mut bytes = b"P6\n2 1\n255\n".to_vec();
        bytes.extend([10, 32, 13, 0, 255, 1]);
        bytes.extend(b"P6\n1 1\n255\n");
        bytes.extend([1, 2, 3]);
        let mut reader = BufReader::new(bytes.as_slice());
        let a = read_ppm(&mut reader, 1, 0).unwrap();
        assert_eq!(a.data, [10, 32, 13, 0, 255, 1]);
        assert_eq!(read_ppm(&mut reader, 2, 0).unwrap().data, [1, 2, 3]);
    }
    #[test]
    fn ppm_rejects_oversize_truncation_and_sample_depth() {
        for bytes in [
            &b"P6\n9999999 9999999\n255\n"[..],
            &b"P6\n1 1\n65535\n"[..],
            &b"P6\n1 1\n255\n\0"[..],
        ] {
            assert!(read_ppm(&mut BufReader::new(bytes), 1, 0).is_err());
        }
    }
    #[test]
    fn sources_redact_credentials_and_forbid_command_protocols() {
        assert_eq!(
            redact_url("rtsp://user:pass@camera:554/live?token=abc&fps=30"),
            "rtsp://<redacted>@camera:554/live?token=<redacted>&fps=30"
        );
        assert_eq!(
            redact_url("srt://host:8890?passphrase=secret&mode=caller"),
            "srt://host:8890?passphrase=<redacted>&mode=caller"
        );
        assert!(info("rtsp://camera/live").is_ok());
        assert!(info("concat:secret|other").is_err());
        assert!(info("rtsp://camera/live\n-x").is_err());
    }
    #[test]
    fn forwarding_output_selection() {
        assert_eq!(
            output_args("rtsp://host/live").unwrap(),
            ["-f", "rtsp", "-rtsp_transport", "tcp", "rtsp://host/live"]
        );
        assert!(
            output_args("srt://host:8890")
                .unwrap()
                .contains(&"mpegts".into())
        );
        assert!(output_args("record.m3u8").is_err());
        assert!(recording_output("record.unknown").is_err());
        assert_eq!(
            recording_output("file:record.mp4").unwrap().unwrap().1,
            "mp4"
        );
    }
    #[test]
    fn jetson_inputs_and_bitrates() {
        assert_eq!(bits_per_second("4M").unwrap(), 4_000_000);
        assert_eq!(bits_per_second("2500k").unwrap(), 2_500_000);
        assert_eq!(bits_per_second("800000").unwrap(), 800_000);
        assert!(bits_per_second("5G").is_err() && bits_per_second("x").is_err());
        let gray = Frame {
            id: 1,
            width: 3,
            height: 3,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: (1..=9).collect(),
        };
        // Luma plane, then 2x2 U and V planes for odd dimensions.
        let i420 = JetsonInput::I420.bytes(&gray).unwrap();
        assert_eq!(i420.len(), 9 + 2 * 4);
        assert_eq!((&i420[..9], &i420[9..]), (&gray.data[..], &[128; 8][..]));
        let rgba = JetsonInput::Rgba.bytes(&gray).unwrap();
        assert_eq!(&rgba[..8], [1, 1, 1, 255, 2, 2, 2, 255]);
        assert!(jetson_encoder("nvv4l2h264enc") && !jetson_encoder("h264_nvenc"));
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_raw_inputs_decode_like_capturefab() {
        // Forwarding hands sensor bytes to FFmpeg. Each mapping must describe
        // the same image Capturefab's own converter produces; a swapped Bayer
        // phase or channel order shows up as a large color error.
        let exe = executable().unwrap();
        let (width, height) = (64usize, 48usize);
        let rgb: Vec<[u8; 3]> = (0..width * height)
            .map(|i| {
                let (x, y) = (i % width, i / width);
                [(40 + 2 * x) as u8, (60 + 3 * y) as u8, (200 - x - y) as u8]
            })
            .collect();
        let luma =
            |p: &[u8; 3]| ((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 8) as u16;
        let wide = |shift: u16| -> Vec<u8> {
            rgb.iter()
                .flat_map(|p| (luma(p) << shift).to_le_bytes())
                .collect()
        };
        let formats: [(u32, Vec<u8>); 10] = [
            (
                crate::types::MONO8,
                rgb.iter().map(|p| luma(p) as u8).collect(),
            ),
            (0x0110_0003, wide(2)),
            (0x0110_0005, wide(4)),
            (0x0110_0007, wide(8)),
            (crate::types::RGB8, rgb.iter().flatten().copied().collect()),
            (
                0x0218_0015,
                rgb.iter().flat_map(|p| [p[2], p[1], p[0]]).collect(),
            ),
            (0x0108_0008, bayer(&rgb, width, [1, 0, 2, 1])),
            (0x0108_0009, bayer(&rgb, width, [0, 1, 1, 2])),
            (0x0108_000a, bayer(&rgb, width, [1, 2, 0, 1])),
            (0x0108_000b, bayer(&rgb, width, [2, 1, 1, 0])),
        ];
        fn bayer(rgb: &[[u8; 3]], width: usize, cfa: [usize; 4]) -> Vec<u8> {
            (0..rgb.len())
                .map(|i| rgb[i][cfa[((i / width) & 1) * 2 + ((i % width) & 1)]])
                .collect()
        }
        for (pixel_format, data) in formats {
            let raw = RawInput::for_format(pixel_format).unwrap();
            let frame = Frame {
                id: 1,
                width: width as u32,
                height: height as u32,
                pixel_format,
                timestamp_ns: 0,
                data,
            };
            let expected = crate::frame::rgb(&frame).unwrap();
            let mut child = command(&exe)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                ])
                .arg(raw.ffmpeg)
                .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let bytes = frame.data.clone();
            let feeder = thread::spawn(move || stdin.write_all(&bytes));
            let output = child.wait_with_output().unwrap();
            feeder.join().unwrap().unwrap();
            assert_eq!(output.stdout.len(), expected.len(), "{}", raw.ffmpeg);
            // Compare away from the border, where demosaic edge rules differ.
            let mut error = 0u64;
            let mut count = 0u64;
            for y in 2..height - 2 {
                for x in 2..width - 2 {
                    for c in 0..3 {
                        let i = (y * width + x) * 3 + c;
                        error += (output.stdout[i] as i64 - expected[i] as i64).unsigned_abs();
                        count += 1;
                    }
                }
            }
            let mean = error as f64 / count as f64;
            assert!(mean < 3.0, "{}: mean error {mean:.2}", raw.ffmpeg);
        }
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_file_decoding_and_recording() {
        let exe = executable().unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "capturefab-media-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).unwrap();
        let input = dir.join("source.mkv");
        let output = dir.join("forward.mp4");
        let args = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=5",
            "-frames:v",
            "5",
            "-c:v",
            "ffv1",
            input.to_str().unwrap(),
        ]
        .map(str::to_string);
        let (success, log) = bounded_output(&exe, &args, Duration::from_secs(5)).unwrap();
        assert!(success, "{log}");
        let mut camera = open_url(input.to_str().unwrap(), Duration::from_secs(3)).unwrap();
        camera.start(64 * 48 * 3).unwrap();
        let mut frames = Vec::new();
        for id in 1..=5 {
            let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
            assert_eq!(frame.id, id);
            assert_eq!(
                (frame.width, frame.height, frame.data.len()),
                (64, 48, 64 * 48 * 3)
            );
            frames.push(frame);
        }
        camera.stop().unwrap();
        let mut forward = Forwarder::new(ForwardConfig {
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 10.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
        })
        .unwrap();
        for frame in &frames {
            forward.push(frame).unwrap();
            thread::sleep(Duration::from_millis(110));
        }
        forward.stop().unwrap();
        assert!(output.metadata().unwrap().len() > 100);
        assert!(forward.stats().written >= 4);
        let mut replay = open_url(output.to_str().unwrap(), Duration::from_secs(3)).unwrap();
        replay.start(64 * 48 * 3).unwrap();
        assert_eq!(replay.next_frame(Duration::from_secs(3)).unwrap().width, 64);
        replay.stop().unwrap();
        fs::remove_file(input).unwrap();
        fs::remove_file(output).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg and permission to bind localhost sockets"]
    fn ffmpeg_stalled_network_input_times_out_and_cancels() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("tcp://{}", listener.local_addr().unwrap());
        let (release, stop) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            let _ = stop.recv_timeout(Duration::from_secs(5));
        });
        let start = Instant::now();
        let mut decoder = Decoder::spawn(&url, Duration::from_millis(150)).unwrap();
        assert!(decoder.next(Duration::from_millis(150)).is_err());
        decoder.stop();
        assert!(start.elapsed() < Duration::from_secs(2));
        let _ = release.send(());
        server.join().unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg and permission to bind localhost sockets"]
    fn ffmpeg_stalled_forwarding_has_bounded_latest_frame_queue() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let output = format!("tcp://{}", listener.local_addr().unwrap());
        let (release, stop) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            socket2::SockRef::from(&stream)
                .set_recv_buffer_size(1024)
                .unwrap();
            let _ = stop.recv_timeout(Duration::from_secs(8));
        });
        let mut forward = Forwarder::new(ForwardConfig {
            output,
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 60.0,
            bitrate: "32M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
        })
        .unwrap();
        let mut noise = 123u32;
        let data = (0..640 * 480 * 3)
            .map(|_| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                noise as u8
            })
            .collect::<Vec<_>>();
        let frame = Frame {
            id: 1,
            width: 640,
            height: 480,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data,
        };
        forward.push(&frame).unwrap();
        let start = Instant::now();
        for _ in 0..120 {
            forward.push(&frame).unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(forward.stats().dropped > 0);
        let stop_started = Instant::now();
        let _ = forward.stop();
        assert!(stop_started.elapsed() < Duration::from_secs(5));
        let _ = release.send(());
        server.join().unwrap();
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_forwarding_reports_file_muxer_failure() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-invalid-encoder-{nonce}.mp4"));
        let mut forward = Forwarder::new(ForwardConfig {
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "capturefab_nonexistent_encoder".into(),
            fps: 30.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
        })
        .unwrap();
        let frame = Frame {
            id: 1,
            width: 64,
            height: 48,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data: vec![0; 64 * 48 * 3],
        };
        forward.push(&frame).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(forward.stop().is_err());
        assert!(
            !path.exists(),
            "invalid recordings must be removed and release their reservation"
        );
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_local_recording_reservation_is_bounded_and_removed_on_failure() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-bounded-recording-{nonce}.mp4"));
        let mut forward = Forwarder::new(ForwardConfig {
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "1M".into(),
            storage: Default::default(),
            max_file_bytes: 512,
        })
        .unwrap();
        let frame = Frame {
            id: 1,
            width: 64,
            height: 48,
            pixel_format: RGB8,
            timestamp_ns: 0,
            data: vec![127; 64 * 48 * 3],
        };
        forward.push(&frame).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while forward.stats().error.is_none() && Instant::now() < deadline {
            let _ = forward.push(&frame);
            thread::sleep(Duration::from_millis(20));
        }
        let error = forward.stop().unwrap_err().to_string();
        assert!(error.contains("Storage full"), "{error}");
        assert!(
            !path.exists(),
            "over-limit partial recordings must be removed"
        );
    }
    #[test]
    fn forwarding_configuration_fails_closed() {
        let config = ForwardConfig {
            output: "video.mp4".into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            storage: Default::default(),
            max_file_bytes: default_recording_cap(),
        };
        assert!(Forwarder::new(config.clone()).is_ok());
        let mut invalid = config;
        invalid.fps = f64::NAN;
        assert!(Forwarder::new(invalid).is_err());
    }
    const C920_MODES: &str = "\
[in#0 @ 0x77dac14000] Selected video size (16x16) is not supported by the device.
[in#0 @ 0x77dac14000] Supported modes:
[in#0 @ 0x77dac14000]   1280x720@[30.000030 30.000030]fps
[in#0 @ 0x77dac14000]   1280x720@[24.000038 24.000038]fps
[in#0 @ 0x77dac14000]   1280x720@[20.000000 20.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[15.000015 15.000015]fps
[in#0 @ 0x77dac14000]   1280x720@[10.000000 10.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[7.500002 7.500002]fps
[in#0 @ 0x77dac14000]   1280x720@[5.000000 5.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[10.000000 10.000000]fps
[in#0 @ 0x77dac14000]   1280x720@[7.500002 7.500002]fps
[in#0 @ 0x77dac14000]   1280x720@[5.000000 5.000000]fps
[in#0 @ 0x77dac14000]   1920x1080@[30.000030 30.000030]fps
[in#0 @ 0x77dac14000]   1920x1080@[24.000038 24.000038]fps
[in#0 @ 0x77dac14000]   1920x1080@[20.000000 20.000000]fps
[in#0 @ 0x77dac14000]   1920x1080@[15.000015 15.000015]fps
[in#0 @ 0x77dac14000]   1920x1080@[10.000000 10.000000]fps
[in#0 @ 0x77dac14000]   1920x1080@[7.500002 7.500002]fps
[in#0 @ 0x77dac14000]   1920x1080@[5.000000 5.000000]fps
[in#0 @ 0x77da808000] Error opening input: Input/output error
Error opening input file 0:none.
Error opening input files: Input/output error
";
    const IPHONE_MODES: &str = "\
[in#0 @ 0x7ad0c24000] Selected video size (16x16) is not supported by the device.
[in#0 @ 0x7ad0c24000] Supported modes:
[in#0 @ 0x7ad0c24000]   640x480@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c24000]   640x480@[1.000000 60.000000]fps
[in#0 @ 0x7ad0c24000]   1280x720@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c24000]   1280x720@[2.000000 60.000000]fps
[in#0 @ 0x7ad0c24000]   1920x1080@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c24000]   1920x1080@[2.000000 60.000000]fps
[in#0 @ 0x7ad0c24000]   1920x1440@[1.000000 30.000000]fps
[in#0 @ 0x7ad0c20000] Error opening input: Input/output error
Error opening input file 2:none.
Error opening input files: Input/output error
";
    fn labels(modes: &[Mode]) -> Vec<String> {
        modes.iter().map(Mode::label).collect()
    }
    fn native(source: &str, modes: Vec<Mode>) -> MediaBackend {
        MediaBackend {
            source: source.into(),
            timeout: Duration::from_secs(1),
            decoder: None,
            first: None,
            width: 0,
            height: 0,
            native: true,
            requested_size: false,
            fps: 0.0,
            running: false,
            modes,
            mode: None,
            format: None,
        }
    }
    #[test]
    fn avfoundation_modes_keep_range_maxima_of_the_last_format_per_size() {
        assert_eq!(
            labels(&avfoundation_modes(C920_MODES)),
            [
                "1280x720@10",
                "1280x720@7.5",
                "1280x720@5",
                "1920x1080@30",
                "1920x1080@24",
                "1920x1080@20",
                "1920x1080@15",
                "1920x1080@10",
                "1920x1080@7.5",
                "1920x1080@5"
            ]
        );
        let iphone = avfoundation_modes(IPHONE_MODES);
        assert_eq!(
            labels(&iphone),
            ["640x480@60", "1280x720@60", "1920x1080@60", "1920x1440@30"]
        );
        assert!(iphone[0].honors(59.995) && !iphone[0].honors(59.98) && !iphone[0].honors(30.0));
        assert_eq!(
            iphone[best(&iphone, |_| true).unwrap()].label(),
            "1920x1440@30"
        );
        let slow =
            [(1600, 896, 7.5), (640, 480, 15.0), (1280, 720, 10.0)].map(|(width, height, fps)| {
                Mode {
                    width,
                    height,
                    fps: Some((fps, fps)),
                }
            });
        assert_eq!(slow[best(&slow, |_| true).unwrap()].label(), "640x480@15");
    }
    #[test]
    fn native_video_modes_are_chosen_validated_and_switched() {
        let mut camera = native(
            "avfoundation:HD Pro Webcam C920",
            avfoundation_modes(C920_MODES),
        );
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "30",
                "-video_size",
                "1920x1080",
                "-pixel_format",
                "uyvy422"
            ]
        );
        let rejected = NativeOptions {
            size: Some((1280, 720)),
            fps: Some(30.0),
        };
        assert_eq!(
            camera.configure(rejected).unwrap_err().to_string(),
            "1280x720@30 is not supported by HD Pro Webcam C920; supported: 1280x720@10, 1280x720@7.5, 1280x720@5, 1920x1080@30"
        );
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: None,
            })
            .unwrap();
        assert_eq!((camera.width, camera.height, camera.fps), (1280, 720, 10.0));
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@10");
        assert_eq!(nodes.choices(&mut camera, "VideoMode").unwrap().len(), 10);
        nodes.set(&mut camera, "Width", "1920").unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1920x1080@10");
        let error = nodes
            .set(&mut camera, "AcquisitionFrameRate", "29.97")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("1920x1080@29.97 is not supported by HD Pro Webcam C920; supported: 1920x1080@30, 1920x1080@24,")
        );
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "30")
            .unwrap();
        nodes.set(&mut camera, "VideoMode", "1280x720@5").unwrap();
        assert_eq!((camera.width, camera.height, camera.fps), (1280, 720, 5.0));
        assert!(nodes.set(&mut camera, "Height", "600").is_err());
        assert!(camera.write_memory(28, &10u32.to_le_bytes()).is_err());
        camera.running = true;
        let locked = nodes.set(&mut camera, "VideoMode", "1920x1080@30");
        assert!(locked.unwrap_err().to_string().contains("locked"));
        let many = native(
            "dshow:video=Capture Card",
            (1..=12)
                .map(|i| Mode {
                    width: 160 * i,
                    height: 90 * i,
                    fps: Some((5.0, 60.0)),
                })
                .collect(),
        );
        let message = many.unsupported(100, 100, 30.0).to_string();
        assert!(
            message.starts_with(
                "100x100@30 is not supported by Capture Card; supported: 1920x1080@60, "
            )
        );
        assert_eq!(message.matches('@').count(), 9);
        let mut stream = native("rtsp://camera/live", Vec::new());
        stream.native = false;
        assert!(
            !NodeMap::parse(&stream.xml().unwrap())
                .unwrap()
                .has("VideoMode")
        );
    }
    #[test]
    fn dshow_and_v4l2_listings_become_honored_modes() {
        let dshow = dshow_modes(concat!(
            "[dshow @ 000001b5e2a6c3c0] DirectShow video device options (from video devices)\n",
            "[dshow @ 000001b5e2a6c3c0]  Pin \"Capture\" (alternative pin name \"0\")\n",
            "[dshow @ 000001b5e2a6c3c0]   vcodec=mjpeg  min s=1920x1080 fps=10 max s=1920x1080 fps=30\n",
            "[dshow @ 000001b5e2a6c3c0]   vcodec=mjpeg  min s=1920x1080 fps=10 max s=1920x1080 fps=30 (pc, bt470bg/bt709/unknown, center)\n",
            "[dshow @ 000001b5e2a6c3c0]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=30\n",
            "[dshow @ 000001b5e2a6c3c0]   pixel_format=yuyv422  min s=640x480 fps=7.5 max s=640x480 fps=29.97(left)\n",
        ));
        assert_eq!(labels(&dshow), ["1920x1080@30", "640x480@29.97"]);
        assert_eq!(dshow[0].fps, Some((5.0, 30.0)));
        let mut camera = native("dshow:video=Integrated Webcam", dshow);
        camera
            .configure(NativeOptions {
                size: None,
                fps: Some(15.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options(),
            ["-framerate", "15", "-video_size", "1920x1080"]
        );
        let (modes, format) = v4l2_modes(concat!(
            "[video4linux2,v4l2 @ 0x5581d1c2a0] Compressed:       mjpeg :          Motion-JPEG : 1920x1080 1280x720 640x480\n",
            "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       : Unsupported :         10-bit Bayer : 4056x3040\n",
            "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720 640x480\n",
            "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       :        gray :            8-bit Greyscale : {8-1920, 8}x{8-1080, 8}\n",
        ));
        assert_eq!(labels(&modes), ["640x480", "1280x720"]);
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.format = format;
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            ["-framerate", "30", "-input_format", "yuyv422"]
        );
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: Some(60.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "60",
                "-video_size",
                "1280x720",
                "-input_format",
                "yuyv422"
            ]
        );
    }
    #[test]
    fn native_devices_prefer_names_and_failures_are_concise() {
        let devices = avfoundation_devices(concat!(
            "[AVFoundation indev @ 0x7b45020140] AVFoundation video devices:\n",
            "[AVFoundation indev @ 0x7b45020140] [0] HD Pro Webcam C920\n",
            "[AVFoundation indev @ 0x7b45020140] [1] iPhone Camera\n",
            "[AVFoundation indev @ 0x7b45020140] [2] FaceTime HD Camera\n",
            "[AVFoundation indev @ 0x7b45020140] [3] Capture screen 0\n",
            "[AVFoundation indev @ 0x7b45020140] [4] Cam\n",
            "[AVFoundation indev @ 0x7b45020140] [5] Cam 2\n",
            "[AVFoundation indev @ 0x7b45020140] [6] 4K Camera\n",
            "[AVFoundation indev @ 0x7b45020140] [7] Desk: Left\n",
            "[AVFoundation indev @ 0x7b45020140] AVFoundation audio devices:\n",
            "[AVFoundation indev @ 0x7b45020140] [0] MacBook Pro Microphone\n",
            "[in#0 @ 0x7b45020000] Error opening input: Input/output error\n",
        ));
        let ids = devices
            .iter()
            .map(|device| device["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "avfoundation:HD Pro Webcam C920",
                "avfoundation:iPhone Camera",
                "avfoundation:FaceTime HD Camera",
                "avfoundation:Capture screen 0",
                "avfoundation:4",
                "avfoundation:Cam 2",
                "avfoundation:6",
                "avfoundation:7"
            ]
        );
        assert_eq!(devices[7]["name"], "Desk: Left");
        assert_eq!(device_name("avfoundation:2"), "avfoundation:2");
        assert_eq!(
            device_name("dshow:video=Integrated Webcam"),
            "Integrated Webcam"
        );
        let timeout = Duration::from_secs(5);
        let failure = |log| native_failure(log, false, "FaceTime HD Camera", timeout);
        assert_eq!(
            failure("[in#0 @ 0x1] Failed to create AV capture input device: Cannot use FaceTime HD Camera").unwrap(),
            "camera access denied: allow this app in System Settings > Privacy & Security > Camera"
        );
        assert_eq!(
            failure("[in#0 @ 0x1] Failed to create AV capture input device: Cannot Use FaceTime HD Camera").unwrap(),
            "camera is in use by another application"
        );
        assert!(failure("[in#0 @ 0x1] Video device not found").is_none());
        let stalled = native_failure("", true, "FaceTime HD Camera", timeout).unwrap();
        assert_eq!(
            stalled,
            "no frames from FaceTime HD Camera (timed out after 5 s): the camera may be suspended (for example a closed MacBook lid), in use, or blocked by permissions"
        );
        assert_eq!(crate::cli::error_code(&anyhow!(stalled)).1, 4);
    }
}
