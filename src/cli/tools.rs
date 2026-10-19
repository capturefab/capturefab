//! Command-line adapters for the shared tool registry. Parsing is offline;
//! hosts, credentials and diagnostic image files are created only at run time.
use super::{Cli, InvalidInput};
use crate::tools::{
    self as registry, ApplyError, CheckFailure, Checked, Executor, Issue, Origin, PlanState, Risk,
    SecretSource, StepId, ToolCall, ToolKind,
    context::{Context, IpcHost, NoSession, RecordingDefaults, ToolHost},
    read::{Image, ReadOptions},
};
use anyhow::{Context as _, Result, anyhow, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const MAX_INPUT: usize = registry::plan::MAX_PLAN_BYTES;
const MAX_IMAGES: usize = 64;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Subcommand)]
pub enum ToolsCommand {
    /// List the shared camera tools, risks and portable argument schemas
    List,
    /// Read session facts or check and run one camera tool
    Call {
        /// Tool name from tools list
        name: String,
        /// JSON object containing every argument; use null for defaults
        #[arg(long, conflicts_with = "args_file")]
        args: Option<String>,
        /// Read the JSON argument object from this file, or - for stdin
        #[arg(long, conflicts_with = "args")]
        args_file: Option<PathBuf>,
        /// Validate and show the normalized call and commands without executing
        #[arg(long)]
        dry_run: bool,
        /// Read exactly one destination secret or stream password from stdin
        #[arg(long)]
        secret_stdin: bool,
        /// Pass only after the user approved this exact action
        #[arg(long)]
        confirmed_by_user: bool,
        /// Save an inspect_frame image here through the storage budget; never overwrite
        #[arg(long)]
        image_out: Option<PathBuf>,
        /// Include inspect_frame image bytes as base64 in the JSON result
        #[arg(long)]
        inline_image: bool,
    },
    /// Check and run a version 1 camera-tool plan in order
    Apply {
        /// Plan JSON file, or - for stdin
        file: PathBuf,
        /// Validate and show normalized calls and commands without executing
        #[arg(long)]
        dry_run: bool,
        /// Read exactly one destination secret or stream password from stdin
        #[arg(long)]
        secret_stdin: bool,
        /// Pass only after the user approved every exact action in this plan
        #[arg(long)]
        confirmed_by_user: bool,
    },
}

impl fmt::Debug for ToolsCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::List => f.write_str("Tools::List"),
            Self::Call {
                name,
                args,
                args_file,
                dry_run,
                secret_stdin,
                confirmed_by_user,
                image_out,
                inline_image,
            } => f
                .debug_struct("Tools::Call")
                .field("name", &registry::redact::for_output_text(name))
                .field("args", &args.as_ref().map(|_| "<JSON arguments>"))
                .field("args_file", args_file)
                .field("dry_run", dry_run)
                .field("secret_stdin", secret_stdin)
                .field("confirmed_by_user", confirmed_by_user)
                .field("image_out", image_out)
                .field("inline_image", inline_image)
                .finish(),
            Self::Apply {
                file,
                dry_run,
                secret_stdin,
                confirmed_by_user,
            } => f
                .debug_struct("Tools::Apply")
                .field("file", file)
                .field("dry_run", dry_run)
                .field("secret_stdin", secret_stdin)
                .field("confirmed_by_user", confirmed_by_user)
                .finish(),
        }
    }
}

/// Cached once during CLI preparation, before session auto-attachment.
pub(super) enum PreparedTools {
    List,
    Calls(Vec<(StepId, ToolCall)>),
}

fn usage(message: impl AsRef<str>) -> anyhow::Error {
    InvalidInput(registry::redact::for_output_text(message.as_ref())).into()
}

fn bounded_read(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_INPUT,
        "Use tool JSON no larger than 256 KiB."
    );
    String::from_utf8(bytes).map_err(|_| anyhow!("Use UTF-8 for tool JSON."))
}

fn read_file(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        bounded_read(io::stdin().lock())
    } else {
        bounded_read(fs::File::open(path).context("Open the tool JSON file.")?)
    }
}

fn failure(issues: Vec<Issue>, checked: Value) -> anyhow::Error {
    CheckFailure {
        issues_by_step: vec![(1, issues)],
        checked,
    }
    .into()
}

