use crate::types::{Backend, CameraInfo, Frame, MONO8, RGB8, RegisterIo, Transport};
use anyhow::{Result, bail, ensure};
use std::time::{Duration, Instant};

pub mod replay;

/// A replay selector keeps the path literal, including spaces and colons.
pub fn replay_source(selector: &str) -> Option<&std::path::Path> {
    selector
        .strip_prefix("sim:replay:")
        .map(std::path::Path::new)
}

pub fn validate_selector_shape(selector: &str) -> Result<()> {
    let name = selector.strip_prefix("sim:").unwrap_or(selector);
    ensure!(
        !name.trim().is_empty(),
        "simulator selector needs a name, for example sim:0"
    );
    if name == "replay" || name.starts_with("replay:") {
        ensure!(
            replay_source(selector).is_some_and(|path| !path.as_os_str().is_empty()),
            "replay simulator needs a file or directory, for example sim:replay:/data/frames"
        );
    }
    Ok(())
}

pub fn open(selector: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    validate_selector_shape(selector)?;
    if let Some(source) = replay_source(selector) {
        replay::open(source, 640, 480, 30.0, timeout)
    } else {
        Ok(Box::new(Simulator::for_selector(selector)))
    }
}

pub fn info() -> CameraInfo {
    info_named("sim:0")
}
pub fn info_named(id: &str) -> CameraInfo {
    let mut info = CameraInfo {
        id: id.into(),
        transport: Transport::Simulator,
        vendor: "Capturefab".into(),
        model: if replay_source(id).is_some() {
            "Replay camera"
        } else {
            SimKind::from_selector(id).model()
        }
        .into(),
        serial: format!("SIM{}", id.trim_start_matches("sim:")),
        address: None,
        profile: Default::default(),
    };
    info.profile = crate::camera_profile::detect(&info, &[]);
    info
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimKind {
    Area,
    Line,
    Depth,
    Microscope,
    Thermal,
}
impl SimKind {
    fn from_selector(selector: &str) -> Self {
        match selector
            .strip_prefix("sim:")
            .unwrap_or(selector)
            .split(':')
            .next()
        {
            Some("line") => Self::Line,
            Some("depth") => Self::Depth,
            Some("microscope") => Self::Microscope,
            Some("thermal") => Self::Thermal,
            _ => Self::Area,
        }
    }
    fn model(self) -> &'static str {
        match self {
            Self::Area => "Pattern camera",
            Self::Line => "Pattern line scan camera",
            Self::Depth => "Pattern depth camera",
            Self::Microscope => "Pattern microscope camera",
            Self::Thermal => "Pattern thermal camera",
        }
    }
}

const XML: &str = r#"<?xml version="1.0"?>
<RegisterDescription ModelName="$Model" VendorName="Capturefab" StandardNameSpace="None">
 <Category Name="Root"><pFeature>DeviceControl</pFeature><pFeature>ImageFormatControl</pFeature><pFeature>AcquisitionControl</pFeature>$SpecialCategory</Category>
 <Category Name="DeviceControl"><pFeature>DeviceScanType</pFeature></Category>
 <Enumeration Name="DeviceScanType"><Value>$ScanType</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Areascan"><Value>0</Value></EnumEntry><EnumEntry Name="Linescan"><Value>1</Value></EnumEntry></Enumeration>
 <Category Name="ImageFormatControl"><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature></Category>
 <Category Name="AcquisitionControl">$AnalogCategory<pFeature>$RateFeature</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature></Category>
 <Integer Name="Width"><Description>Image width in pixels. Stop acquisition before changing.</Description><pValue>WidthReg</pValue><Min>64</Min><Max>1920</Max><Inc>1</Inc></Integer>
 <IntReg Name="WidthReg"><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Integer Name="Height"><Description>$HeightDescription</Description><pValue>HeightReg</pValue><Min>$HeightMin</Min><Max>1080</Max><Inc>1</Inc></Integer>
 <IntReg Name="HeightReg"><Address>0x104</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Enumeration Name="PixelFormat"><pValue>PixelFormatReg</pValue>$PixelEntries</Enumeration>
 <IntReg Name="PixelFormatReg"><Address>0x108</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 $AnalogFeatures
 <Float Name="$RateFeature"><Description>$RateDescription</Description><pValue>RateReg</pValue><Min>1</Min><Max>$RateMax</Max><Unit>Hz</Unit></Float>
 <FloatReg Name="RateReg"><Address>0x120</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer>
 <IntReg Name="PayloadReg"><Address>0x128</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Command Name="AcquisitionStart"><pValue>AcqReg</pValue><CommandValue>1</CommandValue></Command>
 <Command Name="AcquisitionStop"><pValue>AcqReg</pValue><CommandValue>0</CommandValue></Command>
 <IntReg Name="AcqReg"><Address>0x130</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 $SpecialFeatures
