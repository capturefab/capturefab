//! Dequeue admission only: ordinary RW transaction/backend semantics stay intact.
use super::WorkerHandle;
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;

pub(super) fn check(handle: &WorkerHandle, deadline_ns: u64) -> Result<()> {
    if handle.cancelled() {
        return Err(Categorized::error(
            ErrorKind::Interrupted,
            "pinned camera operation cancelled before execution",
        ));
    }
    crate::session::pinned::remaining(deadline_ns).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{Camera, CommandState, JobStream, process},
        session::{SessionCommand, protocol::WorkerCommand},
        transport::simulator::Simulator,
        types::{Backend, Frame, RegisterIo},
    };
    use serde_json::json;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    struct Counted {
        sim: Simulator,
        io: Arc<AtomicUsize>,
    }
    impl RegisterIo for Counted {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            self.io.fetch_add(1, Ordering::Relaxed);
            self.sim.read_memory(address, length)
        }
        fn write_memory(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
            self.io.fetch_add(1, Ordering::Relaxed);
            self.sim.write_memory(address, bytes)
        }
    }
    impl Backend for Counted {
        fn xml(&mut self) -> Result<String> {
            self.sim.xml()
        }
        fn start(&mut self, payload: usize) -> Result<()> {
            self.io.fetch_add(1, Ordering::Relaxed);
            self.sim.start(payload)
        }
        fn stop(&mut self) -> Result<()> {
            self.io.fetch_add(1, Ordering::Relaxed);
            self.sim.stop()
        }
        fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
            self.sim.next_frame(timeout)
        }
    }

    #[test]
    fn queued_rw_wrapper_never_renews_expiry_or_touches_camera_after_cancel() {
        let io = Arc::new(AtomicUsize::new(0));
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Counted {
                    sim: Simulator::default(),
                    io: io.clone(),
                }),
            )
            .unwrap(),
        );
        for cancelled in [false, true] {
            let handle = WorkerHandle::idle();
            let deadline_ns =
                crate::session::action_key::deadline_after(Duration::from_millis(50)).unwrap();
            let command = WorkerCommand::SessionUntil {
                command: serde_json::from_value(json!({"op":"set_values","values":[
                    {"name":"Width","value":"64"}]}))
                .unwrap(),
                deadline_ns,
            };
            let (tx, rx) = mpsc::channel();
            handle
                .queue_request(
                    Some(11),
                    command,
                    Box::new(move |reply| {
                        let _ = tx.send(reply);
                    }),
                )
                .map_err(|(error, _)| error)
                .unwrap();
            if !cancelled {
                std::thread::sleep(Duration::from_millis(60));
            }
            let envelope = handle.next_request(Duration::ZERO).unwrap();
            if cancelled {
                handle.cancel(11);
            }
            io.store(0, Ordering::Relaxed);
            // This is also the loop's admission guard before Effects::before.
            let rejected = check(&handle, deadline_ns).unwrap_err();
            assert_eq!(
                crate::types::classify(&rejected).kind,
                if cancelled {
                    ErrorKind::Interrupted
                } else {
                    ErrorKind::Timeout
                }
            );
            // The body repeats that guard even for direct internal dispatch.
            let result = process(
                &handle,
                CommandState {
                    camera: &mut camera,
                    forwarder: &mut None,
                    auto: &mut None,
                    jobs: &mut Vec::new(),
                    next_job: &mut 1,
                    scheduled_stream: &mut JobStream::default(),
                },
                envelope.command,
            );
            (envelope.reply)(result);
            let error = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err();
            assert_eq!(
                crate::types::classify(&error).kind,
                if cancelled {
                    ErrorKind::Interrupted
                } else {
                    ErrorKind::Timeout
                }
            );
            assert_eq!(io.load(Ordering::Relaxed), 0);
        }
        let command: SessionCommand = serde_json::from_value(json!({"op":"set_values","values":[
            {"name":"Width","value":"64"}]}))
        .unwrap();
        let handle = WorkerHandle::idle();
        process(
            &handle,
            CommandState {
                camera: &mut camera,
                forwarder: &mut None,
                auto: &mut None,
                jobs: &mut Vec::new(),
                next_job: &mut 1,
                scheduled_stream: &mut JobStream::default(),
            },
            command.into(),
        )
        .unwrap();
        assert!(
            io.load(Ordering::Relaxed) > 0,
            "unkeyed RW control still performs ordinary I/O"
        );
    }
    #[test]
    fn pinned_schedule_retains_original_expiry_until_its_first_arm_only() {
        let io = Arc::new(AtomicUsize::new(0));
        let mut camera = Some(
            Camera::from_backend(
                crate::transport::simulator::info(),
                Box::new(Counted {
                    sim: Simulator::default(),
                    io: io.clone(),
                }),
            )
            .unwrap(),
        );
        let handle = WorkerHandle::idle();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("not-armed.raw");
        let mut jobs = Vec::new();
        let mut stream = JobStream::default();
        let deadline_ns =
            crate::session::action_key::deadline_after(Duration::from_secs(2)).unwrap();
        let command = WorkerCommand::SessionUntil {
            command: serde_json::from_value(json!({"op":"schedule","output":output,
                "count":1,"timeout_ms":1000,"format":"raw","first_at_ms":0,
                "interval_ms":0,"warmup_ms":0,"min_free":0}))
            .unwrap(),
            deadline_ns,
        };
        process(
            &handle,
            CommandState {
                camera: &mut camera,
                forwarder: &mut None,
                auto: &mut None,
                jobs: &mut jobs,
                next_job: &mut 1,
                scheduled_stream: &mut stream,
            },
            command,
        )
        .unwrap();
        assert_eq!(jobs[0].first_arm_deadline_ns, Some(deadline_ns));
        // Controlled expiry after admission, before the deferred first tick.
        // No relative timeout is manufactured when that tick eventually runs.
        jobs[0].first_arm_deadline_ns = Some(1);
        io.store(0, Ordering::Relaxed);
        super::super::tick_jobs(&handle, &mut camera, &mut jobs, &mut stream, false, false);
        assert_eq!(io.load(Ordering::Relaxed), 0);
        assert!(!camera.as_ref().unwrap().is_streaming());
        assert!(!stream.started);
        assert_eq!(
            jobs[0].info.status,
            crate::scheduling::CaptureStatus::Failed
        );
        assert!(
            jobs[0]
                .info
                .error
                .as_ref()
                .unwrap()
                .contains("original operation deadline")
        );
        assert!(!output.exists());
    }
}
