use super::*;
use crate::signals::{Line, Quality, TagInfo, Value};
use std::io::{BufReader, Cursor, Write};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "capturefab-task-registry-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn policy(&self, lifetime: Duration) -> TaskDataPolicy {
        TaskDataPolicy {
            parent: self.0.clone(),
            storage: StoragePolicy::default(),
            max_bytes: 16 << 20,
            lifetime,
            name: "local task capture".into(),
        }
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until, "task condition did not become true");
        thread::sleep(Duration::from_millis(5));
    }
}
fn limits() -> CaptureLimits {
    CaptureLimits {
        duration_ms: 1000,
        max_samples: 2,
        max_bytes: 4096,
    }
}
fn ready() -> String {
    format!(
        "{}\n",
        serde_json::to_string(&Line::Ready {
            tags: vec![TagInfo {
                tag: "value".into(),
                unit: None,
                kind: None
            }],
        })
        .unwrap()
    )
}
fn samples(values: &[f64]) -> String {
    format!(
        "{}\n",
        serde_json::to_string(&Line::Samples {
            samples: values
                .iter()
                .enumerate()
                .map(|(i, value)| Sample {
                    ts_ms: 100 + i as u64,
                    source_ts_ms: None,
                    tag: "value".into(),
                    value: Value::Number(*value),
                    quality: Quality::Good,
                })
                .collect(),
        })
        .unwrap()
    )
}
fn capture_reader(
    handle: &TaskDataHandle,
    reader: impl std::io::BufRead + Send + 'static,
) -> SourceStatus {
    handle
        .entry
        .capture_receipt(unacknowledged_capture(handle, reader))
        .unwrap()
}

fn unacknowledged_capture(
    handle: &TaskDataHandle,
    reader: impl std::io::BufRead + Send + 'static,
) -> SourceStatus {
    let registry = handle.registry.upgrade().unwrap();
    registry.check(&handle.entry).unwrap();
    let owner = registry.owner().unwrap();
    owner
        .signals
        .capture_to_private_with_reader(
            &handle.entry.route,
            "sim-signal://registry-owned",
            limits(),
            Box::new(reader),
        )
        .unwrap()
}
fn reserved(handle: &TaskDataHandle) -> bool {
    let status = crate::storage::quota_status().unwrap();
    status["retained_namespaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| {
            item["id"] == handle.entry.lease.id()
                && item["allocated_bytes"] == handle.entry.lease.cap()
        })
}

#[test]
fn typed_private_capture_keeps_its_catalog_and_charge_until_explicit_removal() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(10)))
        .unwrap();
    let source = capture_reader(&handle, Cursor::new(ready() + &samples(&[12.5, 18.0])));
    wait(|| handle.status().catalog.dataset.samples == 2);
    let status = handle.close(Duration::from_secs(2)).unwrap();
    assert_eq!(status.phase, TaskDataPhase::Retained);
    assert!(status.catalog.closed && !status.sink.attached);
    assert_eq!(status.captures.get(&source.id), Some(&source.capture_id));
    assert!(reserved(&handle));
    assert!(session.inner.dataset.status_cached().is_none());
    let path = PathBuf::from(&status.catalog.dataset.path);
    {
        let dataset = crate::dataset::Dataset::open_read_only(&path).unwrap();
        let rows = dataset
            .reader()
            .unwrap()
            .query("SELECT value_double FROM samples ORDER BY ts_ms", 10, 1000)
            .unwrap();
        assert_eq!(rows["rows"], serde_json::json!([[12.5], [18.0]]));
        assert_eq!(
            dataset.status().dataset_id,
            status.catalog.identity.dataset_id
        );
    }
    let removed = handle.remove(Duration::from_secs(2)).unwrap();
    assert_eq!(removed.phase, TaskDataPhase::Removed);
    assert_eq!(removed.catalog.extents.used, 0);
    assert!(session.task_data_workspaces().is_empty());
    assert!(handle.entry.capture_receipt(source.clone()).is_err());
    assert!(!path.parent().unwrap().exists());
    assert!(!reserved(&handle));
    assert_eq!(
        handle.remove(Duration::ZERO).unwrap().phase,
        TaskDataPhase::Removed
    );
    session.shutdown();
}

