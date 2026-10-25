//! Port of `insert_com` and `well_formed` from the native `src/jpeg.rs`.
//! The browser encodes the JPEG itself; this adds the provenance comment.
use crate::Result;

/// A complete baseline JPEG: SOI, a frame header with the expected size,
/// EOI.
pub fn well_formed(bytes: &[u8], width: u32, height: u32) -> bool {
    if bytes.len() < 4 || bytes[..2] != [0xFF, 0xD8] || bytes[bytes.len() - 2..] != [0xFF, 0xD9] {
        return false;
    }
    let mut i = 2;
    while i + 4 <= bytes.len() && bytes[i] == 0xFF {
        let marker = bytes[i + 1];
        let length = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // Baseline, extended or progressive Huffman frame header.
        if matches!(marker, 0xC0..=0xC2) {
            return bytes.get(i + 5..i + 9).is_some_and(|size| {
                u16::from_be_bytes([size[0], size[1]]) as u32 == height
                    && u16::from_be_bytes([size[2], size[3]]) as u32 == width
            });
        }
        if marker == 0xDA || length < 2 {
            return false;
        }
        i += 2 + length;
    }
    false
}

/// Add a COM (comment) segment holding `payload` to an encoded JPEG, after
/// SOI and any leading APPn segments so a JFIF or EXIF header stays first.
pub fn insert_com(jpeg: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        payload.len() <= usize::from(u16::MAX) - 2,
        "JPEG comment is too long"
    );
    ensure!(
        jpeg.len() >= 4 && jpeg[..2] == [0xFF, 0xD8],
        "not a JPEG image"
    );
    let mut at = 2;
    while let [0xFF, 0xE0..=0xEF, high, low, ..] = jpeg[at..] {
        let length = usize::from(u16::from_be_bytes([high, low]));
        ensure!(
            length >= 2 && at + 2 + length <= jpeg.len(),
            "malformed JPEG header"
        );
        at += 2 + length;
    }
    let mut out = Vec::with_capacity(jpeg.len() + payload.len() + 4);
    out.extend_from_slice(&jpeg[..at]);
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend_from_slice(&(payload.len() as u16 + 2).to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&jpeg[at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_follow_app_segments() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xC0, 0, 2, 0xFF, 0xD9,
        ];
        let out = insert_com(&jpeg, b"{\"a\":1}").unwrap();
        assert_eq!(&out[..8], &jpeg[..8]);
        assert_eq!(&out[8..12], &[0xFF, 0xFE, 0, 9]);
        assert_eq!(&out[12..19], b"{\"a\":1}");
        assert_eq!(&out[19..], &jpeg[8..]);
        let bare = insert_com(&[0xFF, 0xD8, 0xFF, 0xD9], b"x").unwrap();
        assert_eq!(bare, [0xFF, 0xD8, 0xFF, 0xFE, 0, 3, b'x', 0xFF, 0xD9]);
        assert_eq!(
            insert_com(&jpeg, &vec![0; 65_534]).unwrap_err(),
            "JPEG comment is too long"
        );
        assert_eq!(
            insert_com(b"\x89PNG", b"x").unwrap_err(),
            "not a JPEG image"
        );
        assert_eq!(
            insert_com(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 40, 0xFF, 0xD9], b"x").unwrap_err(),
            "malformed JPEG header"
        );
    }

    #[test]
    fn well_formed_needs_the_frame_size() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 1, 2, 0xFF, 0xC0, 0, 11, 8, 0, 1, 0, 2, 1, 1, 0x11, 0,
            0xFF, 0xD9,
        ];
        assert!(well_formed(&jpeg, 2, 1));
        assert!(!well_formed(&jpeg, 1, 2));
        assert!(!well_formed(&jpeg[..jpeg.len() - 1], 2, 1));
    }
}
