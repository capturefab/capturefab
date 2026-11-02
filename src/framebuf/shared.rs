//! Frame memory another process can map: a page-aligned segment in a memfd
//! (Linux) or an unlinked POSIX shared memory object (other Unix systems),
//! mapped once by this process and once by a GPU JPEG helper, which reads a
//! frame where the camera wrote it and writes its JPEG where the worker
//! reads it (`jpeg/helper.rs`). Each segment is one buffer of a frame pool;
//! its descriptor travels to the helper once, over the helper's control
//! socket, and its id names it in every request after that.
#![allow(unsafe_code)]

use std::{
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

/// A shared memory segment mapped read-write into this process.
pub struct Segment {
    id: u64,
    fd: OwnedFd,
    base: NonNull<u8>,
    len: usize,
    /// Made here, and counted against this.
    counted: Option<&'static Budget>,
}

// SAFETY: the mapping is plain shared memory, valid until `drop` unmaps it;
// who may write it and when is the business of the buffer that owns it
// (`PoolVec`, which has it alone while it writes, or a frozen `FrameBuf`,
// which only reads).
unsafe impl Send for Segment {}
// SAFETY: as for Send: a shared reference reads, or hands out raw pointers.
unsafe impl Sync for Segment {}

/// Ids name segments in a helper's requests; they are never reused.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// How many segments a process may make at once. Each holds a descriptor,
/// and a burst queued for saving could otherwise fill the descriptor table
/// that files and sockets need; frames past it are heap memory, copied for
/// a helper (`gpu_upload`).
pub(crate) struct Budget {
    live: AtomicUsize,
    most: usize,
}

impl Budget {
    pub(crate) const fn new(most: usize) -> Self {
        Self {
            live: AtomicUsize::new(0),
            most,
        }
    }
}

/// This process's segments.
static SEGMENTS: Budget = Budget::new(64);

/// The size of a memory page.
pub(crate) fn page() -> usize {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size)
        .ok()
        .filter(|&size| size > 0)
        .unwrap_or(4096)
}

impl Segment {
    /// A new segment of at least `len` bytes, rounded up to whole pages and
    /// zeroed (the kernel's pages are, until written).
    pub(crate) fn create(len: usize) -> io::Result<Self> {
        Self::create_within(&SEGMENTS, len)
    }

    /// `create`, counted against `budget`.
    fn create_within(budget: &'static Budget, len: usize) -> io::Result<Self> {
        if budget.live.fetch_add(1, Ordering::AcqRel) >= budget.most {
            budget.live.fetch_sub(1, Ordering::AcqRel);
            return Err(io::Error::other("too many shared memory segments"));
        }
        let made = (|| {
            let len = len
                .max(1)
                .checked_next_multiple_of(page())
                .ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))?;
            let fd = anonymous()?;
            // SAFETY: a descriptor this process just created and owns.
            if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            seal(&fd);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            Self::map(fd, len, id)
        })();
        match made {
            Ok(mut segment) => {
                segment.counted = Some(budget);
                Ok(segment)
            }
            Err(error) => {
                budget.live.fetch_sub(1, Ordering::AcqRel);
                Err(error)
            }
        }
    }

    /// Map `len` bytes of a segment received from another process, which
    /// named it `id`; it must hold at least that many.
    pub(crate) fn map(fd: OwnedFd, len: usize, id: u64) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an empty segment",
            ));
        }
        // SAFETY: fstat writes a stat into the zeroed buffer it is given.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor and an out-pointer to a local.
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if u64::try_from(stat.st_size).unwrap_or(0) < len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the segment is smaller than its stated length",
            ));
        }
        // SAFETY: a new shared mapping of `len` bytes the file holds; it is
        // unmapped exactly once, by Drop.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = NonNull::new(base.cast()).ok_or_else(io::Error::last_os_error)?;
        Ok(Self {
            id,
            fd,
            base,
            len,
            counted: None,
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    /// Its mapped length: whole pages.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
    pub(crate) fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
    /// The first byte of this process's mapping, page-aligned.
    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }
    /// Its first `len` bytes.
    ///
    /// # Safety
    /// Nothing may write them while the slice lives, in this process or
    /// another.
    pub(crate) unsafe fn bytes(&self, len: usize) -> &[u8] {
        assert!(len <= self.len);
        // SAFETY: within the mapping; the caller rules out writers.
        unsafe { std::slice::from_raw_parts(self.base.as_ptr(), len) }
    }
    /// Its first `len` bytes, to write.
    ///
    /// # Safety
    /// Nothing else may read or write them while the slice lives, in this
    /// process or another.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn bytes_mut(&self, len: usize) -> &mut [u8] {
        assert!(len <= self.len);
        // SAFETY: within the mapping; the caller has it alone.
        unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr(), len) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: the mapping `map` made, unmapped once; the descriptor
        // closes with `fd`.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.len) };
        if let Some(budget) = self.counted {
            budget.live.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Segment({}, {} bytes)", self.id, self.len)
    }
}

