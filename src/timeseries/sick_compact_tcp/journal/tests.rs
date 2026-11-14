use super::*;
use std::sync::{Arc, Mutex};
#[derive(Default)]
struct Memory {
    raw: Vec<u8>,
    fail_sync: bool,
    fail_rollback: bool,
    kept: Option<bool>,
}
struct Fake(Arc<Mutex<Memory>>);
impl Sink for Fake {
    fn append(&mut self, raw: &[u8]) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
            .raw
            .extend_from_slice(raw);
        Ok(())
    }
    fn sync(&mut self, _first: bool) -> Result<()> {
        ensure!(
            !self
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
                .fail_sync,
            "injected native sync failure"
        );
        Ok(())
    }
    fn rollback(&mut self, length: u64) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?;
        ensure!(!state.fail_rollback, "injected rollback failure");
        state.raw.truncate(usize::try_from(length)?);
        Ok(())
    }
    fn bytes(&self) -> u64 {
        u64::try_from(self.0.lock().unwrap().raw.len()).unwrap()
    }
    fn close(self: Box<Self>, keep: bool) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture mutex poisoned"))?
            .kept = Some(keep);
        Ok(())
    }
}
fn journal(directory: PathBuf) -> (Journal, Arc<Mutex<Memory>>) {
    let config:Config=serde_json::from_value(json!({"address":"127.0.0.1:2115","profile":"lrs4000","stream":"points","output_directory":directory,"max_bytes":20_000_000,"min_free_bytes":0})).unwrap();
    let mut journal = Journal::new(
        directory,
        &config,
        Context {
            raw: vec![0, 255],
            supplied: true,
        },
        1,
    )
    .unwrap();
    let state = Arc::new(Mutex::new(Memory::default()));
    journal.sink = Some(Box::new(Fake(Arc::clone(&state))));
    (journal, state)
}
#[test]
fn failed_sync_rolls_back_only_new_record_and_preserves_commit_hash_and_part_inventory() {
    let folder = tempfile::tempdir().unwrap();
    let (mut journal, state) = journal(folder.path().join("archive"));
    let raw = vec![2; 36];
    journal.commit(&raw, 10, 0, 3, None).unwrap();
    let old = state.lock().unwrap().raw.clone();
    let before = journal.prefix();
    state.lock().unwrap().fail_sync = true;
    let error = journal.commit(&raw, 20, 36, 4, None).unwrap_err();
    assert_eq!(
        crate::types::classify(&error).kind,
        ErrorKind::OperationFailed
    );
    assert_eq!(state.lock().unwrap().raw, old);
    assert_eq!(
        journal.prefix()["committed_bytes"],
        before["committed_bytes"]
    );
    assert_eq!(
        journal.prefix()["sha256_committed_prefix"],
        before["sha256_committed_prefix"]
    );
    assert_eq!(journal.prefix()["parts_admitted"], 3);
    assert_eq!(journal.telegrams, 1);
    assert_eq!(journal.prefix()["uncommitted_tail_bytes"], 0);
    journal.finish().unwrap();
    assert_eq!(state.lock().unwrap().kept, Some(true));
}
#[test]
fn rollback_failure_exposes_tail_without_claiming_an_extra_telegram_or_native_parts() {
    let folder = tempfile::tempdir().unwrap();
    let (mut journal, state) = journal(folder.path().join("archive"));
    let raw = vec![2; 36];
    journal.commit(&raw, 10, 0, 1, None).unwrap();
    let before = journal.prefix();
    {
        let mut state = state.lock().unwrap();
        state.fail_sync = true;
        state.fail_rollback = true;
    }
    let error = journal.commit(&raw, 20, 36, 5, None).unwrap_err();
    assert_eq!(
        crate::types::classify(&error).kind,
        ErrorKind::OperationFailed
    );
    let after = journal.prefix();
    assert_eq!(after["telegrams"], 1);
    assert_eq!(after["parts_admitted"], 1);
    assert_eq!(
        after["sha256_committed_prefix"],
        before["sha256_committed_prefix"]
    );
    assert!(after["uncommitted_tail_bytes"].as_u64().unwrap() > 0);
    assert!(after["rollback_error"].as_str().is_some());
    journal.finish().unwrap();
    assert_eq!(state.lock().unwrap().kept, Some(true));
}
#[test]
fn byte_ceiling_and_changed_floor_refuse_before_append_preserving_the_old_prefix() {
    let folder = tempfile::tempdir().unwrap();
    let (mut journal, state) = journal(folder.path().join("archive"));
    let raw = vec![2; 36];
    journal.commit(&raw, 10, 0, 1, None).unwrap();
    let before = state.lock().unwrap().raw.clone();
    journal.archive_limit = journal.committed;
    assert_eq!(
        crate::types::classify(&journal.commit(&raw, 20, 36, 1, None).unwrap_err()).kind,
        ErrorKind::StorageFull
    );
    assert_eq!(state.lock().unwrap().raw, before);
    journal.archive_limit = 20_000_000;
    journal.min_free = Some(u64::MAX);
    assert_eq!(
        crate::types::classify(&journal.commit(&raw, 20, 36, 1, None).unwrap_err()).kind,
        ErrorKind::StorageFull
    );
    assert_eq!(state.lock().unwrap().raw, before);
    assert_eq!(journal.telegrams, 1);
    journal.finish().unwrap();
}
#[test]
fn evidence_storage_failure_is_typed_and_cannot_replace_original_decode_reason() {
    let folder = tempfile::tempdir().unwrap();
    let config:Config=serde_json::from_value(json!({"address":"127.0.0.1:2115","profile":"lrs4000","stream":"points","output_directory":folder.path().join("blocked"),"max_bytes":20_000_000,"min_free_bytes":u64::MAX})).unwrap();
    let mut journal = Journal::new(
        folder.path().join("blocked"),
        &config,
        Context {
            raw: vec![0, 255],
            supplied: true,
        },
        1,
    )
    .unwrap();
    let primary = super::super::incomplete("independent decoder failure");
    let evidence = journal
        .reject(b"bad", Some(1), 0, false, &primary)
        .unwrap_err();
    assert_eq!(
        crate::types::classify(&evidence).kind,
        ErrorKind::StorageFull
    );
    assert_eq!(crate::types::classify(&primary).kind, ErrorKind::Incomplete);
    assert_eq!(
        journal.prefix()["evidence_write_error"]["code"],
        "storage_full"
    );
    assert_eq!(journal.prefix()["diagnostic_bytes"], 0);
    assert!(journal.prefix().get("rejected_stream_prefix").is_none());
    journal.finish().unwrap();
    assert!(!folder.path().join("blocked").exists());
}
