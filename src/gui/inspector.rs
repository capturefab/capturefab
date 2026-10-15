//! The settings panel: features, auto exposure, capture, forwarding and storage.
use super::*;
use crate::camera_profile::{CameraCapability, CameraKind, CameraProfile};
use iced::widget::{Row, column};
use motion::Flashes;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// The width of every control in the panel's forms and feature rows, so
/// their edges line up.
const CONTROL: f32 = 136.0;
#[cfg(test)]
mod layout_tests;
/// Room between the panel's sections.
const SECTION_GAP: f32 = 20.0;
/// Room between the rows of a section.
const ROW_GAP: f32 = 12.0;
/// How long a segmented control's thumb takes to slide one segment.
const THUMB: Duration = Duration::from_millis(180);
/// How long a field glows once the camera has taken its value.
const ACCEPTED: Duration = Duration::from_millis(450);
/// How long the camera context row glows as the selection moves to it.
const CONTEXT: Duration = Duration::from_millis(280);
/// The panel's scrollable, for jumping back to its top.
pub(super) const SCROLL: &str = "inspector";
/// The feature groups retain every advertised control, including vendor
/// extensions. Specialized controls come first for their camera type.
fn feature_groups(kind: CameraKind) -> &'static [&'static str] {
    match kind {
        CameraKind::LineScan => &[
            "Line geometry",
            "Line timing",
            "Trigger and encoder",
            "Image",
            "Acquisition",
            "Device",
            "Transport",
            "Other",
        ],
        CameraKind::Depth => &[
            "Components",
            "Depth",
            "Calibration",
            "Image",
            "Acquisition",
            "Device",
            "Transport",
            "Other",
        ],
        CameraKind::Microscope => &[
            "Optics",
            "Illumination",
            "Stage",
            "Image",
            "Acquisition",
            "Device",
            "Transport",
            "Other",
        ],
        CameraKind::Thermal => &[
            "Temperature",
            "Image",
            "Acquisition",
            "Device",
            "Transport",
            "Other",
        ],
        CameraKind::DocumentScanner => &[
            "Scan",
            "Image",
            "Acquisition",
            "Device",
            "Transport",
            "Other",
        ],
        CameraKind::AreaScan | CameraKind::Unknown => {
            &["Image", "Acquisition", "Device", "Transport", "Other"]
        }
    }
}

/// Classify only features the camera supplies. Unknown names stay accessible
/// in the ordinary feature groups and through search.
fn adaptive_feature_group(name: &str, kind: CameraKind) -> &'static str {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| name.starts_with(prefix));
    match kind {
        CameraKind::LineScan if starts(&["Trigger", "Encoder", "ShaftEncoder", "LineTrigger"]) => {
            "Trigger and encoder"
        }
        CameraKind::LineScan
            if starts(&[
                "AcquisitionLineRate",
                "ResultingLineRate",
                "LineRate",
                "LinePeriod",
                "LinePitch",
            ]) =>
        {
            "Line timing"
        }
        CameraKind::LineScan
            if starts(&[
                "Width",
                "Height",
                "Offset",
                "Binning",
                "Decimation",
                "SensorWidth",
                "SensorHeight",
                "LineWidth",
                "LineHeight",
                "BlockHeight",
                "AcquisitionFrameHeight",
            ]) =>
        {
            "Line geometry"
        }
        CameraKind::Depth if starts(&["Component"]) => "Components",
        CameraKind::Depth
            if starts(&[
                "Scan3dCoordinate",
                "Scan3dDistanceUnit",
                "Scan3dAxis",
                "Scan3dTransform",
                "Scan3dInvalid",
                "CoordinateScale",
                "CoordinateOffset",
                "DepthScale",
                "DepthUnit",
                "DistanceUnit",
                "FocalLength",
                "Baseline",
                "SpatialCalibration",
            ]) =>
        {
            "Calibration"
        }
        CameraKind::Depth
            if starts(&[
                "Scan3d",
                "Depth",
                "Distance",
                "Confidence",
                "Range",
                "Disparity",
                "PointCloud",
                "Intensity",
                "DepthQuality",
            ]) =>
        {
            "Depth"
        }
        CameraKind::Microscope
            if starts(&[
                "Focus",
                "AutoFocus",
                "Autofocus",
                "Magnification",
                "Objective",
                "OpticalZoom",
                "Zoom",
                "Lens",
            ]) =>
        {
            "Optics"
        }
        CameraKind::Microscope
            if starts(&[
                "Illumination",
                "LightSource",
                "LightIntensity",
                "Lamp",
                "LED",
                "Led",
            ]) =>
        {
            "Illumination"
        }
        CameraKind::Microscope if starts(&["Stage", "ZPosition", "ZOffset"]) => "Stage",
        CameraKind::Thermal
            if starts(&[
                "Temperature",
                "Thermal",
                "Emissivity",
                "Reflected",
                "ObjectTemperature",
                "Atmospheric",
                "ExternalOptics",
                "IR",
                "Infrared",
                "Radiometric",
                "Blackbody",
                "Transmission",
            ]) =>
        {
            "Temperature"
        }
        CameraKind::DocumentScanner
            if starts(&["Scan", "Page", "Document"])
                || matches!(name, "AcquisitionStart" | "AcquisitionStop") =>
        {
            "Scan"
        }
        _ => feature_group(name),
    }
}

/// Put selectors before their dependent controls, then the settings needed
/// to start acquiring a useful image. Remaining controls keep camera order.
fn adaptive_feature_rank(name: &str, kind: CameraKind) -> usize {
    let order: &[&str] = match kind {
        CameraKind::LineScan => &[
            "Width",
            "LineWidth",
            "Height",
            "BlockHeight",
            "AcquisitionFrameHeight",
            "Offset",
            "Binning",
            "Decimation",
            "AcquisitionLineRateEnable",
            "AcquisitionLineRate",
            "LineRate",
            "LinePeriod",
            "LinePitch",
            "ResultingLineRate",
            "EncoderSelector",
            "EncoderSourceA",
            "EncoderSourceB",
            "EncoderMode",
            "EncoderDivider",
            "TriggerSelector",
            "TriggerMode",
            "TriggerSource",
            "TriggerActivation",
            "TriggerDelay",
            "TriggerSoftware",
        ],
        CameraKind::Depth => &[
            "ComponentSelector",
            "ComponentEnable",
            "Scan3dOutputMode",
            "DepthMode",
            "DepthMin",
            "DepthMax",
            "DistanceMin",
            "DistanceMax",
            "RangeMin",
            "RangeMax",
            "ConfidenceThreshold",
            "Confidence",
            "Scan3dCoordinateSelector",
            "Scan3dDistanceUnit",
            "Scan3dCoordinateUnit",
            "DepthUnit",
            "DistanceUnit",
            "Scan3dCoordinateScale",
            "Scan3dCoordinateOffset",
            "Scan3dInvalidDataFlag",
            "Scan3dInvalidDataValue",
        ],
        CameraKind::Microscope => &[
            "ObjectiveSelector",
            "Objective",
            "Magnification",
            "OpticalZoom",
            "Zoom",
            "FocusMode",
            "FocusAuto",
            "AutoFocus",
            "Autofocus",
            "FocusPosition",
            "Focus",
            "IlluminationMode",
            "IlluminationEnable",
            "IlluminationIntensity",
            "Illumination",
            "LightSource",
            "Lamp",
            "StageX",
            "StageY",
            "StageZ",
            "Stage",
        ],
        CameraKind::Thermal => &[
            "TemperatureSelector",
            "TemperatureLinearMode",
            "TemperatureUnit",
            "TemperatureRange",
            "TemperatureMin",
            "TemperatureMax",
            "Emissivity",
            "ReflectedTemperature",
            "AtmosphericTemperature",
            "AtmosphericTransmission",
        ],
        CameraKind::DocumentScanner => &[
            "ScanSource",
            "ScanResolution",
            "ScanMode",
            "Page",
            "Document",
            "AcquisitionStart",
        ],
        CameraKind::AreaScan | CameraKind::Unknown => return feature_rank(name),
    };
    order
        .iter()
        .position(|prefix| name.starts_with(prefix))
        .unwrap_or(order.len() + feature_rank(name))
}

fn adaptive_feature_label(feature: &FeatureInfo, kind: CameraKind) -> String {
    let label = match (kind, feature.name.as_str()) {
        (CameraKind::LineScan, "Width") => Some("Line width"),
        (CameraKind::LineScan, "Height" | "BlockHeight" | "AcquisitionFrameHeight") => {
            Some("Block height (lines)")
        }
        (CameraKind::LineScan, "AcquisitionLineRate" | "AcquisitionLineRateAbs" | "LineRate") => {
            Some("Line rate")
        }
        (CameraKind::LineScan, "AcquisitionLineRateEnable") => Some("Set line rate"),
        (CameraKind::LineScan, "AcquisitionFrameRate" | "AcquisitionFrameRateAbs") => {
            Some("Block rate")
        }
        (CameraKind::Depth, "ComponentSelector") => Some("Component"),
        (CameraKind::Depth, "ComponentEnable") => Some("Include component"),
        (CameraKind::Depth, "Scan3dOutputMode") => Some("Depth output"),
        (CameraKind::Depth, "Scan3dCoordinateSelector") => Some("Coordinate"),
        (
            CameraKind::Depth,
            "Scan3dDistanceUnit" | "Scan3dCoordinateUnit" | "DepthUnit" | "DistanceUnit",
        ) => Some("Distance unit"),
        (CameraKind::Depth, "Scan3dCoordinateScale") => Some("Coordinate scale"),
        (CameraKind::Depth, "Scan3dCoordinateOffset") => Some("Coordinate offset"),
        (CameraKind::Depth, "Scan3dInvalidDataFlag") => Some("Mark invalid data"),
        (CameraKind::Depth, "Scan3dInvalidDataValue") => Some("Invalid sample value"),
        (CameraKind::Depth, "ConfidenceThreshold") => Some("Minimum confidence"),
        (CameraKind::Microscope, "FocusPosition") => Some("Focus position"),
        (CameraKind::Microscope, "ObjectiveSelector") => Some("Objective"),
        (CameraKind::Microscope, "IlluminationIntensity") => Some("Illumination intensity"),
        (CameraKind::Thermal, "TemperatureLinearMode") => Some("Linear temperature output"),
        (CameraKind::Thermal, "TemperatureRange") => Some("Measurement range"),
        (CameraKind::DocumentScanner, "ScanResolution") => Some("Resolution"),
        (CameraKind::DocumentScanner, "ScanSource") => Some("Page source"),
        _ => None,
    };
    label.map(str::to_owned).unwrap_or_else(|| {
        if feature.display_name.is_empty() || feature.display_name == feature.name {
            words(&feature.name)
        } else {
            feature.display_name.clone()
        }
    })
}

fn adaptive_feature_help(name: &str, kind: CameraKind) -> Option<&'static str> {
    match (kind, name) {
        (CameraKind::LineScan, "Height" | "BlockHeight" | "AcquisitionFrameHeight") => Some(
            "Number of acquired lines assembled into each image block. This is not the sensor's height.",
        ),
        (CameraKind::LineScan, "Width") => Some("Pixels acquired across each sensor line."),
        (CameraKind::LineScan, "AcquisitionLineRate" | "AcquisitionLineRateAbs" | "LineRate") => {
            Some(
                "Lines acquired per second. Match this to object motion or use an encoder trigger.",
            )
        }
        (CameraKind::LineScan, "AcquisitionFrameRate" | "AcquisitionFrameRateAbs") => Some(
            "Image blocks acquired per second; each block contains the configured number of lines.",
        ),
        (CameraKind::Depth, "ComponentSelector") => Some(
            "Select Range, Intensity or Confidence to inspect that component's live settings. Available choices depend on the camera.",
        ),
        (CameraKind::Depth, "Scan3dCoordinateScale" | "Scan3dCoordinateOffset") => Some(
            "Use the selected coordinate's scale, offset and distance unit when interpreting raw depth samples.",
        ),
        (CameraKind::Depth, "ConfidenceThreshold") => Some(
            "Reject uncertain depth measurements. A higher threshold may leave more invalid pixels.",
        ),
        (CameraKind::Microscope, "Magnification" | "ObjectiveSelector") => Some(
            "Choose the optics before adjusting focus and illumination. Magnification alone does not calibrate image measurements.",
        ),
        (CameraKind::Thermal, "Emissivity") => Some(
            "Set the target material's emissivity for temperature measurements; reflected temperature may also affect the result.",
        ),
        _ => None,
    }
}

fn group_guidance(group: &str, kind: CameraKind) -> Option<&'static str> {
    match (kind, group) {
        (CameraKind::LineScan, "Line geometry") => {
            Some("Each block combines consecutive lines into an image.")
        }
        (CameraKind::LineScan, "Trigger and encoder") => Some(
            "Select the trigger or encoder first; its dependent controls update from the camera.",
        ),
        (CameraKind::Depth, "Components") => {
            Some("Select a component to edit its settings. Enable one component for preview.")
        }
        (CameraKind::Depth, "Calibration") => {
            Some("Units, scale and offset describe the selected depth coordinate.")
        }
        (CameraKind::Microscope, "Optics") => {
            Some("Choose an objective and magnification, then bring the specimen into focus.")
        }
        (CameraKind::Thermal, "Temperature") => {
            Some("Choose a measurement range and set emissivity for your target material.")
        }
        _ => None,
    }
}

/// Software exposure automation assumes ordinary image brightness and a
/// frame-rate budget. Specialized acquisition has its own timing or units.
fn offers_auto(profile: &CameraProfile) -> bool {
    matches!(
        profile.kind,
        CameraKind::AreaScan | CameraKind::Unknown | CameraKind::Microscope
    ) && [
        CameraCapability::Exposure,
        CameraCapability::Gain,
        CameraCapability::FrameRate,
    ]
    .into_iter()
    .any(|capability| profile.supports(capability))
}

fn camera_guidance(kind: CameraKind) -> &'static str {
    match kind {
        CameraKind::AreaScan => "Set the image size, exposure and frame rate for each full image.",
        CameraKind::LineScan => {
            "Match line rate to motion, or synchronize with an encoder. Block height sets how many lines form each image."
        }
        CameraKind::Depth => {
            "Enable one component for preview and set its output mode. Check its distance unit, coordinate scale and confidence before interpreting measurements."
        }
        CameraKind::Microscope => {
            "Choose the objective and magnification, then adjust focus and illumination for the specimen."
        }
        CameraKind::Thermal => {
            "Configure temperature output, measurement range and emissivity before taking measurements."
        }
        CameraKind::DocumentScanner => {
            "Each capture scans one page. Choose a source and resolution supported by your scanner."
        }
        CameraKind::Unknown => {
            "Use the controls advertised by this camera. Its acquisition type is not identified yet."
        }
    }
}

