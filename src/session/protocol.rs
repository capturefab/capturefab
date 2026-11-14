//! The camera worker protocol: newline-delimited JSON requests on a worker's
//! stdin, and replies and events on a private copy of its stdout. Every line
//! a worker writes carries `seq`, one more than the line before. The first
//! is `hello`, which says the worker is ready and which protocol it speaks.
//!
//! A request is acknowledged with `accepted` as soon as it is queued, then
//! `started` with its budget when the camera thread takes it, then answered
//! with `reply`, after every event it caused. Status and ping requests are
//! answered at once, beside whatever the camera thread is doing.
#![forbid(unsafe_code)]
#![deny(clippy::missing_const_for_fn)]
use super::SessionCommand;
use crate::engine::saved::SavedFile;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol this build's coordinator and workers speak. A worker of
/// another version is refused before it is sent a request.
// Ownership retirement, binding checks and bounded named-register commands
// require matching coordinator and worker implementations.
pub(crate) const PROTOCOL: u32 = 10;
/// The longest line either side writes or reads. A reply that would be
/// longer is replaced with an error, so the pipe never loses its place.
pub(crate) const MAX_LINE: usize = 16 * 1024 * 1024;
/// The longest request line a worker reads.
pub(crate) const MAX_REQUEST: usize = 1024 * 1024;

