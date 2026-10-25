use crate::{
    genicam::{Bounds, FeatureInfo, NodeMap},
    types::{Backend, CameraInfo, Frame, Transport, TransportStats},
};
use anyhow::{Context, Result, ensure};
use std::{net::Ipv4Addr, time::Duration};

pub fn discover(timeout: Duration, simulated: bool) -> Result<(Vec<CameraInfo>, Vec<String>)> {
    discover_with(timeout, simulated, false)
}

/// Discover cameras, adding cameras on saved hubs when `hubs` is set. Only
/// the session coordinator lists hubs; camera workers never contact them.
pub fn discover_with(
    timeout: Duration,
    simulated: bool,
    hubs: bool,
) -> Result<(Vec<CameraInfo>, Vec<String>)> {
    let mut devices = Vec::new();
    let mut warnings = Vec::new();
    let (gige, onvif, native, scanners, hub_cameras) = std::thread::scope(|s| {
        let g = s.spawn(|| crate::transport::gige::discover(timeout));
        let o = s.spawn(|| crate::onvif::camera_infos(timeout));
        let n = s.spawn(|| crate::media::native_devices_with_timeout(timeout));
        let d = s.spawn(|| crate::scanner::discover(timeout));
        let h = hubs.then(|| s.spawn(|| crate::hubs::discovered(timeout)));
        (
            g.join(),
            o.join(),
            n.join(),
            d.join(),
            h.map(|h| h.join().unwrap_or_default()),
        )
    });
    for (label, result) in [("GigE", gige), ("ONVIF", onvif), ("Scanner", scanners)] {
        match result {
            Ok(Ok(v)) => devices.extend(v),
            Ok(Err(e)) => warnings.push(format!("{label} discovery: {e:#}")),
            Err(_) => warnings.push(format!("{label} discovery worker stopped")),
        }
    }
    #[cfg(feature = "usb")]
    match crate::transport::usb::discover() {
        Ok(v) => devices.extend(v),
        Err(e) => warnings.push(format!("USB discovery: {e:#}")),
    }
    if simulated {
        devices.push(crate::transport::simulator::info());
    }
    match native {
        Ok(Ok(native)) => {
            if let Some(items) = native["devices"].as_array() {
                for d in items {
                    if let Some(id) = d["id"].as_str() {
                        devices.push(CameraInfo {
                            id: id.into(),
                            transport: Transport::Media,
                            vendor: std::env::consts::OS.into(),
                            model: d["name"].as_str().unwrap_or("Host camera").into(),
                            serial: id.into(),
                            address: Some(id.into()),
                            profile: Default::default(),
                        });
                    }
                }
            }
        }
        Ok(Err(error)) => warnings.push(format!("Native camera discovery: {error:#}")),
        Err(_) => warnings.push("Native camera discovery worker stopped".into()),
    }
    devices.sort_by(|a, b| a.id.cmp(&b.id));
    devices.dedup_by(|a, b| a.id == b.id);
    // Hub cameras come after local ones, so they never push USB or GigE
    // cameras off the welcome screen.
    if let Some((hub_devices, hub_warnings)) = hub_cameras {
        devices.extend(hub_devices);
        warnings.extend(hub_warnings);
    }
    for info in &mut devices {
        info.profile = crate::camera_profile::detect(info, &[]);
    }
    Ok((devices, warnings))
}

