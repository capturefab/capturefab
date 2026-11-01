//! A session's jobs: the long operations its camera workers run (captures
//! of several frames or time lapses, recordings, continuous auto exposure),
//! numbered across the session's cameras and followed only by the workers'
//! events. Ended jobs are kept a while, and the book is saved so the jobs of
//! a session that died still read, as lost.
use super::protocol::{JobStatus, Output};
use crate::scheduling::rfc3339;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Condvar, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Ended jobs kept, and for how long.
const KEPT: usize = 256;
const KEPT_MS: u64 = 3_600_000;
/// How long a job whose client waits for it runs on once nobody waits.
pub(crate) const LEASE: Duration = Duration::from_secs(10);
/// The longest wait a client may ask for at once.
pub(crate) const MAX_WAIT_MS: u64 = 25_000;
/// How often the book is saved while only counts change.
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// How often a wait looks whether its waiter went away.
const WATCH: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobKind {
    Capture,
    Record,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobState {
    /// Waiting for its first frame, or its start time.
    Armed,
    Running,
    /// Asked to stop; its end follows.
    Stopping,
    Done,
    Failed,
    Cancelled,
    Interrupted,
    /// Its session died before it ended.
    Lost,
}
impl JobState {
    pub(crate) fn ended(self) -> bool {
        !matches!(self, Self::Armed | Self::Running | Self::Stopping)
    }
}
impl From<JobStatus> for JobState {
    fn from(status: JobStatus) -> Self {
        match status {
            JobStatus::Done => Self::Done,
            JobStatus::Failed => Self::Failed,
            JobStatus::Cancelled => Self::Cancelled,
            JobStatus::Interrupted => Self::Interrupted,
        }
    }
}

/// One job, as the session follows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Job {
    pub number: u64,
    /// The camera's ID with credentials redacted.
    pub camera: String,
    pub kind: JobKind,
    pub state: JobState,
    pub saved: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_file: Option<String>,
    /// The first files saved, in order; `saved` counts them all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
    /// `{message, code?, hint?, details?}` of a failed job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// What its worker last said of it: a capture job's counts and times,
    /// a recording's counters and file.
    #[serde(default)]
    pub detail: Value,
    /// The camera as the session holds it, unredacted; never saved.
    #[serde(skip)]
    pub key: String,
    /// Changes so far, which a wait compares.
    #[serde(skip)]
    seq: u64,
    #[serde(skip)]
    lease: Option<Lease>,
}

/// A job a client waits for: it runs while that client keeps waiting.
#[derive(Debug, Clone)]
struct Lease {
    until: Instant,
    waiters: usize,
}