/// What the camera thread of a worker runs: a public command, or one only a
/// coordinator sends. One lives per request, so its size does not matter.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerCommand {
    /// Validate this exact binding on the camera thread before reusing it.
    CheckConnection,
    /// Absolute host-monotonic expiry, established by coordinator admission.
    WriteActionDeviceKeyUntil { key: u32, deadline_ns: u64 },
    /// Exact-owner source operation: expiry never restarts in the worker queue.
    SessionUntil {
        command: SessionCommand,
        deadline_ns: u64,
    },
    /// Local indexed video whose ownership/completion is observed independently.
    ForwardObservedUntil {
        command: SessionCommand,
        deadline_ns: u64,
    },
    /// Apply this camera's share of the `GigE` bandwidth a network scan planned.
    OptimizeNetwork { bytes_per_second: u64 },
    #[serde(untagged)]
    Session(SessionCommand),
}
impl<'de> Deserialize<'de> for WorkerCommand {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Untagged derive buffers decimals through serde::Content, losing the
        // arbitrary-precision number representation before a typed f64 sees it.
        // The strict JSON value preserves both numbers and duplicate-key checks.
        let value = super::strict_json_value(deserializer)?;
        if value["op"] == "check_connection" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Check {
                op: String,
            }
            let check: Check = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            debug_assert_eq!(check.op, "check_connection");
            Ok(Self::CheckConnection)
        } else if value["op"] == "write_action_device_key_until" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Key {
                op: String,
                #[serde(deserialize_with = "crate::config_number::unsigned")]
                key: u32,
                #[serde(deserialize_with = "crate::config_number::unsigned")]
                deadline_ns: u64,
            }
            let key: Key = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            debug_assert_eq!(key.op, "write_action_device_key_until");
            Ok(Self::WriteActionDeviceKeyUntil {
                key: key.key,
                deadline_ns: key.deadline_ns,
            })
        } else if value["op"] == "session_until" || value["op"] == "forward_observed_until" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Until {
                op: String,
                command: SessionCommand,
                #[serde(deserialize_with = "crate::config_number::unsigned")]
                deadline_ns: u64,
            }
            let until: Until = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            if until.op == "forward_observed_until" {
                Ok(Self::ForwardObservedUntil {
                    command: until.command,
                    deadline_ns: until.deadline_ns,
                })
            } else {
                Ok(Self::SessionUntil {
                    command: until.command,
                    deadline_ns: until.deadline_ns,
                })
            }
        } else if value["op"] == "optimize_network" {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Optimize {
                op: String,
                bytes_per_second: u64,
            }
            let command: Optimize =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            debug_assert_eq!(command.op, "optimize_network");
            Ok(Self::OptimizeNetwork {
                bytes_per_second: command.bytes_per_second,
            })
        } else {
            serde_json::from_value(value)
                .map(Self::Session)
                .map_err(serde::de::Error::custom)
        }
    }
}
impl From<SessionCommand> for WorkerCommand {
    fn from(command: SessionCommand) -> Self {
        Self::Session(command)
    }
}
impl WorkerCommand {
    /// The public command, if this is one.
    pub(crate) const fn session(&self) -> Option<&SessionCommand> {
        match self {
            Self::Session(command)
            | Self::SessionUntil { command, .. }
            | Self::ForwardObservedUntil { command, .. } => Some(command),
            Self::CheckConnection
            | Self::OptimizeNetwork { .. }
            | Self::WriteActionDeviceKeyUntil { .. } => None,
        }
    }
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        match self {
            Self::CheckConnection => Ok(()),
            Self::ForwardObservedUntil {
                command,
                deadline_ns,
            } => {
                super::forward::validate(command)?;
                ensure!(
                    matches!(command, SessionCommand::Forward { job: Some(job), .. } if *job > 0),
                    crate::types::Usage("observed Forward needs a coordinator-assigned job".into())
                );
                super::pinned::remaining(*deadline_ns).map(drop)
            }
            Self::SessionUntil {
                command,
                deadline_ns,
            } => {
                super::pinned::validate(command)?;
                super::pinned::remaining(*deadline_ns).map(drop)
            }
            Self::OptimizeNetwork { bytes_per_second } => {
                ensure!(
                    (1..=125_000_000_000).contains(bytes_per_second),
                    "network budget must be 1..125000000000 bytes per second"
                );
                Ok(())
            }
            Self::WriteActionDeviceKeyUntil { deadline_ns, .. } => {
                super::action_key::instant(*deadline_ns).map(drop)
            }
            Self::Session(SessionCommand::WriteActionDeviceKey { .. }) => Err(crate::types::Usage(
                "action key requires an admitted absolute expiry".into(),
            )
            .into()),
            Self::Session(command) => command.validate(),
        }
    }
    /// Whether it writes camera settings: a worker ended while it ran may
    /// leave the camera half-configured.
    pub(crate) const fn changes_settings(&self) -> bool {
        match self {
            Self::WriteActionDeviceKeyUntil { .. } => true,
            Self::Session(SessionCommand::ReadRegister { name, .. })
            | Self::SessionUntil {
                command: SessionCommand::ReadRegister { name, .. },
                ..
            } => !name.bank.is_empty(),
            Self::Session(command)
            | Self::SessionUntil { command, .. }
            | Self::ForwardObservedUntil { command, .. } => matches!(
                command,
                SessionCommand::SetValues { dry_run: false, .. }
                    | SessionCommand::ApplySetup { .. }
                    | SessionCommand::Exec { .. }
                    | SessionCommand::WriteMemory { .. }
                    | SessionCommand::WriteRegister { .. }
                    | SessionCommand::WriteActionDeviceKey { .. }
                    | SessionCommand::Auto { .. }
                    | SessionCommand::Manual
            ),
            Self::CheckConnection | Self::OptimizeNetwork { .. } => false,
        }
    }
    /// How long the camera thread may take on this command once it starts,
    /// before its worker counts as hung: the frame waits and work it may
    /// do. The coordinator never ends a worker sooner than 10 s.
    pub(crate) fn budget_ms(&self) -> u64 {
        match self {
            Self::SessionUntil { deadline_ns, .. }
            | Self::ForwardObservedUntil { deadline_ns, .. } => {
                super::pinned::remaining(*deadline_ns).map_or(1000, |left| {
                    u64::try_from(left.as_millis())
                        .unwrap_or(30_000)
                        .saturating_add(1000)
                })
            }
            Self::Session(command) => match command {
                SessionCommand::Capture { .. } | SessionCommand::Frame { .. } => {
                    crate::session::default_request_timeout(command)
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX)
                }
                SessionCommand::ReadRegister { timeout_ms, .. }
                | SessionCommand::WriteRegister { timeout_ms, .. } => {
                    timeout_ms.saturating_add(1000)
                }
                SessionCommand::ApplySetup { .. } => 240_000,
                SessionCommand::Connect { timeout_ms, .. } => timeout_ms.saturating_add(30_000),
                _ => 60_000,
            },
            Self::WriteActionDeviceKeyUntil { deadline_ns, .. } => {
                super::action_key::remaining(*deadline_ns).map_or(1000, |left| {
                    u64::try_from(left.as_millis())
                        .unwrap_or(30_000)
                        .saturating_add(1000)
                })
            }
            Self::CheckConnection => 1000,
            Self::OptimizeNetwork { .. } => 60_000,
        }
    }
}

