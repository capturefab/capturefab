use crate::types::{
    Backend, CameraInfo, Frame, MONO8, RGB8, RegisterIo, Transport, TransportStats,
};
use anyhow::{Context as _, Result, bail, ensure};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

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
        return Ok(());
    }
    Options::parse(selector).map(|_| ())
}

pub fn open(selector: &str, timeout: Duration) -> Result<Box<dyn Backend>> {
    validate_selector_shape(selector)?;
    if let Some(source) = replay_source(selector) {
        replay::open(source, 640, 480, 30.0, timeout)
    } else {
        if let Some(delay) = test_hook(selector, "connect=after:") {
            std::thread::sleep(Duration::from_millis(delay));
        }
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
        serial: format!("SIM{}", base(id).trim_start_matches("sim:")),
        address: None,
        profile: Default::default(),
    };
    info.profile = crate::camera_profile::detect(&info, &[]);
    info
}
/// A simulator selector without its `?options`: the camera they configure.
pub fn base(selector: &str) -> &str {
    if replay_source(selector).is_some() {
        return selector;
    }
    selector.split_once('?').map_or(selector, |(base, _)| base)
}

/// What a selector's `?options` ask of the simulated camera: frames lost
/// on the way or arriving incomplete (`drop=0.02`, a share of frames for
/// each), and a hardware line that fires on a schedule (`line0=every:500ms`;
/// without it Line0 never fires, and Line1 never does).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Options {
    drop: f64,
    line0: Option<Duration>,
}
impl Options {
    fn parse(selector: &str) -> Result<Self> {
        let mut options = Self::default();
        let Some((_, query)) = selector.split_once('?') else {
            return Ok(options);
        };
        for option in query.split('&').filter(|option| !option.is_empty()) {
            let (key, value) = option.split_once('=').unwrap_or((option, ""));
            match key {
                "drop" => {
                    options.drop = value
                        .parse::<f64>()
                        .ok()
                        .filter(|share| (0.0..=0.5).contains(share))
                        .with_context(|| {
                            format!("simulator option drop={value} must be a share from 0 to 0.5, such as drop=0.02")
                        })?;
                }
                "line0" => {
                    let every = value
                        .strip_prefix("every:")
                        .and_then(|every| crate::types::parse_duration(every).ok())
                        .filter(|every| *every >= Duration::from_millis(1))
                        .with_context(|| {
                            format!("simulator option line0={value} must be every:DURATION, such as line0=every:500ms")
                        })?;
                    options.line0 = Some(every);
                }
                // Fault-test hooks, honored only with CAPTUREFAB_TEST_HOOKS=1.
                "panic" | "connect" | "exclusive" | "linger" => {}
                _ => bail!("unknown simulator option {key}; use drop=0.02 or line0=every:500ms"),
            }
        }
        Ok(options)
    }
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
        match base(selector)
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
    /// Whether it has exposure, gain, triggers and the other controls of an
    /// image sensor; depth and thermal cameras deliver calibrated samples.
    fn analog(self) -> bool {
        !matches!(self, Self::Depth | Self::Thermal)
    }
}

const XML: &str = r#"<?xml version="1.0"?>
<RegisterDescription ModelName="$Model" VendorName="Capturefab" StandardNameSpace="None" SchemaMajorVersion="1" SchemaMinorVersion="1" SchemaSubMinorVersion="0" MajorVersion="1" MinorVersion="0" SubMinorVersion="0" ProductGuid="a31ba2cf-1b8b-444e-a05a-e90ba9b67084" VersionGuid="e1ec8b77-5fd9-421f-bf7a-8bdb85b89668" xmlns="http://www.genicam.org/GenApi/Version_1_1">
 <Category Name="Root"><pFeature>DeviceControl</pFeature><pFeature>ImageFormatControl</pFeature><pFeature>AcquisitionControl</pFeature>$UserSetCategory$SpecialCategory</Category>
 <Category Name="DeviceControl"><pFeature>DeviceScanType</pFeature></Category>
 <Enumeration Name="DeviceScanType"><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Areascan"><Value>0</Value></EnumEntry><EnumEntry Name="Linescan"><Value>1</Value></EnumEntry><Value>$ScanType</Value></Enumeration>
 <Category Name="ImageFormatControl"><pFeature>Width</pFeature><pFeature>Height</pFeature>$GeometryCategory<pFeature>PixelFormat</pFeature></Category>
 <Category Name="AcquisitionControl"><pFeature>AcquisitionMode</pFeature>$AnalogCategory<pFeature>$RateFeature</pFeature><pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature></Category>
 <Integer Name="Width"><Description>Image width in pixels. Stop acquisition before changing.</Description><pValue>WidthReg</pValue><Min>64</Min>$WidthMax<Inc>1</Inc></Integer>
 <IntReg Name="WidthReg"><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Integer Name="Height"><Description>$HeightDescription</Description><pValue>HeightReg</pValue><Min>$HeightMin</Min>$HeightMax<Inc>1</Inc></Integer>
 <IntReg Name="HeightReg"><Address>0x104</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="PixelFormat">$PixelEntries<pValue>PixelFormatReg</pValue></Enumeration>
 <IntReg Name="PixelFormatReg"><Address>0x108</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 $AnalogFeatures
 <Float Name="$RateFeature"><Description>$RateDescription</Description><pValue>RateReg</pValue><Min>1</Min><Max>$RateMax</Max><Unit>Hz</Unit></Float>
 <FloatReg Name="RateReg"><Address>0x120</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Integer Name="PayloadSize"><pValue>PayloadReg</pValue></Integer>
 <IntReg Name="PayloadReg"><Address>0x128</Address><Length>4</Length><AccessMode>RO</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="AcquisitionMode"><EnumEntry Name="Continuous"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>
 <Command Name="AcquisitionStart"><pValue>AcqReg</pValue><CommandValue>1</CommandValue></Command>
 <Command Name="AcquisitionStop"><pValue>AcqReg</pValue><CommandValue>0</CommandValue></Command>
 <IntReg Name="AcqReg"><Address>0x130</Address><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Integer Name="TLParamsLocked"><Visibility>Invisible</Visibility><Value>0</Value><Min>0</Min><Max>1</Max></Integer>
 $SpecialFeatures
</RegisterDescription>"#;

/// The sensor's full size, which binning divides.
const SENSOR_WIDTH: u32 = 1920;
const SENSOR_HEIGHT: u32 = 1080;

const GEOMETRY_FEATURES: &str = r#"
 <Integer Name="WidthMax"><Description>The widest image at the current binning.</Description><ImposedAccessMode>RO</ImposedAccessMode><pValue>WidthMaxValue</pValue></Integer>
 <IntSwissKnife Name="WidthMaxValue"><pVariable Name="B">BinningHorizontalReg</pVariable><Formula>1920/B</Formula></IntSwissKnife>
 <Integer Name="HeightMax"><Description>The tallest image at the current binning.</Description><ImposedAccessMode>RO</ImposedAccessMode><pValue>HeightMaxValue</pValue></Integer>
 <IntSwissKnife Name="HeightMaxValue"><pVariable Name="B">BinningVerticalReg</pVariable><Formula>1080/B</Formula></IntSwissKnife>
 <IntSwissKnife Name="WidthLimit"><pVariable Name="M">WidthMax</pVariable><pVariable Name="O">OffsetXReg</pVariable><Formula>M-O</Formula></IntSwissKnife>
 <IntSwissKnife Name="HeightLimit"><pVariable Name="M">HeightMax</pVariable><pVariable Name="O">OffsetYReg</pVariable><Formula>M-O</Formula></IntSwissKnife>
 <IntSwissKnife Name="OffsetXLimit"><pVariable Name="M">WidthMax</pVariable><pVariable Name="W">WidthReg</pVariable><Formula>M-W</Formula></IntSwissKnife>
 <IntSwissKnife Name="OffsetYLimit"><pVariable Name="M">HeightMax</pVariable><pVariable Name="H">HeightReg</pVariable><Formula>M-H</Formula></IntSwissKnife>
 <Integer Name="OffsetX"><Description>Left edge of the image on the sensor, at most WidthMax - Width. Stop acquisition before changing.</Description><pValue>OffsetXReg</pValue><Min>0</Min><pMax>OffsetXLimit</pMax><Inc>1</Inc></Integer>
 <IntReg Name="OffsetXReg"><Address>0x1C0</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Integer Name="OffsetY"><Description>Top edge of the image on the sensor, at most HeightMax - Height. Stop acquisition before changing.</Description><pValue>OffsetYReg</pValue><Min>0</Min><pMax>OffsetYLimit</pMax><Inc>1</Inc></Integer>
 <IntReg Name="OffsetYReg"><Address>0x1C4</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Integer Name="BinningHorizontal"><Description>Sensor columns combined into one pixel; changing it scales Width and OffsetX.</Description><pValue>BinningHorizontalReg</pValue><Min>1</Min><Max>4</Max></Integer>
 <IntReg Name="BinningHorizontalReg"><Address>0x1C8</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Integer Name="BinningVertical"><Description>Sensor rows combined into one pixel; changing it scales Height and OffsetY.</Description><pValue>BinningVerticalReg</pValue><Min>1</Min><Max>4</Max></Integer>
 <IntReg Name="BinningVerticalReg"><Address>0x1CC</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"#;

