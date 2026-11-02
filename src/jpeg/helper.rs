//! GPU JPEG encoders in a helper process, `capturefab __jpeg-helper KIND`
//! (`nvjpeg`, `vaapi`), which the camera worker that uses one starts.
//!
//! The helper loads the vendor's libraries when it starts, so a build needs
//! no SDK, and a driver that crashes or hangs takes down only the helper:
//! the worker kills a helper that misses its deadline, and under `auto` a
//! frame it held up, and every later one, is encoded on the CPU
//! (`backend.rs`); a named engine's still fails instead.
//!
//! Frames do not travel. While a GPU engine is open in the worker (its
//! `Client`), the worker's frame pools hand out shared memory segments
//! (`framebuf::share`): a camera writes each frame into one, and the helper
//! maps each segment once, from the descriptor sent with the first request
//! that names it, over the helper's control socket (its standard input). A
//! request names the frame's segment and an output segment, and the reply
//! says how long the JPEG the helper wrote there is: one JSON line each
//! way. A frame from before the engine opened, or in a USB transfer's own
//! buffer, is copied into a segment first (`gpu_upload`).
//!
//! A segment lent to the helper is not reused until it has answered or
//! ended: one killed for missing its deadline keeps what it was lent until
//! the process is reaped, and forever if it cannot be (a process stuck in a
//! GPU driver can outlive SIGKILL). A helper unused for a minute stops,
//! releasing its driver, and starts again for the next frame; the pools
//! keep sharing meanwhile, so the frames written while it rests need no
//! copy when it starts again.
#![allow(unsafe_code)]
// Engines use helpers on Linux; elsewhere the code is built for its tests.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::backend::{Backend, Kind, Unready, Unsupported};
use crate::{
    copies::{self, Site},
    frame::{Body, FrameRef},
    framebuf::{FrameBuf, PoolVec, Sharing, shared::Segment},
    types::Frame,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString, c_int, c_void},
    io::{self, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    process::{Child, Command, Stdio},
    ptr::NonNull,
    sync::{Arc, Mutex, PoisonError, Weak},
    time::{Duration, Instant},
};

/// The hidden first argument that runs a helper.
pub(crate) const ARG: &str = "__jpeg-helper";

/// The longest line either side writes.
const MAX_LINE: usize = 16 * 1024;
/// How long a helper may take to load its driver and say it is ready.
pub(crate) const STARTUP: Duration = Duration::from_secs(20);
/// How long a helper that was told to stop may take before it is killed.
const STOPPING: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Protocol.

/// What the worker asks: unmap these, map those, encode this.
#[derive(Serialize, Deserialize, Default, Debug)]
struct Request {
    /// Segments the worker has freed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    forget: Vec<u64>,
    /// Segments to map, in the order of the descriptors sent with the line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    map: Vec<Mapping>,
    encode: Job,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
struct Mapping {
    id: u64,
    len: usize,
}

/// A frame to encode: the first `len` bytes of segment `input`, as a JPEG
/// at `quality` written to the start of segment `output`, in at most
/// `capacity` bytes.
#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy)]
struct Job {
    input: u64,
    len: usize,
    width: u32,
    height: u32,
    pixel_format: u32,
    quality: i32,
    output: u64,
    capacity: usize,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct Reply {
    ok: bool,
    /// The JPEG's length.
    #[serde(default)]
    len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// It cannot take this frame (its size, its format), and stays usable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unsupported: bool,
}

/// The first line a helper writes: ready, and what it found, or why not.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub(crate) struct Hello {
    pub(crate) ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// For `doctor`: its device, driver and version, and how frames reach
    /// the device (`upload`: `in-place`, `dma` or `copy`).
    #[serde(default)]
    pub(crate) about: Map<String, Value>,
    /// It encodes monochrome frames as gray JPEGs.
    #[serde(default)]
    pub(crate) gray: bool,
}

fn line(message: &impl Serialize) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    ensure!(line.len() <= MAX_LINE, "a JPEG helper message is too long");
    Ok(line)
}

// ---------------------------------------------------------------------------
// Lines and descriptors over a Unix socket.

/// Lines read from a socket, with the descriptors that came with them.
struct Incoming {
    socket: UnixStream,
    buffer: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

impl Incoming {
    fn new(socket: UnixStream) -> Self {
        Self {
            socket,
            buffer: Vec::new(),
            fds: VecDeque::new(),
        }
    }
    /// The next line, without its newline; None at the end. With `until`,
    /// a line not complete by then is a `TimedOut` error.
    fn line(&mut self, until: Option<Instant>) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                return Ok(Some(line));
            }
            if self.buffer.len() > MAX_LINE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a line longer than the protocol allows",
                ));
            }
            if let Some(until) = until {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                self.socket.set_read_timeout(Some(left))?;
            }
            let mut chunk = [0u8; 4096];
            let read = match receive(&self.socket, &mut chunk, &mut self.fds) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Err(error) => return Err(error),
            };
            if read == 0 {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// The most descriptors one message carries.
const MAX_FDS: usize = 8;

/// Bytes from `socket` into `buffer`, and any descriptors that came with
/// them onto `fds`.
fn receive(
    socket: &UnixStream,
    buffer: &mut [u8],
    fds: &mut VecDeque<OwnedFd>,
) -> io::Result<usize> {
    // Aligned room for one message of MAX_FDS descriptors.
    let mut control = [0u64; 16];
    let mut iov = libc::iovec {
        iov_base: buffer.as_mut_ptr().cast(),
        iov_len: buffer.len(),
    };
    // SAFETY: a zeroed msghdr is valid; the pointers set below outlive the
    // call.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let read = loop {
        // SAFETY: a valid socket and a message whose buffers are live.
        let read = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, flags) };
        if read >= 0 {
            break read as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    // SAFETY: walking the control messages recvmsg wrote, within
    // `msg_controllen`; each SCM_RIGHTS payload is an array of descriptors
    // that now belong to this process.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            let cmsg = &*header;
            if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<c_int>();
                let bytes = cmsg.cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..bytes / size_of::<c_int>() {
                    let fd = OwnedFd::from_raw_fd(data.add(i).read_unaligned());
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push_back(fd);
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "descriptors were cut off",
        ));
    }
    Ok(read)
}

