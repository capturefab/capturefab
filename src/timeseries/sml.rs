//! Passive optical SML binary transport v1 and the EDL push response file.
//! BSI TR-03109-1 Annex `IVb` §§5.1.15, 6 and 8.1 define the wire format.
//! <https://www.bsi.bund.de/SharedDocs/Downloads/DE/BSI/Publikationen/TechnischeRichtlinien/TR03109/TR-03109-1_Anlage_Feinspezifikation_Drahtgebundene_LMN-Schnittstelle_Teilb.pdf?__blob=publicationFile&v=1>
//! VDE FNN EDL §7.1.5 specifies the unsolicited open, list and close sequence.
//! <https://www.vde.com/resource/blob/2346190/b0c754439ffbfdd95287d1ae50f9f3d9/fnn-lastenheft-edl---funktionale-merkmale-und-protokolle--2016--data.pdf>

use super::Source;
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    device: String,
    signals: Vec<Signal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signal {
    name: String,
    obis: String,
}

#[cfg(any(unix, windows))]
#[derive(Debug)]
struct Mapping {
    name: String,
    obis: [u8; 6],
    code: String,
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    if config.device.is_empty() || !(1..=256).contains(&config.signals.len()) {
        return Err(Usage("SML needs a device and 1–256 OBIS signal mappings".into()).into());
    }
    let mut names = BTreeSet::new();
    let mut codes = BTreeSet::new();
    #[cfg(any(unix, windows))]
    let mut mappings = Vec::with_capacity(config.signals.len());
    for signal in &config.signals {
        super::validate_name(&signal.name, "SML signal")?;
        let obis = obis(&signal.obis)
            .ok_or_else(|| Usage("SML obis must be A-B:C.D.E*F with six decimal bytes".into()))?;
        if !names.insert(&signal.name) || !codes.insert(obis) {
            return Err(Usage("SML signal names and OBIS codes must be unique".into()).into());
        }
        #[cfg(any(unix, windows))]
        mappings.push(Mapping {
            name: signal.name.clone(),
            obis,
            code: signal.obis.clone(),
        });
    }
    #[cfg(any(unix, windows))]
    {
        wire::open(&config.device, mappings)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "optical SML needs a Unix terminal or Windows COM port",
        ))
    }
}

fn obis(text: &str) -> Option<[u8; 6]> {
    let (a, tail) = text.split_once('-')?;
    let (b, tail) = tail.split_once(':')?;
    let (c, tail) = tail.split_once('.')?;
    let (d, tail) = tail.split_once('.')?;
    let (e, f) = tail.split_once('*')?;
    let mut code = [0; 6];
    for (group, output) in [a, b, c, d, e, f].into_iter().zip(&mut code) {
        if group.is_empty() || group.len() > 3 || !group.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *output = group.parse().ok()?;
    }
    Some(code)
}

#[cfg(any(unix, windows))]
fn malformed(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::OperationFailed, format!("SML: {message}"))
}

#[cfg(any(unix, windows))]
mod wire {
    use super::{Mapping, Source, malformed};
    use crate::{
        timeseries::{
            Reading,
            serial::{self, Parity, Port},
        },
        types::{Categorized, ErrorKind, Usage},
    };
    use anyhow::Result;
    use serde::Serialize;
    use serde_json::Number;
    use std::{
        io,
        str::FromStr as _,
        time::{Duration, Instant},
    };

    const ESCAPE: [u8; 4] = [0x1b; 4];
    const START: [u8; 8] = [0x1b, 0x1b, 0x1b, 0x1b, 1, 1, 1, 1];
    const MAX_FRAME: usize = 64 * 1024;
    const MAX_ENTRIES: usize = 256;

    pub(super) fn open(device: &str, mappings: Vec<Mapping>) -> Result<Box<dyn Source>> {
        Ok(Box::new(Meter {
            port: serial::open(device, 9600, Parity::None, 1, 8)?,
            mappings,
            frames: Frames::default(),
            observed: None,
            drained: Vec::new().into_iter(),
            failed: false,
            ready: false,
        }))
    }

    struct Meter {
        port: Port,
        mappings: Vec<Mapping>,
        frames: Frames,
        observed: Option<u64>,
        drained: std::vec::IntoIter<Reading>,
        failed: bool,
        ready: bool,
    }

