//! Stream copy: a compressed source recorded as it arrives, without
//! decoding or encoding its video.
//!
//! A source's decoder FFmpeg can write the video it reads, as it reads it,
//! to a second output beside the raw frames it decodes for captures,
//! inspection and status: MPEG-TS on a pipe of its own (`-c:v copy`). A
//! [`Tap`] reads that pipe and cuts it into the source's frames, keeping
//! those since the last key frame. A recording that copies the source
//! attaches a [`Feed`] to the tap, starting at that key frame, and its
//! writer hands each frame to the recording's FFmpeg, which only remuxes
//! them into the file (`-c:v copy`), through the same output thread,
//! maximum size and free-space floor as an encoded recording.
//!
//! The tap belongs to the source, not to one FFmpeg: a network stream
//! reopened after a drop goes on feeding the recording from its next key
//! frame, and FFmpeg's MPEG-TS reader turns the new process's timestamps
//! into a continuous timeline. Nothing the tap does blocks the decoder: a
//! recording whose writer falls behind loses frames up to the next key
//! frame instead.
//!
//! Frames arrive in decode order. With B-frames, a recording asked to end
//! just after a reference frame would leave out the frames shown before
//! it, which come next: it goes on taking those ([`Order`]) and ends
//! before the next frame shown after everything it wrote.
// Windows FFmpeg cannot be handed a pipe of its own, so nothing taps a
// source there.
#![cfg_attr(not(unix), allow(dead_code))]
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

/// Whether a recording may copy a compressed source's own video rather
/// than encode its decoded frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamCopy {
    /// Always encode.
    #[default]
    Never,
    /// Copy a source whose codec is the one the recording asks for.
    Same,
    /// Copy a source in any codec a recording can copy (H.264 or HEVC);
    /// encode others in the codec asked for.
    Any,
}
impl StreamCopy {
    pub fn is_never(&self) -> bool {
        *self == Self::Never
    }
    /// Whether a source in `source` codec is copied for a recording that
    /// asks for `wanted`.
    pub(crate) fn takes(self, source: &str, wanted: &str) -> bool {
        match self {
            Self::Never => false,
            Self::Same => super::codec_family(wanted).is_ok_and(|family| family == source),
            Self::Any => true,
        }
    }
}

/// The codec families a recording copies, by FFmpeg's codec name: what
/// MPEG-TS carries and every recording container takes.
pub(crate) fn copyable(codec: &str) -> Option<&'static str> {
    match codec {
        "h264" => Some("h264"),
        "hevc" => Some("hevc"),
        _ => None,
    }
}