pub struct Camera {
    pub info: CameraInfo,
    backend: Box<dyn Backend>,
    nodes: NodeMap,
    streaming: bool,
    xml: String,
    profile_metadata: Vec<FeatureInfo>,
    network_budget_bytes_per_second: Option<u64>,
}
/// Reject malformed known source forms without checking availability or build capabilities.
/// Bare camera IDs and serials are resolved against discovery results on open.
pub fn validate_selector_shape(selector: &str) -> Result<()> {
    ensure!(
        !selector.trim().is_empty() && !selector.chars().any(char::is_control),
        "camera selector must be nonempty and contain no control characters"
    );
    if selector.starts_with("sim:") {
        crate::transport::simulator::validate_selector_shape(selector)?;
    } else if let Some(address) = selector.strip_prefix("gige:") {
        // Discovery names GigE cameras by their 48-bit MAC address.
        let discovered_id = address.len() == 12 && address.bytes().all(|b| b.is_ascii_hexdigit());
        ensure!(
            discovered_id || address.parse::<Ipv4Addr>().is_ok(),
            "GigE selector needs an IPv4 address or a discovered ID, for example gige:192.168.1.10"
        );
    } else if crate::scanner::is_source(selector) {
        crate::scanner::validate_selector_shape(selector)?;
    } else if crate::onvif::is_source(selector) {
        crate::onvif::validate_source_shape(selector)?;
    } else if crate::hubs::is_source(selector) {
        crate::hubs::validate_shape(selector)?;
    } else if crate::media::is_url_source(selector) || selector.contains("://") {
        crate::media::validate_source_shape(selector)?;
    } else if malformed_ipv4(selector) {
        return Err(crate::dataset::Usage(format!(
            "'{selector}' is not a valid IPv4 address; use A.B.C.D or gige:A.B.C.D"
        ))
        .into());
    }
    Ok(())
}

/// How a bare selector (no `sim:`, `gige:`, URL or other explicit form)
/// matched the discovered cameras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// One camera has this ID, serial or address.
    Exact(CameraInfo),
    /// One camera matched loosely: its model or the ID after the first ':',
    /// ignoring case, or whole words of its model.
    Loose {
        info: CameraInfo,
        matched_on: &'static str,
    },
    /// Several cameras matched; never guess among them.
    Ambiguous(Vec<CameraInfo>),
    NotFound,
}

/// Selectors whose syntax already says what they open; they are never
/// matched loosely against discovered cameras.
pub fn is_explicit_selector(selector: &str) -> bool {
    selector.starts_with("sim:")
        || selector.starts_with("gige:")
        || selector.parse::<Ipv4Addr>().is_ok()
        || selector.contains("://")
        || crate::scanner::is_source(selector)
        || crate::onvif::is_source(selector)
        || crate::hubs::is_source(selector)
        || crate::media::is_source(selector)
}

/// Resolve a bare selector against discovered cameras. Exact ID, serial or
/// address wins; otherwise a unique case-insensitive model or name match,
/// then a unique run of whole model words. Hub cameras keep their exact selectors only, so
/// a loose name can never reach a hub's permission checks under another
/// identity.
pub fn resolve_selector(selector: &str, known: &[CameraInfo]) -> Resolution {
    let exact: Vec<_> = known
        .iter()
        .filter(|d| {
            d.id == selector || d.serial == selector || d.address.as_deref() == Some(selector)
        })
        .cloned()
        .collect();
    match exact.len() {
        1 => return Resolution::Exact(exact.into_iter().next().unwrap()),
        0 => {}
        _ => return Resolution::Ambiguous(exact),
    }
    if is_explicit_selector(selector) || selector.trim().is_empty() {
        return Resolution::NotFound;
    }
    let wanted = selector.to_lowercase();
    let local: Vec<_> = known
        .iter()
        .filter(|d| !crate::hubs::is_source(&d.id))
        .collect();
    let suffix = |d: &CameraInfo| {
        d.id.split_once(':')
            .map_or(d.id.as_str(), |(_, rest)| rest)
            .to_lowercase()
    };
    let pick = |matches: Vec<(&CameraInfo, &'static str)>| match matches.as_slice() {
        [] => None,
        [(info, matched_on)] => Some(Resolution::Loose {
            info: (*info).clone(),
            matched_on,
        }),
        _ => Some(Resolution::Ambiguous(
            matches.iter().map(|(info, _)| (*info).clone()).collect(),
        )),
    };
    let equal = local
        .iter()
        .filter_map(|d| {
            if d.model.to_lowercase() == wanted {
                Some((*d, "model"))
            } else if suffix(d) == wanted
                || d.id.to_lowercase() == wanted
                || d.serial.to_lowercase() == wanted
            {
                Some((*d, "name"))
            } else {
                None
            }
        })
        .collect();
    if let Some(resolution) = pick(equal) {
        return resolution;
    }
    // Then whole words of a model (`C920` in `HD Pro Webcam C920`), never
    // part of a word, an ID or a serial: a partial or stale serial must not
    // open another camera. A scanner needs its model in full, because a
    // feeder consumes paper.
    let words = wanted.split_whitespace().collect::<Vec<_>>();
    if wanted.chars().count() < 3 || words.is_empty() {
        return Resolution::NotFound;
    }
    let partial = local
        .iter()
        .filter(|d| !crate::scanner::is_source(&d.id))
        .filter(|d| {
            let model = d.model.to_lowercase();
            let model = model.split_whitespace().collect::<Vec<_>>();
            model.windows(words.len()).any(|window| window == words)
        })
        .map(|d| (*d, "model"))
        .collect();
    pick(partial).unwrap_or(Resolution::NotFound)
}

