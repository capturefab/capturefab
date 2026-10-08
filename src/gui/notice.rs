//! What the workbench says about finished work: the one-line notice in the
//! toolbar, and the brief "copied" and "saved" states that controls show in
//! place.
use super::*;
use serde_json::Value;

/// How much a notice matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    /// It worked. Fades after `NOTICE_LIFE`.
    Done,
    /// It worked, with problems worth reading. Fades after `NOTICE_LIFE`.
    Warning,
    /// It failed. Stays until the next command, a newer error or a
    /// dismissal; later good news does not replace it.
    Error,
}

#[derive(Clone, Debug)]
pub(super) struct Notice {
    /// One line: the outcome, or what failed and why.
    pub(super) text: String,
    /// The full story, for a tooltip: the whole error, every warning, the full path.
    pub(super) detail: Option<String>,
    pub(super) level: Level,
    pub(super) at: Instant,
}

impl Notice {
    /// What a tooltip over the notice shows.
    pub(super) fn about(&self) -> String {
        self.detail.clone().unwrap_or_else(|| self.text.clone())
    }
}

/// The last capture that saved files.
#[derive(Clone, Debug)]
#[allow(dead_code)] // `path` and `count` are adopted by the inspector package
pub(super) struct Saved {
    /// The camera it came from, when known at the send site.
    pub(super) camera: Option<String>,
    /// The first file written.
    pub(super) path: String,
    /// How many files were written.
    pub(super) count: usize,
    pub(super) at: Instant,
}

impl Saved {
    /// The files a capture `result` reports, if it saved any.
    pub(super) fn from_result(camera: Option<String>, result: &Value, at: Instant) -> Option<Self> {
        let files = result["files"].as_array()?;
        let path = files.first()?.as_str()?.to_owned();
        Some(Self {
            camera,
            path,
            count: files.len(),
            at,
        })
    }
}

/// The `copied` key for the whole activity log.
pub(super) const COPIED_LOG: &str = "\0activity log";

/// Commands sent to several cameras at once under one label, reported in
/// one notice when the last finishes.
#[derive(Debug, Default)]
pub(super) struct Batch {
    total: usize,
    left: usize,
    /// The first line of each failure.
    failures: Vec<String>,
}

impl Workbench {
    /// Show a notice. One already showing swaps its text in place rather
    /// than fading out and in again.
    pub(super) fn set_notice(
        &mut self,
        text: impl Into<String>,
        detail: Option<String>,
        level: Level,
    ) {
        // A screenshot scene's notice outlasts the commands that set up the scene.
        if self.scene.notice {
            return;
        }
        // An error stands until it is dealt with: news of work that was
        // already on its way when it failed does not hide it.
        if level != Level::Error && self.error_showing() {
            return;
        }
        self.put_notice(text.into(), detail, level);
    }

