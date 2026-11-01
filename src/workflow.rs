//! Validated, repeatable sequences of the same commands used by session RPC.
//!
//! Loading is offline and validates every step before any command runs. Applying
//! stops on the first error; successful earlier steps remain in the session.
use crate::cli::paths::{resolve_outputs, resolve_selector};
use crate::scrub::{redact_error, redact_value};
use crate::session::{SessionCommand, deserialize_command, validate_selector};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fmt,
    fs::File,
    io::{self, Read},
    path::Path,
};

pub const VERSION: u32 = 1;
pub const MAX_STEPS: usize = 256;
pub const MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    #[serde(default)]
    camera: Option<String>,
    #[serde(deserialize_with = "deserialize_command")]
    command: SessionCommand,
}

/// A workflow whose complete command sequence has passed offline validation.
/// Construct with [`load`] or [`from_reader`] rather than deserializing directly.
#[derive(Debug)]
pub struct Workflow {
    steps: Vec<Step>,
}

/// Read a workflow JSON file, or standard input when `path` is `-`.
/// Relative local outputs use the workflow file's directory; standard input
/// uses the current directory. Named destination outputs keep their own base.
pub fn load(path: &Path) -> Result<Workflow> {
    if path == Path::new("-") {
        let base = std::env::current_dir().context("resolve workflow input directory")?;
        return from_reader(io::stdin().lock(), &base);
    }
    let path = std::path::absolute(path).context("resolve workflow file path")?;
    let file = File::open(&path)
        .with_context(|| format!("cannot read workflow file {}", path.display()))?;
    let base = path
        .parent()
        .context("workflow file needs a parent directory")?;
    from_reader(file, base)
}

/// Load bounded JSON from a reader, resolving local outputs against `base`.
/// No session, camera, storage ledger, output directory or encoder is opened.
pub fn from_reader<R: Read>(reader: R, base: &Path) -> Result<Workflow> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("cannot read workflow JSON")?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "invalid workflow: JSON must be at most {MAX_BYTES} bytes"
    );
    let mut document: Document = serde_json::from_slice(&bytes).context("invalid workflow JSON")?;
    ensure!(
        document.version == VERSION,
        "unsupported workflow version {}; expected {VERSION}",
        document.version
    );
    ensure!(
        (1..=MAX_STEPS).contains(&document.steps.len()),
        "invalid workflow: steps must contain 1..{MAX_STEPS} commands"
    );
    let base = std::path::absolute(base).context("resolve workflow base directory")?;
    for (index, step) in document.steps.iter_mut().enumerate() {
        validate_step(step)
            .and_then(|()| {
                if let Some(camera) = &mut step.camera {
                    resolve_selector(camera, &base)?;
                }
                if let SessionCommand::Connect { camera, .. } | SessionCommand::Select { camera } =
                    &mut step.command
                {
                    resolve_selector(camera, &base)?;
                }
                Ok(())
            })
            .and_then(|()| resolve_outputs(&mut step.command, &base))
            .and_then(|()| validate_step(step))
            .with_context(|| format!("invalid workflow step {}", index + 1))?;
    }
    Ok(Workflow {
        steps: document.steps,
    })
}

fn validate_step(step: &Step) -> Result<()> {
    if let Some(camera) = &step.camera {
        validate_selector(camera)?;
        ensure!(
            !matches!(
                step.command,
                SessionCommand::Connect { .. }
                    | SessionCommand::Select { .. }
                    | SessionCommand::Discover { .. }
                    | SessionCommand::NetworkScan { .. }
            ),
            "this command does not accept a step camera; connect/select use command.camera and discover/network_scan address the session"
        );
    }
    if let SessionCommand::Connect { login: Some(_), .. } = &step.command {
        return Err(crate::types::Usage(
            "workflows cannot carry camera passwords; save the sign-in with `capturefab login add`"
                .into(),
        )
        .into());
    }
    if let SessionCommand::Capture { output, .. } = &step.command {
        ensure!(
            output != "-",
            "workflow capture requires a file or directory; binary stdout output is unavailable in workflows"
        );
    }
    step.command.validate()
}

impl Workflow {
    /// Return resolved commands in execution order with URL secrets redacted.
    pub fn plan(&self) -> Value {
        json!({
            "version": VERSION,
            "step_count": self.steps.len(),
            "steps": self.steps.iter().enumerate()
                .map(|(index, step)| step.context(index + 1)).collect::<Vec<_>>()
        })
    }

