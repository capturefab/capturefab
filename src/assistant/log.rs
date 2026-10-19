//! Private, bounded action logs. Only documented event fields reach disk;
//! provider wire bodies, opaque artifacts and camera pixels are never records.
use anyhow::{Context, Result, ensure};
use chrono::{TimeZone, Utc};
use fs2::FileExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

pub const MAX_RUN_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_RUNS: usize = 100;
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub struct LogFull;
impl fmt::Display for LogFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The assistant action log reached its size limit.")
    }
}
impl std::error::Error for LogFull {}

pub struct ActionLog {
    file: File,
    path: PathBuf,
    id: String,
    seq: u64,
    bytes: u64,
    ended: bool,
}
pub type Log = ActionLog;

impl fmt::Debug for ActionLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionLog")
            .field("id", &self.id)
            .field("seq", &self.seq)
            .field("bytes", &self.bytes)
            .field("ended", &self.ended)
            .finish()
    }
}

impl ActionLog {
    pub fn create(directory: &Path, now_ms: u64) -> Result<Self> {
        ensure_parent(directory)?;
        crate::ipc::ensure_private_dir(directory)?;
        let directory = fs::canonicalize(directory)?;
        let time = Utc
            .timestamp_millis_opt(i64::try_from(now_ms)?)
            .single()
            .ok_or_else(|| anyhow::anyhow!("Use a supported action-log timestamp."))?;
        for _ in 0..100 {
            let mut random = [0u8; 2];
            getrandom::fill(&mut random)
                .map_err(|_| anyhow::anyhow!("Could not allocate an assistant run ID."))?;
            let id = format!(
                "r-{}-{:02x}{:02x}",
                time.format("%Y%m%d-%H%M%S"),
                random[0],
                random[1]
            );
            ensure!(
                valid_run_id(&id),
                "Use an action-log timestamp before the year 10000."
            );
            let path = directory.join(format!("{id}.jsonl"));
            let file = match private_options().append(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => anyhow::bail!("Could not create the assistant action log."),
            };
            FileExt::try_lock_exclusive(&file).context("The assistant action log is busy.")?;
            let log = Self {
                file,
                path,
                id,
                seq: 0,
                bytes: 0,
                ended: false,
            };
            prune(&directory, MAX_RUNS, MAX_TOTAL_BYTES, Some(log.id()))?;
            return Ok(log);
        }
        anyhow::bail!("Could not allocate a unique assistant run ID.")
    }

    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn run_id(&self) -> &str {
        &self.id
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the exact redacted record so a CLI event stream and the GUI can
    /// show the same audit data that was written. Sequence numbers are local to
    /// the run and advance only after a complete write.
    pub fn record(&mut self, kind: &str, fields: Value, now_ms: u64) -> Result<Value> {
        ensure!(
            !self.ended,
            "The assistant action log already has an end record."
        );
        let allowed = fields_for(kind)
            .ok_or_else(|| anyhow::anyhow!("Use a documented assistant action-log event."))?;
        let mut record = serde_json::Map::new();
        if let Some(fields) = fields.as_object() {
            for key in allowed {
                if let Some(value) = fields.get(*key) {
                    let value = sanitize(value, key, 0);
                    let value = if *key != "goal" && serde_json::to_vec(&value)?.len() > 8192 {
                        json!({"truncated":true,"summary":"Log field exceeded 8 KiB."})
                    } else {
                        value
                    };
                    record.insert((*key).into(), value);
                }
            }
        }
        record.insert("v".into(), json!(1));
        record.insert("t".into(), json!(now_ms));
        record.insert("run".into(), json!(self.id));
        record.insert("seq".into(), json!(self.seq + 1));
        record.insert("kind".into(), json!(kind));
        let record = Value::Object(record);
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "Keep each assistant action-log record within 64 KiB."
        );
        let reserved = match kind {
            "end" => 0,
            "stop" => MAX_RECORD_BYTES as u64,
            "approval" => 2 * MAX_RECORD_BYTES as u64,
            _ => 3 * MAX_RECORD_BYTES as u64,
        };
        if self.bytes.saturating_add(bytes.len() as u64) > MAX_RUN_BYTES - reserved {
            return Err(LogFull.into());
        }
        self.file
            .write_all(&bytes)
            .context("Could not append the assistant action log.")?;
        self.file
            .flush()
            .context("Could not flush the assistant action log.")?;
        if matches!(kind, "approval" | "stop" | "end") {
            self.file
                .sync_all()
                .context("Could not sync the assistant action log.")?;
        }
        self.bytes += bytes.len() as u64;
        self.seq += 1;
        self.ended = kind == "end";
        if let Some(directory) = self.path.parent() {
            prune(directory, MAX_RUNS, MAX_TOTAL_BYTES, Some(&self.id))?;
        }
        Ok(record)
    }

