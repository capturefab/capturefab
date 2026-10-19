//! Command-line front end for the optional assistant. Argument validation and
//! prompt reads happen once, before automatic session attachment.
use super::Cli;
use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::{fmt, path::PathBuf};

#[derive(Subcommand)]
pub enum AssistantCommand {
    /// List providers, credential readiness and image support without making a request
    Providers {
        /// Check saved keys in the OS keychain; this can show an OS prompt
        #[arg(long)]
        check_keychain: bool,
    },
    /// Select a provider and save its non-secret settings
    Use {
        /// Provider ID from assistant providers
        provider: String,
        /// Non-secret provider settings to change
        #[command(flatten)]
        settings: ProviderArgs,
    },
    /// Save or remove a provider key in the OS credential store
    Key {
        /// Credential operation
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Send a small tool round trip to the selected provider
    Test {
        /// Provider ID; defaults to the selected provider
        provider: Option<String>,
        /// Also send a generated 64 by 64 image and ask the model to name its colour
        #[arg(long)]
        vision: bool,
    },
    /// Ask for an editable camera-tool plan; changes need --apply to run
    Ask {
        /// Request text or a UTF-8 request file
        #[command(flatten)]
        request: PromptArgs,
        /// Override the selected provider for this request
        #[arg(long)]
        provider: Option<String>,
        /// Override this provider's model for this request
        #[arg(long)]
        model: Option<String>,
        /// Run the whole ready plan only if every tool is allowed and all output stays local
        #[arg(long)]
        apply: bool,
        /// Comma-separated tools allowed by --apply; never permits remote output
        #[arg(long, requires = "apply", value_delimiter = ',')]
        allow_tools: Option<Vec<String>>,
        /// Export the version 1 plan to a new private file; never overwrite
        #[arg(long)]
        plan_out: Option<PathBuf>,
    },
    /// Work within a camera scope, approval policy and hard limits in a running session
    Run {
        /// Task text or a UTF-8 task file
        #[command(flatten)]
        request: PromptArgs,
        /// Camera ID in this task; repeat for up to eight cameras
        #[arg(long, required = true, value_delimiter = ',')]
        camera: Vec<String>,
        /// Saved destination name in this task; repeat to allow more than one
        #[arg(long, value_delimiter = ',')]
        destination: Vec<String>,
        /// Allow inspected camera images to be sent to the provider
        #[arg(long)]
        see_images: bool,
        /// Ask before each change, or run routine changes without asking
        #[arg(long, default_value = "supervised", value_parser = ["supervised", "autonomous"])]
        mode: String,
        /// Ask on the terminal or deny approval requests; defaults to ask with a TTY, deny otherwise
        #[arg(long, value_parser = ["ask", "deny"])]
        approvals: Option<String>,
        /// Hard task limits; omitted values use saved defaults
        #[command(flatten)]
        limits: LimitArgs,
        /// Pause, inform the model, or stop after a person changes a scoped camera
        #[arg(long, default_value = "pause", value_parser = ["pause", "inform", "stop"])]
        on_human_change: String,
        /// Stream the action log JSONL to a new private file, or - for stderr
        #[arg(long)]
        events: Option<PathBuf>,
    },
    /// List local assistant action logs, including unfinished tasks
    Runs,
    /// Read one local assistant action log
    Log {
        /// Run ID from assistant runs
        run_id: String,
    },
}

#[derive(Subcommand)]
pub enum KeyCommand {
    /// Read one key from stdin and save it in the OS credential store
    Set {
        /// Provider ID from assistant providers
        provider: String,
        /// Read the key from stdin, preserving all bytes except one final newline
        #[arg(long, required = true)]
        key_stdin: bool,
    },
    /// Remove the saved key; environment credentials are unchanged
    Remove {
        /// Provider ID from assistant providers
        provider: String,
    },
}

#[derive(Args)]
pub struct ProviderArgs {
    /// Model ID; uses the provider default when no model is saved
    #[arg(long)]
    model: Option<String>,
    /// HTTPS API origin, or a local HTTP origin for Ollama or Custom
    #[arg(long)]
    base_url: Option<String>,
    /// Cloudflare account ID
    #[arg(long)]
    account_id: Option<String>,
    /// Cloudflare AI Gateway ID
    #[arg(long)]
    gateway_id: Option<String>,
    /// AWS region for Bedrock
    #[arg(long)]
    region: Option<String>,
    /// AWS shared-config profile name
    #[arg(long)]
    profile: Option<String>,
    /// Google Cloud project ID for Vertex AI
    #[arg(long)]
    project: Option<String>,
    /// Google Cloud location for Vertex AI
    #[arg(long)]
    location: Option<String>,
    /// Azure AI Foundry endpoint
    #[arg(long)]
    endpoint: Option<String>,
    /// Reasoning effort sent only when this provider has a documented control
    #[arg(long, value_parser = ["low", "medium", "high"])]
    effort: Option<String>,
    /// Resolve image support automatically, force it on, or disable it
    #[arg(long, value_parser = ["auto", "on", "off"])]
    vision: Option<String>,
}

#[derive(Args)]
pub struct PromptArgs {
    /// Request text; omit when using --prompt-file
    #[arg(num_args = 0.., conflicts_with = "prompt_file")]
    prompt: Vec<String>,
    /// Read UTF-8 request text from this file, or - for stdin
    #[arg(long, conflicts_with = "prompt")]
    prompt_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct LimitArgs {
    /// Maximum model rounds, from 1 to 1,000
    #[arg(long)]
    max_rounds: Option<u32>,
    /// Maximum task wall time, from 1m to 24h; waiting counts
    #[arg(long)]
    max_time: Option<String>,
    /// Estimated US-dollar ceiling, from 0 to 500; requires saved model prices
    #[arg(long)]
    max_cost: Option<f64>,
    /// Maximum input tokens, including replayed history, from 1 to 50,000,000
    #[arg(long)]
    max_input_tokens: Option<u64>,
    /// Maximum output tokens, including reasoning, from 1 to 5,000,000
    #[arg(long)]
    max_output_tokens: Option<u64>,
    /// Maximum newly sent observation images, from 0 to 2,000
    #[arg(long)]
    max_images: Option<u32>,
    /// Maximum still captures, from 0 to 100,000; zero also disables schedules
    #[arg(long)]
    max_captures: Option<u32>,
    /// Maximum saved capture and recording bytes, such as 2GiB, within the storage budget
    #[arg(long)]
    max_size: Option<String>,
    /// Maximum file recordings, from 0 to 32; streams require separate exact approval
    #[arg(long)]
    max_recordings: Option<u32>,
    /// Maximum duration of one recording or stream, from 1s to 7d
    #[arg(long)]
    max_recording: Option<String>,
    /// Approval deadline, from 1s to 24h; the task wall limit still applies
    #[arg(long)]
    approval_timeout: Option<String>,
}

// Raw prompts, credential-shaped URLs and keys never appear in argument Debug.
impl fmt::Debug for AssistantCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Providers { .. } => "Assistant::Providers",
            Self::Use { .. } => "Assistant::Use",
            Self::Key { .. } => "Assistant::Key",
            Self::Test { .. } => "Assistant::Test",
            Self::Ask { .. } => "Assistant::Ask",
            Self::Run { .. } => "Assistant::Run",
            Self::Runs => "Assistant::Runs",
            Self::Log { .. } => "Assistant::Log",
        })
    }
}

pub(super) fn uses_session(command: &AssistantCommand) -> bool {
    matches!(
        command,
        AssistantCommand::Ask { .. } | AssistantCommand::Run { .. }
    )
}

pub struct AssistantTaskFailure {
    message: String,
    result: Value,
    exit: i32,
    source: Option<anyhow::Error>,
}
impl AssistantTaskFailure {
    pub fn details(&self) -> Value {
        crate::tools::redact::for_output(self.result.clone())
    }
    pub fn exit(&self) -> i32 {
        self.exit
    }
    /// Keep a partial plan or task report visible when the normal error path
    /// writes its final message to stderr.
    pub fn print_human(&self) -> Result<()> {
        use std::io::{self, Write};
        let mut out = io::stderr().lock();
        if let Some(run) = self.result["run_id"].as_str() {
            writeln!(out, "Task: {run}")?;
        }
        if let Some(log) = self.result["log"].as_str() {
            writeln!(out, "Action log: {log}")?;
        }
        for (index, step) in self.result["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            writeln!(
                out,
                "{}. {}",
                index + 1,
                step["summary"].as_str().unwrap_or("Review this step.")
            )?;
        }
        for leftover in self.result["leftovers"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "Still active: {}",
                leftover["summary"].as_str().unwrap_or("camera work")
            )?;
        }
        Ok(())
    }
}
impl fmt::Debug for AssistantTaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantTaskFailure")
            .field("message", &self.message)
            .field("result", &self.details())
            .finish()
    }
}
impl fmt::Display for AssistantTaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for AssistantTaskFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source.as_ref())
    }
}

pub(super) struct PreparedAssistant {
    #[cfg(feature = "ai")]
    settings: crate::assistant::Settings,
    #[cfg(feature = "ai")]
    provider: Option<crate::assistant::ProviderId>,
    #[cfg(feature = "ai")]
    prompt: Option<String>,
    #[cfg(feature = "ai")]
    limits: Option<crate::assistant::Limits>,
    #[cfg(feature = "ai")]
    key: Option<zeroize::Zeroizing<String>>,
}