const ANALOG_FEATURES: &str = r#"
 <Float Name="ExposureTime"><pValue>ExposureReg</pValue><pIsLocked>ExposureLocked</pIsLocked><Min>10</Min><Max>1000000</Max><Unit>us</Unit></Float>
 <IntSwissKnife Name="ExposureLocked"><pVariable Name="A">ExposureAutoReg</pVariable><Formula>A!=0</Formula></IntSwissKnife>
 <Enumeration Name="ExposureAuto"><Description>Once adjusts ExposureTime until the image is mid-grey, then returns to Off; Continuous keeps adjusting. Either locks ExposureTime.</Description><EnumEntry Name="Off"><Value>0</Value></EnumEntry><EnumEntry Name="Once"><Value>1</Value></EnumEntry><EnumEntry Name="Continuous"><Value>2</Value></EnumEntry><pValue>ExposureAutoReg</pValue></Enumeration>
 <IntReg Name="ExposureAutoReg"><Address>0x190</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="TriggerSelector"><EnumEntry Name="FrameStart"><Value>0</Value></EnumEntry><EnumEntry Name="AcquisitionStart"><Value>1</Value></EnumEntry><pValue>TriggerSelectorReg</pValue><pSelected>TriggerMode</pSelected><pSelected>TriggerSource</pSelected><pSelected>TriggerActivation</pSelected><pSelected>TriggerSoftware</pSelected></Enumeration>
 <IntReg Name="TriggerSelectorReg"><Address>0x160</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="TriggerMode"><Description>On: the selected trigger starts each frame (FrameStart) or the acquisition (AcquisitionStart). Software fires on TriggerSoftware; Line0 fires only with the sim:N?line0=every:500ms option; Line1 never fires.</Description><EnumEntry Name="Off"><Value>0</Value></EnumEntry><EnumEntry Name="On"><Value>1</Value></EnumEntry><pValue>TriggerModeReg</pValue></Enumeration>
 <IntReg Name="TriggerModeReg"><Address>0x168</Address><pIndex Offset="4">TriggerSelector</pIndex><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="TriggerSource"><EnumEntry Name="Software"><Value>0</Value></EnumEntry><EnumEntry Name="Line0"><Value>1</Value></EnumEntry><EnumEntry Name="Line1"><Value>2</Value></EnumEntry><pValue>TriggerSourceReg</pValue></Enumeration>
 <IntReg Name="TriggerSourceReg"><Address>0x170</Address><pIndex Offset="4">TriggerSelector</pIndex><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="TriggerActivation"><EnumEntry Name="RisingEdge"><Value>0</Value></EnumEntry><EnumEntry Name="FallingEdge"><Value>1</Value></EnumEntry><pValue>TriggerActivationReg</pValue></Enumeration>
 <IntReg Name="TriggerActivationReg"><Address>0x178</Address><pIndex Offset="4">TriggerSelector</pIndex><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Command Name="TriggerSoftware"><pValue>TriggerSoftwareReg</pValue><CommandValue>1</CommandValue></Command>
 <IntReg Name="TriggerSoftwareReg"><Address>0x180</Address><pIndex Offset="4">TriggerSelector</pIndex><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <FloatReg Name="ExposureReg"><Address>0x110</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <Enumeration Name="GainSelector"><EnumEntry Name="AnalogAll"><Value>0</Value></EnumEntry><EnumEntry Name="DigitalAll"><Value>1</Value></EnumEntry><pValue>GainSelectorReg</pValue><pSelected>Gain</pSelected></Enumeration>
 <IntReg Name="GainSelectorReg"><Address>0x198</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Float Name="Gain"><Description>Analog and digital gain add up.</Description><pValue>GainReg</pValue><pIsLocked>GainLocked</pIsLocked><Min>0</Min><Max>24</Max><Unit>dB</Unit></Float>
 <FloatReg Name="GainReg"><Address>0x118</Address><pIndex Offset="136">GainSelector</pIndex><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
 <IntSwissKnife Name="GainLocked"><pVariable Name="A">GainAutoReg</pVariable><Formula>A!=0</Formula></IntSwissKnife>
 <Enumeration Name="GainAuto"><Description>Once adjusts the analog gain until the image is mid-grey, then returns to Off; Continuous keeps adjusting. Either locks Gain.</Description><EnumEntry Name="Off"><Value>0</Value></EnumEntry><EnumEntry Name="Once"><Value>1</Value></EnumEntry><EnumEntry Name="Continuous"><Value>2</Value></EnumEntry><pValue>GainAutoReg</pValue></Enumeration>
 <IntReg Name="GainAutoReg"><Address>0x194</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"#;

const USER_SET_FEATURES: &str = r#"
 <Category Name="UserSetControl"><pFeature>UserSetSelector</pFeature><pFeature>UserSetLoad</pFeature><pFeature>UserSetSave</pFeature><pFeature>UserSetDefault</pFeature></Category>
 <Enumeration Name="UserSetSelector"><EnumEntry Name="Default"><Value>0</Value></EnumEntry><EnumEntry Name="UserSet1"><Value>1</Value></EnumEntry><EnumEntry Name="UserSet2"><Value>2</Value></EnumEntry><pValue>UserSetSelectorReg</pValue><pSelected>UserSetLoad</pSelected><pSelected>UserSetSave</pSelected></Enumeration>
 <IntReg Name="UserSetSelectorReg"><Address>0x1B0</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Command Name="UserSetLoad"><Description>Load the selected user set's settings; stop acquisition first.</Description><pValue>UserSetLoadReg</pValue><CommandValue>1</CommandValue></Command>
 <IntReg Name="UserSetLoadReg"><Address>0x1B4</Address><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Command Name="UserSetSave"><Description>Save the current settings in the selected user set; Default holds the factory settings and cannot be saved.</Description><pValue>UserSetSaveReg</pValue><CommandValue>1</CommandValue></Command>
 <IntReg Name="UserSetSaveReg"><Address>0x1B8</Address><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
 <Enumeration Name="UserSetDefault"><Description>The user set loaded when the camera powers on.</Description><EnumEntry Name="Default"><Value>0</Value></EnumEntry><EnumEntry Name="UserSet1"><Value>1</Value></EnumEntry><EnumEntry Name="UserSet2"><Value>2</Value></EnumEntry><pValue>UserSetDefaultReg</pValue></Enumeration>
 <IntReg Name="UserSetDefaultReg"><Address>0x1BC</Address><Length>4</Length><AccessMode>RW</AccessMode><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"#;
