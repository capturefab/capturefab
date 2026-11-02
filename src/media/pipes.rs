//! The pipes frames cross between a camera worker and FFmpeg, on Linux:
//! large enough for a frame to cross in a few wake-ups, and on the way to an
//! encoder, the frame's own pages handed to the pipe (`vmsplice`) instead
//! of copied into it.
//!
//! A pipe holds 64 KiB by default, so a 2 MB frame takes 32 rounds of
//! writer and reader waking each other. `enlarge` asks for up to 1 MiB
//! (`F_SETPIPE_SZ`; an unprivileged process gets at most
//! `/proc/sys/fs/pipe-max-size`, 1 MiB by default), and only for frames that
//! need more than the default, since every enlarged pipe counts against the
//! user's `pipe-user-pages-soft` share.
//!
//! `vmsplice` without `SPLICE_F_GIFT` puts references to the frame's pages
//! in the pipe; the reader's `read` copies from them. The pages must not
//! change until it has, so a [`Splicer`] keeps every frame it sent alive
//! until the bytes in the pipe (`FIONREAD`) say the reader has taken them,
//! and hands back those still in the pipe when it ends, to be dropped only
//! once the reader has exited. A frame's buffer is never written while any
//! clone of the frame lives, and goes back to the pool only after the last
//! one is dropped.

/// The most a pipe is enlarged to.
const MOST: usize = 1 << 20;
/// What a pipe holds unless asked.
const DEFAULT: usize = 64 << 10;

/// Make the pipe `fd` hold a frame of `bytes` in one go where it can, up
/// to 1 MiB; a pipe that cannot grow stays as it was.
pub(super) fn enlarge(fd: std::os::fd::RawFd, bytes: usize) {
    if bytes <= DEFAULT {
        return;
    }
    let size = bytes.next_power_of_two().min(MOST);
    // SAFETY: fcntl on a pipe descriptor this process holds; an error
    // leaves the pipe as it was.
    unsafe {
        libc::fcntl(fd, libc::F_SETPIPE_SZ, size as libc::c_int);
    }
}
/// Sends frames' own bytes into a pipe without copying them, keeping each
/// frame until the reader has read it.
pub(super) struct Splicer {
    fd: std::os::fd::RawFd,
    /// Frames whose pages may still be in the pipe, with the count of
    /// bytes sent once each has been.
    held: std::collections::VecDeque<(crate::types::Frame, u64)>,
    sent: u64,
}
impl Splicer {
    /// For the pipe `fd`, which the caller keeps open while this lives.
    pub(super) fn new(fd: std::os::fd::RawFd) -> Self {
        Self {
            fd,
            held: std::collections::VecDeque::new(),
            sent: 0,
        }
    }
    /// Send `bytes`, which are `frame`'s own, into the pipe.
    pub(super) fn send(
        &mut self,
        frame: &crate::types::Frame,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        self.release();
        let mut at = 0;
        while at < bytes.len() {
            let iov = libc::iovec {
                iov_base: bytes[at..].as_ptr() as *mut libc::c_void,
                iov_len: bytes.len() - at,
            };
            // SAFETY: the iovec points into `bytes`, which `frame` owns and
            // `held` keeps alive, unchanged, until the reader has read them.
            let sent = unsafe { libc::vmsplice(self.fd, &iov, 1, 0) };
            if sent < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // Pages it took before failing are still in the pipe.
                self.hold(frame, at);
                return Err(error);
            }
            at += sent as usize;
        }
        self.hold(frame, at);
        Ok(())
    }
    fn hold(&mut self, frame: &crate::types::Frame, bytes: usize) {
        if bytes > 0 {
            self.sent += bytes as u64;
            self.held.push_back((frame.clone(), self.sent));
        }
    }
    /// Let go of the frames the reader has read all of.
    fn release(&mut self) {
        let mut queued: libc::c_int = 0;
        // SAFETY: FIONREAD writes the bytes waiting in the pipe to `queued`.
        if unsafe { libc::ioctl(self.fd, libc::FIONREAD, &mut queued) } < 0 {
            return;
        }
        let read = self.sent.saturating_sub(queued as u64);
        while self.held.front().is_some_and(|(_, end)| *end <= read) {
            self.held.pop_front();
        }
    }
    /// The frames whose pages may still be in the pipe, to be dropped once
    /// its reader has exited.
    pub(super) fn into_held(mut self) -> Vec<crate::types::Frame> {
        self.release();
        self.held.into_iter().map(|(frame, _)| frame).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;

    /// Spliced frames arrive whole and in order, and each is kept until
    /// the reader has read it.
    #[test]
    fn spliced_frames_arrive_whole_and_are_kept_until_read() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        enlarge(writer.as_raw_fd(), 300_000);
        let frames: Vec<crate::types::Frame> = (0..4u8)
            .map(|n| crate::types::Frame {
                id: n.into(),
                data: (0..300_000u32).map(|i| (i as u8) ^ n).collect(),
                ..Default::default()
            })
            .collect();
        let expected: Vec<u8> = frames.iter().flat_map(|f| f.data.to_vec()).collect();
        let total = expected.len();
        let read = std::thread::spawn(move || {
            let mut bytes = vec![0; total];
            reader.read_exact(&mut bytes).unwrap();
            bytes
        });
        let mut splicer = Splicer::new(writer.as_raw_fd());
        for frame in &frames {
            splicer.send(frame, &frame.data).unwrap();
        }
        let bytes = read.join().unwrap();
        assert_eq!(bytes, expected);
        // Read whole: nothing is held any longer.
        assert!(splicer.into_held().is_empty());
        // Unread, they stay held.
        let (reader, writer) = std::io::pipe().unwrap();
        let mut splicer = Splicer::new(writer.as_raw_fd());
        splicer.send(&frames[0], &frames[0].data[..1000]).unwrap();
        let held = splicer.into_held();
        assert_eq!(held.len(), 1);
        assert!(held[0].data.is_shared());
        drop(reader);
    }
}
