//! Native runtime, real helper and private storage. Only provider judgment and
//! credentials are injected; environment isolation lives in a child process.
#[cfg(all(feature = "ai", feature = "parquet", unix))]
mod native {
    use capturefab::{
        assistant::{
            Auth, Http, Limits, ProviderId, ProviderSettings, ResolvedCredentials, Settings,
            VisionMode,
            agent::{CredentialResolver, Deps},
            approval::{ApprovalDecision, ApprovalRequest, ApprovalScope, Approver, RunMode},
            http::{
                Headers, HttpRequest, HttpResponse, ProviderError, ProviderErrorKind, Transport,
            },
            run::{
                OnHumanChange, Outcome, RunControl, RunEvent, RunRequest, RunScope,
                TaskDataRequest, run_blocking,
            },
        },
        dataset::OpenMode,
        session::{
            ActivityOrigin, ActorKind, SessionActor, SessionCommand, SessionHandle,
            SessionSnapshot,
            task_data::{TaskDataPhase, TaskDataPolicy},
        },
        storage::{self, StoragePolicy},
        tools::context::{RecordingDefaults, SessionHost, ToolHost},
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        collections::{BTreeMap, VecDeque},
        path::{Path, PathBuf},
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const CAP: u64 = 16 << 20;
    const EXPORT_CAP: u64 = 1 << 20;
    const URI: &str = "sim-signal://native-runtime-owned";
    const MANUAL: &str = "sim-signal://native-runtime-human";
    const CHILD: &str = "CAPTUREFAB_NATIVE_TASK_TEST_CHILD";
    const TEST: &str = "native::native_runtime_uses_real_helper_and_retains_private_storage_until_explicit_removal";

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Credentials {
        calls: AtomicUsize,
        fail: AtomicBool,
        blocked: AtomicBool,
        entered: AtomicBool,
    }
    struct CredentialGate(Arc<Credentials>);
    impl Drop for CredentialGate {
        fn drop(&mut self) {
            self.0.blocked.store(false, Ordering::SeqCst);
        }
    }
    impl CredentialResolver for Credentials {
        fn resolve(
            &self,
            _: ProviderId,
            settings: &ProviderSettings,
        ) -> anyhow::Result<ResolvedCredentials> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.blocked.load(Ordering::SeqCst) {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "fixture credential gate exceeded its deadline"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            anyhow::ensure!(
                !self.fail.load(Ordering::SeqCst),
                "fixture provider initialization failed"
            );
            Ok(ResolvedCredentials {
                auth: Auth::None,
                settings: settings.clone(),
            })
        }
    }
    #[derive(Default)]
    struct Model {
        requests: Mutex<Vec<Value>>,
        replies: Mutex<VecDeque<Value>>,
    }
    impl Transport for Model {
        fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, ProviderError> {
            self.requests
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&request.body).unwrap());
            let value = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::new(ProviderErrorKind::Malformed))?;
            Ok(HttpResponse {
                status: 200,
                headers: Headers::new(),
                body: serde_json::to_vec(&value).unwrap(),
            })
        }
    }
    struct Approvals;
    impl Approver for Approvals {
        fn request(
            &self,
            request: ApprovalRequest,
            _: &AtomicBool,
            _: Duration,
        ) -> ApprovalDecision {
            assert!(
                ["start_signal_capture", "annotate_dataset", "export_dataset"]
                    .contains(&request.tool.as_str()),
                "unexpected approval: {}",
                request.tool
            );
            ApprovalDecision::Approved {
                scope: ApprovalScope::Once,
                by: SessionActor::new("native-test-human", "Native test human", ActorKind::Human)
                    .unwrap(),
            }
        }
    }
    struct Unsupported(Arc<dyn ToolHost>);
    impl ToolHost for Unsupported {
        fn snapshot(&self) -> anyhow::Result<SessionSnapshot> {
            self.0.snapshot()
        }
        fn execute(&self, camera: Option<&str>, command: SessionCommand) -> anyhow::Result<Value> {
            self.0.execute(camera, command)
        }
        fn has_session(&self) -> bool {
            true
        }
        fn with_actor(&self, actor: SessionActor) -> anyhow::Result<Arc<dyn ToolHost>> {
            self.0.with_actor(actor)
        }
    }
    fn call(id: &str, name: &str, arguments: Value) -> Value {
        json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":10}})
    }
    fn request() -> RunRequest {
        RunRequest {
            goal: "Capture the approved sensor, verify its saved values, add a note and export the private dataset.".into(),
            mode: RunMode::Autonomous,
            scope: RunScope { data_sources: vec![URI.into()], allow_data: true, ..Default::default() },
            limits: Limits {
                max_rounds: 10, max_wall_ms: 60_000, max_bytes: 64 << 20, max_data_bytes: 3 << 20,
                max_sources: 1, max_samples: 12, max_signal_ms: 3000,
                max_captures: 0, max_images: 0, max_recordings: 0, max_setting_changes: 0,
                min_round_interval_ms: 1000, ..Default::default()
            },
            on_human_change: OnHumanChange::Inform, provider: Some(ProviderId::LmStudio), model: None,
            wait_for_answer: false, task_data: Some(TaskDataRequest { max_bytes: CAP }),
        }
    }
    fn run(
        request: RunRequest,
        deps: &Deps,
    ) -> anyhow::Result<capturefab::assistant::run::RunOutcome> {
        run_blocking(
            request,
            deps,
            Arc::new(Approvals),
            &RunControl::default(),
            &mut |_| {},
        )
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                Instant::now() < until,
                "native runtime condition did not become true"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn records(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn result(requests: &[Value], id: &str) -> Value {
        let message = requests
            .iter()
            .filter_map(|request| request["messages"].as_array())
            .flat_map(|messages| messages.iter())
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap_or_else(|| panic!("provider never received result for {id}"));
        let value: Value = serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
        assert!(
            value["is_error"] != true && value["error"].is_null(),
            "{id}: {value}"
        );
        value
    }
    fn object_with<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        if value.get(key).is_some() {
            return Some(value);
        }
        match value {
            Value::Array(values) => values.iter().find_map(|value| object_with(value, key)),
            Value::Object(values) => values.values().find_map(|value| object_with(value, key)),
            _ => None,
        }
    }
    fn reserved(path: &Path) -> bool {
        storage::quota_status().unwrap()["retained_namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["path"] == path.display().to_string() && item["allocated_bytes"] == CAP
            })
    }

    #[test]
    fn native_runtime_uses_real_helper_and_retains_private_storage_until_explicit_removal() {
        if std::env::var_os(CHILD).is_none() {
            let root = Directory(std::env::temp_dir().join(format!(
                    "capturefab-native-runtime-{}-{}",
                    std::process::id(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )));
            std::fs::create_dir_all(&root.0).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &root.0)
                .env("CAPTUREFAB_SESSION_DIR", root.0.join("state"))
                .env("CAPTUREFAB_CACHE_DIR", root.0.join("cache"))
                .env("CAPTUREFAB_EXECUTABLE", env!("CARGO_BIN_EXE_capturefab"))
                .env(CHILD, "1")
                .args(["--exact", TEST, "--nocapture"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated native runtime failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os("HOME").unwrap());
        let capture_root = root.join("captures");
        std::fs::create_dir_all(&capture_root).unwrap();
        let session = SessionHandle::new();
        let host = Arc::new(SessionHost::new(
            session.clone(),
            ActivityOrigin::Gui,
            SessionActor::new("native-human", "Native human", ActorKind::Human).unwrap(),
        ));
        let shared = host
            .execute(
                None,
                SessionCommand::DatasetOpen {
                    path: root.join("human.duckdb").display().to_string(),
                    mode: OpenMode::Create,
                    max_bytes: None,
                },
            )
            .unwrap();
        host.execute(
            None,
            SessionCommand::DatasetNote {
                text: "human sentinel".into(),
                context: BTreeMap::new(),
                camera: None,
            },
        )
        .unwrap();
        let shared_slot = shared["slot_id"].as_str().unwrap().to_owned();
        let shared_generation = host.snapshot().unwrap().signal_sink.unwrap().generation;
        let model = Arc::new(Model::default());
        let credentials = Arc::new(Credentials {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            entered: AtomicBool::new(false),
        });
        let mut settings = Settings {
            provider: Some(ProviderId::LmStudio),
            ..Default::default()
        };
        settings.providers.insert(
            ProviderId::LmStudio,
            ProviderSettings {
                model: Some("native-test-model".into()),
                base_url: Some("http://127.0.0.1:12345/v1".into()),
                vision: Some(VisionMode::Off),
                ..Default::default()
            },
        );
        let mut deps = Deps::native(
            host.clone(),
            settings,
            capture_root.clone(),
            StoragePolicy::default(),
            RecordingDefaults::default(),
        );
        deps.http = Arc::new(Http::with_transport(model.clone()));
        deps.credentials = credentials.clone();
        deps.log_directory = root.join("runs");

        let mut denied = request();
        denied.scope.allow_data = false;
        assert!(run(denied, &deps).is_err());
        let mut mixed = request();
        mixed.scope.dataset = Some(shared_slot.clone());
        let error = run(mixed, &deps).unwrap_err();
        assert!(error.to_string().contains("sensor-only"), "{error:#}");
        let mut unsupported = deps.clone();
        unsupported.host = Arc::new(Unsupported(host.clone()));
        let error = run(request(), &unsupported).unwrap_err();
        assert!(error.to_string().contains("unavailable"), "{error:#}");
        assert!(model.requests.lock().unwrap().is_empty());
        assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
        assert!(session.task_data_workspaces().is_empty());

        let manual = host
            .execute(
                None,
                SessionCommand::SignalStart {
                    uri: MANUAL.into(),
                    auth: None,
                    interval_ms: 100,
                    tags: vec![],
                },
            )
            .unwrap();
        let manual_id = manual["id"].as_str().unwrap().to_owned();
        let manual_token = manual["capture_id"].as_str().unwrap().to_owned();
        let before = host
            .snapshot()
            .unwrap()
            .signals
            .into_iter()
            .find(|source| source.id == manual_id)
            .unwrap()
            .samples;
        let source = capturefab::signals::source_id(URI);
        *model.replies.lock().unwrap() = vec![
            call("overview", "get_overview", json!({})),
            call("capture", "start_signal_capture", json!({"source":source,"every":"100ms","duration":"3s","max_samples":12,"max_bytes":4096,"tags":null})),
            call("wait", "wait_for_data", json!({"source":source,"timeout":"10s","samples":null})),
            call("query", "query_dataset", json!({"sql":"SELECT count(*) AS n, max(CASE WHEN tag LIKE '%/temp_c' THEN value_double END) AS temperature, current_setting('home_directory') AS engine_home FROM samples","limit":1,"timeout":"5s"})),
            call("note", "annotate_dataset", json!({"text":"native private note"})),
            call("export", "export_dataset", json!({"folder":"native-bundle","format":"csv","tables":["samples","events"],"since_ms":null,"until_ms":null,"max_bytes":EXPORT_CAP,"timeout":"5s"})),
            call("finish", "finish_task", json!({"outcome":"succeeded","summary":"Verified the private sensor capture and exported its saved rows."})),
        ].into();
        let outcome = run(request(), &deps).unwrap();
        assert_eq!(outcome.outcome, Outcome::Finished, "{}", outcome.summary);
        assert_eq!(outcome.signal_reports.len(), 1);
        let report = &outcome.signal_reports[0];
        assert_eq!(
            (
                report.samples,
                report.written,
                report.dropped,
                report.helper_dropped
            ),
            (12, 12, 0, 0)
        );
        assert!(report.persistence_complete && report.completed_ms.is_some());
        assert_eq!((outcome.totals.sources, outcome.totals.samples), (1, 12));
        assert_eq!(outcome.totals.bytes, CAP + 4096 + 2 * EXPORT_CAP);
        assert_eq!(outcome.totals.data_bytes, 4096 + 2 * EXPORT_CAP);
        let data = outcome.task_data.as_ref().unwrap();
        assert_eq!(data.phase, TaskDataPhase::Retained);
        assert!(data.catalog.closed && !data.sink.attached && data.sink.error.is_none());
        assert_eq!(data.catalog.extents.limit, CAP);
        let namespace = Path::new(&data.catalog.dataset.path).parent().unwrap();
        assert!(namespace.exists() && reserved(namespace));
        let requests = model.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 7);
        let query = result(&requests, "query");
        assert_eq!(query["rows"][0][0], 12);
        assert!((20.0..21.0).contains(&query["rows"][0][1].as_f64().unwrap()));
        assert_eq!(query["rows"][0][2], "__capturefab_model_private__");
        let provider_wire = serde_json::to_string(&requests).unwrap();
        for hidden in [
            data.catalog.dataset.path.as_str(),
            namespace.to_str().unwrap(),
            "human sentinel",
            MANUAL,
        ] {
            assert!(
                !provider_wire.contains(hidden),
                "provider learned private/shared metadata: {hidden}"
            );
        }
        let exported = result(&requests, "export");
        let exported = object_with(&exported, "files").unwrap();
        assert_eq!(exported["slot_id"], data.catalog.identity.slot_id);
        let output = capture_root.join("native-bundle");
        for file in exported["files"].as_array().unwrap() {
            let bytes = std::fs::read(output.join(file["path"].as_str().unwrap())).unwrap();
            assert_eq!(bytes.len() as u64, file["bytes"].as_u64().unwrap());
            assert_eq!(
                format!("{:x}", Sha256::digest(bytes)),
                file["sha256"].as_str().unwrap()
            );
        }
        assert_eq!(
            std::fs::read_to_string(output.join("samples.csv"))
                .unwrap()
                .lines()
                .count(),
            13
        );
        let notes = std::fs::read_to_string(output.join("events.csv")).unwrap();
        assert!(notes.contains("native private note") && !notes.contains("human sentinel"));
        let log = records(&outcome.log);
        assert!(
            log.iter().any(|record| record["kind"] == "task_data"
                && record["catalog"]["extents"]["limit"] == CAP)
        );
        let end = log.last().unwrap();
        assert_eq!(end["kind"], "end");
        assert_eq!(end["task_data"]["catalog"]["closed"], true);
        assert_eq!(end["task_data"]["catalog"]["extents"]["limit"], CAP);
        assert_eq!(end["totals"]["bytes"], CAP + 4096 + 2 * EXPORT_CAP);
        let current = host.snapshot().unwrap();
        assert_eq!(
            current.dataset.unwrap().slot_id.as_deref(),
            Some(shared_slot.as_str())
        );
        assert_eq!(current.signal_sink.unwrap().generation, shared_generation);
        let human = current
            .signals
            .into_iter()
            .find(|source| source.id == manual_id)
            .unwrap();
        assert_eq!(human.capture_id, manual_token);
        assert_eq!(human.state, capturefab::signals::SourceState::Running);
        assert!(human.samples > before);
        let foreign = session
            .begin_task_data(TaskDataPolicy {
                parent: capture_root.clone(),
                storage: StoragePolicy::default(),
                max_bytes: CAP,
                lifetime: Duration::from_secs(10),
                name: "foreign source owner".into(),
            })
            .unwrap();
        for (id, token) in [(&manual_id, &manual_token), (&source, &report.capture_id)] {
            assert!(
                foreign
                    .stop_capture_status(id, token, Duration::ZERO)
                    .unwrap_err()
                    .to_string()
                    .contains("not owned")
            );
        }
        foreign.remove(Duration::from_secs(5)).unwrap();
        host.execute(
            None,
            SessionCommand::SignalStopIf {
                id: manual_id.clone(),
                capture_id: manual_token,
            },
        )
        .unwrap();
        let shared_rows = host.execute(None, SessionCommand::DatasetQuery {
            sql: format!("SELECT count(*) FILTER (WHERE source_id='{source}'), count(*) FILTER (WHERE source_id='{manual_id}') FROM samples"), limit: 1, timeout_ms: 1000,
        }).unwrap();
        assert_eq!(shared_rows["rows"][0][0], 0);
        assert!(shared_rows["rows"][0][1].as_u64().unwrap() > 0);
        let shared_notes = host
            .execute(
                None,
                SessionCommand::DatasetQuery {
                    sql: "SELECT text FROM events WHERE kind='note'".into(),
                    limit: 10,
                    timeout_ms: 1000,
                },
            )
            .unwrap();
        assert_eq!(shared_notes["rows"], json!([["human sentinel"]]));
        let own = session
            .task_data_workspaces()
            .into_iter()
            .find(|handle| handle.status().id == data.id)
            .unwrap();
        own.remove(Duration::from_secs(5)).unwrap();
        assert!(!namespace.exists() && !reserved(namespace));

        credentials.fail.store(true, Ordering::SeqCst);
        let mut failed_report = None;
        let failure = run_blocking(
            request(),
            &deps,
            Arc::new(Approvals),
            &RunControl::default(),
            &mut |event| {
                if let RunEvent::Finished(report) = event {
                    failed_report = Some(*report);
                }
            },
        );
        assert!(failure.is_err());
        assert_eq!(model.requests.lock().unwrap().len(), 7);
        let failure =
            failed_report.expect("initialization emits a final retained-artifact receipt");
        assert_eq!(failure.outcome, Outcome::Failed);
        assert_eq!(failure.totals.bytes, CAP);
        let retained = failure.task_data.unwrap();
        assert!(
            retained.catalog.closed
                && reserved(Path::new(&retained.catalog.dataset.path).parent().unwrap())
        );
        let end = records(&failure.log).pop().unwrap();
        assert_eq!(end["kind"], "end");
        assert_eq!(end["task_data"]["catalog"]["closed"], true);
        assert_eq!(end["totals"]["bytes"], CAP);
        session
            .task_data_workspaces()
            .into_iter()
            .find(|handle| handle.status().id == retained.id)
            .unwrap()
            .remove(Duration::from_secs(5))
            .unwrap();

        credentials.fail.store(false, Ordering::SeqCst);
        credentials.entered.store(false, Ordering::SeqCst);
        credentials.blocked.store(true, Ordering::SeqCst);
        let gate = CredentialGate(credentials.clone());
        let control = RunControl::default();
        let worker_control = control.clone();
        let worker_deps = deps.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut report = None;
            let result = run_blocking(
                request(),
                &worker_deps,
                Arc::new(Approvals),
                &worker_control,
                &mut |event| {
                    if let RunEvent::Finished(value) = event {
                        report = Some(*value);
                    }
                },
            );
            sender.send((result, report)).unwrap();
        });
        wait(|| credentials.entered.load(Ordering::SeqCst));
        let workspace = session.task_data_workspaces().pop().unwrap();
        let allocated = workspace.status();
        assert_eq!(allocated.phase, TaskDataPhase::Open);
        assert_eq!(allocated.catalog.extents.limit, CAP);
        assert!(reserved(
            Path::new(&allocated.catalog.dataset.path).parent().unwrap()
        ));
        assert_eq!(model.requests.lock().unwrap().len(), 7);
        control.finish_app_quit();
        drop(gate);
        let (result, event) = receiver.recv_timeout(Duration::from_secs(15)).unwrap();
        worker.join().unwrap();
        let quit = result.unwrap();
        assert_eq!(quit.outcome, Outcome::AppQuit);
        assert_eq!(event.unwrap().outcome, Outcome::AppQuit);
        assert_eq!(quit.totals.bytes, CAP);
        let retained = quit.task_data.as_ref().unwrap();
        assert_eq!(retained.id, allocated.id);
        assert_eq!(retained.phase, TaskDataPhase::Retained);
        assert!(retained.catalog.closed && !retained.sink.attached);
        let namespace = Path::new(&retained.catalog.dataset.path).parent().unwrap();
        assert!(namespace.exists() && reserved(namespace));
        assert_eq!(model.requests.lock().unwrap().len(), 7);
        let log = records(&quit.log);
        let end = log.last().unwrap();
        assert_eq!(
            log.iter().filter(|record| record["kind"] == "end").count(),
            1
        );
        assert_eq!(end["kind"], "end");
        assert_eq!(end["outcome"], "app_quit");
        assert_eq!(end["totals"]["bytes"], CAP);
        assert_eq!(end["task_data"]["id"], allocated.id);
        assert_eq!(end["task_data"]["catalog"]["extents"]["limit"], CAP);
        assert!(matches!(
            end["task_data"]["phase"].as_str(),
            Some("closing" | "retained")
        ));
        if end["task_data"]["phase"] == "retained" {
            assert_eq!(end["task_data"]["catalog"]["closed"], true);
        }
        if end["task_data"]["catalog"]["closed"] == true {
            assert_eq!(end["task_data"]["sink"]["attached"], false);
        }
        workspace.remove(Duration::from_secs(5)).unwrap();
        assert!(!namespace.exists() && !reserved(namespace));
        host.execute(None, SessionCommand::DatasetClose).unwrap();
        wait(|| session.task_data_workspaces().is_empty());
        session.shutdown();
    }
}
