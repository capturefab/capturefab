//! An in-process overlay holding an unforgeable task data capability. It never
//! resolves serialized task, actor, path or slot labels into private authority.
use super::{SessionActor, SessionCommand, SessionHost, SessionSnapshot, ToolHost};
use crate::{
    dataset::{OpenMode, export::ExportRequest},
    session::task_data::{TaskDataHandle, TaskDataPhase, TaskDataStatus},
    signals::CaptureLimits,
};
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const CLOSE_WAIT: Duration = Duration::from_secs(10);

/// Only the last host view closes admission. Changing attribution shares this
/// guard; it does not mint another capability or close the original view.
struct Lease {
    data: TaskDataHandle,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.data.request_close();
    }
}

pub(super) struct NativeTaskHost {
    base: SessionHost,
    lease: Arc<Lease>,
}

impl NativeTaskHost {
    pub(super) fn new(base: SessionHost, data: TaskDataHandle) -> Self {
        Self {
            base,
            lease: Arc::new(Lease { data }),
        }
    }

    fn dataset(status: &TaskDataStatus) -> Option<crate::dataset::DatasetStatus> {
        (!status.catalog.closed && status.phase != TaskDataPhase::Removed)
            .then(|| Self::public_dataset(status.catalog.dataset.clone()))
    }

    fn public_dataset(mut dataset: crate::dataset::DatasetStatus) -> crate::dataset::DatasetStatus {
        dataset.last_error = dataset
            .last_error
            .as_ref()
            .map(|error| Self::scrub_text(&dataset.path, error));
        dataset.path = "task.duckdb".into();
        dataset
    }

    fn scrub_text(path: &str, text: &str) -> String {
        let mut text = text.replace(path, "task.duckdb");
        if let Some(parent) = Path::new(path).parent().and_then(Path::to_str)
            && !parent.is_empty()
        {
            text = text.replace(parent, "[task data]");
        }
        text
    }

    fn public_error(&self, error: anyhow::Error) -> anyhow::Error {
        let message = format!("{error:#}");
        let mut public = message.clone();
        if let Some(stage) = error.downcast_ref::<crate::dataset::task::RetainedExportStage>() {
            public = public.replace(
                stage.path().to_string_lossy().as_ref(),
                "[task export staging]",
            );
            if let Some(id) = stage.namespace_id() {
                public = public.replace(id, "[task export reservation]");
            }
        }
        let public = Self::scrub_text(&self.lease.data.status().catalog.dataset.path, &public);
        if public == message {
            error
        } else {
            anyhow::anyhow!(public)
        }
    }

    fn public_sink(status: &TaskDataStatus) -> crate::signals::SignalSinkStatus {
        let mut sink = status.sink.clone();
        sink.error = sink
            .error
            .as_ref()
            .map(|error| Self::scrub_text(&status.catalog.dataset.path, error));
        sink
    }

    fn source_errors(value: &mut Value, status: &TaskDataStatus) {
        if let Some(sources) = value.as_array_mut() {
            for source in sources {
                Self::source_error(source, status);
            }
        }
    }

    fn source_error(source: &mut Value, status: &TaskDataStatus) {
        let owned = source["id"]
            .as_str()
            .and_then(|id| status.captures.get(id))
            .is_some_and(|token| source["capture_id"].as_str() == Some(token.as_str()));
        if owned {
            for field in ["error", "persistence_error"] {
                if let Some(error) = source[field].as_str() {
                    source[field] = json!(Self::scrub_text(&status.catalog.dataset.path, error));
                }
            }
        }
    }

    fn target(&self, slot: &str, generation: Option<u64>) -> Result<TaskDataStatus> {
        let status = self.lease.data.status();
        ensure!(
            status.catalog.identity.slot_id == slot,
            "This command names another dataset instance; the private task cannot use shared or foreign data."
        );
        ensure!(
            generation.is_none_or(|expected| expected == status.sink.generation),
            "The private task sample sink identity changed; review the task before continuing."
        );
        Ok(status)
    }

    fn open_target(&self, slot: &str, generation: Option<u64>) -> Result<TaskDataStatus> {
        let status = self.target(slot, generation)?;
        ensure!(
            status.phase == TaskDataPhase::Open && !status.catalog.closed,
            "The private task dataset has closed; this host cannot reopen or adopt another dataset."
        );
        ensure!(
            !status.catalog.quarantined,
            "The private task dataset is quarantined after a storage failure."
        );
        Ok(status)
    }

