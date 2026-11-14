//! Camera housekeeping for ordinary changes; an action key changes acceptance,
//! not image layout/trigger source, so its image epoch and active context stay.
use super::{
    Camera, JobStream, Scheduled, SessionCommand, WorkerCommand, WorkerHandle, end_linger,
    keep_settings, settings_changed,
};
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub(super) struct Effects {
    changes: bool,
    touch_stream: bool,
    connects: bool,
}
impl Effects {
    pub(super) const fn of(command: &WorkerCommand) -> Self {
        if matches!(command, WorkerCommand::WriteActionDeviceKeyUntil { .. }) {
            return Self {
                changes: false,
                touch_stream: false,
                connects: false,
            };
        }
        let changes = command.changes_settings();
        Self {
            changes,
            touch_stream: changes
                || matches!(command.session(), Some(SessionCommand::SetValues { .. })),
            connects: matches!(command.session(), Some(SessionCommand::Connect { .. })),
        }
    }
    pub(super) fn before(
        self,
        camera: &mut Option<Camera>,
        stream: &mut JobStream,
        jobs: &[Scheduled],
        forwarding: bool,
    ) -> bool {
        if self.touch_stream {
            end_linger(camera, stream, jobs, forwarding);
        }
        // Ordinary RW behavior is unchanged, including dry-run stream handling.
        self.changes
            && camera
                .as_mut()
                .is_some_and(|c| c.is_streaming() && c.software_triggered())
    }
    pub(super) fn after(
        self,
        h: &WorkerHandle,
        camera: &mut Option<Camera>,
        kept: &mut Option<Value>,
        was_software: bool,
    ) {
        // Even a change that failed and was put back wrote the camera.
        if self.changes
            && let Some(c) = camera.as_mut()
        {
            settings_changed(c, was_software);
            keep_settings(h, camera, kept);
        } else if self.connects {
            *kept = camera.as_mut().and_then(Camera::emulated_settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{CommandState, process},
        transport::simulator::Simulator,
        types::{Backend, Frame, RegisterIo},
    };
    use anyhow::Result;
    use serde_json::json;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    #[derive(Default)]
    struct Counts {
        reads: AtomicUsize,
        writes: AtomicUsize,
        stops: AtomicUsize,
        buffers: AtomicUsize,
        clock: AtomicUsize,
        emulated: AtomicUsize,
    }
    impl Counts {
        fn reset(&self) {
            for count in [
                &self.reads,
                &self.writes,
                &self.stops,
                &self.buffers,
                &self.clock,
                &self.emulated,
            ] {
                count.store(0, Ordering::Relaxed);
            }
        }
        fn all(&self) -> [usize; 6] {
            [
                &self.reads,
                &self.writes,
                &self.stops,
                &self.buffers,
                &self.clock,
                &self.emulated,
            ]
            .map(|count| count.load(Ordering::Relaxed))
        }
    }
    struct Counted {
        sim: Simulator,
        counts: Arc<Counts>,
    }
    impl RegisterIo for Counted {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.counts.reads.fetch_add(1, Ordering::Relaxed);
            self.sim.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
            self.counts.writes.fetch_add(1, Ordering::Relaxed);
            self.sim.write_memory(address, bytes)
        }
    }
    impl Backend for Counted {
        fn xml(&mut self) -> Result<String> {
            self.sim.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.sim.start(payload)
        }
        fn stop(&mut self) -> Result<()> {
            self.counts.stops.fetch_add(1, Ordering::Relaxed);
            self.sim.stop()
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.sim.next_frame(timeout)
        }
        fn buffered_frames(&mut self) -> u32 {
            self.counts.buffers.fetch_add(1, Ordering::Relaxed);
            0
        }
        fn device_time_ns(&mut self) -> Option<u64> {
            self.counts.clock.fetch_add(1, Ordering::Relaxed);
            None
        }
        fn emulated_settings(&mut self) -> Option<Value> {
            self.counts.emulated.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
    #[test]
    fn key_housekeeping_performs_no_camera_io_and_ordinary_changes_keep_their_effects() {
        let counts = Arc::new(Counts::default());
        let mut c = Camera::from_backend(
            crate::transport::simulator::info(),
            Box::new(Counted {
                sim: Simulator::default(),
                counts: counts.clone(),
            }),
        )
        .unwrap();
        c.start().unwrap();
        let mut camera = Some(c);
        let mut handle = WorkerHandle::idle();
        handle.keeps_settings = true;
        let mut stream = JobStream {
            started: true,
            linger_until: Some(Instant::now()),
            ..Default::default()
        };
        let original_kept = json!({"existing":"untouched"});
        let mut kept = Some(original_kept.clone());
        for expired in [false, true] {
            counts.reset();
            let command = WorkerCommand::WriteActionDeviceKeyUntil {
                key: 1,
                deadline_ns: if expired {
                    1
                } else {
                    crate::session::action_key::deadline_after(Duration::from_secs(1)).unwrap()
                },
            };
            assert!(
                command.changes_settings(),
                "IPC failure/watchdog classification stays settings-changing"
            );
            let effects = Effects::of(&command);
            let was_software = effects.before(&mut camera, &mut stream, &[], false);
            let error = process(
                &handle,
                CommandState {
                    camera: &mut camera,
                    forwarder: &mut None,
                    auto: &mut None,
                    jobs: &mut Vec::new(),
                    next_job: &mut 1,
                    scheduled_stream: &mut stream,
                },
                command,
            )
            .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                if expired {
                    crate::types::ErrorKind::Timeout
                } else {
                    crate::types::ErrorKind::Unsupported
                }
            );
            effects.after(&handle, &mut camera, &mut kept, was_software);
            assert_eq!(
                counts.all(),
                [0; 6],
                "no stop, metadata, buffer, clock or persistence calls"
            );
            assert!(camera.as_ref().unwrap().is_streaming());
            assert!(stream.started && stream.linger_until.is_some());
            assert_eq!(kept, Some(original_kept.clone()));
        }
        // The ordinary path still performs its original camera housekeeping.
        counts.reset();
        stream = JobStream::default();
        let command = SessionCommand::SetValues {
            values: vec![crate::controls::Assignment::parse("ExposureTime=1000").unwrap()],
            dry_run: false,
        }
        .into();
        let effects = Effects::of(&command);
        let was_software = effects.before(&mut camera, &mut stream, &[], false);
        effects.after(&handle, &mut camera, &mut kept, was_software);
        assert!(counts.reads.load(Ordering::Relaxed) > 0);
        assert!(counts.buffers.load(Ordering::Relaxed) > 0);
        assert_eq!(counts.emulated.load(Ordering::Relaxed), 1);
        camera.as_mut().unwrap().stop().unwrap();
        handle.shutdown();
    }
}
