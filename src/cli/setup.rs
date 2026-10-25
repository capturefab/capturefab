//! `capturefab setup`: save a tuned camera's settings by name and restore
//! them later, previewing what changes first.
use super::*;
use crate::setup::{self as setups, Setup, Store};
use std::path::Path;

#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Save the camera's current settings as a named setup (needs --camera or --session)
    Save {
        name: String,
        /// A short note, such as the lighting it was tuned for
        #[arg(long)]
        note: Option<String>,
        /// Overwrite a saved setup with the same name
        #[arg(long)]
        replace: bool,
    },
    /// List saved setups and the folder that holds them
    List,
    /// Show a saved setup's values; --json prints the document (redirect it to a file to back up or share; import and --file read that file)
    Show { name: String },
    /// Preview what restoring a setup would change on the camera; changes nothing
    Diff {
        #[arg(required_unless_present = "file")]
        name: Option<String>,
        /// Compare a setup document from a file instead of a saved setup
        #[arg(long)]
        file: Option<PathBuf>,
        /// Leave out the note that setup apply needs --allow-other-model; a diff always compares the settings both cameras share
        #[arg(long)]
        allow_other_model: bool,
    },
    /// Restore a saved setup as one camera change that undo reverts
    Apply {
        #[arg(required_unless_present = "file")]
        name: Option<String>,
        /// Restore a setup document from a file instead of a saved setup
        #[arg(long)]
        file: Option<PathBuf>,
        /// Print what would change and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Fail, changing nothing, if any saved value cannot be applied to this camera
        #[arg(long)]
        strict: bool,
        /// Apply to a camera of another model; only settings both share change
        #[arg(long)]
        allow_other_model: bool,
        /// Confirm changes to settings outside the known image settings, such as triggering and I/O lines
        #[arg(long)]
        yes: bool,
    },
    /// Delete a saved setup; the camera is not changed
    Delete { name: String },
    /// Save a setup document from a file, such as one written by setup show NAME --json
    Import {
        file: PathBuf,
        /// Save it under this name instead of the one in the file
        #[arg(long)]
        name: Option<String>,
        /// Overwrite a saved setup with the same name
        #[arg(long)]
        replace: bool,
    },
}

/// Save, diff and apply talk to a camera; the rest only to the store.
pub(super) fn needs_camera(command: &SetupCommand) -> bool {
    matches!(
        command,
        SetupCommand::Save { .. } | SetupCommand::Diff { .. } | SetupCommand::Apply { .. }
    )
}

/// Check names and notes before connecting to anything.
pub(super) fn prepare(command: &SetupCommand) -> Result<()> {
    match command {
        SetupCommand::Save { name, note, .. } => {
            setups::validate_name(name)?;
            if let Some(note) = note {
                ensure!(
                    note.chars().count() <= setups::MAX_NOTE,
                    "a setup note is at most {} characters",
                    setups::MAX_NOTE
                );
            }
        }
        SetupCommand::Show { name } | SetupCommand::Delete { name } => setups::validate_name(name)?,
        SetupCommand::Diff { name, file, .. } | SetupCommand::Apply { name, file, .. } => {
            if file.is_none()
                && let Some(name) = name
            {
                setups::validate_name(name)?;
            }
        }
        SetupCommand::Import { name, .. } => {
            if let Some(name) = name {
                setups::validate_name(name)?;
            }
        }
        SetupCommand::List => {}
    }
    Ok(())
}

fn taken(error: anyhow::Error) -> anyhow::Error {
    if error.is::<setups::NameTaken>() {
        InvalidInput(error.to_string()).into()
    } else {
        error
    }
}

/// Fail on a missing saved setup, or on a save under a taken name or into
/// a full store, before connecting to a camera.
pub(super) fn check_saved(command: &SetupCommand) -> Result<()> {
    if let SetupCommand::Save { name, replace, .. } = command {
        // Store::save checks again under its lock.
        Store::session().check_free(name, *replace).map_err(taken)?;
    }
    if let SetupCommand::Diff {
        name: Some(name),
        file: None,
        ..
    }
    | SetupCommand::Apply {
        name: Some(name),
        file: None,
        ..
    } = command
    {
        Store::session().get(name)?;
    }
    Ok(())
}

/// Setup commands that need no camera.
pub(super) fn run_store(command: &SetupCommand) -> Result<Value> {
    let store = Store::session();
    Ok(match command {
        SetupCommand::List => json!({"dir":store.dir(),"setups":store.list()?}),
        SetupCommand::Show { name } => serde_json::to_value(store.get(name)?)?,
        SetupCommand::Delete { name } => {
            let removed = store.remove(name)?;
            json!({"removed":removed.name})
        }
        SetupCommand::Import {
            file,
            name,
            replace,
        } => {
            let setup = input(setups::read_file(file))?;
            let mut setup = setup;
            if let Some(name) = name {
                setup.name = name.clone();
            }
            let path = store.save(&setup, *replace).map_err(taken)?;
            summary(&setup, &path)
        }
        _ => unreachable!("camera setup commands run with a client"),
    })
}