    /// Execute through a local or remote session callback in document order.
    /// Errors preserve completed results and identify the failed step. This is
    /// a sequence of operations, not a transaction: no rollback is attempted.
    pub fn apply<F>(&self, mut execute: F) -> std::result::Result<Value, WorkflowFailure>
    where
        F: FnMut(Option<&str>, SessionCommand) -> Result<Value>,
    {
        let mut completed = Vec::new();
        for (index, step) in self.steps.iter().enumerate() {
            let mut context = step.context(index + 1);
            match execute(step.camera.as_deref(), step.command.clone()) {
                Ok(mut result) => {
                    redact_value(&mut result);
                    context["result"] = result;
                    completed.push(context);
                }
                Err(error) => {
                    let message = step.redact_error(&format!("{error:#}"));
                    return Err(WorkflowFailure {
                        completed,
                        step_count: self.steps.len(),
                        failed: context,
                        message,
                        source: error,
                    });
                }
            }
        }
        Ok(json!({
            "step_count": self.steps.len(),
            "completed_steps": completed.len(),
            "completed": completed,
        }))
    }
}

impl Step {
    /// The command as JSON, never with a sign-in (validation refuses one;
    /// this is a second safeguard).
    fn command_value(&self) -> Value {
        let mut command =
            serde_json::to_value(&self.command).expect("validated session command must serialize");
        if let Some(fields) = command.as_object_mut() {
            fields.remove("login");
        }
        command
    }

    fn context(&self, index: usize) -> Value {
        let mut command = self.command_value();
        redact_value(&mut command);
        let mut context = json!({"step": index, "command": command});
        if let Some(camera) = &self.camera {
            context["camera"] = Value::String(crate::media::redact_url(camera));
        }
        context
    }

    fn redact_error(&self, message: &str) -> String {
        let mut known = self.command_value();
        if let Some(camera) = &self.camera {
            known = json!([camera, known]);
        }
        let mut message = redact_error(message, &known);
        if let SessionCommand::Connect {
            login: Some(login), ..
        } = &self.command
            && !login.expose().is_empty()
        {
            message = message.replace(login.expose(), "<redacted>");
        }
        message
    }
}

/// An interrupted workflow, including successful earlier steps for recovery.
#[derive(Debug)]
pub struct WorkflowFailure {
    completed: Vec<Value>,
    step_count: usize,
    failed: Value,
    message: String,
    source: anyhow::Error,
}

impl WorkflowFailure {
    /// Structured details suitable for a CLI error envelope.
    pub fn result(&self) -> Value {
        let mut result = self.failed.clone();
        result.as_object_mut().unwrap().remove("step");
        result["failed_step"] = self.failed["step"].clone();
        result["step_count"] = json!(self.step_count);
        result["completed_steps"] = json!(self.completed.len());
        result["completed"] = json!(self.completed);
        result["error"] = json!(self.message);
        result
    }
}

impl fmt::Display for WorkflowFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "workflow stopped at step {} of {} after {} completed steps: {}",
            self.failed["step"],
            self.step_count,
            self.completed.len(),
            self.message
        )
    }
}