    fn status_value(&self, mut value: Value) -> Result<Value> {
        let status = self.lease.data.status();
        let fields = value
            .as_object_mut()
            .context("the session sent an unreadable status")?;
        fields.insert(
            "dataset".into(),
            serde_json::to_value(Self::dataset(&status))?,
        );
        fields.insert(
            "signal_sink".into(),
            serde_json::to_value(Self::public_sink(&status))?,
        );
        if let Some(sources) = fields.get_mut("signals") {
            Self::source_errors(sources, &status);
        }
        Ok(value)
    }
    fn dispatch(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
        match command {
            SessionCommand::Status => {
                self.status_value(self.base.execute(camera, SessionCommand::Status)?)
            }
            SessionCommand::Signals => {
                let mut value = self.base.execute(camera, SessionCommand::Signals)?;
                let status = self.lease.data.status();
                let sink = Self::public_sink(&status);
                let fields = value
                    .as_object_mut()
                    .context("the session sent unreadable source status")?;
                fields.insert("persisting".into(), json!(sink.attached));
                fields.insert("dropped".into(), json!(sink.dropped_total));
                fields.insert("sink_error".into(), json!(sink.error));
                fields.insert("sink".into(), serde_json::to_value(sink)?);
                if let Some(sources) = fields.get_mut("sources") {
                    Self::source_errors(sources, &status);
                }
                Ok(value)
            }
            SessionCommand::DatasetStatus => Ok(serde_json::to_value(Self::dataset(
                &self.lease.data.status(),
            ))?),
            SessionCommand::DatasetOpenIf {
                expected_slot_id,
                path,
                mode,
                max_bytes,
            } => {
                let slot = expected_slot_id.context("The private task dataset is already allocated; it cannot create or adopt a dataset by path.")?;
                let status = self.open_target(&slot, None)?;
                ensure!(
                    Path::new(&path) == Path::new(&status.catalog.dataset.path),
                    "The private task cannot open another dataset path."
                );
                ensure!(
                    mode != OpenMode::Create,
                    "The private task dataset already exists; choose open or auto."
                );
                ensure!(
                    max_bytes.is_none_or(|cap| cap == status.catalog.dataset.max_bytes),
                    "The private task dataset has a fixed storage cap."
                );
                let mut value = serde_json::to_value(Self::public_dataset(status.catalog.dataset))?;
                value["sink_generation"] = json!(status.sink.generation);
                Ok(value)
            }
            SessionCommand::DatasetCloseIf {
                slot_id,
                sink_generation,
            } => {
                self.target(&slot_id, sink_generation)?;
                let status = self.lease.data.close(CLOSE_WAIT)?;
                Ok(
                    json!({ "closed": Self::public_dataset(status.catalog.dataset), "sink_generation": status.sink.generation }),
                )
            }
            SessionCommand::DatasetModelQueryIf {
                slot_id,
                sql,
                limit,
                timeout_ms,
                max_result_bytes,
            } => {
                self.open_target(&slot_id, None)?;
                self.lease
                    .data
                    .model_query(&slot_id, &sql, limit, timeout_ms, max_result_bytes)
            }
            SessionCommand::DatasetNoteIf {
                slot_id,
                text,
                context,
                camera,
            } => {
                self.open_target(&slot_id, None)?;
                Ok(
                    json!({ "id": self.lease.data.note(&slot_id, &text, &context, camera.as_deref())? }),
                )
            }
            SessionCommand::DatasetExportIf {
                slot_id,
                output,
                approved_root,
                formats,
                tables,
                since_ms,
                until_ms,
                storage,
                max_bytes,
                timeout_ms,
            } => {
                self.open_target(&slot_id, None)?;
                let output = PathBuf::from(output);
                ensure!(output.is_absolute(), "The export folder must be absolute.");
                let approved_root = approved_root.map(PathBuf::from);
                ensure!(
                    approved_root.as_ref().is_none_or(|root| root.is_absolute()),
                    "The approved export root must be absolute."
                );
                self.lease.data.export(
                    &slot_id,
                    &ExportRequest {
                        output,
                        formats,
                        tables,
                        since_ms,
                        until_ms,
                        storage,
                    },
                    max_bytes,
                    timeout_ms,
                    approved_root.as_deref(),
                )
            }
            SessionCommand::SignalCapture {
                uri,
                auth,
                interval_ms,
                tags,
                duration_ms,
                max_samples,
                max_bytes,
                slot_id,
                sink_generation,
            } => {
                self.open_target(&slot_id, sink_generation)?;
                let mut value = serde_json::to_value(self.lease.data.capture(
                    &uri,
                    interval_ms,
                    tags,
                    CaptureLimits {
                        duration_ms,
                        max_samples,
                        max_bytes,
                    },
                    auth.as_ref(),
                )?)?;
                Self::source_error(&mut value, &self.lease.data.status());
                Ok(value)
            }
            SessionCommand::SignalStopIf { id, capture_id } => {
                let stopped = self.lease.data.stop_capture_status(
                    &id,
                    &capture_id,
                    Duration::from_secs(3),
                )?;
                let mut value = json!({ "stopped": stopped });
                Self::source_errors(&mut value["stopped"], &self.lease.data.status());
                Ok(value)
            }
            SessionCommand::DatasetOpen { .. }
            | SessionCommand::DatasetClose
            | SessionCommand::DatasetNote { .. }
            | SessionCommand::DatasetQuery { .. }
            | SessionCommand::DatasetQueryIf { .. }
            | SessionCommand::DatasetExport { .. } => {
                bail!(
                    "This private task host refuses shared, raw or unsupported dataset operations."
                )
            }
            SessionCommand::SignalStart { .. } | SessionCommand::SignalStop { .. } => {
                bail!(
                    "Private tasks require finite captures and an owned capture identity to stop them."
                )
            }
            SessionCommand::CaptureSessionOpen { .. }
            | SessionCommand::CaptureSessionClose
            | SessionCommand::AutoCapture { .. }
            | SessionCommand::CaptureExample { .. }
            | SessionCommand::ReviewCapture { .. }
            | SessionCommand::Capture { .. }
            | SessionCommand::Schedule { .. }
            | SessionCommand::Forward { .. } => {
                bail!(
                    "Camera files are not routed into private task storage yet; use a source-only data task."
                )
            }
            command @ (SessionCommand::Discover { .. }
            | SessionCommand::NetworkScan { .. }
            | SessionCommand::OptimizeNetwork { .. }
            | SessionCommand::Connect { .. }
            | SessionCommand::Select { .. }
            | SessionCommand::Disconnect
            | SessionCommand::Get { .. }
            | SessionCommand::Set { .. }
            | SessionCommand::SetMany { .. }
            | SessionCommand::Execute { .. }
            | SessionCommand::Start
            | SessionCommand::Stop
            | SessionCommand::Undo
            | SessionCommand::Redo
            | SessionCommand::ReplayCameraChange { .. }
            | SessionCommand::Auto { .. }
            | SessionCommand::Manual { .. }
            | SessionCommand::CaptureReview { .. }
            | SessionCommand::Jobs
            | SessionCommand::CancelJob { .. }
            | SessionCommand::Frame { .. }
            | SessionCommand::Grab { .. }
            | SessionCommand::Activity { .. }
            | SessionCommand::Features
            | SessionCommand::Xml
            | SessionCommand::ReadMemory { .. }
            | SessionCommand::WriteMemory { .. }
            | SessionCommand::StopForward
            | SessionCommand::StopForwardIf { .. }
            | SessionCommand::SignalRead { .. }
            | SessionCommand::SignalBrowse { .. }
            | SessionCommand::SignalBind { .. }) => self.base.execute(camera, command),
        }
    }
}

