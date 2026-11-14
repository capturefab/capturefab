//! Word tags from the optoNCDT2300 manual, printed pp85–86 (2015 edition).
//! A single output is both first and last; the second-MSB of H must be clear.
use super::malformed;
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Framing {
    raw: [u8; 3],
    length: usize,
    synchronized: bool,
    alignment: [u8; 2],
    alignment_length: usize,
}

impl Framing {
    pub(super) fn partial(&self) -> bool {
        self.length != 0 || self.alignment_length != 0
    }
    pub(super) fn wanted(&self) -> usize {
        if self.length == 0 { 1 } else { 3 - self.length }
    }
    pub(super) fn push(&mut self, byte: u8) -> Result<Option<Word>> {
        let tag = byte & 0xc0;
        if self.length == 0 && !self.synchronized {
            match (self.alignment_length, tag) {
                (0..=2, 0)
                    if self.alignment_length != 1
                        || self.alignment.first().is_some_and(|b| b & 0xc0 == 0x80) => {}
                (0, 0x40 | 0x80) => {
                    self.alignment[0] = byte;
                    self.alignment_length = 1;
                    return Ok(None);
                }
                (1, 0x80) if self.alignment.first().is_some_and(|b| b & 0xc0 == 0x40) => {
                    self.alignment[1] = byte;
                    self.alignment_length = 2;
                    return Ok(None);
                }
                _ => {
                    return Err(malformed(
                        "startup must be L, H,L or M,H,L; no arbitrary resynchronization",
                    ));
                }
            }
            self.synchronized = true;
        }
        let expected_tag = match self.length {
            0 => 0,
            1 => 0x40,
            2 => 0x80,
            _ => return Err(malformed("word extent overflow")),
        };
        if tag != expected_tag {
            return Err(malformed(
                "expected exact L/M/H tags00/01/10; configure DIST1 only",
            ));
        }
        let slot = self
            .raw
            .get_mut(self.length)
            .ok_or_else(|| malformed("word extent overflow"))?;
        *slot = byte;
        self.length += 1;
        if self.length != self.raw.len() {
            return Ok(None);
        }
        self.length = 0;
        let word = Word {
            raw: self.raw,
            alignment: self.alignment,
            alignment_length: self.alignment_length,
        };
        self.alignment_length = 0;
        Ok(Some(word))
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Word {
    pub raw: [u8; 3],
    alignment: [u8; 2],
    alignment_length: usize,
}

impl Word {
    pub(super) fn alignment(&self) -> &[u8] {
        self.alignment
            .get(..self.alignment_length)
            .expect("bounded initial alignment")
    }
    pub(super) fn value(self, profile: &str) -> Value {
        let [low, middle, high] = self.raw;
        let code = u32::from(low & 0x3f)
            | (u32::from(middle & 0x3f) << 6)
            | (u32::from(high & 0x3f) << 12);
        let mut value = json!({
            "kind":"microepsilon_rs422", "profile":profile,
            "configuration_status":"configured_unverified", "code_bits":18,
            "raw":{"encoding":"base64","bytes":3,"data":STANDARD.encode(self.raw)},
            "raw_code":code, "decode_status":"raw_code",
            "checksum_status":"not_in_word_format", "freshness_status":"not_proven"
        });
        if let Some(error) = error_name(code) {
            value["decode_status"] = json!("device_error");
            value["error"] = json!(error);
        }
        if !self.alignment().is_empty() {
            value["initial_alignment"] = json!({
                "bytes":self.alignment().len(),
                "encoding":"base64", "data":STANDARD.encode(self.alignment())
            });
        }
        value
    }
}

const fn error_name(code: u32) -> Option<&'static str> {
    match code {
        262_073 => Some("scaling_underflow"),
        262_074 => Some("scaling_overflow"),
        262_075 => Some("output_bandwidth_exceeded"),
        262_076 => Some("no_peak"),
        262_077 => Some("peak_before_range"),
        262_078 => Some("peak_after_range"),
        262_079 => Some("calculation_unavailable"),
        262_080 => Some("evaluation_unavailable"),
        262_081 => Some("peak_too_wide"),
        262_082 => Some("laser_off"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: &[u8]) -> Word {
        let mut framing = Framing::default();
        let mut word = None;
        for &byte in bytes {
            word = framing.push(byte).unwrap();
        }
        word.unwrap()
    }
    #[test]
    fn exact_six_bit_groups_and_documented_errors_preserve_all_raw_bits() {
        for (raw, code) in [
            ([0, 0x40, 0x80], 0),
            ([0x38, 0x7f, 0x87], 32_760),
            ([0, 0x40, 0x90], 65_536),
            ([0x39, 0x7e, 0xbf], 262_073),
            ([0x02, 0x7f, 0xbf], 262_082),
            ([0x3f, 0x7f, 0xbf], 262_143),
        ] {
            let word = frame(&raw);
            let value = word.value("ild2300_dist1");
            assert_eq!(value["raw_code"], code);
            assert_eq!(value["raw"]["data"], STANDARD.encode(raw));
            for absent in ["value", "unit", "valid", "source_time"] {
                assert!(value.get(absent).is_none());
            }
        }
        assert_eq!(
            frame(&[0x39, 0x7e, 0xbf]).value("ild2300_dist1")["error"],
            "scaling_underflow"
        );
        assert_eq!(
            frame(&[0x02, 0x7f, 0xbf]).value("ild2300_dist1")["error"],
            "laser_off"
        );
        assert!(
            frame(&[0x3f, 0x7f, 0xbf])
                .value("ild2300_dist1")
                .get("error")
                .is_none()
        );
    }

    #[test]
    fn only_actual_initial_suffixes_are_retained_and_no_later_resync_is_allowed() {
        for prefix in [&[][..], &[0xbf], &[0x7e, 0xbf]] {
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(&[0, 0x40, 0x80]);
            assert_eq!(frame(&bytes).alignment(), prefix);
        }
        for bad in [
            &[0x40, 0x40][..],
            &[0x80, 0x80],
            &[0x80, 0x40],
            &[0x40, 0],
            &[0xc0],
            &[0, 0x40, 0xc0],
            &[0, 0, 0x80],
            &[0, 0x40, 0x40],
        ] {
            for synchronized in [false, true] {
                let mut framing = Framing::default();
                if synchronized {
                    for byte in [0, 0x40, 0x80] {
                        let _ = framing.push(byte).unwrap();
                    }
                }
                let error = bad.iter().find_map(|&byte| framing.push(byte).err());
                assert!(
                    error.is_some(),
                    "accepted {bad:?} after sync={synchronized}"
                );
            }
        }
    }
}