impl Job {
    /// The job document every command shows: `{id, camera, kind, state,
    /// saved, planned?, bytes?, last_file?, next_at?, ends_at?, started,
    /// ended?, error?:{code,message}}`.
    pub(crate) fn doc(&self, session: &str) -> Value {
        let now = crate::scheduling::now_ms();
        let detail = &self.detail;
        let mut value = json!({
            "id": format!("{session}-{}", self.number),
            "camera": self.camera,
            "kind": self.kind,
            "state": self.state,
            "saved": self.saved,
            "started": rfc3339(self.started_ms),
        });
        if let Some(planned) = self.planned {
            value["planned"] = json!(planned);
        }
        if let Some(bytes) = self.bytes {
            value["bytes"] = json!(bytes);
        }
        if let Some(file) = &self.last_file {
            value["last_file"] = json!(file);
        }
        if !self.state.ended() {
            if let Some(next) = detail["next_at_ms"].as_u64().filter(|next| *next > now) {
                value["next_at"] = json!(rfc3339(next));
            }
            let interval = detail["interval_ms"].as_u64().unwrap_or(0);
            let ends = match self.kind {
                JobKind::Capture if interval > 0 => detail["first_at_ms"].as_u64().map(|first| {
                    first.saturating_add(
                        interval.saturating_mul(self.planned.unwrap_or(1).saturating_sub(1)),
                    )
                }),
                JobKind::Record => detail["until_ms"].as_u64(),
                _ => None,
            };
            if let Some(ends) = ends {
                value["ends_at"] = json!(rfc3339(ends));
            }
        }
        if let Some(ended) = self.ended_ms {
            value["ended"] = json!(rfc3339(ended));
        }
        if let Some(error) = &self.error {
            value["error"] = json!({
                "code": error["code"].as_str().unwrap_or("failed"),
                "message": crate::scrub::redact_urls(error["message"].as_str().unwrap_or("the job failed")),
            });
        }
        value
    }
    /// What a command that waited for this job builds its result from: the
    /// worker's account of it, with its files.
    pub(crate) fn detail(&self) -> Value {
        let mut detail = self.detail.clone();
        if !detail.is_object() {
            detail = json!({});
        }
        detail["files"] = json!(self.files);
        if self.saved > self.files.len() as u64 {
            detail["truncated"] = json!(self.saved - self.files.len() as u64);
        }
        // The whole error, with its hint and details, which the document
        // leaves out.
        match &self.error {
            Some(error) => detail["error"] = error.clone(),
            None => {
                if let Some(detail) = detail.as_object_mut() {
                    detail.remove("error");
                }
            }
        }
        detail
    }
    fn touch(&mut self) {
        self.seq += 1;
    }
    /// Its state from a capture job's account: armed until its first frame
    /// or start time, running after.
    fn follow(&mut self, progress: Value) {
        if self.state == JobState::Stopping {
            self.detail = progress;
            return;
        }
        self.state = match self.kind {
            JobKind::Capture => {
                let first = progress["first_at_ms"].as_u64().unwrap_or(0);
                if progress["captured"].as_u64().unwrap_or(0) == 0
                    && first > crate::scheduling::now_ms()
                {
                    JobState::Armed
                } else {
                    JobState::Running
                }
            }
            JobKind::Record if progress["written"].as_u64().unwrap_or(0) == 0 => JobState::Armed,
            _ => JobState::Running,
        };
        self.detail = match (self.kind, &self.detail) {
            // A recording's start and end times come with its reply.
            (JobKind::Record, Value::Object(old)) => {
                let mut merged = old.clone();
                if let Value::Object(new) = progress {
                    merged.extend(new);
                }
                Value::Object(merged)
            }
            _ => progress,
        };
    }
}

pub(super) struct JobBook {
    book: Mutex<Book>,
    changed: Condvar,
}
struct Book {
    session: String,
    next: u64,
    jobs: VecDeque<Job>,
    /// Where the book is saved: a named session's only.
    file: Option<PathBuf>,
    dirty: bool,
    saved_at: Instant,
}

/// Where session `name`'s jobs are saved, in the state folder.
pub(crate) fn path(name: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::dirs::state()?
        .join("jobs")
        .join(format!("{name}.json")))
}

/// The jobs a session saved, as they were when it last saved them.
pub(crate) fn read(name: &str) -> Vec<Job> {
    path(name)
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice::<Vec<Job>>(&data).ok())
        .unwrap_or_default()
}

