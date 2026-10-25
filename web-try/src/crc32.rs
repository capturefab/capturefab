//! CRC-32 (ISO 3309 / ITU-T V.42, polynomial 0xEDB88320 reflected), as PNG
//! chunks and ZIP entries use it. Streaming with zlib's convention: start
//! from 0 and pass each result back in with the next bytes.

const fn table() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        tables[0][n] = c;
        n += 1;
    }
    let mut n = 0;
    while n < 256 {
        let mut t = 1;
        while t < 8 {
            let previous = tables[t - 1][n];
            tables[t][n] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            t += 1;
        }
        n += 1;
    }
    tables
}

/// Slicing-by-8 tables.
static TABLES: [[u32; 256]; 8] = table();

/// The CRC-32 of everything hashed so far followed by `bytes`; `crc` is the
/// previous result, or 0 to start.
pub fn update(crc: u32, bytes: &[u8]) -> u32 {
    let mut c = !crc;
    let (blocks, tail) = bytes.as_chunks::<8>();
    for block in blocks {
        let low = c ^ u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        c = TABLES[7][(low & 0xff) as usize]
            ^ TABLES[6][((low >> 8) & 0xff) as usize]
            ^ TABLES[5][((low >> 16) & 0xff) as usize]
            ^ TABLES[4][(low >> 24) as usize]
            ^ TABLES[3][block[4] as usize]
            ^ TABLES[2][block[5] as usize]
            ^ TABLES[1][block[6] as usize]
            ^ TABLES[0][block[7] as usize];
    }
    for &byte in tail {
        c = TABLES[0][((c ^ u32::from(byte)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// The CRC-32 of `bytes`.
pub fn checksum(bytes: &[u8]) -> u32 {
    update(0, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(checksum(b""), 0);
        assert_eq!(checksum(b"a"), 0xE8B7_BE43);
        assert_eq!(checksum(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            checksum(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
        // PNG's IEND chunk: CRC over the type alone.
        assert_eq!(checksum(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn streaming_matches_one_pass() {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let whole = checksum(&bytes);
        for split in [0, 1, 7, 8, 9, 500, 999, 1000] {
            let (a, b) = bytes.split_at(split);
            assert_eq!(update(update(0, a), b), whole, "{split}");
        }
    }
}