impl ToolHost for NativeTaskHost {
    fn snapshot(&self) -> Result<SessionSnapshot> {
        let mut snapshot = self.base.snapshot()?;
        let status = self.lease.data.status();
        snapshot.dataset = Self::dataset(&status);
        snapshot.signal_sink = Some(Self::public_sink(&status));
        for source in &mut snapshot.signals {
            if status.captures.get(&source.id) == Some(&source.capture_id) {
                for error in [&mut source.error, &mut source.persistence_error]
                    .into_iter()
                    .flatten()
                {
                    *error = Self::scrub_text(&status.catalog.dataset.path, error);
                }
            }
        }
        Ok(snapshot)
    }

    fn execute(&self, camera: Option<&str>, command: SessionCommand) -> Result<Value> {
        command
            .validate()
            .map_err(|error| self.public_error(error))?;
        let result = if matches!(
            &command,
            SessionCommand::DatasetCloseIf { .. }
                | SessionCommand::DatasetNoteIf { .. }
                | SessionCommand::DatasetExportIf { .. }
                | SessionCommand::SignalCapture { .. }
                | SessionCommand::SignalStopIf { .. }
        ) {
            self.base.handle.task_data_activity(
                &self.lease.data,
                self.base.origin,
                self.base.actor.clone(),
                &command,
                || {
                    self.dispatch(camera, command.clone())
                        .map_err(|error| self.public_error(error))
                },
            )
        } else {
            self.dispatch(camera, command)
        };
        result.map_err(|error| self.public_error(error))
    }

