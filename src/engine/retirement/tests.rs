//! Terminal monitor errors keep their first type across Camera, worker and coordinator.
use super::*;
use crate::types::{Backend, CameraAccess, Categorized, ErrorKind, Frame, RegisterIo, classify};
use anyhow::Result;
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const XML: &str = r#"<RegisterDescription>
<Integer Name="PayloadSize"><Value>1</Value></Integer>
<Command Name="AcquisitionStart"><Value>0</Value><CommandValue>1</CommandValue></Command>
<Command Name="AcquisitionStop"><Value>0</Value><CommandValue>0</CommandValue></Command>
</RegisterDescription>"#;
fn first_failure() -> anyhow::Error {
    Categorized::new(ErrorKind::Incomplete, "first selected malformed packet")
        .details(json!({"monitor_connection":{"state":"retired","reason":"receive_failed"}}))
        .into()
}
#[derive(Default)]
struct Calls {
    stops: AtomicUsize,
    writes: AtomicUsize,
    dropped: AtomicBool,
}
struct Terminal {
    access: CameraAccess,
    wait_only: bool,
    failed: bool,
    calls: Arc<Calls>,
}
impl RegisterIo for Terminal {
    fn read_memory(&mut self, _: u64, _: usize) -> Result<Vec<u8>> {
        anyhow::bail!("no register dependency is needed by this fixture")
    }
    fn write_memory(&mut self, _: u64, _: &[u8]) -> Result<()> {
        self.calls.writes.fetch_add(1, Ordering::Relaxed);
        anyhow::bail!("unexpected remote write")
    }
}
impl Backend for Terminal {
    fn access(&self) -> CameraAccess {
        self.access
    }
    fn xml(&mut self) -> Result<String> {
        Ok(XML.into())
    }
    fn start(&mut self, _: usize) -> Result<()> {
        Ok(())
    }
    fn next_frame(&mut self, _: Duration) -> Result<Frame> {
        if self.wait_only {
            return Err(Categorized::error(
                ErrorKind::Timeout,
                "healthy idle wait expired",
            ));
        }
        self.failed = true;
        Err(first_failure())
    }
    fn terminal_error(&self) -> Option<anyhow::Error> {
        self.failed.then(first_failure)
    }
    fn stop_after_frame_error(&self) -> bool {
        self.failed
    }
    fn stop(&mut self) -> Result<()> {
        self.calls.stops.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.calls.dropped.store(true, Ordering::Release);
    }
}
fn camera(access: CameraAccess, wait_only: bool) -> (Option<Camera>, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let mut info = crate::transport::simulator::info();
    // The same URI/namespace is deliberately used in the negative control:
    // worker-side retirement must inspect immutable access, not an id prefix.
    info.id = "gige-monitor://192.0.2.10?interface=192.0.2.20&mac=020000000001".into();
    info.transport = crate::types::Transport::GigE;
    let mut camera = Camera::from_backend(
        info,
        Box::new(Terminal {
            access,
            wait_only,
            failed: false,
            calls: calls.clone(),
        }),
    )
    .unwrap();
    camera.start().unwrap();
    (Some(camera), calls)
}
fn check_camera(
    h: &WorkerHandle,
    camera: &mut Option<Camera>,
    observed: Option<&anyhow::Error>,
) -> bool {
    check(
        h,
        CommandState {
            camera,
            forwarder: &mut None,
            auto: &mut None,
            jobs: &mut Vec::new(),
            next_job: &mut 1,
            scheduled_stream: &mut JobStream::default(),
        },
        &mut inspector::Inspector::default(),
        observed,
    )
}

