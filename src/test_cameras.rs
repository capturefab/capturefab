//! Small, unauthenticated network cameras for unit tests of the GigE Vision
//! transport and the ONVIF client: GigE uses GVCP/GVSP and serves frames from
//! a source camera; ONVIF answers device, media and WS-Discovery requests.
mod gige;
mod onvif;

use crate::{camera::Camera, types::Frame};
use anyhow::{Context, Result};
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
pub(crate) struct ServerOptions {
    pub bind: Ipv4Addr,
    pub advertise: Ipv4Addr,
    pub gige_port: Option<u16>,
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

/// A GigE Vision camera serving frames from a source camera.
pub(crate) struct VirtualCameraServer {
    shutdown: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
    camera: Arc<Mutex<Camera>>,
    gige: Option<gige::GigEServer>,
}
impl VirtualCameraServer {
    pub(crate) fn start(camera: Camera, options: ServerOptions) -> Result<Self> {
        let port = options.gige_port.context("enable GigE serving")?;
        let camera = Arc::new(Mutex::new(camera));
        let frames = Arc::new(LatestFrame::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        // AcquisitionStart from the GigE client starts the source.
        let acquisition = Arc::new(AtomicBool::new(false));
        let mut server = Self {
            shutdown: shutdown.clone(),
            producer: None,
            camera: camera.clone(),
            gige: None,
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
                            Err(error) if polling_timeout(&error) => {}
                            Err(_) => break,
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
        let gige = gige::GigEServer::start(
            &options,
            port,
            server.camera.clone(),
            frames,
            acquisition,
            shutdown,
        )?;
        server.gige = Some(gige);
        Ok(server)
    }
    /// The GVCP control port.
    pub(crate) fn port(&self) -> u16 {
        self.gige.as_ref().map_or(0, gige::GigEServer::port)
    }
}
impl Drop for VirtualCameraServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
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
}