const DEPTH_FEATURES: &str = r#"
 <Category Name="Scan3dControl"><pFeature>ComponentSelector</pFeature><pFeature>Scan3dOutputMode</pFeature><pFeature>Scan3dCoordinateScale</pFeature><pFeature>Scan3dCoordinateOffset</pFeature><pFeature>Scan3dCoordinateUnit</pFeature><pFeature>Scan3dInvalidDataFlag</pFeature><pFeature>Scan3dInvalidDataValue</pFeature></Category>
 <Enumeration Name="ComponentSelector"><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Range"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>
 <Enumeration Name="Scan3dOutputMode"><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="CalibratedC"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>
 <Float Name="Scan3dCoordinateScale"><ImposedAccessMode>RO</ImposedAccessMode><Value>1</Value></Float>
 <Float Name="Scan3dCoordinateOffset"><ImposedAccessMode>RO</ImposedAccessMode><Value>0</Value></Float>
 <Enumeration Name="Scan3dCoordinateUnit"><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Millimeter"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>
 <Boolean Name="Scan3dInvalidDataFlag"><ImposedAccessMode>RO</ImposedAccessMode><Value>1</Value></Boolean>
 <Float Name="Scan3dInvalidDataValue"><Description>Zero denotes an invalid depth sample.</Description><ImposedAccessMode>RO</ImposedAccessMode><Value>0</Value></Float>"#;

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
 <Enumeration Name="TemperatureLinearMode"><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="On"><Value>1</Value></EnumEntry><Value>1</Value></Enumeration>
 <Float Name="TemperatureLinearResolution"><Description>Multiply each raw pixel by this value to obtain Kelvin.</Description><ImposedAccessMode>RO</ImposedAccessMode><Value>0.01</Value><Unit>K</Unit></Float>
 <Enumeration Name="TemperatureUnit"><ImposedAccessMode>RO</ImposedAccessMode><EnumEntry Name="Kelvin"><Value>0</Value></EnumEntry><Value>0</Value></Enumeration>"#;

const MONO12: u32 = 0x0110_0005;
const MONO12P: u32 = 0x010c_0047;
const MONO16: u32 = 0x0110_0007;
const COORD3D_C16: u32 = 0x0110_00b8;
const BAYER_RG8: u32 = 0x0108_0009;
const BAYER_RG12: u32 = 0x0110_0011;
const MONO14P: u32 = 0x010e_0104;
const BAYER_RG12P: u32 = 0x010c_0059;
const RGB12: u32 = 0x0230_001a;
/// The pixel formats of the image sensor kinds.
const SENSOR_FORMATS: [(&str, u32); 10] = [
    ("Mono8", MONO8),
    ("RGB8", RGB8),
    ("Mono12", MONO12),
    ("Mono12p", MONO12P),
    ("Mono16", MONO16),
    ("BayerRG8", BAYER_RG8),
    ("BayerRG12", BAYER_RG12),
    ("Mono14p", MONO14P),
    ("BayerRG12p", BAYER_RG12P),
    ("RGB12", RGB12),
];

// Register addresses.
const WIDTH: usize = 0x100;
const HEIGHT: usize = 0x104;
const PIXEL_FORMAT: usize = 0x108;
const EXPOSURE: usize = 0x110;
const GAIN: usize = 0x118;
const RATE: usize = 0x120;
const PAYLOAD: usize = 0x128;
const ACQUISITION: usize = 0x130;
const FOCUS: usize = 0x140;
const MAGNIFICATION: usize = 0x148;
const ILLUMINATION: usize = 0x150;
const TRIGGER_SELECTOR: usize = 0x160;
/// TriggerMode, TriggerSource, TriggerActivation and TriggerSoftware hold
/// one register per TriggerSelector value, four bytes apart.
const TRIGGER_MODE: usize = 0x168;
const TRIGGER_SOURCE: usize = 0x170;
const TRIGGER_SOFTWARE: usize = 0x180;
const EXPOSURE_AUTO: usize = 0x190;
const GAIN_AUTO: usize = 0x194;
const DIGITAL_GAIN: usize = 0x1A0;
const USER_SET_SELECTOR: usize = 0x1B0;
const USER_SET_LOAD: usize = 0x1B4;
const USER_SET_SAVE: usize = 0x1B8;
const OFFSET_X: usize = 0x1C0;
const OFFSET_Y: usize = 0x1C4;
const BINNING_H: usize = 0x1C8;
const BINNING_V: usize = 0x1CC;
const MEMORY: usize = 0x200;
/// Registers that hold no setting: computed, or commands that act at once.
const VOLATILE: [(usize, usize); 3] = [
    (PAYLOAD, ACQUISITION + 4),
    (TRIGGER_SOFTWARE, TRIGGER_SOFTWARE + 8),
    (USER_SET_LOAD, USER_SET_SAVE + 4),
];
/// Registers that may change while the camera streams: exposure, gain,
/// rate, triggers, auto functions, user set selection and the microscope's
/// optics.
const STREAMING: [(usize, usize); 4] = [
    (EXPOSURE, PAYLOAD),
    (TRIGGER_SELECTOR, DIGITAL_GAIN + 8),
    (USER_SET_SELECTOR, USER_SET_SELECTOR + 4),
    (USER_SET_SAVE, USER_SET_SAVE + 8),
];
/// The trigger selector values.
const FRAME_START: usize = 0;
const ACQUISITION_START: usize = 1;
/// Trigger sources.
const SOFTWARE: u32 = 0;
const LINE0: u32 = 1;
/// The layout of saved settings; settings saved with another are ignored.
const LAYOUT: u32 = 1;
/// The brightness the camera's own auto exposure and gain aim for.
const AUTO_TARGET: f64 = 0.45 * 255.0;

fn scene(width: u32, height: u32) -> Vec<(f32, u8)> {
    (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| {
            let d = (x as f32 - width as f32 * 0.5).hypot(y as f32 - height as f32 * 0.5);
            let ring = (d * 0.11 / std::f32::consts::TAU * 256.0) as u32 as u8;
            (40.0 + 130.0 * x as f32 / width as f32, ring)
        })
        .collect()
}

