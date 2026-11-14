//! `FFmpeg`-backed media inputs and bounded frame forwarding. The optional `FFmpeg`
//! executable is compressed at build time and extracted into a private
//! directory that later processes reuse (`bundled`). No shell is involved in
//! launching decoders or encoders.
mod bundled;
pub(crate) mod copy;
mod encoders;
pub(crate) mod hwdec;
mod learned;
#[cfg(feature = "jpeg")]
mod mjpeg;
mod mkv;
mod native;
#[cfg(target_os = "linux")]
mod pipes;
pub(crate) mod timeline;
mod url;
#[cfg(any(test, target_os = "linux"))]
mod v4l2;

use crate::types::{
    Backend, CameraInfo, Categorized, ErrorKind, Frame, RGB8, RegisterIo, Transport,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
pub(crate) use copy::StreamCopy;
pub(crate) use encoders::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::{
    cmp::Reverse,
    collections::{HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, IoSlice, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
pub(crate) use url::*;

/// The `FFmpeg` build.rs embedded, compressed; empty when the build has none.
const EMBEDDED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/capturefab-ffmpeg.bin"));
const LICENSE: &str = include_str!(concat!(env!("OUT_DIR"), "/capturefab-ffmpeg-license.txt"));
const MAX_FRAME: usize = 256 * 1024 * 1024;
const MAX_LOG: usize = 16 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) struct Executable {
    path: PathBuf,
    /// Probe results kept beside a shared copy, valid for its build and boot.
    probes: Option<PathBuf>,
    /// Held shared while this process may run the shared copy, so another
    /// build's cleanup leaves it in place.
    _in_use: Option<fs::File>,
}
impl Executable {
    /// One whose probes this process alone remembers.
    #[cfg(all(test, unix))]
    const fn external(path: PathBuf) -> Self {
        Self {
            path,
            probes: None,
            _in_use: None,
        }
    }
    /// An `FFmpeg` installed outside the cache, whose probes are kept in the
    /// cache for its build and the boot, as the bundled copy's are: under
    /// its path, size and time, which an upgrade changes.
    fn installed(path: PathBuf) -> Self {
        let probes = fs::metadata(&path).ok().and_then(|meta| {
            use sha2::{Digest as _, Sha256};
            let modified = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            let mut hash = Sha256::new();
            hash.update(path.as_os_str().as_encoded_bytes());
            hash.update(meta.len().to_le_bytes());
            hash.update(modified.as_nanos().to_le_bytes());
            let name = crate::types::hex(&hash.finalize()[..8]);
            Some(
                crate::dirs::cache()?
                    .join("probes")
                    .join(format!("{name}.json")),
            )
        });
        Self {
            path,
            probes,
            _in_use: None,
        }
    }
}
const FFMPEG_NAME: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};
#[cfg(test)]
thread_local! {
    /// A stand-in `FFmpeg` for the current test thread.
    static FAKE_FFMPEG: std::cell::RefCell<Option<Arc<Executable>>> =
        const { std::cell::RefCell::new(None) };
    /// The stand-in writes raw video as it describes it, rather than PPM.
    static FAKE_NATIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// What a test does once a push has looked for the recording's end.
    static PUSH_RACE: std::cell::Cell<Option<fn(&WriterQueue)>> =
        const { std::cell::Cell::new(None) };
}
/// Where the `FFmpeg` this process runs comes from, in the order they are
/// tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FfmpegSource {
    /// `CAPTUREFAB_FFMPEG` names it.
    Override,
    /// Built into this executable, and extracted to the cache on first use.
    Bundled,
    /// An `ffmpeg` in the folder that holds this executable.
    NextToExecutable,
    /// The first `ffmpeg` on PATH.
    Path,
}
impl FfmpegSource {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Override => "CAPTUREFAB_FFMPEG",
            Self::Bundled => "bundled",
            Self::NextToExecutable => "next_to_executable",
            Self::Path => "path",
        }
    }
}
/// The `FFmpeg` to run: `CAPTUREFAB_FFMPEG`, else the one built in, else one
/// beside this executable, else one on PATH. None of them is `ffmpeg_missing`.
pub(crate) fn executable() -> Result<Arc<Executable>> {
    resolve().map(|(exe, _)| exe)
}
fn resolve() -> Result<(Arc<Executable>, FfmpegSource)> {
    #[cfg(test)]
    if let Some(fake) = FAKE_FFMPEG.with_borrow(Clone::clone) {
        return Ok((fake, FfmpegSource::Override));
    }
    if let Some(path) = overridden(std::env::var_os("CAPTUREFAB_FFMPEG"))? {
        return Ok((
            Arc::new(Executable::installed(path)),
            FfmpegSource::Override,
        ));
    }
    // A bundled copy that cannot be extracted anywhere that runs programs,
    // as on a read-only system with a noexec /tmp, leaves an installed one.
    let bundled = bundled().then(|| bundled::extracted(EMBEDDED));
    if let Some(Ok(exe)) = bundled {
        return Ok((exe, FfmpegSource::Bundled));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    match (
        installed(beside.as_deref(), std::env::var_os("PATH").as_deref()),
        bundled,
    ) {
        (Some((path, source)), _) => Ok((Arc::new(Executable::installed(path)), source)),
        (None, Some(Err(error))) => Err(error),
        (None, _) => Err(ffmpeg_missing()),
    }
}
/// The `FFmpeg` `setting` (`CAPTUREFAB_FFMPEG`) names. Empty, as an
/// `Environment=` or `-e` line that names it leaves it, is unset, as for
/// the folder variables; a relative path would run whatever the working
/// folder holds.
fn overridden(setting: Option<std::ffi::OsString>) -> Result<Option<PathBuf>> {
    let Some(path) = setting.filter(|path| !path.is_empty()).map(PathBuf::from) else {
        return Ok(None);
    };
    if !path.is_absolute() {
        return Err(Categorized::new(
            ErrorKind::Usage,
            format!(
                "CAPTUREFAB_FFMPEG must be an absolute path to an FFmpeg executable, not {}",
                path.display()
            ),
        )
        .into());
    }
    Ok(Some(path))
}
fn ffmpeg_missing() -> anyhow::Error {
    Categorized::new(
        ErrorKind::FfmpegMissing,
        "this build has no FFmpeg built in, and none is next to capturefab or on PATH",
    )
    .hint(crate::session::with_environment_hint(
        ffmpeg_hint(),
        "CAPTUREFAB_FFMPEG",
    ))
    .into()
}
/// An `FFmpeg` installed beside the executable, in `beside`, or else in a
/// folder of `path` (PATH). Relative PATH entries are skipped: they would
/// run whatever the working folder holds.
fn installed(
    beside: Option<&std::path::Path>,
    path: Option<&std::ffi::OsStr>,
) -> Option<(PathBuf, FfmpegSource)> {
    let runnable = |file: &std::path::Path| {
        let Ok(meta) = fs::metadata(file) else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        meta.is_file()
    };
    if let Some(file) = beside.map(|dir| dir.join(FFMPEG_NAME))
        && runnable(&file)
    {
        return Some((file, FfmpegSource::NextToExecutable));
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(FFMPEG_NAME))
        .find(|file| runnable(file))
        .map(|file| (file, FfmpegSource::Path))
}
/// Whether this build has `FFmpeg` built in.
pub(crate) const fn bundled() -> bool {
    !EMBEDDED.is_empty()
}
/// What to do about a missing `FFmpeg`, on this OS.
pub(crate) fn ffmpeg_hint() -> String {
    let install = if cfg!(target_os = "macos") {
        "`brew install ffmpeg`"
    } else if cfg!(windows) {
        "`winget install Gyan.FFmpeg`"
    } else {
        "your package manager, such as `sudo apt install ffmpeg`"
    };
    format!(
        "install FFmpeg with {install}, put it next to capturefab, or set CAPTUREFAB_FFMPEG to an FFmpeg executable; Capturefab release downloads have it built in"
    )
}
/// A private file that did not exist before. It is not synced: a copy cut
/// short by a crash fails validation and is written again.
fn new_private_file(path: &std::path::Path, executable: bool) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(options.open(path)?)
}
fn nonce() -> Result<String> {
    crate::types::random_hex(16).context("cannot name the extracted FFmpeg")
}
fn cores() -> usize {
    crate::host::threads(crate::host::cpus())
}
/// `FFmpeg`'s default frame threading runs cores + 1 threads per stream, each
/// holding frames and adding latency.
fn decoder_threads(cores: usize) -> usize {
    cores.clamp(1, 4)
}
/// x264's preset for a recording: faster as the pixel rate per available
/// core rises.
fn x264_preset(width: u32, height: u32, fps: f64, cores: usize) -> &'static str {
    const PRESETS: [&str; 3] = ["veryfast", "superfast", "ultrafast"];
    let per_core = f64::from(width) * f64::from(height) * fps / cores.max(1) as f64;
    let step = if per_core <= 12e6 {
        0
    } else if per_core <= 24e6 {
        1
    } else {
        2
    };
    PRESETS[step]
}
/// Context for a decoder whose `FFmpeg` could not be extracted or started,
/// unlike a source `FFmpeg` reached and failed on. Callers test for it with
/// `error.downcast_ref::<FfmpegUnavailable>()`, never by message text:
/// explained source failures quote `FFmpeg`'s log too.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FfmpegUnavailable;
impl std::fmt::Display for FfmpegUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cannot start FFmpeg")
    }
}
pub(crate) fn command(exe: &Executable) -> Command {
    let mut cmd = Command::new(&exe.path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}
fn bounded_output(exe: &Executable, args: &[String], timeout: Duration) -> Result<(bool, String)> {
    let (success, output, _) = timed_output(exe, args, &[], timeout)?;
    Ok((success, output))
}
/// Run `FFmpeg` with a deadline, feeding it `input` when there is any:
/// whether it succeeded, what it wrote, and whether it ended without an
/// answer, because it ran past `timeout` or something outside it, such as
/// a memory limit's out-of-memory killer, stopped it ([`stopped_from_outside`]).
fn timed_output(
    exe: &Executable,
    args: &[String],
    input: &[u8],
    timeout: Duration,
) -> Result<(bool, String, bool)> {
    let mut command = command(exe);
    if !input.is_empty() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(FfmpegUnavailable)?;
    let deadline = Instant::now() + timeout;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    thread::scope(|scope| {
        // The input is written from a thread of its own: an FFmpeg that
        // stalls before it reads it, such as on a hung device, would block
        // the write. Once FFmpeg is killed the write fails.
        if let Some(mut stdin) = stdin {
            scope.spawn(move || {
                let _ = stdin.write_all(input);
            });
        }
        let reader = |mut input: Box<dyn Read + Send>| {
            scope.spawn(move || {
                let mut bytes = Vec::new();
                let _ = input.by_ref().take(1024 * 1024).read_to_end(&mut bytes);
                bytes
            })
        };
        let out = reader(Box::new(stdout));
        let err = reader(Box::new(stderr));
        let (success, undecided) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (status.success(), stopped_from_outside(status)),
                Ok(None) if Instant::now() < deadline => {}
                // The threads end once FFmpeg does.
                waited => {
                    let _ = child.kill();
                    let _ = child.wait();
                    waited?;
                    break (false, true);
                }
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut bytes = out.join().unwrap_or_default();
        bytes.extend(err.join().unwrap_or_default());
        Ok((
            success,
            String::from_utf8_lossy(&bytes).to_string(),
            undecided,
        ))
    })
}
/// Whether a process that ended with `status` was killed by a signal from
/// outside it (SIGKILL, as the out-of-memory killer sends, or SIGTERM,
/// SIGINT or SIGHUP), which says nothing about what it was doing, unlike
/// a crash of its own such as SIGSEGV or SIGABRT.
fn stopped_from_outside(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        matches!(
            status.signal(),
            Some(libc::SIGKILL | libc::SIGTERM | libc::SIGINT | libc::SIGHUP)
        )
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}
/// The bundled `FFmpeg`'s license notice, with the license texts it is
/// distributed under.
pub(crate) const fn license_notice() -> &'static str {
    LICENSE
}
/// Which `FFmpeg` this process would run, and whether it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FfmpegHealth {
    /// Where it was found: [`FfmpegSource::name`].
    pub source: &'static str,
    pub path: String,
    /// `FFmpeg`'s first `-version` line, such as `ffmpeg version 7.1 ...`.
    pub version: String,
    /// Why the bundled `FFmpeg` is not the one used, when this build has one.
    pub bundled_error: Option<String>,
}
/// Run `FFmpeg`'s `-version` with a 3 s deadline, for `doctor`. Extracting
/// the bundled copy goes to the cache directory, as for any first use.
pub(crate) fn ffmpeg_health() -> Result<FfmpegHealth> {
    let (exe, source) = resolve()?;
    let (ok, output) = bounded_output(&exe, &["-version".into()], Duration::from_secs(3))
        .with_context(|| format!("cannot run {}", exe.path.display()))?;
    let version = output.lines().next().unwrap_or_default().trim().to_owned();
    ensure!(
        ok && version.to_ascii_lowercase().starts_with("ffmpeg version"),
        "{} -version failed{}",
        exe.path.display(),
        if version.is_empty() {
            String::new()
        } else {
            format!(": {version}")
        }
    );
    Ok(FfmpegHealth {
        source: source.name(),
        path: exe.path.display().to_string(),
        version,
        bundled_error: (source != FfmpegSource::Bundled && source != FfmpegSource::Override)
            .then(bundled::refused)
            .flatten(),
    })
}

#[cfg_attr(
    target_os = "linux",
    expect(
        clippy::unnecessary_wraps,
        reason = "macOS and Windows discovery runs FFmpeg and can fail; the platform interface shares one result contract"
    )
)]
pub(crate) fn native_devices_with_timeout(timeout: Duration) -> Result<Value> {
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let _ = timeout;
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + timeout;
        let devices = v4l2_devices(std::path::Path::new("/sys/class/video4linux"), |path| {
            captures_by(deadline, path, v4l2::captures_video)
        });
        Ok(json!({"platform":"linux","devices":devices}))
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
        .map(str::to_owned);
        let (_, listing, timed_out) = timed_output(&exe, &args, &[], timeout)?;
        ensure!(
            !timed_out,
            "native camera discovery timed out after {} s; check camera permissions and host drivers, or retry with a longer list --timeout",
            timeout.as_secs_f64()
        );
        ensure!(
            !listing.contains("Unknown input format"),
            "FFmpeg does not support the {backend} native camera input; use a build containing this host camera backend"
        );
        let devices = if backend == "avfoundation" {
            avfoundation_devices(&listing)
        } else {
            dshow_devices(&listing)
        };
        Ok(json!({"platform":std::env::consts::OS,"devices":devices,"diagnostic":listing}))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err(anyhow!(
            "native camera enumeration is supported on macOS, Linux and Windows"
        ))
    }
}
/// The video capture nodes in the `video4linux` sysfs class, in node order.
/// The class also holds UVC metadata, codec, ISP, subdevice, VBI and radio
/// nodes, which share the camera's name but cannot capture its video.
#[cfg(any(test, target_os = "linux"))]
fn v4l2_devices(
    class: &std::path::Path,
    captures: impl Fn(&std::path::Path) -> bool,
) -> Vec<Value> {
    let mut nodes = fs::read_dir(class)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|node| {
            node.starts_with("video") && captures(std::path::Path::new(&format!("/dev/{node}")))
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| (left.len(), left).cmp(&(right.len(), right)));
    nodes
        .into_iter()
        .map(|node| {
            let name = fs::read_to_string(class.join(&node).join("name")).unwrap_or_default();
            json!({"id":format!("v4l2:/dev/{node}"),"name":name.trim(),"backend":"v4l2"})
        })
        .collect()
}
/// Whether `captures` accepts the node at `path`, or `true` when it has not
/// answered by `deadline`: opening a node can wait on a USB camera resuming
/// from autosuspend.
#[cfg(any(test, target_os = "linux"))]
fn captures_by(
    deadline: Instant,
    path: &std::path::Path,
    captures: fn(&std::path::Path) -> bool,
) -> bool {
    let (sender, receiver) = std::sync::mpsc::channel();
    let path = path.to_owned();
    thread::spawn(move || sender.send(captures(&path)));
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(true)
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
                let (index, name) = line.get(start + 1..)?.split_once("] ")?;
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

/// The video input devices in `FFmpeg`'s `DirectShow` listing. `FFmpeg` lists the
/// video category before the audio one, each device as its friendly name,
/// the media types of its capture pins ("(none)" when it could not read
/// them, as for some virtual cameras) and its moniker ("Alternative name").
/// The moniker selects a device whose friendly name another device shares
/// or `FFmpeg` cannot parse; its `?` is written `%3F`, since `?` starts the
/// locator's options.
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn dshow_devices(listing: &str) -> Vec<Value> {
    const AUDIO_CATEGORY: &str = "{33D9A762-90C8-11D0-BD43-00A0C911CE86}";
    let mut found: Vec<(&str, bool, Option<&str>)> = Vec::new();
    let (mut kept, mut audio) = (false, false);
    for line in listing.lines() {
        let text = line.strip_prefix('[').map_or(line, |rest| {
            rest.split_once("] ").map_or(line, |(_, text)| text)
        });
        if let Some(moniker) = text
            .trim_start()
            .strip_prefix("Alternative name \"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            if kept && let Some(device) = found.last_mut() {
                device.2 = Some(moniker);
            }
        } else if let Some((name, types)) = text
            .strip_prefix('"')
            .and_then(|rest| rest.rsplit_once('"'))
        {
            let types = types.trim().trim_start_matches('(').trim_end_matches(')');
            let none = types == "none";
            kept = types.split(", ").any(|kind| kind == "video") || none && !audio;
            audio |= types == "audio";
            if kept {
                found.push((name, none, None));
            }
        }
    }
    found.retain(|&(_, none, moniker)| {
        !(none
            && moniker.is_some_and(|moniker| moniker.to_ascii_uppercase().contains(AUDIO_CATEGORY)))
    });
    found
        .iter()
        .map(|&(name, _, moniker)| {
            let unique = found.iter().filter(|(other, ..)| *other == name).count() == 1
                && !name.contains([':', '?'])
                && !name.starts_with("@device");
            let id = match moniker {
                Some(moniker) if !unique => moniker.replace('?', "%3F"),
                _ => name.to_owned(),
            };
            json!({"id":format!("dshow:video={id}"),"name":name,"backend":"dshow"})
        })
        .collect()
}

struct Log {
    bytes: Mutex<VecDeque<u8>>,
    /// What `FFmpeg` says it writes, once it has said it, and whether its log
    /// has ended (`collect_lines`).
    output: Mutex<(Option<native::Described>, bool)>,
    described: Condvar,
    /// The input video `FFmpeg` reads, once it has said (`collect_lines`).
    input: Mutex<Option<native::Input>>,
}
impl Log {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(VecDeque::new()),
            output: Mutex::new((None, false)),
            described: Condvar::new(),
            input: Mutex::new(None),
        })
    }
    fn keep(&self, bytes: &[u8]) {
        let mut stored = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stored.extend(bytes);
        let excess = stored.len().saturating_sub(MAX_LOG);
        stored.drain(..excess);
    }
    /// Collect an info-level log a line at a time: its error lines as
    /// `collect` would at the error level, and the description of `FFmpeg`'s
    /// output, which `described` waits for.
    fn collect_lines(self: &Arc<Self>, mut input: impl Read + Send + 'static) -> JoinHandle<()> {
        let log = self.clone();
        thread::spawn(move || {
            let mut lines = native::Lines::default();
            let mut buffer = [0; 4096];
            let mut said = |said| match said {
                native::Said::Error(line) => log.keep(&line),
                native::Said::Input(input) => {
                    *log.input
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(input);
                }
                native::Said::Output(output) => {
                    log.output
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0 = Some(output);
                    log.described.notify_all();
                }
            };
            loop {
                let count = match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                lines.push(&buffer[..count], &mut said);
            }
            lines.finish(&mut said);
            log.output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .1 = true;
            log.described.notify_all();
        })
    }
    /// What `FFmpeg` says it writes, waiting up to `patience` for it to say
    /// so; None when it has not, its log ended first, or the decoder stopped.
    fn wait_described(
        &self,
        stopped: &AtomicBool,
        patience: Duration,
    ) -> Option<native::Described> {
        let deadline = Instant::now() + patience;
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if output.0.is_some()
                || output.1
                || stopped.load(Ordering::Relaxed)
                || Instant::now() >= deadline
            {
                return output.0.clone();
            }
            output = self
                .described
                .wait_timeout(output, Duration::from_millis(100))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
    fn text(&self, source: &str) -> String {
        let bytes = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .collect::<Vec<_>>();
        redact_log(&String::from_utf8_lossy(&bytes), source)
    }
    /// Indexed recording progress reports muxed video packets, not frames
    /// merely handed to the encoder (`FFmpeg`'s `print_report`):
    /// <https://github.com/FFmpeg/FFmpeg/blob/master/fftools/ffmpeg.c>.
    fn completed_video_frames(&self) -> Option<u64> {
        let stored = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = stored.iter().copied().collect::<Vec<_>>();
        let text = String::from_utf8_lossy(&bytes);
        let mut frames = None;
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("frame=") {
                frames = value.trim().parse().ok();
            }
            if line == "progress=end" {
                return frames;
            }
        }
        None
    }
    fn collect(self: &Arc<Self>, mut input: impl Read + Send + 'static) -> JoinHandle<()> {
        let log = self.clone();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            loop {
                let count = match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                log.keep(&buffer[..count]);
            }
        })
    }
}
struct Decoded {
    frames: VecDeque<Frame>,
    error: Option<String>,
    /// Why the frames must be asked for as RGB instead (`NeedsRgb`).
    needs_rgb: Option<String>,
    /// Why the source's JPEGs must be decoded by `FFmpeg` instead
    /// (`NeedsDecoding`).
    needs_decoding: Option<String>,
}
/// What frames a decoder asks `FFmpeg` for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Output {
    /// RGB24, as PPM images: each frame says its size.
    Rgb,
    /// The decoder's own pixel format, as raw video (`native`).
    Native,
    /// A Motion JPEG source's own JPEGs, copied as they arrive and decoded
    /// here, each frame keeping its JPEG (`mjpeg`).
    Jpeg,
}
/// `FFmpeg` would decode the source to frames Capturefab does not carry as
/// they are, or did not say what it writes: the source is opened again
/// for RGB frames.
#[derive(Debug)]
struct NeedsRgb(String);
impl std::fmt::Display for NeedsRgb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NeedsRgb {}
/// The source's JPEGs did not decode here: `FFmpeg` decodes them instead.
#[derive(Debug)]
struct NeedsDecoding(String);
impl std::fmt::Display for NeedsDecoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NeedsDecoding {}
/// The decoder has exited: every later read fails the same way until the
/// source is opened again. The engine counts it as a transport error even when
/// `FFmpeg`'s reason mentions a timeout.
#[derive(Debug)]
pub(crate) struct StreamEnded(pub String);
impl std::fmt::Display for StreamEnded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StreamEnded {}
struct DecoderQueue {
    state: Mutex<Decoded>,
    changed: Condvar,
    stopped: AtomicBool,
}
impl DecoderQueue {
    /// The newest frame decoded so far, without waiting; the older ones
    /// waiting with it are dropped.
    fn newest(&self) -> Option<Frame> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let newest = state.frames.pop_back();
        state.frames.clear();
        drop(state);
        self.changed.notify_all();
        newest
    }
}
struct Decoder {
    child: Child,
    thread: Option<JoinHandle<()>>,
    log_thread: Option<JoinHandle<()>>,
    /// Reads the video it copies for a recording (`copy`), if it does.
    tap_thread: Option<JoinHandle<()>>,
    queue: Arc<DecoderQueue>,
    log: Arc<Log>,
    _exe: Arc<Executable>,
    source: String,
    /// Why `FFmpeg`'s output ended, formatted once with its log.
    failure: Option<String>,
    status: Option<std::process::ExitStatus>,
    /// Its first frame has been looked at (`MediaBackend::observe`).
    observed: bool,
    /// What it asked `FFmpeg` for.
    output: Output,
}
#[deny(clippy::arithmetic_side_effects, clippy::panic_in_result_fn)]
fn ppm_token(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut token = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            filled => filled?,
        };
        ensure!(!available.is_empty(), "media stream ended");
        let byte = available[0];
        reader.consume(1);
        if token.is_empty() && byte == b'#' {
            let mut line = Vec::new();
            // Read only one byte beyond the allowed comment, not an
            // unbounded line from a decoder that may never end it.
            reader.by_ref().take(4097).read_until(b'\n', &mut line)?;
            ensure!(line.len() <= 4096, "PPM comment is too long");
            continue;
        }
        if byte.is_ascii_whitespace() {
            if token.is_empty() {
                continue;
            }
            break;
        }
        token.push(byte);
        ensure!(token.len() <= 32, "PPM header token is too long");
    }
    Ok(token)
}
fn read_ppm(
    reader: &mut impl BufRead,
    id: u64,
    pool: &crate::framebuf::FramePool,
) -> Result<Frame> {
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
    // `read_exact` fills every byte, so a recycled buffer needs no zeroing.
    let mut data = pool.take_stale(size);
    reader
        .read_exact(&mut data)
        .context("truncated FFmpeg RGB frame")?;
    crate::copies::record(crate::copies::Site::PipeRead, size);
    Ok(Frame {
        pixel_encoding: None,
        device_timestamp: None,
        id,
        width,
        height,
        pixel_format: RGB8,
        device_ts_ns: None,
        wall_ms: 0,
        received_monotonic_ns: None,
        chunks: None,
        multipart: None,
        data: data.freeze(),
        jpeg: None,
    })
}
/// Frames from a Motion JPEG source's own JPEGs (`Output::Jpeg`). A JPEG
/// that is not whole or does not decode is skipped; a source none of whose
/// first JPEGs decode goes back to `FFmpeg`'s decoder (`NeedsDecoding`).
#[derive(Default)]
struct JpegReader {
    #[cfg(feature = "jpeg")]
    decompressor: Option<turbojpeg::Decompressor>,
    #[cfg(feature = "jpeg")]
    jpegs: mjpeg::Splitter,
    #[cfg(feature = "jpeg")]
    delivered: bool,
}
impl JpegReader {
    /// How many JPEGs in a row may fail to decode before a source that
    /// delivered none goes back to `FFmpeg`, and one that did ends.
    #[cfg(feature = "jpeg")]
    const FAILURES: [u32; 2] = [3, 30];
    #[cfg(feature = "jpeg")]
    fn next(
        &mut self,
        reader: &mut impl BufRead,
        id: u64,
        pool: &crate::framebuf::FramePool,
    ) -> Result<Frame> {
        let decompressor = match &mut self.decompressor {
            Some(decompressor) => decompressor,
            None => self.decompressor.insert(
                turbojpeg::Decompressor::new()
                    .map_err(|error| NeedsDecoding(format!("libjpeg-turbo: {error}")))?,
            ),
        };
        let mut failures = 0;
        loop {
            let error = match self.jpegs.next(reader) {
                Ok(Some(jpeg)) => match mjpeg::decode(decompressor, jpeg, id, pool) {
                    Ok(frame) => {
                        self.delivered = true;
                        return Ok(frame);
                    }
                    Err(error) => error,
                },
                Ok(None) => bail!("media stream ended"),
                Err(error) if error.is::<mjpeg::Malformed>() => error,
                Err(error) => return Err(error),
            };
            failures += 1;
            if failures >= Self::FAILURES[usize::from(self.delivered)] {
                if self.delivered {
                    return Err(error.context("the source's JPEGs stopped decoding"));
                }
                return Err(NeedsDecoding(format!(
                    "its JPEGs do not decode here ({error:#}); FFmpeg decodes them"
                ))
                .into());
            }
        }
    }
    #[cfg(not(feature = "jpeg"))]
    #[expect(
        clippy::unused_self,
        reason = "the no-JPEG reader preserves the stateful decoder interface while reporting the missing feature"
    )]
    fn next(&self, _: &mut impl BufRead, _: u64, _: &crate::framebuf::FramePool) -> Result<Frame> {
        Err(NeedsDecoding("this build has no JPEG decoder".into()).into())
    }
}
impl Decoder {
    #[cfg(test)]
    fn spawn(source: &str, timeout: Duration) -> Result<Self> {
        Self::spawn_configured(source, timeout, &[], Output::Rgb)
    }
    #[cfg(test)]
    fn spawn_configured(
        source: &str,
        timeout: Duration,
        options: &[String],
        output: Output,
    ) -> Result<Self> {
        Self::spawn_with_output(source, timeout, options, &[], output, None)
    }
    /// `FFmpeg` decoding `source` with input `options`, writing `output`
    /// frames after `output_options`, and with a `tap` also copying the
    /// video it reads to the tap.
    #[deny(clippy::significant_drop_tightening)]
    fn spawn_with_output(
        source: &str,
        timeout: Duration,
        options: &[String],
        output_options: &[String],
        output: Output,
        tap: Option<&Arc<copy::Tap>>,
    ) -> Result<Self> {
        validate_source(source)?;
        ensure!(
            !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
            "media timeout must be positive and fit in the monotonic clock"
        );
        let exe = executable().context(FfmpegUnavailable)?;
        let mut cmd = command(&exe);
        cmd.args(["-nostdin", "-hide_banner"]);
        cmd.args(match output {
            Output::Rgb => &["-loglevel", "error"][..],
            // The output's description is an info line (`native`), and the
            // input's says what a source sends.
            Output::Native | Output::Jpeg => &["-loglevel", "level+info", "-nostats"],
        });
        if !options.iter().any(|option| option == "-threads") {
            cmd.args(["-threads", &decoder_threads(cores()).to_string()]);
        }
        let native = native_input(source)?;
        let input = match &native {
            Some((format, input)) => {
                cmd.args(["-f", format]);
                input.clone()
            }
            None if scheme(source).as_deref() == Some("file") => {
                format!("file:{}", local_file_path(source)?.to_string_lossy())
            }
            None => canonical_scheme(source),
        };
        cmd.args(probing(source));
        cmd.args(options);
        let timeout_us = timeout.as_micros().max(1).to_string();
        let protocol = if matches!(scheme(source).as_deref(), Some("rtsp" | "rtsps")) {
            // RTP interleaved over TCP first. A camera that answers SETUP with
            // 461 Unsupported Transport gets RTP over UDP, the transport ONVIF
            // requires of every camera; FFmpeg keeps rtsps on TCP.
            vec![
                "-rtsp_flags",
                "prefer_tcp",
                "-rtsp_transport",
                "tcp+udp",
                "-timeout",
                &timeout_us,
            ]
        } else if scheme(source)
            .is_some_and(|s| !matches!(s.as_ref(), "file" | "avfoundation" | "v4l2" | "dshow"))
        {
            vec!["-rw_timeout", &timeout_us]
        } else {
            Vec::new()
        };
        // An address with credentials reaches FFmpeg on its stdin, in a
        // concat script, never in its arguments, which every user of the
        // machine can read in the process list.
        let script = (native.is_none() && credentialed(source)).then(|| {
            let options = probing(source).iter().chain(&protocol);
            concat_script(&input, options.copied())
        });
        match &script {
            Some(_) => {
                cmd.args(["-protocol_whitelist", CONCAT_PROTOCOLS])
                    .args(["-f", "concat", "-safe", "0", "-i", "pipe:0"])
                    .stdin(Stdio::piped());
            }
            None => {
                cmd.args(&protocol).args(["-i", &input]);
            }
        }
        cmd.args(output_options).args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-fps_mode",
            "passthrough",
        ]);
        match output {
            Output::Rgb => cmd.args(["-f", "image2pipe", "-c:v", "ppm", "-pix_fmt", "rgb24"]),
            Output::Native => cmd.args([
                "-vf",
                &format!("format=pix_fmts={}", native::PIX_FMTS),
                "-f",
                "rawvideo",
            ]),
            Output::Jpeg => cmd.args(["-c:v", "copy", "-f", "mjpeg"]),
        };
        // One packet per frame, written to the pipe as it is rather than
        // through FFmpeg's I/O buffer.
        cmd.args(["-avioflags", "direct", "pipe:1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The second output, after the frames' (`native` reads the first's
        // description): the video as it arrives, on a pipe of its own.
        #[cfg(unix)]
        let tapped = match tap {
            Some(tap) => Some((tap.clone(), copy::pipe(&mut cmd)?)),
            None => None,
        };
        #[cfg(not(unix))]
        let tapped: Option<(Arc<copy::Tap>, copy::Pipe)> = {
            ensure!(tap.is_none(), "stream copy needs a Unix pipe");
            None
        };
        let mut child = cmd.spawn().context(FfmpegUnavailable)?;
        if let (Some(script), Some(mut stdin)) = (script, child.stdin.take()) {
            // FFmpeg reads it whole before it opens the source; one that
            // exited at once says why in its log.
            let _ = stdin.write_all(script.as_bytes());
        }
        // The child holds the pipe's write end now; this process's copy
        // goes, so the pipe ends when FFmpeg does.
        let tap_thread = tapped.map(|(tap, (reader, writer))| {
            drop(writer);
            thread::spawn(move || tap.read(reader))
        });
        let stdout = child.stdout.take().unwrap();
        let log = Log::new();
        let stderr = child.stderr.take().unwrap();
        let log_thread = match output {
            Output::Rgb => log.collect(stderr),
            Output::Native | Output::Jpeg => log.collect_lines(stderr),
        };
        let queue = Arc::new(DecoderQueue {
            state: Mutex::new(Decoded {
                frames: VecDeque::new(),
                error: None,
                needs_rgb: None,
                needs_decoding: None,
            }),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
        });
        let shared = queue.clone();
        let described = log.clone();
        let live = scheme(source).is_some_and(|s| s != "file");
        #[cfg(target_os = "linux")]
        let mut enlarge = Some({
            use std::os::fd::AsRawFd as _;
            stdout.as_raw_fd()
        });
        let thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            // Raw video is read at the size and format FFmpeg described.
            let native = match output {
                Output::Rgb | Output::Jpeg => None,
                Output::Native => {
                    let fail = |error: Option<String>, needs_rgb: Option<String>| {
                        let mut state = shared
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        (state.error, state.needs_rgb) = (error, needs_rgb);
                        drop(state);
                        shared.changed.notify_all();
                    };
                    // FFmpeg describes its output before it writes the first
                    // frame, though its log may be read a moment later.
                    if reader.fill_buf().map_or(true, <[u8]>::is_empty) {
                        return fail(Some("media stream ended".into()), None);
                    }
                    let output = described.wait_described(&shared.stopped, Duration::from_secs(2));
                    match output.map(|output| (native::frame_format(&output), output)) {
                        Some((Some(format), output)) => Some((output, format)),
                        Some((None, output)) => {
                            let reason = format!(
                                "FFmpeg decodes it to {} {}x{}, which is read as RGB",
                                output.pix_fmt, output.width, output.height
                            );
                            return fail(None, Some(reason));
                        }
                        None if shared.stopped.load(Ordering::Relaxed) => return,
                        None => {
                            let reason = "FFmpeg did not describe its output".to_owned();
                            return fail(None, Some(reason));
                        }
                    }
                }
            };
            let mut id = 0;
            let jpegs = JpegReader::default();
            #[cfg(feature = "jpeg")]
            let mut jpegs = jpegs;
            while !shared.stopped.load(Ordering::Relaxed) {
                id += 1;
                let pool = crate::framebuf::pool();
                let result = match &native {
                    Some((described, format)) => {
                        native::read_frame(&mut reader, id, *format, described, pool)
                    }
                    None if output == Output::Jpeg => match jpegs.next(&mut reader, id, pool) {
                        Err(error) if error.is::<NeedsDecoding>() => {
                            let mut state = shared
                                .state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.needs_decoding = Some(format!("{error:#}"));
                            shared.changed.notify_all();
                            break;
                        }
                        result => result,
                    },
                    None => read_ppm(&mut reader, id, pool),
                };
                // Stamp a frame when it has arrived, not when it is taken.
                let result = result.map(|mut frame| {
                    frame.wall_ms = crate::scheduling::now_ms();
                    frame
                });
                // The pipe holds a frame like this one in one go.
                #[cfg(target_os = "linux")]
                if let (Ok(frame), Some(fd)) = (&result, enlarge.take()) {
                    pipes::enlarge(fd, frame.jpeg.as_ref().unwrap_or(&frame.data).len());
                }
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            tap_thread,
            queue,
            log,
            _exe: exe,
            source: source.into(),
            failure: None,
            status: None,
            observed: false,
            output,
        })
    }
    /// The input video `FFmpeg` said it reads and the format it said it
    /// writes, for raw video output.
    fn facts(&self) -> (Option<native::Input>, Option<String>) {
        let input = self
            .log
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let output = self
            .log
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            input,
            output.0.as_ref().map(|output| output.pix_fmt.clone()),
        )
    }
    fn next(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let queue = self.queue.clone();
        let mut state = queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(frame) = state.frames.pop_front() {
                queue.changed.notify_all();
                return Ok(frame);
            }
            if let Some(reason) = state.needs_rgb.clone() {
                return Err(NeedsRgb(reason).into());
            }
            if let Some(reason) = state.needs_decoding.clone() {
                return Err(NeedsDecoding(reason).into());
            }
            if let Some(error) = state.error.clone() {
                drop(state);
                return Err(StreamEnded(self.fail(&error).to_owned()).into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "media frame capture timed out: {}",
                redact_url(&self.source)
            );
            state = queue
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
    fn newest(&self) -> Option<Frame> {
        self.queue.newest()
    }
    /// Why the stream ended, formatted the first time it is asked for. `FFmpeg`
    /// gets a moment to exit first, so the log it leaves is complete.
    fn fail(&mut self, error: &str) -> &str {
        if self.failure.is_none() {
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                self.status = self.child.try_wait().ok().flatten();
                if self.status.is_some() || Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            if self.status.is_some()
                && let Some(thread) = self.log_thread.take()
            {
                let _ = thread.join();
            }
            let (error, log) = (redact_log(error, &self.source), self.log.text(&self.source));
            self.failure = Some(match log.trim() {
                "" => error,
                log => format!("{error}: {log}"),
            });
        }
        self.failure.as_deref().unwrap_or_default()
    }
    /// `FFmpeg`'s output has ended, as the last frame error reported.
    const fn ended(&self) -> bool {
        self.failure.is_some()
    }
    /// `FFmpeg` exited successfully, for example at the end of a file.
    fn succeeded(&mut self) -> bool {
        if self.status.is_none() {
            self.status = self.child.try_wait().ok().flatten();
        }
        self.status.is_some_and(|status| status.success())
    }
    fn stop(&mut self) {
        // Under the lock, so a reader between seeing it run and waiting for
        // room cannot miss the wake-up.
        let state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.queue.stopped.store(true, Ordering::Relaxed);
        drop(state);
        self.queue.changed.notify_all();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.log_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.tap_thread.take() {
            let _ = thread.join();
        }
    }
    /// It copies the video it reads to a tap.
    const fn tapped(&self) -> bool {
        self.tap_thread.is_some()
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Finite local decoding used by the virtual camera. This shares the media
/// decoder's bounded queue and process lifetime, but distinguishes a clean end
/// of a clip from a damaged input so the simulator can advance its playlist.
pub(crate) struct ReplayDecoder(Decoder);
impl ReplayDecoder {
    pub(crate) fn open(
        path: &std::path::Path,
        width: u32,
        height: u32,
        fps: f64,
        image: bool,
        timeout: Duration,
    ) -> Result<Self> {
        let mut options = vec![
            "-protocol_whitelist".into(),
            "file,pipe".into(),
            "-xerror".into(),
            "-hwaccel".into(),
            "none".into(),
            "-threads".into(),
            "1".into(),
        ];
        if image {
            // Literal still names must not be interpreted as image2 patterns.
            options.extend([
                "-f".into(),
                "image2".into(),
                "-pattern_type".into(),
                "none".into(),
            ]);
        } else {
            // Do not autodetect a renamed playlist or manifest as local video.
            options.extend(["-f".into(), Self::video_format(path)?.into()]);
        }
        let mut output = Self::output(width, height, (!image).then_some(fps));
        if image {
            output.extend(["-frames:v".into(), "1".into()]);
        }
        Self::spawn(path, timeout, &options, &output)
    }
    fn video_format(path: &std::path::Path) -> Result<&'static str> {
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Ok(match extension.as_str() {
            "mp4" | "m4v" | "mov" => "mov",
            "mkv" | "webm" => "matroska",
            "ts" | "mts" | "m2ts" => "mpegts",
            "mpg" | "mpeg" | "vob" => "mpeg",
            "avi" => "avi",
            "flv" => "flv",
            "wmv" => "asf",
            "ogv" => "ogg",
            _ => bail!("unsupported simulation video extension: {extension}"),
        })
    }
    pub(crate) fn sequence(
        manifest: &std::path::Path,
        width: u32,
        height: u32,
        fps: f64,
        count: usize,
        timeout: Duration,
    ) -> Result<Self> {
        let options = [
            "-protocol_whitelist",
            "file,pipe",
            "-xerror",
            "-hwaccel",
            "none",
            "-threads",
            "1",
            "-f",
            "concat",
            "-safe",
            "0",
            "-reinit_filter",
            "0",
        ]
        .map(str::to_owned);
        let mut output = Self::output(width, height, Some(fps));
        output.extend(["-frames:v".into(), count.to_string()]);
        Self::spawn(manifest, timeout, &options, &output)
    }
    fn output(width: u32, height: u32, fps: Option<f64>) -> Vec<String> {
        let mut filter = format!(
            "scale={width}:{height}:force_original_aspect_ratio=decrease:eval=frame,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:eval=frame"
        );
        if let Some(fps) = fps {
            let _ = write!(filter, ",fps={fps}");
        }
        vec!["-vf".into(), filter, "-filter_threads".into(), "1".into()]
    }
    fn spawn(
        path: &std::path::Path,
        timeout: Duration,
        options: &[String],
        output: &[String],
    ) -> Result<Self> {
        let source = path
            .to_str()
            .context("simulation source path is not valid UTF-8")?;
        Decoder::spawn_with_output(source, timeout, options, output, Output::Rgb, None).map(Self)
    }
    pub(crate) fn next(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("simulation frame timeout exceeds the monotonic clock")?;
        loop {
            let mut state = self
                .0
                .queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(frame) = state.frames.pop_front() {
                self.0.queue.changed.notify_all();
                return Ok(Some(frame));
            }
            if let Some(error) = &state.error {
                let error = error.clone();
                drop(state);
                if error == "media stream ended" {
                    if let Some(status) = self.0.child.try_wait()? {
                        if status.success() {
                            return Ok(None);
                        }
                        bail!(
                            "cannot decode simulation media {}: {}",
                            self.0.source,
                            self.0.log.text(&self.0.source).trim()
                        );
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "simulation media decoder did not exit before the frame timeout"
                    );
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                bail!(
                    "cannot decode simulation media {}: {}: {}",
                    self.0.source,
                    error,
                    self.0.log.text(&self.0.source).trim()
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "simulation media frame timed out: {}",
                self.0.source
            );
            let _ = self
                .0
                .queue
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
/// How much of an input `FFmpeg` reads before it decodes, which for a live
/// source is how long the first frame waits. By default `FFmpeg` reads up to
/// 5 s of a stream to measure its frame rate, which frames passed through
/// as they come never need: a camera on this computer sends whole frames,
/// whose first says all there is to know, and a network stream says its
/// codec and size within half a second, the rest of the wait being for its
/// next keyframe. Packets read while probing are kept (no `-fflags
/// nobuffer`), so the keyframe they hold is decoded rather than waited for
/// again. Files are read from the disk at once, and probed in full.
fn probing(source: &str) -> &'static [&'static str] {
    if is_host_camera(source) {
        &["-probesize", "32", "-analyzeduration", "100000"]
    } else if is_live_stream(source) {
        &["-probesize", "500000", "-analyzeduration", "500000"]
    } else {
        &[]
    }
}
/// An http(s) URL naming a video file rather than a live stream.
fn is_video_file_url(source: &str) -> bool {
    const VIDEO: [&str; 10] = [
        "mp4", "m4v", "mov", "mkv", "webm", "avi", "mpg", "mpeg", "wmv", "ogv",
    ];
    let path = source.split(['?', '#']).next().unwrap_or_default();
    matches!(scheme(source).as_deref(), Some("http" | "https"))
        && path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(_, extension)| {
                VIDEO
                    .iter()
                    .any(|video| extension.eq_ignore_ascii_case(video))
            })
}
/// A network source that sends as it goes rather than a file it serves.
fn is_live_stream(source: &str) -> bool {
    match scheme(source).as_deref() {
        Some("rtsp" | "rtsps" | "srt" | "rtmp" | "rtmps" | "udp" | "tcp" | "rtp") => true,
        Some("http" | "https") => !is_video_file_url(source),
        _ => false,
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
                v4l2_path(path),
                "V4L2 URI must point to /dev/videoN or a /dev/v4l/by-id or /dev/v4l/by-path link"
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
            ("dshow", dshow_input(name))
        } else {
            return Ok(None);
        },
    ))
}
/// `FFmpeg`'s `DirectShow` input for a `dshow:` locator, with the `?` restored
/// in a moniker as `dshow_devices` lists it.
fn dshow_input(name: &str) -> String {
    if name.starts_with("video=@device") {
        name.replace("%3F", "?")
    } else {
        name.into()
    }
}
/// A V4L2 device node, or a persistent link udev makes to one, which stays
/// bound to the same camera when node numbers change across reboots and
/// replugs.
fn v4l2_path(path: &str) -> bool {
    path.starts_with("/dev/video")
        || ["/dev/v4l/by-id/", "/dev/v4l/by-path/"]
            .iter()
            .any(|links| {
                path.strip_prefix(links)
                    .is_some_and(|name| !matches!(name, "" | "." | "..") && !name.contains('/'))
            })
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
    input_format: Option<usize>,
    /// The `DirectShow` frame intervals, in 100 ns, to request at the top and
    /// bottom of `fps`: `FFmpeg` only opens a capability whose interval range
    /// holds the interval it derives from `-framerate`.
    intervals: Option<(u32, u32)>,
    /// The smallest size of a `DirectShow` capability that takes any size up
    /// to this mode's, and the lowest rate it takes them at.
    min_size: Option<((u32, u32), f64)>,
}
impl Mode {
    const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn fits(&self, (width, height): (u32, u32)) -> bool {
        self.size() == (width, height)
            || self.min_size.is_some_and(|((low_width, low_height), _)| {
                (low_width..=self.width).contains(&width)
                    && (low_height..=self.height).contains(&height)
            })
    }
    fn honors(&self, fps: f64) -> bool {
        self.fps
            .is_none_or(|(low, high)| fps > low - 0.01 && fps < high + 0.01)
    }
    /// Whether this mode runs at `fps` at `size`, which it fits.
    fn honors_at(&self, size: (u32, u32), fps: f64) -> bool {
        self.honors(fps)
            && (size == self.size() || self.min_size.is_none_or(|(_, low)| fps > low - 0.01))
    }
    fn label(&self) -> String {
        match self.fps {
            Some((_, high)) => format!("{}x{}@{high}", self.width, self.height),
            None => format!("{}x{}", self.width, self.height),
        }
    }
    fn rank(&self) -> (bool, u64, u64, u64, u32) {
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "floating-point rate scores retain Rust's saturating float-to-integer behavior"
        )]
        let fps = self
            .fps
            .map_or(0, |(_, high)| (high * 100.0).round() as u64);
        let area = u64::from(self.width) * u64::from(self.height);
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
                .map(|((a, high), (b, _))| (a.min(b), high));
            m.intervals = m
                .intervals
                .zip(mode.intervals)
                .map(|((a, c), (b, d))| (a.max(b), c.max(d)));
            // Keep only sizes and rates one of the capabilities takes: the
            // wider of two nested ranges, or what two crossed ones share.
            m.min_size = match (m.min_size, mode.min_size) {
                (Some((a, low)), Some((b, other))) => Some(if a.0 <= b.0 && a.1 <= b.1 {
                    (a, low)
                } else if b.0 <= a.0 && b.1 <= a.1 {
                    (b, other)
                } else {
                    ((a.0.max(b.0), a.1.max(b.1)), low.min(other))
                }),
                (a, b) => a.or(b),
            };
        }
        None => modes.push(mode),
    }
    modes
}
/// The modes `FFmpeg`'s `AVFoundation` input can open, from its listing of each
/// format's frame rate ranges. It makes the last format of the requested
/// size active and applies the range of any same-size format whose maximum
/// is the requested rate, which the active format accepts when one of its
/// own ranges holds it.
fn avfoundation_modes(log: &str) -> Vec<Mode> {
    type Format = ((u32, u32), Vec<(f64, f64)>);
    let mut formats: Vec<Format> = Vec::new();
    for (size, range) in log.lines().filter_map(|line| {
        let (head, tail) = line.trim_end().split_once("@[")?;
        let size = dimensions(head.rsplit(' ').next()?)?;
        let (low, high) = tail.strip_suffix("]fps")?.split_once(' ')?;
        Some((size, (rate(low)?, rate(high)?)))
    }) {
        // A format lists its ranges from the fastest down.
        match formats.last_mut() {
            Some((last, ranges))
                if *last == size && ranges.last().is_some_and(|r| r.1 > range.1) =>
            {
                ranges.push(range);
            }
            _ => formats.push((size, vec![range])),
        }
    }
    let mut modes: Vec<Mode> = Vec::new();
    for (index, (size, ranges)) in formats.iter().enumerate() {
        if formats[index + 1..].iter().any(|(other, _)| other == size) {
            continue;
        }
        let earlier = formats[..index]
            .iter()
            .filter(|(other, _)| other == size)
            .flat_map(|(_, earlier)| earlier.iter().map(|range| range.1))
            .filter(|&high| {
                ranges
                    .iter()
                    .any(|&(low, top)| high > low - 0.01 && high < top + 0.01)
            });
        for high in ranges.iter().map(|range| range.1).chain(earlier) {
            if !modes.iter().any(|m| m.size() == *size && m.honors(high)) {
                modes.push(Mode {
                    width: size.0,
                    height: size.1,
                    fps: Some((high, high)),
                    input_format: None,
                    intervals: None,
                    min_size: None,
                });
            }
        }
    }
    modes
}
fn dshow_modes(log: &str) -> Vec<Mode> {
    log.lines()
        .filter_map(|line| {
            let (min, max) = line.split_once(" min s=")?.1.split_once(" max s=")?;
            fn caps(text: &str) -> Option<(&str, &str)> {
                let (size, fps) = text.split_once(" fps=")?;
                Some((size, fps.split([' ', '(']).next()?))
            }
            let ((smallest, slowest), (size, fastest)) = (caps(min)?, caps(max)?);
            let (low, high) = (rate(slowest)?, rate(fastest)?);
            let (width, height) = dimensions(size)?;
            let (fast, slow) = (dshow_intervals(fastest), dshow_intervals(slowest));
            let intervals = if fastest == slowest {
                dshow_nominal(&fast).map(|interval| (interval, interval))
            } else {
                fast.last().copied().zip(slow.first().copied())
            };
            Some(Mode {
                width,
                height,
                fps: Some((low.min(high), high)),
                input_format: None,
                intervals: intervals.filter(|(fast, slow)| fast <= slow),
                min_size: dimensions(smallest)
                    .filter(|&(low_width, low_height)| {
                        (low_width, low_height) != (width, height)
                            && low_width <= width
                            && low_height <= height
                    })
                    .map(|smallest| (smallest, low.min(high))),
            })
        })
        .fold(Vec::new(), merge)
}
/// A positive device interval, truncated to whole 100 ns ticks.
#[deny(clippy::cast_precision_loss)]
fn dshow_interval(value: f64) -> Option<u32> {
    u32::try_from(crate::types::integral_float(value.floor())?)
        .ok()
        .filter(|&ticks| ticks > 0)
}

/// The frame intervals, in 100 ns, that `FFmpeg`'s `DirectShow` listing prints
/// as the rate `text`, in ascending order. It prints `1e7 / interval` to six
/// significant digits, so 24 fps can stand for 416666 or 416667. Taking the
/// largest at the top of a range and the smallest at its bottom stays inside
/// it.
#[deny(clippy::cast_precision_loss)]
fn dshow_intervals(text: &str) -> Vec<u32> {
    let six = |fps: f64| format!("{fps:.5e}").parse::<f64>().ok();
    let Some(fps) = text
        .parse::<f64>()
        .ok()
        .filter(|fps| fps.is_finite() && *fps >= 0.1)
    else {
        return Vec::new();
    };
    let center = 1e7 / fps;
    let spread = (center * 6e-6).ceil() + 2.0;
    let (Some(low), Some(high)) = (
        dshow_interval((center - spread).max(1.0)),
        dshow_interval(center + spread),
    ) else {
        return Vec::new();
    };
    let advertised = six(fps);
    (low..=high)
        .filter(|&interval| six(1e7 / f64::from(interval)) == advertised)
        .collect()
}
/// The interval a device with a single listed rate most likely uses: the
/// whole or NTSC rate it approximates, truncated to 100 ns as UVC cameras
/// (15 fps as 666666) and Blackmagic cards (60 fps as 166666) do.
#[deny(clippy::cast_precision_loss)]
fn dshow_nominal(intervals: &[u32]) -> Option<u32> {
    let &first = intervals.iter().find(|&&ticks| ticks > 0)?;
    let fps = 1e7 / f64::from(first);
    let whole = fps.round().max(1.0);
    let ntsc = (fps * 1.001).round() / 1.001;
    let nominal = 1e7
        / if (whole - fps).abs() <= (ntsc - fps).abs() {
            whole
        } else {
            ntsc
        };
    if let Some(truncated) = dshow_interval(nominal)
        && intervals.contains(&truncated)
    {
        return Some(truncated);
    }
    intervals
        .iter()
        .copied()
        .filter(|&ticks| ticks > 0)
        .min_by(|a, b| {
            (f64::from(*a) - nominal)
                .abs()
                .total_cmp(&(f64::from(*b) - nominal).abs())
        })
}
/// Modes from `FFmpeg`'s V4L2 format listing, with the frame rate bounds
/// `rates` reports for a format's listed description and a size.
fn v4l2_modes(
    log: &str,
    rates: impl Fn(&str, (u32, u32)) -> Option<(f64, f64)>,
) -> (Vec<Mode>, Vec<String>) {
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
            let (name, description) = (fields.next()?.trim(), fields.next()?.trim());
            let mut sizes = Vec::new();
            for size in fields.next()?.split_whitespace().filter_map(dimensions) {
                if !sizes.contains(&size) {
                    sizes.push(size);
                }
            }
            (name != "Unsupported" && !sizes.is_empty()).then_some((
                compressed,
                name,
                description,
                sizes,
            ))
        })
        .collect::<Vec<_>>();
    // MJPEG frames decode independently, without H.264's inter-frame delay.
    formats.sort_by_key(|&(compressed, name, ..)| (compressed, name != "mjpeg"));
    let mut names = Vec::new();
    let mut modes: Vec<Mode> = Vec::new();
    for (_, name, description, sizes) in formats {
        let input_format = Some(names.len());
        names.push(name.to_owned());
        for (width, height) in sizes {
            let fps = rates(description, (width, height))
                .map(|(low, high)| (rounded(low), rounded(high)))
                .filter(|&(_, high)| high > 0.0);
            // Prefer a raw format when the same size is available in both,
            // and keep a compressed one where it reaches a higher frame rate:
            // USB 2.0 webcams offer HD sizes in YUYV at 5-10 fps but in MJPEG
            // at 30.
            let faster = modes
                .iter()
                .filter(|mode| mode.size() == (width, height))
                .all(|mode| {
                    mode.fps
                        .zip(fps)
                        .is_some_and(|((_, kept), (_, high))| high > kept)
                });
            if faster {
                modes.push(Mode {
                    width,
                    height,
                    fps,
                    input_format,
                    intervals: None,
                    min_size: None,
                });
            }
        }
    }
    (modes, names)
}
/// The frame rate bounds the driver reports for a format, by its description
/// in `FFmpeg`'s listing, and a size; `FFmpeg` lists only the sizes.
#[cfg(target_os = "linux")]
fn v4l2_rates(path: &str) -> impl Fn(&str, (u32, u32)) -> Option<(f64, f64)> {
    let formats = v4l2::Formats::open(std::path::Path::new(path)).ok();
    move |description, (width, height)| formats.as_ref()?.frame_rates(description, width, height)
}
#[cfg(not(target_os = "linux"))]
fn v4l2_rates(_: &str) -> impl Fn(&str, (u32, u32)) -> Option<(f64, f64)> {
    |_, _| None
}

fn probe(source: &str) -> (Vec<Mode>, Vec<String>) {
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
        .map(ToString::to_string)
        .chain(["-i".into(), input.clone()])
        .collect::<Vec<_>>();
    let Ok((_, log)) = executable().and_then(|exe| bounded_output(&exe, &args, PROBE_TIMEOUT))
    else {
        return Default::default();
    };
    match format {
        "avfoundation" => (avfoundation_modes(&log), Vec::new()),
        "v4l2" => v4l2_modes(&log, v4l2_rates(&input)),
        _ => (dshow_modes(&log), Vec::new()),
    }
}
/// How a host camera's connect error begins when the system refused access.
pub(crate) const CAMERA_ACCESS_DENIED: &str = "camera access denied";

fn native_failure(
    log: &str,
    stalled: bool,
    source: &str,
    timeout: Duration,
) -> Option<(ErrorKind, String)> {
    let lower = log.to_ascii_lowercase();
    let platform = scheme(source);
    if log.contains("Cannot use ")
        || [
            "permission denied",
            "access denied",
            "operation not permitted",
            "0x80070005",
        ]
        .iter()
        .any(|text| lower.contains(text))
    {
        let guidance = match platform.as_deref() {
            Some("v4l2") => {
                "check access to the video node and membership in its owning group (commonly video)"
            }
            Some("dshow") => {
                "allow desktop apps to access the camera in Windows Settings > Privacy & security > Camera"
            }
            _ => "allow this app in System Settings > Privacy & Security > Camera",
        };
        Some((
            ErrorKind::PermissionDenied,
            format!("{CAMERA_ACCESS_DENIED}: {guidance}"),
        ))
    } else if platform.as_deref() == Some("dshow") && log.contains("Could not run graph") {
        // DirectShow reports a device another app holds and one Windows
        // camera privacy blocks alike, without the underlying error.
        Some((
            ErrorKind::CameraBusy,
            format!(
                "camera is in use by another application, or {CAMERA_ACCESS_DENIED} by Windows privacy settings: close other apps using the camera, or allow desktop apps to access the camera in Windows Settings > Privacy & security > Camera"
            ),
        ))
    } else if log.contains("Cannot Use ")
        || ["device or resource busy", "already in use"]
            .iter()
            .any(|text| lower.contains(text))
    {
        Some((
            ErrorKind::CameraBusy,
            "camera is in use by another application".into(),
        ))
    } else if log.contains("Not a video capture device.")
        // ENOTTY, as glibc and musl word it.
        || ["Inappropriate ioctl for device", "Not a tty"]
            .iter()
            .any(|text| log.contains(&format!("ioctl(VIDIOC_G_INPUT): {text}")))
    {
        Some((
            ErrorKind::Unsupported,
            format!(
                "video capture is not supported by {}, which may be a metadata, codec or ISP node; run capturefab list to see capture nodes",
                device_name(source)
            ),
        ))
    } else if [
        "video device not found",
        "could not find video device",
        "no such file or directory",
    ]
    .iter()
    .any(|text| lower.contains(text))
    {
        Some((
            ErrorKind::CameraNotFound,
            format!(
                "camera device not found: {}; reconnect the device and run capturefab list to obtain its current locator",
                device_name(source)
            ),
        ))
    } else {
        let guidance = if platform.as_deref() == Some("avfoundation") {
            "the camera may be suspended (for example a closed MacBook lid), in use, or blocked by permissions"
        } else {
            "check camera permissions, whether another app is using it, and the selected video mode"
        };
        stalled.then(|| {
            (
                ErrorKind::FrameTimeout,
                format!(
                    "no frames from {} (timed out after {} s): {guidance}",
                    device_name(source),
                    timeout.as_secs_f64()
                ),
            )
        })
    }
}

/// The scheme, host, explicit port and path of a network source, without
/// its credentials or query.
fn network_parts(source: &str) -> Option<(String, String, Option<u16>, String)> {
    let (scheme, rest) = source.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host, port.parse().ok())
        }
        _ => (host_port, None),
    };
    let path = tail.split(['?', '#']).next().unwrap_or_default();
    (!host.is_empty()).then(|| {
        (
            scheme.to_ascii_lowercase(),
            host.to_owned(),
            port,
            path.to_owned(),
        )
    })
}

/// Why a network stream failed to open, in one sentence with its likely
/// fix, from `FFmpeg`'s log of the attempt. `timed_out` is when no frame came
/// within `timeout` while `FFmpeg` still ran. The category is decided here, so
/// exit codes no longer depend on `FFmpeg`'s wording: refused, unresolved,
/// unreachable and missing streams are `unavailable`, rejected credentials
/// `operation_failed` (as before: asking again cannot fix them) and silence
/// `timeout`. None keeps the error `FFmpeg` gave, for logs this does not know.
fn network_failure(
    source: &str,
    log: &str,
    timed_out: bool,
    timeout: Duration,
) -> Option<anyhow::Error> {
    // Parsed from the redacted URL, so the path suggested back never
    // carries an RTMP stream key or a query token.
    let url = redact_url(source);
    let (scheme, host, port, path) = network_parts(&url)?;
    let at = port.map_or_else(|| host.clone(), |port| format!("{host}:{port}"));
    // The first log line saying one of `texts`, which follows the
    // explanation as FFmpeg's own words.
    let line = |texts: &[&str]| {
        log.lines().find(|line| {
            let line = line.to_ascii_lowercase();
            texts.iter().any(|text| line.contains(text))
        })
    };
    let (kind, message, said) = if let Some(said) =
        line(&["connection refused", "actively refused"])
    {
        let hint = if scheme == "rtsp" && port.is_none() {
            " RTSP normally uses port 554."
        } else {
            ""
        };
        (
            ErrorKind::Unavailable,
            format!(
                "Nothing accepted a connection at {at} ({url}). Check the camera's stream port and that streaming is turned on.{hint}"
            ),
            Some(said),
        )
    } else if let Some(said) = log.lines().find(|line| refused(line)) {
        (
            ErrorKind::OperationFailed,
            format!(
                "{host} rejected the username or password ({url}). Check them, or add them as {scheme}://USER:PASS@{at}{path} if none were given."
            ),
            Some(said),
        )
    } else if let Some(said) = line(&["404 not found", "failed: 404", "stream not found"]) {
        let path = if path.is_empty() { "/" } else { path.as_str() };
        (
            ErrorKind::Unavailable,
            format!(
                "{at} answered but has no stream at {path} ({url}). Check the path in the camera's settings, or use onvif://USER:PASS@{host}/onvif/device_service to look it up."
            ),
            Some(said),
        )
    } else if let Some(said) = line(&[
        "failed to resolve hostname",
        "nodename nor servname",
        "name or service not known",
        "no such host",
        "temporary failure in name resolution",
    ]) {
        (
            ErrorKind::Unavailable,
            format!("Cannot resolve {host} ({url}). Check the address."),
            Some(said),
        )
    } else if let Some(said) = line(&["no route to host", "network is unreachable", "host is down"])
    {
        let hint = host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .ok()
            .filter(|_| said.to_ascii_lowercase().contains("no route to host"))
            .and_then(|ip| {
                crate::network::local_network_hint(
                    &std::io::Error::from(std::io::ErrorKind::HostUnreachable),
                    Some(ip),
                )
            })
            .map(|hint| format!(" {hint}."))
            .unwrap_or_default();
        (
            ErrorKind::Unavailable,
            format!("{host} is not reachable from this computer ({url}).{hint}"),
            Some(said),
        )
    } else if let Some(said) = line(&["connection timed out", "operation timed out"]) {
        (
            ErrorKind::Timeout,
            format!(
                "No reply from {at} ({url}). Check it is on this network and streaming{}.",
                longer_wait(timeout)
                    .map(|ms| format!(
                        ", or retry with a longer wait (capture and record take --timeout {}s)",
                        ms / 1000
                    ))
                    .unwrap_or_default()
            ),
            Some(said),
        )
    } else if timed_out && log.trim().is_empty() {
        // FFmpeg logs only errors, so silence is also a stream that
        // connected and is slow to send its first frame, such as a long
        // GOP waiting for a keyframe.
        (
            ErrorKind::Timeout,
            format!(
                "No video arrived from {at} within {} s ({url}). It may be unreachable or slow to send its first frame; {}.",
                timeout.as_secs_f64(),
                longer_wait(timeout).map_or_else(
                    || "check it is on this network and streaming".to_owned(),
                    |ms| format!(
                        "retry with a longer wait (capture and record take --timeout {}s)",
                        ms / 1000
                    )
                )
            ),
            None,
        )
    } else {
        // Anything else keeps FFmpeg's own error.
        return None;
    };
    let detail = said
        .map(ffmpeg_words)
        .filter(|said| !said.is_empty())
        .map(|said| format!(" (FFmpeg: {said})"))
        .unwrap_or_default();
    Some(Categorized::error(kind, format!("{message}{detail}")))
}
/// A longer timeout in milliseconds to suggest, while one up to the 60 s
/// limit is left to try: double the wait, and at least 10 s. The advice
/// names the flag in parentheses, after words any caller reads.
fn longer_wait(timeout: Duration) -> Option<u128> {
    let current = timeout.as_millis();
    let suggested = (current * 2).clamp(10_000, 60_000);
    (suggested > current).then_some(suggested)
}
/// A log line without `FFmpeg`'s internals: its `[tcp @ 0x…]` component
/// prefix and the `?timeout=` Capturefab passed, with any URL redacted.
fn ffmpeg_words(line: &str) -> String {
    let mut line = line.trim();
    while let Some((_, rest)) = line
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .filter(|(component, _)| component.contains(" @ 0x"))
    {
        line = rest.trim_start();
    }
    line.split(' ')
        .map(|word| {
            let word = word
                .split_once("?timeout=")
                .map_or(word, |(before, _)| before);
            if word.contains("://") {
                redact_url(word)
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug, Default)]
struct NativeOptions {
    size: Option<(u32, u32)>,
    fps: Option<f64>,
}
/// Whether `selector` names a camera the operating system attaches to this
/// computer (`avfoundation:`, `v4l2:` or `dshow:`), such as a webcam, rather
/// than a network stream, a file or a machine-vision camera.
pub(crate) fn is_host_camera(selector: &str) -> bool {
    matches!(
        scheme(selector).as_deref(),
        Some("avfoundation" | "v4l2" | "dshow")
    )
}
/// Whether the host camera a recent listing found as `info` is still the
/// device its ID opens. Webcams listed by name keep it; a Linux video node
/// still has the name it was listed with, as one renumbered after a replug
/// may not; a webcam listed by its index is listed again.
pub(crate) fn host_camera_unchanged(info: &CameraInfo) -> bool {
    let Some((backend, device)) = info.id.split_once(':') else {
        return false;
    };
    match backend {
        "avfoundation" => !device.is_empty() && !device.bytes().all(|b| b.is_ascii_digit()),
        "dshow" => true,
        "v4l2" => device
            .strip_prefix("/dev/")
            .filter(|node| node.starts_with("video") && !node.contains('/'))
            .and_then(|node| fs::read_to_string(format!("/sys/class/video4linux/{node}/name")).ok())
            .is_some_and(|name| name.trim() == info.model),
        _ => false,
    }
}
fn native_source(source: &str) -> Result<(String, Option<NativeOptions>)> {
    if !is_host_camera(source) {
        return Ok((source.into(), None));
    }
    let (base, query) = source.split_once('?').unwrap_or((source, ""));
    ensure!(
        !base.split_once(':').unwrap().1.trim().is_empty(),
        "native media source needs a device name or index"
    );
    ensure!(
        !(base.ends_with('\\') && query.starts_with('\\')),
        "write the ? of a DirectShow alternative name as %3F, as capturefab list shows it"
    );
    let mut options = NativeOptions::default();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = pair
            .split_once('=')
            .context("native camera options need key=value")?;
        match key {
            "fps" => {
                ensure!(
                    options.fps.is_none(),
                    "native camera fps option was supplied more than once"
                );
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
                ensure!(
                    options.size.is_none(),
                    "native camera size option was supplied more than once"
                );
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
    formats: Vec<String>,
    /// Set while a network stream is being reopened after `FFmpeg` exited.
    reconnect: Option<Reconnect>,
    /// A video file plays at its own frame rate, like a camera, unless
    /// `PlaybackSpeed` asks for every frame as fast as it decodes.
    paced: bool,
    /// Whether this acquisition's first frame was asked for on demand, by a
    /// capture, rather than taken from a running stream; unknown until then.
    on_demand: Option<bool>,
    /// A capture asked a paced file for frames while the frame it opened
    /// with was still there: that frame is taken, and the file starts again
    /// unpaced only if another is asked for.
    unpace: bool,
    /// The frames asked of `FFmpeg`: decoded as they are, until the source
    /// turns out to need RGB.
    output: Output,
    /// The last frame's pixel format, for the `PixelFormat` register.
    pixel_format: u32,
    /// The hardware decoding method of the running decoder, if any.
    hwdec: Option<&'static str>,
    /// A launch with hardware decoding failed: this source decodes in
    /// software from now on.
    software: bool,
    /// The video `FFmpeg` last said the source has.
    known: Option<native::Input>,
    /// A Motion JPEG source is read as its own JPEGs (`Output::Jpeg`) until
    /// they turn out not to decode here.
    passthrough: bool,
    /// An earlier process found this source to send Motion JPEG
    /// ([`learned`]): its JPEGs are asked for from the first launch.
    jpeg_before: bool,
    /// The source's video as it arrives, for a recording that copies it:
    /// every `FFmpeg` it starts copies it here until the camera stops with no
    /// recording attached.
    tap: Option<Arc<copy::Tap>>,
}
/// How a media source's stream ended, for callers that act on the end rather
/// than on the wording of its error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    /// `FFmpeg` exited and the network source is being reopened. The message
    /// says "timed out", so a caller waiting for a frame keeps waiting.
    Lost,
    /// A video file played to its end.
    Finished,
    /// The server rejected the credentials of several reopenings in a row.
    Abandoned,
}
#[derive(Debug)]
struct Ended {
    end: StreamEnd,
    message: String,
}
impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Ended {}
/// The stream end a frame error reports, if it reports one.
pub(crate) fn stream_end(error: &anyhow::Error) -> Option<StreamEnd> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Ended>())
        .map(|ended| ended.end)
}
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Launches in a row whose credentials the server rejects before
/// reconnecting stops.
const MAX_REFUSED: u32 = 5;
/// A stream that stays up this long ends its outage; one that drops again
/// sooner keeps backing off.
const STEADY: Duration = Duration::from_secs(10);
/// Reopening a network stream, with backoff from 1 s to 30 s between launches.
struct Reconnect {
    /// The launch in progress or due next, from 1 in each outage.
    attempt: u32,
    retry_at: Instant,
    backoff: Duration,
    /// Launches in a row that ended with rejected credentials.
    refused: u32,
    /// When frames came back, while the stream has not yet stayed up for
    /// `STEADY`.
    resumed: Option<Instant>,
}
impl Reconnect {
    fn new() -> Self {
        Self {
            attempt: 1,
            retry_at: Instant::now() + RECONNECT_BACKOFF,
            backoff: RECONNECT_BACKOFF * 2,
            refused: 0,
            resumed: None,
        }
    }
    fn retry_later(&mut self) {
        self.attempt += 1;
        self.retry_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }
    fn waiting(&self, source: &str) -> anyhow::Error {
        Categorized::error(
            ErrorKind::Timeout,
            format!(
                "reconnecting to {} (attempt {}): timed out",
                redact_url(source),
                self.attempt
            ),
        )
    }
}
/// `FFmpeg`'s wording when the server rejects the credentials, which asking
/// again cannot fix. A missing stream path can: a relay such as `MediaMTX`
/// answers 404 while its camera is away.
fn refused(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    ["401 ", "403 ", "unauthorized", "forbidden"]
        .iter()
        .any(|text| reason.contains(text))
}
impl MediaBackend {
    /// Network streams are reopened when `FFmpeg` exits; files and local
    /// devices report their end instead.
    fn reconnects(&self) -> bool {
        scheme(&self.source)
            .is_some_and(|s| !matches!(s.as_ref(), "file" | "avfoundation" | "v4l2" | "dshow"))
    }
    /// The next frame within `timeout`. A network stream whose `FFmpeg` exited
    /// is relaunched with backoff, and the wait for the next launch fits in
    /// `timeout`, so a caller polling for frames never spins.
    fn decode(&mut self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let (reconnects, file, clip) = (self.reconnects(), self.file(), self.clip());
        loop {
            if let Some(decoder) = self.decoder.as_mut() {
                let error = match decoder.next(deadline.saturating_duration_since(Instant::now())) {
                    Ok(frame) => {
                        let facts = (!decoder.observed).then(|| {
                            decoder.observed = true;
                            decoder.facts()
                        });
                        if let Some(facts) = facts {
                            self.observe(facts);
                        }
                        self.delivered();
                        return Ok(frame);
                    }
                    Err(error) => error,
                };
                // A stream that came back decoding to another format.
                if self.output == Output::Native
                    && error
                        .chain()
                        .any(<dyn std::error::Error + 'static>::is::<NeedsRgb>)
                {
                    self.output = Output::Rgb;
                    self.decoder = Some(self.spawn()?);
                    continue;
                }
                if error
                    .chain()
                    .any(<dyn std::error::Error + 'static>::is::<NeedsDecoding>)
                {
                    self.passthrough = false;
                    self.decoder = Some(self.spawn()?);
                    continue;
                }
                if !decoder.ended() {
                    return Err(match &self.reconnect {
                        Some(reconnect) if reconnect.resumed.is_none() => {
                            reconnect.waiting(&self.source)
                        }
                        _ => error,
                    });
                }
                if (file || clip) && decoder.succeeded() {
                    let name = if file {
                        local_file_path(&self.source)?.display().to_string()
                    } else {
                        redact_url(&self.source)
                    };
                    return Err(Ended {
                        end: StreamEnd::Finished,
                        message: format!("video file finished: {name}"),
                    }
                    .into());
                }
                if !reconnects {
                    return Err(error);
                }
                self.decoder = None;
                return Err(self.lost(&format!("{error:#}")));
            }
            let reconnect = self.reconnect.as_mut().context("media decoder stopped")?;
            let wait = reconnect.retry_at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
                if Instant::now() < reconnect.retry_at {
                    return Err(reconnect.waiting(&self.source));
                }
            }
            match self.spawn() {
                Ok(decoder) => self.decoder = Some(decoder),
                Err(error) => return Err(self.lost(&format!("{error:#}"))),
            }
        }
    }
    /// A frame arrived: an outage ends once the stream has stayed up for
    /// `STEADY`.
    fn delivered(&mut self) {
        if let Some(reconnect) = &mut self.reconnect
            && reconnect.resumed.get_or_insert_with(Instant::now).elapsed() >= STEADY
        {
            self.reconnect = None;
        }
    }
    /// The network stream's `FFmpeg` exited: schedule the next launch, or
    /// stop once the server has rejected the credentials several times in a
    /// row. A loss is reported when the stream was up, and the backoff of
    /// an outage carries on until the stream stays up.
    fn lost(&mut self, reason: &str) -> anyhow::Error {
        let url = redact_url(&self.source);
        let attempt = match self.reconnect.as_mut() {
            None => self.reconnect.insert(Reconnect::new()).attempt,
            // Frames came back, then stopped before the stream settled.
            Some(reconnect) if reconnect.resumed.is_some() => {
                (reconnect.resumed, reconnect.refused) = (None, 0);
                reconnect.retry_later();
                reconnect.attempt
            }
            Some(reconnect) => {
                reconnect.refused = if refused(reason) {
                    reconnect.refused + 1
                } else {
                    0
                };
                if reconnect.refused >= MAX_REFUSED {
                    let attempts = reconnect.attempt;
                    self.reconnect = None;
                    return Ended {
                        end: StreamEnd::Abandoned,
                        message: format!(
                            "stopped reconnecting to {url} after {attempts} attempts ({reason})"
                        ),
                    }
                    .into();
                }
                reconnect.retry_later();
                return reconnect.waiting(&self.source);
            }
        };
        Ended {
            end: StreamEnd::Lost,
            message: format!(
                "reconnecting to {url} (attempt {attempt}): timed out after the stream ended ({reason})"
            ),
        }
        .into()
    }
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
            options.size.is_none_or(|size| m.fits(size))
                && options
                    .fps
                    .is_none_or(|fps| m.honors_at(options.size.unwrap_or_else(|| m.size()), fps))
        })
        .ok_or_else(|| {
            let (width, height) = options.size.unwrap_or_else(|| {
                best(&self.modes, |_| true).map_or((0, 0), |i| self.modes[i].size())
            });
            self.unsupported(width, height, self.fps)
        })?;
        let index = options.fps.map_or(index, |fps| self.raw_first(index, fps));
        self.select(index, options.fps);
        if let Some(size) = options.size {
            (self.width, self.height) = size;
        }
        Ok(())
    }
    /// The first listed V4L2 mode of the size at `index` that reaches `fps`.
    /// Raw formats are listed first, so this is raw wherever a raw format
    /// delivers the rate, rather than the faster compressed format ranking
    /// prefers.
    fn raw_first(&self, index: usize, fps: f64) -> usize {
        let size = self.modes[index].size();
        self.modes
            .iter()
            .position(|m| m.size() == size && m.honors(fps) && m.input_format.is_some())
            .unwrap_or(index)
    }
    fn select(&mut self, index: usize, fps: Option<f64>) {
        let mode = self.modes[index];
        self.mode = Some(index);
        (self.width, self.height) = mode.size();
        self.fps = mode.fps.map_or_else(
            || fps.unwrap_or(self.fps),
            |(low, high)| fps.map_or(high, |fps| fps.clamp(low, high)),
        );
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
    fn file(&self) -> bool {
        matches!(scheme(&self.source).as_deref(), None | Some("file"))
    }
    /// An http(s) URL naming a video file rather than a live stream: it
    /// finishes like a local file instead of reconnecting.
    fn clip(&self) -> bool {
        is_video_file_url(&self.source)
    }
    fn input_options(&self) -> Vec<String> {
        if self.file() && self.paced && self.on_demand != Some(true) {
            return vec!["-readrate".into(), "1".into()];
        }
        if !self.native {
            return Vec::new();
        }
        let rate = match self.mode.map(|index| self.modes[index]) {
            Some(Mode {
                fps: Some((low, high)),
                intervals: Some((fast, slow)),
                ..
            }) if self.fps >= high || self.fps <= low => {
                // A rate FFmpeg turns back into exactly this interval.
                let interval = if self.fps >= high { fast } else { slow };
                format!("{:.9}", 1e7 / (f64::from(interval) + 0.5))
            }
            _ => self.fps.to_string(),
        };
        let mut options = vec!["-framerate".to_owned(), rate];
        let size = (self.width, self.height);
        if let Some((width, height)) = self
            .mode
            .map(|index| self.modes[index])
            .map(|mode| if mode.fits(size) { size } else { mode.size() })
            .or_else(|| self.requested_size.then_some(size))
        {
            options.extend(["-video_size".into(), format!("{width}x{height}")]);
        }
        let format = self
            .mode
            .and_then(|index| self.modes[index].input_format)
            .and_then(|index| self.formats.get(index))
            .or_else(|| self.formats.first());
        if let Some(format) = format {
            options.extend(["-input_format".into(), format.clone()]);
        }
        if self.source.starts_with("avfoundation:") {
            options.extend(["-pixel_format".into(), "uyvy422".into()]);
        }
        options
    }
    /// `FFmpeg` decoding the source as it is set up now, in hardware where
    /// `hwdec` chooses it.
    fn spawn(&mut self) -> Result<Decoder> {
        let mut options = self.input_options();
        self.hwdec = None;
        if let Some(source) = self.hwdec_source() {
            let exe = executable().context(FfmpegUnavailable)?;
            let known = self.known.as_ref().map(|input| {
                let class = hwdec::size_class(input.width, input.height);
                (hwdec::codec_key(input), class)
            });
            let known = known
                .as_ref()
                .map(|(codec, class)| (codec.as_str(), *class));
            if let Some(method) = hwdec::choose(&exe, source, known)?
                && let Some(extra) = hwdec::options(
                    method,
                    self.codec(),
                    known.is_some_and(|(codec, class)| {
                        hwdec::remembered(&exe, method, codec, class) == Some(true)
                    }),
                )
            {
                options.extend(extra);
                self.hwdec = Some(method);
            }
        }
        Decoder::spawn_with_output(
            &self.source,
            self.timeout,
            &options,
            &[],
            self.launch_output(),
            self.tap.as_ref(),
        )
    }
    /// What a launch asks `FFmpeg` for: a Motion JPEG source's own JPEGs
    /// while they decode here, else `output`.
    fn launch_output(&self) -> Output {
        let mjpeg = self
            .codec()
            .map_or(self.jpeg_before, |codec| codec == "mjpeg");
        if self.output == Output::Native && self.passthrough && mjpeg {
            Output::Jpeg
        } else {
            self.output
        }
    }
    /// The source's JPEGs do not decode here: `FFmpeg` decodes them from now
    /// on.
    fn needs_decoding(&mut self, error: &anyhow::Error) -> bool {
        let needs = error
            .chain()
            .any(<dyn std::error::Error + 'static>::is::<NeedsDecoding>);
        if needs {
            self.passthrough = false;
        }
        needs
    }
    /// A tap no recording is attached to goes once the camera lets its
    /// `FFmpeg` go.
    fn untap(&mut self) {
        if self.tap.as_ref().is_some_and(|tap| !tap.attached()) {
            self.tap = None;
        }
    }
    /// How a source would decode in hardware, if it is one that can: a
    /// compressed stream or file, or a webcam in a compressed mode, whose
    /// frames are read as decoded.
    fn hwdec_source(&self) -> Option<hwdec::Source> {
        if self.launch_output() != Output::Native || self.software {
            return None;
        }
        if self.native {
            let compressed = self.codec().is_some_and(|codec| codec != "rawvideo");
            return compressed.then_some(hwdec::Source::Live);
        }
        Some(if self.file() || self.clip() {
            hwdec::Source::File
        } else {
            hwdec::Source::Live
        })
    }
    /// The source's codec: a webcam mode's compressed format, else what
    /// `FFmpeg` last said.
    fn codec(&self) -> Option<&str> {
        if self.native {
            let format = self
                .mode
                .and_then(|index| self.modes[index].input_format)
                .and_then(|index| self.formats.get(index))?;
            return matches!(format.as_str(), "mjpeg" | "h264" | "hevc").then_some(format);
        }
        self.known.as_ref().map(|input| input.codec.as_str())
    }
    /// What a decoder's first frame says about the source: its codec and
    /// size, and whether the hardware decoded it, which is kept for the
    /// boot so a codec the method cannot decode goes to software at once.
    fn observe(&mut self, facts: (Option<native::Input>, Option<String>)) {
        let (Some(input), Some(pix_fmt)) = facts else {
            return;
        };
        if let (Some(method), Ok(exe)) = (self.hwdec, executable())
            && !input.deep
        {
            let class = hwdec::size_class(input.width, input.height);
            let hardware = hwdec::decoded_in_hardware(&pix_fmt);
            hwdec::learned(&exe, method, &hwdec::codec_key(&input), class, hardware);
        }
        // The next worker to open it reads a Motion JPEG source's JPEGs from
        // its first launch.
        let mjpeg = input.codec == "mjpeg";
        if !self.native && self.passthrough && mjpeg != self.jpeg_before {
            learned::found(&self.source, mjpeg);
            self.jpeg_before = mjpeg;
        }
        if !self.native
            && let Some(fps) = input.fps.filter(|fps| (0.1..=240.0).contains(fps))
        {
            // A file or stream's own rate, as FFmpeg reads it.
            self.fps = fps;
        }
        self.known = Some(input);
    }
    /// The source would decode to frames carried only as RGB: ask for RGB
    /// from now on.
    fn needs_rgb(&mut self, error: &anyhow::Error) -> bool {
        let needs = self.output == Output::Native
            && error
                .chain()
                .any(<dyn std::error::Error + 'static>::is::<NeedsRgb>);
        if needs {
            self.output = Output::Rgb;
        }
        needs
    }
    fn launch(&mut self) -> Result<()> {
        // Its JPEGs asked for only because an earlier process found it
        // sent Motion JPEG.
        let guessed = self.codec().is_none() && self.launch_output() == Output::Jpeg;
        let mut decoder = self.spawn()?;
        let timeout = if self.native {
            self.timeout.max(FIRST_FRAME_TIMEOUT)
        } else {
            self.timeout
        };
        let mut first = decoder.next(timeout);
        // It sends another codec now, which FFmpeg cannot copy as JPEGs.
        if guessed
            && first.is_err()
            && let Some(input) = decoder.facts().0
            && input.codec != "mjpeg"
        {
            decoder.stop();
            learned::found(&self.source, false);
            self.jpeg_before = false;
            decoder = self.spawn()?;
            first = decoder.next(timeout);
        }
        if let Err(error) = &first
            && (self.needs_rgb(error) || self.needs_decoding(error))
        {
            decoder.stop();
            decoder = self.spawn()?;
            first = decoder.next(timeout);
        }
        // A source whose FFmpeg exited with the hardware is decoded in
        // software, and if that works the method is not used for it again.
        // One that failed before FFmpeg found its video (refused, not found,
        // a bad sign-in), or that sent no frame in time, which software
        // would not change, is not opened again for nothing.
        if let Err(error) = &first
            && error
                .chain()
                .any(<dyn std::error::Error + 'static>::is::<StreamEnded>)
            && let Some(method) = self.hwdec
            && let input @ Some(_) = decoder.facts().0
        {
            decoder.stop();
            self.software = true;
            decoder = self.spawn()?;
            first = decoder.next(timeout);
            if first.is_ok()
                && let (Some(input), Ok(exe)) = (input, executable())
            {
                let class = hwdec::size_class(input.width, input.height);
                hwdec::learned(&exe, method, &hwdec::codec_key(&input), class, false);
            }
        }
        if first.is_ok() {
            decoder.observed = true;
            let facts = decoder.facts();
            self.observe(facts);
            // A stream or file found to be Motion JPEG is read again as its
            // own JPEGs, once.
            if decoder.output != Output::Jpeg && self.launch_output() == Output::Jpeg {
                decoder.stop();
                decoder = self.spawn()?;
                first = decoder.next(timeout);
                if let Err(error) = &first
                    && self.needs_decoding(error)
                {
                    decoder.stop();
                    decoder = self.spawn()?;
                    first = decoder.next(timeout);
                }
            }
        }
        let first = if self.native {
            first.map_err(|error| {
                let stalled = decoder
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .error
                    .is_none();
                decoder.stop();
                let log = decoder.log.text(&self.source);
                native_failure(&log, stalled, &self.source, timeout)
                    .map_or(error, |(kind, message)| Categorized::error(kind, message))
            })?
        } else {
            first.map_err(|error| {
                if !self.reconnects() {
                    return error.context("open media input");
                }
                let timed_out = !error
                    .chain()
                    .any(<dyn std::error::Error + 'static>::is::<StreamEnded>);
                decoder.stop();
                let log = decoder.log.text(&self.source);
                // FFmpeg's own words stay below the explanation, for
                // `{error:#}` and whoever debugs it.
                network_failure(&self.source, &log, timed_out, self.timeout)
                    .unwrap_or_else(|| error.context("open media input"))
            })?
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
        self.pixel_format = first.pixel_format;
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
/// What frames a source is asked for first: as it decodes them, except from
/// a test's stand-in `FFmpeg` that writes PPM.
fn first_output() -> Output {
    #[cfg(test)]
    if FAKE_FFMPEG.with_borrow(Option::is_some) && !FAKE_NATIVE.get() {
        return Output::Rgb;
    }
    Output::Native
}
pub(crate) fn open(info: &CameraInfo, timeout: Duration) -> Result<Box<dyn Backend>> {
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
pub(crate) fn open_url(source: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    open_url_with(source, None, timeout)
}
/// `FFmpeg`'s wording when an RTSP server asks for credentials or refuses them.
/// 403 is left out: RTSP servers often answer it for an address or ACL
/// restriction that no password changes.
fn auth_refused(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    reason.contains("401 ") || reason.contains("unauthorized")
}
/// Open a media URL, with a camera sign-in given beside it. A sign-in request
/// or refusal from an rtsp:// or rtsps:// server becomes a typed
/// `auth_required` or `auth_rejected` error naming the address without
/// credentials.
pub(crate) fn open_url_with(
    source: &str,
    login: Option<&crate::logins::CameraLogin>,
    timeout: Duration,
) -> Result<Box<dyn Backend>> {
    let url = match login {
        Some(login) => crate::logins::with_credentials(source, login)?,
        None => zeroize::Zeroizing::new(source.to_owned()),
    };
    open_source(&url, timeout).map_err(|error| {
        let rtsp = scheme(source).is_some_and(|s| s == "rtsp" || s == "rtsps");
        if rtsp && auth_refused(&format!("{error:#}")) {
            let had_credentials = login.is_some()
                || source.split_once('@').is_some_and(|(before, _)| {
                    before
                        .split_once("://")
                        .is_some_and(|(_, authority)| !authority.contains('/'))
                });
            crate::logins::auth_error(
                &redact_url(&crate::logins::strip_userinfo(source)),
                had_credentials,
            )
        } else {
            error
        }
    })
}
/// `FFmpeg`'s RTSP client copies the user name and password, as written in the
/// address, into a 128-byte buffer and silently cuts what does not fit
/// (`av_url_split` in `ff_rtsp_connect`), so the camera refuses a cut-off
/// password.
fn check_rtsp_userinfo(source: &str) -> Result<()> {
    const MAX: usize = 127;
    let Some((_, rest)) = source.split_once("://") else {
        return Ok(());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    match authority.rsplit_once('@') {
        Some((userinfo, _)) if userinfo.len() > MAX => Err(Categorized::error(
            ErrorKind::Unsupported,
            format!(
                "the user name and password for {} take {} bytes in the address, but FFmpeg's RTSP client keeps only {MAX}; use a shorter password (symbols and non-ASCII letters take 3 bytes or more when percent-encoded)",
                redact_url(&crate::logins::strip_userinfo(source)),
                userinfo.len()
            ),
        )),
        _ => Ok(()),
    }
}
fn open_source(source: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    validate_source(source)?;
    if matches!(scheme(source).as_deref(), Some("rtsp" | "rtsps")) {
        check_rtsp_userinfo(source)?;
    }
    ensure!(
        !timeout.is_zero() && Instant::now().checked_add(timeout).is_some(),
        "media timeout must be positive and fit in the monotonic clock"
    );
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
        formats: Vec::new(),
        reconnect: None,
        paced: true,
        on_demand: None,
        unpace: false,
        output: first_output(),
        pixel_format: RGB8,
        hwdec: None,
        software: false,
        known: None,
        passthrough: cfg!(feature = "jpeg"),
        jpeg_before: false,
        tap: None,
    };
    backend.jpeg_before =
        backend.passthrough && options.is_none() && learned::motion_jpeg(&backend.source);
    if let Some(options) = options {
        (backend.modes, backend.formats) = probe(&backend.source);
        backend.configure(options)?;
    }
    backend.launch()?;
    Ok(Box::new(backend))
}
fn backend_xml(native: bool, modes: &[Mode], playback: bool) -> String {
    let video_mode = if !modes.is_empty() {
        "<pFeature>VideoMode</pFeature>"
    } else if playback {
        "<pFeature>PlaybackSpeed</pFeature>"
    } else {
        ""
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
        let writable = native && matches!(name, "Width" | "Height");
        let access = if writable { "RW" } else { "RO" };
        let bounds = if writable {
            "<Min>16</Min><Max>8192</Max><pIsLocked>IsAcquiring</pIsLocked>"
        } else {
            ""
        };
        let _ = write!(
            xml,
            "<Integer Name='{name}'><pValue>{name}Reg</pValue>{bounds}</Integer><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>{access}</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
        );
    }
    let access = if native { "RW" } else { "RO" };
    let _ = write!(
        xml,
        "<Float Name='AcquisitionFrameRate'><pValue>FrameRateReg</pValue><Min>0.1</Min><Max>240</Max><Unit>Hz</Unit><pIsLocked>IsAcquiring</pIsLocked></Float><FloatReg Name='FrameRateReg'><Address>20</Address><Length>8</Length><AccessMode>{access}</AccessMode><Endianess>LittleEndian</Endianess></FloatReg><Integer Name='IsAcquiring'><pValue>AcquiringReg</pValue></Integer><IntReg Name='AcquiringReg'><Address>16</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"
    );
    if !modes.is_empty() {
        xml.push_str("<Enumeration Name='VideoMode'><pValue>VideoModeReg</pValue>");
        for (index, mode) in modes.iter().enumerate() {
            let _ = write!(
                xml,
                "<EnumEntry Name='{}'><Value>{index}</Value></EnumEntry>",
                mode.label()
            );
        }
        xml.push_str("<pIsLocked>IsAcquiring</pIsLocked></Enumeration><IntReg Name='VideoModeReg'><Address>28</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>");
    } else if playback {
        xml.push_str("<Enumeration Name='PlaybackSpeed'><ToolTip>RealTime plays the file at its frame rate in live view and recordings, while captures read it as fast as they ask; Fastest always decodes every frame as fast as possible</ToolTip><EnumEntry Name='RealTime'><Value>1</Value></EnumEntry><EnumEntry Name='Fastest'><Value>0</Value></EnumEntry><pValue>PlaybackReg</pValue><pIsLocked>IsAcquiring</pIsLocked></Enumeration><IntReg Name='PlaybackReg'><Address>32</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>");
    }
    xml.push_str("<Command Name='AcquisitionStart'><pValue>Control</pValue><CommandValue>1</CommandValue></Command><Command Name='AcquisitionStop'><pValue>Control</pValue><CommandValue>0</CommandValue></Command><IntReg Name='Control'><Address>16</Address><Length>4</Length><AccessMode>WO</AccessMode></IntReg></RegisterDescription>");
    xml
}
/// The 32-byte register file every media-like source exposes: width,
/// height, pixel format, payload size, acquisition state, frame rate and
/// video mode.
fn registers(
    width: u32,
    height: u32,
    pixel_format: u32,
    running: bool,
    fps: f64,
    mode: u32,
) -> [u8; 32] {
    let mut registers = [0u8; 32];
    let payload = crate::frame::frame_len(pixel_format, width, height).unwrap_or(0);
    registers[0..4].copy_from_slice(&width.to_le_bytes());
    registers[4..8].copy_from_slice(&height.to_le_bytes());
    registers[8..12].copy_from_slice(&pixel_format.to_le_bytes());
    registers[12..16].copy_from_slice(&u32::try_from(payload).unwrap_or(u32::MAX).to_le_bytes());
    registers[16..20].copy_from_slice(&u32::from(running).to_le_bytes());
    registers[20..28].copy_from_slice(&fps.to_le_bytes());
    registers[28..32].copy_from_slice(&mode.to_le_bytes());
    registers
}
fn register_range(registers: &[u8], address: u64, length: usize) -> Result<Vec<u8>> {
    let start = usize::try_from(address)?;
    let end = start
        .checked_add(length)
        .context("media register range overflow")?;
    Ok(registers
        .get(start..end)
        .context("unknown media register")?
        .to_vec())
}
impl RegisterIo for MediaBackend {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let mode = self
            .mode
            .map(u32::try_from)
            .transpose()
            .context("media mode index exceeds u32")?
            .unwrap_or(u32::MAX);
        let mut registers = [0u8; 36];
        registers[..32].copy_from_slice(&self::registers(
            self.width,
            self.height,
            self.pixel_format,
            self.running,
            self.fps,
            mode,
        ));
        registers[32..36].copy_from_slice(&u32::from(self.paced).to_le_bytes());
        register_range(&registers, address, length)
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        if address == 16 && data.len() == 4 {
            return Ok(());
        }
        if address == 32 && data.len() == 4 && self.file() {
            ensure!(
                !self.running,
                "stop acquisition before changing the playback speed"
            );
            self.paced = u32::from_le_bytes(data.try_into()?) != 0;
            self.decoder.take();
            self.first = None;
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
                            let (low_width, low_height) = m
                                .min_size
                                .map_or_else(|| m.size(), |(smallest, _)| smallest);
                            if address == 0 {
                                (low_width..=m.width).contains(&w)
                            } else {
                                (low_height..=m.height).contains(&h)
                            }
                        })
                        .max_by_key(|&(index, m)| {
                            let current = self.mode == Some(index);
                            let fits = m.fits((w, h));
                            (
                                fits,
                                if fits {
                                    m.honors_at((w, h), self.fps)
                                } else {
                                    m.honors(self.fps)
                                },
                                current,
                                m.rank(),
                            )
                        })
                        .map(|(index, _)| index)
                        .ok_or_else(|| self.unsupported(w, h, self.fps))?;
                    let index = if self.mode == Some(index) {
                        index
                    } else {
                        self.raw_first(index, self.fps)
                    };
                    self.select(index, Some(self.fps));
                    let mode = self.modes[index];
                    if mode.fits((w, h)) && (w, h) != mode.size() {
                        (self.width, self.height) = (w, h);
                        if let Some((_, low)) = mode.min_size {
                            self.fps = self.fps.max(low);
                        }
                    }
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
                    let index = self
                        .mode
                        .filter(|&i| {
                            self.modes[i].fits(size) && self.modes[i].honors_at(size, value)
                        })
                        .or_else(|| {
                            best(&self.modes, |m| m.fits(size) && m.honors_at(size, value))
                                .map(|i| self.raw_first(i, value))
                        })
                        .ok_or_else(|| self.unsupported(size.0, size.1, value))?;
                    self.select(index, Some(value));
                    (self.width, self.height) = size;
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
        self.reconnect = None;
        Ok(())
    }
}
impl Backend for MediaBackend {
    fn lingers(&self) -> bool {
        !self.file() && !self.clip()
    }
    fn frames_wait(&self) -> bool {
        self.file() || self.clip()
    }
    fn rest(&mut self) {
        if !self.running {
            self.decoder = None;
            self.first = None;
            self.unpace = false;
            self.untap();
        }
    }
    fn xml(&mut self) -> Result<String> {
        Ok(backend_xml(self.native, &self.modes, self.file()))
    }
    fn start(&mut self, _: usize) -> Result<()> {
        self.on_demand = None;
        if self.decoder.is_none() {
            self.launch()?;
        }
        self.running = true;
        Ok(())
    }
    /// A capture that asks for frames before a stream takes any reads a
    /// paced file as fast as it asks, like frames from a triggered camera;
    /// the file starts again unpaced. Its first frame is the one it opened
    /// with when that is still there, so one frame needs one `FFmpeg`.
    fn request_frame(&mut self) -> Result<()> {
        if self.on_demand.is_none() {
            self.on_demand = Some(true);
            if self.file() && self.paced {
                if self.first.is_some() {
                    self.unpace = true;
                } else {
                    self.decoder = None;
                    self.decoder = Some(self.spawn()?);
                }
            }
        }
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.running, "media acquisition has not started");
        if self.unpace && self.first.is_none() {
            self.unpace = false;
            self.decoder = None;
            self.decoder = Some(self.spawn()?);
            // It starts again with the frame already taken.
            self.decode(timeout)?;
        }
        self.on_demand.get_or_insert(false);
        let live = !self.file() && !self.clip();
        let mut frame = match self.first.take() {
            // The frame a live source opened with gives way to any it sent
            // since: that one may be long past.
            Some(first) if live => self
                .decoder
                .as_ref()
                .and_then(Decoder::newest)
                .unwrap_or(first),
            Some(first) => first,
            None => self.decode(timeout)?,
        };
        // A file's frames wait for their turn, so they arrive when taken.
        if !live {
            frame.wall_ms = 0;
        }
        self.width = frame.width;
        self.height = frame.height;
        self.pixel_format = frame.pixel_format;
        Ok(frame)
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.decoder.take();
        self.first = None;
        self.unpace = false;
        self.reconnect = None;
        self.untap();
        Ok(())
    }
    /// The tap a recording copies the source from, when its codec is one
    /// `wanted` takes for a recording in `codec`: a decoder that does not
    /// copy yet is started again with the tap, from the source's next key
    /// frame. A source that cannot be tapped is encoded instead.
    fn copy_stream(&mut self, wanted: StreamCopy, codec: &str) -> Result<Option<Arc<copy::Tap>>> {
        if cfg!(not(unix)) {
            return Ok(None);
        }
        let Some(source) = self.codec().and_then(copy::copyable) else {
            return Ok(None);
        };
        if !wanted.takes(source, codec) {
            return Ok(None);
        }
        let tap = match &self.tap {
            Some(tap) if tap.codec() == source => tap.clone(),
            _ => copy::Tap::new(source),
        };
        self.tap = Some(tap.clone());
        if self
            .decoder
            .as_ref()
            .is_some_and(|decoder| !decoder.tapped())
        {
            self.decoder = None;
            self.first = None;
            if let Err(error) = self.launch() {
                // FFmpeg could not copy this video as MPEG-TS: the
                // recording encodes it instead.
                self.tap = None;
                self.launch().map_err(|_| error)?;
                return Ok(None);
            }
        }
        Ok(Some(tap))
    }
    /// A webcam's video mode, size and rate, and a file's playback speed,
    /// which `FFmpeg` is opened with: kept for the next process to open the
    /// camera as a webcam keeps them until it is unplugged.
    fn emulated_settings(&mut self) -> Option<Value> {
        if self.native {
            Some(serde_json::json!({
                "mode": self.mode.map(|index| self.modes[index].label()),
                "width": self.width,
                "height": self.height,
                "requested_size": self.requested_size,
                "fps": self.fps,
            }))
        } else if self.file() {
            Some(serde_json::json!({ "paced": self.paced }))
        } else {
            None
        }
    }
    fn restore_settings(&mut self, settings: &Value) -> Result<()> {
        if let Some(paced) = settings["paced"].as_bool().filter(|_| self.file()) {
            return self.write_memory(32, &u32::from(paced).to_le_bytes());
        }
        ensure!(self.native, "settings saved for another kind of source");
        if let Some(index) = settings["mode"]
            .as_str()
            .and_then(|label| self.modes.iter().position(|mode| mode.label() == label))
        {
            self.write_memory(
                28,
                &u32::try_from(index)
                    .context("saved media mode index exceeds u32")?
                    .to_le_bytes(),
            )?;
        }
        if settings["requested_size"] == true {
            for (address, key) in [(0, "width"), (4, "height")] {
                let value = settings[key].as_u64().context("saved size")?;
                self.write_memory(address, &u32::try_from(value)?.to_le_bytes())?;
            }
        }
        if let Some(fps) = settings["fps"].as_f64() {
            self.write_memory(20, &fps.to_le_bytes())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ForwardConfig {
    pub output: String,
    pub codec: String,
    pub encoder: String,
    pub fps: f64,
    pub bitrate: String,
    /// A local recording ends, as asked, within this many bytes, stopped
    /// while `FFmpeg` can still finish it there (`NearEnd`); none by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_bytes: Option<u64>,
    /// The client's own free-space floor, which a local recording leaves
    /// as well as the worker's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free: Option<u64>,
    /// Optional per-frame correspondence for a synchronized local recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<timeline::Config>,
}
impl ForwardConfig {
    /// Validate forwarding settings before starting a process, changing camera
    /// settings or creating a file.
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.fps.is_finite() && (0.1..=240.0).contains(&self.fps),
            "forwarding FPS must be a finite number within 0.1..240"
        );
        let family = codec_family(&self.codec)?;
        validate_encoder(&self.codec, &self.encoder)?;
        if jetson_encoder(&self.encoder) {
            jetson_framerate(self.fps)?;
        }
        validate_bitrate(&self.bitrate)?;
        output_args(&self.output)?;
        crate::naming::validate_output(&self.output)?;
        let recording = recording_output(&self.output)?;
        if let Some(timeline) = &self.timeline {
            ensure!(
                recording.is_some(),
                "a video timeline requires a local recording"
            );
            timeline.validate(&self.output)?;
        }
        ensure!(
            self.max_file_bytes != Some(0),
            "a recording's maximum size must be positive"
        );
        if let Some((_, container)) = &recording {
            let container = *container;
            ensure!(
                container != "webm" || family == "av1",
                "WebM recordings require the av1 codec; use .mkv or .mp4 for h264 or hevc"
            );
        }
        ensure!(
            family != "ffv1"
                || recording.as_ref().is_some_and(|(path, _)| {
                    path.extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("mkv"))
                }),
            "ffv1 recordings are .mkv files"
        );
        Ok(())
    }
}
impl ForwardConfig {
    /// The new file of a local recording at `path`, bounded by
    /// `max_file_bytes` when it has one.
    pub(crate) fn writer(&self, path: &std::path::Path) -> Result<crate::storage::Recording> {
        crate::storage::Recording::create(path, self.max_file_bytes, self.min_free)
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ForwardStats {
    pub accepted: u64,
    pub written: u64,
    pub dropped: u64,
    /// Frames replaced while `FFmpeg` was still taking an earlier one: the
    /// encoder cannot keep up. `dropped` includes them.
    #[serde(default)]
    pub late: u64,
    /// No new camera frame for 2 s or more; nothing new reaches the encoder
    /// meanwhile.
    #[serde(default)]
    pub stalled: bool,
    pub encoder: String,
    /// The codec the recording is in: the source's own when it copies it.
    #[serde(default)]
    pub codec: String,
    pub error: Option<String>,
    /// The finished local recording, set once its file is complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<crate::engine::saved::RecordedFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Value>,
}
struct WriterState {
    /// The newest frame, converted only once the writer takes it.
    latest: Option<Frame>,
    error: Option<String>,
    error_tag: Option<crate::types::ErrorTag>,
    /// The first failure came from an encoder input pipe, not conversion.
    input_broken_pipe: bool,
    chunks: Option<Value>,
}
struct WriterQueue {
    state: Mutex<WriterState>,
    changed: Condvar,
    running: AtomicBool,
    accepted: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    late: AtomicU64,
    /// The writer is blocked handing a frame to `FFmpeg`.
    busy: AtomicBool,
    /// Writing the encoded recording failed, for example on a full disk.
    output_failed: AtomicBool,
    /// Encoded bytes already stored; final joined storage remains authoritative.
    encoded_bytes: AtomicU64,
    /// The recording is near its maximum size, which ends it as asked:
    /// its `FFmpeg` is stopped while what it still holds fits
    /// ([`NearEnd`]), or the file reached that size.
    full_size: AtomicBool,
    epoch: Instant,
    /// When the newest frame arrived, in milliseconds since `epoch`.
    arrived_ms: AtomicU64,
    /// When the first frame a copy wrote arrived, UTC Unix milliseconds: it
    /// can come from before the recording was asked for, back to the key
    /// frame it starts at.
    first_copied_ms: AtomicU64,
    /// Chunk payloads require a one-to-one mapping to encoder inputs.
    chunks_seen: AtomicBool,
}
/// Without a new frame for this long a recording counts as stalled.
const STALL: Duration = Duration::from_secs(2);
/// A recording this small holds at most its container's header, no video.
const HEADER_BYTES: u64 = 4096;
enum WriterFrame {
    New(Frame),
    Repeat,
}

impl WriterQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(WriterState {
                latest: None,
                error: None,
                error_tag: None,
                input_broken_pipe: false,
                chunks: None,
            }),
            changed: Condvar::new(),
            running: AtomicBool::new(true),
            accepted: AtomicU64::new(0),
            written: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            late: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            output_failed: AtomicBool::new(false),
            encoded_bytes: AtomicU64::new(0),
            full_size: AtomicBool::new(false),
            epoch: Instant::now(),
            arrived_ms: AtomicU64::new(0),
            first_copied_ms: AtomicU64::new(0),
            chunks_seen: AtomicBool::new(false),
        }
    }
    fn stalled(&self) -> bool {
        let arrived = Duration::from_millis(self.arrived_ms.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(arrived) >= STALL
    }
    /// When the newest frame arrived.
    fn arrived(&self) -> Instant {
        self.epoch + Duration::from_millis(self.arrived_ms.load(Ordering::Relaxed))
    }
    /// Wait until `slot` and a new frame. From `repeat_from` on the slot
    /// alone is enough for `WriterFrame::Repeat`. Returns `None` once the
    /// writer stops.
    fn take(&self, slot: Instant, repeat_from: Option<Instant>) -> Option<WriterFrame> {
        self.take_mode(slot, repeat_from, false)
    }
    fn take_mode(
        &self,
        slot: Instant,
        repeat_from: Option<Instant>,
        drain: bool,
    ) -> Option<WriterFrame> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if !self.running.load(Ordering::Acquire) {
                let final_frame = drain
                    .then(|| state.latest.take())
                    .flatten()
                    .map(WriterFrame::New);
                self.changed.notify_all();
                return final_frame;
            }
            let now = Instant::now();
            let ready = state.latest.is_some() || repeat_from.is_some_and(|from| now >= from);
            let wait = slot.saturating_duration_since(now);
            if ready && wait.is_zero() {
                let taken = state
                    .latest
                    .take()
                    .map_or(WriterFrame::Repeat, WriterFrame::New);
                // A source that waits for its frame to be taken goes on.
                self.changed.notify_all();
                return Some(taken);
            }
            let wait = if ready {
                wait
            } else {
                repeat_from.map_or(Duration::from_secs(1), |from| {
                    from.saturating_duration_since(now).max(wait)
                })
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
    fn fail_timeline(&self, error: &anyhow::Error) {
        self.fail_artifact("video timeline", error);
    }
    fn fail_artifact(&self, artifact: &str, error: &anyhow::Error) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.error.is_none() {
            state.error = Some(format!("{artifact}: {error:#}"));
        }
        if state.error_tag.is_none() {
            state.error_tag = crate::types::error_tag(error);
        }
    }
    fn typed_error(&self) -> Option<anyhow::Error> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .error
            .as_ref()
            .map(|message| crate::types::error_from_tag(state.error_tag.clone(), message.clone()))
    }
    fn fail(&self, error: String) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error
            .get_or_insert(error);
    }
    fn fail_input(&self, error: &std::io::Error) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.error.is_none() {
            state.error = Some(format!("FFmpeg input failed: {error}"));
            state.input_broken_pipe = error.kind() == std::io::ErrorKind::BrokenPipe;
        }
    }
    fn input_closed(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.input_broken_pipe && state.error_tag.is_none()
    }
    fn fallback_failure(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.error_tag.is_none() && (state.error.is_none() || state.input_broken_pipe)
    }
}
/// Hands a frame's own bytes to the encoder's pipe without copying them.
#[cfg(target_os = "linux")]
use pipes::Splicer;
/// Elsewhere every frame is written into the pipe: there is never a splicer.
#[cfg(not(target_os = "linux"))]
type Splicer = std::convert::Infallible;
/// When a writer sends the last frame again on a slot without a new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repeat {
    /// Never: `FFmpeg` stamps a recording's frames itself, so a stall leaves
    /// a gap.
    Never,
    /// While the camera streams: `GStreamer` stamps a recording's frames by
    /// count.
    UntilStalled,
    /// On every slot, stalled or not: a Jetson stream's `FFmpeg` only copies
    /// `GStreamer`'s video, and its receivers may drop a silent publisher.
    Always,
    /// Once no new frame has come for `KEEP_ALIVE`, on every slot after:
    /// a stream's `FFmpeg` repeats frames to keep its rate (`-fps_mode cfr`),
    /// but its receivers may drop a publisher silent for long.
    AfterSilence,
}
impl Repeat {
    /// How the writer of a Jetson encoder or of `FFmpeg`'s, for a local
    /// recording or a stream, repeats.
    const fn of(jetson: bool, local: bool) -> Self {
        match (jetson, local) {
            (true, true) => Self::UntilStalled,
            (true, false) => Self::Always,
            (false, true) => Self::Never,
            (false, false) => Self::AfterSilence,
        }
    }
}
/// How long a stream goes without a new frame before its writer sends the
/// last one again.
const KEEP_ALIVE: Duration = Duration::from_millis(500);
/// The writer thread's loop, until the queue stops or a frame cannot be
/// converted or written: on each slot of `interval`, the newest frame the
/// queue holds goes to `input`, converted first where the encoder cannot
/// read it, and a slot without a new frame sends the last one again as
/// `repeat` says. How many frames it took from the queue, each converted
/// at most once.
fn write_frames(
    shared: &WriterQueue,
    (input, mut splicer): (&mut impl Write, Option<&mut Splicer>),
    (raw, jetson): (RawInput, Option<JetsonInput>),
    interval: Duration,
    repeat: Repeat,
    mut timeline: Option<timeline::Writer>,
    mut chunks: Option<timeline::ChunkWriter>,
) -> u64 {
    let mut next = Instant::now();
    let mut last: Option<Frame> = None;
    let mut scratch = Scratch::default();
    let mut taken = 0;
    loop {
        let repeat_from = match repeat {
            _ if last.is_none() || shared.chunks_seen.load(Ordering::Acquire) => None,
            Repeat::Never => None,
            Repeat::UntilStalled => (!shared.stalled()).then(Instant::now),
            Repeat::Always => Some(Instant::now()),
            Repeat::AfterSilence => Some(shared.arrived() + KEEP_ALIVE),
        };
        let Some(fresh) = (if timeline.is_some() || shared.chunks_seen.load(Ordering::Acquire) {
            shared.take_mode(next, repeat_from, true)
        } else {
            shared.take(next, repeat_from)
        }) else {
            break;
        };
        if let WriterFrame::New(frame) = fresh {
            if frame.chunks.is_some() {
                shared.chunks_seen.store(true, Ordering::Release);
                if chunks.is_none() {
                    let error = Categorized::error(
                        ErrorKind::Unsupported,
                        "image chunk data requires a local recording with chunk artifacts",
                    );
                    shared.fail_artifact("video chunks", &error);
                    break;
                }
            }
            if let Some(timeline) = &timeline {
                match timeline.accepts(&frame) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        shared.fail_timeline(&error);
                        break;
                    }
                }
            }
            taken += 1;
            // A frame that needs converting is converted here, once: the
            // frames a newer one replaced in the queue never are, and a
            // repeat sends the conversion again.
            let prepared = match jetson {
                Some(jetson) => jetson.prepare(&frame, &mut scratch),
                None => raw.prepare(&frame, &mut scratch.bytes),
            };
            if let Err(error) = prepared {
                shared.fail(format!("{error:#}"));
                break;
            }
            last = Some(frame);
        }
        let Some(frame) = &last else {
            continue;
        };
        let payload = match jetson {
            Some(jetson) => jetson.payload(frame, &scratch),
            None => raw.payload(frame, &scratch.bytes),
        };
        shared.busy.store(true, Ordering::Relaxed);
        let written = match (splicer.as_deref_mut(), payload) {
            // The frame's own bytes: their pages go into the pipe.
            #[cfg(target_os = "linux")]
            (Some(splicer), Payload::Slice(bytes)) if !raw.converts() && jetson.is_none() => {
                crate::copies::record(crate::copies::Site::PipeSplice, bytes.len());
                splicer.send(frame, bytes.len())
            }
            _ => {
                crate::copies::record(crate::copies::Site::PipeWrite, payload.len());
                payload.write_to(input)
            }
        };
        shared.busy.store(false, Ordering::Relaxed);
        if let Err(e) = written {
            shared.fail_input(&e);
            break;
        }
        shared.written.fetch_add(1, Ordering::Relaxed);
        if let Some(chunks) = &mut chunks
            && let Err(error) = chunks.wrote(frame)
        {
            shared.fail_artifact("video chunks", &error);
            break;
        }
        if let Some(timeline) = &mut timeline
            && let Err(error) = timeline.wrote(frame)
        {
            shared.fail_timeline(&error);
            break;
        }
        if repeat == Repeat::Never || shared.chunks_seen.load(Ordering::Acquire) {
            last = None;
        }
        // Slots stay on a fixed grid; a writer that fell behind skips the
        // slots it missed instead of catching up in a burst.
        let now = Instant::now();
        next = (next + interval).max(now.checked_sub(interval).unwrap_or(now));
    }
    if let Some(timeline) = timeline
        && let Err(error) = timeline.finish()
    {
        shared.fail_timeline(&error);
    }
    if let Some(chunks) = chunks {
        match chunks.finish() {
            Ok(summary) => {
                shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .chunks = summary;
            }
            Err(error) => shared.fail_artifact("video chunks", &error),
        }
    }
    taken
}
/// How long a copy that is ending waits for each frame that completes its
/// display order ([`copy::Order`]), and for all of them.
const COMPLETING: (Duration, Duration) = (Duration::from_millis(500), Duration::from_secs(2));
/// The copying writer's loop: the source's frames from `feed` go to
/// `input` as they come, until the writer stops, the feed is closed and
/// empty, or a write fails. Once stopped, what was queued still goes, and
/// then, for a source with B-frames, the frames that arrive next and are
/// shown before one written, so the file ends with none missing.
fn write_copied(shared: &WriterQueue, feed: &copy::Feed, input: &mut impl Write) {
    let mut order = copy::Order::default();
    let mut ending: Option<Instant> = None;
    loop {
        let running = shared.running.load(Ordering::Acquire);
        if !running && ending.is_none() {
            feed.end();
            ending = Some(Instant::now() + COMPLETING.1);
        }
        let wait = match ending {
            None => Duration::from_millis(100),
            Some(_) if !order.reordered() => Duration::ZERO,
            Some(deadline) => COMPLETING
                .0
                .min(deadline.saturating_duration_since(Instant::now())),
        };
        let (tables, frame) = match feed.take(wait) {
            copy::Taken::Frame(_, frame, true) if !order.completes(&frame) => break,
            copy::Taken::Frame(tables, frame, _) => (tables, frame),
            copy::Taken::Waiting if running => continue,
            copy::Taken::Waiting | copy::Taken::Closed => break,
        };
        shared.busy.store(true, Ordering::Relaxed);
        let written = tables
            .as_ref()
            .map_or(Ok(()), |tables| input.write_all(tables))
            .and_then(|()| input.write_all(&frame.data));
        shared.busy.store(false, Ordering::Relaxed);
        if let Err(e) = written {
            shared.fail_input(&e);
            break;
        }
        crate::copies::record(crate::copies::Site::StreamCopy, frame.data.len());
        order.wrote(&frame);
        if shared.written.fetch_add(1, Ordering::Relaxed) == 0 {
            shared
                .first_copied_ms
                .store(frame.arrived_ms, Ordering::Relaxed);
        }
    }
}
/// What a recording's `FFmpeg` reads.
enum Input {
    /// Camera frames, converted where it cannot read them; through a
    /// Jetson encoder when there is one.
    Frames(RawInput, Option<JetsonInput>),
    /// The source's own video, from its tap through this feed.
    Copied(Arc<copy::Tap>, Arc<copy::Feed>),
}
struct EncoderProcess {
    child: Child,
    /// The frame writer: it ends with the frames whose pages may still be
    /// in the pipe (`pipes::Splicer`), which live until `FFmpeg` has exited.
    writer: Option<JoinHandle<Vec<Frame>>>,
    output_thread: Option<JoinHandle<EncodedOutput>>,
    log_thread: Option<JoinHandle<()>>,
    queue: Arc<WriterQueue>,
    log: Arc<Log>,
    _exe: Arc<Executable>,
    width: u32,
    height: u32,
    /// How frames reach `FFmpeg`; none when it copies the source's video.
    raw: Option<RawInput>,
    /// Set when a `GStreamer` Jetson encoder process precedes `FFmpeg`.
    jetson: Option<JetsonInput>,
    /// The source's tap and the feed `FFmpeg` copies from.
    copied: Option<(Arc<copy::Tap>, Arc<copy::Feed>)>,
    stage: Option<Child>,
    stage_log: Option<JoinHandle<()>>,
    encoder: String,
    /// When the recording file was created, UTC Unix milliseconds.
    started_ms: u64,
    spawned: Instant,
    /// What the recording's file finds on its disk.
    alarm: Option<Arc<crate::storage::Alarm>>,
}
struct EncodedOutput {
    storage: crate::storage::Recording,
    error: Option<String>,
    /// A Matroska file's start, for its length and index (`mkv::finish`).
    head: Option<mkv::Head>,
}
/// The longest a frame from a source that can wait (`Forwarder::waits`)
/// waits for the writer to take the one before, as while the encoder
/// starts; after that it replaces it.
const WAIT_FOR_WRITER: Duration = Duration::from_secs(3);
pub(crate) struct Forwarder {
    config: ForwardConfig,
    process: Option<EncoderProcess>,
    final_stats: ForwardStats,
    /// The recording file, created when its name was picked so no other
    /// writer can take the name before the first frame.
    reserved: Option<crate::storage::Recording>,
    /// An automatically chosen encoder that exited early; the recording
    /// falls back once to the next candidate.
    failed: Option<String>,
    /// The source's tap, when the recording copies its video rather than
    /// encoding frames.
    copy: Option<Arc<copy::Tap>>,
    /// Its source can wait ([`crate::types::Backend::frames_wait`]): a
    /// frame waits for the writer to take the one before rather than
    /// replacing it, so a file is recorded whole, even while the encoder
    /// starts.
    waits: bool,
    timeline: Option<timeline::Writer>,
}
/// Frames handed to a Jetson encoder: formats `nvvidconv` accepts in system
/// memory and converts on the VIC engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JetsonInput {
    /// Gray as the luma plane with neutral chroma.
    I420,
    Rgba,
}
#[deny(clippy::cast_precision_loss)]
impl JetsonInput {
    const fn caps(self) -> &'static str {
        match self {
            Self::I420 => "i420",
            Self::Rgba => "rgba",
        }
    }
    /// The bytes of each frame `GStreamer` reads.
    #[deny(clippy::arithmetic_side_effects)]
    fn frame_bytes(self, width: u32, height: u32) -> Result<usize> {
        let width = usize::try_from(width).context("invalid Jetson input width")?;
        let height = usize::try_from(height).context("invalid Jetson input height")?;
        ensure!(width > 0 && height > 0, "invalid Jetson input dimensions");
        let pixels = width.checked_mul(height);
        let bytes = match self {
            Self::I420 => {
                let chroma = width
                    .div_ceil(2)
                    .checked_mul(height.div_ceil(2))
                    .and_then(|plane| plane.checked_mul(2));
                pixels
                    .zip(chroma)
                    .and_then(|(luma, chroma)| luma.checked_add(chroma))
            }
            Self::Rgba => pixels.and_then(|pixels| pixels.checked_mul(4)),
        };
        bytes
            .filter(|&bytes| bytes <= MAX_FRAME)
            .context("Jetson input frame exceeds size limit")
    }
    /// Convert `frame` into `scratch` for `payload`, in one pass: RGBA, or
    /// gray for a monochrome frame that is not 8-bit gray already.
    fn prepare(self, frame: &Frame, scratch: &mut Scratch) -> Result<()> {
        self.frame_bytes(frame.width, frame.height)?;
        match self {
            Self::Rgba => {
                crate::frame::convert_into(frame, &mut scratch.rgba, |[r, g, b]| [r, g, b, 255])?;
                crate::copies::record(
                    crate::copies::Site::JetsonInput,
                    scratch.rgba.as_flattened().len(),
                );
            }
            Self::I420 => {
                if Self::gray8(frame) {
                    let required =
                        crate::frame::frame_len(frame.pixel_format, frame.width, frame.height)
                            .context("Jetson grayscale frame size overflow")?;
                    ensure!(
                        frame.data.len() >= required,
                        "truncated Jetson grayscale frame"
                    );
                } else {
                    crate::frame::convert_into(frame, &mut scratch.bytes, |[gray, _, _]| gray)?;
                    crate::copies::record(crate::copies::Site::JetsonInput, scratch.bytes.len());
                }
                // Neutral chroma for both planes, the same for every frame.
                let chroma =
                    (frame.width as usize).div_ceil(2) * (frame.height as usize).div_ceil(2);
                scratch.chroma.resize(2 * chroma, 128);
            }
        }
        Ok(())
    }
    /// Whether the frame's bytes are the luma plane as they are.
    const fn gray8(frame: &Frame) -> bool {
        matches!(
            frame.pixel_format,
            crate::types::MONO8 | 0x0108_00b1 | 0x0108_00c6
        )
    }
    /// What the encoder reads of a prepared frame: the luma plane, the
    /// frame's own bytes for 8-bit gray, then the chroma planes.
    fn payload<'a>(self, frame: &'a Frame, scratch: &'a Scratch) -> Payload<'a> {
        match self {
            Self::Rgba => Payload::Slice(scratch.rgba.as_flattened()),
            Self::I420 => {
                let pixels = frame.width as usize * frame.height as usize;
                let luma = if Self::gray8(frame) {
                    &frame.data[..pixels]
                } else {
                    scratch.bytes.as_slice()
                };
                Payload::Parts([luma, &scratch.chroma])
            }
        }
    }
    /// The bytes the encoder reads for `frame`, as one buffer.
    #[cfg(test)]
    fn bytes(self, frame: &Frame) -> Result<Vec<u8>> {
        let mut scratch = Scratch::default();
        self.prepare(frame, &mut scratch)?;
        let payload = self.payload(frame, &scratch);
        let mut bytes = Vec::with_capacity(payload.len());
        payload.write_to(&mut bytes)?;
        Ok(bytes)
    }
}
/// What an encoder's input pipe reads of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Payload<'a> {
    Slice(&'a [u8]),
    /// Rows of `row` bytes, each followed by `pad`: odd-width 4:2:2, whose
    /// rows `FFmpeg` reads as whole pixel pairs.
    Rows {
        data: &'a [u8],
        row: usize,
        pad: [u8; 2],
    },
    /// Parts in order: a Jetson I420 frame's luma and chroma planes.
    Parts([&'a [u8]; 2]),
}
impl Payload<'_> {
    fn len(&self) -> usize {
        match self {
            Payload::Slice(data) => data.len(),
            Payload::Rows { data, row, pad } => data.len() / row * (row + pad.len()),
            Payload::Parts(parts) => parts.iter().map(|part| part.len()).sum(),
        }
    }
    /// Write it to `out`, gathering parts and rows into as few writes as
    /// the pipe takes.
    fn write_to(&self, out: &mut impl Write) -> std::io::Result<()> {
        /// Slices per write, within every platform's `IOV_MAX`.
        const BATCH: usize = 1024;
        match self {
            Payload::Slice(data) => out.write_all(data),
            Payload::Parts(parts) => {
                crate::frame::write_all_vectored(out, &mut parts.map(IoSlice::new))
            }
            Payload::Rows { data, row, pad } => {
                let mut slices = Vec::with_capacity(BATCH);
                for line in data.chunks_exact(*row) {
                    slices.extend([IoSlice::new(line), IoSlice::new(pad)]);
                    if slices.len() == BATCH {
                        crate::frame::write_all_vectored(out, &mut slices)?;
                        slices.clear();
                    }
                }
                crate::frame::write_all_vectored(out, &mut slices)
            }
        }
    }
}
/// A recording writer's reused buffers for the frames it converts.
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    rgba: Vec<[u8; 4]>,
    chroma: Vec<u8>,
}
fn output_args(output: &str) -> Result<Vec<String>> {
    ensure!(
        !output.is_empty() && output.len() <= 16384 && !output.chars().any(char::is_control),
        "invalid forwarding destination"
    );
    if !is_recording(output) {
        validate_network_url(output, "forwarding destination URL")?;
    }
    let mut args = Vec::new();
    match scheme(output).as_deref() {
        Some("rtsp" | "rtsps") => {
            args.extend(["-f", "rtsp", "-rtsp_transport", "tcp"].map(str::to_owned));
        }
        Some("rtmp" | "rtmps") => args.extend(["-f", "flv"].map(str::to_owned)),
        Some("srt" | "udp" | "tcp" | "http" | "https") => {
            args.extend(["-f", "mpegts"].map(str::to_owned));
        }
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
        canonical_scheme(output)
    };
    args.push(destination);
    Ok(args)
}
fn recording_output(output: &str) -> Result<Option<(PathBuf, &'static str)>> {
    if !is_recording(output) {
        return Ok(None);
    }
    let path = local_file_path(output)?;
    let invalid = path.as_os_str().is_empty()
        || path == std::path::Path::new("-")
        || output.ends_with('/')
        || cfg!(windows) && output.ends_with('\\')
        || path.file_name().is_none();
    ensure!(!invalid, "recording needs an output file");
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

pub(crate) fn local_file_path(value: &str) -> Result<PathBuf> {
    if scheme(value).as_deref() != Some("file") {
        return Ok(value.into());
    }
    let path = value.split_once(':').unwrap().1;
    ensure!(!path.is_empty(), "file URL needs a path");
    if let Some(rest) = path.strip_prefix("//") {
        ensure!(
            !rest.contains(['?', '#']),
            "file URL cannot contain a query or fragment; percent-encode filename characters or use a plain path"
        );
        let (authority, path) = rest
            .split_once('/')
            .context("file URL needs a local file path")?;
        ensure!(
            authority.is_empty() || authority.eq_ignore_ascii_case("localhost") || cfg!(windows),
            "file URL host must be localhost; use a mounted network folder for other hosts"
        );
        let path = decode_file_url_path(path)?;
        if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
            if cfg!(windows)
                && path.len() >= 3
                && path.as_bytes()[0].is_ascii_alphabetic()
                && path.as_bytes()[1] == b':'
                && path.as_bytes()[2] == b'/'
            {
                return Ok(path.into());
            }
            return Ok(PathBuf::from(format!("/{path}")));
        }
        return Ok(PathBuf::from(format!("//{authority}/{path}")));
    }
    Ok(path.into())
}

fn decode_file_url_path(path: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(path.len());
    let mut source = path.as_bytes().iter();
    while let Some(&byte) = source.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hex = |byte: &u8| {
            char::from(*byte)
                .to_digit(16)
                .and_then(|digit| u8::try_from(digit).ok())
        };
        let high = source.next().and_then(hex);
        let low = source.next().and_then(hex);
        let (Some(high), Some(low)) = (high, low) else {
            bail!("file URL path contains invalid percent escaping");
        };
        bytes.push(high * 16 + low);
    }
    let decoded = String::from_utf8(bytes).context("file URL path must be valid UTF-8")?;
    ensure!(
        !decoded.chars().any(char::is_control),
        "file URL path must not contain encoded control characters"
    );
    Ok(decoded)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawPreparation {
    Direct,
    Unpack,
    GrayOrRgb,
    /// Deep RGB for FFV1, preserving every bit in planar G, B and R.
    Gbr,
}
impl RawPreparation {
    const fn active(self) -> bool {
        !matches!(self, Self::Direct)
    }
    const fn decodes(self) -> bool {
        matches!(self, Self::GrayOrRgb | Self::Gbr)
    }
    const fn unpacks(self) -> bool {
        matches!(self, Self::Unpack)
    }
}

/// How a camera frame is handed to `FFmpeg`: its rawvideo pixel format, bytes
/// per pixel and whether it carries color. Sending sensor-native bytes
/// avoids a CPU conversion on the camera thread and, for mono and Bayer, moves
/// a third of the bytes RGB24 would through the pipe. `FFmpeg` reads no packed
/// samples and no 10- or 12-bit Bayer, so those are unpacked and widened to
/// its 16-bit formats first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[deny(clippy::struct_excessive_bools)]
struct RawInput {
    pixel_encoding: Option<crate::types::PixelEncoding>,
    pixel_format: u32,
    ffmpeg: &'static str,
    bytes_per_pixel: usize,
    color: bool,
    /// The exclusive preparation pass before the pipe, if one is needed.
    preparation: RawPreparation,
    /// Bits each 16-bit sample is shifted up to span the full range.
    widen: u32,
    /// The color range and matrix of `Y'CbCr` frames, for `FFmpeg`'s input
    /// options: full-range BT.601 for PFNC's YUV and YCbCr formats, video
    /// levels for its `YCbCr601` and `YCbCr709` ones.
    yuv: Option<(&'static str, &'static str)>,
    /// Planar or semiplanar: the frame is its planes, not equal rows.
    planes: bool,
    /// Samples narrowed to 8 bits in the pass that unpacks or widens them,
    /// by this shift, for an encoder that keeps 8 bits (`for_encoder`):
    /// `ffmpeg` and `bytes_per_pixel` then describe the 8-bit samples.
    narrow: Option<u32>,
}
/// `FFmpeg`'s planar and semiplanar `Y'CbCr` layouts.
fn planes(pix_fmt: &str) -> bool {
    matches!(
        pix_fmt,
        "nv12" | "nv21" | "nv16" | "yuv420p" | "yuv422p" | "yuv444p"
    )
}
#[deny(clippy::cast_precision_loss)]
impl RawInput {
    fn for_frame(frame: &Frame) -> Result<Self> {
        crate::frame::validate_encoding(frame)?;
        if let Some(ids) = crate::frame::frame_encoding(frame) {
            let mut raw = Self::for_format(crate::frame::frame_unpacked_format(frame))?;
            raw.pixel_format = frame.pixel_format;
            raw.pixel_encoding = frame.pixel_encoding;
            raw.preparation = RawPreparation::Unpack;
            debug_assert!(matches!(ids.bits(), 10 | 12));
            return Ok(raw);
        }
        Self::for_format(frame.pixel_format)
    }
    fn for_format(pixel_format: u32) -> Result<Self> {
        let unpacked = crate::frame::packing(pixel_format).map_or(pixel_format, |(v, ..)| v);
        let mut convert = false;
        let (ffmpeg, bytes_per_pixel, color) = match unpacked {
            crate::types::MONO8 | 0x0108_00b1 | 0x0108_00c6 => ("gray", 1, false),
            0x0110_0003 => ("gray10le", 2, false),
            0x0110_0005 => ("gray12le", 2, false),
            0x0110_0025 => ("gray14le", 2, false),
            0x0110_0007 | 0x0110_00b8 | 0x0110_00c7 => ("gray16le", 2, false),
            RGB8 => ("rgb24", 3, true),
            0x0218_0015 => ("bgr24", 3, true),
            0x0220_0016 => ("rgba", 4, true),
            0x0220_0017 => ("bgra", 4, true),
            0x0210_001f | 0x0210_0043 | 0x0210_0044 | 0x0210_0045 => ("uyvy422", 2, true),
            0x0210_0032 | 0x0210_003b | 0x0210_003e | 0x0210_0041 => ("yuyv422", 2, true),
            0x020c_0112 | crate::frame::NV12_601 | crate::frame::NV12_709 => ("nv12", 1, true),
            0x020c_0114 => ("nv21", 1, true),
            0x0210_0113 => ("nv16", 1, true),
            0x820c_0001..=0x820c_0003 => ("yuv420p", 1, true),
            0x8210_0001..=0x8210_0003 => ("yuv422p", 1, true),
            0x8218_0001..=0x8218_0003 => ("yuv444p", 1, true),
            0x0108_0008 => ("bayer_grbg8", 1, true),
            0x0108_0009 => ("bayer_rggb8", 1, true),
            0x0108_000a => ("bayer_gbrg8", 1, true),
            0x0108_000b => ("bayer_bggr8", 1, true),
            0x0110_000c | 0x0110_0010 | 0x0110_002e | 0x0110_0109 => ("bayer_grbg16le", 2, true),
            0x0110_000d | 0x0110_0011 | 0x0110_002f | 0x0110_010a => ("bayer_rggb16le", 2, true),
            0x0110_000e | 0x0110_0012 | 0x0110_0030 | 0x0110_010b => ("bayer_gbrg16le", 2, true),
            0x0110_000f | 0x0110_0013 | 0x0110_0031 | 0x0110_010c => ("bayer_bggr16le", 2, true),
            _ if crate::frame::convertible(pixel_format) => {
                convert = true;
                if crate::frame::monochrome(pixel_format) {
                    ("gray", 1, false)
                } else {
                    ("rgb24", 3, true)
                }
            }
            _ => bail!(
                "unsupported PFNC format 0x{pixel_format:08x}; capture with --format raw to preserve bytes"
            ),
        };
        let widen = match unpacked {
            0x0110_000c..=0x0110_000f => 6,
            0x0110_0010..=0x0110_0013 => 4,
            0x0110_0109..=0x0110_010c => 2,
            _ => 0,
        };
        Ok(Self {
            pixel_format,
            pixel_encoding: None,
            ffmpeg,
            bytes_per_pixel,
            color,
            preparation: if convert {
                RawPreparation::GrayOrRgb
            } else if unpacked != pixel_format {
                RawPreparation::Unpack
            } else {
                RawPreparation::Direct
            },
            widen,
            yuv: match pixel_format {
                0x0210_003e
                | 0x0210_0044
                | crate::frame::NV12_601
                | 0x820c_0002
                | 0x8210_0002
                | 0x8218_0002 => Some(("tv", "bt470bg")),
                0x0210_0041
                | 0x0210_0045
                | crate::frame::NV12_709
                | 0x820c_0003
                | 0x8210_0003
                | 0x8218_0003 => Some(("tv", "bt709")),
                _ if matches!(ffmpeg, "yuyv422" | "uyvy422") || planes(ffmpeg) => {
                    Some(("pc", "bt470bg"))
                }
                _ => None,
            },
            planes: planes(ffmpeg),
            narrow: None,
        })
    }
    /// What `encoder` reads: frames `FFmpeg` gets only after a pass that
    /// unpacks or widens 10- to 16-bit samples (packed monochrome, and
    /// Bayer mosaics deeper than 8 bits) are narrowed to 8 bits in that
    /// pass for any encoder but FFV1, which keeps every bit: every other
    /// encoder's input is 8-bit 4:2:0 or RGB, so the pipe and `FFmpeg`'s
    /// conversion take half the bytes. Samples keep their top 8 bits, as
    /// stills do.
    fn for_encoder(self, encoder: &str) -> Self {
        let base = Self::for_format(self.pixel_format).unwrap_or(self);
        // Lossless keeps every bit of deep RGB too, which FFmpeg reads as
        // planar G, B and R of 16-bit samples.
        if encoder == "ffv1"
            && let Some(bits) = crate::frame::deep_color_bits(self.pixel_format)
        {
            return Self {
                ffmpeg: match bits {
                    9 | 10 => "gbrp10le",
                    11 | 12 => "gbrp12le",
                    13 | 14 => "gbrp14le",
                    _ => "gbrp16le",
                },
                bytes_per_pixel: 6,
                preparation: RawPreparation::Gbr,
                ..base
            };
        }
        if encoder == "ffv1"
            || jetson_encoder(encoder)
            || base.preparation.decodes()
            || !(base.preparation.unpacks() || base.widen > 0)
        {
            return base;
        }
        match self
            .pixel_encoding
            .and_then(|encoding| encoding.ids(self.pixel_format).ok())
            .map(|ids| (ids.reduced_format(), ids.bits() - 8))
            .or_else(|| crate::frame::reduced(self.pixel_format))
            .and_then(|(eight, shift)| Some((Self::for_format(eight).ok()?, shift)))
        {
            Some((eight, shift)) => Self {
                ffmpeg: eight.ffmpeg,
                bytes_per_pixel: 1,
                widen: 0,
                narrow: Some(shift),
                ..base
            },
            None => base,
        }
    }
    /// The pixel format `FFmpeg` reads these frames as for `encoder`: their
    /// own, except that FFV1 gets a Bayer mosaic as gray, which it stores
    /// losslessly as it is (players show the mosaic in gray) where it would
    /// otherwise demosaic 8-bit Bayer into RGB and reject 16-bit Bayer at
    /// the level it is run at.
    fn input_pix_fmt(&self, encoder: &str) -> &'static str {
        match (encoder, self.bytes_per_pixel) {
            ("ffv1", 1) if self.bayer() => "gray",
            ("ffv1", _) if self.bayer() => "gray16le",
            _ => self.ffmpeg,
        }
    }
    fn bayer(&self) -> bool {
        self.ffmpeg.starts_with("bayer_")
    }
    /// Packed RGB, with or without padding, in either component order.
    fn rgb(&self) -> bool {
        matches!(self.ffmpeg, "rgb24" | "bgr24" | "rgba" | "bgra")
    }
    /// The bytes `FFmpeg` reads for each frame: odd-width 4:2:2 rows end in
    /// a whole pixel pair.
    #[deny(clippy::arithmetic_side_effects)]
    fn pipe_len(&self, width: u32, height: u32) -> Result<usize> {
        let length = self.frame_len(width, height)?;
        if self.pairs() && !width.is_multiple_of(2) {
            let width = usize::try_from(width).context("invalid forwarding width")?;
            let height = usize::try_from(height).context("invalid forwarding height")?;
            return width
                .checked_add(1)
                .and_then(|width| width.checked_mul(2))
                .and_then(|row| row.checked_mul(height))
                .filter(|&length| length <= MAX_FRAME)
                .context("padded forwarding frame exceeds size limit");
        }
        Ok(length)
    }
    /// YUV 4:2:2, where each two pixels share one Cb and one Cr.
    fn pairs(&self) -> bool {
        matches!(self.ffmpeg, "uyvy422" | "yuyv422")
    }
    /// `FFmpeg` input options for the frames' color range and matrix.
    fn color_options(&self) -> Vec<&'static str> {
        match self.yuv {
            Some((range, matrix)) => vec!["-color_range", range, "-colorspace", matrix],
            None => Vec::new(),
        }
    }
    /// Size of the normalized image, checked before unpacking or allocating.
    #[deny(clippy::arithmetic_side_effects)]
    fn frame_len(&self, width: u32, height: u32) -> Result<usize> {
        let pixels = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .and_then(|(width, height)| width.checked_mul(height));
        ensure!(
            width > 0 && height > 0 && pixels.is_some_and(|n| n <= MAX_FRAME / 3),
            "invalid forwarding dimensions"
        );
        if self.planes {
            return crate::frame::frame_len(self.pixel_format, width, height)
                .context("forwarding frame exceeds size limit");
        }
        pixels
            .and_then(|n| n.checked_mul(self.bytes_per_pixel))
            .filter(|&n| n <= MAX_FRAME)
            .context("forwarding frame exceeds size limit")
    }
    /// Check a frame before it is queued, so a frame that cannot be
    /// recorded fails its push: its format and size, and that it holds
    /// every byte its conversion reads.
    fn check(&self, frame: &Frame) -> Result<()> {
        let length = self.frame_len(frame.width, frame.height)?;
        ensure!(
            frame.pixel_format == self.pixel_format && frame.pixel_encoding == self.pixel_encoding,
            "the camera's pixel format changed; start a new recording to continue"
        );
        crate::frame::validate_encoding(frame)?;
        let name = crate::frame::frame_format_name(frame);
        if self.preparation.unpacks() || self.preparation.decodes() {
            return crate::frame::validate_source(frame);
        }
        let needed = if self.preparation.unpacks() || self.narrow.is_some() {
            crate::frame::frame_row_bytes(frame)
                .checked_mul(frame.height as usize)
                .context("forwarding frame exceeds size limit")?
        } else {
            length
        };
        ensure!(frame.data.len() >= needed, "truncated {name} frame");
        if self.pairs() && !frame.width.is_multiple_of(2) {
            (frame.width as usize * 2 + 2)
                .checked_mul(frame.height as usize)
                .filter(|&n| n <= MAX_FRAME)
                .context("forwarding frame exceeds size limit")?;
        }
        Ok(())
    }
    /// Whether `FFmpeg` reads a conversion of the frame rather than its bytes.
    const fn converts(&self) -> bool {
        self.preparation.active() || self.widen > 0 || self.narrow.is_some()
    }
    /// Convert a checked `frame` into `scratch` when `FFmpeg` cannot read its
    /// bytes: packed samples are unpacked and widened in one pass a row at
    /// a time, and other layouts converted to gray or RGB.
    fn prepare(&self, frame: &Frame, scratch: &mut Vec<u8>) -> Result<()> {
        if !self.converts() {
            return Ok(());
        }
        let length = self.frame_len(frame.width, frame.height)?;
        if let Some(shift) = self.narrow {
            scratch.resize(length, 0);
            let width = frame.width as usize;
            let eight =
                |sample: &[u8; 2]| crate::frame::clamp8(u16::from_le_bytes(*sample) >> shift);
            if let Some((_, bits, packing)) = crate::frame::frame_packing(frame) {
                let row = crate::frame::frame_row_bytes(frame);
                let rows = frame.data[..row * frame.height as usize].chunks_exact(row);
                let mut samples = vec![[0; 2]; width];
                for (line, out) in rows.zip(scratch.chunks_exact_mut(width)) {
                    crate::frame::unpack_row(line, bits, packing, &mut samples);
                    for (value, sample) in out.iter_mut().zip(&samples) {
                        *value = eight(sample);
                    }
                }
            } else {
                let source = frame.data[..2 * length].as_chunks::<2>().0;
                for (value, sample) in scratch.iter_mut().zip(source) {
                    *value = eight(sample);
                }
            }
        } else if self.preparation == RawPreparation::Gbr {
            crate::frame::gbr_planes(frame, scratch)?;
        } else if self.preparation.decodes() {
            let channels = if self.color {
                crate::frame::Channels::Rgb
            } else {
                crate::frame::Channels::Gray
            };
            crate::frame::decode_into(frame, channels, scratch)?;
        } else {
            scratch.resize(length, 0);
            let widened = |sample: [u8; 2]| {
                let value = u16::from_le_bytes(sample).min(u16::MAX >> self.widen);
                (value << self.widen).to_le_bytes()
            };
            if let Some((_, bits, packing)) =
                crate::frame::frame_packing(frame).filter(|_| self.preparation.unpacks())
            {
                let row = crate::frame::frame_row_bytes(frame);
                let rows = frame.data[..row * frame.height as usize].chunks_exact(row);
                let out_row = frame.width as usize * 2;
                for (line, out) in rows.zip(scratch.chunks_exact_mut(out_row)) {
                    let samples = out.as_chunks_mut::<2>().0;
                    crate::frame::unpack_row(line, bits, packing, samples);
                    if self.widen > 0 {
                        for sample in samples {
                            *sample = widened(*sample);
                        }
                    }
                }
            } else {
                // Whole 16-bit samples, widened on their way.
                let source = frame.data[..length].as_chunks::<2>().0;
                for (out, sample) in scratch.as_chunks_mut::<2>().0.iter_mut().zip(source) {
                    *out = widened(*sample);
                }
            }
        }
        crate::copies::record(crate::copies::Site::FfmpegUnpack, scratch.len());
        ensure!(
            scratch.len() == length,
            "truncated {} frame",
            crate::frame::frame_format_name(frame)
        );
        Ok(())
    }
    /// What `FFmpeg` reads of a prepared frame: the sensor buffer itself
    /// unless its samples are packed or widened, a 4:2:2 frame's rows each
    /// with the half pixel pair `FFmpeg`'s rows end in.
    fn payload<'a>(&self, frame: &'a Frame, scratch: &'a [u8]) -> Payload<'a> {
        if self.converts() {
            return Payload::Slice(scratch);
        }
        let (width, height) = (frame.width as usize, frame.height as usize);
        let length = if self.planes {
            frame
                .data
                .len()
                .min(self.frame_len(frame.width, frame.height).unwrap_or(0))
        } else {
            width * height * self.bytes_per_pixel
        };
        if self.pairs() && !width.is_multiple_of(2) {
            // A camera ends each odd-width 4:2:2 row with a lone pixel and
            // its Cb, but FFmpeg reads rows of whole pairs, 4 * ceil(width /
            // 2) bytes. Each row gets the missing luma, which lies outside
            // the image, and a neutral Cr, as `frame::unpacked` decodes that
            // pixel.
            return Payload::Rows {
                data: &frame.data[..length],
                row: width * 2,
                pad: [128, 128],
            };
        }
        Payload::Slice(&frame.data[..length])
    }
    /// The bytes `FFmpeg` reads for `frame`, as one buffer.
    #[cfg(test)]
    fn bytes(&self, frame: &Frame) -> Result<Vec<u8>> {
        self.check(frame)?;
        let mut scratch = Vec::new();
        self.prepare(frame, &mut scratch)?;
        let payload = self.payload(frame, &scratch);
        let mut bytes = Vec::with_capacity(payload.len());
        payload.write_to(&mut bytes)?;
        Ok(bytes)
    }
}
impl Forwarder {
    /// Keep the first failure's classification and reason while retaining the
    /// diagnostics collected when the encoder and its stderr finish.
    pub(crate) fn completed_error(
        primary: &anyhow::Error,
        completed: &anyhow::Error,
    ) -> anyhow::Error {
        let first = format!("{primary:#}");
        let finished = format!("{completed:#}");
        let message = if first == finished {
            first
        } else {
            format!("{first}; while finishing forwarding: {finished}")
        };
        crate::types::error_from_tag(crate::types::error_tag(primary), message)
    }
    pub(crate) fn new(config: ForwardConfig) -> Result<Self> {
        config.validate()?;
        check_codec(&config)?;
        let timeline = config
            .timeline
            .as_ref()
            .map(|timeline| timeline::Writer::create(timeline, config.fps, config.min_free))
            .transpose()?;
        Ok(Self {
            timeline,
            config,
            process: None,
            final_stats: ForwardStats::default(),
            reserved: None,
            failed: None,
            copy: None,
            waits: false,
        })
    }
    /// Use an output the caller already named. `reserved` is the already
    /// created recording file, if any.
    pub(crate) fn with_resolved(
        config: ForwardConfig,
        reserved: Option<crate::storage::Recording>,
    ) -> Result<Self> {
        if let Err(error) = config.validate().and_then(|()| check_codec(&config)) {
            if let Some(reserved) = reserved {
                reserved.abort().context("remove unused recording output")?;
            }
            return Err(error);
        }
        let timeline = match config
            .timeline
            .as_ref()
            .map(|timeline| timeline::Writer::create(timeline, config.fps, config.min_free))
            .transpose()
        {
            Ok(timeline) => timeline,
            Err(error) => {
                if let Some(reserved) = reserved {
                    reserved.abort().context("remove unused recording output")?;
                }
                return Err(error);
            }
        };
        Ok(Self {
            timeline,
            config,
            process: None,
            final_stats: ForwardStats::default(),
            reserved,
            failed: None,
            copy: None,
            waits: false,
        })
    }
    /// Record the source's own video from `tap` (`MediaBackend::copy_stream`)
    /// in its codec, which `config` names: no encoder is needed. `reserved`
    /// is the already created recording file, if any.
    pub(crate) fn copying(
        config: ForwardConfig,
        reserved: Option<crate::storage::Recording>,
        tap: Arc<copy::Tap>,
    ) -> Result<Self> {
        let checked = config.validate().and_then(|()| {
            ensure!(
                config.timeline.is_none(),
                "indexed video cannot copy compressed source packets"
            );
            ensure!(
                codec_family(&config.codec)? == tap.codec(),
                "a copied recording keeps the source's {} video",
                tap.codec()
            );
            Ok(())
        });
        if let Err(error) = checked {
            if let Some(reserved) = reserved {
                reserved.abort().context("remove unused recording output")?;
            }
            return Err(error);
        }
        Ok(Self {
            config,
            process: None,
            final_stats: ForwardStats::default(),
            reserved,
            failed: None,
            copy: Some(tap),
            waits: false,
            timeline: None,
        })
    }
    /// The index this forwarder exclusively created, before writer transfer.
    pub(crate) fn owned_timeline_path(&self) -> Option<&std::path::Path> {
        self.timeline.as_ref().map(timeline::Writer::path)
    }
    /// The local file this forwarder records to, if it records.
    pub(crate) fn recording_path(&self) -> Option<PathBuf> {
        recording_output(&self.config.output)
            .ok()
            .flatten()
            .map(|(path, _)| path)
    }
    #[deny(clippy::cast_possible_wrap)]
    fn spawn(&mut self, width: u32, height: u32, raw: RawInput) -> Result<()> {
        raw.frame_len(width, height)?;
        let gop = crate::types::integral_float((self.config.fps * 2.0).round().max(1.0))
            .and_then(|frames| u32::try_from(frames).ok())
            .context("recording keyframe interval exceeds the encoder range")?
            .to_string();
        let exe = executable()?;
        let mut skip = self.failed.iter().cloned().collect::<Vec<_>>();
        if self.config.timeline.is_some() {
            skip.extend(["nvv4l2h264enc", "nvv4l2h265enc", "nvv4l2av1enc"].map(str::to_owned));
        }
        let encoder = select_encoder(&exe, &self.config, (width, height), &raw, &skip)?;
        ensure!(
            self.config.timeline.is_none() || !jetson_encoder(&encoder),
            "indexed video requires a direct FFmpeg encoder"
        );
        let raw = raw.for_encoder(&encoder);
        let jetson = jetson_encoder(&encoder).then_some(if raw.color {
            JetsonInput::Rgba
        } else {
            JetsonInput::I420
        });
        let fps = self.config.fps.to_string();
        let mut cmd = command(&exe);
        let mut stage = None;
        if let Some(input) = jetson {
            let (fps_numerator, fps_denominator) = jetson_framerate(self.config.fps)?;
            let mut gst = Command::new("gst-launch-1.0");
            gst.args([
                "-q",
                "fdsrc",
                "fd=0",
                // A frame a read, rather than 4 KiB.
                &format!("blocksize={}", input.frame_bytes(width, height)?),
                "!",
                "rawvideoparse",
                &format!("width={width}"),
                &format!("height={height}"),
                &format!("format={}", input.caps()),
                &format!("framerate={fps_numerator}/{fps_denominator}"),
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
            let input = encoder_input(&exe, &encoder, &raw, (width, height))?;
            let pix_fmt = raw.input_pix_fmt(&encoder);
            cmd.args(encoder_device_options(&encoder));
            // Each frame is stamped when FFmpeg reads it and a recording keeps
            // that time in the output, so the file lasts as long as it did
            // even when frames arrive late, slower than `fps`, or not at all
            // for a while. Repeating frames instead would make FFmpeg encode
            // a whole outage's worth of copies at once when frames return.
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-n",
                "-f",
                "rawvideo",
                "-pix_fmt",
                pix_fmt,
                "-video_size",
                &format!("{width}x{height}"),
                "-framerate",
                &fps,
            ]);
            if self.config.timeline.is_none() {
                cmd.args(["-use_wallclock_as_timestamps", "1"]);
            }
            cmd.args(raw.color_options());
            cmd.args([
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                &encoder,
                "-b:v",
                &self.config.bitrate,
                "-g",
                &gop,
            ]);
            let options = encoder_output_options(&encoder, input, pix_fmt);
            cmd.args(
                if is_recording(&self.config.output) && self.config.timeline.is_none() {
                    recording_clock_options(options)
                } else {
                    options
                },
            );
            if self.config.timeline.is_some() {
                cmd.args([
                    "-fps_mode",
                    "passthrough",
                    "-enc_time_base",
                    "0",
                    "-avoid_negative_ts",
                    "disabled",
                ]);
                if self.config.codec != "ffv1" {
                    cmd.args(["-bf", "0"]);
                }
            } else if is_recording(&self.config.output) {
                cmd.args(["-fps_mode", "vfr"]);
            } else {
                // A stream keeps its rate: FFmpeg repeats a frame for the
                // slots it lasted, so the writer sends each one once, and
                // the last again only after `KEEP_ALIVE` without one, so
                // FFmpeg never has more than that to fill at once.
                cmd.args(["-fps_mode", "cfr", "-r", &fps]);
            }
            cmd.args(software_encoder_options(
                &encoder,
                width,
                height,
                self.config.fps,
            ));
            if encoder.ends_with("_videotoolbox") {
                cmd.args(["-realtime", "1"]);
            }
        }
        self.launch(
            (exe, cmd, stage),
            encoder,
            (width, height),
            Input::Frames(raw, jetson),
        )
    }
    /// Record the source's own video from `tap`, as it arrives: `FFmpeg`
    /// reads it as MPEG-TS and only remuxes it.
    fn spawn_copy(&mut self, width: u32, height: u32, tap: Arc<copy::Tap>) -> Result<()> {
        let exe = executable()?;
        let mut cmd = command(&exe);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-n",
            "-f",
            "mpegts",
            "-i",
            "pipe:0",
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-c:v",
            "copy",
        ]);
        self.launch(
            (exe, cmd, None),
            "copy".into(),
            (width, height),
            Input::Copied(tap, copy::Feed::new()),
        )
    }
    /// Start `cmd`, whose input options are set, writing to the output: its
    /// container's options, then the frames' writer, the encoded output's
    /// reader and its log. A `GStreamer` `stage`, when there is one, reads
    /// the frames and feeds `FFmpeg`.
    fn launch(
        &mut self,
        (exe, mut cmd, mut stage): (Arc<Executable>, Command, Option<Child>),
        encoder: String,
        (width, height): (u32, u32),
        input: Input,
    ) -> Result<()> {
        let recording = recording_output(&self.config.output)?;
        if recording.is_some() && !matches!(&input, Input::Copied(..)) {
            cmd.args(["-nostats", "-progress", "pipe:2"]);
        }
        let local = recording.is_some();
        let matroska = recording
            .as_ref()
            .is_some_and(|(_, container)| matches!(*container, "matroska" | "webm"));
        if let Some((_, container)) = &recording {
            if *container == "mp4" {
                cmd.args(["-movflags", "frag_keyframe+empty_moov"]);
            }
            if *container == "mpegts" && self.config.timeline.is_some() {
                // MPEG-TS otherwise adds its default mux delay to every PTS,
                // shifting the sidecar's frame-count clock by 1.4 seconds.
                cmd.args(["-muxdelay", "0", "-mpegts_copyts", "1"]);
            }
            // Its length and index are added once FFmpeg is done (`mkv`).
            if matroska {
                cmd.args(mkv::output_args());
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
        // Validate and probe before creating a file.
        let storage = match self.reserved.take() {
            Some(reserved) => Some(reserved),
            None => recording
                .map(|(path, _)| self.config.writer(&path))
                .transpose()?,
        };
        let started_ms = crate::scheduling::now_ms();
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
                return Err(error).context(FfmpegUnavailable);
            }
        };
        let log = Log::new();
        // Frames go to the first process: GStreamer when it encodes, else FFmpeg.
        let (mut input_pipe, stage_log) = match stage.as_mut() {
            Some(stage) => (
                stage.stdin.take().context("GStreamer stdin")?,
                Some(log.collect(stage.stderr.take().context("GStreamer stderr")?)),
            ),
            None => (child.stdin.take().context("FFmpeg stdin")?, None),
        };
        let log_thread = log.collect(child.stderr.take().unwrap());
        let queue = Arc::new(WriterQueue::new());
        let alarm = storage.as_ref().map(crate::storage::Recording::alarm);
        let chunks = storage.as_ref().map(|storage| {
            timeline::ChunkWriter::new(storage.path().to_path_buf(), self.config.min_free)
        });
        let limit = self.config.max_file_bytes;
        let output_thread = storage.map(|mut storage| {
            let mut output = child.stdout.take().expect("piped encoded output");
            let shared = queue.clone();
            thread::spawn(move || {
                let mut near = limit.map(NearEnd::new);
                let mut start = matroska.then(mkv::Rewriter::new);
                let error = copy_recording(&mut output, &mut storage, start.as_mut(), |written| {
                    shared.encoded_bytes.store(written, Ordering::Release);
                    if near.as_mut().is_some_and(|near| near.reached(written)) {
                        shared.full_size.store(true, Ordering::Release);
                    }
                })
                .err()
                .map(|error| format!("write bounded recording: {error}"));
                shared
                    .encoded_bytes
                    .store(storage.bytes_written(), Ordering::Release);
                let head = start.and_then(|start| start.head());
                if storage.cut() == Some(crate::storage::Cut::MaxSize) {
                    shared.full_size.store(true, Ordering::Release);
                }
                if let Some(error) = &error {
                    shared.fail(error.clone());
                    shared.output_failed.store(true, Ordering::Release);
                    shared.running.store(false, Ordering::Release);
                    shared.changed.notify_all();
                }
                // Only the owner observing the child's exit status may commit
                // a recording. EOF alone can also mean an encoder failure.
                EncodedOutput {
                    storage,
                    error,
                    head,
                }
            })
        });
        let shared = queue.clone();
        let interval = Duration::from_secs_f64(1.0 / self.config.fps);
        let (raw, jetson, copied) = match &input {
            Input::Frames(raw, jetson) => (Some(*raw), *jetson, None),
            Input::Copied(tap, feed) => (None, None, Some((tap.clone(), feed.clone()))),
        };
        if let Input::Frames(raw, None) = &input {
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd as _;
                pipes::enlarge(input_pipe.as_raw_fd(), raw.pipe_len(width, height)?);
            }
            #[cfg(not(target_os = "linux"))]
            let _ = raw;
        }
        let timeline = self.timeline.take();
        let writer = thread::spawn(move || {
            let held = match input {
                Input::Frames(raw, jetson) => {
                    // Each new frame once, at most one per slot.
                    let repeat = if timeline.is_some() {
                        Repeat::Never
                    } else {
                        Repeat::of(jetson.is_some(), local)
                    };
                    // FFmpeg reads a frame's own bytes straight from its
                    // pages, which stay as they are until it has.
                    #[cfg(target_os = "linux")]
                    let mut splicer = (jetson.is_none() && !raw.converts()).then(|| {
                        use std::os::fd::AsRawFd as _;
                        Splicer::new(input_pipe.as_raw_fd())
                    });
                    #[cfg(not(target_os = "linux"))]
                    let mut splicer: Option<Splicer> = None;
                    write_frames(
                        &shared,
                        (&mut input_pipe, splicer.as_mut()),
                        (raw, jetson),
                        interval,
                        repeat,
                        timeline,
                        chunks,
                    );
                    #[cfg(target_os = "linux")]
                    let held = splicer.map(Splicer::into_held).unwrap_or_default();
                    #[cfg(not(target_os = "linux"))]
                    let held = Vec::new();
                    held
                }
                Input::Copied(tap, feed) => {
                    tap.attach(feed.clone());
                    write_copied(&shared, &feed, &mut input_pipe);
                    tap.detach(&feed);
                    feed.close();
                    Vec::new()
                }
            };
            // Closing stdin lets FFmpeg flush its muxer and finalize files.
            drop(input_pipe);
            shared.running.store(false, Ordering::Release);
            shared.changed.notify_all();
            held
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
            copied,
            stage,
            stage_log,
            encoder,
            started_ms,
            spawned: Instant::now(),
            alarm,
        });
        Ok(())
    }
    /// An automatically chosen encoder that exits within its first seconds
    /// of a local recording, before writing any video, usually rejected this
    /// frame size or ran out of hardware sessions. Its file is removed and
    /// the recording starts again, once, in a new file with the next
    /// candidate. One that had written video ends the recording and keeps
    /// it. A stream's early exit is more likely its server's doing, and a
    /// failed write the disk's.
    fn fallback_available(&self, process: &EncoderProcess) -> bool {
        !(self.config.timeline.is_some()
            || process.queue.chunks_seen.load(Ordering::Acquire)
            || self.config.encoder != "auto"
            || process.raw.is_none()
            || self.failed.is_some()
            || !is_recording(&self.config.output)
            || process.queue.output_failed.load(Ordering::Acquire)
            || process.queue.encoded_bytes.load(Ordering::Acquire) > HEADER_BYTES
            || process.spawned.elapsed() > Duration::from_secs(10)
            || self.file_end(process).is_some()
            || !process.queue.fallback_failure())
    }
    fn fall_back(&mut self) -> Result<bool> {
        let status = match self.process.as_mut() {
            Some(process) => process.child.try_wait()?,
            None => return Ok(false),
        };
        let Some(process) = self.process.as_ref() else {
            return Ok(false);
        };
        if status.is_some_and(|status| status.success())
            || (status.is_none() && !process.queue.input_closed())
            || !self.fallback_available(process)
        {
            return Ok(false);
        }
        self.final_stats = self.stats();
        let mut process = self.process.take().expect("encoder process");
        let encoder = process.encoder.clone();
        let (width, height) = (process.width, process.height);
        let raw = process.raw.context("a copying recording has no encoder")?;
        let finished = finish(&mut process);
        // A broken input pipe may precede child exit. Existing bounded shutdown
        // must confirm failure, and final storage/artifact facts must still
        // permit replacing its header-only file.
        if finished.0.is_none_or(|status| status.success())
            || !self.fallback_available(&process)
            || finished
                .1
                .as_ref()
                .is_none_or(|output| output.storage.bytes_written() > HEADER_BYTES)
        {
            let end = self.file_end(&process);
            let settled = self.settle(process, finished);
            if let Some(end) = end {
                return Err(match settled {
                    Ok(()) => end,
                    Err(error) => end.context(format!("while finishing forwarding: {error:#}")),
                });
            }
            settled?;
            bail!("FFmpeg forwarding stopped before an encoder replacement was possible");
        }
        if let Some(output) = finished.1 {
            output
                .storage
                .abort()
                .context("remove failed recording output")?;
        }
        if let Ok(exe) = executable() {
            remember(&exe, |probes| {
                probes.encoders.insert(
                    probe_key(
                        &encoder,
                        EncoderInput::Yuv,
                        Some(raw.input_pix_fmt(&encoder)),
                        width,
                        height,
                    ),
                    Found::now(false),
                );
            });
        }
        self.failed = Some(encoder);
        self.spawn(width, height, raw)?;
        Ok(true)
    }
    /// Frames come from a source that can wait for them to be taken.
    pub(crate) const fn wait_for_writer(&mut self) {
        self.waits = true;
    }
    /// Queue a frame for encoding; it is owned so its buffer moves to the
    /// writer thread without a copy.
    pub(crate) fn push(&mut self, frame: Frame) -> Result<bool> {
        // The output thread notes why the file ended before it fails the
        // queue, so a failure seen once the file has ended is that end.
        self.queue_frame(frame).map_err(|error| {
            self.process
                .as_ref()
                .and_then(|process| self.file_end(process))
                .unwrap_or(error)
        })
    }
    fn queue_frame(&mut self, frame: Frame) -> Result<bool> {
        if frame.multipart.is_some() {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "multipart component archives currently require a local still capture",
            ));
        }
        if frame.chunks.is_some() && (!is_recording(&self.config.output) || self.copy.is_some()) {
            return Err(Categorized::error(
                ErrorKind::Unsupported,
                "image chunk data requires a local encoded recording; network streams cannot retain chunk artifacts",
            ));
        }
        if let Some(timeline) = &self.config.timeline {
            let received = frame.received_monotonic_ns.ok_or_else(|| {
                Categorized::error(
                    ErrorKind::Incomplete,
                    "indexed video frame has no host receive timestamp",
                )
            })?;
            if received > timeline.acquisition_end_ns {
                return Ok(false);
            }
        }
        if self.process.is_none() {
            match self.copy.clone() {
                Some(tap) => self.spawn_copy(frame.width, frame.height, tap)?,
                None => self.spawn(frame.width, frame.height, RawInput::for_frame(&frame)?)?,
            }
        }
        let process = self.process.as_ref().unwrap();
        if frame.chunks.is_some() {
            process.queue.chunks_seen.store(true, Ordering::Release);
        }
        ensure!(
            frame.width == process.width && frame.height == process.height,
            "the camera's resolution changed from {}x{} to {}x{}; start a new recording to continue",
            process.width,
            process.height,
            frame.width,
            frame.height
        );
        // A copy records the video as it arrives, whatever its frames are
        // decoded to.
        ensure!(
            process
                .raw
                .is_none_or(|raw| frame.pixel_format == raw.pixel_format),
            "the camera's pixel format changed; start a new recording to continue"
        );
        // Bad source bytes are an input error, never a reason to replace an
        // encoder whose pipe happened to close at the same time.
        match (process.jetson, process.raw) {
            (Some(_), _) => crate::frame::validate_source(&frame)?,
            (None, Some(raw)) => raw.check(&frame)?,
            (None, None) => {}
        }
        // A recording at its maximum size, or on a disk below its floor,
        // ends: as asked, or with storage_full.
        if let Some(end) = self.file_end(process) {
            return Err(end);
        }
        // An encoder that died at start leaves a broken pipe behind; falling
        // back to the next one comes first.
        if self.fall_back()? {
            return self.queue_frame(frame);
        }
        let process = self.process.as_mut().unwrap();
        if let Some(error) = process.queue.typed_error() {
            return Err(error);
        }
        let stored_error = process
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error
            .clone();
        if let Some(error) = stored_error {
            bail!("{error}: {}", process.log.text(&self.config.output).trim());
        }
        if let Some(status) = process.child.try_wait()? {
            bail!(
                "FFmpeg forwarding exited ({status}): {}",
                process.log.text(&self.config.output).trim()
            );
        }
        #[cfg(test)]
        if let Some(race) = PUSH_RACE.get() {
            race(&process.queue);
        }
        // FFmpeg converts on its own threads with SIMD swscale (or the encoder
        // hardware), so the camera's buffer itself is queued for the pipe.
        // What FFmpeg cannot read, and Jetson's RGBA or I420, the writer
        // builds from it in one pass; only what that reads is checked here.
        if process.raw.is_none() {
            // A copied recording's frames only show the source is live.
            let queue = &process.queue;
            queue.accepted.fetch_add(1, Ordering::Relaxed);
            let arrived = u64::try_from(queue.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
            queue.arrived_ms.store(arrived, Ordering::Relaxed);
            return Ok(true);
        }
        let queue = &process.queue;
        let mut state = queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + WAIT_FOR_WRITER;
        while self.waits
            && state.latest.is_some()
            && state.error.is_none()
            && queue.running.load(Ordering::Acquire)
        {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = queue
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        if state.error.is_some() {
            drop(state);
            // The writer can close its input after the first health check or
            // while this source waits for its queued frame to be taken.
            if self.fall_back()? {
                return self.queue_frame(frame);
            }
            if let Some(error) = self
                .process
                .as_ref()
                .and_then(|process| process.queue.typed_error())
            {
                return Err(error);
            }
            bail!("FFmpeg forwarding writer stopped");
        }
        ensure!(
            queue.running.load(Ordering::Acquire),
            "FFmpeg forwarding writer stopped"
        );
        let replaced = state.latest.replace(frame).is_some();
        queue.accepted.fetch_add(1, Ordering::Relaxed);
        let arrived = u64::try_from(queue.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        queue.arrived_ms.store(arrived, Ordering::Relaxed);
        if replaced {
            queue.dropped.fetch_add(1, Ordering::Relaxed);
            if queue.busy.load(Ordering::Relaxed) {
                queue.late.fetch_add(1, Ordering::Relaxed);
            }
        }
        drop(state);
        queue.changed.notify_one();
        Ok(!replaced)
    }
    /// Why the recording must end for its file: it reached its maximum
    /// size ([`crate::storage::MaxSize`]), or its disk fell below the
    /// free-space floor (`storage_full`).
    pub(crate) fn storage_end(&self) -> Option<anyhow::Error> {
        let process = self.process.as_ref()?;
        self.file_end(process).or_else(|| {
            // Only an actual early input EPIPE can await the next push's
            // bounded confirmation. Existing startup age ends this deferral
            // even if no more frames arrive; stop never restarts a recording.
            if process.queue.input_closed() && self.fallback_available(process) {
                None
            } else {
                process.queue.typed_error()
            }
        })
    }
    fn file_end(&self, process: &EncoderProcess) -> Option<anyhow::Error> {
        if process.queue.full_size.load(Ordering::Acquire) {
            let limit = self.config.max_file_bytes.unwrap_or_default();
            return Some(crate::storage::MaxSize(limit).into());
        }
        process.alarm.as_ref().and_then(|alarm| alarm.error())
    }
    pub(crate) fn stats(&self) -> ForwardStats {
        self.process.as_ref().map_or_else(
            || self.final_stats.clone(),
            |p| {
                let lost = p.copied.as_ref().map_or(0, |(_, feed)| feed.dropped());
                let state = p
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                ForwardStats {
                    accepted: p.queue.accepted.load(Ordering::Relaxed),
                    written: p.queue.written.load(Ordering::Relaxed),
                    dropped: p.queue.dropped.load(Ordering::Relaxed) + lost,
                    late: p.queue.late.load(Ordering::Relaxed),
                    stalled: p.queue.stalled(),
                    encoder: p.encoder.clone(),
                    codec: self.config.codec.clone(),
                    error: state.error.clone(),
                    recording: None,
                    chunks: state.chunks.clone(),
                }
            },
        )
    }
    pub(crate) fn stop(&mut self) -> Result<()> {
        self.final_stats = self.stats();
        // A recording that never got a frame leaves no empty file behind.
        if let Some(reserved) = self.reserved.take() {
            reserved.abort().context("remove unused recording output")?;
        }
        if let Some(timeline) = self.timeline.take() {
            timeline.finish()?;
        }
        let Some(mut process) = self.process.take() else {
            return Ok(());
        };
        let mut finished = finish(&mut process);
        if self.config.timeline.is_some() || process.queue.chunks_seen.load(Ordering::Acquire) {
            let indexed = process.queue.written.load(Ordering::Relaxed);
            let encoded = process.log.completed_video_frames();
            if let Some(chunks) = process
                .queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .chunks
                .as_mut()
            {
                chunks["encoded_frames"] = json!(encoded);
                chunks["video_frames_verified"] = json!(encoded == Some(indexed));
            }
            if encoded != Some(indexed) {
                let error=Categorized::new(ErrorKind::Incomplete,"encoded video frame count differs from its frame index or final encoder progress is missing")
                    .details(json!({"index_frames":indexed,"encoded_frames":encoded}));
                let error: anyhow::Error = error.into();
                process.queue.fail_timeline(&error);
                finished.2.get_or_insert_with(|| format!("{error:#}"));
            }
        }
        self.settle(process, finished)
    }
    /// Keep or remove the recording of a finished encoder process, and say
    /// how it ended.
    #[deny(clippy::let_underscore_must_use)]
    fn settle(&mut self, process: EncoderProcess, finished: Finished) -> Result<()> {
        let (status, recording, mut error) = finished;
        self.final_stats.written = process.queue.written.load(Ordering::Relaxed);
        self.final_stats.chunks.clone_from(
            &process
                .queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .chunks,
        );
        let mut full_size = false;
        if let Some(mut output) = recording {
            // Video the encoder wrote stays even when it failed or had to be
            // stopped, or when a write to the file failed, since every
            // recording container plays up to its last complete part. The
            // cause still ends the recording. One cut at its maximum size,
            // which FFmpeg overran though it was stopped before it, ended
            // as asked: the encoder then fails on its closed output, which
            // is no error, but the file is reported as cut.
            let cut = output.storage.cut();
            full_size = cut == Some(crate::storage::Cut::MaxSize);
            if full_size
                && self.config.timeline.is_none()
                && !process.queue.chunks_seen.load(Ordering::Acquire)
            {
                (error, output.error) = (None, None);
            } else if full_size {
                let failure: anyhow::Error = Categorized::new(
                    ErrorKind::Incomplete,
                    "indexed video was truncated at its file-size limit",
                )
                .details(json!({"max_file_bytes":self.config.max_file_bytes}))
                .into();
                process.queue.fail_timeline(&failure);
                error.get_or_insert_with(|| format!("{failure:#}"));
            }
            let kept = (output.error.is_none() || cut.is_some())
                && output.storage.bytes_written() > HEADER_BYTES;
            if full_size && !kept {
                error = Some("the recording reached its maximum size before any video".into());
            }
            if error.is_none() {
                error = output.error;
            }
            let valid = kept
                || status
                    .as_ref()
                    .is_some_and(std::process::ExitStatus::success)
                    && error.is_none()
                    && output.storage.bytes_written() > 0;
            let path = output.storage.path().to_path_buf();
            // A Matroska file FFmpeg finished gets its length and index;
            // one cut short, at its maximum size or by a failed write,
            // stays as it was cut.
            if valid
                && cut.is_none()
                && let Some(head) = &output.head
                && let Err(failure) = output
                    .storage
                    .amend(|file, room| mkv::finish(file, head, room))
            {
                error.get_or_insert_with(|| format!("recording finalization failed: {failure:#}"));
            }
            let result = if valid {
                output.storage.finish().map(|bytes| {
                    let recorded = crate::engine::saved::RecordedFile {
                        path: path.display().to_string(),
                        bytes,
                        // A copy's video starts with its first frame.
                        started_ms: match process.queue.first_copied_ms.load(Ordering::Relaxed) {
                            0 => process.started_ms,
                            first => first,
                        },
                        ended_ms: crate::scheduling::now_ms(),
                        codec: self.config.codec.clone(),
                        encoder: process.encoder,
                        truncated: full_size,
                        chunks: process
                            .queue
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .chunks
                            .clone(),
                    };
                    self.final_stats.recording = Some(recorded);
                })
            } else {
                output.storage.abort()
            };
            if let Err(failure) = result {
                error.get_or_insert_with(|| format!("recording finalization failed: {failure:#}"));
            }
        }
        if error.is_none() && !full_size {
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
        self.final_stats.error.clone_from(&error);
        if let Some(error) = error {
            let tag = process
                .queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .error_tag
                .clone();
            let failure = crate::types::error_from_tag(tag, error);
            let log = process.log.text(&self.config.output);
            return if log.trim().is_empty() {
                Err(failure)
            } else {
                Err(failure.context(log.trim().to_owned()))
            };
        }
        Ok(())
    }
}
/// Copy `FFmpeg`'s encoded `output` into the recording's `storage`, through
/// a Matroska file's `start` rewriter when there is one, telling `wrote`
/// the bytes written after each read.
fn copy_recording(
    output: &mut impl Read,
    storage: &mut crate::storage::Recording,
    mut start: Option<&mut mkv::Rewriter>,
    mut wrote: impl FnMut(u64),
) -> std::io::Result<()> {
    let mut buffer = vec![0; 64 * 1024];
    let read = loop {
        let count = match output.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(error),
        };
        match start.as_deref_mut() {
            Some(start) => storage.write_all(&start.push(&buffer[..count]))?,
            None => storage.write_all(&buffer[..count])?,
        }
        wrote(storage.bytes_written());
    };
    // A start still held goes as it is.
    if let Some(start) = start {
        storage.write_all(&start.flush())?;
    }
    read
}
/// When a recording with a maximum size must end so that `FFmpeg` can
/// finish its file within it. `FFmpeg` writes a container a part at a time
/// (an MP4 fragment, a key frame to the next; a Matroska cluster), so it is
/// judged after each write: it ends once the file could not take another
/// part as large as the largest burst of bytes so far, with a quarter more,
/// nor a second more of its output, nor `NEAR_END_MIN`, which also cover
/// the frames on their way through `FFmpeg` and its trailer. A file `FFmpeg`
/// still overruns is cut at the limit and reported as such
/// (`RecordedFile::truncated`).
struct NearEnd {
    limit: u64,
    burst: u64,
    largest: u64,
    first: Option<Instant>,
    last: Option<(Instant, u64)>,
}
/// The least a recording with a maximum size keeps for `FFmpeg` to finish.
const NEAR_END_MIN: u64 = 256 * 1024;
/// Writes this close together belong to one burst.
const BURST_GAP: Duration = Duration::from_millis(20);
impl NearEnd {
    const fn new(limit: u64) -> Self {
        Self {
            limit,
            burst: 0,
            largest: 0,
            first: None,
            last: None,
        }
    }
    /// `written` bytes are in the file now: whether it is time to end.
    fn reached(&mut self, written: u64) -> bool {
        self.reached_at(written, Instant::now())
    }
    fn reached_at(&mut self, written: u64, now: Instant) -> bool {
        let first = *self.first.get_or_insert(now);
        let added = match self.last {
            Some((at, before)) if now.duration_since(at) < BURST_GAP => {
                self.burst.saturating_add(written.saturating_sub(before))
            }
            Some((_, before)) => written.saturating_sub(before),
            None => written,
        };
        self.burst = added;
        self.largest = self.largest.max(self.burst);
        self.last = Some((now, written));
        // Bytes a second, over at least one.
        let elapsed = now.duration_since(first).as_nanos().max(1_000_000_000);
        let per_second = u64::try_from(u128::from(written) * 1_000_000_000 / elapsed)
            .expect("a rate over at least one second cannot exceed the bytes written");
        let held = self
            .largest
            .saturating_add(self.largest / 4)
            .max(per_second)
            .max(NEAR_END_MIN);
        written.saturating_add(held) >= self.limit
    }
}
/// How an encoder process ended: its exit status (`None` when it had to be
/// killed), the recording output its writer holds, and the first error any
/// of its threads reported.
type Finished = (
    Option<std::process::ExitStatus>,
    Option<EncodedOutput>,
    Option<String>,
);
/// Stop an encoder process and its threads.
fn finish(process: &mut EncoderProcess) -> Finished {
    // A copy's writer writes what it queued and the frames that complete
    // its display order, then lets the source's video go (`write_copied`).
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
                shutdown_error = Some(format!("cannot inspect FFmpeg forwarding process: {error}"));
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
    // Frames FFmpeg may not have read out of the pipe live until here,
    // where it has exited.
    let _held = match process.writer.take().map(JoinHandle::join) {
        Some(Ok(held)) => held,
        Some(Err(_)) => {
            shutdown_error
                .get_or_insert_with(|| "FFmpeg input writer terminated unexpectedly".to_owned());
            Vec::new()
        }
        None => Vec::new(),
    };
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
    let error = process
        .queue
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .error
        .clone()
        .or(shutdown_error);
    if let Some(thread) = process.log_thread.take() {
        let _ = thread.join();
    }
    (status, recording, error)
}
impl Drop for Forwarder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn log_collectors_retry_interruptions_and_stop_on_other_errors() {
        for lines in [false, true] {
            for kind in [
                std::io::ErrorKind::Interrupted,
                std::io::ErrorKind::PermissionDenied,
            ] {
                let log = Log::new();
                let reader = crate::test_io::FaultReader::new(
                    b"[error] first "
                        .as_slice()
                        .chain(b"message\n[error] last".as_slice()),
                    kind,
                );
                let collector = if lines {
                    log.collect_lines(reader)
                } else {
                    log.collect(reader)
                };
                collector.join().unwrap();
                let expected = match (kind, lines) {
                    (std::io::ErrorKind::Interrupted, true) => "first message\nlast\n",
                    (std::io::ErrorKind::Interrupted, false) => {
                        "[error] first message\n[error] last"
                    }
                    _ => "",
                };
                assert_eq!(log.text(""), expected);
            }
        }
    }

    #[test]
    fn log_description_survives_interrupted_reads() {
        let log = Log::new();
        let reader = crate::test_io::FaultReader::new(
            b"[info] Output #0, rawvideo, to 'pipe:1':\n[info] Stream #0:0: Video: raw"
                .as_slice()
                .chain(b"video, gray, 33x17, 1 fps\n".as_slice()),
            std::io::ErrorKind::Interrupted,
        );
        log.collect_lines(reader).join().unwrap();
        let output = log
            .wait_described(&AtomicBool::new(false), Duration::ZERO)
            .unwrap();
        assert_eq!((output.width, output.height), (33, 17));
        assert!(log.output.lock().unwrap().1);
    }

    #[test]
    fn ppm_comment_limits_are_checked_before_reading_the_whole_line() {
        let mut exact = vec![b'#'];
        exact.extend(std::iter::repeat_n(b'x', 4095));
        exact.extend_from_slice(b"\nP6 1 1 255\n");
        assert_eq!(
            ppm_token(&mut BufReader::new(exact.as_slice())).unwrap(),
            b"P6"
        );
        let mut excessive = vec![b'#'];
        excessive.extend(std::iter::repeat_n(b'x', 16_384));
        excessive.extend_from_slice(b"\nP6 1 1 255\n");
        let mut reader = BufReader::with_capacity(32, std::io::Cursor::new(excessive));
        let error = ppm_token(&mut reader).unwrap_err();
        assert!(error.to_string().contains("PPM comment is too long"));
        assert!(
            reader.get_ref().position() <= 4_128,
            "oversized comment read too far"
        );
    }

    use super::*;

    use crate::genicam::NodeMap;
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-media-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    /// A stand-in for `FFmpeg` on this test thread: a shell script that can
    /// keep notes in `$dir`.
    #[cfg(unix)]
    struct Fake {
        dir: PathBuf,
    }
    #[cfg(unix)]
    impl Fake {
        fn new(name: &str, body: &str) -> Self {
            let dir = temp_dir(name);
            let path = dir.join("ffmpeg");
            let script = format!("#!/bin/sh\ndir='{}'\n{body}\n", dir.display());
            fs::write(dir.join("script"), script).unwrap();
            std::os::unix::fs::symlink(Self::launcher(), &path).unwrap();
            FAKE_FFMPEG.set(Some(Arc::new(Executable::external(path))));
            Self { dir }
        }
        /// The one executable every stand-in links to: it runs the `script`
        /// beside the link it was started through. macOS checks a new
        /// executable the first time it runs, which takes a second or more
        /// while other tests start theirs, longer than the waits the tests
        /// allow; this one is checked once, here, before any test waits.
        fn launcher() -> &'static std::path::Path {
            static LAUNCHER: OnceLock<PathBuf> = OnceLock::new();
            extern "C" fn remove() {
                if let Some(dir) = LAUNCHER.get().and_then(|path| path.parent()) {
                    let _ = fs::remove_dir_all(dir);
                }
            }
            LAUNCHER.get_or_init(|| {
                use std::os::unix::fs::PermissionsExt as _;
                let path = temp_dir("launcher").join("ffmpeg");
                fs::write(&path, "#!/bin/sh\nexec /bin/sh \"${0%/*}/script\" \"$@\"\n").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                // Its first run finds no script beside it and fails at once.
                let _ = Command::new(&path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                // SAFETY: `remove` only reads a set OnceLock and removes a
                // directory, after the tests have finished.
                unsafe { libc::atexit(remove) };
                path
            })
        }
        /// A stand-in that writes raw video as `native` reads it.
        fn native(name: &str, body: &str) -> Self {
            FAKE_NATIVE.set(true);
            Self::new(name, body)
        }
        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.dir.join(name)).unwrap_or_default()
        }
        /// Launches whose arguments contain every one of `parts`.
        fn launches(&self, parts: &[&str]) -> usize {
            self.read("argv")
                .lines()
                .filter(|line| parts.iter().all(|part| line.contains(part)))
                .count()
        }
    }
    #[cfg(unix)]
    impl Drop for Fake {
        fn drop(&mut self) {
            FAKE_FFMPEG.set(None);
            FAKE_NATIVE.set(false);
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
    #[cfg(unix)]
    const PAYLOAD: &[u8] = b"#!/bin/sh\necho stand-in for FFmpeg\n";
    #[test]
    #[cfg(unix)]
    fn ffmpeg_is_found_next_to_the_executable_then_on_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = temp_dir("installed");
        let [beside, unrelated, plain, bin] =
            ["beside", "unrelated", "plain", "bin"].map(|name| base.join(name));
        for (dir, mode) in [(&beside, 0o755), (&plain, 0o644), (&bin, 0o755)] {
            fs::create_dir_all(dir).unwrap();
            let file = dir.join(FFMPEG_NAME);
            fs::write(&file, PAYLOAD).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::create_dir_all(&unrelated).unwrap();
        // A relative entry and a file that cannot run are passed over.
        let path = std::env::join_paths([std::path::Path::new("beside"), &unrelated, &plain, &bin])
            .unwrap();
        assert_eq!(
            installed(Some(&beside), Some(&path)),
            Some((beside.join(FFMPEG_NAME), FfmpegSource::NextToExecutable))
        );
        assert_eq!(
            installed(Some(&unrelated), Some(&path)),
            Some((bin.join(FFMPEG_NAME), FfmpegSource::Path))
        );
        assert_eq!(installed(Some(&unrelated), None), None);
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn an_empty_ffmpeg_setting_is_unset_and_a_relative_one_refused() {
        assert_eq!(overridden(None).unwrap(), None);
        assert_eq!(overridden(Some("".into())).unwrap(), None);
        let absolute = std::env::temp_dir().join("ffmpeg");
        assert_eq!(
            overridden(Some(absolute.clone().into())).unwrap(),
            Some(absolute)
        );
        let error = overridden(Some("bin/ffmpeg".into())).unwrap_err();
        assert_eq!(crate::cli::error_code(&error), ("usage", 2), "{error:#}");
    }
    /// An `FFmpeg` that cannot start is `ffmpeg_missing` whichever command
    /// starts it: a recording's encoder tests and encoder too.
    #[test]
    #[cfg(unix)]
    fn an_ffmpeg_that_cannot_start_is_missing_for_recordings_too() {
        let dir = temp_dir("unstartable");
        FAKE_FFMPEG.set(Some(Arc::new(Executable::external(dir.join("ffmpeg")))));
        let error =
            timed_output(&executable().unwrap(), &[], &[], Duration::from_secs(1)).unwrap_err();
        assert_eq!(
            crate::cli::error_code(&error),
            ("ffmpeg_missing", 3),
            "{error:#}"
        );
        let mut forward = Forwarder::new(recording(&dir.join("out.mkv"), "libx264")).unwrap();
        let error = forward.push(tiny(16, 16)).unwrap_err();
        assert_eq!(
            crate::cli::error_code(&error),
            ("ffmpeg_missing", 3),
            "{error:#}"
        );
        drop(forward);
        FAKE_FFMPEG.set(None);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_missing_ffmpeg_is_typed_and_says_how_to_get_one() {
        for error in [
            ffmpeg_missing(),
            ffmpeg_missing().context(FfmpegUnavailable),
            anyhow!("No such file or directory").context(FfmpegUnavailable),
        ] {
            let classified = crate::types::classify(&error);
            assert_eq!(classified.kind.code(), ("ffmpeg_missing", 3), "{error:#}");
            let hint = classified.hint.unwrap();
            assert!(hint.contains("CAPTUREFAB_FFMPEG"), "{hint}");
        }
        // A build with FFmpeg built in that has nowhere to run it says
        // which folder would, not how to install FFmpeg.
        let nowhere = Categorized::new(
            ErrorKind::FfmpegMissing,
            "cannot extract the bundled FFmpeg to a folder that runs programs",
        )
        .hint("set CAPTUREFAB_CACHE_DIR to a writable folder that allows running programs");
        let error = anyhow::Error::from(nowhere)
            .context(FfmpegUnavailable)
            .context("open camera");
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind.code(), ("ffmpeg_missing", 3));
        assert!(
            classified
                .hint
                .unwrap()
                .starts_with("set CAPTUREFAB_CACHE_DIR"),
            "{error:#}"
        );
    }
    /// A source the hardware cannot open is decoded in software at once,
    /// and a codec it decoded in software (frames that are not NV12) is
    /// left to software when the stream comes back.
    #[test]
    #[cfg(unix)]
    fn hardware_decoding_falls_back_to_software_per_source() {
        hwdec::SETTING.set(Some(hwdec::Setting::Named("cuda")));
        let describe = |pix_fmt: &str| {
            format!(
                "[info]   Stream #0:0: Video: h264 (High), yuv420p, 4x2, 30 fps\\n[info] Output #0, rawvideo, to 'pipe:1':\\n[info]   Stream #0:0: Video: rawvideo, {pix_fmt}(tv, bt709), 4x2\\n"
            )
        };
        let fake = Fake::native(
            "hwdec-open",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-hwaccels*) printf 'Hardware acceleration methods:\ncuda\n' ;;
*hwdownload*) cat > /dev/null ;;
*"-hwaccel cuda"*) printf '[info]   Stream #0:0: Video: h264 (High), yuv420p, 4x2\n[error] Device creation failed\n' >&2; exit 1 ;;
*) printf "{}" >&2; while :; do head -c 12 /dev/zero; sleep 0.05; done ;;
esac"#,
                describe("yuv420p")
            ),
        );
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(12).unwrap();
        let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(
            crate::frame::frame_format_name(&frame),
            "YCbCr709_420_8_Planar"
        );
        // The probe, the launch that failed, and one in software.
        assert_eq!(fake.launches(&["hwdownload"]), 1);
        assert_eq!(fake.launches(&["-hwaccel cuda", "-i rtsp"]), 1);
        assert_eq!(fake.launches(&["-f rawvideo"]), 2);
        drop(camera);
        drop(fake);
        // A stream that sends no frame in time would send none in software
        // either: it is not opened again.
        let fake = Fake::native(
            "hwdec-stall",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-hwaccels*) printf 'Hardware acceleration methods:\ncuda\n' ;;
*hwdownload*) cat > /dev/null ;;
*) printf "{}" >&2; exec sleep 10 ;;
esac"#,
                describe("nv12")
            ),
        );
        let Err(error) = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(1)) else {
            panic!("a stream that sent nothing opened");
        };
        assert!(format!("{error:#}").contains("within 1 s"), "{error:#}");
        assert_eq!(fake.launches(&["-i rtsp"]), 1);
        drop(fake);
        // The hardware opens the stream but FFmpeg decodes it in software.
        let fake = Fake::native(
            "hwdec-software",
            &format!(
                r#"echo "$*" >> "$dir/argv"
n=$(($(cat "$dir/launches" 2>/dev/null || echo 0) + 1))
case "$*" in
*-hwaccels*) printf 'Hardware acceleration methods:\ncuda\n' ; exit 0 ;;
*hwdownload*) cat > /dev/null ; exit 0 ;;
esac
echo $n > "$dir/launches"
printf "{}" >&2
for i in 1 2 3; do head -c 12 /dev/zero; sleep 0.05; done"#,
                describe("yuv420p")
            ),
        );
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(12).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while fake.read("launches").trim() != "2" {
            assert!(Instant::now() < deadline, "the stream did not come back");
            let _ = camera.next_frame(Duration::from_millis(200));
        }
        let launches: Vec<String> = fake
            .read("argv")
            .lines()
            .filter(|line| line.contains("-i rtsp"))
            .map(str::to_owned)
            .collect();
        // Not yet proven, the first launch keeps FFmpeg's decoder threads,
        // which the software decoding FFmpeg fell back to then has.
        assert!(launches[0].contains("-hwaccel cuda"), "{launches:?}");
        assert!(!launches[0].contains("-threads 1 "), "{launches:?}");
        assert!(!launches[1].contains("-hwaccel"), "{launches:?}");
        drop(camera);
        drop(fake);
        // Once the hardware has decoded the stream (NV12 frames), it does
        // so on one decoder thread.
        let fake = Fake::native(
            "hwdec-proven",
            &format!(
                r#"echo "$*" >> "$dir/argv"
n=$(($(cat "$dir/launches" 2>/dev/null || echo 0) + 1))
case "$*" in
*-hwaccels*) printf 'Hardware acceleration methods:\ncuda\n' ; exit 0 ;;
*hwdownload*) cat > /dev/null ; exit 0 ;;
esac
echo $n > "$dir/launches"
printf "{}" >&2
for i in 1 2 3; do head -c 12 /dev/zero; sleep 0.05; done"#,
                describe("nv12")
            ),
        );
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(12).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while fake.read("launches").trim() != "2" {
            assert!(Instant::now() < deadline, "the stream did not come back");
            let _ = camera.next_frame(Duration::from_millis(200));
        }
        let launches: Vec<String> = fake
            .read("argv")
            .lines()
            .filter(|line| line.contains("-i rtsp"))
            .map(str::to_owned)
            .collect();
        assert!(!launches[0].contains("-threads 1 "), "{launches:?}");
        assert!(
            launches[1].contains("-hwaccel cuda -threads 1"),
            "{launches:?}"
        );
        hwdec::SETTING.set(None);
    }

    /// Under `auto` a stream is decoded in hardware only by a session's
    /// camera worker, whose decoder runs on between commands; a command's
    /// own worker decodes its one frame sooner in software.
    #[test]
    #[cfg(unix)]
    fn auto_decodes_streams_in_hardware_only_in_sessions() {
        let Some(&method) = hwdec::candidates().first() else {
            return;
        };
        hwdec::SETTING.set(Some(hwdec::Setting::Auto));
        let fake = Fake::native(
            "hwdec-auto",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-hwaccels*) printf 'Hardware acceleration methods:\n{method}\n' ; exit 0 ;;
*hwdownload*) cat > /dev/null ; exit 0 ;;
esac
printf "[info] Output #0, rawvideo, to 'pipe:1':\n[info]   Stream #0:0: Video: rawvideo, nv12(tv, bt709), 4x2\n" >&2
while :; do head -c 12 /dev/zero; sleep 0.05; done"#
            ),
        );
        let hardware = format!("-hwaccel {method}");
        for (lasting, launches) in [(false, 0), (true, 1)] {
            hwdec::LASTING.set(Some(lasting));
            let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
            camera.start(12).unwrap();
            camera.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!(
                fake.launches(&[&hardware, "-i rtsp"]),
                launches,
                "{lasting}"
            );
        }
        hwdec::LASTING.set(None);
        hwdec::SETTING.set(None);
    }

    /// A probe whose `FFmpeg` never reads its input still ends at its
    /// deadline: the input is written beside the wait, not before it.
    #[test]
    #[cfg(unix)]
    fn probes_end_at_their_deadline_when_ffmpeg_reads_nothing() {
        let fake = Fake::new("stalled", "exec sleep 30");
        let exe = FAKE_FFMPEG.with_borrow(Clone::clone).unwrap();
        let started = Instant::now();
        let (success, _, timed_out) =
            timed_output(&exe, &[], &vec![0; 8 << 20], Duration::from_millis(300)).unwrap();
        assert!(!success && timed_out);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(fake);
    }
    /// Raw video is read at the size and format `FFmpeg`'s log describes,
    /// one fixed number of bytes a frame.
    #[test]
    #[cfg(unix)]
    fn native_frames_are_read_as_ffmpeg_describes_them() {
        let fake = Fake::native(
            "native",
            r#"echo "$*" >> "$dir/argv"
printf "[info] Output #0, rawvideo, to 'pipe:1':\n[info]   Stream #0:0: Video: rawvideo (I420 / 0x30323449), yuv420p(tv, bt709, progressive), 4x2, 30 fps\n" >&2
for i in 1 2 3; do head -c 12 /dev/zero | tr '\0' "\00$i"; sleep 0.02; done
sleep 5"#,
        );
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(3)).unwrap();
        camera.start(12).unwrap();
        for _ in 0..2 {
            let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!((frame.width, frame.height, frame.data.len()), (4, 2, 12));
            assert_eq!(
                crate::frame::frame_format_name(&frame),
                "YCbCr709_420_8_Planar"
            );
        }
        let registers = camera.read_memory(8, 8).unwrap();
        assert_eq!(registers[..4], 0x820c_0003u32.to_le_bytes());
        assert_eq!(registers[4..], 12u32.to_le_bytes());
        assert_eq!(
            fake.launches(&["-f rawvideo", "format=pix_fmts=gray|nv12"]),
            1
        );
    }
    /// A format Capturefab does not carry as decoded, or no description at
    /// all, has the source opened again for RGB frames.
    #[test]
    #[cfg(unix)]
    fn sources_decoded_to_other_formats_are_read_as_rgb() {
        for description in [
            "[info] Output #0, rawvideo, to 'pipe:1':\\n[info]   Stream #0:0: Video: rawvideo, yuv420p(tv, bt2020nc), 2x2\\n",
            "",
        ] {
            let fake = Fake::native(
                "rgb-again",
                &format!(
                    r#"echo "$*" >> "$dir/argv"
case "$*" in
*rawvideo*) printf "{description}" >&2; while :; do head -c 6 /dev/zero; sleep 0.05; done ;;
*) while :; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; sleep 0.05; done ;;
esac"#
                ),
            );
            let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
            camera.start(12).unwrap();
            let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!((frame.pixel_format, frame.data.len()), (RGB8, 12));
            assert_eq!(fake.launches(&["-f rawvideo"]), 1, "{description:?}");
            assert_eq!(fake.launches(&["-c:v ppm"]), 1, "{description:?}");
        }
    }
    #[test]
    #[cfg(unix)]
    fn dropped_network_stream_reconnects_with_backoff_without_spinning() {
        let fake = Fake::new(
            "reconnect",
            r#"case "$*" in *rtsp*|*"-f concat"*) ;; *) exit 0 ;; esac
n=$(($(cat "$dir/launches" 2>/dev/null || echo 0) + 1)); echo $n > "$dir/launches"
frame() { printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; }
case $n in
1) frame; exit 0 ;;
2) echo 'Connection refused' >&2; exit 1 ;;
*) while frame; do sleep 0.05; done ;;
esac"#,
        );
        let mut camera = open_url(
            "rtsp://user:secret@127.0.0.1:9/live",
            Duration::from_secs(2),
        )
        .unwrap();
        camera.start(12).unwrap();
        assert_eq!(
            camera.next_frame(Duration::from_millis(100)).unwrap().width,
            2
        );
        let lost = camera.next_frame(Duration::from_millis(100)).unwrap_err();
        assert_eq!(stream_end(&lost), Some(StreamEnd::Lost));
        let message = format!("{lost:#}");
        assert!(
            message.starts_with(
                "reconnecting to rtsp://<redacted>@127.0.0.1:9/live (attempt 1): timed out"
            ),
            "{message}"
        );
        assert_eq!(crate::cli::error_code(&lost), ("disconnected", 3));
        // Until a relaunch is due, each poll waits out its budget instead of
        // failing at once; the first relaunch is refused, the next streams.
        let started = Instant::now();
        let mut polls = 0u128;
        let frame = loop {
            match camera.next_frame(Duration::from_millis(100)) {
                Ok(frame) => break frame,
                Err(error) => {
                    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
                    assert!(!format!("{error:#}").contains("secret"));
                    assert_eq!(stream_end(&error), None);
                    polls += 1;
                }
            }
            assert!(started.elapsed() < Duration::from_secs(10));
        };
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(2500), "{elapsed:?}");
        assert!(
            polls <= elapsed.as_millis() / 80 + 2,
            "{polls} polls in {elapsed:?}"
        );
        assert_eq!(frame.width, 2);
        assert_eq!(fake.read("launches").trim(), "3");
        camera.stop().unwrap();
    }
    #[test]
    fn first_open_failures_explain_ffmpeg_in_one_sentence() {
        let wait = Duration::from_secs(2);
        let source = "rtsp://user:pw@127.0.0.1:1/x";
        let explained = |source: &str, log: &str, timed_out: bool| {
            let error = network_failure(source, log, timed_out, wait).expect(log);
            (crate::cli::error_code(&error).0, format!("{error:#}"))
        };
        let refused = "[tcp @ 0x7b8820000] Connection to tcp://127.0.0.1:1?timeout=2000000 failed: Connection refused\n[in#0 @ 0x993014000] Error opening input: Connection refused\nError opening input file rtsp://<redacted>@127.0.0.1:1/x.";
        let (code, message) = explained(source, refused, false);
        assert_eq!(code, "unavailable");
        assert_eq!(
            message,
            "Nothing accepted a connection at 127.0.0.1:1 (rtsp://<redacted>@127.0.0.1:1/x). Check the camera's stream port and that streaming is turned on. (FFmpeg: Connection to tcp://127.0.0.1:1 failed: Connection refused)"
        );
        // Without a port, RTSP's default is the likely fix.
        let (_, message) = explained("rtsp://10.0.0.5/live", "Connection refused", false);
        assert!(
            message.contains("RTSP normally uses port 554."),
            "{message}"
        );
        let (code, message) = explained(
            source,
            "[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized",
            false,
        );
        assert_eq!(code, "failed");
        assert!(
            message.starts_with("127.0.0.1 rejected the username or password (rtsp://<redacted>@127.0.0.1:1/x). Check them, or add them as rtsp://USER:PASS@127.0.0.1:1/x if none were given."),
            "{message}"
        );
        let (code, message) = explained(
            source,
            "[rtsp @ 0x1] method DESCRIBE failed: 404 Not Found",
            false,
        );
        assert_eq!(code, "unavailable");
        assert!(
            message.starts_with("127.0.0.1:1 answered but has no stream at /x"),
            "{message}"
        );
        assert!(message.contains("onvif://USER:PASS@127.0.0.1/onvif/device_service"));
        // An RTMP stream key never comes back in the suggested URL or path.
        for log in [
            "[rtmp @ 0x1] Server error: 401 Unauthorized",
            "[rtmp @ 0x1] stream not found",
        ] {
            let (_, message) = explained("rtmp://10.0.0.5/live/SECRETKEY", log, false);
            assert!(!message.contains("SECRETKEY"), "{message}");
        }
        let (code, message) = explained(
            "rtsp://nonexistent.invalid/live",
            "[tcp @ 0x2] Failed to resolve hostname nonexistent.invalid: nodename nor servname provided, or not known",
            false,
        );
        assert_eq!(code, "unavailable");
        assert!(
            message.starts_with("Cannot resolve nonexistent.invalid"),
            "{message}"
        );
        let (code, message) = explained(
            "srt://203.0.113.9:9000",
            "Connection to tcp://203.0.113.9:9000 failed: No route to host",
            false,
        );
        assert_eq!(code, "unavailable");
        assert!(
            message.starts_with("203.0.113.9 is not reachable from this computer"),
            "{message}"
        );
        // Silence within the wait is a timeout, worded for a stream that
        // may be merely slow: FFmpeg logs only errors.
        let (code, message) = explained("rtsp://user:pw@10.255.255.1/stream", "", true);
        assert_eq!(code, "timeout");
        assert_eq!(
            message,
            "No video arrived from 10.255.255.1 within 2 s (rtsp://<redacted>@10.255.255.1/stream). It may be unreachable or slow to send its first frame; retry with a longer wait (capture and record take --timeout 10s)."
        );
        // A connect that timed out says the host did not reply.
        let (code, message) = explained(
            "rtsp://10.255.255.1/stream",
            "[tcp @ 0x3] Connection to tcp://10.255.255.1:554?timeout=2000000 failed: Operation timed out",
            false,
        );
        assert_eq!(code, "timeout");
        assert!(
            message.starts_with("No reply from 10.255.255.1 (rtsp://10.255.255.1/stream). Check it is on this network and streaming, or retry with a longer wait (capture and record take --timeout 10s). (FFmpeg: Connection to tcp://10.255.255.1:554 failed: Operation timed out)"),
            "{message}"
        );
        // A longer wait is suggested only while one is left to try.
        assert_eq!(longer_wait(Duration::from_secs(20)), Some(40_000));
        assert_eq!(longer_wait(Duration::from_secs(40)), Some(60_000));
        assert_eq!(longer_wait(Duration::from_secs(60)), None);
        let ((), message) = network_failure(source, "", true, Duration::from_secs(60))
            .map(|error| ((), format!("{error:#}")))
            .unwrap();
        assert!(
            message.ends_with("It may be unreachable or slow to send its first frame; check it is on this network and streaming."),
            "{message}"
        );
        for (log, timed_out) in [
            (refused, false),
            ("", true),
            ("method DESCRIBE failed: 401 Unauthorized", false),
        ] {
            let (_, message) = explained(source, log, timed_out);
            for internal in ["@ 0x", "?timeout=", "user:pw", "\n"] {
                assert!(!message.contains(internal), "{message}");
            }
        }
        // Anything else keeps FFmpeg's own error.
        assert!(
            network_failure(
                source,
                "Invalid data found when processing input",
                false,
                wait
            )
            .is_none()
        );
        assert!(network_failure(source, "", false, wait).is_none());
        assert!(network_failure("file:/tmp/x.mp4", refused, false, wait).is_none());
    }
    #[test]
    #[cfg(unix)]
    fn a_refused_first_open_says_so_in_one_line() {
        let _fake = Fake::new(
            "first-open-refused",
            r#"case "$*" in *rtsp*|*"-f concat"*) ;; *) exit 0 ;; esac
echo '[tcp @ 0x7b8820000] Connection to tcp://127.0.0.1:1?timeout=2000000 failed: Connection refused' >&2
exit 1"#,
        );
        let Err(error) = open_url("rtsp://user:secret@127.0.0.1:1/x", Duration::from_secs(2))
        else {
            panic!("a refused stream opened");
        };
        let message = format!("{error:#}");
        assert_eq!(
            crate::cli::error_code(&error),
            ("unavailable", 3),
            "{message}"
        );
        assert!(
            message.starts_with("Nothing accepted a connection at 127.0.0.1:1"),
            "{message}"
        );
        assert!(
            !message.contains("secret") && !message.contains('\n'),
            "{message}"
        );
    }
    #[test]
    fn reconnecting_stops_after_repeated_refusals() {
        let mut camera = native("rtsp://camera/live", Vec::new());
        camera.native = false;
        let lost = camera.lost("media stream ended: 401 Unauthorized");
        assert_eq!(stream_end(&lost), Some(StreamEnd::Lost));
        let refusal = "media stream ended: method DESCRIBE failed: 401 Unauthorized";
        for attempt in 2..=3 {
            let waiting = camera.lost(refusal);
            assert_eq!(stream_end(&waiting), None);
            assert!(format!("{waiting}").contains(&format!("(attempt {attempt}): timed out")));
        }
        // An unrelated failure resets the count; backoff keeps doubling
        // up to its limit.
        camera.lost("media stream ended: Connection refused");
        let backoff = camera.reconnect.as_ref().unwrap().backoff;
        assert_eq!(backoff, Duration::from_secs(16));
        for _ in 1..MAX_REFUSED {
            assert_eq!(stream_end(&camera.lost(refusal)), None);
        }
        assert_eq!(
            camera.reconnect.as_ref().unwrap().backoff,
            RECONNECT_BACKOFF_MAX
        );
        let abandoned = camera.lost(refusal);
        assert_eq!(stream_end(&abandoned), Some(StreamEnd::Abandoned));
        assert!(
            format!("{abandoned}")
                .starts_with("stopped reconnecting to rtsp://camera/live after 8 attempts")
        );
        assert!(camera.reconnect.is_none());
        // A relay answers 404 while its camera is away: keep trying.
        camera.lost("media stream ended: Connection refused");
        for _ in 0..MAX_REFUSED * 4 {
            let missing = "media stream ended: method DESCRIBE failed: 404 Not Found";
            assert_eq!(stream_end(&camera.lost(missing)), None);
        }
        assert_eq!(
            camera.reconnect.as_ref().unwrap().backoff,
            RECONNECT_BACKOFF_MAX
        );
    }
    #[test]
    #[cfg(unix)]
    fn rtsp_sign_in_requests_and_refusals_are_typed() {
        let fake = Fake::new(
            "auth",
            r#"echo "$*" >> "$dir/argv"
script=""
case "$*" in *"-f concat -safe 0 -i pipe:0"*) script=$(cat); printf '%s\n' "$script" >> "$dir/stdin" ;; esac
case "$*$script" in
  *missing*) echo '[rtsp @ 0x1] method DESCRIBE failed: 404 Not Found' >&2 ;;
  *blocked*) echo '[rtsp @ 0x1] method DESCRIBE failed: 403 Forbidden' >&2 ;;
  *) echo '[rtsp @ 0x1] method DESCRIBE failed: 401 Unauthorized' >&2 ;;
esac
exit 1"#,
        );
        let timeout = Duration::from_secs(10);
        let code = |result: Result<Box<dyn Backend>>| {
            let error = result.err().expect("open fails");
            (
                crate::types::error_transport_code(&error),
                format!("{error:#}"),
            )
        };
        let (kind, text) = code(open_url("rtsp://cam.local/live", timeout));
        assert_eq!(kind, Some("auth_required"), "{text}");
        assert!(text.contains("rtsp://cam.local/live"), "{text}");
        // TCP first, then UDP for a camera that answers 461 to TCP.
        assert_eq!(
            fake.launches(&["-rtsp_flags prefer_tcp -rtsp_transport tcp+udp -timeout"]),
            1
        );
        let login = crate::logins::CameraLogin::new("ad min", "s3cret:pw").unwrap();
        let (kind, text) = code(open_url_with(
            "rtsp://cam.local/live",
            Some(&login),
            timeout,
        ));
        assert_eq!(kind, Some("auth_rejected"), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
        // The password reaches FFmpeg on its stdin, never in its
        // arguments, which every local user can read.
        assert_eq!(fake.launches(&["s3cret"]), 0);
        let script = fake.read("stdin");
        assert!(
            script.contains("file 'rtsp://ad%20min:s3cret%3Apw@cam.local/live'\n")
                && script.contains("option rtsp_transport 'tcp+udp'\n")
                && script.contains("option timeout '10000000'\n"),
            "{script}"
        );
        let (kind, text) = code(open_url("rtsp://admin:typed-pw@cam.local/live", timeout));
        assert_eq!(kind, Some("auth_rejected"), "{text}");
        assert!(!text.contains("typed-pw"), "{text}");
        assert_eq!(fake.launches(&["typed-pw"]), 0);
        assert!(fake.read("stdin").contains("admin:typed-pw@"));
        // FFmpeg keeps 127 bytes of `user:password`, percent-encoded.
        let fits = format!("{}a", "!".repeat(40));
        let login = crate::logins::CameraLogin::new("admin", &fits).unwrap();
        let (kind, _) = code(open_url_with(
            "rtsp://cam.local/live",
            Some(&login),
            timeout,
        ));
        assert_eq!(kind, Some("auth_rejected"));
        assert!(
            fake.read("stdin")
                .contains(&format!("admin:{}a@", "%21".repeat(40)))
        );
        let long = format!("{}ab", "!".repeat(40));
        let login = crate::logins::CameraLogin::new("admin", &long).unwrap();
        let (kind, text) = code(open_url_with(
            "rtsp://cam.local/live",
            Some(&login),
            timeout,
        ));
        assert_eq!(kind, Some("unsupported"), "{text}");
        assert!(text.contains("rtsp://cam.local/live") && text.contains("128 bytes"));
        assert!(!text.contains("%21"), "{text}");
        assert_eq!(fake.launches(&["ab@cam.local"]), 0);
        assert!(!fake.read("stdin").contains("ab@cam.local"));
        for other in ["rtsp://cam.local/missing", "rtsp://cam.local/blocked"] {
            // Not a sign-in problem; the connection diagnosis may still
            // categorize it (a 404 is unavailable).
            let (kind, text) = code(open_url(other, timeout));
            assert!(
                !matches!(kind, Some("auth_required" | "auth_rejected")),
                "{text}"
            );
        }
    }
    #[test]
    fn a_stream_that_drops_again_soon_keeps_backing_off() {
        let mut camera = native("rtsp://camera/live", Vec::new());
        camera.native = false;
        let first = camera.lost("media stream ended");
        assert_eq!(stream_end(&first), Some(StreamEnd::Lost));
        // One frame, then the stream ends again: reported, with the backoff
        // carried on instead of starting again from 1 s.
        camera.delivered();
        assert!(camera.reconnect.is_some());
        let again = camera.lost("media stream ended");
        assert_eq!(stream_end(&again), Some(StreamEnd::Lost));
        assert!(
            format!("{again}").contains("(attempt 2): timed out"),
            "{again}"
        );
        let reconnect = camera.reconnect.as_ref().unwrap();
        assert_eq!(reconnect.backoff, Duration::from_secs(4));
        assert!(reconnect.retry_at > Instant::now() + Duration::from_secs(1));
        // A stream that stays up ends the outage.
        camera.delivered();
        camera.reconnect.as_mut().unwrap().resumed = Instant::now().checked_sub(STEADY);
        camera.delivered();
        assert!(camera.reconnect.is_none());
        let fresh = camera.lost("media stream ended");
        assert!(
            format!("{fresh}").contains("(attempt 1): timed out"),
            "{fresh}"
        );
    }
    #[test]
    #[cfg(unix)]
    fn video_files_play_in_real_time_and_finish() {
        let fake = Fake::new(
            "file",
            r#"echo "$*" >> "$dir/argv"
case "$*" in *clip.mkv*) ;; *) exit 0 ;; esac
for i in 1 2 3; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; done"#,
        );
        let clip = fake.dir.join("clip.mkv");
        fs::write(&clip, b"").unwrap();
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(2)).unwrap();
        let argv = fake.read("argv");
        let launch = argv.lines().find(|line| line.contains("clip.mkv")).unwrap();
        let input = launch.find(" -i ").unwrap();
        assert!(
            launch.find("-readrate 1").is_some_and(|at| at < input),
            "{launch}"
        );
        assert!(
            launch.find("-threads ").is_some_and(|at| at < input),
            "{launch}"
        );
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        assert_eq!(
            nodes.get(camera.as_mut(), "PlaybackSpeed").unwrap(),
            "RealTime"
        );
        camera.start(12).unwrap();
        for _ in 0..3 {
            camera.next_frame(Duration::from_secs(2)).unwrap();
        }
        for _ in 0..2 {
            let finished = camera.next_frame(Duration::from_secs(2)).unwrap_err();
            assert_eq!(stream_end(&finished), Some(StreamEnd::Finished));
            assert!(format!("{finished}").starts_with("video file finished"));
        }
        // Fastest decodes every frame unpaced, for extraction.
        camera.stop().unwrap();
        nodes
            .set(camera.as_mut(), "PlaybackSpeed", "Fastest")
            .unwrap();
        camera.start(12).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        let relaunch = fake.read("argv");
        assert!(!relaunch.lines().last().unwrap().contains("-readrate"));
        camera.stop().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn captures_read_video_files_as_fast_as_they_ask() {
        let fake = Fake::new(
            "on-demand",
            r#"case "$*" in *clip.mkv*) echo "$*" >> "$dir/argv" ;; *) exit 0 ;; esac
for i in 1 2 3; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; done"#,
        );
        let clip = fake.dir.join("clip.mkv");
        fs::write(&clip, b"").unwrap();
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(2)).unwrap();
        let paced = |line: &str| line.contains("-readrate 1");
        // A capture's own start: the file starts again unpaced, once.
        camera.start(12).unwrap();
        for _ in 0..3 {
            camera.request_frame().unwrap();
            camera.next_frame(Duration::from_secs(2)).unwrap();
        }
        let launches = fake.read("argv");
        let launches = launches.lines().collect::<Vec<_>>();
        assert_eq!(launches.len(), 2, "{launches:?}");
        assert!(paced(launches[0]) && !paced(launches[1]), "{launches:?}");
        camera.stop().unwrap();
        // Captures while it streams keep its pace.
        camera.start(12).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        let launches = fake.read("argv");
        assert_eq!(launches.lines().count(), 3, "{launches}");
        assert!(paced(launches.lines().last().unwrap()), "{launches}");
        camera.stop().unwrap();
        // One frame asked for is the one the file opened with: no second
        // FFmpeg.
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(2)).unwrap();
        camera.start(12).unwrap();
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(fake.read("argv").lines().count(), 4);
        // A second starts it again unpaced, past the frame already taken.
        camera.request_frame().unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        let launches = fake.read("argv");
        assert_eq!(launches.lines().count(), 5, "{launches}");
        assert!(!paced(launches.lines().last().unwrap()), "{launches}");
        let finished = camera.next_frame(Duration::from_secs(2)).unwrap_err();
        assert_eq!(stream_end(&finished), Some(StreamEnd::Finished));
        camera.stop().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn http_video_files_finish_and_http_streams_reconnect() {
        let _fake = Fake::new(
            "http",
            r#"case "$*" in *127.0.0.1*|*"-f concat"*) ;; *) exit 0 ;; esac
for i in 1 2; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; done"#,
        );
        for (url, end) in [
            (
                "http://127.0.0.1:9/media/clip.MP4?token=1",
                StreamEnd::Finished,
            ),
            ("http://127.0.0.1:9/video.mjpg", StreamEnd::Lost),
            ("http://127.0.0.1:9/", StreamEnd::Lost),
        ] {
            let mut camera = open_url(url, Duration::from_secs(2)).unwrap();
            camera.start(12).unwrap();
            // A file gives each of its two frames; a live stream its newest,
            // which can be the second by the time the first is asked for.
            let mut frames = 0;
            let ended = loop {
                match camera.next_frame(Duration::from_secs(2)) {
                    Ok(_) => frames += 1,
                    Err(error) => break error,
                }
                assert!(frames <= 2, "{url}: a third frame");
            };
            match end {
                StreamEnd::Finished => assert_eq!(frames, 2, "{url}"),
                _ => assert!(frames >= 1, "{url}: no frame"),
            }
            assert_eq!(stream_end(&ended), Some(end), "{url}: {ended:#}");
            if end == StreamEnd::Finished {
                assert_eq!(
                    format!("{ended}"),
                    "video file finished: http://127.0.0.1:9/media/clip.MP4?token=<redacted>"
                );
            }
            camera.stop().unwrap();
        }
    }
    #[test]
    #[cfg(unix)]
    fn replay_decoders_limit_threads_on_the_decoder() {
        let fake = Fake::new("replay", r#"echo "$*" >> "$dir/argv""#);
        let clip = fake.dir.join("clip.mkv");
        fs::write(&clip, b"").unwrap();
        let decoder = ReplayDecoder::open(&clip, 64, 48, 30.0, false, Duration::from_millis(200));
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.read("argv").is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        drop(decoder);
        let launch = fake.read("argv");
        let input = launch.find(" -i ").unwrap();
        assert!(
            launch.find("-threads 1").is_some_and(|at| at < input),
            "{launch}"
        );
        assert!(
            launch
                .find("-filter_threads 1")
                .is_some_and(|at| at > input),
            "{launch}"
        );
        assert_eq!(launch.matches("-threads ").count(), 1, "{launch}");
        assert!(launch.contains("-hwaccel none"), "{launch}");
        assert!(
            launch.trim_end().ends_with("-avioflags direct pipe:1"),
            "{launch}"
        );
    }
    #[test]
    fn decoders_and_software_encoders_are_sized_to_the_host() {
        assert_eq!(decoder_threads(16), 4);
        assert_eq!(decoder_threads(2), 2);
        assert_eq!(decoder_threads(0), 1);
        // 1080p30 is 62 Mpx/s.
        assert_eq!(x264_preset(1920, 1080, 30.0, 14), "veryfast");
        assert_eq!(x264_preset(1920, 1080, 30.0, 4), "superfast");
        assert_eq!(x264_preset(1920, 1080, 30.0, 2), "ultrafast");
    }
    #[test]
    fn probe_results_persist_for_the_boot_and_ignore_corrupt_files() {
        let Some(boot) = boot_id() else {
            return;
        };
        let dir = temp_dir("probes");
        let path = dir.join("probes.json");
        let exe = Executable {
            path: dir.join("ffmpeg"),
            probes: Some(path.clone()),
            _in_use: None,
        };
        remember(&exe, |probes| {
            probes
                .encoders
                .insert("libx264 yuv 64x48".into(), Found::now(true));
        });
        // Another process starts from the file.
        let stored = load_probes(&exe);
        assert_eq!(stored.boot, boot);
        let found = stored.encoders.get("libx264 yuv 64x48").copied();
        assert_eq!(found.and_then(Found::current), Some(true));
        let mut previous = stored;
        previous.boot = "an earlier boot".into();
        fs::write(&path, serde_json::to_vec(&previous).unwrap()).unwrap();
        assert!(load_probes(&exe).encoders.is_empty());
        fs::write(&path, b"{not json").unwrap();
        assert!(load_probes(&exe).encoders.is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_probes_expire_and_timed_out_probes_are_not_remembered() {
        let dir = temp_dir("probe-failures");
        let exe = Executable {
            path: dir.join("ffmpeg"),
            probes: Some(dir.join("probes.json")),
            _in_use: None,
        };
        // The answer, and whether the probe ran to give it.
        let probe = |key: &str, answer: Option<bool>| {
            let mut ran = false;
            let works = probed(
                &exe,
                |probes| probes.encoders.get(key).copied(),
                |probes, found| {
                    probes.encoders.insert(key.into(), found);
                },
                || {
                    ran = true;
                    Ok(answer)
                },
            )
            .unwrap();
            (works, ran)
        };
        // A probe that timed out tells nothing, so it runs again next time.
        assert_eq!(probe("slow", None), (None, true));
        assert_eq!(probe("slow", Some(true)), (Some(true), true));
        assert_eq!(probe("slow", None), (Some(true), false));
        // A failure is believed for a while, then probed again.
        assert_eq!(probe("busy", Some(false)), (Some(false), true));
        assert_eq!(probe("busy", Some(true)), (Some(false), false));
        with_probes(&exe, |probes| {
            probes.encoders.get_mut("busy").unwrap().at_ms -=
                u64::try_from(FAILURE_KEPT.as_millis()).unwrap();
        });
        assert_eq!(probe("busy", Some(true)), (Some(true), true));
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    fn tiny(width: u32, height: u32) -> Frame {
        Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width,
            height,
            pixel_format: RGB8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: vec![7; (width * height * 3) as usize].into(),
            jpeg: None,
        }
    }
    #[cfg(unix)]
    fn stream(encoder: &str, fps: f64) -> ForwardConfig {
        ForwardConfig {
            timeline: None,
            output: "tcp://127.0.0.1:9".into(),
            codec: "h264".into(),
            encoder: encoder.into(),
            fps,
            bitrate: "1M".into(),
            max_file_bytes: None,
            min_free: None,
        }
    }
    #[cfg(unix)]
    fn recording(path: &std::path::Path, encoder: &str) -> ForwardConfig {
        ForwardConfig {
            output: path.to_string_lossy().into(),
            ..stream(encoder, 30.0)
        }
    }
    #[test]
    #[cfg(unix)]
    fn recording_amendment_errors_are_reported_without_removing_video() {
        let fake = Fake::new(
            "amendment-error",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat "$dir/piped.mkv"; cat >/dev/null ;;
esac"#,
        );
        fs::write(fake.dir.join("piped.mkv"), mkv::tests::piped()).unwrap();
        let path = fake.dir.join("recording.mkv");
        let mut forward = Forwarder::new(recording(&path, "libx264")).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        let mut process = forward.process.take().unwrap();
        let mut finished = finish(&mut process);
        let output = finished.1.as_mut().unwrap();
        assert!(output.head.is_some());
        // Reads and syncing still work; only the metadata amendment fails.
        let readonly = fs::File::open(&path).unwrap();
        readonly.sync_all().unwrap();
        output
            .storage
            .amend(|file, _| {
                *file = readonly;
                Ok(())
            })
            .unwrap();
        let error = forward.settle(process, finished).unwrap_err();
        assert!(format!("{error:#}").contains("recording finalization failed"));
        assert!(path.metadata().unwrap().len() > 0);
        assert!(forward.stats().recording.is_some());
        assert!(forward.stats().error.is_some());
    }
    #[test]
    #[cfg(unix)]
    fn recordings_get_each_new_frame_once_on_the_wall_clock() {
        let fake = Fake::new(
            "unique",
            r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat ;;
esac"#,
        );
        let output = fake.dir.join("unique.mkv");
        let mut forward = Forwarder::new(recording(&output, "libx264")).unwrap();
        for _ in 0..3 {
            forward.push(tiny(8, 4)).unwrap();
            thread::sleep(Duration::from_millis(200));
        }
        assert_eq!(forward.stats().written, 3);
        assert!(!forward.stats().stalled);
        thread::sleep(STALL);
        assert!(forward.stats().stalled);
        assert_eq!(forward.stats().written, 3);
        forward.stop().unwrap();
        // No repeated frames reach FFmpeg, and nothing fills the stall: the
        // output keeps the time each frame arrived.
        assert_eq!(fs::read(&output).unwrap().len(), 3 * 8 * 4 * 3);
        assert_eq!(
            fake.launches(&[
                "-use_wallclock_as_timestamps 1 -i pipe:0",
                "-fps_mode vfr",
                "-preset ",
                "-threads ",
                "-x264-params force-cfr=0"
            ]),
            1
        );
        assert!(!fake.read("argv").contains(" -r "));
    }
    /// A stream gets each new frame once while frames come, even slower
    /// than its rate: `FFmpeg` repeats them to keep the rate.
    #[test]
    #[cfg(unix)]
    fn streams_get_each_frame_once_and_ffmpeg_keeps_their_rate() {
        let fake = Fake::new(
            "cfr",
            r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > "$dir/encoded" ;;
esac"#,
        );
        let mut forward = Forwarder::new(stream("libx264", 60.0)).unwrap();
        for _ in 0..20 {
            forward.push(tiny(8, 4)).unwrap();
            thread::sleep(Duration::from_millis(40));
        }
        let stats = forward.stats();
        forward.stop().unwrap();
        assert_eq!((stats.accepted, stats.written), (20, 20), "{stats:?}");
        assert_eq!(
            fake.launches(&["-fps_mode cfr -r 60", "tcp://127.0.0.1:9"]),
            1
        );
        assert_eq!(fake.read("encoded").len(), 20 * 8 * 4 * 3);
    }
    #[test]
    #[cfg(unix)]
    fn streams_get_the_last_frame_again_through_a_stall() {
        let fake = Fake::new(
            "repeat",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > "$dir/encoded" ;;
esac"#,
        );
        let mut forward = Forwarder::new(stream("libx264", 20.0)).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        thread::sleep(STALL + Duration::from_millis(500));
        let stats = forward.stats();
        // About 50 slots: receivers keep getting a picture.
        assert!(stats.stalled && stats.written >= 30, "{stats:?}");
        forward.stop().unwrap();
        let sent = fake.read("encoded").len() as u64;
        assert!(sent >= stats.written * 8 * 4 * 3, "{sent}");
    }
    #[test]
    #[cfg(unix)]
    fn a_recording_keeps_its_video_when_ffmpeg_must_be_stopped() {
        let fake = Fake::new(
            "stuck",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) head -c 10000 /dev/zero; cat > /dev/null; exec sleep 30 ;;
esac"#,
        );
        let output = fake.dir.join("stuck.mkv");
        let mut forward = Forwarder::new(recording(&output, "libx264")).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        thread::sleep(Duration::from_millis(300));
        let error = forward.stop().unwrap_err();
        assert!(
            format!("{error:#}").contains("shutdown timed out"),
            "{error:#}"
        );
        assert_eq!(forward.stats().recording.unwrap().bytes, 10_000);
        assert_eq!(fs::read(&output).unwrap().len(), 10_000);
    }
    #[test]
    #[cfg(unix)]
    fn a_recording_that_fills_up_during_a_push_ends_at_its_maximum_size() {
        let fake = Fake::new(
            "full-during-push",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) sleep 0.2; head -c 10000 /dev/zero; cat > /dev/null ;;
esac"#,
        );
        let output = fake.dir.join("full.mkv");
        let mut config = recording(&output, "libx264");
        config.max_file_bytes = Some(5000);
        let mut forward = Forwarder::new(config).unwrap();
        // The file fills up after the push has looked for its end, and
        // before the push hands the frame on.
        PUSH_RACE.set(Some(|queue| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while queue.state.lock().unwrap().error.is_none() {
                assert!(Instant::now() < deadline, "the file never filled up");
                thread::sleep(Duration::from_millis(10));
            }
        }));
        let error = forward.push(tiny(8, 4)).unwrap_err();
        PUSH_RACE.set(None);
        assert!(error.is::<crate::storage::MaxSize>(), "{error:#}");
        forward.stop().unwrap();
        assert_eq!(forward.stats().recording.unwrap().bytes, 5000);
        assert_eq!(fs::read(&output).unwrap().len(), 5000);
    }
    #[test]
    #[cfg(unix)]
    fn a_resolution_change_stops_the_recording_with_a_reason() {
        let _fake = Fake::new(
            "resize",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > /dev/null ;;
esac"#,
        );
        let mut forward = Forwarder::new(stream("libx264", 30.0)).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        let error = forward.push(tiny(16, 8)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the camera's resolution changed from 8x4 to 16x8; start a new recording to continue"
        );
        forward.stop().unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn encoder_probes_run_once_per_process() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "memo",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat > /dev/null ;;
esac"#
            ),
        );
        let mut probes = 0;
        for _ in 0..3 {
            let mut forward = Forwarder::new(stream("auto", 30.0)).unwrap();
            forward.push(tiny(16, 16)).unwrap();
            forward.stop().unwrap();
            assert_eq!(forward.stats().encoder, first);
            if probes == 0 {
                probes = fake.launches(&["-f null"]);
            }
        }
        // One listing, and one probe for each encoder, input and size; later
        // recordings only launch their encoder.
        assert_eq!(fake.launches(&["-encoders"]), 1);
        assert_eq!(fake.launches(&["size=640x480", "-f null"]), 1);
        assert!(fake.launches(&["-video_size 16x16", "-f null"]) >= 1);
        assert_eq!(fake.launches(&["-f null"]), probes);
        assert_eq!(fake.launches(&["-use_wallclock_as_timestamps"]), 3);
    }
    #[test]
    #[cfg(unix)]
    fn an_encoder_test_killed_from_outside_is_unavailable_and_not_remembered() {
        let first = encoder_candidates("h264")[0];
        // The test encodes are killed, as a memory limit's out-of-memory
        // killer would, until `$dir/ok` exists.
        let fake = Fake::new(
            "probe-killed",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) [ -e "$dir/ok" ] || kill -9 $$ ;;
*) cat > /dev/null ;;
esac"#
            ),
        );
        // Undecided is no reason to refuse the codec before the first frame.
        let mut forward = Forwarder::new(stream("auto", 30.0)).unwrap();
        let error = forward.push(tiny(16, 16)).unwrap_err();
        assert_eq!(
            crate::cli::error_code(&error),
            ("unavailable", 3),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("did not finish"), "{error:#}");
        // x264 is tested as it records: its threads, preset and latency.
        assert!(
            fake.launches(&[
                "-c:v libx264",
                "-threads",
                "-preset",
                "-tune zerolatency",
                "-f null"
            ]) >= 1
        );
        drop(forward);
        // Nothing was remembered: the encoders are tested again, and work.
        fs::write(fake.dir.join("ok"), b"").unwrap();
        let mut forward = Forwarder::new(stream("auto", 30.0)).unwrap();
        forward.push(tiny(16, 16)).unwrap();
        forward.stop().unwrap();
        assert_eq!(forward.stats().encoder, first);
    }
    #[test]
    #[cfg(unix)]
    fn codecs_without_a_working_encoder_fail_before_recording() {
        let _fake = Fake::new(
            "unsupported",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n V....D hevc_bogus bogus (codec hevc)\n' ;;
*"-c:v hevc_bogus"*) exit 1 ;;
esac"#,
        );
        let mut config = stream("auto", 30.0);
        config.codec = "hevc".into();
        let error = Forwarder::new(config).err().unwrap();
        let message = format!("{error:#}");
        assert!(
            message.starts_with("hevc recording is not supported on this host"),
            "{message}"
        );
        assert!(message.contains("working codecs: h264."), "{message}");
        assert_eq!(crate::cli::error_code(&error), ("unsupported", 5));
    }
    #[test]
    fn completed_forwarding_diagnostics_preserve_the_first_typed_cause() {
        let primary: anyhow::Error =
            Categorized::new(ErrorKind::Disconnected, "first ownership lease expired")
                .details(json!({"controller_ownership":"retired","reason":"lease_expired"}))
                .into();
        let completed = anyhow!("FFmpeg input failed: Broken pipe; Connection refused");
        let error = Forwarder::completed_error(&primary, &completed);
        assert_eq!(
            crate::types::error_tag(&error),
            crate::types::error_tag(&primary)
        );
        assert!(crate::transport::gige::ownership_retired(&error));
        let message = format!("{error:#}");
        assert!(message.starts_with("first ownership lease expired"));
        assert!(message.contains("Connection refused"));
        assert_eq!(
            format!("{}", Forwarder::completed_error(&primary, &primary)),
            format!("{primary:#}")
        );
    }
    #[test]
    #[cfg(unix)]
    fn an_early_pipe_failure_keeps_stderr_collected_during_forwarding_finish() {
        let fake = Fake::new(
            "late-encoder-diagnostic",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) exec 0<&-; : > "$dir/input-closed"
   while [ ! -e "$dir/release-diagnostic" ]; do sleep 0.01; done
   printf 'Connection refused while opening output\n' >&2; exit 1 ;;
esac"#,
        );
        let mut forward = Forwarder::new(stream("libx264", 30.0)).unwrap();
        forward.push(tiny(8, 4)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fake.dir.join("input-closed").exists() {
            assert!(Instant::now() < deadline, "encoder did not close its input");
            thread::sleep(Duration::from_millis(5));
        }
        // Force the writer-before-stderr interleaving independently of pipe
        // capacity or reader scheduling. The real encoder has closed stdin,
        // but cannot publish its diagnostic until this test releases it.
        forward
            .process
            .as_ref()
            .unwrap()
            .queue
            .fail("FFmpeg input failed: Broken pipe".into());
        let primary = forward.storage_end().unwrap();
        assert!(format!("{primary:#}").contains("Broken pipe"));
        assert!(!format!("{primary:#}").contains("refused"));
        fs::write(fake.dir.join("release-diagnostic"), b"").unwrap();
        let completed = forward.stop().unwrap_err();
        assert!(format!("{completed:#}").contains("Connection refused"));
        let error = Forwarder::completed_error(&primary, &completed);
        assert_eq!(crate::cli::error_code(&error), ("failed", 1));
        assert!(format!("{error:#}").contains("Connection refused"));
        assert!(forward.stats().recording.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_stream_whose_encoder_exits_does_not_fall_back() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "stream-exit",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) echo 'Connection refused' >&2; exit 1 ;;
esac"#
            ),
        );
        let mut forward = Forwarder::new(stream("auto", 30.0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.push(tiny(16, 16)).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(30));
        }
        let _ = forward.stop();
        assert_eq!(fake.launches(&["-use_wallclock_as_timestamps"]), 1);
        assert_eq!(
            fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
            0
        );
    }
    #[test]
    #[cfg(unix)]
    fn only_the_first_actual_input_broken_pipe_is_retryable() {
        #[derive(Clone, Copy)]
        struct RejectInput(std::io::ErrorKind);
        impl Write for RejectInput {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(self.0, "rejected encoder input"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::PermissionDenied,
        ] {
            let queue = WriterQueue::new();
            queue.state.lock().unwrap().latest = Some(tiny(8, 4));
            let taken = write_frames(
                &queue,
                (&mut RejectInput(kind), None),
                (RawInput::for_format(RGB8).unwrap(), None),
                Duration::ZERO,
                Repeat::Never,
                None,
                None,
            );
            assert_eq!(taken, 1);
            assert_eq!(queue.written.load(Ordering::Relaxed), 0);
            assert_eq!(queue.input_closed(), kind == std::io::ErrorKind::BrokenPipe);
            assert!(
                queue
                    .typed_error()
                    .unwrap()
                    .to_string()
                    .contains("rejected encoder input")
            );
        }
        let queue = WriterQueue::new();
        queue.fail("Broken pipe while preparing invalid source pixels".into());
        queue.fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(!queue.input_closed());
        assert!(!queue.fallback_failure());
        assert_eq!(
            queue.typed_error().unwrap().to_string(),
            "Broken pipe while preparing invalid source pixels"
        );
        let queue = WriterQueue::new();
        queue.fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        queue.fail_timeline(&Categorized::error(ErrorKind::Incomplete, "index failed"));
        assert!(!queue.input_closed());
        assert!(!queue.fallback_failure());
        assert_eq!(
            crate::types::classify(&queue.typed_error().unwrap()).kind,
            ErrorKind::Incomplete
        );
    }
    #[test]
    #[cfg(unix)]
    fn a_live_startup_child_with_an_input_epipe_is_confirmed_before_one_fallback() {
        for during_push in [false, true] {
            let first = encoder_candidates("h264")[0];
            let fake = Fake::new(
                "fallback-before-exit",
                &format!(
                    r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*"-c:v {first}"*) cat >/dev/null; printf 'partial'; exit 1 ;;
*) cat ;;
esac"#
                ),
            );
            let output = fake.dir.join("recover.mkv");
            let mut forward = Forwarder::new(recording(&output, "auto")).unwrap();
            forward.wait_for_writer();
            forward
                .spawn(16, 16, RawInput::for_format(RGB8).unwrap())
                .unwrap();
            assert_eq!(
                forward.process.as_mut().unwrap().child.try_wait().unwrap(),
                None
            );
            if during_push {
                // Arrive after the child/error health checks, at the same
                // final queue-error branch used after waiting for a slot.
                PUSH_RACE.set(Some(|queue| {
                    PUSH_RACE.set(None);
                    queue.fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
                }));
            } else {
                forward
                    .process
                    .as_ref()
                    .unwrap()
                    .queue
                    .fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
                // Engine idle polling must leave this startup failure for
                // bounded confirmation instead of ending before push runs.
                assert!(forward.storage_end().is_none());
            }
            assert!(forward.push(tiny(16, 16)).unwrap());
            assert_eq!(forward.stats().encoder, "libx264");
            assert_eq!(forward.failed.as_deref(), Some(first));
            assert!(forward.push(tiny(16, 16)).unwrap());
            forward.stop().unwrap();
            let written = usize::try_from(forward.stats().written).unwrap();
            assert!(written >= 1);
            assert_eq!(fs::read(&output).unwrap(), vec![7; written * 16 * 16 * 3]);
            assert_eq!(
                fake.launches(&[&format!("-c:v {first}"), "-use_wallclock_as_timestamps"]),
                1
            );
            assert_eq!(
                fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
                1
            );
        }
    }
    #[test]
    #[cfg(unix)]
    fn startup_pipe_deferral_is_bounded_and_does_not_mask_input_or_artifact_errors() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "fallback-guards",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat >/dev/null; exit 1 ;;
esac"#
            ),
        );
        let output = fake.dir.join("guarded.mkv");
        let mut forward = Forwarder::new(recording(&output, "auto")).unwrap();
        forward
            .spawn(16, 16, RawInput::for_format(RGB8).unwrap())
            .unwrap();
        let queue = forward.process.as_ref().unwrap().queue.clone();
        queue.fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(forward.storage_end().is_none());
        let resized = forward.push(tiny(8, 4)).unwrap_err();
        assert!(resized.to_string().contains("resolution changed"));
        let mut truncated = tiny(16, 16);
        truncated.data = vec![7; 1].into();
        let malformed = forward.push(truncated).unwrap_err();
        assert!(malformed.to_string().contains("truncated"), "{malformed:#}");
        assert!(
            !malformed.to_string().contains("Broken pipe"),
            "{malformed:#}"
        );
        assert_eq!(forward.stats().accepted, 0);
        forward.config.encoder = "libx264".into();
        assert!(forward.storage_end().is_some());
        forward.config.encoder = "auto".into();
        forward.config.output = "tcp://127.0.0.1:9".into();
        assert!(forward.storage_end().is_some());
        forward.config.output = output.to_string_lossy().into();
        forward.config.timeline = Some(timeline::Config {
            path: fake.dir.join("unused.jsonl").to_string_lossy().into(),
            acquisition_end_ns: 1,
        });
        assert!(forward.storage_end().is_some());
        forward.config.timeline = None;
        queue.chunks_seen.store(true, Ordering::Release);
        assert!(forward.storage_end().is_some());
        queue.chunks_seen.store(false, Ordering::Release);
        queue.output_failed.store(true, Ordering::Release);
        assert!(forward.storage_end().is_some());
        queue.output_failed.store(false, Ordering::Release);
        queue
            .encoded_bytes
            .store(HEADER_BYTES + 1, Ordering::Release);
        assert!(forward.storage_end().is_some());
        queue.encoded_bytes.store(0, Ordering::Release);
        let spawned = forward.process.as_ref().unwrap().spawned;
        forward.process.as_mut().unwrap().spawned =
            Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        assert!(forward.storage_end().is_some());
        forward.process.as_mut().unwrap().spawned = spawned;
        forward.failed = Some(first.into());
        assert!(forward.storage_end().is_some());
        forward.failed = None;
        let raw = forward.process.as_mut().unwrap().raw.take();
        assert!(forward.storage_end().is_some());
        forward.process.as_mut().unwrap().raw = raw;
        queue.full_size.store(true, Ordering::Release);
        assert!(
            forward
                .storage_end()
                .unwrap()
                .is::<crate::storage::MaxSize>()
        );
        queue.full_size.store(false, Ordering::Release);
        queue.fail_artifact(
            "video chunks",
            &Categorized::error(ErrorKind::StorageFull, "artifact storage exhausted"),
        );
        let error = forward.storage_end().unwrap();
        assert_eq!(crate::types::classify(&error).kind, ErrorKind::StorageFull);
        assert!(forward.process.is_some());
        drop(forward);
        assert_eq!(
            fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
            0
        );
    }
    #[test]
    #[cfg(unix)]
    fn fallback_confirmation_keeps_final_video_and_file_size_cutoffs() {
        for limit in [None, Some(5_000)] {
            let first = encoder_candidates("h264")[0];
            let fake = Fake::new(
                "fallback-finish-output",
                &format!(
                    r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat >/dev/null; head -c 10000 /dev/zero; exit 1 ;;
esac"#
                ),
            );
            let output = fake.dir.join("retained.mkv");
            let mut config = recording(&output, "auto");
            config.max_file_bytes = limit;
            let mut forward = Forwarder::new(config).unwrap();
            forward
                .spawn(16, 16, RawInput::for_format(RGB8).unwrap())
                .unwrap();
            let queue = forward.process.as_ref().unwrap().queue.clone();
            assert_eq!(queue.encoded_bytes.load(Ordering::Acquire), 0);
            queue.fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            assert!(forward.storage_end().is_none());
            let error = forward.push(tiny(16, 16)).unwrap_err();
            if limit.is_some() {
                assert!(error.is::<crate::storage::MaxSize>(), "{error:#}");
            } else {
                assert!(error.to_string().contains("FFmpeg"), "{error:#}");
            }
            let kept = limit.unwrap_or(10_000);
            assert_eq!(fs::metadata(&output).unwrap().len(), kept);
            let recorded = forward.stats().recording.unwrap();
            assert_eq!(recorded.bytes, kept);
            assert_eq!(recorded.truncated, limit.is_some());
            assert!(forward.process.is_none());
            assert!(forward.failed.is_none());
            forward.stop().unwrap();
            assert_eq!(
                fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
                0
            );
        }
    }
    #[test]
    #[cfg(unix)]
    fn fallback_requires_a_failed_exit_not_a_successful_or_stuck_child() {
        for ending in ["exit 0", r#"mkfifo "$dir/hold" && exec cat "$dir/hold""#] {
            let first = encoder_candidates("h264")[0];
            let fake = Fake::new(
                "fallback-confirmation",
                &format!(
                    r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) cat >/dev/null; {ending} ;;
esac"#
                ),
            );
            let output = fake.dir.join("unconfirmed.mkv");
            let mut forward = Forwarder::new(recording(&output, "auto")).unwrap();
            forward
                .spawn(16, 16, RawInput::for_format(RGB8).unwrap())
                .unwrap();
            forward
                .process
                .as_ref()
                .unwrap()
                .queue
                .fail_input(&std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            let error = forward.push(tiny(16, 16)).unwrap_err();
            assert!(error.to_string().contains("FFmpeg"), "{error:#}");
            assert!(forward.process.is_none());
            assert!(forward.failed.is_none());
            assert!(!output.exists());
            forward.stop().unwrap();
            assert_eq!(
                fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
                0
            );
        }
    }
    #[test]
    #[cfg(unix)]
    fn an_encoder_that_exits_at_start_falls_back_once_to_a_new_file() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "fallback",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*"-c:v {first}"*) printf 'partial' ; exit 1 ;;
*"-c:v libx264"*) [ -e "$dir/x264-fails" ] && exit 1; cat ;;
esac"#
            ),
        );
        let output = fake.dir.join("clip.mkv");
        let mut config = stream("auto", 30.0);
        config.output = output.to_string_lossy().into();
        let mut forward = Forwarder::new(config.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.stats().encoder != "libx264" || forward.stats().written < 2 {
            assert!(Instant::now() < deadline, "{:?}", forward.stats());
            forward.push(tiny(16, 16)).unwrap();
            thread::sleep(Duration::from_millis(30));
        }
        forward.stop().unwrap();
        // The failed encoder's bytes never reach the new file.
        let written = usize::try_from(forward.stats().written).unwrap();
        assert_eq!(fs::read(&output).unwrap().len(), written * 16 * 16 * 3);
        assert_eq!(
            fake.launches(&[&format!("-c:v {first}"), "-use_wallclock_as_timestamps"]),
            1
        );
        assert_eq!(
            fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
            1
        );
        // A second early exit is reported instead of trying again.
        fs::write(fake.dir.join("x264-fails"), b"").unwrap();
        config.output = fake.dir.join("again.mkv").to_string_lossy().into();
        let mut forward = Forwarder::new(config).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.push(tiny(32, 32)).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(30));
        }
        let _ = forward.stop();
        assert_eq!(
            fake.launches(&[&format!("-c:v {first}"), "-use_wallclock_as_timestamps"]),
            2
        );
        assert_eq!(
            fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
            2
        );
        assert!(!fake.dir.join("again.mkv").exists());
    }
    #[test]
    #[cfg(unix)]
    fn an_encoder_that_fails_after_writing_video_keeps_it_instead_of_falling_back() {
        let first = encoder_candidates("h264")[0];
        let fake = Fake::new(
            "late-failure",
            &format!(
                r#"echo "$*" >> "$dir/argv"
case "$*" in
*-encoders*) printf ' V....D {first} first (codec h264)\n V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*"-c:v {first}"*) head -c 10000 /dev/zero; exit 1 ;;
*) cat ;;
esac"#
            ),
        );
        let output = fake.dir.join("late.mkv");
        let mut forward = Forwarder::new(recording(&output, "auto")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while forward.push(tiny(16, 16)).is_ok() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(30));
        }
        // The writer can observe the broken encoder pipe before push settles
        // the process. In that ordering stop must report the same failure;
        // otherwise push has already reported it and stop is idempotent.
        if let Err(error) = forward.stop() {
            assert!(error.to_string().contains("FFmpeg"), "{error:#}");
        }
        let stats = forward.stats();
        assert!(stats.error.is_some(), "{stats:?}");
        assert_eq!(stats.recording.unwrap().bytes, 10_000);
        assert_eq!(fs::read(&output).unwrap().len(), 10_000);
        assert_eq!(
            fake.launches(&["-c:v libx264", "-use_wallclock_as_timestamps"]),
            0
        );
    }
    #[test]
    fn ppm_retries_interrupted_header_and_pixel_reads() {
        let mut bytes = b"P6\n# header comment\n2 1\n255\n".to_vec();
        bytes.extend([10, 32, 13, 0, 255, 1]);
        bytes.extend(b"P6\n1 1\n255\n");
        bytes.extend([1, 2, 3]);
        for capacity in [1, 2, 3, 64] {
            let mut reader = crate::test_io::FaultReader::new(
                BufReader::with_capacity(capacity, bytes.as_slice()),
                std::io::ErrorKind::Interrupted,
            );
            let pool = crate::framebuf::pool();
            assert_eq!(
                read_ppm(&mut reader, 1, pool).unwrap().data,
                [10, 32, 13, 0, 255, 1]
            );
            assert_eq!(read_ppm(&mut reader, 2, pool).unwrap().data, [1, 2, 3]);
        }
    }
    #[test]
    fn ppm_preserves_whitespace_pixels_and_multiple_frames() {
        let mut bytes = b"P6\n2 1\n255\n".to_vec();
        bytes.extend([10, 32, 13, 0, 255, 1]);
        bytes.extend(b"P6\n1 1\n255\n");
        bytes.extend([1, 2, 3]);
        let mut reader = BufReader::new(bytes.as_slice());
        let pool = crate::framebuf::pool();
        let a = read_ppm(&mut reader, 1, pool).unwrap();
        assert_eq!(a.data, [10, 32, 13, 0, 255, 1]);
        assert_eq!(read_ppm(&mut reader, 2, pool).unwrap().data, [1, 2, 3]);
    }
    #[test]
    fn ppm_frames_reuse_the_buffers_of_frames_dropped() {
        let size = 64 * 48 * 3;
        let mut bytes = Vec::new();
        for frame in 0..20u8 {
            bytes.extend(b"P6\n64 48\n255\n");
            bytes.extend(std::iter::repeat_n(frame, size));
        }
        let pool = crate::framebuf::FramePool::new(|| None);
        let mut reader = BufReader::new(bytes.as_slice());
        for id in 0..20 {
            let frame = read_ppm(&mut reader, id, &pool).unwrap();
            assert!(frame.data.iter().all(|&byte| u64::from(byte) == id));
        }
        // One buffer for the first frame, used again for the rest.
        let stats = pool.stats();
        assert_eq!((stats.allocations, stats.reuses), (1, 19));
    }
    #[test]
    fn ppm_rejects_oversize_truncation_and_sample_depth() {
        for bytes in [
            &b"P6\n9999999 9999999\n255\n"[..],
            &b"P6\n1 1\n65535\n"[..],
            &b"P6\n1 1\n255\n\0"[..],
        ] {
            read_ppm(&mut BufReader::new(bytes), 1, crate::framebuf::pool()).unwrap_err();
        }
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
        output_args("record.m3u8").unwrap_err();
        recording_output("record.unknown").unwrap_err();
        assert_eq!(
            recording_output("file:record.mp4").unwrap().unwrap().1,
            "mp4"
        );
    }
    #[test]
    fn jetson_frame_sizes_are_checked_before_preparation() {
        for input in [JetsonInput::I420, JetsonInput::Rgba] {
            for (width, height) in [(0, 1), (1, 0), (u32::MAX, 1), (u32::MAX, u32::MAX)] {
                assert!(
                    input.frame_bytes(width, height).is_err(),
                    "{input:?}: {width}x{height}"
                );
            }
        }
        let tallest = u32::try_from(MAX_FRAME / 4).unwrap();
        assert_eq!(
            JetsonInput::Rgba.frame_bytes(1, tallest).unwrap(),
            MAX_FRAME
        );
        JetsonInput::Rgba.frame_bytes(1, tallest + 1).unwrap_err();
        let mut frame = Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width: 3,
            height: 3,
            pixel_format: crate::types::MONO8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: vec![7; 8].into(),
            jpeg: None,
        };
        let mut scratch = Scratch::default();
        assert!(JetsonInput::I420.prepare(&frame, &mut scratch).is_err());
        assert_eq!(scratch.chroma, [] as [u8; 0]);
        frame.width = u32::MAX;
        assert!(JetsonInput::I420.prepare(&frame, &mut scratch).is_err());
        assert_eq!(scratch.chroma, [] as [u8; 0]);
    }

    #[test]
    fn jetson_inputs_and_bitrates() {
        for (fps, expected) in [
            (30.0, (30, 1)),
            (0.1, (1, 10)),
            (30.12345, (602_469, 20000)),
            (30000.0 / 1001.0, (30000, 1001)),
            (240.0, (240, 1)),
            (
                2_147_483_647.0 / 1_073_741_824.0,
                (2_147_483_647, 1_073_741_824),
            ),
            (
                268_435_456.0 / 2_147_483_647.0,
                (268_435_456, 2_147_483_647),
            ),
        ] {
            let fraction = jetson_framerate(fps).unwrap();
            assert_eq!(fraction, expected);
            assert_eq!(f64::from(fraction.0) / f64::from(fraction.1), fps);
        }
        let unrepresentable = f64::from_bits(30.0f64.to_bits() + 1);
        assert!(
            jetson_framerate(unrepresentable)
                .unwrap_err()
                .to_string()
                .contains("cannot be represented exactly")
        );
        for fps in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            0.1f64.next_down(),
            240.0f64.next_up(),
        ] {
            assert!(jetson_framerate(fps).is_err(), "{fps}");
        }
        assert_eq!(bits_per_second("4M").unwrap(), 4_000_000);
        assert_eq!(bits_per_second("2500k").unwrap(), 2_500_000);
        assert_eq!(bits_per_second("800000").unwrap(), 800_000);
        assert!(bits_per_second("5G").is_err() && bits_per_second("x").is_err());
        let gray = Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width: 3,
            height: 3,
            pixel_format: crate::types::MONO8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: (1..=9).collect(),
            jpeg: None,
        };
        // Luma plane, then 2x2 U and V planes for odd dimensions.
        let i420 = JetsonInput::I420.bytes(&gray).unwrap();
        assert_eq!(i420.len(), 9 + 2 * 4);
        assert_eq!(i420.len(), JetsonInput::I420.frame_bytes(3, 3).unwrap());
        assert_eq!((&i420[..9], &i420[9..]), ((&*gray.data), &[128; 8][..]));
        let rgba = JetsonInput::Rgba.bytes(&gray).unwrap();
        assert_eq!(&rgba[..8], [1, 1, 1, 255, 2, 2, 2, 255]);
        assert_eq!(rgba.len(), JetsonInput::Rgba.frame_bytes(3, 3).unwrap());
        // The luma plane of a Mono8 frame is the frame's own buffer.
        let mut scratch = Scratch::default();
        JetsonInput::I420.prepare(&gray, &mut scratch).unwrap();
        let Payload::Parts([luma, chroma]) = JetsonInput::I420.payload(&gray, &scratch) else {
            panic!("I420 is luma and chroma");
        };
        assert_eq!((luma.as_ptr(), chroma.len()), (gray.data.as_ptr(), 8));
        // 8-bit coordinate and confidence planes are their own luma, as a
        // conversion would make them.
        for format in [0x0108_00b1, 0x0108_00c6] {
            let plane = Frame {
                pixel_encoding: None,
                pixel_format: format,
                ..gray.clone()
            };
            let luma = crate::frame::convert(&plane, |[gray, _, _]| gray).unwrap();
            assert_eq!(JetsonInput::I420.bytes(&plane).unwrap()[..9], luma[..]);
        }
        // Other monochrome formats are converted to 8-bit luma.
        let wide = Frame {
            pixel_encoding: None,
            pixel_format: 0x0110_0005,
            data: (1..=9u16).flat_map(|v| (v << 4).to_le_bytes()).collect(),
            ..gray.clone()
        };
        assert_eq!(
            JetsonInput::I420.bytes(&wide).unwrap(),
            JetsonInput::I420.bytes(&gray).unwrap()
        );
        assert!(jetson_encoder("nvv4l2h264enc") && !jetson_encoder("h264_nvenc"));
    }
    /// Frames converted on their way to `FFmpeg` anyway, packed or wide, are
    /// narrowed to 8 bits in that pass for an 8-bit encoder, as stills
    /// reduce them; FFV1 keeps every bit and frames `FFmpeg` reads as they
    /// are stay as they are.
    #[test]
    fn deep_frames_are_narrowed_for_eight_bit_encoders_in_their_one_pass() {
        let (width, height) = (6u32, 4u32);
        for (format, ffmpeg) in [
            (0x010c_0047, "gray"),        // Mono12p
            (0x010a_0046, "gray"),        // Mono10p
            (0x010c_0006, "gray"),        // Mono12Packed
            (0x010c_0059, "bayer_rggb8"), // BayerRG12p
            (0x010a_0058, "bayer_rggb8"), // BayerRG10p
            (0x0110_0011, "bayer_rggb8"), // BayerRG12
            (0x0110_000d, "bayer_rggb8"), // BayerRG10
        ] {
            let frame = crate::frame::noise_frame(format, width, height);
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(raw.for_encoder("ffv1"), raw, "{format:x}");
            let narrowed = raw.for_encoder("libx264");
            assert_eq!(
                (narrowed.ffmpeg, narrowed.bytes_per_pixel),
                (ffmpeg, 1),
                "{format:x}"
            );
            assert_eq!(narrowed.input_pix_fmt("libx264"), ffmpeg);
            assert_eq!(narrowed.pipe_len(width, height).unwrap(), 24);
            // The top 8 bits of every sample, as a still keeps them.
            let shift = crate::frame::reduced(format).unwrap().1;
            let expected: Vec<u8> = (0..height as usize)
                .flat_map(|y| (0..width as usize).map(move |x| (y, x)))
                .map(|(y, x)| (crate::frame::native_sample(&frame, x, y) >> shift).min(255) as u8)
                .collect();
            assert_eq!(narrowed.bytes(&frame).unwrap(), expected, "{format:x}");
            // Once narrowed, another encoder starts from the frame again.
            assert_eq!(narrowed.for_encoder("ffv1"), raw);
        }
        // Read by FFmpeg as they are, BayerRG16 among them: not narrowed.
        for format in [
            crate::types::MONO8,
            0x0110_0005,
            0x0110_0007,
            0x0110_002f,
            0x0108_0009,
            RGB8,
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(raw.for_encoder("libx264"), raw, "{format:x}");
        }
        assert!(
            RawInput::for_format(0x010c_0059)
                .unwrap()
                .for_encoder("nvv4l2h264enc")
                .narrow
                .is_none()
        );
    }
    #[test]
    fn odd_yuv_pipe_sizes_validate_both_source_and_padding() {
        for format in [0x0210_001f, 0x0210_0032] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(raw.pipe_len(3, 2).unwrap(), 16);
            assert_eq!(raw.pipe_len(4, 2).unwrap(), 16);
            for (width, height) in [
                (1, 0),
                (0, 1),
                (u32::MAX, 1),
                (1, u32::MAX),
                (u32::MAX, u32::MAX),
            ] {
                assert!(
                    raw.pipe_len(width, height).is_err(),
                    "{format:x} {width}x{height}"
                );
            }
            let tallest = u32::try_from(MAX_FRAME / 4).unwrap();
            assert_eq!(raw.pipe_len(1, tallest).unwrap(), MAX_FRAME);
            raw.pipe_len(1, tallest + 1).unwrap_err();
        }
    }

    #[test]
    fn raw_inputs_unpack_and_widen_what_ffmpeg_cannot_read() {
        let frame = |pixel_format, width, data| Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width,
            height: 1,
            pixel_format,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data,
            jpeg: None,
        };
        let le =
            |samples: &[u16]| -> Vec<u8> { samples.iter().flat_map(|v| v.to_le_bytes()).collect() };
        // Mono12p 0xabc, 0x123 as gray12le; Mono12Packed the same samples.
        for (format, data) in [
            (0x010c_0047, vec![0xbc, 0x3a, 0x12]),
            (0x010c_0006, vec![0xab, 0x3c, 0x12]),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(
                (raw.ffmpeg, raw.bytes_per_pixel, raw.color),
                ("gray12le", 2, false)
            );
            assert_eq!(
                raw.bytes(&frame(format, 2, data.into())).unwrap(),
                le(&[0xabc, 0x123])
            );
        }
        // 10- and 12-bit Bayer fill FFmpeg's 16-bit Bayer formats.
        let raw = RawInput::for_format(0x0110_0011).unwrap();
        assert_eq!(raw.ffmpeg, "bayer_rggb16le");
        let samples = frame(0x0110_0011, 2, le(&[0xfff, 0x001]).into());
        assert_eq!(raw.bytes(&samples).unwrap(), le(&[0xfff0, 0x0010]));
        let raw = RawInput::for_format(0x010c_0029).unwrap();
        assert_eq!(raw.ffmpeg, "bayer_bggr16le");
        let packed = frame(0x010c_0029, 2, vec![0x55, 0x21, 0xaa].into());
        assert_eq!(raw.bytes(&packed).unwrap(), le(&[0x155 << 6, 0x2aa << 6]));
        raw.bytes(&frame(0x010c_0029, 2, vec![0x55, 0x21].into()))
            .unwrap_err();
        for (format, ffmpeg) in [
            (0x0110_0025, "gray14le"),
            (0x0110_002e, "bayer_grbg16le"),
            (0x010a_0056, "bayer_grbg16le"),
            (0x010c_002d, "bayer_bggr16le"),
            (0x0220_0016, "rgba"),
            (0x0220_0017, "bgra"),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!((raw.ffmpeg, raw.yuv), (ffmpeg, None));
            assert_eq!(raw.color_options(), [] as [&str; 0]);
        }
        // YUV and YCbCr 4:2:2 go as they are, tagged full-range BT.601.
        for (format, ffmpeg) in [
            (0x0210_001f, "uyvy422"),
            (0x0210_0043, "uyvy422"),
            (0x0210_0032, "yuyv422"),
            (0x0210_003b, "yuyv422"),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(
                (raw.ffmpeg, raw.bytes_per_pixel, raw.color),
                (ffmpeg, 2, true)
            );
            assert_eq!(
                raw.color_options(),
                ["-color_range", "pc", "-colorspace", "bt470bg"]
            );
            let pair = vec![1, 2, 3, 4, 5];
            assert_eq!(
                raw.bytes(&frame(format, 2, pair.into())).unwrap(),
                [1, 2, 3, 4]
            );
        }
        let error = RawInput::for_format(0x0110_0099).unwrap_err().to_string();
        assert!(error.contains("0x01100099") && error.contains("--format raw"));
    }
    /// FFV1 gets RGB deeper than 8 bits as planar GBR with every bit of
    /// each component; other encoders get RGB24.
    #[test]
    fn ffv1_keeps_every_bit_of_deep_rgb() {
        // RGB12: two pixels of 16-bit containers.
        let frame = Frame {
            pixel_encoding: None,
            width: 2,
            height: 1,
            pixel_format: 0x0230_001a,
            data: [0x0fffu16, 0x0123, 0x0abc, 0x0001, 0x0800, 0x0f0f]
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect::<Vec<u8>>()
                .into(),
            ..Frame::default()
        };
        let raw = RawInput::for_format(frame.pixel_format)
            .unwrap()
            .for_encoder("ffv1");
        assert_eq!(raw.input_pix_fmt("ffv1"), "gbrp12le");
        let planes = raw.bytes(&frame).unwrap();
        let samples = planes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|sample| u16::from_le_bytes(*sample))
            .collect::<Vec<_>>();
        // G, then B, then R.
        assert_eq!(samples, [0x0123, 0x0800, 0x0abc, 0x0f0f, 0x0fff, 0x0001]);
        let eight = RawInput::for_format(frame.pixel_format)
            .unwrap()
            .for_encoder("libx264");
        assert_eq!(eight.input_pix_fmt("libx264"), "rgb24");
        assert_eq!(
            eight.bytes(&frame).unwrap(),
            [0xff, 0x12, 0xab, 0x00, 0x80, 0xf0]
        );
        for (format, ffmpeg) in [
            (0x0230_0018, "gbrp10le"),
            (0x021e_005c, "gbrp10le"),
            (0x0230_005e, "gbrp14le"),
            (0x0240_0064, "gbrp16le"),
            (RGB8, "rgb24"),
        ] {
            let raw = RawInput::for_format(format).unwrap().for_encoder("ffv1");
            assert_eq!(raw.input_pix_fmt("ffv1"), ffmpeg, "{format:x}");
        }
    }
    /// FFV1 stores a mosaic as gray samples, 16-bit for wider ones; every
    /// other encoder, and every other format, reads the frames' own format.
    #[test]
    fn ffv1_stores_bayer_mosaics_as_gray() {
        let bayer = (0x0108_0000..=0x0110_0200)
            .filter(|&v| crate::frame::is_bayer(v))
            .filter_map(|v| RawInput::for_format(v).ok())
            .collect::<Vec<_>>();
        assert_eq!(bayer.len(), 40);
        for raw in bayer {
            let wide = crate::frame::sample_bits(raw.pixel_format).unwrap() > 8;
            let expected = if wide { "gray16le" } else { "gray" };
            assert_eq!(
                raw.input_pix_fmt("ffv1"),
                expected,
                "{:x}",
                raw.pixel_format
            );
            assert_eq!(raw.input_pix_fmt("libx264"), raw.ffmpeg);
            assert!(!raw.rgb());
        }
        for (format, ffmpeg) in [
            (crate::types::MONO8, "gray"),
            (0x010e_0104, "gray14le"),
            (0x0110_0007, "gray16le"),
            (RGB8, "rgb24"),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(raw.input_pix_fmt("ffv1"), ffmpeg, "{format:x}");
        }
    }
    /// Which pixel layout each encoder gets for each kind of source: RGB
    /// only for RGB sources on encoders that convert it themselves, and a
    /// mosaic demosaicked to RGB24 before any 4:2:0.
    #[test]
    fn encoders_get_the_input_their_source_converts_to_most_directly() {
        let vf = |options: &[String]| {
            let at = options.iter().position(|o| o == "-vf").unwrap();
            let pix = options.iter().position(|o| o == "-pix_fmt").unwrap();
            (options[at + 1].clone(), options[pix + 1].clone())
        };
        let pad = "pad=ceil(iw/2)*2:ceil(ih/2)*2";
        for (encoder, source, input, filters, pix_fmt) in [
            (
                "libx264",
                "bayer_rggb8",
                EncoderInput::Yuv,
                format!("format=rgb24,{pad}"),
                "yuv420p",
            ),
            (
                "libx264",
                "rgb24",
                EncoderInput::Yuv,
                pad.to_owned(),
                "yuv420p",
            ),
            (
                "h264_nvenc",
                "bayer_rggb16le",
                EncoderInput::Yuv,
                format!("format=rgb24,{pad}"),
                "yuv420p",
            ),
            (
                "h264_nvenc",
                "rgb24",
                EncoderInput::Rgb,
                pad.to_owned(),
                "bgr0",
            ),
            (
                "hevc_amf",
                "yuyv422",
                EncoderInput::Yuv,
                pad.to_owned(),
                "yuv420p",
            ),
            (
                "h264_videotoolbox",
                "bayer_rggb8",
                EncoderInput::Yuv,
                format!("format=rgb24,{pad}"),
                "nv12",
            ),
            (
                "h264_videotoolbox",
                "rgb24",
                EncoderInput::Yuv,
                pad.to_owned(),
                "nv12",
            ),
            (
                "h264_qsv",
                "bayer_grbg16le",
                EncoderInput::Yuv,
                format!("format=rgb24,{pad}"),
                "nv12",
            ),
            (
                "h264_vaapi",
                "bayer_rggb8",
                EncoderInput::Yuv,
                format!("format=rgb24,{pad},format=nv12,hwupload"),
                "vaapi",
            ),
            (
                "av1_vulkan",
                "gray",
                EncoderInput::Yuv,
                format!("{pad},format=nv12,hwupload"),
                "vulkan",
            ),
        ] {
            let options = encoder_output_options(encoder, input, source);
            assert_eq!(
                vf(&options),
                (filters, pix_fmt.to_owned()),
                "{encoder} {source}"
            );
        }
        assert_eq!(
            encoder_output_options("ffv1", EncoderInput::Yuv, "gray16le"),
            ["-level", "3", "-g", "1"]
        );
        for (format, rgb) in [
            (RGB8, true),
            (0x0218_0015, true),
            (0x0220_0016, true),
            (0x0108_0009, false),
            (0x0210_0032, false),
            (crate::types::MONO8, false),
        ] {
            assert_eq!(
                RawInput::for_format(format).unwrap().rgb(),
                rgb,
                "{format:x}"
            );
        }
    }
    #[test]
    fn raw_inputs_keep_fourteen_bit_precision() {
        let frame = |pixel_format, data| Frame {
            pixel_encoding: None,
            width: 2,
            height: 1,
            pixel_format,
            data,
            ..Frame::default()
        };
        let samples = [0x1234u16, 0x3abc];
        let unpacked = samples
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let packed = [0x34, 0x12, 0xaf, 0x0e].to_vec();
        let mono = RawInput::for_format(0x010e_0104).unwrap();
        assert_eq!(mono.ffmpeg, "gray14le");
        assert_eq!(
            mono.bytes(&frame(0x010e_0104, packed.clone().into()))
                .unwrap(),
            unpacked
        );
        for (plain, packed_format, ffmpeg) in [
            (0x0110_0109, 0x010e_0105, "bayer_grbg16le"),
            (0x0110_010a, 0x010e_0106, "bayer_rggb16le"),
            (0x0110_010b, 0x010e_0107, "bayer_gbrg16le"),
            (0x0110_010c, 0x010e_0108, "bayer_bggr16le"),
        ] {
            for (format, data) in [(plain, unpacked.clone()), (packed_format, packed.clone())] {
                let raw = RawInput::for_format(format).unwrap();
                assert_eq!(raw.ffmpeg, ffmpeg);
                assert_eq!(
                    raw.bytes(&frame(format, data.into())).unwrap(),
                    [0x48d0u16, 0xeaf0]
                        .into_iter()
                        .flat_map(u16::to_le_bytes)
                        .collect::<Vec<_>>(),
                    "{format:#x}"
                );
            }
        }
    }
    /// How every frame was turned into the encoder's input before it was
    /// converted on the writer thread: the reference the payloads match.
    fn reference_bytes(raw: &RawInput, frame: &Frame) -> Result<Vec<u8>> {
        let (width, height) = (frame.width as usize, frame.height as usize);
        let length = raw.frame_len(frame.width, frame.height)?;
        ensure!(frame.pixel_format == raw.pixel_format, "format changed");
        let mut data = if raw.preparation.decodes() {
            if raw.color {
                crate::frame::rgb(frame)?
            } else {
                crate::frame::gray8(frame)?.context("no gray conversion")?
            }
        } else if raw.preparation.unpacks() {
            crate::frame::unpacked(frame)?.data.to_vec()
        } else {
            frame.data.to_vec()
        };
        ensure!(data.len() >= length, "truncated frame");
        data.truncate(length);
        if raw.widen > 0 {
            for sample in data.as_chunks_mut::<2>().0 {
                let value = u16::from_le_bytes(*sample).min(u16::MAX >> raw.widen);
                *sample = (value << raw.widen).to_le_bytes();
            }
        }
        if raw.pairs() && !width.is_multiple_of(2) {
            let row = width * 2;
            let mut padded = Vec::with_capacity((row + 2) * height);
            for line in data.chunks_exact(row) {
                padded.extend_from_slice(line);
                padded.extend_from_slice(&[128, 128]);
            }
            data = padded;
        }
        Ok(data)
    }
    /// The writer converts a frame when it takes it from the queue, once:
    /// frames a newer one replaced while `FFmpeg` was busy are never
    /// converted, and a repeat writes the same conversion again.
    #[test]
    fn the_writer_converts_only_the_frames_it_writes() {
        /// A pipe that holds its first write until the test lets it go.
        struct Held {
            bytes: Vec<u8>,
            gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
        }
        impl Write for Held {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Some((writing, go)) = self.gate.take() {
                    writing.send(()).map_err(std::io::Error::other)?;
                    go.recv().map_err(std::io::Error::other)?;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        const MONO12P: u32 = 0x010c_0047;
        let raw = RawInput::for_format(MONO12P).unwrap();
        assert!(raw.preparation.unpacks());
        let frames: Vec<Frame> = (1..=4u8)
            .map(|n| Frame {
                pixel_encoding: None,
                id: n.into(),
                width: 8,
                height: 4,
                pixel_format: MONO12P,
                data: (0..48u8).map(|i| i.wrapping_mul(37) ^ n).collect(),
                ..Frame::default()
            })
            .collect();
        for repeat in [false, true] {
            let queue = Arc::new(WriterQueue::new());
            let offer = |frame: &Frame| {
                let mut state = queue.state.lock().unwrap();
                state.latest = Some(frame.clone());
                drop(state);
                queue.changed.notify_one();
            };
            let (writing, held) = std::sync::mpsc::channel();
            let (go, gate) = std::sync::mpsc::channel();
            let writer = thread::spawn({
                let queue = queue.clone();
                move || {
                    let mut pipe = Held {
                        bytes: Vec::new(),
                        gate: Some((writing, gate)),
                    };
                    let interval = Duration::from_millis(1);
                    let repeat = if repeat {
                        Repeat::UntilStalled
                    } else {
                        Repeat::Never
                    };
                    let taken = write_frames(
                        &queue,
                        (&mut pipe, None),
                        (raw, None),
                        interval,
                        repeat,
                        None,
                        None,
                    );
                    (pipe.bytes, taken)
                }
            });
            // The second and third frames are replaced by the fourth while
            // the first is still being written.
            offer(&frames[0]);
            held.recv().unwrap();
            for frame in &frames[1..] {
                offer(frame);
            }
            go.send(()).unwrap();
            let writes = if repeat { 5 } else { 2 };
            let deadline = Instant::now() + Duration::from_secs(10);
            while queue.written.load(Ordering::Relaxed) < writes {
                assert!(Instant::now() < deadline, "repeat {repeat}");
                thread::sleep(Duration::from_millis(1));
            }
            queue.running.store(false, Ordering::Release);
            queue.changed.notify_all();
            let (bytes, taken) = writer.join().unwrap();
            let written = usize::try_from(queue.written.load(Ordering::Relaxed)).unwrap();
            assert_eq!(taken, 2, "repeat {repeat}");
            assert!(written == 2 || repeat && written >= 5, "{written}");
            let first = reference_bytes(&raw, &frames[0]).unwrap();
            let last = reference_bytes(&raw, &frames[3]).unwrap();
            assert_ne!(first, last);
            let expected: Vec<u8> = std::iter::once(first)
                .chain(std::iter::repeat_n(last, written - 1))
                .flatten()
                .collect();
            assert_eq!(bytes, expected, "repeat {repeat}");
        }
    }
    /// Once the camera has stalled, a Jetson stream's writer still sends
    /// the last frame on every slot; a Jetson recording's waits for a new
    /// one.
    #[test]
    fn a_jetson_stream_repeats_through_stalls() {
        assert_eq!(Repeat::of(true, false), Repeat::Always);
        assert_eq!(Repeat::of(true, true), Repeat::UntilStalled);
        const MONO8: u32 = 0x0108_0001;
        let raw = RawInput::for_format(MONO8).unwrap();
        for repeat in [Repeat::Always, Repeat::UntilStalled] {
            let mut queue = WriterQueue::new();
            // The newest frame arrived well over `STALL` ago.
            queue.epoch = Instant::now()
                .checked_sub(STALL * 2)
                .expect("test clock can represent a stalled stream");
            assert!(queue.stalled());
            let queue = Arc::new(queue);
            queue.state.lock().unwrap().latest = Some(Frame {
                pixel_encoding: None,
                id: 1,
                width: 4,
                height: 2,
                pixel_format: MONO8,
                data: vec![7; 8].into(),
                ..Frame::default()
            });
            let writer = thread::spawn({
                let queue = queue.clone();
                move || {
                    let mut pipe = Vec::new();
                    let interval = Duration::from_millis(1);
                    write_frames(
                        &queue,
                        (&mut pipe, None),
                        (raw, None),
                        interval,
                        repeat,
                        None,
                        None,
                    );
                    pipe
                }
            });
            if repeat == Repeat::Always {
                let deadline = Instant::now() + Duration::from_secs(10);
                while queue.written.load(Ordering::Relaxed) < 5 {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
            } else {
                // Fifty slots go by.
                thread::sleep(Duration::from_millis(50));
            }
            queue.running.store(false, Ordering::Release);
            queue.changed.notify_all();
            let pipe = writer.join().unwrap();
            let written = queue.written.load(Ordering::Relaxed);
            match repeat {
                Repeat::Always => assert!(written >= 5, "{written}"),
                _ => assert_eq!(written, 1),
            }
            assert_eq!(
                pipe,
                vec![7; 8 * usize::try_from(written).unwrap()],
                "{repeat:?}"
            );
        }
    }
    #[test]
    fn payloads_are_written_whole_through_short_writes() {
        /// A pipe that takes at most 5 bytes per write, gathered or not.
        struct Short(Vec<u8>, usize);
        impl Write for Short {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.1 += 1;
                let n = bytes.len().min(5);
                self.0.extend_from_slice(&bytes[..n]);
                Ok(n)
            }
            fn write_vectored(&mut self, slices: &[IoSlice<'_>]) -> std::io::Result<usize> {
                self.1 += 1;
                let mut room = 5;
                for slice in slices {
                    let n = slice.len().min(room);
                    self.0.extend_from_slice(&slice[..n]);
                    room -= n;
                }
                Ok(5 - room)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let rows: Vec<u8> = (0u32..2100 * 6).map(|i| i.to_le_bytes()[0]).collect();
        let parts = [&rows[..7], &[][..]];
        for payload in [
            Payload::Slice(&rows[..13]),
            Payload::Parts(parts),
            Payload::Parts([&[][..], &rows[..9]]),
            Payload::Rows {
                data: &rows,
                row: 6,
                pad: [128, 128],
            },
        ] {
            let mut pipe = Short(Vec::new(), 0);
            payload.write_to(&mut pipe).unwrap();
            let expected: Vec<u8> = match payload {
                Payload::Slice(data) => data.to_vec(),
                Payload::Parts(parts) => parts.concat(),
                Payload::Rows { data, row, pad } => data
                    .chunks(row)
                    .flat_map(|line| [line, &pad[..]].concat())
                    .collect(),
            };
            assert_eq!(
                (pipe.0.len(), payload.len()),
                (expected.len(), expected.len())
            );
            assert_eq!(pipe.0, expected);
            // Full writes, but at the end of each batch of rows.
            assert!(pipe.1 <= expected.len().div_ceil(5) + 5, "{}", pipe.1);
        }
    }
    #[test]
    fn payloads_match_the_reference_conversion_for_every_format() {
        let mut formats = Vec::new();
        for family in [0x0100_0000u32, 0x0200_0000] {
            for bits in 1..=64u32 {
                for id in 0..0x200u32 {
                    let format = family | (bits << 16) | id;
                    if crate::frame::pixel_format_name(format) != "Unknown"
                        && RawInput::for_format(format).is_ok()
                    {
                        formats.push(format);
                    }
                }
            }
        }
        assert!(formats.len() > 60, "{}", formats.len());
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut converted = 0;
        for &format in &formats {
            let raw = RawInput::for_format(format).unwrap();
            for (width, height) in [(1, 1), (2, 3), (3, 2), (5, 3), (8, 2), (12, 4)] {
                let frame = Frame {
                    pixel_encoding: None,
                    width,
                    height,
                    pixel_format: format,
                    data: std::iter::repeat_with(|| {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed.to_le_bytes()[0]
                    })
                    .take(crate::frame::row_bytes(width, format) * height as usize)
                    .collect(),
                    ..Frame::default()
                };
                let reference = reference_bytes(&raw, &frame);
                let bytes = raw.bytes(&frame);
                match (&reference, &bytes) {
                    (Ok(reference), Ok(bytes)) => {
                        assert_eq!(bytes, reference, "{format:#x} {width}x{height}");
                        converted += 1;
                    }
                    (Err(_), Err(_)) => {}
                    _ => panic!("{format:#x} {width}x{height}: {reference:?} vs {bytes:?}"),
                }
            }
        }
        assert!(converted > formats.len() * 3, "{converted}");
    }
    #[test]
    fn scalar_raw_inputs_preserve_samples_without_expanding_to_rgb() {
        for (format, ffmpeg, bytes_per_pixel) in [
            (0x0108_00b1, "gray", 1),
            (0x0108_00c6, "gray", 1),
            (0x0110_00b8, "gray16le", 2),
            (0x0110_00c7, "gray16le", 2),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(
                (raw.ffmpeg, raw.bytes_per_pixel, raw.color),
                (ffmpeg, bytes_per_pixel, false)
            );
            let data = (1..=3 * u8::try_from(bytes_per_pixel).unwrap()).collect::<Vec<_>>();
            let expected = data.clone();
            let buffer = data.as_ptr();
            let frame = Frame {
                pixel_encoding: None,
                width: 3,
                height: 1,
                pixel_format: format,
                data: data.into(),
                ..Frame::default()
            };
            let Payload::Slice(payload) = raw.payload(&frame, &[]) else {
                panic!("{format:#x} is one slice");
            };
            assert_eq!(payload.as_ptr(), buffer, "the pipe reads the sensor buffer");
            assert_eq!(raw.bytes(&frame).unwrap(), expected, "{format:#x}");
        }
    }
    #[test]
    fn raw_inputs_reject_invalid_dimensions_before_reading_samples() {
        for format in [crate::types::MONO8, 0x0210_0032, 0x010c_0047] {
            let raw = RawInput::for_format(format).unwrap();
            for (width, height) in [(0, 1), (1, 0), (u32::MAX, u32::MAX), (100_000, 100_000)] {
                let frame = Frame {
                    pixel_encoding: None,
                    width,
                    height,
                    pixel_format: format,
                    data: vec![].into(),
                    ..Frame::default()
                };
                assert!(
                    raw.bytes(&frame)
                        .unwrap_err()
                        .to_string()
                        .contains("invalid forwarding dimensions"),
                    "{format:#x} {width}x{height}"
                );
            }
        }
        let rgba = RawInput::for_format(0x0220_0016).unwrap();
        rgba.frame_len(8192, 8192).unwrap();
        rgba.frame_len(8193, 8192).unwrap_err();
        let gray = RawInput::for_format(crate::types::MONO8).unwrap();
        let frame = Frame {
            pixel_encoding: None,
            width: 1,
            height: 1,
            pixel_format: RGB8,
            data: vec![1, 2, 3].into(),
            ..Frame::default()
        };
        assert!(
            gray.bytes(&frame)
                .unwrap_err()
                .to_string()
                .contains("pixel format changed")
        );
    }
    #[test]
    fn raw_inputs_normalize_formats_ffmpeg_cannot_consume_directly() {
        for (format, width, height, data, ffmpeg, expected) in [
            (
                0x0101_0037,
                5,
                2,
                vec![0b10101, 0b01010],
                "gray",
                vec![255, 0, 255, 0, 255, 0, 255, 0, 255, 0],
            ),
            (
                0x0108_0002,
                4,
                1,
                vec![128, 255, 0, 127],
                "gray",
                vec![0, 127, 128, 255],
            ),
            (
                0x0218_0021,
                2,
                1,
                vec![10, 40, 20, 50, 30, 60],
                "rgb24",
                vec![10, 20, 30, 40, 50, 60],
            ),
            (
                0x0230_0033,
                2,
                1,
                vec![0, 10, 0, 20, 0, 30, 0, 40, 0, 50, 0, 60],
                "rgb24",
                vec![10, 20, 30, 40, 50, 60],
            ),
            (
                0x0224_0049,
                1,
                1,
                vec![0x00, 0x0f, 0x80, 0x00, 0x01],
                "rgb24",
                vec![16, 128, 240],
            ),
            (
                0x0218_0040,
                1,
                1,
                vec![102, 63, 240],
                "rgb24",
                vec![255, 1, 0],
            ),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(raw.ffmpeg, ffmpeg, "{format:#x}");
            assert!(raw.preparation.decodes(), "{format:#x}");
            assert!(
                raw.color_options().is_empty(),
                "converted RGB has no YUV matrix tag"
            );
            let frame = Frame {
                pixel_encoding: None,
                width,
                height,
                pixel_format: format,
                data: data.into(),
                ..Frame::default()
            };
            assert_eq!(raw.bytes(&frame).unwrap(), expected, "{format:#x}");
        }
    }
    /// Video-level 4:2:2 and the planar and semiplanar layouts `FFmpeg`
    /// decodes to go to an encoder as they are, tagged with their range and
    /// matrix.
    #[test]
    fn decoded_yuv_layouts_are_recorded_as_they_are() {
        for (format, ffmpeg, color, len) in [
            (0x0210_003e, "yuyv422", ["tv", "bt470bg"], 8),
            (0x0210_0045, "uyvy422", ["tv", "bt709"], 8),
            (0x020c_0112, "nv12", ["pc", "bt470bg"], 6),
            (crate::frame::NV12_709, "nv12", ["tv", "bt709"], 6),
            (0x820c_0002, "yuv420p", ["tv", "bt470bg"], 6),
            (0x8210_0001, "yuv422p", ["pc", "bt470bg"], 8),
            (0x8218_0003, "yuv444p", ["tv", "bt709"], 12),
        ] {
            let raw = RawInput::for_format(format).unwrap();
            assert_eq!(
                (raw.ffmpeg, raw.preparation.decodes()),
                (ffmpeg, false),
                "{format:#x}"
            );
            assert_eq!(
                raw.color_options(),
                ["-color_range", color[0], "-colorspace", color[1]],
                "{format:#x}"
            );
            let data: Vec<u8> = (0..u8::try_from(len).unwrap()).collect();
            let frame = Frame {
                pixel_encoding: None,
                width: 2,
                height: 2,
                pixel_format: format,
                data: data.clone().into(),
                ..Frame::default()
            };
            assert_eq!(raw.bytes(&frame).unwrap(), data, "{format:#x}");
        }
    }
    #[test]
    fn odd_width_yuv_422_rows_fill_whole_pixel_pairs() {
        // FFmpeg reads 4:2:2 rows of 4 * ceil(width / 2) bytes; a 3x2 frame
        // has rows of 6, so each gains an outside luma and a neutral Cr.
        for format in [0x0210_0032, 0x0210_001f, 0x0210_003b, 0x0210_0043] {
            let raw = RawInput::for_format(format).unwrap();
            let frame = |width, height, data| Frame {
                pixel_encoding: None,
                device_timestamp: None,
                id: 1,
                width,
                height,
                pixel_format: format,
                device_ts_ns: None,
                wall_ms: 0,
                received_monotonic_ns: None,
                chunks: None,
                multipart: None,
                data,
                jpeg: None,
            };
            let padded = raw.bytes(&frame(3, 2, (1..=13).collect())).unwrap();
            assert_eq!(
                padded,
                [1, 2, 3, 4, 5, 6, 128, 128, 7, 8, 9, 10, 11, 12, 128, 128],
                "{format:x}"
            );
            assert_eq!(
                raw.bytes(&frame(1, 1, vec![9, 7].into())).unwrap(),
                [9, 7, 128, 128]
            );
            raw.bytes(&frame(3, 2, vec![0; 11].into())).unwrap_err();
            // Even widths and other formats pass through unchanged.
            let even = (1..=8).collect::<Vec<u8>>();
            assert_eq!(raw.bytes(&frame(2, 2, even.clone().into())).unwrap(), even);
        }
        let mono = RawInput::for_format(crate::types::MONO8).unwrap();
        let odd = Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width: 3,
            height: 1,
            pixel_format: crate::types::MONO8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: vec![1, 2, 3].into(),
            jpeg: None,
        };
        assert_eq!(mono.bytes(&odd).unwrap(), [1, 2, 3]);
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_reads_odd_width_yuv_422_like_capturefab() {
        // Chroma is constant along each row, with Cr neutral, so the lone last
        // pixel decodes the same whichever way FFmpeg spreads chroma.
        let exe = executable().unwrap();
        let (width, height) = (5usize, 3usize);
        for (format, order) in [(0x0210_0032, [0, 1, 2, 3]), (0x0210_001f, [1, 0, 3, 2])] {
            let raw = RawInput::for_format(format).unwrap();
            let data = (0..height)
                .flat_map(|y| {
                    let cb = 60 + 40 * u8::try_from(y).unwrap();
                    (0..width).step_by(2).flat_map(move |x| {
                        let pair = [
                            40 + 30 * u8::try_from(x).unwrap(),
                            cb,
                            55 + 30 * u8::try_from(x).unwrap(),
                            128,
                        ];
                        let bytes = order.map(|i| pair[i]);
                        bytes.into_iter().take(if x + 1 < width { 4 } else { 2 })
                    })
                })
                .collect::<Vec<u8>>();
            assert_eq!(data.len(), width * height * 2);
            let frame = Frame {
                pixel_encoding: None,
                device_timestamp: None,
                id: 1,
                width: u32::try_from(width).unwrap(),
                height: u32::try_from(height).unwrap(),
                pixel_format: format,
                device_ts_ns: None,
                wall_ms: 0,
                received_monotonic_ns: None,
                chunks: None,
                multipart: None,
                data: data.into(),
                jpeg: None,
            };
            let expected = crate::frame::rgb(&frame).unwrap();
            let mut child = command(&exe)
                .args(["-hide_banner", "-loglevel", "error", "-f", "rawvideo"])
                .args(["-pix_fmt", raw.ffmpeg])
                .args(raw.color_options())
                .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let bytes = raw.bytes(&frame).unwrap();
            let feeder = thread::spawn(move || stdin.write_all(&bytes));
            let output = child.wait_with_output().unwrap();
            feeder.join().unwrap().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{}: {stderr}", raw.ffmpeg);
            assert_eq!(output.stdout.len(), expected.len(), "{}", raw.ffmpeg);
            for (i, (got, want)) in output.stdout.iter().zip(&expected).enumerate() {
                let difference = got.abs_diff(*want);
                assert!(difference <= 3, "{} byte {i}: {got} vs {want}", raw.ffmpeg);
            }
        }
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
                [
                    u8::try_from(40 + 2 * x).unwrap(),
                    u8::try_from(60 + 3 * y).unwrap(),
                    u8::try_from(200 - x - y).unwrap(),
                ]
            })
            .collect();
        let luma = |p: &[u8; 3]| {
            u16::try_from(
                (u32::from(p[0]) * 54 + u32::from(p[1]) * 183 + u32::from(p[2]) * 19) >> 8,
            )
            .unwrap()
        };
        let wide = |shift: u16| -> Vec<u8> {
            rgb.iter()
                .flat_map(|p| (luma(p) << shift).to_le_bytes())
                .collect()
        };
        let lumas: Vec<u16> = rgb.iter().map(luma).collect();
        let deep = |samples: &[u8], shift: u16| -> Vec<u16> {
            samples.iter().map(|&v| u16::from(v) << shift).collect()
        };
        let mosaic = |cfa| bayer(&rgb, width, cfa);
        let (gr, rg, gb, bg) = ([1, 0, 2, 1], [0, 1, 1, 2], [1, 2, 0, 1], [2, 1, 1, 0]);
        // JFIF Y'CbCr, the chroma of each pair averaged, in `order`.
        #[expect(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "the independent YCbCr reference retains rounded, clamped float-to-byte conversion"
        )]
        let yuv = |order: [usize; 4]| -> Vec<u8> {
            let ycbcr = |p: &[u8; 3]| {
                let [r, g, b] = p.map(f64::from);
                [
                    0.299 * r + 0.587 * g + 0.114 * b,
                    -0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0,
                    0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0,
                ]
            };
            rgb.as_chunks::<2>()
                .0
                .iter()
                .flat_map(|[a, b]| {
                    let (a, b) = (ycbcr(a), ycbcr(b));
                    let values = [a[0], a[1].midpoint(b[1]), b[0], a[2].midpoint(b[2])];
                    order.map(|i| values[i].round().clamp(0.0, 255.0) as u8)
                })
                .collect()
        };
        let formats: Vec<(u32, Vec<u8>)> = vec![
            (
                crate::types::MONO8,
                rgb.iter().map(|p| u8::try_from(luma(p)).unwrap()).collect(),
            ),
            (0x0110_0003, wide(2)),
            (0x0110_0005, wide(4)),
            (0x0110_0025, wide(6)),
            (0x0110_0007, wide(8)),
            (RGB8, rgb.iter().flatten().copied().collect()),
            (
                0x0218_0015,
                rgb.iter().flat_map(|p| [p[2], p[1], p[0]]).collect(),
            ),
            (
                0x0220_0016,
                rgb.iter().flat_map(|p| [p[0], p[1], p[2], 0x80]).collect(),
            ),
            (
                0x0220_0017,
                rgb.iter().flat_map(|p| [p[2], p[1], p[0], 0x80]).collect(),
            ),
            (0x0108_0008, mosaic(gr)),
            (0x0108_0009, mosaic(rg)),
            (0x0108_000a, mosaic(gb)),
            (0x0108_000b, mosaic(bg)),
            (0x010c_0047, lsb(&deep_luma(&lumas, 4), 12)),
            (0x010c_0004, gige(&deep_luma(&lumas, 2), 10)),
            (0x0110_0011, le(&deep(&mosaic(rg), 4))),
            (0x0110_0030, le(&deep(&mosaic(gb), 8))),
            (0x010a_0052, lsb(&deep(&mosaic(bg), 2), 10)),
            (0x010c_002a, gige(&deep(&mosaic(gr), 4), 12)),
            (0x0210_0032, yuv([0, 1, 2, 3])),
            (0x0210_0043, yuv([1, 0, 3, 2])),
        ];
        fn bayer(rgb: &[[u8; 3]], width: usize, cfa: [usize; 4]) -> Vec<u8> {
            (0..rgb.len())
                .map(|i| rgb[i][cfa[((i / width) & 1) * 2 + ((i % width) & 1)]])
                .collect()
        }
        fn deep_luma(lumas: &[u16], shift: u16) -> Vec<u16> {
            lumas.iter().map(|v| v << shift).collect()
        }
        fn le(samples: &[u16]) -> Vec<u8> {
            samples.iter().flat_map(|v| v.to_le_bytes()).collect()
        }
        // PFNC lsb packing, bit by bit; rows of 64 samples end on whole bytes.
        fn lsb(samples: &[u16], bits: usize) -> Vec<u8> {
            let mut out = vec![0u8; (samples.len() * bits).div_ceil(8)];
            for (i, &v) in samples.iter().enumerate() {
                for b in (0..bits).filter(|b| (v >> b) & 1 == 1) {
                    out[(i * bits + b) / 8] |= 1 << ((i * bits + b) % 8);
                }
            }
            out
        }
        // GigE Vision Packed: high bits outside, low bits in the middle byte.
        fn gige(samples: &[u16], bits: u16) -> Vec<u8> {
            let low = bits - 8;
            let mask = (1 << low) - 1;
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&[a, b]| {
                    [
                        u8::try_from(a >> low).unwrap(),
                        u8::try_from(a & mask | ((b & mask) << 4)).unwrap(),
                        u8::try_from(b >> low).unwrap(),
                    ]
                })
                .collect()
        }
        for (pixel_format, data) in formats {
            let raw = RawInput::for_format(pixel_format).unwrap();
            let frame = Frame {
                pixel_encoding: None,
                device_timestamp: None,
                id: 1,
                width: u32::try_from(width).unwrap(),
                height: u32::try_from(height).unwrap(),
                pixel_format,
                device_ts_ns: None,
                wall_ms: 0,
                received_monotonic_ns: None,
                chunks: None,
                multipart: None,
                data: data.into(),
                jpeg: None,
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
                .args(raw.color_options())
                .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let bytes = raw.bytes(&frame).unwrap();
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
                        error +=
                            (i64::from(output.stdout[i]) - i64::from(expected[i])).unsigned_abs();
                        count += 1;
                    }
                }
            }
            let mean = error as f64 / count as f64;
            assert!(mean < 3.0, "{}: mean error {mean:.2}", raw.ffmpeg);
        }
    }
    /// Frames decoded in their own format and converted by Capturefab look
    /// as `FFmpeg`'s own RGB24 conversion of them did, whatever the format,
    /// color range and matrix.
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn native_frames_decode_like_ffmpeg_rgb() {
        let exe = executable().unwrap();
        let dir = std::env::temp_dir().join(format!("capturefab-native-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let cases: [(&str, &[&str], &str); 10] = [
            (
                "709full.mp4",
                &[
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-colorspace",
                    "bt709",
                    "-color_range",
                    "pc",
                ],
                "YCbCr709Full_420_8_Planar",
            ),
            (
                "709.mp4",
                &[
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-colorspace",
                    "bt709",
                    "-color_range",
                    "tv",
                ],
                "YCbCr709_420_8_Planar",
            ),
            (
                "601.mp4",
                &["-c:v", "libx264", "-pix_fmt", "yuv420p"],
                "YCbCr601_420_8_Planar",
            ),
            (
                "full.mp4",
                &["-c:v", "libx264", "-pix_fmt", "yuvj420p"],
                "YCbCr420_8_Planar",
            ),
            (
                "422.mkv",
                &[
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv422p",
                    "-colorspace",
                    "bt709",
                ],
                "YCbCr709_422_8_Planar",
            ),
            (
                "444.mkv",
                &["-c:v", "libx264", "-pix_fmt", "yuv444p"],
                "YCbCr601_444_8_Planar",
            ),
            (
                "yuyv.nut",
                &["-c:v", "rawvideo", "-pix_fmt", "yuyv422"],
                "YCbCr601_422_8",
            ),
            (
                "uyvy.nut",
                &["-c:v", "rawvideo", "-pix_fmt", "uyvy422"],
                "YCbCr601_422_8_CbYCrY",
            ),
            ("rgb.mkv", &["-c:v", "ffv1", "-pix_fmt", "bgr0"], "RGB8"),
            (
                "gray.nut",
                &["-c:v", "rawvideo", "-pix_fmt", "gray"],
                "Mono8",
            ),
        ];
        for (name, encode, expected) in cases {
            let path = dir.join(name);
            let mut args: Vec<String> = [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=322x182:rate=10",
                "-frames:v",
                "4",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
            args.extend(encode.iter().map(ToString::to_string));
            args.push(path.to_string_lossy().into());
            let (success, log) = bounded_output(&exe, &args, Duration::from_secs(20)).unwrap();
            assert!(success, "{name}: {log}");
            let source = path.to_string_lossy().to_string();
            let frames = |output| {
                let mut decoder =
                    Decoder::spawn_configured(&source, Duration::from_secs(10), &[], output)
                        .unwrap();
                std::iter::repeat_with(|| decoder.next(Duration::from_secs(10)).unwrap())
                    .take(4)
                    .collect::<Vec<_>>()
            };
            let (rgb, native) = (frames(Output::Rgb), frames(Output::Native));
            for (rgb, native) in rgb.iter().zip(&native) {
                assert_eq!(
                    crate::frame::pixel_format_name(native.pixel_format),
                    expected,
                    "{name}"
                );
                assert_eq!((native.width, native.height), (rgb.width, rgb.height));
                let converted = crate::frame::rgb(native).unwrap();
                let (mut total, mut worst) = (0u64, 0u8);
                for (a, b) in converted.iter().zip(rgb.data.iter()) {
                    let difference = a.abs_diff(*b);
                    total += u64::from(difference);
                    worst = worst.max(difference);
                }
                let mean = total as f64 / converted.len() as f64;
                // FFmpeg's own conversion rounds a little differently.
                assert!(
                    mean < 1.5 && worst <= 4,
                    "{name}: mean {mean:.2}, worst {worst}"
                );
            }
        }
        // Matrices and ranges Capturefab does not carry as decoded, and
        // packed 4:2:2 of odd width, whose rows FFmpeg pads, are read as
        // RGB, the source opened again for it.
        for (name, (width, height), encode) in [
            (
                "2020.mp4",
                (64, 48),
                &[
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-colorspace",
                    "bt2020nc",
                    "-color_range",
                    "tv",
                ][..],
            ),
            (
                "yuyv-odd.nut",
                (321, 241),
                &["-vf", "format=yuyv422", "-c:v", "rawvideo"],
            ),
            (
                "uyvy-odd.nut",
                (321, 241),
                &["-vf", "format=uyvy422", "-c:v", "rawvideo"],
            ),
        ] {
            let path = dir.join(name);
            let mut args: Vec<String> = [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                // testsrc2 would round an odd size to an even one.
                &format!("testsrc=size={width}x{height}:rate=10"),
                "-frames:v",
                "3",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
            args.extend(encode.iter().map(ToString::to_string));
            args.push(path.to_string_lossy().into());
            let (success, log) = bounded_output(&exe, &args, Duration::from_secs(20)).unwrap();
            assert!(success, "{name}: {log}");
            let mut camera = open_url(path.to_str().unwrap(), Duration::from_secs(10)).unwrap();
            camera.start(0).unwrap();
            let frame = camera.next_frame(Duration::from_secs(10)).unwrap();
            assert_eq!(frame.pixel_format, RGB8, "{name}");
            assert_eq!(
                frame.data.len(),
                usize::try_from(width * height * 3).unwrap(),
                "{name}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }
    /// On a Mac, `VideoToolbox` decodes H.264 and HEVC files into NV12 frames
    /// that look as software decoding's do; a profile it lacks (H.264
    /// 4:4:4) is decoded in software and remembered.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn videotoolbox_decodes_files_into_nv12() {
        let exe = executable().unwrap();
        if !hwdec::works(&exe, "videotoolbox").unwrap() {
            eprintln!("no VideoToolbox decoding here; skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("capturefab-hwdec-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Whether the frames show the hardware decoded them: a 10-bit
        // source's are 8-bit whichever did, so nothing is learned from it.
        for (name, codec, pix_fmt, hardware) in [
            ("h264.mp4", "libx264", "yuv420p", Some(true)),
            ("hevc.mp4", "hevc_videotoolbox", "nv12", Some(true)),
            ("hevc10.mp4", "hevc_videotoolbox", "p010le", None),
            ("h264-444.mp4", "libx264", "yuv444p", Some(false)),
        ] {
            let path = dir.join(name);
            let args = [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=30",
                "-frames:v",
                "10",
                "-c:v",
                codec,
                "-pix_fmt",
                pix_fmt,
                path.to_str().unwrap(),
            ]
            .map(str::to_owned);
            let (success, log) = bounded_output(&exe, &args, Duration::from_secs(30)).unwrap();
            assert!(success, "{name}: {log}");
            let frame = |setting| {
                hwdec::SETTING.set(Some(setting));
                let mut camera = open_url(path.to_str().unwrap(), Duration::from_secs(10)).unwrap();
                camera.start(0).unwrap();
                let frame = camera.next_frame(Duration::from_secs(10)).unwrap();
                hwdec::SETTING.set(None);
                frame
            };
            let software = frame(hwdec::Setting::None);
            let decoded = frame(hwdec::Setting::Named("videotoolbox"));
            let format = crate::frame::pixel_format_name(decoded.pixel_format);
            if let Some(hardware) = hardware {
                assert_eq!(format.ends_with("Semiplanar"), hardware, "{name}: {format}");
            }
            let (a, b) = (
                crate::frame::rgb(&software).unwrap(),
                crate::frame::rgb(&decoded).unwrap(),
            );
            let mean = a
                .iter()
                .zip(&b)
                .map(|(a, b)| f64::from(a.abs_diff(*b)))
                .sum::<f64>()
                / a.len() as f64;
            assert!(mean < 2.0, "{name}: mean difference {mean:.2}");
            let class = hwdec::size_class(1280, 720);
            let codec = match name {
                "hevc10.mp4" => "hevc-deep",
                name if name.starts_with("hevc") => "hevc",
                _ => "h264",
            };
            assert_eq!(
                hwdec::remembered(&exe, "videotoolbox", codec, class),
                hardware,
                "{name}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }
    /// A mosaic recorded to a 4:2:0 encoder is demosaicked once, to RGB24,
    /// and never widened to RGB48 on its way.
    /// A stream from a camera slower than its rate gets each frame once and
    /// still carries the rate asked for: `FFmpeg` repeats the frames.
    #[test]
    #[cfg(unix)]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_streams_keep_their_rate_from_slower_cameras() {
        let exe = executable().unwrap();
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let receiver = command(&exe)
            .args(["-v", "error", "-i"])
            .arg(format!("udp://127.0.0.1:{port}?timeout=2000000"))
            .args(["-fps_mode", "passthrough", "-f", "framemd5", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        let mut forward = Forwarder::new(ForwardConfig {
            output: format!("udp://127.0.0.1:{port}"),
            ..stream("libx264", 60.0)
        })
        .unwrap();
        let started = Instant::now();
        let mut pushed = 0u64;
        while started.elapsed() < Duration::from_secs(2) {
            let mut frame = tiny(64, 48);
            frame.data = vec![(pushed * 8).to_le_bytes()[0]; 64 * 48 * 3].into();
            forward.push(frame).unwrap();
            pushed += 1;
            thread::sleep(Duration::from_millis(33));
        }
        let stats = forward.stats();
        forward.stop().unwrap();
        let output = receiver.wait_with_output().unwrap();
        let received = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("0,"))
            .count();
        // About 60 written and 120 received.
        assert!(stats.written <= pushed, "{stats:?}");
        assert!(
            received as u64 >= stats.written * 3 / 2,
            "{received} frames received of {} written: {}",
            stats.written,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    /// The decoded frames of a video, as `FFmpeg`'s framemd5 checksums them.
    fn decoded_checksums(exe: &Executable, path: &std::path::Path) -> Vec<String> {
        let output = command(exe)
            .args(["-hide_banner", "-v", "error", "-i"])
            .arg(path)
            .args([
                "-map",
                "0:v:0",
                "-fps_mode",
                "passthrough",
                "-f",
                "framemd5",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("0,"))
            .map(|line| line.rsplit(',').next().unwrap().trim().to_owned())
            .collect()
    }
    /// JPEGs of a gradient that moves with `index`, 4:2:2 as webcams send.
    #[cfg(feature = "jpeg")]
    fn moving_jpeg(width: usize, height: usize, index: usize) -> Vec<u8> {
        let pixels: Vec<u8> = (0..width * height * 3)
            .map(|i| {
                let (pixel, channel) = (i / 3, i % 3);
                ((pixel % width + index * 4) * (channel + 1) + pixel / width).to_le_bytes()[0]
            })
            .collect();
        turbojpeg::compress(
            turbojpeg::Image {
                pixels: &*pixels,
                width,
                pitch: width * 3,
                height,
                format: turbojpeg::PixelFormat::RGB,
            },
            90,
            turbojpeg::Subsamp::Sub2x1,
        )
        .unwrap()
        .to_vec()
    }
    /// A JPEG without an EOI, as webcams send, is read whole, and one that
    /// is not well formed is skipped like one that does not decode: the
    /// stream goes on. A source whose first JPEGs are all broken goes back
    /// to `FFmpeg`'s decoder.
    #[test]
    #[cfg(feature = "jpeg")]
    fn broken_motion_jpegs_are_skipped_and_open_ones_read_whole() {
        let jpegs: Vec<Vec<u8>> = (0..4).map(|i| moving_jpeg(32, 16, i)).collect();
        let open = &jpegs[1][..jpegs[1].len() - 2];
        let broken = [0xFF, 0xD8, 0x12, 0x34];
        let stream = [(&*jpegs[0]), open, &jpegs[2], &broken, &jpegs[3]].concat();
        let mut reader = BufReader::with_capacity(64, &*stream);
        let pool = crate::framebuf::pool();
        let mut jpeg_reader = JpegReader::default();
        for (id, jpeg) in [&jpegs[0], &jpegs[1], &jpegs[2], &jpegs[3]]
            .into_iter()
            .enumerate()
        {
            let frame = jpeg_reader.next(&mut reader, id as u64, pool).unwrap();
            assert_eq!(frame.jpeg.as_deref(), Some(&**jpeg), "frame {id}");
        }
        let ended = jpeg_reader.next(&mut reader, 4, pool).unwrap_err();
        assert!(!ended.is::<NeedsDecoding>(), "{ended:#}");
        let stream = [&broken[..], &broken, &broken, &jpegs[0]].concat();
        let mut reader = BufReader::new(&*stream);
        let error = JpegReader::default()
            .next(&mut reader, 1, pool)
            .unwrap_err();
        assert!(error.is::<NeedsDecoding>(), "{error:#}");
    }
    /// A Motion JPEG file, and a Motion JPEG stream over HTTP, are read as
    /// their own JPEGs: each frame keeps the one it was decoded from, and
    /// decodes as `FFmpeg` decodes it.
    #[test]
    #[cfg(feature = "jpeg")]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_motion_jpeg_frames_keep_their_jpegs() {
        let exe = executable().unwrap();
        let dir = temp_dir("mjpeg");
        let jpegs: Vec<Vec<u8>> = (0..30).map(|i| moving_jpeg(320, 240, i)).collect();
        let raw = dir.join("clip.mjpeg");
        fs::write(&raw, jpegs.concat()).unwrap();
        let clip = dir.join("clip.mkv");
        let (made, log) = bounded_output(
            &exe,
            &[
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "mjpeg",
                "-framerate",
                "30",
                "-i",
                raw.to_str().unwrap(),
                "-c:v",
                "copy",
                clip.to_str().unwrap(),
            ]
            .map(str::to_owned),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(made, "{log}");
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        camera.write_memory(32, &0u32.to_le_bytes()).unwrap();
        camera.start(0).unwrap();
        for (index, jpeg) in jpegs.iter().enumerate().take(10) {
            let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
            assert_eq!(frame.jpeg.as_deref(), Some(&**jpeg), "frame {index}");
            assert_eq!(crate::frame::frame_format_name(&frame), "YCbCr422_8_Planar");
        }
        camera.stop().unwrap();
        // FFmpeg's own decoding of the same JPEGs, as planes.
        let decoded = command(&exe)
            .args(["-v", "error", "-i"])
            .arg(&clip)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv422p",
                "-",
            ])
            .output()
            .unwrap();
        let mut camera = open_url(clip.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        camera.start(0).unwrap();
        let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
        camera.stop().unwrap();
        assert_eq!(frame.data.len(), decoded.stdout.len());
        let worst = frame
            .data
            .iter()
            .zip(&decoded.stdout)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 2, "differs from FFmpeg's decoding by {worst}");
        // A live stream over HTTP, found to be Motion JPEG once opened.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = jpegs.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let served = served.clone();
                thread::spawn(move || {
                    let mut request = [0; 4096];
                    let _ = stream.read(&mut request);
                    let header = "HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n";
                    if stream.write_all(header.as_bytes()).is_err() {
                        return;
                    }
                    for jpeg in served.iter().cycle() {
                        let part = format!(
                            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                            jpeg.len()
                        );
                        if stream.write_all(part.as_bytes()).is_err()
                            || stream.write_all(jpeg).is_err()
                            || stream.write_all(b"\r\n").is_err()
                        {
                            return;
                        }
                        thread::sleep(Duration::from_millis(33));
                    }
                });
            }
        });
        let opened = Instant::now();
        let mut camera = open_url(
            &format!("http://127.0.0.1:{port}/video"),
            Duration::from_secs(5),
        )
        .unwrap();
        camera.start(0).unwrap();
        let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
        println!("HTTP Motion JPEG: first frame after {:?}", opened.elapsed());
        assert!(
            jpegs
                .iter()
                .any(|jpeg| frame.jpeg.as_deref() == Some(&**jpeg))
        );
        camera.stop().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
    /// A copied recording is the source's own video: its frames decode to
    /// exactly a run of the source's, from a key frame, in each container
    /// it records to.
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_copies_compressed_video_frame_for_frame() {
        let exe = executable().unwrap();
        let listing = encoder_listing(&exe).unwrap();
        let dir = temp_dir("stream-copy");
        let mut sources = vec![("h264", "libx264")];
        for hevc in ["libx265", "hevc_videotoolbox"] {
            if listed(&listing, hevc) {
                sources.push(("hevc", hevc));
                break;
            }
        }
        for (codec, encoder) in sources {
            let source = dir.join(format!("{codec}.mp4"));
            let (made, log) = bounded_output(
                &exe,
                &[
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=320x240:rate=30",
                    "-t",
                    "6",
                    "-c:v",
                    encoder,
                    "-g",
                    "15",
                    // No B-frames, as cameras send: a stream cut after a
                    // P-frame then misses none that display before it.
                    "-bf",
                    "0",
                    "-pix_fmt",
                    "yuv420p",
                    source.to_str().unwrap(),
                ]
                .map(str::to_owned),
                Duration::from_secs(20),
            )
            .unwrap();
            assert!(made, "{log}");
            let original = decoded_checksums(&exe, &source);
            for container in ["mkv", "mp4", "ts"] {
                let mut camera =
                    open_url(source.to_str().unwrap(), Duration::from_secs(5)).unwrap();
                let tap = camera
                    .copy_stream(StreamCopy::Any, "h264")
                    .unwrap()
                    .unwrap();
                assert_eq!(tap.codec(), codec);
                camera.start(0).unwrap();
                let output = dir.join(format!("{codec}-copy.{container}"));
                let mut forward = Forwarder::copying(
                    ForwardConfig {
                        timeline: None,
                        output: output.to_string_lossy().into(),
                        codec: codec.into(),
                        encoder: "auto".into(),
                        fps: 30.0,
                        bitrate: "8M".into(),
                        max_file_bytes: None,
                        min_free: None,
                    },
                    None,
                    tap,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    forward
                        .push(camera.next_frame(Duration::from_secs(3)).unwrap())
                        .unwrap();
                }
                forward.stop().unwrap();
                camera.stop().unwrap();
                let stats = forward.stats();
                assert_eq!(stats.encoder, "copy");
                let copied = decoded_checksums(&exe, &output);
                assert!(copied.len() >= 30, "{container}: {} frames", copied.len());
                assert_eq!(copied.len() as u64, stats.written, "{container}");
                let start = original
                    .iter()
                    .position(|frame| *frame == copied[0])
                    .unwrap();
                assert_eq!(start % 15, 0, "{codec} {container} starts at a key frame");
                assert_eq!(
                    copied,
                    original[start..start + copied.len()],
                    "{codec} {container}"
                );
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffmpeg_demosaics_a_recorded_mosaic_once() {
        let exe = executable().unwrap();
        let (width, height) = (64u32, 48u32);
        let mut encoders = vec!["libx264"];
        if cfg!(target_os = "macos") {
            encoders.push("h264_videotoolbox");
        }
        // BayerRG8, BayerRG12p and BayerRG16.
        for (format, expected) in [
            (0x0108_0009, "bayer_rggb8"),
            (0x010c_0059, "bayer_rggb8"),
            (0x0110_002f, "bayer_rggb16le"),
        ] {
            let frame = Frame {
                pixel_encoding: None,
                width,
                height,
                pixel_format: format,
                data: vec![0x5a; crate::frame::row_bytes(width, format) * height as usize].into(),
                ..Frame::default()
            };
            for encoder in &encoders {
                // As a recording hands it over: BayerRG12p narrowed to 8
                // bits, BayerRG16 read by FFmpeg as it is.
                let raw = RawInput::for_format(format).unwrap().for_encoder(encoder);
                let bytes = raw.bytes(&frame).unwrap();
                let pix_fmt = raw.input_pix_fmt(encoder);
                assert_eq!(pix_fmt, expected);
                let mut child = command(&exe)
                    .args([
                        "-hide_banner",
                        "-v",
                        "verbose",
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                    ])
                    .arg(pix_fmt)
                    .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                    .args(["-frames:v", "1", "-c:v", encoder])
                    .args(encoder_output_options(encoder, EncoderInput::Yuv, pix_fmt))
                    .args(["-f", "null", "-"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut stdin = child.stdin.take().unwrap();
                let input = bytes;
                let feeder = thread::spawn(move || stdin.write_all(&input));
                let output = child.wait_with_output().unwrap();
                let _ = feeder.join().unwrap();
                let log = String::from_utf8_lossy(&output.stderr);
                if !output.status.success() && log.contains("cannot create compression session") {
                    eprintln!("{encoder}: no hardware encoder here; skipped");
                    continue;
                }
                assert!(output.status.success(), "{encoder} {pix_fmt}: {log}");
                // The filter graph's conversions, `fmt:a ... -> ... fmt:b`.
                let conversions: Vec<(String, String)> = log
                    .lines()
                    .filter(|line| line.contains(" -> ") && line.contains("fmt:"))
                    .map(|line| {
                        let formats: Vec<String> = line
                            .split_whitespace()
                            .filter_map(|word| word.strip_prefix("fmt:"))
                            .map(str::to_owned)
                            .collect();
                        (formats[0].clone(), formats[1].clone())
                    })
                    .collect();
                let from_bayer: Vec<_> = conversions
                    .iter()
                    .filter(|(from, _)| from.starts_with("bayer_"))
                    .collect();
                assert_eq!(from_bayer.len(), 1, "{encoder} {pix_fmt}: {conversions:?}");
                assert_eq!(
                    from_bayer[0].1, "rgb24",
                    "{encoder} {pix_fmt}: {conversions:?}"
                );
                assert!(
                    !conversions.iter().any(|(_, to)| to.starts_with("rgb48")),
                    "{encoder} {pix_fmt}: {conversions:?}"
                );
            }
        }
    }
    /// FFV1 keeps a recorded mosaic exactly: decoded as gray, every sample
    /// is the frame's own (widened to 16 bits for a 12-bit mosaic). Deep
    /// RGB, packed or not, keeps every bit of each component.
    #[test]
    #[ignore = "requires a bundled FFmpeg or CAPTUREFAB_FFMPEG executable"]
    fn ffv1_recordings_keep_the_mosaic_exactly() {
        let exe = executable().unwrap();
        let dir = std::env::temp_dir().join(format!("capturefab-ffv1-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (width, height) = (64u32, 48u32);
        // BayerRG8, BayerRG12p, BayerRG12, RGB12, BGR10p, RGB16.
        for format in [
            0x0108_0009,
            0x010c_0059,
            0x0110_0011,
            0x0230_001a,
            0x021e_0048,
            0x0230_0033,
        ] {
            let raw = RawInput::for_format(format).unwrap().for_encoder("ffv1");
            let mut seed = format;
            let data: Vec<u8> = std::iter::repeat_with(|| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed.to_le_bytes()[0]
            })
            .take(crate::frame::row_bytes(width, format) * height as usize)
            .collect();
            let frame = Frame {
                pixel_encoding: None,
                width,
                height,
                pixel_format: format,
                data: data.into(),
                ..Frame::default()
            };
            // BayerRG12 keeps only its 12 bits.
            let frame = if format == 0x0110_0011 {
                let samples: Vec<u8> = frame
                    .data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .flat_map(|s| (u16::from_le_bytes(*s) & 0x0fff).to_le_bytes())
                    .collect();
                Frame {
                    data: samples.into(),
                    ..frame
                }
            } else {
                frame
            };
            let bytes = raw.bytes(&frame).unwrap();
            let pix_fmt = raw.input_pix_fmt("ffv1");
            let path = dir.join(format!("{format:08x}.mkv"));
            let mut child = command(&exe)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                ])
                .arg(pix_fmt)
                .args(["-video_size", &format!("{width}x{height}"), "-i", "pipe:0"])
                .args(["-c:v", "ffv1"])
                .args(encoder_output_options("ffv1", EncoderInput::Yuv, pix_fmt))
                .arg(&path)
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let input = bytes.clone();
            let feeder = thread::spawn(move || stdin.write_all(&input));
            let output = child.wait_with_output().unwrap();
            feeder.join().unwrap().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let decoded = command(&exe)
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&path)
                .args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "pipe:1"])
                .output()
                .unwrap();
            assert!(decoded.status.success());
            assert!(decoded.stdout == bytes, "{format:08x}: the mosaic changed");
        }
        let _ = fs::remove_dir_all(dir);
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
        .map(str::to_owned);
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
            timeline: None,
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 10.0,
            bitrate: "1M".into(),
            max_file_bytes: None,
            min_free: None,
        })
        .unwrap();
        for frame in frames {
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
    #[ignore = "requires FFmpeg with libx264 and FFV1"]
    fn ffmpeg_recording_clock_preserves_frames_received_in_bursts() {
        let exe = executable().unwrap();
        let dir = temp_dir("recording-clock");
        for (encoder, frozen) in [
            ("ffv1", false),
            ("libx264", false),
            ("ffv1", true),
            ("libx264", true),
        ] {
            let path = dir.join(format!("{encoder}-{frozen}.mkv"));
            let mut options =
                recording_clock_options(encoder_output_options(encoder, EncoderInput::Yuv, "gray"));
            if frozen {
                // Model a clock whose value does not advance between frames.
                for option in &mut options {
                    *option = option.replace("RTCTIME-RTCSTART", "0");
                }
            }
            let mut child = command(&exe)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "gray",
                    "-video_size",
                    "64x48",
                    "-framerate",
                    "30",
                    "-use_wallclock_as_timestamps",
                    "1",
                    "-i",
                    "pipe:0",
                    "-an",
                    "-c:v",
                    encoder,
                ])
                .args(options)
                .args(["-fps_mode", "vfr"])
                .arg(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let feeder = thread::spawn(move || -> std::io::Result<()> {
                for value in 0..10u8 {
                    input.write_all(&vec![value; 64 * 48 * 3])?;
                    if !frozen {
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                Ok(())
            });
            let output = child.wait_with_output().unwrap();
            let fed = feeder.join().unwrap();
            assert!(
                output.status.success(),
                "{encoder}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            fed.unwrap();
            let decoded = command(&exe)
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-fps_mode", "passthrough", "-f", "framemd5", "-"])
                .output()
                .unwrap();
            assert!(
                decoded.status.success() && decoded.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&decoded.stdout)
                    .lines()
                    .filter(|line| line.starts_with("0,"))
                    .count(),
                30,
                "{encoder} frozen={frozen}"
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264; loads every core for about 10 s"]
    fn ffmpeg_recording_lasts_as_long_as_the_wall_clock_under_load() {
        let dir = temp_dir("timeline");
        let output = dir.join("load.mkv");
        let busy = Arc::new(AtomicBool::new(true));
        let hogs = std::iter::repeat_with(|| {
            let busy = busy.clone();
            thread::spawn(move || {
                let mut x = 1u64;
                while busy.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005) + 1);
                }
            })
        })
        .take(cores() * 2)
        .collect::<Vec<_>>();
        let mut forward = Forwarder::new(ForwardConfig {
            timeline: None,
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "8M".into(),
            max_file_bytes: None,
            min_free: None,
        })
        .unwrap();
        let (width, height) = (1920usize, 1080usize);
        // Moving content keeps the encoder busy.
        let pictures = (0..8u8)
            .map(|shift| {
                (0..width * height * 3)
                    .map(|i| {
                        (i / 3 % width).to_le_bytes()[0]
                            ^ (i / (3 * width)).to_le_bytes()[0]
                            ^ (shift * 32)
                    })
                    .collect::<Vec<u8>>()
            })
            .collect::<Vec<_>>();
        let interval = Duration::from_secs_f64(1.0 / 30.0);
        let mut next = Instant::now();
        let mut started = None;
        for id in 0u64.. {
            forward
                .push(Frame {
                    pixel_encoding: None,
                    device_timestamp: None,
                    id,
                    width: u32::try_from(width).unwrap(),
                    height: u32::try_from(height).unwrap(),
                    pixel_format: RGB8,
                    device_ts_ns: None,
                    wall_ms: 0,
                    received_monotonic_ns: None,
                    chunks: None,
                    multipart: None,
                    data: pictures
                        [usize::try_from(id % u64::try_from(pictures.len()).unwrap()).unwrap()]
                    .clone()
                    .into(),
                    jpeg: None,
                })
                .unwrap();
            // The recording starts when FFmpeg takes its first frame.
            if started.is_none() && forward.stats().written > 0 {
                started = Some(Instant::now());
            }
            if started.is_some_and(|started| started.elapsed() >= Duration::from_secs(10)) {
                break;
            }
            next += interval;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        let wall = started.unwrap().elapsed().as_secs_f64();
        let stats = forward.stats();
        forward.stop().unwrap();
        busy.store(false, Ordering::Relaxed);
        for hog in hogs {
            hog.join().unwrap();
        }
        let (frames, duration) = recorded(&output);
        eprintln!(
            "wall {wall:.3} s, file {duration:.3} s, frames {frames}, accepted {}, written {}, late {}",
            stats.accepted, stats.written, stats.late
        );
        assert!(
            (duration - wall).abs() / wall < 0.02,
            "{duration} vs {wall}"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    /// A recording's frame count and the seconds its packets span.
    fn recorded(path: &std::path::Path) -> (usize, f64) {
        let exe = executable().unwrap();
        let args = ["-hide_banner", "-i", path.to_str().unwrap()]
            .into_iter()
            .chain(["-c", "copy", "-f", "framecrc", "-"])
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let (ok, log) = bounded_output(&exe, &args, Duration::from_secs(60)).unwrap();
        assert!(ok, "{log}");
        let base = log
            .lines()
            .find_map(|line| line.strip_prefix("#tb 0: "))
            .and_then(|base| base.split_once('/'))
            .map(|(num, den)| {
                num.trim().parse::<f64>().unwrap() / den.trim().parse::<f64>().unwrap()
            })
            .unwrap();
        // stream, dts, pts, duration, size, checksum
        let packets = log
            .lines()
            .filter(|line| line.starts_with("0,"))
            .map(|line| {
                let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
                let number = |index: usize| fields[index].parse::<f64>().unwrap();
                (number(2), number(2) + number(3))
            })
            .collect::<Vec<_>>();
        let start = packets.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let end = packets.iter().map(|p| p.1).fold(0.0, f64::max);
        (packets.len(), (end - start) * base)
    }
    #[test]
    #[ignore = "requires FFmpeg with libx264"]
    fn ffmpeg_recording_leaves_a_gap_for_a_stall_and_stops_at_once() {
        let dir = temp_dir("gap");
        let output = dir.join("gap.mkv");
        let mut forward = Forwarder::new(ForwardConfig {
            timeline: None,
            output: output.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            max_file_bytes: None,
            min_free: None,
        })
        .unwrap();
        let (width, height) = (1280usize, 720usize);
        let push = |forward: &mut Forwarder, id: u64| {
            forward
                .push(Frame {
                    pixel_encoding: None,
                    device_timestamp: None,
                    id,
                    width: u32::try_from(width).unwrap(),
                    height: u32::try_from(height).unwrap(),
                    pixel_format: RGB8,
                    device_ts_ns: None,
                    wall_ms: 0,
                    received_monotonic_ns: None,
                    chunks: None,
                    multipart: None,
                    data: vec![(id * 8).to_le_bytes()[0]; width * height * 3].into(),
                    jpeg: None,
                })
                .unwrap();
            thread::sleep(Duration::from_secs_f64(1.0 / 30.0));
        };
        let started = Instant::now();
        for id in 0..30 {
            push(&mut forward, id);
        }
        // The camera is gone for 8 s, then returns.
        thread::sleep(Duration::from_secs(8));
        let resumed = Instant::now();
        for id in 30..60 {
            push(&mut forward, id);
        }
        let catching_up = resumed.elapsed();
        let wall = started.elapsed().as_secs_f64();
        let stopping = Instant::now();
        forward.stop().unwrap();
        let stopped = stopping.elapsed();
        let (frames, duration) = recorded(&output);
        eprintln!(
            "wall {wall:.3} s, file {duration:.3} s, frames {frames}, resumed in {catching_up:?}, stopped in {stopped:?}"
        );
        // Each frame once, the outage a gap rather than copies to encode.
        assert!(frames <= 60, "{frames}");
        assert!((duration - wall).abs() < 0.5, "{duration} vs {wall}");
        assert!(catching_up < Duration::from_secs(2), "{catching_up:?}");
        assert!(stopped < Duration::from_secs(2), "{stopped:?}");
        fs::remove_dir_all(dir).unwrap();
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
        decoder.next(Duration::from_millis(150)).unwrap_err();
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
            timeline: None,
            output,
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 60.0,
            bitrate: "32M".into(),
            max_file_bytes: None,
            min_free: None,
        })
        .unwrap();
        let mut noise = 123u32;
        let data = std::iter::repeat_with(|| {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            noise.to_le_bytes()[0]
        })
        .take(640 * 480 * 3)
        .collect::<Vec<_>>();
        let frame = Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width: 640,
            height: 480,
            pixel_format: RGB8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: data.into(),
            jpeg: None,
        };
        forward.push(frame.clone()).unwrap();
        let start = Instant::now();
        for _ in 0..120 {
            forward.push(frame.clone()).unwrap();
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
            timeline: None,
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "capturefab_nonexistent_encoder".into(),
            fps: 30.0,
            bitrate: "1M".into(),
            max_file_bytes: None,
            min_free: None,
        })
        .unwrap();
        let frame = Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width: 64,
            height: 48,
            pixel_format: RGB8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: vec![0; 64 * 48 * 3].into(),
            jpeg: None,
        };
        let error = forward.push(frame).unwrap_err();
        assert!(format!("{error:#}").contains("is unavailable in this FFmpeg build"));
        forward.stop().unwrap();
        assert!(!path.exists(), "invalid recordings must be removed");
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_a_recording_cut_before_any_video_is_removed() {
        // A full disk is not an encoder failure, so auto does not fall back.
        for encoder in ["libx264", "auto"] {
            bounded_recording_is_removed_on_failure(encoder);
        }
    }
    fn bounded_recording_is_removed_on_failure(encoder: &str) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-bounded-recording-{nonce}.mp4"));
        let mut forward = Forwarder::new(ForwardConfig {
            timeline: None,
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: encoder.into(),
            fps: 30.0,
            bitrate: "1M".into(),
            max_file_bytes: Some(512),
            min_free: None,
        })
        .unwrap();
        let frame = Frame {
            pixel_encoding: None,
            device_timestamp: None,
            id: 1,
            width: 64,
            height: 48,
            pixel_format: RGB8,
            device_ts_ns: None,
            wall_ms: 0,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            data: vec![127; 64 * 48 * 3].into(),
            jpeg: None,
        };
        forward.push(frame.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while forward.stats().error.is_none() && Instant::now() < deadline {
            let _ = forward.push(frame.clone());
            thread::sleep(Duration::from_millis(20));
        }
        let encoder = forward.stats().encoder;
        let error = forward.stop().unwrap_err().to_string();
        assert!(
            error.contains("reached its maximum size before any video"),
            "{error}"
        );
        assert_eq!(forward.stats().encoder, encoder);
        assert!(
            !path.exists(),
            "a recording cut before it holds any video must be removed"
        );
    }
    #[test]
    #[ignore = "requires FFmpeg"]
    fn ffmpeg_a_recording_that_reaches_its_maximum_size_keeps_the_video_before() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("capturefab-capped-recording-{nonce}.mkv"));
        let limit = 256 << 10;
        let mut forward = Forwarder::new(ForwardConfig {
            timeline: None,
            output: path.to_string_lossy().into(),
            codec: "h264".into(),
            encoder: "libx264".into(),
            fps: 30.0,
            bitrate: "8M".into(),
            max_file_bytes: Some(limit),
            min_free: None,
        })
        .unwrap();
        // Noise compresses poorly, so the limit comes within seconds.
        let mut seed = 1u32;
        let deadline = Instant::now() + Duration::from_secs(20);
        for id in 1.. {
            if forward.stats().error.is_some() || Instant::now() >= deadline {
                break;
            }
            let data = std::iter::repeat_with(|| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .take(160 * 120 * 3)
            .collect();
            let frame = Frame {
                pixel_encoding: None,
                device_timestamp: None,
                received_monotonic_ns: None,
                chunks: None,
                multipart: None,
                id,
                width: 160,
                height: 120,
                pixel_format: RGB8,
                device_ts_ns: None,
                wall_ms: id * 33,
                data,
                jpeg: None,
            };
            let _ = forward.push(frame);
            thread::sleep(Duration::from_millis(33));
        }
        // It ends as asked, within the bytes it was given; one FFmpeg
        // overran is cut at exactly that many, and says so.
        let end = forward
            .storage_end()
            .expect("the recording reached its size");
        assert!(end.is::<crate::storage::MaxSize>(), "{end:#}");
        forward.stop().unwrap();
        let recorded = forward
            .stats()
            .recording
            .expect("the recording up to its limit is kept");
        let bytes = fs::metadata(&path).unwrap().len();
        assert!(bytes <= limit);
        assert_eq!(recorded.truncated, bytes == limit);
        assert_eq!(recorded.bytes, bytes);
        let exe = executable().unwrap();
        let args = [
            "-v",
            "error",
            "-i",
            &path.to_string_lossy(),
            "-fps_mode",
            "passthrough",
            "-f",
            "framemd5",
            "-",
        ]
        .map(String::from);
        let (_, log) = bounded_output(&exe, &args, Duration::from_secs(30)).unwrap();
        let frames = log.lines().filter(|line| line.starts_with("0,")).count();
        assert!(frames > 0, "{log}");
        fs::remove_file(&path).unwrap();
    }
    #[test]
    fn recording_limit_counters_saturate_instead_of_wrapping() {
        let now = Instant::now();
        let mut near = NearEnd::new(u64::MAX);
        assert!(near.reached_at(u64::MAX - 10, now));
        assert!(near.reached_at(u64::MAX, now + Duration::from_millis(1)));
        // Counter resets cannot make a later burst wrap below its limit.
        assert!(near.reached_at(0, now + Duration::from_millis(2)));
        assert!(near.reached_at(u64::MAX, now + Duration::from_millis(3)));
    }

    /// A recording with a maximum size ends while `FFmpeg`'s next part, the
    /// largest so far with a quarter more, and a second of its output
    /// still fit.
    #[test]
    fn a_capped_recording_ends_while_its_next_part_still_fits() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut near = NearEnd::new(10 << 20);
        // MP4 fragments of 1 MiB every 2 s, each in reads 5 ms apart.
        let mut written = 0;
        let mut ended = None;
        'parts: for part in 0..20u64 {
            for read in 0..16u64 {
                written += 64 << 10;
                if near.reached_at(written, at(part * 2000 + read * 5)) {
                    ended = Some(written);
                    break 'parts;
                }
            }
        }
        // Within the ninth part, once another 1.25 MiB would not fit in 10.
        assert!(ended.is_some_and(|ended| ended > 8 << 20 && ended <= 9 << 20));
        assert_eq!(near.largest, 1 << 20);
        // A fast stream's second of output counts more than its parts.
        let mut near = NearEnd::new(100 << 20);
        let mut written = 0;
        let mut ms = 0;
        let ended = loop {
            written += 3 << 20;
            ms += 33;
            if near.reached_at(written, at(ms)) {
                break written;
            }
        };
        assert!(
            ended <= 100 << 20 && ended + (60 << 20) >= 100 << 20,
            "{ended}"
        );
        // However small the limit, a quarter MiB is kept.
        assert!(NearEnd::new(1 << 20).reached_at((1 << 20) - (200 << 10), at(0)));
    }
    #[test]
    fn forwarding_configuration_fails_closed() {
        let config = ForwardConfig {
            timeline: None,
            output: "video.mp4".into(),
            codec: "h264".into(),
            encoder: "auto".into(),
            fps: 30.0,
            bitrate: "4M".into(),
            max_file_bytes: None,
            min_free: None,
        };
        Forwarder::new(config.clone()).unwrap();
        let mut serialized = serde_json::to_value(&config).unwrap();
        serialized["bitarte"] = json!("8M");
        assert!(
            serde_json::from_value::<ForwardConfig>(serialized)
                .unwrap_err()
                .to_string()
                .contains("unknown field `bitarte`")
        );
        for bitrate in [
            "0",
            "0M",
            "NaN",
            "inf",
            "1.2.3M",
            "4MM",
            "5G",
            "0.0000001M",
            "4294967296",
            "1m",
            "1g",
            "1\u{212a}",
        ] {
            let mut invalid = config.clone();
            invalid.bitrate = bitrate.into();
            assert!(invalid.validate().is_err(), "{bitrate}");
        }
        for (bitrate, expected) in [
            ("2.01k", 2010),
            ("2500K", 2_500_000),
            ("1G", 1_000_000_000),
            (".5M", 500_000),
            ("1.000000M", 1_000_000),
            ("0.000001M", 1),
            ("4294967295", u64::from(u32::MAX)),
        ] {
            assert_eq!(bits_per_second(bitrate).unwrap(), expected, "{bitrate}");
        }
        for encoder in ["", "-i", "libx264;", "libx265"] {
            let mut invalid = config.clone();
            invalid.encoder = encoder.into();
            assert!(invalid.validate().is_err(), "{encoder}");
        }
        validate_encoder("av1", "libaom-av1").unwrap();
        for output in [
            "https://",
            "rtsp://camera:70000/live",
            "file:",
            "video.mp4/",
            "video.webm",
        ] {
            let mut invalid = config.clone();
            invalid.output = output.into();
            assert!(invalid.validate().is_err(), "{output}");
        }
        let mut invalid = config;
        invalid.fps = f64::NAN;
        assert!(Forwarder::new(invalid).is_err());
    }
    #[test]
    fn host_cameras_are_named_by_their_operating_system_scheme() {
        for selector in [
            "avfoundation:FaceTime HD Camera",
            "avfoundation:0",
            "AVFoundation:1?fps=30",
            "v4l2:/dev/video0",
            "dshow:video=Integrated Camera",
        ] {
            assert!(is_host_camera(selector), "{selector}");
        }
        for selector in [
            "sim:0",
            "rtsp://camera/live",
            "file:clip.mp4",
            "clip.mp4",
            "C:/video/clip.mp4",
            "gige:192.168.1.20",
            "scanner:sane:x",
            "",
        ] {
            assert!(!is_host_camera(selector), "{selector}");
        }
    }
    #[test]
    fn explicit_encoders_must_be_installed_video_encoders_for_the_selected_codec() {
        let listing = " V....D libx264 H.264 (codec h264)\n V....D libaom-av1 AV1 (codec av1)\n A..... aac AAC\n V..... mjpeg Motion JPEG\n V....D custom New encoder (codec hevc)\n";
        validate_encoder_listing(listing, "libx264", "h264").unwrap();
        validate_encoder_listing(listing, "libaom-av1", "av1").unwrap();
        validate_encoder_listing(listing, "custom", "hevc").unwrap();
        for (encoder, family) in [
            ("libx264", "av1"),
            ("aac", "h264"),
            ("mjpeg", "h264"),
            ("missing", "h264"),
        ] {
            assert!(validate_encoder_listing(listing, encoder, family).is_err());
        }
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
            formats: Vec::new(),
            reconnect: None,
            paced: true,
            on_demand: None,
            unpace: false,
            output: Output::Native,
            pixel_format: RGB8,
            hwdec: None,
            software: false,
            known: None,
            passthrough: cfg!(feature = "jpeg"),
            jpeg_before: false,
            tap: None,
        }
    }
    #[test]
    fn every_link_of_a_sources_xml_names_a_node() {
        let mode = Mode {
            width: 640,
            height: 480,
            fps: Some((15.0, 30.0)),
            input_format: None,
            intervals: None,
            min_size: None,
        };
        for (native, modes, playback) in [
            (true, vec![mode], false),
            (true, Vec::new(), false),
            (false, Vec::new(), true),
            (false, Vec::new(), false),
        ] {
            let nodes = NodeMap::parse(&backend_xml(native, &modes, playback)).unwrap();
            assert_eq!(nodes.unresolved(), None, "{native} {playback}");
        }
    }
    #[test]
    #[cfg(unix)]
    fn a_live_source_resting_lets_go_of_the_ffmpeg_it_connected_with() {
        let fake = Fake::new(
            "rest",
            r#"case "$*" in *udp://127.0.0.1:5999*) echo "$*" >> "$dir/argv" ;; *) exit 0 ;; esac
while :; do printf 'P6\n2 2\n255\n'; head -c 12 /dev/zero; sleep 0.05; done"#,
        );
        let source = "udp://127.0.0.1:5999";
        let mut camera = open_url(source, Duration::from_secs(5)).unwrap();
        assert!(camera.lingers());
        let opened = fake.launches(&[source]);
        assert!(opened >= 1);
        camera.rest();
        // Started again, it opens the source again.
        camera.start(12).unwrap();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(fake.launches(&[source]), opened + 1);
        // While it streams, it keeps the FFmpeg it streams with.
        camera.rest();
        camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(fake.launches(&[source]), opened + 1);
        camera.stop().unwrap();
    }
    /// A recording copies a tapped H.264 stream from its last key frame,
    /// through `FFmpeg` reading MPEG-TS, with no encoder; other codecs and
    /// recordings that want frames are not tapped.
    #[test]
    #[cfg(unix)]
    fn recordings_copy_a_compressed_source_from_its_last_key_frame() {
        use copy::tests::{frame, tables};
        let fake = Fake::native(
            "copy",
            r#"echo "$*" >> "$dir/argv"
case "$*" in
*"-i pipe:0"*) exec cat ;;
*-encoders*) exit 1 ;;
esac
printf '[info]   Stream #0:0: Video: h264 (High), yuv420p, 4x2, 30 fps\n[info] Output #0, rawvideo, to '"'"'pipe:1'"'"':\n[info]   Stream #0:0: Video: rawvideo, yuv420p(tv, bt709), 4x2\n' >&2
fd=$(echo "$*" | sed -n 's/.*-f mpegts pipe:\([0-9]*\).*/\1/p')
if [ -n "$fd" ]; then
    { cat "$dir/ts1"; sleep 0.5; cat "$dir/ts2"; } > "/dev/fd/$fd" &
fi
while :; do head -c 12 /dev/zero; sleep 0.02; done"#,
        );
        let ts1 = [
            tables(),
            frame(1, true, 2),
            frame(2, false, 1),
            tables(),
            frame(3, true, 3),
            frame(4, false, 1),
        ]
        .concat();
        let ts2 = [frame(5, false, 2), frame(6, true, 1), frame(7, false, 1)].concat();
        fs::write(fake.dir.join("ts1"), &ts1).unwrap();
        fs::write(fake.dir.join("ts2"), &ts2).unwrap();
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        // Recordings that ask for another codec are encoded.
        assert!(
            camera
                .copy_stream(StreamCopy::Same, "hevc")
                .unwrap()
                .is_none()
        );
        assert_eq!(fake.launches(&["-f mpegts"]), 0);
        let tap = camera
            .copy_stream(StreamCopy::Any, "h264")
            .unwrap()
            .unwrap();
        assert_eq!(tap.codec(), "h264");
        // The decoder started again, copying the video beside its frames.
        assert_eq!(
            fake.launches(&[
                "-f rawvideo -avioflags direct pipe:1 -map 0:v:0 -an -sn -dn -c:v copy -flush_packets 1 -f mpegts pipe:"
            ]),
            1
        );
        camera.start(12).unwrap();
        thread::sleep(Duration::from_millis(200));
        let output = fake.dir.join("copy.mkv");
        let mut forward = Forwarder::copying(
            ForwardConfig {
                timeline: None,
                output: output.to_string_lossy().into(),
                codec: "h264".into(),
                encoder: "auto".into(),
                fps: 30.0,
                bitrate: "8M".into(),
                max_file_bytes: None,
                min_free: None,
            },
            None,
            tap.clone(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < deadline {
            forward
                .push(camera.next_frame(Duration::from_secs(1)).unwrap())
                .unwrap();
        }
        assert!(tap.attached());
        forward.stop().unwrap();
        assert!(!tap.attached());
        let stats = forward.stats();
        assert_eq!(
            (stats.encoder.as_str(), stats.codec.as_str()),
            ("copy", "h264")
        );
        let recorded = stats.recording.unwrap();
        assert_eq!(
            (recorded.codec.as_str(), recorded.encoder.as_str()),
            ("h264", "copy")
        );
        // From the last key frame before it started; the frame still
        // being received when it stopped is not in it.
        assert_eq!(
            fs::read(&output).unwrap(),
            [
                tables(),
                frame(3, true, 3),
                frame(4, false, 1),
                frame(5, false, 2),
                frame(6, true, 1)
            ]
            .concat()
        );
        assert_eq!(stats.written, 4);
        assert_eq!(
            fake.launches(&[
                "-f mpegts -i pipe:0",
                "-c:v copy -t 3155760000 -f matroska pipe:1"
            ]),
            1
        );
        camera.stop().unwrap();
    }
    #[test]
    fn a_live_source_gives_its_newest_frame_and_drops_the_older_ones() {
        let frame = |id| Frame {
            pixel_encoding: None,
            device_timestamp: None,
            received_monotonic_ns: None,
            chunks: None,
            multipart: None,
            id,
            width: 1,
            height: 1,
            pixel_format: RGB8,
            device_ts_ns: None,
            wall_ms: id,
            data: vec![0; 3].into(),
            jpeg: None,
        };
        let queue = DecoderQueue {
            state: Mutex::new(Decoded {
                frames: (1..=3).map(frame).collect(),
                error: None,
                needs_rgb: None,
                needs_decoding: None,
            }),
            changed: Condvar::new(),
            stopped: AtomicBool::new(false),
        };
        assert_eq!(queue.newest().map(|frame| frame.id), Some(3));
        assert!(queue.newest().is_none());
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
        // Continuity Camera: the 60 fps format of each size also runs at the
        // 30 fps its sibling format lists.
        assert_eq!(
            labels(&iphone),
            [
                "640x480@60",
                "640x480@30",
                "1280x720@60",
                "1280x720@30",
                "1920x1080@60",
                "1920x1080@30",
                "1920x1440@30"
            ]
        );
        assert!(iphone[0].honors(59.995) && !iphone[0].honors(59.98) && !iphone[0].honors(30.0));
        assert!(iphone[1].honors(30.0) && !iphone.iter().any(|m| m.honors(45.0)));
        let mut camera = native("avfoundation:iPhone Camera", iphone.clone());
        camera
            .configure(NativeOptions {
                size: Some((1920, 1080)),
                fps: Some(30.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options()[..4],
            ["-framerate", "30", "-video_size", "1920x1080"]
        );
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
                    input_format: None,
                    intervals: None,
                    min_size: None,
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
            format!("{error:#}")
                .contains("1920x1080@29.97 is not supported by HD Pro Webcam C920; supported: 1920x1080@30, 1920x1080@24,")
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
                    input_format: None,
                    intervals: None,
                    min_size: None,
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
        // A driver that enumerates no frame intervals leaves rates unbounded.
        let (modes, format) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Compressed:       mjpeg :          Motion-JPEG : 1920x1080 1280x720 640x480\n",
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       : Unsupported :         10-bit Bayer : 4056x3040\n",
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720 640x480\n",
                "[video4linux2,v4l2 @ 0x5581d1c2a0] Raw       :        gray :            8-bit Greyscale : {8-1920, 8}x{8-1080, 8}\n",
            ),
            |_, _| None,
        );
        assert_eq!(labels(&modes), ["640x480", "1280x720", "1920x1080"]);
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = format;
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
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        assert_eq!(
            nodes.choices(&mut camera, "VideoMode").unwrap(),
            ["640x480", "1280x720", "1920x1080"]
        );
        nodes.set(&mut camera, "VideoMode", "1920x1080").unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "60",
                "-video_size",
                "1920x1080",
                "-input_format",
                "mjpeg"
            ]
        );
        nodes.set(&mut camera, "VideoMode", "640x480").unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "60",
                "-video_size",
                "640x480",
                "-input_format",
                "yuyv422"
            ]
        );
    }
    #[test]
    fn dshow_intervals_reject_nonfinite_nonpositive_and_out_of_range_values() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            0.0,
            0.99,
            f64::from(u32::MAX) + 1.0,
        ] {
            assert_eq!(dshow_interval(value), None, "{value}");
        }
        assert_eq!(dshow_interval(1.99), Some(1));
        assert_eq!(dshow_interval(f64::from(u32::MAX)), Some(u32::MAX));
        for rate in [
            "NaN",
            "inf",
            "-inf",
            "0",
            "-1",
            "0.09",
            "1e300",
            "not a rate",
        ] {
            assert_eq!(dshow_intervals(rate), [] as [u32; 0], "{rate}");
        }
        assert_eq!(dshow_nominal(&[]), None);
        assert_eq!(dshow_nominal(&[0]), None);
        assert_eq!(dshow_nominal(&[0, 416_666]), Some(416_666));
    }

    #[test]
    fn dshow_rates_are_requested_as_the_listed_frame_intervals() {
        // A Blackmagic DeckLink listing from FFmpeg's capture wiki: each
        // capability has a single frame interval, which the requested rate
        // must reproduce exactly.
        let decklink = dshow_modes(concat!(
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=23.976 max s=1920x1080 fps=23.976\n",
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=24 max s=1920x1080 fps=24\n",
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=29.97 max s=1920x1080 fps=29.97\n",
            "[dshow @ 0000007201cda8a0]   pixel_format=uyvy422  min s=1920x1080 fps=60.0002 max s=1920x1080 fps=60.0002\n",
        ));
        assert_eq!(
            decklink.iter().map(|m| m.intervals).collect::<Vec<_>>(),
            [
                Some((417_083, 417_083)),
                Some((416_666, 416_666)),
                Some((333_667, 333_667)),
                Some((166_666, 166_666))
            ]
        );
        let mut camera = native("dshow:video=Decklink Video Capture", decklink);
        for (fps, rate, interval) in [
            (23.976, "23.976014395", 417_083),
            (24.0, "24.000009600", 416_666),
            (60.0, "60.000060000", 166_666),
        ] {
            camera
                .configure(NativeOptions {
                    size: Some((1920, 1080)),
                    fps: Some(fps),
                })
                .unwrap();
            assert_eq!(camera.input_options()[..2], ["-framerate", rate]);
            // FFmpeg floors 1e7 / rate into 100 ns units.
            #[expect(
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "this reference mirrors FFmpeg's truncation of a positive floating-point frame interval"
            )]
            let actual = (1e7 / rate.parse::<f64>().unwrap()) as u32;
            assert_eq!(actual, interval);
        }
        // A webcam whose fastest interval is 333334 rather than 333333:
        // asking for 30 fps would fall outside its range.
        let webcam = dshow_modes(
            "[dshow @ 000001b5e2a6c3c0]   pixel_format=yuyv422  min s=1280x720 fps=5 max s=1280x720 fps=29.9999\n",
        );
        assert_eq!(webcam[0].intervals, Some((333_335, 1_999_999)));
        let mut camera = native("dshow:video=USB Camera", webcam);
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(camera.input_options()[..2], ["-framerate", "29.999805001"]);
        camera
            .configure(NativeOptions {
                size: None,
                fps: Some(5.0),
            })
            .unwrap();
        assert_eq!(camera.input_options()[..2], ["-framerate", "5.000001250"]);
        camera
            .configure(NativeOptions {
                size: None,
                fps: Some(15.0),
            })
            .unwrap();
        assert_eq!(camera.input_options()[..2], ["-framerate", "15"]);
    }
    #[test]
    fn dshow_size_ranges_take_the_sizes_inside_them() {
        let modes = dshow_modes(concat!(
            "[dshow @ 000002a3c8b4e0c0]   vcodec=mjpeg  min s=1920x1080 fps=5 max s=1920x1080 fps=30\n",
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=bgr24  min s=640x480 fps=1 max s=1920x1080 fps=60\n",
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=rgb32  min s=320x240 fps=1 max s=1920x1080 fps=60\n",
        ));
        assert_eq!(modes.len(), 2);
        assert_eq!(modes[0].min_size, None);
        assert_eq!(modes[1].min_size, Some(((320, 240), 1.0)));
        let mut camera = native("dshow:video=Capture Filter", modes);
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: Some(30.0),
            })
            .unwrap();
        assert_eq!(camera.mode, Some(1));
        assert_eq!(
            camera.input_options(),
            ["-framerate", "30", "-video_size", "1280x720"]
        );
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        nodes.set(&mut camera, "Width", "640").unwrap();
        nodes.set(&mut camera, "Height", "480").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "15")
            .unwrap();
        assert_eq!(
            camera.input_options(),
            ["-framerate", "15", "-video_size", "640x480"]
        );
        assert!(
            camera
                .configure(NativeOptions {
                    size: Some((160, 120)),
                    fps: None,
                })
                .is_err()
        );
        // A fixed size at 5-60 fps merged with a range only from 30 fps.
        let merged = dshow_modes(concat!(
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=yuyv422  min s=1920x1080 fps=5 max s=1920x1080 fps=60\n",
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=bgr24  min s=640x480 fps=30 max s=1920x1080 fps=60\n",
        ));
        assert_eq!(merged.len(), 1);
        let mut camera = native("dshow:video=Capture Filter", merged);
        let open = |camera: &mut MediaBackend, size, fps| {
            camera
                .configure(NativeOptions {
                    size: Some(size),
                    fps: Some(fps),
                })
                .is_ok()
        };
        assert!(open(&mut camera, (1920, 1080), 5.0));
        assert!(open(&mut camera, (640, 480), 30.0));
        assert!(!open(&mut camera, (640, 480), 5.0));
        // Crossed ranges share only the sizes both reach.
        let crossed = dshow_modes(concat!(
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=bgr24  min s=640x240 fps=1 max s=1920x1080 fps=60\n",
            "[dshow @ 000002a3c8b4e0c0]   pixel_format=rgb32  min s=320x480 fps=1 max s=1920x1080 fps=60\n",
        ));
        assert_eq!(crossed[0].min_size, Some(((640, 480), 1.0)));
        let mut camera = native("dshow:video=Capture Filter", crossed);
        assert!(open(&mut camera, (800, 600), 30.0));
        assert!(!open(&mut camera, (320, 240), 30.0));
    }
    #[test]
    fn v4l2_modes_take_a_compressed_format_where_raw_cannot_reach_the_rate() {
        // The UVC camera of FFmpeg trac #9878, whose v4l2-ctl listing gives
        // YUYV 1280x720 at 10 fps and every other format and size at 30.
        let (modes, formats) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x5694e62800] Compressed:       mjpeg :          Motion-JPEG : 1280x720 320x240 640x360 640x480\n",
                "[video4linux2,v4l2 @ 0x5694e62800] Raw       :     yuyv422 :           YUYV 4:2:2 : 1280x720 320x240 640x360 640x480\n",
            ),
            |description, size| match (description, size) {
                ("YUYV 4:2:2", (1280, 720)) => Some((10.0, 10.0)),
                ("YUYV 4:2:2" | "Motion-JPEG", _) => Some((30.0, 30.0)),
                _ => None,
            },
        );
        assert_eq!(formats, ["yuyv422", "mjpeg"]);
        assert_eq!(
            labels(&modes),
            [
                "1280x720@10",
                "320x240@30",
                "640x360@30",
                "640x480@30",
                "1280x720@30"
            ]
        );
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = formats;
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "30",
                "-video_size",
                "1280x720",
                "-input_format",
                "mjpeg"
            ]
        );
        camera
            .configure(NativeOptions {
                size: Some((1280, 720)),
                fps: Some(10.0),
            })
            .unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "10",
                "-video_size",
                "1280x720",
                "-input_format",
                "yuyv422"
            ]
        );
        let rejected = NativeOptions {
            size: Some((1280, 720)),
            fps: Some(60.0),
        };
        assert_eq!(
            camera.configure(rejected).unwrap_err().to_string(),
            "1280x720@60 is not supported by /dev/video0; supported: 1280x720@30, 1280x720@10, 640x480@30, 640x360@30, 320x240@30"
        );
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "30")
            .unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@30");
        assert_eq!(camera.input_options()[5], "mjpeg");
        assert!(
            nodes
                .set(&mut camera, "AcquisitionFrameRate", "15")
                .is_err()
        );
        nodes.set(&mut camera, "VideoMode", "1280x720@10").unwrap();
        assert_eq!(camera.fps, 10.0);
        assert_eq!(camera.input_options()[5], "yuyv422");
    }
    #[test]
    fn v4l2_raw_format_is_kept_where_it_reaches_the_requested_rate() {
        // A C920's uvcvideo intervals: YUYV 1280x720 at 10, 7.5 and 5 fps,
        // and MJPEG 1280x720 and both 640x480 formats at 30 down to 5.
        let (modes, formats) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720\n",
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720\n",
            ),
            |description, size| match (description, size) {
                ("YUYV 4:2:2", (1280, 720)) => Some((5.0, 10.0)),
                _ => Some((5.0, 30.0)),
            },
        );
        assert_eq!(labels(&modes), ["640x480@30", "1280x720@10", "1280x720@30"]);
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = formats;
        let format = |camera: &MediaBackend| camera.input_options()[5].clone();
        for (fps, expected) in [
            (Some(10.0), "yuyv422"),
            (Some(30.0), "mjpeg"),
            (None, "mjpeg"),
        ] {
            let size = Some((1280, 720));
            camera.configure(NativeOptions { size, fps }).unwrap();
            assert_eq!(format(&camera), expected, "{fps:?}");
        }
        let nodes = NodeMap::parse(&camera.xml().unwrap()).unwrap();
        nodes.set(&mut camera, "VideoMode", "1280x720@10").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "10")
            .unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@10");
        assert_eq!(format(&camera), "yuyv422");
        nodes.set(&mut camera, "VideoMode", "1280x720@30").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "10")
            .unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@30");
        nodes.set(&mut camera, "VideoMode", "640x480@30").unwrap();
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "10")
            .unwrap();
        nodes.set(&mut camera, "Width", "1280").unwrap();
        nodes.set(&mut camera, "Height", "720").unwrap();
        assert_eq!(nodes.get(&mut camera, "VideoMode").unwrap(), "1280x720@10");
        assert_eq!(format(&camera), "yuyv422");
        nodes
            .set(&mut camera, "AcquisitionFrameRate", "30")
            .unwrap();
        assert_eq!(format(&camera), "mjpeg");
    }
    #[test]
    fn v4l2_modes_prefer_mjpeg_over_h264() {
        // The original C920 (046d:082d) lists YUYV, then H.264, then MJPEG,
        // with both compressed formats at 30 down to 5 fps.
        let (modes, formats) = v4l2_modes(
            concat!(
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Raw       :     yuyv422 :           YUYV 4:2:2 : 640x480 1280x720 1920x1080\n",
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Compressed:        h264 :                H.264 : 640x480 1280x720 1920x1080\n",
                "[video4linux2,v4l2 @ 0x55d0c8a0c0] Compressed:       mjpeg :          Motion-JPEG : 640x480 1280x720 1920x1080\n",
            ),
            |description, size| match (description, size) {
                ("YUYV 4:2:2", (1280, 720)) => Some((5.0, 10.0)),
                ("YUYV 4:2:2", (1920, 1080)) => Some((5.0, 5.0)),
                _ => Some((5.0, 30.0)),
            },
        );
        assert_eq!(formats, ["yuyv422", "mjpeg", "h264"]);
        assert_eq!(
            modes.iter().map(|m| m.input_format).collect::<Vec<_>>(),
            [Some(0), Some(0), Some(0), Some(1), Some(1)]
        );
        let mut camera = native("v4l2:/dev/video0", modes);
        camera.formats = formats;
        camera.configure(NativeOptions::default()).unwrap();
        assert_eq!(
            camera.input_options(),
            [
                "-framerate",
                "30",
                "-video_size",
                "1920x1080",
                "-input_format",
                "mjpeg"
            ]
        );
    }
    /// A stream found to send Motion JPEG is read again as its own JPEGs,
    /// decoded here, each frame keeping its JPEG; JPEGs that do not decode
    /// send it back to `FFmpeg`'s decoder. A webcam in its MJPEG mode is read
    /// that way from the start.
    #[test]
    #[cfg(all(unix, feature = "jpeg"))]
    fn motion_jpeg_sources_are_read_as_their_own_jpegs() {
        let (width, height) = (16usize, 8usize);
        let pixels: Vec<u8> = (0..width * height * 3)
            .map(|i| u8::try_from(i * 3 % 256).unwrap())
            .collect();
        let jpeg = turbojpeg::compress(
            turbojpeg::Image {
                pixels: &*pixels,
                width,
                pitch: width * 3,
                height,
                format: turbojpeg::PixelFormat::RGB,
            },
            90,
            turbojpeg::Subsamp::Sub2x1,
        )
        .unwrap()
        .to_vec();
        let script = |name: &str| {
            Fake::native(
                name,
                r#"echo "$*" >> "$dir/argv"
printf '[info]   Stream #0:0: Video: mjpeg (Baseline), yuvj422p(pc, bt470bg), 16x8, 30 fps\n' >&2
case "$*" in
*"-f mjpeg"*) while :; do cat "$dir/frame.jpg"; sleep 0.02; done ;;
esac
printf '[info] Output #0, rawvideo, to '"'"'pipe:1'"'"':\n[info]   Stream #0:0: Video: rawvideo, yuvj422p(pc, bt470bg), 16x8\n' >&2
while :; do head -c 256 /dev/zero; sleep 0.02; done"#,
            )
        };
        let fake = script("mjpeg");
        fs::write(fake.dir.join("frame.jpg"), &jpeg).unwrap();
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(0).unwrap();
        for _ in 0..3 {
            let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
            assert_eq!(frame.jpeg.as_deref(), Some(&*jpeg));
            assert_eq!(crate::frame::frame_format_name(&frame), "YCbCr422_8_Planar");
        }
        // Decoded by FFmpeg once, to learn what it is, then copied.
        assert_eq!(fake.launches(&["-f rawvideo"]), 1);
        assert_eq!(
            fake.launches(&["-c:v copy -f mjpeg -avioflags direct pipe:1"]),
            1
        );
        drop(camera);
        // The next process to open it knows: its JPEGs from the first launch.
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(0).unwrap();
        let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(frame.jpeg.as_deref(), Some(&*jpeg));
        assert_eq!(fake.launches(&["-f rawvideo"]), 1);
        assert_eq!(fake.launches(&["-f mjpeg"]), 2);
        drop(camera);
        drop(fake);
        // A source that sends something else now is decoded at once, and
        // no longer taken for Motion JPEG.
        let fake = Fake::native(
            "mjpeg-now-h264",
            r#"echo "$*" >> "$dir/argv"
printf '[info]   Stream #0:0: Video: h264 (High), yuv420p(tv, bt709), 16x8, 30 fps\n' >&2
case "$*" in
*"-f mjpeg"*) printf '[error] Could not write header\n' >&2; exit 1 ;;
esac
printf '[info] Output #0, rawvideo, to '"'"'pipe:1'"'"':\n[info]   Stream #0:0: Video: rawvideo, yuv420p(tv, bt709), 16x8\n' >&2
while :; do head -c 192 /dev/zero; sleep 0.02; done"#,
        );
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(0).unwrap();
        let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
        assert!(frame.jpeg.is_none());
        assert_eq!(fake.launches(&["-f mjpeg"]), 1);
        assert_eq!(fake.launches(&["-f rawvideo"]), 1);
        drop(camera);
        assert!(!learned::motion_jpeg("rtsp://127.0.0.1:9/live"));
        drop(fake);
        // JPEGs that do not decode here are FFmpeg's to decode.
        let fake = script("mjpeg-broken");
        let broken = [&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2][..], &[1, 2, 3, 0xFF, 0xD9]].concat();
        fs::write(fake.dir.join("frame.jpg"), broken).unwrap();
        let mut camera = open_url("rtsp://127.0.0.1:9/live", Duration::from_secs(5)).unwrap();
        camera.start(0).unwrap();
        let frame = camera.next_frame(Duration::from_secs(2)).unwrap();
        assert!(frame.jpeg.is_none());
        assert_eq!(fake.launches(&["-f rawvideo"]), 2);
        assert_eq!(fake.launches(&["-f mjpeg"]), 1);
        drop(camera);
        // A webcam mode's format is known before it opens.
        let mode = Mode {
            width: 640,
            height: 480,
            fps: Some((30.0, 30.0)),
            input_format: Some(0),
            intervals: None,
            min_size: None,
        };
        let mut webcam = native("v4l2:/dev/video0", vec![mode]);
        webcam.formats = vec!["mjpeg".into()];
        webcam.configure(NativeOptions::default()).unwrap();
        assert_eq!(webcam.launch_output(), Output::Jpeg);
        webcam.passthrough = false;
        assert_eq!(webcam.launch_output(), Output::Native);
        webcam.formats = vec!["yuyv422".into()];
        webcam.passthrough = true;
        assert_eq!(webcam.launch_output(), Output::Native);
    }
    #[test]
    #[cfg(unix)]
    fn discovery_command_distinguishes_timeout_from_expected_nonzero_exit() {
        let exe = Executable::external("/bin/sh".into());
        let start = Instant::now();
        let (success, _, timed_out) = timed_output(
            &exe,
            &["-c".into(), "exec sleep 5".into()],
            &[],
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(!success);
        assert!(timed_out);
        assert!(start.elapsed() < Duration::from_secs(2));
        let (success, log, timed_out) = timed_output(
            &exe,
            &["-c".into(), "echo devices; exit 1".into()],
            &[],
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(!success);
        assert!(!timed_out);
        assert_eq!(log.trim(), "devices");
        // Killed from outside, as by the out-of-memory killer, it tells
        // nothing; a crash of its own does.
        for (signal, undecided) in [("KILL", true), ("TERM", true), ("SEGV", false)] {
            let (success, _, stopped) = timed_output(
                &exe,
                &["-c".into(), format!("kill -{signal} $$")],
                &[],
                Duration::from_secs(2),
            )
            .unwrap();
            assert!(!success, "{signal}");
            assert_eq!(stopped, undecided, "{signal}");
        }
    }
    #[test]
    fn native_errors_give_platform_specific_recovery() {
        for (source, log, guidance) in [
            (
                "v4l2:/dev/video0",
                "Cannot open video device /dev/video0: Permission denied",
                "owning group",
            ),
            (
                "dshow:video=Webcam",
                "Could not run graph (sometimes caused by a device already in use): 0x80070005",
                "Windows Settings",
            ),
        ] {
            let (kind, message) =
                native_failure(log, false, source, Duration::from_secs(2)).unwrap();
            assert!(message.contains(guidance));
            assert_eq!(kind.code().1, 3);
            let (_, timeout) = native_failure("", true, source, Duration::from_secs(2)).unwrap();
            assert!(!timeout.contains("MacBook"));
            assert!(timeout.contains("selected video mode"));
        }
        let (busy, _) = native_failure(
            "Device or resource busy",
            false,
            "v4l2:/dev/video0",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(busy.code(), ("camera_busy", 3));
        assert!(
            native_failure(
                "unknown decoding error",
                false,
                "v4l2:/dev/video0",
                Duration::from_secs(2)
            )
            .is_none()
        );
        for (node, log) in [
            (
                "/dev/video1",
                concat!(
                    "[video4linux2,v4l2 @ 0x55d0c8a0c0] ioctl(VIDIOC_G_INPUT): Inappropriate ioctl for device\n",
                    "[in#0 @ 0x55d0c8a000] Error opening input: Inappropriate ioctl for device\n",
                    "Error opening input file /dev/video1.\n",
                ),
            ),
            (
                "/dev/video3",
                concat!(
                    "[video4linux2,v4l2 @ 0x7f3a1c4c40] ioctl(VIDIOC_G_INPUT): Not a tty\n",
                    "[in#0 @ 0x7f3a1c4b80] Error opening input: Not a tty\n",
                    "Error opening input file /dev/video3.\n",
                ),
            ),
            (
                "/dev/video10",
                concat!(
                    "[video4linux2,v4l2 @ 0x55d0c8a0c0] Not a video capture device.\n",
                    "[in#0 @ 0x55d0c8a000] Error opening input: No such device\n",
                    "Error opening input file /dev/video10.\n",
                ),
            ),
        ] {
            let source = format!("v4l2:{node}");
            let (kind, message) =
                native_failure(log, false, &source, Duration::from_secs(2)).unwrap();
            assert_eq!(
                message,
                format!(
                    "video capture is not supported by {node}, which may be a metadata, codec or ISP node; run capturefab list to see capture nodes"
                )
            );
            assert_eq!(kind.code().1, 5);
        }
    }
    #[test]
    fn v4l2_discovery_lists_capture_nodes_only() {
        // A Raspberry Pi with a USB webcam: uvcvideo's metadata node shares
        // the camera's name, and the codec, subdevice and VBI nodes sit in
        // the same class.
        let class = temp_dir("video4linux");
        for (node, name) in [
            ("video0", "HD Pro Webcam C920"),
            ("video1", "HD Pro Webcam C920"),
            ("video10", "bcm2835-codec-decode"),
            ("video2", "unicam-image"),
            ("v4l-subdev0", "imx219 10-0010"),
            ("vbi0", "saa7134"),
        ] {
            fs::create_dir(class.join(node)).unwrap();
            fs::write(class.join(node).join("name"), format!("{name}\n")).unwrap();
        }
        let devices = v4l2_devices(&class, |path| {
            !matches!(path.to_str(), Some("/dev/video1" | "/dev/video10"))
        });
        assert_eq!(
            devices,
            [
                json!({"id":"v4l2:/dev/video0","name":"HD Pro Webcam C920","backend":"v4l2"}),
                json!({"id":"v4l2:/dev/video2","name":"unicam-image","backend":"v4l2"}),
            ]
        );
        fs::remove_dir_all(class).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(!captures_by(deadline, "/dev/video1".as_ref(), |_| false));
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        let wedged = |_: &std::path::Path| {
            thread::sleep(Duration::from_secs(5));
            false
        };
        assert!(captures_by(deadline, "/dev/video0".as_ref(), wedged));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn v4l2_locators_accept_persistent_udev_links() {
        let by_path = "/dev/v4l/by-path/pci-0000:00:14.0-usb-0:1:1.0-video-index0";
        for path in [
            "/dev/video0",
            "/dev/v4l/by-id/usb-046d_HD_Pro_Webcam_C920_8A4F3D5F-video-index0",
            by_path,
            "/dev/v4l/by-path/platform-fe801000.csi-video-index0",
        ] {
            assert!(v4l2_path(path), "{path}");
        }
        for path in [
            "/dev/v4l/by-id/",
            "/dev/v4l/by-id/..",
            "/dev/v4l/by-path/../../sda",
            "/dev/v4l-subdev0",
            "/dev/sda",
        ] {
            assert!(!v4l2_path(path), "{path}");
        }
        let source = format!("v4l2:{by_path}?size=1280x720");
        validate_source_shape(&source).unwrap();
        let (base, options) = native_source(&source).unwrap();
        assert_eq!(base, format!("v4l2:{by_path}"));
        assert_eq!(options.unwrap().size, Some((1280, 720)));
        let missing = native_failure(
            &format!("[video4linux2,v4l2 @ 0x55d0c8a0c0] Cannot open video device {by_path}: No such file or directory\n"),
            false,
            &base,
            Duration::from_secs(2),
        )
        .unwrap()
        .1;
        assert_eq!(
            missing,
            format!(
                "camera device not found: {by_path}; reconnect the device and run capturefab list to obtain its current locator"
            )
        );
    }
    #[test]
    fn dshow_devices_keep_video_sources_and_tell_identical_cameras_apart() {
        let devices = dshow_devices(concat!(
            "[dshow @ 000001eb780baf00] \"HD Pro Webcam C920\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_046d&pid_082d&mi_00#7&1d6d0b0e&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"HD Pro Webcam C920\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"USB Capture HDMI\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_2935&pid_0006&mi_00#8&3027539&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"Game Capture HD60 S\" (audio, video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_pnp_\\\\?\\usb#vid_0fd9&pid_004e&mi_00#7&2bc4c6a&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\\global\"\n",
            "[dshow @ 000001eb780baf00] \"OBS Virtual Camera\" (none)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}\"\n",
            "[dshow @ 000001eb780baf00] \"Studio: Left\" (video)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\\{5C2CD55C-92AD-4999-8666-912BD3E70010}\"\n",
            "[dshow @ 000001eb780baf00] \"VoiceMeeter Aux Output (VB-Audio VoiceMeeter AUX VAIO)\" (audio)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\\wave_{024F58BD-C3C7-401F-B905-455306BF3F33}\"\n",
            "[dshow @ 000001eb780baf00] \"Line In (Realtek Audio)\" (none)\n",
            "[dshow @ 000001eb780baf00]   Alternative name \"@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\\wave_{0881BEEB-1C3E-4C22-997E-C1A757D757DF}\"\n",
            "dummy: Immediate exit requested\n",
        ));
        let ids = devices
            .iter()
            .map(|device| device["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                r"dshow:video=@device_pnp_\\%3F\usb#vid_046d&pid_082d&mi_00#7&1d6d0b0e&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
                r"dshow:video=@device_pnp_\\%3F\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global",
                "dshow:video=USB Capture HDMI",
                "dshow:video=Game Capture HD60 S",
                "dshow:video=OBS Virtual Camera",
                r"dshow:video=@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\{5C2CD55C-92AD-4999-8666-912BD3E70010}",
            ]
        );
        assert_eq!(devices[1]["name"], "HD Pro Webcam C920");
        assert_eq!(devices[5]["name"], "Studio: Left");
        let (base, options) = native_source(&format!("{}?size=1280x720&fps=30", ids[1])).unwrap();
        assert_eq!(base, ids[1]);
        assert_eq!(options.unwrap().size, Some((1280, 720)));
        assert_eq!(
            dshow_input(base.strip_prefix("dshow:").unwrap()),
            r"video=@device_pnp_\\?\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global"
        );
        assert_eq!(
            dshow_input("video=HD Pro Webcam C920"),
            "video=HD Pro Webcam C920"
        );
        let pasted = native_source(r"dshow:video=@device_pnp_\\?\usb#vid_046d&pid_082d&mi_00#7&3a1e7f3&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global").unwrap_err();
        assert!(format!("{pasted}").contains("%3F"), "{pasted}");
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
        let failure = |log| native_failure(log, false, "avfoundation:FaceTime HD Camera", timeout);
        assert_eq!(
            failure("[in#0 @ 0x1] Failed to create AV capture input device: Cannot use FaceTime HD Camera").unwrap().1,
            "camera access denied: allow this app in System Settings > Privacy & Security > Camera"
        );
        for (source, log) in [
            (
                "avfoundation:FaceTime HD Camera",
                "Cannot use FaceTime HD Camera",
            ),
            (
                "dshow:video=Integrated Webcam",
                "Error opening input: access denied",
            ),
            ("v4l2:/dev/video0", "Permission denied"),
        ] {
            let (kind, denied) = native_failure(log, false, source, timeout).unwrap();
            assert!(denied.starts_with(CAMERA_ACCESS_DENIED), "{denied}");
            assert_eq!(kind.code(), ("permission_denied", 3));
        }
        let (kind, blocked) = native_failure(
            "[dshow @ 000001b5e2a6c3c0] Could not run graph (sometimes caused by a device already in use by other application)\n[in#0 @ 000001b5e2a6c000] Error opening input: I/O error\n",
            false,
            "dshow:video=Integrated Webcam",
            timeout,
        )
        .unwrap();
        assert!(blocked.contains(CAMERA_ACCESS_DENIED), "{blocked}");
        assert!(blocked.contains("in use by another application"));
        assert!(blocked.contains("Privacy & security > Camera"));
        assert_eq!(kind.code().1, 3);
        assert_eq!(
            failure("[in#0 @ 0x1] Failed to create AV capture input device: Cannot Use FaceTime HD Camera").unwrap().1,
            "camera is in use by another application"
        );
        let (kind, missing) = failure("[in#0 @ 0x1] Video device not found").unwrap();
        assert!(missing.contains("run capturefab list"));
        assert_eq!(kind.code(), ("camera_not_found", 3));
        let (kind, stalled) =
            native_failure("", true, "avfoundation:FaceTime HD Camera", timeout).unwrap();
        assert_eq!(
            stalled,
            "no frames from FaceTime HD Camera (timed out after 5 s): the camera may be suspended (for example a closed MacBook lid), in use, or blocked by permissions"
        );
        assert_eq!(kind.code().1, 4);
    }
    #[test]
    fn indexed_progress_requires_the_final_muxed_frame_count() {
        let log = Log::new();
        log.keep(b"frame=2\nprogress=continue\n");
        assert_eq!(log.completed_video_frames(), None);
        log.keep(b"frame=5\nprogress=end\n");
        assert_eq!(log.completed_video_frames(), Some(5));
        let malformed = Log::new();
        malformed.keep(b"frame=5\nframe=invalid\nprogress=end\n");
        assert_eq!(malformed.completed_video_frames(), None);
    }

    fn chunked_frame() -> Frame {
        with_test_chunks(Frame {
            pixel_encoding: None,
            id: 23,
            width: 2,
            height: 2,
            pixel_format: crate::types::MONO8,
            data: vec![7; 4].into(),
            received_monotonic_ns: Some(123),
            ..Frame::default()
        })
    }
    fn with_test_chunks(mut frame: Frame) -> Frame {
        let image_bytes = frame.data.len();
        let mut payload = frame.data.to_vec();
        payload.extend(123u64.to_be_bytes());
        payload.extend(0xffff_1234u32.to_be_bytes());
        payload.extend(8u32.to_be_bytes());
        let mut chunks = crate::transport::chunks::ChunkData::parse(
            &payload,
            image_bytes,
            image_bytes,
            crate::transport::chunks::ByteOrder::Big,
            67,
        )
        .unwrap();
        chunks.image_layout = Some(crate::transport::chunks::ImageLayout {
            pixel_encoding: None,
            offset: 0,
            pfnc_code: Some(frame.pixel_format),
            width: frame.width,
            height: frame.height,
            row_bytes: crate::frame::frame_row_bytes(&frame),
            row_stride: crate::frame::frame_row_bytes(&frame),
            padding_y: 0,
            byte_order: Some(crate::transport::chunks::ByteOrder::Little),
            continuous_packing: false,
        });
        chunks.payload = payload.into();
        frame.chunks = Some(chunks.into());
        frame
    }

    #[test]
    fn chunked_writer_drains_once_and_finalizes_payload_before_reporting_it() {
        let directory = temp_dir("chunked-writer");
        let video = directory.join("video.mkv");
        let queue = WriterQueue::new();
        queue.state.lock().unwrap().latest = Some(chunked_frame());
        queue.chunks_seen.store(true, Ordering::Release);
        queue.running.store(false, Ordering::Release);
        let mut encoded = Vec::new();
        let taken = write_frames(
            &queue,
            (&mut encoded, None),
            (RawInput::for_format(crate::types::MONO8).unwrap(), None),
            Duration::from_millis(1),
            Repeat::Always,
            None,
            Some(timeline::ChunkWriter::new(video, None)),
        );
        assert_eq!(taken, 1);
        assert_eq!(encoded, vec![7; 4]);
        assert_eq!(queue.written.load(Ordering::Relaxed), 1);
        assert!(queue.typed_error().is_none());
        let summary = queue.state.lock().unwrap().chunks.clone().unwrap();
        assert_eq!(summary["chunk_frames"], 1);
        assert_eq!(summary["complete"], true);
        assert_eq!(
            fs::read(summary["payload_path"].as_str().unwrap()).unwrap(),
            chunked_frame().chunks.unwrap().payload.to_vec()
        );
        let record: Value =
            serde_json::from_str(&fs::read_to_string(summary["path"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(record["frame_index"], 0);
        assert_eq!(record["frame_id"], 23);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn chunked_writer_rejects_destinations_that_cannot_retain_chunks() {
        let queue = WriterQueue::new();
        queue.state.lock().unwrap().latest = Some(chunked_frame());
        queue.running.store(false, Ordering::Release);
        queue.chunks_seen.store(true, Ordering::Release);
        let mut encoded = Vec::new();
        write_frames(
            &queue,
            (&mut encoded, None),
            (RawInput::for_format(crate::types::MONO8).unwrap(), None),
            Duration::from_millis(1),
            Repeat::Never,
            None,
            None,
        );
        assert_eq!(encoded, [] as [u8; 0]);
        assert_eq!(
            crate::types::error_transport_code(&queue.typed_error().unwrap()),
            Some("unsupported")
        );
        let mut forward = Forwarder {
            config: ForwardConfig {
                output: "udp://127.0.0.1:9".into(),
                codec: "h264".into(),
                encoder: "auto".into(),
                fps: 30.0,
                bitrate: "1M".into(),
                max_file_bytes: None,
                min_free: None,
                timeline: None,
            },
            process: None,
            final_stats: ForwardStats::default(),
            reserved: None,
            failed: None,
            copy: None,
            waits: false,
            timeline: None,
        };
        let error = forward.push(chunked_frame()).unwrap_err();
        assert_eq!(
            crate::types::error_transport_code(&error),
            Some("unsupported")
        );
        assert!(forward.process.is_none());
    }

    #[test]
    fn chunked_writer_retains_companion_errors_without_replacing_existing_files() {
        let directory = temp_dir("chunked-collision");
        let video = directory.join("video.mkv");
        let index = directory.join("video.mkv.chunks.jsonl");
        fs::write(&index, b"keep").unwrap();
        let queue = WriterQueue::new();
        queue.state.lock().unwrap().latest = Some(chunked_frame());
        queue.chunks_seen.store(true, Ordering::Release);
        queue.running.store(false, Ordering::Release);
        let mut encoded = Vec::new();
        write_frames(
            &queue,
            (&mut encoded, None),
            (RawInput::for_format(crate::types::MONO8).unwrap(), None),
            Duration::from_millis(1),
            Repeat::Never,
            None,
            Some(timeline::ChunkWriter::new(video, None)),
        );
        assert_eq!(encoded, vec![7; 4]);
        assert!(
            queue
                .typed_error()
                .unwrap()
                .to_string()
                .contains("video chunks")
        );
        assert_eq!(fs::read(&index).unwrap(), b"keep");
        assert!(!directory.join("video.mkv.chunks.raw").exists());
        assert!(queue.state.lock().unwrap().chunks.is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn indexed_writer_drains_only_the_latest_pending_frame_after_stop() {
        let directory = temp_dir("indexed-pending");
        let index = directory.join("frames.jsonl");
        let config = timeline::Config {
            path: index.display().to_string(),
            acquisition_end_ns: 1000,
        };
        let writer = timeline::Writer::create(&config, 10.0, None).unwrap();
        let frame = |id: u64, pixel: u8| Frame {
            pixel_encoding: None,
            id,
            width: 2,
            height: 2,
            pixel_format: crate::types::MONO8,
            data: vec![pixel; 4].into(),
            received_monotonic_ns: Some(id * 100),
            ..Frame::default()
        };
        let queue = WriterQueue::new();
        queue.state.lock().unwrap().latest = Some(frame(1, 11));
        queue.state.lock().unwrap().latest.replace(frame(2, 22));
        queue.running.store(false, Ordering::Release);
        let mut encoded = Vec::new();
        let taken = write_frames(
            &queue,
            (&mut encoded, None),
            (RawInput::for_format(crate::types::MONO8).unwrap(), None),
            Duration::from_secs(1),
            Repeat::Never,
            Some(writer),
            None,
        );
        assert_eq!(taken, 1);
        assert_eq!(queue.written.load(Ordering::Relaxed), 1);
        assert_eq!(encoded, [22; 4]);
        let records = fs::read_to_string(&index)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<timeline::FrameRecord>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].frame_index, 0);
        assert_eq!(records[0].frame_id, 2);
        assert_eq!(records[0].received_monotonic_ns, 200);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn indexed_writer_never_records_a_failed_encoder_input_write() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "fixture encoder closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let directory = temp_dir("indexed-broken");
        let index = directory.join("frames.jsonl");
        let writer = timeline::Writer::create(
            &timeline::Config {
                path: index.display().to_string(),
                acquisition_end_ns: 1000,
            },
            30.0,
            None,
        )
        .unwrap();
        let queue = WriterQueue::new();
        queue.state.lock().unwrap().latest = Some(Frame {
            pixel_encoding: None,
            id: 1,
            width: 2,
            height: 2,
            pixel_format: crate::types::MONO8,
            data: vec![7; 4].into(),
            received_monotonic_ns: Some(100),
            chunks: chunked_frame().chunks,
            ..Frame::default()
        });
        queue.running.store(false, Ordering::Release);
        write_frames(
            &queue,
            (&mut Broken, None),
            (RawInput::for_format(crate::types::MONO8).unwrap(), None),
            Duration::from_millis(1),
            Repeat::Never,
            Some(writer),
            Some(timeline::ChunkWriter::new(
                directory.join("video.mkv"),
                None,
            )),
        );
        assert_eq!(queue.written.load(Ordering::Relaxed), 0);
        assert_eq!(fs::read(&index).unwrap(), [] as [u8; 0]);
        assert!(!directory.join("video.mkv.chunks.raw").exists());
        assert!(!directory.join("video.mkv.chunks.jsonl").exists());
        assert!(
            queue
                .typed_error()
                .unwrap()
                .to_string()
                .contains("fixture encoder closed")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn indexed_recording_fails_when_the_encoder_reports_missing_output_frames() {
        let fake = Fake::new(
            "indexed-drop",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) head -c 8192 /dev/zero; cat >/dev/null; printf 'frame=0\nprogress=end\n' >&2 ;;
esac"#,
        );
        let video = fake.dir.join("video.ts");
        let index = fake.dir.join("video.frames.jsonl");
        let mut config = recording(&video, "libx264");
        config.timeline = Some(timeline::Config {
            path: index.display().to_string(),
            acquisition_end_ns: u64::MAX,
        });
        let mut forward = Forwarder::new(config).unwrap();
        let mut frame = tiny(16, 16);
        frame.received_monotonic_ns = Some(100);
        forward.push(frame).unwrap();
        let error = forward.stop().unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::Incomplete,
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("frame count differs"));
        assert_eq!(forward.stats().written, 1);
        assert_eq!(fs::read_to_string(&index).unwrap().lines().count(), 1);
        assert!(
            video.exists(),
            "a failed indexed recording preserves its partial video"
        );
    }

    #[test]
    #[cfg(unix)]
    fn indexed_size_limits_never_erase_finalization_errors() {
        let fake = Fake::new(
            "indexed-size",
            r#"case "$*" in
*-encoders*) printf ' V....D libx264 x264 (codec h264)\n' ;;
*"-f null"*) ;;
*) head -c 8192 /dev/zero; cat >/dev/null; printf 'frame=0\nprogress=end\n' >&2 ;;
esac"#,
        );
        let mut config = recording(&fake.dir.join("video.ts"), "libx264");
        config.max_file_bytes = Some(5000);
        config.timeline = Some(timeline::Config {
            path: fake.dir.join("index.jsonl").display().to_string(),
            acquisition_end_ns: u64::MAX,
        });
        let mut forward = Forwarder::new(config).unwrap();
        let mut frame = tiny(16, 16);
        frame.received_monotonic_ns = Some(100);
        drop(forward.push(frame));
        let error = forward.stop().unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            ErrorKind::Incomplete,
            "{error:#}"
        );
        assert!(forward.stats().recording.as_ref().unwrap().truncated);
    }

    #[test]
    #[ignore = "requires FFmpeg and ffprobe"]
    fn chunked_recording_retains_measurements_alongside_decodable_video() {
        let directory = temp_dir("chunked-real");
        let video = directory.join("video.mkv");
        let config = ForwardConfig {
            output: video.display().to_string(),
            codec: "ffv1".into(),
            encoder: "ffv1".into(),
            fps: 10.0,
            bitrate: "1M".into(),
            max_file_bytes: None,
            min_free: None,
            timeline: None,
        };
        let mut forward = Forwarder::new(config).unwrap();
        forward.wait_for_writer();
        for id in 1..=3u8 {
            forward
                .push(with_test_chunks(Frame {
                    pixel_encoding: None,
                    id: u64::from(id),
                    width: 16,
                    height: 16,
                    pixel_format: crate::types::MONO8,
                    data: vec![id; 16 * 16].into(),
                    received_monotonic_ns: Some(u64::from(id) * 100),
                    ..Frame::default()
                }))
                .unwrap();
        }
        forward.stop().unwrap();
        let summary = forward.stats().recording.unwrap().chunks.unwrap();
        assert_eq!(summary["chunk_frames"], 3);
        assert_eq!(summary["video_frames_verified"], true);
        let records = fs::read_to_string(summary["path"].as_str().unwrap())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        let payload = fs::read(summary["payload_path"].as_str().unwrap()).unwrap();
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record["frame_index"], index);
            let offset = usize::try_from(record["payload_offset"].as_u64().unwrap()).unwrap();
            assert_eq!(&payload[offset + 256..offset + 264], &123u64.to_be_bytes());
        }
        let decoded = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&video)
            .args([
                "-fps_mode",
                "passthrough",
                "-pix_fmt",
                "gray",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let expected = (1..=3u8)
            .flat_map(|value| std::iter::repeat_n(value, 16 * 16))
            .collect::<Vec<_>>();
        assert_eq!(decoded.stdout, expected);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires FFmpeg and ffprobe"]
    fn indexed_recording_matches_decoded_frames_and_presentation_times() {
        let directory = temp_dir("indexed-real");
        let video = directory.join("video.mkv");
        let index = directory.join("frames.jsonl");
        let mut config = ForwardConfig {
            output: video.display().to_string(),
            codec: "ffv1".into(),
            encoder: "ffv1".into(),
            fps: 10.0,
            bitrate: "1M".into(),
            max_file_bytes: None,
            min_free: None,
            timeline: Some(timeline::Config {
                path: index.display().to_string(),
                acquisition_end_ns: u64::MAX,
            }),
        };
        config.validate().unwrap();
        let mut forward = Forwarder::new(config.clone()).unwrap();
        forward.wait_for_writer();
        for id in 1..=5u8 {
            forward
                .push(Frame {
                    pixel_encoding: None,
                    id: u64::from(id),
                    width: 16,
                    height: 16,
                    pixel_format: crate::types::MONO8,
                    data: vec![id; 16 * 16].into(),
                    received_monotonic_ns: Some(u64::from(id) * 100),
                    device_ts_ns: Some(u64::from(id) * 1000),
                    ..Frame::default()
                })
                .unwrap();
        }
        forward.stop().unwrap();
        let records = fs::read_to_string(&index)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<timeline::FrameRecord>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 5);
        assert_eq!(forward.stats().written, 5);
        for (position, record) in records.iter().enumerate() {
            assert_eq!(record.frame_index, u64::try_from(position).unwrap());
            assert_eq!(record.frame_id, u64::try_from(position + 1).unwrap());
            assert_eq!(
                record.video_time_ns,
                u64::try_from(position).unwrap() * 100_000_000
            );
        }
        let exe = executable().unwrap();
        let decoded = command(&exe)
            .args(["-v", "error", "-i"])
            .arg(&video)
            .args([
                "-fps_mode",
                "passthrough",
                "-pix_fmt",
                "gray",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let expected = (1..=5u8)
            .flat_map(|value| std::iter::repeat_n(value, 16 * 16))
            .collect::<Vec<_>>();
        assert_eq!(decoded.stdout, expected);
        let probed = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=best_effort_timestamp_time",
                "-of",
                "json",
            ])
            .arg(&video)
            .output()
            .unwrap();
        assert!(
            probed.status.success(),
            "{}",
            String::from_utf8_lossy(&probed.stderr)
        );
        let document: Value = serde_json::from_slice(&probed.stdout).unwrap();
        let frames = document["frames"].as_array().unwrap();
        assert_eq!(frames.len(), records.len());
        for (frame, record) in frames.iter().zip(&records) {
            let seconds = frame["best_effort_timestamp_time"]
                .as_str()
                .unwrap()
                .parse::<f64>()
                .unwrap();
            let actual = Duration::from_secs_f64(seconds);
            assert!(
                actual.abs_diff(Duration::from_nanos(record.video_time_ns))
                    <= Duration::from_millis(1)
            );
        }
        // H.264/MP4 must preserve the same presentation correspondence,
        // independently of its lossy pixel format conversion and muxer.
        for extension in ["mp4", "mov", "mkv", "ts"] {
            let h264_video = directory.join(format!("h264-video.{extension}"));
            let h264_index = directory.join(format!("h264-{extension}.frames.jsonl"));
            let mut h264_config = config.clone();
            h264_config.output = h264_video.display().to_string();
            h264_config.codec = "h264".into();
            h264_config.encoder = "libx264".into();
            h264_config.timeline = Some(timeline::Config {
                path: h264_index.display().to_string(),
                acquisition_end_ns: u64::MAX,
            });
            let mut h264 = Forwarder::new(h264_config).unwrap();
            h264.wait_for_writer();
            for id in 1..=5u8 {
                h264.push(Frame {
                    pixel_encoding: None,
                    id: u64::from(id),
                    width: 16,
                    height: 16,
                    pixel_format: crate::types::MONO8,
                    data: vec![id * 40; 16 * 16].into(),
                    received_monotonic_ns: Some(u64::from(id) * 100),
                    ..Frame::default()
                })
                .unwrap();
            }
            h264.stop().unwrap();
            let h264_records = fs::read_to_string(&h264_index)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<timeline::FrameRecord>(line).unwrap())
                .collect::<Vec<_>>();
            let decoded = command(&exe)
                .args(["-v", "error", "-i"])
                .arg(&h264_video)
                .args([
                    "-fps_mode",
                    "passthrough",
                    "-pix_fmt",
                    "gray",
                    "-f",
                    "rawvideo",
                    "-",
                ])
                .output()
                .unwrap();
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            assert_eq!(decoded.stdout.len(), 5 * 16 * 16);
            assert_eq!(h264_records.len(), 5);
            for (position, pixels) in decoded
                .stdout
                .as_chunks::<{ 16 * 16 }>()
                .0
                .iter()
                .enumerate()
            {
                let mean = pixels.iter().map(|&pixel| u64::from(pixel)).sum::<u64>() / (16 * 16);
                let expected = u64::try_from(position + 1).unwrap() * 40;
                assert!(
                    mean.abs_diff(expected) <= 3,
                    "frame {position}: mean {mean}, expected {expected}"
                );
            }
            let probed = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "frame=best_effort_timestamp_time",
                    "-of",
                    "json",
                ])
                .arg(&h264_video)
                .output()
                .unwrap();
            assert!(
                probed.status.success(),
                "{}",
                String::from_utf8_lossy(&probed.stderr)
            );
            let document: Value = serde_json::from_slice(&probed.stdout).unwrap();
            let frames = document["frames"].as_array().unwrap();
            assert_eq!(frames.len(), h264_records.len());
            for (frame, record) in frames.iter().zip(&h264_records) {
                let actual = Duration::from_secs_f64(
                    frame["best_effort_timestamp_time"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                );
                assert!(
                    actual.abs_diff(Duration::from_nanos(record.video_time_ns))
                        <= Duration::from_millis(1)
                );
            }
        }
        // The deadline excludes future host frames before an encoder/index opens.
        config.output = directory.join("late.mkv").display().to_string();
        config.timeline = Some(timeline::Config {
            path: directory.join("late.jsonl").display().to_string(),
            acquisition_end_ns: 100,
        });
        let mut late = Forwarder::new(config).unwrap();
        assert!(
            !late
                .push(Frame {
                    received_monotonic_ns: Some(101),
                    ..Frame::default()
                })
                .unwrap()
        );
        late.stop().unwrap();
        assert!(!directory.join("late.mkv").exists());
        assert_eq!(
            fs::read(directory.join("late.jsonl")).unwrap(),
            [] as [u8; 0]
        );
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn ids_grouped_video_reuses_exact_unpacked_intensity_and_retains_source_declaration() {
        use crate::types::IdsPixelFormat;
        for (format, bytes, samples) in [
            (
                IdsPixelFormat::Mono10g40,
                vec![0, 85, 170, 255, 249, 255, 170, 85, 0, 6],
                vec![1_u16, 342, 683, 1023, 1022, 681, 340, 0],
            ),
            (
                IdsPixelFormat::Mono12g24,
                vec![
                    0x12, 0xab, 0xc3, 0x45, 0xde, 0xf6, 0, 0xff, 0xf0, 0x78, 0x13, 0x59,
                ],
                vec![0x123_u16, 0xabc, 0x456, 0xdef, 0, 0xfff, 0x789, 0x135],
            ),
        ] {
            let frame = Frame {
                width: 4,
                height: 2,
                pixel_format: format.code(),
                pixel_encoding: Some(format.encoding(None)),
                data: bytes.clone().into(),
                ..Frame::default()
            };
            let raw = RawInput::for_frame(&frame).unwrap();
            let lossless = raw.for_encoder("ffv1");
            lossless.check(&frame).unwrap();
            let mut scratch = Vec::new();
            lossless.prepare(&frame, &mut scratch).unwrap();
            assert_eq!(
                scratch,
                samples
                    .iter()
                    .flat_map(|sample| sample.to_le_bytes())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                lossless.input_pix_fmt("ffv1"),
                if format.bits() == 10 {
                    "gray10le"
                } else {
                    "gray12le"
                }
            );
            assert_eq!(lossless.pixel_encoding, frame.pixel_encoding);
            let eight = raw.for_encoder("libx264");
            eight.check(&frame).unwrap();
            eight.prepare(&frame, &mut scratch).unwrap();
            assert_eq!(
                scratch,
                samples
                    .iter()
                    .map(|sample| u8::try_from(sample >> (format.bits() - 8)).unwrap())
                    .collect::<Vec<_>>()
            );
            assert_eq!(eight.input_pix_fmt("libx264"), "gray");
            assert_eq!(&*frame.data, &bytes);
            let bare = Frame {
                pixel_encoding: None,
                ..frame.clone()
            };
            assert!(
                RawInput::for_frame(&bare)
                    .unwrap_err()
                    .to_string()
                    .contains("unsupported")
            );
            assert!(
                lossless
                    .check(&bare)
                    .unwrap_err()
                    .to_string()
                    .contains("changed")
            );
            let job = crate::scheduling::JobFrame {
                width: 4,
                height: 2,
                pixel_format: format.code(),
                pixel_encoding: frame.pixel_encoding,
            };
            let restored: crate::scheduling::JobFrame =
                serde_json::from_value(serde_json::to_value(job).unwrap()).unwrap();
            assert_eq!(restored, job);
        }
    }
    #[test]
    fn ids_bayer_video_keeps_exact_mosaic_for_ffv1_and_color_for_lossy_encoders() {
        use crate::types::IdsPixelFormat;
        for (format, input_format) in [
            (IdsPixelFormat::BayerRg10g40, "bayer_rggb8"),
            (IdsPixelFormat::BayerRg12g24, "bayer_rggb8"),
            (IdsPixelFormat::BayerGr10g40, "bayer_grbg8"),
            (IdsPixelFormat::BayerGr12g24, "bayer_grbg8"),
            (IdsPixelFormat::BayerGb10g40, "bayer_gbrg8"),
            (IdsPixelFormat::BayerGb12g24, "bayer_gbrg8"),
            (IdsPixelFormat::BayerBg10g40, "bayer_bggr8"),
            (IdsPixelFormat::BayerBg12g24, "bayer_bggr8"),
        ] {
            let (frame, samples) = crate::frame::ids_bayer_tests::image(format);
            let raw = RawInput::for_frame(&frame).unwrap();
            assert!(raw.color);
            let lossless = raw.for_encoder("ffv1");
            lossless.check(&frame).unwrap();
            assert_eq!(lossless.input_pix_fmt("ffv1"), "gray16le");
            assert_eq!(lossless.pixel_encoding, frame.pixel_encoding);
            let mut scratch = Vec::new();
            lossless.prepare(&frame, &mut scratch).unwrap();
            assert_eq!(
                scratch,
                samples
                    .iter()
                    .flat_map(|sample| (sample << (16 - format.bits())).to_le_bytes())
                    .collect::<Vec<_>>()
            );
            for encoder in ["libx264", "h264_vaapi", "h264_videotoolbox"] {
                let lossy = raw.for_encoder(encoder);
                lossy.check(&frame).unwrap();
                assert_eq!(lossy.input_pix_fmt(encoder), input_format);
                assert!(lossy.color);
                lossy.prepare(&frame, &mut scratch).unwrap();
                assert_eq!(
                    scratch,
                    samples
                        .iter()
                        .map(|sample| u8::try_from(sample >> (format.bits() - 8)).unwrap())
                        .collect::<Vec<_>>()
                );
            }
            let standard = crate::frame::unpacked(&frame).unwrap();
            let (mut source, mut converted) = (Vec::new(), Vec::new());
            crate::frame::decode_into(&frame, crate::frame::Channels::Rgb, &mut source).unwrap();
            crate::frame::decode_into(&standard, crate::frame::Channels::Rgb, &mut converted)
                .unwrap();
            assert_eq!(source, converted);
        }
    }
}
