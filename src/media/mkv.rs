//! A Matroska recording finished as `FFmpeg` would finish a file it could
//! seek in: its length (Info's Duration) and its index (Cues).
//!
//! A local recording reaches its file through a pipe (the storage writer
//! bounds and watches every byte), so `FFmpeg` cannot go back to write what
//! it learns only at the end. Two steps make up for it:
//!
//! - `FFmpeg` is asked to stop after a century (`-t`), which makes it write a
//!   Duration into Info at the start. As the start passes through the
//!   recording's output thread, [`Rewriter`] turns that Duration into a
//!   Void of the same size, so a file left behind by a crash has no false
//!   length, and notes where the parts of the start are ([`Head`]).
//! - Once `FFmpeg` has finished, [`finish`] reads the clusters' starts and
//!   appends Cues, one entry a cluster that starts with a key frame, names
//!   them in the `SeekHead` in the space `FFmpeg` left after it, gives the
//!   Segment its size, and writes the Duration back in place of the Void.
//!
//! Each element `FFmpeg` protects with a CRC-32 gets a new one. Whatever this
//! cannot read as expected it leaves as `FFmpeg` wrote it.
#![forbid(unsafe_code)]
#![deny(clippy::missing_const_for_fn)]

use std::{
    cmp::Ordering,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write as _},
};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x002A_D7B1;
const DURATION: u32 = 0x4489;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const DEFAULT_DURATION: u32 = 0x0023_E383;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const CRC32: u32 = 0xBF;
const VOID: u32 = 0xEC;

/// The longest start of a file looked through for its first cluster.
const MOST_HEAD: usize = 1 << 20;

/// `FFmpeg`'s output options that make it write a Duration to take the place
/// of: a stop after a century, which no recording reaches.
pub(super) const fn output_args() -> [&'static str; 2] {
    ["-t", "3155760000"]
}

/// An element's ID, its size (None when unknown) and its header's length.
#[deny(clippy::arithmetic_side_effects)]
fn element(bytes: &[u8]) -> Option<(u32, Option<u64>, usize)> {
    let first = *bytes.first()?;
    let id_len = usize::try_from(first.leading_zeros().checked_add(1)?).ok()?;
    if id_len > 4 {
        return None;
    }
    let id = bytes
        .get(..id_len)?
        .iter()
        .fold(0u32, |id, &byte| (id << 8) | u32::from(byte));
    let (size, size_len) = vint(bytes.get(id_len..)?)?;
    Some((id, size, id_len.checked_add(size_len)?))
}

/// A size: its value (None when every value bit is set, unknown) and its
/// length.
#[deny(clippy::arithmetic_side_effects)]
fn vint(bytes: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *bytes.first()?;
    let width = first.leading_zeros().checked_add(1)?;
    if width > 8 {
        return None;
    }
    let len = usize::try_from(width).ok()?;
    let mut value = u64::from(first & 0xFFu8.checked_shr(width).unwrap_or(0));
    for &byte in bytes.get(1..len)? {
        value = (value << 8) | u64::from(byte);
    }
    let unknown = value == 1u64.checked_shl(width.checked_mul(7)?)?.checked_sub(1)?;
    Some(((!unknown).then_some(value), len))
}

fn uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

fn id_length(id: u32) -> usize {
    usize::try_from((32 - id.leading_zeros()).div_ceil(8).max(1))
        .expect("an element ID uses at most four bytes")
}

fn id_bytes(id: u32) -> Vec<u8> {
    id.to_be_bytes()[4 - id_length(id)..].to_vec()
}

/// `value` as a size of `len` bytes.
fn size_bytes(value: u64, len: usize) -> Vec<u8> {
    size_array(value, len)[8 - len..].to_vec()
}

const fn size_array(value: u64, len: usize) -> [u8; 8] {
    let mut bytes = value.to_be_bytes();
    bytes[8 - len] |= 0x80 >> (len - 1);
    bytes
}

/// The shortest size field for `value`.
fn size_len(value: u64) -> usize {
    (1..=8)
        .find(|&len| value < (1u64 << (7 * len)) - 1)
        .unwrap_or(8)
}

fn master(id: u32, data: &[u8]) -> Vec<u8> {
    let id_len = id_length(id);
    let length = u64::try_from(data.len()).expect("element data length fits its 64-bit size");
    let size_len = size_len(length);
    let capacity = data
        .len()
        .checked_add(id_len)
        .and_then(|length| length.checked_add(size_len))
        .expect("element and header fit the address space");
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(&id.to_be_bytes()[4 - id_len..]);
    bytes.extend_from_slice(&size_array(length, size_len)[8 - size_len..]);
    bytes.extend_from_slice(data);
    bytes
}

fn unsigned(id: u32, value: u64) -> Vec<u8> {
    let len = (64 - value.leading_zeros() as usize).div_ceil(8).max(1);
    master(id, &value.to_be_bytes()[8 - len..])
}