</RegisterDescription>"#;

const ANALOG_FEATURES: &str = r#"
 <Float Name="ExposureTime"><pValue>ExposureReg</pValue><Min>10</Min><Max>1000000</Max><Unit>us</Unit></Float>
 <FloatReg Name="ExposureReg"><Address>0x110</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Float Name="Gain"><pValue>GainReg</pValue><Min>0</Min><Max>24</Max><Unit>dB</Unit></Float>
 <FloatReg Name="GainReg"><Address>0x118</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>"#;

const DEPTH_FEATURES: &str = r#"
 <Category Name="Scan3dControl"><pFeature>ComponentSelector</pFeature><pFeature>Scan3dOutputMode</pFeature><pFeature>Scan3dCoordinateScale</pFeature><pFeature>Scan3dCoordinateOffset</pFeature><pFeature>Scan3dCoordinateUnit</pFeature><pFeature>Scan3dInvalidDataFlag</pFeature><pFeature>Scan3dInvalidDataValue</pFeature></Category>
 <Enumeration Name="ComponentSelector"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Range"><Value>0</Value></EnumEntry></Enumeration>
 <Enumeration Name="Scan3dOutputMode"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="CalibratedC"><Value>0</Value></EnumEntry></Enumeration>
 <Float Name="Scan3dCoordinateScale"><Value>1</Value><ImposedAccessMode>RO</ImposedAccessMode></Float>
 <Float Name="Scan3dCoordinateOffset"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode></Float>
 <Enumeration Name="Scan3dCoordinateUnit"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Millimeter"><Value>0</Value></EnumEntry></Enumeration>
 <Boolean Name="Scan3dInvalidDataFlag"><Value>1</Value><ImposedAccessMode>RO</ImposedAccessMode></Boolean>
 <Float Name="Scan3dInvalidDataValue"><Description>Zero denotes an invalid depth sample.</Description><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode></Float>"#;

const MICROSCOPE_FEATURES: &str = r#"
 <Category Name="MicroscopeControl"><pFeature>FocusPosition</pFeature><pFeature>Magnification</pFeature><pFeature>IlluminationIntensity</pFeature></Category>
 <Float Name="FocusPosition"><Description>Focus on the specimen near position 50; moving away blurs the image.</Description><pValue>FocusReg</pValue><Min>0</Min><Max>100</Max><Unit>um</Unit></Float>
 <FloatReg Name="FocusReg"><Address>0x140</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Float Name="Magnification"><Description>Optical magnification of the specimen.</Description><pValue>MagnificationReg</pValue><Min>1</Min><Max>40</Max><Unit>x</Unit></Float>
 <FloatReg Name="MagnificationReg"><Address>0x148</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Float Name="IlluminationIntensity"><Description>Specimen illumination brightness.</Description><pValue>IlluminationReg</pValue><Min>0</Min><Max>100</Max><Unit>%</Unit></Float>
 <FloatReg Name="IlluminationReg"><Address>0x150</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>"#;

const THERMAL_FEATURES: &str = r#"
 <Category Name="TemperatureControl"><pFeature>TemperatureLinearMode</pFeature><pFeature>TemperatureLinearResolution</pFeature><pFeature>TemperatureUnit</pFeature></Category>
 <Enumeration Name="TemperatureLinearMode"><Value>1</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="On"><Value>1</Value></EnumEntry></Enumeration>
 <Float Name="TemperatureLinearResolution"><Description>Multiply each raw pixel by this value to obtain Kelvin.</Description><Value>0.01</Value><ImposedAccessMode>RO</ImposedAccessMode><Unit>K</Unit></Float>
 <Enumeration Name="TemperatureUnit"><Value>0</Value><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Kelvin"><Value>0</Value></EnumEntry></Enumeration>"#;

