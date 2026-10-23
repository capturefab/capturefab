//! Camera purpose and controls are separate from the transport carrying frames.
//!
//! Classification is deliberately conservative: acquisition geometry, bit depth,
//! vendor names and a one-row ROI are not enough to identify a specialty camera.
use crate::{genicam::FeatureInfo, types::CameraInfo};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CameraKind {
    #[default]
    Unknown,
    AreaScan,
    LineScan,
    Depth,
    Microscope,
    Thermal,
    DocumentScanner,
}

impl CameraKind {
    /// Brightness-driven auto adjustment is meaningful only for image cameras.
    /// Scientific scalar measurements and line timing need their own controls.
    pub fn supports_auto(self) -> bool {
        matches!(self, Self::Unknown | Self::AreaScan | Self::Microscope)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Camera",
            Self::AreaScan => "Area scan",
            Self::LineScan => "Line scan",
            Self::Depth => "Depth",
            Self::Microscope => "Microscope",
            Self::Thermal => "Thermal",
            Self::DocumentScanner => "Document scanner",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::Unknown => "Camera type is not reported. Available controls are shown below.",
            Self::AreaScan => {
                "Capture complete image frames. Adjust exposure, frame timing and image region."
            }
            Self::LineScan => {
                "Build an image one line at a time. Match line timing and triggers to object motion."
            }
            Self::Depth => {
                "Capture distance data. Select components and configure the measurement range and calibration."
            }
            Self::Microscope => {
                "Inspect small details. Adjust focus, magnification and illumination before capturing."
            }
            Self::Thermal => {
                "Capture thermal measurements. Configure radiometric output and temperature calibration."
            }
            Self::DocumentScanner => {
                "Capture a page on demand. Choose scan resolution, color mode and paper source."
            }
        }
    }
}

impl std::fmt::Display for CameraKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DetectionSource {
    #[default]
    Unknown,
    Transport,
    Identity,
    Features,
    DeviceMetadata,
}

impl std::fmt::Display for DetectionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unknown => "Not reported",
            Self::Transport => "Source transport",
            Self::Identity => "Camera identity",
            Self::Features => "Camera features",
            Self::DeviceMetadata => "Device metadata",
        })
    }
}

/// An advertised control family, independent of the camera's classification.
/// Individual FeatureInfo metadata remains authoritative for availability,
/// write access, choices and limits.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CameraCapability {
    Exposure,
    Gain,
    Roi,
    Binning,
    FrameRate,
    LineRate,
    Trigger,
    Encoder,
    DepthComponents,
    DepthRange,
    SpatialCalibration,
    Focus,
    Magnification,
    Illumination,
    Temperature,
    ScanResolution,
    ScanSource,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraProfile {
    pub kind: CameraKind,
    pub source: DetectionSource,
    pub evidence: Vec<String>,
    pub capabilities: Vec<CameraCapability>,
}

impl CameraProfile {
    pub fn supports(&self, capability: CameraCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn metadata_kind(value: &str) -> Option<CameraKind> {
    match normalized(value).as_str() {
        "areascan" | "areascan2d" | "areascancamera" => Some(CameraKind::AreaScan),
        "linescan" | "linescan2d" | "linescancamera" => Some(CameraKind::LineScan),
        "depth" | "depthcamera" | "depthsensor" | "areascan3d" | "linescan3d" | "3dcamera"
        | "timeofflight" | "tof" | "stereo" => Some(CameraKind::Depth),
        "microscope" | "microscopecamera" | "microscopy" => Some(CameraKind::Microscope),
        "thermal" | "thermalcamera" | "thermalimager" | "radiometric" => Some(CameraKind::Thermal),
        "documentscanner" | "scanner" => Some(CameraKind::DocumentScanner),
        _ => None,
    }
}

fn feature<'a>(features: &'a [FeatureInfo], name: &str) -> Option<&'a FeatureInfo> {
    features.iter().find(|f| f.name == name)
}

fn declared_kind(features: &[FeatureInfo], names: &[&str]) -> Option<(CameraKind, String)> {
    names.iter().find_map(|name| {
        let f = feature(features, name)?;
        let value = f.value.as_ref()?.as_str()?;
        let kind = metadata_kind(value)?;
        Some((kind, format!("{} = {}", f.name, value)))
    })
}

fn capability(features: &[FeatureInfo], names: &[&str], prefixes: &[&str]) -> bool {
    features.iter().any(|feature| {
        names.contains(&feature.name.as_str())
            || prefixes
                .iter()
                .any(|prefix| feature.name.starts_with(prefix))
    })
}