/// A Void element exactly `len` bytes long (`len` is at least 2).
fn void(len: usize) -> Option<Vec<u8>> {
    let size = match len {
        0 | 1 => return None,
        2..=128 => size_bytes(len as u64 - 2, 1),
        _ => size_bytes(len as u64 - 9, 8),
    };
    let mut bytes = vec![u8::try_from(VOID).expect("Void has a one-byte ID")];
    bytes.extend(size);
    bytes.resize(len, 0);
    Some(bytes)
}

fn crc(data: &[u8]) -> [u8; 4] {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum().to_le_bytes()
}

/// The children of a master element's `data`: each one's ID, its data's
/// range within `data`, and its own start.
fn children(data: &[u8]) -> Vec<(u32, std::ops::Range<usize>, usize)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some((id, Some(size), header)) = element(&data[at..]) {
        let start = at + header;
        let Some(end) = usize::try_from(size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .filter(|&end| end <= data.len())
        else {
            break;
        };
        found.push((id, start..end, at));
        at = end;
    }
    found
}

/// Where the parts of a Matroska file's start are, in bytes from its
/// start, as [`Rewriter`] found them.
#[deny(clippy::struct_field_names)]
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Head {
    /// Where the Segment's data starts: positions in it count from here.
    segment_data: u64,
    /// The Segment's size field, when `FFmpeg` left it unknown in 8 bytes.
    segment_size_at: Option<u64>,
    /// The `SeekHead`, with the Void after it.
    seek: Option<SeekHead>,
    /// Where Info's data ends, and its CRC-32 element's start when it has
    /// one.
    info: (u64, Option<u64>),
    /// The Void that took the Duration's place.
    duration_at: u64,
    /// Nanoseconds a timestamp tick.
    scale: u64,
    /// The video track, and how long its frames last when it says.
    track: Option<u64>,
    frame_ns: Option<u64>,
    first_cluster: u64,
}

#[deny(clippy::struct_field_names)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct SeekHead {
    offset: u64,
    /// Space occupied by the element and any following Void.
    space: usize,
    /// Encoded Seek elements, preserved when adding the Cues entry.
    entries: Vec<u8>,
    crc: bool,
}

/// `FFmpeg`'s output on its way to the file: its start is held until its
/// first cluster, its Duration made a Void and the start's parts noted.
pub(super) struct Rewriter {
    held: Vec<u8>,
    /// Once the start has gone on: what it says, if it could be read.
    start: Start,
}
impl Rewriter {
    pub(super) const fn new() -> Self {
        Self {
            held: Vec::new(),
            start: Start::More,
        }
    }
    /// What to write of `bytes` now: none while the start is held.
    pub(super) fn push<'a>(&mut self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        if !matches!(self.start, Start::More) {
            return bytes.into();
        }
        self.held.extend_from_slice(bytes);
        let (start, info) = read_head(&self.held);
        let start = match start {
            Start::More if self.held.len() < MOST_HEAD => return Vec::new().into(),
            Start::More | Start::Unknown => Start::Unknown,
            Start::Head(head) => Start::Head(head),
        };
        // A file whose start cannot be read further still loses its false
        // Duration.
        if let Some(info) = info {
            info.patch(&mut self.held);
        }
        self.start = start;
        std::mem::take(&mut self.held).into()
    }
    /// What is still held, at the output's end.
    pub(super) fn flush(&mut self) -> Vec<u8> {
        if matches!(self.start, Start::More) {
            if let (_, Some(info)) = read_head(&self.held) {
                info.patch(&mut self.held);
            }
            self.start = Start::Unknown;
        }
        std::mem::take(&mut self.held)
    }
    /// What the start said, once it has gone on.
    pub(super) fn head(&self) -> Option<Head> {
        match &self.start {
            Start::Head(head) => Some(head.clone()),
            Start::More | Start::Unknown => None,
        }
    }
}

enum Start {
    More,
    Unknown,
    Head(Head),
}

/// Where Info's Duration is, to make a Void, and its CRC-32 when it has
/// one: the element's start and where Info's data ends.
#[derive(Clone, Copy)]
struct Patch {
    duration_at: usize,
    duration_len: usize,
    crc: Option<(usize, usize)>,
}
impl Patch {
    fn patch(self, bytes: &mut [u8]) {
        let Some(void) = void(self.duration_len) else {
            return;
        };
        bytes[self.duration_at..self.duration_at + self.duration_len].copy_from_slice(&void);
        if let Some((crc_at, end)) = self.crc {
            let sum = crc(&bytes[crc_at + 6..end]);
            bytes[crc_at + 2..crc_at + 6].copy_from_slice(&sum);
        }
    }
}