/// All of `bytes` to `socket`, the descriptors with the first of them.
fn send(socket: &UnixStream, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if fds.is_empty() {
        return (&*socket).write_all(bytes);
    }
    assert!(fds.len() <= MAX_FDS && !bytes.is_empty());
    let payload = (fds.len() * size_of::<c_int>()) as u32;
    // SAFETY: CMSG_SPACE is arithmetic.
    let space = unsafe { libc::CMSG_SPACE(payload) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: as in `receive`.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = space as _;
    // SAFETY: the control buffer holds one header and its descriptors.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(payload) as _;
        let data = libc::CMSG_DATA(header).cast::<c_int>();
        for (i, fd) in fds.iter().enumerate() {
            data.add(i).write_unaligned(fd.as_raw_fd());
        }
    }
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let sent = loop {
        // SAFETY: a valid socket and a message whose buffers are live.
        let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &message, flags) };
        if sent >= 0 {
            break sent as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    // The descriptors went with the first byte; the rest is plain.
    (&*socket).write_all(&bytes[sent..])
}

// ---------------------------------------------------------------------------
// Libraries loaded at run time.

/// A shared library opened with dlopen, closed when dropped.
pub(crate) struct Library(NonNull<c_void>);

// SAFETY: a dlopen handle may be used and closed from any thread.
unsafe impl Send for Library {}

impl Library {
    /// The library at `name` (a path, or a soname for the loader to find).
    pub(crate) fn open(name: &str) -> Result<Self> {
        let path = CString::new(name)?;
        // SAFETY: a NUL-terminated name; opening runs the library's
        // initializers, which is its documented use.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        match NonNull::new(handle) {
            Some(handle) => Ok(Self(handle)),
            None => bail!("{name}: {}", dl_error()),
        }
    }
    /// The function `name` as `T`, a function pointer type.
    ///
    /// # Safety
    /// `T` must be the symbol's C signature.
    pub(crate) unsafe fn symbol<T: Copy>(&self, name: &CStr) -> Result<T> {
        assert_eq!(size_of::<T>(), size_of::<*mut c_void>());
        // SAFETY: a live handle and a NUL-terminated name.
        let symbol = unsafe { libc::dlsym(self.0.as_ptr(), name.as_ptr()) };
        ensure!(
            !symbol.is_null(),
            "{} is missing ({})",
            name.to_string_lossy(),
            dl_error()
        );
        // SAFETY: the caller vouches for the type, a function pointer.
        Ok(unsafe { std::mem::transmute_copy(&symbol) })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: opened by `open`, closed once.
        unsafe { libc::dlclose(self.0.as_ptr()) };
    }
}

fn dl_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        "unknown error".into()
    } else {
        // SAFETY: non-null and NUL-terminated.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }
}

// ---------------------------------------------------------------------------
// The helper process.

/// A GPU JPEG encoder inside the helper.
pub(crate) trait Encoder {
    /// A segment the worker sent was mapped.
    fn attach(&mut self, _segment: &Segment) {}
    /// A segment is about to be unmapped: release what refers to it.
    fn detach(&mut self, _segment: &Segment) {}
    /// `frame`, whose bytes start `input`, as a JPEG at `quality` written to
    /// the start of `out`; its length. An [`Unsupported`] error declines
    /// this frame and keeps the helper.
    fn encode(
        &mut self,
        frame: &Frame,
        input: &Segment,
        quality: i32,
        out: &mut [u8],
    ) -> Result<usize>;
}

/// What a helper opens: its encoder and its hello, or why it cannot.
pub(crate) type Opened = Result<(Box<dyn Encoder>, Hello)>;

/// `capturefab __jpeg-helper KIND PARENT`: open the encoder, say hello on
/// standard input (the control socket), then answer requests until the
/// worker closes it.
pub(crate) fn run(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    let kind = args.next().unwrap_or_default();
    let parent: Option<u32> = args.next().and_then(|pid| pid.to_str()?.parse().ok());
    #[cfg(target_os = "linux")]
    {
        // Ending with the worker, even one that is killed: a helper the
        // worker cannot reach any more only holds the GPU.
        // SAFETY: prctl with a signal number.
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
        // A worker that ended before that would never send the signal.
        // (Its pid, not the parent's: CAPTUREFAB_EXECUTABLE may name a
        // wrapper that runs the helper as its own child.)
        // SAFETY: kill with signal 0 only checks that the process exists.
        let gone = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if parent.is_some_and(gone) {
            return Ok(());
        }
    }
    let _ = parent;
    // SAFETY: the worker made standard input the control socket; this
    // process owns it from here on.
    let socket = UnixStream::from(unsafe { OwnedFd::from_raw_fd(0) });
    let opened: Opened = match kind.to_str() {
        #[cfg(feature = "nvjpeg")]
        Some("nvjpeg") => super::nvjpeg::open(),
        #[cfg(feature = "vaapi")]
        Some("vaapi") => super::vaapi::open(),
        _ => Err(anyhow!(
            "this build has no {} JPEG helper",
            kind.to_string_lossy()
        )),
    };
    serve(socket, opened)
}