fn capabilities(features: &[FeatureInfo]) -> Vec<CameraCapability> {
    use CameraCapability::*;
    // This stable order also makes discovery and snapshots reproducible.
    [
        (
            Exposure,
            capability(features, &[], &["ExposureTime", "ExposureAuto"]),
        ),
        (Gain, capability(features, &[], &["Gain"])),
        (
            Roi,
            capability(features, &["Width", "Height", "OffsetX", "OffsetY"], &[]),
        ),
        (
            Binning,
            capability(features, &[], &["Binning", "Decimation"]),
        ),
        (
            FrameRate,
            capability(features, &[], &["AcquisitionFrameRate"]),
        ),
        (
            LineRate,
            capability(features, &[], &["AcquisitionLineRate", "LineRate"]),
        ),
        (Trigger, capability(features, &[], &["Trigger"])),
        (Encoder, capability(features, &[], &["Encoder"])),
        (
            DepthComponents,
            capability(
                features,
                &["ComponentSelector", "ComponentEnable", "Scan3dOutputMode"],
                &[],
            ),
        ),
        (
            DepthRange,
            capability(
                features,
                &[
                    "DepthMin",
                    "DepthMax",
                    "DepthRange",
                    "DepthRangeMin",
                    "DepthRangeMax",
                    "Scan3dDistanceMin",
                    "Scan3dDistanceMax",
                ],
                &[],
            ),
        ),
        (
            SpatialCalibration,
            capability(
                features,
                &["DepthScale", "DepthUnit", "StereoBaseline"],
                &["Scan3dCoordinate", "Scan3dTransform", "Scan3dAxis"],
            ),
        ),
        (Focus, capability(features, &["LensPosition"], &["Focus"])),
        (
            Magnification,
            capability(
                features,
                &["Zoom", "ZoomFactor"],
                &["Magnification", "Objective"],
            ),
        ),
        (
            Illumination,
            capability(
                features,
                &[],
                &["Illumination", "LightSource", "Lighting", "LEDIntensity"],
            ),
        ),
        (
            Temperature,
            capability(
                features,
                &[
                    "IRFormat",
                    "Emissivity",
                    "ReflectedTemperature",
                    "AtmosphericTemperature",
                    "ObjectTemperature",
                ],
                &["TemperatureLinear", "TemperatureRange", "Radiometric"],
            ),
        ),
        (
            ScanResolution,
            capability(
                features,
                &["ScanResolution", "ScanResolutionX", "ScanResolutionY"],
                &[],
            ),
        ),
        (
            ScanSource,
            capability(features, &["ScanSource", "ScanDuplex", "ScanPageSize"], &[]),
        ),
    ]
    .into_iter()
    .filter_map(|(capability, present)| present.then_some(capability))
    .collect()
}

fn specialty_features(features: &[FeatureInfo]) -> Option<(CameraKind, Vec<String>)> {
    let names_for = |names: &[&str], prefixes: &[&str]| {
        let mut found: Vec<_> = features
            .iter()
            .filter(|f| {
                names.contains(&f.name.as_str()) || prefixes.iter().any(|p| f.name.starts_with(p))
            })
            .map(|f| f.name.clone())
            .collect();
        found.sort();
        found.dedup();
        found
    };
    let mut depth = names_for(
        &[
            "DepthMin",
            "DepthMax",
            "DepthRange",
            "DepthRangeMin",
            "DepthRangeMax",
            "DepthScale",
            "DepthUnit",
            "StereoBaseline",
        ],
        &["Scan3d"],
    );
    if let Some(format) = feature(features, "PixelFormat")
        && format
            .value
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .is_some_and(|v| v.starts_with("Coord3D_"))
    {
        depth.push("PixelFormat provides Coord3D data".into());
    }
    if !depth.is_empty() {
        return Some((CameraKind::Depth, depth));
    }
    let thermal = names_for(&["IRFormat"], &["TemperatureLinear", "Radiometric"]);
    if !thermal.is_empty() {
        return Some((CameraKind::Thermal, thermal));
    }
    let magnification = names_for(&[], &["Magnification", "Objective"]);
    let focus = names_for(&[], &["Focus"]);
    // Autofocus is common on webcams. Objective/magnification together with
    // focus identifies a microscope control family without mislabeling them.
    if !magnification.is_empty() && !focus.is_empty() {
        return Some((CameraKind::Microscope, [magnification, focus].concat()));
    }
    let line = names_for(&[], &["AcquisitionLineRate", "LineRate"]);
    if !line.is_empty() {
        return Some((CameraKind::LineScan, line));
    }
    None
}