    fn has_session(&self) -> bool {
        self.base.has_session()
    }
    fn supports_task_data(&self) -> bool {
        true
    }
    fn task_data_reserved_bytes(&self) -> u64 {
        let status = self.lease.data.status();
        if status.phase == TaskDataPhase::Open {
            status.catalog.extents.limit
        } else {
            0
        }
    }
    fn task_data_status(&self) -> Option<TaskDataStatus> {
        Some(self.lease.data.status())
    }
    fn request_close_task_data(&self) {
        self.lease.data.request_close();
    }
    fn actor_id(&self) -> Option<String> {
        self.base.actor_id()
    }
    fn with_actor(&self, actor: SessionActor) -> Result<Arc<dyn ToolHost>> {
        Ok(Arc::new(Self {
            base: SessionHost::new(self.base.handle.clone(), self.base.origin, actor),
            lease: self.lease.clone(),
        }))
    }
    fn close_task_data(&self, wait: Duration) -> Result<Option<TaskDataStatus>> {
        self.lease.data.close(wait).map(Some)
    }
}

#[cfg(all(test, unix, feature = "parquet"))]
mod tests {
    use super::*;
    use crate::{
        session::{ActivityOrigin, ActorKind, SessionHandle, task_data::TaskDataPolicy},
        signals::{Line, Quality, SourceState, TagInfo, Value as SignalValue},
        storage::StoragePolicy,
    };
    use std::{
        io::{BufReader, Cursor},
        time::Instant,
    };