    impl Meter {
        fn next(&mut self) -> Result<Option<Vec<Reading>>> {
            let Some(file) = self.frames.take()? else {
                return Ok(None);
            };
            let readings = decode(&file, &self.mappings)?;
            self.ready = true;
            Ok(Some(readings))
        }

        fn receive(&mut self, deadline: Instant) -> Result<Vec<Reading>> {
            loop {
                if let Some(readings) = self.next()? {
                    return Ok(readings);
                }
                if Instant::now() >= deadline {
                    return Ok(Vec::new());
                }
                let mut bytes = [0; 512];
                let count = self.port.receive(&mut bytes, deadline).map_err(|error| {
                    let kind = match error.kind() {
                        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
                        _ => ErrorKind::Disconnected,
                    };
                    Categorized::error(kind, format!("SML serial I/O: {error}"))
                })?;
                match count {
                    None => return Ok(Vec::new()),
                    Some(0) => {
                        return Err(Categorized::error(
                            ErrorKind::Disconnected,
                            "SML serial stream closed",
                        ));
                    }
                    Some(count) => {
                        self.observed = crate::host::monotonic_ns();
                        self.frames.pending.extend_from_slice(&bytes[..count]);
                    }
                }
            }
        }
    }

    impl Source for Meter {
        fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| Usage("SML timeout is too large".into()))?;
            let result = self.receive(deadline);
            self.failed = result.is_err();
            result
        }

        fn ready(&self) -> bool {
            self.ready
        }

        fn observed_ns(&self) -> Option<u64> {
            self.observed
        }

        fn buffered_timestamps(&self) -> bool {
            true
        }

        fn drain(&mut self) -> Result<Option<Reading>> {
            if self.failed {
                return Ok(None);
            }
            if self.drained.len() == 0
                && let Some(readings) = self.next()?
            {
                self.drained = readings.into_iter();
            }
            Ok(self.drained.next())
        }
    }

    #[derive(Default)]
    struct Frames {
        pending: Vec<u8>,
        scan: usize,
    }

    impl Frames {
        fn take(&mut self) -> Result<Option<Vec<u8>>> {
            if self.scan == 0 {
                let Some(start) = self
                    .pending
                    .windows(START.len())
                    .position(|part| part == START)
                else {
                    // Keep the possible prefix of a start sequence spanning reads.
                    let discard = self.pending.len().saturating_sub(START.len() - 1);
                    self.pending.drain(..discard);
                    return Ok(None);
                };
                self.pending.drain(..start);
                self.scan = START.len();
            }

            while let Some(offset) = self.pending[self.scan..]
                .windows(ESCAPE.len())
                .position(|part| part == ESCAPE)
            {
                let index = self.scan + offset;
                let end = index + 8;
                if end > MAX_FRAME {
                    return Err(malformed("transport frame exceeds 64 KiB"));
                }
                let Some(marker) = self.pending.get(index + 4..end) else {
                    self.scan = index;
                    return Ok(None);
                };
                match marker {
                    [0x1b, 0x1b, 0x1b, 0x1b] => self.scan = end,
                    [0x1a, padding, low, high] => {
                        let wanted = u16::from_le_bytes([*low, *high]);
                        if crc(&self.pending[..end - 2]) != wanted {
                            return Err(malformed("transport CRC mismatch"));
                        }
                        let payload = unescape(&self.pending[START.len()..index], *padding)?;
                        self.pending.drain(..end);
                        self.scan = 0;
                        return Ok(Some(payload));
                    }
                    _ => return Err(malformed("invalid transport escape or missing end marker")),
                }
            }
            if self.pending.len() > MAX_FRAME {
                return Err(malformed("transport frame exceeds 64 KiB"));
            }
            self.scan = self
                .scan
                .max(self.pending.len().saturating_sub(ESCAPE.len() - 1));
            Ok(None)
        }
    }

    fn unescape(mut wire: &[u8], padding: u8) -> Result<Vec<u8>> {
        let fill = usize::from(padding);
        if fill > 3 || !wire.len().is_multiple_of(4) {
            return Err(malformed("invalid transport padding length"));
        }
        let mut output = Vec::with_capacity(wire.len());
        // The frame scanner has already validated every escape pair.
        while let Some(start) = wire.windows(ESCAPE.len()).position(|part| part == ESCAPE) {
            output.extend_from_slice(&wire[..start + ESCAPE.len()]);
            wire = &wire[start + 2 * ESCAPE.len()..];
        }
        output.extend_from_slice(wire);
        let size = output
            .len()
            .checked_sub(fill)
            .ok_or_else(|| malformed("truncated transport padding"))?;
        if output[size..].iter().any(|&byte| byte != 0) {
            return Err(malformed("nonzero transport padding"));
        }
        output.truncate(size);
        Ok(output)
    }

    fn crc(bytes: &[u8]) -> u16 {
        let mut value = u16::MAX;
        for &byte in bytes {
            value ^= u16::from(byte);
            for _ in 0..8 {
                let polynomial = if value & 1 == 0 { 0 } else { 0x8408 };
                value = (value >> 1) ^ polynomial;
            }
        }
        !value
    }

    enum Field<'a> {
        End,
        Octets(&'a [u8]),
        Boolean,
        Signed(&'a [u8]),
        Unsigned(&'a [u8]),
        List(usize),
    }

    impl Field<'_> {
        fn unsigned(&self, width: usize) -> Result<u64> {
            match self {
                Self::Unsigned(bytes) if bytes.len() <= width => Ok(unsigned(bytes)),
                _ => Err(malformed("unexpected unsigned value type or length")),
            }
        }

        fn optional_unsigned(&self, width: usize) -> Result<Option<u64>> {
            match self {
                Self::Octets([]) => Ok(None),
                field => field.unsigned(width).map(Some),
            }
        }
    }

    struct Fields<'a> {
        left: &'a [u8],
    }

    impl<'a> Fields<'a> {
        fn byte(&mut self) -> Result<u8> {
            let Some((&byte, rest)) = self.left.split_first() else {
                return Err(malformed("truncated type/length field"));
            };
            self.left = rest;
            Ok(byte)
        }

        fn field(&mut self) -> Result<Field<'a>> {
            let first = self.byte()?;
            if first == 0 {
                return Ok(Field::End);
            }
            let mut byte = first;
            let mut length = usize::from(byte & 15);
            let mut width = 1;
            while byte & 0x80 != 0 {
                byte = self.byte()?;
                if byte & 0x70 != 0 || width == 4 {
                    return Err(malformed("invalid type/length continuation"));
                }
                width += 1;
                length = (length << 4) | usize::from(byte & 15);
            }
            let kind = first & 0x70;
            if kind == 0x70 {
                return Ok(Field::List(length));
            }
            let count = length
                .checked_sub(width)
                .ok_or_else(|| malformed("invalid type/length payload size"))?;
            let Some((value, rest)) = self.left.split_at_checked(count) else {
                return Err(malformed("truncated type/length payload"));
            };
            self.left = rest;
            match (kind, count) {
                (0, _) => Ok(Field::Octets(value)),
                (0x40, 1) => Ok(Field::Boolean),
                (0x50, 1..=8) => Ok(Field::Signed(value)),
                (0x60, 1..=8) => Ok(Field::Unsigned(value)),
                _ => Err(malformed("unsupported type/length value")),
            }
        }

        fn list(&mut self, count: usize) -> Result<()> {
            if matches!(self.field()?, Field::List(actual) if actual == count) {
                return Ok(());
            }
            Err(malformed("unexpected list width or type"))
        }

        fn skip(&mut self, depth: usize) -> Result<()> {
            if depth == 0 {
                return Err(malformed("type/length nesting exceeds limit"));
            }
            match self.field()? {
                Field::End => Err(malformed("unexpected message end inside field")),
                Field::List(count) => {
                    for _ in 0..count {
                        self.skip(depth - 1)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
    }

    fn unsigned(bytes: &[u8]) -> u64 {
        bytes
            .iter()
            .fold(0, |value, byte| (value << 8) | u64::from(*byte))
    }

    fn signed(bytes: &[u8]) -> i64 {
        let initial = if bytes.first().is_some_and(|&byte| byte & 0x80 != 0) {
            -1
        } else {
            0
        };
        bytes
            .iter()
            .fold(initial, |value, byte| (value << 8) | i64::from(*byte))
    }

    fn decode(file: &[u8], mappings: &[Mapping]) -> Result<Vec<Reading>> {
        let mut input = Fields { left: file };
        let mut values = vec![None; mappings.len()];
        // FNN's standard push profile is exactly PublicOpen, GetList, PublicClose.
        for expected in [0x0101, 0x0701, 0x0201] {
            let start = input.left;
            input.list(6)?;
            if !matches!(input.field()?, Field::Octets(_)) {
                return Err(malformed("invalid message transaction identifier"));
            }
            input.field()?.unsigned(1)?; // groupNo
            input.field()?.unsigned(1)?; // abortOnError
            input.list(2)?;
            if input.field()?.unsigned(4)? != expected {
                return Err(malformed("expected open, list and close push responses"));
            }
            match expected {
                0x0101 => {
                    input.list(6)?;
                    for _ in 0..6 {
                        input.skip(12)?;
                    }
                }
                0x0701 => list(&mut input, mappings, &mut values)?,
                _ => {
                    input.list(1)?;
                    input.skip(12)?;
                }
            }
            let covered = &start[..start.len() - input.left.len()];
            let checksum = input.field()?.unsigned(2)?;
            // SML's numeric Unsigned16 tag carries the HDLC low byte first.
            let wanted = u64::from(crc(covered).swap_bytes());
            if wanted != checksum {
                return Err(malformed("message CRC mismatch"));
            }
            if !matches!(input.field()?, Field::End) {
                return Err(malformed("missing message end"));
            }
        }
        if !input.left.is_empty() {
            return Err(malformed("extra bytes after SML close response"));
        }
        mappings
            .iter()
            .zip(values)
            .map(|(mapping, value)| {
                let value = value.ok_or_else(|| {
                    malformed(&format!(
                        "configured OBIS {} is absent from the push list",
                        mapping.code
                    ))
                })?;
                Ok(Reading {
                    signal: mapping.name.clone(),
                    value: serde_json::to_value(value)?,
                    source_time: None,
                })
            })
            .collect()
    }

    #[derive(Debug, Clone, Serialize)]
    struct Value<'a> {
        obis: &'a str,
        raw: Number,
        value: Number,
        #[serde(skip_serializing_if = "Option::is_none")]
        scaler: Option<i8>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unit: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u64>,
    }

    fn list<'a>(
        input: &mut Fields<'_>,
        mappings: &'a [Mapping],
        values: &mut [Option<Value<'a>>],
    ) -> Result<()> {
        input.list(7)?;
        for _ in 0..4 {
            input.skip(12)?; // clientId, serverId, listName, actSensorTime
        }
        let Field::List(count) = input.field()? else {
            return Err(malformed("missing GetList value list"));
        };
        if count > MAX_ENTRIES {
            return Err(malformed("GetList has more than 256 entries"));
        }
        for _ in 0..count {
            input.list(7)?;
            let Field::Octets(code) = input.field()? else {
                return Err(malformed("invalid GetList object name"));
            };
            let status = input.field()?.optional_unsigned(8)?;
            input.skip(12)?; // valTime, which can be a meter-relative index
            let unit = input.field()?.optional_unsigned(1)?;
            let scaler = match input.field()? {
                Field::Octets([]) => None,
                Field::Signed([byte]) => Some(i8::from_be_bytes([*byte])),
                _ => return Err(malformed("invalid GetList scaler")),
            };
            let field = input.field()?;
            let Some((mapping, output)) = mappings
                .iter()
                .zip(values.iter_mut())
                .find(|(mapping, _)| mapping.obis.as_slice() == code)
            else {
                match field {
                    Field::End => return Err(malformed("invalid GetList value")),
                    Field::List(count) => {
                        for _ in 0..count {
                            input.skip(11)?;
                        }
                    }
                    _ => {}
                }
                input.skip(12)?; // valueSignature
                continue;
            };
            let raw = match field {
                Field::Signed(bytes) => Number::from(signed(bytes)),
                Field::Unsigned(bytes) => Number::from(unsigned(bytes)),
                _ => return Err(malformed("mapped GetList value must be an integer")),
            };
            input.skip(12)?; // valueSignature
            if output.is_some() {
                return Err(malformed("duplicate configured OBIS in GetList"));
            }
            let value = match scaler {
                Some(scale) if scale != 0 => Number::from_str(&format!("{raw}e{scale}"))?,
                _ => raw.clone(),
            };
            *output = Some(Value {
                obis: &mapping.code,
                raw,
                value,
                scaler,
                unit,
                status,
            });
        }
        input.skip(12)?; // listSignature
        input.skip(12) // actGatewayTime
    }
}
