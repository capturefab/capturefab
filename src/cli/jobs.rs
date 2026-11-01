//! `capturefab jobs`: the captures, recordings and continuous auto exposure
//! running in sessions, as each session's job book has them, and how to stop
//! or wait for one. A job's ID is `<session>-<n>`; n counts a session's
//! jobs. The jobs of a session that died are read from what it saved.
use super::{Global, output::Report, term};
use crate::{
    ipc,
    session::SessionCommand as Request,
    types::{Categorized, ErrorKind},
};
use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

#[derive(Debug, Args)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub action: Option<JobsAction>,
}

#[derive(Debug, Subcommand)]
pub enum JobsAction {
    /// Every running session's jobs, finished ones included
    #[command(after_help = "Examples:\n  capturefab jobs list\n  capturefab --json jobs")]
    List,
    /// Stop a job, or all of them; files saved so far stay
    #[command(after_help = "Examples:\n  capturefab jobs stop auto-1\n  capturefab jobs stop all")]
    Stop {
        /// A job ID such as auto-1, or all
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Wait for a job: exit 0 when done, its own code if it failed, 4 if still running
    #[command(
        after_help = "Examples:\n  capturefab jobs wait auto-1\n  capturefab --json jobs wait auto-1 --timeout 10m"
    )]
    Wait {
        /// A job ID such as auto-1
        #[arg(value_name = "ID")]
        id: String,
        /// How long to wait; none waits until the job ends
        #[arg(long, default_value = "none", value_name = "DUR", value_parser = super::args::wait)]
        timeout: super::args::Wait,
        #[command(flatten)]
        progress: ProgressArg,
    },
}

/// `--progress`, of the commands that wait for a job.
#[derive(Debug, Clone, Copy, Args)]
pub struct ProgressArg {
    /// Report progress on stderr, also without a terminal: a JSON line {progress:{job,camera,kind,state,saved,planned?,bytes?,elapsed_s,left_s?,next_s?}} as the job changes, at least each second
    #[arg(long)]
    pub progress: bool,
}

pub fn run(global: &Global, args: JobsArgs) -> Result<Report> {
    match args.action.unwrap_or(JobsAction::List) {
        JobsAction::List => list(global),
        JobsAction::Stop { id } => stop(global, &id),
        JobsAction::Wait {
            id,
            timeout,
            progress,
        } => wait(&id, timeout, progress),
    }
}

/// Every job of camera `camera`, a session status entry.
pub(super) fn of_camera(camera: &Value) -> Vec<Value> {
    camera["jobs"].as_array().cloned().unwrap_or_default()
}

/// A job in one line, for people.
pub(super) fn line(job: &Value) -> String {
    let mut text = format!(
        "{}  {} {}  {}",
        job["id"].as_str().unwrap_or_default(),
        job["kind"].as_str().unwrap_or_default(),
        job["state"].as_str().unwrap_or_default(),
        job["camera"].as_str().unwrap_or_default(),
    );
    match job["planned"].as_u64() {
        Some(planned) => {
            let _ = write!(text, "  {}/{planned} saved", job["saved"]);
        }
        None => {
            if let Some(bytes) = job["bytes"].as_u64() {
                let _ = write!(text, "  {}", crate::types::human_bytes(bytes));
            }
        }
    }
    if let Some(file) = job["last_file"].as_str() {
        let _ = write!(text, "  {file}");
    }
    if let Some(error) = job["error"]["message"].as_str() {
        let _ = write!(text, "  ({error})");
    }
    text
}

/// Whether a job document is of a job that has ended.
pub(super) fn ended(job: &Value) -> bool {
    !matches!(
        job["state"].as_str(),
        Some("armed" | "running" | "stopping")
    )
}

