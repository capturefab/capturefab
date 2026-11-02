//! Frame buffers that are shared rather than copied, and recycled rather
//! than allocated for every frame.
//!
//! A camera's receive loop takes a buffer from the process's [`pool`],
//! writes the frame into it and freezes it into a [`FrameBuf`]: a
//! reference-counted, read-only buffer. Cloning a `Frame` then costs a
//! reference, so the saver, the inspector and the recording's writer all
//! read the one buffer the camera filled, and it goes back to the pool from
//! whichever thread drops it last. Nothing writes to a frozen buffer.
//!
//! The pool bounds idle memory, not live frames: it keeps at most
//! [`MAX_IDLE`] buffers, holding no more than 1/[`IDLE_SHARE`] of the memory
//! this process may still use, and a miss allocates. It never blocks a
//! receive loop. Live frames stay bounded by their consumers' queues.
//!
//! Recycled buffers keep their old bytes ("stale"): only a producer that
//! writes every byte it delivers may take one with [`FramePool::take_stale`].
//!
//! The images frames are converted into for an encoder (a Bayer frame's
//! Y'CbCr planes for a JPEG) come from a second pool, [`transforms`], so
//! they never push a camera's frames out of this one.
//!
//! While a GPU JPEG engine is open in this process, its helper resting
//! included, both pools hand out shared memory segments ([`shared`])
//! instead of heap vectors, so the helper reads a frame where the camera
//! wrote it ([`share`]).
#[cfg(jpeg_helper)]
pub(crate) mod shared;

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

/// The process's pool for the images frames are converted into for an
/// encoder that cannot read them as they are (the RGB image of a Bayer
/// frame a JPEG is compressed from). It is not the frame pool, so encoding
/// never pushes a camera's buffers out of that, and it keeps one buffer
/// idle for each encoder thread, within the same share of memory.
pub fn transforms() -> &'static FramePool {
    static POOL: OnceLock<FramePool> = OnceLock::new();
    POOL.get_or_init(|| {
        FramePool::with_idle(crate::host::threads(usize::MAX), crate::host::memory_budget)
    })
}

/// Frame-sized buffers waiting to be used again.
pub struct FramePool {
    shared: Arc<Shared>,
}

struct Shared {
    idle: Mutex<Vec<Store>>,
    /// The most buffers kept idle.
    max_idle: usize,
    /// The memory the process may still use, when known.
    budget: Box<dyn Fn() -> Option<u64> + Send + Sync>,
    allocations: AtomicU64,
    reuses: AtomicU64,
    freed: AtomicU64,
    /// How many helpers read this pool's buffers: while any do, new
    /// buffers are shared memory.
    #[cfg(jpeg_helper)]
    sharers: std::sync::atomic::AtomicUsize,
}

/// The memory behind a buffer.
enum Store {
    /// Every byte of the vector is initialized; its capacity may be larger.
    Heap(Vec<u8>),
    /// Shared memory a helper process can map: whole pages, every byte
    /// initialized.
    #[cfg(jpeg_helper)]
    Segment(Arc<shared::Segment>),
}

impl Store {
    fn capacity(&self) -> usize {
        match self {
            Store::Heap(vec) => vec.capacity(),
            #[cfg(jpeg_helper)]
            Store::Segment(segment) => segment.len(),
        }
    }
    /// What holding `len` bytes takes of this kind of memory: whole pages
    /// for a segment.
    fn rounded(&self, len: usize) -> usize {
        match self {
            Store::Heap(_) => len,
            #[cfg(jpeg_helper)]
            Store::Segment(_) => len.next_multiple_of(shared::page()),
        }
    }
    fn is_heap(&self) -> bool {
        match self {
            Store::Heap(_) => true,
            #[cfg(jpeg_helper)]
            Store::Segment(_) => false,
        }
    }
    fn fits(&self, want: Want) -> bool {
        match want {
            Want::Any => true,
            Want::Heap => self.is_heap(),
            #[cfg(jpeg_helper)]
            Want::Segment => !self.is_heap(),
        }
    }
}