/// Say hello, then answer the worker's requests on `socket` until it is
/// closed.
pub(crate) fn serve(socket: UnixStream, opened: Opened) -> Result<()> {
    let mut output = socket.try_clone()?;
    let mut incoming = Incoming::new(socket);
    let mut encoder = match opened {
        Ok((encoder, hello)) => {
            output.write_all(&line(&hello)?)?;
            encoder
        }
        Err(error) => {
            let hello = Hello {
                error: Some(format!("{error:#}")),
                ..Hello::default()
            };
            output.write_all(&line(&hello)?)?;
            return Ok(());
        }
    };
    let mut segments: HashMap<u64, Arc<Segment>> = HashMap::new();
    let result = (|| -> Result<()> {
        while let Some(line_bytes) = incoming.line(None)? {
            let request: Request =
                serde_json::from_slice(&line_bytes).context("a malformed request")?;
            for id in &request.forget {
                if let Some(segment) = segments.remove(id) {
                    encoder.detach(&segment);
                }
            }
            for mapping in &request.map {
                let fd = incoming
                    .fds
                    .pop_front()
                    .context("a segment came without its descriptor")?;
                let segment = Arc::new(Segment::map(fd, mapping.len, mapping.id)?);
                encoder.attach(&segment);
                if let Some(old) = segments.insert(mapping.id, segment) {
                    encoder.detach(&old);
                }
            }
            let reply = match encode(&mut *encoder, &segments, &request.encode) {
                Ok(len) => Reply {
                    ok: true,
                    len,
                    ..Reply::default()
                },
                Err(error) => Reply {
                    unsupported: error.is::<Unsupported>(),
                    error: Some(format!("{error:#}")),
                    ..Reply::default()
                },
            };
            // Before the answer, so a command never ends before the copies
            // made for it are reported.
            copies::report_as("helper");
            output.write_all(&line(&reply)?)?;
        }
        Ok(())
    })();
    for segment in segments.values() {
        encoder.detach(segment);
    }
    result
}

/// One request's frame, read where the worker wrote it, encoded into the
/// segment it lent for the JPEG.
fn encode(
    encoder: &mut dyn Encoder,
    segments: &HashMap<u64, Arc<Segment>>,
    job: &Job,
) -> Result<usize> {
    ensure!(
        job.input != job.output,
        "the frame and its JPEG share a segment"
    );
    let input = segments
        .get(&job.input)
        .context("an unknown frame segment")?;
    let output = segments
        .get(&job.output)
        .context("an unknown JPEG segment")?;
    ensure!(
        job.len <= input.len() && job.capacity <= output.len(),
        "a frame or JPEG larger than its segment"
    );
    let frame = Frame {
        width: job.width,
        height: job.height,
        pixel_format: job.pixel_format,
        data: PoolVec::mapped(input.clone(), job.len).freeze(),
        ..Frame::default()
    };
    // SAFETY: the worker lent this segment for the JPEG and does not read
    // or write it until this request is answered; the frame is another.
    let out = unsafe { output.bytes_mut(job.capacity) };
    let len = encoder.encode(&frame, input, job.quality, out)?;
    ensure!(len <= job.capacity, "a JPEG longer than its segment");
    Ok(len)
}

// ---------------------------------------------------------------------------
// The worker's side.

/// How a client starts its helper.
#[derive(Clone)]
pub(crate) enum Launch {
    /// `capturefab __jpeg-helper KIND`, the program this process runs.
    Executable,
    /// Another program speaking the protocol, such as a test's script.
    #[cfg(test)]
    Program(std::path::PathBuf),
    /// `serve` on a thread of this process, for tests.
    #[cfg(test)]
    #[cfg_attr(not(feature = "jpeg"), allow(dead_code))]
    Thread(Arc<dyn Fn() -> Opened + Send + Sync>),
}

enum Process {
    Child(Child),
    #[cfg(test)]
    Thread(std::thread::JoinHandle<()>),
}