/// The names of sessions that saved jobs.
pub(crate) fn saved_sessions() -> Vec<String> {
    let Ok(state) = crate::dirs::state() else {
        return Vec::new();
    };
    let mut names = std::fs::read_dir(state.join("jobs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension()? == "json")
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .filter(|name| crate::ipc::validate_name(name).is_ok())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Jobs a dead session left: those that had not ended are lost.
pub(crate) fn orphaned(jobs: Vec<Job>) -> Vec<Job> {
    let now = crate::scheduling::now_ms();
    jobs.into_iter()
        .map(|mut job| {
            if !job.state.ended() {
                job.state = JobState::Lost;
            }
            job
        })
        .filter(|job| {
            job.state == JobState::Lost
                || job
                    .ended_ms
                    .is_some_and(|ended| now.saturating_sub(ended) < KEPT_MS)
        })
        .collect()
}

impl JobBook {
    /// The jobs of an unnamed session, never saved.
    pub(super) fn new(session: &str) -> Self {
        Self::with(session, None, VecDeque::new(), 0)
    }
    /// Session `name`'s jobs, saved as they change: those a session of the
    /// same name left are kept, as lost if they had not ended, and its
    /// numbers go on after theirs.
    /// Without a writable state folder, as on a read-only system, the jobs
    /// are kept only while the session runs.
    pub(super) fn saved(name: &str) -> Self {
        let jobs = orphaned(read(name));
        let next = jobs.iter().map(|job| job.number).max().unwrap_or(0);
        let writable = crate::dirs::found(crate::dirs::Kind::State)
            .chosen
            .as_ref()
            .is_some_and(|chosen| chosen.writable);
        let file = path(name).ok().filter(|_| writable);
        let book = Self::with(name, file, jobs.into(), next);
        book.lock().dirty = true;
        book
    }
    fn with(session: &str, file: Option<PathBuf>, jobs: VecDeque<Job>, next: u64) -> Self {
        Self {
            book: Mutex::new(Book {
                session: session.into(),
                next,
                jobs,
                file,
                dirty: false,
                saved_at: Instant::now(),
            }),
            changed: Condvar::new(),
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }
    /// A new job's number, before its worker is asked to run it.
    pub(super) fn start(&self, kind: JobKind, key: &str, planned: Option<u64>, lease: bool) -> u64 {
        let mut book = self.lock();
        book.next += 1;
        let number = book.next;
        book.jobs.push_back(Job {
            number,
            camera: crate::media::redact_url(key),
            kind,
            state: JobState::Armed,
            saved: 0,
            planned,
            bytes: None,
            last_file: None,
            files: Vec::new(),
            started_ms: crate::scheduling::now_ms(),
            ended_ms: None,
            error: None,
            detail: Value::Null,
            key: key.into(),
            seq: 1,
            lease: lease.then(|| Lease {
                until: Instant::now() + LEASE,
                waiters: 0,
            }),
        });
        book.prune();
        book.dirty = true;
        book.save(true);
        number
    }
    /// The worker took job `number`: what its reply says of it.
    pub(super) fn accepted(&self, number: u64, detail: Value) {
        let mut book = self.lock();
        if let Some(job) = book.find(number) {
            match (&mut job.detail, detail) {
                (Value::Null, detail) => job.follow(detail),
                // Its worker's lines that came first stay; the reply adds
                // what only it says, such as a recording's warm-up.
                (Value::Object(known), Value::Object(reply)) => {
                    for (key, value) in reply {
                        known.entry(key).or_insert(value);
                    }
                }
                _ => {}
            }
            job.touch();
        }
        drop(book);
        self.changed.notify_all();
    }
    /// The worker refused job `number`: it never ran.
    pub(super) fn refused(&self, number: u64) {
        let mut book = self.lock();
        book.jobs.retain(|job| job.number != number);
        book.dirty = true;
        book.save(true);
    }
    /// Follow a worker's line about one of its jobs.
    pub(super) fn event(&self, output: &Output) {
        let mut book = self.lock();
        let ended = match output {
            Output::Saved {
                job: Some(number),
                file,
            } => {
                let Some(job) = book.find(*number) else {
                    return;
                };
                job.saved += 1;
                if job.files.len() < crate::scheduling::MAX_JOB_FILES {
                    job.files.push(file.path.clone());
                }
                job.last_file = Some(file.path.clone());
                if job.kind == JobKind::Record {
                    job.bytes = Some(file.bytes);
                }
                job.touch();
                false
            }
            Output::Progress { job, progress } => {
                let Some(job) = book.find(*job) else {
                    return;
                };
                if job.state.ended() {
                    return;
                }
                job.follow(progress.clone());
                job.touch();
                false
            }
            Output::JobEnd {
                job,
                status,
                error,
                summary,
            } => {
                let Some(job) = book.find(*job) else {
                    return;
                };
                if job.state.ended() {
                    return;
                }
                job.follow(summary.clone());
                job.state = (*status).into();
                job.error = error.clone();
                job.ended_ms = Some(crate::scheduling::now_ms());
                job.lease = None;
                job.touch();
                true
            }
            _ => return,
        };
        book.dirty = true;
        book.save(ended);
        drop(book);
        self.changed.notify_all();
    }
    /// End camera `key`'s jobs that had not ended with `error`: its worker
    /// is gone.
    pub(super) fn fail_camera(&self, key: &str, error: &anyhow::Error) {
        self.end_camera(key, JobState::Failed, error);
    }
    /// End camera `key`'s jobs that had not ended as `state`, because of
    /// `error`.
    pub(super) fn end_camera(&self, key: &str, state: JobState, error: &anyhow::Error) {
        let wire = super::protocol::wire_error(error);
        let mut book = self.lock();
        let now = crate::scheduling::now_ms();
        let mut changed = false;
        for job in book
            .jobs
            .iter_mut()
            .filter(|job| job.key == key && !job.state.ended())
        {
            job.state = state;
            job.error = (state == JobState::Failed).then(|| wire.clone());
            job.ended_ms = Some(now);
            job.lease = None;
            job.touch();
            changed = true;
        }
        if changed {
            book.dirty = true;
            book.save(true);
            drop(book);
            self.changed.notify_all();
        }
    }
    /// Mark job `number` as stopping: the camera and job to stop, or why
    /// there is nothing to stop.
    pub(super) fn stopping(&self, number: u64) -> Option<(String, Job)> {
        let mut book = self.lock();
        let job = book.find(number)?;
        if !job.state.ended() {
            job.state = JobState::Stopping;
            job.lease = None;
            job.touch();
        }
        let found = (job.key.clone(), job.clone());
        drop(book);
        self.changed.notify_all();
        Some(found)
    }
    /// Job `number` once it changed after `after_seq` or ended, or as it is
    /// when `wait` ends, or once `gone` says its waiter went away. A wait
    /// keeps a waited-for job's lease; the lease runs out `LEASE` after the
    /// last wait ends.
    pub(super) fn wait(
        &self,
        number: u64,
        after_seq: u64,
        wait: Duration,
        gone: &dyn Fn() -> bool,
    ) -> Option<(u64, Job)> {
        let deadline = Instant::now() + wait;
        let mut book = self.lock();
        if let Some(lease) = book.find(number)?.lease.as_mut() {
            lease.waiters += 1;
        }
        loop {
            let job = book.find(number)?;
            if job.seq > after_seq || job.state.ended() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            book = self
                .changed
                .wait_timeout(book, (deadline - now).min(WATCH))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            // Looked at without the book, which the workers' events need.
            drop(book);
            let left = gone();
            book = self.lock();
            if left {
                break;
            }
        }
        let job = book.find(number)?;
        if let Some(lease) = job.lease.as_mut() {
            lease.waiters = lease.waiters.saturating_sub(1);
            lease.until = Instant::now() + LEASE;
        }
        Some((job.seq, job.clone()))
    }
    /// Jobs whose client stopped waiting for them: no wait for `LEASE`.
    pub(super) fn abandoned(&self) -> Vec<(String, u64)> {
        let mut book = self.lock();
        let now = Instant::now();
        let mut gone = Vec::new();
        for job in book.jobs.iter_mut() {
            if let Some(lease) = &job.lease
                && lease.waiters == 0
                && now >= lease.until
                && !job.state.ended()
            {
                job.lease = None;
                gone.push((job.key.clone(), job.number));
            }
        }
        gone
    }
    pub(super) fn get(&self, number: u64) -> Option<Job> {
        self.lock().find(number).cloned()
    }
    /// Every job, oldest first.
    pub(super) fn all(&self) -> Vec<Job> {
        self.lock().jobs.iter().cloned().collect()
    }
    /// Whether a job has yet to end; of camera `key` alone when given.
    pub(super) fn active(&self, key: Option<&str>) -> bool {
        self.lock()
            .jobs
            .iter()
            .any(|job| !job.state.ended() && key.is_none_or(|key| job.key == key))
    }
    pub(super) fn session(&self) -> String {
        self.lock().session.clone()
    }
    /// Save the book if it changed and the last save is a second old.
    pub(super) fn flush(&self) {
        self.lock().save(false);
    }
}
impl Book {
    fn find(&mut self, number: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.number == number)
    }
    /// Forget ended jobs past the newest `KEPT`, and those ended an hour ago.
    fn prune(&mut self) {
        let now = crate::scheduling::now_ms();
        self.jobs.retain(|job| {
            !job.state.ended()
                || job
                    .ended_ms
                    .is_none_or(|ended| now.saturating_sub(ended) < KEPT_MS)
        });
        while self.jobs.iter().filter(|job| job.state.ended()).count() > KEPT {
            if let Some(index) = self.jobs.iter().position(|job| job.state.ended()) {
                self.jobs.remove(index);
            }
        }
    }
    /// Write the book, now or once `SAVE_EVERY` passed since the last write.
    /// Best effort: a session whose state folder is unwritable still runs.
    fn save(&mut self, now: bool) {
        let Some(file) = &self.file else {
            return;
        };
        if !self.dirty || (!now && self.saved_at.elapsed() < SAVE_EVERY) {
            return;
        }
        self.dirty = false;
        self.saved_at = Instant::now();
        let jobs = self.jobs.iter().collect::<Vec<_>>();
        let Ok(data) = serde_json::to_vec(&jobs) else {
            return;
        };
        let write = || -> anyhow::Result<()> {
            let dir = file.parent().ok_or_else(|| anyhow::anyhow!("no folder"))?;
            crate::dirs::ensure_private_dir(dir)?;
            let temp = dir.join(format!(".{}.tmp", std::process::id()));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            std::io::Write::write_all(&mut options.open(&temp)?, &data)?;
            std::fs::rename(&temp, file)?;
            Ok(())
        };
        if let Err(error) = write() {
            crate::eprintln!(
                "capturefab: cannot save the session's jobs to {}: {error:#}",
                file.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::saved::{SavedFile, SavedItem};

    fn saved(job: u64, path: &str) -> Output {
        Output::Saved {
            job: Some(job),
            file: SavedFile {
                wall_ms: 1,
                path: path.into(),
                bytes: 10,
                sha256: None,
                context: Default::default(),
                item: SavedItem::Recording {
                    started_ms: 0,
                    ended_ms: 1,
                    codec: "h264".into(),
                    encoder: "libx264".into(),
                    fps: 30.0,
                },
            },
        }
    }

    #[test]
    fn jobs_follow_their_events_and_end_once() {
        let book = JobBook::new("bench");
        let number = book.start(JobKind::Capture, "rtsp://u:secret@cam/x", Some(2), false);
        assert_eq!(number, 1);
        book.accepted(
            number,
            json!({"captured": 0, "first_at_ms": 0, "status": "pending"}),
        );
        book.event(&saved(number, "/a.png"));
        book.event(&saved(number, "/b.png"));
        book.event(&Output::JobEnd {
            job: number,
            status: JobStatus::Done,
            error: None,
            summary: json!({"captured": 2}),
        });
        // A late line changes nothing.
        book.event(&Output::JobEnd {
            job: number,
            status: JobStatus::Failed,
            error: None,
            summary: json!({}),
        });
        let job = book.get(number).unwrap();
        assert_eq!(job.state, JobState::Done);
        let doc = job.doc("bench");
        assert_eq!(doc["id"], "bench-1");
        assert_eq!(doc["saved"], 2);
        assert_eq!(doc["last_file"], "/b.png");
        assert!(!doc.to_string().contains("secret"), "{doc}");
        assert_eq!(job.detail()["files"], json!(["/a.png", "/b.png"]));
    }

    #[test]
    fn a_reply_after_its_jobs_first_lines_adds_what_only_it_says() {
        let book = JobBook::new("bench");
        let number = book.start(JobKind::Record, "sim:0", None, false);
        book.event(&Output::Progress {
            job: number,
            progress: json!({"written": 3}),
        });
        book.accepted(
            number,
            json!({"since_ms": 1, "warmup_ms": 52, "settled": false, "written": 0}),
        );
        let detail = book.get(number).unwrap().detail();
        assert_eq!(
            (&detail["written"], &detail["warmup_ms"], &detail["settled"]),
            (&json!(3), &json!(52), &json!(false)),
            "{detail}"
        );
    }

    #[test]
    fn waits_return_on_a_change_or_the_end_and_keep_the_lease() {
        let book = std::sync::Arc::new(JobBook::new("bench"));
        let number = book.start(JobKind::Record, "sim:0", None, true);
        let here = || false;
        let (seq, job) = book
            .wait(number, 0, Duration::from_millis(10), &here)
            .unwrap();
        assert_eq!(job.state, JobState::Armed);
        let started = Instant::now();
        let (later, _) = book
            .wait(number, seq, Duration::from_millis(50), &here)
            .unwrap();
        assert_eq!(later, seq);
        assert!(started.elapsed() >= Duration::from_millis(50));
        let waiter = {
            let book = book.clone();
            std::thread::spawn(move || book.wait(number, seq, Duration::from_secs(5), &|| false))
        };
        std::thread::sleep(Duration::from_millis(20));
        book.event(&Output::Progress {
            job: number,
            progress: json!({"written": 3}),
        });
        let (_, job) = waiter.join().unwrap().unwrap();
        assert_eq!(job.state, JobState::Running);
        assert!(book.abandoned().is_empty(), "a wait renews the lease");
    }

    #[test]
    fn a_wait_whose_client_left_ends_and_starts_the_lease() {
        let book = std::sync::Arc::new(JobBook::new("bench"));
        let number = book.start(JobKind::Capture, "sim:0", Some(1), true);
        let left = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (book, left) = (book.clone(), left.clone());
            std::thread::spawn(move || {
                let gone = || left.load(std::sync::atomic::Ordering::Acquire);
                book.wait(number, 1, Duration::from_secs(25), &gone)
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            book.lock()
                .find(number)
                .unwrap()
                .lease
                .as_ref()
                .unwrap()
                .waiters,
            1
        );
        let asked = Instant::now();
        left.store(true, std::sync::atomic::Ordering::Release);
        let (_, job) = waiter.join().unwrap().unwrap();
        assert!(asked.elapsed() < Duration::from_secs(1));
        assert!(!job.state.ended());
        let mut book = book.lock();
        let lease = book.find(number).unwrap().lease.clone().unwrap();
        assert_eq!(lease.waiters, 0);
        assert!(lease.until > Instant::now() + LEASE - Duration::from_secs(1));
    }

    #[test]
    fn a_crashed_worker_fails_its_jobs_and_a_dead_session_loses_them() {
        let book = JobBook::new("bench");
        let first = book.start(JobKind::Capture, "sim:0", Some(5), false);
        let other = book.start(JobKind::Capture, "sim:1", Some(5), false);
        let error = crate::types::Categorized::error(
            crate::types::ErrorKind::WorkerCrashed,
            "camera worker exited",
        );
        book.fail_camera("sim:0", &error);
        let failed = book.get(first).unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.doc("bench")["error"]["code"], "worker_crashed");
        assert!(!book.get(other).unwrap().state.ended());
        let lost = orphaned(book.all());
        assert_eq!(lost[1].state, JobState::Lost);
        assert_eq!(lost[0].state, JobState::Failed);
    }
}
