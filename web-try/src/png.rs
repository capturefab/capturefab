//! PNG stills laid out as the native `frame::encode_with_meta` writes them
//! with the `png` crate: signature, IHDR (8-bit gray for the monochrome
//! family, else 8-bit RGB), an iTXt `Description` holding the context tags
//! when there are any, an iTXt `capturefab` holding the provenance JSON,
//! IDAT and IEND. Rows use the fixed Paeth filter, like the native encoder.
//!
//! The zlib stream between the two steps comes from the browser's
//! `CompressionStream('deflate')`, so the IDAT bytes differ from the native
//! file while the decoded pixels and every other chunk are identical.
use crate::Result;
use crate::crc32;
use crate::frame;
use crate::types::Frame;

pub const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";
/// PNG color types used here.
pub const GRAYSCALE: u8 = 0;
pub const RGB: u8 = 2;
/// The `png` crate's largest IDAT chunk.
const MAX_IDAT: usize = (u32::MAX >> 1) as usize;

/// The samples a PNG of `frame` stores and their color type: one gray
/// channel for the monochrome family (via `gray8`), RGB otherwise.
pub fn samples(frame: &Frame) -> Result<(u8, Vec<u8>)> {
    Ok(match frame::gray8(frame)? {
        Some(gray) => (GRAYSCALE, gray),
        None => (RGB, frame::rgb(frame)?),
    })
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (i16::from(a), i16::from(b), i16::from(c));
    let pa = (ib - ic).abs();
    let pb = (ia - ic).abs();
    let pc = (ia + ib - 2 * ic).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Scanlines ready for zlib: each row is filter type 4 (Paeth) followed by
/// its filtered bytes, `channels` bytes per pixel.
pub fn filtered_rows(samples: &[u8], width: u32, height: u32, channels: usize) -> Vec<u8> {
    let row = width as usize * channels;
    let mut out = Vec::with_capacity((row + 1) * height as usize);
    let zeros = vec![0; row];
    let mut previous: &[u8] = &zeros;
    for line in samples.chunks_exact(row).take(height as usize) {
        out.push(4);
        for i in 0..row {
            let a = if i >= channels { line[i - channels] } else { 0 };
            let c = if i >= channels {
                previous[i - channels]
            } else {
                0
            };
            out.push(line[i].wrapping_sub(paeth(a, previous[i], c)));
        }
        previous = line;
    }
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32::update(crc32::checksum(kind), data);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An uncompressed iTXt chunk body with no language tag or translated
/// keyword, as `png::Encoder::add_itxt_chunk` writes it.
fn itxt(keyword: &str, text: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(keyword.len() + text.len() + 5);
    data.extend_from_slice(keyword.as_bytes());
    data.extend_from_slice(&[0, 0, 0, 0, 0]);
    data.extend_from_slice(text.as_bytes());
    data
}

/// A complete PNG from a zlib stream of [`filtered_rows`]. `meta` is the
/// compact provenance JSON and `description` the `key=value; …` context
/// tags; the Description chunk is written only with metadata, as natively.
pub fn assemble(
    width: u32,
    height: u32,
    color_type: u8,
    zlib: &[u8],
    meta: Option<&str>,
    description: Option<&str>,
) -> Result<Vec<u8>> {
    ensure!(
        width > 0 && height > 0,
        "image dimensions exceed conversion limit"
    );
    ensure!(
        matches!(color_type, GRAYSCALE | RGB),
        "unsupported PNG color type {color_type}"
    );
    if let Some(meta) = meta {
        frame::check_meta(meta.len())?;
    }
    let mut out = Vec::with_capacity(zlib.len() + meta.map_or(0, str::len) + 256);
    out.extend_from_slice(&SIGNATURE);
    let mut header = [0u8; 13];
    header[..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8] = 8;
    header[9] = color_type;
    chunk(&mut out, b"IHDR", &header);
    if let Some(meta) = meta {
        if let Some(description) = description.filter(|d| !d.is_empty()) {
            chunk(&mut out, b"iTXt", &itxt("Description", description));
        }
        chunk(&mut out, b"iTXt", &itxt("capturefab", meta));
    }
    for part in zlib.chunks(MAX_IDAT) {
        chunk(&mut out, b"IDAT", part);
    }
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reverse the Paeth filter, as a decoder does.
    pub fn unfilter(rows: &[u8], width: u32, channels: usize) -> Vec<u8> {
        let row = width as usize * channels;
        let mut out: Vec<u8> = Vec::new();
        for (y, line) in rows.chunks_exact(row + 1).enumerate() {
            assert_eq!(line[0], 4);
            for i in 0..row {
                let a = if i >= channels {
                    out[y * row + i - channels]
                } else {
                    0
                };
                let b = if y > 0 { out[(y - 1) * row + i] } else { 0 };
                let c = if y > 0 && i >= channels {
                    out[(y - 1) * row + i - channels]
                } else {
                    0
                };
                out.push(line[1 + i].wrapping_add(paeth(a, b, c)));
            }
        }
        out
    }

    #[test]
    fn paeth_matches_the_specification() {
        for (a, b, c) in [
            (0, 0, 0),
            (10, 20, 30),
            (200, 10, 100),
            (5, 5, 255),
            (255, 0, 128),
        ] {
            let p = i32::from(a) + i32::from(b) - i32::from(c);
            let (pa, pb, pc) = (
                (p - i32::from(a)).abs(),
                (p - i32::from(b)).abs(),
                (p - i32::from(c)).abs(),
            );
            let expected = if pa <= pb && pa <= pc {
                a
            } else if pb <= pc {
                b
            } else {
                c
            };
            assert_eq!(paeth(a, b, c), expected);
        }
    }

    #[test]
    fn filtered_rows_round_trip() {
        let mut state = 0x2545_f491_u32;
        for (width, height, channels) in [(1, 1, 1), (3, 2, 3), (33, 17, 1), (8, 5, 3)] {
            let samples: Vec<u8> = (0..width * height * channels as u32)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect();
            let rows = filtered_rows(&samples, width, height, channels);
            assert_eq!(
                rows.len(),
                (width as usize * channels + 1) * height as usize
            );
            assert_eq!(unfilter(&rows, width, channels), samples);
        }
    }

    #[test]
    fn chunks_carry_lengths_types_and_crcs() {
        let png = assemble(2, 1, GRAYSCALE, b"zz", Some("{}"), Some("a=b")).unwrap();
        assert_eq!(&png[..8], &SIGNATURE);
        assert_eq!(&png[8..16], b"\0\0\0\x0dIHDR");
        assert_eq!(&png[16..29], &[0, 0, 0, 2, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
        assert_eq!(
            u32::from_be_bytes(png[29..33].try_into().unwrap()),
            crc32::checksum(&png[12..29])
        );
        let mut at = 33;
        let mut seen = Vec::new();
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
            assert_eq!(crc, crc32::checksum(&png[at + 4..at + 8 + len]));
            seen.push(String::from_utf8(kind.to_vec()).unwrap());
            at += 12 + len;
        }
        assert_eq!(seen, ["iTXt", "iTXt", "IDAT", "IEND"]);
        assert!(png.ends_with(b"\0\0\0\0IEND\xae\x42\x60\x82"));
        // No metadata, no text chunks; a description alone is not written.
        let plain = assemble(2, 1, RGB, b"zz", None, Some("a=b")).unwrap();
        assert_eq!(plain.len(), 8 + 25 + 14 + 12);
        assert_eq!(
            assemble(2, 1, RGB, b"", Some(&"x".repeat(frame::MAX_META + 1)), None).unwrap_err(),
            "capture metadata exceeds 16384 bytes"
        );
    }
}