    pub fn finish(&mut self, fields: Value, now_ms: u64) -> Result<Value> {
        self.record("end", fields, now_ms)
    }
}

impl Drop for ActionLog {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn fields_for(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "start" => &[
            "goal",
            "mode",
            "scope",
            "limits",
            "provider",
            "model",
            "effort",
            "app_version",
            "actor",
        ],
        "request" => &["segment", "round", "ms", "stop", "usage", "cost_usd"],
        "text" => &["segment", "round", "text"],
        "tool" => &[
            "call_id", "name", "risk", "args", "outcome", "issues", "summary", "commands",
        ],
        "observation" => &["camera", "frame_id", "frame_ts_ns", "sent", "metrics"],
        "approval" => &[
            "request_id",
            "tool",
            "risk",
            "summary",
            "decision",
            "scope",
            "by",
            "note",
            "waited_ms",
        ],
        "human_change" => &[
            "activity_id",
            "actor",
            "camera",
            "operation",
            "feature",
            "label",
        ],
        "rollover" => &["from", "to", "reason", "checkpoint_sha256"],
        "limit" => &["name", "used", "max"],
        "stop" => &["by", "reason"],
        "end" => &["outcome", "summary", "totals", "leftovers"],
        _ => return None,
    })
}

fn sensitive_key(key: &str, value: &Value) -> bool {
    let key = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        key.as_str(),
        "raw"
            | "wire"
            | "headers"
            | "body"
            | "authorization"
            | "apikey"
            | "credentials"
            | "accesskeyid"
            | "secretaccesskey"
            | "password"
            | "secret"
            | "token"
            | "sessiontoken"
            | "base64"
            | "data"
            | "pixels"
            | "thinking"
            | "redactedthinking"
            | "reasoningcontent"
            | "reasoningdetails"
            | "encryptedcontent"
            | "signature"
            | "thoughtsignature"
    ) || (key == "reasoning" && !value.is_number())
        || ((key.contains("image")
            || key.contains("secret")
            || key.ends_with("token")
            || key.ends_with("signature"))
            && !value.is_number())
}

fn sanitize(value: &Value, key: &str, depth: usize) -> Value {
    if depth > 12 {
        return json!("<omitted>");
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, value)| !sensitive_key(key, value))
                .take(64)
                .map(|(key, value)| (key.clone(), sanitize(value, key, depth + 1)))
                .collect(),
        ),
        Value::Array(array) => {
            if array.len() >= 32
                && array
                    .iter()
                    .all(|value| value.as_u64().is_some_and(|n| n <= 255))
            {
                json!("<binary omitted>")
            } else {
                Value::Array(
                    array
                        .iter()
                        .take(64)
                        .map(|value| sanitize(value, key, depth + 1))
                        .collect(),
                )
            }
        }
        Value::String(text) => json!(scrub(text, if key == "goal" { 16 * 1024 } else { 4096 })),
        _ => value.clone(),
    }
}

fn scrub(text: &str, max: usize) -> String {
    if super::prompt::contains_secret(text) {
        return "<redacted>".into();
    }
    let text = crate::tools::redact::for_output_text(text);
    let mut output = String::new();
    let mut bearer = false;
    for word in text.split_inclusive(char::is_whitespace) {
        let token = word.trim_end_matches(char::is_whitespace);
        let whitespace = &word[token.len()..];
        if bearer {
            output.push_str("<redacted>");
            bearer = false;
        } else if token.starts_with("data:image/")
            || (token.len() >= 128
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"+/=_-".contains(&byte)))
        {
            output.push_str("<binary omitted>");
        } else {
            output.extend(token.chars().filter(|c| !c.is_control()));
            bearer = token.eq_ignore_ascii_case("bearer");
        }
        output.push_str(whitespace);
    }
    if output.len() > max {
        let mut end = max.saturating_sub(3);
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push('…');
    }
    output
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn ensure_parent(directory: &Path) -> Result<()> {
    if let Some(parent) = directory.parent()
        && parent.file_name().is_some_and(|name| name == "assistant")
        && fs::symlink_metadata(parent).is_ok()
    {
        check_directory(parent)?;
    }
    Ok(())
}