/// A selector that names a local file rather than a camera.
fn path_like(selector: &str) -> bool {
    const MEDIA: [&str; 14] = [
        "mp4", "mov", "mkv", "avi", "webm", "ts", "m4v", "jpg", "jpeg", "png", "tif", "tiff",
        "bmp", "gif",
    ];
    selector.contains(['/', '\\'])
        || std::path::Path::new(selector)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| MEDIA.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Four dot-separated digit groups that are not a valid IPv4 address.
fn malformed_ipv4(selector: &str) -> bool {
    let parts: Vec<_> = selector.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        && selector.parse::<Ipv4Addr>().is_err()
}

/// `sim0`, `SIM0` or `SIM:0` written for the simulator `sim:0`.
fn simulator_typo(selector: &str) -> Option<String> {
    if selector.starts_with("sim:") {
        return None;
    }
    let lower = selector.to_ascii_lowercase();
    let rest = lower.strip_prefix("sim")?;
    let digits = rest.strip_prefix(':').unwrap_or(rest);
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("sim:{digits}"))
}

/// `Pattern camera`, the simulator's model, which discovery lists only
/// with `--simulate`.
fn simulator_model(selector: &str) -> Option<String> {
    selector
        .trim()
        .eq_ignore_ascii_case("pattern camera")
        .then(|| "sim:0".into())
}

/// Bare selectors discovery cannot help: a missing file path or a malformed
/// IPv4 address fails at once instead of after a scan.
pub fn needs_no_discovery(selector: &str) -> bool {
    path_like(selector) || malformed_ipv4(selector)
}

/// Why `selector` did not open, with the closest cameras that did answer.
fn selector_not_matched(
    selector: &str,
    known: &[CameraInfo],
    matches: &[CameraInfo],
) -> anyhow::Error {
    use crate::types::{Categorized, ErrorKind};
    let shown = crate::media::redact_url(selector);
    let unavailable = |message: String| Categorized::error(ErrorKind::Unavailable, message);
    if matches.len() > 1 {
        let listed = matches
            .iter()
            .take(5)
            .map(|d| format!("{} ({})", crate::media::redact_url(&d.id), d.model))
            .collect::<Vec<_>>()
            .join(", ");
        let more = if matches.len() > 5 { ", ..." } else { "" };
        return unavailable(format!(
            "camera '{shown}' is ambiguous: it matches {} cameras: {listed}{more}; pass a camera ID or serial",
            matches.len()
        ));
    }
    if let Some(simulator) = simulator_typo(selector).or_else(|| simulator_model(selector)) {
        return unavailable(format!(
            "camera '{shown}' not found; did you mean {simulator}?"
        ));
    }
    if path_like(selector) {
        return unavailable(format!("file not found: {shown}"));
    }
    let mut candidates: Vec<(usize, &CameraInfo)> = known
        .iter()
        .filter_map(|d| {
            let suffix = d.id.split_once(':').map_or(d.id.as_str(), |(_, rest)| rest);
            [d.id.as_str(), suffix, d.serial.as_str(), d.model.as_str()]
                .into_iter()
                .filter(|name| crate::hint::is_close(selector, name))
                .map(|name| crate::hint::edit_distance(selector, name))
                .min()
                .map(|distance| (distance, d))
        })
        .collect();
    candidates.sort_by_key(|(distance, _)| *distance);
    if let Some((_, best)) = candidates.first() {
        return unavailable(format!(
            "camera '{shown}' not found; did you mean {}? Run capturefab discover to list cameras",
            crate::hint::shell_quote(&crate::media::redact_url(&best.id))
        ));
    }
    if known.is_empty() {
        return unavailable(format!(
            "camera '{shown}' not found; run capturefab discover to list cameras"
        ));
    }
    let found = known
        .iter()
        .take(5)
        .map(|d| crate::media::redact_url(&d.id))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if known.len() > 5 { ", ..." } else { "" };
    unavailable(format!(
        "camera '{shown}' not found. Found: {found}{more}; run capturefab discover to list cameras"
    ))
}

