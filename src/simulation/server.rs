//! Small, unauthenticated network cameras for integration tests on a trusted lab network.
//! GigE uses GVCP/GVSP; RTSP and ONVIF expose the same replay/pattern source.
mod gige;
mod onvif;
mod rtsp;

use crate::{camera::Camera, types::Frame};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    net::Ipv4Addr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub bind: Ipv4Addr,
    pub advertise: Ipv4Addr,
    pub gige_port: Option<u16>,
    pub rtsp_port: Option<u16>,
    pub onvif_port: Option<u16>,
    pub name: String,
    pub fps: f64,
    pub bitrate: String,
}

/// A bounded mailbox: slow consumers skip frames instead of accumulating test data.
#[derive(Default)]
pub(crate) struct LatestFrame {
    value: Mutex<(u64, Option<Arc<Frame>>)>,
    wake: Condvar,
}
impl LatestFrame {
    fn publish(&self, frame: Frame) {
        let mut value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        value.0 = value.0.wrapping_add(1).max(1);
        value.1 = Some(Arc::new(frame));
        self.wake.notify_all();
    }
    pub(crate) fn wait_after(&self, sequence: u64, timeout: Duration) -> Option<(u64, Arc<Frame>)> {
        let value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        let (value, _) = self
            .wake
            .wait_timeout_while(value, timeout, |value| {
                value.0 == sequence || value.1.is_none()
            })
            .unwrap_or_else(|e| e.into_inner());
        if value.0 == sequence {
            return None;
        }
        value.1.as_ref().map(|frame| (value.0, frame.clone()))
    }
}