fn summary(setup: &Setup, path: &Path) -> Value {
    json!({"setup":{
        "name":setup.name,
        "camera":setup.camera,
        "values":setup.values.len(),
        "skipped":setup.skipped,
        "saved_at":setup.saved_at,
        "path":path,
    }})
}

/// The setup a diff or apply names: a saved one (sent by name, so the
/// session loads it) or a file (sent inline).
fn load(name: &Option<String>, file: &Option<PathBuf>) -> Result<(Option<String>, Setup)> {
    match file {
        Some(file) => Ok((None, input(setups::read_file(file))?)),
        None => {
            let name = name.clone().context("name a setup or pass --file")?;
            let setup = Store::session().get(&name)?;
            Ok((Some(name), setup))
        }
    }
}

/// The refusal for a setup from another model, if the connected camera is
/// one; the camera worker checks again for every other caller.
fn other_model(client: &Client, setup: &Setup, allow_other_model: bool) -> Result<Option<String>> {
    if allow_other_model {
        return Ok(None);
    }
    let status = client.call(SessionCommand::Status)?;
    Ok(
        serde_json::from_value::<crate::types::CameraInfo>(status["connected"].clone())
            .ok()
            .filter(|info| setups::matching(&setup.camera, info).0 == setups::Match::OtherModel)
            .map(|info| {
                setups::other_model_message(setup, &info).replace(
                    "Pass allow_other_model (CLI: --allow-other-model)",
                    "Pass --allow-other-model",
                )
            }),
    )
}

/// Refuse a setup from another model before asking the camera, as a usage
/// error.
fn precheck(client: &Client, setup: &Setup, allow_other_model: bool) -> Result<()> {
    match other_model(client, setup, allow_other_model)? {
        Some(message) => Err(InvalidInput(message).into()),
        None => Ok(()),
    }
}

fn inline(name: &Option<String>, setup: &Setup) -> Option<Setup> {
    name.is_none().then(|| setup.clone())
}

/// Setup commands that read or change a camera.
pub(super) fn run_camera(cli: &Cli, client: &Client, command: &SetupCommand) -> Result<Value> {
    match command {
        SetupCommand::Save {
            name,
            note,
            replace,
        } => {
            let reading = serde_json::from_value(client.call(SessionCommand::ReadSetup)?)
                .context("read the camera's settings")?;
            let setup = setups::from_read(name, note.clone(), reading)?;
            let path = Store::session().save(&setup, *replace).map_err(taken)?;
            let mut result = summary(&setup, &path);
            // The camera the restore hint names; the document holds no id.
            let status = client.call(SessionCommand::Status)?;
            if let Some(id) = status["connected"]["id"].as_str() {
                result["camera"] = json!({"id": crate::media::redact_url(id)});
            }
            Ok(result)
        }
        SetupCommand::Diff {
            name,
            file,
            allow_other_model,
        } => {
            let (name, setup) = load(name, file)?;
            // A preview changes nothing: another model is compared too, with
            // its note in warnings, so the person sees what
            // --allow-other-model applies.
            if other_model(client, &setup, *allow_other_model)?.is_some() {
                eprintln!(
                    "capturefab: setup apply needs --allow-other-model for a setup from another model"
                );
            }
            client.call(SessionCommand::SetupDiff {
                setup: inline(&name, &setup),
                name,
                allow_other_model: true,
            })
        }
        SetupCommand::Apply {
            name,
            file,
            dry_run,
            strict,
            allow_other_model,
            yes,
        } => {
            let (name, setup) = load(name, file)?;
            precheck(client, &setup, *allow_other_model)?;
            let mut diff = client.call(SessionCommand::SetupDiff {
                setup: inline(&name, &setup),
                name: name.clone(),
                allow_other_model: *allow_other_model,
            })?;
            if *dry_run {
                diff["dry_run"] = json!(true);
            }
            // A strict restore this camera would refuse fails here too, so a
            // --dry-run --strict preview never reports success for it; the
            // worker checks again.
            if *strict && let Some(refusal) = strict_refusal(&diff) {
                if !cli.json {
                    write_diff(&mut io::stdout().lock(), &diff, "Would change")?;
                }
                return Err(refusal);
            }
            if *dry_run {
                return Ok(diff);
            }
            let disruptive = changes(&diff)
                .filter(|change| change["risk"] == "disruptive")
                .map(|change| change["feature"].as_str().unwrap_or_default().to_owned())
                .fold(Vec::<String>::new(), |mut names, name| {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                    names
                });
            if !disruptive.is_empty() && !*yes {
                if !cli.json {
                    let mut out = io::stdout().lock();
                    write_diff(&mut out, &diff, "Would change")?;
                }
                return Err(InvalidInput(confirm_message(
                    cli,
                    diff["camera"]["id"].as_str(),
                    &disruptive,
                    &name,
                    file,
                    *allow_other_model,
                    *strict,
                ))
                .into());
            }
            // The worker compares again; a disruptive change that appeared
            // since this diff was not confirmed and is refused.
            let result = client.call(SessionCommand::ApplySetup {
                setup: inline(&name, &setup),
                name,
                digest: diff["digest"].as_str().map(str::to_owned),
                allow_other_model: *allow_other_model,
                strict: *strict,
                // Each by feature and bank, as reviewed.
                confirmed_disruptive: Some(
                    changes(&diff)
                        .filter(|change| change["risk"] == "disruptive")
                        .filter_map(crate::setup::Confirmed::of_change)
                        .fold(Vec::new(), |mut confirmed, change| {
                            if !confirmed.contains(&change) {
                                confirmed.push(change);
                            }
                            confirmed
                        }),
                ),
            })?;
            if !cli.json {
                let skipped = result["not_applied"].as_array().map_or(0, Vec::len);
                if skipped > 0 {
                    eprintln!(
                        "capturefab: {skipped} saved {} not applied; see the list below",
                        if skipped == 1 {
                            "value was"
                        } else {
                            "values were"
                        }
                    );
                }
            }
            Ok(result)
        }
        _ => unreachable!("store setup commands run without a camera"),
    }
}

