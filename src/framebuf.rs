//! Frame buffers that are shared rather than copied, and recycled rather
//! than allocated for every frame.
//!
//! A camera's receive loop takes a buffer from the process's [`pool`],
//! writes the frame into it and freezes it into a [`FrameBuf`]: a
//! reference-counted, read-only buffer. Cloning a `Frame` then costs a
//! reference, so the saver, the inspector and the recording's writer all
//! read the one buffer the camera filled, and it goes back to the pool from
//! whichever thread drops it last. Writing to a shared frame goes through
//! [`FrameBuf::make_mut`], which copies only when the buffer is shared, and
//! counts that copy.
//!
//! The pool bounds idle memory, not live frames: it keeps at most
//! [`MAX_IDLE`] buffers, holding no more than 1/[`IDLE_SHARE`] of the memory
//! this process may still use, and a miss allocates. It never blocks a
//! receive loop. Live frames stay bounded by their consumers' queues.
//!
//! Recycled buffers keep their old bytes ("stale"): only a producer that
//! writes every byte it delivers may take one with [`FramePool::take_stale`].
use std::{
    fmt,
    ops::{Deref, DerefMut},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

/// The most buffers the pool keeps idle: enough for a camera's next frame
/// while its last one is still being written out.
pub(crate) const MAX_IDLE: usize = 2;
/// Idle buffers hold at most this fraction of the memory the process may
/// still use (`host::memory_budget`), so a small board keeps none of a
/// large camera's frames idle.
pub(crate) const IDLE_SHARE: u64 = 20;

/// The process's pool. Each camera runs in a worker process of its own, so
/// its frames are of one size or two.
pub fn pool() -> &'static FramePool {
    static POOL: OnceLock<FramePool> = OnceLock::new();
    POOL.get_or_init(|| FramePool::new(crate::host::memory_budget))
}

/// Frame-sized buffers waiting to be used again.
pub struct FramePool {
    shared: Arc<Shared>,
}

struct Shared {
    idle: Mutex<Vec<Vec<u8>>>,
    /// The memory the process may still use, when known.
    budget: Box<dyn Fn() -> Option<u64> + Send + Sync>,
    allocations: AtomicU64,
    reuses: AtomicU64,
    freed: AtomicU64,
}

/// What a pool has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Buffers allocated because none idle would do.
    pub allocations: u64,
    /// Buffers taken from those idle.
    pub reuses: u64,
    /// Buffers given back and freed, over the idle bounds.
    pub freed: u64,
}

impl FramePool {
    pub(crate) fn new(budget: impl Fn() -> Option<u64> + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                idle: Mutex::new(Vec::new()),
                budget: Box::new(budget),
                allocations: AtomicU64::new(0),
                reuses: AtomicU64::new(0),
                freed: AtomicU64::new(0),
            }),
        }
    }
    /// A buffer of `len` bytes whose contents are whatever it last held: the
    /// caller must write every byte it hands on. A new one is zeroed by the
    /// allocator, which costs nothing until its pages are touched.
    pub fn take_stale(&self, len: usize) -> PoolVec {
        let mut vec = self.reuse(len).unwrap_or_else(|| {
            self.shared.allocations.fetch_add(1, Ordering::Relaxed);
            vec![0; len]
        });
        // Bytes past what the buffer ever held are not stale but unwritten;
        // they are zeroed rather than read uninitialized.
        if vec.len() < len {
            vec.resize(len, 0);
        }
        let mut buffer = self.adopt(vec);
        buffer.len = len;
        #[cfg(test)]
        buffer.fill(tests::POISON);
        buffer
    }
    /// An empty buffer that can grow to `capacity` bytes without moving, for
    /// a producer that appends.
    pub(crate) fn take_empty(&self, capacity: usize) -> PoolVec {
        let vec = self.reuse(capacity).unwrap_or_else(|| {
            self.shared.allocations.fetch_add(1, Ordering::Relaxed);
            Vec::with_capacity(capacity)
        });
        let mut buffer = self.adopt(vec);
        buffer.len = 0;
        buffer
    }
    /// `vec` as a buffer of this pool, which it goes back to when dropped.
    pub(crate) fn adopt(&self, vec: Vec<u8>) -> PoolVec {
        PoolVec {
            len: vec.len(),
            vec,
            home: Some(self.shared.clone()),
        }
    }
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            allocations: self.shared.allocations.load(Ordering::Relaxed),
            reuses: self.shared.reuses.load(Ordering::Relaxed),
            freed: self.shared.freed.load(Ordering::Relaxed),
        }
    }
    /// The idle buffer that fits `len` most closely, if one fits without
    /// wasting more than it holds.
    fn reuse(&self, len: usize) -> Option<Vec<u8>> {
        let mut idle = self.shared.idle.lock().unwrap_or_else(|e| e.into_inner());
        let best = idle
            .iter()
            .enumerate()
            .filter(|(_, vec)| vec.capacity() >= len && vec.capacity() / 2 <= len.max(4096))
            .min_by_key(|(_, vec)| vec.capacity())
            .map(|(index, _)| index)?;
        self.shared.reuses.fetch_add(1, Ordering::Relaxed);
        Some(idle.swap_remove(best))
    }
}