const MONO12: u32 = 0x0110_0005;
const MONO16: u32 = 0x0110_0007;
const COORD3D_C16: u32 = 0x0110_00b8;
const BAYER_RG8: u32 = 0x0108_0009;
fn bytes_per_pixel(pixel_format: u32) -> u32 {
    match pixel_format {
        RGB8 => 3,
        MONO12 | MONO16 | COORD3D_C16 => 2,
        _ => 1,
    }
}
fn scene(width: u32, height: u32) -> Vec<(f32, u8)> {
    (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| {
            let grid = ((x / 64 + y / 64) % 2) as f32 * 25.0;
            let d = (x as f32 - width as f32 * 0.5).hypot(y as f32 - height as f32 * 0.5);
            let ring = (d * 0.11 / std::f32::consts::TAU * 256.0) as u32 as u8;
            (40.0 + 130.0 * x as f32 / width as f32 + grid, ring)
        })
        .collect()
}

pub struct Simulator {
    kind: SimKind,
    memory: Vec<u8>,
    running: bool,
    next: Instant,
    epoch: Instant,
    frame_id: u64,
    scene: (u32, u32, Vec<(f32, u8)>),
}
impl Default for Simulator {
    fn default() -> Self {
        Self::for_selector("sim:0")
    }
}
impl Simulator {
    /// Typed selectors reserve their first component; arbitrary other names
    /// keep the original area-scan simulator behavior.
    pub fn for_selector(selector: &str) -> Self {
        let kind = SimKind::from_selector(selector);
        let mut s = Self {
            kind,
            memory: vec![0; 0x200],
            running: false,
            next: Instant::now(),
            epoch: Instant::now(),
            frame_id: 0,
            scene: (0, 0, Vec::new()),
        };
        s.put_u32(0x100, 640);
        s.put_u32(0x104, if kind == SimKind::Line { 16 } else { 480 });
        s.put_u32(
            0x108,
            match kind {
                SimKind::Depth => COORD3D_C16,
                SimKind::Thermal => MONO16,
                _ => MONO8,
            },
        );
        s.put_f64(
            0x110,
            if kind == SimKind::Line {
                1_000.0
            } else {
                10_000.0
            },
        );
        s.put_f64(0x120, if kind == SimKind::Line { 480.0 } else { 30.0 });
        s.put_f64(0x140, 50.0);
        s.put_f64(0x148, 1.0);
        s.put_f64(0x150, 100.0);
        s.refresh_payload();
        s
    }