/// "--session NAME --camera ID " or "--camera ID " for a printed command
/// that targets the camera this one used. In a session that is `camera`,
/// the id the command resolved (a session's default camera may be another
/// one); without a session, the --camera given. A placeholder otherwise.
fn target(cli: &Cli, camera: Option<&str>) -> String {
    let flag = |camera: &str| {
        format!(
            "--camera {} ",
            crate::hint::shell_quote(&crate::media::redact_url(camera))
        )
    };
    match &cli.session {
        Some(session) => format!(
            "--session {} {}",
            crate::hint::shell_quote(session),
            camera
                .or(cli.camera.as_deref())
                .map(flag)
                .unwrap_or_default()
        ),
        None => cli
            .camera
            .as_deref()
            .or(camera)
            .map_or_else(|| "--camera ID ".into(), flag),
    }
}

/// The refusal for disruptive changes without --yes, with the commands
/// that review and confirm this same setup on this same camera.
fn confirm_message(
    cli: &Cli,
    camera: Option<&str>,
    disruptive: &[String],
    name: &Option<String>,
    file: &Option<PathBuf>,
    allow_other_model: bool,
    strict: bool,
) -> String {
    let target = target(cli, camera);
    let source = match (file, name) {
        (Some(file), _) => format!(
            "--file {}",
            crate::hint::shell_quote(&file.to_string_lossy())
        ),
        (None, name) => crate::hint::shell_quote(name.as_deref().unwrap_or_default()),
    };
    format!(
        "This setup changes {}, which {} and may change triggering, I/O lines or acquisition rate. Review with capturefab {target}setup diff {source}, then run capturefab {target}setup apply {source}{}{} --yes",
        disruptive.join(", "),
        if disruptive.len() == 1 {
            "is not a known image setting"
        } else {
            "are not known image settings"
        },
        if allow_other_model {
            " --allow-other-model"
        } else {
            ""
        },
        if strict { " --strict" } else { "" },
    )
}