/// A new anonymous shared memory file, closed on exec.
#[cfg(target_os = "linux")]
fn anonymous() -> io::Result<OwnedFd> {
    // SAFETY: a NUL-terminated name and valid flags.
    let fd = unsafe {
        libc::memfd_create(
            c"capturefab-frame".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd >= 0 {
        // SAFETY: a new descriptor this process owns.
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let error = io::Error::last_os_error();
    // Kernels before 3.17 lack memfd; /dev/shm serves instead.
    if error.raw_os_error() == Some(libc::ENOSYS) {
        return posix();
    }
    Err(error)
}

#[cfg(not(target_os = "linux"))]
fn anonymous() -> io::Result<OwnedFd> {
    posix()
}

/// A POSIX shared memory object, unlinked at once so only descriptors
/// name it.
fn posix() -> io::Result<OwnedFd> {
    for _ in 0..8 {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        // Short: macOS allows 31 bytes.
        let name = format!(
            "/cf{}-{:016x}",
            std::process::id() % 100_000,
            u64::from_ne_bytes(random)
        );
        let name = std::ffi::CString::new(name).expect("no NUL in the name");
        // SAFETY: a NUL-terminated name, valid flags and a mode.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(error);
        }
        // SAFETY: a new descriptor this process owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the name just created.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        // SAFETY: a valid descriptor; FD_CLOEXEC keeps it from programs
        // this process runs, which are handed descriptors explicitly.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        return Ok(fd);
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

/// Fix a memfd's size, so the helper that maps it can rely on it.
#[cfg(target_os = "linux")]
fn seal(fd: &OwnedFd) {
    // SAFETY: a valid descriptor; failing leaves it unsealed, which only
    // gives up that protection.
    unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_whole_zeroed_pages_two_mappings_share() {
        let segment = Segment::create(10_000).unwrap();
        assert_eq!(segment.len() % page(), 0);
        assert!(segment.len() >= 10_000);
        assert_eq!(segment.as_ptr() as usize % page(), 0);
        // SAFETY: this test alone uses the segment.
        let bytes = unsafe { segment.bytes_mut(segment.len()) };
        assert!(bytes.iter().all(|&byte| byte == 0));
        bytes[..4].copy_from_slice(&[1, 2, 3, 4]);
        // A second mapping, as a helper makes from the descriptor it is
        // sent: another address, the same bytes.
        let fd = segment.fd().try_clone_to_owned().unwrap();
        let other = Segment::map(fd, segment.len(), segment.id()).unwrap();
        assert_ne!(other.as_ptr(), segment.as_ptr());
        // SAFETY: as above.
        assert_eq!(unsafe { other.bytes(4) }, &[1, 2, 3, 4]);
        unsafe { other.bytes_mut(8)[7] = 9 };
        assert_eq!(unsafe { segment.bytes(8) }[7], 9);
        // Ids are new for every segment.
        assert_ne!(Segment::create(1).unwrap().id(), segment.id());
    }

    #[test]
    fn a_stated_length_beyond_the_segment_is_refused() {
        let segment = Segment::create(page()).unwrap();
        let fd = segment.fd().try_clone_to_owned().unwrap();
        assert!(Segment::map(fd, segment.len() + page(), 1).is_err());
        let fd = segment.fd().try_clone_to_owned().unwrap();
        assert!(Segment::map(fd, 0, 1).is_err());
    }

    /// Past its budget, more segments are refused rather than filling the
    /// descriptor table; freeing one makes room.
    #[test]
    fn segments_are_bounded() {
        static BUDGET: Budget = Budget::new(3);
        let held: Vec<_> = (0..3)
            .map(|_| Segment::create_within(&BUDGET, 1).unwrap())
            .collect();
        let refused = Segment::create_within(&BUDGET, 1).unwrap_err();
        assert!(refused.to_string().contains("too many"), "{refused}");
        drop(held);
        let again = Segment::create_within(&BUDGET, 1).unwrap();
        // A mapping of another's is not counted.
        let fd = again.fd().try_clone_to_owned().unwrap();
        let mapped = Segment::map(fd, again.len(), again.id()).unwrap();
        assert_eq!(BUDGET.live.load(Ordering::Acquire), 1);
        drop((again, mapped));
        assert_eq!(BUDGET.live.load(Ordering::Acquire), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memfd_segments_cannot_change_size() {
        let segment = Segment::create(page()).unwrap();
        // SAFETY: a valid descriptor; the seal refuses the change.
        let shrunk = unsafe { libc::ftruncate(segment.fd().as_raw_fd(), 0) };
        assert_ne!(shrunk, 0);
    }
}
