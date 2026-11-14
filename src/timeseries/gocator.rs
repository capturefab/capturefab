//! Passive acquisition of complete legacy Gocator Data Protocol groups.
use super::{Reading, Source, net};
use crate::{
    storage::{self, SyncMode},
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    time::{Duration, Instant},
};

const HEADER: usize = 6;
const MAX_GROUP: usize = 128 * 1024 * 1024;
const MAX_MESSAGES: usize = 128;
const LAST: u16 = 0x8000;
const STAMP: u16 = 1;
const PROFILE_POINT_CLOUD: u16 = 5;
const UNIFORM_PROFILE: u16 = 6;
const UNIFORM_SURFACE: u16 = 8;
const SURFACE_INTENSITY: u16 = 9;
const SURFACE_POINT_CLOUD: u16 = 28;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub address: SocketAddr,
    pub output_directory: String,
    #[serde(
        default = "default_group_bytes",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub max_group_bytes: usize,
    #[serde(default = "default_signal")]
    pub signal: String,
}
fn default_signal() -> String {
    "profile_group".into()
}
const fn default_group_bytes() -> usize {
    4 * 1024 * 1024
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    super::validate_name(&config.signal, "Gocator signal")?;
    crate::naming::validate_output(&config.output_directory)?;
    if !(HEADER..=MAX_GROUP).contains(&config.max_group_bytes) {
        return Err(Usage(
            "Gocator GDP max_group_bytes must be between 6 and 134217728 bytes".into(),
        )
        .into());
    }
    if config.address.port() == 0
        || config.output_directory == "-"
        || config.output_directory.contains("://")
    {
        return Err(Usage(
            "Gocator GDP requires a nonzero TCP port and a local output directory".into(),
        )
        .into());
    }
    let directory = std::path::absolute(&config.output_directory)?;
    if directory.exists() && !directory.is_dir() {
        return Err(Usage("Gocator GDP output_directory must be a directory".into()).into());
    }
    if crate::host::monotonic_ns().is_none() {
        return Err(Categorized::error(
            ErrorKind::Unsupported,
            "Gocator GDP acquisition requires a host-wide monotonic clock",
        ));
    }
    Ok(Box::new(Stream {
        address: config.address,
        directory,
        signal: config.signal.clone(),
        connection: None,
        group: Group {
            raw: Vec::new(),
            messages: Vec::new(),
            offset: 0,
            max_bytes: config.max_group_bytes,
        },
        sequence: 0,
        observed: None,
        cutoff: None,
        folders: storage::Folders::default(),
    }))
}