pub struct VirtualCameraServer {
    shutdown: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
    camera: Arc<Mutex<Camera>>,
    gige: Option<gige::GigEServer>,
    rtsp: Option<rtsp::RtspServer>,
    onvif: Option<onvif::OnvifServer>,
    endpoints: Value,
    failure: Arc<Mutex<Option<String>>>,
}
impl VirtualCameraServer {
    pub fn start(mut camera: Camera, options: ServerOptions) -> Result<Self> {
        ensure!(
            options.fps.is_finite() && options.fps >= 1.0 && options.fps <= 240.0,
            "virtual camera rate must be 1..=240 Hz"
        );
        ensure!(
            !options.advertise.is_unspecified()
                && !options.advertise.is_multicast()
                && options.advertise != Ipv4Addr::BROADCAST,
            "virtual camera advertised address must be a unicast IPv4 address"
        );
        ensure!(
            !options.name.is_empty()
                && options.name.len() <= 128
                && !options.name.chars().any(char::is_control),
            "virtual camera name must be 1..=128 printable characters"
        );
        ensure!(
            options.gige_port.is_some() || options.rtsp_port.is_some(),
            "enable GigE or RTSP serving"
        );
        ensure!(
            options.onvif_port.is_none() || options.rtsp_port.is_some(),
            "ONVIF simulation requires RTSP"
        );
        let width = camera
            .get("Width")?
            .as_u64()
            .context("source Width must be an integer")? as u32;
        let height = camera
            .get("Height")?
            .as_u64()
            .context("source Height must be an integer")? as u32;
        let active = options.rtsp_port.is_some();
        if active {
            camera.start()?;
        }
        let camera = Arc::new(Mutex::new(camera));
        let frames = Arc::new(LatestFrame::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let acquisition = Arc::new(AtomicBool::new(active));
        let failure = Arc::new(Mutex::new(None));
        let mut server = Self {
            shutdown: shutdown.clone(),
            producer: None,
            camera: camera.clone(),
            gige: None,
            rtsp: None,
            onvif: None,
            endpoints: json!({}),
            failure: failure.clone(),
        };
        let source_frames = frames.clone();
        let source_shutdown = shutdown.clone();
        let source_acquisition = acquisition.clone();
        server.producer = Some(
            thread::Builder::new()
                .name("capturefab-virtual-source".into())
                .spawn(move || {
                    while !source_shutdown.load(Ordering::Relaxed) {
                        if !source_acquisition.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        let result = {
                            let mut source = camera.lock().unwrap_or_else(|e| e.into_inner());
                            if !source_acquisition.load(Ordering::Relaxed) || !source.is_streaming()
                            {
                                continue;
                            }
                            source.next_frame(Duration::from_millis(100))
                        };
                        match result {
                            Ok(frame) => source_frames.publish(frame),
                            Err(error) => {
                                let message = format!("{error:#}");
                                if !polling_timeout(&error) {
                                    *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(message);
                                    break;
                                }
                            }
                        }
                        // Give control requests a chance to acquire the source
                        // mutex. A yield is not enough: std's mutex is unfair,
                        // and on Linux the producer takes the lock straight
                        // back, starving a stop request for seconds.
                        thread::sleep(Duration::from_millis(1));
                    }
                })
                .context("starting virtual camera source")?,
        );
        if let Some(port) = options.gige_port {
            let gige = gige::GigEServer::start(
                &options,
                port,
                server.camera.clone(),
                frames.clone(),
                acquisition,
                shutdown.clone(),
            )?;
            server.endpoints["gige"] = json!({"address":options.advertise,"port":gige.port()});
            if gige.port() == 3956 {
                server.endpoints["gige"]["selector"] = json!(format!("gige:{}", options.advertise));
            } else {
                server.endpoints["gige"]["note"] = json!(
                    "A custom control port requires a client that supports it. Standard GigE discovery and Capturefab selectors use port 3956."
                );
            }
            server.gige = Some(gige);
        }
        if let Some(port) = options.rtsp_port {
            let rtsp = rtsp::RtspServer::start(
                options.bind,
                port,
                width,
                height,
                options.fps,
                &options.bitrate,
                frames.clone(),
                shutdown.clone(),
            )?;
            let uri = format!(
                "rtsp://{}:{}/live",
                options.advertise,
                rtsp.local_addr().port()
            );
            server.endpoints["rtsp"] = json!(uri);
            server.rtsp = Some(rtsp);
            if let Some(port) = options.onvif_port {
                let onvif = onvif::OnvifServer::start(
                    &options, port, width, height, uri, frames, shutdown,
                )?;
                server.endpoints["onvif"] = json!(onvif.endpoint());
                server.endpoints["snapshot"] = json!(onvif.snapshot());
                server.endpoints["ws_discovery"] = json!(onvif.discovery_enabled());
                server.onvif = Some(onvif);
            }
        }
        server.endpoints["name"] = json!(options.name);
        server.endpoints["authentication"] = json!("none (test camera)");
        Ok(server)
    }
    pub fn endpoints(&self) -> Value {
        self.endpoints.clone()
    }
    pub fn check(&self) -> Result<()> {
        if let Some(error) = self.rtsp.as_ref().and_then(|server| server.error()) {
            anyhow::bail!("virtual camera encoder failed: {error}");
        }
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            anyhow::bail!("virtual camera source failed: {error}");
        }
        Ok(())
    }
}
impl Drop for VirtualCameraServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.onvif.take();
        self.rtsp.take();
        self.gige.take();
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
        let _ = self.camera.lock().unwrap_or_else(|e| e.into_inner()).stop();
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn polling_timeout(error: &anyhow::Error) -> bool {
    let message = error.root_cause().to_string();
    matches!(
        message.as_str(),
        "frame timeout"
            | "simulation frame capture timed out"
            | "simulation frame capture timed out before the next scheduled frame"
    ) || message.starts_with("simulation media frame timed out:")
        || message.starts_with("media frame capture timed out:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_source_names_and_decoder_failures_are_not_polling_timeouts() {
        assert!(polling_timeout(&anyhow::anyhow!("frame timeout")));
        assert!(polling_timeout(&anyhow::anyhow!(
            "simulation media frame timed out: waiting for frame"
        )));
        assert!(!polling_timeout(&anyhow::anyhow!(
            "simulation media decoder exited: corrupt video /tmp/timeout.mp4"
        )));
        assert!(!polling_timeout(&anyhow::anyhow!(
            "simulation media decoder did not exit before the frame timeout"
        )));
    }

    #[test]
    #[ignore = "requires FFmpeg with libx264 and local loopback sockets"]
    fn onvif_virtual_camera_opens_and_decodes_through_real_media_transport() {
        let mut source = Camera::open("sim:0", &[], Duration::from_secs(1)).unwrap();
        source.set("Width", "64").unwrap();
        source.set("Height", "64").unwrap();
        source.set("PixelFormat", "RGB8").unwrap();
        let server = VirtualCameraServer::start(
            source,
            ServerOptions {
                bind: Ipv4Addr::LOCALHOST,
                advertise: Ipv4Addr::LOCALHOST,
                gige_port: None,
                rtsp_port: Some(0),
                onvif_port: Some(0),
                name: "ONVIF replay integration".into(),
                fps: 30.0,
                bitrate: "1M".into(),
            },
        )
        .unwrap();
        server.check().unwrap();
        let selector = format!("onvif:{}", server.endpoints()["onvif"].as_str().unwrap());
        let mut camera = Camera::open(&selector, &[], Duration::from_secs(5)).unwrap();
        camera.start().unwrap();
        let frame = camera.next_frame(Duration::from_secs(5)).unwrap();
        assert_eq!(
            (frame.width, frame.height, frame.pixel_format),
            (64, 64, crate::types::RGB8)
        );
        assert_eq!(frame.data.len(), 64 * 64 * 3);
        assert!(frame.data.iter().any(|value| *value > 32));
        camera.stop().unwrap();
        server.check().unwrap();
    }
}
