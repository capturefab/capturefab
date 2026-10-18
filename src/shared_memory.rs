//! Three-slot, single-producer/single-consumer frame ring shared across processes.
//! Slot ownership uses acquire/release AtomicU32 transitions; pixels never travel in JSON.
//!
//! A ring lives in anonymous memory where the platform has it: a Linux memfd,
//! or a macOS POSIX shared memory object unlinked as soon as it exists. The
//! worker inherits its descriptor, so no file holds frames and nothing
//! outlives the two processes. Windows, and hosts where that fails, use a file
//! in the session directory that its creator keeps locked and removes;
//! `sweep` removes files whose creator died.
//!
//! Publishing follows the reader. Every take counts as a poll. While the
//! reader has polled within `READER_IDLE` the writer copies every frame;
//! otherwise it copies one keep-alive frame per `KEEP_ALIVE`, so a camera
//! nobody watches costs no copies. The sequence counts every frame offered,
//! published or not, so watchers still see acquisition move. Consumers rely on:
//! - Continuous readers (GUI previews, the browser's 250 ms preview poll) keep
//!   the ring watched and get current frames.
//! - An occasional poller may get a keep-alive frame up to `KEEP_ALIVE` old.
//!   Take one plainly only where that is acceptable; otherwise poll until
//!   `fresh` (`SessionHandle::fresh_frame_for`), which happens within one
//!   frame because the poll itself re-arms the writer.
//! - Frames a command captured are forced in, so a capture's own frame is the
//!   newest one published (`capture -o -`).
//! - A frame larger than a slot is replaced by a downscaled RGB8 preview
//!   marked `preview_only`, which must never stand in for the captured frame.
use crate::types::{Frame, RGB8};
use anyhow::{Context, Result, ensure};
use memmap2::MmapRaw;
use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
const HEADER: usize = 64;
const META: usize = 64;
const SLOTS: usize = 3;
const MAGIC: &[u8; 8] = b"CFABRNG2";
// Header words after the magic and capacity.
const SEQUENCE: usize = 16;
const SKIPPED: usize = 20;
const CANCEL: usize = 24;
const POLLS: usize = 28;
const FREE: u32 = 0;
const WRITING: u32 = 1;
const READY: u32 = 2;
const READING: u32 = 3;
/// Slot flag: a downscaled RGB8 stand-in for a frame larger than the slot.
const PREVIEW: u32 = 1;
pub const DEFAULT_CAPACITY: usize = 16 * 1024 * 1024;
/// Without a poll for this long the reader is gone.
const READER_IDLE: Duration = Duration::from_secs(2);
/// An unwatched ring still gets a frame this often.
const KEEP_ALIVE: Duration = Duration::from_secs(1);

/// What `write` did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// Copied into a slot.
    Frame,
    /// Larger than a slot: a downscaled preview took its place.
    Preview,
    /// Not copied: nobody is watching and no keep-alive is due.
    Skipped,
}

