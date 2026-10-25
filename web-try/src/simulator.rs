//! The Pattern camera: a port of the native area-scan simulator `sim:0`
//! (`src/transport/simulator.rs`) with the node map its GenICam XML
//! describes, so feature lists, writes, error messages and pixels match the
//! desktop app.
//!
//! The native renderer runs on its own clock; here JavaScript paces frames
//! (see [`Simulator::period`]) and passes each frame's timestamp in. Pixels
//! go through `f32::sin` and `f32::hypot`, whose last bit can differ between
//! math libraries, so a level may differ by one from a native frame.
use crate::Result;
use crate::genicam::{
    self, FeatureInfo, Value, input_value, integer_literal, literal, unsigned_integer,
    validate_numeric,
};
use crate::json::Json;
use crate::types::{Frame, MONO8, MONO12, RGB8};
use std::f32::consts::TAU;

const BAYER_RG8: u32 = 0x0108_0009;

/// What `camera.info` says about the Pattern camera.
pub const ID: &str = "sim:0";
pub const VENDOR: &str = "Capturefab";
pub const MODEL: &str = "Pattern camera";
pub const SERIAL: &str = "SIM0";
pub const TRANSPORT: &str = "simulator";

fn bytes_per_pixel(pixel_format: u32) -> u32 {
    match pixel_format {
        RGB8 => 3,
        MONO12 | 0x0110_0007 | 0x0110_00b8 => 2,
        _ => 1,
    }
}

/// The static scene: a 64-pixel checkerboard on a left-to-right ramp, and
/// each pixel's ring index for the moving concentric waves.
fn scene(width: u32, height: u32) -> Vec<(f32, u8)> {
    (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| {
            let grid = ((x / 64 + y / 64) % 2) as f32 * 25.0;
            let d = (x as f32 - width as f32 * 0.5).hypot(y as f32 - height as f32 * 0.5);
            let ring = (d * 0.11 / TAU * 256.0) as u32 as u8;
            (40.0 + 130.0 * x as f32 / width as f32 + grid, ring)
        })
        .collect()
}

/// Access as the XML's `AccessMode`/`ImposedAccessMode` give it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

