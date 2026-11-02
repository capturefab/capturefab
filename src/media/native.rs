//! Frames delivered in the pixel format FFmpeg decodes them to: gray, YUV
//! 4:2:0, 4:2:2 or 4:4:4 (planar, semiplanar or packed), or RGB24 for RGB
//! sources, instead of every frame converted to RGB24 inside FFmpeg.
//!
//! FFmpeg writes them as raw video, one packet a frame straight to the pipe
//! (`-avioflags direct`). Raw video has no header, so FFmpeg's own log says
//! what it writes: the output stream's pixel format, color range, matrix
//! and size, on the info line it prints before the first frame. The size
//! stays the same for the life of the process (FFmpeg scales a source that
//! changes size mid-stream to the size it started with), so every frame is
//! one fixed number of bytes. A format Capturefab cannot carry as decoded
//! (another matrix, limited-range gray, packed 4:2:2 of odd width), or an
//! output FFmpeg did not describe, makes the decoder ask for RGB24 instead.
use crate::frame::{Matrix, ycbcr_planes};
use crate::types::{Frame, MONO8, RGB8};
use anyhow::{Context, Result, ensure};
use std::io::Read;

/// The decoded formats FFmpeg may hand over as they are: it picks the
/// source's own when it is one of these, else the closest (8 bits for a
/// deeper source).
pub(super) const PIX_FMTS: &str =
    "gray|nv12|yuv420p|yuvj420p|yuyv422|uyvy422|yuv422p|yuvj422p|yuv444p|yuvj444p|rgb24";

/// A color range FFmpeg names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Range {
    /// `tv`: video levels, 16-235 luma.
    Limited,
    /// `pc`: 0-255.
    Full,
}

/// The raw video FFmpeg says it writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Described {
    pub pix_fmt: String,
    pub width: u32,
    pub height: u32,
    pub range: Option<Range>,
    /// The YUV matrix's name, such as `bt709`; None when unspecified.
    pub colorspace: Option<String>,
}