pub struct Simulator {
    kind: SimKind,
    options: Options,
    memory: Vec<u8>,
    /// The factory settings, which the Default user set loads.
    factory: Vec<u8>,
    /// The registers UserSetSave copied into UserSet1 and UserSet2.
    user_sets: [Option<Vec<u8>>; 2],
    running: bool,
    /// When a free-running sensor reads out its next frame.
    next: Instant,
    epoch: Instant,
    frame_id: u64,
    scene: (u32, u32, Vec<(f32, u8)>),
    /// Panic after this many frames: `?panic=after:N`, a fault-test hook
    /// honored only with CAPTUREFAB_TEST_HOOKS=1.
    panic_after: Option<u64>,
    /// Go on streaming after a capture as a live FFmpeg source does:
    /// `?linger=1`, a test hook honored only with CAPTUREFAB_TEST_HOOKS=1.
    lingers: bool,
    /// When each software frame trigger that no frame answered yet fired.
    triggers: VecDeque<Instant>,
    /// The last Line0 edge taken: only later edges start frames.
    line_edge: Instant,
    /// An acquisition still waiting for its AcquisitionStart trigger.
    armed: bool,
    random: u64,
    stats: TransportStats,
}
/// The frame count of a selector's `?panic=after:N`, when test hooks are on.
fn panic_after(selector: &str) -> Option<u64> {
    test_hook(selector, "panic=after:")
}
/// Whether a selector's `?exclusive=1` makes the simulator a device one
/// process at a time owns, as a GigE camera is, when test hooks are on.
pub fn exclusive(selector: &str) -> bool {
    test_hook(selector, "exclusive=") == Some(1)
}
/// The number after `prefix` in a selector's options, when test hooks are
/// on: `?panic=after:N` frames, or `?connect=after:MS`, which makes opening
/// the camera take that long.
fn test_hook(selector: &str, prefix: &str) -> Option<u64> {
    if std::env::var_os("CAPTUREFAB_TEST_HOOKS").is_none_or(|hooks| hooks != "1") {
        return None;
    }
    selector
        .split_once('?')?
        .1
        .split('&')
        .find_map(|option| option.strip_prefix(prefix)?.parse().ok())
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
        let now = Instant::now();
        let mut s = Self {
            kind,
            options: Options::parse(selector).unwrap_or_default(),
            memory: vec![0; MEMORY],
            factory: Vec::new(),
            user_sets: [None, None],
            running: false,
            next: now,
            epoch: now,
            frame_id: 0,
            scene: (0, 0, Vec::new()),
            panic_after: panic_after(selector),
            lingers: test_hook(selector, "linger=") == Some(1),
            triggers: VecDeque::new(),
            line_edge: now,
            armed: false,
            // A fixed seed: the same option loses the same frames each run.
            random: 0x9e37_79b9_7f4a_7c15,
            stats: TransportStats::default(),
        };
        s.put_u32(WIDTH, 640);
        s.put_u32(HEIGHT, if kind == SimKind::Line { 16 } else { 480 });
        s.put_u32(
            PIXEL_FORMAT,
            match kind {
                SimKind::Depth => COORD3D_C16,
                SimKind::Thermal => MONO16,
                _ => MONO8,
            },
        );
        s.put_f64(
            EXPOSURE,
            if kind == SimKind::Line {
                1_000.0
            } else {
                10_000.0
            },
        );
        s.put_f64(RATE, if kind == SimKind::Line { 480.0 } else { 30.0 });
        s.put_f64(FOCUS, 50.0);
        s.put_f64(MAGNIFICATION, 1.0);
        s.put_f64(ILLUMINATION, 100.0);
        s.put_u32(BINNING_H, 1);
        s.put_u32(BINNING_V, 1);
        s.refresh_payload();
        s.factory = s.memory.clone();
        s
    }

    fn genicam_xml(&self) -> String {
        let analog = self.kind.analog();
        let pixel_entries = match self.kind {
            SimKind::Depth => {
                r#"<EnumEntry Name="Coord3D_C16"><Value>0x011000b8</Value></EnumEntry>"#.to_owned()
            }
            SimKind::Thermal => {
                r#"<EnumEntry Name="Mono16"><Value>0x01100007</Value></EnumEntry>"#.to_owned()
            }
            _ => SENSOR_FORMATS
                .iter()
                .map(|(name, value)| {
                    format!(r#"<EnumEntry Name="{name}"><Value>{value:#010x}</Value></EnumEntry>"#)
                })
                .collect(),
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
        let analog_features = if analog {
            [ANALOG_FEATURES, GEOMETRY_FEATURES, USER_SET_FEATURES].concat()
        } else {
            String::new()
        };
        // GenApi model names are C identifiers.
        XML.replace("$Model", &self.kind.model().replace(' ', "_"))
            .replace("$SpecialCategory", special_category)
            .replace("$SpecialFeatures", special_features)
            .replace("$UserSetCategory", if analog { "<pFeature>UserSetControl</pFeature>" } else { "" })
            .replace("$GeometryCategory", if analog { "<pFeature>WidthMax</pFeature><pFeature>HeightMax</pFeature><pFeature>OffsetX</pFeature><pFeature>OffsetY</pFeature><pFeature>BinningHorizontal</pFeature><pFeature>BinningVertical</pFeature>" } else { "" })
            .replace("$WidthMax", if analog { "<pMax>WidthLimit</pMax>" } else { "<Max>1920</Max>" })
            .replace("$HeightMax", if analog { "<pMax>HeightLimit</pMax>" } else { "<Max>1080</Max>" })
            .replace("$ScanType", if line { "1" } else { "0" })
            .replace("$HeightMin", if line { "1" } else { "64" })
            .replace("$HeightDescription", if line {
                "Lines per delivered image block. One line is valid; larger blocks group consecutive lines."
            } else {
                "Image height in pixels. Stop acquisition before changing."
            })
            .replace("$PixelEntries", &pixel_entries)
            .replace("$AnalogCategory", if analog { "<pFeature>ExposureTime</pFeature><pFeature>ExposureAuto</pFeature><pFeature>GainSelector</pFeature><pFeature>Gain</pFeature><pFeature>GainAuto</pFeature><pFeature>TriggerSelector</pFeature><pFeature>TriggerMode</pFeature><pFeature>TriggerSource</pFeature><pFeature>TriggerActivation</pFeature><pFeature>TriggerSoftware</pFeature>" } else { "" })
            .replace("$AnalogFeatures", &analog_features)
            .replace("$RateFeature", if line { "AcquisitionLineRate" } else { "AcquisitionFrameRate" })
            .replace("$RateMax", if line { "100000" } else { "120" })
            .replace("$RateDescription", if line {
                "Requested lines per second. Each image contains Height consecutive lines; exposure may limit the line rate."
            } else {
                "Requested frames per second. Exposure may limit the frame rate."
            })
    }

    fn u32(&self, p: usize) -> u32 {
        read_u32(&self.memory, p)
    }
    fn f64(&self, p: usize) -> f64 {
        read_f64(&self.memory, p)
    }
    fn put_u32(&mut self, p: usize, v: u32) {
        self.memory[p..p + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_f64(&mut self, p: usize, v: f64) {
        self.memory[p..p + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn refresh_payload(&mut self) {
        let bytes = crate::frame::row_bytes(self.u32(WIDTH), self.u32(PIXEL_FORMAT))
            * self.u32(HEIGHT) as usize;
        self.put_u32(PAYLOAD, u32::try_from(bytes).unwrap_or(u32::MAX));
    }
    /// Check every setting in `memory` before it becomes the camera's.
    fn validate(&self, memory: &[u8]) -> Result<()> {
        let value = |p| read_u32(memory, p);
        let (width, height, format) = (value(WIDTH), value(HEIGHT), value(PIXEL_FORMAT));
        let (binning_h, binning_v) = (value(BINNING_H), value(BINNING_V));
        ensure!(
            (1..=4).contains(&binning_h) && (1..=4).contains(&binning_v),
            "simulator binning must be 1 to 4"
        );
        let (width_max, height_max) = (SENSOR_WIDTH / binning_h, SENSOR_HEIGHT / binning_v);
        let height_min = if self.kind == SimKind::Line { 1 } else { 64 };
        ensure!(
            (64..=width_max).contains(&width) && (height_min..=height_max).contains(&height),
            "simulator dimensions out of bounds"
        );
        ensure!(
            value(OFFSET_X) <= width_max - width && value(OFFSET_Y) <= height_max - height,
            "the image must stay on the sensor: OffsetX + Width at most {width_max}, OffsetY + Height at most {height_max}"
        );
        ensure!(
            match self.kind {
                SimKind::Depth => format == COORD3D_C16,
                SimKind::Thermal => format == MONO16,
                _ => SENSOR_FORMATS.iter().any(|(_, known)| *known == format),
            },
            "unsupported simulator pixel format"
        );
        let rate = read_f64(memory, RATE);
        let rate_max = if self.kind == SimKind::Line {
            100_000.0
        } else {
            120.0
        };
        ensure!(
            rate.is_finite() && (1.0..=rate_max).contains(&rate),
            "simulator acquisition rate must be a finite number from 1 to {rate_max} Hz"
        );
        let exposure = read_f64(memory, EXPOSURE);
        ensure!(
            exposure.is_finite() && (10.0..=1_000_000.0).contains(&exposure),
            "simulator exposure must be a finite number from 10 to 1000000 microseconds"
        );
        for gain in [read_f64(memory, GAIN), read_f64(memory, DIGITAL_GAIN)] {
            ensure!(
                gain.is_finite() && (0.0..=24.0).contains(&gain),
                "simulator gain must be a finite number from 0 to 24 dB"
            );
        }
        ensure!(
            value(ACQUISITION) <= 1,
            "simulator acquisition control must be 0 (stop) or 1 (start)"
        );
        for (register, max, name) in [
            (TRIGGER_SELECTOR, 1, "trigger selector"),
            (TRIGGER_MODE, 1, "trigger mode"),
            (TRIGGER_MODE + 4, 1, "trigger mode"),
            (TRIGGER_SOURCE, 2, "trigger source"),
            (TRIGGER_SOURCE + 4, 2, "trigger source"),
            (EXPOSURE_AUTO, 2, "exposure auto"),
            (GAIN_AUTO, 2, "gain auto"),
            (GAIN_AUTO + 4, 1, "gain selector"),
            (USER_SET_SELECTOR, 2, "user set selector"),
            (USER_SET_SAVE + 4, 2, "user set default"),
        ] {
            ensure!(value(register) <= max, "invalid simulator {name}");
        }
        if self.kind == SimKind::Microscope {
            for (address, min, max, name) in [
                (FOCUS, 0.0, 100.0, "focus position"),
                (MAGNIFICATION, 1.0, 40.0, "magnification"),
                (ILLUMINATION, 0.0, 100.0, "illumination intensity"),
            ] {
                let value = read_f64(memory, address);
                ensure!(
                    value.is_finite() && (min..=max).contains(&value),
                    "simulator {name} must be a finite number from {min} to {max}"
                );
            }
        }
        Ok(())
    }
    /// A new binning scales the image and its offsets, as cameras do, to
    /// keep the same part of the sensor.
    fn rebin(&self, proposed: &mut [u8]) {
        let line = self.kind == SimKind::Line;
        for (binning, size, offset, sensor, min) in [
            (BINNING_H, WIDTH, OFFSET_X, SENSOR_WIDTH, 64),
            (
                BINNING_V,
                HEIGHT,
                OFFSET_Y,
                SENSOR_HEIGHT,
                if line { 1 } else { 64 },
            ),
        ] {
            let (before, after) = (self.u32(binning), read_u32(proposed, binning));
            if before == after || !(1..=4).contains(&after) {
                continue;
            }
            let max = sensor / after;
            let scaled = (read_u32(proposed, size) * before / after).clamp(min, max);
            let moved = (read_u32(proposed, offset) * before / after).min(max - scaled);
            proposed[size..size + 4].copy_from_slice(&scaled.to_le_bytes());
            proposed[offset..offset + 4].copy_from_slice(&moved.to_le_bytes());
        }
    }
    /// A TriggerSoftware for trigger `selector`; ignored while the camera
    /// is stopped, or when that trigger is off or has another source.
    fn trigger(&mut self, selector: usize) {
        if !self.running || !self.triggered_by(selector, SOFTWARE) {
            return;
        }
        if selector == ACQUISITION_START {
            self.armed = false;
        } else if self.triggers.len() < 64 {
            self.triggers.push_back(Instant::now());
        }
    }
    /// Whether trigger `selector` is On with source `source`.
    fn triggered_by(&self, selector: usize, source: u32) -> bool {
        self.u32(TRIGGER_MODE + 4 * selector) == 1
            && self.u32(TRIGGER_SOURCE + 4 * selector) == source
    }
    fn trigger_on(&self, selector: usize) -> bool {
        self.u32(TRIGGER_MODE + 4 * selector) == 1
    }
    /// The first Line0 edge after `after`, when the line fires at all.
    fn line_edge_after(&self, after: Instant) -> Option<Instant> {
        let every = self.options.line0?;
        let since = after.saturating_duration_since(self.epoch).as_nanos();
        let edges = since / every.as_nanos() + 1;
        Some(self.epoch + every * u32::try_from(edges).ok()?)
    }
    fn exposure(&self) -> Duration {
        if self.kind.analog() {
            Duration::from_secs_f64(self.f64(EXPOSURE) / 1e6)
        } else {
            Duration::ZERO
        }
    }
    /// When the next frame is read out, and what starts it; None while it
    /// waits for a trigger that has not fired.
    fn next_readout(&self) -> Option<(Instant, Start)> {
        let gate = if self.armed {
            if !self.triggered_by(ACQUISITION_START, LINE0) {
                return None;
            }
            Some(self.line_edge_after(self.line_edge)?)
        } else {
            None
        };
        if !self.trigger_on(FRAME_START) {
            let readout = gate.map_or(self.next, |edge| edge.max(self.next));
            return Some((readout, Start::FreeRun));
        }
        let start = match self.u32(TRIGGER_SOURCE + 4 * FRAME_START) {
            SOFTWARE => *self.triggers.front()?,
            LINE0 => self.line_edge_after(gate.unwrap_or(self.line_edge).max(self.line_edge))?,
            _ => return None,
        };
        let source = if self.u32(TRIGGER_SOURCE + 4 * FRAME_START) == SOFTWARE {
            Start::Software
        } else {
            Start::Line(start)
        };
        Some((start + self.exposure(), source))
    }
    /// Lose this frame on the way, as `drop` asks: true when it was lost
    /// whole, false when it arrived incomplete; either way it is counted
    /// and its block ID skipped.
    fn lose(&mut self) -> Option<bool> {
        if self.options.drop <= 0.0 {
            return None;
        }
        // xorshift64*: cheap, and the same every run.
        self.random ^= self.random >> 12;
        self.random ^= self.random << 25;
        self.random ^= self.random >> 27;
        let unit =
            (self.random.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64;
        if unit < self.options.drop {
            self.stats.lost_frames += 1;
            Some(true)
        } else if unit < 2.0 * self.options.drop {
            self.stats.incomplete_frames += 1;
            Some(false)
        } else {
            None
        }
    }
    /// The camera's own auto exposure and gain, from the brightness of the
    /// frame just read out: Once turns itself off when the image is
    /// mid-grey or a limit is reached; Continuous keeps going.
    fn adjust(&mut self, mean: f64) {
        let (exposure_auto, gain_auto) = (self.u32(EXPOSURE_AUTO), self.u32(GAIN_AUTO));
        if exposure_auto == 0 && gain_auto == 0 {
            return;
        }
        let ratio = (AUTO_TARGET / mean.max(1.0)).clamp(0.125, 8.0);
        let settled = (ratio - 1.0).abs() < 0.03;
        let mut limited = true;
        if exposure_auto != 0 {
            // Exposure stays within the frame period, as the rate asks.
            let max = (1e6 / self.f64(RATE)).min(1_000_000.0);
            let exposure = self.f64(EXPOSURE);
            let wanted = (exposure * ratio.powf(0.8)).clamp(10.0, max);
            limited = (wanted - exposure).abs() < f64::EPSILON;
            if !settled {
                self.put_f64(EXPOSURE, wanted);
            }
            if exposure_auto == 1 && (settled || limited) {
                self.put_u32(EXPOSURE_AUTO, 0);
            }
        }
        if gain_auto != 0 && (exposure_auto == 0 || limited) {
            let gain = self.f64(GAIN);
            let wanted = (gain + 20.0 * ratio.log10() * 0.8).clamp(0.0, 24.0);
            if !settled {
                self.put_f64(GAIN, wanted);
            }
            if gain_auto == 1 && (settled || (wanted - gain).abs() < f64::EPSILON) {
                self.put_u32(GAIN_AUTO, 0);
            }
        }
    }
    fn load_user_set(&mut self) -> Result<()> {
        ensure!(!self.running, "stop acquisition before loading a user set");
        let selected = self.u32(USER_SET_SELECTOR) as usize;
        let source = match selected {
            0 => self.factory.clone(),
            1 | 2 => self.user_sets[selected - 1].clone().with_context(|| {
                format!("UserSet{selected} holds no settings yet; save them with UserSetSave first")
            })?,
            _ => bail!("invalid simulator user set"),
        };
        let mut proposed = self.memory.clone();
        copy_settings(&source, &mut proposed);
        // Which set is selected and loaded at power-on stay.
        proposed[USER_SET_SELECTOR..USER_SET_SELECTOR + 4]
            .copy_from_slice(&self.memory[USER_SET_SELECTOR..USER_SET_SELECTOR + 4]);
        proposed[USER_SET_SAVE + 4..USER_SET_SAVE + 8]
            .copy_from_slice(&self.memory[USER_SET_SAVE + 4..USER_SET_SAVE + 8]);
        self.validate(&proposed)?;
        self.memory = proposed;
        self.refresh_payload();
        Ok(())
    }
    fn save_user_set(&mut self) -> Result<()> {
        let selected = self.u32(USER_SET_SELECTOR) as usize;
        ensure!(
            matches!(selected, 1 | 2),
            "the Default user set holds the factory settings; select UserSet1 or UserSet2 to save"
        );
        self.user_sets[selected - 1] = Some(self.memory.clone());
        Ok(())
    }
}
/// What starts a frame.
#[derive(Clone, Copy)]
enum Start {
    FreeRun,
    Software,
    /// The Line0 edge.
    Line(Instant),
}
fn read_u32(memory: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(memory[p..p + 4].try_into().unwrap_or_default())
}
fn read_f64(memory: &[u8], p: usize) -> f64 {
    f64::from_le_bytes(memory[p..p + 8].try_into().unwrap_or_default())
}
/// Copy every setting register of `from` into `to`, leaving the volatile
/// ones.
fn copy_settings(from: &[u8], to: &mut [u8]) {
    for (i, byte) in from.iter().enumerate().take(MEMORY).skip(WIDTH) {
        if !VOLATILE
            .iter()
            .any(|(start, end)| (*start..*end).contains(&i))
        {
            to[i] = *byte;
        }
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn unhex(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).context("invalid hex"))
        .collect()
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
        ensure!(end <= self.memory.len(), "invalid simulator address");
        // Commands act at once and keep nothing in their registers.
        match a {
            TRIGGER_SOFTWARE => {
                self.trigger(FRAME_START);
                return Ok(());
            }
            _ if a == TRIGGER_SOFTWARE + 4 => {
                self.trigger(ACQUISITION_START);
                return Ok(());
            }
            USER_SET_LOAD => return self.load_user_set(),
            USER_SET_SAVE => return self.save_user_set(),
            _ => {}
        }
        ensure!(
            !self.running
                || STREAMING
                    .iter()
                    .any(|(start, stop)| a >= *start && end <= *stop)
                || (self.kind == SimKind::Microscope && a >= FOCUS && end <= ILLUMINATION + 8)
                || (a == ACQUISITION && end <= ACQUISITION + 4),
            "stop acquisition before changing image format"
        );
        let mut proposed = self.memory.clone();
        proposed[a..end].copy_from_slice(data);
        self.rebin(&mut proposed);
        self.validate(&proposed)?;
        self.memory = proposed;
        self.refresh_payload();
        Ok(())
    }
}

impl Backend for Simulator {
    fn xml(&mut self) -> Result<String> {
        Ok(self.genicam_xml())
    }
    fn lingers(&self) -> bool {
        self.lingers
    }
    fn start(&mut self, _: usize) -> Result<()> {
        let now = Instant::now();
        self.running = true;
        self.next = now;
        self.line_edge = now;
        self.triggers.clear();
        self.armed = self.kind.analog() && self.trigger_on(ACQUISITION_START);
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.triggers.clear();
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.running, "camera is not streaming");
        if self.panic_after.is_some_and(|after| self.frame_id >= after) {
            panic!("simulated camera fault after {} frames", self.frame_id);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let Some((readout, start)) = self.next_readout().filter(|(at, _)| *at <= deadline)
            else {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                bail!("frame timeout");
            };
            std::thread::sleep(readout.saturating_duration_since(Instant::now()));
            self.armed = false;
            match start {
                Start::FreeRun => {
                    // Keep to the frame clock, as a sensor does, instead of
                    // adding the time spent drawing; after a stall, restart
                    // it rather than catch up. The frame an acquisition
                    // trigger let through restarts it too.
                    self.next = (readout.max(self.next) + self.period()).max(Instant::now());
                }
                Start::Software => {
                    self.triggers.pop_front();
                }
                Start::Line(edge) => self.line_edge = edge,
            }
            self.frame_id += 1;
            if self.lose().is_some() {
                continue;
            }
            return Ok(self.render(readout));
        }
    }
    fn stats(&self) -> Option<TransportStats> {
        (self.options.drop > 0.0).then(|| self.stats.clone())
    }
    fn emulated_settings(&mut self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "layout": LAYOUT,
            "model": self.kind.model(),
            "registers": hex(&self.memory),
            "user_sets": self.user_sets.iter().map(|set| set.as_deref().map(hex)).collect::<Vec<_>>(),
        }))
    }
    fn restore_settings(&mut self, settings: &serde_json::Value) -> Result<()> {
        ensure!(
            settings["layout"] == LAYOUT && settings["model"] == self.kind.model(),
            "settings saved for another simulator"
        );
        let saved = unhex(settings["registers"].as_str().unwrap_or_default())?;
        ensure!(
            saved.len() == MEMORY,
            "settings saved for another simulator"
        );
        let mut proposed = self.memory.clone();
        copy_settings(&saved, &mut proposed);
        self.validate(&proposed)?;
        let mut user_sets = [None, None];
        for (slot, saved) in user_sets
            .iter_mut()
            .zip(settings["user_sets"].as_array().into_iter().flatten())
        {
            if let Some(text) = saved.as_str() {
                let set = unhex(text)?;
                ensure!(set.len() == MEMORY, "settings saved for another simulator");
                *slot = Some(set);
            }
        }
        self.memory = proposed;
        self.user_sets = user_sets;
        self.refresh_payload();
        Ok(())
    }
}
impl Simulator {
    /// The time between frames of a free-running sensor.
    fn period(&self) -> Duration {
        let exposure = self.exposure().as_secs_f64();
        let period = (1.0 / self.f64(RATE)).max(exposure)
            * if self.kind == SimKind::Line {
                f64::from(self.u32(HEIGHT))
            } else {
                1.0
            };
        Duration::from_secs_f64(period)
    }
    /// Draw the frame read out at `readout`.
    fn render(&mut self, readout: Instant) -> Frame {
        let width = self.u32(WIDTH);
        let height = self.u32(HEIGHT);
        let pixel_format = self.u32(PIXEL_FORMAT);
        let (offset_x, offset_y) = (self.u32(OFFSET_X), self.u32(OFFSET_Y));
        if (self.scene.0, self.scene.1) != (width, height) {
            self.scene = (width, height, scene(width, height));
        }
        let shift = (self.frame_id.saturating_sub(1) * 4) as u32;
        let illumination = if self.kind == SimKind::Microscope {
            self.f64(ILLUMINATION) / 100.0
        } else {
            1.0
        };
        let exposure_reference = if self.kind == SimKind::Line {
            1_000.0
        } else {
            10_000.0
        };
        let gain = (10f64.powf((self.f64(GAIN) + self.f64(DIGITAL_GAIN)) / 20.0)
            * self.f64(EXPOSURE)
            / exposure_reference
            * illumination) as f32;
        let level = |v: f32| (v * gain).clamp(0.0, 255.0) as u8;
        let wave: Vec<f32> = (0..256)
            .map(|i| 30.0 * (std::f32::consts::TAU * (i + shift % 256) as f32 / 256.0).sin())
            .collect();
        let color = matches!(
            pixel_format,
            RGB8 | RGB12 | BAYER_RG8 | BAYER_RG12 | BAYER_RG12P
        );
        // Samples wider than a byte keep a sensor's low-bit texture, and a
        // clipped one is at the top of its range.
        let wide = |level: u8, bits: u32, texture: u32| -> u16 {
            if level == 255 {
                ((1u32 << bits) - 1) as u16
            } else {
                ((u32::from(level) << (bits - 8)) | (texture & ((1 << (bits - 8)) - 1))) as u16
            }
        };
        let mut sum = 0u64;
        // Every byte of every row is written below, so a recycled buffer's
        // old pixels never show.
        let mut data = crate::framebuf::pool().take_stale(self.u32(PAYLOAD) as usize);
        let row_bytes = crate::frame::row_bytes(width, pixel_format);
        for (i, (base, ring)) in self.scene.2.iter().enumerate() {
            let (x, y) = (i as u32 % width, i as u32 / width);
            let p = &mut data[y as usize * row_bytes..];
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
                let at = x as usize * 2;
                p[at..at + 2].copy_from_slice(&(sample as u16).to_le_bytes());
                continue;
            }
            // The checkerboard is fixed on the sensor, so it moves with the
            // offsets.
            let grid = (((x + offset_x) / 64 + (y + offset_y) / 64) % 2) as f32 * 25.0;
            let marker = self.kind != SimKind::Microscope
                && (x.abs_diff((shift % width).max(1)) < 2
                    || (self.kind != SimKind::Line && y == height / 2));
            let intensity = if self.kind == SimKind::Microscope {
                let magnification = self.f64(MAGNIFICATION) as f32;
                let sx = width as f32 * 0.5 + (x as f32 - width as f32 * 0.5) / magnification;
                let sy = height as f32 * 0.5 + (y as f32 - height as f32 * 0.5) / magnification;
                let blur = (self.f64(FOCUS) - 50.0).abs() as f32;
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
                    let grid = (((bx + offset_x) / 64 + (by + offset_y) / 64) % 2) as f32 * 25.0;
                    intensity += base + grid + wave[ring as usize];
                }
                intensity / 9.0
            } else {
                base + grid + wave[*ring as usize]
            };
            let red = level(if marker { 230.0 } else { intensity });
            sum += u64::from(red);
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
            let x = x as usize;
            let bayer = rgb[[0, 1, 1, 2][((y & 1) * 2 + (x as u32 & 1)) as usize]];
            match pixel_format {
                RGB8 => p[x * 3..x * 3 + 3].copy_from_slice(&rgb),
                MONO12 => {
                    p[x * 2..x * 2 + 2].copy_from_slice(&wide(red, 12, x as u32).to_le_bytes())
                }
                MONO16 => p[x * 2..x * 2 + 2]
                    .copy_from_slice(&wide(red, 16, x as u32 * 7 + y).to_le_bytes()),
                BAYER_RG12 => {
                    p[x * 2..x * 2 + 2].copy_from_slice(&wide(bayer, 12, x as u32).to_le_bytes())
                }
                // PFNC lsb packing: two 12-bit samples in three bytes. An
                // even sample clears the high nibble, which the odd one
                // after it fills, or which stays zero at an odd width.
                MONO12P => {
                    let sample = wide(red, 12, x as u32);
                    let at = x / 2 * 3;
                    if x.is_multiple_of(2) {
                        p[at] = sample as u8;
                        p[at + 1] = (sample >> 8) as u8;
                    } else {
                        p[at + 1] = (p[at + 1] & 0x0f) | ((sample & 0x0f) << 4) as u8;
                        p[at + 2] = (sample >> 4) as u8;
                    }
                }
                MONO14P => put_lsb(p, x * 14, 14, wide(red, 14, x as u32)),
                BAYER_RG12P => put_lsb(p, x * 12, 12, wide(bayer, 12, x as u32)),
                RGB12 => {
                    for (c, &level) in rgb.iter().enumerate() {
                        let at = x * 6 + c * 2;
                        p[at..at + 2].copy_from_slice(&wide(level, 12, x as u32).to_le_bytes());
                    }
                }
                BAYER_RG8 => p[x] = bayer,
                _ => p[x] = red,
            }
        }
        if self.kind.analog() {
            let pixels = u64::from(width) * u64::from(height);
            self.adjust(sum as f64 / pixels.max(1) as f64);
        }
        Frame {
            id: self.frame_id,
            width,
            height,
            pixel_format,
            device_ts_ns: Some(
                u64::try_from(readout.saturating_duration_since(self.epoch).as_nanos())
                    .unwrap_or(u64::MAX),
            ),
            wall_ms: 0,
            data: data.freeze(),
            jpeg: None,
        }
    }
}