/// A ring file its creator holds open, and on Unix locked, until it removes it.
struct RingFile {
    path: PathBuf,
    file: Option<File>,
}
impl RingFile {
    fn map(&self, length: u64) -> Result<MmapRaw> {
        let file = self.file.as_ref().context("ring file is closed")?;
        #[cfg(unix)]
        fs2::FileExt::try_lock_exclusive(file)?;
        reserve(file, length)?;
        Ok(MmapRaw::map_raw(file)?)
    }
}
impl Drop for RingFile {
    fn drop(&mut self) {
        // Windows refuses to remove a file this process still has open.
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}
/// Size a ring file. On a disk its blocks are reserved now: in a sparse file
/// a disk that fills up later kills the worker with SIGBUS when a frame first
/// touches a page.
fn reserve(file: &File, length: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    match allocate(file, length) {
        Err(e) if e.kind() != std::io::ErrorKind::StorageFull => {}
        reserved => return reserved,
    }
    file.set_len(length)
}
/// tmpfs is left sparse: reserving there would commit RAM for slot space
/// smaller frames never use.
#[cfg(target_os = "linux")]
fn allocate(file: &File, length: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    let fd = file.as_raw_fd();
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs fills `stat` for a valid descriptor; it is read only
    // when that succeeded.
    let tmpfs = unsafe {
        libc::fstatfs(fd, stat.as_mut_ptr()) == 0 && stat.assume_init().f_type as u64 == TMPFS_MAGIC
    };
    if tmpfs {
        return Err(std::io::ErrorKind::Unsupported.into());
    }
    // SAFETY: a valid descriptor. posix_fallocate returns its error rather
    // than setting errno.
    match unsafe { libc::posix_fallocate(fd, 0, length as libc::off_t) } {
        0 => Ok(()),
        error => Err(std::io::Error::from_raw_os_error(error)),
    }
}
#[cfg(all(unix, not(target_os = "linux")))]
fn allocate(file: &File, length: u64) -> std::io::Result<()> {
    fs2::FileExt::allocate(file, length)
}

/// A mapped ring. Fields drop in order, so a creator's file is removed only
/// once nothing maps it.
struct Mapping {
    raw: MmapRaw,
    file: Option<RingFile>,
}
impl Mapping {
    fn atomic(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.raw.len());
        // SAFETY: mmap is page aligned, all atomic offsets are 4-byte aligned and
        // exclusively accessed atomically after initial construction. Both processes
        // use the same native-endian AtomicU32 ABI; state guards all non-atomic bytes.
        unsafe { &*self.raw.as_ptr().add(offset).cast::<AtomicU32>() }
    }
    fn bytes(&self, offset: usize, length: usize) -> &[u8] {
        assert!(offset + length <= self.raw.len());
        // SAFETY: in bounds; the caller owns these bytes through a slot state
        // or reads fields that never change after creation.
        unsafe { std::slice::from_raw_parts(self.raw.as_ptr().add(offset), length) }
    }
}