#[test]
fn selected_malformed_monitor_stops_once_then_immediately_retires_with_original_kind() {
    let h = WorkerHandle::idle();
    let (mut camera, calls) = camera(CameraAccess::ReadOnlyMonitor, false);
    let first = camera
        .as_mut()
        .unwrap()
        .next_frame(Duration::from_millis(100))
        .unwrap_err();
    assert_eq!(classify(&first).kind, ErrorKind::Incomplete);
    assert!(!camera.as_ref().unwrap().is_streaming());
    assert_eq!(calls.stops.load(Ordering::Relaxed), 1);
    // This is the actual worker loop's post-read sticky-state check. No second
    // read or ten-error threshold is needed after Camera has already stopped.
    assert!(check_camera(&h, &mut camera, None));
    assert!(camera.is_none());
    assert!(calls.dropped.load(Ordering::Acquire));
    assert_eq!(calls.stops.load(Ordering::Relaxed), 1);
    assert_eq!(calls.writes.load(Ordering::Relaxed), 0);
    let wire = h.retired.lock().unwrap().clone().unwrap();
    assert_eq!(wire["code"], "incomplete");
    assert_eq!(wire["details"]["monitor_connection"]["state"], "retired");
    assert!(
        wire["message"]
            .as_str()
            .unwrap()
            .contains("first selected malformed packet")
    );
    assert!(wire_is_retired(&wire));
    let events = h.events.drain();
    let retired: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Output::Retired { error } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(retired, [&wire]);
    assert_eq!(
        classify(&admission(&h, &WorkerCommand::CheckConnection).unwrap()).kind,
        ErrorKind::Incomplete
    );
    assert!(!check_camera(
        &h,
        &mut camera,
        Some(&Categorized::error(ErrorKind::Timeout, "later wait"))
    ));
    assert_eq!(h.retired.lock().unwrap().as_ref(), Some(&wire));
    assert_eq!(h.events.drain().len(), 0);
    h.shutdown();
}

#[test]
fn a_control_backend_cannot_retire_using_the_monitor_namespace() {
    let h = WorkerHandle::idle();
    let (mut camera, calls) = camera(CameraAccess::Control, false);
    let observed = camera
        .as_mut()
        .unwrap()
        .next_frame(Duration::from_millis(100))
        .unwrap_err();
    assert_eq!(classify(&observed).kind, ErrorKind::Incomplete);
    assert!(!camera.as_ref().unwrap().is_streaming());
    assert!(!check_camera(&h, &mut camera, Some(&observed)));
    assert!(camera.is_some());
    assert!(h.retired.lock().unwrap().is_none());
    assert_eq!(h.events.drain().len(), 0);
    assert_eq!(calls.writes.load(Ordering::Relaxed), 0);
    drop(camera);
    assert!(calls.dropped.load(Ordering::Acquire));
    h.shutdown();
}

#[test]
fn healthy_monitor_wait_stays_live_and_wire_recognition_preserves_controller_rules() {
    let h = WorkerHandle::idle();
    let (mut camera, calls) = camera(CameraAccess::ReadOnlyMonitor, true);
    let observed = camera
        .as_mut()
        .unwrap()
        .next_frame(Duration::from_millis(100))
        .unwrap_err();
    assert_eq!(classify(&observed).kind, ErrorKind::Timeout);
    assert!(camera.as_ref().unwrap().is_streaming());
    assert!(!check_camera(&h, &mut camera, Some(&observed)));
    assert!(h.retired.lock().unwrap().is_none());
    assert_eq!(calls.stops.load(Ordering::Relaxed), 0);
    assert!(!wire_is_retired(&protocol::wire_error(&observed)));
    assert!(wire_is_retired(
        &json!({"code":"disconnected","message":"old lease", "details":{"controller_ownership":"retired"}})
    ));
    assert!(!wire_is_retired(
        &json!({"code":"frame_timeout","message":"wait", "details":{"controller_ownership":"retired"}})
    ));
    for code in ["incomplete", "unsupported", "unavailable"] {
        let wire = json!({"code":code,"message":"first native cause", "details":{"monitor_connection":{"state":"retired"}}});
        assert!(wire_is_retired(&wire));
        assert_eq!(classify(&protocol::from_wire(&wire)).kind.code().0, code);
    }
    assert!(!wire_is_retired(
        &json!({"code":"not_a_native_error","message":"bad", "details":{"monitor_connection":{"state":"retired"}}})
    ));
    assert!(!wire_is_retired(
        &json!({"message":"monitor_connection retired"})
    ));
    drop(camera);
    h.shutdown();
}