fn check_directory(path: &Path) -> Result<()> {
    ensure_parent(path)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "Use a regular private assistant log directory."
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "Keep assistant logs private (chmod 700)."
        );
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= MAX_RUN_BYTES,
        "Use a regular private assistant action log within 16 MiB."
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "Keep assistant logs private (chmod 600)."
        );
    }
    Ok(meta)
}

pub fn valid_run_id(id: &str) -> bool {
    id.len() == 22
        && id.starts_with("r-")
        && id.as_bytes()[10] == b'-'
        && id.as_bytes()[17] == b'-'
        && id.as_bytes()[2..10].iter().all(u8::is_ascii_digit)
        && id.as_bytes()[11..17].iter().all(u8::is_ascii_digit)
        && id.as_bytes()[18..].iter().all(u8::is_ascii_hexdigit)
}

fn paths(directory: &Path) -> Result<Vec<(String, PathBuf, u64)>> {
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    check_directory(directory)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl").filter(|id| valid_run_id(id)) else {
            continue;
        };
        if let Ok(meta) = private_file(&entry.path()) {
            paths.push((id.to_owned(), entry.path(), meta.len()));
        }
    }
    paths.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(paths)
}

fn open_checked(path: &Path) -> Result<File> {
    let before = private_file(path)?;
    let file = File::open(path)?;
    let after = private_file(path)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == after.dev()
                && before.ino() == after.ino()
                && after.dev() == opened.dev()
                && after.ino() == opened.ino(),
            "The assistant log changed while opening it."
        );
    }
    #[cfg(not(unix))]
    {
        ensure!(
            before.len() == after.len() && after.len() == opened.len() && opened.is_file(),
            "The assistant log changed while opening it."
        );
    }
    Ok(file)
}

fn prune(directory: &Path, max_runs: usize, max_bytes: u64, current: Option<&str>) -> Result<()> {
    let files = paths(directory)?;
    let mut count = files.len();
    let mut total = files.iter().map(|file| file.2).sum::<u64>();
    for (id, path, bytes) in files.into_iter().rev() {
        if count <= max_runs && total <= max_bytes {
            break;
        }
        if current == Some(id.as_str()) {
            continue;
        }
        let Ok(file) = open_checked(&path) else {
            continue;
        };
        if FileExt::try_lock_exclusive(&file).is_err() {
            continue;
        }
        // Recheck the directory entry after locking, before deleting only this
        // validated run file. Never follow or remove arbitrary/symlink entries.
        let safe = private_file(&path).is_ok_and(|meta| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                file.metadata()
                    .is_ok_and(|opened| opened.dev() == meta.dev() && opened.ino() == meta.ino())
            }
            #[cfg(not(unix))]
            {
                meta.len() == bytes
            }
        });
        if safe && fs::remove_file(&path).is_ok() {
            count -= 1;
            total = total.saturating_sub(bytes);
        }
        let _ = FileExt::unlock(&file);
    }
    Ok(())
}

struct ReadLog {
    records: Vec<Value>,
    partial: bool,
}
fn read_in(directory: &Path, id: &str) -> Result<ReadLog> {
    ensure!(
        valid_run_id(id),
        "Choose an assistant run ID from the recent tasks list."
    );
    check_directory(directory)?;
    let path = directory.join(format!("{id}.jsonl"));
    let file = open_checked(&path)?;
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut bytes = 0u64;
    let mut seq = 1u64;
    let mut ended = false;
    loop {
        let mut line = Vec::new();
        let read = Read::by_ref(&mut reader)
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(ReadLog {
                records,
                partial: false,
            });
        }
        bytes = bytes.saturating_add(read as u64);
        ensure!(
            read <= MAX_RECORD_BYTES && bytes <= MAX_RUN_BYTES,
            "The assistant action log exceeds its size limit."
        );
        if line.last() != Some(&b'\n') {
            return Ok(ReadLog {
                records,
                partial: true,
            });
        }
        let value: Value = serde_json::from_slice(&line)
            .map_err(|_| anyhow::anyhow!("The assistant action log has an unreadable record."))?;
        ensure!(
            !ended
                && value["v"] == 1
                && value["run"] == id
                && value["seq"].as_u64() == Some(seq)
                && value["t"].as_u64().is_some()
                && value["kind"]
                    .as_str()
                    .is_some_and(|kind| fields_for(kind).is_some()),
            "The assistant action log has an invalid record."
        );
        ended = value["kind"] == "end";
        seq += 1;
        // Apply the same projection when reading a planted/tampered file.
        let kind = value["kind"].as_str().expect("checked");
        let allowed = fields_for(kind).expect("checked");
        let mut clean = serde_json::Map::new();
        for key in ["v", "t", "run", "seq", "kind"]
            .into_iter()
            .chain(allowed.iter().copied())
        {
            if let Some(value) = value.get(key) {
                clean.insert(key.into(), sanitize(value, key, 0));
            }
        }
        records.push(Value::Object(clean));
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub started_ms: Option<u64>,
    pub last_ms: Option<u64>,
    pub goal: String,
    pub outcome: Option<String>,
    pub summary: String,
    pub complete: bool,
    pub active: bool,
    pub path: PathBuf,
}