fn secret_targets(steps: &[(StepId, ToolCall)]) -> Vec<(StepId, &'static str)> {
    steps
        .iter()
        .filter_map(|(id, call)| match call.tool.as_str() {
            "stream_to_url" => Some((*id, "password")),
            "create_s3_destination" if call.args["credentials"] == "keychain" => {
                Some((*id, "secret"))
            }
            _ => None,
        })
        .collect()
}

pub(super) fn prepare(cli: &Cli, command: &ToolsCommand) -> Result<PreparedTools> {
    super::input((|| {
        ensure!(
            cli.camera.is_none(),
            "tools choose their own cameras; omit --camera"
        );
        cli.session_actor()?.validate()?;
        crate::media::validate_hwaccel(&cli.hwaccel)?;
        crate::session::validate_timeout(cli.timeout_ms)?;
        if let Some(session) = &cli.session {
            crate::ipc::validate_name(session)?;
        }
        Ok(())
    })())
    .map_err(|error| usage(format!("{error:#}")))?;
    let (steps, secret_stdin) = match command {
        ToolsCommand::List => return Ok(PreparedTools::List),
        ToolsCommand::Call {
            name,
            args,
            args_file,
            secret_stdin,
            image_out,
            inline_image,
            ..
        } => {
            if args.is_some() && args_file.is_some() {
                return Err(usage("Choose --args or --args-file, not both."));
            }
            if *secret_stdin && args_file.as_deref() == Some(Path::new("-")) {
                return Err(usage(
                    "Use a JSON file or --args when reading a secret from stdin.",
                ));
            }
            if (image_out.is_some() || *inline_image) && name != "inspect_frame" {
                return Err(usage(
                    "Use --image-out and --inline-image with inspect_frame only.",
                ));
            }
            if image_out
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty() || path == Path::new("-"))
            {
                return Err(usage("Choose a file path for --image-out."));
            }
            let spec =
                registry::spec(name).ok_or_else(|| usage("Choose a tool from tools list."))?;
            let text = match (args, args_file) {
                (Some(args), _) => args.clone(),
                (_, Some(path)) => read_file(path).map_err(|error| usage(format!("{error:#}")))?,
                _ => "{}".into(),
            };
            if text.len() > MAX_INPUT {
                return Err(usage("Use tool JSON no larger than 256 KiB."));
            }
            let args = registry::plan::parse_value(&text)
                .map_err(|error| usage(format!("Read one JSON argument object: {error}.")))?;
            if !args.is_object() {
                return Err(usage("Provide tool arguments as one JSON object."));
            }
            let (args, issues) = registry::schema::check(spec.name, &args, &(spec.schema)());
            if !issues.is_empty() {
                return Err(failure(issues, json!([])));
            }
            (
                vec![(
                    StepId(1),
                    ToolCall {
                        tool: name.clone(),
                        args,
                    },
                )],
                *secret_stdin,
            )
        }
        ToolsCommand::Apply {
            file, secret_stdin, ..
        } => {
            if *secret_stdin && file == Path::new("-") {
                return Err(usage("Use a plan file when reading a secret from stdin."));
            }
            let text = read_file(file).map_err(|error| usage(format!("{error:#}")))?;
            let document =
                registry::plan::parse(&text).map_err(|error| usage(format!("{error:#}")))?;
            (
                document
                    .calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| (StepId(index as u64 + 1), call))
                    .collect(),
                *secret_stdin,
            )
        }
    };
    if secret_stdin {
        match secret_targets(&steps).len() {
            0 => return Err(usage("Omit --secret-stdin; no step takes a secret.")),
            1 => (),
            _ => {
                return Err(usage(
                    "Use --secret-stdin with exactly one step that takes a secret.",
                ));
            }
        }
    }
    Ok(PreparedTools::Calls(steps))
}

pub struct ToolsConfirmationRequired {
    pub risk: Risk,
    pub checked: Value,
}
impl ToolsConfirmationRequired {
    pub fn details(&self) -> Value {
        registry::redact::for_output(json!({"risk":self.risk,"checked":self.checked}))
    }
}
impl fmt::Debug for ToolsConfirmationRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.details().fmt(f)
    }
}
impl fmt::Display for ToolsConfirmationRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "This action is {}; pass --confirmed-by-user only after the user approved this exact action.",
            if self.risk == Risk::HumanOnly {
                "human_only"
            } else {
                "disruptive"
            }
        )
    }
}
impl std::error::Error for ToolsConfirmationRequired {}