/// The nodes of the sim:0 GenICam XML (area kind).
#[derive(Clone, Copy)]
enum Node {
    Category,
    /// Integer with literal bounds (None when the XML has none) and a value
    /// register.
    Integer {
        link: &'static str,
        bounds: Option<(&'static str, &'static str, &'static str)>,
        description: &'static str,
    },
    Float {
        link: &'static str,
        min: &'static str,
        max: &'static str,
        unit: &'static str,
        description: &'static str,
    },
    Enumeration {
        /// A value register, or a fixed `<Value>`.
        link: Option<&'static str>,
        value: &'static str,
        access: Access,
        entries: &'static [(&'static str, &'static str)],
    },
    Command {
        link: &'static str,
        command_value: &'static str,
    },
    IntReg {
        address: u64,
        len: usize,
        access: Access,
    },
    FloatReg {
        address: u64,
        len: usize,
    },
}

const PIXEL_FORMATS: &[(&str, &str)] = &[
    ("Mono8", "0x01080001"),
    ("RGB8", "0x02180014"),
    ("Mono12", "0x01100005"),
    ("BayerRG8", "0x01080009"),
];

/// Every node, byte-sorted by name as the native `BTreeMap` holds them.
const NODES: &[(&str, Node)] = &[
    (
        "AcqReg",
        Node::IntReg {
            address: 0x130,
            len: 4,
            access: Access::WriteOnly,
        },
    ),
    ("AcquisitionControl", Node::Category),
    (
        "AcquisitionFrameRate",
        Node::Float {
            link: "RateReg",
            min: "1",
            max: "120",
            unit: "Hz",
            description: "Requested frames per second. Exposure may limit the frame rate.",
        },
    ),
    (
        "AcquisitionMode",
        Node::Enumeration {
            link: None,
            value: "0",
            access: Access::ReadWrite,
            entries: &[("Continuous", "0")],
        },
    ),
    (
        "AcquisitionStart",
        Node::Command {
            link: "AcqReg",
            command_value: "1",
        },
    ),
    (
        "AcquisitionStop",
        Node::Command {
            link: "AcqReg",
            command_value: "0",
        },
    ),
    ("DeviceControl", Node::Category),
    (
        "DeviceScanType",
        Node::Enumeration {
            link: None,
            value: "0",
            access: Access::ReadOnly,
            entries: &[("Areascan", "0"), ("Linescan", "1")],
        },
    ),
    (
        "ExposureReg",
        Node::FloatReg {
            address: 0x110,
            len: 8,
        },
    ),
    (
        "ExposureTime",
        Node::Float {
            link: "ExposureReg",
            min: "10",
            max: "1000000",
            unit: "us",
            description: "",
        },
    ),
    (
        "Gain",
        Node::Float {
            link: "GainReg",
            min: "0",
            max: "24",
            unit: "dB",
            description: "",
        },
    ),
    (
        "GainReg",
        Node::FloatReg {
            address: 0x118,
            len: 8,
        },
    ),
    (
        "Height",
        Node::Integer {
            link: "HeightReg",
            bounds: Some(("64", "1080", "1")),
            description: "Image height in pixels. Stop acquisition before changing.",
        },
    ),
    (
        "HeightReg",
        Node::IntReg {
            address: 0x104,
            len: 4,
            access: Access::ReadWrite,
        },
    ),
    ("ImageFormatControl", Node::Category),
    (
        "PayloadReg",
        Node::IntReg {
            address: 0x128,
            len: 4,
            access: Access::ReadOnly,
        },
    ),
    (
        "PayloadSize",
        Node::Integer {
            link: "PayloadReg",
            bounds: None,
            description: "",
        },
    ),
    (
        "PixelFormat",
        Node::Enumeration {
            link: Some("PixelFormatReg"),
            value: "",
            access: Access::ReadWrite,
            entries: PIXEL_FORMATS,
        },
    ),
    (
        "PixelFormatReg",
        Node::IntReg {
            address: 0x108,
            len: 4,
            access: Access::ReadWrite,
        },
    ),
    (
        "RateReg",
        Node::FloatReg {
            address: 0x120,
            len: 8,
        },
    ),
    ("Root", Node::Category),
    (
        "Width",
        Node::Integer {
            link: "WidthReg",
            bounds: Some(("64", "1920", "1")),
            description: "Image width in pixels. Stop acquisition before changing.",
        },
    ),
    (
        "WidthReg",
        Node::IntReg {
            address: 0x100,
            len: 4,
            access: Access::ReadWrite,
        },
    ),
];

/// The features reachable from Root, byte-sorted as `NodeMap::features`
/// returns them; registers and PayloadSize are hidden.
const FEATURES: [&str; 10] = [
    "AcquisitionFrameRate",
    "AcquisitionMode",
    "AcquisitionStart",
    "AcquisitionStop",
    "DeviceScanType",
    "ExposureTime",
    "Gain",
    "Height",
    "PixelFormat",
    "Width",
];

fn node(name: &str) -> Result<Node> {
    NODES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, node)| *node)
        .ok_or_else(|| format!("unknown GenICam feature {name}"))
}

fn kind(node: Node) -> &'static str {
    match node {
        Node::Category => "Category",
        Node::Integer { .. } => "Integer",
        Node::Float { .. } => "Float",
        Node::Enumeration { .. } => "Enumeration",
        Node::Command { .. } => "Command",
        Node::IntReg { .. } => "IntReg",
        Node::FloatReg { .. } => "FloatReg",
    }
}

fn access(node: Node) -> Option<Access> {
    match node {
        Node::Enumeration { access, .. } if access != Access::ReadWrite => Some(access),
        Node::IntReg { access, .. } => Some(access),
        Node::FloatReg { .. } => Some(Access::ReadWrite),
        _ => None,
    }
}

/// The simulator's register state and frame counter.
#[derive(Clone)]
pub struct Simulator {
    memory: [u8; 0x200],
    running: bool,
    frame_id: u64,
    scene: (u32, u32, Vec<(f32, u8)>),
}