pub fn read_log_in(directory: &Path, id: &str) -> Result<Vec<Value>> {
    Ok(read_in(directory, id)?.records)
}
pub fn read_log(id: &str) -> Result<Vec<Value>> {
    read_log_in(&crate::ipc::session_dir().join("assistant/runs"), id)
}
pub fn runs_in(directory: &Path) -> Result<Vec<RunSummary>> {
    let mut runs = Vec::new();
    for (id, path, _) in paths(directory)? {
        let Ok(log) = read_in(directory, &id) else {
            continue;
        };
        let start = log.records.iter().find(|record| record["kind"] == "start");
        let end = log.records.last().filter(|record| record["kind"] == "end");
        let active = open_checked(&path).is_ok_and(|file| {
            let active = FileExt::try_lock_exclusive(&file).is_err();
            if !active {
                let _ = FileExt::unlock(&file);
            }
            active
        });
        runs.push(RunSummary {
            run_id: id,
            started_ms: start.and_then(|value| value["t"].as_u64()),
            last_ms: log.records.last().and_then(|value| value["t"].as_u64()),
            goal: start
                .and_then(|value| value["goal"].as_str())
                .unwrap_or_default()
                .into(),
            outcome: end
                .and_then(|value| value["outcome"].as_str())
                .map(str::to_owned),
            summary: end
                .and_then(|value| value["summary"].as_str())
                .unwrap_or_default()
                .into(),
            complete: end.is_some() && !log.partial,
            active,
            path,
        });
    }
    Ok(runs)
}
pub fn runs() -> Result<Vec<RunSummary>> {
    runs_in(&crate::ipc::session_dir().join("assistant/runs"))
}