/// Read the start of a file in `bytes`, up to its first cluster, and say
/// where its Duration is once Info is whole.
fn read_head(bytes: &[u8]) -> (Start, Option<Patch>) {
    let mut patch = None;
    let start = read_start(bytes, &mut patch);
    (start, patch)
}
fn read_start(bytes: &[u8], patch: &mut Option<Patch>) -> Start {
    let len = bytes.len();
    let more = |at: usize, header: usize, size: u64| {
        usize::try_from(size)
            .ok()
            .and_then(|size| (at + header).checked_add(size))
            .is_none_or(|end| end > len)
    };
    let Some((id, size, header)) = element(bytes) else {
        return if bytes.len() < 12 {
            Start::More
        } else {
            Start::Unknown
        };
    };
    let Some(size) = size.filter(|_| id == EBML) else {
        return Start::Unknown;
    };
    if more(0, header, size) {
        return Start::More;
    }
    let mut at = header + usize::try_from(size).expect("element fits the available header buffer");
    let Some((id, _, header)) = element(&bytes[at..]) else {
        return Start::More;
    };
    if id != SEGMENT {
        return Start::Unknown;
    }
    let size_at = at + id_bytes(id).len();
    let segment_size_at = (header - id_bytes(id).len() == 8).then_some(size_at as u64);
    let segment_data = at + header;
    at = segment_data;
    let mut seek: Option<SeekHead> = None;
    let mut info = None;
    let (mut track, mut frame_ns) = (None, None);
    loop {
        let Some((id, size, header)) = element(&bytes[at..]) else {
            return Start::More;
        };
        if id == CLUSTER {
            let Some((info, duration_at, scale)) = info else {
                return Start::Unknown;
            };
            return Start::Head(Head {
                segment_data: segment_data as u64,
                segment_size_at,
                seek,
                info,
                duration_at,
                scale,
                track,
                frame_ns,
                first_cluster: at as u64,
            });
        }
        let Some(size) = size else {
            return Start::Unknown;
        };
        if more(at, header, size) {
            return Start::More;
        }
        let data = at + header
            ..at + header
                + usize::try_from(size).expect("element fits the available header buffer");
        match id {
            SEEK_HEAD => {
                let found = children(&bytes[data.clone()]);
                let crc = found.first().is_some_and(|(id, ..)| *id == CRC32);
                let seeks = found
                    .iter()
                    .filter(|(id, ..)| *id == SEEK)
                    .flat_map(|(_, range, start)| {
                        bytes[data.start + start..data.start + range.end]
                            .iter()
                            .copied()
                    })
                    .collect();
                seek = Some(SeekHead {
                    offset: at as u64,
                    space: data.end - at,
                    entries: seeks,
                    crc,
                });
            }
            // The space FFmpeg keeps after its SeekHead.
            VOID if seek.as_ref().is_some_and(|seek| {
                usize::try_from(seek.offset)
                    .ok()
                    .and_then(|start| start.checked_add(seek.space))
                    == Some(at)
            }) =>
            {
                if let Some(seek) = &mut seek {
                    seek.space += data.end - at;
                }
            }
            INFO if info.is_none() => {
                let found = children(&bytes[data.clone()]);
                let crc_at = found
                    .first()
                    .filter(|(id, range, _)| *id == CRC32 && range.len() == 4)
                    .map(|(_, _, start)| data.start + start);
                let scale = found
                    .iter()
                    .find(|(id, ..)| *id == TIMESTAMP_SCALE)
                    .map_or(1_000_000, |(_, range, _)| {
                        uint(&bytes[data.start..][range.clone()])
                    });
                let Some((_, range, start)) = found
                    .iter()
                    .find(|(id, range, _)| *id == DURATION && range.len() == 8)
                else {
                    return Start::Unknown;
                };
                let duration_at = data.start + start;
                let duration_len = data.start + range.end - duration_at;
                if void(duration_len).is_none() {
                    return Start::Unknown;
                }
                *patch = Some(Patch {
                    duration_at,
                    duration_len,
                    crc: crc_at.map(|crc_at| (crc_at, data.end)),
                });
                info = Some((
                    (data.end as u64, crc_at.map(|at| at as u64)),
                    duration_at as u64,
                    scale.max(1),
                ));
            }
            TRACKS => {
                for (_, entry, _) in children(&bytes[data.clone()])
                    .into_iter()
                    .filter(|(id, ..)| *id == TRACK_ENTRY)
                {
                    let entry = &bytes[data.start..][entry];
                    let fields = children(entry);
                    let field = |wanted| {
                        fields
                            .iter()
                            .find(|(id, ..)| *id == wanted)
                            .map(|(_, range, _)| uint(&entry[range.clone()]))
                    };
                    if field(TRACK_TYPE) == Some(1) && track.is_none() {
                        track = field(TRACK_NUMBER);
                        frame_ns = field(DEFAULT_DURATION);
                    }
                }
            }
            _ => {}
        }
        at = data.end;
    }
}

#[deny(clippy::panic_in_result_fn)]
fn read_at(file: &mut (impl Read + Seek), at: u64, buffer: &mut [u8]) -> io::Result<usize> {
    file.seek(SeekFrom::Start(at))?;
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn write_at(file: &mut File, at: u64, bytes: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(at))?;
    file.write_all(bytes)
}

