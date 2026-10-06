//! JPEG accelerators in isolated helper processes.
//!
//! A helper (`capturefab __nvjpeg`, `capturefab __vajpeg`) loads the vendor's
//! libraries at run time, so builds need no SDK and hosts without the
//! hardware simply keep libjpeg-turbo. A driver fault, or a hang inside the
//! driver (observed on a Jetson whose GPU another process was saturating,
//! where the stuck process survived SIGKILL), cannot freeze or crash the
//! camera worker: requests have a deadline, after which the helper is
//! abandoned and the process keeps using the CPU encoder.
//!
//! Driver start-up costs far more than one CPU encode, so a helper starts in
//! the background on first use; frames are encoded on the CPU until it
//! reports ready. The parent converts each frame directly into a private
//! shared-memory file that the helper maps, and the helper writes the JPEG
//! after the pixels; the pipes carry one JSON line each way.
use crate::jpeg::{monochrome, well_formed};
use crate::types::Frame;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

/// One kind of accelerator and its (single, lazily started) helper process.
pub struct Kind {
    /// Shown in diagnostics, for example "nvJPEG".
    pub name: &'static str,
    /// The hidden first argument that runs the helper.
    pub argument: &'static str,
    /// Disables this accelerator when set to `0`.
    pub variable: &'static str,
    /// Whether this platform can have the hardware at all.
    pub platform: bool,
    /// Extra environment for the helper process.
    pub environment: &'static [(&'static str, &'static str)],
    pub(crate) accelerator: OnceLock<Accelerator>,
}

/// One image to encode. Its `width * height * channels` pixels sit at offset 0
/// of the shared buffer; the helper writes the JPEG directly after them.
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub width: u32,
    pub height: u32,
    /// 1 for gray, 3 for interleaved RGB.
    pub channels: usize,
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
    /// The helper cannot encode this request but remains usable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unsupported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

/// An encoder error for a request the hardware cannot take, such as a size
/// outside its range: the parent encodes that frame on the CPU and keeps
/// the helper.
#[derive(Debug)]
pub struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unsupported {}

const MAX_LINE: u64 = 4096;
pub const MAX_PIXELS: usize = 64 * 1024 * 1024;
const STARTUP_DEADLINE: Duration = Duration::from_secs(30);