impl std::error::Error for WorkflowFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn parse(value: Value) -> Result<Workflow> {
        from_reader(serde_json::to_vec(&value)?.as_slice(), Path::new("."))
    }

    fn capture(output: &str) -> Value {
        json!({"op":"capture", "output":output, "count":1, "timeout_ms":1000, "format":"png"})
    }

    fn forward(output: &str) -> Value {
        json!({"op":"forward", "output":output, "codec":"h264", "encoder":"auto", "fps":30.0, "bitrate":"4M"})
    }

    #[test]
    fn rejects_unknown_and_duplicate_fields_at_every_level() {
        for source in [
            r#"{"version":1,"steps":[{"command":{"op":"status"}}],"typo":true}"#,
            r#"{"version":1,"steps":[{"command":{"op":"status"},"typo":true}]}"#,
            r#"{"version":1,"steps":[{"command":{"op":"status","typo":true}}]}"#,
            r#"{"version":1,"steps":[{"command":{"op":"set","feature":"Width","value":"128","typo":true}}]}"#,
            r#"{"version":1,"version":1,"steps":[{"command":{"op":"status"}}]}"#,
            r#"{"version":1,"steps":[{"camera":"sim:0","camera":"sim:1","command":{"op":"status"}}]}"#,
            r#"{"version":1,"steps":[{"command":{"op":"status","op":"start"}}]}"#,
            r#"{"version":1,"steps":[{"command":{"op":"capture","output":"x.png","count":1,"timeout_ms":1000,"format":"png","storage":{"max_bytes":100,"max_bytes":100}}}]}"#,
            r#"{"version":1,"steps":[{"command":{"op":"status"}}]} {}"#,
        ] {
            assert!(
                from_reader(source.as_bytes(), Path::new(".")).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn camera_passwords_never_ride_in_workflows() {
        let connect = json!({"op":"connect","camera":"rtsp://cam.local/live","timeout_ms":1000,
            "login":{"user":"admin","password":"hunter2"}});
        let error = parse(json!({"version":1,"steps":[{"command":connect}]})).unwrap_err();
        assert!(
            format!("{error:#}").contains("capturefab login add"),
            "{error:#}"
        );
        // Should a sign-in get past validation, the plan still leaves it out.
        let workflow = Workflow {
            steps: vec![Step {
                camera: None,
                command: SessionCommand::Connect {
                    camera: "rtsp://cam.local/live".into(),
                    timeout_ms: 1000,
                    login: Some(crate::logins::CameraLogin::new("admin", "hunter2").unwrap()),
                    saved_login: true,
                },
            }],
        };
        let plan = workflow.plan().to_string();
        assert!(
            !plan.contains("hunter2") && !plan.contains("login"),
            "{plan}"
        );
        let failure = workflow
            .apply(|_, _| anyhow::bail!("camera echoed hunter2"))
            .unwrap_err();
        assert!(!format!("{failure}").contains("hunter2"), "{failure}");
    }

    #[test]
    fn enforces_version_step_and_input_bounds() {
        for version in [0, 2] {
            assert!(
                parse(json!({"version":version,"steps":[{"command":{"op":"status"}}]})).is_err()
            );
        }
        for count in [0, MAX_STEPS + 1] {
            assert!(
                parse(json!({"version":1,"steps":vec![json!({"command":{"op":"status"}});count]}))
                    .is_err()
            );
        }
        assert!(
            parse(json!({"version":1,"steps":vec![json!({"command":{"op":"status"}});MAX_STEPS]}))
                .is_ok()
        );
        let mut bytes = br#"{"version":1,"steps":[{"command":{"op":"status"}}]}"#.to_vec();
        bytes.resize(MAX_BYTES, b' ');
        assert!(from_reader(bytes.as_slice(), Path::new(".")).is_ok());
        bytes.push(b' ');
        assert!(from_reader(bytes.as_slice(), Path::new(".")).is_err());
    }

    #[test]
    fn validates_every_step_and_disallows_ambiguous_targets_or_stdout() {
        let mut called = false;
        let result = parse(json!({"version":1,"steps":[
            {"command":{"op":"start"}},
            {"command":{"op":"set","feature":"bad feature","value":"1"}}
        ]}));
        if let Ok(workflow) = result {
            let _ = workflow.apply(|_, _| {
                called = true;
                Ok(Value::Null)
            });
        }
        assert!(!called, "a later invalid step must prevent all execution");
        for command in [
            json!({"op":"connect","camera":"sim:0","timeout_ms":1000}),
            json!({"op":"select","camera":"sim:0"}),
            json!({"op":"discover","timeout_ms":1000,"simulated":true}),
            json!({"op":"network_scan","timeout_ms":1000}),
        ] {
            assert!(
                parse(json!({"version":1,"steps":[{"camera":"sim:0","command":command}]})).is_err()
            );
        }
        assert!(parse(json!({"version":1,"steps":[{"command":capture("-")}]})).is_err());
        assert!(
            parse(json!({"version":1,"steps":[{"camera":" ","command":{"op":"status"}}]})).is_err()
        );
    }

    #[test]
    fn a_recording_step_carries_its_duration_for_the_session_to_keep() {
        let hour = 3_600_000;
        let mut timed = forward("clip.mkv");
        timed["duration_ms"] = json!(hour);
        timed["max_file_bytes"] = json!(crate::media::recording_reservation("4M", hour).unwrap());
        let workflow =
            parse(json!({"version":1,"steps":[{"camera":"sim:0","command":timed}]})).unwrap();
        assert!(matches!(
            workflow.steps[0].command,
            SessionCommand::Forward {
                duration_ms: Some(3_600_000),
                ..
            }
        ));
        assert_eq!(workflow.plan()["steps"][0]["command"]["duration_ms"], hour);
        // Checked like `record --duration`, before any step runs.
        for duration in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("1h"),
            json!(crate::scheduling::MAX_DURATION_MS + 1),
        ] {
            let mut invalid = forward("clip.mkv");
            invalid["duration_ms"] = duration.clone();
            assert!(
                parse(json!({"version":1,"steps":[{"command":invalid}]})).is_err(),
                "{duration}"
            );
        }
    }

    #[test]
    fn bundled_examples_validate_and_timed_recordings_fit_their_reservation() {
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/workflows");
        let mut timed = 0;
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let workflow = load(&path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()));
            for step in &workflow.steps {
                if let SessionCommand::Forward {
                    bitrate,
                    max_file_bytes,
                    duration_ms: Some(duration_ms),
                    ..
                } = &step.command
                {
                    let reservation =
                        crate::media::recording_reservation(bitrate, *duration_ms).unwrap();
                    assert!(*max_file_bytes >= reservation, "{}", path.display());
                    timed += 1;
                }
            }
        }
        assert!(timed > 0, "an example records for a set time");
    }

    #[test]
    fn resolves_local_outputs_and_preserves_urls_and_destination_names() {
        let base = std::env::temp_dir().join("capturefab-workflow-path-tests");
        let mut schedule = capture("scheduled/shot-{frame}.png");
        schedule["op"] = json!("schedule");
        schedule["first_at_ms"] = json!(0);
        schedule["interval_ms"] = json!(1000);
        let mut destination = capture("shots/frame.png");
        destination["destination"] = json!("archive");
        let document = json!({"version":1,"steps":[
            {"command":capture("shots/")},
            {"command":schedule},
            {"command":forward("clip.mkv")},
            {"command":forward("file:second.mkv")},
            {"command":forward("srt://localhost:9000?passphrase=secret")},
            {"command":destination}
        ]});
        let workflow =
            from_reader(serde_json::to_vec(&document).unwrap().as_slice(), &base).unwrap();
        let outputs: Vec<_> = workflow
            .steps
            .iter()
            .map(|step| match &step.command {
                SessionCommand::Capture { output, .. }
                | SessionCommand::Schedule { output, .. }
                | SessionCommand::Forward { output, .. } => output.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(outputs[0], base.join("shots/").to_str().unwrap());
        assert!(outputs[0].ends_with(['/', '\\']));
        assert_eq!(
            outputs[1],
            base.join("scheduled/shot-{frame}.png").to_str().unwrap()
        );
        assert_eq!(outputs[2], base.join("clip.mkv").to_str().unwrap());
        assert_eq!(
            outputs[3],
            format!("file:{}", base.join("second.mkv").display())
        );
        assert_eq!(outputs[4], "srt://localhost:9000?passphrase=secret");
        assert_eq!(outputs[5], "shots/frame.png");
        assert!(!workflow.plan().to_string().contains("secret"));
    }

    #[test]
    fn stops_on_first_failure_and_keeps_redacted_completed_results() {
        let workflow = parse(json!({"version":1,"steps":[
            {"camera":"rtsp://user:secret@localhost/live","command":{"op":"status"}},
            {"camera":"sim:0","command":{"op":"start"}},
            {"command":{"op":"stop"}}
        ]}))
        .unwrap();
        let mut calls = Vec::new();
        let failure = workflow
            .apply(|camera, command| {
                calls.push((camera.map(str::to_owned), command));
                if calls.len() == 1 {
                    Ok(json!({"url":"srt://localhost:9000?passphrase=secret"}))
                } else {
                    anyhow::bail!("camera is unavailable")
                }
            })
            .unwrap_err();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].0.as_deref(),
            Some("rtsp://user:secret@localhost/live")
        );
        let result = failure.result();
        assert_eq!(result["failed_step"], 2);
        assert_eq!(result["completed_steps"], 1);
        assert_eq!(result["camera"], "sim:0");
        assert!(!result.to_string().contains("secret"));
        assert!(failure.to_string().contains("step 2 of 3"));
    }

    #[test]
    fn applies_simulator_settings_in_order_and_acquires_a_frame() {
        let workflow = parse(json!({"version":1,"steps":[
            {"command":{"op":"connect","camera":"sim:workflow","timeout_ms":1000}},
            {"camera":"sim:workflow","command":{"op":"set","feature":"Width","value":"128"}},
            {"camera":"sim:workflow","command":{"op":"set","feature":"Height","value":"96"}},
            {"camera":"sim:workflow","command":{"op":"start"}},
            {"camera":"sim:workflow","command":{"op":"get","feature":"Width"}}
        ]}))
        .unwrap();
        let mut camera = None;
        let result = workflow
            .apply(|target, command| {
                if let SessionCommand::Connect {
                    camera: selector,
                    timeout_ms,
                    ..
                } = command
                {
                    assert!(target.is_none());
                    camera = Some(crate::camera::Camera::open(
                        &selector,
                        &[],
                        Duration::from_millis(timeout_ms),
                    )?);
                    return Ok(json!({"connected":selector}));
                }
                assert_eq!(target, Some("sim:workflow"));
                let camera = camera.as_mut().unwrap();
                match command {
                    SessionCommand::Set { feature, value } => {
                        camera.set(&feature, &value)?;
                        Ok(Value::Null)
                    }
                    SessionCommand::Start => {
                        camera.start()?;
                        Ok(Value::Null)
                    }
                    SessionCommand::Get { feature } => camera.get(&feature),
                    _ => unreachable!(),
                }
            })
            .unwrap();
        assert_eq!(result["completed_steps"], 5);
        assert_eq!(result["completed"][4]["result"], 128);
        let frame = camera
            .as_mut()
            .unwrap()
            .next_frame(Duration::from_secs(1))
            .unwrap();
        assert_eq!((frame.width, frame.height), (128, 96));
    }
}