impl Process {
    fn kill(&mut self) {
        match self {
            Process::Child(child) => {
                let _ = child.kill();
            }
            #[cfg(test)]
            Process::Thread(_) => {}
        }
    }
    /// Wait for it to end; None when it does not end itself by `until`.
    fn wait_until(&mut self, until: Instant) -> Option<String> {
        loop {
            match self {
                Process::Child(child) => match child.try_wait() {
                    Ok(Some(status)) => return Some(describe_exit(status)),
                    Err(error) => return Some(format!("an unknown status ({error})")),
                    Ok(None) => {}
                },
                #[cfg(test)]
                Process::Thread(handle) if handle.is_finished() => {
                    return Some("the end of its thread".into());
                }
                #[cfg(test)]
                Process::Thread(_) => {}
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// Wait for it to end, however long that takes.
    fn wait(self) {
        match self {
            Process::Child(mut child) => {
                let _ = child.wait();
            }
            #[cfg(test)]
            Process::Thread(handle) => {
                let _ = handle.join();
            }
        }
    }
}

fn describe_exit(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (_, Some(signal)) => format!("signal {signal}"),
        (Some(code), _) => format!("exit status {code}"),
        _ => "no status".into(),
    }
}

/// What a request lent the helper: freed only once it has answered or
/// ended, by dropping it.
#[derive(Default)]
#[allow(dead_code)]
struct Lent {
    input: Option<FrameBuf>,
    output: Option<PoolVec>,
}

/// A running helper.
struct Helper {
    kind: Kind,
    process: Process,
    socket: UnixStream,
    incoming: Incoming,
    /// The segments it has mapped, until they are freed here.
    mapped: HashMap<u64, Weak<Segment>>,
}

impl Helper {
    /// Start a helper and wait up to `wait` for its hello.
    fn start(kind: Kind, launch: &Launch, wait: Duration) -> Result<(Self, Hello)> {
        let (ours, theirs) = UnixStream::pair().context("a socket for the JPEG helper")?;
        let process = match launch {
            Launch::Executable => {
                let mut command = Command::new(crate::session::executable()?);
                command
                    .arg(ARG)
                    .arg(kind.name())
                    .arg(std::process::id().to_string());
                // Its stderr is the worker's, which passes it on to the
                // user, so libva's messages go in its errors (`vaapi.rs`).
                Process::Child(spawn(command, theirs)?)
            }
            #[cfg(test)]
            Launch::Program(path) => Process::Child(spawn(Command::new(path), theirs)?),
            #[cfg(test)]
            Launch::Thread(open) => {
                let open = open.clone();
                Process::Thread(std::thread::spawn(move || {
                    let _ = serve(theirs, open());
                }))
            }
        };
        let mut helper = Self {
            kind,
            process,
            socket: ours.try_clone()?,
            incoming: Incoming::new(ours),
            mapped: HashMap::new(),
        };
        let hello: Hello = match helper.read(Instant::now() + wait) {
            Ok(hello) => hello,
            Err(error) => {
                helper.abandon(Lent::default());
                return Err(error);
            }
        };
        if !hello.ok {
            helper.stop();
            bail!(
                "{}",
                hello
                    .error
                    .unwrap_or_else(|| "its helper did not start".into())
            );
        }
        Ok((helper, hello))
    }

    /// The next line it writes, as a `T`, by `until`.
    fn read<T: DeserializeOwned>(&mut self, until: Instant) -> Result<T> {
        let started = Instant::now();
        let line = match self.incoming.line(Some(until)) {
            Ok(Some(line)) => line,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => bail!(
                "its helper did not answer within {} ms",
                until.saturating_duration_since(started).as_millis().max(1)
            ),
            other => match (other, self.ended()) {
                (_, Some(why)) => bail!("its helper ended with {why}"),
                (Err(error), None) => return Err(error).context("reading from its helper"),
                _ => bail!("its helper closed its socket"),
            },
        };
        // Descriptors are for the helper; any sent here are closed.
        self.incoming.fds.clear();
        serde_json::from_slice(&line).context("its helper wrote a malformed line")
    }

    /// Why the process has ended, after giving it a moment to.
    fn ended(&mut self) -> Option<String> {
        self.process
            .wait_until(Instant::now() + Duration::from_millis(500))
    }

    /// Ask for `job`, mapping the segments it names that the helper lacks
    /// and unmapping those freed here, and read the answer by `until`.
    fn exchange(
        &mut self,
        input: &Arc<Segment>,
        output: &Arc<Segment>,
        job: Job,
        until: Instant,
    ) -> Result<Reply> {
        let mut request = Request {
            encode: job,
            ..Request::default()
        };
        // Bounded, so the line stays short; the rest go with later ones.
        let mut room = 256;
        self.mapped.retain(|&id, segment| {
            if segment.strong_count() > 0 || room == 0 {
                return true;
            }
            room -= 1;
            request.forget.push(id);
            false
        });
        let mut fds = Vec::new();
        for segment in [input, output] {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.mapped.entry(segment.id())
            {
                request.map.push(Mapping {
                    id: segment.id(),
                    len: segment.len(),
                });
                fds.push(segment.fd());
                entry.insert(Arc::downgrade(segment));
            }
        }
        send(&self.socket, &line(&request)?, &fds).context("writing to its helper")?;
        self.read(until)
    }

    /// Stop it: close its socket, which ends a healthy helper, and kill it
    /// if it has not ended shortly after. Nothing is lent to it.
    fn stop(self) {
        let Self {
            mut process,
            socket,
            incoming,
            ..
        } = self;
        drop((socket, incoming));
        let reap = move || {
            if process.wait_until(Instant::now() + STOPPING).is_none() {
                process.kill();
                process.wait();
            }
        };
        if let Err(error) = std::thread::Builder::new()
            .name("jpeg-helper-stop".into())
            .spawn(reap)
        {
            crate::eprintln!("capturefab worker: cannot stop the JPEG helper: {error}");
        }
    }

    /// Kill it, keeping what it was lent until the process has been reaped;
    /// forever if it cannot be.
    fn abandon(self, lent: Lent) {
        let Self {
            kind,
            mut process,
            socket,
            incoming,
            ..
        } = self;
        process.kill();
        drop((socket, incoming));
        let held = Arc::new(Mutex::new(Some((process, lent))));
        let reaping = held.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("jpeg-{}-reap", kind.name()))
            .spawn(move || {
                let taken = reaping
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some((process, lent)) = taken {
                    process.wait();
                    drop(lent);
                }
            });
        if spawned.is_err() {
            // Without a thread to wait for it, what it was lent stays lent.
            std::mem::forget(held);
        }
    }
}

fn spawn(mut command: Command, socket: UnixStream) -> Result<Child> {
    command
        .stdin(Stdio::from(OwnedFd::from(socket)))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command.spawn().context("cannot start its helper process")
}

/// A GPU JPEG encoder in a helper process, from the worker's side: the
/// backend an engine of kind `nvjpeg` or `vaapi` runs.
pub(crate) struct Client {
    kind: Kind,
    launch: Launch,
    helper: Option<Helper>,
    hello: Hello,
    startup: Duration,
    /// The frame pools hand out shared memory while the client lives.
    _sharing: [Sharing; 2],
    /// A shorter deadline for answers than a frame's, for tests.
    #[cfg(test)]
    answer_within: Option<Duration>,
}

impl Client {
    /// Start a helper of `kind` and wait up to `startup` for it.
    pub(crate) fn open(kind: Kind, launch: Launch, startup: Duration) -> Result<Self> {
        let (helper, hello) = Helper::start(kind, &launch, startup)?;
        Ok(Self {
            kind,
            launch,
            helper: Some(helper),
            hello,
            startup,
            _sharing: crate::framebuf::share(),
            #[cfg(test)]
            answer_within: None,
        })
    }