    /// Whether an error notice shows and is not fading out.
    fn error_showing(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|n| n.level == Level::Error)
            && self.notice_shown.target() > 0.0
    }

    /// `set_notice` even while a screenshot scene holds its own notice.
    pub(super) fn put_notice(&mut self, text: String, detail: Option<String>, level: Level) {
        self.notice = Some(Notice {
            text,
            detail,
            level,
            at: self.now,
        });
        self.notice_shown.go(1.0, self.now);
    }

    /// Fade the notice out; `age_notice` drops it once it has gone.
    #[allow(dead_code)] // adopted by the chrome package's dismiss button
    pub(super) fn dismiss_notice(&mut self) {
        self.notice_shown.go(0.0, self.now);
    }

    /// A new command is on its way: the notice gives way at once, whatever
    /// its level, so the command's progress shows rather than an outcome it
    /// may change. The activity log keeps every error.
    pub(super) fn give_way(&mut self) {
        if !self.scene.notice && self.notice.is_some() {
            self.notice = None;
            self.notice_shown.set(0.0);
        }
    }

    /// Send `command` to each of `cameras` under `label`, as one batch with
    /// one notice; see `batch_result`.
    pub(super) fn send_batch(
        &mut self,
        cameras: Vec<String>,
        label: &str,
        command: impl Fn() -> SessionCommand,
    ) {
        for camera in cameras {
            let sent = self
                .submit(Some(&camera), label.to_owned(), command())
                .map(|pending| pending.batch = true)
                .is_some();
            if sent {
                let batch = self.batches.entry(label.to_owned()).or_default();
                batch.total += 1;
                batch.left += 1;
            }
        }
    }

    /// One command of the batch under `label` finished. Once the last has,
    /// the batch's notice: the usual outcome when all worked, otherwise an
    /// error counting the failures, listed in full as its detail.
    pub(super) fn batch_result(
        &mut self,
        label: &str,
        result: &Result<Value>,
    ) -> Option<(String, Option<String>, Level)> {
        let Some(batch) = self.batches.get_mut(label) else {
            return command_notice(label, result);
        };
        batch.left = batch.left.saturating_sub(1);
        if let Err(error) = result {
            let full = format!("{error:#}");
            batch
                .failures
                .push(full.lines().next().unwrap_or_default().to_owned());
        }
        if batch.left > 0 {
            return None;
        }
        let batch = self.batches.remove(label)?;
        let Some(first) = batch.failures.first() else {
            return command_notice(label, result);
        };
        let failed = batch.failures.len();
        let text = match label {
            "Starting streams" => format!("Could not start {failed} of {} streams", batch.total),
            "Stopping streams" => format!("Could not stop {failed} of {} streams", batch.total),
            _ => format!("{} on {failed} of {} cameras", failure(label), batch.total),
        };
        Some((
            format!("{text}: {first}"),
            Some(batch.failures.join("\n")),
            Level::Error,
        ))
    }

    /// Start fading done and warning notices at `NOTICE_LIFE`, and drop a
    /// notice once it has faded. From `tick()`, so the fade always starts
    /// at an update and runs in full.
    pub(super) fn age_notice(&mut self) {
        let Some(notice) = &self.notice else {
            return;
        };
        if !self.scene.notice
            && notice.level != Level::Error
            && self.now.saturating_duration_since(notice.at) >= motion::NOTICE_LIFE
        {
            self.notice_shown.go(0.0, self.now);
        }
        if self.notice_shown.target() == 0.0 && !self.notice_shown.animating(self.now) {
            self.notice = None;
        }
    }

    /// The notice's opacity now.
    pub(super) fn notice_alpha(&self) -> f32 {
        self.notice_shown.get(self.now)
    }

    /// Whether `value` was copied within the last `COPIED`. The slow tick
    /// redraws when that ends, so no per-frame redraws are needed.
    #[allow(dead_code)] // adopted by the sheets, chrome and sidebar packages
    pub(super) fn just_copied(&self, value: &str) -> bool {
        self.copied.as_ref().is_some_and(|(copied, at)| {
            copied == value && self.now.saturating_duration_since(*at) <= motion::COPIED
        })
    }

    /// Note that `value` went to the clipboard.
    pub(super) fn note_copied(&mut self, value: impl Into<String>) {
        self.copied = Some((value.into(), self.now));
    }

    /// Forget a copy once it no longer reads as just copied; from `tick()`.
    pub(super) fn age_copied(&mut self) {
        if self
            .copied
            .as_ref()
            .is_some_and(|(_, at)| self.now.saturating_duration_since(*at) > motion::COPIED)
        {
            self.copied = None;
        }
    }

    /// Whether a capture from `camera` (any camera for `None`) saved files
    /// within the last `SAVED`.
    #[allow(dead_code)] // adopted by the inspector and stage packages
    pub(super) fn just_saved(&self, camera: Option<&str>) -> bool {
        self.last_saved.as_ref().is_some_and(|saved| {
            self.now.saturating_duration_since(saved.at) <= motion::SAVED
                && camera.is_none_or(|camera| saved.camera.as_deref() == Some(camera))
        })
    }
}