#[derive(Default)]
pub struct CrashTracker {
    seen: HashSet<String>,
}
impl CrashTracker {
    pub fn take_latest_unfinished(&mut self, directory: &Path) -> Result<Option<RunSummary>> {
        let latest = runs_in(directory)?
            .into_iter()
            .find(|run| !run.complete && !run.active);
        Ok(latest.filter(|run| self.seen.insert(run.run_id.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cfa-action-log-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            crate::ipc::ensure_private_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn records_are_private_ordered_bounded_and_finished() {
        let root = Directory::new();
        let mut log = ActionLog::create(&root.0, 1792324800000).unwrap();
        assert!(valid_run_id(log.id()));
        let start=log.record("start",json!({"goal":"Tune the exposure","mode":"autonomous","provider":"anthropic","model":"claude-sonnet-5-5","scope":{"cameras":["sim:0"]},"raw":"must disappear"}),1000).unwrap();
        assert_eq!(start["seq"], 1);
        assert!(start.get("raw").is_none());
        log.record(
            "approval",
            json!({"decision":"approved","scope":"once","by":{"id":"desktop","name":"Desktop"}}),
            1001,
        )
        .unwrap();
        log.record(
            "stop",
            json!({"by":"desktop","reason":"user stopped"}),
            1002,
        )
        .unwrap();
        log.finish(json!({"outcome":"stopped","summary":"Settings changed; nothing is running.","totals":{"rounds":1,"images":1,"usage":{"input":120}}}),1003).unwrap();
        assert!(log.record("text", json!({"text":"late"}), 1004).is_err());
        let id = log.id().to_owned();
        let path = log.path().to_owned();
        drop(log);
        let records = read_log_in(&root.0, &id).unwrap();
        assert_eq!(records.len(), 4);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record["seq"], index + 1);
            assert_eq!(record["run"], id);
            assert_eq!(record["v"], 1);
        }
        assert_eq!(records[3]["totals"]["images"], 1);
        let summaries = runs_in(&root.0).unwrap();
        assert!(summaries[0].complete);
        assert_eq!(summaries[0].outcome.as_deref(), Some("stopped"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn caller_planted_secrets_artifacts_and_pixels_never_reach_disk_or_readers() {
        let root = Directory::new();
        let mut log = ActionLog::create(&root.0, 1000).unwrap();
        let planted = json!({"call_id":"call_1","name":"set_camera_features","risk":"reversible","outcome":"ok","summary":"sk-ant-abcdefghijklmnopqrstuvwxyz123456","args":{"url":"rtsp://user:planted-password@camera.example/path","access_key_id":"AKIA1234567890ABCDEF","password":"field-password","nested":{"api_key":"unpatterned-key","raw":"raw-body","thinking":"hidden-thinking","thoughtSignature":"private-signature","image_base64":"pixel-base64","data":[1,2,3]}},"commands":[{"camera":"sim:0","op":"set","before":"Bearer hidden-bearer","after":"AIzaabcdefghijklmnopqrstuvwxyz123456789"}],"headers":{"authorization":"unpatterned-key"},"raw":"provider-body","thinking":"private-thought"});
        let record = log.record("tool", planted, 1001).unwrap();
        let id = log.id().to_owned();
        log.record("observation",json!({"camera":"sim:0","sent":{"w":8,"h":8,"bytes":600,"sha256":"pixel-hash"},"metrics":{"exposure":{"mean":0.5},"raw":vec![1u8;128],"image":"pixels","signature":"hidden"}}),1002).unwrap();
        log.record(
            "text",
            json!({"text":"data:image/jpeg;base64,PLANTED-PIXEL-BYTES"}),
            1003,
        )
        .unwrap();
        log.finish(json!({"outcome":"finished","summary":"done"}), 1004)
            .unwrap();
        let disk = fs::read_to_string(log.path()).unwrap();
        for secret in [
            "abcdefghijklmnopqrstuvwxyz",
            "planted-password",
            "AKIA1234567890ABCDEF",
            "field-password",
            "unpatterned-key",
            "raw-body",
            "hidden-thinking",
            "private-signature",
            "pixel-base64",
            "hidden-bearer",
            "provider-body",
            "private-thought",
            "PLANTED-PIXEL-BYTES",
        ] {
            assert!(!disk.contains(secret), "{secret}");
            assert!(!record.to_string().contains(secret), "{secret}");
        }
        let records = read_log_in(&root.0, &id).unwrap();
        assert_eq!(records[1]["sent"]["bytes"], 600);
        assert_eq!(records[1]["sent"]["sha256"], "pixel-hash");
        assert_eq!(records[1]["metrics"]["exposure"]["mean"], 0.5);
        drop(log);
        let path = root.0.join("r-20261018-120000-abcd.jsonl");
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        writeln!(file,"{}",json!({"v":1,"t":1,"run":"r-20261018-120000-abcd","seq":1,"kind":"tool","summary":"Bearer planted-read-secret","args":{"api_key":"planted-read-key","thinking":"private-read-thinking"},"wire":"private-wire"})).unwrap();
        let read = serde_json::to_string(&read_log_in(&root.0, "r-20261018-120000-abcd").unwrap())
            .unwrap();
        for secret in [
            "planted-read-secret",
            "planted-read-key",
            "private-read-thinking",
            "private-wire",
        ] {
            assert!(!read.contains(secret));
        }
    }

    #[test]
    fn log_full_preserves_space_for_approval_stop_and_end() {
        let root = Directory::new();
        let mut log = ActionLog::create(&root.0, 1000).unwrap();
        // Sparse test file exercises the real append offset without filling
        // the disk with millions of repetitive test records.
        log.bytes = MAX_RUN_BYTES - 3 * MAX_RECORD_BYTES as u64;
        log.file.set_len(log.bytes).unwrap();
        let error = log.record("request", json!({"round":1}), 1001).unwrap_err();
        assert!(error.downcast_ref::<LogFull>().is_some());
        log.record("approval", json!({"decision":"denied"}), 1002)
            .unwrap();
        log.record("stop", json!({"reason":"action log limit"}), 1003)
            .unwrap();
        log.finish(
            json!({"outcome":"limit","summary":"The action log limit was reached.","leftovers":[]}),
            1004,
        )
        .unwrap();
        assert!(fs::metadata(log.path()).unwrap().len() <= MAX_RUN_BYTES);
    }

    #[test]
    fn retention_deletes_only_old_whole_private_run_files_and_skips_active_logs() {
        let root = Directory::new();
        let old = ActionLog::create(&root.0, 1000).unwrap();
        let old_path = old.path().to_owned();
        let middle = ActionLog::create(&root.0, 2000).unwrap();
        let middle_path = middle.path().to_owned();
        drop(middle);
        let new = ActionLog::create(&root.0, 3000).unwrap();
        let new_path = new.path().to_owned();
        drop(new);
        fs::write(root.0.join("unrelated.txt"), b"keep").unwrap();
        prune(&root.0, 2, MAX_TOTAL_BYTES, None).unwrap();
        assert!(old_path.exists());
        assert!(!middle_path.exists());
        assert!(new_path.exists());
        assert!(root.0.join("unrelated.txt").exists());
        drop(old);
        prune(&root.0, 1, MAX_TOTAL_BYTES, None).unwrap();
        assert!(!old_path.exists());
        assert!(new_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let victim = root.0.join("victim");
            fs::write(&victim, b"keep me").unwrap();
            let link = root.0.join("r-19700101-000000-ffff.jsonl");
            symlink(&victim, &link).unwrap();
            prune(&root.0, 0, 0, None).unwrap();
            assert!(link.exists());
            assert_eq!(fs::read(victim).unwrap(), b"keep me");
        }
    }

    #[test]
    fn crash_detection_ignores_live_runs_and_reports_a_partial_last_line_once() {
        let root = Directory::new();
        let mut tracker = CrashTracker::default();
        let mut log = ActionLog::create(&root.0, 1000).unwrap();
        log.record("start", json!({"goal":"Tune exposure"}), 1000)
            .unwrap();
        assert!(tracker.take_latest_unfinished(&root.0).unwrap().is_none());
        let path = log.path().to_owned();
        let id = log.id().to_owned();
        drop(log);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"partial\":")
            .unwrap();
        assert_eq!(read_log_in(&root.0, &id).unwrap().len(), 1);
        let run = tracker.take_latest_unfinished(&root.0).unwrap().unwrap();
        assert_eq!(run.run_id, id);
        assert!(!run.complete);
        assert!(tracker.take_latest_unfinished(&root.0).unwrap().is_none());
        let mut complete = ActionLog::create(&root.0, 2000).unwrap();
        complete
            .record("start", json!({"goal":"Other task"}), 2000)
            .unwrap();
        complete
            .finish(json!({"outcome":"finished"}), 2001)
            .unwrap();
        drop(complete);
        assert!(tracker.take_latest_unfinished(&root.0).unwrap().is_none());
    }

    #[test]
    fn names_corruption_and_oversized_lines_are_refused_without_raw_diagnostics() {
        let root = Directory::new();
        for id in [
            "../outside",
            "r-20261018-120000-abcd/../../x",
            "r-20261018-120000-gggg",
            "",
        ] {
            assert!(read_log_in(&root.0, id).is_err());
        }
        let id = "r-20261018-120000-abcd";
        let path = root.0.join(format!("{id}.jsonl"));
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({"v":1,"t":1,"run":id,"seq":2,"kind":"start","goal":"DO-NOT-LEAK"})
        )
        .unwrap();
        assert!(
            !read_log_in(&root.0, id)
                .unwrap_err()
                .to_string()
                .contains("DO-NOT-LEAK")
        );
        drop(file);
        fs::remove_file(&path).unwrap();
        let mut file = private_options()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(&vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(read_log_in(&root.0, id).is_err());
        let missing = root.0.join("missing");
        assert!(runs_in(&missing).unwrap().is_empty());
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_files_and_directory_symlinks_are_never_read_or_written() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = Directory::new();
        let mut log = ActionLog::create(&root.0, 1000).unwrap();
        log.finish(json!({"outcome":"finished"}), 1001).unwrap();
        let path = log.path().to_owned();
        let id = log.id().to_owned();
        drop(log);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_log_in(&root.0, &id).is_err());
        let target = root.0.join("target");
        crate::ipc::ensure_private_dir(&target).unwrap();
        let linked = root.0.join("linked");
        symlink(&target, &linked).unwrap();
        assert!(ActionLog::create(&linked, 1000).is_err());
        assert!(runs_in(&linked).is_err());
        let parent = root.0.join("assistant");
        symlink(&target, &parent).unwrap();
        assert!(ActionLog::create(&parent.join("runs"), 1000).is_err());
        assert!(!target.join("runs").exists());
    }
}