/// The FFmpeg output options that copy the source's video to `pipe`: MPEG-TS,
/// written a frame at a time.
pub(super) fn output_args(pipe: &str) -> Vec<String> {
    [
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-c:v",
        "copy",
        "-flush_packets",
        "1",
        "-f",
        "mpegts",
        pipe,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The most bytes of copied video one source keeps for a recording that
/// starts (the frames since the last key frame) and one recording queues
/// for its FFmpeg: a share of the memory left, within 4 to 64 MiB, far more
/// than seconds of camera video.
fn limit() -> usize {
    const SHARE: u64 = 32;
    let budget = crate::host::memory_budget().map_or(u64::MAX, |budget| budget / SHARE);
    budget.clamp(4 << 20, 64 << 20) as usize
}

/// MPEG-TS packets are this long.
const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// One of the source's frames as MPEG-TS packets: from the start of its
/// video PES to the start of the next.
#[derive(Debug)]
pub(crate) struct TsFrame {
    pub(crate) data: Vec<u8>,
    /// The frame is one a decoder can start at.
    pub(crate) key: bool,
    /// For a key frame, the program tables (PAT then PMT) a reader starting
    /// at it needs first.
    pub(crate) tables: Tables,
    /// When it arrived, UTC Unix milliseconds.
    pub(crate) arrived_ms: u64,
    /// Its presentation time, in MPEG-TS's 90 kHz units (33 bits), when
    /// its PES header has one.
    pub(crate) pts: Option<u64>,
}

/// MPEG-TS from a tap's pipe, cut into frames.
#[derive(Default)]
pub(crate) struct Framer {
    /// The start of a packet that has not arrived whole.
    partial: Vec<u8>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pat: Option<Vec<u8>>,
    pmt: Option<Vec<u8>>,
    /// The frame being received.
    frame: Option<TsFrame>,
}
impl Framer {
    /// Take `bytes`, handing each frame they complete to `out`.
    pub(crate) fn push(&mut self, mut bytes: &[u8], out: &mut impl FnMut(TsFrame)) {
        if !self.partial.is_empty() {
            let wanted = (PACKET - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..wanted]);
            bytes = &bytes[wanted..];
            if self.partial.len() < PACKET {
                return;
            }
            let packet = std::mem::take(&mut self.partial);
            self.packet(&packet, out);
        }
        while !bytes.is_empty() {
            // FFmpeg writes whole packets; anything else is skipped up to
            // the next sync byte.
            if bytes[0] != SYNC {
                let skip = bytes.iter().position(|&b| b == SYNC).unwrap_or(bytes.len());
                bytes = &bytes[skip..];
                continue;
            }
            if bytes.len() < PACKET {
                self.partial.extend_from_slice(bytes);
                return;
            }
            let (packet, rest) = bytes.split_at(PACKET);
            self.packet(packet, out);
            bytes = rest;
        }
    }
    /// The stream has ended: the frame being received is complete.
    pub(crate) fn finish(&mut self, out: &mut impl FnMut(TsFrame)) {
        if let Some(frame) = self.frame.take() {
            out(frame);
        }
    }
    fn packet(&mut self, packet: &[u8], out: &mut impl FnMut(TsFrame)) {
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let start = packet[1] & 0x40 != 0;
        let control = (packet[3] >> 4) & 3;
        let adaptation = control & 2 != 0;
        let payload_at = if adaptation {
            5 + usize::from(packet[4])
        } else {
            4
        };
        let payload = (control & 1 != 0)
            .then(|| packet.get(payload_at..))
            .flatten()
            .unwrap_or_default();
        if pid == 0 && start {
            if let Some(pmt_pid) = section(payload, 0x00).and_then(program_map_pid) {
                self.pmt_pid = Some(pmt_pid);
                self.pat = Some(packet.to_vec());
            }
        } else if Some(pid) == self.pmt_pid && start {
            if let Some(video_pid) = section(payload, 0x02).and_then(video_pid) {
                self.video_pid = Some(video_pid);
                self.pmt = Some(packet.to_vec());
            }
        } else if Some(pid) == self.video_pid && start {
            // A random access indicator marks a key frame's first packet.
            let key = adaptation && packet[4] > 0 && packet[5] & 0x40 != 0;
            let tables = match (&self.pat, &self.pmt) {
                (Some(pat), Some(pmt)) if key => Some([&pat[..], &pmt[..]].concat().into()),
                _ => None,
            };
            if let Some(frame) = self.frame.take() {
                out(frame);
            }
            self.frame = Some(TsFrame {
                data: Vec::with_capacity(64 * PACKET),
                key,
                tables,
                arrived_ms: 0,
                pts: pts(payload),
            });
        }
        // Packets before the first frame are only the tables, kept above.
        if let Some(frame) = &mut self.frame {
            frame.data.extend_from_slice(packet);
        }
    }
}

/// The presentation time in the PES header a packet's payload starts with.
fn pts(payload: &[u8]) -> Option<u64> {
    if payload.get(..3)? != [0, 0, 1] || payload.get(7)? & 0x80 == 0 {
        return None;
    }
    let b = payload.get(9..14)?;
    Some(
        (u64::from(b[0] >> 1) & 0x07) << 30
            | u64::from(b[1]) << 22
            | u64::from(b[2] >> 1) << 15
            | u64::from(b[3]) << 7
            | u64::from(b[4] >> 1),
    )
}

/// The display order of the frames a recording wrote: the latest
/// presentation time, and whether any frame came after one shown later
/// (B-frames).
#[derive(Default)]
pub(crate) struct Order {
    latest: Option<u64>,
    reordered: bool,
}
impl Order {
    /// `frame` was written.
    pub(crate) fn wrote(&mut self, frame: &TsFrame) {
        let Some(pts) = frame.pts else {
            return;
        };
        match self.latest {
            Some(latest) if before(pts, latest) => self.reordered = true,
            _ => self.latest = Some(pts),
        }
    }
    /// Frames may yet come that are shown before one written.
    pub(crate) fn reordered(&self) -> bool {
        self.reordered
    }
    /// `frame` is shown before a frame written: a recording that ends
    /// keeps it, so it ends with no frame missing.
    pub(crate) fn completes(&self, frame: &TsFrame) -> bool {
        self.reordered
            && frame
                .pts
                .zip(self.latest)
                .is_some_and(|(pts, latest)| before(pts, latest))
    }
}
/// Whether 33-bit time `a` comes before `b`, across a wrap.
fn before(a: u64, b: u64) -> bool {
    const BITS: u64 = (1 << 33) - 1;
    a != b && b.wrapping_sub(a) & BITS < 1 << 32
}

/// The PSI section of `table` a packet's payload starts, up to its CRC.
fn section(payload: &[u8], table: u8) -> Option<&[u8]> {
    let pointer = usize::from(*payload.first()?);
    let section = payload.get(1 + pointer..)?;
    if *section.first()? != table {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([*section.get(1)?, *section.get(2)?]) & 0x0fff);
    // The header's 3 bytes, then `length` bytes ending in a 4-byte CRC.
    section
        .get(..3 + length)?
        .get(..(3 + length).checked_sub(4)?)
}

/// The PID of the first program's map in a program association section.
fn program_map_pid(section: &[u8]) -> Option<u16> {
    section
        .get(8..)?
        .as_chunks::<4>()
        .0
        .iter()
        .find_map(|entry| {
            let program = u16::from_be_bytes([entry[0], entry[1]]);
            (program != 0).then(|| u16::from_be_bytes([entry[2], entry[3]]) & 0x1fff)
        })
}

/// The PID of the video in a program map section: its H.264 or HEVC
/// stream, else its first.
fn video_pid(section: &[u8]) -> Option<u16> {
    let info = usize::from(u16::from_be_bytes([*section.get(10)?, *section.get(11)?]) & 0x0fff);
    let mut streams = section.get(12 + info..)?;
    let mut first = None;
    while streams.len() >= 5 {
        let kind = streams[0];
        let pid = u16::from_be_bytes([streams[1], streams[2]]) & 0x1fff;
        let skip = usize::from(u16::from_be_bytes([streams[3], streams[4]]) & 0x0fff);
        // H.264 and HEVC.
        if matches!(kind, 0x1b | 0x24) {
            return Some(pid);
        }
        first.get_or_insert(pid);
        streams = streams.get(5 + skip..)?;
    }
    first
}

/// A source's copied video, kept across its FFmpeg processes, for the
/// recording that copies it.
pub struct Tap {
    codec: &'static str,
    limit: usize,
    state: Mutex<TapState>,
}
#[derive(Default)]
struct TapState {
    /// The frames since the last key frame, which a recording that starts
    /// now begins with; none while they would outgrow the limit.
    gop: Vec<Arc<TsFrame>>,
    gop_bytes: usize,
    feed: Option<Arc<Feed>>,
}
impl Tap {
    pub(crate) fn new(codec: &'static str) -> Arc<Self> {
        Arc::new(Self {
            codec,
            limit: limit(),
            state: Mutex::default(),
        })
    }
    /// The codec family the source sends, such as "h264".
    pub(crate) fn codec(&self) -> &'static str {
        self.codec
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, TapState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Start feeding `feed`, from the last key frame.
    pub(crate) fn attach(&self, feed: Arc<Feed>) {
        let mut state = self.lock();
        for frame in &state.gop {
            feed.offer(frame);
        }
        state.feed = Some(feed);
    }
    /// Stop feeding `feed`, if it is the one fed.
    pub(crate) fn detach(&self, feed: &Arc<Feed>) {
        let mut state = self.lock();
        if state
            .feed
            .as_ref()
            .is_some_and(|fed| Arc::ptr_eq(fed, feed))
        {
            state.feed = None;
        }
    }
    /// A recording is copying the source.
    pub(crate) fn attached(&self) -> bool {
        self.lock().feed.is_some()
    }
    /// A new FFmpeg process starts writing: what came before it is no
    /// start for a recording, and the one fed goes on from the new
    /// process's first key frame.
    fn restart(&self) {
        let mut state = self.lock();
        (state.gop, state.gop_bytes) = (Vec::new(), 0);
        if let Some(feed) = &state.feed {
            feed.resync();
        }
    }
    fn offer(&self, mut frame: TsFrame) {
        frame.arrived_ms = crate::scheduling::now_ms();
        let frame = Arc::new(frame);
        let mut state = self.lock();
        if frame.key {
            (state.gop, state.gop_bytes) = (Vec::new(), 0);
        }
        if frame.key || !state.gop.is_empty() {
            if state.gop_bytes + frame.data.len() <= self.limit {
                state.gop_bytes += frame.data.len();
                state.gop.push(frame.clone());
            } else {
                (state.gop, state.gop_bytes) = (Vec::new(), 0);
            }
        }
        if let Some(feed) = &state.feed {
            feed.offer(&frame);
        }
    }
    /// Read a decoder's copied video from `pipe` until its FFmpeg exits.
    pub(crate) fn read(&self, mut pipe: impl Read) {
        self.restart();
        let mut framer = Framer::default();
        let mut buffer = vec![0; 256 * PACKET];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => framer.push(&buffer[..count], &mut |frame| self.offer(frame)),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        framer.finish(&mut |frame| self.offer(frame));
    }
}

/// One recording's queue of copied frames, between the tap and the
/// recording's writer.
pub(crate) struct Feed {
    state: Mutex<FeedState>,
    changed: Condvar,
    limit: usize,
}
#[derive(Default)]
struct FeedState {
    /// Each frame, after the tables when it starts the stream again, with
    /// how many were queued before it.
    queue: VecDeque<(u64, Tables, Arc<TsFrame>)>,
    /// How many frames were queued when the recording was asked to end
    /// (`end`): those after came later.
    ending: Option<u64>,
    bytes: usize,
    /// It has a key frame to go on from: not until its first, nor after
    /// frames were dropped or the source's FFmpeg started again.
    synced: bool,
    queued: u64,
    /// Frames it could not keep: one that would have outgrown its limit,
    /// with those queued before it, and those up to the next key frame.
    dropped: u64,
    closed: bool,
}
/// The program tables a key frame that starts a stream is written after.
pub(crate) type Tables = Option<Arc<[u8]>>;
/// What a recording's writer takes from its feed.
pub(crate) enum Taken {
    /// A frame, after the program tables when it starts the stream again,
    /// and whether it was queued after the recording was asked to end.
    Frame(Tables, Arc<TsFrame>, bool),
    /// Nothing arrived in time.
    Waiting,
    /// The feed is closed and every frame it queued taken.
    Closed,
}
impl Feed {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            changed: Condvar::new(),
            limit: limit(),
        })
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn offer(&self, frame: &Arc<TsFrame>) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        if !state.synced && (!frame.key || frame.tables.is_none()) {
            // Before its first key frame a recording has not started.
            if state.queued > 0 {
                state.dropped += 1;
            }
            return;
        }
        if state.bytes + frame.data.len() > self.limit {
            // The recording's writer fell behind: what it has not written
            // goes, and it goes on from a key frame.
            state.dropped += state.queue.len() as u64 + 1;
            state.queue.clear();
            (state.bytes, state.synced) = (0, false);
            return;
        }
        let tables = if state.synced {
            None
        } else {
            frame.tables.clone()
        };
        state.synced = true;
        let index = state.queued;
        state.queued += 1;
        state.bytes += frame.data.len();
        state.queue.push_back((index, tables, frame.clone()));
        drop(state);
        self.changed.notify_one();
    }
    fn resync(&self) {
        self.lock().synced = false;
    }
    /// The next frame, waiting up to `wait` for one.
    pub(crate) fn take(&self, wait: Duration) -> Taken {
        let mut state = self.lock();
        if state.queue.is_empty() && !state.closed {
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        match state.queue.pop_front() {
            Some((index, tables, frame)) => {
                state.bytes -= frame.data.len();
                let late = state.ending.is_some_and(|ending| index >= ending);
                Taken::Frame(tables, frame, late)
            }
            None if state.closed => Taken::Closed,
            None => Taken::Waiting,
        }
    }
    /// The recording is asked to end: frames queued from now on are taken
    /// as late ones.
    pub(crate) fn end(&self) {
        let mut state = self.lock();
        let queued = state.queued;
        state.ending.get_or_insert(queued);
    }
    /// Take no more frames; those queued can still be taken.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }
    pub(crate) fn dropped(&self) -> u64 {
        self.lock().dropped
    }
}