    fn genicam_xml(&self) -> String {
        let analog = !matches!(self.kind, SimKind::Depth | SimKind::Thermal);
        let pixel_entries = match self.kind {
            SimKind::Depth => {
                r#"<EnumEntry Name="Coord3D_C16"><Value>0x011000b8</Value></EnumEntry>"#
            }
            SimKind::Thermal => r#"<EnumEntry Name="Mono16"><Value>0x01100007</Value></EnumEntry>"#,
            _ => {
                r#"<EnumEntry Name="Mono8"><Value>0x01080001</Value></EnumEntry><EnumEntry Name="RGB8"><Value>0x02180014</Value></EnumEntry><EnumEntry Name="Mono12"><Value>0x01100005</Value></EnumEntry><EnumEntry Name="BayerRG8"><Value>0x01080009</Value></EnumEntry>"#
            }
        };
        let (special_category, special_features) = match self.kind {
            SimKind::Depth => ("<pFeature>Scan3dControl</pFeature>", DEPTH_FEATURES),
            SimKind::Microscope => (
                "<pFeature>MicroscopeControl</pFeature>",
                MICROSCOPE_FEATURES,
            ),
            SimKind::Thermal => ("<pFeature>TemperatureControl</pFeature>", THERMAL_FEATURES),
            _ => ("", ""),
        };
        let line = self.kind == SimKind::Line;
        XML.replace("$Model", self.kind.model())
            .replace("$SpecialCategory", special_category)
            .replace("$SpecialFeatures", special_features)
            .replace("$ScanType", if line { "1" } else { "0" })
            .replace("$HeightMin", if line { "1" } else { "64" })
            .replace("$HeightDescription", if line {
                "Lines per delivered image block. One line is valid; larger blocks group consecutive lines."
            } else {
                "Image height in pixels. Stop acquisition before changing."
            })
            .replace("$PixelEntries", pixel_entries)
            .replace("$AnalogCategory", if analog { "<pFeature>ExposureTime</pFeature><pFeature>Gain</pFeature>" } else { "" })
            .replace("$AnalogFeatures", if analog { ANALOG_FEATURES } else { "" })
            .replace("$RateFeature", if line { "AcquisitionLineRate" } else { "AcquisitionFrameRate" })
            .replace("$RateMax", if line { "100000" } else { "120" })
            .replace("$RateDescription", if line {
                "Requested lines per second. Each image contains Height consecutive lines; exposure may limit the line rate."
            } else {
                "Requested frames per second. Exposure may limit the frame rate."
            })
    }

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
}
impl RegisterIo for Simulator {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        let a = usize::try_from(address)?;
        let end = a
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("address overflow"))?;
        self.refresh_payload();
        Ok(self
            .memory
            .get(a..end)
            .ok_or_else(|| anyhow::anyhow!("invalid simulator address"))?
            .to_vec())
    }
    fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        let a = usize::try_from(address)?;
        let end = a
            .checked_add(data.len())
            .ok_or_else(|| anyhow::anyhow!("address overflow"))?;
        ensure!(
            !self.running
                || (a >= 0x110 && end <= 0x128)
                || (self.kind == SimKind::Microscope && a >= 0x140 && end <= 0x158)
                || (a == 0x130 && end <= 0x134),
            "stop acquisition before changing image format"
        );
        let mut proposed = self.memory.clone();
        proposed
            .get_mut(a..end)
            .ok_or_else(|| anyhow::anyhow!("invalid simulator address"))?
            .copy_from_slice(data);
        let width = u32::from_le_bytes(proposed[0x100..0x104].try_into()?);
        let height = u32::from_le_bytes(proposed[0x104..0x108].try_into()?);
        let format = u32::from_le_bytes(proposed[0x108..0x10c].try_into()?);
        ensure!(
            (64..=1920).contains(&width)
                && ((if self.kind == SimKind::Line { 1 } else { 64 })..=1080).contains(&height),
            "simulator dimensions out of bounds"
        );
        ensure!(
            match self.kind {
                SimKind::Depth => format == COORD3D_C16,
                SimKind::Thermal => format == MONO16,
                _ => [MONO8, RGB8, MONO12, BAYER_RG8].contains(&format),
            },
            "unsupported simulator pixel format"
        );
        let rate = f64::from_le_bytes(proposed[0x120..0x128].try_into()?);
        let rate_max = if self.kind == SimKind::Line {
            100_000.0
        } else {
            120.0
        };
        ensure!(
            rate.is_finite() && (1.0..=rate_max).contains(&rate),
            "simulator acquisition rate must be a finite number from 1 to {rate_max} Hz"
        );
        let exposure = f64::from_le_bytes(proposed[0x110..0x118].try_into()?);
        ensure!(
            exposure.is_finite() && (10.0..=1_000_000.0).contains(&exposure),
            "simulator exposure must be a finite number from 10 to 1000000 microseconds"
        );
        let gain = f64::from_le_bytes(proposed[0x118..0x120].try_into()?);
        ensure!(
            gain.is_finite() && (0.0..=24.0).contains(&gain),
            "simulator gain must be a finite number from 0 to 24 dB"
        );
        let control = u32::from_le_bytes(proposed[0x130..0x134].try_into()?);
        ensure!(
            control <= 1,
            "simulator acquisition control must be 0 (stop) or 1 (start)"
        );
        if self.kind == SimKind::Microscope {
            for (address, min, max, name) in [
                (0x140, 0.0, 100.0, "focus position"),
                (0x148, 1.0, 40.0, "magnification"),
                (0x150, 0.0, 100.0, "illumination intensity"),
            ] {
                let value = f64::from_le_bytes(proposed[address..address + 8].try_into()?);
                ensure!(
                    value.is_finite() && (min..=max).contains(&value),
                    "simulator {name} must be a finite number from {min} to {max}"
                );
            }
        }
        self.memory = proposed;
        self.refresh_payload();
        Ok(())
    }
}