/// Check both selector syntax and local source availability before opening.
pub fn validate_selector(selector: &str) -> Result<()> {
    validate_selector_shape(selector)?;
    if let Some(source) = crate::transport::simulator::replay_source(selector) {
        crate::transport::simulator::replay::source_files(source)?;
    } else if crate::scanner::is_source(selector) {
        crate::scanner::info(selector)?;
    } else if crate::onvif::is_source(selector) {
        crate::onvif::validate_source(selector)?;
    } else if crate::hubs::is_source(selector) {
        crate::hubs::validate(selector)?;
    } else if (crate::media::is_source(selector) || selector.contains("://"))
        && !crate::onvif::is_source(selector)
    {
        crate::media::validate_source(selector)?;
    }
    Ok(())
}
impl Camera {
    pub fn open(selector: &str, known: &[CameraInfo], timeout: Duration) -> Result<Self> {
        validate_selector(selector)?;
        if crate::onvif::is_source(selector) {
            let (info, backend) = crate::onvif::connect(selector, timeout)?;
            return Self::from_backend(info, backend);
        }
        if crate::hubs::is_source(selector) {
            let (info, backend) = crate::hubs::connect(selector, timeout)?;
            return Self::from_backend(info, backend);
        }
        let info = if selector.starts_with("sim:") {
            crate::transport::simulator::info_named(selector)
        } else if crate::scanner::is_source(selector) {
            known
                .iter()
                .find(|d| d.id == selector && d.transport == Transport::Scanner)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| crate::scanner::info(selector))?
        } else if crate::media::is_source(selector) {
            crate::media::info(selector)?
        } else if let Ok(ip) = selector
            .strip_prefix("gige:")
            .unwrap_or(selector)
            .parse::<Ipv4Addr>()
        {
            crate::transport::gige::probe(ip, timeout)?
        } else {
            match resolve_selector(selector, known) {
                Resolution::Exact(info) | Resolution::Loose { info, .. } => info,
                Resolution::Ambiguous(matches) => {
                    return Err(selector_not_matched(selector, known, &matches));
                }
                Resolution::NotFound => return Err(selector_not_matched(selector, known, &[])),
            }
        };
        let backend: Box<dyn Backend> = match info.transport {
            Transport::Simulator => crate::transport::simulator::open(&info.id, timeout)?,
            Transport::Scanner => crate::scanner::open(&info)?,
            Transport::Media => {
                if crate::onvif::is_source(&info.id) {
                    let (info, backend) = crate::onvif::connect(&info.id, timeout)?;
                    return Self::from_backend(info, backend);
                }
                // A hub camera opened by its serial keeps the options its
                // discovered address carries.
                if crate::hubs::is_source(&info.id) {
                    let address = info.address.as_deref().unwrap_or(&info.id);
                    let (info, backend) = crate::hubs::connect(address, timeout)?;
                    return Self::from_backend(info, backend);
                }
                if crate::media::is_source(selector) {
                    crate::media::open_url(selector, timeout)?
                } else {
                    crate::media::open(&info, timeout)?
                }
            }
            Transport::GigE => crate::transport::gige::open(&info, timeout)?,
            Transport::Usb3 => {
                #[cfg(feature = "usb")]
                {
                    crate::transport::usb::open(&info, timeout)?
                }
                #[cfg(not(feature = "usb"))]
                {
                    anyhow::bail!("USB support disabled in this build")
                }
            }
        };
        Self::from_backend(info, backend)
    }
    pub fn from_backend(mut info: CameraInfo, mut backend: Box<dyn Backend>) -> Result<Self> {
        let xml = backend.xml().context("load camera GenICam XML")?;
        let nodes = NodeMap::parse(&xml).context("parse camera GenICam XML")?;
        let mut profile_metadata = nodes.profile_features(backend.as_mut());
        info.profile = crate::camera_profile::detect(&info, &profile_metadata);
        // Devices may keep their identifying nodes outside Root or Invisible.
        // Preserve those declarations when normal feature snapshots omit them.
        profile_metadata.retain(|feature| {
            matches!(
                feature.name.as_str(),
                "DeviceType"
                    | "CameraType"
                    | "SensorType"
                    | "DeviceScanType"
                    | "PixelFormat"
                    | "ComponentSelector"
                    | "Scan3dOutputMode"
            )
        });
        Ok(Self {
            info,
            backend,
            nodes,
            streaming: false,
            xml,
            profile_metadata,
            network_budget_bytes_per_second: None,
        })
    }
    pub fn features(&mut self) -> Vec<FeatureInfo> {
        let mut features = self.nodes.features(self.backend.as_mut());
        let visible_count = features.len();
        for declaration in &self.profile_metadata {
            if !features
                .iter()
                .any(|feature| feature.name == declaration.name)
            {
                features.push(declaration.clone());
            }
        }
        self.info.profile = crate::camera_profile::detect(&self.info, &features);
        features.truncate(visible_count);
        features
    }
    pub fn get(&mut self, name: &str) -> Result<serde_json::Value> {
        self.nodes.get(self.backend.as_mut(), name)
    }
    pub(crate) fn selectors(&self, name: &str) -> Vec<String> {
        self.nodes.selectors(name)
    }
    pub(crate) fn is_configuration_value(&mut self, name: &str) -> bool {
        self.nodes
            .is_configuration_value(self.backend.as_mut(), name)
    }
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        self.nodes.set(self.backend.as_mut(), name, value)
    }
    pub fn validate_set(&mut self, name: &str, value: &str) -> Result<()> {
        self.nodes.validate_set(self.backend.as_mut(), name, value)
    }
    pub fn has(&self, name: &str) -> bool {
        self.nodes.has(name)
    }
    pub(crate) fn feature_unit(&self, name: &str) -> Option<&str> {
        self.nodes.unit(name)
    }
    pub fn is_writable(&mut self, name: &str) -> bool {
        self.nodes.is_writable(self.backend.as_mut(), name)
    }
    pub fn bounds(&mut self, name: &str) -> Result<Bounds> {
        self.nodes.bounds(self.backend.as_mut(), name)
    }
    pub fn choices(&mut self, name: &str) -> Result<Vec<String>> {
        self.nodes.choices(self.backend.as_mut(), name)
    }
    pub fn stats(&self) -> Option<TransportStats> {
        self.backend.stats()
    }
    pub fn network_budget_bytes_per_second(&self) -> Option<u64> {
        self.network_budget_bytes_per_second
    }
    pub(crate) fn set_network_budget_bytes_per_second(&mut self, budget: u64) {
        self.network_budget_bytes_per_second = Some(budget.max(1));
    }
    /// Apply measured GigE defaults without interfering with an acquisition.
    pub fn optimize_network(&mut self) -> Result<crate::network::NetworkTuning> {
        ensure!(
            !self.streaming,
            "stop acquisition before optimizing the camera network"
        );
        let size = self
            .get("PayloadSize")?
            .as_u64()
            .context("PayloadSize must be a positive integer")?;
        ensure!(
            size > 0 && size <= 256 * 1024 * 1024,
            "camera payload exceeds 256 MiB limit"
        );
        self.backend.optimize_network(size as usize)
    }
    pub fn execute(&mut self, name: &str) -> Result<()> {
        ensure!(
            !["AcquisitionStart", "AcquisitionStop"].contains(&name),
            "use start/stop to manage transport and acquisition together"
        );
        self.nodes.execute(self.backend.as_mut(), name)
    }
    pub fn start(&mut self) -> Result<()> {
        if self.streaming {
            return Ok(());
        }
        let size = self
            .get("PayloadSize")?
            .as_u64()
            .context("PayloadSize must be a positive integer")?;
        ensure!(
            size > 0 && size <= 256 * 1024 * 1024,
            "camera payload exceeds 256 MiB limit"
        );
        self.backend.start(size as usize)?;
        if let Err(e) = self
            .nodes
            .execute(self.backend.as_mut(), "AcquisitionStart")
        {
            let _ = self.backend.stop();
            return Err(e);
        }
        self.streaming = true;
        Ok(())
    }
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        ensure!(self.streaming, "camera is not streaming");
        self.backend.next_frame(timeout)
    }
    pub fn request_frame(&mut self) -> Result<()> {
        ensure!(self.streaming, "camera is not streaming");
        self.backend.request_frame()
    }
    pub fn is_scanner(&self) -> bool {
        self.info.transport == Transport::Scanner
    }
    pub fn stop(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        let acquisition = self.nodes.execute(self.backend.as_mut(), "AcquisitionStop");
        let transport = self.backend.stop();
        self.streaming = false;
        acquisition?;
        transport
    }
    pub fn xml(&self) -> &str {
        &self.xml
    }
    pub fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.backend.read_memory(address, length)
    }
    pub fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.backend.write_memory(address, data)
    }
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }
}
impl Drop for Camera {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_profile::{CameraCapability, CameraKind, DetectionSource};