/// The output stream an FFmpeg log line describes: `Video: rawvideo (FOURCC
/// / 0x…), PIX_FMT(RANGE, COLORSPACE/PRIMARIES/TRANSFER, FIELD), WxH …`,
/// each parenthesized part optional.
pub(super) fn describe(line: &str) -> Option<Described> {
    let rest = &line[line.find("Video: rawvideo")? + "Video: rawvideo".len()..];
    // The codec tag, if FFmpeg names one.
    let rest = match rest.strip_prefix(" (") {
        Some(tagged) => &tagged[tagged.find("), ")? + 1..],
        None => rest,
    };
    let rest = rest.strip_prefix(", ")?;
    let end = rest.find(['(', ',', ' ']).unwrap_or(rest.len());
    let pix_fmt = rest[..end].to_owned();
    let mut rest = &rest[end..];
    let (mut range, mut colorspace) = (None, None);
    if let Some(inner) = rest.strip_prefix('(') {
        let close = inner.find(')')?;
        for part in inner[..close].split(", ") {
            match part {
                "tv" => range = Some(Range::Limited),
                "pc" => range = Some(Range::Full),
                part => {
                    let space = part.split('/').next().unwrap_or(part);
                    if colorspace.is_none() && MATRICES.contains(&space) {
                        colorspace = Some(space.to_owned()).filter(|space| space != "unknown");
                    }
                }
            }
        }
        rest = &inner[close + 1..];
    }
    let size = rest.split([',', ' ']).find_map(|word| {
        word.split_once('x')
            .filter(|(w, h)| is_number(w) && is_number(h))
    })?;
    Some(Described {
        pix_fmt,
        width: size.0.parse().ok()?,
        height: size.1.parse().ok()?,
        range,
        colorspace,
    })
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The color space names FFmpeg prints (`av_color_space_name`).
const MATRICES: [&str; 17] = [
    "gbr",
    "bt709",
    "unknown",
    "reserved",
    "fcc",
    "bt470bg",
    "smpte170m",
    "smpte240m",
    "ycgco",
    "bt2020nc",
    "bt2020c",
    "smpte2085",
    "chroma-derived-nc",
    "chroma-derived-c",
    "ictcp",
    "ipt-c2",
    "ycgco-re",
];

/// The frame format of described raw video: its PFNC (or Capturefab) code
/// and the bytes of each frame. None for a format, range or matrix
/// Capturefab does not carry as decoded, and for packed 4:2:2 of odd width,
/// whose rows FFmpeg pads to whole pixel pairs where PFNC's end on the last
/// pixel. An unspecified matrix is BT.601 and an unspecified range video
/// levels (full for `yuvj` and gray), as FFmpeg's own conversions take them.
pub(super) fn frame_format(described: &Described) -> Option<(u32, usize)> {
    let full = match described.range {
        Some(range) => range == Range::Full,
        None => described.pix_fmt.starts_with("yuvj") || described.pix_fmt == "gray",
    };
    let (width, height) = (described.width, described.height);
    let sized = |code| {
        let len = crate::frame::frame_len(code, width, height)?;
        (width > 0 && height > 0 && len <= super::MAX_FRAME).then_some((code, len))
    };
    // RGB and gray have no matrix.
    match described.pix_fmt.as_str() {
        "rgb24" => return sized(RGB8),
        "gray" if full => return sized(MONO8),
        "gray" => return None,
        _ => {}
    }
    let matrix = match (described.colorspace.as_deref(), full) {
        (None | Some("bt470bg" | "smpte170m"), true) => Matrix::Full,
        (None | Some("bt470bg" | "smpte170m"), false) => Matrix::Bt601,
        (Some("bt709"), false) => Matrix::Bt709,
        (Some("bt709"), true) => Matrix::Full709,
        _ => return None,
    };
    let yuv = |sub, interleaved| ycbcr_planes(sub, matrix, interleaved);
    let code = match described.pix_fmt.as_str() {
        "nv12" => yuv((2, 2), true)?,
        "yuv420p" | "yuvj420p" => yuv((2, 2), false)?,
        "yuv422p" | "yuvj422p" => yuv((2, 1), false)?,
        "yuv444p" | "yuvj444p" => yuv((1, 1), false)?,
        "yuyv422" | "uyvy422" if !width.is_multiple_of(2) => return None,
        "yuyv422" => match matrix {
            Matrix::Full => 0x0210_0032,
            Matrix::Bt601 => 0x0210_003e,
            Matrix::Bt709 => 0x0210_0041,
            Matrix::Full709 => return None,
        },
        "uyvy422" => match matrix {
            Matrix::Full => 0x0210_001f,
            Matrix::Bt601 => 0x0210_0044,
            Matrix::Bt709 => 0x0210_0045,
            Matrix::Full709 => return None,
        },
        _ => return None,
    };
    sized(code)
}

/// One frame of `len` bytes from FFmpeg's raw video, read straight into a
/// pooled buffer (every byte is read, so a recycled one needs no zeroing).
pub(super) fn read_frame(
    reader: &mut impl Read,
    id: u64,
    (pixel_format, len): (u32, usize),
    described: &Described,
    pool: &crate::framebuf::FramePool,
) -> Result<Frame> {
    let mut data = pool.take_stale(len);
    let mut read = 0;
    while read < len {
        match reader.read(&mut data[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("cannot read FFmpeg's frames"),
        }
    }
    ensure!(read > 0, "media stream ended");
    ensure!(read == len, "truncated FFmpeg frame");
    crate::copies::record(crate::copies::Site::PipeRead, len);
    Ok(Frame {
        id,
        width: described.width,
        height: described.height,
        pixel_format,
        device_ts_ns: None,
        wall_ms: 0,
        data: data.freeze(),
    })
}

/// The video stream FFmpeg reads: its codec, size and whether its samples
/// are wider than 8 bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Input {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub deep: bool,
}

/// What a line of FFmpeg's log says.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Said {
    /// An error, its level tag dropped, as `-loglevel error` would print it.
    Error(Vec<u8>),
    Input(Input),
    Output(Described),
}

/// The input stream an FFmpeg log line describes: `Video: CODEC (PROFILE)
/// …, PIX_FMT(…), WxH …`.
fn input(line: &str) -> Option<Input> {
    let rest = &line[line.find("Video: ")? + "Video: ".len()..];
    let codec = rest.split([' ', ',']).next()?.to_owned();
    let (width, height) = rest.split([',', ' ']).find_map(|word| {
        word.split_once('x')
            .filter(|(w, h)| is_number(w) && is_number(h))
    })?;
    // The pixel format follows the codec's part, whose parentheses hold
    // no commas. FFmpeg's names of formats wider than 8 bits end in their
    // byte order, such as `yuv420p10le`.
    let pix_fmt = rest.split(", ").nth(1).unwrap_or_default();
    let pix_fmt = pix_fmt.split(['(', ' ']).next().unwrap_or_default();
    Some(Input {
        codec,
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        deep: pix_fmt.ends_with("le") || pix_fmt.ends_with("be"),
    })
}

