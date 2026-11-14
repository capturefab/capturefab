//! Several GVSP datagrams received in one system call (Linux `recvmmsg`),
//! each payload packet straight into its predicted place in its frame.
//!
//! A receive of one datagram costs one system call, which dominates
//! receiving a frame once payloads land in place: a 12 MP frame is about
//! 8,400 packets at a 1500-byte MTU. A batch predicts the next payload
//! packets of the newest frame, one per message, each split into its
//! header, its place in the frame's buffer and the rest of the datagram;
//! messages past the frame's next packets are received whole.
//!
//! The kernel fills messages in arrival order, so a packet can land where
//! another was expected, and one copied into its own place could then
//! overwrite a later message of the same batch that landed there. So every
//! message that did not land as predicted is copied out before any message
//! is taken in, and the messages are then taken in in the order they came.
//! A landed message whose frame changed meanwhile (completed, dropped or
//! given a new buffer by an earlier one) is dropped: its packet is lost,
//! and asked for again like any other.
use super::{Assembler, Frame, Result, Site, Slot};
use std::time::Instant;

/// The most datagrams one call receives.
pub(super) const BATCH: usize = 32;

/// How one message of a batch arrived.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Received {
    /// Its datagram's length, and whether it was cut to fit.
    pub(super) len: usize,
    pub(super) truncated: bool,
    /// It came from the camera's stream, not from anywhere else.
    pub(super) from_camera: bool,
}

/// A batch's buffers: for each message a header place, the rest of its
/// datagram (all of it when no slot was predicted for it), and a place to
/// put one that did not land back together.
pub(super) struct Batch {
    pub(super) headers: Vec<[u8; 20]>,
    pub(super) rests: Vec<Vec<u8>>,
    scratch: Vec<Vec<u8>>,
}

/// What became of a message in the pass before any is taken in.
enum Fate {
    /// From elsewhere: it only overwrote a place still empty.
    Stray,
    /// Landed in its slot as predicted.
    Landed(super::GvspPacket<'static>),
    /// Copied out whole into its scratch.
    Moved,
    /// Received whole into its rest, this long.
    Whole(usize),
}

impl Batch {
    /// Buffers for `BATCH` messages of datagrams up to `datagram` bytes.
    pub(super) fn new(datagram: usize) -> Self {
        Self {
            headers: vec![[0; 20]; BATCH],
            rests: vec![vec![0; datagram]; BATCH],
            scratch: vec![Vec::new(); BATCH],
        }
    }