/// Every job of the running sessions, and of sessions that died with jobs
/// running, as lost: by session.
fn every_job(global: &Global) -> Vec<(String, Vec<Value>)> {
    let running = ipc::sessions();
    let mut all = running
        .iter()
        .filter_map(|descriptor| {
            let jobs = ipc::call(&descriptor.name, Request::Jobs, Duration::from_secs(5)).ok()?;
            Some((
                descriptor.name.clone(),
                jobs["jobs"].as_array().cloned().unwrap_or_default(),
            ))
        })
        .collect::<Vec<_>>();
    for name in crate::session::jobs::saved_sessions() {
        if running.iter().any(|descriptor| descriptor.name == name) {
            continue;
        }
        let jobs = crate::session::jobs::orphaned(crate::session::jobs::read(&name))
            .iter()
            .map(|job| job.doc(&name))
            .collect::<Vec<_>>();
        if !jobs.is_empty() {
            all.push((name, jobs));
        }
    }
    all.retain(|(name, _)| global.session.as_deref().is_none_or(|only| only == name));
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

fn list(global: &Global) -> Result<Report> {
    let jobs = every_job(global)
        .into_iter()
        .flat_map(|(_, jobs)| jobs)
        .collect::<Vec<_>>();
    let mut text = String::new();
    if jobs.is_empty() {
        text.push_str("No jobs.\n");
    }
    for job in &jobs {
        let _ = writeln!(text, "{}", line(job));
    }
    Ok(Report::json(json!({ "jobs": jobs })).text(text))
}

/// The session of job ID `id`.
fn session_of(id: &str) -> Result<String> {
    let (session, _) = crate::session::split_job(id)?;
    if session == crate::session::DIRECT {
        return Err(Categorized::new(
            ErrorKind::Usage,
            format!("{id} ran in a command's own worker, which has ended"),
        )
        .into());
    }
    Ok(session.to_owned())
}

/// A job of a session that is not running: lost, or ended, as it saved it.
fn saved_job(session: &str, id: &str) -> Result<Value> {
    crate::session::jobs::orphaned(crate::session::jobs::read(session))
        .iter()
        .map(|job| job.doc(session))
        .find(|job| job["id"] == id)
        .ok_or_else(|| {
            Categorized::new(
                ErrorKind::SessionNotFound,
                format!("session {session} is not running"),
            )
            .hint("capturefab jobs lists the jobs of every running session")
            .into()
        })
}

fn stop(global: &Global, id: &str) -> Result<Report> {
    let targets = if id == "all" {
        every_job(global)
            .into_iter()
            .flat_map(|(session, jobs)| {
                jobs.into_iter()
                    .filter(|job| !ended(job))
                    .map(move |job| (session.clone(), job))
            })
            .collect::<Vec<_>>()
    } else {
        let session = session_of(id)?;
        if ipc::running_session(&session).is_none() {
            vec![(session.clone(), saved_job(&session, id)?)]
        } else {
            vec![(session, json!({"id": id}))]
        }
    };
    let mut stopped = Vec::new();
    for (session, job) in targets {
        let id = job["id"].as_str().unwrap_or_default().to_owned();
        if job.get("state").is_some_and(|_| ended(&job)) {
            stopped.push(job);
            continue;
        }
        let reply = super::JobHost::Session(session).stop(&id)?;
        stopped.push(reply["job"].clone());
    }
    let mut text = String::new();
    if stopped.is_empty() {
        text.push_str("No jobs were running.\n");
    }
    for job in &stopped {
        let _ = writeln!(text, "{}", line(job));
    }
    Ok(Report::json(json!({ "stopped": stopped })).text(text))
}

fn wait(id: &str, timeout: super::args::Wait, progress: ProgressArg) -> Result<Report> {
    super::signals::take_over();
    let session = session_of(id)?;
    if ipc::running_session(&session).is_none() {
        return finished(saved_job(&session, id)?);
    }
    let host = super::JobHost::Session(session);
    let deadline = match timeout {
        super::args::Wait::For(wait) => Instant::now().checked_add(wait),
        super::args::Wait::Forever => None,
    };
    let began = Instant::now();
    let mut shown = Progress::new(format!("waiting for job {id}…"), progress);
    let mut seq = 0;
    loop {
        let left = deadline.map_or(Duration::MAX, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let reply = host.wait(id, seq, left.min(Duration::from_secs(1)))?;
        seq = reply["seq"].as_u64().unwrap_or(seq);
        let job = reply["job"].clone();
        if ended(&job) {
            return finished(job);
        }
        shown.show(&job, began);
        if left.is_zero() {
            return Err(Categorized::new(
                ErrorKind::Timeout,
                format!(
                    "job {id} is still {}",
                    job["state"].as_str().unwrap_or("running")
                ),
            )
            .hint(format!("wait again with capturefab jobs wait {id}"))
            .details(json!({ "job": job }))
            .into());
        }
        if super::signals::interrupted() {
            return Err(
                Categorized::new(ErrorKind::Interrupted, "interrupted; the job runs on")
                    .details(json!({ "job": job }))
                    .into(),
            );
        }
    }
}

/// How far a job is: how long it has run, and how long it has left or
/// until its next frame.
struct Estimate {
    elapsed: Duration,
    left: Option<Duration>,
    next: Option<Duration>,
}

/// How far job `job` is. Its time is counted from when it started, else
/// from `began`, when this began waiting for it; a burst without an end
/// time is estimated from how fast it saved so far.
fn estimate(job: &Value, began: Instant) -> Estimate {
    let since = |key: &str| {
        let at = chrono::DateTime::parse_from_rfc3339(job[key].as_str()?).ok()?;
        (chrono::Utc::now() - at.with_timezone(&chrono::Utc))
            .to_std()
            .ok()
    };
    let until = |key: &str| {
        let at = chrono::DateTime::parse_from_rfc3339(job[key].as_str()?).ok()?;
        (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
            .to_std()
            .ok()
    };
    let elapsed = since("started").unwrap_or_else(|| began.elapsed());
    let saved = job["saved"].as_u64().unwrap_or(0);
    let estimate = || {
        let planned = job["planned"].as_u64()?;
        (saved > 0 && planned > saved)
            .then(|| elapsed.mul_f64((planned - saved) as f64 / saved as f64))
    };
    let left = until("ends_at").or_else(estimate);
    Estimate {
        elapsed,
        left,
        next: left.is_none().then(|| until("next_at")).flatten(),
    }
}

/// How far job `job` is, for the status line: what it saved or recorded,
/// how long it has run, and how long it has left or until its next frame.
pub(super) fn progress(job: &Value, began: Instant) -> String {
    let Estimate {
        elapsed,
        left,
        next,
    } = estimate(job, began);
    let mut parts = Vec::new();
    let saved = job["saved"].as_u64().unwrap_or(0);
    match (job["kind"].as_str(), job["planned"].as_u64()) {
        (Some("record"), _) => parts.push(format!("recording {}", term::clock(elapsed))),
        (_, Some(planned)) => parts.push(format!("{saved}/{planned} saved")),
        _ => parts.push(format!("{saved} saved")),
    }
    if let Some(bytes) = job["bytes"].as_u64().filter(|bytes| *bytes > 0) {
        parts.push(crate::types::human_bytes(bytes));
    }
    if job["kind"] != "record" {
        parts.push(term::clock(elapsed));
    }
    if let Some(left) = left {
        parts.push(format!("{} left", term::clock(left)));
    } else if let Some(next) = next {
        parts.push(format!("next in {}", term::clock(next)));
    }
    format!(
        "{}: {}",
        crate::media::redact_url(job["camera"].as_str().unwrap_or("camera")),
        parts.join(" · ")
    )
}

/// How far job `job` is, as a `--progress` line.
fn progress_line(job: &Value, began: Instant) -> Value {
    let Estimate {
        elapsed,
        left,
        next,
    } = estimate(job, began);
    let seconds = |duration: Duration| (duration.as_secs_f64() * 10.0).round() / 10.0;
    crate::types::without_nulls(json!({
        "progress": {
            "job": job["id"],
            "camera": job["camera"].as_str().map(crate::media::redact_url),
            "kind": job["kind"],
            "state": job["state"],
            "saved": job["saved"],
            "planned": job["planned"],
            "bytes": job["bytes"],
            "elapsed_s": seconds(elapsed),
            "left_s": left.map(seconds),
            "next_s": next.map(seconds),
        }
    }))
}

/// The shortest time between `--progress` lines while frames are saved,
/// and the longest while nothing changes.
const LINES_EVERY: (Duration, Duration) = (Duration::from_millis(200), Duration::from_secs(1));

/// How a command that waits for a job shows how far it is: a status line
/// at a terminal, or with `--progress` a JSON line on stderr when the job
/// changes, and each second.
pub(super) struct Progress {
    status: Option<term::Activity>,
    lines: bool,
    /// The last line, and when it was written.
    last: Option<(Instant, Value)>,
}

impl Progress {
    pub(super) fn new(text: impl Into<String>, progress: ProgressArg) -> Self {
        Self {
            status: (!progress.progress).then(|| term::activity(text)),
            lines: progress.progress,
            last: None,
        }
    }
    pub(super) fn show(&mut self, job: &Value, began: Instant) {
        if let Some(status) = &self.status {
            status.progress(progress(job, began));
        }
        if !self.lines {
            return;
        }
        let line = progress_line(job, began);
        let due = match &self.last {
            None => true,
            Some((at, last)) => {
                let (soonest, latest) = LINES_EVERY;
                let changed = |key: &str| last["progress"][key] != line["progress"][key];
                changed("state")
                    || at.elapsed() >= latest
                    || (at.elapsed() >= soonest && (changed("saved") || changed("bytes")))
            }
        };
        if due {
            let mut text = line.to_string();
            text.push('\n');
            term::before_output();
            let _ = std::io::Write::write_all(&mut std::io::stderr().lock(), text.as_bytes());
            self.last = Some((Instant::now(), line));
        }
    }
    /// How long a wait for a job's next change may take: about a second
    /// while progress is shown, so its clock moves.
    pub(super) fn step(&self, wait: Duration) -> Duration {
        if self.lines || term::live() {
            wait.min(Duration::from_secs(1))
        } else {
            wait
        }
    }
}

/// A finished job as a result (done) or as its own error.
fn finished(job: Value) -> Result<Report> {
    let id = job["id"].as_str().unwrap_or_default().to_owned();
    match job["state"].as_str() {
        Some("done") => {
            let text = format!("{}\n", line(&job));
            Ok(Report::json(json!({ "job": job })).text(text))
        }
        Some("cancelled" | "interrupted" | "lost") => Err(Categorized::new(
            ErrorKind::Incomplete,
            match job["state"].as_str() {
                Some("lost") => format!("job {id} was lost when its session ended"),
                Some("interrupted") => format!("job {id} ended with its session"),
                _ => format!("job {id} was stopped after {} file(s)", job["saved"]),
            },
        )
        .details(json!({ "job": job }))
        .into()),
        _ => {
            let kind = job["error"]["code"]
                .as_str()
                .and_then(ErrorKind::from_code)
                .unwrap_or(ErrorKind::OperationFailed);
            Err(Categorized::new(
                kind,
                job["error"]["message"]
                    .as_str()
                    .unwrap_or("the job failed")
                    .to_owned(),
            )
            .details(json!({ "job": job }))
            .into())
        }
    }
}

/// The job document, for `schema`: every command that shows a job shows
/// this.
macro_rules! job_row {
    () => {
        "{id,camera,kind:capture|record|auto,state:armed|running|stopping|done|failed|cancelled|interrupted|lost,saved,planned?,bytes?,last_file?,next_at?,ends_at?,started,ended?,error?:{code,message}}"
    };
}
pub(super) use job_row;
pub const LIST_FIELDS: &[(&str, &str)] = &[(
    "jobs",
    concat!(
        "[",
        job_row!(),
        "] — every running session's, and those of a session that died, as lost"
    ),
)];
pub const STOP_FIELDS: &[(&str, &str)] = &[(
    "stopped",
    concat!("[", job_row!(), "] — the jobs as they are after stopping"),
)];
pub const WAIT_FIELDS: &[(&str, &str)] = &[(
    "job",
    concat!(
        job_row!(),
        " — done; a failed job exits with its own error code and details.job"
    ),
)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_session_jobs_can_be_waited_for_by_id() {
        assert_eq!(session_of("line-1-12").unwrap(), "line-1");
        for bad in ["auto", "auto-0", "-3", "auto-x", "a b-1", "direct-1"] {
            assert!(session_of(bad).is_err(), "{bad}");
        }
        assert!(ended(&json!({"state": "lost"})));
        assert!(!ended(&json!({"state": "stopping"})));
    }

    #[test]
    fn progress_says_what_a_job_did_and_what_is_left() {
        let began = Instant::now() - Duration::from_millis(2500);
        // A job joined late is timed from when it started, not from when
        // the wait began: 30 of 40 frames in 3 s leave 1 s.
        let joined = json!({
            "camera": "sim:0", "kind": "capture", "saved": 30, "planned": 40,
            "started": crate::scheduling::rfc3339(crate::scheduling::now_ms() - 3_000),
        });
        assert_eq!(
            progress(&joined, Instant::now()),
            "sim:0: 30/40 saved · 0:03 · 0:01 left"
        );
        let line = progress_line(&joined, Instant::now());
        assert_eq!(line["progress"]["saved"], 30);
        assert!((line["progress"]["elapsed_s"].as_f64().unwrap() - 3.0).abs() < 0.5);
        assert!((line["progress"]["left_s"].as_f64().unwrap() - 1.0).abs() < 0.5);
        assert!(line["progress"].get("next_s").is_none(), "{line}");
        let burst = json!({"camera": "sim:0", "kind": "capture", "saved": 5, "planned": 10});
        assert_eq!(
            progress(&burst, began),
            "sim:0: 5/10 saved · 0:02 · 0:02 left"
        );
        let lapse = json!({
            "camera": "sim:0", "kind": "capture", "saved": 1, "planned": 3,
            "ends_at": crate::scheduling::rfc3339(crate::scheduling::now_ms() + 20_500),
        });
        assert_eq!(
            progress(&lapse, began),
            "sim:0: 1/3 saved · 0:02 · 0:20 left"
        );
        let recording = json!({
            "camera": "rtsp://admin:secret@cam/live", "kind": "record", "saved": 0, "bytes": 3_145_728,
        });
        let shown = progress(&recording, began);
        assert!(shown.ends_with(": recording 0:02 · 3.0 MiB"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }
}