/// A block's track, timestamp relative to its cluster, and whether it is
/// a key frame, from the start of its data (a `SimpleBlock`'s, or a Block's
/// in a group, which `group` holds whole when it is one).
fn block(data: &[u8], group: Option<&[u8]>) -> Option<Block> {
    let (track, len) = vint(data)?;
    let timestamp = i16::from_be_bytes([*data.get(len)?, *data.get(len + 1)?]);
    let key = match group {
        Some(group) => !children(group)
            .iter()
            .any(|(id, ..)| *id == REFERENCE_BLOCK),
        None => data.get(len + 2)? & 0x80 != 0,
    };
    Some((track?, i64::from(timestamp), key))
}

/// A block's track, time relative to its cluster, and whether it is a key
/// frame.
type Block = (u64, i64, bool);

/// A file length capped to a host buffer, including lengths beyond usize.
fn bounded_len(length: u64, capacity: usize) -> usize {
    usize::try_from(length).unwrap_or(capacity).min(capacity)
}

/// The cluster whose data starts `at` and lasts `size` bytes: its own
/// time, and its blocks (only the first unless `every`), read a child at a
/// time.
fn cluster(
    file: &mut File,
    at: u64,
    size: u64,
    every: bool,
) -> io::Result<(Option<u64>, Vec<Block>)> {
    let end = at.checked_add(size).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Matroska cluster range overflow",
        )
    })?;
    let (mut time, mut blocks) = (None, Vec::new());
    let mut child = at;
    let mut header = [0u8; 24];
    while child < end {
        let count = bounded_len(end - child, header.len());
        let read = read_at(file, child, &mut header[..count])?;
        let Some((id, Some(size), len)) = element(&header[..read]) else {
            break;
        };
        let Some(data) = child.checked_add(len as u64) else {
            break;
        };
        let Some(next) = data.checked_add(size).filter(|next| *next <= end) else {
            break;
        };
        match id {
            TIMESTAMP => {
                let mut value = [0u8; 8];
                let count = bounded_len(size, value.len());
                read_at(file, data, &mut value[..count])?;
                time = Some(uint(&value[..count]));
            }
            SIMPLE_BLOCK => {
                let count = bounded_len(size, read - len);
                let start = &header[len..len + count];
                blocks.extend(block(start, None));
            }
            BLOCK_GROUP if size <= 4096 => {
                let mut group = vec![0; bounded_len(size, 4096)];
                read_at(file, data, &mut group)?;
                if let Some((_, range, _)) =
                    children(&group).into_iter().find(|(id, ..)| *id == BLOCK)
                {
                    blocks.extend(block(&group[range], Some(&group)));
                }
            }
            _ => {}
        }
        if !every && !blocks.is_empty() {
            break;
        }
        child = next;
    }
    Ok((time, blocks))
}

/// The latest timestamp and the preceding distinct one, regardless of order.
fn latest_times(times: impl IntoIterator<Item = u64>) -> Option<(u64, Option<u64>)> {
    let mut times = times.into_iter();
    let last = times.next()?;
    Some(times.fold(
        (last, None),
        |(last, previous), time| match time.cmp(&last) {
            Ordering::Greater => (time, Some(last)),
            Ordering::Less => (
                last,
                Some(previous.map_or(time, |value: u64| value.max(time))),
            ),
            Ordering::Equal => (last, previous),
        },
    ))
}