pub(super) fn prepare(cli: &Cli, command: &AssistantCommand) -> Result<PreparedAssistant> {
    #[cfg(feature = "ai")]
    {
        enabled::prepare(cli, command)
    }
    #[cfg(not(feature = "ai"))]
    {
        let _ = (cli, command);
        Err(anyhow!(
            "the assistant is unsupported in this build; rebuild with --features ai"
        ))
    }
}

pub(super) fn run(
    cli: &Cli,
    command: &AssistantCommand,
    prepared: PreparedAssistant,
) -> Result<Value> {
    #[cfg(feature = "ai")]
    {
        enabled::run(cli, command, prepared)
    }
    #[cfg(not(feature = "ai"))]
    {
        let _ = (cli, command, prepared);
        Err(anyhow!(
            "the assistant is unsupported in this build; rebuild with --features ai"
        ))
    }
}

pub(super) fn doctor() -> Value {
    #[cfg(feature = "ai")]
    {
        crate::assistant::doctor()
    }
    #[cfg(not(feature = "ai"))]
    {
        json!({"supported":false,"rebuild":"--features ai"})
    }
}

pub(super) fn schema_contract() -> Value {
    #[cfg(feature = "ai")]
    {
        enabled::schema_contract()
    }
    #[cfg(not(feature = "ai"))]
    {
        json!({"supported":false,"rebuild":"--features ai","exit_codes":{"unsupported":5}})
    }
}

#[cfg(feature = "ai")]
pub fn test_connection(
    deps: &crate::assistant::agent::Deps,
    provider: crate::assistant::ProviderId,
    settings: crate::assistant::ProviderSettings,
    vision: bool,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Value> {
    enabled::test_connection(deps, provider, settings, vision, cancel)
}

pub(super) fn print_human(command: &AssistantCommand, value: &Value) -> Result<()> {
    use std::io::{self, Write};
    let mut out = io::stdout().lock();
    match command {
        AssistantCommand::Providers { .. } => {
            for entry in value.as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "{}{}  {}  {}",
                    if entry["selected"] == true {
                        "* "
                    } else {
                        "  "
                    },
                    entry["id"].as_str().unwrap_or(""),
                    entry["model"].as_str().unwrap_or(""),
                    entry["credential"]["source"]
                        .as_str()
                        .unwrap_or("Not configured")
                )?;
            }
        }
        AssistantCommand::Ask { .. } => {
            if let Some(text) = value["text"].as_str().filter(|text| !text.is_empty()) {
                writeln!(out, "{text}")?;
            }
            for (index, step) in value["steps"].as_array().into_iter().flatten().enumerate() {
                writeln!(
                    out,
                    "{}. {} [{}]",
                    index + 1,
                    step["summary"].as_str().unwrap_or(""),
                    step["risk"].as_str().unwrap_or("")
                )?;
                for issue in step["issues"].as_array().into_iter().flatten() {
                    writeln!(out, "   {}", issue["message"].as_str().unwrap_or(""))?;
                }
            }
            if value.get("applied").is_some() {
                writeln!(out, "Applied the plan.")?;
            }
        }
        AssistantCommand::Run { .. } => {
            writeln!(
                out,
                "{}: {}",
                value["outcome"].as_str().unwrap_or(""),
                value["summary"].as_str().unwrap_or("")
            )?;
            writeln!(out, "Action log: {}", value["log"].as_str().unwrap_or(""))?;
            for effect in value["leftovers"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "Still active: {}",
                    effect["summary"].as_str().unwrap_or("camera work")
                )?;
            }
        }
        AssistantCommand::Runs => {
            for run in value.as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "{}  {}  {}",
                    run["run_id"].as_str().unwrap_or(""),
                    run["outcome"].as_str().unwrap_or(if run["active"] == true {
                        "running"
                    } else {
                        "unfinished"
                    }),
                    run["goal"].as_str().unwrap_or("")
                )?;
            }
        }
        AssistantCommand::Log { .. } => {
            for record in value.as_array().into_iter().flatten() {
                writeln!(out, "{}", serde_json::to_string(record)?)?;
            }
        }
        _ => {
            writeln!(
                out,
                "{}",
                value["message"]
                    .as_str()
                    .or_else(|| value["text"].as_str())
                    .unwrap_or("Done.")
            )?;
        }
    }
    Ok(())
}