/// A ring's header counters, readable from any thread without the ring: they
/// are atomics, so a sequence poll never waits behind a frame copy.
#[derive(Clone)]
pub struct RingCounters(Arc<Mapping>);
impl RingCounters {
    pub fn sequence(&self) -> u32 {
        self.0.atomic(SEQUENCE).load(Ordering::Acquire)
    }
    pub fn preview_skipped(&self) -> u32 {
        self.0.atomic(SKIPPED).load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.0.atomic(CANCEL).store(1, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
struct Taken {
    sequence: u32,
    preview: bool,
}

pub struct SharedRing {
    map: Arc<Mapping>,
    capacity: usize,
    /// The creator's descriptor for its worker to inherit, until it has.
    #[cfg(unix)]
    handoff: Option<std::os::fd::OwnedFd>,
    /// Writer: the reader's poll count as last seen, and when it last changed.
    polls: u32,
    polled_at: Option<Instant>,
    published_at: Option<Instant>,
    /// Reader: the frame last taken.
    taken: Option<Taken>,
}
fn valid_capacity(capacity: usize) -> bool {
    (4096..=256 * 1024 * 1024).contains(&capacity) && capacity.is_multiple_of(64)
}
fn ring_bytes(capacity: usize) -> usize {
    HEADER + SLOTS * (META + capacity)
}
impl SharedRing {
    /// A ring for a new worker: anonymous memory where the platform has it,
    /// otherwise a file in `dir`, which must be private.
    pub fn create(dir: &Path, capacity: usize) -> Result<Self> {
        ensure!(
            valid_capacity(capacity),
            "ring capacity must be aligned to 64 bytes, 4096..268435456"
        );
        #[cfg(unix)]
        if std::env::var_os("CAPTUREFAB_RING_FILE_TEST").is_none() {
            match memory(ring_bytes(capacity)) {
                Ok(fd) => {
                    let mut ring = Self::init(MmapRaw::map_raw(&fd)?, None, capacity);
                    ring.handoff = Some(fd);
                    return Ok(ring);
                }
                Err(e) => eprintln!(
                    "capturefab: shared memory unavailable ({e}); keeping frames in {}",
                    dir.display()
                ),
            }
        }
        Self::create_file(dir, capacity)
    }
    /// A ring in a file in `dir`. On Unix it is locked before it takes the
    /// name `sweep` looks for, so a sweep never mistakes it for a dead one.
    fn create_file(dir: &Path, capacity: usize) -> Result<Self> {
        let length = ring_bytes(capacity) as u64;
        enough_space(dir, fs2::available_space(dir).unwrap_or(u64::MAX), length)?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random ring name: {e}"))?;
        let name = hex(&random);
        let path = dir.join(format!("worker-{name}.shm"));
        let staging = if cfg!(unix) {
            dir.join(format!("worker-{name}.new"))
        } else {
            path.clone()
        };
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // On Windows the open handle is the lock: without FILE_SHARE_DELETE,
        // no other process can remove the file while it is in use.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        let mut file = RingFile {
            file: Some(options.open(&staging)?),
            path: staging,
        };
        let raw = file.map(length).map_err(|e| {
            let full = e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::StorageFull);
            if full {
                storage_full(dir, fs2::available_space(dir).unwrap_or(0), length)
            } else {
                e
            }
        })?;
        if file.path != path {
            std::fs::rename(&file.path, &path)?;
            file.path = path;
        }
        Ok(Self::init(raw, Some(file), capacity))
    }
    fn init(raw: MmapRaw, file: Option<RingFile>, capacity: usize) -> Self {
        let mut ring = Self::new(Arc::new(Mapping { raw, file }), capacity);
        // New memory and files read as zeros, so every slot starts FREE.
        // Writing slot headers here would commit pages no frame needs yet.
        ring.put(0, MAGIC);
        ring.put(8, &(capacity as u64).to_le_bytes());
        ring
    }
    fn new(map: Arc<Mapping>, capacity: usize) -> Self {
        Self {
            map,
            capacity,
            #[cfg(unix)]
            handoff: None,
            polls: 0,
            polled_at: None,
            published_at: None,
            taken: None,
        }
    }
    /// Open the ring a worker was given: `fd:N` for an inherited descriptor,
    /// otherwise a file path.
    pub fn open(spec: impl AsRef<OsStr>) -> Result<Self> {
        let spec = spec.as_ref();
        #[cfg(unix)]
        if let Some(fd) = spec.to_str().and_then(|spec| spec.strip_prefix("fd:")) {
            return Self::open_fd(fd.parse().context("invalid shared ring descriptor")?);
        }
        let path = Path::new(spec);
        let meta = std::fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "ring must be a regular file"
        );
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        ensure!(
            file.metadata()?.len() >= HEADER as u64,
            "truncated shared ring"
        );
        Self::opened(MmapRaw::map_raw(&file)?)
    }
    #[cfg(unix)]
    fn open_fd(fd: std::os::fd::RawFd) -> Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: fcntl only inspects the descriptor number.
        let open = unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
        ensure!(fd > 2 && open, "invalid shared ring descriptor");
        // SAFETY: the parent passed this descriptor for the ring alone, and
        // nothing else in this process uses it. It closes once mapped.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        Self::opened(MmapRaw::map_raw(&fd)?)
    }
    fn opened(raw: MmapRaw) -> Result<Self> {
        ensure!(raw.len() >= HEADER, "truncated shared ring");
        let map = Arc::new(Mapping { raw, file: None });
        ensure!(map.bytes(0, 8) == MAGIC, "shared ring version mismatch");
        let capacity = usize::try_from(u64::from_le_bytes(map.bytes(8, 8).try_into()?))?;
        // Shared memory may be rounded up to whole pages.
        ensure!(
            valid_capacity(capacity) && map.raw.len() >= ring_bytes(capacity),
            "invalid shared ring layout"
        );
        Ok(Self::new(map, capacity))
    }
    /// Let `command` open this ring: append the worker's argument and, for
    /// anonymous memory, have that child, and only it, inherit the descriptor.
    pub fn pass_to(&self, command: &mut std::process::Command) {
        command.args(self.worker_arg());
        #[cfg(unix)]
        if let Some(fd) = &self.handoff {
            use std::os::{fd::AsRawFd, unix::process::CommandExt};
            let fd = fd.as_raw_fd();
            // SAFETY: only async-signal-safe fcntl calls run between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    /// The worker has started, so the creator's descriptor is no longer needed.
    pub fn passed(&mut self) {
        #[cfg(unix)]
        {
            self.handoff = None;
        }
    }
    /// What a worker opens this ring with: `fd:N` for an inherited
    /// descriptor, otherwise the file's path.
    fn worker_arg(&self) -> Option<OsString> {
        #[cfg(unix)]
        if let Some(fd) = &self.handoff {
            use std::os::fd::AsRawFd;
            return Some(format!("fd:{}", fd.as_raw_fd()).into());
        }
        self.map.file.as_ref().map(|file| file.path.clone().into())
    }
    /// A second mapping of a ring this process created, opened the way its
    /// worker would, for tests and benchmarks.
    pub fn reader(&self) -> Result<Self> {
        #[cfg(unix)]
        if let Some(fd) = &self.handoff {
            use std::os::fd::IntoRawFd;
            let fd = fd.try_clone()?.into_raw_fd();
            return Self::open(format!("fd:{fd}"));
        }
        let file = self
            .map
            .file
            .as_ref()
            .context("not a ring this process created")?;
        Self::open(&file.path)
    }
    pub fn counters(&self) -> RingCounters {
        RingCounters(self.map.clone())
    }
    pub fn capacity(&self) -> usize {
        self.capacity
    }
    fn base(&self, slot: usize) -> usize {
        HEADER + slot * (META + self.capacity)
    }
    fn atomic(&self, offset: usize) -> &AtomicU32 {
        self.map.atomic(offset)
    }
    fn put(&mut self, offset: usize, data: &[u8]) {
        assert!(offset + data.len() <= self.map.raw.len());
        // SAFETY: in bounds, and this side owns these bytes: the header before
        // the ring is shared, or a slot it holds as WRITING.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.map.raw.as_mut_ptr().add(offset),
                data.len(),
            );
        }
    }
    fn word(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.map.bytes(offset, 4).try_into().expect("four bytes"))
    }
    fn quad(&self, offset: usize) -> u64 {
        u64::from_le_bytes(self.map.bytes(offset, 8).try_into().expect("eight bytes"))
    }
    pub fn sequence(&self) -> u32 {
        self.atomic(SEQUENCE).load(Ordering::Acquire)
    }
    /// Frames replaced unread while the reader was polling, because a newer
    /// one came first.
    pub fn preview_skipped(&self) -> u32 {
        self.atomic(SKIPPED).load(Ordering::Acquire)
    }
    pub fn cancelled(&self) -> bool {
        self.atomic(CANCEL).load(Ordering::Acquire) != 0
    }
    /// Whether the reader polled within `READER_IDLE`. Polls are counted in
    /// the header and timed by the writer's own clock.
    fn watched(&mut self, now: Instant) -> bool {
        let polls = self.atomic(POLLS).load(Ordering::Relaxed);
        if polls != self.polls {
            self.polls = polls;
            self.polled_at = Some(now);
        }
        self.polled_at
            .is_some_and(|at| now.duration_since(at) < READER_IDLE)
    }
    /// Offer `frame` to the reader. It is copied while the reader polls, as
    /// a keep-alive once per `KEEP_ALIVE` otherwise, and always with `force`,
    /// for frames a command captured.
    pub fn write(&mut self, frame: &Frame, force: bool) -> Result<Published> {
        let now = Instant::now();
        let watched = self.watched(now);
        let sequence = self.sequence().wrapping_add(1);
        let due = self
            .published_at
            .is_none_or(|at| now.duration_since(at) >= KEEP_ALIVE);
        if !(force || watched || due) {
            self.atomic(SEQUENCE).store(sequence, Ordering::Release);
            return Ok(Published::Skipped);
        }
        let small = (frame.data.len() > self.capacity)
            .then(|| preview(frame, self.capacity))
            .transpose()?;
        let Some(slot) = self.claim(watched) else {
            self.atomic(SEQUENCE).store(sequence, Ordering::Release);
            return Ok(Published::Skipped);
        };
        let (frame, flags) = small.as_ref().map_or((frame, 0), |small| (small, PREVIEW));
        let b = self.base(slot);
        self.put(b + 4, &sequence.to_le_bytes());
        self.put(b + 8, &frame.id.to_le_bytes());
        self.put(b + 16, &frame.width.to_le_bytes());
        self.put(b + 20, &frame.height.to_le_bytes());
        self.put(b + 24, &frame.pixel_format.to_le_bytes());
        self.put(b + 28, &(frame.data.len() as u32).to_le_bytes());
        self.put(b + 32, &frame.timestamp_ns.to_le_bytes());
        self.put(b + 40, &flags.to_le_bytes());
        self.put(b + META, &frame.data);
        self.atomic(b).store(READY, Ordering::Release);
        self.atomic(SEQUENCE).store(sequence, Ordering::Release);
        self.published_at = Some(now);
        Ok(if small.is_some() {
            Published::Preview
        } else {
            Published::Frame
        })
    }
    /// A FREE slot, or else one holding a frame the reader never took. That
    /// frame counts as skipped only while the reader polls; unwatched rings
    /// replace keep-alives nobody missed.
    fn claim(&mut self, watched: bool) -> Option<usize> {
        for state in [FREE, READY] {
            for i in 0..SLOTS {
                if self
                    .atomic(self.base(i))
                    .compare_exchange(state, WRITING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    if state == READY && watched {
                        self.atomic(SKIPPED).fetch_add(1, Ordering::Relaxed);
                    }
                    return Some(i);
                }
            }
        }
        if watched {
            self.atomic(SKIPPED).fetch_add(1, Ordering::Relaxed);
        }
        None
    }
    pub fn take_latest(&mut self) -> Result<Option<Frame>> {
        let mut frame = Frame::default();
        Ok(self.take_latest_into(&mut frame)?.then_some(frame))
    }
    /// Copy the newest ready frame into `frame`, reusing its pixel allocation.
    /// Returns false, leaving `frame` untouched, when no new frame is ready.
    /// Every call counts as a poll, which keeps the writer publishing.
    pub fn take_latest_into(&mut self, frame: &mut Frame) -> Result<bool> {
        self.atomic(POLLS).fetch_add(1, Ordering::Relaxed);
        let mut newest: Option<(usize, u32)> = None;
        // Claim before inspecting non-atomic metadata. A writer may reclaim READY slots.
        for i in 0..SLOTS {
            let b = self.base(i);
            if self
                .atomic(b)
                .compare_exchange(READY, READING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let seq = self.word(b + 4);
                if newest.is_none_or(|(_, old)| seq.wrapping_sub(old) < u32::MAX / 2) {
                    if let Some((old, _)) = newest {
                        self.atomic(self.base(old)).store(FREE, Ordering::Release);
                    }
                    newest = Some((i, seq));
                } else {
                    self.atomic(b).store(FREE, Ordering::Release);
                }
            }
        }
        let Some((slot, sequence)) = newest else {
            return Ok(false);
        };
        let b = self.base(slot);
        let result = (|| -> Result<()> {
            let length = self.word(b + 28) as usize;
            ensure!(length <= self.capacity, "corrupt ring frame length");
            frame.id = self.quad(b + 8);
            frame.width = self.word(b + 16);
            frame.height = self.word(b + 20);
            frame.pixel_format = self.word(b + 24);
            frame.timestamp_ns = self.quad(b + 32);
            frame.data.clear();
            frame
                .data
                .extend_from_slice(self.map.bytes(b + META, length));
            self.taken = Some(Taken {
                sequence,
                preview: self.word(b + 40) & PREVIEW != 0,
            });
            Ok(())
        })();
        // Release the slot even when the metadata was corrupt.
        self.atomic(b).store(FREE, Ordering::Release);
        result.map(|()| true)
    }
    /// The frame last taken is a downscaled stand-in for a frame larger than
    /// a slot, fit for display only.
    pub fn preview_only(&self) -> bool {
        self.taken.is_some_and(|taken| taken.preview)
    }
    /// The frame last taken is the newest the writer was offered, rather
    /// than a keep-alive with newer frames skipped since.
    pub fn fresh(&self) -> bool {
        let newest = self.sequence();
        match self.taken {
            Some(taken) => newest.wrapping_sub(taken.sequence) as i32 <= 0,
            None => newest == 0,
        }
    }
}