/// Which idle buffers a request may take.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Any,
    /// A vector, to hand to an API that takes one.
    Heap,
    /// Shared memory, for a helper to read.
    #[cfg(jpeg_helper)]
    Segment,
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
        Self::with_idle(MAX_IDLE, budget)
    }
    /// A pool that keeps up to `max_idle` buffers idle.
    pub(crate) fn with_idle(
        max_idle: usize,
        budget: impl Fn() -> Option<u64> + Send + Sync + 'static,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                idle: Mutex::new(Vec::new()),
                max_idle: max_idle.max(1),
                budget: Box::new(budget),
                allocations: AtomicU64::new(0),
                reuses: AtomicU64::new(0),
                freed: AtomicU64::new(0),
                #[cfg(jpeg_helper)]
                sharers: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }
    /// A buffer of `len` bytes whose contents are whatever it last held: the
    /// caller must write every byte it hands on. A new one is zeroed by the
    /// allocator, which costs nothing until its pages are touched.
    pub fn take_stale(&self, len: usize) -> PoolVec {
        let store = self.shared_store(len).unwrap_or_else(|| {
            self.reuse(len, Want::Any).unwrap_or_else(|| {
                self.shared.allocations.fetch_add(1, Ordering::Relaxed);
                Store::Heap(vec![0; len])
            })
        });
        let mut buffer = self.holding(store);
        buffer.reserve_initialized(len);
        buffer.len = len;
        #[cfg(test)]
        buffer.fill(tests::POISON);
        buffer
    }
    /// An empty buffer that can grow to `capacity` bytes without moving, for
    /// a producer that appends.
    // USB3 Vision assembles frames by appending and hands transfers
    // their buffers.
    #[cfg_attr(not(feature = "usb"), allow(dead_code))]
    pub(crate) fn take_empty(&self, capacity: usize) -> PoolVec {
        let store = self.shared_store(capacity).unwrap_or_else(|| {
            self.reuse(capacity, Want::Any).unwrap_or_else(|| {
                self.shared.allocations.fetch_add(1, Ordering::Relaxed);
                Store::Heap(Vec::with_capacity(capacity))
            })
        });
        self.holding(store)
    }
    /// A vector of `len` stale bytes for an API that takes ownership of one
    /// and gives it back (`adopt`): a USB transfer. Always heap memory.
    // USB3 Vision hands transfers the buffers of whole frames.
    #[cfg_attr(not(feature = "usb"), allow(dead_code))]
    pub(crate) fn take_vec(&self, len: usize) -> Vec<u8> {
        let mut vec = match self.reuse(len, Want::Heap) {
            Some(Store::Heap(vec)) => vec,
            _ => {
                self.shared.allocations.fetch_add(1, Ordering::Relaxed);
                vec![0; len]
            }
        };
        if vec.len() < len {
            vec.resize(len, 0);
        }
        vec.truncate(len);
        #[cfg(test)]
        vec.fill(tests::POISON);
        vec
    }
    /// A buffer of `len` stale bytes in shared memory, for a helper process
    /// to read or write, whether or not the pool shares its frames; None
    /// when shared memory cannot be had. Its callers overwrite what they
    /// copy into it, or keep only what the helper wrote, so tests do not
    /// poison it.
    #[cfg(jpeg_helper)]
    pub(crate) fn take_shared(&self, len: usize) -> Option<PoolVec> {
        let store = match self.reuse(len, Want::Segment) {
            Some(store) => store,
            None => {
                let segment = shared::Segment::create(len).ok()?;
                self.shared.allocations.fetch_add(1, Ordering::Relaxed);
                Store::Segment(Arc::new(segment))
            }
        };
        let mut buffer = self.holding(store);
        buffer.len = len;
        Some(buffer)
    }
    /// `vec` as a buffer of this pool, which it goes back to when dropped.
    // USB3 Vision hands transfers the buffers of whole frames.
    #[cfg_attr(not(feature = "usb"), allow(dead_code))]
    pub(crate) fn adopt(&self, vec: Vec<u8>) -> PoolVec {
        PoolVec {
            len: vec.len(),
            store: Store::Heap(vec),
            home: Some(self.shared.clone()),
        }
    }
    /// From now until the guard is dropped, a helper process reads this
    /// pool's buffers: new ones are shared memory.
    #[cfg(jpeg_helper)]
    pub(crate) fn share(&self) -> Sharing {
        self.shared.sharers.fetch_add(1, Ordering::AcqRel);
        Sharing(self.shared.clone())
    }
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            allocations: self.shared.allocations.load(Ordering::Relaxed),
            reuses: self.shared.reuses.load(Ordering::Relaxed),
            freed: self.shared.freed.load(Ordering::Relaxed),
        }
    }
    fn holding(&self, store: Store) -> PoolVec {
        PoolVec {
            store,
            len: 0,
            home: Some(self.shared.clone()),
        }
    }
    /// A shared memory buffer for `len` bytes while helpers read this
    /// pool's buffers: an idle one, else a new one; None when none read
    /// them, or a new one cannot be had (the heap serves then).
    fn shared_store(&self, len: usize) -> Option<Store> {
        #[cfg(jpeg_helper)]
        if self.shared.sharers.load(Ordering::Acquire) > 0 {
            return self.reuse(len, Want::Segment).or_else(|| {
                let segment = shared::Segment::create(len).ok()?;
                self.shared.allocations.fetch_add(1, Ordering::Relaxed);
                Some(Store::Segment(Arc::new(segment)))
            });
        }
        let _ = len;
        None
    }
    /// The idle buffer of the kind wanted that fits `len` most closely, if
    /// one fits without wasting more than it holds.
    fn reuse(&self, len: usize, want: Want) -> Option<Store> {
        let mut idle = self.shared.idle.lock().unwrap_or_else(|e| e.into_inner());
        let best = idle
            .iter()
            .enumerate()
            .filter(|(_, store)| {
                store.fits(want)
                    && store.capacity() >= len
                    && store.capacity() / 2 <= store.rounded(len).max(4096)
            })
            .min_by_key(|(_, store)| store.capacity())
            .map(|(index, _)| index)?;
        self.shared.reuses.fetch_add(1, Ordering::Relaxed);
        Some(idle.swap_remove(best))
    }
}