struct Group {
    raw: Vec<u8>,
    messages: Vec<Value>,
    offset: usize,
    max_bytes: usize,
}
impl Group {
    /// Fill only as far as this message needs. Partial bytes survive idle read deadlines.
    fn fill(&mut self, connection: &mut dyn net::Transport, target: usize) -> Result<bool> {
        ensure!(
            target <= self.max_bytes,
            Categorized::new(
                ErrorKind::Incomplete,
                "Gocator GDP group exceeds max_group_bytes"
            )
            .details(json!({"max_group_bytes":self.max_bytes,"required_bytes":target}))
        );
        let mut buffer = [0; 8192];
        while self.raw.len() < target {
            let needed = (target - self.raw.len()).min(buffer.len());
            match connection.read(&mut buffer[..needed]) {
                Ok(0) if self.raw.is_empty() => {
                    return Err(Categorized::error(
                        ErrorKind::Disconnected,
                        "Gocator GDP connection closed",
                    ));
                }
                Ok(0) => return Err(incomplete("Gocator GDP connection ended within a group")),
                Ok(count) => {
                    let end = self.raw.len() + count;
                    if end > self.raw.capacity() {
                        let capacity = self
                            .raw
                            .capacity()
                            .saturating_mul(2)
                            .min(self.max_bytes)
                            .max(end);
                        self.raw
                            .try_reserve_exact(capacity - self.raw.len())
                            .context("allocate bounded Gocator GDP group")?;
                    }
                    self.raw.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(error).context("read Gocator GDP stream"),
            }
        }
        Ok(true)
    }

    fn receive(&mut self, connection: &mut dyn net::Transport) -> Result<Option<u64>> {
        loop {
            if !self.fill(connection, self.offset + HEADER)? {
                return Ok(None);
            }
            let start = self.offset;
            let size = usize::try_from(u32::from_le_bytes(field(&self.raw, start)?))?;
            let control = u16::from_le_bytes(field(&self.raw, start + 4)?);
            ensure!(
                size >= HEADER,
                incomplete("invalid Gocator GDP message length")
            );
            let end = start
                .checked_add(size)
                .ok_or_else(|| incomplete("Gocator GDP message range overflow"))?;
            if !self.fill(connection, end)? {
                return Ok(None);
            }
            let last = control & LAST != 0;
            let observed = last
                .then(|| {
                    crate::host::monotonic_ns().context("Gocator GDP receive clock unavailable")
                })
                .transpose()?;
            let type_id = control & !LAST;
            let data = &self.raw[start..end];
            let mut message = json!({"type_id":type_id, "offset":start, "bytes":size, "last":last});
            let decoded = match type_id {
                STAMP => stamp(data, start)?.map(|value| ("stamp", value)),
                PROFILE_POINT_CLOUD => {
                    point_cloud_profile(data, start)?.map(|value| ("point_cloud_profile", value))
                }
                UNIFORM_PROFILE => {
                    uniform_profile(data, start)?.map(|value| ("uniform_profile", value))
                }
                UNIFORM_SURFACE | SURFACE_INTENSITY | SURFACE_POINT_CLOUD => {
                    surface(data, start, type_id)?
                }
                _ => None,
            };
            if let Some((name, value)) = decoded {
                message[name] = value;
            }
            self.messages.push(message);
            self.offset = end;
            if last {
                return Ok(observed);
            }
            ensure!(
                self.messages.len() < MAX_MESSAGES,
                incomplete("Gocator GDP group exceeds 128 messages")
            );
        }
    }

    fn clear(&mut self) {
        self.raw.clear();
        self.messages.clear();
        self.offset = 0;
    }
}
fn incomplete(message: &str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn field<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N]> {
    data.get(offset..offset + N)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| incomplete("truncated Gocator GDP record metadata"))
}

fn uniform_profile(data: &[u8], start: usize) -> Result<Option<Value>> {
    let attributes = usize::from(u16::from_le_bytes(field(data, 6)?));
    if !matches!(attributes, 32 | 40 | 48) {
        return Ok(None);
    }
    let offset = 8 + attributes;
    let rows = u32::from_le_bytes(field(data, 8)?);
    let width = u32::from_le_bytes(field(data, 12)?);
    let bytes = u64::from(rows)
        .checked_mul(u64::from(width))
        .and_then(|n| n.checked_mul(2));
    ensure!(
        bytes.is_some_and(|bytes| Some(bytes)
            == data
                .len()
                .checked_sub(offset)
                .and_then(|n| u64::try_from(n).ok())),
        incomplete("Gocator GDP Uniform Profile length differs from its geometry")
    );
    let mut profile = json!({
        "data_offset":start + offset, "profiles":rows, "width":width,
        "sample_type":"int16", "byte_order":"little", "invalid_code":i16::MIN,
        "x_scale_nm":u32::from_le_bytes(field(data,16)?), "z_scale_nm":u32::from_le_bytes(field(data,20)?),
        "x_offset_um":i32::from_le_bytes(field(data,24)?), "z_offset_um":i32::from_le_bytes(field(data,28)?),
        "source_id":data[32], "exposure_raw":u32::from_le_bytes(field(data,33)?)
    });
    array_context(data, attributes, 32, &mut profile)?;
    Ok(Some(profile))
}