    fn deadline(&self, frame: &Frame) -> Duration {
        #[cfg(test)]
        if let Some(within) = self.answer_within {
            return within;
        }
        super::backend::deadline(frame)
    }
}

/// The most a baseline JPEG of `frame` can take, as libjpeg-turbo reckons
/// (tj3JPEGBufSize): whole MCUs of 2 bytes a pixel for gray, 3 for 4:2:0
/// color, and room for headers.
pub(crate) fn jpeg_bound(frame: &Frame) -> usize {
    let (width, height) = (frame.width as usize, frame.height as usize);
    let bytes = if crate::jpeg::monochrome(frame.pixel_format) {
        2
    } else {
        3
    };
    width.next_multiple_of(16) * height.next_multiple_of(16) * bytes + 64 * 1024
}

impl Backend for Client {
    /// Every format the CPU encodes, at any quality; a helper that cannot
    /// take one declines it.
    fn accepts(view: FrameRef<'_>, quality: i32) -> bool {
        (1..=100).contains(&quality) && crate::frame::convertible(view.pixel_format())
    }

    fn encode(&mut self, view: FrameRef<'_>, quality: i32) -> Result<Body<'static>> {
        let frame = view.frame;
        if !view.is_whole() || crate::frame::validate_source(frame).is_err() {
            // The CPU says what is wrong with it. A fault of this frame, not
            // of its size or format: the engine learns nothing from it.
            return Err(Unready("not a whole, valid frame".into()).into());
        }
        if crate::jpeg::monochrome(frame.pixel_format) && !self.hello.gray {
            return Err(Unsupported("it does not encode gray JPEGs here".into()).into());
        }
        if self.helper.is_none() {
            self.wake()?;
        }
        // The frame's own shared memory, or a copy in a segment.
        let input = match frame.data.segment() {
            Some(_) => frame.data.clone(),
            None => {
                let mut copy = crate::framebuf::transforms()
                    .take_shared(frame.data.len())
                    .ok_or_else(|| Unready("no shared memory for the frame".into()))?;
                copy.copy_from_slice(&frame.data);
                copies::record(Site::GpuUpload, frame.data.len());
                copy.freeze()
            }
        };
        let capacity = jpeg_bound(frame);
        let output = crate::framebuf::transforms()
            .take_shared(capacity)
            .ok_or_else(|| Unready("no shared memory for the JPEG".into()))?;
        let (input_segment, output_segment) = (
            input.segment().expect("shared").clone(),
            output.segment().expect("shared").clone(),
        );
        let job = Job {
            input: input_segment.id(),
            len: frame.data.len(),
            width: frame.width,
            height: frame.height,
            pixel_format: frame.pixel_format,
            quality,
            output: output_segment.id(),
            capacity,
        };
        let until = Instant::now() + self.deadline(frame);
        let helper = self.helper.as_mut().expect("woken");
        let answered = helper.exchange(&input_segment, &output_segment, job, until);
        drop((input_segment, output_segment));
        match answered {
            Ok(Reply { ok: true, len, .. }) if len <= capacity => {
                drop(input);
                let mut output = output;
                output.truncate(len);
                copies::record(Site::JpegEncode, len);
                Ok(Body::Pooled(output))
            }
            Ok(Reply { ok: true, .. }) => {
                let helper = self.helper.take().expect("running");
                helper.abandon(Lent {
                    input: Some(input),
                    output: Some(output),
                });
                bail!("its helper reported a JPEG longer than its segment")
            }
            Ok(reply) if reply.unsupported => Err(Unsupported(
                reply
                    .error
                    .unwrap_or_else(|| "it declined the frame".into()),
            )
            .into()),
            Ok(reply) => Err(anyhow!(
                "{}",
                reply.error.unwrap_or_else(|| "it failed".into())
            )),
            Err(error) => {
                let helper = self.helper.take().expect("running");
                helper.abandon(Lent {
                    input: Some(input),
                    output: Some(output),
                });
                Err(error)
            }
        }
    }

    fn describe(&self) -> Map<String, Value> {
        self.hello.about.clone()
    }

    fn rest(&mut self) -> bool {
        match self.helper.take() {
            Some(helper) => {
                helper.stop();
                true
            }
            None => false,
        }
    }

    fn wake(&mut self) -> Result<()> {
        if self.helper.is_none() {
            let (helper, hello) = Helper::start(self.kind, &self.launch, self.startup)?;
            self.helper = Some(helper);
            self.hello = hello;
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.rest();
    }
}

/// The engine of `kind` this process uses: its helper is started on first
/// use, when this host has the device.
#[cfg(target_os = "linux")]
pub(crate) fn engine(kind: Kind) -> super::backend::Engine {
    let absent = match kind {
        #[cfg(feature = "nvjpeg")]
        Kind::Nvjpeg => super::nvjpeg::absent,
        #[cfg(feature = "vaapi")]
        Kind::Vaapi => super::vaapi::absent,
        _ => unreachable!("no helper for {kind:?}"),
    };
    super::backend::Engine::new(kind, move || {
        Client::open(kind, Launch::Executable, STARTUP)
    })
    .absent_when(absent)
    .open_within(STARTUP)
}

#[cfg(test)]
// Without libjpeg-turbo the fake helper has nothing to encode with.
#[cfg_attr(not(feature = "jpeg"), allow(dead_code, unused_imports))]
pub(crate) mod tests {
    use super::*;
    use crate::jpeg::QUALITY;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What the fake encoder saw of one frame.
    #[derive(Clone, Debug, PartialEq)]
    pub(crate) struct Seen {
        /// Where the frame's bytes were in the helper, and where the
        /// segment it was named by is mapped there.
        pub(crate) frame: usize,
        pub(crate) segment: usize,
        pub(crate) id: u64,
        pub(crate) first: u8,
        pub(crate) out: usize,
    }

    /// An encoder that writes libjpeg-turbo's JPEG of each frame, and
    /// records where it read it.
    #[derive(Default, Clone)]
    pub(crate) struct Fake {
        pub(crate) seen: Arc<Mutex<Vec<Seen>>>,
        pub(crate) attached: Arc<Mutex<Vec<u64>>>,
        pub(crate) detached: Arc<Mutex<Vec<u64>>>,
        /// Decline frames this wide.
        pub(crate) decline_width: Option<u32>,
    }

    impl Encoder for Fake {
        fn attach(&mut self, segment: &Segment) {
            self.attached.lock().unwrap().push(segment.id());
        }
        fn detach(&mut self, segment: &Segment) {
            self.detached.lock().unwrap().push(segment.id());
        }
        fn encode(
            &mut self,
            frame: &Frame,
            input: &Segment,
            quality: i32,
            out: &mut [u8],
        ) -> Result<usize> {
            if Some(frame.width) == self.decline_width {
                return Err(Unsupported("too wide".into()).into());
            }
            self.seen.lock().unwrap().push(Seen {
                frame: frame.data.as_ptr() as usize,
                segment: input.as_ptr() as usize,
                id: input.id(),
                first: frame.data[0],
                out: out.as_ptr() as usize,
            });
            let jpeg = crate::jpeg::encode(frame, quality)?;
            out[..jpeg.len()].copy_from_slice(&jpeg);
            Ok(jpeg.len())
        }
    }

    pub(crate) fn fake_launch(fake: Fake, gray: bool) -> Launch {
        Launch::Thread(Arc::new(move || {
            let hello = Hello {
                ok: true,
                gray,
                about: serde_json::json!({"device": "fake", "upload": "in-place"})
                    .as_object()
                    .unwrap()
                    .clone(),
                ..Hello::default()
            };
            Ok((Box::new(fake.clone()) as Box<dyn Encoder>, hello))
        }))
    }

    /// A frame in a segment of the frame pool, as a camera writes it while
    /// a helper runs.
    fn shared_frame(format: u32, width: u32, height: u32, fill: u8) -> Frame {
        let source = crate::frame::noise_frame(format, width, height);
        let mut data = crate::framebuf::pool()
            .take_shared(source.data.len())
            .unwrap();
        data.copy_from_slice(&source.data);
        data[0] = fill;
        Frame {
            data: data.freeze(),
            ..source
        }
    }

    /// A script standing in for a helper.
    pub(crate) fn script(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "capturefab-jpeg-helper-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    const READY: &str = r#"printf '{"ok":true,"gray":true}\n' >&0"#;

    /// The helper reads the frame in the very segment the camera wrote it
    /// to, through its own mapping, and writes the JPEG straight into the
    /// segment the worker reads it from: no copy on either side.
    #[cfg(feature = "jpeg")]
    #[test]
    fn helpers_read_frames_where_the_camera_wrote_them() {
        let fake = Fake::default();
        let mut client =
            Client::open(Kind::Nvjpeg, fake_launch(fake.clone(), true), STARTUP).unwrap();
        for (index, (format, fill)) in
            [(crate::types::RGB8, 7), (0x0108_0009, 9), (0x0108_0009, 11)]
                .into_iter()
                .enumerate()
        {
            let frame = shared_frame(format, 96, 64, fill);
            let segment = frame.data.segment().unwrap().clone();
            let jpeg = client.encode(FrameRef::whole(&frame), QUALITY).unwrap();
            let Body::Pooled(output) = &jpeg else {
                panic!("not the output segment")
            };
            assert!(output.segment().is_some());
            assert_eq!(jpeg, crate::jpeg::encode(&frame, QUALITY).unwrap());
            let seen = fake.seen.lock().unwrap()[index].clone();
            // The helper's own mapping of the frame's segment, at its start.
            assert_eq!(seen.id, segment.id());
            assert_eq!(seen.frame, seen.segment);
            assert_eq!(seen.first, fill);
            assert_ne!(
                seen.frame,
                segment.as_ptr() as usize,
                "a mapping of its own"
            );
        }
        // Each segment was sent once: the frames reused theirs.
        let attached = fake.attached.lock().unwrap().clone();
        let mut unique = attached.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), attached.len());
    }

    /// Frames from before the helper started are copied into a segment
    /// once; segments the worker frees are unmapped by the helper on the
    /// next request.
    #[cfg(feature = "jpeg")]
    #[test]
    fn heap_frames_are_copied_once_and_freed_segments_forgotten() {
        let fake = Fake::default();
        let mut client =
            Client::open(Kind::Vaapi, fake_launch(fake.clone(), true), STARTUP).unwrap();
        let heap = crate::frame::noise_frame(crate::types::RGB8, 64, 48);
        assert!(heap.data.segment().is_none());
        let jpeg = client.encode(FrameRef::whole(&heap), 75).unwrap();
        assert_eq!(jpeg, crate::jpeg::encode(&heap, 75).unwrap());
        let first = fake.attached.lock().unwrap().clone();
        assert_eq!(first.len(), 2, "the copy and the output");
        drop(jpeg);
        // A segment of no pool is freed with its frame; the helper unmaps
        // it with the next request.
        let frame = unpooled_frame(crate::types::RGB8, 64, 48);
        let id = frame.data.segment().unwrap().id();
        client.encode(FrameRef::whole(&frame), QUALITY).unwrap();
        assert!(fake.attached.lock().unwrap().contains(&id));
        drop(frame);
        let other = shared_frame(crate::types::RGB8, 64, 48, 1);
        client.encode(FrameRef::whole(&other), QUALITY).unwrap();
        assert_eq!(*fake.detached.lock().unwrap(), [id]);
    }

    /// A frame in a segment of no pool, freed when nothing holds it.
    fn unpooled_frame(format: u32, width: u32, height: u32) -> Frame {
        let source = crate::frame::noise_frame(format, width, height);
        let mut data = PoolVec::mapped(
            Arc::new(Segment::create(source.data.len()).unwrap()),
            source.data.len(),
        );
        data.copy_from_slice(&source.data);
        Frame {
            data: data.freeze(),
            ..source
        }
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn declined_frames_keep_the_helper() {
        let fake = Fake {
            decline_width: Some(32),
            ..Fake::default()
        };
        let mut client = Client::open(Kind::Nvjpeg, fake_launch(fake, false), STARTUP).unwrap();
        let frame = shared_frame(crate::types::RGB8, 32, 16, 1);
        let error = client.encode(FrameRef::whole(&frame), QUALITY).unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
        // A helper without gray JPEGs declines monochrome frames itself.
        let mono = shared_frame(crate::types::MONO8, 64, 16, 1);
        let error = client.encode(FrameRef::whole(&mono), QUALITY).unwrap_err();
        assert!(error.is::<Unsupported>(), "{error:#}");
        // A truncated frame is a fault of that frame, not a decline of its
        // size: the engine keeps no verdict for it.
        let frame = shared_frame(crate::types::RGB8, 64, 16, 1);
        let truncated = Frame {
            height: 32,
            ..frame.clone()
        };
        let error = client
            .encode(FrameRef::whole(&truncated), QUALITY)
            .unwrap_err();
        assert!(error.is::<Unready>(), "{error:#}");
        assert!(client.encode(FrameRef::whole(&frame), QUALITY).is_ok());
    }

    /// A helper that crashes, hangs, writes garbage or refuses to start is
    /// an error the engine turns into a CPU fallback; a hung one is killed.
    #[test]
    fn misbehaving_helpers_fail_their_request() {
        let frame = shared_frame(crate::types::RGB8, 32, 16, 1);
        for (name, body, expected) in [
            (
                "crashes",
                format!("{READY}; read line; kill -SEGV $$"),
                "ended with signal 11",
            ),
            (
                "hangs",
                format!("{READY}; read line; exec sleep 30"),
                "did not answer within",
            ),
            (
                "garbage",
                format!("{READY}; read line; printf 'nonsense\\n' >&0; sleep 5"),
                "malformed line",
            ),
            (
                "lies",
                format!(
                    "{READY}; read line; printf '{{\"ok\":true,\"len\":999999999}}\\n' >&0; sleep 5"
                ),
                "longer than its segment",
            ),
        ] {
            let path = script(name, &body);
            let mut client = Client::open(Kind::Nvjpeg, Launch::Program(path.clone()), STARTUP)
                .unwrap_or_else(|error| panic!("{name}: {error:#}"));
            client.answer_within = Some(Duration::from_millis(300));
            let started = Instant::now();
            let error = client.encode(FrameRef::whole(&frame), QUALITY).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{name}: {error:#}");
            assert!(!error.is::<Unsupported>(), "{name}");
            assert!(started.elapsed() < Duration::from_secs(3), "{name}");
            assert!(client.helper.is_none(), "{name}: abandoned");
            let _ = std::fs::remove_file(path);
        }
        let refuses = script(
            "refuses",
            r#"printf '{"ok":false,"error":"no device"}\n' >&0"#,
        );
        let error = Client::open(Kind::Vaapi, Launch::Program(refuses.clone()), STARTUP)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("no device"), "{error:#}");
        let silent = script("silent", "exec sleep 30");
        let started = Instant::now();
        let error = Client::open(
            Kind::Vaapi,
            Launch::Program(silent.clone()),
            Duration::from_millis(300),
        )
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("did not answer"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
        let _ = std::fs::remove_file(refuses);
        let _ = std::fs::remove_file(silent);
    }

    /// An engine whose helper process crashes on a frame: under `auto` that
    /// frame and later ones are the CPU's, and the engine is off; named, the
    /// still fails as unavailable.
    #[cfg(feature = "jpeg")]
    #[test]
    fn a_crashing_helper_leaves_frames_to_the_cpu() {
        use super::super::backend::Engine;
        let path = script("crash", &format!("{READY}; read line; kill -SEGV $$"));
        let frame = shared_frame(crate::types::RGB8, 64, 32, 1);
        let cpu = crate::jpeg::encode(&frame, QUALITY).unwrap();
        let launch = Launch::Program(path.clone());
        let engine = Engine::new(Kind::Nvjpeg, move || {
            Client::open(Kind::Nvjpeg, launch, STARTUP)
        })
        .any_size();
        // Checked beside the first frame, which the CPU encodes.
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        let reason = engine.settled().unavailable().unwrap();
        assert!(reason.contains("signal 11"), "{reason}");
        assert_eq!(engine.encode(&frame, QUALITY, false).unwrap(), cpu);
        let error = engine.encode(&frame, QUALITY, true).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::Unavailable
        );
        let _ = std::fs::remove_file(path);
    }

    /// What a request lent stays lent until the helper is reaped: a helper
    /// that cannot be (a thread here, as a process stuck in a driver) keeps
    /// the frame's and the JPEG's segments for good.
    #[cfg(feature = "jpeg")]
    #[test]
    fn lent_segments_stay_lent_until_the_helper_is_reaped() {
        /// Hangs on a frame until the test lets go.
        struct Hang(Arc<Mutex<()>>);
        impl Encoder for Hang {
            fn encode(
                &mut self,
                frame: &Frame,
                input: &Segment,
                quality: i32,
                out: &mut [u8],
            ) -> Result<usize> {
                drop(self.0.lock().unwrap());
                Fake::default().encode(frame, input, quality, out)
            }
        }
        let stuck = Arc::new(Mutex::new(()));
        let held = stuck.clone();
        let held = held.lock().unwrap();
        let launch = Launch::Thread(Arc::new(move || {
            let hello = Hello {
                ok: true,
                ..Hello::default()
            };
            Ok((Box::new(Hang(stuck.clone())) as Box<dyn Encoder>, hello))
        }));
        let mut client = Client::open(Kind::Nvjpeg, launch, STARTUP).unwrap();
        client.answer_within = Some(Duration::from_millis(200));
        let frame = unpooled_frame(crate::types::RGB8, 64, 32);
        let segment = Arc::downgrade(frame.data.segment().unwrap());
        let error = client.encode(FrameRef::whole(&frame), QUALITY).unwrap_err();
        assert!(format!("{error:#}").contains("did not answer"), "{error:#}");
        drop(frame);
        std::thread::sleep(Duration::from_millis(300));
        assert!(segment.upgrade().is_some(), "freed while the helper lives");
        // Once it ends and is reaped, what it was lent goes back.
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(10);
        while segment.upgrade().is_some() {
            assert!(Instant::now() < deadline, "never freed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A rested client stops its helper and starts a new one for the next
    /// frame.
    #[cfg(feature = "jpeg")]
    #[test]
    fn rested_helpers_start_again() {
        let fake = Fake::default();
        let mut client =
            Client::open(Kind::Nvjpeg, fake_launch(fake.clone(), true), STARTUP).unwrap();
        let frame = shared_frame(crate::types::RGB8, 32, 16, 1);
        client.encode(FrameRef::whole(&frame), QUALITY).unwrap();
        client.rest();
        assert!(client.helper.is_none());
        client.encode(FrameRef::whole(&frame), QUALITY).unwrap();
        // The new helper mapped the frame's segment again.
        let id = frame.data.segment().unwrap().id();
        let attached = fake.attached.lock().unwrap();
        assert_eq!(attached.iter().filter(|&&a| a == id).count(), 2);
        assert_eq!(client.describe()["device"], "fake");
    }

    /// What a request costs beyond the encoder's own work: the frame in a
    /// segment as cameras write it while a helper runs, and a frame from
    /// the heap, which is copied into one first.
    #[test]
    #[ignore = "a measurement; run with --ignored --nocapture"]
    fn helper_round_trip_cost() {
        struct Nothing;
        impl Encoder for Nothing {
            fn encode(&mut self, _: &Frame, _: &Segment, _: i32, out: &mut [u8]) -> Result<usize> {
                out[..2].copy_from_slice(&[0xFF, 0xD9]);
                Ok(2)
            }
        }
        let launch = Launch::Thread(Arc::new(|| {
            let hello = Hello {
                ok: true,
                gray: true,
                ..Hello::default()
            };
            Ok((Box::new(Nothing) as Box<dyn Encoder>, hello))
        }));
        let mut client = Client::open(Kind::Nvjpeg, launch, STARTUP).unwrap();
        for (name, frame) in [
            (
                "1080p RGB8 in a segment",
                shared_frame(crate::types::RGB8, 1920, 1080, 1),
            ),
            (
                "12 MP Mono8 in a segment",
                shared_frame(crate::types::MONO8, 4096, 3000, 1),
            ),
            (
                "12 MP Mono8 on the heap",
                crate::frame::noise_frame(crate::types::MONO8, 4096, 3000),
            ),
        ] {
            for _ in 0..20 {
                client.encode(FrameRef::whole(&frame), QUALITY).unwrap();
            }
            let started = Instant::now();
            let count = 500;
            for _ in 0..count {
                client.encode(FrameRef::whole(&frame), QUALITY).unwrap();
            }
            let each = started.elapsed() / count;
            eprintln!("{name}: {} us a request", each.as_micros());
        }
    }

    #[test]
    fn descriptors_travel_with_their_line() {
        let (a, b) = UnixStream::pair().unwrap();
        let segment = Segment::create(4096).unwrap();
        // SAFETY: this test alone uses it.
        unsafe { segment.bytes_mut(1)[0] = 42 };
        send(&a, b"{\"x\":1}\n{\"y\":2}\n", &[segment.fd()]).unwrap();
        let mut incoming = Incoming::new(b);
        assert_eq!(incoming.line(None).unwrap().unwrap(), b"{\"x\":1}");
        let fd = incoming.fds.pop_front().unwrap();
        let mapped = Segment::map(fd, 4096, 9).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { mapped.bytes(1) }, &[42]);
        assert_eq!(incoming.line(None).unwrap().unwrap(), b"{\"y\":2}");
        drop(a);
        assert!(incoming.line(None).unwrap().is_none());
        // A line that never ends is cut off at the protocol's limit.
        let (a, b) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = (&a).write_all(&vec![b'x'; MAX_LINE * 2]);
        });
        let error = Incoming::new(b).line(None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().unwrap();
    }

    #[test]
    fn libraries_that_are_missing_say_so() {
        let error = Library::open("libcapturefab-no-such-library.so.1")
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("libcapturefab-no-such-library"));
        // A library every Unix process has, and a symbol it lacks.
        #[cfg(target_os = "linux")]
        let name = "libc.so.6";
        #[cfg(not(target_os = "linux"))]
        let name = "/usr/lib/libSystem.B.dylib";
        if let Ok(library) = Library::open(name) {
            // SAFETY: getpid's C signature.
            let getpid: unsafe extern "C" fn() -> c_int =
                unsafe { library.symbol(c"getpid") }.unwrap();
            assert_eq!(unsafe { getpid() } as u32, std::process::id());
            let missing = unsafe { library.symbol::<unsafe extern "C" fn()>(c"capturefab_none") };
            assert!(missing.is_err());
        }
    }
}
