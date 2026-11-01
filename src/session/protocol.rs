//! The camera worker protocol: newline-delimited JSON requests on a worker's
//! stdin, and replies and events on a private copy of its stdout. Every line
//! a worker writes carries `seq`, one more than the line before. The first
//! is `hello`, which says the worker is ready and which protocol it speaks.
//!
//! A request is acknowledged with `accepted` as soon as it is queued, then
//! `started` with its budget when the camera thread takes it, then answered
//! with `reply`, after every event it caused. Status and ping requests are
//! answered at once, beside whatever the camera thread is doing.
use super::SessionCommand;
use crate::engine::saved::SavedFile;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol this build's coordinator and workers speak. A worker of
/// another version is refused before it is sent a request.
pub(crate) const PROTOCOL: u32 = 1;
/// The longest line either side writes or reads. A reply that would be
/// longer is replaced with an error, so the pipe never loses its place.
pub(crate) const MAX_LINE: usize = 16 * 1024 * 1024;
/// The longest request line a worker reads.
pub(crate) const MAX_REQUEST: usize = 1024 * 1024;

/// What the camera thread of a worker runs: a public command, or one only a
/// coordinator sends. One lives per request, so its size does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerCommand {
    /// Apply this camera's share of the GigE bandwidth a network scan planned.
    OptimizeNetwork { bytes_per_second: u64 },
    /// Single-feature reads and writes, which unit tests drive the camera
    /// thread with; no coordinator sends them.
    #[cfg(test)]
    Get { feature: String },
    #[cfg(test)]
    Set { feature: String, value: String },
    /// Write several features in order as one camera change: a failure
    /// restores every value. Tests drive the rollback checkpoints with it.
    #[cfg(test)]
    SetMany { values: Vec<super::FeatureValue> },
    #[cfg(test)]
    Execute { feature: String },
    #[cfg(test)]
    Features,
    /// End the camera's recording or forward, as a stop of its job does.
    #[cfg(test)]
    StopForward,
    #[serde(untagged)]
    Session(SessionCommand),
}
impl From<SessionCommand> for WorkerCommand {
    fn from(command: SessionCommand) -> Self {
        Self::Session(command)
    }
}
impl WorkerCommand {
    /// The public command, if this is one.
    pub(crate) fn session(&self) -> Option<&SessionCommand> {
        match self {
            Self::Session(command) => Some(command),
            _ => None,
        }
    }
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        match self {
            Self::OptimizeNetwork { bytes_per_second } => {
                ensure!(
                    (1..=125_000_000_000).contains(bytes_per_second),
                    "network budget must be 1..125000000000 bytes per second"
                );
                Ok(())
            }
            #[cfg(test)]
            Self::Get { feature } | Self::Execute { feature } | Self::Set { feature, .. } => {
                super::validate_feature(feature)
            }
            #[cfg(test)]
            Self::SetMany { values } => {
                ensure!(
                    (1..=super::MAX_SET_MANY).contains(&values.len()),
                    "set_many takes 1..32 feature values"
                );
                for value in values {
                    super::validate_feature(&value.feature)?;
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Features | Self::StopForward => Ok(()),
            Self::Session(command) => command.validate(),
        }
    }
    /// Whether it writes camera settings: a worker ended while it ran may
    /// leave the camera half-configured.
    pub(crate) fn changes_settings(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Set { .. } | Self::SetMany { .. } | Self::Execute { .. } => true,
            Self::Session(command) => matches!(
                command,
                SessionCommand::SetValues { dry_run: false, .. }
                    | SessionCommand::ApplySetup { .. }
                    | SessionCommand::Exec { .. }
                    | SessionCommand::WriteMemory { .. }
                    | SessionCommand::Auto { .. }
                    | SessionCommand::Manual { .. }
            ),
            _ => false,
        }
    }
    /// How long the camera thread may take on this command once it starts,
    /// before its worker counts as hung: the frame waits and work it may
    /// do. The coordinator never ends a worker sooner than 10 s.
    pub(crate) fn budget_ms(&self) -> u64 {
        match self {
            Self::Session(command) => match command {
                SessionCommand::Capture { .. } | SessionCommand::Frame { .. } => {
                    crate::session::default_request_timeout(command)
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX)
                }
                SessionCommand::ApplySetup { .. } => 240_000,
                SessionCommand::Connect { timeout_ms, .. } => timeout_ms.saturating_add(30_000),
                _ => 60_000,
            },
            _ => 60_000,
        }
    }
}

/// One line a coordinator sends a worker.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Input {
    /// Queue `command` for the camera thread.
    Run { id: u64, command: WorkerCommand },
    /// Cancel request `id`: answered `interrupted` while still queued, or
    /// its frame waits end. Unknown and finished requests are ignored.
    Cancel { id: u64 },
    /// Stop job `job`; its `job_end` follows.
    Stop { job: u64 },
    /// The worker's state, answered at once from its snapshot.
    Status { id: u64 },
    /// How long ago the camera thread last made progress, answered at once.
    Ping { id: u64 },
    /// Stop taking requests, end jobs, finish recordings and exit.
    Shutdown,
}