fn point_cloud_profile(data: &[u8], start: usize) -> Result<Option<Value>> {
    let attributes = usize::from(u16::from_le_bytes(field(data, 6)?));
    if !matches!(attributes, 32 | 40 | 48) {
        return Ok(None);
    }
    let offset = 8 + attributes;
    let rows = u32::from_le_bytes(field(data, 8)?);
    let width = u32::from_le_bytes(field(data, 12)?);
    let bytes = u64::from(rows)
        .checked_mul(u64::from(width))
        .and_then(|n| n.checked_mul(4));
    ensure!(
        bytes.is_some_and(|bytes| Some(bytes)
            == data
                .len()
                .checked_sub(offset)
                .and_then(|n| u64::try_from(n).ok())),
        incomplete("Gocator GDP Point Cloud Profile length differs from its geometry")
    );
    let [source_id] = field(data, 32)?;
    let [camera_index] = field(data, 37)?;
    let mut profile = json!({
        "data_offset":start + offset, "profiles":rows, "width":width,
        "sample_type":"int16", "byte_order":"little", "organization":"interleaved",
        "components":["x","z"], "point_stride_bytes":4, "invalid_x_code":i16::MIN,
        "x_scale_nm":u32::from_le_bytes(field(data,16)?), "z_scale_nm":u32::from_le_bytes(field(data,20)?),
        "x_offset_um":i32::from_le_bytes(field(data,24)?), "z_offset_um":i32::from_le_bytes(field(data,28)?),
        "source_id":source_id, "camera_index":camera_index,
        "exposure_raw":u32::from_le_bytes(field(data,33)?)
    });
    array_context(data, attributes, 32, &mut profile)?;
    Ok(Some(profile))
}

fn surface(data: &[u8], start: usize, type_id: u16) -> Result<Option<(&'static str, Value)>> {
    let attributes = usize::from(u16::from_le_bytes(field(data, 6)?));
    let (name, known, base, sample_bytes) = match type_id {
        UNIFORM_SURFACE => ("uniform_surface", matches!(attributes, 44 | 52 | 60), 44, 2),
        SURFACE_INTENSITY => (
            "surface_intensity",
            matches!(attributes, 32 | 40 | 48),
            32,
            1,
        ),
        SURFACE_POINT_CLOUD => ("point_cloud_surface", matches!(attributes, 46 | 54), 38, 6),
        _ => return Ok(None),
    };
    if !known {
        return Ok(None);
    }
    let offset = 8 + attributes;
    let rows = u32::from_le_bytes(field(data, 8)?);
    let width = u32::from_le_bytes(field(data, 12)?);
    let row_bytes = u64::from(width) * sample_bytes;
    let bytes = u64::from(rows).checked_mul(row_bytes);
    ensure!(
        bytes.is_some_and(|bytes| Some(bytes)
            == data
                .len()
                .checked_sub(offset)
                .and_then(|n| u64::try_from(n).ok())),
        incomplete("Gocator GDP Surface length differs from its geometry")
    );
    let mut surface = json!({
        "data_offset":start + offset, "rows":rows, "width":width, "row_stride_bytes":row_bytes,
        "byte_order":"little", "x_scale_nm":u32::from_le_bytes(field(data,16)?),
        "y_scale_nm":u32::from_le_bytes(field(data,20)?)
    });
    if type_id == SURFACE_INTENSITY {
        surface["sample_type"] = json!("uint8");
        surface["x_offset_um"] = json!(i32::from_le_bytes(field(data, 24)?));
        surface["y_offset_um"] = json!(i32::from_le_bytes(field(data, 28)?));
        surface["source_id"] = json!(u8::from_le_bytes(field(data, 32)?));
        surface["exposure_raw"] = json!(u32::from_le_bytes(field(data, 33)?));
    } else {
        surface["sample_type"] = json!("int16");
        surface["z_scale_nm"] = json!(u32::from_le_bytes(field(data, 24)?));
        surface["x_offset_um"] = json!(i32::from_le_bytes(field(data, 28)?));
        surface["y_offset_um"] = json!(i32::from_le_bytes(field(data, 32)?));
        surface["z_offset_um"] = json!(i32::from_le_bytes(field(data, 36)?));
        surface["source_id"] = json!(u8::from_le_bytes(field(data, 40)?));
        surface["exposure_raw"] = json!(u32::from_le_bytes(field(data, 41)?));
        if type_id == SURFACE_POINT_CLOUD {
            surface["components"] = json!(["x", "y", "z"]);
            surface["organization"] = json!("interleaved");
            surface["point_stride_bytes"] = json!(6);
            surface["invalid_z_code"] = json!(i16::MIN);
            surface["adjacent_raw"] = json!(u8::from_le_bytes(field(data, 45)?));
        } else {
            surface["invalid_code"] = json!(i16::MIN);
        }
    }
    array_context(data, attributes, base, &mut surface)?;
    Ok(Some((name, surface)))
}