    #[test]
    fn typed_simulator_serial_preserves_its_backend() {
        let info = crate::transport::simulator::info_named("sim:depth:alias");
        let mut camera = Camera::open(
            &info.serial,
            std::slice::from_ref(&info),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(camera.info.id, "sim:depth:alias");
        assert_eq!(camera.info.profile.kind, CameraKind::Depth);
        assert_eq!(camera.get("PixelFormat").unwrap(), "Coord3D_C16");
    }

    #[test]
    fn hidden_type_metadata_survives_refresh_without_extra_feature_polling() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct MetadataBackend(Arc<AtomicUsize>);
        impl crate::types::RegisterIo for MetadataBackend {
            fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
                self.0.fetch_add(1, Ordering::Relaxed);
                if address == 0 {
                    let mut value = b"Linescan".to_vec();
                    value.resize(length, 0);
                    Ok(value)
                } else {
                    Ok(vec![0; length])
                }
            }
            fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
                Ok(())
            }
        }
        impl Backend for MetadataBackend {
            fn xml(&mut self) -> Result<String> {
                Ok(r#"<RegisterDescription>
                    <Category Name="Root"><pFeature>ExposureTime</pFeature><pFeature>Width</pFeature><pFeature>Height</pFeature><pFeature>AcquisitionFrameRate</pFeature></Category>
                    <StringReg Name="DeviceScanType"><Address>0</Address><Length>32</Length><AccessMode>RO</AccessMode><Visibility>Invisible</Visibility></StringReg>
                    <FloatReg Name="ExposureTime"><Address>64</Address><Length>8</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></FloatReg>
                    <Integer Name="Width"><Value>640</Value></Integer>
                    <Integer Name="Height"><Value>1</Value></Integer>
                    <Float Name="AcquisitionFrameRate"><Value>30</Value></Float>
                    </RegisterDescription>"#.into())
            }
            fn start(&mut self, _: usize) -> Result<()> {
                Ok(())
            }
            fn next_frame(&mut self, _: Duration) -> Result<Frame> {
                anyhow::bail!("no frames")
            }
            fn stop(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let reads = Arc::new(AtomicUsize::new(0));
        let info = crate::transport::simulator::info();
        let mut camera =
            Camera::from_backend(info, Box::new(MetadataBackend(reads.clone()))).unwrap();
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "opening must not poll unrelated exposure registers"
        );
        assert_eq!(camera.info.profile.kind, CameraKind::LineScan);
        assert!(camera.info.profile.supports(CameraCapability::Exposure));
        assert!(!camera.features().iter().any(|f| f.name == "DeviceScanType"));
        assert_eq!(camera.info.profile.kind, CameraKind::LineScan);
        assert_eq!(camera.info.profile.source, DetectionSource::DeviceMetadata);
    }

    fn device(id: &str, model: &str, serial: &str) -> CameraInfo {
        let mut info = crate::transport::simulator::info();
        info.id = id.into();
        info.model = model.into();
        info.serial = serial.into();
        info.address = Some(id.into());
        info
    }
    fn known() -> Vec<CameraInfo> {
        vec![
            device(
                "avfoundation:HD Pro Webcam C920",
                "HD Pro Webcam C920",
                "avfoundation:HD Pro Webcam C920",
            ),
            device(
                "avfoundation:Capture screen 0",
                "Capture screen 0",
                "avfoundation:Capture screen 0",
            ),
            device("gige:02007f000001", "Virtual GigE camera", "SIM7F000001"),
            device(
                "hub:home/camera.front_door",
                "Front door",
                "home/camera.front_door",
            ),
        ]
    }
    fn open_error(selector: &str, known: &[CameraInfo]) -> anyhow::Error {
        match Camera::open(selector, known, Duration::from_millis(1)) {
            Ok(_) => panic!("{selector} opened"),
            Err(error) => error,
        }
    }

    #[test]
    fn exact_selectors_win_over_loose_matches() {
        let mut known = known();
        known.push(device("usb:C920", "Other", "C920"));
        let Resolution::Exact(info) = resolve_selector("C920", &known) else {
            panic!("serial must match exactly");
        };
        assert_eq!(info.id, "usb:C920");
    }

    #[test]
    fn unique_loose_names_resolve_and_several_are_ambiguous() {
        let known = known();
        for (selector, id, on) in [
            (
                "hd pro webcam c920",
                "avfoundation:HD Pro Webcam C920",
                "model",
            ),
            ("C920", "avfoundation:HD Pro Webcam C920", "model"),
            ("virtual gige", "gige:02007f000001", "model"),
            ("02007F000001", "gige:02007f000001", "name"),
        ] {
            let Resolution::Loose { info, matched_on } = resolve_selector(selector, &known) else {
                panic!("{selector} did not match loosely");
            };
            assert_eq!((info.id.as_str(), matched_on), (id, on), "{selector}");
        }
        let mut known = known;
        known.push(device(
            "avfoundation:HD Pro Webcam C922",
            "HD Pro Webcam C922",
            "avfoundation:HD Pro Webcam C922",
        ));
        let Resolution::Ambiguous(matches) = resolve_selector("pro webcam", &known) else {
            panic!("two cameras share the name");
        };
        assert_eq!(matches.len(), 2);
        let error = open_error("Pro Webcam", &known);
        let message = error.to_string();
        assert!(message.contains("ambiguous"), "{message}");
        assert!(message.contains("C922"), "{message}");
        assert!(message.contains("HD Pro Webcam C920"), "{message}");
        assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
    }

    #[test]
    fn partial_serials_ids_and_words_never_match() {
        let mut known = known();
        known.push(device("usb:2676:ba02:23015", "Industrial camera", "23015"));
        known.push(device(
            "scanner:ica:E3248000-80CE-11DB-8000-3C2AF4FA0DE9",
            "Brother MFC-L2750DW series",
            "E3248000-80CE-11DB-8000-3C2AF4FA0DE9",
        ));
        for selector in [
            "2301",
            "ba02:2301",
            "E3248",
            "e3248000",
            "cam",
            "webca",
            "Brother",
        ] {
            assert_eq!(
                resolve_selector(selector, &known),
                Resolution::NotFound,
                "{selector}"
            );
        }
        assert!(matches!(
            resolve_selector("brother mfc-l2750dw series", &known),
            Resolution::Loose { info, .. } if info.id.starts_with("scanner:")
        ));
    }

    #[test]
    fn hub_cameras_are_never_matched_loosely() {
        let known = known();
        assert_eq!(resolve_selector("front door", &known), Resolution::NotFound);
        assert!(matches!(
            resolve_selector("home/camera.front_door", &known),
            Resolution::Exact(info) if info.id.starts_with("hub:")
        ));
    }

    #[test]
    fn unmatched_selectors_say_what_was_meant() {
        let known = known();
        let cases = [
            ("sim0", "did you mean sim:0?"),
            ("SIM:2", "did you mean sim:2?"),
            ("./nope.mp4", "file not found: ./nope.mp4"),
            ("nope.mp4", "file not found: nope.mp4"),
            ("SIM7F00001", "did you mean gige:02007f000001?"),
            ("Pattern camera", "did you mean sim:0?"),
            ("SIM0", "did you mean sim:0?"),
            ("zzzz", "Found: avfoundation:HD Pro Webcam C920"),
        ];
        for (selector, expected) in cases {
            let error = open_error(selector, &known);
            assert!(
                error.to_string().contains(expected),
                "{selector}: {error:#}"
            );
            assert_eq!(
                crate::cli::error_code(&error),
                ("unavailable", 3),
                "{selector}"
            );
        }
        let error = open_error("zzzz", &[]);
        assert!(
            error.to_string().contains("run capturefab discover"),
            "{error:#}"
        );
        assert!(needs_no_discovery("./nope.mp4"));
        assert!(needs_no_discovery("192.168.1.300"));
        assert!(!needs_no_discovery("C920"));
    }

    #[test]
    fn suggestions_never_show_credentials() {
        let known = vec![device(
            "rtsp://admin:secret@10.0.0.5/stream",
            "Door",
            "rtsp://admin:secret@10.0.0.5/stream",
        )];
        let message = selector_not_matched("dor", &known, &[]).to_string();
        assert!(!message.contains("secret"), "{message}");
        let message =
            selector_not_matched("x", &known, &[known[0].clone(), known[0].clone()]).to_string();
        assert!(!message.contains("secret"), "{message}");
    }

    #[test]
    fn malformed_ipv4_is_a_usage_error() {
        let error = validate_selector_shape("192.168.1.300").unwrap_err();
        assert!(
            error.to_string().contains("not a valid IPv4 address"),
            "{error:#}"
        );
        assert_eq!(crate::cli::error_code(&error), ("usage", 2));
        validate_selector_shape("192.168.1.30").unwrap();
    }

    #[test]
    fn malformed_selectors_fail_before_opening_a_backend() {
        for selector in [
            "",
            "  ",
            "sim:",
            "sim:   ",
            "sim:a\0b",
            "sim:a\nb",
            "gige:",
            "gige:not-an-ip",
            "gige:gige:127.0.0.1",
            "scanner:unknown:device",
        ] {
            assert!(validate_selector(selector).is_err(), "{selector:?}");
            assert!(Camera::open(selector, &[], Duration::from_millis(1)).is_err());
        }
        assert!(Camera::open("sim:named camera", &[], Duration::from_millis(1)).is_ok());
        for selector in ["gige:00305311aabb", "gige:00305311AABB", "gige:192.0.2.1"] {
            validate_selector_shape(selector).unwrap();
        }
        for selector in [
            "gige:00305311aab",
            "gige:00305311aabbc",
            "gige:00305311aabg",
        ] {
            assert!(validate_selector_shape(selector).is_err(), "{selector:?}");
        }
    }

    #[test]
    fn selector_shape_does_not_depend_on_local_availability() {
        let source = format!(
            "file:/private/tmp/capturefab-missing-{}-{}.mp4",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        validate_selector_shape(&source).unwrap();
        assert!(validate_selector(&source).is_err());
        for source in [
            "scanner:sane:device",
            "scanner:wia:device",
            "scanner:ica:device",
        ] {
            validate_selector_shape(source).unwrap();
        }
    }

    #[test]
    #[ignore = "requires FFmpeg"]
    fn discovered_media_opens_by_id_and_serial() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "capturefab-camera-{}-{nonce}.ppm",
            std::process::id()
        ));
        let mut ppm = b"P6\n16 16\n255\n".to_vec();
        ppm.extend(vec![127; 16 * 16 * 3]);
        std::fs::write(&path, ppm).unwrap();
        let info = crate::media::info(path.to_str().unwrap()).unwrap();
        for selector in [&info.id, &info.serial] {
            let mut camera = Camera::open(
                selector,
                std::slice::from_ref(&info),
                Duration::from_secs(3),
            )
            .unwrap();
            camera.start().unwrap();
            let frame = camera.next_frame(Duration::from_secs(3)).unwrap();
            assert_eq!((frame.width, frame.height), (16, 16));
            assert_eq!(frame.data, vec![127; 16 * 16 * 3]);
            camera.stop().unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn discovered_onvif_serial_uses_onvif_transport() {
        let info = CameraInfo {
            id: "onvif:https://camera/service".into(),
            transport: Transport::Media,
            vendor: "ONVIF".into(),
            model: "Network camera".into(),
            serial: "network-serial".into(),
            address: Some("https://camera/service".into()),
            profile: Default::default(),
        };
        let error = Camera::open(
            &info.serial,
            std::slice::from_ref(&info),
            Duration::from_millis(100),
        )
        .err()
        .expect("HTTPS SOAP should be rejected before any network request");
        assert!(error.to_string().contains("HTTPS ONVIF SOAP"), "{error:#}");
    }
}
