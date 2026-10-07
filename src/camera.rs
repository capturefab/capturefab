use crate::{
    genicam::{Bounds, FeatureInfo, NodeMap},
    types::{Backend, CameraInfo, Frame, Transport, TransportStats},
};
use anyhow::{Context, Result, ensure};
use std::{net::Ipv4Addr, time::Duration};

pub fn discover(timeout: Duration, simulated: bool) -> Result<(Vec<CameraInfo>, Vec<String>)> {
    let mut devices = Vec::new();
    let mut warnings = Vec::new();
    let (gige, onvif, native) = std::thread::scope(|s| {
        let g = s.spawn(|| crate::transport::gige::discover(timeout));
        let o = s.spawn(|| crate::onvif::camera_infos(timeout));
        let n = s.spawn(|| crate::media::native_devices_with_timeout(timeout));
        (g.join(), o.join(), n.join())
    });
    for (label, result) in [("GigE", gige), ("ONVIF", onvif)] {
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
    Ok((devices, warnings))
}

pub struct Camera {
    pub info: CameraInfo,
    backend: Box<dyn Backend>,
    nodes: NodeMap,
    streaming: bool,
    xml: String,
}
impl Camera {
    pub fn open(selector: &str, known: &[CameraInfo], timeout: Duration) -> Result<Self> {
        if crate::onvif::is_source(selector) {
            let (info, backend) = crate::onvif::connect(selector, timeout)?;
            return Self::from_backend(info, backend);
        }
        let info = if selector.starts_with("sim:") {
            crate::transport::simulator::info_named(selector)
        } else if crate::media::is_source(selector) {
            crate::media::info(selector)?
        } else if let Ok(ip) = selector.trim_start_matches("gige:").parse::<Ipv4Addr>() {
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
            Transport::Simulator => Box::new(crate::transport::simulator::Simulator::default()),
            Transport::Media => {
                if crate::onvif::is_source(&info.id) {
                    let (info, backend) = crate::onvif::connect(&info.id, timeout)?;
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
    pub fn from_backend(info: CameraInfo, mut backend: Box<dyn Backend>) -> Result<Self> {
        let xml = backend.xml().context("load camera GenICam XML")?;
        let nodes = NodeMap::parse(&xml).context("parse camera GenICam XML")?;
        Ok(Self {
            info,
            backend,
            nodes,
            streaming: false,
            xml,
        })
    }
    pub fn features(&mut self) -> Vec<FeatureInfo> {
        self.nodes.features(self.backend.as_mut())
    }
    pub fn get(&mut self, name: &str) -> Result<serde_json::Value> {
        self.nodes.get(self.backend.as_mut(), name)
    }
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        self.nodes.set(self.backend.as_mut(), name, value)
    }
    pub fn has(&self, name: &str) -> bool {
        self.nodes.has(name)
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