impl Default for Simulator {
    fn default() -> Self {
        let mut s = Self {
            memory: [0; 0x200],
            running: false,
            frame_id: 0,
            scene: (0, 0, Vec::new()),
        };
        s.put_u32(0x100, 640);
        s.put_u32(0x104, 480);
        s.put_u32(0x108, MONO8);
        s.put_f64(0x110, 10_000.0);
        s.put_f64(0x120, 30.0);
        s.put_f64(0x140, 50.0);
        s.put_f64(0x148, 1.0);
        s.put_f64(0x150, 100.0);
        s.refresh_payload();
        s
    }
}

impl Simulator {
    fn u32(&self, p: usize) -> u32 {
        u32::from_le_bytes(self.memory[p..p + 4].try_into().unwrap())
    }
    fn f64(&self, p: usize) -> f64 {
        f64::from_le_bytes(self.memory[p..p + 8].try_into().unwrap())
    }
    fn put_u32(&mut self, p: usize, v: u32) {
        self.memory[p..p + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_f64(&mut self, p: usize, v: f64) {
        self.memory[p..p + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn refresh_payload(&mut self) {
        self.put_u32(
            0x128,
            self.u32(0x100) * self.u32(0x104) * bytes_per_pixel(self.u32(0x108)),
        );
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn width(&self) -> u32 {
        self.u32(0x100)
    }

    pub fn height(&self) -> u32 {
        self.u32(0x104)
    }

    pub fn pixel_format(&self) -> u32 {
        self.u32(0x108)
    }

    /// Frames delivered so far; the next frame's id is one more.
    pub fn frames(&self) -> u64 {
        self.frame_id
    }

    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let a = address as usize;
        self.refresh_payload();
        Ok(self
            .memory
            .get(a..a + length)
            .ok_or_else(|| "invalid simulator address".to_string())?
            .to_vec())
    }

    /// `RegisterIo::write_memory`: the whole proposed state is checked
    /// before anything changes.
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let a = address as usize;
        let end = a + data.len();
        ensure!(
            !self.running || (a >= 0x110 && end <= 0x128) || (a == 0x130 && end <= 0x134),
            "stop acquisition before changing image format"
        );
        let mut proposed = self.memory;
        proposed
            .get_mut(a..end)
            .ok_or_else(|| "invalid simulator address".to_string())?
            .copy_from_slice(data);
        let read_u32 = |p: usize| u32::from_le_bytes(proposed[p..p + 4].try_into().unwrap());
        let read_f64 = |p: usize| f64::from_le_bytes(proposed[p..p + 8].try_into().unwrap());
        let (width, height, format) = (read_u32(0x100), read_u32(0x104), read_u32(0x108));
        ensure!(
            (64..=1920).contains(&width) && (64..=1080).contains(&height),
            "simulator dimensions out of bounds"
        );
        ensure!(
            [MONO8, RGB8, MONO12, BAYER_RG8].contains(&format),
            "unsupported simulator pixel format"
        );
        let rate = read_f64(0x120);
        let rate_max = 120.0;
        ensure!(
            rate.is_finite() && (1.0..=rate_max).contains(&rate),
            "simulator acquisition rate must be a finite number from 1 to {rate_max} Hz"
        );
        let exposure = read_f64(0x110);
        ensure!(
            exposure.is_finite() && (10.0..=1_000_000.0).contains(&exposure),
            "simulator exposure must be a finite number from 10 to 1000000 microseconds"
        );
        let gain = read_f64(0x118);
        ensure!(
            gain.is_finite() && (0.0..=24.0).contains(&gain),
            "simulator gain must be a finite number from 0 to 24 dB"
        );
        ensure!(
            read_u32(0x130) <= 1,
            "simulator acquisition control must be 0 (stop) or 1 (start)"
        );
        self.memory = proposed;
        self.refresh_payload();
        Ok(())
    }

    fn check_access(name: &str, node: Node, write: bool) -> Result<()> {
        match access(node) {
            Some(Access::ReadOnly) if write => bail!("{name} is read-only"),
            Some(Access::WriteOnly) if !write => bail!("{name} is write-only"),
            _ => Ok(()),
        }
    }

    /// `NodeMap::get`.
    pub fn get(&mut self, name: &str) -> Result<Value> {
        let n = node(name)?;
        Self::check_access(name, n, false)?;
        match n {
            Node::IntReg { address, len, .. } => {
                let bytes = self.read_memory(address, len)?;
                let raw = bytes
                    .iter()
                    .enumerate()
                    .fold(0u64, |v, (i, b)| v | (u64::from(*b) << (i * 8)));
                Ok(Value::PosInt(raw))
            }
            Node::FloatReg { address, len } => {
                let bytes = self.read_memory(address, len)?;
                let v = f64::from_le_bytes(bytes.try_into().unwrap());
                ensure!(
                    v.is_finite(),
                    "register contains non-finite floating point value"
                );
                Ok(Value::float(v))
            }
            Node::Integer { link, .. } | Node::Float { link, .. } => self.get(link),
            Node::Enumeration {
                link,
                value,
                entries,
                ..
            } => {
                let raw = match link {
                    Some(link) => self.get(link)?,
                    None => literal(value)?,
                };
                for (symbol, entry) in entries {
                    if literal(entry)? == raw {
                        return Ok(Value::Str((*symbol).into()));
                    }
                }
                let shown = match raw {
                    Value::PosInt(v) => v.to_string(),
                    other => format!("{other:?}"),
                };
                bail!("{name} contains unknown enumeration value {shown}")
            }
            Node::Command { .. } => Ok(Value::Null),
            Node::Category => bail!("unsupported GenApi node type Category ({name})"),
        }
    }

    /// `NodeMap::set` as `SessionCommand::Set` runs it, then the re-read
    /// value of the reply.
    pub fn set(&mut self, name: &str, text: &str) -> Result<Value> {
        let n = node(name)?;
        let value = input_value(name, kind(n), text)?;
        self.set_inner(name, &value)
            .map_err(|error| format!("cannot set {name} to {text:?}: {error}"))?;
        self.get(name)
    }

    fn set_inner(&mut self, name: &str, value: &Value) -> Result<()> {
        let n = node(name)?;
        Self::check_access(name, n, true)?;
        let translated = match n {
            Node::Category | Node::Command { .. } => {
                bail!("unsupported writable node type {}", kind(n))
            }
            Node::Enumeration { entries, .. } => {
                let symbol = value
                    .as_str()
                    .ok_or_else(|| "expected enumeration symbol".to_string())?;
                let Some((_, entry)) = entries.iter().find(|(s, _)| *s == symbol) else {
                    let names: Vec<&str> = entries.iter().map(|(s, _)| *s).collect();
                    bail!(
                        "unknown {name} choice {symbol:?}; choose one of: {}",
                        names.join(", ")
                    );
                };
                literal(entry)?
            }
            _ => value.clone(),
        };
        let (min, max, inc) = match n {
            Node::Integer {
                bounds: Some((min, max, inc)),
                ..
            } => (
                Some(integer_literal(min)?),
                Some(integer_literal(max)?),
                Some(integer_literal(inc)?),
            ),
            Node::Float { min, max, .. } => (Some(literal(min)?), Some(literal(max)?), None),
            _ => (None, None, None),
        };
        if !matches!(n, Node::Enumeration { .. }) {
            validate_numeric(name, kind(n), value, min, max, inc)?;
        }
        match n {
            Node::Integer { link, .. } | Node::Float { link, .. } => {
                self.set_inner(link, &translated)
            }
            Node::Enumeration {
                link: Some(link), ..
            } => self.set_inner(link, &translated),
            Node::IntReg { address, len, .. } => {
                let bits = len * 8;
                let raw = unsigned_integer(&translated)?;
                let mask = (1u64 << bits) - 1;
                ensure!(
                    raw <= mask,
                    "{name} requires an unsigned {bits}-bit integer from 0 to {mask}"
                );
                self.write_memory(address, &raw.to_le_bytes()[..len])
            }
            Node::FloatReg { address, .. } => {
                let v = genicam::number(&translated)?;
                self.write_memory(address, &v.to_bits().to_le_bytes())
            }
            // A fixed-value enumeration holds one value: read-only ones are
            // refused above, and AcquisitionMode's only choice is Continuous.
            _ => Ok(()),
        }
    }

    /// `Camera::start`: check the payload, then execute AcquisitionStart.
    pub fn start(&mut self) -> Result<()> {
        if self.running {
            return Ok(());
        }
        let size = self.get("PayloadSize")?.as_u64().unwrap_or(0);
        ensure!(
            size > 0 && size <= 256 * 1024 * 1024,
            "camera payload exceeds 256 MiB limit"
        );
        self.running = true;
        self.execute_command("AcquisitionStart")
    }

    /// `Camera::stop`: execute AcquisitionStop, then stop the transport.
    pub fn stop(&mut self) -> Result<()> {
        if !self.running {
            return Ok(());
        }
        let acquisition = self.execute_command("AcquisitionStop");
        self.running = false;
        acquisition
    }

    fn execute_command(&mut self, name: &str) -> Result<()> {
        let Node::Command {
            link,
            command_value,
        } = node(name)?
        else {
            bail!("{name} is not a command");
        };
        let value = integer_literal(command_value)?;
        self.set_inner(link, &value)
    }

    /// The feature list the desktop shows, with each feature's live value.
    pub fn features(&mut self) -> Vec<FeatureInfo> {
        FEATURES
            .iter()
            .map(|&name| {
                let n = node(name).unwrap();
                let (value, error) = match self.get(name) {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error)),
                };
                let number = |text: &str| literal(text).ok().and_then(|v| v.as_f64());
                let (min, max, inc, unit, description) = match n {
                    Node::Integer {
                        bounds: Some((min, max, inc)),
                        description,
                        ..
                    } => (number(min), number(max), number(inc), None, description),
                    Node::Float {
                        min,
                        max,
                        unit,
                        description,
                        ..
                    } => (
                        number(min),
                        number(max),
                        None,
                        Some(unit.to_string()),
                        description,
                    ),
                    _ => (None, None, None, None, ""),
                };
                let choices = match n {
                    Node::Enumeration { entries, .. } => {
                        entries.iter().map(|(s, _)| s.to_string()).collect()
                    }
                    _ => Vec::new(),
                };
                FeatureInfo {
                    name: name.into(),
                    display_name: name.into(),
                    kind: kind(n).into(),
                    value,
                    writable: access(n) != Some(Access::ReadOnly),
                    description: description.into(),
                    unit,
                    min,
                    max,
                    inc,
                    choices,
                    error,
                }
            })
            .collect()
    }

    /// The feature list as a JSON array.
    pub fn features_json(&mut self) -> String {
        let mut json = Json::default();
        json.begin_array();
        for feature in self.features() {
            feature.write(&mut json);
        }
        json.end_array();
        json.finish()
    }

    /// Seconds from one frame to the next: the frame period, or the
    /// exposure when it is longer.
    pub fn period(&self) -> f64 {
        (1.0 / self.f64(0x120)).max(self.f64(0x110) / 1e6)
    }

    /// Render the next frame, as `Backend::next_frame` does once its
    /// frame is due.
    pub fn render(&mut self, timestamp_ns: u64) -> Result<Frame> {
        ensure!(self.running, "camera is not streaming");
        let width = self.u32(0x100);
        let height = self.u32(0x104);
        let pixel_format = self.u32(0x108);
        if (self.scene.0, self.scene.1) != (width, height) {
            self.scene = (width, height, scene(width, height));
        }
        let shift = (self.frame_id * 4) as u32;
        let illumination = 1.0;
        let exposure_reference = 10_000.0;
        let gain = (10f64.powf(self.f64(0x118) / 20.0) * self.f64(0x110) / exposure_reference
            * illumination) as f32;
        let level = |v: f32| (v * gain).clamp(0.0, 255.0) as u8;
        let wave: Vec<f32> = (0..256u32)
            .map(|i| 30.0 * (TAU * (i + shift % 256) as f32 / 256.0).sin())
            .collect();
        let color = matches!(pixel_format, RGB8 | BAYER_RG8);
        let bytes = bytes_per_pixel(pixel_format) as usize;
        let mut data = vec![0; width as usize * height as usize * bytes];
        let marker_x = (shift % width).max(1);
        for (i, (p, (base, ring))) in data.chunks_exact_mut(bytes).zip(&self.scene.2).enumerate() {
            let (x, y) = (i as u32 % width, i as u32 / width);
            let marker = x.abs_diff(marker_x) < 2 || y == height / 2;
            let intensity = base + wave[*ring as usize];
            let red = level(if marker { 230.0 } else { intensity });
            let rgb = if color {
                [
                    red,
                    level(if marker {
                        230.0
                    } else {
                        ((y * 255 / height + shift % 256) % 256) as f32
                    }),
                    level(if marker {
                        230.0
                    } else {
                        ((x + y + shift % 256) % 256) as f32
                    }),
                ]
            } else {
                [red; 3]
            };
            match pixel_format {
                RGB8 => p.copy_from_slice(&rgb),
                // Twelve significant bits with low-bit texture.
                MONO12 => {
                    p.copy_from_slice(&((u16::from(red) << 4) | (x as u16 & 15)).to_le_bytes())
                }
                BAYER_RG8 => p[0] = rgb[[0, 1, 1, 2][((y & 1) * 2 + (x & 1)) as usize]],
                _ => p[0] = red,
            }
        }
        self.frame_id += 1;
        Ok(Frame {
            id: self.frame_id,
            width,
            height,
            pixel_format,
            timestamp_ns,
            data,
        })
    }
}

/// `{"id","vendor","model","serial","transport"}` of the Pattern camera.
pub fn info_json() -> String {
    let mut json = Json::default();
    json.begin_object();
    json.field_str("id", ID);
    json.field_str("vendor", VENDOR);
    json.field_str("model", MODEL);
    json.field_str("serial", SERIAL);
    json.field_str("transport", TRANSPORT);
    json.end_object();
    json.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(frame: &Frame, x: u32, y: u32) -> Vec<u8> {
        let bytes = bytes_per_pixel(frame.pixel_format) as usize;
        let at = (y * frame.width + x) as usize * bytes;
        frame.data[at..at + bytes].to_vec()
    }

    /// The simulator report's hand-emulated vectors (±1 for libm).
    #[test]
    fn pixels_match_the_reference_vectors() {
        let mut camera = Simulator::default();
        camera.start().unwrap();
        let first = camera.render(0).unwrap();
        assert_eq!((first.id, first.width, first.height), (1, 640, 480));
        for (x, y, mono) in [
            (0, 0, 230),
            (5, 5, 20),
            (100, 100, 48),
            (320, 100, 114),
            (319, 239, 134),
            (639, 479, 165),
        ] {
            assert!(pixel(&first, x, y)[0].abs_diff(mono) <= 1, "({x}, {y})");
        }
        camera.stop().unwrap();
        camera.set("PixelFormat", "RGB8").unwrap();
        camera.start().unwrap();
        let second = camera.render(1).unwrap();
        assert_eq!(second.id, 2);
        let p = pixel(&second, 100, 100);
        for (got, want) in p.iter().zip([46u8, 57, 204]) {
            assert!(got.abs_diff(want) <= 1, "{p:?}");
        }
    }

    #[test]
    fn writes_are_checked_like_the_desktop() {
        let mut camera = Simulator::default();
        assert_eq!(
            camera.set("Width", "9999").unwrap_err(),
            "cannot set Width to \"9999\": Width violates Max=1920; enter 1920 or less"
        );
        assert_eq!(camera.set("Width", "0x320"), Ok(Value::PosInt(800)));
        assert_eq!(camera.set("Gain", "6"), Ok(Value::Float(6.0)));
        assert_eq!(
            camera.set("DeviceScanType", "Linescan").unwrap_err(),
            "cannot set DeviceScanType to \"Linescan\": DeviceScanType is read-only"
        );
        camera.start().unwrap();
        assert_eq!(
            camera.set("Width", "640").unwrap_err(),
            "cannot set Width to \"640\": stop acquisition before changing image format"
        );
        assert_eq!(
            camera.set("ExposureTime", "20000"),
            Ok(Value::Float(20000.0))
        );
        assert_eq!(camera.period(), 1.0 / 30.0);
        camera.set("AcquisitionFrameRate", "120").unwrap();
        assert_eq!(camera.period(), 0.02);
        camera.stop().unwrap();
        assert_eq!(camera.render(0).unwrap_err(), "camera is not streaming");
    }
}