fn identity_kind(info: &CameraInfo) -> Option<CameraKind> {
    // Vendor names, source URLs and user IDs are intentionally excluded: FLIR
    // also makes visible-light cameras and a URL can contain arbitrary words.
    let words: Vec<_> = info
        .model
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let word = |value: &str| words.iter().any(|w| w == value);
    let phrase = |a: &str, b: &str| words.windows(2).any(|w| w[0] == a && w[1] == b);
    if word("microscope") || word("microscopy") {
        Some(CameraKind::Microscope)
    } else if word("realsense")
        || word("kinect")
        || word("rgbd")
        || phrase("depth", "camera")
        || phrase("depth", "sensor")
        || phrase("3d", "camera")
        || phrase("tof", "camera")
        || words.windows(3).any(|w| w == ["time", "of", "flight"])
    {
        Some(CameraKind::Depth)
    } else if phrase("thermal", "camera")
        || phrase("thermal", "imager")
        || phrase("thermal", "imaging")
    {
        Some(CameraKind::Thermal)
    } else if word("linescan") || phrase("line", "scan") {
        Some(CameraKind::LineScan)
    } else if word("areascan") || phrase("area", "scan") {
        Some(CameraKind::AreaScan)
    } else {
        None
    }
}

/// Detect from read-only metadata. Type never determines capabilities, and
/// missing metadata leaves a camera usable with its ordinary feature list.
pub fn detect(info: &CameraInfo, features: &[FeatureInfo]) -> CameraProfile {
    let capabilities = capabilities(features);
    let profile = |kind, source, evidence| CameraProfile {
        kind,
        source,
        evidence,
        capabilities: capabilities.clone(),
    };
    if info.transport == crate::types::Transport::Scanner {
        return profile(
            CameraKind::DocumentScanner,
            DetectionSource::Transport,
            vec!["Document scanner transport".into()],
        );
    }
    let declared = declared_kind(features, &["DeviceType", "CameraType", "SensorType"]);
    // Device class is authoritative. Scan geometry is less specific: depth
    // and microscope cameras commonly use an area sensor.
    if let Some((kind, evidence)) = &declared {
        return profile(
            *kind,
            DetectionSource::DeviceMetadata,
            vec![evidence.clone()],
        );
    }
    let scan = declared_kind(features, &["DeviceScanType"]);
    if let Some((kind, evidence)) = &scan
        && *kind == CameraKind::LineScan
    {
        return profile(
            *kind,
            DetectionSource::DeviceMetadata,
            vec![evidence.clone()],
        );
    }
    if let Some((kind, evidence)) = specialty_features(features) {
        return profile(kind, DetectionSource::Features, evidence);
    }
    if let Some((kind, evidence)) = declared.or(scan) {
        return profile(kind, DetectionSource::DeviceMetadata, vec![evidence]);
    }
    if let Some(kind) = identity_kind(info) {
        return profile(
            kind,
            DetectionSource::Identity,
            vec![format!("Model identifies {}", kind.label().to_lowercase())],
        );
    }
    // A conventional GenICam image plane with frame timing is a useful area
    // scan fallback; Height=1 is never interpreted as a line scan camera.
    if feature(features, "Width").is_some()
        && feature(features, "Height").is_some()
        && capabilities.contains(&CameraCapability::FrameRate)
    {
        return profile(
            CameraKind::AreaScan,
            DetectionSource::Features,
            vec!["Image dimensions and frame acquisition rate".into()],
        );
    }
    profile(CameraKind::Unknown, DetectionSource::Unknown, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Transport;
    use serde_json::json;

    fn camera(model: &str) -> CameraInfo {
        CameraInfo {
            id: "test".into(),
            transport: Transport::GigE,
            vendor: "Test".into(),
            model: model.into(),
            serial: "1".into(),
            address: None,
            profile: Default::default(),
        }
    }
    fn f(name: &str, value: serde_json::Value) -> FeatureInfo {
        FeatureInfo {
            name: name.into(),
            display_name: name.into(),
            kind: "String".into(),
            value: Some(value),
            writable: false,
            description: String::new(),
            unit: None,
            min: None,
            max: None,
            inc: None,
            choices: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn explicit_class_overrides_names_and_scan_geometry() {
        let features = [
            f("DeviceType", json!("DepthCamera")),
            f("DeviceScanType", json!("Linescan")),
            f("AcquisitionLineRate", json!(5000)),
        ];
        let p = detect(&camera("Thermal microscope camera"), &features);
        assert_eq!(p.kind, CameraKind::Depth);
        assert_eq!(p.source, DetectionSource::DeviceMetadata);
        assert!(p.supports(CameraCapability::LineRate));
    }

    #[test]
    fn specialty_controls_refine_area_geometry() {
        for (name, kind) in [
            ("Scan3dOutputMode", CameraKind::Depth),
            ("TemperatureLinearMode", CameraKind::Thermal),
        ] {
            assert_eq!(
                detect(
                    &camera("Camera"),
                    &[f("DeviceScanType", json!("Areascan")), f(name, json!("On"))]
                )
                .kind,
                kind
            );
        }
        assert_eq!(
            detect(
                &camera("Camera"),
                &[
                    f("DeviceScanType", json!("Areascan")),
                    f("FocusPosition", json!(0)),
                    f("Magnification", json!(10))
                ]
            )
            .kind,
            CameraKind::Microscope
        );
    }

    #[test]
    fn linescan_metadata_overrides_secondary_families() {
        let p = detect(
            &camera("Depth camera"),
            &[
                f("DeviceScanType", json!("Linescan")),
                f("FocusPosition", json!(0)),
                f("Magnification", json!(10)),
            ],
        );
        assert_eq!(p.kind, CameraKind::LineScan);
        assert_eq!(p.source, DetectionSource::DeviceMetadata);
    }

    #[test]
    fn explicit_3d_scan_geometry_identifies_depth_without_pixel_hints() {
        for geometry in ["Areascan3D", "Linescan3D"] {
            let p = detect(&camera("Camera"), &[f("DeviceScanType", json!(geometry))]);
            assert_eq!(p.kind, CameraKind::Depth);
            assert_eq!(p.source, DetectionSource::DeviceMetadata);
            assert!(p.capabilities.is_empty());
        }
        let p = detect(&camera("Camera"), &[f("PixelFormat", json!("Coord3D_C16"))]);
        assert_eq!(p.kind, CameraKind::Depth);
        assert_eq!(p.source, DetectionSource::Features);
        let p = detect(
            &camera("Camera"),
            &[
                f("DeviceScanType", json!("Areascan")),
                f("Scan3dSensorType", json!("LaserTriangulation")),
            ],
        );
        assert_eq!(p.kind, CameraKind::Depth);
    }

    #[test]
    fn roi_bit_depth_webcam_focus_and_vendor_are_not_types() {
        let mut info = camera("Baseline pipeline camera");
        info.vendor = "FLIR Depth Line Scan Industries".into();
        let features = [
            f("Width", json!(640)),
            f("Height", json!(1)),
            f("PixelFormat", json!("Mono16")),
            f("FocusPosition", json!(0)),
            f("DeviceTemperature", json!(30)),
        ];
        assert_eq!(detect(&info, &features).kind, CameraKind::Unknown);
        for model in [
            "Linearity camera",
            "Depth of field camera",
            "Infrared camera",
            "Thermaline camera",
        ] {
            assert_eq!(
                detect(&camera(model), &features).kind,
                CameraKind::Unknown,
                "{model}"
            );
        }
    }

    #[test]
    fn identity_hints_have_word_boundaries() {
        for (model, kind) in [
            ("USB microscope", CameraKind::Microscope),
            ("Pattern line scan camera", CameraKind::LineScan),
            ("Pattern depth camera", CameraKind::Depth),
            ("Pattern thermal camera", CameraKind::Thermal),
        ] {
            let p = detect(&camera(model), &[]);
            assert_eq!(p.kind, kind, "{model}");
            assert_eq!(p.source, DetectionSource::Identity);
            assert!(p.capabilities.is_empty());
        }
    }

    #[test]
    fn unknown_metadata_is_safe_and_capabilities_are_independent() {
        let features = [
            f("DeviceType", json!("Transmitter")),
            f("ExposureTime", json!(1000)),
            f("ScanSource", json!("Flatbed")),
        ];
        let p = detect(&camera("Generic"), &features);
        assert_eq!(p.kind, CameraKind::Unknown);
        assert!(p.supports(CameraCapability::Exposure));
        assert!(p.supports(CameraCapability::ScanSource));
        assert!(!p.supports(CameraCapability::DepthRange));
        let mut scanner = camera("Thermal camera");
        scanner.transport = Transport::Scanner;
        assert_eq!(
            detect(&scanner, &features).kind,
            CameraKind::DocumentScanner
        );
    }

    #[test]
    fn detection_is_deterministic_and_profiles_round_trip() {
        let info = camera("Generic");
        let features = [
            f("Scan3dCoordinateScale", json!(0.1)),
            f("Scan3dOutputMode", json!("CalibratedC")),
            f("ExposureTime", json!(1000)),
        ];
        let mut reversed = features.to_vec();
        reversed.reverse();
        assert_eq!(detect(&info, &features), detect(&info, &reversed));
        assert_eq!(
            serde_json::to_value(CameraKind::LineScan).unwrap(),
            json!("line_scan")
        );
        assert_eq!(
            serde_json::from_value::<CameraProfile>(
                serde_json::to_value(detect(&info, &features)).unwrap()
            )
            .unwrap(),
            detect(&info, &features)
        );
    }
}