#[test]
fn registry_rejects_foreign_coordinators_and_unowned_capture_tokens() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let other = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(10)))
        .unwrap();
    session.check_task_data(&handle).unwrap();
    assert!(
        other
            .check_task_data(&handle)
            .unwrap_err()
            .to_string()
            .contains("another coordinator")
    );
    let error = handle
        .stop_capture("signal:0000000000000000", "forged", Duration::ZERO)
        .unwrap_err();
    assert!(error.to_string().contains("not owned"));
    handle.remove(Duration::from_secs(2)).unwrap();
    session.shutdown();
    other.shutdown();
}

#[test]
fn late_start_acknowledgment_cannot_resurrect_a_removed_workspace() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(10)))
        .unwrap();
    let source = unacknowledged_capture(&handle, Cursor::new(ready() + &samples(&[1.0, 2.0])));
    wait(|| handle.status().catalog.dataset.samples == 2);
    assert!(handle.status().captures.is_empty());
    handle.remove(Duration::from_secs(2)).unwrap();
    assert!(handle.entry.capture_receipt(source).is_err());
    assert_eq!(handle.status().phase, TaskDataPhase::Removed);
    assert!(handle.status().captures.is_empty());
    assert!(session.task_data_workspaces().is_empty());
    session.shutdown();
}

#[test]
fn client_drop_does_not_cancel_coordinator_deadline_or_refund_storage() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(2)))
        .unwrap();
    let id = handle.status().id;
    drop(handle);
    let recovery = session
        .task_data_workspaces()
        .into_iter()
        .find(|h| h.status().id == id)
        .unwrap();
    assert!(reserved(&recovery));
    wait(|| recovery.status().phase == TaskDataPhase::Retained);
    assert!(recovery.status().catalog.closed && reserved(&recovery));
    assert!(
        recovery
            .capture("sim-signal://late", 100, vec![], limits(), None)
            .is_err()
    );
    recovery.remove(Duration::from_secs(2)).unwrap();
    session.shutdown();
}

#[test]
fn timed_out_close_keeps_engine_and_lease_until_reader_exits_and_deadline_retries() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(2)))
        .unwrap();
    let (reader, mut writer) = std::io::pipe().unwrap();
    capture_reader(&handle, BufReader::new(reader));
    writer.write_all(ready().as_bytes()).unwrap();
    writer.write_all(samples(&[1.0]).as_bytes()).unwrap();
    wait(|| handle.status().catalog.dataset.samples == 1);
    assert!(handle.close(Duration::from_millis(20)).is_err());
    assert_eq!(handle.status().phase, TaskDataPhase::Closing);
    assert!(!handle.status().catalog.closed);
    assert!(reserved(&handle));
    drop(writer);
    wait(|| handle.status().phase == TaskDataPhase::Retained);
    assert!(handle.status().catalog.closed && reserved(&handle));
    handle.remove(Duration::from_secs(2)).unwrap();
    session.shutdown();
}

#[test]
fn explicit_shutdown_and_owner_drop_close_engines_without_refunding_files() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(10)))
        .unwrap();
    session.shutdown();
    assert!(handle.status().catalog.closed && reserved(&handle));
    handle.remove(Duration::from_secs(2)).unwrap();
    drop(session);

    let session = SessionHandle::new();
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(10)))
        .unwrap();
    let owner = Arc::downgrade(&session.inner);
    drop(session);
    wait(|| owner.strong_count() == 0 && handle.status().catalog.closed);
    assert!(reserved(&handle));
    // The capability still permits explicit cleanup after coordinator loss.
    handle.remove(Duration::from_secs(2)).unwrap();
    assert!(!reserved(&handle));
}

#[test]
fn cleanup_mutex_wait_is_bounded_and_invalid_names_never_reserve() {
    let temp = Temp::new();
    let session = SessionHandle::new();
    let mut policy = temp.policy(Duration::from_secs(10));
    policy.name = "invalid\nname".into();
    assert!(session.begin_task_data(policy).is_err());
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
    let expired = session
        .begin_task_data(temp.policy(Duration::from_nanos(1)))
        .unwrap_err();
    assert!(format!("{expired:#}").contains("setup exceeded its deadline"));
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
    let handle = session
        .begin_task_data(temp.policy(Duration::from_secs(10)))
        .unwrap();
    handle.close(Duration::from_secs(2)).unwrap();
    let turn = locked(&handle.entry.close_turn);
    let started = Instant::now();
    assert!(handle.remove(Duration::from_millis(20)).is_err());
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(reserved(&handle));
    drop(turn);
    handle.remove(Duration::from_secs(2)).unwrap();
    session.shutdown();
}
