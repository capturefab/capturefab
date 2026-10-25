//! Validated, repeatable sequences of the same commands used by session RPC.
//!
//! Loading is offline and validates every step before any command runs. Applying
//! stops on the first error; successful earlier steps remain in the session.
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

/// Local sources follow the same caller/file base as outputs. Network/device
/// locators and discovered camera IDs are never rewritten.
pub(crate) fn resolve_selector(selector: &mut String, base: &Path) -> Result<()> {
    // Preserve malformed-input errors: an empty local path must never become
    // the caller's valid current directory during absolute path resolution.
    validate_selector(selector)?;
    if let Some(path) = selector.strip_prefix("sim:replay:") {
        let mut path = path.to_owned();
        resolve_path(&mut path, base)?;
        *selector = format!("sim:replay:{path}");
    } else if selector
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        let path = &selector[5..];
        if !path.starts_with("//") && Path::new(path).is_relative() {
            let mut path = path.to_owned();
            resolve_path(&mut path, base)?;
            *selector = format!("file:{path}");
        }
    } else if selector.parse::<std::net::Ipv4Addr>().is_err()
        && crate::media::is_recording(selector)
        && base.join(&*selector).is_file()
    {
        resolve_path(selector, base)?;
    }
    Ok(())
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
    if let SessionCommand::Capture { output, .. } = &step.command {
        ensure!(
            output != "-",
            "workflow capture requires a file or directory; binary stdout output is unavailable in workflows"
        );
    }
    step.command.validate()
}

pub(crate) fn resolve_outputs(command: &mut SessionCommand, base: &Path) -> Result<()> {
    match command {
        SessionCommand::DatasetOpen { path, .. }
        | SessionCommand::DatasetOpenIf { path, .. }
        | SessionCommand::DatasetExport { output: path, .. }
        | SessionCommand::DatasetExportIf { output: path, .. } => resolve_path(path, base),
        SessionCommand::Capture {
            output,
            destination: None,
            ..
        }
        | SessionCommand::Schedule {
            output,
            destination: None,
            ..
        } => resolve_path(output, base),
        SessionCommand::Forward {
            output,
            destination: None,
            ..
        } if crate::media::is_recording(output) => {
            // `file:relative.mkv` is a local path, while `file:///...` and
            // `file://localhost/...` are already absolute file URLs.
            if let Some((scheme, path)) = output.split_once(':')
                && scheme.eq_ignore_ascii_case("file")
            {
                if !path.starts_with("//") && Path::new(path).is_relative() {
                    let mut path = path.to_owned();
                    resolve_path(&mut path, base)?;
                    *output = format!("file:{path}");
                }
                Ok(())
            } else {
                resolve_path(output, base)
            }
        }
        _ => Ok(()),
    }
}

fn resolve_path(output: &mut String, base: &Path) -> Result<()> {
    let path = Path::new(output);
    #[cfg(windows)]
    ensure!(
        !(path
            .components()
            .next()
            .is_some_and(|component| matches!(component, std::path::Component::Prefix(_)))
            && !path.is_absolute()),
        "drive-relative paths are ambiguous; use an absolute path or a relative path without a drive prefix"
    );
    if path.is_relative() {
        *output = base
            .join(path)
            .to_str()
            .context("workflow output path must be valid UTF-8")?
            .to_owned();
    }
    Ok(())
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
    fn context(&self, index: usize) -> Value {
        let mut command =
            serde_json::to_value(&self.command).expect("validated session command must serialize");
        redact_value(&mut command);
        let mut context = json!({"step": index, "command": command});
        if let Some(camera) = &self.camera {
            context["camera"] = Value::String(crate::media::redact_url(camera));
        }
        context
    }

    fn redact_error(&self, message: &str) -> String {
        let mut known =
            serde_json::to_value(&self.command).expect("validated session command must serialize");
        if let Some(camera) = &self.camera {
            known = json!([camera, known]);
        }
        redact_error(message, &known)
    }
}

/// `message` with every string in `known` that carries URL credentials
/// replaced by its redacted form, longest first, so an error that quotes a
/// step's own camera or output shows no secret even where it is not a URL.
pub(crate) fn redact_error(message: &str, known: &Value) -> String {
    let mut values = Vec::new();
    collect_strings(known, &mut values);
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut message = message.to_owned();
    for value in values {
        let redacted = crate::media::redact_url(&value);
        if redacted != value {
            message = message.replace(&value, &redacted);
        }
    }
    message
}

fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(value) => strings.push(value.clone()),
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_strings(value, strings)),
        Value::Object(fields) => fields
            .values()
            .for_each(|value| collect_strings(value, strings)),
        _ => {}
    }
}

/// Redact URL credentials in every string of `value`, in place.
pub(crate) fn redact_value(value: &mut Value) {
    match value {
        Value::String(value) => *value = crate::media::redact_url(value),
        Value::Array(values) => values.iter_mut().for_each(redact_value),
        Value::Object(fields) => fields.values_mut().for_each(redact_value),
        _ => {}
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
#[test]
fn resolves_relative_sources_without_rewriting_network_or_device_locators() {
    let base = std::env::temp_dir().join("capturefab-workflow-source-tests");
    let mut replay = "sim:replay:frames".to_owned();
    resolve_selector(&mut replay, &base).unwrap();
    assert_eq!(
        replay,
        format!("sim:replay:{}", base.join("frames").display())
    );
    let mut file = "file:clip.mp4".to_owned();
    resolve_selector(&mut file, &base).unwrap();
    assert_eq!(file, format!("file:{}", base.join("clip.mp4").display()));
    for source in [
        "192.168.1.10",
        "rtsp://camera/live",
        "sim:0",
        "avfoundation:0",
        "v4l2:/dev/video0",
        "dshow:video=Camera Name",
        "file:///tmp/clip.mp4",
    ] {
        let mut resolved = source.to_owned();
        resolve_selector(&mut resolved, &base).unwrap();
        assert_eq!(resolved, source);
    }
    let workflow = from_reader(
        serde_json::to_vec(&json!({"version":1,"steps":[
            {"command":{"op":"connect","camera":"sim:replay:frames","timeout_ms":2000}},
            {"camera":"sim:replay:frames","command":{"op":"get","feature":"Width"}}
        ]}))
        .unwrap()
        .as_slice(),
        &base,
    )
    .unwrap();
    assert_eq!(workflow.plan()["steps"][0]["command"]["camera"], replay);
    assert_eq!(workflow.plan()["steps"][1]["camera"], replay);
}

#[test]
fn invalid_source_selectors_are_rejected_before_path_resolution() {
    let base = std::env::temp_dir();
    for source in ["sim:replay:", "file:", "FiLe:"] {
        let mut selector = source.to_owned();
        assert!(resolve_selector(&mut selector, &base).is_err(), "{source}");
        assert_eq!(
            selector, source,
            "invalid selectors retain their original meaning"
        );
    }
}
