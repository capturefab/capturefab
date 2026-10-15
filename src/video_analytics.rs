//! Explicitly started, bounded sampling of a live camera for numeric measurements.
//! HTTPS, credential reads and database writes run outside acquisition and the UI.
use crate::{
    ai::{MeasurementDefinition, MeasurementOutput, ModelConfig},
    analytics::MeasurementSample,
    session::SessionHandle,
    types::Frame,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, Thread},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const MAX_MEASUREMENTS: usize = 16;
pub const MIN_INTERVAL_SECONDS: u64 = 5;
pub const MAX_INTERVAL_SECONDS: u64 = 3_600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoAnalyticsConfig {
    pub camera_id: String,
    pub model: ModelConfig,
    pub measurements: Vec<MeasurementDefinition>,
    pub interval_seconds: u64,
}

impl VideoAnalyticsConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.camera_id.trim().is_empty() && self.camera_id.len() <= 512,
            "Select a camera for video analytics."
        );
        self.model.validate()?;
        ensure!(
            (MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&self.interval_seconds),
            "Sampling interval must be 5..3600 seconds."
        );
        ensure!(
            (1..=MAX_MEASUREMENTS).contains(&self.measurements.len()),
            "Configure 1..16 measurements."
        );
        let mut ids = BTreeSet::new();
        for measurement in &self.measurements {
            measurement.validate()?;
            ensure!(
                ids.insert(&measurement.id),
                "Measurement IDs must be unique."
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct VideoAnalyticsStatus {
    pub running: bool,
    /// A stopped request may still finish, but its response is discarded.
    pub busy: bool,
    pub completed_samples: u64,
    pub error: Option<String>,
}

struct Active {
    cancelled: Arc<AtomicBool>,
    thread: Option<Thread>,
}

#[derive(Default)]
struct Shared {
    status: VideoAnalyticsStatus,
    active: Option<Active>,
}

pub struct VideoAnalytics {
    handle: SessionHandle,
    shared: Arc<Mutex<Shared>>,
}

impl VideoAnalytics {
    pub fn new(handle: SessionHandle) -> Self {
        Self {
            handle,
            shared: Arc::default(),
        }
    }

    pub fn status(&self) -> VideoAnalyticsStatus {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    pub fn start(&self, config: VideoAnalyticsConfig) -> Result<()> {
        config.validate()?;
        let snapshot = self.handle.snapshot();
        ensure!(
            snapshot
                .cameras
                .iter()
                .any(|camera| { camera.info.id == config.camera_id && camera.streaming }),
            "Start the selected camera's live stream before collecting measurements."
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            shared.active.is_none(),
            "The previous analytics worker is still finishing. Try again shortly."
        );
        shared.status.running = true;
        shared.status.error = None;
        shared.active = Some(Active {
            cancelled: cancelled.clone(),
            thread: None,
        });
        let state = self.shared.clone();
        let handle = self.handle.clone();
        let worker_cancelled = cancelled.clone();
        let spawned = thread::Builder::new()
            .name("capturefab-video-analytics".into())
            .spawn(move || {
                let outcome = worker_loop(
                    &state,
                    &worker_cancelled,
                    &config,
                    || {
                        ensure!(handle.is_running(), "The camera session has closed.");
                        let snapshot = handle.snapshot();
                        let camera = snapshot
                            .cameras
                            .iter()
                            .find(|camera| camera.info.id == config.camera_id)
                            .context("The analytics camera was disconnected.")?;
                        ensure!(
                            camera.streaming,
                            "The analytics camera's stream has stopped."
                        );
                        Ok(handle
                            .latest_frame_for(&config.camera_id)
                            .map(|frame| (camera.worker_pid, frame)))
                    },
                    crate::ai::analyze_frame,
                    |samples| handle.analytics().record_measurements(samples),
                    Duration::from_secs(config.interval_seconds),
                );
                finish(&state, &worker_cancelled, outcome);
            });
        match spawned {
            Ok(worker) => {
                if let Some(active) = &mut shared.active {
                    active.thread = Some(worker.thread().clone());
                }
                Ok(())
            }
            Err(error) => {
                shared.active = None;
                shared.status.running = false;
                shared.status.error = Some(format!("Could not start video analytics: {error}"));
                Err(error).context("Could not start video analytics")
            }
        }
    }

    /// Immediately prevents additional requests. Never waits for HTTPS to finish.
    pub fn stop(&self) {
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.status.running = false;
        if let Some(active) = &shared.active {
            active.cancelled.store(true, Ordering::Release);
            if let Some(thread) = &active.thread {
                thread.unpark();
            }
        }
    }
}

impl Drop for VideoAnalytics {
    fn drop(&mut self) {
        self.stop();
    }
}

type FrameSource = Option<(u32, Arc<Frame>)>;

fn worker_loop(
    shared: &Mutex<Shared>,
    cancelled: &AtomicBool,
    config: &VideoAnalyticsConfig,
    mut source: impl FnMut() -> Result<FrameSource>,
    mut analyze: impl FnMut(
        &ModelConfig,
        &[MeasurementDefinition],
        &Frame,
    ) -> Result<Vec<MeasurementOutput>>,
    mut record: impl FnMut(&[MeasurementSample]) -> Result<()>,
    interval: Duration,
) -> Result<()> {
    let mut previous = None;
    while !cancelled.load(Ordering::Acquire) {
        let Some((worker_pid, frame)) = source()? else {
            thread::park_timeout(Duration::from_millis(250));
            continue;
        };
        let identity = (worker_pid, frame.id, frame.timestamp_ns);
        if previous == Some(identity) {
            thread::park_timeout(Duration::from_millis(250));
            continue;
        }
        previous = Some(identity);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        {
            let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            shared.status.busy = true;
        }
        let outputs = analyze(&config.model, &config.measurements, &frame);
        // Cancellation takes precedence even if a stopped request failed.
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        let outputs = outputs?;
        crate::ai::validate_outputs(&config.measurements, &outputs)?;
        let mut samples = Vec::with_capacity(outputs.len());
        for definition in &config.measurements {
            let result = outputs
                .iter()
                .find(|output| output.id == definition.id)
                .context("The model omitted a configured measurement.")?
                .clone();
            samples.push(MeasurementSample {
                timestamp_ms,
                camera_id: config.camera_id.clone(),
                frame_id: frame.id,
                definition: definition.clone(),
                provider: config.model.provider,
                model: config.model.model.clone(),
                result,
            });
        }
        {
            let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
            // Linearize stopping against the small atomic database commit.
            // This lock never covers image conversion or a provider request.
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            record(&samples)?;
            shared.status.busy = false;
            shared.status.completed_samples = shared
                .status
                .completed_samples
                .saturating_add(samples.len() as u64);
        }
        // No request overlap or catch-up burst: interval starts after completion.
        if !cancelled.load(Ordering::Acquire) {
            thread::park_timeout(interval);
        }
    }
    Ok(())
}

fn finish(shared: &Mutex<Shared>, cancelled: &AtomicBool, outcome: Result<()>) {
    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
    shared.status.running = false;
    shared.status.busy = false;
    shared.active = None;
    if !cancelled.load(Ordering::Acquire)
        && let Err(error) = outcome
    {
        shared.status.error = Some(format!("{error:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VideoAnalyticsConfig {
        VideoAnalyticsConfig {
            camera_id: "sim:0".into(),
            model: ModelConfig::default(),
            measurements: vec![MeasurementDefinition {
                id: "tubes".into(),
                label: "Tubes".into(),
                prompt: "Count the number of tubes".into(),
                unit: "tubes".into(),
            }],
            interval_seconds: 30,
        }
    }

    fn output() -> MeasurementOutput {
        MeasurementOutput {
            id: "tubes".into(),
            value: Some(3.0),
            confidence: Some(0.8),
            note: "Three visible tubes".into(),
        }
    }

    #[test]
    fn validates_sampling_limits_and_duplicate_measurements() {
        let mut config = config();
        assert!(config.validate().is_ok());
        for interval in [0, 4, 3601] {
            config.interval_seconds = interval;
            assert!(config.validate().is_err());
        }
        config.interval_seconds = 30;
        config.measurements.push(config.measurements[0].clone());
        assert!(config.validate().is_err());
    }

    #[test]
    fn cancelled_inflight_response_is_discarded() {
        let shared = Mutex::new(Shared::default());
        let cancelled = AtomicBool::new(false);
        let mut recorded = false;
        let result = worker_loop(
            &shared,
            &cancelled,
            &config(),
            || Ok(Some((1, Arc::new(Frame::default())))),
            |_, _, _| {
                cancelled.store(true, Ordering::Release);
                Ok(vec![output()])
            },
            |_| {
                recorded = true;
                Ok(())
            },
            Duration::ZERO,
        );
        assert!(result.is_ok());
        assert!(!recorded);
        finish(&shared, &cancelled, result);
        let status = &shared.lock().unwrap().status;
        assert!(!status.running && !status.busy);
        assert!(status.error.is_none());
    }

    #[test]
    fn response_ids_match_definitions_even_if_returned_out_of_order() {
        let shared = Mutex::new(Shared::default());
        let cancelled = AtomicBool::new(false);
        let mut config = config();
        let mut ribbon = config.measurements[0].clone();
        ribbon.id = "ribbon".into();
        ribbon.label = "Ribbon".into();
        ribbon.prompt = "Estimate ribbon length in meters".into();
        ribbon.unit = "m".into();
        config.measurements.push(ribbon);
        let mut recorded = Vec::new();
        worker_loop(
            &shared,
            &cancelled,
            &config,
            || {
                Ok(Some((
                    1,
                    Arc::new(Frame {
                        id: 42,
                        ..Frame::default()
                    }),
                )))
            },
            |_, _, _| {
                Ok(vec![
                    MeasurementOutput {
                        id: "ribbon".into(),
                        value: None,
                        confidence: None,
                        note: "No scale reference".into(),
                    },
                    output(),
                ])
            },
            |samples| {
                recorded = samples.to_vec();
                cancelled.store(true, Ordering::Release);
                Ok(())
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].result.value, Some(3.0));
        assert_eq!(recorded[1].result.value, None);
        assert!(
            recorded
                .iter()
                .all(|sample| sample.frame_id == 42 && sample.camera_id == "sim:0")
        );
        assert_eq!(shared.lock().unwrap().status.completed_samples, 2);
    }

    #[test]
    fn failed_request_stops_collection_without_writing_zero() {
        let shared = Mutex::new(Shared::default());
        let cancelled = AtomicBool::new(false);
        let result = worker_loop(
            &shared,
            &cancelled,
            &config(),
            || Ok(Some((1, Arc::new(Frame::default())))),
            |_, _, _| anyhow::bail!("Provider refused the request"),
            |_| panic!("Failed requests must not reach the database"),
            Duration::ZERO,
        );
        finish(&shared, &cancelled, result);
        let status = &shared.lock().unwrap().status;
        assert!(!status.running && !status.busy);
        assert!(status.error.as_deref().unwrap().contains("refused"));
        assert_eq!(status.completed_samples, 0);
    }

    #[test]
    fn repeated_frames_are_skipped_and_new_worker_identity_is_sampled() {
        let shared = Mutex::new(Shared::default());
        let cancelled = AtomicBool::new(false);
        let frames = [(1, 10, 100), (1, 10, 100), (1, 11, 200), (2, 11, 200)];
        let mut next = frames.into_iter();
        let mut calls = 0;
        let mut recorded = 0;
        worker_loop(
            &shared,
            &cancelled,
            &config(),
            || {
                let (worker, id, timestamp_ns) = next.next().expect("Unexpected extra sample");
                Ok(Some((
                    worker,
                    Arc::new(Frame {
                        id,
                        timestamp_ns,
                        ..Frame::default()
                    }),
                )))
            },
            |_, _, _| {
                calls += 1;
                Ok(vec![output()])
            },
            |_| {
                recorded += 1;
                if recorded == 3 {
                    cancelled.store(true, Ordering::Release);
                }
                Ok(())
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(shared.lock().unwrap().status.completed_samples, 3);
    }

    #[test]
    fn database_failure_stops_worker_without_incrementing_count() {
        let shared = Mutex::new(Shared::default());
        let cancelled = AtomicBool::new(false);
        let result = worker_loop(
            &shared,
            &cancelled,
            &config(),
            || Ok(Some((1, Arc::new(Frame::default())))),
            |_, _, _| Ok(vec![output()]),
            |_| anyhow::bail!("Could not commit measurements"),
            Duration::ZERO,
        );
        finish(&shared, &cancelled, result);
        let status = &shared.lock().unwrap().status;
        assert!(!status.running && !status.busy);
        assert_eq!(status.completed_samples, 0);
        assert!(status.error.as_deref().unwrap().contains("commit"));
    }
}