/// A downscaled RGB8 copy of `frame` that fits `capacity`.
fn preview(frame: &Frame, capacity: usize) -> Result<Frame> {
    // Each preview pixel takes three bytes.
    let scale = (f64::from(frame.width) * f64::from(frame.height) * 3.0 / capacity as f64)
        .sqrt()
        .max(1.0);
    let bound = |side: u32| ((f64::from(side) / scale) as u32).max(1);
    let (width, height, rgba) =
        crate::frame::preview_rgba(frame, bound(frame.width), bound(frame.height))?;
    let data: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    ensure!(
        data.len() <= capacity,
        "frame preview exceeds shared-memory capacity ({capacity} bytes)"
    );
    Ok(Frame {
        id: frame.id,
        width,
        height,
        pixel_format: RGB8,
        timestamp_ns: frame.timestamp_ns,
        data,
    })
}

/// A ring file must fit on its disk or tmpfs: a frame touching a page the
/// filesystem cannot back kills the worker with SIGBUS instead of an error.
fn enough_space(dir: &Path, free: u64, length: u64) -> Result<()> {
    if free < length {
        return Err(storage_full(dir, free, length));
    }
    Ok(())
}
fn storage_full(dir: &Path, free: u64, length: u64) -> anyhow::Error {
    anyhow::anyhow!(
        "storage full: {} has {} free, and a camera frame ring needs {}; free space or lower CAPTUREFAB_FRAME_BYTES",
        dir.display(),
        crate::analytics::human_bytes(free),
        crate::analytics::human_bytes(length)
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}

/// Remove ring files in `dir` whose creator died without removing them. A
/// live creator holds its file locked (on Windows, open without delete sharing).
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let ring = name
            .to_str()
            .is_some_and(|name| name.starts_with("worker-") && name.ends_with(".shm"));
        if !ring || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        #[cfg(unix)]
        {
            let Ok(file) = File::open(entry.path()) else {
                continue;
            };
            if fs2::FileExt::try_lock_exclusive(&file).is_err() {
                continue;
            }
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Where a ring created now would live: `memfd` (Linux), `posix-shm` (macOS),
/// or `file` in the session directory.
pub fn backing() -> &'static str {
    #[cfg(unix)]
    if std::env::var_os("CAPTUREFAB_RING_FILE_TEST").is_none() && memory(4096).is_ok() {
        return if cfg!(target_os = "macos") {
            "posix-shm"
        } else {
            "memfd"
        };
    }
    "file"
}

#[cfg(target_os = "linux")]
fn memory(length: usize) -> std::io::Result<std::os::fd::OwnedFd> {
    // SAFETY: a static C string and flags; the result is checked.
    let fd = unsafe { libc::memfd_create(c"capturefab-ring".as_ptr(), libc::MFD_CLOEXEC) };
    sized(fd, length)
}

#[cfg(target_os = "macos")]
fn memory(length: usize) -> std::io::Result<std::os::fd::OwnedFd> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|e| std::io::Error::other(e.to_string()))?;
    // 28 bytes, within macOS's 31-byte limit for shared memory names.
    let name = std::ffi::CString::new(format!("/capturefab-{}", hex(&random)))?;
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL;
    // SAFETY: a valid C string; the descriptor is checked below.
    let fd = unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint) };
    if fd >= 0 {
        // Nameless from here on: only descriptors and mappings keep it.
        // SAFETY: the same valid C string.
        unsafe { libc::shm_unlink(name.as_ptr()) };
    }
    sized(fd, length)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn memory(_: usize) -> std::io::Result<std::os::fd::OwnedFd> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sized(fd: libc::c_int, length: usize) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a new descriptor that nothing else owns.
    let mut fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Children inherit it only through `pass_to`.
    // SAFETY: sets a flag on a valid descriptor; the result is checked.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if fd.as_raw_fd() <= 2 {
        // A child's stdio replaces descriptors 0-2 before it could inherit one.
        // SAFETY: duplicates a valid descriptor; the result is checked.
        let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if moved < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the new duplicate is owned here alone.
        fd = unsafe { OwnedFd::from_raw_fd(moved) };
    }
    let file = File::from(fd);
    file.set_len(length as u64)?;
    Ok(file.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn frame(id: u64) -> Frame {
        Frame {
            id,
            width: 8,
            height: 8,
            pixel_format: crate::types::MONO8,
            timestamp_ns: id * 10,
            data: vec![id as u8; 64],
        }
    }
    #[test]
    fn mapped_ring_roundtrip_and_bounds() {
        let dir = temp_dir("ring-test");
        let mut writer = SharedRing::create_file(&dir, 4096).unwrap();
        let path = writer.worker_arg().unwrap();
        let mut reader = SharedRing::open(&path).unwrap();
        // A reader that polls sees every frame; the ones it missed count.
        assert!(reader.take_latest().unwrap().is_none());
        for id in 1..=5 {
            assert_eq!(writer.write(&frame(id), false).unwrap(), Published::Frame);
        }
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 5);
        assert!(reader.take_latest().unwrap().is_none());
        assert!(writer.preview_skipped() > 0);
        assert!(reader.fresh() && !reader.preview_only());
        drop(reader);
        drop(writer);
        assert!(!Path::new(&path).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn take_latest_into_reuses_the_callers_allocation() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        let mut frame = Frame {
            data: Vec::with_capacity(64),
            ..Frame::default()
        };
        let buffer = frame.data.as_ptr();
        assert!(!reader.take_latest_into(&mut frame).unwrap());
        assert_eq!((frame.id, frame.data.len()), (0, 0));
        for id in 1..=3 {
            let sent = Frame {
                id,
                width: 8,
                height: 4,
                pixel_format: crate::types::MONO8,
                timestamp_ns: id * 10,
                data: vec![id as u8; 32],
            };
            assert_eq!(writer.write(&sent, false).unwrap(), Published::Frame);
            assert!(reader.take_latest_into(&mut frame).unwrap());
            assert_eq!(
                (frame.id, frame.width, frame.height, frame.timestamp_ns),
                (id, 8, 4, id * 10)
            );
            assert_eq!(frame.data, sent.data);
            assert_eq!(frame.data.as_ptr(), buffer);
        }
        assert!(!reader.take_latest_into(&mut frame).unwrap());
        assert_eq!(frame.id, 3);
    }
    #[test]
    fn unwatched_rings_skip_copies_but_keep_counting() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        let results: Vec<_> = (1..=5)
            .map(|id| writer.write(&frame(id), false).unwrap())
            .collect();
        // The first frame is a keep-alive; the rest are not copied at all.
        assert_eq!(results[0], Published::Frame);
        assert!(results[1..].iter().all(|r| *r == Published::Skipped));
        assert_eq!((writer.sequence(), writer.counters().sequence()), (5, 5));
        // Skipping for nobody is not a loss.
        assert_eq!(writer.preview_skipped(), 0);
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 1);
        assert!(!reader.fresh(), "newer frames went unpublished");
        // That take re-armed the writer: the next frame is copied and fresh.
        assert_eq!(writer.write(&frame(6), false).unwrap(), Published::Frame);
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 6);
        assert!(reader.fresh());
    }
    #[test]
    fn idle_readers_get_one_keep_alive_a_second() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        assert!(reader.take_latest().unwrap().is_none());
        writer.write(&frame(1), false).unwrap();
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 1);
        // That was the reader's last poll, READER_IDLE ago.
        writer.watched(Instant::now());
        writer.polled_at = Some(Instant::now() - READER_IDLE);
        assert_eq!(writer.write(&frame(2), false).unwrap(), Published::Skipped);
        writer.published_at = Some(Instant::now() - KEEP_ALIVE);
        assert_eq!(writer.write(&frame(3), false).unwrap(), Published::Frame);
        assert_eq!(writer.write(&frame(4), false).unwrap(), Published::Skipped);
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 3);
        assert_eq!(writer.preview_skipped(), 0);
    }
    #[test]
    fn captures_are_published_even_unwatched() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        writer.write(&frame(1), false).unwrap();
        assert_eq!(writer.write(&frame(2), false).unwrap(), Published::Skipped);
        assert_eq!(writer.write(&frame(3), true).unwrap(), Published::Frame);
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 3);
        assert!(reader.fresh());
    }
    #[test]
    fn oversize_frames_publish_a_flagged_preview() {
        let mut writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let mut reader = writer.reader().unwrap();
        let large = Frame {
            id: 7,
            width: 80,
            height: 80,
            pixel_format: crate::types::MONO8,
            timestamp_ns: 70,
            data: (0..6400).map(|i| i as u8).collect(),
        };
        assert_eq!(writer.write(&large, true).unwrap(), Published::Preview);
        let preview = reader.take_latest().unwrap().unwrap();
        assert!(reader.preview_only());
        assert_eq!((preview.id, preview.timestamp_ns), (7, 70));
        assert_eq!(preview.pixel_format, RGB8);
        assert!(preview.width < 80 && preview.height < 80);
        assert_eq!(
            preview.data.len(),
            (preview.width * preview.height * 3) as usize
        );
        // The next frame that fits is a real one again.
        writer.write(&frame(8), true).unwrap();
        reader.take_latest().unwrap().unwrap();
        assert!(!reader.preview_only());
    }
    #[test]
    #[cfg(unix)]
    fn workers_open_inherited_descriptors() {
        let writer = SharedRing::create(&std::env::temp_dir(), 4096).unwrap();
        let arg = writer.worker_arg().unwrap();
        if backing() == "file" {
            return;
        }
        assert!(arg.to_str().unwrap().starts_with("fd:"));
        let mut writer = writer;
        let mut reader = writer.reader().unwrap();
        assert!(reader.take_latest().unwrap().is_none());
        writer.write(&frame(1), false).unwrap();
        assert_eq!(reader.take_latest().unwrap().unwrap().id, 1);
        for bad in ["fd:0", "fd:2", "fd:-1", "fd:x", "fd:999999"] {
            assert!(SharedRing::open(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn sweep_removes_dead_ring_files_and_keeps_live_ones() {
        let dir = temp_dir("ring-sweep");
        let live = SharedRing::create_file(&dir, 4096).unwrap();
        let live_path = PathBuf::from(live.worker_arg().unwrap());
        let dead = dir.join("worker-0123456789abcdef.shm");
        std::fs::write(&dead, b"left by a crash").unwrap();
        let other = dir.join("worker-note.json");
        std::fs::write(&other, b"{}").unwrap();
        sweep(&dir);
        assert!(!dead.exists());
        assert!(live_path.exists() && other.exists());
        drop(live);
        assert!(!live_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn ring_files_reserve_their_blocks_except_on_tmpfs() {
        use std::os::unix::fs::MetadataExt;
        let allocated = |dir: &Path| {
            let ring = SharedRing::create_file(dir, 1 << 20).unwrap();
            let path = PathBuf::from(ring.worker_arg().unwrap());
            let meta = std::fs::metadata(path).unwrap();
            assert_eq!(meta.len(), ring_bytes(1 << 20) as u64);
            meta.blocks() * 512
        };
        let dir = temp_dir("ring-reserve");
        assert!(allocated(&dir) >= ring_bytes(1 << 20) as u64);
        std::fs::remove_dir_all(&dir).unwrap();
        // Only the header page is touched in a ring on tmpfs.
        #[cfg(target_os = "linux")]
        {
            let dir = Path::new("/dev/shm").join(dir.file_name().unwrap());
            if std::fs::create_dir(&dir).is_ok() {
                assert!(allocated(&dir) < 1 << 20);
                std::fs::remove_dir_all(dir).unwrap();
            }
        }
    }
    #[test]
    fn a_full_disk_fails_with_storage_full() {
        let error = enough_space(Path::new("/sessions"), 1024, 50_331_904).unwrap_err();
        assert!(error.to_string().starts_with("storage full: "), "{error}");
        assert_eq!(crate::cli::error_code(&error), ("storage_full", 6));
        enough_space(Path::new("/sessions"), 50_331_904, 50_331_904).unwrap();
    }
}