fn capture_title(kind: CameraKind) -> &'static str {
    match kind {
        CameraKind::LineScan => "Save line blocks",
        CameraKind::Depth => "Save depth frames",
        CameraKind::Microscope => "Save micrographs",
        CameraKind::Thermal => "Save thermal frames",
        CameraKind::DocumentScanner => "Save document scans",
        CameraKind::AreaScan | CameraKind::Unknown => "Save frames",
    }
}

fn capture_guidance(kind: CameraKind) -> Option<&'static str> {
    match kind {
        CameraKind::LineScan => {
            Some("Each saved block contains the configured number of consecutive lines.")
        }
        CameraKind::Depth => Some(
            "Use Raw pixels to preserve depth samples. Image formats produce 8-bit images. Keep the component's unit, scale and offset with your measurements.",
        ),
        CameraKind::Thermal => Some(
            "Use Raw pixels to preserve temperature samples. Image formats produce 8-bit images. Keep the camera's temperature unit and calibration with your measurements.",
        ),
        _ => None,
    }
}

fn forward_guidance(kind: CameraKind) -> Option<&'static str> {
    match kind {
        CameraKind::LineScan => Some(
            "Forwarding encodes image blocks at the chosen video frame rate. Match acquisition line timing separately in Features.",
        ),
        CameraKind::Depth | CameraKind::Thermal => Some(
            "Forwarding encodes an 8-bit image without measurement calibration or the preview color map. Use Capture with Raw pixels for quantitative data.",
        ),
        _ => None,
    }
}

/// State for the settings panel.
pub(super) struct InspectorState {
    /// The last write or command that failed on each of the shown camera's
    /// features, as the whole error; show its first line at the row. Kept by
    /// `failed()` and cleared by the feature's next success, a new draft, a
    /// refresh or another camera; hidden while another try is on its way.
    pub(super) write_errors: HashMap<String, String>,
    /// Feature writes the camera took, glowing on their fields for a moment.
    accepted: Flashes<String>,
    /// The camera whose last capture failed, and the whole error, shown
    /// under the capture button until that camera's next capture works.
    capture_failed: Option<(Option<String>, String)>,
    /// The stream-lockable features each camera could change when seen
    /// idle, by camera ID. The stream locks only these, so a source that
    /// never lets them change offers no Stop to unlock them. Only ever
    /// added to while the camera is connected: a feature list read while it
    /// streamed can outlast the stream.
    lockable: HashMap<String, HashSet<String>>,
    /// The tab thumb's place: a tab's index.
    tab_thumb: Motion,
    /// The Manual/Auto thumb's place: 1 for auto. It moves as soon as a
    /// switch is asked for, and back should the camera refuse.
    mode_thumb: Motion,
    /// Whether the thumbs have been placed; until then they jump.
    placed: bool,
    /// The camera the panel last showed.
    shown_camera: Option<String>,
    /// The camera context row lighting up as the selection moves to it.
    context: Motion,
    /// How open the storage and auto changes disclosures are.
    storage_turn: Motion,
    changes_turn: Motion,
    /// The features grouped and ordered for display.
    index: FeatureIndex,
    /// The settings each camera's forward was started with from here, as
    /// sent, until its reply; by camera ID.
    forward_sent: HashMap<String, String>,
    /// The output each camera's forward was started with from here, and a
    /// summary of its settings, while it lasts; by camera ID.
    forwarded: HashMap<String, (String, String)>,
    /// The destination the last capture went to, when one was named.
    saved_to: Option<String>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self {
            write_errors: HashMap::new(),
            accepted: Flashes::new(ACCEPTED),
            capture_failed: None,
            lockable: HashMap::new(),
            tab_thumb: Motion::new(0.0, THUMB, THUMB, Kind::Move),
            mode_thumb: Motion::new(0.0, THUMB, THUMB, Kind::Move),
            placed: false,
            shown_camera: None,
            context: Motion::new(0.0, CONTEXT, CONTEXT, Kind::Fade),
            storage_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
            changes_turn: Motion::new(0.0, motion::RING, motion::RING_OUT, Kind::Move),
            index: FeatureIndex::default(),
            forward_sent: HashMap::new(),
            forwarded: HashMap::new(),
            saved_to: None,
        }
    }
}

impl InspectorState {
    super::motion::registry! {
        motions: [tab_thumb, mode_thumb, context, storage_turn, changes_turn],
        flashes: [accepted],
    }
}

/// The features as the panel lists them: grouped, in setup order, with
/// their labels and search text. Built again when the type or text changes, so
/// the view, which runs on every camera frame, does no per-row sorting,
/// grouping or lowercasing.
#[derive(Default)]
struct FeatureIndex {
    /// The type and display metadata it was built from, hashed.
    key: u64,
    kind: CameraKind,
    /// How many features it was built from.
    len: usize,
    /// Each group with its features' indices, in display order.
    groups: Vec<(&'static str, Vec<usize>)>,
    /// Each feature's label and tooltip.
    labels: Vec<String>,
    tips: Vec<String>,
    /// What a search matches for each feature: its name, label and
    /// description, lowercased.
    words: Vec<String>,
    /// How many features are listed; AcquisitionStop folds into the Stream row.
    listed: usize,
}

impl FeatureIndex {
    fn key(features: &[FeatureInfo], kind: CameraKind) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        kind.hash(&mut hasher);
        for feature in features {
            feature.name.hash(&mut hasher);
            feature.display_name.hash(&mut hasher);
            feature.description.hash(&mut hasher);
        }
        hasher.finish()
    }

    fn new(features: &[FeatureInfo], kind: CameraKind) -> Self {
        let scanner = kind == CameraKind::DocumentScanner
            || features.iter().any(|f| f.name == "ScanResolution");
        // One Stream row starts and stops, so AcquisitionStop goes.
        let folded = features.iter().any(|f| f.name == "AcquisitionStart");
        let mut groups = Vec::new();
        let mut listed = 0;
        for &group in feature_groups(kind) {
            let mut members: Vec<usize> = (0..features.len())
                .filter(|&i| {
                    let name = features[i].name.as_str();
                    adaptive_feature_group(name, kind) == group
                        && !(folded && name == "AcquisitionStop")
                })
                .collect();
            // Stable, so ties keep the camera's order.
            members.sort_by_key(|&i| adaptive_feature_rank(&features[i].name, kind));
            listed += members.len();
            if !members.is_empty() {
                groups.push((group, members));
            }
        }
        let labels: Vec<String> = features
            .iter()
            .map(|f| {
                if folded && f.name == "AcquisitionStart" {
                    if scanner {
                        "Scan".into()
                    } else {
                        "Stream".into()
                    }
                } else {
                    adaptive_feature_label(f, kind)
                }
            })
            .collect();
        let tips = features
            .iter()
            .map(|f| {
                let mut tip = if f.description.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}\n{}", f.name, f.description)
                };
                if let Some(help) = adaptive_feature_help(&f.name, kind) {
                    tip.push('\n');
                    tip.push_str(help);
                }
                tip
            })
            .collect();
        let words = features
            .iter()
            .zip(&labels)
            .map(|(f, label)| {
                format!("{}\n{}\n{label}\n{}", f.name, f.display_name, f.description).to_lowercase()
            })
            .collect();
        Self {
            key: Self::key(features, kind),
            kind,
            len: features.len(),
            groups,
            labels,
            tips,
            words,
            listed,
        }
    }
}

/// How a text field shows where its edit stands.
#[derive(Clone, Copy)]
enum Edit {
    /// The camera refused it.
    Refused,
    /// Typed but not applied.
    Dirty,
    /// At rest, glowing by the amount while the camera's acceptance fades.
    Rest(f32),
}

/// The settings panel's hooks into the shared update cycle.
impl Workbench {
    /// Point the settings panel's motions at what they show; from
    /// `sync_animations`.
    pub(super) fn sync_inspector(&mut self) {
        let now = self.now;
        // The tick checks metadata; a new type or length cannot wait for it.
        let kind = self.camera_kind();
        if self.inspect.index.len != self.snapshot.features.len() || self.inspect.index.kind != kind
        {
            self.inspect.index = FeatureIndex::new(&self.snapshot.features, kind);
        }
        let tab = self.tab as usize as f32;
        let mode = self.mode_target();
        let moved = self.inspect.shown_camera.as_deref() != self.snapshot.active_camera.as_deref();
        let inspect = &mut self.inspect;
        if moved {
            // Light up the context row as another camera takes the panel,
            // but not for the first one.
            if inspect.shown_camera.is_some() && self.snapshot.cameras.len() > 1 {
                inspect.context.replay(1.0, 0.0, now);
            }
            inspect.shown_camera = self.snapshot.active_camera.clone();
        }
        // Another camera's mode is no change of mode, so the thumb jumps.
        if !inspect.placed || moved {
            inspect.mode_thumb.set(mode);
        } else {
            inspect.mode_thumb.go(mode, now);
        }
        if inspect.placed {
            inspect.tab_thumb.go(tab, now);
        } else {
            inspect.tab_thumb.set(tab);
            inspect.placed = true;
        }
        inspect.storage_turn.show(self.storage_open, now);
        inspect.changes_turn.show(self.auto_changes_open, now);
        // Note a forward's settings in the update that sent it, before the
        // form can change.
        let unnoted = self.pending.iter().find_map(|pending| match &pending.job {
            Job::Forward { .. } => pending
                .camera
                .clone()
                .filter(|camera| !self.inspect.forward_sent.contains_key(camera)),
            _ => None,
        });
        if let Some(camera) = unnoted {
            let summary = self.forward_summary();
            self.inspect.forward_sent.insert(camera, summary);
        }
    }

    /// The settings panel's bookkeeping on the slow tick, after the snapshot
    /// refresh; from `tick()`.
    pub(super) fn tick_inspector(&mut self) {
        let features = &self.snapshot.features;
        let kind = self.camera_kind();
        if self.inspect.index.len != features.len()
            || self.inspect.index.key != FeatureIndex::key(features, kind)
        {
            self.inspect.index = FeatureIndex::new(features, kind);
        }
        // A summary goes when its own camera stops forwarding, not when
        // another camera is shown.
        let cameras = &self.snapshot.cameras;
        self.inspect.forwarded.retain(|id, (output, _)| {
            cameras.iter().any(|camera| {
                &camera.info.id == id
                    && (camera.stale || camera.forwarding.as_deref() == Some(output.as_str()))
            })
        });
        let active = self.snapshot.active_camera.as_ref();
        let lockable = &mut self.inspect.lockable;
        lockable.retain(|id, _| {
            Some(id) == active || cameras.iter().any(|camera| &camera.info.id == id)
        });
        if let Some(id) = active.filter(|_| !self.snapshot.streaming) {
            let known = lockable.entry(id.clone()).or_default();
            for feature in features
                .iter()
                .filter(|f| f.writable && stream_locked(&f.name))
            {
                if !known.contains(&feature.name) {
                    known.insert(feature.name.clone());
                }
            }
        }
    }

    /// Whether `pending`, a write or command button press that just
    /// settled, has the last word on its feature's row: it went to the
    /// camera the panel shows, and no later one for that feature is still on
    /// its way there. Otherwise its error, value and glow would land on
    /// another camera's row, or on a newer edit.
    pub(super) fn settles_row(&self, pending: &Pending) -> bool {
        pending.camera == self.snapshot.active_camera
            && !self
                .pending
                .iter()
                .any(|other| other.job == pending.job && other.camera == pending.camera)
    }

    /// The newest write or command for feature `name` on its way to the
    /// shown camera. Runs per row on every camera frame: nothing to search
    /// while nothing is pending.
    fn asked(&self, name: &str) -> Option<&Pending> {
        if self.pending.is_empty() {
            return None;
        }
        let camera = self.snapshot.active_camera.as_deref();
        self.pending
            .iter()
            .filter(|pending| {
                pending.job.feature() == Some(name) && pending.camera.as_deref() == camera
            })
            .max_by_key(|pending| pending.at)
    }

    /// The refusal to show at feature `name`'s row: none while `asked`,
    /// another try, is on its way, as its outcome replaces it.
    fn refusal(&self, name: &str, asked: Option<&Pending>) -> Option<&String> {
        if self.inspect.write_errors.is_empty() || asked.is_some() {
            None
        } else {
            self.inspect.write_errors.get(name)
        }
    }

    /// Whether the stream locks `feature` now: a geometry or format feature
    /// of a streaming camera that could change it when idle. Sources that
    /// never let it change show it as plain read-only instead.
    fn stream_locks(&self, feature: &FeatureInfo) -> bool {
        self.snapshot.streaming
            && stream_locked(&feature.name)
            && (feature.writable
                || self
                    .snapshot
                    .active_camera
                    .as_ref()
                    .and_then(|id| self.inspect.lockable.get(id))
                    .is_some_and(|known| known.contains(&feature.name)))
    }

    /// A command finished, after the shared bookkeeping (`finished`,
    /// `failed`) and before its notice; from `settle()`.
    pub(super) fn result_inspector(
        &mut self,
        pending: &Pending,
        result: &anyhow::Result<serde_json::Value>,
    ) {
        match (&pending.job, result) {
            (Job::Capture, Ok(value)) => {
                self.inspect.saved_to = value["destination"].as_str().map(str::to_owned);
                if self
                    .inspect
                    .capture_failed
                    .as_ref()
                    .is_some_and(|(camera, _)| *camera == pending.camera)
                {
                    self.inspect.capture_failed = None;
                }
            }
            (Job::Capture, Err(error)) => {
                // The line under the button tells of the last press, so an
                // earlier file no longer reads as this one's.
                if self
                    .last_saved
                    .as_ref()
                    .is_some_and(|saved| saved.camera == pending.camera)
                {
                    self.last_saved = None;
                }
                self.inspect.capture_failed = Some((pending.camera.clone(), format!("{error:#}")));
            }
            (Job::Forward { .. }, _) => {
                let Some(camera) = &pending.camera else {
                    return;
                };
                let summary = self.inspect.forward_sent.remove(camera);
                if let (Ok(value), Some(summary)) = (result, summary)
                    && let Some(output) = value["forwarding"].as_str()
                {
                    self.inspect
                        .forwarded
                        .insert(camera.clone(), (output.to_owned(), summary));
                }
            }
            _ => {}
        }
        let Job::Set(feature) = &pending.job else {
            return;
        };
        let Ok(value) = result else {
            return;
        };
        if !self.settles_row(pending) {
            return;
        }
        // Show what the camera stored, which may be rounded or clamped,
        // unless the field was edited since this write went out.
        if let Some(stored) = value.get("value")
            && let Some(draft) = self.edits.get_mut(feature)
            && pending.value.as_ref() == Some(draft)
        {
            *draft = value_text(stored);
        }
        self.inspect.accepted.hit(feature.clone(), self.now);
    }