#[cfg(feature = "ai")]
mod enabled {
    use super::*;
    use crate::{
        assistant::{
            self as ai,
            agent::{self, Deps, ModelSession, PlanRequest},
            approval::{self, ApprovalDecision, ApprovalRequest, ApprovalScope, Approver},
            conversation::{Conversation, ImagePart, ResultRecord, Stop, ToolDef},
            limits::Limits,
            protocol::Mode,
            provider::{ProviderId, ProviderSettings},
            run::{
                self as runtime, OnHumanChange, Outcome, RunControl, RunMode, RunRequest, RunScope,
            },
            settings,
            vision::{self, VisionMode},
        },
        session::{ActivityOrigin, ActorKind, SessionActor},
        tools::{
            self, ApplyError, CheckFailure, Issue,
            context::{IpcHost, NoSession, RecordingDefaults, ToolHost},
        },
    };
    use anyhow::{Context as _, ensure};
    use std::{
        fs::{self, File, OpenOptions},
        io::{self, IsTerminal, Read, Write},
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    const DEFAULT_ALLOW: &[&str] = &[
        "connect_camera",
        "start_recording",
        "stop_recording",
        "capture_photos",
        "schedule_captures",
        "cancel_job",
    ];

    fn field(field: &str, message: impl Into<String>) -> anyhow::Error {
        CheckFailure {
            issues_by_step: vec![(
                1,
                vec![Issue::usage(
                    Some(field),
                    tools::redact::for_output_text(&message.into()),
                )],
            )],
            checked: json!([]),
        }
        .into()
    }
    fn provider(value: &str) -> Result<ProviderId> {
        value
            .parse()
            .map_err(|_| field("provider", "Choose a provider ID from assistant providers."))
    }
    fn selected(store: &ai::Settings, value: Option<&str>) -> Result<ProviderId> {
        value
            .map(provider)
            .transpose()?
            .or(store.provider)
            .ok_or_else(|| {
                field(
                    "provider",
                    "Choose a provider with capturefab assistant use PROVIDER, or pass --provider.",
                )
            })
    }
    fn settings_valid(id: ProviderId, value: &ProviderSettings) -> Result<()> {
        value.validate(id).map_err(|issues| {
            CheckFailure {
                issues_by_step: vec![(
                    1,
                    issues
                        .into_iter()
                        .map(|issue| Issue::usage(Some(&issue.field), issue.message))
                        .collect(),
                )],
                checked: json!([]),
            }
            .into()
        })
    }
    fn read_prompt(args: &PromptArgs) -> Result<String> {
        if !args.prompt.is_empty() && args.prompt_file.is_some() {
            return Err(field(
                "prompt_file",
                "Use request text or --prompt-file, not both.",
            ));
        }
        let text = if let Some(path) = &args.prompt_file {
            let mut data = Vec::new();
            let input: Box<dyn Read> = if path == Path::new("-") {
                Box::new(io::stdin())
            } else {
                Box::new(
                    File::open(path)
                        .map_err(|_| field("prompt_file", "Open a readable UTF-8 request file."))?,
                )
            };
            input
                .take(16_001)
                .read_to_end(&mut data)
                .map_err(|_| field("prompt_file", "Read a UTF-8 request file."))?;
            if data.len() > 16_000 {
                return Err(field("prompt", "Keep the request within 4,000 characters."));
            }
            String::from_utf8(data).map_err(|_| field("prompt_file", "Use UTF-8 request text."))?
        } else {
            args.prompt.join(" ")
        };
        ai::prompt::validate_prompt(&text).map_err(|error| field("prompt", error.to_string()))?;
        Ok(text)
    }
    fn with_flags(
        id: ProviderId,
        mut value: ProviderSettings,
        args: &ProviderArgs,
    ) -> Result<ProviderSettings> {
        for (name, flag, target) in [
            ("model", &args.model, &mut value.model),
            ("base_url", &args.base_url, &mut value.base_url),
            ("account_id", &args.account_id, &mut value.account_id),
            ("gateway_id", &args.gateway_id, &mut value.gateway_id),
            ("region", &args.region, &mut value.region),
            ("profile", &args.profile, &mut value.profile),
            ("project", &args.project, &mut value.project),
            ("location", &args.location, &mut value.location),
            ("endpoint", &args.endpoint, &mut value.endpoint),
        ] {
            if let Some(flag) = flag {
                if name != "model" && !id.info().settings.contains(&name) {
                    return Err(field(name, "This provider does not use this setting."));
                }
                *target = Some(flag.clone());
            }
        }
        if let Some(effort) = &args.effort {
            value.effort = Some(match effort.as_str() {
                "low" => ai::Effort::Low,
                "medium" => ai::Effort::Medium,
                "high" => ai::Effort::High,
                _ => return Err(field("effort", "Choose low, medium or high effort.")),
            });
        }
        if let Some(vision) = &args.vision {
            value.vision = Some(match vision.as_str() {
                "auto" => VisionMode::Auto,
                "on" => VisionMode::On,
                "off" => VisionMode::Off,
                _ => return Err(field("vision", "Choose auto, on or off.")),
            });
        }
        settings_valid(id, &value)?;
        Ok(value)
    }
    fn duration(field_name: &str, value: &str) -> Result<u64> {
        crate::scheduling::parse_duration(value)
            .map_err(|_| field(field_name, "Use a duration such as 30s, 15m or 2h."))
    }
    fn limits(base: &Limits, args: &LimitArgs) -> Result<Limits> {
        let mut value = base.clone();
        if let Some(v) = args.max_rounds {
            value.max_rounds = v;
        }
        if let Some(v) = &args.max_time {
            value.max_wall_ms = duration("max_time", v)?;
        }
        if let Some(v) = args.max_cost {
            value.max_cost_usd = Some(v);
        }
        if let Some(v) = args.max_input_tokens {
            value.max_input_tokens = v;
        }
        if let Some(v) = args.max_output_tokens {
            value.max_output_tokens = v;
        }
        if let Some(v) = args.max_images {
            value.max_images = v;
        }
        if let Some(v) = args.max_captures {
            value.max_captures = v;
        }
        if let Some(v) = &args.max_size {
            value.max_bytes = crate::storage::parse_bytes(v)
                .map_err(|error| field("max_size", error.to_string()))?;
        }
        if let Some(v) = args.max_recordings {
            value.max_recordings = v;
        }
        if let Some(v) = &args.max_recording {
            value.max_recording_ms = duration("max_recording", v)?;
        }
        if let Some(v) = &args.approval_timeout {
            value.approval_timeout_ms = duration("approval_timeout", v)?;
        }
        if let Err(messages) = value.validate() {
            let names = [
                "max_rounds",
                "max_time",
                "max_input_tokens",
                "max_output_tokens",
                "max_cost",
                "max_images",
                "max_captures",
                "max_size",
                "max_recordings",
                "max_recording",
                "max_setting_changes",
                "max_consecutive_errors",
                "max_identical_calls",
                "approval_timeout",
                "min_round_interval",
            ];
            // Each invalid value is attributed to its own CLI field, without
            // relying on error wording for exit classification.
            let checks = [
                (1..=1000).contains(&value.max_rounds),
                (60_000..=86_400_000).contains(&value.max_wall_ms),
                (1..=50_000_000).contains(&value.max_input_tokens),
                (1..=5_000_000).contains(&value.max_output_tokens),
                value
                    .max_cost_usd
                    .is_none_or(|v| v.is_finite() && (0.0..=500.0).contains(&v)),
                value.max_images <= 2000,
                value.max_captures <= 100_000,
                value.max_bytes > 0,
                value.max_recordings <= 32,
                (1000..=604_800_000).contains(&value.max_recording_ms),
                value.max_setting_changes <= 10_000,
                (1..=5).contains(&value.max_consecutive_errors),
                (1..=3).contains(&value.max_identical_calls),
                (1000..=86_400_000).contains(&value.approval_timeout_ms),
                (1000..=86_400_000).contains(&value.min_round_interval_ms),
            ];
            let issues = names
                .into_iter()
                .zip(checks)
                .filter(|(_, valid)| !*valid)
                .zip(messages)
                .map(|((name, _), message)| Issue::usage(Some(name), message))
                .collect();
            return Err(CheckFailure {
                issues_by_step: vec![(1, issues)],
                checked: json!([]),
            }
            .into());
        }
        Ok(value)
    }

    pub(super) fn prepare(cli: &Cli, command: &AssistantCommand) -> Result<PreparedAssistant> {
        if cli.camera.is_some() && !matches!(command, AssistantCommand::Run { .. }) {
            return Err(field(
                "camera",
                "Use camera IDs in the request, or assistant run --camera ID; omit the global --camera.",
            ));
        }
        cli.session_actor()
            .and_then(|actor| actor.validate())
            .map_err(|error| field("actor", error.to_string()))?;
        crate::media::validate_hwaccel(&cli.hwaccel)
            .map_err(|error| field("hwaccel", error.to_string()))?;
        crate::session::validate_timeout(cli.timeout_ms)
            .map_err(|error| field("timeout_ms", error.to_string()))?;
        if let Some(session) = &cli.session {
            crate::ipc::validate_name(session)
                .map_err(|error| field("session", error.to_string()))?;
        }
        if let Some(timeout) = &cli.request_timeout {
            duration("request_timeout", timeout)?;
        }
        let loaded = settings::load();
        for warning in &loaded.warnings {
            eprintln!("capturefab: {}", tools::redact::for_output_text(warning));
        }
        let mut result = PreparedAssistant {
            settings: loaded.settings,
            provider: None,
            prompt: None,
            limits: None,
            key: None,
        };
        match command {
            AssistantCommand::Use {
                provider: id,
                settings: flags,
            } => {
                let id = provider(id)?;
                let value = with_flags(id, result.settings.provider_settings(id), flags)?;
                result.settings.set_provider_settings(id, value)?;
                result.provider = Some(id);
            }
            AssistantCommand::Key { command } => {
                let id = match command {
                    KeyCommand::Set { provider, .. } | KeyCommand::Remove { provider } => provider,
                };
                result.provider = Some(provider(id)?);
                if let KeyCommand::Set { key_stdin, .. } = command {
                    if !key_stdin {
                        return Err(field("key_stdin", "Read the key with --key-stdin."));
                    }
                    result.key = Some(
                        crate::secret_input::read_stdin_secret(io::stdin().lock(), "provider key")
                            .map_err(|error| field("key_stdin", error.to_string()))?,
                    );
                }
            }
            AssistantCommand::Test { provider: id, .. } => {
                result.provider = Some(selected(&result.settings, id.as_deref())?);
            }
            AssistantCommand::Ask {
                request,
                provider: id,
                model,
                allow_tools,
                plan_out,
                ..
            } => {
                result.prompt = Some(read_prompt(request)?);
                let id = selected(&result.settings, id.as_deref())?;
                if let Some(model) = model {
                    let mut value = result.settings.provider_settings(id);
                    value.model = Some(model.clone());
                    settings_valid(id, &value)?;
                }
                if let Some(allow) = allow_tools
                    && (allow.is_empty()
                        || allow.iter().any(|name| {
                            tools::spec(name).is_none_or(|spec| spec.kind == tools::ToolKind::Read)
                        }))
                {
                    return Err(field(
                        "allow_tools",
                        "List action-tool names from capturefab tools list.",
                    ));
                }
                if let Some(path) = plan_out {
                    new_output(path, "plan_out")?;
                }
                result.provider = Some(id);
            }
            AssistantCommand::Run {
                request,
                camera,
                destination,
                approvals,
                limits: args,
                events,
                ..
            } => {
                result.prompt = Some(read_prompt(request)?);
                let scope = RunScope {
                    cameras: camera.clone(),
                    destinations: destination.clone(),
                    allow_images: false,
                    allow_recordings: false,
                    allow_schedules: false,
                    effects_may_outlive_run: false,
                };
                scope
                    .validate()
                    .map_err(|error| field("camera", error.to_string()))?;
                if approvals.as_deref() == Some("ask")
                    && (!io::stdin().is_terminal() || !io::stderr().is_terminal())
                {
                    return Err(field(
                        "approvals",
                        "Use --approvals deny without an interactive terminal.",
                    ));
                }
                let id = selected(&result.settings, None)?;
                let limits = limits(&result.settings.autonomy.limits, args)?;
                if limits.max_cost_usd.is_some()
                    && !result.settings.prices.contains_key(&format!(
                        "{}/{}",
                        id.id(),
                        result.settings.provider_settings(id).model(id)
                    ))
                {
                    return Err(field(
                        "max_cost",
                        "Set prices for this model before choosing an estimated cost limit.",
                    ));
                }
                if let Some(path) = events.as_ref().filter(|path| *path != Path::new("-")) {
                    new_output(path, "events")?;
                }
                if cli.direct {
                    return Err(field(
                        "direct",
                        "An assistant task needs a running session; omit --direct.",
                    ));
                }
                result.provider = Some(id);
                result.limits = Some(limits);
            }
            AssistantCommand::Log { run_id } => {
                if !ai::log::valid_run_id(run_id) {
                    return Err(field("run_id", "Choose a run ID from assistant runs."));
                }
            }
            AssistantCommand::Providers { .. } | AssistantCommand::Runs => {}
        }
        Ok(result)
    }
    fn new_output(path: &Path, name: &str) -> Result<()> {
        if path.as_os_str().is_empty() || path == Path::new("-") {
            return Err(field(name, "Choose a new local file path."));
        }
        match fs::symlink_metadata(path) {
            Ok(_) => Err(field(
                name,
                "Choose a new file; existing files are never overwritten.",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                if !fs::metadata(parent)
                    .is_ok_and(|meta| meta.is_dir() && !meta.permissions().readonly())
                {
                    return Err(field(
                        name,
                        "Choose a new file in an existing writable directory.",
                    ));
                }
                Ok(())
            }
            Err(_) => Err(field(name, "Choose a writable local file path.")),
        }
    }

    fn host(cli: &Cli) -> Result<Arc<dyn ToolHost>> {
        let timeout = cli
            .request_timeout
            .as_deref()
            .map(crate::scheduling::parse_duration)
            .transpose()?
            .map(Duration::from_millis);
        Ok(match &cli.session {
            Some(name) => Arc::new(IpcHost::new(name, Some(cli.session_actor()?), timeout)),
            None => Arc::new(NoSession),
        })
    }
    fn deps(cli: &Cli, settings: ai::Settings) -> Result<Deps> {
        Ok(Deps::native(
            host(cli)?,
            settings,
            std::env::current_dir()?,
            crate::storage::configured_policy()?,
            RecordingDefaults::default(),
        ))
    }
    fn model_settings(
        prepared: &PreparedAssistant,
        id: ProviderId,
        model: Option<&str>,
    ) -> Result<ProviderSettings> {
        let mut value = prepared.settings.provider_settings(id);
        if let Some(model) = model {
            value.model = Some(model.to_owned());
        }
        settings_valid(id, &value)?;
        Ok(value)
    }
    fn endpoint_host(id: ProviderId, value: &ProviderSettings) -> Option<(String, Option<u16>)> {
        let url = value
            .endpoint
            .as_deref()
            .or(value.base_url.as_deref())
            .unwrap_or(id.info().base_url);
        ai::provider::parse_endpoint(url)
            .ok()
            .map(|address| (address.host, address.port))
    }
    fn providers(store: &ai::Settings, check: bool) -> Value {
        Value::Array(ProviderId::ALL.iter().map(|id| {
            let info=id.info();let settings=store.provider_settings(*id);
            let vision=vision::resolve(*id,settings.model(*id),settings.vision.unwrap_or_default(),None,false);
            json!({"id":id.id(),"name":info.name,"selected":store.provider==Some(*id),"model":settings.model(*id),"default_model":info.default_model,"env_vars":info.key_envs,"settings":info.settings,"suggested_models":info.suggested_models,"suggested_vision_model":info.suggested_vision_model,"credential":ai::credentials::status(*id,&settings,check),"vision":vision})
        }).collect())
    }
    fn apply_failure(error: ApplyError, mut plan: Value) -> anyhow::Error {
        let (details, source): (_, anyhow::Error) = match error {
            ApplyError::Check(error) => (error.details(), error.into()),
            ApplyError::Run(error) => (error.details(), error.into()),
        };
        plan["applied"] = details;
        let exit = crate::cli::error_code(&source).1;
        AssistantTaskFailure {
            message: tools::redact::for_output_text(&source.to_string()),
            result: plan,
            exit,
            source: Some(source),
        }
        .into()
    }
    fn plan_value(outcome: &agent::PlanOutcome) -> Value {
        plan_parts(&outcome.text, &outcome.plan)
    }
    fn plan_parts(text: &str, plan: &agent::Plan) -> Value {
        tools::redact::for_output(
            json!({"text":text,"plan":plan.document(),"steps":plan.steps.iter().map(|step|{
            if let Some(checked)=&step.checked {json!({"step":step.id,"tool":checked.call.tool,"title":checked.title,"summary":checked.summary,"issues":checked.issues,"risk":checked.risk,"leaves_computer":checked.leaves_computer})}
            else {json!({"step":step.id,"tool":step.call.tool,"title":"Review this step","summary":"This step needs a fresh check.","issues":[Issue::usage(None,"Check this step before running it.")],"risk":"human_only","leaves_computer":false})}
        }).collect::<Vec<_>>() }),
        )
    }
    fn ask(cli: &Cli, command: &AssistantCommand, prepared: PreparedAssistant) -> Result<Value> {
        let AssistantCommand::Ask {
            model,
            apply,
            allow_tools,
            plan_out,
            ..
        } = command
        else {
            unreachable!()
        };
        if *apply && cli.session.is_none() {
            return Err(field(
                "apply",
                "Applying a plan needs a running session; start one with capturefab serve --name NAME.",
            ));
        }
        let id = prepared
            .provider
            .expect("provider checked before session access");
        let mut request = PlanRequest::new(prepared.prompt.as_deref().expect("prompt read once"));
        request.provider = Some(id);
        request.model = model.clone();
        let deps = deps(cli, prepared.settings)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
            .context("Install the assistant stop handler.")?;
        let mut last_plan = request.plan.clone();
        let mut text = String::new();
        let outcome = agent::plan_turn(
            request,
            &deps,
            &mut |event| match event {
                agent::Event::Plan(plan) => last_plan = plan,
                agent::Event::Text(part) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&part);
                }
                agent::Event::Notice(message) if !cli.json => {
                    eprintln!("capturefab: {message}");
                }
                _ => (),
            },
            &cancel,
        )
        .map_err(|source| AssistantTaskFailure {
            message: tools::redact::for_output_text(&source.to_string()),
            result: plan_parts(&text, &last_plan),
            exit: crate::cli::error_code(&source).1,
            source: Some(source),
        })?;
        let mut value = plan_value(&outcome);
        if outcome.cancelled || outcome.error.is_some() {
            return Err(AssistantTaskFailure {
                message: if outcome.cancelled {
                    "assistant task stopped".into()
                } else {
                    tools::redact::for_output_text(
                        outcome
                            .error
                            .as_deref()
                            .unwrap_or("The assistant could not finish this request."),
                    )
                },
                result: value,
                exit: 1,
                source: None,
            }
            .into());
        }
        if let Some(path) = plan_out
            && let Err(error) = tools::plan::write(path, &outcome.plan.document())
        {
            value["issues"] = json!([Issue::usage(
                Some("plan_out"),
                tools::redact::for_output_text(&error.to_string())
            )]);
            return Err(AssistantTaskFailure{message:"The assistant plan could not be exported. Choose a new writable file and a plan with at least one step.".into(),result:value,exit:2,source:None}.into());
        }
        if *apply {
            let allowed: Vec<&str> = allow_tools
                .as_ref()
                .map(|values| values.iter().map(String::as_str).collect())
                .unwrap_or_else(|| DEFAULT_ALLOW.to_vec());
            let mut issues = Vec::new();
            let mut checked = Vec::new();
            for (index, step) in outcome.plan.steps.iter().enumerate() {
                let mut problems = Vec::new();
                match &step.checked {
                    Some(item) => {
                        problems
                            .extend(item.issues.iter().filter(|issue| issue.is_error()).cloned());
                        if !allowed.contains(&item.call.tool.as_str()) {
                            problems.push(Issue::usage(Some("allow_tools"),"This plan contains a tool outside --allow-tools; review the plan without --apply."));
                        }
                        if item.leaves_computer
                            || item
                                .destination
                                .as_ref()
                                .is_some_and(|destination| destination.remote)
                        {
                            problems.push(Issue::usage(Some("apply"),"Remote output cannot be applied by assistant ask; review and run it with capturefab tools apply."));
                        }
                        checked.push(item.clone());
                    }
                    None => problems.push(Issue::usage(
                        None,
                        "Check every plan step before applying it.",
                    )),
                }
                if !problems.is_empty() {
                    issues.push((index + 1, problems));
                }
            }
            if !issues.is_empty() {
                value["issues"]=json!(issues.iter().flat_map(|(step,items)|items.iter().map(move|issue|json!({"step":step,"field":issue.field,"message":issue.message,"severity":issue.severity,"code":issue.code}))).collect::<Vec<_>>());
                return Err(AssistantTaskFailure {
                    message: "The assistant plan cannot be applied. Review its steps.".into(),
                    result: value,
                    exit: 2,
                    source: None,
                }
                .into());
            }
            if !outcome.plan.steps.is_empty() {
                let applied = tools::apply(
                    deps.host.as_ref(),
                    deps.secrets.as_ref(),
                    || deps.gather(deps.host.as_ref()),
                    &outcome.plan.calls(),
                    Some(&checked),
                    |progress| {
                        if !cli.json {
                            eprintln!(
                                "capturefab: step {} of {}: {}",
                                progress.step, progress.step_count, progress.summary
                            );
                        }
                    },
                    &cancel,
                )
                .map_err(|error| apply_failure(error, value.clone()))?;
                value["applied"] = applied;
            } else {
                value["applied"] = json!({"completed":[]});
            }
        }
        Ok(value)
    }