/// Finish the Matroska file `file` `FFmpeg` wrote through a pipe, whose
/// start [`Rewriter`] read as `head`: add its index, where it may grow by
/// `room` bytes, and its length. Nothing is changed when its clusters
/// cannot be read as expected.
pub(super) fn finish(file: &mut File, head: &Head, room: u64) -> io::Result<()> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut cues = Vec::new();
    let mut last = None;
    let mut at = head.first_cluster;
    let mut header = [0u8; 12];
    while at < len {
        let count = bounded_len(len - at, header.len());
        let read = read_at(file, at, &mut header[..count])?;
        let Some((id, Some(size), header_len)) = element(&header[..read]) else {
            break;
        };
        let Some(data) = at.checked_add(header_len as u64) else {
            break;
        };
        let Some(next) = data.checked_add(size).filter(|next| *next <= len) else {
            break;
        };
        if id == CLUSTER {
            let (time, blocks) = cluster(file, data, size, false)?;
            let time = time.unwrap_or(0);
            if let Some(&(track, relative, key)) = blocks.first()
                && key
                && head.track.is_none_or(|video| video == track)
            {
                cues.push((
                    time.saturating_add_signed(relative),
                    track,
                    at - head.segment_data,
                ));
            }
            last = Some((data, size, time));
        }
        at = next;
    }
    // A file that ends inside an element, as one whose FFmpeg failed may,
    // takes nothing after it: it would be read as part of that element.
    let whole = at == len;
    let Some((data, size, time)) = last else {
        return Ok(());
    };
    // The length: to the end of the frame shown last.
    let (_, blocks) = cluster(file, data, size, true)?;
    let Some((shown_last, previous)) = latest_times(
        blocks
            .iter()
            .filter(|(track, ..)| head.track.is_none_or(|video| video == *track))
            .map(|(_, relative, _)| time.saturating_add_signed(*relative)),
    ) else {
        return Ok(());
    };
    let frame = head
        .frame_ns
        .map(|ns| ns / head.scale)
        .or_else(|| previous.map(|previous| shown_last - previous))
        .unwrap_or(0);
    #[deny(clippy::arithmetic_side_effects)]
    let Some(duration) = shown_last.checked_add(frame) else {
        return Ok(());
    };
    let duration = duration as f64;
    // The index, at the end, named in the SeekHead.
    let mut end = len;
    if let Some(SeekHead {
        offset: seek_at,
        space,
        entries: seeks,
        crc: has_crc,
    }) = &head.seek
        && !cues.is_empty()
        && whole
    {
        let points: Vec<u8> = cues
            .iter()
            .flat_map(|&(time, track, position)| {
                let positions = [
                    unsigned(CUE_TRACK, track),
                    unsigned(CUE_CLUSTER_POSITION, position),
                ]
                .concat();
                master(
                    CUE_POINT,
                    &[
                        unsigned(CUE_TIME, time),
                        master(CUE_TRACK_POSITIONS, &positions),
                    ]
                    .concat(),
                )
            })
            .collect();
        let index = master(CUES, &points);
        let seek = master(
            SEEK,
            &[
                master(SEEK_ID, &id_bytes(CUES)),
                unsigned(SEEK_POSITION, len - head.segment_data),
            ]
            .concat(),
        );
        let entries = [seeks.as_slice(), seek.as_slice()].concat();
        let body = if *has_crc {
            [
                &[
                    u8::try_from(CRC32).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "CRC-32 identifier exceeds one byte",
                        )
                    })?,
                    0x84,
                ][..],
                &crc(&entries),
                &entries,
            ]
            .concat()
        } else {
            entries
        };
        // A SeekHead and a Void that fill FFmpeg's space exactly.
        let filled = (1..=8).find_map(|size_len| {
            let mut seek_head = id_bytes(SEEK_HEAD);
            seek_head.extend(size_bytes(body.len() as u64, size_len));
            seek_head.extend_from_slice(&body);
            match space.checked_sub(seek_head.len())? {
                0 => Some(seek_head),
                rest => void(rest).map(|void| [seek_head, void].concat()),
            }
        });
        if let Some(filled) = filled
            && (index.len() as u64) <= room
        {
            write_at(file, len, &index)?;
            write_at(file, *seek_at, &filled)?;
            end = len + index.len() as u64;
        }
    }
    if let Some(size_at) = head.segment_size_at.filter(|_| whole) {
        write_at(file, size_at, &size_bytes(end - head.segment_data, 8))?;
    }
    let mut length = id_bytes(DURATION);
    length.push(0x88);
    length.extend(duration.to_be_bytes());
    write_at(file, head.duration_at, &length)?;
    let (info_end, crc_at) = head.info;
    if let Some(crc_at) = crc_at {
        let mut info = vec![
            0;
            usize::try_from(info_end - crc_at - 6).map_err(|_| io::Error::new(
                io::ErrorKind::InvalidData,
                "Matroska Info exceeds host address size"
            ))?
        ];
        read_at(file, crc_at + 6, &mut info)?;
        write_at(file, crc_at + 2, &crc(&info))?;
    }
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn metadata_reads_retry_interruptions_and_preserve_short_eof() {
        for (offset, expected) in [
            (2, b"2345".as_slice()),
            (4, b"45".as_slice()),
            (8, b"".as_slice()),
        ] {
            let mut reader = crate::test_io::FaultReader::new(
                io::Cursor::new(b"012345"),
                io::ErrorKind::Interrupted,
            );
            let mut bytes = [0; 4];
            let count = read_at(&mut reader, offset, &mut bytes).unwrap();
            assert_eq!(&bytes[..count], expected);
            assert!(bytes[count..].iter().all(|byte| *byte == 0));
        }
        let mut reader = crate::test_io::FaultReader::new(
            io::Cursor::new(b"012345"),
            io::ErrorKind::PermissionDenied,
        );
        assert_eq!(
            read_at(&mut reader, 0, &mut [0; 4]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn latest_distinct_times_match_sorting_for_reordered_and_duplicate_blocks() {
        let domain = [0, 1, 2, u64::MAX];
        for a in domain {
            for b in domain {
                for c in domain {
                    let values = [a, b, c];
                    for count in 0..=values.len() {
                        let values = &values[..count];
                        let mut sorted = values.to_vec();
                        sorted.sort_unstable();
                        sorted.dedup();
                        let expected = sorted.pop().map(|last| (last, sorted.pop()));
                        assert_eq!(latest_times(values.iter().copied()), expected, "{values:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn encoded_elements_match_fixed_wire_bytes_at_reserved_size_boundaries() {
        for (length, header) in [
            (0, &[0xa3, 0x80][..]),
            (126, &[0xa3, 0xfe][..]),
            (127, &[0xa3, 0x40, 0x7f][..]),
            (16_382, &[0xa3, 0x7f, 0xfe][..]),
            (16_383, &[0xa3, 0x20, 0x3f, 0xff][..]),
        ] {
            let payload = vec![0x5a; length];
            let encoded = master(SIMPLE_BLOCK, &payload);
            assert_eq!(&encoded[..header.len()], header, "payload length {length}");
            assert_eq!(&encoded[header.len()..], payload);
        }
        assert_eq!(master(0x4286, &[]), [0x42, 0x86, 0x80]);
        assert_eq!(master(TIMESTAMP_SCALE, &[]), [0x2a, 0xd7, 0xb1, 0x80]);
        assert_eq!(master(EBML, &[]), [0x1a, 0x45, 0xdf, 0xa3, 0x80]);
        assert_eq!(unsigned(TIMESTAMP, 0), [0xe7, 0x81, 0]);
        assert_eq!(unsigned(TIMESTAMP, 255), [0xe7, 0x81, 255]);
        assert_eq!(unsigned(TIMESTAMP, 256), [0xe7, 0x82, 1, 0]);
        assert_eq!(
            unsigned(TIMESTAMP, u64::MAX),
            [0xe7, 0x88, 255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn ebml_headers_check_every_supported_width_and_truncation() {
        assert_eq!(vint(&[]), None);
        assert_eq!(vint(&[0; 9]), None);
        for len in 1..=8 {
            let unknown = (1u64 << (7 * len)) - 1;
            let encoded = size_bytes(unknown - 1, len);
            assert_eq!(vint(&encoded), Some((Some(unknown - 1), len)));
            assert_eq!(vint(&size_bytes(unknown, len)), Some((None, len)));
            for end in 0..len {
                assert_eq!(vint(&encoded[..end]), None);
            }
        }
        for id in [0xa3, 0x4286, 0x002a_d7b1, EBML] {
            let mut encoded = id_bytes(id);
            encoded.push(0x80);
            assert_eq!(element(&encoded), Some((id, Some(0), encoded.len())));
            for end in 0..encoded.len() {
                assert_eq!(element(&encoded[..end]), None);
            }
        }
        assert_eq!(element(&[0x08, 0, 0, 0, 0, 0x80]), None);
    }

    fn binary(id: u32, data: &[u8]) -> Vec<u8> {
        master(id, data)
    }
    fn with_crc(id: u32, children: &[u8]) -> Vec<u8> {
        master(
            id,
            &[
                &[u8::try_from(CRC32).expect("CRC-32 has a one-byte ID"), 0x84][..],
                &crc(children),
                children,
            ]
            .concat(),
        )
    }
    fn simple(track: u8, relative: i16, key: bool, payload: usize) -> Vec<u8> {
        let mut data = vec![0x80 | track];
        data.extend(relative.to_be_bytes());
        data.push(if key { 0x80 } else { 0 });
        data.resize(4 + payload, 0x55);
        binary(SIMPLE_BLOCK, &data)
    }
    /// A file as `FFmpeg` writes one to a pipe, with its early Duration:
    /// clusters of three frames 40 ms apart, each starting with a key
    /// frame but the second.
    pub(in crate::media) fn piped() -> Vec<u8> {
        let header = master(EBML, &unsigned(0x4286, 1));
        let seeks = [
            binary(
                SEEK,
                &[
                    binary(SEEK_ID, &id_bytes(INFO)),
                    unsigned(SEEK_POSITION, 120),
                ]
                .concat(),
            ),
            binary(
                SEEK,
                &[
                    binary(SEEK_ID, &id_bytes(TRACKS)),
                    unsigned(SEEK_POSITION, 200),
                ]
                .concat(),
            ),
        ]
        .concat();
        let seek_head = with_crc(SEEK_HEAD, &seeks);
        let space = void(100 - seek_head.len()).unwrap();
        let mut early = id_bytes(DURATION);
        early.push(0x88);
        early.extend(31_536_000_000.0f64.to_be_bytes());
        let info = with_crc(
            INFO,
            &[unsigned(TIMESTAMP_SCALE, 1_000_000), early].concat(),
        );
        let tracks = master(
            TRACKS,
            &master(
                TRACK_ENTRY,
                &[unsigned(TRACK_NUMBER, 1), unsigned(TRACK_TYPE, 1)].concat(),
            ),
        );
        let clusters: Vec<u8> = (0..4u64)
            .flat_map(|index| {
                let blocks: Vec<u8> = (0..3)
                    .flat_map(|frame| simple(1, frame * 40, frame == 0 && index != 1, 300))
                    .collect();
                with_crc(
                    CLUSTER,
                    &[unsigned(TIMESTAMP, index * 120), blocks].concat(),
                )
            })
            .collect();
        let mut segment = id_bytes(SEGMENT);
        segment.extend([0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        [header, segment, seek_head, space, info, tracks, clusters].concat()
    }

    /// Element `id`'s data among the top-level elements of the segment.
    fn top(file: &[u8], wanted: u32) -> Option<(usize, Vec<u8>)> {
        let (_, Some(size), header) = element(file)? else {
            return None;
        };
        let mut at = header.checked_add(usize::try_from(size).ok()?)?;
        let (_, _, header) = element(file.get(at..)?)?;
        at = at.checked_add(header)?;
        while let Some((id, Some(size), header)) = element(file.get(at..)?) {
            let start = at.checked_add(header)?;
            let end = start.checked_add(usize::try_from(size).ok()?)?;
            let data = file.get(start..end)?;
            if id == wanted {
                return Some((at, data.to_vec()));
            }
            at = end;
        }
        None
    }
    fn crc_holds(data: &[u8]) -> bool {
        let (header, body) = data.split_first_chunk::<6>().expect("CRC-32 header");
        header[..2] == [u8::try_from(CRC32).expect("CRC-32 has a one-byte ID"), 0x84]
            && header[2..] == crc(body)
    }

    #[test]
    fn a_piped_file_gets_its_length_and_index() {
        let original = piped();
        // Its start is held until the first cluster, however it arrives.
        let mut rewriter = Rewriter::new();
        let mut written = Vec::new();
        for chunk in original.chunks(7) {
            written.extend_from_slice(&rewriter.push(chunk));
        }
        written.extend(rewriter.flush());
        let head = rewriter.head().expect("the start is read");
        assert_eq!(written.len(), original.len());
        // Until it is finished, a Void stands where the Duration was.
        let (_, info) = top(&written, INFO).unwrap();
        assert!(crc_holds(&info));
        assert!(children(&info).iter().all(|(id, ..)| *id != DURATION));
        let path = std::env::temp_dir().join(format!(
            "capturefab-mkv-{}-{}.mkv",
            std::process::id(),
            crate::types::random_hex(4).unwrap()
        ));
        std::fs::write(&path, &written).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        finish(&mut file, &head, u64::MAX).unwrap();
        drop(file);
        let finished = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        // 4 clusters of 3 frames 40 ms apart: 480 ms.
        let (_, info) = top(&finished, INFO).unwrap();
        assert!(crc_holds(&info));
        let found = children(&info);
        let (_, range, _) = found.iter().find(|(id, ..)| *id == DURATION).unwrap();
        assert_eq!(
            f64::from_be_bytes(info[range.clone()].try_into().unwrap()),
            480.0
        );
        // Cues for the clusters that start with a key frame, which the
        // SeekHead names.
        let (cues_at, cues) = top(&finished, CUES).unwrap();
        let points = children(&cues);
        assert_eq!(points.len(), 3);
        let times: Vec<u64> = points
            .iter()
            .map(|(_, point, _)| {
                let point = &cues[point.clone()];
                let (_, time, _) = children(point)
                    .into_iter()
                    .find(|(id, ..)| *id == CUE_TIME)
                    .unwrap();
                uint(&point[time])
            })
            .collect();
        assert_eq!(times, [0, 240, 360]);
        // The segment's positions count from its data, after its header.
        let segment_data = head.segment_data;
        let (_, ebml_size, ebml_header) = element(&finished).unwrap();
        assert_eq!(
            segment_data,
            ebml_header as u64 + ebml_size.unwrap() + 4 + 8
        );
        let (seek_at, seek_head) = top(&finished, SEEK_HEAD).unwrap();
        assert!(crc_holds(&seek_head));
        let named = children(&seek_head)
            .into_iter()
            .filter(|(id, ..)| *id == SEEK)
            .map(|(_, seek, _)| {
                let seek = &seek_head[seek];
                let fields = children(seek);
                (
                    seek[fields[0].1.clone()].to_vec(),
                    uint(&seek[fields[1].1.clone()]),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(named.len(), 3);
        assert_eq!(named[2], (id_bytes(CUES), cues_at as u64 - segment_data));
        // The space FFmpeg left after the SeekHead is filled exactly, and
        // the segment has its size.
        let (info_at, _) = top(&finished, INFO).unwrap();
        let (void_at, _) = top(&finished, VOID).unwrap();
        assert!(seek_at < void_at && void_at < info_at);
        let (before, _) = top(&written, INFO).unwrap();
        assert_eq!(info_at, before);
        let (_, size, _) =
            element(&finished[usize::try_from(segment_data).unwrap() - 12..]).unwrap();
        assert_eq!(size, Some(finished.len() as u64 - segment_data));
        // A file with no room left for an index still gets its length.
        std::fs::write(&path, &written).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        finish(&mut file, &head, 10).unwrap();
        drop(file);
        let finished = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(finished.len(), written.len());
        assert!(top(&finished, CUES).is_none());
        let (_, info) = top(&finished, INFO).unwrap();
        assert!(crc_holds(&info));
        assert!(children(&info).iter().any(|(id, ..)| *id == DURATION));
        // One that ends inside its last cluster gets its length, from the
        // clusters before, and nothing after the cut.
        let cut = &written[..written.len() - 10];
        std::fs::write(&path, cut).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        finish(&mut file, &head, u64::MAX).unwrap();
        drop(file);
        let finished = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(finished.len(), cut.len());
        assert!(!finished.windows(4).any(|bytes| bytes == id_bytes(CUES)));
        let (_, size, _) =
            element(&finished[usize::try_from(segment_data).unwrap() - 12..]).unwrap();
        assert_eq!(size, None);
        let duration_at = usize::try_from(head.duration_at).unwrap();
        assert_eq!(finished[duration_at..duration_at + 3], [0x44, 0x89, 0x88]);
        let length = f64::from_be_bytes(
            finished[duration_at + 3..duration_at + 11]
                .try_into()
                .unwrap(),
        );
        assert_eq!(length, 360.0);
    }

    #[test]
    fn overflowing_frame_end_leaves_the_recording_unchanged() {
        let mut rewriter = Rewriter::new();
        let mut written = rewriter.push(&piped()).into_owned();
        written.extend(rewriter.flush());
        let head = rewriter.head().unwrap();
        written.truncate(usize::try_from(head.first_cluster).unwrap());
        written.extend(with_crc(
            CLUSTER,
            &[
                unsigned(TIMESTAMP, u64::MAX - 1),
                simple(1, 0, true, 1),
                simple(1, 1, false, 1),
            ]
            .concat(),
        ));
        let path = std::env::temp_dir().join(format!(
            "capturefab-mkv-overflow-{}-{}.mkv",
            std::process::id(),
            crate::types::random_hex(4).unwrap()
        ));
        std::fs::write(&path, &written).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let result = finish(&mut file, &head, u64::MAX);
        drop(file);
        let finished = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        result.unwrap();
        assert_eq!(finished, written);
    }

    #[test]
    fn cluster_children_cannot_read_past_their_declared_ranges() {
        let prefix = [master(SIMPLE_BLOCK, &[0x81]), void(6).unwrap()].concat();
        let timestamp = unsigned(TIMESTAMP, 0x0102_0304_0506_0708);
        let bytes = [(&*prefix), &timestamp].concat();
        let path = std::env::temp_dir().join(format!(
            "capturefab-mkv-bounds-{}-{}.mkv",
            std::process::id(),
            crate::types::random_hex(4).unwrap()
        ));
        std::fs::write(&path, bytes).unwrap();
        let mut file = File::open(&path).unwrap();
        // A one-byte block cannot borrow its timestamp and flags from Void.
        let (time, blocks) = cluster(&mut file, 0, prefix.len() as u64, true).unwrap();
        assert!(time.is_none() && blocks.is_empty());
        // The timestamp's final byte exists in the file, outside the cluster.
        let (time, blocks) = cluster(
            &mut file,
            prefix.len() as u64,
            timestamp.len() as u64 - 1,
            true,
        )
        .unwrap();
        assert!(time.is_none() && blocks.is_empty());
        assert_eq!(
            cluster(&mut file, u64::MAX - 1, 8, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn what_cannot_be_read_goes_on_as_it_is() {
        let mut rewriter = Rewriter::new();
        let junk = vec![0x42; 64];
        assert_eq!(rewriter.push(&junk)[..], junk[..]);
        assert!(rewriter.head().is_none());
        assert_eq!(rewriter.push(b"more")[..], b"more"[..]);
        // A file cut before its first cluster still loses its Duration.
        let original = piped();
        let first_cluster = original
            .windows(4)
            .position(|bytes| bytes == id_bytes(CLUSTER))
            .unwrap();
        let mut rewriter = Rewriter::new();
        assert!(rewriter.push(&original[..first_cluster]).is_empty());
        let start = rewriter.flush();
        assert_eq!(start.len(), first_cluster);
        assert!(!start.windows(2).any(|bytes| bytes == [0x44, 0x89]));
        assert!(rewriter.head().is_none());
    }

    #[test]
    fn voids_fill_any_space() {
        for len in [2, 3, 9, 128, 129, 200, 1 << 16] {
            let void = void(len).unwrap();
            assert_eq!(void.len(), len);
            let (id, size, header) = element(&void).unwrap();
            assert_eq!(
                (id, usize::try_from(size.unwrap()).unwrap() + header),
                (VOID, len)
            );
        }
        assert!(void(1).is_none());
    }
}