impl Shared {
    /// Keep `vec` for the next frame, within the idle bounds: the oldest
    /// buffers are freed first.
    fn give_back(&self, vec: Vec<u8>) {
        let limit = (self.budget)().map_or(usize::MAX, |budget| {
            usize::try_from(budget / IDLE_SHARE).unwrap_or(usize::MAX)
        });
        if vec.capacity() > limit {
            self.freed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut freed = Vec::new();
        {
            let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
            idle.push(vec);
            let mut bytes: usize = idle.iter().map(Vec::capacity).sum();
            while idle.len() > MAX_IDLE || bytes > limit {
                let oldest = idle.remove(0);
                bytes -= oldest.capacity();
                freed.push(oldest);
            }
        }
        self.freed.fetch_add(freed.len() as u64, Ordering::Relaxed);
        // Freed outside the lock: unmapping a large buffer takes a while.
        drop(freed);
    }
}

/// A frame buffer being written: a byte vector that goes back to its pool
/// when dropped. Its bytes past `len` that the vector already held stay
/// initialized, so taking it again needs no zeroing.
pub struct PoolVec {
    /// Every byte of `vec` is initialized; `len` is how many are in use.
    vec: Vec<u8>,
    len: usize,
    home: Option<Arc<Shared>>,
}

impl PoolVec {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn capacity(&self) -> usize {
        self.vec.capacity()
    }
    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        if end <= self.vec.len() {
            self.vec[self.len..end].copy_from_slice(bytes);
        } else {
            self.vec.truncate(self.len);
            self.vec.extend_from_slice(bytes);
        }
        self.len = end;
    }
    #[cfg(test)]
    pub(crate) fn pop(&mut self) -> Option<u8> {
        let last = self.last().copied()?;
        self.len -= 1;
        Some(last)
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }
    /// Grow to `len` bytes of `value`, or shrink to `len`.
    pub(crate) fn resize(&mut self, len: usize, value: u8) {
        if len > self.len {
            let held = self.vec.len().min(len);
            self.vec[self.len..held].fill(value);
            if len > held {
                self.vec.resize(len, value);
            }
        }
        self.len = len;
    }
    /// Make it a frame's read-only buffer.
    pub fn freeze(self) -> FrameBuf {
        FrameBuf(Arc::new(Backing::Pool(self)))
    }
    /// The vector itself, at its length, for an API that takes ownership of
    /// it and gives it back (`FramePool::adopt`): a USB transfer.
    pub(crate) fn into_vec(mut self) -> Vec<u8> {
        let mut vec = std::mem::take(&mut self.vec);
        vec.truncate(self.len);
        vec
    }
}

impl Deref for PoolVec {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.vec[..self.len]
    }
}
impl DerefMut for PoolVec {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.vec[..self.len]
    }
}
impl Drop for PoolVec {
    fn drop(&mut self) {
        if let Some(home) = &self.home
            && self.vec.capacity() > 0
        {
            home.give_back(std::mem::take(&mut self.vec));
        }
    }
}
impl fmt::Debug for PoolVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PoolVec({} bytes)", self.len)
    }
}

/// A frame's bytes: read-only and shared by every clone of the frame. The
/// backing is behind an enum so other kinds of memory, such as slots mapped
/// by a helper process, can hold frames without their readers knowing.
#[derive(Clone)]
pub struct FrameBuf(Arc<Backing>);