fn read_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    reader.take(MAX_LINE + 1).read_until(b'\n', &mut line)?;
    if line.is_empty() {
        return Ok(None);
    }
    ensure!(
        line.len() as u64 <= MAX_LINE && line.ends_with(b"\n"),
        "invalid JPEG helper message"
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
/// A helper executable and the state of its helper process.
pub(crate) struct Accelerator {
    kind: &'static Kind,
    executable: std::path::PathBuf,
    /// Minimum per-request deadline; healthy hardware needs milliseconds.
    deadline: Duration,
    state: Mutex<State>,
}
fn helper_executable() -> std::path::PathBuf {
    std::env::var_os("CAPTUREFAB_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| "capturefab".into())
}

/// Pixels in, JPEG out: a private mapped file shared with the helper so
/// frames never cross the pipe.
struct Shared {
    path: std::path::PathBuf,
    map: memmap2::MmapMut,
    /// The helper has mapped it, so the file name is no longer needed.
    unlinked: bool,
}
impl Shared {
    fn create(prefix: &str, capacity: usize) -> Result<Self> {
        let dir = crate::ipc::session_dir();
        crate::ipc::ensure_private_dir(&dir)?;
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow!("random buffer name: {e}"))?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{prefix}-{name}.shm"));
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
    jobs: mpsc::Sender<Request>,
    results: mpsc::Receiver<Result<Reply, String>>,
    /// Owned by the caller, which converts each frame straight into it and
    /// reads the JPEG back, so pixels are never staged in another buffer.
    shared: Option<Shared>,
    /// What the helper reported when it became ready.
    version: String,
}
impl Helper {
    fn start(kind: &Kind, executable: &std::path::Path) -> Result<Self> {
        let mut child = Command::new(executable)
            .arg(kind.argument)
            .envs(kind.environment.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("launch {} helper", kind.name))?;
        let mut input = child.stdin.take().context("helper stdin")?;
        let mut output = BufReader::new(child.stdout.take().context("helper stdout")?);
        let ready = (|| -> Result<String> {
            let line = read_line(&mut output)?.ok_or_else(|| anyhow!("helper exited"))?;
            let reply: Reply = serde_json::from_slice(&line)?;
            if !reply.ok {
                bail!("{}", reply.error.unwrap_or_else(|| "unavailable".into()));
            }
            Ok(reply.version.unwrap_or_default())
        })();
        let version = match ready {
            Ok(version) => version,
            Err(error) => {
                abandon(child);
                return Err(error);
            }
        };
        let (jobs, job_queue) = mpsc::channel::<Request>();
        let (result_sender, results) = mpsc::channel();
        // The I/O thread owns the pipes so a stalled helper blocks only it.
        thread::Builder::new()
            .name("capturefab-jpeg-helper".into())
            .spawn(move || {
                for request in job_queue {
                    let result = (|| -> Result<Reply> {
                        write_message(&mut input, &request)?;
                        let line =
                            read_line(&mut output)?.ok_or_else(|| anyhow!("JPEG helper exited"))?;
                        Ok(serde_json::from_slice(&line)?)
                    })();
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
            shared: None,
            version,
        })
    }
}
/// Stop a helper without waiting on the caller's thread: a process stuck in
/// a GPU driver may not exit even after SIGKILL.
fn abandon(mut child: Child) {
    let _ = child.kill();
    let _ = thread::Builder::new()
        .name("capturefab-jpeg-reap".into())
        .spawn(move || child.wait());
}
impl Kind {
    fn wanted(&self) -> bool {
        self.platform && std::env::var_os(self.variable).is_none_or(|value| value != "0")
    }
    fn unavailable(&self) -> String {
        format!(
            "not available on this platform or disabled by {}=0",
            self.variable
        )
    }
}

/// Encode on this accelerator when its helper is ready; `None` means use the
/// CPU.
pub fn encode(kind: &'static Kind, frame: &Frame) -> Option<Vec<u8>> {
    if !kind.wanted() {
        return None;
    }
    kind.accelerator
        .get_or_init(|| Accelerator::new(kind, helper_executable(), Duration::from_secs(2)))
        .encode(frame)
}

/// Start a helper and wait for it, for `doctor`: what it reported when
/// ready, or why the accelerator is unavailable.
pub fn probe(kind: &'static Kind) -> Result<String> {
    if !kind.wanted() {
        bail!("{}", kind.unavailable());
    }
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(Helper::start(kind, &helper_executable()).map(|helper| {
            abandon(helper.child);
            helper.version
        }));
    });
    match receiver.recv_timeout(STARTUP_DEADLINE) {
        Ok(Ok(version)) if version.is_empty() => Ok("ready".into()),
        Ok(Ok(version)) => Ok(version),
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("helper did not start within {STARTUP_DEADLINE:?}"),
    }
}

impl Accelerator {
    fn new(kind: &'static Kind, executable: std::path::PathBuf, deadline: Duration) -> Self {
        Self {
            kind,
            executable,
            deadline,
            state: Mutex::new(State::Idle),
        }
    }
    fn disable(&self, state: &mut State, reason: &str) {
        eprintln!(
            "capturefab: {} disabled ({reason}); using libjpeg-turbo",
            self.kind.name
        );
        if let State::Ready(helper) = std::mem::replace(state, State::Disabled) {
            abandon(helper.child);
        }
    }
    fn encode(&self, frame: &Frame) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &*state {
            State::Idle => {
                let (sender, receiver) = mpsc::channel();
                let (kind, executable) = (self.kind, self.executable.clone());
                let started = thread::Builder::new()
                    .name("capturefab-jpeg-start".into())
                    .spawn(move || {
                        let _ = sender
                            .send(Helper::start(kind, &executable).map_err(|e| format!("{e:#}")));
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
                        eprintln!(
                            "capturefab: {} unavailable ({error}); using libjpeg-turbo",
                            self.kind.name
                        );
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
        match exchange(self.kind, helper, frame, self.deadline) {
            Ok(jpeg) => jpeg,
            Err(reason) => {
                self.disable(&mut state, &reason);
                None
            }
        }
    }
}
/// One encode on a ready helper: `Ok(None)` keeps the helper but uses the
/// CPU for this frame; `Err` means the helper can no longer be trusted.
fn exchange(
    kind: &Kind,
    helper: &mut Helper,
    frame: &Frame,
    deadline: Duration,
) -> Result<Option<Vec<u8>>, String> {
    let channels = if monochrome(frame.pixel_format) { 1 } else { 3 };
    let Some(size) = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .filter(|&n| n > 0 && n <= MAX_PIXELS)
        .map(|n| n * channels)
    else {
        return Ok(None);
    };
    // Room for the frame and a JPEG larger than it, which noise can produce.
    let capacity = size * 2 + (1 << 20);
    if helper
        .shared
        .as_ref()
        .is_none_or(|s| s.map.len() < capacity)
    {
        // Drop the old buffer first; the helper remaps on the next request.
        helper.shared = None;
        let prefix = kind.argument.trim_start_matches('_');
        match Shared::create(prefix, capacity) {
            Ok(shared) => helper.shared = Some(shared),
            Err(_) => return Ok(None),
        }
    }
    let shared = helper.shared.as_mut().expect("shared buffer");
    // Demosaic or convert directly into the buffer the helper encodes from.
    let pixels = &mut shared.map[..size];
    let converted = if channels == 1 {
        crate::frame::convert_slice(frame, pixels, |[gray, _, _]| gray)
    } else {
        crate::frame::convert_slice(frame, pixels.as_chunks_mut::<3>().0, |rgb| rgb)
    };
    if converted.is_err() {
        // Unsupported or truncated: the CPU path reports the error.
        return Ok(None);
    }
    let request = Request {
        width: frame.width,
        height: frame.height,
        channels,
        buffer: shared.path.clone(),
        capacity: shared.map.len(),
    };
    if helper.jobs.send(request).is_err() {
        return Err("helper I/O stopped".into());
    }
    // Generous for multi-megapixel frames: healthy hardware takes milliseconds.
    let deadline = deadline + Duration::from_millis(size as u64 / 50_000);
    let reply = match helper.results.recv_timeout(deadline) {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err("hardware encode exceeded its deadline".into()),
    };
    if !shared.unlinked {
        // Mapped on both sides now; keeping the name only risks a stale file.
        shared.unlinked = std::fs::remove_file(&shared.path).is_ok();
    }
    if !reply.ok && reply.unsupported {
        return Ok(None);
    }
    if !reply.ok {
        return Err(reply.error.unwrap_or_else(|| "encode failed".into()));
    }
    if reply.bytes > shared.map.len() - size {
        return Err("helper reported an implausible size".into());
    }
    let jpeg = shared.map[size..size + reply.bytes].to_vec();
    if !well_formed(&jpeg, frame.width, frame.height) {
        return Err("helper returned a malformed JPEG".into());
    }
    Ok(Some(jpeg))
}

// ---------------------------------------------------------------------------
// Helper side.

/// A hardware JPEG encoder inside a helper process.
pub trait Encoder {
    /// A shared buffer was just mapped, for example to page-lock it.
    fn attach(&mut self, _map: &mut memmap2::MmapMut) {}
    /// That buffer is about to be unmapped.
    fn detach(&mut self, _map: &mut memmap2::MmapMut) {}
    /// Encode `pixels` as `request` describes and write the JPEG into `out`,
    /// returning its length. Return an [`Unsupported`] error to decline one
    /// request without disabling the helper.
    fn encode(&mut self, request: &Request, pixels: &[u8], out: &mut [u8]) -> Result<usize>;
}

/// Run a helper: report readiness (or why the hardware is unavailable), then
/// encode requests from stdin until it closes.
pub fn serve<E: Encoder>(start: impl FnOnce() -> Result<(E, String)>) -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut encoder = match start() {
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
            .context("invalid JPEG helper request")?;
        if mapping
            .as_ref()
            .is_none_or(|(path, map)| *path != request.buffer || map.len() != request.capacity)
        {
            if let Some((_, mut old)) = mapping.take() {
                encoder.detach(&mut old);
            }
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
            let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }?;
            encoder.attach(&mut map);
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
                unsupported: error.downcast_ref::<Unsupported>().is_some(),
                error: Some(format!("{error:#} after {:?}", started.elapsed())),
                ..Reply::default()
            },
        };
        write_message(&mut output, &reply)?;
    }
    if let Some((_, mut last)) = mapping {
        encoder.detach(&mut last);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    static FAKE: Kind = Kind {
        name: "fake",
        argument: "__fake",
        variable: "CAPTUREFAB_FAKE_JPEG",
        platform: true,
        environment: &[],
        accelerator: OnceLock::new(),
    };
    /// A fake helper: a shell script standing in for `capturefab __nvjpeg`.
    fn script(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "capturefab-accel-{name}-{}-{}",
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
    fn frame() -> Frame {
        Frame {
            id: 1,
            width: 8,
            height: 8,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 0,
            data: vec![0; 64],
        }
    }
    /// Drive the accelerator until it leaves the start-up phase.
    fn settle(accelerator: &Accelerator) -> Option<Vec<u8>> {
        let start = Instant::now();
        loop {
            let result = accelerator.encode(&frame());
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
        let accelerator = Accelerator::new(&FAKE, path.clone(), Duration::from_millis(200));
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
        assert!(accelerator.encode(&frame()).is_none());
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
            let accelerator = Accelerator::new(&FAKE, path.clone(), Duration::from_millis(500));
            assert!(settle(&accelerator).is_none(), "{name}");
            assert!(
                matches!(*accelerator.state.lock().unwrap(), State::Disabled),
                "{name}"
            );
            let _ = std::fs::remove_file(path);
        }
    }
    #[test]
    fn unsupported_requests_keep_the_helper() {
        // Declines every request but stays usable.
        let path = script(
            "declines",
            "echo '{\"ok\":true}'; while read line; do echo '{\"ok\":false,\"unsupported\":true}'; done",
        );
        let accelerator = Accelerator::new(&FAKE, path.clone(), Duration::from_millis(500));
        assert!(settle(&accelerator).is_none());
        assert!(matches!(
            *accelerator.state.lock().unwrap(),
            State::Ready(_)
        ));
        assert!(accelerator.encode(&frame()).is_none());
        assert!(matches!(
            *accelerator.state.lock().unwrap(),
            State::Ready(_)
        ));
        let _ = std::fs::remove_file(path);
    }
}