/// One line a coordinator sends a worker.
#[cfg_attr(
    target_pointer_width = "64",
    expect(
        clippy::large_enum_variant,
        reason = "one protocol message is held per request; boxing would add allocation on the worker channel"
    )
)]
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
    /// Exclusive index creation by the exact assigned local video job.
    IndexOwned {
        job: u64,
        forwarding_id: String,
        index_path: String,
        output_path: String,
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
    /// This exact worker binding is terminal; pending owned files may still finish.
    Retired {
        error: Value,
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
    fn per_job_indices_are_exact_bounded_numeric_requests_and_require_a_format() {
        for index in ["0", "1.0", "3.1e1"] {
            let request = format!(
                r#"{{"op":"schedule","output":"selected.raw","count":1,"timeout_ms":1000,"format":"raw","part_format":"Mono8","part_index":{index},"first_at_ms":0,"interval_ms":0}}"#
            );
            let command: WorkerCommand = serde_json::from_str(&request).unwrap();
            command.validate().unwrap();
            let value = serde_json::to_value(command).unwrap();
            assert_eq!(value["part_format"], "Mono8");
            assert!(value["part_index"].as_u64().unwrap() < 32);
        }
        for (format, index) in [
            ("\"Mono8\"", "32"),
            ("null", "1"),
            ("\"Mono8\"", "-1"),
            ("\"Mono8\"", "0.5"),
            ("\"Mono8\"", "\"1\""),
        ] {
            let request = format!(
                r#"{{"op":"schedule","output":"selected.raw","count":1,"timeout_ms":1000,"format":"raw","part_format":{format},"part_index":{index},"first_at_ms":0,"interval_ms":0}}"#
            );
            if let Ok(command) = serde_json::from_str::<WorkerCommand>(&request) {
                command.validate().unwrap_err();
            }
        }
    }

    #[test]
    fn terminal_binding_protocol_is_versioned_and_strict() {
        assert_eq!(PROTOCOL, 10);
        let command: WorkerCommand = serde_json::from_str(r#"{"op":"check_connection"}"#).unwrap();
        assert!(matches!(command, WorkerCommand::CheckConnection));
        assert!(command.session().is_none() && !command.changes_settings());
        command.validate().unwrap();
        assert_eq!(
            serde_json::to_value(command).unwrap(),
            json!({"op":"check_connection"})
        );
        serde_json::from_str::<WorkerCommand>(r#"{"op":"check_connection","x":1}"#).unwrap_err();
        let wire = json!({"code":"disconnected","message":"expired", "details":{"controller_ownership":"retired"}});
        let line = Line {
            seq: 9,
            output: Output::Retired {
                error: wire.clone(),
            },
        };
        let decoded: Line = serde_json::from_str(&serde_json::to_string(&line).unwrap()).unwrap();
        assert!(matches!(decoded.output, Output::Retired { error } if error == wire));
        assert!(crate::engine::retirement::wire_is_retired(&wire));
        assert!(!crate::engine::retirement::wire_is_retired(
            &json!({"message":"controller ownership retired"})
        ));
        assert!(!crate::engine::retirement::wire_is_retired(
            &json!({"code":"frame_timeout","message":"expired", "details":{"controller_ownership":"retired"}})
        ));
    }

    #[test]
    fn requests_carry_public_or_internal_commands() {
        let run = |command: WorkerCommand| {
            let line = serde_json::to_string(&Input::Run { id: 3, command }).unwrap();
            serde_json::from_str::<Input>(&line).unwrap()
        };
        assert!(matches!(
            run(WorkerCommand::OptimizeNetwork {
                bytes_per_second: 1000
            }),
            Input::Run {
                id: 3,
                command: WorkerCommand::OptimizeNetwork {
                    bytes_per_second: 1000
                }
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
        SessionCommand::from_json_slice(br#"{"op":"optimize_network","bytes_per_second":1}"#)
            .unwrap_err();
        serde_json::from_str::<Input>(r#"{"op":"run","id":1}"#).unwrap_err();
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
        assert!(
            !WorkerCommand::OptimizeNetwork {
                bytes_per_second: 1000
            }
            .changes_settings()
        );
        let exec = SessionCommand::Exec {
            name: crate::controls::Name::parse("TriggerSoftware").unwrap(),
        };
        assert!(WorkerCommand::from(exec).changes_settings());
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
    #[test]
    fn worker_run_keeps_decimal_forward_fields_and_strict_command_keys() {
        for (lexical, expected) in [("1e1", 10.0), ("30.5000", 30.5), ("5.0", 5.0)] {
            let raw = format!(
                r#"{{"op":"run","id":1,"command":{{"op":"forward","output":"video.mkv","codec":"h264","encoder":"auto","fps":{lexical},"bitrate":"1M"}}}}"#
            );
            let input: Input = serde_json::from_str(&raw).unwrap();
            let Input::Run {
                command: WorkerCommand::Session(SessionCommand::Forward { fps, .. }),
                ..
            } = input
            else {
                panic!("expected recording request");
            };
            assert_eq!(fps, expected);
        }
        let input:Input=serde_json::from_str(r#"{"op":"run","id":1,"command":{"op":"auto","region":[0.1000,2e-1,0.500,5e-1],"max_exposure_us":1e3}}"#).unwrap();
        let Input::Run {
            command:
                WorkerCommand::Session(SessionCommand::Auto {
                    region,
                    max_exposure_us,
                    ..
                }),
            ..
        } = input
        else {
            panic!("expected auto request");
        };
        assert_eq!(region, Some([0.1, 0.2, 0.5, 0.5]));
        assert_eq!(max_exposure_us, Some(1000.0));
        let wire = serde_json::json!({"op":"run","id":7,"command":{
            "op":"forward","output":"video.mkv","codec":"h264","encoder":"auto",
            "fps":5.0,"bitrate":"1M","copy":"never","timeline":{
                "path":std::env::temp_dir().join("index.jsonl").display().to_string(),"acquisition_end_ns":123_456_789
            }
        }});
        let input: Input = serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        let Input::Run {
            command: WorkerCommand::Session(SessionCommand::Forward { fps, timeline, .. }),
            ..
        } = input
        else {
            panic!("expected recording request");
        };
        assert_eq!(fps, 5.0);
        assert_eq!(timeline.unwrap().acquisition_end_ns, 123_456_789);
        let command=SessionCommand::from_json_slice(br#"{"op":"forward","output":"video.mkv","codec":"h264","encoder":"auto","fps":30.5,"bitrate":"1M"}"#).unwrap();
        let input = Input::Run {
            id: 8,
            command: command.into(),
        };
        let input: Input = serde_json::from_slice(&serde_json::to_vec(&input).unwrap()).unwrap();
        let Input::Run {
            command: WorkerCommand::Session(SessionCommand::Forward { fps, .. }),
            ..
        } = input
        else {
            panic!("expected recording request");
        };
        assert_eq!(fps, 30.5);
        let error=serde_json::from_slice::<Input>(br#"{"op":"run","id":8,"command":{"op":"forward","output":"video.mkv","codec":"h264","encoder":"auto","fps":30.5,"fps":31.0,"bitrate":"1M"}}"#).unwrap_err();
        assert!(
            error.to_string().contains("duplicate field `fps`"),
            "{error}"
        );
        serde_json::from_slice::<Input>(br#"{"op":"run","id":9,"command":{"op":"optimize_network","bytes_per_second":1000,"extra":1}}"#).unwrap_err();
    }
    #[test]
    fn raw_register_budget_and_interruption_state_include_selector_writes() {
        let read = |name: &str| {
            WorkerCommand::from(SessionCommand::ReadRegister {
                name: crate::controls::Name::parse(name).unwrap(),
                timeout_ms: 25,
            })
        };
        assert!(!read("Buffer").changes_settings());
        assert!(read("Buffer[Bank=B]").changes_settings());
        assert_eq!(read("Buffer").budget_ms(), 1025);
        let write = WorkerCommand::from(SessionCommand::WriteRegister {
            name: crate::controls::Name::parse("Buffer").unwrap(),
            hex: "0080ff".into(),
            timeout_ms: 25,
        });
        assert!(write.changes_settings());
        write.validate().unwrap();
        for hex in ["", "0", "zz", "0x80"] {
            WorkerCommand::from(SessionCommand::WriteRegister {
                name: crate::controls::Name::parse("Buffer").unwrap(),
                hex: hex.into(),
                timeout_ms: 25,
            })
            .validate()
            .unwrap_err();
        }
        for value in [
            "{\"op\":\"read_register\",\"name\":{\"name\":\"Buffer\",\"bank\":[]},\"timeout_ms\":25.0}",
            "{\"op\":\"read_register\",\"name\":{\"name\":\"Buffer\",\"bank\":[]},\"timeout_ms\":2.5e1}",
        ] {
            let command = SessionCommand::from_json_slice(value.as_bytes()).unwrap();
            command.validate().unwrap();
            assert!(matches!(
                command,
                SessionCommand::ReadRegister { timeout_ms: 25, .. }
            ));
        }
    }
}