    struct Fixture {
        root: PathBuf,
        session: SessionHandle,
        base: SessionHost,
        host: Option<Arc<dyn ToolHost>>,
        data: TaskDataHandle,
        shared_slot: String,
    }
    fn nonce() -> String {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).unwrap();
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
    fn policy(root: &Path) -> TaskDataPolicy {
        TaskDataPolicy {
            parent: root.join("private"),
            storage: StoragePolicy::default(),
            max_bytes: 16 << 20,
            lifetime: Duration::from_secs(30),
            name: "Native host test".into(),
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "capturefab-native-task-host-{}-{}",
                std::process::id(),
                nonce()
            ));
            crate::ipc::ensure_private_dir(&root).unwrap();
            crate::ipc::ensure_private_dir(&root.join("private")).unwrap();
            let session = SessionHandle::new();
            let base = SessionHost::new(
                session.clone(),
                ActivityOrigin::Gui,
                SessionActor::new("native-human", "Native human", ActorKind::Human).unwrap(),
            );
            let opened = base
                .execute(
                    None,
                    SessionCommand::DatasetOpen {
                        path: root.join("human.duckdb").display().to_string(),
                        mode: OpenMode::Create,
                        max_bytes: None,
                    },
                )
                .unwrap();
            let shared_slot = opened["slot_id"].as_str().unwrap().to_string();
            base.execute(
                None,
                SessionCommand::DatasetNote {
                    text: "human sentinel".into(),
                    context: Default::default(),
                    camera: None,
                },
            )
            .unwrap();
            let actor =
                SessionActor::new("native-agent", "Native agent", ActorKind::Agent).unwrap();
            let host = base
                .with_actor(actor)
                .unwrap()
                .with_task_data(policy(&root))
                .unwrap();
            let data = session.task_data_workspaces().pop().unwrap();
            Self {
                root,
                session,
                base,
                host: Some(host),
                data,
                shared_slot,
            }
        }
        fn host(&self) -> &dyn ToolHost {
            self.host.as_ref().unwrap().as_ref()
        }
        fn slot(&self) -> String {
            self.data.status().catalog.identity.slot_id
        }
        fn assert_shared_untouched(&self) {
            assert_eq!(
                self.base
                    .snapshot()
                    .unwrap()
                    .dataset
                    .unwrap()
                    .slot_id
                    .as_deref(),
                Some(self.shared_slot.as_str())
            );
            let rows = self
                .base
                .execute(
                    None,
                    SessionCommand::DatasetQuery {
                        sql: "SELECT text FROM events WHERE kind='note' ORDER BY id".into(),
                        limit: 10,
                        timeout_ms: 10_000,
                    },
                )
                .unwrap();
            assert_eq!(rows["rows"], json!([["human sentinel"]]));
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.host.take();
            let _ = self.data.remove(Duration::from_secs(5));
            let _ = self.base.execute(None, SessionCommand::DatasetClose);
            self.session.shutdown();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "native host condition not reached"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn limits() -> CaptureLimits {
        CaptureLimits {
            duration_ms: 1000,
            max_samples: 2,
            max_bytes: 4096,
        }
    }
    fn wire() -> String {
        let ready = Line::Ready {
            tags: vec![TagInfo {
                tag: "value".into(),
                unit: None,
                kind: None,
            }],
        };
        let samples = Line::Samples {
            samples: [12.5, 18.0]
                .into_iter()
                .enumerate()
                .map(|(i, value)| crate::signals::Sample {
                    ts_ms: i as u64 + 1,
                    source_ts_ms: None,
                    tag: "value".into(),
                    value: SignalValue::Number(value),
                    quality: Quality::Good,
                })
                .collect(),
        };
        format!(
            "{}\n{}\n",
            serde_json::to_string(&ready).unwrap(),
            serde_json::to_string(&samples).unwrap()
        )
    }

    #[test]
    fn native_task_host_keeps_shared_dataset_separate_and_refuses_untrusted_selectors() {
        let f = Fixture::new();
        let status = f.data.status();
        let snapshot = f.host().snapshot().unwrap();
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let actual_path = Path::new(&status.catalog.dataset.path);
        assert!(!encoded.contains(actual_path.to_str().unwrap()));
        assert!(!encoded.contains(actual_path.parent().unwrap().to_str().unwrap()));
        assert_eq!(snapshot.dataset.as_ref().unwrap().path, "task.duckdb");
        assert_eq!(snapshot.dataset.unwrap().slot_id, Some(f.slot()));
        assert_eq!(
            snapshot.signal_sink.unwrap().generation,
            status.sink.generation
        );
        assert_ne!(
            status.sink.generation,
            f.base.snapshot().unwrap().signal_sink.unwrap().generation
        );
        assert_eq!(f.host().task_data_reserved_bytes(), 16 << 20);
        let opened = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetOpenIf {
                    expected_slot_id: Some(f.slot()),
                    path: status.catalog.dataset.path.clone(),
                    mode: OpenMode::Open,
                    max_bytes: Some(16 << 20),
                },
            )
            .unwrap();
        assert_eq!(opened["slot_id"], f.slot());
        assert_eq!(opened["path"], "task.duckdb");
        assert_eq!(opened["sink_generation"], status.sink.generation);
        let foreign = f.shared_slot.clone();
        let output = f.root.join("forbidden-export").display().to_string();
        let commands = vec![
            SessionCommand::DatasetOpen {
                path: f.root.join("forbidden.duckdb").display().to_string(),
                mode: OpenMode::Create,
                max_bytes: None,
            },
            SessionCommand::DatasetClose,
            SessionCommand::DatasetOpenIf {
                expected_slot_id: None,
                path: status.catalog.dataset.path.clone(),
                mode: OpenMode::Open,
                max_bytes: None,
            },
            SessionCommand::DatasetOpenIf {
                expected_slot_id: Some(foreign.clone()),
                path: status.catalog.dataset.path,
                mode: OpenMode::Open,
                max_bytes: None,
            },
            SessionCommand::DatasetOpenIf {
                expected_slot_id: Some(f.slot()),
                path: f.root.join("human.duckdb").display().to_string(),
                mode: OpenMode::Open,
                max_bytes: None,
            },
            SessionCommand::DatasetCloseIf {
                slot_id: foreign.clone(),
                sink_generation: None,
            },
            SessionCommand::DatasetCloseIf {
                slot_id: f.slot(),
                sink_generation: Some(status.sink.generation + 1),
            },
            SessionCommand::DatasetNote {
                text: "wrong dataset".into(),
                context: Default::default(),
                camera: None,
            },
            SessionCommand::DatasetNoteIf {
                slot_id: foreign.clone(),
                text: "wrong dataset".into(),
                context: Default::default(),
                camera: None,
            },
            SessionCommand::DatasetQuery {
                sql: "SELECT 1".into(),
                limit: 1,
                timeout_ms: 10_000,
            },
            SessionCommand::DatasetQueryIf {
                slot_id: f.slot(),
                sql: "SELECT 1".into(),
                limit: 1,
                timeout_ms: 10_000,
                max_result_bytes: 4096,
            },
            SessionCommand::DatasetModelQueryIf {
                slot_id: foreign.clone(),
                sql: "SELECT 1".into(),
                limit: 1,
                timeout_ms: 10_000,
                max_result_bytes: 4096,
            },
            SessionCommand::DatasetExport {
                output: output.clone(),
                formats: vec![],
                tables: vec![],
                since_ms: None,
                until_ms: None,
                storage: Default::default(),
            },
            SessionCommand::DatasetExportIf {
                slot_id: foreign.clone(),
                output: output.clone(),
                approved_root: Some(f.root.display().to_string()),
                formats: vec![crate::dataset::ExportFormat::Csv],
                tables: vec!["samples".into()],
                since_ms: None,
                until_ms: None,
                storage: Default::default(),
                max_bytes: 1 << 20,
                timeout_ms: 10_000,
            },
            SessionCommand::SignalCapture {
                uri: "sim-signal://forbidden-private".into(),
                auth: None,
                interval_ms: 100,
                tags: vec![],
                duration_ms: 1000,
                max_samples: 2,
                max_bytes: 4096,
                slot_id: foreign,
                sink_generation: None,
            },
            SessionCommand::SignalCapture {
                uri: "sim-signal://forbidden-private".into(),
                auth: None,
                interval_ms: 100,
                tags: vec![],
                duration_ms: 1000,
                max_samples: 2,
                max_bytes: 4096,
                slot_id: f.slot(),
                sink_generation: Some(status.sink.generation + 1),
            },
            SessionCommand::SignalStart {
                uri: "sim-signal://forbidden-private".into(),
                auth: None,
                interval_ms: 100,
                tags: vec![],
            },
            SessionCommand::SignalStop { id: "all".into() },
        ];
        for command in commands {
            assert!(
                f.host().execute(None, command.clone()).is_err(),
                "shared fallback: {command:?}"
            );
        }
        assert!(!Path::new(&output).exists());
        assert!(!f.root.join("forbidden.duckdb").exists());
        assert!(f.base.snapshot().unwrap().signals.is_empty());
        assert_eq!(f.data.status().catalog.dataset.events, 0);
        f.assert_shared_untouched();
    }

    #[test]
    fn native_task_host_uses_private_rows_notes_and_actual_conditional_stop_receipts() {
        let f = Fixture::new();
        let source = f
            .data
            .capture_with_reader(
                "sim-signal://native-owned",
                limits(),
                Box::new(Cursor::new(wire())),
            )
            .unwrap();
        wait(|| f.data.status().catalog.dataset.samples == 2);
        let cached = f
            .host()
            .snapshot()
            .unwrap()
            .signals
            .into_iter()
            .find(|s| s.id == source.id)
            .unwrap();
        assert_eq!(cached.capture_id, source.capture_id);
        assert_eq!(cached.dataset_slot_id, Some(f.slot()));
        let signals = f.host().execute(None, SessionCommand::Signals).unwrap();
        assert_eq!(signals["sink"]["dataset_slot_id"], f.slot());
        assert_eq!(signals["sources"][0]["capture_id"], source.capture_id);
        let rows = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetModelQueryIf {
                    slot_id: f.slot(),
                    sql: "SELECT value_double * 2 AS doubled FROM samples ORDER BY ts_ms".into(),
                    limit: 10,
                    timeout_ms: 10_000,
                    max_result_bytes: 16_384,
                },
            )
            .unwrap();
        assert_eq!(rows["rows"], json!([[25.0], [36.0]]));
        f.host()
            .execute(
                None,
                SessionCommand::DatasetNoteIf {
                    slot_id: f.slot(),
                    text: "private note".into(),
                    context: Default::default(),
                    camera: None,
                },
            )
            .unwrap();
        let notes = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetModelQueryIf {
                    slot_id: f.slot(),
                    sql: "SELECT text FROM events WHERE kind='note' ORDER BY id".into(),
                    limit: 10,
                    timeout_ms: 10_000,
                    max_result_bytes: 16_384,
                },
            )
            .unwrap();
        assert_eq!(notes["rows"], json!([["private note"]]));
        let output = f.root.join("private-bundle");
        let exported = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetExportIf {
                    slot_id: f.slot(),
                    output: output.display().to_string(),
                    approved_root: Some(f.root.display().to_string()),
                    formats: vec![crate::dataset::ExportFormat::Csv],
                    tables: vec!["samples".into(), "events".into()],
                    since_ms: None,
                    until_ms: None,
                    storage: StoragePolicy::default(),
                    max_bytes: 1 << 20,
                    timeout_ms: 3000,
                },
            )
            .unwrap();
        assert_eq!(exported["slot_id"], f.slot());
        let csv = std::fs::read_to_string(output.join("samples.csv")).unwrap();
        let mut lines = csv.lines();
        let column = lines
            .next()
            .unwrap()
            .split(',')
            .position(|name| name == "value_double")
            .unwrap();
        let mut values = lines
            .map(|line| line.split(',').nth(column).unwrap().parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        assert_eq!(values, vec![12.5, 18.0]);
        let events = std::fs::read_to_string(output.join("events.csv")).unwrap();
        assert!(events.contains("private note") && !events.contains("human sentinel"));
        let stopped = f
            .host()
            .execute(
                None,
                SessionCommand::SignalStopIf {
                    id: source.id.clone(),
                    capture_id: source.capture_id.clone(),
                },
            )
            .unwrap();
        assert_eq!(stopped["stopped"][0]["capture_id"], source.capture_id);
        assert_eq!(stopped["stopped"][0]["written"], 2);
        assert_eq!(stopped["stopped"][0]["persistence_complete"], true);
        let status = f.host().execute(None, SessionCommand::Status).unwrap();
        assert_eq!(status["dataset"]["slot_id"], f.slot());
        let activity = f.base.snapshot().unwrap().activity;
        for operation in ["dataset_note", "dataset_export", "signal_stop_if"] {
            let events = activity
                .iter()
                .filter(|event| {
                    event.operation == operation
                        && event
                            .actor
                            .as_ref()
                            .is_some_and(|actor| actor.id == "native-agent")
                })
                .collect::<Vec<_>>();
            assert_eq!(events.len(), 1, "{operation}: {activity:?}");
            let event = events[0];
            assert_eq!(event.actor.as_ref().unwrap().id, "native-agent");
            assert_eq!(event.origin, ActivityOrigin::Gui);
        }
        let closed_reply = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetCloseIf {
                    slot_id: f.slot(),
                    sink_generation: Some(f.data.status().sink.generation),
                },
            )
            .unwrap();
        assert_eq!(closed_reply["closed"]["path"], "task.duckdb");
        let closed = f
            .host()
            .close_task_data(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(closed.catalog.closed);
        assert_ne!(closed.catalog.dataset.path, "task.duckdb");
        assert!(f.host().snapshot().unwrap().dataset.is_none());
        assert_eq!(f.host().task_data_reserved_bytes(), 0);
        f.assert_shared_untouched();
    }

    #[test]
    fn native_task_host_preserves_replacement_identity_and_cannot_stop_other_owners() {
        let f = Fixture::new();
        let uri = "sim-signal://native-replacement";
        let first = f
            .data
            .capture_with_reader(uri, limits(), Box::new(Cursor::new(wire())))
            .unwrap();
        wait(|| f.data.status().catalog.dataset.samples == 2);
        let other = f.session.begin_task_data(policy(&f.root)).unwrap();
        let (reader, writer) = std::io::pipe().unwrap();
        let second = other
            .capture_with_reader(uri, limits(), Box::new(BufReader::new(reader)))
            .unwrap();
        assert_ne!(first.capture_id, second.capture_id);
        let actual = f
            .host()
            .snapshot()
            .unwrap()
            .signals
            .into_iter()
            .find(|s| s.id == first.id)
            .unwrap();
        assert_eq!(actual.capture_id, second.capture_id);
        assert_eq!(
            actual.dataset_slot_id,
            Some(other.status().catalog.identity.slot_id)
        );
        for token in [first.capture_id, second.capture_id.clone()] {
            assert!(
                f.host()
                    .execute(
                        None,
                        SessionCommand::SignalStopIf {
                            id: first.id.clone(),
                            capture_id: token
                        }
                    )
                    .is_err()
            );
        }
        let after = f
            .base
            .snapshot()
            .unwrap()
            .signals
            .into_iter()
            .find(|s| s.id == first.id)
            .unwrap();
        assert_eq!(after.capture_id, second.capture_id);
        assert_eq!(after.state, SourceState::Connecting);
        drop(writer);
        other.remove(Duration::from_secs(2)).unwrap();
        f.assert_shared_untouched();
    }

    #[test]
    fn native_task_host_actor_views_share_one_capability_until_the_last_drop() {
        let mut f = Fixture::new();
        let peer = f
            .host()
            .with_actor(SessionActor::new("native-peer", "Native peer", ActorKind::Agent).unwrap())
            .unwrap();
        assert_eq!(peer.actor_id().as_deref(), Some("native-peer"));
        assert_eq!(f.host().actor_id().as_deref(), Some("native-agent"));
        assert!(peer.with_task_data(policy(&f.root)).is_err());
        assert_eq!(f.session.task_data_workspaces().len(), 1);
        f.host.take();
        assert_eq!(f.data.status().phase, TaskDataPhase::Open);
        assert_eq!(
            peer.snapshot().unwrap().dataset.unwrap().slot_id,
            Some(f.slot())
        );
        drop(peer);
        wait(|| f.data.status().phase == TaskDataPhase::Retained);
        assert!(f.data.status().catalog.closed);
        f.assert_shared_untouched();
    }

    #[test]
    fn unsupported_tool_hosts_refuse_private_storage_before_io() {
        let root = std::env::temp_dir().join(format!("capturefab-no-private-host-{}", nonce()));
        let no_session = super::super::NoSession;
        let ipc = super::super::IpcHost::new("no-private-task-server", None, None);
        for host in [&no_session as &dyn ToolHost, &ipc] {
            assert!(!host.supports_task_data());
            assert_eq!(host.task_data_reserved_bytes(), 0);
            assert!(host.task_data_status().is_none());
            host.request_close_task_data();
            assert!(host.with_task_data(policy(&root)).is_err());
            assert!(host.close_task_data(Duration::ZERO).unwrap().is_none());
        }
        assert!(!root.exists());
    }

    #[test]
    fn native_task_host_protects_its_engine_paths_without_rewriting_values_or_identities() {
        let f = Fixture::new();
        let mut status = f.data.status();
        let actual = status.catalog.dataset.path.clone();
        let parent = Path::new(&actual).parent().unwrap().display().to_string();
        let error = format!("database failed at {actual}; retained files in {parent}");
        status.catalog.dataset.last_error = Some(error.clone());
        status.sink.error = Some(error.clone());
        let source = crate::signals::source_id("sim-signal://protected-engine-errors");
        let token = "0123456789abcdef0123456789abcdef";
        status.captures.insert(source.clone(), token.into());
        let uri = format!("http://example.test/{parent}");
        let mut values = json!([{
            "id": source, "capture_id": token, "uri": uri,
            "error": error, "persistence_error": error,
            "tags": [{"tag":"literal", "value": actual}]
        }, {
            "id":source, "capture_id":"ffffffffffffffffffffffffffffffff", "error":error
        }]);
        NativeTaskHost::source_errors(&mut values, &status);
        assert_eq!(values[0]["id"], source);
        assert_eq!(values[0]["capture_id"], token);
        assert_eq!(values[0]["uri"], uri);
        assert_eq!(values[0]["tags"][0]["value"], actual);
        assert!(!values[0]["error"].as_str().unwrap().contains(&parent));
        assert_eq!(
            values[1]["error"], error,
            "replacement errors are not attributed to this task"
        );
        let dataset = NativeTaskHost::public_dataset(status.catalog.dataset.clone());
        assert_eq!(dataset.path, "task.duckdb");
        assert!(!dataset.last_error.unwrap().contains(&parent));
        assert!(
            !NativeTaskHost::public_sink(&status)
                .error
                .unwrap()
                .contains(&parent)
        );
        let rejected = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetModelQueryIf {
                    slot_id: f.slot(),
                    sql: format!("SELECT \"{actual}\" FROM samples"),
                    limit: 1,
                    timeout_ms: 10_000,
                    max_result_bytes: 4096,
                },
            )
            .unwrap_err();
        assert!(!format!("{rejected:#}").contains(&parent));
        let outside = f.root.join("retained-export").display().to_string();
        assert_eq!(NativeTaskHost::scrub_text(&actual, &outside), outside);
        let native = NativeTaskHost::new(
            SessionHost::new(
                f.session.clone(),
                ActivityOrigin::Gui,
                SessionActor::new("native-error-view", "Error view", ActorKind::Agent).unwrap(),
            ),
            f.data.clone(),
        );
        let stage_parent = Path::new(&parent).parent().unwrap();
        for (id, path) in [
            (
                Some("private-export-reservation".to_owned()),
                stage_parent.join("task-private-export-reservation"),
            ),
            (None, stage_parent.to_owned()),
        ] {
            let retained = crate::dataset::task::RetainedExportStage::at(id.clone(), path.clone());
            let local = anyhow::anyhow!("could not close staging; approved export {outside}")
                .context(retained)
                .context("export failed");
            assert!(format!("{local:#}").contains(&path.display().to_string()));
            let public = format!("{:#}", native.public_error(local));
            assert!(!public.contains(&path.display().to_string()));
            if let Some(id) = id {
                assert!(!public.contains(&id));
                assert!(
                    public.contains(&outside),
                    "a reviewed output remains identifiable"
                );
            }
            assert!(public.contains("inspect storage status"));
        }
        let refused = f
            .host()
            .execute(
                None,
                SessionCommand::DatasetExportIf {
                    slot_id: f.slot(),
                    output: f.root.join("refused-bundle").display().to_string(),
                    approved_root: Some(f.root.display().to_string()),
                    formats: vec![crate::dataset::ExportFormat::Csv],
                    tables: vec!["samples".into()],
                    since_ms: None,
                    until_ms: None,
                    storage: StoragePolicy {
                        max_bytes: 1,
                        ..StoragePolicy::default()
                    },
                    max_bytes: 4096,
                    timeout_ms: 10_000,
                },
            )
            .unwrap_err();
        assert!(format!("{refused:#}").contains("[task export staging]"));
        let activity = f.base.snapshot().unwrap().activity;
        let failed = activity
            .iter()
            .find(|event| {
                event.operation == "dataset_export"
                    && event
                        .actor
                        .as_ref()
                        .is_some_and(|actor| actor.id == "native-agent")
            })
            .unwrap();
        assert_eq!(failed.state, crate::session::ActivityState::Failed);
        let detail = failed.detail.as_deref().unwrap();
        assert!(detail.contains("[task export staging]"));
        assert!(!detail.contains(stage_parent.to_str().unwrap()));
        assert!(
            !serde_json::to_string(&f.host().snapshot().unwrap())
                .unwrap()
                .contains(stage_parent.to_str().unwrap())
        );
        assert_eq!(
            f.data.status().catalog.dataset.path,
            actual,
            "local receipt retains the recovery locator"
        );
    }
}