struct StdinSecret {
    target: Option<(StepId, &'static str)>,
    value: Option<Zeroizing<String>>,
}
impl SecretSource for StdinSecret {
    fn secret(&self, id: StepId, field: &str) -> Option<Zeroizing<String>> {
        self.target
            .filter(|target| target.0 == id && target.1 == field)
            .and_then(|_| {
                self.value
                    .as_ref()
                    .map(|value| Zeroizing::new(value.to_string()))
            })
    }
}

fn check_failure(checked: &[Checked]) -> Option<CheckFailure> {
    let issues_by_step: Vec<_> = checked
        .iter()
        .enumerate()
        .filter(|(_, step)| !step.ready())
        .map(|(index, step)| (index + 1, step.issues.clone()))
        .collect();
    (!issues_by_step.is_empty()).then(|| CheckFailure {
        issues_by_step,
        checked: json!(checked),
    })
}

fn unwrap_apply(error: ApplyError) -> anyhow::Error {
    match error {
        ApplyError::Check(error) => error.into(),
        ApplyError::Run(error) => error.into(),
    }
}

pub(super) fn run(cli: &Cli, command: &ToolsCommand, prepared: PreparedTools) -> Result<Value> {
    let PreparedTools::Calls(steps) = prepared else {
        return Ok(registry::catalog_json());
    };
    let override_timeout = cli
        .request_timeout
        .as_deref()
        .map(crate::scheduling::parse_duration)
        .transpose()?
        .map(Duration::from_millis);
    let host: Box<dyn ToolHost> = match &cli.session {
        Some(name) => Box::new(IpcHost::new(
            name,
            Some(cli.session_actor()?),
            override_timeout,
        )),
        None => Box::new(NoSession),
    };
    let (dry_run, secret_stdin, confirmed) = match command {
        ToolsCommand::Call {
            dry_run,
            secret_stdin,
            confirmed_by_user,
            ..
        }
        | ToolsCommand::Apply {
            dry_run,
            secret_stdin,
            confirmed_by_user,
            ..
        } => (*dry_run, *secret_stdin, *confirmed_by_user),
        ToolsCommand::List => unreachable!("list has no prepared calls"),
    };
    if !host.has_session() {
        for (_, call) in &steps {
            if registry::spec(&call.tool).is_some_and(|spec| spec.needs_session) {
                let message = if call.tool == "inspect_frame" {
                    "inspect_frame needs a running session; start one with `capturefab serve --name NAME` (or `gui --name NAME`) and connect the camera there"
                } else {
                    registry::context::NO_SESSION
                };
                return Err(failure(vec![Issue::usage(None, message)], json!([])));
            }
        }
    }
    let target = secret_targets(&steps).into_iter().next();
    let secret = if secret_stdin {
        Some(
            crate::secret_input::read_stdin_secret(io::stdin().lock(), "tool secret")
                .map_err(|error| usage(format!("{error:#}")))?,
        )
    } else {
        None
    };
    let secrets = StdinSecret {
        target,
        value: secret,
    };
    let root = std::env::current_dir()?;
    let policy = crate::storage::configured_policy()?;
    let gather = || {
        Context::gather(
            host.as_ref(),
            root.clone(),
            policy.clone(),
            RecordingDefaults::default(),
        )
    };
    let ctx = gather()?;
    let mut state = PlanState::new(&ctx);
    state.actor_id = host.actor_id();
    let present = |id, field: &str| secrets.secret(id, field).is_some();
    let checked = registry::check_plan_with(&ctx, &mut state, &steps, &present);
    // Missing approval takes precedence over checked availability issues, so
    // installing FFmpeg cannot change an unconfirmed action's error category.
    if !dry_run
        && !confirmed
        && let Some(risk) = checked
            .iter()
            .map(|step| step.risk)
            .filter(|risk| *risk >= Risk::Disruptive)
            .max()
    {
        return Err(ToolsConfirmationRequired {
            risk,
            checked: json!(checked),
        }
        .into());
    }
    if let Some(failure) = check_failure(&checked) {
        return Err(failure.into());
    }
    if !cli.json {
        for (index, step) in checked.iter().enumerate() {
            for issue in &step.issues {
                eprintln!("capturefab: step {}: {}", index + 1, issue.message);
            }
        }
    }
    if dry_run {
        return Ok(registry::redact::for_output(
            json!({"checked":checked,"commands":checked.iter().flat_map(|step|step.ops.iter()).collect::<Vec<_>>()}),
        ));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let signal_cancel = cancel.clone();
    ctrlc::set_handler(move || signal_cancel.store(true, Ordering::Relaxed))
        .context("Install the camera-tool stop handler.")?;
    if steps.len() == 1
        && registry::spec(&steps[0].1.tool).is_some_and(|spec| spec.kind == ToolKind::Read)
    {
        let call = &steps[0].1;
        let mut output = registry::read::run_read(
            host.as_ref(),
            &ctx,
            call,
            &ReadOptions::new(Origin::Cli, &cancel),
        )
        .map_err(|issues| failure(issues, json!(checked)))?;
        if let Some(image) = output.images.first() {
            let (image_out, inline_image) = match command {
                ToolsCommand::Call {
                    image_out,
                    inline_image,
                    ..
                } => (image_out.as_deref(), *inline_image),
                _ => unreachable!("read tool is one call"),
            };
            output.value["image"] = deliver_image(
                cli,
                call,
                image,
                output.value["frame"]["seq"].as_u64().unwrap_or(0),
                image_out,
                inline_image,
                &ctx.storage.request,
            )?;
        }
        return Ok(registry::redact::for_output(output.value));
    }
    if matches!(command, ToolsCommand::Apply { .. }) {
        // Preserve normalized cameras and pinned times from this first check.
        let normalized: Vec<_> = checked
            .iter()
            .map(|step| (step.id, step.call.clone()))
            .collect();
        return registry::apply(
            host.as_ref(),
            &secrets,
            gather,
            &normalized,
            Some(&checked),
            |progress| {
                if !cli.json && matches!(progress.state, registry::exec::ProgressState::Running) {
                    eprintln!(
                        "capturefab: step {} of {}: {}",
                        progress.step, progress.step_count, progress.summary
                    );
                }
            },
            &cancel,
        )
        .map_err(unwrap_apply);
    }
    let mut executor = Executor::new(host.as_ref(), &secrets);
    let outcome =
        executor
            .run_step(&checked[0], &cancel)
            .map_err(|source| registry::PlanFailure {
                source,
                completed: Vec::new(),
                failed_step: 1,
                step_count: 1,
            })?;
    Ok(registry::redact::for_output(json!(outcome)))
}

fn safe_label(value: &str) -> String {
    // Redact before replacing punctuation: otherwise a URL password loses
    // its URL shape, and output redaction can change a path after writing.
    let value = registry::redact::for_output_text(value);
    let label: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if label.is_empty() {
        "camera".into()
    } else {
        label
    }
}

fn private_image(dir: &Path, label: &str, bytes: &[u8], extension: &str) -> Result<PathBuf> {
    crate::ipc::ensure_private_dir(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..100 {
        let seq = NEXT_IMAGE.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{label}-{stamp}-{seq}.{extension}"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(bytes) {
                    let _ = fs::remove_file(&path);
                    return Err(error.into());
                }
                drop(file);
                prune_images(dir, Some(&path))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!("Choose a new diagnostic image name."))
}

fn prune_images(dir: &Path, keep: Option<&Path>) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_file() && !meta.file_type().is_symlink() {
            files.push((
                entry.path(),
                meta.modified().unwrap_or(UNIX_EPOCH),
                meta.len(),
            ));
        }
    }
    files.sort_by(|a, b| {
        (keep == Some(b.0.as_path()))
            .cmp(&(keep == Some(a.0.as_path())))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.0.cmp(&a.0))
    });
    let mut bytes = 0u64;
    for (index, (path, _, size)) in files.into_iter().enumerate() {
        bytes = bytes.saturating_add(size);
        if index >= MAX_IMAGES || bytes > MAX_IMAGE_BYTES {
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn deliver_image(
    cli: &Cli,
    call: &ToolCall,
    image: &Image,
    frame_seq: u64,
    image_out: Option<&Path>,
    inline: bool,
    policy: &crate::storage::StoragePolicy,
) -> Result<Value> {
    let path = if let Some(path) = image_out {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        crate::storage::save_bytes(&image.data, &path, policy)?;
        path
    } else {
        let label = format!(
            "{}-{}-{frame_seq}",
            safe_label(cli.session.as_deref().unwrap_or("session")),
            safe_label(call.args["camera"].as_str().unwrap_or("camera"))
        );
        private_image(
            &crate::ipc::session_dir().join("inspect"),
            &label,
            &image.data,
            if image.mime == "image/jpeg" {
                "jpg"
            } else {
                "png"
            },
        )?
    };
    let mut value = json!({"path":path,"mime":image.mime,"width":image.width,"height":image.height,"bytes":image.data.len(),"sha256":format!("{:x}",Sha256::digest(image.data.as_ref()))});
    if inline {
        use base64::Engine as _;
        value["base64"] =
            json!(base64::engine::general_purpose::STANDARD.encode(image.data.as_ref()));
    }
    Ok(value)
}

pub(super) fn print_human(command: &ToolsCommand, value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    if matches!(command, ToolsCommand::List) {
        writeln!(out, "{:<28} {:<12} TITLE", "TOOL", "RISK")?;
        for spec in registry::catalog() {
            writeln!(
                out,
                "{:<28} {:<12} {}",
                spec.name,
                serde_json::to_value(spec.risk)?
                    .as_str()
                    .unwrap_or_default(),
                spec.title
            )?;
        }
    } else if let Some(checked) = value["checked"].as_array() {
        for (index, step) in checked.iter().enumerate() {
            writeln!(
                out,
                "{}. {}",
                index + 1,
                step["summary"].as_str().unwrap_or_default()
            )?;
        }
    } else if let Some(summary) = value["summary"].as_str() {
        writeln!(out, "{summary}")?;
    } else if let Some(completed) = value["completed"].as_array() {
        for outcome in completed {
            writeln!(
                out,
                "{}",
                outcome["summary"].as_str().unwrap_or("Completed step.")
            )?;
        }
    } else {
        serde_json::to_writer_pretty(&mut out, value)?;
        writeln!(out)?;
    }
    Ok(())
}

pub(super) fn schema_contract() -> Value {
    json!({
        "catalog":registry::catalog_json(),
        "plan":{"version":registry::PLAN_VERSION,"shape":{"version":1,"calls":[{"tool":"tool name","args":"complete argument object"}]},"max_calls":registry::MAX_PLAN_CALLS,"max_bytes":MAX_INPUT,"kinds":["control","capture","configure"],"unknown_fields":"refused","export":"0600; credential URLs refused"},
        "issues":{"shape":{"step":"1-based integer","field":"argument path or null","message":"redacted sentence","severity":"error or warning","code":"typed category"},"exit_codes":{"usage":2,"unavailable":3,"timeout":4,"unsupported":5,"storage_full":6,"operation_failed":1}},
        "confirmation":{"risks":["disruptive","human_only"],"flag":"--confirmed-by-user","rule":"Pass only after the user approved this exact action.","missing":"usage (exit 2) with risk and checked steps, before checked validation or availability errors","dry_run":"confirmation is not necessary; checked issues determine the exit code"},
        "images":{"tool":"inspect_frame","default":"private ephemeral <session_dir>/inspect/ files outside the storage ledger","directory_mode":"0700","file_mode":"0600","max_files":MAX_IMAGES,"max_bytes":MAX_IMAGE_BYTES,"image_out":"storage-budgeted new file; existing files are never overwritten","inline_image":"image.base64 holds the encoded preview","metadata":["path","mime","width","height","bytes","sha256"]},
        "secrets":{"source":"--secret-stdin","count":"exactly one eligible step","fields":["create_s3_destination.secret","stream_to_url.password"],"schemas":"secret and password are UI/CLI-only and absent from schemas"},
        "redaction":"URL passwords, credential query values, RTMP stream keys and AWS access key IDs are removed from output; model output additionally removes serial, address, profile and full network URL paths."
    })
}

#[cfg(test)]
mod tests {
    use super::super::Command;
    use super::*;
    use clap::Parser;

    #[test]
    fn bounded_json_reads_stop_at_the_contract_limit() {
        assert!(bounded_read("x".repeat(MAX_INPUT + 1).as_bytes()).is_err());
        assert!(bounded_read(&[0xff][..]).is_err());
        assert_eq!(bounded_read(b"{}".as_slice()).unwrap(), "{}");
    }

    #[test]
    fn invalid_tool_arguments_keep_typed_field_issues() {
        let cli = Cli::try_parse_from([
            "capturefab",
            "tools",
            "call",
            "start_live_view",
            "--args",
            "{}",
        ])
        .unwrap();
        let Some(Command::Tools { command }) = &cli.command else {
            panic!("tools")
        };
        let error = prepare(&cli, command).err().unwrap();
        let failure = error.downcast_ref::<CheckFailure>().unwrap();
        assert_eq!(failure.details()["issues"][0]["field"], "camera");
        assert_eq!(super::super::error_code(&error), ("usage", 2));
    }

    #[test]
    fn secret_stdin_rejects_ambiguous_targets_and_json_stdin() {
        for args in [
            vec!["tools", "call", "get_overview", "--secret-stdin"],
            vec![
                "tools",
                "call",
                "get_overview",
                "--args-file",
                "-",
                "--secret-stdin",
            ],
            vec!["tools", "apply", "-", "--secret-stdin"],
        ] {
            let cli = Cli::try_parse_from(std::iter::once("capturefab").chain(args)).unwrap();
            let Some(Command::Tools { command }) = &cli.command else {
                panic!("tools")
            };
            assert!(prepare(&cli, command).is_err());
        }
    }

    #[test]
    fn confirmation_and_check_failure_exit_codes_use_types() {
        let error: anyhow::Error = ToolsConfirmationRequired {
            risk: Risk::Disruptive,
            checked: json!([]),
        }
        .into();
        assert_eq!(super::super::error_code(&error), ("usage", 2));
        for code in [
            registry::IssueCode::Usage,
            registry::IssueCode::Unavailable,
            registry::IssueCode::Timeout,
            registry::IssueCode::Unsupported,
            registry::IssueCode::StorageFull,
            registry::IssueCode::OperationFailed,
        ] {
            let error: anyhow::Error = CheckFailure {
                issues_by_step: vec![(
                    1,
                    vec![Issue::error(
                        Some("camera"),
                        "Words such as timed out and unsupported do not decide this exit.",
                        code,
                    )],
                )],
                checked: json!([]),
            }
            .into();
            assert_eq!(super::super::error_code(&error), code.exit());
        }
    }

    #[test]
    fn private_images_are_bounded_private_and_never_follow_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-tools-images-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for index in 0..MAX_IMAGES + 3 {
            private_image(&root, &format!("camera-{index}"), b"jpeg", "jpg").unwrap();
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), MAX_IMAGES);
        // Sparse files exercise the byte limit without consuming 80 MiB
        // on the shared build disk.
        for index in 0..2 {
            let path = private_image(&root, &format!("large-{index}"), b"jpeg", "jpg").unwrap();
            OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_len(40 * 1024 * 1024)
                .unwrap();
        }
        prune_images(&root, None).unwrap();
        let size: u64 = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(size <= MAX_IMAGE_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for file in fs::read_dir(&root).unwrap() {
                assert_eq!(
                    file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let link = root.with_extension("link");
            symlink(&root, &link).unwrap();
            assert!(private_image(&link, "camera", b"jpeg", "jpg").is_err());
            fs::remove_file(link).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_names_are_redacted_before_sanitizing_and_paths_round_trip() {
        let root = std::env::temp_dir().join(format!(
            "capturefab-tools-image-name-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for camera in [
            "sim:AKIA1234567890ABCDEF",
            "rtsp://alice:private-password@host/path",
        ] {
            let path = private_image(&root, &safe_label(camera), b"jpeg", "jpg").unwrap();
            let shown = registry::redact::for_output(json!({"path":path}));
            assert_eq!(Path::new(shown["path"].as_str().unwrap()), path);
            assert!(path.exists());
            assert!(!path.to_string_lossy().contains("AKIA1234567890ABCDEF"));
            assert!(!path.to_string_lossy().contains("private-password"));
        }
        fs::remove_dir_all(root).unwrap();
    }
}