/// A pool's frames going to a helper process; dropping it ends that.
#[cfg(jpeg_helper)]
pub(crate) struct Sharing(Arc<Shared>);

#[cfg(jpeg_helper)]
impl Drop for Sharing {
    fn drop(&mut self) {
        self.0.sharers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Both of this process's pools share their buffers with a helper process
/// until the guards are dropped.
#[cfg(jpeg_helper)]
pub(crate) fn share() -> [Sharing; 2] {
    [pool().share(), transforms().share()]
}

impl Shared {
    /// Keep `store` for the next frame, within the idle bounds: the oldest
    /// buffers are freed first.
    fn give_back(&self, store: Store) {
        let limit = (self.budget)().map_or(usize::MAX, |budget| {
            usize::try_from(budget / IDLE_SHARE).unwrap_or(usize::MAX)
        });
        if store.capacity() > limit {
            self.freed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut freed = Vec::new();
        {
            let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
            idle.push(store);
            let mut bytes: usize = idle.iter().map(Store::capacity).sum();
            while idle.len() > self.max_idle || bytes > limit {
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

/// A frame buffer being written: a byte vector, or a shared memory segment,
/// that goes back to its pool when dropped. Its bytes past `len` that the
/// buffer already held stay initialized, so taking it again needs no
/// zeroing.
pub struct PoolVec {
    store: Store,
    /// How many bytes are in use.
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
        self.store.capacity()
    }
    /// A helper's mapping of a segment another process wrote: `len` bytes
    /// of it, read as a frame, belonging to no pool.
    #[cfg(jpeg_helper)]
    pub(crate) fn mapped(segment: Arc<shared::Segment>, len: usize) -> Self {
        assert!(len <= segment.len());
        Self {
            store: Store::Segment(segment),
            len,
            home: None,
        }
    }
    /// The shared memory it is in, if it is.
    #[cfg(jpeg_helper)]
    pub(crate) fn segment(&self) -> Option<&Arc<shared::Segment>> {
        match &self.store {
            Store::Segment(segment) => Some(segment),
            Store::Heap(_) => None,
        }
    }
    // USB3 Vision assembles frames by appending and hands transfers
    // their buffers.
    #[cfg_attr(not(feature = "usb"), allow(dead_code))]
    pub(crate) fn extend_from_slice(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        self.reserve(end);
        match &mut self.store {
            Store::Heap(vec) if end <= vec.len() => vec[self.len..end].copy_from_slice(bytes),
            Store::Heap(vec) => {
                vec.truncate(self.len);
                vec.extend_from_slice(bytes);
            }
            #[cfg(jpeg_helper)]
            Store::Segment(segment) => {
                // SAFETY: this buffer has the segment alone while it writes
                // (see `deref`), and `reserve` made room for `end`.
                let room = unsafe { segment.bytes_mut(end) };
                room[self.len..].copy_from_slice(bytes);
            }
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
            self.reserve(len);
            match &mut self.store {
                Store::Heap(vec) => {
                    let held = vec.len().min(len);
                    vec[self.len..held].fill(value);
                    if len > held {
                        vec.resize(len, value);
                    }
                }
                #[cfg(jpeg_helper)]
                Store::Segment(segment) => {
                    // SAFETY: as in `extend_from_slice`.
                    let bytes = unsafe { segment.bytes_mut(len) };
                    bytes[self.len..].fill(value);
                }
            }
        }
        self.len = len;
    }
    /// Make it a frame's read-only buffer.
    pub fn freeze(self) -> FrameBuf {
        FrameBuf(Arc::new(Backing::Pool(self)))
    }
    /// Room for `len` bytes: a segment too small for them is left for a
    /// vector, as a vector that grows moves.
    fn reserve(&mut self, len: usize) {
        #[cfg(jpeg_helper)]
        if matches!(self.store, Store::Segment(_)) && len > self.store.capacity() {
            self.move_to_heap(len);
        }
        let _ = len;
    }
    /// For a heap buffer, make its first `len` bytes initialized; a segment's
    /// pages all are.
    fn reserve_initialized(&mut self, len: usize) {
        match &mut self.store {
            // Bytes past what the buffer ever held are not stale but
            // unwritten; they are zeroed rather than read uninitialized.
            Store::Heap(vec) if vec.len() < len => vec.resize(len, 0),
            _ => {}
        }
    }
    /// Move the bytes in use into a vector that can hold `capacity`, giving
    /// a segment back to its pool.
    #[cfg(jpeg_helper)]
    fn move_to_heap(&mut self, capacity: usize) {
        if let Store::Segment(_) = &self.store {
            let mut vec = Vec::with_capacity(capacity.max(self.len));
            vec.extend_from_slice(self);
            let Store::Segment(segment) = std::mem::replace(&mut self.store, Store::Heap(vec))
            else {
                unreachable!()
            };
            if let Some(home) = &self.home {
                home.give_back(Store::Segment(segment));
            }
        }
    }
}

impl Deref for PoolVec {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.store {
            Store::Heap(vec) => &vec[..self.len],
            // SAFETY: a buffer being written has its segment alone: the
            // pool hands a segment to one buffer at a time, and a helper
            // writes only a segment a request lends it, which the worker
            // holds untouched until the helper answers or has ended. A
            // frozen buffer is only read, here and by the helper.
            #[cfg(jpeg_helper)]
            Store::Segment(segment) => unsafe { segment.bytes(self.len) },
        }
    }
}
impl DerefMut for PoolVec {
    fn deref_mut(&mut self) -> &mut [u8] {
        match &mut self.store {
            Store::Heap(vec) => &mut vec[..self.len],
            // SAFETY: as for `deref`; `&mut self` rules out readers here.
            #[cfg(jpeg_helper)]
            Store::Segment(segment) => unsafe { segment.bytes_mut(self.len) },
        }
    }
}
impl Drop for PoolVec {
    fn drop(&mut self) {
        if let Some(home) = &self.home
            && self.store.capacity() > 0
        {
            home.give_back(std::mem::replace(&mut self.store, Store::Heap(Vec::new())));
        }
    }
}
impl fmt::Debug for PoolVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PoolVec({} bytes)", self.len)
    }
}

/// A frame's bytes: read-only and shared by every clone of the frame. The
/// backing is behind an enum so other kinds of memory can hold frames
/// without their readers knowing; shared memory a helper process maps is a
/// pooled buffer's.
#[derive(Clone)]
pub struct FrameBuf(Arc<Backing>);

enum Backing {
    /// A pooled buffer (heap or shared memory), or a plain vector with no
    /// pool.
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
    /// The shared memory segment the bytes are at the start of, if they
    /// are in one: what a helper process can read without a copy.
    #[cfg(jpeg_helper)]
    pub(crate) fn segment(&self) -> Option<&Arc<shared::Segment>> {
        match &*self.0 {
            Backing::Pool(vec) => vec.segment(),
        }
    }
    /// `into_pool_vec` in place, for tests that change frames they hold to
    /// make bad ones.
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
    /// The buffer itself, to write to: this one when nothing else shares
    /// it, else a copy from the pool, which is counted.
    #[cfg(test)]
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
#[cfg(test)]
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
            store: Store::Heap(vec),
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
        let mut vec = pool.take_vec(32);
        let address = vec.as_ptr();
        assert_eq!(vec.len(), 32);
        assert!(vec.iter().all(|&byte| byte == POISON));
        vec.truncate(10);
        let back = pool.adopt(vec);
        assert_eq!((back.len(), back.as_ptr()), (10, address));
        drop(back);
        assert_eq!(pool.take_stale(20).as_ptr(), address);
        // Handed out again as a vector.
        let again = pool.take_vec(30);
        assert_eq!(again.as_ptr(), address);
    }

    /// While a helper reads a pool's buffers they are shared memory, reused
    /// as such; a vector is still a vector, and growing a segment past its
    /// pages moves its bytes to the heap and gives it back.
    #[cfg(jpeg_helper)]
    #[test]
    fn sharing_pools_hand_out_segments() {
        let pool = pool(None);
        assert!(pool.take_stale(5000).segment().is_none());
        let sharing = pool.share();
        let mut buffer = pool.take_stale(5000);
        let segment = buffer.segment().expect("shared memory").clone();
        assert!(buffer.capacity() >= 5000);
        buffer.fill(3);
        let frame = buffer.freeze();
        assert_eq!(frame.segment().map(|s| s.id()), Some(segment.id()));
        assert_eq!(frame.as_ptr(), segment.as_ptr());
        drop(frame);
        let again = pool.take_stale(4000);
        assert_eq!(again.segment().map(|s| s.id()), Some(segment.id()));
        drop(again);
        assert!(pool.take_vec(100).len() == 100);
        let mut appended = pool.take_empty(shared::page());
        let room = appended.capacity();
        appended.extend_from_slice(&vec![1; room]);
        assert!(appended.segment().is_some());
        appended.extend_from_slice(&[2; 10]);
        assert!(appended.segment().is_none(), "moved to the heap");
        assert_eq!(
            (appended.len(), appended[room - 1], appended[room]),
            (room + 10, 1, 2)
        );
        let mut resized = pool.take_stale(10);
        resized.resize(20, 7);
        assert_eq!(&resized[10..], &[7; 10]);
        drop(sharing);
        drop((appended, resized));
        // Without a helper, new buffers are heap again; idle segments are
        // still used.
        assert!(pool.take_stale(1 << 20).segment().is_none());
        // Shared memory on request, whether or not the pool shares.
        let forced = pool.take_shared(100).unwrap();
        assert!(forced.segment().is_some() && forced.len() == 100);
    }
}