    /// Take in the messages `received`, of which the first `slots.len()`
    /// were scattered as `slots` predict into the frame whose buffer starts
    /// at `base`, the others whole. What one receive of a single datagram
    /// would return: a frame completed (later ones wait in `ready`), else
    /// the first error, else nothing.
    pub(super) fn take(
        &mut self,
        assembler: &mut Assembler,
        (slots, base): (&[Slot], *const u8),
        received: &[Received],
        site: Site,
        now: Instant,
    ) -> Option<Result<Option<Frame>>> {
        // Copy out every message that did not land, before any is taken in.
        let mut fates: [_; BATCH] = std::array::from_fn(|_| Fate::Stray);
        for (index, message) in received.iter().enumerate() {
            let fate = match slots.get(index) {
                _ if !message.from_camera || message.truncated => Fate::Stray,
                Some(slot) => {
                    let header = &self.headers[index][..slot.header];
                    if let Some(packet) = Assembler::lands(slot, (header, message.len, false)) {
                        Fate::Landed(packet)
                    } else {
                        let scratch = &mut self.scratch[index];
                        scratch.clear();
                        scratch.extend_from_slice(&header[..message.len.min(slot.header)]);
                        let rest = message.len.saturating_sub(slot.header);
                        let in_slot = rest.min(slot.len);
                        if let Some(place) = assembler.slot_mut(slot) {
                            scratch.extend_from_slice(&place[..in_slot]);
                        }
                        let beyond = (rest - in_slot).min(self.rests[index].len());
                        scratch.extend_from_slice(&self.rests[index][..beyond]);
                        crate::copies::record(Site::GvspFallback, scratch.len());
                        Fate::Moved
                    }
                }
                None => Fate::Whole(message.len.min(self.rests[index].len())),
            };
            if !matches!(fate, Fate::Stray) {
                crate::copies::record(Site::GvspReceive, message.len);
            }
            fates[index] = fate;
        }
        // Then take them in, in the order they came.
        let mut completed = Vec::new();
        let mut error = None;
        let mut taken = false;
        for (index, fate) in fates.into_iter().take(received.len()).enumerate() {
            let result = match fate {
                Fate::Stray => continue,
                Fate::Landed(packet) => {
                    if !assembler.still_empty(&slots[index], base) {
                        continue;
                    }
                    assembler.take(packet, now)
                }
                Fate::Moved => {
                    assembler.push_counted(&self.scratch[index], now, Site::GvspFallback)
                }
                Fate::Whole(len) => assembler.push_counted(&self.rests[index][..len], now, site),
            };
            taken = true;
            match result {
                Ok(Some(frame)) => completed.push(frame),
                Ok(None) => {}
                Err(failure) => {
                    error.get_or_insert(failure);
                }
            }
        }
        // Frames come out oldest first: the first is returned, the others
        // go back in front of those still waiting.
        let mut completed = completed.into_iter();
        let first = completed.next();
        for frame in completed.rev() {
            assembler.ready.push_front(frame);
        }
        match (first, error) {
            (Some(frame), _) => Some(Ok(Some(frame))),
            (None, Some(error)) => Some(Err(error)),
            (None, None) => taken.then_some(Ok(None)),
        }
    }
}

/// How many datagrams to receive at once with `slots` predicted: with
/// payloads received in place, the frame's predicted packets and one more
/// (its trailer), else one (a leader, after which the next can be
/// predicted), so a new frame's packets are not received whole in the
/// same call; copying every packet, as many as come.
#[cfg(target_os = "linux")]
pub(super) fn wanted(scatter: bool, slots: &[Slot]) -> usize {
    match (scatter, slots.len()) {
        (false, _) => BATCH,
        (true, 0) => 1,
        (true, predicted) => (predicted + 1).min(BATCH),
    }
}

/// Receive up to `count` datagrams from `socket` into `batch`: the first
/// `slots.len()` scattered into their header, their slot of the frame
/// whose buffer starts at `base` and their rest, the others whole. Blocks
/// for the first as the socket's timeout allows, then takes what has
/// arrived. Fill the first returned-count entries of `received` with how
/// each message arrived, from `camera` (not its control port `gvcp`) or elsewhere.
#[cfg(target_os = "linux")]
pub(super) fn receive(
    socket: &std::net::UdpSocket,
    batch: &mut Batch,
    (slots, base, count): (&[Slot], *mut u8, usize),
    (camera, gvcp): (std::net::IpAddr, std::net::SocketAddr),
    received: &mut [Received; BATCH],
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd as _;
    let iovec = |pointer: *mut u8, len: usize| libc::iovec {
        iov_base: pointer.cast(),
        iov_len: len,
    };
    let mut iovecs: [_; BATCH] = std::array::from_fn(|index| {
        let header = &mut batch.headers[index];
        let rest = &mut batch.rests[index];
        match slots.get(index) {
            // SAFETY: each slot lies inside the frame's buffer, which starts
            // at `base` and is not touched until the call returns, and the
            // slots of one batch never overlap.
            Some(slot) => [
                iovec(header.as_mut_ptr(), slot.header),
                iovec(unsafe { base.add(slot.offset) }, slot.len),
                iovec(rest.as_mut_ptr(), rest.len()),
            ],
            None => [
                iovec(rest.as_mut_ptr(), rest.len()),
                iovec(std::ptr::null_mut(), 0),
                iovec(std::ptr::null_mut(), 0),
            ],
        }
    });
    // SAFETY: an all-zero sockaddr_storage and mmsghdr are valid values.
    let mut names: [libc::sockaddr_storage; BATCH] = unsafe { std::mem::zeroed() };
    let mut messages: [_; BATCH] = std::array::from_fn(|index| {
        let iovecs = &mut iovecs[index];
        let name = &mut names[index];
        // SAFETY: as above.
        let mut message: libc::mmsghdr = unsafe { std::mem::zeroed() };
        message.msg_hdr.msg_name = std::ptr::from_mut(name).cast();
        message.msg_hdr.msg_namelen =
            libc::socklen_t::try_from(size_of::<libc::sockaddr_storage>())
                .expect("sockaddr_storage fits socklen_t");
        message.msg_hdr.msg_iov = iovecs.as_mut_ptr();
        message.msg_hdr.msg_iovlen = if index < slots.len() { 3 } else { 1 };
        message
    });
    // musl declares the flags unsigned; GNU libc declares them signed.
    #[cfg(target_env = "musl")]
    let flags = u32::try_from(libc::MSG_WAITFORONE).map_err(std::io::Error::other)?;
    #[cfg(not(target_env = "musl"))]
    let flags = libc::MSG_WAITFORONE;
    let count = u32::try_from(count.clamp(1, BATCH)).map_err(std::io::Error::other)?;
    // SAFETY: every message points at buffers that live, unmoved, until the
    // call returns, each iovec within its buffer; the kernel writes at most
    // their lengths and the sockaddr_storage it is given.
    let count = unsafe {
        libc::recvmmsg(
            socket.as_raw_fd(),
            messages.as_mut_ptr(),
            count,
            flags,
            std::ptr::null_mut(),
        )
    };
    let count = usize::try_from(count).map_err(|_| std::io::Error::last_os_error())?;
    for ((message, name), received) in messages[..count].iter().zip(&names).zip(received) {
        let source = socket_address(name);
        *received = Received {
            len: message.msg_len as usize,
            truncated: message.msg_hdr.msg_flags & libc::MSG_TRUNC != 0,
            from_camera: source.is_some_and(|source| source.ip() == camera && source != gvcp),
        };
    }
    Ok(count)
}

/// The IPv4 or IPv6 address the kernel wrote.
#[cfg(target_os = "linux")]
fn socket_address(name: &libc::sockaddr_storage) -> Option<std::net::SocketAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    match i32::from(name.ss_family) {
        libc::AF_INET => {
            // SAFETY: the kernel wrote a sockaddr_in, which fits.
            let address = unsafe { &*std::ptr::from_ref(name).cast::<libc::sockaddr_in>() };
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
                u16::from_be(address.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: the kernel wrote a sockaddr_in6, which fits.
            let address = unsafe { &*std::ptr::from_ref(name).cast::<libc::sockaddr_in6>() };
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(address.sin6_addr.s6_addr),
                u16::from_be(address.sin6_port),
                address.sin6_flowinfo,
                address.sin6_scope_id,
            )))
        }
        _ => None,
    }
}