/// A pipe's read end, and its write end for a child.
pub(super) type Pipe = (std::io::PipeReader, std::io::PipeWriter);

/// Hand FFmpeg a pipe of its own for the copied video, named in the output
/// arguments added to `command`: the end this process reads, and the end
/// the child writes, which the caller drops once the child is spawned so
/// the pipe ends with the child.
#[cfg(unix)]
pub(super) fn pipe(command: &mut std::process::Command) -> anyhow::Result<Pipe> {
    use anyhow::Context;
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let (reader, writer) = std::io::pipe().context("cannot create a pipe for stream copy")?;
    let fd = writer.as_raw_fd();
    command.args(output_args(&format!("pipe:{fd}")));
    // SAFETY: fcntl is async-signal-safe, as pre_exec requires; the
    // descriptor is the pipe's write end, which the caller keeps open
    // until the spawn has happened. Only the child loses close-on-exec, so
    // no other process this one starts inherits it.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok((reader, writer))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A TS packet on `pid`, starting a PES or section when `start`, with
    /// a random access indicator when `key`, carrying `payload` (padded).
    pub(crate) fn packet(pid: u16, start: bool, key: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![
            SYNC,
            (u8::from(start) << 6) | (pid >> 8) as u8,
            pid as u8,
            0x30,
            1,
            if key { 0x40 } else { 0 },
        ];
        packet.extend_from_slice(payload);
        packet.resize(PACKET, 0xff);
        packet
    }
    /// PAT naming PMT PID 0x1000, then a PMT naming H.264 video on 0x100.
    pub(crate) fn tables() -> Vec<u8> {
        let pat = [
            0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0x00, 0, 0, 0, 0,
        ];
        let pmt = [
            0, 0x02, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0, 0x1b, 0xe1, 0x00, 0xf0, 0, 0,
            0, 0, 0,
        ];
        [
            packet(0, true, false, &pat),
            packet(0x1000, true, false, &pmt),
        ]
        .concat()
    }
    /// A frame of `packets` packets on the video PID, its first tagged `id`.
    pub(crate) fn frame(id: u8, key: bool, packets: usize) -> Vec<u8> {
        (0..packets)
            .flat_map(|i| packet(0x100, i == 0, key && i == 0, &[0, 0, 1, 0xe0, id]))
            .collect()
    }
    fn cut(bytes: &[u8], step: usize) -> Vec<TsFrame> {
        let mut framer = Framer::default();
        let mut frames = Vec::new();
        for chunk in bytes.chunks(step) {
            framer.push(chunk, &mut |frame| frames.push(frame));
        }
        framer.finish(&mut |frame| frames.push(frame));
        frames
    }

    #[test]
    fn mpeg_ts_is_cut_into_frames_with_their_tables_at_key_frames() {
        let stream = [
            tables(),
            frame(1, true, 3),
            frame(2, false, 2),
            tables(),
            frame(3, true, 1),
        ]
        .concat();
        // However the pipe splits it.
        for step in [1, 7, PACKET, 1000, stream.len()] {
            let frames = cut(&stream, step);
            assert_eq!(frames.len(), 3, "{step}");
            assert_eq!(
                frames.iter().map(|f| f.key).collect::<Vec<_>>(),
                [true, false, true]
            );
            assert_eq!(frames[0].data, frame(1, true, 3));
            assert_eq!(frames[0].tables.as_deref(), Some(&tables()[..]));
            assert!(frames[1].tables.is_none());
            // Tables between frames stay with the frame before them.
            assert_eq!(frames[1].data, [frame(2, false, 2), tables()].concat());
            assert_eq!(frames[2].data, frame(3, true, 1));
        }
        // Bytes out of step with the packets are skipped.
        let noisy = [&[1, 2, 3][..], &stream].concat();
        assert_eq!(cut(&noisy, 50).len(), 3);
        // No tables, no frames: the video's PID is not known.
        assert!(cut(&frame(1, true, 2), PACKET).is_empty());
    }

    fn ts(id: u8, key: bool) -> TsFrame {
        TsFrame {
            data: frame(id, key, 1),
            key,
            tables: key.then(|| tables().into()),
            arrived_ms: 0,
            pts: None,
        }
    }
    fn taken(feed: &Feed) -> Vec<(bool, u8)> {
        let mut ids = Vec::new();
        while let Taken::Frame(tables, frame, _) = feed.take(Duration::ZERO) {
            ids.push((tables.is_some(), frame.data[10]));
        }
        ids
    }

    /// A PES header carrying presentation time `pts`.
    fn pes(pts: u64) -> Vec<u8> {
        vec![
            0,
            0,
            1,
            0xe0,
            0,
            0,
            0x80,
            0x80,
            5,
            0x21 | ((pts >> 29) & 0x0e) as u8,
            (pts >> 22) as u8,
            0x01 | ((pts >> 14) & 0xfe) as u8,
            (pts >> 7) as u8,
            0x01 | ((pts << 1) & 0xfe) as u8,
        ]
    }

    #[test]
    fn frames_keep_their_presentation_time_and_order() {
        let stream = [
            tables(),
            packet(0x100, true, true, &pes(3600)),
            packet(0x100, true, false, &pes((1 << 33) - 3600)),
            packet(0x100, true, false, &[0, 0, 1, 0xe0, 0, 0, 0x80, 0, 0]),
        ]
        .concat();
        let frames = cut(&stream, PACKET);
        assert_eq!(
            frames.iter().map(|f| f.pts).collect::<Vec<_>>(),
            [Some(3600), Some((1 << 33) - 3600), None]
        );
        // Decode order I0 P3 B1 B2 P6 B4 B5, in 90 kHz ticks a frame.
        let at = |frame: u64| TsFrame {
            pts: Some(frame * 3600),
            ..ts(frame as u8, frame == 0)
        };
        let mut order = Order::default();
        for frame in [0, 3, 1, 2, 6] {
            order.wrote(&at(frame));
        }
        assert!(order.reordered());
        // After P6, B4 and B5 complete it; P9 does not.
        assert!(order.completes(&at(4)) && order.completes(&at(5)));
        assert!(!order.completes(&at(9)));
        // Without B-frames nothing is waited for.
        let mut plain = Order::default();
        for frame in [0, 1, 2] {
            plain.wrote(&at(frame));
        }
        assert!(!plain.reordered() && !plain.completes(&at(1)));
        // Across the 33-bit wrap.
        assert!(before((1 << 33) - 3600, 3600) && !before(3600, (1 << 33) - 3600));
    }

    #[test]
    fn frames_queued_after_the_end_was_asked_are_late() {
        let feed = Feed::new();
        feed.offer(&Arc::new(ts(1, true)));
        feed.end();
        feed.offer(&Arc::new(ts(2, false)));
        let late = |feed: &Feed| match feed.take(Duration::ZERO) {
            Taken::Frame(_, _, late) => Some(late),
            _ => None,
        };
        assert_eq!(late(&feed), Some(false));
        assert_eq!(late(&feed), Some(true));
        assert_eq!(late(&feed), None);
    }

    #[test]
    fn a_feed_starts_at_the_last_key_frame_and_resyncs_after_a_restart() {
        let tap = Tap::new("h264");
        tap.offer(ts(1, false));
        tap.offer(ts(2, true));
        tap.offer(ts(3, false));
        let feed = Feed::new();
        tap.attach(feed.clone());
        tap.offer(ts(4, false));
        assert!(tap.attached());
        assert_eq!(taken(&feed), [(true, 2), (false, 3), (false, 4)]);
        // A new FFmpeg: the frames up to its first key frame are dropped.
        tap.restart();
        tap.offer(ts(5, false));
        tap.offer(ts(6, true));
        assert_eq!(taken(&feed), [(true, 6)]);
        assert_eq!(feed.dropped(), 1);
        tap.detach(&feed);
        tap.offer(ts(7, true));
        assert!(!tap.attached());
        assert!(taken(&feed).is_empty());
        feed.close();
        assert!(matches!(feed.take(Duration::ZERO), Taken::Closed));
    }

    #[test]
    fn a_feed_that_falls_behind_drops_up_to_the_next_key_frame() {
        let feed = Arc::new(Feed {
            state: Mutex::default(),
            changed: Condvar::new(),
            limit: 3 * PACKET,
        });
        for (id, key) in [(1, true), (2, false), (3, false), (4, false), (5, false)] {
            feed.offer(&Arc::new(ts(id, key)));
        }
        // The fourth frame did not fit: it and the three before it went.
        assert_eq!(feed.dropped(), 5);
        feed.offer(&Arc::new(ts(6, true)));
        assert_eq!(taken(&feed), [(true, 6)]);
        // A closed feed hands over what it queued, then ends.
        feed.offer(&Arc::new(ts(7, false)));
        feed.close();
        feed.offer(&Arc::new(ts(8, false)));
        assert_eq!(taken(&feed), [(false, 7)]);
        assert!(matches!(feed.take(Duration::ZERO), Taken::Closed));
    }

    #[test]
    fn recordings_copy_what_they_ask_for() {
        assert!(StreamCopy::Any.takes("hevc", "h264"));
        assert!(StreamCopy::Same.takes("hevc", "h265"));
        assert!(!StreamCopy::Same.takes("hevc", "h264"));
        assert!(!StreamCopy::Never.takes("h264", "h264"));
        assert_eq!(copyable("h264"), Some("h264"));
        assert_eq!(copyable("mjpeg"), None);
    }
}