/// Write the `bits` low bits of `sample` into `row` from bit `first` on,
/// least significant bit first, as PFNC lsb packing lays samples out. The
/// bits below `first` in its first byte are kept, and those above the
/// sample in its last byte cleared: samples written in order then fill a
/// row whose old contents were anything, with its padding bits zero.
fn put_lsb(row: &mut [u8], first: usize, bits: usize, sample: u16) {
    let value = u32::from(sample) << (first % 8);
    let kept = (1u8 << (first % 8)) - 1;
    for (i, byte) in row[first / 8..(first + bits).div_ceil(8)]
        .iter_mut()
        .enumerate()
    {
        let old = if i == 0 { *byte & kept } else { 0 };
        *byte = old | (value >> (8 * i)) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{camera::Camera, camera_profile::CameraKind};

    #[test]
    fn every_link_of_the_simulators_xml_names_a_node() {
        for selector in [
            "sim:0",
            "sim:line:a",
            "sim:depth:a",
            "sim:microscope:a",
            "sim:thermal:a",
        ] {
            let xml = Simulator::for_selector(selector).xml().unwrap();
            let nodes = crate::genicam::NodeMap::parse(&xml).unwrap();
            assert_eq!(nodes.unresolved(), None, "{selector}");
        }
    }

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
            // SFNC-mandatory acquisition features that clients set around
            // AcquisitionStart.
            assert_eq!(camera.get("AcquisitionMode").unwrap(), "Continuous");
            assert!(camera.is_writable("TLParamsLocked"), "{selector}");
            assert!(camera.set("TLParamsLocked", "1").is_err(), "{selector}");
            camera.start().unwrap();
            assert_eq!(camera.get("TLParamsLocked").unwrap(), 1, "{selector}");
            let frame = camera.next_frame(Duration::from_secs(1)).unwrap();
            assert_eq!(
                frame.data.len(),
                crate::frame::row_bytes(frame.width, frame.pixel_format) * frame.height as usize,
                "{selector}"
            );
            camera.stop().unwrap();
            assert_eq!(camera.get("TLParamsLocked").unwrap(), 0, "{selector}");
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
            camera
                .next_frame(Duration::from_secs(1))
                .unwrap()
                .data
                .to_vec()
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

    fn camera(selector: &str) -> Camera {
        Camera::open(selector, &[], Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn software_triggers_send_one_frame_each_and_lines_fire_only_when_asked() {
        let mut c = camera("sim:0");
        c.set("TriggerMode", "On").unwrap();
        c.set("TriggerSource", "Software").unwrap();
        c.set("AcquisitionFrameRate", "120").unwrap();
        c.set("ExposureTime", "1000").unwrap();
        c.start().unwrap();
        assert!(c.software_triggered());
        assert!(c.next_frame(Duration::from_millis(100)).is_err());
        c.request_frame().unwrap();
        c.request_frame().unwrap();
        let first = c.next_frame(Duration::from_millis(500)).unwrap();
        let second = c.next_frame(Duration::from_millis(500)).unwrap();
        assert_eq!(second.id, first.id + 1);
        assert!(second.device_ts_ns > first.device_ts_ns);
        assert!(c.next_frame(Duration::from_millis(100)).is_err());
        c.stop().unwrap();
        // Line1 never fires, nor Line0 without its option.
        for source in ["Line0", "Line1"] {
            c.set("TriggerSource", source).unwrap();
            c.start().unwrap();
            assert!(
                c.next_frame(Duration::from_millis(150)).is_err(),
                "{source}"
            );
            c.stop().unwrap();
        }
        let mut lined = camera("sim:0?line0=every:50ms");
        lined.set("TriggerMode", "On").unwrap();
        lined.set("TriggerSource", "Line0").unwrap();
        lined.start().unwrap();
        let started = Instant::now();
        for _ in 0..3 {
            lined.next_frame(Duration::from_secs(1)).unwrap();
        }
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(100) && took < Duration::from_millis(400),
            "{took:?}"
        );
        // An AcquisitionStart trigger holds every frame until it fires.
        let mut gated = camera("sim:0");
        gated.set("TriggerSelector", "AcquisitionStart").unwrap();
        gated.set("TriggerMode", "On").unwrap();
        gated.set("TriggerSource", "Software").unwrap();
        gated.start().unwrap();
        assert!(gated.next_frame(Duration::from_millis(100)).is_err());
        gated.request_frame().unwrap();
        gated.next_frame(Duration::from_millis(500)).unwrap();
        gated.next_frame(Duration::from_millis(500)).unwrap();
        // The frames after a line started the acquisition keep to the frame
        // rate from that frame on.
        let mut lined = camera("sim:0?line0=every:300ms");
        lined.set("TriggerSelector", "AcquisitionStart").unwrap();
        lined.set("TriggerMode", "On").unwrap();
        lined.set("TriggerSource", "Line0").unwrap();
        lined.start().unwrap();
        let times = (0..3)
            .map(|_| {
                lined
                    .next_frame(Duration::from_secs(1))
                    .unwrap()
                    .device_ts_ns
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let period = 1e9 / 30.0;
        for pair in times.windows(2) {
            let gap = (pair[1] - pair[0]) as f64;
            assert!((gap - period).abs() < period * 0.2, "{times:?}");
        }
        assert!(
            ["sim:0?drop=2", "sim:0?line0=50ms", "sim:0?fast=1"]
                .iter()
                .all(|selector| validate_selector_shape(selector).is_err())
        );
    }

    #[test]
    fn dropped_frames_leave_counted_gaps_in_the_block_ids() {
        let mut c = camera("sim:0?drop=0.2");
        c.set("Width", "64").unwrap();
        c.set("Height", "64").unwrap();
        c.set("AcquisitionFrameRate", "120").unwrap();
        c.set("ExposureTime", "100").unwrap();
        c.start().unwrap();
        let ids = (0..40)
            .map(|_| c.next_frame(Duration::from_secs(1)).unwrap().id)
            .collect::<Vec<_>>();
        let stats = c.stats().unwrap();
        let gaps = ids
            .windows(2)
            .map(|pair| pair[1] - pair[0] - 1)
            .sum::<u64>();
        assert!(
            stats.lost_frames > 0 && stats.incomplete_frames > 0,
            "{stats:?}"
        );
        assert_eq!(
            gaps,
            stats.lost_frames + stats.incomplete_frames - (ids[0] - 1)
        );
        assert!(camera("sim:0").stats().is_none());
    }

    #[test]
    fn offsets_and_binning_keep_the_image_on_the_sensor() {
        let mut c = camera("sim:0");
        assert_eq!(c.get("WidthMax").unwrap(), 1920);
        assert_eq!(c.bounds("Width").unwrap().max, Some(1920.0));
        c.set("OffsetX", "1280").unwrap();
        assert_eq!(c.bounds("Width").unwrap().max, Some(640.0));
        assert!(c.set("Width", "800").is_err());
        assert!(c.set("OffsetX", "1300").is_err());
        c.set("BinningHorizontal", "2").unwrap();
        assert_eq!(c.get("WidthMax").unwrap(), 960);
        assert_eq!(c.get("Width").unwrap(), 320);
        assert_eq!(c.get("OffsetX").unwrap(), 640);
        c.set("BinningVertical", "4").unwrap();
        assert_eq!(c.get("Height").unwrap(), 120);
        c.start().unwrap();
        assert!(
            c.set("OffsetX", "0").is_err(),
            "geometry is locked while streaming"
        );
        let frame = c.next_frame(Duration::from_secs(1)).unwrap();
        assert_eq!((frame.width, frame.height), (320, 120));
        c.stop().unwrap();
    }

    #[test]
    fn wide_and_packed_formats_carry_the_same_samples() {
        let grab = |format: &str| {
            let mut c = camera("sim:0");
            c.set("PixelFormat", format).unwrap();
            c.set("ExposureTime", "40000").unwrap();
            c.start().unwrap();
            let frame = c.next_frame(Duration::from_secs(1)).unwrap();
            assert_eq!(
                frame.data.len(),
                crate::frame::row_bytes(frame.width, frame.pixel_format) * 480
            );
            frame
        };
        let samples = |format: &str| crate::frame::unpacked(&grab(format)).unwrap().into_owned();
        let mono12 = samples("Mono12");
        let packed = samples("Mono12p");
        assert_eq!(packed.pixel_format, MONO12);
        assert_eq!(packed.data, mono12.data);
        // Clipped pixels are at full scale.
        let top = mono12
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes(*v))
            .max();
        assert_eq!(top, Some(4095));
        let mono16 = samples("Mono16");
        assert!(mono16.data.as_chunks::<2>().0.contains(&[255, 255]));
        assert_eq!(samples("BayerRG12").pixel_format, BAYER_RG12);
        // Packed BayerRG12p carries BayerRG12's samples; Mono14p keeps 14 bits.
        let bayer = samples("BayerRG12");
        assert_eq!(samples("BayerRG12p").data, bayer.data);
        let mono14 = samples("Mono14p");
        assert_eq!(mono14.pixel_format, 0x0110_0025);
        let top = mono14
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes(*v))
            .max();
        assert_eq!(top, Some(16_383));
        assert!(
            mono14
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .zip(mono12.data.as_chunks::<2>().0)
                .all(|(fourteen, twelve)| u16::from_le_bytes(*fourteen) >> 6
                    == u16::from_le_bytes(*twelve) >> 4)
        );
        // RGB12 keeps RGB8's colors in its top bits.
        let rgb12 = grab("RGB12");
        assert_eq!(rgb12.pixel_format, RGB12);
        let rgb8 = grab("RGB8");
        assert!(
            rgb12
                .data
                .as_chunks::<2>()
                .0
                .iter()
                .zip(rgb8.data.iter())
                .all(|(wide, &narrow)| (u16::from_le_bytes(*wide) >> 4) as u8 == narrow)
        );
    }

    /// Frames are rendered into recycled buffers, which in tests hold
    /// `framebuf::tests::POISON`: every byte of a packed row is written,
    /// and the bits past an odd width's last sample are zero.
    #[test]
    fn packed_rows_at_odd_widths_are_written_whole() {
        let grab = |format: &str| {
            let mut c = camera("sim:0");
            c.set("Width", "65").unwrap();
            c.set("PixelFormat", format).unwrap();
            c.start().unwrap();
            c.next_frame(Duration::from_secs(1)).unwrap()
        };
        let samples = |format: &str| crate::frame::unpacked(&grab(format)).unwrap().into_owned();
        assert_eq!(samples("Mono12p").data, samples("Mono12").data);
        assert_eq!(samples("BayerRG12p").data, samples("BayerRG12").data);
        for (format, bits) in [("Mono12p", 12), ("BayerRG12p", 12), ("Mono14p", 14)] {
            let frame = grab(format);
            let row = crate::frame::row_bytes(65, frame.pixel_format);
            assert_eq!(frame.data.len(), row * 480);
            let unused = row * 8 - 65 * bits;
            assert!(unused > 0, "{format}");
            for line in frame.data.chunks_exact(row) {
                assert_eq!(line[row - 1] >> (8 - unused), 0, "{format}");
            }
        }
    }

    #[test]
    fn the_cameras_own_auto_exposure_settles_once_or_keeps_adjusting() {
        let mut c = camera("sim:0");
        c.set("ExposureTime", "1000").unwrap();
        c.set("ExposureAuto", "Once").unwrap();
        assert!(!c.is_writable("ExposureTime"));
        c.start().unwrap();
        for _ in 0..30 {
            c.next_frame(Duration::from_secs(1)).unwrap();
            if c.get("ExposureAuto").unwrap() == "Off" {
                break;
            }
        }
        assert_eq!(c.get("ExposureAuto").unwrap(), "Off");
        let settled = c.get("ExposureTime").unwrap().as_f64().unwrap();
        assert!((7_000.0..14_000.0).contains(&settled), "{settled}");
        c.set("ExposureAuto", "Continuous").unwrap();
        c.set("GainSelector", "DigitalAll").unwrap();
        c.set("Gain", "12").unwrap();
        for _ in 0..30 {
            c.next_frame(Duration::from_secs(1)).unwrap();
        }
        assert_eq!(c.get("ExposureAuto").unwrap(), "Continuous");
        let darker = c.get("ExposureTime").unwrap().as_f64().unwrap();
        assert!(darker < settled / 2.0, "{darker} after 12 dB more gain");
        c.set("GainSelector", "AnalogAll").unwrap();
        assert_eq!(c.get("Gain").unwrap(), 0.0);
        c.stop().unwrap();
    }

    #[test]
    fn user_sets_save_and_load_settings_and_settings_move_between_processes() {
        let mut c = camera("sim:0");
        c.set("UserSetSelector", "Default").unwrap();
        assert!(c.execute("UserSetSave").is_err());
        c.set("UserSetSelector", "UserSet1").unwrap();
        assert!(c.execute("UserSetLoad").is_err(), "nothing saved yet");
        c.set("Width", "320").unwrap();
        c.set("ExposureTime", "2000").unwrap();
        c.execute("UserSetSave").unwrap();
        c.set("UserSetDefault", "UserSet1").unwrap();
        c.set("Width", "640").unwrap();
        c.execute("UserSetLoad").unwrap();
        assert_eq!(c.get("Width").unwrap(), 320);
        assert_eq!(c.get("UserSetSelector").unwrap(), "UserSet1");
        c.set("UserSetSelector", "Default").unwrap();
        c.execute("UserSetLoad").unwrap();
        assert_eq!(c.get("Width").unwrap(), 640);
        assert_eq!(c.get("ExposureTime").unwrap(), 10_000.0);
        c.set("Height", "240").unwrap();

        let mut simulator = Simulator::for_selector("sim:0");
        simulator
            .write_memory(HEIGHT as u64, &200u32.to_le_bytes())
            .unwrap();
        simulator.user_sets[0] = Some(simulator.memory.clone());
        let saved = simulator.emulated_settings().unwrap();
        let mut reopened = Simulator::for_selector("sim:0?drop=0.1");
        reopened.restore_settings(&saved).unwrap();
        assert_eq!(reopened.u32(HEIGHT), 200);
        assert!(reopened.user_sets[0].is_some());
        assert!(
            Simulator::for_selector("sim:thermal")
                .restore_settings(&saved)
                .is_err()
        );
    }
}