    /// Take a screenshot scene word for the settings panel: `late` is false
    /// while the scene is set up and true once its cameras stream. Returns
    /// whether the word was taken; see `apply_scene`. Words:
    /// `inspector-search` and `inspector-nomatch` (a search with and without
    /// matches), `inspector-storage` (storage settings open),
    /// `inspector-noforward` (no forward output yet), `inspector-widest` (the
    /// longest choices in every form, all of it open), `inspector-context`
    /// (the camera context row lighting up), `inspector-dirty` (an
    /// exposure time typed, not applied), `inspector-applying` (and being
    /// applied), `inspector-accepted` (just taken by the camera),
    /// `inspector-switching` (auto mode on its way), `inspector-connecting`
    /// (with `welcome`: a camera connecting), `inspector-deletes` (deleting
    /// the oldest files at capacity, storage closed),
    /// `inspector-capture-failed` (a capture refused) and
    /// `inspector-capturing` (a capture on its way), `inspector-nocapture`
    /// (no output name) and `inspector-noforward` (no forwarding destination).
    pub(super) fn scene_inspector(&mut self, word: &str, late: bool) -> bool {
        let camera = self.snapshot.active_camera.clone();
        let feature = "ExposureTime";
        match (word, late) {
            ("inspector-search", _) => self.search = "gain".into(),
            ("inspector-nomatch", _) => self.search = "white balance".into(),
            ("inspector-storage", _) => self.storage_open = true,
            ("inspector-noforward", _) => self.forward_output.clear(),
            ("inspector-nocapture", _) => self.output.clear(),
            ("inspector-widest", _) => {
                self.format = "pgm";
                self.forward_codec = "h265";
                self.quota_action = "delete-oldest";
                self.storage_open = true;
                self.schedule_enabled = true;
            }
            // These need streaming cameras.
            (
                "inspector-dirty"
                | "inspector-applying"
                | "inspector-accepted"
                | "inspector-switching"
                | "inspector-connecting"
                | "inspector-context"
                | "inspector-capture-failed"
                | "inspector-capturing",
                false,
            ) => return false,
            ("inspector-deletes", _) => self.quota_action = "delete-oldest",
            ("inspector-dirty", true) => {
                self.edits.insert(feature.into(), "15000".into());
            }
            ("inspector-applying", true) => {
                self.edits.insert(feature.into(), "15000".into());
                self.scene_hold(Job::Set(feature.into())).camera = camera;
            }
            ("inspector-accepted", true) => {
                self.inspect.accepted.hit(feature.into(), self.now);
            }
            ("inspector-switching", true) => {
                self.scene_hold(Job::AutoOn).camera = camera;
                // Where it slides to, for a still.
                self.inspect.mode_thumb.set(1.0);
            }
            ("inspector-connecting", true) => {
                // The welcome card it came from shows it connecting too.
                self.scene_hold(Job::Connect).target = Some("sim:0".into());
            }
            ("inspector-context", true) => self.inspect.context.replay(1.0, 0.0, self.now),
            ("inspector-capture-failed", true) => self.settle(
                &Pending {
                    camera,
                    ..Pending::unanswered(Job::Capture)
                },
                &Err(anyhow::anyhow!(
                    "save capture.png: capture output already exists: capture.png; choose a new file name"
                )),
            ),
            ("inspector-capturing", true) => self.scene_hold(Job::Capture).camera = camera,
            _ => return false,
        }
        true
    }

    /// Where the Manual/Auto thumb heads: where a switch on its way to the
    /// selected camera goes, else where the camera is.
    fn mode_target(&self) -> f32 {
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        if self.pending_for(Job::AutoOn, camera) {
            1.0
        } else if self.pending_for(Job::AutoOff, camera) {
            0.0
        } else if self.snapshot.auto.is_some() {
            1.0
        } else {
            0.0
        }
    }

    /// The forward settings as sent: "H.264 · 30 fps · 4M".
    fn forward_summary(&self) -> String {
        let mut parts = vec![
            find(&CODECS, self.forward_codec)
                .map_or(self.forward_codec, |codec| codec.label)
                .to_owned(),
            format!("{} fps", number(self.forward_fps)),
        ];
        let bitrate = self.forward_bitrate.trim();
        if !bitrate.is_empty() {
            parts.push(bitrate.to_owned());
        }
        let encoder = self.forward_encoder.trim();
        if !encoder.is_empty() && encoder != "auto" {
            parts.push(encoder.to_owned());
        }
        parts.join(" · ")
    }
}

/// A section's title, with an action such as Refresh at the right.
fn section<'a>(
    title: &'a str,
    action: Option<Element<'a, Message>>,
    p: &'static Palette,
) -> Element<'a, Message> {
    let mut head = row![
        text(title)
            .size(style::CAPTION)
            .font(style::SEMIBOLD)
            .color(p.secondary)
    ]
    .align_y(Alignment::Center);
    if let Some(action) = action {
        head = head.push(space::horizontal()).push(action);
    }
    head.into()
}

/// A form row: the label, the control in the shared column, and its unit
/// in a slot kept even when empty, so every box shares both edges.
fn form_row<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
    unit_label: &'a str,
    p: &'static Palette,
) -> Element<'a, Message> {
    row![
        container(text(label).size(style::BODY).color(p.secondary))
            .padding(iced::Padding::ZERO.top(5.0))
            .width(Fill),
        container(control).width(CONTROL),
        container(text(unit_label).size(style::SMALL).color(p.secondary))
            .padding(iced::Padding::ZERO.top(6.0))
            .width(28.0),
    ]
    .spacing(6)
    .align_y(Alignment::Start)
    .into()
}

/// What a schedule will do: "10 frames, one every 2.5 seconds".
fn schedule_text_for(kind: CameraKind, count: u32, interval: f64) -> String {
    let (singular, multiple) = match kind {
        CameraKind::LineScan => ("block", "blocks"),
        CameraKind::DocumentScanner => ("page", "pages"),
        _ => ("frame", "frames"),
    };
    if interval == 0.0 {
        return format!(
            "{}, captured consecutively",
            plural(count.into(), singular, multiple)
        );
    }
    format!(
        "{}, one every {} {}",
        plural(count.into(), singular, multiple),
        number(interval),
        if interval == 1.0 { "second" } else { "seconds" }
    )
}

/// A quiet line saying why the action above it cannot run.
fn reason<'a>(why: &'a str, p: &'static Palette) -> Element<'a, Message> {
    text(why).size(style::SMALL).color(p.secondary).into()
}

/// Field captions and the toolbar own live announcements. Keep an action's
/// explanation readable without announcing it again; quiet it during a retry.
fn error_caption<'a>(message: String, quiet: bool, p: &'static Palette) -> Element<'a, Message> {
    let ink = if quiet {
        p.secondary
    } else {
        Level::Error.color(p)
    };
    row![
        container(icon(Level::Error.icon(), 11.0, ink))
            .height(style::line_height(style::CAPTION))
            .align_y(Alignment::Center),
        text(message).size(style::CAPTION).color(ink),
    ]
    .spacing(5)
    .into()
}

/// An accent text action.
fn link<'a>(label: &'a str, on: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(style::SMALL))
        .padding([3, 8])
        .style(style::link)
        .on_press_maybe(on)
        .into()
}

/// A full-width action: `glyph` and `label` centered on a button styled
/// `base`. While `busy` it ignores presses but keeps its look.
fn wide_button<'a>(
    glyph: Element<'a, Message>,
    label: &'a str,
    base: fn(&Theme, button::Status) -> button::Style,
    on: Option<Message>,
    busy: bool,
) -> button::Button<'a, Message> {
    button(
        center(
            row![glyph, text(label).size(style::BODY).font(style::MEDIUM)]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .height(Length::Shrink),
    )
    .width(Fill)
    .padding([9, 14])
    .style(move |theme, status| base(theme, if busy { button::Status::Active } else { status }))
    .on_press_maybe(on.filter(|_| !busy))
}

impl Workbench {
    pub(super) fn inspector(&self, p: &'static Palette) -> Element<'_, Message> {
        let os = Os::CURRENT;
        let tabs = inspector_tabs(
            vec![
                (
                    "Features",
                    Some(Message::Tab(Tab::Features)),
                    Action::FeaturesTab.hint("Features panel", os),
                ),
                (
                    "Capture",
                    Some(Message::Tab(Tab::Capture)),
                    Action::CaptureTab.hint("Capture panel", os),
                ),
                (
                    "Forward",
                    Some(Message::Tab(Tab::Forward)),
                    Action::ForwardTab.hint("Forward panel", os),
                ),
            ],
            self.tab as usize,
            self.inspect.tab_thumb.get(self.now),
        );
        let mut panel = column![
            self.titlebar(
                container(tabs)
                    .padding(iced::Padding {
                        left: 18.0,
                        right: 20.0,
                        ..iced::Padding::ZERO
                    })
                    .into()
            )
        ];
        if let Some(context) = self.camera_context(p) {
            panel = panel.push(context);
        }
        if let Some(status) = self
            .snapshot
            .connected
            .as_ref()
            .and_then(|camera| self.network_status(camera))
        {
            panel = panel.push(container(self.network_indicator(status, p)).padding([4, 14]));
        }
        let body: Element<'_, Message> =
            if self.tab == Tab::Features && self.snapshot.connected.is_none() {
                self.no_camera(p)
            } else {
                let content = match self.tab {
                    Tab::Features => self.features(p),
                    Tab::Capture => self.capture_settings(p),
                    Tab::Forward => self.forward_settings(p),
                };
                let mut scroll: Option<Element<'_, Message>> = Some(
                    scrollable(container(content).padding(iced::Padding {
                        top: 4.0,
                        right: 20.0,
                        bottom: 24.0,
                        left: 18.0,
                    }))
                    .id(SCROLL)
                    .height(Fill)
                    .style(style::scroll)
                    .into(),
                );
                // Each tab scrolls in a slot of its own, so changing tabs,
                // however it happens, starts the new one at its top.
                let mut slots = Row::new().height(Fill);
                for tab in [Tab::Features, Tab::Capture, Tab::Forward] {
                    slots = slots.push(match scroll.take_if(|_| tab == self.tab) {
                        Some(scroll) => scroll,
                        None => space().into(),
                    });
                }
                slots.into()
            };
        container(panel.push(space().height(8)).push(body))
            .width(INSPECTOR)
            .height(Fill)
            .style(style::base)
            .into()
    }