/// A worker's line: its sequence number and what it says.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Line {
    pub seq: u64,
    #[serde(flatten)]
    pub output: Output,
}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStatus {
    Done,
    Failed,
    /// `jobs stop`, or a stop from the client that ran it.
    Cancelled,
    /// Ended by the session or worker shutting down.
    Interrupted,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Output {
    Hello {
        protocol: u32,
        build: String,
    },
    Accepted {
        id: u64,
    },
    Started {
        id: u64,
        budget_ms: u64,
    },
    Reply {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Value>,
    },
    /// A file, once it is published. `job` is unset for a capture command's.
    Saved {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job: Option<u64>,
        file: SavedFile,
    },
    /// A job's state now; only the newest of a job's waiting progress lines
    /// is written.
    Progress {
        job: u64,
        progress: Value,
    },
    /// The one last line of a job, after its files' `saved` lines.
    JobEnd {
        job: u64,
        status: JobStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Value>,
        #[serde(default)]
        summary: Value,
    },
    Log {
        level: String,
        message: String,
    },
}

/// An error as it crosses a pipe or connection: its message, and its
/// reason, hint and details, so the far side reports it the same way.
pub(crate) fn wire_error(error: &anyhow::Error) -> Value {
    let mut wire = serde_json::json!({"message": format!("{error:#}")});
    if let Some(tag) = crate::types::error_tag(error)
        && let (Some(wire), Ok(Value::Object(tag))) =
            (wire.as_object_mut(), serde_json::to_value(tag))
    {
        wire.extend(tag);
    }
    wire
}
/// The error `wire` describes.
pub(crate) fn from_wire(wire: &Value) -> anyhow::Error {
    let message = wire["message"]
        .as_str()
        .unwrap_or("the camera worker failed")
        .to_owned();
    let tag = serde_json::from_value::<crate::types::ErrorTag>(wire.clone()).ok();
    crate::types::error_from_tag(tag, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_carry_public_or_internal_commands() {
        let run = |command: WorkerCommand| {
            let line = serde_json::to_string(&Input::Run { id: 3, command }).unwrap();
            serde_json::from_str::<Input>(&line).unwrap()
        };
        assert!(matches!(
            run(WorkerCommand::Features),
            Input::Run {
                id: 3,
                command: WorkerCommand::Features
            }
        ));
        assert!(matches!(
            run(SessionCommand::Info.into()),
            Input::Run {
                command: WorkerCommand::Session(SessionCommand::Info),
                ..
            }
        ));
        let text = serde_json::to_value(Input::Stop { job: 4 }).unwrap();
        assert_eq!(text, json!({"op": "stop", "job": 4}));
        assert_eq!(
            serde_json::to_value(Input::Shutdown).unwrap(),
            json!({"op": "shutdown"})
        );
        // An internal operation never parses as a public command.
        assert!(SessionCommand::from_json_slice(br#"{"op":"features"}"#).is_err());
        assert!(serde_json::from_str::<Input>(r#"{"op":"run","id":1}"#).is_err());
    }

    #[test]
    fn settings_changes_are_known_by_what_they_write() {
        let set = |dry_run| SessionCommand::SetValues {
            values: vec![crate::controls::Assignment::parse("Width=320").unwrap()],
            dry_run,
        };
        assert!(WorkerCommand::from(set(false)).changes_settings());
        assert!(!WorkerCommand::from(set(true)).changes_settings());
        assert!(!WorkerCommand::from(SessionCommand::Info).changes_settings());
        assert!(!WorkerCommand::Features.changes_settings());
        assert!(
            WorkerCommand::Set {
                feature: "Width".into(),
                value: "320".into()
            }
            .changes_settings()
        );
    }

    #[test]
    fn lines_say_what_they_are_after_their_sequence_number() {
        let line = Line {
            seq: 7,
            output: Output::JobEnd {
                job: 2,
                status: JobStatus::Cancelled,
                error: None,
                summary: json!({"captured": 3}),
            },
        };
        let text = serde_json::to_value(&line).unwrap();
        assert_eq!(
            text,
            json!({"seq": 7, "event": "job_end", "job": 2, "status": "cancelled", "summary": {"captured": 3}})
        );
        let read: Line = serde_json::from_value(text).unwrap();
        assert!(matches!(
            read.output,
            Output::JobEnd {
                status: JobStatus::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn errors_keep_their_reason_across_the_pipe() {
        let error = crate::types::Categorized::new(crate::types::ErrorKind::CameraBusy, "held")
            .hint("stop it")
            .details(json!({"held_by": {"pid": 9}}))
            .into();
        let back = from_wire(&wire_error(&error));
        assert_eq!(crate::cli::error_code(&back), ("camera_busy", 3));
        let classified = crate::types::classify(&back);
        assert_eq!(classified.hint.as_deref(), Some("stop it"));
        assert_eq!(classified.details.unwrap()["held_by"]["pid"], 9);
        assert_eq!(back.to_string(), "held");
    }
}
