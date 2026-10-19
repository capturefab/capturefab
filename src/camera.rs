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
    } else if let Some(ip) = selector.strip_prefix("gige:") {
        ip.parse::<Ipv4Addr>()
            .context("GigE selector needs an IPv4 address, for example gige:192.168.1.10")?;
    } else if crate::scanner::is_source(selector) {
        crate::scanner::validate_selector_shape(selector)?;
    } else if crate::onvif::is_source(selector) {
        crate::onvif::validate_source_shape(selector)?;
    } else if crate::hubs::is_source(selector) {
        crate::hubs::validate_shape(selector)?;
    } else if crate::media::is_url_source(selector) || selector.contains("://") {
        crate::media::validate_source_shape(selector)?;
    }
    Ok(())
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
            let matches: Vec<_> = known
                .iter()
                .filter(|d| d.id == selector || d.serial == selector)
                .collect();
            ensure!(
                matches.len() == 1,
                "camera '{selector}' not found or ambiguous; run capturefab discover"
            );
            matches[0].clone()
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