    /// With several cameras, which one the panel edits: pinned under the
    /// tabs, lighting up as the selection moves.
    fn camera_context(&self, p: &'static Palette) -> Option<Element<'_, Message>> {
        if self.snapshot.cameras.len() < 2 {
            return None;
        }
        let camera = self.snapshot.connected.as_ref()?;
        let glow = self.inspect.context.get(self.now);
        // The same state dot as the camera's row in the camera list, and
        // like it, ahead of the name.
        let state = self
            .snapshot
            .cameras
            .iter()
            .find(|state| state.info.id == camera.id)
            .map_or(CameraState::Idle, |state| self.camera_state(state));
        let line = row![
            container(status_dot(state, 6.0, p)).center_x(13),
            tip(
                one_line(camera.model.as_str(), style::SMALL, style::MEDIUM, p.text).yield_room(),
                camera.model.as_str(),
            ),
            tip(
                one_line(identity(camera), style::SMALL, style::SANS, p.secondary).width(Fill),
                identity(camera),
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        Some(
            container(
                container(line)
                    .padding([6, 10])
                    .width(Fill)
                    .style(move |_| container::Style {
                        background: Some(fade(p.accent_soft, glow).into()),
                        border: iced::border::rounded(style::RADIUS),
                        ..container::Style::default()
                    }),
            )
            .padding(iced::Padding {
                top: 6.0,
                right: 10.0,
                bottom: 0.0,
                left: 8.0,
            })
            .into(),
        )
    }

    /// The Features tab with no camera: what will show here, and what can
    /// be set up meanwhile.
    fn no_camera(&self, p: &'static Palette) -> Element<'_, Message> {
        let connecting = self.pending(Job::Connect);
        let content = if connecting {
            column![
                icon::spinner(22.0, p.secondary, self.spin()),
                text("Reading camera features…")
                    .size(style::BODY)
                    .color(p.secondary),
            ]
        } else {
            column![
                icon(Icon::Sliders, 28.0, p.tertiary),
                text("No camera connected")
                    .size(style::HEADING)
                    .font(style::SEMIBOLD),
                text("Features appear here once a camera is connected.")
                    .size(style::SMALL)
                    .color(p.secondary)
                    .align_x(Alignment::Center),
                row![
                    link("Set up capture", Some(Message::Tab(Tab::Capture))),
                    link("Set up forwarding", Some(Message::Tab(Tab::Forward))),
                ]
                .spacing(4),
            ]
        };
        center(
            content
                .spacing(10)
                .align_x(Alignment::Center)
                .max_width(240),
        )
        .padding(iced::Padding {
            bottom: 80.0,
            ..iced::Padding::ZERO
        })
        .into()
    }

    pub(super) fn features(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let index = &self.inspect.index;
        let kind = self.camera_kind();
        let query = self.search.trim().to_lowercase();
        let mut groups = column![].spacing(SECTION_GAP);
        let mut matched = 0;
        for (group, members) in &index.groups {
            let mut rows = column![].spacing(ROW_GAP);
            let mut shown = false;
            let mut locked = false;
            for &i in members {
                let Some(feature) = snapshot.features.get(i) else {
                    continue;
                };
                if !query.is_empty() && !index.words[i].contains(&query) {
                    continue;
                }
                shown = true;
                matched += 1;
                locked |= self.stream_locks(feature);
                rows = rows.push(self.feature(feature, &index.labels[i], &index.tips[i], p));
            }
            if shown {
                let banner = locked.then(|| self.locked_banner(p));
                let mut content = column![section(group, banner, p)].spacing(10);
                if let Some(guidance) = group_guidance(group, kind) {
                    content = content.push(reason(guidance, p));
                }
                groups = groups.push(content.push(rows));
            }
        }
        let refreshing = self.pending(Job::RefreshFeatures);
        let total = index.listed;
        let count = if query.is_empty() {
            plural(total as u64, "feature", "features")
        } else {
            format!(
                "{} of {} features",
                grouped(matched as u64),
                grouped(total as u64)
            )
        };
        let count = row![
            text(count).size(style::SMALL).color(p.secondary),
            space::horizontal(),
            if refreshing {
                Element::from(
                    container(text("Refreshing…").size(style::SMALL).color(p.secondary))
                        .padding([3, 8]),
                )
            } else {
                link("Refresh", Some(Message::RefreshFeatures))
            },
        ]
        .align_y(Alignment::Center);
        let list: Element<'_, Message> = if matched == 0 && !query.is_empty() {
            column![
                text(format!("No features match “{}”", self.search.trim()))
                    .size(style::BODY)
                    .color(p.secondary)
                    .align_x(Alignment::Center),
                link("Clear search", Some(Message::Search(String::new()))),
            ]
            .spacing(4)
            .width(Fill)
            .align_x(Alignment::Center)
            .into()
        } else {
            groups.into()
        };
        let mut content = column![].spacing(SECTION_GAP);
        if let Some(profile) = snapshot.connected.as_ref().map(|camera| &camera.profile) {
            content = content.push(self.camera_type_card(profile, p));
        }
        if snapshot.auto.is_some()
            || snapshot
                .connected
                .as_ref()
                .is_some_and(|camera| offers_auto(&camera.profile))
        {
            content = content.push(self.auto_card(p));
        }
        content
            .push(column![self.search_field(p), count].spacing(6))
            .push(list)
            .into()
    }

    /// Camera type and the acquisition model it implies. Detection evidence
    /// is available on the title without taking room from everyday controls.
    fn camera_type_card<'a>(
        &'a self,
        profile: &'a CameraProfile,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let about = if profile.evidence.is_empty() {
            if profile.kind == CameraKind::Unknown {
                "This camera has not advertised a recognized acquisition type.".into()
            } else {
                "Camera type detected automatically.".into()
            }
        } else {
            format!(
                "Camera type detected automatically.\n{}",
                profile.evidence.join("\n")
            )
        };
        let head = row![
            tip(
                text("Camera type").size(style::SMALL).color(p.secondary),
                about
            ),
            space::horizontal(),
            text(if profile.kind == CameraKind::Unknown {
                "Unidentified".into()
            } else {
                profile.kind.to_string()
            })
            .size(style::BODY)
            .font(style::SEMIBOLD),
        ]
        .align_y(Alignment::Center);
        container(column![head, reason(camera_guidance(profile.kind), p)].spacing(8))
            .padding(14)
            .width(Fill)
            .style(style::well)
            .into()
    }

    /// The feature search, with a button that clears it once it holds text.
    fn search_field(&self, p: &'static Palette) -> Element<'_, Message> {
        let input = text_input("Search features", &self.search)
            .id("feature-search")
            .icon(icon::input_icon(Icon::Search))
            .on_input(Message::Search)
            .size(style::BODY)
            // Room for the clear button, held even without it so the text
            // never reflows.
            .padding(iced::Padding {
                top: 7.0,
                right: 30.0,
                bottom: 7.0,
                left: 10.0,
            })
            .style(style::input);
        // Keep the input at the same place in the widget tree when the clear
        // action appears, preserving its focus, cursor and native identity.
        let mut field = stack![input];
        if !self.search.is_empty() {
            field = field.push(
                container(tip(
                    button(icon(Icon::Close, 11.0, p.secondary))
                        .padding(5)
                        .style(style::plain)
                        .on_press(Message::Search(String::new())),
                    "Clear search",
                ))
                .align_right(Fill)
                .center_y(Fill)
                .padding(iced::Padding {
                    right: 5.0,
                    ..iced::Padding::ZERO
                }),
            );
        }
        field.into()
    }

    /// Said once per group that holds features the stream locks.
    fn locked_banner(&self, p: &'static Palette) -> Element<'_, Message> {
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        let stopping = self.stream_pending(camera) == Some(true);
        row![
            icon(Icon::Lock, 11.0, p.secondary),
            text("Locked while streaming")
                .size(style::CAPTION)
                .color(p.secondary),
            tip(
                button(text("Stop").size(style::CAPTION))
                    .padding([1, 6])
                    .style(style::link)
                    .on_press_maybe((!stopping).then_some(Message::ToggleStream)),
                Action::ToggleStream.hint("Stop the stream", Os::CURRENT),
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .into()
    }

    pub(super) fn auto_card(&self, p: &'static Palette) -> Element<'_, Message> {
        let auto = self.snapshot.auto.as_ref();
        let busy = self.auto_busy_here();
        let os = Os::CURRENT;
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        let switching =
            self.pending_for(Job::AutoOn, camera) || self.pending_for(Job::AutoOff, camera);
        let mut head = row![
            text("Exposure").size(style::BODY).font(style::SEMIBOLD),
            space::horizontal(),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        if switching {
            head = head
                .push(icon::spinner(10.0, p.secondary, self.spin()))
                .push(
                    text("Switching…")
                        .size(style::CAPTION)
                        .font(style::MEDIUM)
                        .color(p.secondary),
                );
        } else if let Some(status) = auto {
            head = head.push(
                text(capitalize(&status.state))
                    .size(style::CAPTION)
                    .font(style::MEDIUM)
                    .color(match status.state.as_str() {
                        "stable" => p.ink(p.live),
                        "limited" => p.ink(p.warn),
                        _ => p.secondary,
                    }),
            );
        }
        let modes = segmented(
            "Exposure mode",
            vec![
                (
                    "Manual",
                    (!busy).then_some(Message::Auto(false)),
                    Action::ToggleAuto.hint("Set exposure, gain and frame rate yourself", os),
                ),
                (
                    "Auto",
                    (!busy).then_some(Message::Auto(true)),
                    Action::ToggleAuto.hint("Tune exposure, gain and frame rate automatically", os),
                ),
            ],
            usize::from(auto.is_some()),
            self.inspect.mode_thumb.get(self.now),
        );
        let mut card = column![head, modes].spacing(10);
        match auto {
            None => {
                card = card.push(
                    text("Auto tunes exposure, gain and frame rate for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                );
            }
            Some(status) => {
                card = card
                    .push(
                        column![
                            balance_slider(self.balance),
                            row![
                                text("Quality").size(style::CAPTION).color(p.secondary),
                                space::horizontal(),
                                text("Frame rate").size(style::CAPTION).color(p.secondary),
                            ],
                        ]
                        .spacing(4),
                    )
                    .push(
                        text(auto_summary(status))
                            .size(style::SMALL)
                            .color(p.secondary),
                    );
                for note in &status.notes {
                    card = card.push(text(note.clone()).size(style::SMALL).color(p.ink(p.warn)));
                }
                if !status.changes.is_empty() {
                    card = card.push(disclosure(
                        format!("Auto changes ({})", status.changes.len()),
                        None,
                        self.inspect.changes_turn.get(self.now),
                        Message::ToggleAutoChanges,
                        p,
                    ));
                    if self.auto_changes_open {
                        let mut changes = column![].spacing(3);
                        for change in status.changes.iter().rev() {
                            let unit = self
                                .snapshot
                                .features
                                .iter()
                                .find(|f| f.name == change.feature)
                                .and_then(|f| f.unit.as_deref());
                            changes = changes.push(tip(
                                text(change_text(change, unit))
                                    .size(style::CAPTION)
                                    .font(style::MONO)
                                    .color(p.secondary),
                                change.reason.clone(),
                            ));
                        }
                        card = card.push(
                            scrollable(changes)
                                .height(Length::Shrink)
                                .style(style::scroll),
                        );
                    }
                }
            }
        }
        container(card)
            .padding(14)
            .width(Fill)
            .style(style::well)
            .into()
    }

    /// One feature: its label and control, and a caption under them with its
    /// range, where an edit stands, or why the camera refused it. Runs per
    /// row on every camera frame, so each check here is constant time and
    /// skips at once while nothing is pending, refused or glowing.
    pub(super) fn feature<'a>(
        &'a self,
        feature: &'a FeatureInfo,
        label: &'a str,
        about: &'a str,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let name = feature.name.as_str();
        let streaming = self.snapshot.streaming;
        let managed = self
            .snapshot
            .auto
            .as_ref()
            .is_some_and(|auto| auto.managed.iter().any(|m| m == name));
        let locked = self.stream_locks(feature);
        let writable = (feature.writable || managed) && !locked;
        let kind = feature.kind.as_str();
        let command = kind.eq_ignore_ascii_case("command");
        // A write or command for this row on its way to the shown camera:
        // its control shows what it asked for, and an earlier refusal waits
        // for its outcome.
        let asked = self.asked(name);
        let draft_error = self
            .edits
            .get(name)
            .filter(|_| writable && !command)
            .and_then(|value| feature.validate_input(value).err())
            .map(|error| format!("{error:#}"));
        let refused = draft_error.as_ref().or_else(|| self.refusal(name, asked));
        let current = feature_value(feature);
        // The caption's left side, and whether this row holds a text field,
        // which keeps its caption line so nothing moves as an edit begins.
        let mut notes: Vec<Element<'a, Message>> = Vec::new();
        let note = |note: String, color: Color| -> Element<'a, Message> {
            text(note).size(style::CAPTION).color(color).into()
        };
        let mut trailing = None;
        let read_only = !writable && !command && !locked;
        let scan = command && name == "AcquisitionStart" && self.scanner_selected();
        let control: Element<'_, Message> = if scan {
            button(text("Scan & save").size(style::SMALL))
                .padding([4, 10])
                .style(style::secondary)
                .on_press_maybe(
                    (!self.capturing_here()
                        && self.validate_input(&self.capture_command(false)).is_ok())
                    .then_some(Message::Capture),
                )
                .into()
        } else if command && name == "AcquisitionStart" {
            self.stream_toggle(p)
        } else if let Some(error) = feature.error.as_ref().filter(|_| !command) {
            notes.push(note(error.clone(), p.ink(p.warn)));
            space().into()
        } else if command {
            let (action, enabled) = match name {
                "AcquisitionStart" => ("Start stream", !streaming),
                "AcquisitionStop" => ("Stop stream", streaming),
                _ => ("Execute", true),
            };
            let running = asked.is_some_and(|pending| matches!(pending.job, Job::Execute(_)));
            let mut label = row![].spacing(6).align_y(Alignment::Center);
            if running {
                label = label.push(icon::spinner(11.0, p.secondary, self.spin()));
            }
            button(label.push(text(action).size(style::SMALL)))
                .padding([4, 10])
                .style(move |theme, status| {
                    style::secondary(
                        theme,
                        if running {
                            button::Status::Active
                        } else {
                            status
                        },
                    )
                })
                .on_press_maybe(
                    (feature.writable && enabled && !running)
                        .then(|| Message::Execute(feature.name.clone())),
                )
                .into()
        } else if !writable {
            let value = match feature.unit.as_deref() {
                Some(unit) => format!("{current} {}", unit_label(unit)),
                None => current.clone(),
            };
            let mut shown = row![].spacing(6).align_y(Alignment::Center);
            if locked {
                shown = shown.push(tip(
                    icon(Icon::Lock, 11.0, p.tertiary),
                    format!("Stop the stream to change {label}"),
                ));
            }
            // In line with the digits of the fields above and below.
            container(shown.push(text(value).size(style::SMALL).color(p.secondary)))
                .padding(iced::Padding {
                    right: 8.0,
                    ..iced::Padding::ZERO
                })
                .into()
        } else if kind.eq_ignore_ascii_case("boolean") || kind.eq_ignore_ascii_case("bool") {
            // What a write on its way asked for, else what the camera has.
            let checked = match asked.and_then(|pending| pending.value.as_deref()) {
                Some(value) => value == "true" || value == "1",
                None => feature
                    .value
                    .as_ref()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(current == "true" || current == "1"),
            };
            widgets::checkbox_named("", label, checked, move |value| {
                Message::Set(feature.name.clone(), value.to_string())
            })
        } else if !feature.choices.is_empty() {
            let glow = self.inspect.accepted.level(name, self.now);
            // What a write on its way asked for, else what the camera has.
            let shown = asked
                .and_then(|pending| pending.value.as_deref())
                .unwrap_or(&current);
            pick_list(
                &feature.choices[..],
                feature.choices.iter().find(|c| *c == shown).cloned(),
                move |value| Message::Set(feature.name.clone(), value),
            )
            .name(label)
            .placeholder(current.clone())
            .width(CONTROL)
            .text_size(style::SMALL)
            .padding([5, 8])
            .style(style::pick_flash(glow))
            .menu_style(style::menu)
            .into()
        } else {
            let draft = self.edits.get(name).unwrap_or(&current);
            let dirty = *draft != current;
            let applying = asked.is_some_and(|pending| matches!(pending.job, Job::Set(_)));
            let edit = if refused.is_some() {
                Edit::Refused
            } else if dirty {
                Edit::Dirty
            } else {
                Edit::Rest(self.inspect.accepted.level(name, self.now))
            };
            trailing = Some(if applying {
                text("Applying…")
                    .size(style::CAPTION)
                    .color(p.secondary)
                    .into()
            } else if dirty {
                tip(
                    button(text("Apply").size(style::CAPTION))
                        .padding([0, 4])
                        .style(style::link)
                        .on_press_maybe(
                            draft_error
                                .is_none()
                                .then(|| Message::Commit(feature.name.clone())),
                        ),
                    Action::FocusCamera.hint("Apply", Os::CURRENT),
                )
            } else {
                // One caption line, as tall as Apply, so a field with no
                // range keeps its line too.
                Element::from(space().height(style::line_height(style::CAPTION)))
            });
            let feature_name = feature.name.clone();
            text_input(&current, draft)
                .name(label)
                .description(about)
                .invalid(refused.cloned())
                .id(format!("feature:{}", feature.name))
                .on_input(move |value| Message::Draft(feature_name.clone(), value))
                .on_submit(Message::Commit(feature.name.clone()))
                .size(style::SMALL)
                .padding([5, 8])
                .width(CONTROL)
                .align_x(Alignment::End)
                .style(move |theme, status| match edit {
                    Edit::Refused => style::input_invalid(theme, status),
                    Edit::Dirty => style::input_dirty(theme, status),
                    Edit::Rest(glow) => style::input_flash(glow)(theme, status),
                })
                .into()
        };
        if managed && !locked {
            notes.push(tip(
                note("Set by auto".into(), p.accent_text),
                "Editing it switches the camera to manual",
            ));
        }
        if writable && !command {
            let range = range_text(feature);
            if !range.is_empty() {
                notes.push(note(range, p.secondary));
            }
        }
        let label = text(label).size(style::BODY);
        let label = if read_only {
            tip(label, format!("{about}\nRead only"))
        } else {
            tip(label, about)
        };
        let head = row![container(label).width(Fill), control,]
            .spacing(10)
            .align_y(Alignment::Center);
        // A refusal takes the notes' place; Apply stays beside it.
        let caption: Option<Element<'_, Message>> =
            if refused.is_some() || !notes.is_empty() || trailing.is_some() {
                let line: Element<'_, Message> = match refused {
                    Some(error) => field_error(capitalize(first_line(error)), p),
                    None => Row::with_children(notes).spacing(8).wrap().into(),
                };
                let mut caption = row![container(line).width(Fill)].align_y(Alignment::Start);
                if let Some(trailing) = trailing {
                    caption = caption.push(trailing);
                }
                Some(caption.into())
            } else {
                None
            };
        match caption {
            Some(caption) => column![head, caption].spacing(2).into(),
            None => head.into(),
        }
    }

    /// The Stream row: starts or stops the selected camera, with a spinner
    /// while that is on its way.
    fn stream_toggle(&self, p: &'static Palette) -> Element<'_, Message> {
        let camera = self.snapshot.active_camera.as_deref().unwrap_or_default();
        let pending = self.stream_pending(camera);
        let (starting, stopping) = (pending == Some(false), pending == Some(true));
        let busy = pending.is_some();
        let streaming = self.snapshot.streaming;
        let glyph = if busy {
            icon::spinner(11.0, p.secondary, self.spin())
        } else if streaming {
            icon(Icon::Stop, 11.0, p.text)
        } else {
            icon(Icon::Play, 11.0, p.accent_text)
        };
        let label = match (starting, stopping, streaming) {
            (true, _, _) => "Starting…",
            (_, true, _) => "Stopping…",
            (_, _, true) => "Stop",
            _ => "Start",
        };
        tip(
            button(
                row![glyph, text(label).size(style::SMALL)]
                    .spacing(6)
                    .align_y(Alignment::Center),
            )
            .padding([4, 10])
            .style(move |theme, status| {
                style::secondary(theme, if busy { button::Status::Active } else { status })
            })
            .on_press_maybe((!busy).then_some(Message::ToggleStream)),
            Action::ToggleStream.hint(
                if streaming {
                    "Stop the stream"
                } else {
                    "Start the stream"
                },
                Os::CURRENT,
            ),
        )
    }

    pub(super) fn num(&self, key: Num) -> Element<'_, Message> {
        let value = self
            .drafts
            .get(&key)
            .cloned()
            .unwrap_or_else(|| self.num_text(key));
        let valid = num_valid(key, &value);
        let input = text_input("", &value)
            .name(match key {
                Num::Count => "Capture count",
                Num::Timeout => "Capture timeout",
                Num::Delay => "Capture delay",
                Num::Interval => "Capture interval",
                Num::Fps => "Recording frame rate",
                Num::FileMib => "Maximum file size",
                Num::QuotaGib => "Maximum storage size",
                Num::QuotaFiles => "Maximum managed file count",
                Num::RetentionDays => "Maximum file age",
            })
            .invalid((!valid).then(|| num_requirement(key).to_owned()))
            .id(format!("number:{key:?}"))
            .on_input(move |value| Message::Num(key, value))
            .size(style::BODY)
            .padding([5, 8])
            .width(Fill)
            .align_x(Alignment::End)
            .style(if valid {
                style::input
            } else {
                style::input_invalid
            });
        let mut content = column![input].spacing(4);
        if !valid {
            content = content.push(field_error(
                num_requirement(key),
                self.window_palette(self.main_window),
            ));
        }
        content.into()
    }

    pub(super) fn capture_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let connected = self.snapshot.connected.is_some();
        let scanner = self.scanner_selected();
        let kind = self.camera_kind();
        let capturing = self.capturing_here();
        let saved = self.just_saved(self.snapshot.active_camera.as_deref());
        let os = Os::CURRENT;
        let problem = self.validate_input(&self.capture_command(false)).err();
        let ready = connected && problem.is_none();
        // Busy actions keep their blue fill; unavailable actions use a
        // neutral fill and need the same readable ink as their labels.
        let action_ink = if ready || capturing {
            Color::WHITE
        } else {
            p.secondary
        };
        let (glyph, label) = if capturing {
            (
                icon::spinner(14.0, action_ink, self.spin()),
                if scanner { "Scanning…" } else { "Saving…" },
            )
        } else if saved {
            (icon(Icon::Check, 14.0, action_ink), "Saved")
        } else {
            (
                icon(
                    if scanner { Icon::Scan } else { Icon::Camera },
                    14.0,
                    action_ink,
                ),
                if scanner {
                    "Scan & save"
                } else {
                    "Capture & save"
                },
            )
        };
        let mut action = column![tip(
            wide_button(
                glyph,
                label,
                style::primary,
                ready.then_some(Message::Capture),
                capturing
            ),
            Action::Capture.hint("Capture and save", os),
        )]
        .spacing(8);
        // Kept while the next capture saves, so nothing below it moves.
        if let Some(status) = self.capture_status(connected, capturing, p) {
            action = action.push(status);
        }
        let mut head = column![section(capture_title(kind), None, p)].spacing(8);
        if let Some(guidance) = capture_guidance(kind) {
            head = head.push(reason(guidance, p));
        }
        let mut save = column![
            head,
            accessibility::mapped(
                self.capture_to.view(
                    &self.output,
                    "Output path",
                    p,
                    crate::destination::validate_output(
                        self.capture_to.selected.as_deref(),
                        &self.output
                    )
                    .and_then(|_| crate::engine::validate_capture_output(&self.output))
                    .err()
                    .map(|error| error.to_string()),
                ),
                Message::CapturePicker,
            ),
            form_row(
                match kind {
                    CameraKind::DocumentScanner => "Pages",
                    CameraKind::LineScan => "Blocks",
                    _ => "Frames",
                },
                self.num(Num::Count),
                "",
                p
            ),
            form_row(
                "Format",
                pick_list(&FORMATS[..], find(&FORMATS, self.format), Message::Format)
                    .name("Capture format")
                    .width(Fill)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu),
                "",
                p,
            ),
            form_row("Timeout", self.num(Num::Timeout), "ms", p),
            action,
            checkbox(
                "Capture on a schedule",
                self.schedule_enabled,
                Message::ScheduleEnabled,
            ),
        ]
        .spacing(ROW_GAP);
        if self.schedule_enabled {
            let problem = self.validate_input(&self.capture_command(true)).err();
            save = save
                .push(form_row("Start after", self.num(Num::Delay), "s", p))
                .push(form_row(
                    match kind {
                        CameraKind::DocumentScanner => "Page interval",
                        CameraKind::LineScan => "Block interval",
                        _ => "Frame interval",
                    },
                    self.num(Num::Interval),
                    "s",
                    p,
                ))
                .push(
                    text(schedule_text_for(
                        kind,
                        self.count,
                        self.schedule_interval_seconds,
                    ))
                    .size(style::SMALL)
                    .color(p.secondary),
                )
                .push(
                    button(
                        center(text("Schedule capture").size(style::BODY)).height(Length::Shrink),
                    )
                    .width(Fill)
                    .padding([8, 14])
                    .style(style::secondary)
                    .on_press_maybe((connected && problem.is_none()).then_some(Message::Schedule)),
                );
            if let Some(problem) = problem {
                save = save.push(error_caption(problem.to_string(), false, p));
            }
        }
        column![
            self.auto_capture_settings(p),
            save,
            self.storage_settings(p),
            self.jobs(p),
            column![
                section("Automation", None, p),
                text(
                    "Control this camera from a shell or coding agent while this window keeps it."
                )
                .size(style::SMALL)
                .color(p.secondary),
                self.copyable(self.session_command(), p),
            ]
            .spacing(ROW_GAP),
        ]
        .spacing(SECTION_GAP)
        .into()
    }

    /// Under the capture button: why it cannot capture, or else how this
    /// camera's last capture went: why it failed, or the file it saved, with
    /// a button that copies its path. Quiet while the next one saves.
    fn capture_status(
        &self,
        connected: bool,
        capturing: bool,
        p: &'static Palette,
    ) -> Option<Element<'_, Message>> {
        if !connected {
            return Some(reason("Connect a camera to capture.", p));
        }
        if let Err(error) = self.validate_input(&self.capture_command(false)) {
            return Some(error_caption(error.to_string(), false, p));
        }
        let here =
            |camera: &Option<String>| camera.is_none() || *camera == self.snapshot.active_camera;
        if let Some((_, error)) = self
            .inspect
            .capture_failed
            .as_ref()
            .filter(|(camera, _)| here(camera))
        {
            let first = capitalize(first_line(error));
            let line = error_caption(first, capturing, p);
            return Some(tip(container(line).width(Fill), error.clone()));
        }
        let saved = self
            .last_saved
            .as_ref()
            .filter(|saved| here(&saved.camera))?;
        let path = std::path::Path::new(&saved.path);
        let name = |path: &std::path::Path| {
            path.file_name()
                .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
                .into_owned()
        };
        let folder = path
            .parent()
            .filter(|folder| !folder.as_os_str().is_empty());
        let (mut summary, copy) = match folder {
            Some(folder) if saved.count > 1 => (
                format!("{} files in {}", grouped(saved.count as u64), name(folder)),
                folder.to_string_lossy().into_owned(),
            ),
            _ if saved.count > 1 => (
                format!("{} files", grouped(saved.count as u64)),
                saved.path.clone(),
            ),
            _ => (name(path), saved.path.clone()),
        };
        if let Some(destination) = &self.inspect.saved_to {
            summary = format!("{summary} · {destination}");
        }
        let copied = self.just_copied(&copy);
        Some(
            row![
                icon(
                    Icon::Check,
                    12.0,
                    if capturing {
                        p.secondary
                    } else {
                        p.ink(p.live)
                    }
                ),
                tip(
                    one_line(summary, style::SMALL, style::SANS, p.secondary).width(Fill),
                    copy.clone(),
                ),
                tip(
                    button(icon(
                        if copied { Icon::Check } else { Icon::Copy },
                        12.0,
                        if copied { p.ink(p.live) } else { p.secondary },
                    ))
                    .padding(4)
                    .style(style::plain)
                    .on_press(Message::Copy(copy)),
                    if copied { "Copied" } else { "Copy path" },
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        )
    }

    pub(super) fn forward_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        if self.scanner_selected() {
            return column![
                section("Document scanner", None, p),
                text("Use Capture to scan and save pages.")
                    .size(style::BODY)
                    .color(p.secondary),
                link("Open Capture", Some(Message::Tab(Tab::Capture))),
            ]
            .spacing(ROW_GAP)
            .into();
        }
        let first = match &self.snapshot.forwarding {
            Some(destination) => self.forward_card(destination, p),
            None => self.forward_form(p),
        };
        let mut content = column![].spacing(SECTION_GAP);
        if let Some(guidance) = forward_guidance(self.camera_kind()) {
            content = content.push(reason(guidance, p));
        }
        content.push(first)
            .push(
            self.storage_settings(p),
            )
            .push(
            column![
                section("Encoder availability", None, p),
                text("Auto picks an available hardware or software encoder. To list what this computer has, run:")
                    .size(style::SMALL)
                    .color(p.secondary),
                self.copyable("capturefab doctor".into(), p),
            ]
            .spacing(ROW_GAP),
            )
        .into()
    }

    fn forward_form(&self, p: &'static Palette) -> Element<'_, Message> {
        let connected = self.snapshot.connected.is_some();
        let recording = Output::of(&self.forward_output).recording();
        let starting = self.forward_pending() == Some(false);
        let problem = self.validate_input(&self.forward_command()).err();
        let ready = connected && problem.is_none();
        let action_ink = if ready || starting {
            Color::WHITE
        } else {
            p.secondary
        };
        let glyph = if starting {
            icon::spinner(15.0, action_ink, self.spin())
        } else {
            icon(Icon::Broadcast, 15.0, action_ink)
        };
        let mut action = column![wide_button(
            glyph,
            if starting {
                "Starting…"
            } else if recording {
                "Start recording"
            } else {
                "Start forwarding"
            },
            style::primary,
            ready.then_some(Message::StartForward),
            starting,
        )]
        .spacing(8);
        if !connected {
            action = action.push(reason(
                if recording {
                    "Connect a camera to record."
                } else {
                    "Connect a camera to forward it."
                },
                p,
            ));
        } else if let Some(problem) = problem {
            action = action.push(error_caption(problem.to_string(), false, p));
        }
        column![
            section(if recording { "Record to a file" } else { "Forward to a recorder" }, None, p),
            text(if recording { "Save this camera's video to a recording file." } else { "Publish this camera to MediaMTX or an NVR." })
                .size(style::SMALL)
                .color(p.secondary),
            accessibility::mapped(
                self.record_to.view(&self.forward_output, "Stream URL or file", p,
                    crate::destination::validate_output(self.record_to.selected.as_deref(), &self.forward_output)
                        .and_then(|_| crate::media::validate_forward_output(&self.forward_output))
                        .err().map(|error| error.to_string())),
                Message::RecordPicker,
            ),
            form_row(
                "Codec",
                pick_list(
                    &CODECS[..],
                    find(&CODECS, self.forward_codec),
                    Message::Codec
                )
                .name("Video codec")
                .width(Fill)
                .text_size(style::BODY)
                .padding([5, 8])
                .style(style::pick)
                .menu_style(style::menu),
                "",
                p,
            ),
            form_row(
                "Encoder",
                column![tip(
                    text_input("auto", &self.forward_encoder)
                        .name("Recording encoder")
                        .invalid(crate::media::validate_encoder(self.forward_codec, self.forward_encoder.trim()).err().map(|error| error.to_string()))
                        .id("forward-encoder")
                        .on_input(Message::Encoder)
                        .size(style::BODY)
                        .padding([5, 8])
                        .width(Fill)
                        .style(if crate::media::validate_encoder(self.forward_codec, self.forward_encoder.trim()).is_ok() { style::input } else { style::input_invalid }),
                    "auto picks an available encoder. Enter an FFmpeg encoder name to choose one.",
                ),
                self.forward_field_error(true, p)].spacing(4),
                "",
                p,
            ),
            form_row("Frame rate", self.num(Num::Fps), "fps", p),
            form_row(
                "Bitrate",
                column![text_input("4M", &self.forward_bitrate)
                    .name("Recording bitrate")
                    .description("An FFmpeg bitrate, such as 4M for four megabits per second.")
                    .invalid(crate::media::validate_bitrate(self.forward_bitrate.trim()).err().map(|error| error.to_string()))
                    .id("forward-bitrate")
                    .on_input(Message::Bitrate)
                    .size(style::BODY)
                    .padding([5, 8])
                    .width(Fill)
                    .style(if crate::media::validate_bitrate(self.forward_bitrate.trim()).is_ok() { style::input } else { style::input_invalid }),
                    self.forward_field_error(false, p)].spacing(4),
                "",
                p,
            ),
            form_row(
                "Maximum file",
                tip(
                    self.num(Num::FileMib),
                    "For file destinations, recording stops when this file size limit is reached.",
                ),
                "MiB",
                p,
            ),
            action,
        ]
        .spacing(ROW_GAP)
        .into()
    }

    fn forward_field_error(&self, encoder: bool, p: &'static Palette) -> Element<'_, Message> {
        let result = if encoder {
            crate::media::validate_encoder(self.forward_codec, self.forward_encoder.trim())
        } else {
            crate::media::validate_bitrate(self.forward_bitrate.trim())
        };
        match result {
            Ok(()) => space().into(),
            Err(error) => field_error(error.to_string(), p),
        }
    }

    /// While forwarding: where to, with what, and how to stop. Its settings
    /// cannot change mid-stream, so the form gives way to this.
    fn forward_card<'a>(
        &'a self,
        destination: &'a str,
        p: &'static Palette,
    ) -> Element<'a, Message> {
        let stopping = self.forward_pending() == Some(true);
        // A file rather than a URL is a recording on this computer.
        let recording = Output::of(destination).recording();
        let (mark, title, stop) = if recording {
            (dot(p.ink(p.danger), 8.0), "Recording", "Stop recording")
        } else {
            (
                icon(Icon::Broadcast, 14.0, p.accent_text),
                "Forwarding",
                "Stop forwarding",
            )
        };
        let mut card = column![
            row![mark, text(title).size(style::BODY).font(style::SEMIBOLD)]
                .spacing(8)
                .align_y(Alignment::Center),
            tip(
                one_line(
                    redact_address(destination),
                    style::SMALL,
                    style::MONO,
                    p.secondary
                )
                .width(Fill),
                redact_address(destination),
            ),
        ]
        .spacing(6);
        // Only settings sent from here are known; the CLI may have started it.
        if let Some((_, summary)) = self
            .snapshot
            .active_camera
            .as_ref()
            .and_then(|camera| self.inspect.forwarded.get(camera))
            .filter(|(output, _)| output == destination)
        {
            card = card.push(text(summary.as_str()).size(style::SMALL).color(p.secondary));
        }
        let glyph = if stopping {
            icon::spinner(12.0, p.secondary, self.spin())
        } else {
            icon(Icon::Stop, 12.0, p.text)
        };
        card = card.push(space().height(2)).push(wide_button(
            glyph,
            if stopping { "Stopping…" } else { stop },
            style::stop,
            Some(Message::StopForward),
            stopping,
        ));
        column![
            container(card).padding(14).width(Fill).style(style::well),
            reason(
                if recording {
                    "Stop recording to change its settings."
                } else {
                    "Stop forwarding to change its settings."
                },
                p
            ),
        ]
        .spacing(10)
        .into()
    }

    pub(super) fn storage_settings(&self, p: &'static Palette) -> Element<'_, Message> {
        let open = self.storage_open;
        let summary = self.storage_status.state().summary(&self.storage_policy());
        let mut content = column![disclosure(
            "Storage & retention",
            (!open).then_some(summary),
            self.inspect.storage_turn.get(self.now),
            Message::ToggleStorage,
            p
        )]
        .spacing(ROW_GAP);
        if open {
            content = content
                .push(form_row(
                    "Requested budget",
                    self.num(Num::QuotaGib),
                    "GiB",
                    p,
                ))
                .push(form_row(
                    "Requested files",
                    self.num(Num::QuotaFiles),
                    "",
                    p,
                ))
                .push(form_row(
                    "Requested action",
                    pick_list(
                        &ON_FULL[..],
                        find(&ON_FULL, self.quota_action),
                        Message::OnFull,
                    )
                    .name("Requested action when a storage limit is reached")
                    .width(Fill)
                    .text_size(style::BODY)
                    .padding([5, 8])
                    .style(style::pick)
                    .menu_style(style::menu),
                    "",
                    p,
                ))
                .push(checkbox(
                    "Request an age limit",
                    self.retention_enabled,
                    Message::RetentionEnabled,
                ));
            if self.retention_enabled {
                content = content.push(form_row(
                    "Requested age",
                    self.num(Num::RetentionDays),
                    "days",
                    p,
                ));
            }
        }
        // The backend also merges a separately configured app-wide policy.
        // Show its actual limits/deletion behavior even with the form folded.
        for caption in self.storage_status.state().captions(&self.storage_policy()) {
            let color = match caption.tone {
                storage_policy::Tone::Normal => p.secondary,
                storage_policy::Tone::Warning => p.ink(p.warn),
                storage_policy::Tone::Error => p.ink(p.danger),
            };
            content = content.push(text(caption.text).size(style::SMALL).color(color));
        }
        content.into()
    }

    pub(super) fn jobs(&self, p: &'static Palette) -> Element<'_, Message> {
        let snapshot = &self.snapshot;
        let refresh = if self.pending(Job::RefreshJobs) {
            container(text("Refreshing…").size(style::SMALL).color(p.secondary))
                .padding([3, 8])
                .into()
        } else {
            link(
                "Refresh",
                snapshot.connected.is_some().then_some(Message::RefreshJobs),
            )
        };
        let mut content = column![section("Scheduled jobs", Some(refresh), p)].spacing(10);
        if snapshot.jobs.is_empty() {
            return content
                .push(
                    text("No scheduled captures for this camera.")
                        .size(style::SMALL)
                        .color(p.secondary),
                )
                .into();
        }
        let jobs = serde_json::to_value(&snapshot.jobs).unwrap_or_default();
        for job in jobs.as_array().into_iter().flatten().rev() {
            let id = job["id"].as_u64().unwrap_or_default();
            let status = job["status"].as_str().unwrap_or("unknown");
            let captured = job["captured"].as_u64().unwrap_or_default();
            let count = job["count"].as_u64().unwrap_or(1);
            let mut card = column![
                row![
                    text(format!("Job {id}"))
                        .size(style::BODY)
                        .font(style::MEDIUM),
                    space::horizontal(),
                    text(capitalize(status))
                        .size(style::CAPTION)
                        .color(match status {
                            "failed" => p.ink(p.danger),
                            "complete" => p.ink(p.live),
                            "running" | "pending" => p.accent_text,
                            _ => p.secondary,
                        }),
                ]
                .align_y(Alignment::Center),
                progress_bar(0.0..=1.0, captured as f32 / count.max(1) as f32)
                    .girth(4)
                    .style(style::progress),
                text(format!("{captured} / {count} frames"))
                    .size(style::CAPTION)
                    .color(p.secondary),
                text(job["output"].as_str().unwrap_or("").to_owned())
                    .size(style::CAPTION)
                    .font(style::MONO)
                    .color(p.secondary),
            ]
            .spacing(6);
            if matches!(status, "pending" | "running") {
                let wait = job["next_at_ms"]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_sub(epoch_ms());
                card = card.push(
                    row![
                        text(format!("Next frame in {:.1} s", wait as f64 / 1000.0))
                            .size(style::CAPTION)
                            .color(p.secondary),
                        space::horizontal(),
                        button(text("Stop").size(style::SMALL))
                            .padding([3, 8])
                            .style(style::stop)
                            .on_press(Message::CancelJob(id)),
                    ]
                    .align_y(Alignment::Center),
                );
            }
            if let Some(error) = job["error"].as_str() {
                card = card.push(
                    text(error.to_owned())
                        .size(style::CAPTION)
                        .color(p.ink(p.danger)),
                );
            }
            content = content.push(container(card).padding(12).width(Fill).style(style::well));
        }
        content.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actual_capture_and_recording_fields_expose_precise_invalid_drafts_and_recovery() {
        use iced::advanced::{renderer::Headless, widget::Tree};
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        let mut bench = bench();
        bench.snapshot.connected = Some(liveness::streaming_camera(0, 0, 0.0).info);
        bench.capture_to = destinations::Picker::new(false, false);
        bench.record_to = destinations::Picker::new(false, true);
        let mut tree = Tree::empty();
        let mut identity = None;
        for (invalid, value) in [(true, "rtsp://host/live"), (false, "capture.png")] {
            let _ = bench.update(Message::CapturePicker(destinations::Message::Output(
                value.into(),
            )));
            let mut view = bench.capture_settings(Palette::of(false));
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(320.0, 800.0)),
            );
            let (native, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                iced::advanced::Layout::new(&node),
                &renderer,
            );
            let (id, field) = native
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == accessibility::Role::TextInput
                        && node.label() == Some("Output path")
                })
                .unwrap();
            assert_eq!(field.value(), Some(value));
            assert_eq!(field.invalid(), invalid.then_some(accesskit::Invalid::True));
            if invalid {
                let cause = field.description().unwrap();
                assert_eq!(
                    native
                        .nodes
                        .iter()
                        .filter(|(_, node)| node.live() == Some(accesskit::Live::Assertive)
                            && node.label() == Some(cause))
                        .count(),
                    1
                );
            }
            assert_eq!(
                field.description(),
                invalid.then_some(
                    "capture output must be a file or directory; use forward for a network stream"
                )
            );
            assert!(field.supports_action(accesskit::Action::SetValue));
            if let Some(identity) = identity {
                assert_eq!(*id, identity);
            }
            identity = Some(*id);
        }
        let mut tree = Tree::empty();
        let mut identities = None;
        for (invalid, output, bitrate) in [
            (true, "recording.unsupported", "invalid"),
            (false, "recording.mkv", "4M"),
            (false, "rtsp://localhost:8554/camera", "4M"),
        ] {
            let _ = bench.update(Message::RecordPicker(destinations::Message::Output(
                output.into(),
            )));
            let _ = bench.update(Message::Bitrate(bitrate.into()));
            let output_error = crate::media::validate_forward_output(output)
                .err()
                .map(|error| error.to_string());
            let bitrate_error = crate::media::validate_bitrate(bitrate)
                .err()
                .map(|error| error.to_string());
            let mut view = bench.forward_form(Palette::of(false));
            tree.diff(view.as_widget());
            let node = view.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(320.0, 800.0)),
            );
            let (native, _, _) = accessibility::snapshot_widget(
                view.as_widget_mut(),
                &mut tree,
                iced::advanced::Layout::new(&node),
                &renderer,
            );
            let (output_id, output_field) = native
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.role() == accessibility::Role::TextInput
                        && node.label() == Some("Stream URL or file")
                })
                .unwrap();
            let (bitrate_id, bitrate_field) = native
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some("Recording bitrate"))
                .unwrap();
            assert_eq!(output_field.value(), Some(output));
            assert_eq!(
                output_field.invalid(),
                invalid.then_some(accesskit::Invalid::True)
            );
            assert_eq!(output_field.description(), output_error.as_deref());
            for cause in [output_error.as_deref(), bitrate_error.as_deref()]
                .into_iter()
                .flatten()
            {
                assert_eq!(
                    native
                        .nodes
                        .iter()
                        .filter(|(_, node)| node.live() == Some(accesskit::Live::Assertive)
                            && node.label() == Some(cause))
                        .count(),
                    1,
                    "each invalid draft is announced once: {cause}"
                );
            }
            assert_eq!(bitrate_field.value(), Some(bitrate));
            assert_eq!(
                bitrate_field.invalid(),
                invalid.then_some(accesskit::Invalid::True)
            );
            assert_eq!(
                bitrate_field.description(),
                bitrate_error.as_deref().or(Some(
                    "An FFmpeg bitrate, such as 4M for four megabits per second."
                ))
            );
            let start = native
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label()
                        == Some(if Output::of(output).recording() {
                            "Start recording"
                        } else {
                            "Start forwarding"
                        })
                })
                .unwrap();
            assert_eq!(start.1.is_disabled(), invalid);
            assert_eq!(start.1.supports_action(accesskit::Action::Click), !invalid);
            let current = (*output_id, *bitrate_id);
            if let Some(identities) = identities {
                assert_eq!(current, identities);
            }
            identities = Some(current);
        }
    }

    #[test]
    fn capture_failure_has_one_live_announcement_and_a_readable_inline_outcome() {
        use iced::advanced::{renderer::Headless, widget::Tree};
        let renderer = iced::futures::executor::block_on(iced::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
            Some("tiny-skia"),
        ))
        .expect("software renderer");
        let mut bench = bench();
        bench.snapshot.connected = Some(liveness::streaming_camera(0, 0, 0.0).info);
        bench.capture_to = destinations::Picker::new(false, false);
        bench.output = "capture.png".into();
        bench.inspect.capture_failed = Some((
            bench.snapshot.active_camera.clone(),
            "permission denied".into(),
        ));
        bench.put_notice(
            "Capture failed: permission denied".into(),
            Some("permission denied".into()),
            Level::Error,
        );
        let mut view: Element<'_, Message> = iced::widget::column![
            bench.capture_settings(Palette::of(false)),
            bench.toolbar(Palette::of(false))
        ]
        .into();
        let mut tree = Tree::new(view.as_widget());
        let node = view.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &iced::advanced::layout::Limits::new(Size::ZERO, Size::new(420.0, 1000.0)),
        );
        let (native, _, _) = accessibility::snapshot_widget(
            view.as_widget_mut(),
            &mut tree,
            iced::advanced::Layout::new(&node),
            &renderer,
        );
        let alerts: Vec<_> = native
            .nodes
            .iter()
            .filter(|(_, node)| node.live() == Some(accesskit::Live::Assertive))
            .collect();
        assert_eq!(alerts.len(), 1, "the command outcome is announced once");
        assert_eq!(
            alerts[0].1.label(),
            Some("Capture failed: permission denied")
        );
        assert!(
            native
                .nodes
                .iter()
                .any(|(_, node)| node.role() == accessibility::Role::Label
                    && node.label() == Some("Permission denied")
                    && node.live().is_none())
        );
    }

    #[test]
    fn search_preserves_editor_focus_and_selection_when_clear_action_changes() {
        use iced::advanced::widget::{Tree, tree::Tag};
        use iced::widget::text_input::{State, Value, cursor};
        type Editor = State<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>;

        fn editor(tree: &mut Tree) -> Option<&mut Editor> {
            if tree.tag == Tag::of::<Editor>() {
                Some(tree.state.downcast_mut::<Editor>())
            } else {
                tree.children.iter_mut().find_map(editor)
            }
        }

        let mut bench = bench();
        let p = Palette::of(false);
        let mut tree = Tree::new(bench.search_field(p).as_widget());
        editor(&mut tree).unwrap().focus();
        bench.search = "Exposure".into();
        tree.diff(bench.search_field(p).as_widget());
        let state = editor(&mut tree).unwrap();
        assert!(state.is_focused(), "typing the first letter retains focus");
        state.select_range(1, 3);
        bench.search.clear();
        tree.diff(bench.search_field(p).as_widget());
        let state = editor(&mut tree).unwrap();
        assert!(state.is_focused(), "clearing the field retains focus");
        assert_eq!(
            state.cursor().state(&Value::new("Exposure")),
            cursor::State::Selection { start: 1, end: 3 },
            "view changes preserve the editor state rather than replacing it"
        );
    }

    fn bench() -> Workbench {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        bench.snapshot.features = vec![
            sample_feature("Gain", "Float", json!(0.0)),
            sample_feature("Width", "Integer", json!(640)),
            sample_feature("AcquisitionStop", "Command", json!(null)),
            sample_feature("ExposureTime", "Float", json!(10000.0)),
            sample_feature("Height", "Integer", json!(480)),
            sample_feature("AcquisitionStart", "Command", json!(null)),
            sample_feature("DeviceModelName", "String", json!("Pattern")),
        ];
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.tick_inspector();
        bench
    }

    /// Whether anything the inspector registered still moves.
    fn moving(bench: &Workbench) -> bool {
        let now = bench.now;
        bench.inspect.motions().any(|motion| motion.animating(now))
            || bench
                .inspect
                .flashes()
                .any(|flashes| flashes.animating(now))
    }

    type Reply = mpsc::Sender<anyhow::Result<serde_json::Value>>;

    /// A command doing `job` on its way to `camera`.
    fn asked_of(bench: &mut Workbench, camera: &str, job: Job, value: Option<&str>) -> Reply {
        let (sender, receiver) = mpsc::channel();
        bench.pending.push(Pending {
            receiver,
            camera: Some(camera.into()),
            value: value.map(str::to_owned),
            at: bench.now,
            ..Pending::unanswered(job)
        });
        bench.sync_animations();
        sender
    }

    /// A write of `feature` on its way to sim:0 with what its field holds,
    /// as `Message::Commit` sends it.
    fn sent(bench: &mut Workbench, feature: &str) -> Reply {
        let draft = bench.edits.get(feature).cloned();
        asked_of(bench, "sim:0", Job::Set(feature.into()), draft.as_deref())
    }

    fn draft(bench: &Workbench) -> Option<&str> {
        bench.edits.get("ExposureTime").map(String::as_str)
    }

    #[test]
    fn features_list_in_setup_order_with_one_stream_row() {
        let bench = bench();
        let index = &bench.inspect.index;
        let names = |group: &str| -> Vec<&str> {
            index
                .groups
                .iter()
                .find(|(name, _)| *name == group)
                .map(|(_, members)| {
                    members
                        .iter()
                        .map(|&i| bench.snapshot.features[i].name.as_str())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(names("Image"), ["Width", "Height"]);
        assert_eq!(
            names("Acquisition"),
            ["AcquisitionStart", "ExposureTime", "Gain"]
        );
        assert_eq!(names("Device"), ["DeviceModelName"]);
        assert_eq!(index.listed, 6, "AcquisitionStop folds into the Stream row");
        assert_eq!(index.labels[5], "Stream");
        assert_eq!(index.labels[3], "Exposure Time");
        assert!(index.words[3].contains("exposure time"));
    }

    #[test]
    fn the_index_follows_the_names_not_the_values() {
        let mut bench = bench();
        let key = bench.inspect.index.key;
        bench.snapshot.features[0].value = Some(json!(3.5));
        bench.tick_inspector();
        assert_eq!(bench.inspect.index.key, key);
        bench.snapshot.features[0].name = "BlackLevel".into();
        bench.tick_inspector();
        assert_ne!(bench.inspect.index.key, key);
        bench.snapshot.features.pop();
        bench.sync_animations();
        assert_eq!(bench.inspect.index.len, bench.snapshot.features.len());
    }

    fn members<'a>(index: &FeatureIndex, features: &'a [FeatureInfo], group: &str) -> Vec<&'a str> {
        index
            .groups
            .iter()
            .find(|(name, _)| *name == group)
            .map(|(_, members)| members.iter().map(|&i| features[i].name.as_str()).collect())
            .unwrap_or_default()
    }

    fn features_named(names: &[&str]) -> Vec<FeatureInfo> {
        names
            .iter()
            .map(|name| sample_feature(name, "Integer", json!(1)))
            .collect()
    }

    #[test]
    fn line_scan_separates_block_geometry_line_timing_and_motion_sync() {
        let mut features = features_named(&[
            "TriggerSource",
            "Height",
            "AcquisitionLineRate",
            "EncoderSourceA",
            "Width",
            "TriggerSelector",
            "VendorAdvancedSetting",
            "AcquisitionFrameRate",
        ]);
        features[1].display_name = "Vendor frame rows".into();
        let index = FeatureIndex::new(&features, CameraKind::LineScan);
        assert_eq!(
            members(&index, &features, "Line geometry"),
            ["Width", "Height"]
        );
        assert_eq!(
            members(&index, &features, "Line timing"),
            ["AcquisitionLineRate"]
        );
        assert_eq!(
            members(&index, &features, "Trigger and encoder"),
            ["EncoderSourceA", "TriggerSelector", "TriggerSource"]
        );
        assert_eq!(index.labels[1], "Block height (lines)");
        assert_eq!(index.labels[2], "Line rate");
        assert_eq!(index.labels[4], "Line width");
        assert_eq!(index.labels[7], "Block rate");
        assert!(index.tips[1].contains("not the sensor's height"));
        assert!(index.words[1].contains("block height"));
        assert!(index.words[1].contains("vendor frame rows"));
        assert_eq!(
            members(&index, &features, "Other"),
            ["VendorAdvancedSetting"]
        );
        assert_eq!(index.listed, features.len());
    }

    #[test]
    fn depth_components_and_coordinate_selectors_precede_dependent_controls() {
        let features = features_named(&[
            "Scan3dCoordinateScale",
            "ConfidenceThreshold",
            "ComponentEnable",
            "Scan3dDistanceUnit",
            "Scan3dCoordinateSelector",
            "ComponentSelector",
            "DepthMax",
            "DepthMin",
            "PixelFormat",
        ]);
        let index = FeatureIndex::new(&features, CameraKind::Depth);
        assert_eq!(
            members(&index, &features, "Components"),
            ["ComponentSelector", "ComponentEnable"]
        );
        assert_eq!(
            members(&index, &features, "Depth"),
            ["DepthMin", "DepthMax", "ConfidenceThreshold"]
        );
        assert_eq!(
            members(&index, &features, "Calibration"),
            [
                "Scan3dCoordinateSelector",
                "Scan3dDistanceUnit",
                "Scan3dCoordinateScale"
            ]
        );
        assert_eq!(index.labels[1], "Minimum confidence");
        assert_eq!(index.labels[3], "Distance unit");
        assert_eq!(index.labels[5], "Component");
        assert!(index.tips[5].contains("live settings"));
        assert_eq!(members(&index, &features, "Image"), ["PixelFormat"]);
    }

    #[test]
    fn microscope_and_thermal_controls_follow_their_measurement_workflows() {
        let features = features_named(&[
            "FocusPosition",
            "IlluminationIntensity",
            "Magnification",
            "ObjectiveSelector",
            "StageZ",
            "ExposureTime",
            "DeviceTemperature",
        ]);
        let index = FeatureIndex::new(&features, CameraKind::Microscope);
        assert_eq!(
            members(&index, &features, "Optics"),
            ["ObjectiveSelector", "Magnification", "FocusPosition"]
        );
        assert_eq!(
            members(&index, &features, "Illumination"),
            ["IlluminationIntensity"]
        );
        assert_eq!(members(&index, &features, "Stage"), ["StageZ"]);
        assert_eq!(members(&index, &features, "Acquisition"), ["ExposureTime"]);
        assert_eq!(members(&index, &features, "Device"), ["DeviceTemperature"]);

        let features = features_named(&[
            "Emissivity",
            "DeviceTemperature",
            "TemperatureRange",
            "TemperatureLinearMode",
        ]);
        let index = FeatureIndex::new(&features, CameraKind::Thermal);
        assert_eq!(
            members(&index, &features, "Temperature"),
            ["TemperatureLinearMode", "TemperatureRange", "Emissivity"]
        );
        assert_eq!(members(&index, &features, "Device"), ["DeviceTemperature"]);
        assert_eq!(index.labels[2], "Measurement range");
        assert!(index.tips[0].contains("target material"));
    }

    #[test]
    fn scanner_and_unknown_cameras_keep_available_controls_and_vendor_labels() {
        let mut features = features_named(&[
            "ScanResolution",
            "ScanSource",
            "AcquisitionStop",
            "AcquisitionStart",
        ]);
        let index = FeatureIndex::new(&features, CameraKind::DocumentScanner);
        assert_eq!(
            members(&index, &features, "Scan"),
            ["ScanSource", "ScanResolution", "AcquisitionStart"]
        );
        assert_eq!(index.labels[3], "Scan");
        assert_eq!(index.listed, 3);

        features = features_named(&["Width", "Height", "VendorSpecialControl"]);
        features[2].display_name = "Vendor precision mode".into();
        features[2].description = "A vendor-specific extension".into();
        let index = FeatureIndex::new(&features, CameraKind::Unknown);
        assert_eq!(members(&index, &features, "Image"), ["Width", "Height"]);
        assert_eq!(
            members(&index, &features, "Other"),
            ["VendorSpecialControl"]
        );
        assert_eq!(index.labels[2], "Vendor precision mode");
        assert!(index.words[2].contains("vendor-specific extension"));
        assert_eq!(
            index.listed,
            features.len(),
            "no unsupported controls are invented"
        );
    }

    #[test]
    fn the_index_refreshes_on_camera_type_and_selector_text_changes() {
        let mut bench = bench();
        bench.snapshot.connected = Some(liveness::streaming_camera(10, 0, 30.0).info);
        bench.tick_inspector();
        let key = bench.inspect.index.key;
        bench.snapshot.connected.as_mut().unwrap().profile.kind = CameraKind::LineScan;
        bench.sync_inspector();
        assert_ne!(bench.inspect.index.key, key);
        assert_eq!(bench.inspect.index.labels[4], "Block height (lines)");
        let key = bench.inspect.index.key;
        bench.snapshot.features[0].display_name = "Component gain".into();
        bench.snapshot.features[0].description = "Gain for selected component".into();
        bench.tick_inspector();
        assert_ne!(bench.inspect.index.key, key);
        assert_eq!(bench.inspect.index.labels[0], "Component gain");
        assert!(bench.inspect.index.words[0].contains("selected component"));
    }

    #[test]
    fn specialized_guidance_and_automation_respect_measurement_types() {
        let mut profile = CameraProfile::default();
        assert!(!offers_auto(&profile), "no controls were advertised");
        profile.capabilities = vec![CameraCapability::Exposure];
        for kind in [
            CameraKind::AreaScan,
            CameraKind::Unknown,
            CameraKind::Microscope,
        ] {
            profile.kind = kind;
            assert!(offers_auto(&profile));
        }
        for kind in [
            CameraKind::LineScan,
            CameraKind::Depth,
            CameraKind::Thermal,
            CameraKind::DocumentScanner,
        ] {
            profile.kind = kind;
            assert!(!offers_auto(&profile));
        }
        assert!(camera_guidance(CameraKind::LineScan).contains("Block height"));
        assert!(camera_guidance(CameraKind::Depth).contains("one component"));
        assert!(camera_guidance(CameraKind::Microscope).contains("focus"));
        assert!(camera_guidance(CameraKind::Thermal).contains("emissivity"));
        assert!(camera_guidance(CameraKind::DocumentScanner).contains("one page"));
        assert!(
            capture_guidance(CameraKind::Depth)
                .unwrap()
                .contains("Raw pixels")
        );
        assert!(
            forward_guidance(CameraKind::Thermal)
                .unwrap()
                .contains("8-bit")
        );
        assert_eq!(capture_title(CameraKind::LineScan), "Save line blocks");
    }

    #[test]
    fn an_accepted_write_shows_the_stored_value_and_glows_once() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "15003".into());
        let reply = sent(&mut bench, "ExposureTime");
        reply
            .send(Ok(json!({"name": "ExposureTime", "value": 15000.0})))
            .unwrap();
        bench.poll();
        assert_eq!(draft(&bench), Some("15000"), "what the camera stored");
        let start = bench.now;
        assert_eq!(bench.inspect.accepted.level("ExposureTime", start), 1.0);
        assert_eq!(bench.inspect.accepted.level("Gain", start), 0.0);
        assert!(moving(&bench));
        bench.now = start + ACCEPTED;
        assert!(!moving(&bench), "the glow settles");
        assert_eq!(bench.inspect.accepted.level("ExposureTime", bench.now), 0.0);
    }

    #[test]
    fn a_draft_edited_while_its_write_is_on_its_way_stays() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "15000".into());
        let reply = sent(&mut bench, "ExposureTime");
        bench.edits.insert("ExposureTime".into(), "150001".into());
        reply
            .send(Ok(json!({"name": "ExposureTime", "value": 15000.0})))
            .unwrap();
        bench.poll();
        assert_eq!(draft(&bench), Some("150001"));
        assert_eq!(
            bench.inspect.accepted.level("ExposureTime", bench.now),
            1.0,
            "the camera still took the write"
        );
    }

    #[test]
    fn only_the_newest_write_of_a_field_settles_it() {
        // Both replies in one poll, then one at a time.
        for together in [true, false] {
            let mut bench = bench();
            bench.edits.insert("ExposureTime".into(), "15000".into());
            let first = sent(&mut bench, "ExposureTime");
            bench.now += Duration::from_millis(1);
            bench.edits.insert("ExposureTime".into(), "16000".into());
            let second = sent(&mut bench, "ExposureTime");
            bench.edits.insert("ExposureTime".into(), "17".into());
            first.send(Ok(json!({"value": 15000.0}))).unwrap();
            if !together {
                bench.poll();
                assert_eq!(draft(&bench), Some("17"));
                assert!(bench.inspect.accepted.is_empty(), "the newer one glows");
            }
            second.send(Ok(json!({"value": 16000.0}))).unwrap();
            bench.poll();
            assert_eq!(draft(&bench), Some("17"), "typed, not applied");
        }
        // With nothing typed since, the newest reply's stored value shows.
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "15000".into());
        let first = sent(&mut bench, "ExposureTime");
        bench.edits.insert("ExposureTime".into(), "16000.4".into());
        let second = sent(&mut bench, "ExposureTime");
        first.send(Ok(json!({"value": 15000.0}))).unwrap();
        second.send(Ok(json!({"value": 16000.0}))).unwrap();
        bench.poll();
        assert_eq!(draft(&bench), Some("16000"));
    }

    #[test]
    fn replies_settle_in_the_order_sent() {
        // A capture keeps the worker busy while two writes queue behind it,
        // and all three answer in one poll. Either way round, only the
        // newer write has the last word.
        for newer_taken in [true, false] {
            let mut bench = bench();
            let capture = asked_of(&mut bench, "sim:0", Job::Capture, None);
            bench.edits.insert("ExposureTime".into(), "5".into());
            let older = sent(&mut bench, "ExposureTime");
            bench.now += Duration::from_millis(1);
            bench.edits.insert("ExposureTime".into(), "15000".into());
            let newer = sent(&mut bench, "ExposureTime");
            let (taken, refused) = if newer_taken {
                (newer, older)
            } else {
                (older, newer)
            };
            capture
                .send(Ok(json!({"files": ["capture-0001.png"], "count": 1})))
                .unwrap();
            refused.send(Err(anyhow::anyhow!("out of range"))).unwrap();
            taken.send(Ok(json!({"value": 15000.0}))).unwrap();
            bench.poll();
            assert!(bench.pending.is_empty());
            let error = bench.inspect.write_errors.get("ExposureTime");
            let glow = bench.inspect.accepted.level("ExposureTime", bench.now);
            if newer_taken {
                assert_eq!(error, None);
                assert_eq!(draft(&bench), Some("15000"));
                assert_eq!(glow, 1.0);
            } else {
                assert_eq!(error.map(String::as_str), Some("out of range"));
                assert_eq!(glow, 0.0, "the older one taken does not glow");
            }
        }
    }

    #[test]
    fn writes_settle_only_the_rows_of_their_own_camera() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "5".into());
        let refused = sent(&mut bench, "ExposureTime");
        let taken = asked_of(&mut bench, "sim:0", Job::Set("Gain".into()), Some("3"));
        assert!(bench.asked("ExposureTime").is_some());
        // Another camera takes the panel, as `tick()` does it.
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.edits.clear();
        bench.inspect.write_errors.clear();
        assert!(bench.asked("ExposureTime").is_none(), "not applying here");
        bench
            .inspect
            .write_errors
            .insert("Gain".into(), "its own".into());
        bench.edits.insert("Gain".into(), "3".into());
        refused.send(Err(anyhow::anyhow!("too short"))).unwrap();
        taken.send(Ok(json!({"value": 3.5}))).unwrap();
        bench.poll();
        assert!(!bench.inspect.write_errors.contains_key("ExposureTime"));
        assert_eq!(
            bench.inspect.write_errors.get("Gain").map(String::as_str),
            Some("its own"),
            "kept"
        );
        assert_eq!(bench.edits.get("Gain").map(String::as_str), Some("3"));
        assert!(bench.inspect.accepted.is_empty(), "no glow");
    }

    #[test]
    fn a_retry_holds_back_the_old_refusal_until_its_outcome() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "5".into());
        let reply = sent(&mut bench, "ExposureTime");
        reply.send(Err(anyhow::anyhow!("auto owns it"))).unwrap();
        bench.poll();
        let shown = |bench: &Workbench| {
            bench
                .refusal("ExposureTime", bench.asked("ExposureTime"))
                .cloned()
        };
        assert_eq!(shown(&bench).as_deref(), Some("auto owns it"));
        // The same value again: the row shows it applying, not the old error.
        let retry = sent(&mut bench, "ExposureTime");
        assert_eq!(shown(&bench), None);
        retry.send(Err(anyhow::anyhow!("still auto"))).unwrap();
        bench.poll();
        assert_eq!(shown(&bench).as_deref(), Some("still auto"));
        let retry = sent(&mut bench, "ExposureTime");
        retry.send(Ok(json!({"value": 5.0}))).unwrap();
        bench.poll();
        assert_eq!(shown(&bench), None);
        assert!(bench.inspect.write_errors.is_empty());
    }

    #[test]
    fn controls_show_what_a_write_on_its_way_asked_for() {
        let mut bench = bench();
        let _pick = asked_of(
            &mut bench,
            "sim:0",
            Job::Set("TriggerMode".into()),
            Some("On"),
        );
        let _run = asked_of(
            &mut bench,
            "sim:0",
            Job::Execute("UserSetLoad".into()),
            None,
        );
        let _elsewhere = asked_of(&mut bench, "sim:1", Job::Set("Gain".into()), Some("2"));
        let asked = bench.asked("TriggerMode").expect("on its way");
        assert_eq!(asked.value.as_deref(), Some("On"));
        assert!(matches!(
            bench.asked("UserSetLoad").map(|p| &p.job),
            Some(Job::Execute(_))
        ));
        assert!(bench.asked("Gain").is_none(), "another camera's");
        bench.pending.clear();
        assert!(bench.asked("TriggerMode").is_none());
    }

    #[test]
    fn a_failed_capture_replaces_the_last_saved_line() {
        let mut bench = bench();
        let capture = || Pending {
            camera: Some("sim:0".into()),
            ..Pending::unanswered(Job::Capture)
        };
        bench.settle(
            &capture(),
            &Ok(json!({"files": ["capture.png"], "count": 1})),
        );
        assert!(bench.last_saved.is_some());
        assert!(bench.just_saved(Some("sim:0")));
        bench.settle(
            &capture(),
            &Err(anyhow::anyhow!(
                "capture output already exists: capture.png"
            )),
        );
        assert!(bench.last_saved.is_none(), "not this press's file");
        assert!(!bench.just_saved(None), "no Saved on the buttons");
        assert_eq!(
            bench.inspect.capture_failed,
            Some((
                Some("sim:0".into()),
                "capture output already exists: capture.png".into()
            ))
        );
        // Another camera's success leaves it; this camera's clears it.
        bench.settle(
            &Pending {
                camera: Some("sim:1".into()),
                ..Pending::unanswered(Job::Capture)
            },
            &Ok(json!({"files": ["b.png"], "count": 1})),
        );
        assert!(bench.inspect.capture_failed.is_some());
        bench.settle(&capture(), &Ok(json!({"files": ["c.png"], "count": 1})));
        assert!(bench.inspect.capture_failed.is_none());
    }

    #[test]
    fn the_stream_locks_only_what_it_keeps_from_changing() {
        let mut bench = bench();
        let mut other = liveness::streaming_camera(10, 0, 30.0);
        other.info.id = "sim:1".into();
        bench.snapshot.cameras = vec![liveness::streaming_camera(10, 0, 30.0), other];
        // Width can change while idle; Height, as on an RTSP source, never.
        bench.inspect.lockable.clear();
        bench.snapshot.features[4].writable = false;
        bench.tick_inspector();
        bench.snapshot.streaming = true;
        let width = bench.snapshot.features[1].clone();
        let height = bench.snapshot.features[4].clone();
        assert!(bench.stream_locks(&width));
        assert!(!bench.stream_locks(&height), "plain read-only");
        // Read again mid-stream, the camera reports Width locked.
        bench.snapshot.features[1].writable = false;
        bench.tick_inspector();
        let locked = bench.snapshot.features[1].clone();
        assert!(bench.stream_locks(&locked));
        assert!(!bench.stream_locks(&bench.snapshot.features[3].clone()));
        // Stopped, the list still holds that mid-stream read; started
        // again, Width is still the stream's to unlock.
        bench.snapshot.streaming = false;
        bench.tick_inspector();
        bench.snapshot.streaming = true;
        bench.tick_inspector();
        assert!(bench.stream_locks(&locked));
        // Another camera, never seen idle, goes by what it reports.
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.tick_inspector();
        assert!(!bench.stream_locks(&locked));
        // Back to the first while it streams: what it showed idle holds.
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.tick_inspector();
        assert!(bench.stream_locks(&locked));
        // A camera that leaves is forgotten.
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.snapshot.cameras.remove(0);
        bench.tick_inspector();
        assert!(!bench.inspect.lockable.contains_key("sim:0"));
    }

    #[test]
    fn a_forward_keeps_the_settings_it_was_sent_with() {
        let mut bench = bench();
        let mut other = liveness::streaming_camera(10, 0, 30.0);
        other.info.id = "sim:1".into();
        bench.snapshot.cameras = vec![liveness::streaming_camera(10, 0, 30.0), other];
        let reply = asked_of(&mut bench, "sim:0", Job::Forward { recording: false }, None);
        bench.forward_codec = "h265";
        reply
            .send(Ok(json!({"forwarding": "rtsp://localhost:8554/cam"})))
            .unwrap();
        bench.poll();
        bench.snapshot.cameras[0].forwarding = Some("rtsp://localhost:8554/cam".into());
        let summary = |bench: &Workbench| {
            bench
                .inspect
                .forwarded
                .get("sim:0")
                .map(|(_, summary)| summary.clone())
        };
        assert_eq!(summary(&bench).as_deref(), Some("H.264 · 30 fps · 4M"));
        assert!(bench.inspect.forward_sent.is_empty());
        // Showing another camera and back keeps it.
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.snapshot.forwarding = None;
        bench.tick_inspector();
        bench.snapshot.active_camera = Some("sim:0".into());
        bench.tick_inspector();
        assert!(summary(&bench).is_some());
        // Its own stop ends it.
        bench.snapshot.cameras[0].forwarding = None;
        bench.tick_inspector();
        assert!(summary(&bench).is_none());
    }

    #[test]
    fn a_refused_write_keeps_the_draft_and_its_error() {
        let mut bench = bench();
        bench.edits.insert("ExposureTime".into(), "5".into());
        let reply = sent(&mut bench, "ExposureTime");
        reply
            .send(Err(anyhow::anyhow!("5 is below the minimum of 10")))
            .unwrap();
        bench.poll();
        assert_eq!(draft(&bench), Some("5"));
        assert!(bench.inspect.write_errors.contains_key("ExposureTime"));
        assert!(bench.inspect.accepted.is_empty(), "no glow");
    }

    #[test]
    fn thumbs_jump_into_place_then_slide() {
        let _motion = motion::test_reduce_motion(false);
        let mut bench = bench();
        bench.tab = Tab::Forward;
        bench.sync_animations();
        let start = bench.now;
        assert_eq!(bench.inspect.tab_thumb.get(start), 2.0, "placed at once");
        assert!(!moving(&bench));
        bench.tab = Tab::Features;
        bench.sync_animations();
        let mid = bench.inspect.tab_thumb.get(start + THUMB / 2);
        assert!(mid > 0.0 && mid < 2.0, "slides: {mid}");
        bench.now = start + THUMB;
        assert!(!moving(&bench), "settles");
        assert_eq!(bench.inspect.tab_thumb.get(bench.now), 0.0);
    }

    #[test]
    fn the_mode_thumb_moves_on_the_request_and_back_on_refusal() {
        let mut bench = bench();
        bench.sync_animations();
        let (sender, receiver) = mpsc::channel();
        bench.pending.push(Pending {
            receiver,
            camera: Some("sim:0".into()),
            at: bench.now,
            ..Pending::unanswered(Job::AutoOn)
        });
        bench.sync_animations();
        assert_eq!(bench.inspect.mode_thumb.target(), 1.0, "optimistic");
        sender.send(Err(anyhow::anyhow!("refused"))).unwrap();
        bench.poll();
        bench.sync_animations();
        assert_eq!(bench.inspect.mode_thumb.target(), 0.0, "slides back");
        bench.now += THUMB;
        assert!(!moving(&bench));
        // Another camera's mode is no change of mode: the thumb jumps.
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.snapshot.auto = Some(Default::default());
        bench.sync_animations();
        assert_eq!(bench.inspect.mode_thumb.get(bench.now), 1.0);
        assert!(!bench.inspect.mode_thumb.animating(bench.now));
    }

    #[test]
    fn the_context_row_lights_up_when_another_camera_is_selected() {
        let mut bench = bench();
        bench.snapshot.cameras = vec![
            liveness::streaming_camera(10, 0, 30.0),
            liveness::streaming_camera(10, 0, 30.0),
        ];
        bench.sync_animations();
        let start = bench.now;
        assert_eq!(bench.inspect.context.get(start), 0.0);
        bench.snapshot.active_camera = Some("sim:1".into());
        bench.sync_animations();
        assert_eq!(bench.inspect.context.get(start), 1.0);
        bench.now = start + CONTEXT;
        assert!(!moving(&bench), "fades out and settles");
    }

    #[test]
    fn disclosures_turn_open_and_settle() {
        let _motion = motion::test_reduce_motion(false);
        let mut bench = bench();
        bench.sync_animations();
        let start = bench.now;
        bench.storage_open = true;
        bench.sync_animations();
        let mid = bench.inspect.storage_turn.get(start + motion::RING / 2);
        assert!(mid > 0.0 && mid < 1.0, "turns: {mid}");
        bench.now = start + motion::RING;
        assert!(!moving(&bench));
        assert_eq!(bench.inspect.storage_turn.get(bench.now), 1.0);
        bench.storage_open = false;
        bench.sync_animations();
        bench.now += motion::RING_OUT;
        assert!(!moving(&bench));
        assert_eq!(bench.inspect.storage_turn.get(bench.now), 0.0);
    }

    #[test]
    fn schedules_read_in_the_right_number() {
        assert_eq!(
            schedule_text_for(CameraKind::Unknown, 1, 60.0),
            "1 frame, one every 60 seconds"
        );
        assert_eq!(
            schedule_text_for(CameraKind::AreaScan, 1200, 1.0),
            "1,200 frames, one every 1 second"
        );
        assert_eq!(
            schedule_text_for(CameraKind::Microscope, 10, 2.5),
            "10 frames, one every 2.5 seconds"
        );
        assert_eq!(
            schedule_text_for(CameraKind::LineScan, 3, 0.0),
            "3 blocks, captured consecutively"
        );
        assert_eq!(
            schedule_text_for(CameraKind::DocumentScanner, 1, 2.0),
            "1 page, one every 2 seconds"
        );
    }

    #[test]
    fn forward_settings_sum_up_what_was_sent() {
        let mut bench = bench();
        assert_eq!(bench.forward_summary(), "H.264 · 30 fps · 4M");
        bench.forward_encoder = "h264_videotoolbox".into();
        bench.forward_bitrate = " ".into();
        assert_eq!(
            bench.forward_summary(),
            "H.264 · 30 fps · h264_videotoolbox"
        );
    }
}