/// FFmpeg's log at `-loglevel level+info`, a line at a time: its errors,
/// for the failure a stream ends with, the input video it reads and the
/// raw video it writes.
#[derive(Default)]
pub(super) struct Lines {
    partial: Vec<u8>,
    /// The log has reached the output's section.
    output: bool,
    /// The input video has been described.
    input: bool,
}

impl Lines {
    /// Take `bytes`, telling `said` what each whole line says.
    pub(super) fn push(&mut self, bytes: &[u8], said: &mut impl FnMut(Said)) {
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.partial);
                self.line(&line, said);
            } else if self.partial.len() < 4096 {
                self.partial.push(byte);
            }
        }
    }
    /// The last line, when the log ends without a newline.
    pub(super) fn finish(&mut self, said: &mut impl FnMut(Said)) {
        let line = std::mem::take(&mut self.partial);
        if !line.is_empty() {
            self.line(&line, said);
        }
    }
    fn line(&mut self, line: &[u8], said: &mut impl FnMut(Said)) {
        let text = String::from_utf8_lossy(line);
        let tag = ["[panic] ", "[fatal] ", "[error] ", "[warning] ", "[info] "]
            .into_iter()
            .chain(["[verbose] ", "[debug] ", "[trace] "])
            .find_map(|tag| text.find(tag).map(|at| (tag, at)));
        match tag {
            Some((tag @ ("[panic] " | "[fatal] " | "[error] "), at)) => {
                let mut kept = text[..at].to_owned();
                kept.push_str(&text[at + tag.len()..]);
                kept.push('\n');
                said(Said::Error(kept.into_bytes()));
            }
            Some(("[info] ", _)) => {
                if text.contains("Output #0,") {
                    self.output = true;
                } else if !text.contains("Stream #") {
                } else if self.output {
                    if let Some(output) = describe(&text) {
                        self.output = false;
                        said(Said::Output(output));
                    }
                } else if !self.input
                    && let Some(input) = input(&text)
                {
                    self.input = true;
                    said(Said::Input(input));
                }
            }
            Some(_) => {}
            // A line without a level continues the one before it, such as
            // "Last message repeated".
            None => {
                let mut kept = line.to_vec();
                kept.push(b'\n');
                said(Said::Error(kept));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_streams_are_described_from_ffmpeg_s_log() {
        let described = |line| describe(line).unwrap();
        let hd = described(
            "[info]   Stream #0:0(und): Video: rawvideo (NV12 / 0x3231564E), nv12(tv, bt709/unknown/unknown, progressive), 1920x1080 [SAR 1:1 DAR 16:9], q=2-31, 746496 kb/s, 30 fps, 30 tbn (default)",
        );
        assert_eq!(
            hd,
            Described {
                pix_fmt: "nv12".into(),
                width: 1920,
                height: 1080,
                range: Some(Range::Limited),
                colorspace: Some("bt709".into()),
            }
        );
        let plain = described(
            "  Stream #0:0: Video: rawvideo (I420 / 0x30323449), yuv420p(progressive), 320x240 [SAR 1:1 DAR 4:3], q=2-31, 27648 kb/s, 30 fps, 30 tbn",
        );
        assert_eq!((plain.range, plain.colorspace), (None, None));
        assert_eq!((plain.width, plain.height), (320, 240));
        let jpeg = described(
            "Stream #0:0: Video: rawvideo (I420 / 0x30323449), yuvj420p(pc, progressive), 640x480, 30 fps",
        );
        assert_eq!(
            (jpeg.pix_fmt.as_str(), jpeg.range),
            ("yuvj420p", Some(Range::Full))
        );
        let untagged = described("Stream #0:0: Video: rawvideo, gray, 33x17, 1 fps");
        assert_eq!((untagged.pix_fmt.as_str(), untagged.width), ("gray", 33));
        let primaries_only =
            described("Video: rawvideo (UYVY / 0x59565955), uyvy422(unknown/bt709/bt709), 64x48");
        assert_eq!(primaries_only.colorspace, None);
        assert_eq!(
            describe("Stream #0:0: Video: h264 (High), yuv420p, 1920x1080"),
            None
        );
        assert_eq!(describe("Video: rawvideo, nv12"), None);
    }

    #[test]
    fn decoded_formats_keep_their_range_and_matrix() {
        let format = |pix_fmt: &str, range, colorspace: Option<&str>| {
            frame_format(&Described {
                pix_fmt: pix_fmt.into(),
                width: 5,
                height: 3,
                range,
                colorspace: colorspace.map(Into::into),
            })
        };
        use Range::{Full, Limited};
        // Luma 15 bytes, two 3x2 chroma planes.
        assert_eq!(format("yuv420p", None, None), Some((0x820c_0002, 27)));
        assert_eq!(format("yuvj420p", None, None), Some((0x820c_0001, 27)));
        assert_eq!(
            format("yuv420p", Some(Limited), Some("bt709")),
            Some((0x820c_0003, 27))
        );
        assert_eq!(
            format("nv12", Some(Full), Some("bt470bg")),
            Some((0x020c_0112, 27))
        );
        assert_eq!(
            format("nv12", None, Some("smpte170m")),
            Some((crate::frame::NV12_601, 27))
        );
        assert_eq!(
            format("yuv422p", Some(Limited), None),
            Some((0x8210_0002, 15 + 18))
        );
        assert_eq!(format("yuv444p", Some(Full), None), Some((0x8218_0001, 45)));
        let even = |pix_fmt: &str, range, colorspace: Option<&str>| {
            frame_format(&Described {
                pix_fmt: pix_fmt.into(),
                width: 6,
                height: 3,
                range,
                colorspace: colorspace.map(Into::into),
            })
        };
        assert_eq!(even("yuyv422", None, None), Some((0x0210_003e, 36)));
        assert_eq!(even("uyvy422", Some(Full), None), Some((0x0210_001f, 36)));
        assert_eq!(
            even("uyvy422", None, Some("bt709")),
            Some((0x0210_0045, 36))
        );
        // Full-range BT.709, as network cameras often tag their video.
        assert_eq!(
            format("yuvj420p", Some(Full), Some("bt709")),
            Some((0x820c_0004, 27))
        );
        assert_eq!(
            format("nv12", Some(Full), Some("bt709")),
            Some((0x820c_0013, 27))
        );
        assert_eq!(format("gray", None, None), Some((MONO8, 15)));
        assert_eq!(format("rgb24", None, None), Some((RGB8, 45)));
        // What Capturefab asks for as RGB instead.
        assert_eq!(format("gray", Some(Limited), None), None);
        assert_eq!(even("yuyv422", Some(Full), Some("bt709")), None);
        // FFmpeg pads an odd row to a whole pixel pair; PFNC does not.
        assert_eq!(format("yuyv422", None, None), None);
        assert_eq!(format("uyvy422", Some(Full), None), None);
        assert_eq!(format("yuv420p", None, Some("bt2020nc")), None);
        assert_eq!(format("p010le", None, None), None);
    }

    #[test]
    fn logs_keep_errors_and_describe_the_input_and_output() {
        let mut lines = Lines::default();
        let mut said = Vec::new();
        let log = "[info]   Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(tv, bt709), 1920x1080 [SAR 1:1 DAR 16:9], 30 fps\n\
                   [in#0 @ 0x1] [warning] Packet corrupt\n\
                   [info] Output #0, rawvideo, to 'pipe:1':\n\
                   [info]   Stream #0:0: Video: rawvideo (NV12 / 0x3231564E), nv12(tv), 64x48, 30 fps\n\
                   [vist#0:0/h264 @ 0x2] [error] Invalid data found\n    Last message repeated 1 times\n\
                   [fatal] Conversion failed!";
        for chunk in log.as_bytes().chunks(7) {
            lines.push(chunk, &mut |line| said.push(line));
        }
        lines.finish(&mut |line| said.push(line));
        let errors: Vec<u8> = said
            .iter()
            .filter_map(|said| match said {
                Said::Error(line) => Some(line.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            String::from_utf8(errors).unwrap(),
            "[vist#0:0/h264 @ 0x2] Invalid data found\n    Last message repeated 1 times\nConversion failed!\n"
        );
        assert_eq!(
            said[0],
            Said::Input(Input {
                codec: "h264".into(),
                width: 1920,
                height: 1080,
                deep: false,
            })
        );
        let deep = input(
            "[info]   Stream #0:0[0x1](und): Video: hevc (Main 10) (hvc1 / 0x31637668), yuv420p10le(tv, bt709, progressive), 3840x2160, 30 fps",
        )
        .unwrap();
        assert_eq!(
            (deep.codec.as_str(), deep.width, deep.deep),
            ("hevc", 3840, true)
        );
        let plain = input("Stream #0:0: Video: mjpeg (Baseline), yuvj422p(pc, bt470bg/unknown/unknown), 1280x720, 30 fps").unwrap();
        assert!(!plain.deep);
        let outputs: Vec<_> = said
            .iter()
            .filter_map(|said| match said {
                Said::Output(output) => Some(output),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            (outputs[0].pix_fmt.as_str(), outputs[0].width),
            ("nv12", 64)
        );
    }
}
