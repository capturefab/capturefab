use crate::types::{Backend, CameraInfo, Frame, MONO8, RGB8, RegisterIo, Transport};
use anyhow::{Result, bail, ensure};
use std::time::{Duration, Instant};

pub fn info() -> CameraInfo {
    info_named("sim:0")
}
pub fn info_named(id: &str) -> CameraInfo {
    CameraInfo {
        id: id.into(),
        transport: Transport::Simulator,
        vendor: "Capturefab".into(),
        model: "Pattern camera".into(),
        serial: format!("SIM{}", id.trim_start_matches("sim:")),
        address: None,
    }
}

const XML: &str = r#"<?xml version="1.0"?>
<RegisterDescription ModelName="Pattern camera" VendorName="Capturefab" StandardNameSpace="None">
 <Category Name="Root"><pFeature>ImageFormatControl</pFeature><pFeature>AcquisitionControl</pFeature></Category>
 <Category Name="ImageFormatControl"><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>PixelFormat</pFeature></Category>
 <Category Name="AcquisitionControl"><pFeature>ExposureTime</pFeature><pFeature>Gain</pFeature><pFeature>AcquisitionFrameRate</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature></Category>
 <Integer Name="Width"><Description>Image width in pixels. Stop acquisition before changing.</Description><pValue>WidthReg</pValue><Min>64</Min><Max>1920</Max><Inc>1</Inc></Integer>
 <IntReg Name="WidthReg"><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Integer Name="Height"><pValue>HeightReg</pValue><Min>64</Min><Max>1080</Max><Inc>1</Inc></Integer>
 <IntReg Name="HeightReg"><Address>0x104</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Enumeration Name="PixelFormat"><pValue>PixelFormatReg</pValue><pEnumEntry>Mono8</pEnumEntry><pEnumEntry>RGB8</pEnumEntry><pEnumEntry>Mono12</pEnumEntry><pEnumEntry>BayerRG8</pEnumEntry></Enumeration>
 <EnumEntry Name="Mono8"><Value>0x01080001</Value></EnumEntry><EnumEntry Name="RGB8"><Value>0x02180014</Value></EnumEntry><EnumEntry Name="Mono12"><Value>0x01100005</Value></EnumEntry><EnumEntry Name="BayerRG8"><Value>0x01080009</Value></EnumEntry>
 <IntReg Name="PixelFormatReg"><Address>0x108</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Float Name="ExposureTime"><pValue>ExposureReg</pValue><Min>10</Min><Max>1000000</Max><Unit>us</Unit></Float>
 <FloatReg Name="ExposureReg"><Address>0x110</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Float Name="Gain"><pValue>GainReg</pValue><Min>0</Min><Max>24</Max><Unit>dB</Unit></Float>
 <FloatReg Name="GainReg"><Address>0x118</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Float Name="AcquisitionFrameRate"><pValue>RateReg</pValue><Min>1</Min><Max>120</Max><Unit>Hz</Unit></Float>
 <FloatReg Name="RateReg"><Address>0x120</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer>
 <IntReg Name="PayloadReg"><Address>0x128</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
 <Command Name="AcquisitionStart"><pValue>AcqReg</pValue><CommandValue>1</CommandValue></Command>
 <Command Name="AcquisitionStop"><pValue>AcqReg</pValue><CommandValue>0</CommandValue></Command>
 <IntReg Name="AcqReg"><Address>0x130</Address><Length>4</Length><AccessMode>WO</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
</RegisterDescription>"#;

const MONO12: u32 = 0x0110_0005;
const BAYER_RG8: u32 = 0x0108_0009;
fn bytes_per_pixel(pixel_format: u32) -> u32 {
    match pixel_format {
        RGB8 => 3,
        MONO12 => 2,
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
    memory: Vec<u8>,
    running: bool,
    next: Instant,
    epoch: Instant,
    frame_id: u64,
    scene: (u32, u32, Vec<(f32, u8)>),
}
impl Default for Simulator {
    fn default() -> Self {
        let mut s = Self {
            memory: vec![0; 0x200],
            running: false,
            next: Instant::now(),
            epoch: Instant::now(),
            frame_id: 0,
            scene: (0, 0, Vec::new()),
        };
        s.put_u32(0x100, 640);
        s.put_u32(0x104, 480);
        s.put_u32(0x108, MONO8);
        s.memory[0x110..0x118].copy_from_slice(&10000_f64.to_le_bytes());
        s.memory[0x120..0x128].copy_from_slice(&30_f64.to_le_bytes());
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
            !self.running || (a >= 0x110 && end <= 0x128) || a == 0x130,
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
            (64..=1920).contains(&width) && (64..=1080).contains(&height),
            "simulator dimensions out of bounds"
        );
        ensure!(
            [MONO8, RGB8, MONO12, BAYER_RG8].contains(&format),
            "unsupported simulator pixel format"
        );
        let rate = f64::from_le_bytes(proposed[0x120..0x128].try_into()?);
        ensure!(
            rate.is_finite() && (1.0..=120.0).contains(&rate),
            "frame rate out of bounds"
        );
        self.memory = proposed;
        self.refresh_payload();
        Ok(())
    }
}
impl Backend for Simulator {
    fn xml(&mut self) -> Result<String> {
        Ok(XML.into())
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
        let gain = (10f64.powf(self.f64(0x118) / 20.0) * self.f64(0x110) / 10000.0) as f32;
        let level = |v: f32| (v * gain).clamp(0.0, 255.0) as u8;
        let wave: Vec<f32> = (0..256)
            .map(|i| 30.0 * (std::f32::consts::TAU * (i + shift % 256) as f32 / 256.0).sin())
            .collect();
        let color = matches!(pixel_format, RGB8 | BAYER_RG8);
        let bytes = bytes_per_pixel(pixel_format) as usize;
        let mut data = vec![0; width as usize * height as usize * bytes];
        for (i, (p, (base, ring))) in data.chunks_exact_mut(bytes).zip(&self.scene.2).enumerate() {
            let (x, y) = (i as u32 % width, i as u32 / width);
            let marker = x.abs_diff((shift % width).max(1)) < 2 || y == height / 2;
            let red = level(if marker {
                230.0
            } else {
                base + wave[*ring as usize]
            });
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
                // Twelve significant bits with low-bit texture, as a sensor would deliver.
                MONO12 => {
                    p.copy_from_slice(&((u16::from(red) << 4) | (x as u16 & 15)).to_le_bytes())
                }
                BAYER_RG8 => p[0] = rgb[[0, 1, 1, 2][((y & 1) * 2 + (x & 1)) as usize]],
                _ => p[0] = red,
            }
        }
        self.frame_id += 1;
        let period = (1.0 / self.f64(0x120)).max(self.f64(0x110) / 1e6);
        self.next = Instant::now() + Duration::from_secs_f64(period);
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