fn array_context(data: &[u8], attributes: usize, base: usize, array: &mut Value) -> Result<()> {
    let offset = 8 + base;
    if attributes >= base + 8 {
        array["stream_step"] = json!(i32::from_le_bytes(field(data, offset)?));
        array["stream_step_id"] = json!(i32::from_le_bytes(field(data, offset + 4)?));
    }
    if attributes == base + 16 {
        array["arrayed_count"] = json!(i32::from_le_bytes(field(data, offset + 8)?));
        array["arrayed_index"] = json!(i32::from_le_bytes(field(data, offset + 12)?));
    }
    Ok(())
}

fn stamp(data: &[u8], start: usize) -> Result<Option<Value>> {
    const PREFIX: usize = 14;
    let count = u32::from_le_bytes(field(data, 6)?);
    let stride = usize::from(u16::from_le_bytes(field(data, 10)?));
    if !matches!(stride, 56 | 64) {
        return Ok(None);
    }
    let bytes = usize::try_from(count)?.checked_mul(stride);
    ensure!(
        bytes.is_some_and(|bytes| Some(bytes) == data.len().checked_sub(PREFIX)),
        incomplete("Gocator GDP Stamp length differs from its declared array")
    );
    let mut stamp = json!({
        "data_offset":start + PREFIX, "count":count, "stride_bytes":stride, "source_id":data[12]
    });
    if count > 0 {
        // Keep the full stamp array on disk. The first stamp describes only its own frame.
        let first = &data[PREFIX..PREFIX + stride];
        stamp["first"] = json!({
            "frame_index":u64::from_le_bytes(field(first,0)?), "timestamp_us":u64::from_le_bytes(field(first,8)?),
            "encoder_ticks":i64::from_le_bytes(field(first,16)?), "encoder_at_z_ticks":i64::from_le_bytes(field(first,24)?),
            "status":u64::from_le_bytes(field(first,32)?), "serial_number":u32::from_le_bytes(field(first,40)?)
        });
        if stride == 64 {
            stamp["first"]["ptp_timestamp_us"] = json!(u64::from_le_bytes(field(first, 56)?));
        }
    }
    Ok(Some(stamp))
}