/// The worker's strict refusal for a diff: any value this camera lacks,
/// cannot write or would not take.
fn strict_refusal(diff: &Value) -> Option<anyhow::Error> {
    let failing = diff["not_applied"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| {
            matches!(
                row["status"].as_str(),
                Some("missing" | "read_only" | "invalid" | "refused")
            )
        })
        .map(|row| {
            format!(
                "{} ({})",
                row["feature"].as_str().unwrap_or_default(),
                row["reason"]
                    .as_str()
                    .or(row["status"].as_str())
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    (!failing.is_empty()).then(|| {
        crate::types::Categorized::error(
            crate::types::ErrorKind::Unsupported,
            format!(
                "Setup {} cannot be applied strictly; nothing changed. Not applicable: {}",
                diff["setup"].as_str().unwrap_or_default(),
                failing.join(", ")
            ),
        )
    })
}

/// Software auto exposure's change in a diff, as people read it.
fn auto_row(auto: &Value) -> Option<String> {
    let balance = |value: &Value| value.as_f64().map_or("?".into(), |b| format!("{b:.2}"));
    match auto["action"].as_str()? {
        "enter" => Some(format!(
            "Auto exposure: off → on (balance {})",
            balance(&auto["saved"])
        )),
        "balance" => Some(format!(
            "Auto exposure balance: {} → {}",
            balance(&auto["current"]),
            balance(&auto["saved"])
        )),
        "release" => Some("Auto exposure: on → off (manual)".into()),
        _ => None,
    }
}

fn changes(diff: &Value) -> impl Iterator<Item = &Value> {
    diff["changes"].as_array().into_iter().flatten()
}

/// A value as people read it; whole floats without ".0", as the desktop
/// shows them.
fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "?".into(),
        Value::Number(number) => {
            let text = number.to_string();
            match text.strip_suffix(".0") {
                Some("-0") => "0".into(),
                Some(whole) => whole.into(),
                None => text,
            }
        }
        value => value.to_string(),
    }
}

fn banks(row: &Value) -> String {
    let at = row["at"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|bank| {
            format!(
                "{} {}",
                bank["selector"].as_str().unwrap_or_default(),
                bank["value"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    if at.is_empty() {
        String::new()
    } else {
        format!(" ({})", at.join(", "))
    }
}

fn write_diff(out: &mut impl Write, diff: &Value, verb: &str) -> Result<()> {
    let auto = auto_row(&diff["auto_change"]);
    let count = diff["changes"].as_array().map_or(0, Vec::len) + usize::from(auto.is_some());
    let unchanged = diff["unchanged"].as_u64().unwrap_or(0);
    let camera = diff["camera"]["id"].as_str().unwrap_or("the camera");
    if count == 0 {
        let setup = diff["setup"].as_str().unwrap_or("this setup");
        match unchanged {
            0 => writeln!(out, "Nothing to change on {camera}.")?,
            1 => writeln!(
                out,
                "Nothing to change: the 1 setting in {setup} already matches {camera}."
            )?,
            n => writeln!(
                out,
                "Nothing to change: all {n} settings in {setup} already match {camera}."
            )?,
        }
    } else {
        writeln!(
            out,
            "{verb} {count} on {camera}, {unchanged} already {}.",
            if unchanged == 1 { "matches" } else { "match" }
        )?;
    }
    for warning in diff["warnings"].as_array().into_iter().flatten() {
        writeln!(out, "  Note: {}", shown(warning))?;
    }
    for change in changes(diff) {
        writeln!(
            out,
            "  {}{}{}: {} → {}{}",
            if change["risk"] == "disruptive" {
                "! "
            } else {
                ""
            },
            change["feature"].as_str().unwrap_or_default(),
            banks(change),
            shown(&change["current"]),
            shown(&change["saved"]),
            change["warning"]
                .as_str()
                .map(|w| format!(" ({w})"))
                .unwrap_or_default()
        )?;
    }
    if let Some(auto) = &auto {
        writeln!(out, "  {auto}")?;
    }
    if changes(diff).any(|change| change["risk"] == "disruptive") {
        writeln!(
            out,
            "  ! not a known image setting (may change triggering or I/O lines); restoring needs --yes"
        )?;
    }
    let not_applied = diff["not_applied"].as_array().cloned().unwrap_or_default();
    if !not_applied.is_empty() {
        writeln!(out, "Not applied:")?;
        for row in &not_applied {
            writeln!(
                out,
                "  {}{} ({})",
                row["feature"].as_str().unwrap_or_default(),
                banks(row),
                row["reason"].as_str().unwrap_or_default()
            )?;
        }
    }
    if diff["restarts_stream"] == true {
        writeln!(
            out,
            "Restoring stops and restarts the stream to change {}.",
            changes(diff)
                .filter(|change| change["stream_locked"] == true)
                .filter_map(|change| change["feature"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    if let Some(blocked) = diff["blocked"].as_str() {
        writeln!(out, "Blocked: {blocked}")?;
    }
    Ok(())
}

fn camera_label(camera: &Value) -> String {
    let vendor = camera["vendor"].as_str().unwrap_or_default().trim();
    let model = camera["model"].as_str().unwrap_or_default().trim();
    let name = if vendor.is_empty() || model.starts_with(vendor) {
        model.to_owned()
    } else {
        format!("{vendor} {model}")
    };
    match camera["serial"].as_str() {
        Some(serial) if !serial.is_empty() => format!("{name} (serial {serial})"),
        _ => name,
    }
}

/// A restore's result: what changed, what did not apply, and the undo.
fn write_applied(out: &mut impl Write, cli: &Cli, value: &Value) -> Result<()> {
    let setup = value["setup"].as_str().unwrap_or_default();
    let camera = value["camera"]["id"].as_str().unwrap_or("the camera");
    let auto = (value["auto_changed"] == true)
        .then(|| auto_row(&value["auto_change"]))
        .flatten();
    if value["applied"].as_u64() == Some(0) && auto.is_none() {
        writeln!(
            out,
            "{camera} already matches {setup}; nothing was changed."
        )?;
    } else if value["applied"].as_u64() == Some(0) {
        writeln!(
            out,
            "Restored {setup} on {camera}: only auto exposure changed, {} already matched.",
            value["unchanged"]
        )?;
    } else {
        writeln!(
            out,
            "Restored {setup} on {camera}: {} changed, {} already matched.",
            value["applied"], value["unchanged"]
        )?;
    }
    if let Some(auto) = &auto {
        writeln!(out, "  {auto}")?;
    }
    for warning in value["warnings"].as_array().into_iter().flatten() {
        writeln!(out, "  Note: {}", shown(warning))?;
    }
    let disruptive = changes(value)
        .filter(|change| change["risk"] == "disruptive")
        .map(|change| {
            format!(
                "{}{} {} → {}",
                change["feature"].as_str().unwrap_or_default(),
                banks(change),
                shown(&change["current"]),
                shown(&change["saved"])
            )
        })
        .collect::<Vec<_>>();
    if !disruptive.is_empty() {
        writeln!(out, "  Disruptive: {}", disruptive.join(", "))?;
    }
    let not_applied = value["not_applied"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            format!(
                "{}{} ({})",
                row["feature"].as_str().unwrap_or_default(),
                banks(row),
                row["reason"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    if !not_applied.is_empty() {
        writeln!(out, "  Not applied: {}", not_applied.join(", "))?;
    }
    if value["restarted_stream"] == true {
        writeln!(out, "  The stream was stopped and restarted.")?;
    }
    // Undo lives in the session; a direct worker exits with this command.
    // It names the camera: the session's default camera may be another.
    if value.get("camera_change").is_some() && cli.session.is_some() {
        writeln!(
            out,
            "Undo with: capturefab {}undo",
            target(cli, value["camera"]["id"].as_str())
        )?;
    }
    Ok(())
}

pub(super) fn print_human(cli: &Cli, command: &SetupCommand, value: &Value) -> Result<()> {
    let mut out = io::stdout().lock();
    match command {
        SetupCommand::Save { .. } | SetupCommand::Import { .. } => {
            let setup = &value["setup"];
            writeln!(
                out,
                "{} setup {} with {} settings from {}.",
                if matches!(command, SetupCommand::Save { .. }) {
                    "Saved"
                } else {
                    "Imported"
                },
                setup["name"].as_str().unwrap_or_default(),
                setup["values"],
                camera_label(&setup["camera"])
            )?;
            let skipped = setup["skipped"].as_array().cloned().unwrap_or_default();
            if !skipped.is_empty() {
                writeln!(out, "Not saved ({}):", skipped.len())?;
                for row in skipped.iter().take(10) {
                    writeln!(
                        out,
                        "  {}{}: {}",
                        row["feature"].as_str().unwrap_or_default(),
                        banks(row),
                        row["reason"].as_str().unwrap_or_default()
                    )?;
                }
                if skipped.len() > 10 {
                    writeln!(out, "  and {} more (see --json)", skipped.len() - 10)?;
                }
            }
            writeln!(
                out,
                "Network, user-set and file settings are never saved. Restore with: capturefab {}setup apply {}",
                target(cli, value["camera"]["id"].as_str()),
                crate::hint::shell_quote(setup["name"].as_str().unwrap_or_default())
            )?;
        }
        SetupCommand::List => {
            let setups = value["setups"].as_array().cloned().unwrap_or_default();
            if setups.is_empty() {
                writeln!(
                    out,
                    "No saved setups. Save one with: capturefab --camera ID setup save NAME"
                )?;
            } else {
                let width = setups
                    .iter()
                    .map(|s| s["name"].as_str().unwrap_or_default().len())
                    .max()
                    .unwrap_or(4)
                    .max(4);
                writeln!(out, "{:width$}  SAVED       SETTINGS  CAMERA", "NAME")?;
                for setup in &setups {
                    let saved = setup["saved_at"].as_str().unwrap_or_default();
                    writeln!(
                        out,
                        "{:width$}  {:10}  {:>8}  {}{}",
                        setup["name"].as_str().unwrap_or_default(),
                        saved.get(..10).unwrap_or(saved),
                        setup["values"].to_string(),
                        camera_label(setup),
                        setup["note"]
                            .as_str()
                            .map(|note| format!(" · {note}"))
                            .unwrap_or_default()
                    )?;
                }
            }
            writeln!(out, "Folder: {}", shown(&value["dir"]))?;
        }
        SetupCommand::Show { .. } => {
            writeln!(
                out,
                "Setup {} · {} · saved {}",
                value["name"].as_str().unwrap_or_default(),
                camera_label(&value["camera"]),
                value["saved_at"].as_str().unwrap_or_default()
            )?;
            if let Some(note) = value["note"].as_str() {
                writeln!(out, "Note: {note}")?;
            }
            if let Some(balance) = value["auto"]["balance"].as_f64() {
                writeln!(out, "Auto exposure on, balance {balance}")?;
            }
            for row in value["values"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "  {}{} = {}",
                    row["feature"].as_str().unwrap_or_default(),
                    banks(row),
                    shown(&row["value"])
                )?;
            }
            let skipped = value["skipped"].as_array().cloned().unwrap_or_default();
            if !skipped.is_empty() {
                writeln!(out, "Not saved:")?;
                for row in &skipped {
                    writeln!(
                        out,
                        "  {}{}: {}",
                        row["feature"].as_str().unwrap_or_default(),
                        banks(row),
                        row["reason"].as_str().unwrap_or_default()
                    )?;
                }
            }
        }
        SetupCommand::Diff { .. } => write_diff(&mut out, value, "Would change")?,
        SetupCommand::Apply { dry_run: true, .. } => write_diff(&mut out, value, "Would change")?,
        SetupCommand::Apply { .. } => write_applied(&mut out, cli, value)?,
        SetupCommand::Delete { .. } => {
            writeln!(
                out,
                "Deleted setup {}. The camera was not changed.",
                value["removed"].as_str().unwrap_or_default()
            )?;
        }
    }
    Ok(())
}

/// The `setup` section of `capturefab schema`, its RPC examples, and the
/// note on how a restore writes.
pub(super) fn extend_schema(contract: &mut Value) {
    contract["setup"] = json!({
        "cli":"setup save NAME [--note TEXT] [--replace] | list | show NAME | diff NAME [--file PATH] [--allow-other-model] | apply NAME [--file PATH] [--dry-run] [--strict] [--allow-other-model] [--yes] | delete NAME | import FILE [--name NAME] [--replace]; save, diff and apply need --camera or --session",
        "purpose":"A setup is a named, host-side snapshot of one camera's writable configuration values. It is previewed as a diff against a connected camera and restored as one camera change with one undo token. Camera user sets are never read or written.",
        "store":{"dir":"setups/ in the state directory (setup list --json reports dir); one <lowercase name>.json per setup, private (0700 directory, 0600 files)","backup":"setup show NAME --json > FILE writes the document inside the --json envelope; setup import FILE and setup diff|apply --file FILE accept that file or the bare document (the envelope's result). An inline setup field (RPC, workflows) takes the bare document","names":"1-64 ASCII letters, digits, '.', '-' and '_'; not starting or ending with '.'; not a reserved Windows device name such as con or nul; unique ignoring case"},
        "limits":{"bytes":crate::setup::MAX_SETUP_BYTES,"values":crate::setup::MAX_SETUP_VALUES,"setups":crate::setup::MAX_SETUPS,"note_chars":crate::setup::MAX_NOTE,"string_bytes":crate::setup::MAX_STRING,"selectors_per_value":crate::setup::MAX_BANKS},
        "document":{"version":1,"name":"bench","saved_at":"2026-10-25T14:03:11Z","note":"Ring light at 60%","camera":{"transport":"simulator","vendor":"Capturefab","model":"Pattern camera","serial":"SIM0","kind":"area_scan"},"auto":null,"values":[{"feature":"PixelFormat","value":"Mono8"},{"feature":"Width","value":640},{"feature":"Gain","value":2.5,"at":[{"selector":"GainSelector","value":"All"}]},{"feature":"ExposureTime","value":10000.0}],"skipped":[{"feature":"BalanceRatio","reason":"set by BalanceWhiteAuto = Continuous"}]},
        "captured":"Every writable configuration value at the selected bank, and every bank of GainSelector, BlackLevelSelector, BalanceRatioSelector, TriggerSelector, LineSelector, UserOutputSelector, CounterSelector and TimerSelector, including values unavailable at the selected bank (an output line's source while an input line is selected). Never: transport, network, stream-channel (DeviceStreamChannel*), user-set, file, chunk and event settings, commands, registers, DeviceUserID, values a firmware auto drives, values Capturefab auto exposure manages (auto records its balance instead), stream state, capture settings, credentials or addresses. Banks that cannot be read are listed in skipped; banks that are read-only or not available are listed only when another bank of the value was saved. Media sources store only transport and model.",
        "match":{"same_camera":"same vendor, model and serial (hub cameras: same hub:NAME/CAMERA id); transport is ignored","same_model":"same vendor and model, another serial; applies with a warning","other_model":"apply is refused unless allow_other_model (CLI --allow-other-model); then only settings both cameras share change, and warnings names both models. CLI setup diff previews it anyway, with that note in warnings and a hint on stderr that its --allow-other-model leaves out","unknown_source":"a webcam, stream or file of the same kind; Capturefab cannot tell whether it is the same source; applies with a warning"},
        "diff":{"fields":["setup","digest","camera","match","warnings","changes[{feature,at?,saved,current,risk,stream_locked,deferred?,warning?}]","unchanged","matched[{feature,at?,value}]","not_applied[{feature,at?,status,saved,reason}]","risk","restarts_stream","blocked","auto_change{current,saved,action,risk}"],"deferred":"true for a value unavailable now (current null) that an earlier write in this setup should make available; it is written after the others, and moves to not_applied in the result if it stays unavailable","auto_change":"Capturefab auto exposure balance now and in the setup; action none, enter, balance or release; always reversible","stream_locked":"geometry and pixel format, or a value the camera locks (pIsLocked) while acquiring that no other change in this setup unlocks","status":["missing","read_only","invalid","skipped","refused"],"risk":"reversible when every change only affects the image (exposure, gain, white balance, ...) or selects another bank of a selector setups read bank by bank; disruptive otherwise, such as trigger and I/O line settings"},
        "apply":{"result":"the diff fields plus applied (values changed, including matched values written again after an earlier write moved them), auto_changed, restarted_stream, auto and camera_change","order":"geometry and pixel format first; then switches that disarm (TriggerMode Off, LineMode Input, *Enable false); other modes; other values; switches that arm or drive (TriggerMode On, LineMode Output, *Enable true) after their sources; auto flags; selectors last. Up to 4 passes retry values an earlier write unlocked, made available or moved, then every saved value, changed or matched, is verified","failure":"any value that does not take within 4 passes or 90 s restores every captured value and fails with \"Restoring setup NAME failed; every value was put back\", adding whether a stopped stream was restarted; nothing is recorded in history","stream":"a running stream is stopped once and started again when the setup changes Width, Height, PixelFormat, binning, decimation, offsets, a video mode, or a value the camera locks while acquiring; refused while recording, forwarding or scheduled captures run","strict":"refuses, changing nothing, when any value is missing, read-only, invalid or refused, or when a deferred value stays unavailable; CLI --dry-run --strict fails the same way (exit 5) for values the diff already lists","digest":"apply_setup with digest refuses a setup changed since that digest was reviewed","confirmation":"The CLI refuses disruptive changes without --yes (exit 2, naming the setup diff and setup apply --yes commands for the same session, camera and --file), and sends the reviewed disruptive changes as confirmed_disruptive [{feature, at}]; the worker compares again and refuses (exit 2), changing nothing, a disruptive change at a feature and bank outside that list. A bare feature name confirms only a value without banks. RPC, browser and workflow callers that omit confirmed_disruptive have no confirmation gate (as with set_many), but the model, kind and busy checks always apply."},
        "rpc":{"read_setup":"read-only, no arguments: the values a new setup would save","setup_diff":"read-only: {name} or {setup: DOCUMENT}, optional allow_other_model","apply_setup":"{name} or {setup: DOCUMENT}, optional digest, allow_other_model, strict, confirmed_disruptive: [FEATURE | {feature, at: [{selector, value}]}]"},
        "workflow":{"camera":"sim:0","command":{"op":"apply_setup","name":"bench"}},
        "exit_codes":{"missing setup":3,"strict refusal (CLI --dry-run --strict)":5,"other model (CLI apply)":2,"other model (CLI diff)":0,"other model (RPC)":5,"disruptive without --yes":2,"disruptive change not in confirmed_disruptive":2,"name taken, invalid name or document":2,"blocked by recording, forwarding or schedules":3,"strict refusal":5,"setup changed since review":1,"restore failed (rolled back)":1}
    });
    if let Some(commands) = contract["rpc"]["commands"].as_array_mut() {
        commands.extend([
            json!({"op":"read_setup"}),
            json!({"op":"setup_diff","name":"bench"}),
            json!({"op":"apply_setup","name":"bench","strict":true}),
        ]);
    }
    if let Some(writes) = contract["writes"].as_str() {
        contract["writes"] = json!(format!(
            "{writes} apply_setup restores up to {} values as one camera change with one undo token; a failure restores every value, and stream-locked changes restart the stream once and are refused while recording, forwarding or scheduled captures run.",
            crate::setup::MAX_SETUP_VALUES
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_example_is_a_valid_setup_and_its_commands_parse() {
        let mut contract = json!({"rpc":{"commands":[]},"writes":"Writes."});
        extend_schema(&mut contract);
        let document = serde_json::to_vec(&contract["setup"]["document"]).unwrap();
        crate::setup::parse(&document).unwrap();
        for command in contract["rpc"]["commands"].as_array().unwrap() {
            SessionCommand::from_json_slice(command.to_string().as_bytes())
                .unwrap()
                .validate()
                .unwrap();
        }
        let workflow = contract["setup"]["workflow"]["command"].to_string();
        assert!(contract["setup"]["workflow"]["camera"].is_string());
        SessionCommand::from_json_slice(workflow.as_bytes())
            .unwrap()
            .validate()
            .unwrap();
        assert!(contract["writes"].as_str().unwrap().contains("apply_setup"));
    }

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from([&["capturefab"], args].concat()).unwrap()
    }

    #[test]
    fn printed_commands_name_the_session_and_the_camera_used() {
        let session = cli(&["--session", "t", "setup", "apply", "bench"]);
        assert_eq!(
            target(&session, Some("sim:0")),
            "--session t --camera sim:0 "
        );
        let named = cli(&["--session", "t", "--camera", "sim:1", "setup", "save", "x"]);
        assert_eq!(target(&named, None), "--session t --camera sim:1 ");
        assert_eq!(target(&session, None), "--session t ");
        let direct = cli(&[
            "--camera",
            "rtsp://user:secret@cam/live",
            "setup",
            "save",
            "x",
        ]);
        let printed = target(&direct, Some("media:1234"));
        assert!(!printed.contains("secret"), "{printed}");
        assert!(printed.starts_with("--camera "), "{printed}");
        assert_eq!(target(&cli(&["setup", "list"]), None), "--camera ID ");
        let mut out = Vec::new();
        write_applied(
            &mut out,
            &session,
            &json!({"setup":"bench","camera":{"id":"sim:0"},"applied":1,"unchanged":2,"camera_change":{"label":"Restore setup bench"}}),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("Undo with: capturefab --session t --camera sim:0 undo"),
            "{text}"
        );
    }

    #[test]
    fn the_confirmation_names_the_file_camera_and_flags_it_was_given() {
        let file = cli(&[
            "--session",
            "t",
            "setup",
            "apply",
            "--file",
            "my setup.json",
            "--strict",
        ]);
        let message = confirm_message(
            &file,
            Some("sim:line:conveyor"),
            &["AcquisitionLineRate".into()],
            &None,
            &Some(PathBuf::from("my setup.json")),
            true,
            true,
        );
        assert!(
            message.contains(
                "AcquisitionLineRate, which is not a known image setting and may change triggering"
            ),
            "{message}"
        );
        assert!(!message.contains("triggers or signals"), "{message}");
        assert!(
            message.contains("capturefab --session t --camera sim:line:conveyor setup diff --file 'my setup.json',"),
            "{message}"
        );
        assert!(
            message.ends_with(
                "capturefab --session t --camera sim:line:conveyor setup apply --file 'my setup.json' --allow-other-model --strict --yes"
            ),
            "{message}"
        );
        let named = confirm_message(
            &cli(&["--camera", "sim:0", "setup", "apply", "bench"]),
            Some("sim:0"),
            &["TriggerMode".into(), "LineMode".into()],
            &Some("bench".into()),
            &None,
            false,
            false,
        );
        assert!(
            named.contains(
                "TriggerMode, LineMode, which are not known image settings and may change"
            ),
            "{named}"
        );
        assert!(
            named.contains("capturefab --camera sim:0 setup diff bench,"),
            "{named}"
        );
        assert!(named.ends_with("setup apply bench --yes"), "{named}");
    }

    #[test]
    fn software_auto_changes_show_in_previews_and_results() {
        let diff = json!({
            "setup":"bench","camera":{"id":"sim:0"},"changes":[],"unchanged":3,
            "auto_change":{"current":null,"saved":0.25,"action":"enter","risk":"reversible"},
        });
        let mut out = Vec::new();
        write_diff(&mut out, &diff, "Would change").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("Would change 1 on sim:0, 3 already match."),
            "{text}"
        );
        assert!(
            text.contains("  Auto exposure: off → on (balance 0.25)"),
            "{text}"
        );
        let mut out = Vec::new();
        let applied = json!({
            "setup":"bench","camera":{"id":"sim:0"},"applied":0,"unchanged":3,"auto_changed":true,
            "auto_change":{"current":0.5,"saved":null,"action":"release","risk":"reversible"},
        });
        write_applied(&mut out, &cli(&["setup", "apply", "bench"]), &applied).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with(
                "Restored bench on sim:0: only auto exposure changed, 3 already matched."
            ),
            "{text}"
        );
        assert!(
            text.contains("  Auto exposure: on → off (manual)"),
            "{text}"
        );
        assert_eq!(
            auto_row(&json!({"action":"balance","current":0.5,"saved":0.75})).as_deref(),
            Some("Auto exposure balance: 0.50 → 0.75")
        );
        assert_eq!(auto_row(&json!({"action":"none"})), None);
    }

    #[test]
    fn a_strict_preview_refuses_what_a_strict_restore_would() {
        let diff = json!({"setup":"s0x","not_applied":[
            {"feature":"Scan3dOutputMode","status":"missing","reason":"not on this camera"},
            {"feature":"BalanceRatio","status":"skipped","reason":"auto"},
        ]});
        let error = strict_refusal(&diff).unwrap();
        assert_eq!(super::super::error_code(&error), ("unsupported", 5));
        assert_eq!(
            error.to_string(),
            "Setup s0x cannot be applied strictly; nothing changed. Not applicable: Scan3dOutputMode (not on this camera)"
        );
        assert!(
            strict_refusal(
                &json!({"setup":"x","not_applied":[{"feature":"A","status":"skipped"}]})
            )
            .is_none()
        );
    }
}
