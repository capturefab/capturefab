//! `capturefab jobs`: the captures and recordings running in sessions, as
//! job documents built from each session's status, and how to stop or wait
//! for one. A job's ID is `<session>-<n>`; n counts a session's jobs.
use super::{Global, output::Report};
use crate::{
    ipc,
    session::SessionCommand as Request,
    types::{Categorized, ErrorKind},
};
use anyhow::{Context, Result};
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
    },
}

/// How often waits look at a session's jobs.
pub(super) const POLL: Duration = Duration::from_millis(200);

pub fn run(global: &Global, args: JobsArgs) -> Result<Report> {
    match args.action.unwrap_or(JobsAction::List) {
        JobsAction::List => list(global),
        JobsAction::Stop { id } => stop(global, &id),
        JobsAction::Wait { id, timeout } => wait(&id, timeout),
    }
}

fn rfc3339(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(i64::try_from(ms).unwrap_or(i64::MAX))
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A capture job of `camera` (a session status entry) as a job document.
pub(super) fn capture(session: &str, camera: &Value, job: &Value) -> Value {
    let now = crate::scheduling::now_ms();
    let saved = job["captured"].as_u64().unwrap_or(0);
    let state = match job["status"].as_str().unwrap_or_default() {
        "pending" if saved == 0 && job["first_at_ms"].as_u64().unwrap_or(0) > now => "armed",
        "pending" | "running" => "running",
        "complete" => "done",
        "cancelled" => "cancelled",
        _ => "failed",
    };
    let count = job["count"].as_u64().unwrap_or(1);
    let first_at = job["first_at_ms"].as_u64().unwrap_or(0);
    let interval = job["interval_ms"].as_u64().unwrap_or(0);
    let mut value = json!({
        "id": format!("{session}-{}", job["id"]),
        "camera": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "kind": "capture",
        "state": state,
        "saved": saved,
        "planned": count,
        "last_file": job["last_file"],
        "started": job["created_ms"].as_u64().map(rfc3339),
        "ended": job["ended_ms"].as_u64().map(rfc3339),
    });
    if matches!(state, "armed" | "running") {
        if let Some(next) = job["next_at_ms"].as_u64().filter(|next| *next > now) {
            value["next_at"] = json!(rfc3339(next));
        }
        if interval > 0 {
            value["ends_at"] = json!(rfc3339(
                first_at.saturating_add(interval.saturating_mul(count.saturating_sub(1)))
            ));
        }
    }
    if state == "failed" {
        value["error"] = error_object(job);
    }
    crate::types::without_nulls(value)
}

/// `{code,message}` of a failed capture job.
fn error_object(job: &Value) -> Value {
    json!({
        "code": job["error_tag"]["code"].as_str().unwrap_or("failed"),
        "message": crate::scrub::redact_urls(job["error"].as_str().unwrap_or("the job failed")),
    })
}

/// The current or last recording of `camera` as a job document.
pub(super) fn recording(session: &str, camera: &Value) -> Option<Value> {
    let job = camera["record_job"].as_u64()?;
    let progress = &camera["forwarding_progress"];
    let finished = &progress["recording"];
    let running = camera["forwarding"].is_string();
    let state = if running {
        if progress["written"].as_u64().unwrap_or(0) == 0 {
            "armed"
        } else {
            "running"
        }
    } else if finished.is_object() || camera["last_error"].is_null() {
        "done"
    } else {
        "failed"
    };
    let mut value = json!({
        "id": format!("{session}-{job}"),
        "camera": crate::media::redact_url(camera["info"]["id"].as_str().unwrap_or_default()),
        "kind": "record",
        "state": state,
        "saved": u64::from(finished.is_object()),
        "bytes": finished["bytes"],
        "last_file": if running { camera["recording_path"].clone() } else { finished["path"].clone() },
        "started": camera["forwarding_since_ms"].as_u64().or(finished["started_ms"].as_u64()).map(rfc3339),
        "ends_at": camera["forwarding_until_ms"].as_u64().filter(|_| running).map(rfc3339),
        "ended": finished["ended_ms"].as_u64().filter(|_| !running).map(rfc3339),
    });
    if state == "failed" {
        value["error"] = json!({
            "code": "failed",
            "message": crate::scrub::redact_urls(camera["last_error"].as_str().unwrap_or_default()),
        });
    }
    Some(crate::types::without_nulls(value))
}

/// Every job of `camera`, a session status entry.
pub(super) fn of_camera(session: &str, camera: &Value) -> Vec<Value> {
    let mut jobs = camera["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|job| capture(session, camera, job))
        .collect::<Vec<_>>();
    jobs.extend(recording(session, camera));
    jobs
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

/// The status of every running session, by name; sessions that do not
/// answer are left out.
fn sessions() -> Vec<(String, Value)> {
    ipc::sessions()
        .into_iter()
        .filter_map(|descriptor| {
            ipc::call(&descriptor.name, Request::Status, Duration::from_secs(5))
                .ok()
                .map(|state| (descriptor.name, state))
        })
        .collect()
}

/// Every job of session `name`'s status.
fn jobs_of(name: &str, state: &Value) -> Vec<Value> {
    state["cameras"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|camera| of_camera(name, camera))
        .collect()
}

fn list(global: &Global) -> Result<Report> {
    let mut jobs = Vec::new();
    for (name, state) in sessions() {
        if global.session.as_deref().is_none_or(|only| only == name) {
            jobs.extend(jobs_of(&name, &state));
        }
    }
    let mut text = String::new();
    if jobs.is_empty() {
        text.push_str("No jobs.\n");
    }
    for job in &jobs {
        let _ = writeln!(text, "{}", line(job));
    }
    Ok(Report::json(json!({ "jobs": jobs })).text(text))
}

/// The session and number of a job ID.
fn parse(id: &str) -> Result<(String, u64)> {
    id.rsplit_once('-')
        .and_then(|(session, number)| {
            Some((
                session.to_owned(),
                number.parse::<u64>().ok().filter(|n| *n > 0)?,
            ))
        })
        .filter(|(session, _)| ipc::validate_name(session).is_ok())
        .ok_or_else(|| {
            Categorized::new(ErrorKind::Usage, format!("'{id}' is not a job ID"))
                .hint("job IDs look like auto-1; capturefab jobs lists them")
                .into()
        })
}

/// Job `id` in its session now: the document, and the camera status entry
/// that holds it.
fn find(id: &str) -> Result<(Value, Value)> {
    let (session, number) = parse(id)?;
    let state = ipc::call(&session, Request::Status, Duration::from_secs(5))?;
    for camera in state["cameras"].as_array().into_iter().flatten() {
        if let Some(job) = of_camera(&session, camera)
            .into_iter()
            .find(|job| job["id"] == id)
        {
            return Ok((job, camera.clone()));
        }
    }
    Err(Categorized::new(
        ErrorKind::Usage,
        format!("session {session} has no job {number}"),
    )
    .hint("capturefab jobs lists the jobs of every running session")
    .into())
}

/// Stop job `job` of `camera` in `session`.
pub(super) fn stop_job(session: &str, camera: &Value, job: &Value) -> Result<()> {
    let target = camera["info"]["id"].as_str().context("camera without ID")?;
    let command = if job["kind"] == "record" {
        Request::StopForwardIf {
            forwarding_id: camera["forwarding_id"]
                .as_str()
                .context("recording without an identity")?
                .to_owned(),
        }
    } else {
        let id = job["id"].as_str().unwrap_or_default();
        Request::CancelJob { id: parse(id)?.1 }
    };
    ipc::call_to(
        session,
        Some(target),
        command.clone(),
        crate::session::default_request_timeout(&command),
    )
    .map(drop)
}

fn active(job: &Value) -> bool {
    matches!(job["state"].as_str(), Some("armed" | "running"))
}

fn stop(global: &Global, id: &str) -> Result<Report> {
    let targets = if id == "all" {
        sessions()
            .into_iter()
            .filter(|(name, _)| global.session.as_deref().is_none_or(|only| only == name))
            .flat_map(|(name, state)| {
                state["cameras"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .flat_map(move |camera| {
                        let name = name.clone();
                        of_camera(&name, &camera)
                            .into_iter()
                            .filter(active)
                            .map(move |job| (name.clone(), camera.clone(), job))
                            .collect::<Vec<_>>()
                    })
            })
            .collect::<Vec<_>>()
    } else {
        let (job, camera) = find(id)?;
        vec![(parse(id)?.0, camera, job)]
    };
    let mut stopped = Vec::new();
    for (session, camera, job) in targets {
        if active(&job) {
            stop_job(&session, &camera, &job)?;
        }
        let id = job["id"].as_str().unwrap_or_default().to_owned();
        stopped.push(find(&id).map(|(job, _)| job).unwrap_or(job));
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

fn wait(id: &str, timeout: super::args::Wait) -> Result<Report> {
    super::signals::take_over();
    let deadline = match timeout {
        super::args::Wait::For(wait) => Instant::now().checked_add(wait),
        super::args::Wait::Forever => None,
    };
    loop {
        let (job, _) = find(id)?;
        if !active(&job) {
            return finished(job);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
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
        std::thread::sleep(POLL);
    }
}

/// A finished job as a result (done) or as its own error.
fn finished(job: Value) -> Result<Report> {
    match job["state"].as_str() {
        Some("done") => {
            let text = format!("{}\n", line(&job));
            Ok(Report::json(json!({ "job": job })).text(text))
        }
        Some("cancelled") => Err(Categorized::new(
            ErrorKind::Incomplete,
            format!(
                "job {} was stopped after {} file(s)",
                job["id"].as_str().unwrap_or_default(),
                job["saved"]
            ),
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

/// The job document, for `schema`.
pub const JOB: &str = "{id,camera,kind:capture|record,state:armed|running|done|failed|cancelled,saved,planned?,bytes?,last_file?,next_at?,ends_at?,started,ended?,error?:{code,message}}";
pub const LIST_FIELDS: &[(&str, &str)] = &[(
    "jobs",
    "[{id,camera,kind:capture|record,state:armed|running|done|failed|cancelled,saved,planned?,bytes?,last_file?,next_at?,ends_at?,started,ended?,error?:{code,message}}] — every running session's",
)];
pub const STOP_FIELDS: &[(&str, &str)] = &[(
    "stopped",
    "[{id,camera,kind,state,saved,planned?,bytes?,last_file?,started,ended?}] — the jobs as they are after stopping",
)];
pub const WAIT_FIELDS: &[(&str, &str)] = &[(
    "job",
    "{id,camera,kind,state:done,saved,planned?,bytes?,last_file?,started,ended} — a failed job exits with its own error code and details.job",
)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_name_a_session_and_a_number() {
        assert_eq!(parse("auto-3").unwrap(), ("auto".into(), 3));
        assert_eq!(parse("line-1-12").unwrap(), ("line-1".into(), 12));
        for bad in ["auto", "auto-0", "-3", "auto-x", "a b-1"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn capture_jobs_read_as_job_documents() {
        let camera = json!({"info": {"id": "rtsp://u:secret@cam/live"}});
        let job = json!({"id": 4, "status": "failed", "count": 3, "captured": 1,
            "first_at_ms": 0, "interval_ms": 0, "created_ms": 1, "ended_ms": 2,
            "last_file": "/tmp/a.png", "error": "no trigger",
            "error_tag": {"code": "trigger_timeout"}});
        let doc = capture("bench", &camera, &job);
        assert_eq!(doc["id"], "bench-4");
        assert_eq!(doc["state"], "failed");
        assert_eq!(doc["error"]["code"], "trigger_timeout");
        assert!(!doc.to_string().contains("secret"), "{doc}");
        assert_eq!(doc["planned"], 3);
        assert!(doc.get("next_at").is_none());
    }
}