/// The notice for a finished command: its outcome in one line, the full
/// story as detail, and how much it matters. `None` when nothing is worth
/// saying because the result is already on screen.
pub(super) fn command_notice(
    label: &str,
    result: &Result<Value>,
) -> Option<(String, Option<String>, Level)> {
    match result {
        Ok(value) => outcome(label, value),
        Err(error) => {
            let full = format!("{error:#}");
            let first = full.lines().next().unwrap_or_default();
            Some((
                format!("{}: {first}", failure(label)),
                Some(full.clone()),
                Level::Error,
            ))
        }
    }
}

fn outcome(label: &str, value: &Value) -> Option<(String, Option<String>, Level)> {
    let done = |text: String| Some((text, None, Level::Done));
    match label {
        "Saving capture" => Some(saved(value)),
        "Discovering cameras" => Some(discovered(value)),
        "Connecting camera" | "Connecting demo camera" => {
            done(match value["connected"]["model"].as_str() {
                Some(model) => format!("Connected to {model}"),
                None => "Connected".into(),
            })
        }
        "Disconnecting" => done("Disconnected".into()),
        // The selection and job list already show the result.
        "Selecting camera" | "Refreshing capture jobs" => None,
        "Starting stream" | "Starting demo stream" => done("Stream started".into()),
        "Starting streams" => done("Streams started".into()),
        "Stopping stream" => done("Stream stopped".into()),
        "Stopping streams" => done("Streams stopped".into()),
        "Enabling auto mode" => done("Auto mode on".into()),
        "Switching to manual" => done("Auto mode off".into()),
        "Updating auto balance" => done("Auto balance updated".into()),
        "Refreshing features" => done(match value["features"].as_array() {
            Some(features) => format!("Read {}", count(features.len(), "feature", "features")),
            None => "Features read".into(),
        }),
        "Scheduling capture" => done(match value["job"]["id"].as_u64() {
            Some(id) => format!("Capture job {id} scheduled"),
            None => "Capture scheduled".into(),
        }),
        "Cancelling capture job" => done(match value["cancelled"].as_u64() {
            Some(id) => format!("Capture job {id} cancelled"),
            None => "Capture job cancelled".into(),
        }),
        "Starting forwarding" => done(match value["forwarding"].as_str() {
            Some(output) => format!("Forwarding to {}", redact_address(output)),
            None => "Forwarding started".into(),
        }),
        "Stopping forwarding" => done("Forwarding stopped".into()),
        _ => {
            if let (Some(name), Some(set)) = (value["name"].as_str(), value.get("value")) {
                let mut text = format!("{} set to {}", words(name), value_text(set));
                // The camera left auto mode to take a manual value.
                if value.get("auto").is_some_and(Value::is_null) {
                    text.push_str(" · auto mode off");
                }
                done(text)
            } else if let Some(feature) = value["executed"].as_str() {
                done(format!("{} executed", words(feature)))
            } else if let Some(streaming) = value["streaming"].as_bool() {
                done(
                    if streaming {
                        "Stream started"
                    } else {
                        "Stream stopped"
                    }
                    .into(),
                )
            } else {
                done(format!("{label} · done"))
            }
        }
    }
}

/// "Capture failed", "Could not connect": what did not happen, for a label.
fn failure(label: &str) -> String {
    let text = match label {
        "Saving capture" => "Capture failed",
        "Discovering cameras" => "Discovery failed",
        "Connecting camera" | "Connecting demo camera" => "Could not connect",
        "Disconnecting" => "Could not disconnect",
        "Selecting camera" => "Could not select the camera",
        "Starting stream" | "Starting streams" | "Starting demo stream" => {
            "Could not start the stream"
        }
        "Stopping stream" | "Stopping streams" => "Could not stop the stream",
        "Enabling auto mode" => "Could not turn on auto mode",
        "Switching to manual" => "Could not switch to manual",
        "Updating auto balance" => "Could not update the auto balance",
        "Refreshing features" => "Could not read the features",
        "Scheduling capture" => "Could not schedule the capture",
        "Refreshing capture jobs" => "Could not read the capture jobs",
        "Cancelling capture job" => "Could not cancel the capture job",
        "Starting forwarding" => "Could not start forwarding",
        "Stopping forwarding" => "Could not stop forwarding",
        _ => {
            return if let Some(feature) = label.strip_prefix("Setting ") {
                format!("Could not set {}", words(feature))
            } else if let Some(feature) = label.strip_prefix("Executing ") {
                format!("Could not execute {}", words(feature))
            } else {
                format!("{label} failed")
            };
        }
    };
    text.into()
}