struct Stream {
    address: SocketAddr,
    directory: PathBuf,
    signal: String,
    connection: Option<Box<dyn net::Transport>>,
    group: Group,
    sequence: u64,
    observed: Option<u64>,
    cutoff: Option<u64>,
    folders: storage::Folders,
}
impl Source for Stream {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now() + timeout;
        if self.connection.is_none() {
            let stream = TcpStream::connect_timeout(&self.address, timeout)
                .context("connect to Gocator GDP data port")?;
            self.connection = Some(net::plain(stream, deadline));
        }
        let connection = self
            .connection
            .as_deref_mut()
            .context("Gocator GDP socket unavailable")?;
        connection.set_deadline(deadline);
        let Some(observed) = self.group.receive(connection)? else {
            return Ok(Vec::new());
        };
        ensure!(
            self.cutoff.is_none_or(|cutoff| observed <= cutoff),
            incomplete("Gocator GDP group completed after the acquisition cutoff")
        );
        let sequence = self
            .sequence
            .checked_add(1)
            .context("Gocator GDP group counter exhausted")?;
        let path = self.directory.join(format!("group-{sequence:08}.gdp"));
        let stored = storage::save(&self.group.raw, &path, SyncMode::File, None, true)?;
        self.folders.add(&path);
        let value = json!({
            "kind":"gocator_gdp_group", "path":path, "bytes":stored.bytes,
            "sha256":stored.sha256, "messages":self.group.messages,
            "group_start_confirmed":self.sequence > 0
        });
        self.group.clear();
        self.sequence = sequence;
        self.observed = Some(observed);
        Ok(vec![Reading {
            signal: self.signal.clone(),
            value,
            source_time: None,
        }])
    }

    fn ready(&self) -> bool {
        self.sequence > 0
    }

    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }

    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        self.cutoff = end_ns;
    }

    fn finish(&mut self, _timeout: Duration) -> Result<()> {
        self.connection = None;
        self.folders.sync()?;
        ensure!(
            self.group.raw.is_empty(),
            incomplete("Gocator GDP acquisition ended with an unpublished group")
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_limit_config_keeps_exact_decimal_and_exponent_notation() {
        for (number, expected) in [("6.0", 6), ("7e6", 7_000_000), ("1.34217728e8", MAX_GROUP)] {
            let text = format!(
                r#"{{"address":"127.0.0.1:3196","output_directory":"groups","max_group_bytes":{number}}}"#
            );
            let config: Config = serde_json::from_str(&text).unwrap();
            assert_eq!(config.max_group_bytes, expected);
        }
        let config: Config =
            serde_json::from_value(json!({"address":"127.0.0.1:3196","output_directory":"groups"}))
                .unwrap();
        assert_eq!(config.max_group_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn declared_limit_is_checked_before_any_payload_allocation_or_read() {
        struct NoIo;
        impl io::Read for NoIo {
            fn read(&mut self, _bytes: &mut [u8]) -> io::Result<usize> {
                panic!("oversized GDP target reached transport read");
            }
        }
        impl io::Write for NoIo {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                panic!("passive GDP source wrote to transport");
            }
            fn flush(&mut self) -> io::Result<()> {
                panic!("passive GDP source flushed transport");
            }
        }
        impl net::Transport for NoIo {
            fn set_deadline(&mut self, _deadline: Instant) {}
        }
        let mut group = Group {
            raw: Vec::new(),
            messages: Vec::new(),
            offset: 0,
            max_bytes: HEADER,
        };
        assert!(
            group
                .fill(&mut NoIo, HEADER + 1)
                .unwrap_err()
                .to_string()
                .contains("max_group_bytes")
        );
        assert_eq!(group.raw.capacity(), 0);
    }

    #[test]
    fn sdk_surface_versions_enforce_exact_geometry_including_empty_arrays() {
        for (type_id, versions, stride) in [
            (UNIFORM_SURFACE, &[44u16, 52, 60][..], 2),
            (SURFACE_INTENSITY, &[32, 40, 48][..], 1),
            (SURFACE_POINT_CLOUD, &[46, 54][..], 6),
        ] {
            for &attributes in versions {
                for (rows, width) in [(0, 0), (0, u32::MAX), (u32::MAX, 0), (1, 2)] {
                    let mut data = vec![0; 8 + usize::from(attributes)];
                    data[6..8].copy_from_slice(&attributes.to_le_bytes());
                    data[8..12].copy_from_slice(&rows.to_le_bytes());
                    data[12..16].copy_from_slice(&width.to_le_bytes());
                    let data_offset = data.len();
                    if rows == 1 {
                        data.resize(data.len() + 2 * stride, 0);
                    }
                    let (_, parsed) = surface(&data, 9, type_id).unwrap().unwrap();
                    assert_eq!(parsed["data_offset"], 9 + data_offset);
                    assert_eq!(parsed["rows"], rows);
                    assert_eq!(parsed["width"], width);
                    assert_eq!(
                        parsed["row_stride_bytes"],
                        u64::from(width) * u64::try_from(stride).unwrap()
                    );
                    data.push(0);
                    assert!(
                        surface(&data, 9, type_id)
                            .unwrap_err()
                            .to_string()
                            .contains("Surface length differs from its geometry")
                    );
                }
                let mut data = vec![0; 8 + usize::from(attributes)];
                data[6..8].copy_from_slice(&attributes.to_le_bytes());
                data[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
                data[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
                assert!(
                    surface(&data, 0, type_id)
                        .unwrap_err()
                        .to_string()
                        .contains("Surface length differs from its geometry")
                );
                data[6..8].copy_from_slice(&(attributes + 1).to_le_bytes());
                assert!(surface(&data, 0, type_id).unwrap().is_none());
            }
        }
    }

    #[test]
    fn sdk_empty_profiles_and_stamps_retain_their_declared_layouts() {
        for attributes in [32u16, 40, 48] {
            for (rows, width) in [(0, 0), (0, u32::MAX), (u32::MAX, 0)] {
                let mut data = vec![0; 8 + usize::from(attributes)];
                data[6..8].copy_from_slice(&attributes.to_le_bytes());
                data[8..12].copy_from_slice(&rows.to_le_bytes());
                data[12..16].copy_from_slice(&width.to_le_bytes());
                let cloud = point_cloud_profile(&data, 9).unwrap().unwrap();
                let uniform = uniform_profile(&data, 9).unwrap().unwrap();
                for profile in [cloud, uniform] {
                    assert_eq!(profile["profiles"], rows);
                    assert_eq!(profile["width"], width);
                    assert_eq!(profile["data_offset"], 9 + data.len());
                }
                data.push(0);
                assert!(
                    point_cloud_profile(&data, 9)
                        .unwrap_err()
                        .to_string()
                        .contains("length differs from its geometry")
                );
                assert!(
                    uniform_profile(&data, 9)
                        .unwrap_err()
                        .to_string()
                        .contains("length differs from its geometry")
                );
            }
        }
        for stride in [56u16, 64] {
            let mut data = vec![0; 14];
            data[10..12].copy_from_slice(&stride.to_le_bytes());
            assert_eq!(
                stamp(&data, 9).unwrap().unwrap(),
                json!({"data_offset":23,"count":0,"stride_bytes":stride,"source_id":0})
            );
            data.push(0);
            assert!(
                stamp(&data, 9)
                    .unwrap_err()
                    .to_string()
                    .contains("Stamp length differs from its declared array")
            );
        }
    }

    #[test]
    fn point_cloud_geometry_rejects_overflow_unaccounted_and_partial_samples() {
        let mut data = vec![0; 60];
        data[6..8].copy_from_slice(&48u16.to_le_bytes());
        data[8..12].copy_from_slice(&1u32.to_le_bytes());
        data[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            point_cloud_profile(&data, 17).unwrap().unwrap()["data_offset"],
            73
        );
        let mut zero = data.clone();
        zero[8..12].copy_from_slice(&0u32.to_le_bytes());
        let mut overflow = data.clone();
        overflow[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        overflow[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let partial = data[..59].to_vec();
        for malformed in [zero, overflow, partial] {
            assert!(
                point_cloud_profile(&malformed, 0)
                    .unwrap_err()
                    .to_string()
                    .contains("length differs from its geometry")
            );
        }
        assert!(
            point_cloud_profile(&data[..11], 0)
                .unwrap_err()
                .to_string()
                .contains("truncated Gocator GDP record metadata")
        );
        data[6..8].copy_from_slice(&56u16.to_le_bytes());
        assert!(point_cloud_profile(&data, 0).unwrap().is_none());
    }

    #[test]
    fn newer_uniform_context_and_stamp_stride_reject_truncated_success_layouts() {
        let mut profile = vec![0; 58];
        profile[6..8].copy_from_slice(&48u16.to_le_bytes());
        profile[8..12].copy_from_slice(&1u32.to_le_bytes());
        profile[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            uniform_profile(&profile, 0).unwrap().unwrap()["data_offset"],
            56
        );
        assert!(
            uniform_profile(&profile[..57], 0)
                .unwrap_err()
                .to_string()
                .contains("length differs from its geometry")
        );
        let mut data = vec![0; 78];
        data[6..10].copy_from_slice(&1u32.to_le_bytes());
        data[10..12].copy_from_slice(&64u16.to_le_bytes());
        assert_eq!(
            stamp(&data, 0).unwrap().unwrap()["first"]["ptp_timestamp_us"],
            0
        );
        assert!(
            stamp(&data[..77], 0)
                .unwrap_err()
                .to_string()
                .contains("Stamp length differs from its declared array")
        );
        data[10..12].copy_from_slice(&56u16.to_le_bytes());
        assert!(
            stamp(&data, 0)
                .unwrap_err()
                .to_string()
                .contains("Stamp length differs from its declared array")
        );
        data[10..12].copy_from_slice(&72u16.to_le_bytes());
        assert!(stamp(&data, 0).unwrap().is_none());
        profile[6..8].copy_from_slice(&56u16.to_le_bytes());
        assert!(uniform_profile(&profile, 0).unwrap().is_none());
    }
}