    struct TerminalApprover {
        by: SessionActor,
        input: std::sync::Mutex<Option<mpsc::Receiver<String>>>,
    }
    fn terminal_decision(
        line: &str,
        req: &ApprovalRequest,
        by: &SessionActor,
    ) -> Option<ApprovalDecision> {
        let mut words = line.split_whitespace();
        let command = words.next()?;
        let id = words.next()?.parse::<u64>().ok()?;
        if id != req.id || words.next().is_some() {
            return None;
        }
        match command.to_ascii_lowercase().as_str() {
            "yes" => Some(ApprovalDecision::Approved {
                scope: ApprovalScope::Once,
                by: by.clone(),
            }),
            "run" if req.allow_run_scope => Some(ApprovalDecision::Approved {
                scope: ApprovalScope::ForRun,
                by: by.clone(),
            }),
            "no" => Some(ApprovalDecision::Denied {
                note: None,
                by: by.clone(),
            }),
            _ => None,
        }
    }
    impl Approver for TerminalApprover {
        fn request(
            &self,
            req: ApprovalRequest,
            cancel: &AtomicBool,
            timeout: Duration,
        ) -> ApprovalDecision {
            let mut input = self.input.lock().unwrap_or_else(|error| error.into_inner());
            let receiver = input.get_or_insert_with(|| {
                let (sender, receiver) = mpsc::sync_channel(16);
                thread::spawn(move || {
                    use std::io::BufRead;
                    let mut stdin = io::stdin().lock();
                    loop {
                        let mut line = String::new();
                        match stdin.by_ref().take(1025).read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) if line.len() > 1024 => {
                                if sender.send(String::new()).is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {
                                if sender.send(line).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                receiver
            });
            eprintln!(
                "\ncapturefab: approval {} · {:?}\n{}\n{}",
                req.id, req.risk, req.title, req.summary
            );
            for (name, value) in &req.facts {
                eprintln!("  {name}: {value}");
            }
            for reason in &req.reasons {
                eprintln!("  {reason}");
            }
            eprintln!(
                "Type yes {} to approve once{}, or no {} to deny.",
                req.id,
                if req.allow_run_scope {
                    format!(", run {} for this tool and camera during this task", req.id)
                } else {
                    String::new()
                },
                req.id
            );
            let start = Instant::now();
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return ApprovalDecision::Cancelled;
                }
                let Some(left) = timeout
                    .checked_sub(start.elapsed())
                    .filter(|left| !left.is_zero())
                else {
                    return ApprovalDecision::TimedOut;
                };
                match receiver.recv_timeout(left.min(Duration::from_millis(250))) {
                    Ok(line) => {
                        if cancel.load(Ordering::SeqCst) {
                            return ApprovalDecision::Cancelled;
                        }
                        if start.elapsed() >= timeout {
                            return ApprovalDecision::TimedOut;
                        }
                        if let Some(decision) = terminal_decision(&line, &req, &self.by) {
                            return decision;
                        }
                        eprintln!(
                            "capturefab: Include the displayed approval number in your answer."
                        );
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return ApprovalDecision::Denied {
                            note: Some("Terminal input ended.".into()),
                            by: self.by.clone(),
                        };
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }

    struct EventStream {
        output: Box<dyn Write>,
        source: Option<File>,
        pending: Vec<u8>,
        run: String,
        next_seq: u64,
    }
    fn private_options() -> OpenOptions {
        let mut options = OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
    }
    impl EventStream {
        fn new(path: &Path) -> Result<Self> {
            let output: Box<dyn Write> = if path == Path::new("-") {
                Box::new(io::stderr())
            } else {
                Box::new(
                    private_options()
                        .write(true)
                        .create_new(true)
                        .open(path)
                        .context("Create a new private assistant event file.")?,
                )
            };
            Ok(Self {
                output,
                source: None,
                pending: Vec::new(),
                run: String::new(),
                next_seq: 1,
            })
        }
        fn poll(&mut self, directory: &Path, run: &str) -> Result<()> {
            if run.is_empty() {
                return Ok(());
            }
            ensure!(
                ai::log::valid_run_id(run),
                "The assistant returned an invalid run ID."
            );
            if self.source.is_none() {
                let path = directory.join(format!("{run}.jsonl"));
                let before = fs::symlink_metadata(&path)?;
                ensure!(
                    before.is_file() && !before.file_type().is_symlink(),
                    "Read a regular private assistant action log."
                );
                let file = File::open(&path).context("Read the assistant action log.")?;
                let after = fs::symlink_metadata(&path)?;
                let opened = file.metadata()?;
                ensure!(
                    after.is_file() && !after.file_type().is_symlink() && opened.is_file(),
                    "The assistant action log changed while opening it."
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    ensure!(
                        before.dev() == after.dev()
                            && before.ino() == after.ino()
                            && after.dev() == opened.dev()
                            && after.ino() == opened.ino()
                            && opened.permissions().mode() & 0o077 == 0,
                        "Read the same private assistant action log."
                    );
                }
                #[cfg(not(unix))]
                {
                    ensure!(
                        before.created().ok() == opened.created().ok()
                            && after.created().ok() == opened.created().ok(),
                        "The assistant action log changed while opening it."
                    );
                }
                self.source = Some(file);
                self.run = run.to_owned();
            }
            ensure!(
                self.run == run,
                "The assistant run changed while streaming its log."
            );
            let mut chunk = [0u8; 8192];
            loop {
                let read = self.source.as_mut().unwrap().read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                self.pending.extend_from_slice(&chunk[..read]);
                while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                    ensure!(
                        end < 64 * 1024,
                        "The assistant action log has an oversized record."
                    );
                    let line: &[u8] = &self.pending[..=end];
                    let value: Value = serde_json::from_slice(&line[..end])
                        .context("Read an assistant action-log record.")?;
                    ensure!(
                        value["v"] == 1
                            && value["run"] == run
                            && value["seq"].as_u64() == Some(self.next_seq),
                        "The assistant action-log sequence changed."
                    );
                    self.output.write_all(line)?;
                    self.next_seq = self.next_seq.saturating_add(1);
                    self.pending.drain(..=end);
                }
                ensure!(
                    self.pending.len() < 64 * 1024,
                    "The assistant action log has an oversized record."
                );
            }
            self.output.flush()?;
            Ok(())
        }
    }
    fn run_task(
        cli: &Cli,
        command: &AssistantCommand,
        prepared: PreparedAssistant,
    ) -> Result<Value> {
        let AssistantCommand::Run {
            camera,
            destination,
            see_images,
            mode,
            approvals,
            on_human_change,
            events,
            ..
        } = command
        else {
            unreachable!()
        };
        if cli.session.is_none() {
            return Err(field(
                "session",
                "An assistant task needs a running session; start one with capturefab serve --name NAME.",
            ));
        }
        let limits = prepared.limits.expect("limits checked offline");
        let request = RunRequest {
            goal: prepared.prompt.expect("prompt read once"),
            mode: if mode == "autonomous" {
                RunMode::Autonomous
            } else {
                RunMode::Supervised
            },
            scope: RunScope {
                cameras: camera.clone(),
                destinations: destination.clone(),
                allow_images: *see_images,
                allow_recordings: limits.max_recordings > 0,
                allow_schedules: limits.max_captures > 0,
                effects_may_outlive_run: false,
            },
            limits,
            on_human_change: match on_human_change.as_str() {
                "inform" => OnHumanChange::Inform,
                "stop" => OnHumanChange::Stop,
                _ => OnHumanChange::Pause,
            },
            provider: prepared.provider,
            model: None,
            wait_for_answer: false,
        };
        let ask = approvals.as_deref().map_or(
            io::stdin().is_terminal() && io::stderr().is_terminal(),
            |value| value == "ask",
        );
        let approver: Arc<dyn Approver> = if ask {
            let mut by = cli.session_actor()?;
            if by.kind != ActorKind::Human {
                by = SessionActor::process(ActivityOrigin::Client);
                by.kind = ActorKind::Human;
            }
            Arc::new(TerminalApprover {
                by,
                input: std::sync::Mutex::new(None),
            })
        } else {
            Arc::new(approval::DenyApprover::new(cli.session_actor()?))
        };
        let deps = deps(cli, prepared.settings)?;
        let log_directory = deps.log_directory.clone();
        let control = RunControl::default();
        let signal = control.clone();
        ctrlc::set_handler(move || signal.stop()).context("Install the assistant stop handler.")?;
        let mut stream = events.as_deref().map(EventStream::new).transpose()?;
        let worker_control = control.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (event_sender, event_receiver) = mpsc::channel();
        thread::Builder::new()
            .name("capturefab-assistant-cli".into())
            .spawn(move || {
                let mut report = None;
                let outcome = runtime::run_blocking(
                    request,
                    &deps,
                    approver,
                    &worker_control,
                    &mut |event| {
                        if let runtime::RunEvent::Finished(outcome) = &event {
                            report = Some(outcome.clone());
                        }
                        let _ = event_sender.send(event);
                    },
                );
                let _ = sender.send((outcome, report));
            })?;
        let mut stream_error = None;
        let outcome = loop {
            for event in event_receiver.try_iter() {
                if !cli.json {
                    match event {
                        runtime::RunEvent::Agent(agent::Event::Notice(message)) => {
                            eprintln!("capturefab: {message}")
                        }
                        runtime::RunEvent::State(state)
                            if matches!(state.state, runtime::RunState::Paused) =>
                        {
                            eprintln!("capturefab: {}", state.summary)
                        }
                        _ => {}
                    }
                }
            }
            if stream_error.is_none()
                && let Some(stream) = &mut stream
                && let Err(error) = stream.poll(&log_directory, &control.status().run_id)
            {
                control.stop();
                stream_error = Some(error);
            }
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(outcome) => break outcome,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("The assistant task worker ended unexpectedly."));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        let (result, report) = outcome;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(source) => {
                if stream_error.is_none()
                    && let Some(stream) = &mut stream
                {
                    let _ = stream.poll(&log_directory, &control.status().run_id);
                }
                if let Some(outcome) = report {
                    return Err(AssistantTaskFailure {
                        message: tools::redact::for_output_text(&source.to_string()),
                        result: run_value(&outcome),
                        exit: crate::cli::error_code(&source).1,
                        source: Some(source),
                    }
                    .into());
                }
                if let Some(error) = source.downcast_ref::<ai::limits::LimitConfigError>() {
                    return Err(field("max_size", error.issues.join(" ")));
                }
                return Err(source);
            }
        };
        if stream_error.is_none()
            && let Some(stream) = &mut stream
            && let Err(error) = stream.poll(&log_directory, &outcome.run_id)
        {
            stream_error = Some(error);
        }
        let value = run_value(&outcome);
        if stream_error.is_some() {
            return Err(AssistantTaskFailure{message:"The assistant event stream could not be written. The task has ended; its private action log is available.".into(),result:value,exit:1,source:None}.into());
        }
        match outcome.outcome {
            Outcome::Finished | Outcome::NeedsUser => Ok(value),
            Outcome::Stopped | Outcome::AppQuit => Err(AssistantTaskFailure {
                message: "assistant task stopped".into(),
                result: value,
                exit: 1,
                source: None,
            }
            .into()),
            Outcome::Limit => Err(AssistantTaskFailure {
                message: tools::redact::for_output_text(&outcome.summary),
                result: value,
                exit: 1,
                source: None,
            }
            .into()),
            Outcome::Failed => Err(AssistantTaskFailure {
                message: "The assistant task failed. Review its action log.".into(),
                result: value,
                exit: 1,
                source: None,
            }
            .into()),
        }
    }
    fn run_value(outcome: &runtime::RunOutcome) -> Value {
        tools::redact::for_output(
            json!({"run_id":outcome.run_id,"outcome":outcome.outcome,"summary":outcome.summary,"totals":outcome.totals,"leftovers":outcome.leftovers,"log":outcome.log}),
        )
    }

    pub(super) fn run(
        cli: &Cli,
        command: &AssistantCommand,
        mut prepared: PreparedAssistant,
    ) -> Result<Value> {
        match command {
            AssistantCommand::Providers { check_keychain } => {
                Ok(providers(&prepared.settings, *check_keychain))
            }
            AssistantCommand::Use { .. } => {
                let id = prepared.provider.unwrap();
                let old = settings::load().settings.provider_settings(id);
                let value = prepared.settings.provider_settings(id);
                let warning = old.saved_key && endpoint_host(id, &old) != endpoint_host(id, &value);
                settings::select(id, value.clone())?;
                if warning {
                    eprintln!(
                        "capturefab: The provider host changed. A saved key will be sent to the new host when you send a request."
                    );
                }
                Ok(
                    json!({"provider":id,"model":value.model(id),"settings":value,"message":format!("Selected {} ({}).",id.info().name,value.model(id)),"warning":warning.then_some("The provider host changed; review the saved key before sending a request.")}),
                )
            }
            AssistantCommand::Key { command } => {
                let id = prepared.provider.unwrap();
                match command {
                    KeyCommand::Set { .. } => {
                        let mut key = prepared.key.take().expect("key read once");
                        ai::credentials::store_key(id, std::mem::take(&mut *key))?;
                    }
                    KeyCommand::Remove { .. } => ai::credentials::delete_key(id)?,
                }
                Ok(
                    json!({"provider":id,"saved":matches!(command,KeyCommand::Set{..}),"message":if matches!(command,KeyCommand::Set{..}){"Saved the provider key in the OS credential store."}else{"Removed the saved provider key."}}),
                )
            }
            AssistantCommand::Test { vision, .. } => {
                let id = prepared.provider.unwrap();
                let value = model_settings(&prepared, id, None)?;
                let deps = deps(cli, prepared.settings)?;
                let cancel = Arc::new(AtomicBool::new(false));
                let signal = cancel.clone();
                ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
                    .context("Install the assistant stop handler.")?;
                super::test_connection(&deps, id, value, *vision, &cancel)
            }
            AssistantCommand::Ask { .. } => ask(cli, command, prepared),
            AssistantCommand::Run { .. } => run_task(cli, command, prepared),
            AssistantCommand::Runs => Ok(serde_json::to_value(ai::log::runs()?)?),
            AssistantCommand::Log { run_id } => Ok(json!(ai::log::read_log(run_id)?)),
        }
    }

    fn test_image() -> ImagePart {
        ImagePart {
            mime: "image/jpeg",
            data: Arc::from(&include_bytes!("../assistant/connection-test-red.jpg")[..]),
            width: 64,
            height: 64,
            label: "Generated connection-test image, 64x64. This is not a camera frame.".into(),
        }
    }
    pub(super) fn test_connection(
        deps: &Deps,
        id: ProviderId,
        mut settings: ProviderSettings,
        vision: bool,
        cancel: &AtomicBool,
    ) -> Result<Value> {
        if vision {
            settings.vision = Some(VisionMode::On);
        }
        let mut model = ModelSession::resolve(deps, id, settings, Mode::Plan, vision)?;
        let tool = ToolDef {
            name: "capturefab_test".into(),
            description: "Check the Capturefab connection without reading or changing a camera."
                .into(),
            parameters: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}),
            strict: true,
        };
        let mut conversation = Conversation::new(
            id,
            &model.model,
            "This is a connection test. First call capturefab_test exactly once with value capturefab-test. After its result, answer briefly in English. If the result has an image, name its dominant colour. Do not request any other tools.",
            vec![tool],
        );
        conversation.append_user("Test the connection now.", model.wire.as_ref())?;
        let first = model.request(&mut conversation, deps, cancel)?;
        ensure!(
            matches!(first.stop, Stop::ToolUse)
                && first.calls.len() == 1
                && first.calls[0].name == "capturefab_test"
                && first.calls[0].args == json!({"value":"capturefab-test"}),
            "The provider did not complete the connection-test tool call. Choose a model that supports tools."
        );
        let call = first.calls[0].clone();
        conversation.push_reply(first)?;
        conversation.append_results(
            vec![ResultRecord {
                id: call.id,
                name: call.name,
                content: json!({"ok":true,"message":"The connection-test tool ran successfully."}),
                is_error: false,
                images: if vision {
                    vec![test_image()]
                } else {
                    Vec::new()
                },
            }],
            model.wire.as_ref(),
            &model.vision,
        )?;
        let final_reply = model.request(&mut conversation, deps, cancel)?;
        ensure!(
            final_reply.calls.is_empty() && matches!(final_reply.stop, Stop::EndTurn),
            "The provider did not finish the connection-test tool round trip."
        );
        if vision {
            ensure!(
                final_reply
                    .text
                    .split(|character: char| !character.is_ascii_alphabetic())
                    .any(|word| word.eq_ignore_ascii_case("red")),
                "The provider did not name the generated image's colour. Check the model's image support."
            );
        }
        Ok(tools::redact::for_output(
            json!({"provider":id,"model":model.model,"tool_round_trip":true,"vision":vision.then_some(true),"text":final_reply.text,"message":if vision{"Connection and image test passed."}else{"Connection test passed."}}),
        ))
    }

    pub(super) fn schema_contract() -> Value {
        let mut value = ai::schema_contract();
        value["providers"]=json!(ProviderId::ALL.iter().map(|id|{let info=id.info();json!({"id":id.id(),"name":info.name,"protocol":info.protocol,"env_vars":info.key_envs,"settings":info.settings,"default_model":info.default_model,"suggested_models":info.suggested_models,"suggested_vision_model":info.suggested_vision_model,"vision_default":info.vision_default,"vision":vision::resolve(*id,info.default_model,VisionMode::Auto,None,false)})}).collect::<Vec<_>>());
        value["supported"] = json!(true);
        value["settings"] = settings::schema_contract();
        value["commands"] = json!([
            "providers",
            "use",
            "key set",
            "key remove",
            "test",
            "ask",
            "run",
            "runs",
            "log"
        ]);
        value["modes"] = json!(["plan", "supervised", "autonomous"]);
        value["risks"] = json!([
            "observe",
            "reversible",
            "bounded",
            "disruptive",
            "human_only"
        ]);
        value["always_ask"] = json!([
            "stop_live_view or restart while the camera is busy",
            "write after a recent non-agent write, or while non-agent jobs or recordings run",
            "stop or cancel work not started by this run",
            "set_auto_exposure, connect or disconnect outside scope (runtime refuses these calls)",
            "stream_to_url",
            "commands or features outside the safe allowlist",
            "scanner starts",
            "overwrite a value a human set during this run",
            "create a destination (HumanOnly; approve once)"
        ]);
        value["scope"] = json!({"cameras":{"min":1,"max":8,"pinned_ids":true},"destinations":"saved names; empty means the local capture root","images":"--see-images is explicit consent for this task","recordings":"max_recordings > 0 permits file recordings","schedules":"max_captures > 0 permits schedules","effects_may_outlive_run":false});
        value["limits"] = json!({"defaults":Limits::default(),"ceilings":{"max_rounds":1000,"max_wall_ms":86_400_000,"max_input_tokens":50_000_000,"max_output_tokens":5_000_000,"max_cost_usd":500,"max_images":2000,"max_captures":100_000,"max_bytes":"live storage ceiling","max_recordings":32,"max_recording_ms":604_800_000,"max_setting_changes":10_000,"max_consecutive_errors":5,"max_identical_calls":3,"approval_timeout_ms":86_400_000},"minimums":{"max_wall_ms":60_000,"max_recording_ms":1000,"approval_timeout_ms":1000,"min_round_interval_ms":1000},"prices":"user-entered USD per million tokens; no built-in prices; max_cost needs known prices","unique_images":true,"projected_usage_before_request":true});
        value["approvals"] = json!({"cli":["ask","deny"],"default":"ask with stdin/stderr TTY, deny otherwise","scopes":["once","run"],"run_scope_binding":["tool","camera","risk","exact egress"],"human_only":"once only","recheck_after_wait":true,"max_denials_or_timeouts":3,"no_auto_approve":true});
        value["action_log"] = json!({"path":"<session_dir>/assistant/runs/<run_id>.jsonl","version":1,"mode":"0600","directory_mode":"0700","events":"--events FILE|- copies the exact flushed JSONL; - writes stderr","records":["start","request","text","tool","observation","approval","human_change","rollover","limit","stop","end"],"image_bytes":false,"credentials":false,"opaque_provider_reasoning":false});
        value["data_sent"] = json!({"when":"only when the user sends an assistant request","text":["request and conversation","current plan","tool schemas and descriptions","camera names, models and IDs","discovered devices","destination names and redacted locations","storage use","job file names","recent activity","tool observations, settings and metrics"],"images":"only inspected or checkpoint images when explicitly allowed and supported; resized bytes and labels","never":["API keys","passwords","destination secrets","credential URLs","base64 images in logs"],"privacy":"Requests go to the selected provider under its data terms. Loopback and plain HTTP requests bypass proxies."});
        value["apply"] = json!({"default_allow_tools":DEFAULT_ALLOW,"all_steps_ready":true,"whole_plan_allowlist_check":true,"remote_output":false,"needs_session":true});
        value["exit_codes"] = json!({"finished_or_needs_user":0,"stopped_limit_or_failed":1,"refused_config":2,"unavailable":3,"timeout":4,"unsupported_build":5,"storage_full":6});
        value
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::assistant::{
            agent::CredentialResolver,
            credentials::{Auth, ResolvedCredentials},
            http::{Http, HttpRequest, HttpResponse, ProviderError, ProviderErrorKind, Transport},
        };
        use std::{
            collections::VecDeque,
            sync::{
                Mutex,
                atomic::{AtomicU64, AtomicUsize},
            },
        };

        fn directory() -> PathBuf {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let root = std::env::temp_dir().join(format!(
                "cfa-assistant-cli-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            crate::ipc::ensure_private_dir(&root).unwrap();
            root
        }
        fn flags() -> LimitArgs {
            LimitArgs {
                max_rounds: None,
                max_time: None,
                max_cost: None,
                max_input_tokens: None,
                max_output_tokens: None,
                max_images: None,
                max_captures: None,
                max_size: None,
                max_recordings: None,
                max_recording: None,
                approval_timeout: None,
            }
        }
        fn error_field(error: &anyhow::Error) -> String {
            error.downcast_ref::<CheckFailure>().unwrap().details()["issues"][0]["field"]
                .as_str()
                .unwrap()
                .to_owned()
        }

        #[test]
        fn request_sources_are_bounded_exclusive_filtered_and_read_once() {
            let root = directory();
            let file = root.join("prompt.txt");
            fs::write(&file, "Inspect the desk camera.").unwrap();
            let args = PromptArgs {
                prompt: vec![],
                prompt_file: Some(file.clone()),
            };
            let text = read_prompt(&args).unwrap();
            fs::remove_file(&file).unwrap();
            assert_eq!(text, "Inspect the desk camera.");
            assert_eq!(
                error_field(
                    &read_prompt(&PromptArgs {
                        prompt: vec!["also text".into()],
                        prompt_file: Some(file)
                    })
                    .unwrap_err()
                ),
                "prompt_file"
            );
            for text in [
                "".to_owned(),
                "é".repeat(4001),
                "connect rtsp://user:private-password@camera.test/live".into(),
            ] {
                assert_eq!(
                    error_field(
                        &read_prompt(&PromptArgs {
                            prompt: vec![text],
                            prompt_file: None
                        })
                        .unwrap_err()
                    ),
                    "prompt"
                );
            }
            assert_eq!(
                read_prompt(&PromptArgs {
                    prompt: vec!["é".repeat(4000)],
                    prompt_file: None
                })
                .unwrap()
                .chars()
                .count(),
                4000
            );
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn limits_use_saved_defaults_and_report_the_correct_fields() {
            let defaults = Limits {
                max_captures: 7,
                ..Limits::default()
            };
            assert_eq!(limits(&defaults, &flags()).unwrap().max_captures, 7);
            let mut args = flags();
            args.max_time = Some("59s".into());
            args.max_rounds = Some(1001);
            args.max_cost = Some(f64::NAN);
            let error = limits(&defaults, &args).unwrap_err();
            let details = error.downcast_ref::<CheckFailure>().unwrap().details();
            let fields = details["issues"]
                .as_array()
                .unwrap()
                .iter()
                .map(|issue| issue["field"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(fields, vec!["max_rounds", "max_time", "max_cost"]);
            let mut args = flags();
            args.max_time = Some("24h".into());
            args.max_recordings = Some(0);
            args.max_captures = Some(0);
            args.max_size = Some("10MiB".into());
            args.approval_timeout = Some("1s".into());
            let values = limits(&defaults, &args).unwrap();
            assert_eq!(values.max_wall_ms, 86_400_000);
            assert_eq!(values.max_bytes, 10 * 1024 * 1024);
            assert_eq!(values.max_recordings, 0);
            assert_eq!(values.max_captures, 0);
        }

        #[test]
        fn output_paths_reject_existing_files_symlinks_and_missing_parents_before_requests() {
            let root = directory();
            let output = root.join("events.jsonl");
            assert!(new_output(&output, "events").is_ok());
            fs::write(&output, b"unchanged").unwrap();
            assert_eq!(
                error_field(&new_output(&output, "events").unwrap_err()),
                "events"
            );
            assert!(new_output(&root.join("missing/events.jsonl"), "events").is_err());
            assert!(new_output(&output.join("child"), "events").is_err());
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(root.join("missing-target"), root.join("link")).unwrap();
                assert!(new_output(&root.join("link"), "plan_out").is_err());
            }
            assert_eq!(fs::read(&output).unwrap(), b"unchanged");
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn terminal_approvals_require_the_displayed_number_and_respect_once_only() {
            let by = SessionActor::new("cli-human", "Operator", ActorKind::Human).unwrap();
            let mut request = ApprovalRequest {
                id: 42,
                step: tools::StepId(7),
                run: "r-20261018-120000-abcd".into(),
                tool: "set_camera_features".into(),
                risk: tools::Risk::Disruptive,
                title: "Set Gain".into(),
                summary: "Gain 1 → 2".into(),
                facts: vec![],
                reasons: vec![],
                camera: Some("sim:0".into()),
                needs_secret: false,
                allow_run_scope: true,
                expires_at_ms: 1000,
            };
            assert!(terminal_decision("yes", &request, &by).is_none());
            assert!(terminal_decision("yes 41", &request, &by).is_none());
            assert!(terminal_decision("yes 42 extra", &request, &by).is_none());
            assert!(matches!(
                terminal_decision("yes 42", &request, &by),
                Some(ApprovalDecision::Approved {
                    scope: ApprovalScope::Once,
                    ..
                })
            ));
            assert!(matches!(
                terminal_decision("run 42", &request, &by),
                Some(ApprovalDecision::Approved {
                    scope: ApprovalScope::ForRun,
                    ..
                })
            ));
            request.allow_run_scope = false;
            assert!(terminal_decision("run 42", &request, &by).is_none());
            assert!(matches!(
                terminal_decision("no 42", &request, &by),
                Some(ApprovalDecision::Denied { .. })
            ));
        }

        #[test]
        fn event_export_copies_flushed_log_bytes_exactly_without_duplicates() {
            let root = directory();
            let directory = root.join("runs");
            let export = root.join("events.jsonl");
            let mut log = ai::log::ActionLog::create(&directory, 1_800_000_000_000).unwrap();
            log.record(
                "start",
                json!({"goal":"Inspect the desk camera."}),
                1_800_000_000_000,
            )
            .unwrap();
            let mut stream = EventStream::new(&export).unwrap();
            stream.poll(&directory, log.id()).unwrap();
            stream.poll(&directory, log.id()).unwrap();
            log.record(
                "text",
                json!({"text":"Desk camera observed."}),
                1_800_000_000_001,
            )
            .unwrap();
            log.finish(
                json!({"outcome":"finished","summary":"Done."}),
                1_800_000_000_002,
            )
            .unwrap();
            stream.poll(&directory, log.id()).unwrap();
            stream.poll(&directory, log.id()).unwrap();
            assert_eq!(fs::read(&export).unwrap(), fs::read(log.path()).unwrap());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&export).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            assert!(EventStream::new(&export).is_err());
            drop(stream);
            drop(log);
            fs::remove_dir_all(root).unwrap();
        }

        #[test]
        #[cfg(unix)]
        fn event_source_refuses_a_symlink() {
            let root = directory();
            let id = "r-20261018-120000-abcd";
            let source = root.join(format!("{id}.jsonl"));
            let target = root.join("outside");
            fs::write(&target, b"private outside file").unwrap();
            std::os::unix::fs::symlink(&target, &source).unwrap();
            let mut stream = EventStream::new(&root.join("export")).unwrap();
            assert!(stream.poll(&root, id).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"private outside file");
            drop(stream);
            fs::remove_dir_all(root).unwrap();
        }

        struct Credentials(AtomicUsize);
        impl CredentialResolver for Credentials {
            fn resolve(
                &self,
                _: ProviderId,
                settings: &ProviderSettings,
            ) -> Result<ResolvedCredentials> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ResolvedCredentials {
                    auth: Auth::None,
                    settings: settings.clone(),
                })
            }
        }
        struct Script {
            replies: Mutex<VecDeque<Value>>,
            requests: Mutex<Vec<Value>>,
        }
        impl Transport for Script {
            fn send(
                &self,
                request: &HttpRequest<'_>,
            ) -> std::result::Result<HttpResponse, ProviderError> {
                self.requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&request.body).unwrap());
                let body = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| ProviderError::new(ProviderErrorKind::Malformed))?;
                Ok(HttpResponse {
                    status: 200,
                    headers: Default::default(),
                    body: serde_json::to_vec(&body).unwrap(),
                })
            }
        }
        fn probe_deps(replies: Vec<Value>) -> (Deps, Arc<Script>, Arc<Credentials>) {
            let script = Arc::new(Script {
                replies: Mutex::new(replies.into()),
                requests: Mutex::new(Vec::new()),
            });
            let credentials = Arc::new(Credentials(AtomicUsize::new(0)));
            let mut deps = Deps::native(
                Arc::new(NoSession),
                ai::Settings::default(),
                std::env::temp_dir(),
                crate::storage::StoragePolicy::default(),
                RecordingDefaults::default(),
            );
            deps.credentials = credentials.clone();
            deps.http = Arc::new(Http::with_transport(script.clone()));
            (deps, script, credentials)
        }
        fn call_reply() -> Value {
            json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"probe-1","type":"function","function":{"name":"capturefab_test","arguments":"{\"value\":\"capturefab-test\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
        }
        fn text_reply(text: &str) -> Value {
            json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
        }

        #[test]
        fn connection_probe_uses_one_safe_tool_round_trip_and_fixed_red_jpeg() {
            for images in [false, true] {
                let (deps, script, credentials) =
                    probe_deps(vec![call_reply(), text_reply("The image is red.")]);
                let settings = ProviderSettings {
                    base_url: Some("http://127.0.0.1:8123".into()),
                    model: Some("offline-test".into()),
                    ..ProviderSettings::default()
                };
                let value = super::super::test_connection(
                    &deps,
                    ProviderId::Custom,
                    settings,
                    images,
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!(value["tool_round_trip"], true);
                assert_eq!(credentials.0.load(Ordering::SeqCst), 1);
                let requests = script.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0]["tools"].as_array().unwrap().len(), 1);
                assert_eq!(
                    requests[0]["tools"][0]["function"]["name"],
                    "capturefab_test"
                );
                let body = requests[1].to_string();
                assert_eq!(body.contains("data:image/jpeg;base64,"), images);
                assert!(!body.contains("get_overview"));
                assert!(!body.contains("camera names"));
            }
            let image = test_image();
            assert_eq!(
                (image.width, image.height, image.mime),
                (64, 64, "image/jpeg")
            );
            assert_eq!(&image.data[..2], &[0xff, 0xd8]);
        }

        #[test]
        fn connection_probe_rejects_an_unexpected_tool_without_executing_anything() {
            let mut call = call_reply();
            call["choices"][0]["message"]["tool_calls"][0]["function"]["name"] =
                json!("capture_photos");
            let (deps, script, _) = probe_deps(vec![call]);
            let settings = ProviderSettings {
                base_url: Some("http://127.0.0.1:8123".into()),
                model: Some("offline-test".into()),
                ..ProviderSettings::default()
            };
            assert!(
                test_connection(
                    &deps,
                    ProviderId::Custom,
                    settings,
                    false,
                    &AtomicBool::new(false)
                )
                .is_err()
            );
            assert_eq!(script.requests.lock().unwrap().len(), 1);
        }

        #[test]
        fn schema_names_the_actual_risks_events_and_provider_vision_defaults() {
            let schema = schema_contract();
            assert_eq!(
                schema["risks"],
                json!([
                    "observe",
                    "reversible",
                    "bounded",
                    "disruptive",
                    "human_only"
                ])
            );
            assert_eq!(
                schema["action_log"]["records"],
                json!([
                    "start",
                    "request",
                    "text",
                    "tool",
                    "observation",
                    "approval",
                    "human_change",
                    "rollover",
                    "limit",
                    "stop",
                    "end"
                ])
            );
            assert_eq!(schema["providers"].as_array().unwrap().len(), 22);
            for provider in schema["providers"].as_array().unwrap() {
                assert!(provider.get("vision_default").is_some());
                assert!(provider["vision"]["placement"].is_string());
                assert!(provider["env_vars"].is_array());
            }
            assert_eq!(schema["scope"]["effects_may_outlive_run"], false);
        }

        #[test]
        fn apply_failure_retains_the_proposed_plan_and_partial_execution() {
            let error = apply_failure(
                ApplyError::Run(tools::PlanFailure {
                    source: anyhow::anyhow!("Camera disconnected."),
                    completed: vec![json!({"tool":"capture_photos","files":["first.png"]})],
                    failed_step: 2,
                    step_count: 3,
                }),
                json!({"text":"Capture three views.","plan":{"version":1,"calls":[{"tool":"capture_photos","args":{"camera":"sim:0"}}]},"steps":[{"summary":"Capture a photo."}]}),
            );
            let failure = error.downcast_ref::<AssistantTaskFailure>().unwrap();
            let details = failure.details();
            assert_eq!(details["plan"]["calls"][0]["tool"], "capture_photos");
            assert_eq!(details["applied"]["failed_step"], 2);
            assert_eq!(details["applied"]["completed"][0]["files"][0], "first.png");
            assert!(error.chain().any(|cause| cause.is::<tools::PlanFailure>()));
        }

        #[test]
        fn task_failure_retains_a_provider_cause_and_reviewable_partial_report() {
            let mut source = ProviderError::new(ProviderErrorKind::Unavailable);
            source.excerpt = Some("Scripted provider offline.".into());
            let failure = AssistantTaskFailure {
                message: source.to_string(),
                result: json!({"run_id":"r-20261018-120000-abcd","outcome":"failed","log":"private/action.jsonl","leftovers":[{"summary":"Recording continues."}]}),
                exit: 1,
                source: Some(source.into()),
            };
            let error: anyhow::Error = failure.into();
            assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
            assert!(
                error
                    .chain()
                    .any(|cause| cause.downcast_ref::<ProviderError>().is_some())
            );
            let details = error
                .downcast_ref::<AssistantTaskFailure>()
                .unwrap()
                .details();
            assert_eq!(details["outcome"], "failed");
            assert_eq!(details["leftovers"][0]["summary"], "Recording continues.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn assistant_commands_parse_without_feature_specific_clap_types() {
        for args in [
            vec!["capturefab", "assistant", "providers"],
            vec![
                "capturefab",
                "assistant",
                "use",
                "custom",
                "--base-url",
                "http://127.0.0.1:8123",
                "--vision",
                "off",
            ],
            vec!["capturefab", "assistant", "key", "remove", "custom"],
            vec!["capturefab", "assistant", "test", "custom", "--vision"],
            vec!["capturefab", "assistant", "ask", "Inspect", "the", "desk"],
            vec!["capturefab", "assistant", "runs"],
            vec!["capturefab", "assistant", "log", "r-20261018-120000-abcd"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        assert!(Cli::try_parse_from(["capturefab", "assistant", "key", "set", "custom"]).is_err());
        let cli = Cli::try_parse_from([
            "capturefab",
            "assistant",
            "run",
            "Inspect the desk",
            "--camera",
            "sim:0",
            "--camera",
            "sim:1",
            "--approvals",
            "deny",
        ])
        .unwrap();
        let Some(super::super::Command::Assistant { command }) = cli.command else {
            panic!("assistant command")
        };
        let AssistantCommand::Run { camera, mode, .. } = &command else {
            panic!("run command")
        };
        assert_eq!(camera, &vec!["sim:0".to_owned(), "sim:1".to_owned()]);
        assert_eq!(mode, "supervised");
        assert!(uses_session(&command));
    }
    #[test]
    fn raw_prompts_and_urls_are_absent_from_debug() {
        let cli = Cli::try_parse_from([
            "capturefab",
            "assistant",
            "ask",
            "rtsp://user:private-password@camera.test/live",
        ])
        .unwrap();
        let debug = format!("{:?}", cli.command);
        assert!(!debug.contains("private-password"));
        assert!(!debug.contains("rtsp://"));
    }
    #[cfg(not(feature = "ai"))]
    #[test]
    fn unsupported_dispatcher_has_the_documented_message_and_exit_code() {
        let cli = Cli::try_parse_from(["capturefab", "assistant", "providers"]).unwrap();
        let Some(super::super::Command::Assistant { command }) = &cli.command else {
            panic!("assistant command")
        };
        let error = prepare(&cli, command).err().expect("AI disabled");
        assert_eq!(
            error.to_string(),
            "the assistant is unsupported in this build; rebuild with --features ai"
        );
        assert_eq!(super::super::error_code(&error), ("unsupported", 5));
        assert_eq!(schema_contract()["supported"], false);
    }
}