enum Backing {
    /// A pooled buffer, or a plain vector with no pool.
    Pool(PoolVec),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Backing::Pool(vec) => vec,
        }
    }
}

impl FrameBuf {
    /// The buffer to write to: this one when nothing else shares it, else a
    /// copy from the pool, which is counted. Frames are written only before
    /// they are frozen; tests change frames they hold to make bad ones.
    #[cfg(test)]
    pub(crate) fn make_mut(&mut self) -> &mut PoolVec {
        if Arc::get_mut(&mut self.0).is_none() {
            let copy = owned_copy(self);
            self.0 = Arc::new(Backing::Pool(copy));
        }
        match Arc::get_mut(&mut self.0).expect("unshared after the copy") {
            Backing::Pool(vec) => vec,
        }
    }
    /// The buffer itself, to write to, by the same rule as `make_mut`.
    pub(crate) fn into_pool_vec(self) -> PoolVec {
        match Arc::try_unwrap(self.0) {
            Ok(Backing::Pool(vec)) => vec,
            Err(shared) => owned_copy(&FrameBuf(shared)),
        }
    }
    /// Whether another frame shares these bytes.
    #[cfg(test)]
    pub(crate) fn is_shared(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

/// A copy of a shared buffer's bytes in a pooled buffer of its own.
fn owned_copy(bytes: &[u8]) -> PoolVec {
    crate::copies::record(crate::copies::Site::FrameCow, bytes.len());
    let mut copy = pool().take_empty(bytes.len());
    copy.extend_from_slice(bytes);
    copy
}

impl Default for FrameBuf {
    fn default() -> Self {
        Vec::new().into()
    }
}
impl Deref for FrameBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.bytes()
    }
}
impl AsRef<[u8]> for FrameBuf {
    fn as_ref(&self) -> &[u8] {
        self
    }
}
/// A vector of its own, not from the pool: what tests and decoded stills
/// build.
impl From<Vec<u8>> for FrameBuf {
    fn from(vec: Vec<u8>) -> Self {
        PoolVec {
            len: vec.len(),
            vec,
            home: None,
        }
        .freeze()
    }
}
impl FromIterator<u8> for FrameBuf {
    fn from_iter<I: IntoIterator<Item = u8>>(bytes: I) -> Self {
        bytes.into_iter().collect::<Vec<u8>>().into()
    }
}
impl From<PoolVec> for FrameBuf {
    fn from(vec: PoolVec) -> Self {
        vec.freeze()
    }
}
impl fmt::Debug for FrameBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FrameBuf({} bytes)", self.len())
    }
}
impl PartialEq for FrameBuf {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl PartialEq<[u8]> for FrameBuf {
    fn eq(&self, other: &[u8]) -> bool {
        **self == *other
    }
}
impl PartialEq<&[u8]> for FrameBuf {
    fn eq(&self, other: &&[u8]) -> bool {
        **self == **other
    }
}
impl PartialEq<Vec<u8>> for FrameBuf {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == **other
    }
}
impl PartialEq<FrameBuf> for Vec<u8> {
    fn eq(&self, other: &FrameBuf) -> bool {
        **self == **other
    }
}
impl<const N: usize> PartialEq<[u8; N]> for FrameBuf {
    fn eq(&self, other: &[u8; N]) -> bool {
        **self == *other
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What a recycled buffer holds in tests, where `take_stale` fills
    /// every buffer with it: a producer that leaves a byte unwritten
    /// delivers this instead of the zero a fresh allocation would hold.
    pub(crate) const POISON: u8 = 0xA5;

    fn pool(budget: Option<u64>) -> FramePool {
        FramePool::new(move || budget)
    }

    #[test]
    fn buffers_are_reused_within_the_idle_bound() {
        let pool = pool(None);
        let first = pool.take_stale(100_000);
        let address = first.as_ptr();
        drop(first);
        let again = pool.take_stale(90_000);
        assert_eq!(again.as_ptr(), address);
        assert_eq!(again.len(), 90_000);
        assert_eq!(
            pool.stats(),
            PoolStats {
                allocations: 1,
                reuses: 1,
                freed: 0
            }
        );
        // A much larger buffer is not wasted on a small frame, nor a small
        // one stretched.
        drop(again);
        let small = pool.take_stale(10_000);
        assert_ne!(small.as_ptr(), address);
        let large = pool.take_stale(500_000);
        assert_ne!(large.as_ptr(), address);
        // Only `MAX_IDLE` stay idle; the oldest go first.
        let held: Vec<_> = (0..MAX_IDLE + 2)
            .map(|_| pool.take_stale(100_000))
            .collect();
        drop((small, large));
        drop(held);
        assert_eq!(pool.shared.idle.lock().unwrap().len(), MAX_IDLE);
        assert!(pool.stats().freed >= 2);
    }

    #[test]
    fn idle_bytes_stay_within_a_share_of_free_memory() {
        // 20 MB free: idle buffers may hold 1 MB.
        let pool = pool(Some(20_000_000));
        let (first, second) = (pool.take_stale(700_000), pool.take_stale(600_000));
        drop(first);
        assert_eq!(pool.shared.idle.lock().unwrap().len(), 1);
        drop(second);
        let idle = pool.shared.idle.lock().unwrap();
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].capacity(), 600_000);
        drop(idle);
        // A buffer larger than the share is freed at once.
        drop(pool.take_stale(2_000_000));
        assert_eq!(pool.shared.idle.lock().unwrap().len(), 1);
        assert_eq!(pool.stats().freed, 2);
    }

    #[test]
    fn stale_buffers_keep_their_bytes_and_grow_zeroed() {
        let pool = pool(None);
        let mut buffer = pool.take_stale(16);
        assert!(buffer.iter().all(|&b| b == POISON));
        buffer.truncate(4);
        buffer.extend_from_slice(&[1, 2]);
        assert_eq!(&buffer[..], &[POISON, POISON, POISON, POISON, 1, 2]);
        buffer.resize(8, 7);
        assert_eq!(&buffer[4..], &[1, 2, 7, 7]);
        buffer.resize(20, 9);
        assert_eq!(&buffer[8..], &[9; 12]);
        // Appending past what the vector held grows it.
        let mut empty = pool.take_empty(4);
        assert!(empty.is_empty() && empty.capacity() >= 4);
        empty.extend_from_slice(&[3; 10]);
        assert_eq!(&empty[..], &[3; 10]);
        let frame = empty.freeze();
        assert_eq!(frame, [3; 10]);
        assert_eq!(format!("{frame:?}"), "FrameBuf(10 bytes)");
    }

    #[test]
    fn frames_go_back_from_the_thread_that_drops_them_last() {
        let pool = pool(None);
        let mut buffer = pool.take_stale(64);
        buffer.fill(1);
        let address = buffer.as_ptr();
        let frame = buffer.freeze();
        let shared = frame.clone();
        assert!(frame.is_shared());
        std::thread::spawn(move || drop(shared)).join().unwrap();
        assert!(!frame.is_shared());
        drop(frame);
        assert_eq!(pool.take_stale(64).as_ptr(), address);
    }

    #[test]
    fn writes_copy_only_shared_frames() {
        let mut frame = FrameBuf::from(vec![1, 2, 3]);
        let address = frame.as_ptr();
        frame.make_mut()[0] = 9;
        assert_eq!(
            frame.as_ptr(),
            address,
            "an unshared frame is written in place"
        );
        let copy = frame.clone();
        frame.make_mut()[1] = 8;
        assert_ne!(frame.as_ptr(), address);
        assert_eq!((frame, copy), (vec![9, 8, 3].into(), vec![9, 2, 3].into()));
        let shared = FrameBuf::from(vec![4, 5]);
        let held = shared.clone();
        let mut owned = shared.into_pool_vec();
        owned[0] = 6;
        assert_eq!((&owned[..], &held[..]), (&[6, 5][..], &[4, 5][..]));
        assert_eq!(held.into_pool_vec().len(), 2);
    }

    #[test]
    fn vectors_handed_out_come_back_to_the_pool() {
        let pool = pool(None);
        let buffer = pool.take_stale(32);
        let address = buffer.as_ptr();
        let mut vec = buffer.into_vec();
        assert_eq!(vec.len(), 32);
        vec.truncate(10);
        let back = pool.adopt(vec);
        assert_eq!((back.len(), back.as_ptr()), (10, address));
        drop(back);
        assert_eq!(pool.take_stale(20).as_ptr(), address);
    }
}
