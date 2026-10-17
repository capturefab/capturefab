use super::*;
use crate::{
    network::NetworkTuning,
    transport::simulator::Simulator,
    types::{Backend, RegisterIo, Transport, TransportStats},
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Start,
    Stop,
    Optimize,
    Write(u64),
}

#[derive(Default)]
struct BackendState {
    calls: Vec<Call>,
    starts: usize,
    fail_stop: bool,
    fail_tuning: bool,
    fail_resume: bool,
    stats: TransportStats,
}

struct NetworkBackend {
    simulator: Simulator,
    state: Arc<Mutex<BackendState>>,
}

impl RegisterIo for NetworkBackend {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.simulator.read_memory(address, length)
    }

    fn write_memory(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        self.state.lock().unwrap().calls.push(Call::Write(address));
        self.simulator.write_memory(address, bytes)
    }
}

impl Backend for NetworkBackend {
    fn xml(&mut self) -> Result<String> {
        let extra = r#"
            <Integer Name="DeviceLinkSpeed"><Value>125000000</Value><ImposedAccessMode>RO</ImposedAccessMode><Unit>Bps</Unit></Integer>
            <Integer Name="DeviceLinkThroughputLimit"><pValue>ThroughputReg</pValue><Min>1000</Min><Max>100000000</Max><Inc>1000</Inc></Integer>
            <IntReg Name="ThroughputReg"><Address>0x180</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess><Sign>Unsigned</Sign></IntReg>
        "#;
        Ok(self.simulator.xml()?.replace(
            "</RegisterDescription>",
            &format!("{extra}</RegisterDescription>"),
        ))
    }

    fn start(&mut self, payload: usize) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Start);
            state.starts += 1;
            ensure!(
                !(state.fail_resume && state.starts > 1),
                "injected resume failure"
            );
        }
        self.simulator.start(payload)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        self.simulator.next_frame(timeout)
    }

    fn stop(&mut self) -> Result<()> {
        let fail = {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call::Stop);
            state.fail_stop
        };
        self.simulator.stop()?;
        ensure!(!fail, "injected stop failure");
        Ok(())
    }

    fn optimize_network(&mut self, _: usize) -> Result<NetworkTuning> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(Call::Optimize);
        ensure!(!state.fail_tuning, "injected tuning failure");
        Ok(NetworkTuning {
            packet_size: Some(1500),
            changes: vec!["1500-byte stream packets verified".into()],
            ..NetworkTuning::default()
        })
    }

    fn stats(&self) -> Option<TransportStats> {
        Some(self.state.lock().unwrap().stats.clone())
    }
}

struct Harness {
    handle: WorkerHandle,
    camera: Option<Camera>,
    backend: Arc<Mutex<BackendState>>,
    forwarder: Option<(Forwarder, f64)>,
    auto: Option<AutoController>,
    jobs: Vec<Scheduled>,
    next_job: u64,
    scheduled_stream: bool,
}

impl Harness {
    fn new(transport: Transport, running: bool) -> Self {
        let backend = Arc::new(Mutex::new(BackendState::default()));
        let mut simulator = Simulator::default();
        simulator
            .write_memory(0x180, &100_000_000u32.to_le_bytes())
            .unwrap();
        let mut info = crate::transport::simulator::info();
        info.transport = transport;
        let mut camera = Camera::from_backend(
            info,
            Box::new(NetworkBackend {
                simulator,
                state: backend.clone(),
            }),
        )
        .unwrap();
        if running {
            camera.start().unwrap();
        }
        // The commands under test run synchronously, without a second camera
        // owner or a background acquisition changing their state.
        let (sender, _) = mpsc::sync_channel(32);
        let handle = WorkerHandle {
            capture_session: Arc::default(),
            sender,
            state: Arc::new(RwLock::new(SessionSnapshot::default())),
            counters: Arc::default(),
            frame: Arc::new(RwLock::new(None)),
            quitting: Arc::new(AtomicBool::new(false)),
            thread: Arc::new(Mutex::new(None)),
            ring: None,
            history: Arc::default(),
            saved: Arc::default(),
            recording: Arc::default(),
        };
        let mut harness = Self {
            handle,
            camera: Some(camera),
            backend,
            forwarder: None,
            auto: None,
            jobs: Vec::new(),
            next_job: 1,
            scheduled_stream: false,
        };
        refresh(&harness.handle, &mut harness.camera, false);
        harness.backend.lock().unwrap().calls.clear();
        harness
    }

    fn run(&mut self, command: SessionCommand) -> Result<Value> {
        process_plain(
            &self.handle,
            &mut self.camera,
            &mut self.forwarder,
            &mut self.auto,
            &mut self.jobs,
            &mut self.next_job,
            &mut self.scheduled_stream,
            command,
        )
    }

    fn optimize(&mut self) -> Result<Value> {
        self.run(SessionCommand::OptimizeNetwork {
            bytes_per_second: 33_333_333,
        })
    }

    fn transport_calls(&self) -> Vec<Call> {
        self.backend
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|call| !matches!(call, Call::Write(_)))
            .cloned()
            .collect()
    }
}

#[test]
fn running_preview_is_paused_tuned_and_resumed() {
    let mut harness = Harness::new(Transport::GigE, true);
    let result = harness.optimize().unwrap();
    assert_eq!(
        harness.transport_calls(),
        [Call::Stop, Call::Optimize, Call::Start]
    );
    assert_eq!(result["tuning"]["packet_size"], 1500);
    assert_eq!(result["tuning"]["throughput_limit_bps"], 33_333_000);
    assert!(harness.camera.as_ref().unwrap().is_streaming());
    assert!(harness.handle.snapshot().streaming);
}