/// "Saved capture-0001.png", "Saved 10 files to captures".
fn saved(value: &Value) -> (String, Option<String>, Level) {
    let files: Vec<&str> = value["files"]
        .as_array()
        .map(|files| files.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let name = |path: &str| {
        std::path::Path::new(path).file_name().map_or_else(
            || path.to_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
    };
    let (mut text, detail) = match files.as_slice() {
        [] => {
            let frames = value["count"].as_u64().unwrap_or(1) as usize;
            (
                format!("Captured {}", count(frames, "frame", "frames")),
                None,
            )
        }
        [file] => (format!("Saved {}", name(file)), Some((*file).to_owned())),
        [first, ..] => {
            let folder = std::path::Path::new(first)
                .parent()
                .filter(|folder| !folder.as_os_str().is_empty());
            let text = match folder {
                Some(folder) => format!(
                    "Saved {} files to {}",
                    grouped(files.len() as u64),
                    name(&folder.to_string_lossy())
                ),
                None => format!("Saved {} files", grouped(files.len() as u64)),
            };
            (
                text,
                folder.map(|folder| folder.to_string_lossy().into_owned()),
            )
        }
    };
    // Only bucket destinations report a queue.
    if let (Some(queued), Some(destination)) = (
        value["queued_uploads"].as_u64(),
        value["destination"].as_str(),
    ) {
        let stuck = (files.len() as u64).saturating_sub(queued);
        if stuck == 0 {
            if queued > 0 {
                text.push_str(&format!(" · uploading to {destination}"));
            }
        } else {
            text.push_str(&if stuck as usize == files.len() {
                " · not queued for upload".to_owned()
            } else {
                format!(" · {} not queued for upload", grouped(stuck))
            });
            let why = format!(
                "{} not queued for upload to {destination}; Activity has the reason.",
                count(stuck as usize, "file was", "files were")
            );
            let detail = Some(match detail {
                Some(saved) => format!("{saved}\n{why}"),
                None => why,
            });
            return (text, detail, Level::Warning);
        }
    }
    (text, detail, Level::Done)
}

/// "Found 2 cameras"; with warnings, a warning listing them as detail.
fn discovered(value: &Value) -> (String, Option<String>, Level) {
    let found = match value["devices"].as_array().map_or(0, Vec::len) {
        0 => "No cameras found".to_owned(),
        found => format!("Found {}", count(found, "camera", "cameras")),
    };
    let warnings: Vec<&str> = value["warnings"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if warnings.is_empty() {
        (found, None, Level::Done)
    } else {
        (
            format!("{found} · {}", count(warnings.len(), "warning", "warnings")),
            Some(warnings.join("\n")),
            Level::Warning,
        )
    }
}

/// "1 camera", "12 cameras".
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", grouped(n as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn notice(label: &str, value: Value) -> (String, Option<String>, Level) {
        command_notice(label, &Ok(value)).expect("a notice")
    }

    #[test]
    fn discovery_warnings_remain_visible_after_success() {
        let (text, detail, level) = notice(
            "Discovering cameras",
            json!({
                "devices": [{"id": "sim:0"}],
                "warnings": ["Native discovery timed out", "USB access denied"]
            }),
        );
        assert_eq!(level, Level::Warning);
        assert_eq!(text, "Found 1 camera · 2 warnings");
        let detail = detail.unwrap();
        assert!(detail.contains("Native discovery timed out"));
        assert!(detail.contains("USB access denied"));
        assert_eq!(
            notice(
                "Discovering cameras",
                json!({"devices": [], "warnings": []})
            ),
            ("No cameras found".into(), None, Level::Done)
        );
        assert_eq!(
            notice(
                "Discovering cameras",
                json!({"devices": [{}, {}], "warnings": []})
            )
            .0,
            "Found 2 cameras"
        );
    }

    #[test]
    fn notices_state_the_outcome() {
        let cases = [
            (
                "Saving capture",
                json!({"files": ["shots/capture-0001.png"], "count": 1}),
                "Saved capture-0001.png",
            ),
            (
                "Saving capture",
                json!({"files": ["shots/a-1.png", "shots/a-2.png"], "count": 2}),
                "Saved 2 files to shots",
            ),
            (
                "Saving capture",
                json!({"files": ["c.png"], "destination": "lab", "queued_uploads": 1}),
                "Saved c.png · uploading to lab",
            ),
            (
                "Saving capture",
                json!({"files": ["c.png"], "destination": "nas"}),
                "Saved c.png",
            ),
            (
                "Connecting camera",
                json!({"connected": {"model": "Pattern camera"}}),
                "Connected to Pattern camera",
            ),
            (
                "Starting stream",
                json!({"streaming": true}),
                "Stream started",
            ),
            (
                "Stopping stream",
                json!({"streaming": false}),
                "Stream stopped",
            ),
            (
                "Setting ExposureTime",
                json!({"name": "ExposureTime", "value": 15000}),
                "Exposure Time set to 15000",
            ),
            (
                "Setting Gain",
                json!({"name": "Gain", "value": 2.5, "auto": null}),
                "Gain set to 2.5 · auto mode off",
            ),
            (
                "Executing AcquisitionStart",
                json!({"streaming": true}),
                "Stream started",
            ),
            (
                "Executing TriggerSoftware",
                json!({"executed": "TriggerSoftware"}),
                "Trigger Software executed",
            ),
            (
                "Refreshing features",
                json!({"features": [{}, {}, {}]}),
                "Read 3 features",
            ),
            (
                "Starting forwarding",
                json!({"forwarding": "rtsp://user:secret@host/live"}),
                "Forwarding to rtsp://[redacted]@host/live",
            ),
            (
                "Scheduling capture",
                json!({"job": {"id": 4}}),
                "Capture job 4 scheduled",
            ),
            ("Enabling auto mode", json!({"auto": {}}), "Auto mode on"),
            ("Something new", json!({}), "Something new · done"),
        ];
        for (label, value, expected) in cases {
            let (text, _, level) = notice(label, value);
            assert_eq!(text, expected, "{label}");
            assert_eq!(level, Level::Done, "{label}");
        }
        let (_, detail, _) = notice(
            "Saving capture",
            json!({"files": ["shots/capture-0001.png"]}),
        );
        assert_eq!(detail.as_deref(), Some("shots/capture-0001.png"));
        assert!(command_notice("Selecting camera", &Ok(json!({}))).is_none());
    }

    #[test]
    fn failures_say_what_did_not_happen_on_one_line() {
        let error = anyhow::anyhow!("frame timeout\nwhile waiting").context("save capture.png");
        let (text, detail, level) = command_notice("Saving capture", &Err(error)).unwrap();
        assert_eq!(level, Level::Error);
        assert_eq!(text, "Capture failed: save capture.png: frame timeout");
        assert_eq!(
            detail.as_deref(),
            Some("save capture.png: frame timeout\nwhile waiting")
        );
        let failed = |label: &str| {
            command_notice(label, &Err(anyhow::anyhow!("busy")))
                .unwrap()
                .0
        };
        assert_eq!(failed("Connecting camera"), "Could not connect: busy");
        assert_eq!(
            failed("Setting ExposureTime"),
            "Could not set Exposure Time: busy"
        );
        assert_eq!(
            failed("Starting stream"),
            "Could not start the stream: busy"
        );
        assert_eq!(failed("Doing something"), "Doing something failed: busy");
    }

    #[test]
    fn uploads_that_could_not_be_queued_are_a_warning() {
        let (text, detail, level) = notice(
            "Saving capture",
            json!({"files": ["shots/c.png"], "destination": "lab", "queued_uploads": 0}),
        );
        assert_eq!(level, Level::Warning);
        assert_eq!(text, "Saved c.png · not queued for upload");
        let detail = detail.unwrap();
        assert!(detail.starts_with("shots/c.png\n"));
        assert!(detail.contains("1 file was not queued for upload to lab"));
        let (text, _, level) = notice(
            "Saving capture",
            json!({
                "files": ["shots/a.png", "shots/b.png", "shots/c.png"],
                "destination": "lab",
                "queued_uploads": 1
            }),
        );
        assert_eq!(level, Level::Warning);
        assert_eq!(text, "Saved 3 files to shots · 2 not queued for upload");
    }

    fn bench() -> Workbench {
        Workbench::new(SessionHandle::new(), "test".into(), true, None)
    }

    fn sent(label: &str) -> Pending {
        Pending {
            label: label.into(),
            receiver: mpsc::channel().1,
            target: None,
            camera: None,
            feature: None,
            at: Instant::now(),
            batch: false,
        }
    }

    fn text(bench: &Workbench) -> Option<&str> {
        bench.notice.as_ref().map(|notice| notice.text.as_str())
    }

    #[test]
    fn notices_fade_but_errors_stay() {
        let mut bench = bench();
        let start = bench.now;
        bench.set_notice("Stream started", None, Level::Done);
        bench.now = start + motion::NOTICE_IN;
        assert_eq!(bench.notice_alpha(), 1.0);
        bench.now = start + motion::NOTICE_LIFE;
        bench.age_notice();
        assert!(bench.notice.is_some(), "fades first");
        bench.now += motion::NOTICE_OUT;
        bench.age_notice();
        assert!(bench.notice.is_none());

        bench.set_notice("Capture failed: busy", None, Level::Error);
        bench.now += motion::NOTICE_LIFE * 3;
        bench.age_notice();
        assert!(bench.notice.is_some(), "errors stay");
        bench.set_notice("Found 2 cameras", None, Level::Done);
        bench.set_notice("Found 2 cameras · 1 warning", None, Level::Warning);
        assert_eq!(
            text(&bench),
            Some("Capture failed: busy"),
            "over later news"
        );
        bench.set_notice("Could not connect: busy", None, Level::Error);
        assert_eq!(
            text(&bench),
            Some("Could not connect: busy"),
            "a newer error"
        );
        bench.dismiss_notice();
        bench.set_notice("Stream started", None, Level::Done);
        assert_eq!(text(&bench), Some("Stream started"), "once dismissed");

        bench.set_notice("Capture failed: busy", None, Level::Error);
        bench.give_way();
        assert!(bench.notice.is_none(), "a new command's progress shows");
        bench.set_notice("Capture failed: busy", None, Level::Error);
        bench.dismiss_notice();
        bench.now += motion::NOTICE_OUT;
        bench.age_notice();
        assert!(bench.notice.is_none());
        assert!(!bench.animating());
    }

    #[test]
    fn copy_buttons_confirm_in_place() {
        let mut bench = bench();
        let _ = bench.handle_message(Message::Copy("capturefab doctor".into()));
        let _ = bench.handle_message(Message::CopySessionCommand);
        assert!(bench.notice.is_none());
        assert!(bench.just_copied(&bench.session_command()));
        let _ = bench.copy_session_command(true);
        assert_eq!(
            text(&bench),
            Some("Session command copied"),
            "from the keyboard"
        );
    }

    #[test]
    fn batches_report_once_and_keep_failures() {
        let mut bench = bench();
        let batch = |bench: &mut Workbench, total: usize| {
            bench.batches.insert(
                "Starting streams".into(),
                Batch {
                    total,
                    left: total,
                    failures: Vec::new(),
                },
            );
            Pending {
                batch: true,
                ..sent("Starting streams")
            }
        };
        let pending = batch(&mut bench, 3);
        bench.settle(&pending, &Ok(json!({"streaming": true})));
        bench.settle(&pending, &Err(anyhow::anyhow!("camera busy\nretry later")));
        assert!(bench.notice.is_none(), "not before the last");
        bench.settle(&pending, &Ok(json!({"streaming": true})));
        assert_eq!(
            text(&bench),
            Some("Could not start 1 of 3 streams: camera busy")
        );
        assert_eq!(bench.notice.as_ref().unwrap().level, Level::Error);
        assert!(bench.batches.is_empty());

        bench.give_way();
        let pending = batch(&mut bench, 2);
        bench.settle(&pending, &Ok(json!({"streaming": true})));
        bench.settle(&pending, &Ok(json!({"streaming": true})));
        assert_eq!(text(&bench), Some("Streams started"));
    }

    #[test]
    fn failures_are_kept_where_they_were_asked_for() {
        let mut bench = bench();
        let discover = sent("Discovering cameras");
        bench.settle(&discover, &Err(anyhow::anyhow!("network unreachable")));
        assert_eq!(bench.side.discovery_issues, ["network unreachable"]);
        bench.settle(
            &discover,
            &Ok(json!({"devices": [], "warnings": ["USB access denied"]})),
        );
        assert_eq!(bench.side.discovery_issues, ["USB access denied"]);
        bench.settle(&discover, &Ok(json!({"devices": []})));
        assert!(bench.side.discovery_issues.is_empty());

        let set = Pending {
            feature: Some("Gain".into()),
            ..sent("Setting Gain")
        };
        bench.settle(&set, &Err(anyhow::anyhow!("out of range")));
        assert_eq!(bench.inspect.write_errors["Gain"], "out of range");
        let _ = bench.handle_message(Message::Draft("Gain".into(), "2".into()));
        assert!(bench.inspect.write_errors.is_empty(), "a new draft");
        bench.settle(&set, &Err(anyhow::anyhow!("out of range")));
        bench.settle(&set, &Ok(json!({"name": "Gain", "value": 2})));
        assert!(bench.inspect.write_errors.is_empty(), "a success");

        for n in 0..12 {
            bench.now += Duration::from_millis(1);
            let connect = Pending {
                target: Some(format!("10.0.0.{n}")),
                ..sent("Connecting camera")
            };
            bench.settle(&connect, &Err(anyhow::anyhow!("no answer")));
        }
        let failures = &bench.side.connect_failures;
        assert_eq!(failures.len(), 8, "the newest few");
        assert!(failures.contains_key("10.0.0.11") && !failures.contains_key("10.0.0.3"));
        bench.address = "10.0.0.11".into();
        let _ = bench.handle_message(Message::Address("10.0.0.1".into()));
        assert!(
            !bench.side.connect_failures.contains_key("10.0.0.11"),
            "edited"
        );
    }

    #[test]
    fn copies_and_saves_read_as_recent_briefly() {
        let mut bench = Workbench::new(SessionHandle::new(), "test".into(), true, None);
        let start = bench.now;
        bench.note_copied("capturefab status");
        assert!(bench.just_copied("capturefab status"));
        assert!(!bench.just_copied("something else"));
        bench.now = start + motion::COPIED + Duration::from_millis(1);
        assert!(!bench.just_copied("capturefab status"));
        bench.age_copied();
        assert!(bench.copied.is_none());

        bench.last_saved = Saved::from_result(
            Some("sim:0".into()),
            &json!({"files": ["a.png", "b.png"]}),
            bench.now,
        );
        assert_eq!(bench.last_saved.as_ref().map(|saved| saved.count), Some(2));
        assert!(bench.just_saved(Some("sim:0")));
        assert!(bench.just_saved(None));
        assert!(!bench.just_saved(Some("sim:1")));
        bench.now += motion::SAVED + Duration::from_millis(1);
        assert!(!bench.just_saved(None));
        assert!(Saved::from_result(None, &json!({"files": []}), bench.now).is_none());
    }
}