impl Backend for Simulator {
    fn xml(&mut self) -> Result<String> {
        Ok(self.genicam_xml())
    }
    fn start(&mut self, _: usize) -> Result<()> {
        self.running = true;
        self.next = Instant::now();
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.running, "camera is not streaming");
        let wait = self.next.saturating_duration_since(Instant::now());
        if wait > timeout {
            std::thread::sleep(timeout);
            bail!("frame timeout");
        }
        std::thread::sleep(wait);
        let width = self.u32(0x100);
        let height = self.u32(0x104);
        let pixel_format = self.u32(0x108);
        if (self.scene.0, self.scene.1) != (width, height) {
            self.scene = (width, height, scene(width, height));
        }
        let shift = (self.frame_id * 4) as u32;
        let illumination = if self.kind == SimKind::Microscope {
            self.f64(0x150) / 100.0
        } else {
            1.0
        };
        let exposure_reference = if self.kind == SimKind::Line {
            1_000.0
        } else {
            10_000.0
        };
        let gain = (10f64.powf(self.f64(0x118) / 20.0) * self.f64(0x110) / exposure_reference
            * illumination) as f32;
        let level = |v: f32| (v * gain).clamp(0.0, 255.0) as u8;
        let wave: Vec<f32> = (0..256)
            .map(|i| 30.0 * (std::f32::consts::TAU * (i + shift % 256) as f32 / 256.0).sin())
            .collect();
        let color = matches!(pixel_format, RGB8 | BAYER_RG8);
        let bytes = bytes_per_pixel(pixel_format) as usize;
        let mut data = vec![0; width as usize * height as usize * bytes];
        for (i, (p, (base, ring))) in data.chunks_exact_mut(bytes).zip(&self.scene.2).enumerate() {
            let (x, y) = (i as u32 % width, i as u32 / width);
            if matches!(self.kind, SimKind::Depth | SimKind::Thermal) {
                let dx = (x as f32 - width as f32 * (0.3 + (shift as f32 * 0.01).sin() * 0.1))
                    / width as f32;
                let dy = (y as f32 - height as f32 * 0.5) / height as f32;
                let object = dx * dx * 16.0 + dy * dy * 9.0 < 1.0;
                let sample = if self.kind == SimKind::Depth {
                    // Millimeters along calibrated C (depth), with zero denoting
                    // an invalid sample. A foreground object crosses a sloped plane.
                    if i == 0 {
                        0
                    } else if object {
                        750
                    } else {
                        1_200 + x * 2 + y
                    }
                } else {
                    // 0.01 Kelvin per unsigned sample, including a warm object.
                    28_315 + x * 1_000 / width + y * 500 / height + if object { 2_000 } else { 0 }
                };
                p.copy_from_slice(&(sample as u16).to_le_bytes());
                continue;
            }
            let marker = self.kind != SimKind::Microscope
                && (x.abs_diff((shift % width).max(1)) < 2
                    || (self.kind != SimKind::Line && y == height / 2));
            let intensity = if self.kind == SimKind::Microscope {
                let magnification = self.f64(0x148) as f32;
                let sx = width as f32 * 0.5 + (x as f32 - width as f32 * 0.5) / magnification;
                let sy = height as f32 * 0.5 + (y as f32 - height as f32 * 0.5) / magnification;
                let blur = (self.f64(0x140) - 50.0).abs() as f32;
                let mut intensity = 0.0;
                for (ox, oy) in [
                    (0.0, 0.0),
                    (-1.0, 0.0),
                    (1.0, 0.0),
                    (0.0, -1.0),
                    (0.0, 1.0),
                    (-1.0, -1.0),
                    (1.0, -1.0),
                    (-1.0, 1.0),
                    (1.0, 1.0),
                ] {
                    let bx = (sx + ox * blur).clamp(0.0, (width - 1) as f32) as u32;
                    let by = (sy + oy * blur).clamp(0.0, (height - 1) as f32) as u32;
                    let (base, ring) = self.scene.2[(by * width + bx) as usize];
                    intensity += base + wave[ring as usize];
                }
                intensity / 9.0
            } else {
                base + wave[*ring as usize]
            };
            let red = level(if marker { 230.0 } else { intensity });
            let rgb = if color && self.kind == SimKind::Microscope {
                [red, level(intensity * 0.9), level(intensity * 0.75)]
            } else if color {
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
                // Twelve significant bits with low-bit texture, as a sensor would deliver.
                MONO12 => {
                    p.copy_from_slice(&((u16::from(red) << 4) | (x as u16 & 15)).to_le_bytes())
                }
                BAYER_RG8 => p[0] = rgb[[0, 1, 1, 2][((y & 1) * 2 + (x & 1)) as usize]],
                _ => p[0] = red,
            }
        }
        self.frame_id += 1;
        let exposure = if matches!(self.kind, SimKind::Depth | SimKind::Thermal) {
            0.0
        } else {
            self.f64(0x110) / 1e6
        };
        let period = (1.0 / self.f64(0x120)).max(exposure)
            * if self.kind == SimKind::Line {
                f64::from(height)
            } else {
                1.0
            };
        // Keep to the frame clock, as a sensor does, instead of adding the time
        // spent drawing; after a stall, restart it rather than catch up.
        self.next = (self.next + Duration::from_secs_f64(period)).max(Instant::now());
        Ok(Frame {
            id: self.frame_id,
            width,
            height,
            pixel_format,
            timestamp_ns: self.epoch.elapsed().as_nanos() as u64,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{camera::Camera, camera_profile::CameraKind};

    #[test]
    fn typed_selectors_open_with_specialized_profiles_and_real_features() {
        for (selector, kind, feature, expected) in [
            ("sim:0", CameraKind::AreaScan, "DeviceScanType", "Areascan"),
            (
                "sim:bench",
                CameraKind::AreaScan,
                "DeviceScanType",
                "Areascan",
            ),
            (
                "sim:line:conveyor",
                CameraKind::LineScan,
                "DeviceScanType",
                "Linescan",
            ),
            (
                "sim:depth",
                CameraKind::Depth,
                "Scan3dOutputMode",
                "CalibratedC",
            ),
            (
                "sim:microscope",
                CameraKind::Microscope,
                "DeviceScanType",
                "Areascan",
            ),
            (
                "sim:thermal:oven",
                CameraKind::Thermal,
                "TemperatureLinearMode",
                "On",
            ),
        ] {
            let mut camera = Camera::open(selector, &[], Duration::from_secs(1)).unwrap();
            assert_eq!(camera.info.profile.kind, kind, "{selector}");
            assert_eq!(camera.get(feature).unwrap(), expected, "{selector}");
            assert!(!camera.is_writable(feature), "{selector}");
            camera.start().unwrap();
            let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
            assert_eq!(
                frame.data.len(),
                (frame.width * frame.height * bytes_per_pixel(frame.pixel_format)) as usize,
                "{selector}"
            );
            camera.stop().unwrap();
        }
    }

    #[test]
    fn line_scan_accepts_single_line_and_schedules_blocks_at_the_line_rate() {
        let mut camera = Camera::open("sim:line", &[], Duration::from_secs(1)).unwrap();
        assert!(!camera.has("AcquisitionFrameRate"));
        assert_eq!(camera.bounds("Height").unwrap().min, Some(1.0));
        camera.set("Height", "1").unwrap();
        camera.set("Width", "64").unwrap();
        camera.set("AcquisitionLineRate", "100000").unwrap();
        assert!(camera.set("AcquisitionLineRate", "100001").is_err());
        camera.start().unwrap();
        let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
        assert_eq!((frame.width, frame.height, frame.data.len()), (64, 1, 64));
        assert!(
            frame
                .data
                .windows(2)
                .any(|samples| samples[0] != samples[1])
        );
        assert!(camera.set("Height", "2").is_err());
        camera.set("AcquisitionLineRate", "1000").unwrap();
        camera.stop().unwrap();

        let mut simulator = Simulator::for_selector("sim:line");
        simulator.write_memory(0x100, &64u32.to_le_bytes()).unwrap();
        simulator.write_memory(0x104, &4u32.to_le_bytes()).unwrap();
        simulator.write_memory(0x120, &40f64.to_le_bytes()).unwrap();
        simulator.start(256).unwrap();
        let start = simulator.next;
        simulator.next_frame(Duration::from_secs(1)).unwrap();
        assert_eq!(
            simulator.next.duration_since(start),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn depth_and_thermal_stream_calibrated_samples_without_intensity_controls() {
        for (selector, format, min, max) in [
            ("sim:depth", COORD3D_C16, 750, 3000),
            ("sim:thermal", MONO16, 28_315, 31_815),
        ] {
            let mut camera = Camera::open(selector, &[], Duration::from_secs(1)).unwrap();
            assert!(!camera.has("Gain"));
            assert!(!camera.has("ExposureTime"));
            assert!(camera.set("PixelFormat", "RGB8").is_err());
            camera.start().unwrap();
            let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
            assert_eq!(frame.pixel_format, format);
            let samples: Vec<_> = frame
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|v| u16::from_le_bytes(*v))
                .collect();
            assert!(samples.iter().skip(1).all(|v| (min..=max).contains(v)));
            assert!(samples.contains(&min));
            if selector == "sim:depth" {
                assert_eq!(samples[0], 0);
                assert_eq!(camera.get("Scan3dCoordinateScale").unwrap(), 1.0);
                assert_eq!(camera.get("Scan3dCoordinateUnit").unwrap(), "Millimeter");
                assert_eq!(camera.get("Scan3dInvalidDataFlag").unwrap(), true);
                assert_eq!(camera.get("Scan3dInvalidDataValue").unwrap(), 0.0);
            } else {
                assert_eq!(camera.get("TemperatureLinearResolution").unwrap(), 0.01);
                assert!(camera.set("TemperatureLinearMode", "On").is_err());
            }
            camera.stop().unwrap();
        }
    }

    #[test]
    fn microscope_controls_change_the_specimen_and_reject_invalid_values() {
        fn specimen(setting: Option<(&str, &str)>) -> Vec<u8> {
            let mut camera = Camera::open("sim:microscope", &[], Duration::from_secs(1)).unwrap();
            camera.set("Width", "64").unwrap();
            camera.set("Height", "64").unwrap();
            if let Some((name, value)) = setting {
                camera.set(name, value).unwrap();
            }
            camera.start().unwrap();
            camera.next_frame(Duration::from_secs(1)).unwrap().data
        }
        let baseline = specimen(None);
        assert_ne!(baseline, specimen(Some(("FocusPosition", "0"))));
        assert_ne!(baseline, specimen(Some(("Magnification", "4"))));
        assert!(
            specimen(Some(("IlluminationIntensity", "0")))
                .iter()
                .all(|v| *v == 0)
        );
        let mut camera = Camera::open("sim:microscope", &[], Duration::from_secs(1)).unwrap();
        for (name, value) in [
            ("FocusPosition", "101"),
            ("Magnification", "0"),
            ("IlluminationIntensity", "-1"),
        ] {
            assert!(camera.set(name, value).is_err());
        }
        camera.start().unwrap();
        camera.set("FocusPosition", "45").unwrap();
        camera.set("Magnification", "10").unwrap();
        camera.set("IlluminationIntensity", "25").unwrap();
        assert_eq!(camera.get("IlluminationIntensity").unwrap(), 25.0);
        camera.stop().unwrap();
    }

    #[test]
    fn invalid_raw_feature_writes_leave_simulator_state_unchanged() {
        let mut simulator = Simulator::default();
        for (address, values) in [
            (0x110, [f64::NAN, f64::INFINITY, 0.0, 1_000_001.0]),
            (0x118, [f64::NAN, f64::NEG_INFINITY, -1.0, 25.0]),
            (0x120, [f64::NAN, f64::INFINITY, 0.0, 121.0]),
        ] {
            let before = simulator.read_memory(address, 8).unwrap();
            for value in values {
                assert!(
                    simulator
                        .write_memory(address, &value.to_le_bytes())
                        .is_err()
                );
                assert_eq!(simulator.read_memory(address, 8).unwrap(), before);
            }
        }
        assert!(simulator.write_memory(0x130, &2u32.to_le_bytes()).is_err());
        assert_eq!(simulator.read_memory(0x130, 4).unwrap(), [0; 4]);
    }
}