#[test]
fn failed_tuning_resumes_running_preview_and_publishes_stream_state() {
    let mut harness = Harness::new(Transport::GigE, true);
    harness.backend.lock().unwrap().fail_tuning = true;
    let error = harness.optimize().unwrap_err();
    assert!(error.to_string().contains("injected tuning failure"));
    assert_eq!(
        harness.transport_calls(),
        [Call::Stop, Call::Optimize, Call::Start]
    );
    assert!(harness.camera.as_ref().unwrap().is_streaming());
    assert!(harness.handle.snapshot().streaming);
}

#[test]
fn failed_stop_still_attempts_to_resume_without_tuning() {
    let mut harness = Harness::new(Transport::GigE, true);
    harness.backend.lock().unwrap().fail_stop = true;
    let error = harness.optimize().unwrap_err();
    assert!(error.to_string().contains("injected stop failure"));
    assert_eq!(harness.transport_calls(), [Call::Stop, Call::Start]);
    assert!(harness.camera.as_ref().unwrap().is_streaming());
    assert!(harness.handle.snapshot().streaming);
}

#[test]
fn tuning_and_resume_errors_are_both_reported() {
    let mut harness = Harness::new(Transport::GigE, true);
    {
        let mut state = harness.backend.lock().unwrap();
        state.fail_tuning = true;
        state.fail_resume = true;
    }
    let error = format!("{:#}", harness.optimize().unwrap_err());
    assert!(error.contains("injected tuning failure"), "{error}");
    assert!(error.contains("injected resume failure"), "{error}");
    assert_eq!(
        harness.transport_calls(),
        [Call::Stop, Call::Optimize, Call::Start]
    );
    assert!(!harness.camera.as_ref().unwrap().is_streaming());
    assert!(!harness.handle.snapshot().streaming);
}

#[test]
fn resume_failure_after_successful_tuning_reports_stopped_preview() {
    let mut harness = Harness::new(Transport::GigE, true);
    harness.backend.lock().unwrap().fail_resume = true;
    let error = format!("{:#}", harness.optimize().unwrap_err());
    assert!(
        error.contains("resuming stream after network scan"),
        "{error}"
    );
    assert!(error.contains("injected resume failure"), "{error}");
    assert_eq!(
        harness.transport_calls(),
        [Call::Stop, Call::Optimize, Call::Start]
    );
    assert!(!harness.camera.as_ref().unwrap().is_streaming());
    assert!(!harness.handle.snapshot().streaming);
}

#[test]
fn stop_and_resume_errors_are_both_reported() {
    let mut harness = Harness::new(Transport::GigE, true);
    {
        let mut state = harness.backend.lock().unwrap();
        state.fail_stop = true;
        state.fail_resume = true;
    }
    let error = format!("{:#}", harness.optimize().unwrap_err());
    assert!(error.contains("injected stop failure"), "{error}");
    assert!(error.contains("injected resume failure"), "{error}");
    assert_eq!(harness.transport_calls(), [Call::Stop, Call::Start]);
    assert!(!harness.handle.snapshot().streaming);
}

#[test]
fn idle_camera_is_tuned_without_starting_acquisition() {
    let mut harness = Harness::new(Transport::GigE, false);
    harness.optimize().unwrap();
    assert_eq!(harness.transport_calls(), [Call::Optimize]);
    assert!(!harness.camera.as_ref().unwrap().is_streaming());
    assert!(!harness.handle.snapshot().streaming);
}

#[test]
fn pending_and_running_capture_jobs_prevent_network_writes() {
    for status in ["pending", "running"] {
        let mut harness = Harness::new(Transport::GigE, true);
        let output = "network-scan-protected.png";
        harness.jobs.push(Scheduled {
            info: crate::scheduling::CaptureJob {
                id: 1,
                status: status.into(),
                output: output.into(),
                destination: None,
                count: 1,
                captured: 0,
                first_at_ms: 0,
                interval_ms: 1000,
                next_at_ms: 1000,
                last_file: None,
                error: None,
                context: Default::default(),
            },
            resolved: crate::destination::Resolved {
                output: output.into(),
                destination: None,
                upload: None,
                root: None,
            },
            timeout_ms: 1000,
            format: "png".into(),
            storage: Default::default(),
            deadline: None,
            warm: None,
        });
        let error = harness.optimize().unwrap_err();
        assert!(error.to_string().contains("scheduled captures"));
        assert!(harness.backend.lock().unwrap().calls.is_empty(), "{status}");
        assert!(harness.camera.as_ref().unwrap().is_streaming());
        assert_eq!(harness.jobs[0].info.status, status);
    }
}

#[test]
fn non_gige_camera_is_rejected_before_pausing_or_writing() {
    let mut harness = Harness::new(Transport::Simulator, true);
    let error = harness.optimize().unwrap_err();
    assert!(error.to_string().contains("requires a GigE camera"));
    assert!(harness.backend.lock().unwrap().calls.is_empty());
    assert!(harness.camera.as_ref().unwrap().is_streaming());
}

#[test]
fn status_refreshes_live_transport_counters() {
    let mut harness = Harness::new(Transport::GigE, true);
    for count in [7, 12] {
        {
            let mut backend = harness.backend.lock().unwrap();
            backend.stats.lost_frames = count;
            backend.stats.incomplete_frames = count * 2;
            backend.stats.resend_requested = count * 3;
        }
        let status = harness.run(SessionCommand::Status).unwrap();
        assert_eq!(status["transport"]["lost_frames"], count);
        assert_eq!(status["transport"]["incomplete_frames"], count * 2);
        assert_eq!(status["transport"]["resend_requested"], count * 3);
        assert_eq!(
            harness.handle.snapshot().transport.unwrap().lost_frames,
            count
        );
    }
    assert!(harness.backend.lock().unwrap().calls.is_empty());
}
