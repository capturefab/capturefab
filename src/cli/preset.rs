//! `capturefab preset`: save a tuned camera's settings by name, preview
//! them against a camera and restore them as one change. A preset lives on
//! this computer and works with any camera of the model; a camera's user
//! sets live in the camera (`--persist` writes one).
use super::{Client, Global, args::CameraArg, args::KeepArg, output::Report};
use crate::session::SessionCommand;
use crate::setup::{self as setups, Setup, Store};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum PresetCommand {
    /// Save the camera's current settings as a named preset
    #[command(
        after_help = "Examples:\n  capturefab preset save bench -c sim:0\n  capturefab preset save line1 -c gige:00c0df123456 --note 'ring light 60%'"
    )]
    Save {
        name: String,
        #[command(flatten)]
        camera: CameraArg,
        /// A short note, such as the lighting it was tuned for
        #[arg(long)]
        note: Option<String>,
        /// Overwrite a saved preset with the same name
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        keep: KeepArg,
    },
    /// List saved presets and the folder that holds them
    #[command(after_help = "Examples:\n  capturefab preset list\n  capturefab --json preset list")]
    List,
    /// Show a saved preset's values
    #[command(
        after_help = "Examples:\n  capturefab preset show bench\n  capturefab --json preset show bench"
    )]
    Show { name: String },
    /// Restore a preset as one camera change; a failure puts every value back
    #[command(
        after_help = "Examples:\n  capturefab preset apply bench -c sim:0 --dry-run\n  capturefab preset apply bench -c sim:0\n  capturefab preset apply --file bench.json -c sim:0 --strict\n  capturefab preset apply bench -c gige:00c0df123456 --persist UserSet1"
    )]
    Apply {
        #[arg(required_unless_present = "file")]
        name: Option<String>,
        #[command(flatten)]
        camera: CameraArg,
        /// Apply a preset document from a file instead of a saved preset
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Show what would change, disruptive changes included; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Fail, changing nothing, if any saved value cannot be applied to this camera
        #[arg(long)]
        strict: bool,
        /// Apply to a camera of another model; only settings both share change
        #[arg(long)]
        allow_other_model: bool,
        /// Then save it in the camera's user set (default UserSet1) and make that the power-on default
        #[arg(long, value_name = "USERSET", num_args = 0..=1, default_missing_value = "UserSet1")]
        persist: Option<String>,
        #[command(flatten)]
        keep: KeepArg,
    },
    /// Delete a saved preset; no camera changes
    #[command(
        after_help = "Examples:\n  capturefab preset rm bench\n  capturefab --json preset rm bench"
    )]
    Rm { name: String },
    /// Save a preset document from a file, such as one preset export wrote
    #[command(
        after_help = "Examples:\n  capturefab preset import bench.json\n  capturefab preset import bench.json --name bench2"
    )]
    Import {
        file: PathBuf,
        /// Save it under this name instead of the one in the file
        #[arg(long)]
        name: Option<String>,
        /// Overwrite a saved preset with the same name
        #[arg(long)]
        replace: bool,
    },
    /// Write a preset's document to a file, or to stdout
    #[command(
        after_help = "Examples:\n  capturefab preset export bench -o bench.json\n  capturefab preset export bench > bench.json"
    )]
    Export {
        name: String,
        /// The file to write; it must not exist
        #[arg(short = 'o', long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
}

/// Check names and notes before connecting to anything.
fn prepare(command: &PresetCommand) -> Result<()> {
    let usage = |result: Result<()>| -> Result<()> {
        result.map_err(|error| Usage(format!("{error:#}")).into())
    };
    match command {
        PresetCommand::Save { name, note, .. } => {
            usage(setups::validate_name(name))?;
            if let Some(note) = note {
                usage(setups::validate_note(note))?;
            }
        }
        PresetCommand::Show { name }
        | PresetCommand::Rm { name }
        | PresetCommand::Export { name, .. } => usage(setups::validate_name(name))?,
        PresetCommand::Apply { name, file, .. } => {
            if file.is_none()
                && let Some(name) = name
            {
                usage(setups::validate_name(name))?;
            }
        }
        PresetCommand::Import { name, .. } => {
            if let Some(name) = name {
                usage(setups::validate_name(name))?;
            }
        }
        PresetCommand::List => {}
    }
    Ok(())
}

/// A path made absolute against this terminal's directory.
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}

/// The worker's report says `setup`; results say `preset`.
fn renamed(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut()
        && let Some(name) = object.remove("setup")
    {
        object.insert("preset".into(), name);
    }
    value
}

pub fn run(global: &Global, command: PresetCommand) -> Result<Report> {
    prepare(&command)?;
    // Presets live in the state folder, looked for only by the commands
    // that use it: applying a file needs none.
    let store = Store::session;
    match command {
        PresetCommand::List => {
            let store = store()?;
            let value = json!({"dir": store.dir(), "presets": store.list()?});
            let text = list_text(&value);
            Ok(Report::json(value).text(text))
        }
        PresetCommand::Show { name } => {
            let value = serde_json::to_value(store()?.get(&name)?)?;
            let text = show_text(&value);
            Ok(Report::json(value).text(text))
        }
        PresetCommand::Rm { name } => {
            let removed = store()?.remove(&name)?;
            let text = format!("Deleted preset {}. No camera changed.\n", removed.name);
            Ok(Report::json(json!({"removed": removed.name})).text(text))
        }
        PresetCommand::Import {
            file,
            name,
            replace,
        } => {
            let mut setup = document_input(setups::read_file(&absolute(&file)?))?;
            if let Some(name) = name {
                setup.name = name;
            }
            let path = store()?.save(&setup, replace).map_err(taken)?;
            let value = summary(&setup, &path);
            let text = format!(
                "Imported preset {} with {} settings from {}.\n",
                setup.name,
                setup.values.len(),
                setup.camera.label()
            );
            Ok(Report::json(value).text(text))
        }
        PresetCommand::Export { name, output } => {
            let setup = store()?.get(&name)?;
            let document = serde_json::to_value(&setup)?;
            let Some(output) = output else {
                let text = serde_json::to_string_pretty(&document)? + "\n";
                return Ok(Report::json(document).text(text));
            };
            let path = absolute(&output)?;
            let mut bytes = serde_json::to_vec_pretty(&document)?;
            bytes.push(b'\n');
            crate::storage::save_one(&bytes, &path, None)?;
            let text = format!("Wrote preset {name} to {}.\n", path.display());
            Ok(Report::json(json!({"preset": name, "path": path})).text(text))
        }
        PresetCommand::Save {
            name,
            camera,
            note,
            replace,
            keep,
        } => {
            // Store::save checks again under its lock.
            let store = store()?;
            store.check_free(&name, replace).map_err(taken)?;
            let client = Client::open_with(global, camera.camera.as_deref(), keep.route())?;
            let reading = serde_json::from_value(client.call(SessionCommand::ReadSetup)?)
                .context("read the camera's settings")?;
            let setup = setups::from_read(&name, note, reading)?;
            let path = store.save(&setup, replace).map_err(taken)?;
            let mut value = summary(&setup, &path);
            let info = client.call(SessionCommand::Status)?;
            let id = info["cameras"][0]["info"]["id"]
                .as_str()
                .map(crate::media::redact_url);
            if let Some(id) = &id {
                value["camera"] = json!(id);
            }
            let text = save_text(&value, id.as_deref());
            Ok(Report::json(value).text(text))
        }
        PresetCommand::Apply {
            name,
            camera,
            file,
            dry_run,
            strict,
            allow_other_model,
            persist,
            keep,
        } => {
            let file = file.as_deref().map(absolute).transpose()?;
            let setup = load(&name, &file)?;
            if let Some(user_set) = &persist {
                crate::controls::Name::parse(user_set)?;
            }
            let client = Client::open_with(global, camera.camera.as_deref(), keep.route())?;
            // A camera that cannot keep the preset is refused before
            // anything changes.
            let persisting = persist
                .as_deref()
                .map(|user_set| check_persist(&client, user_set))
                .transpose()?;
            // The session gets the preset this command read, never just its
            // name: a session may keep its presets in another state folder
            // (CAPTUREFAB_STATE_DIR), so it would apply another preset of
            // that name, or none, than the one checked here.
            let mut diff = client.call(SessionCommand::SetupDiff {
                setup: Some(setup.clone()),
                name: None,
                allow_other_model: true,
            })?;
            if !allow_other_model && diff["match"] == "other_model" {
                return Err(Categorized::new(
                    ErrorKind::Usage,
                    format!(
                        "preset {} was saved from another model ({})",
                        setup.name,
                        setup.camera.label()
                    ),
                )
                .hint("pass --allow-other-model to apply the settings both models share")
                .into());
            }
            if strict && let Some(refusal) = strict_refusal(&diff) {
                return Err(refusal);
            }
            if dry_run {
                diff["dry_run"] = json!(true);
                if let Some(persisting) = persisting {
                    diff["persist"] = persisting;
                }
                let mut text = Vec::new();
                write_diff(&mut text, &diff, "Would change")?;
                return Ok(Report::json(renamed(diff)).text(String::from_utf8_lossy(&text)));
            }
            let mut result = client.call(SessionCommand::ApplySetup {
                setup: Some(setup),
                name: None,
                allow_other_model,
                strict,
            })?;
            if let Some(user_set) = persist {
                // The preset is on the camera now; say so beside the error.
                result["persisted"] = persist_user_set(&client, &user_set).map_err(|error| {
                    crate::types::with_details(
                        error,
                        json!({"applied": result["applied"], "changes": result["changes"]}),
                    )
                })?;
            }
            let mut text = Vec::new();
            write_applied(&mut text, &result)?;
            if let Some(user_set) = result["persisted"]["user_set"].as_str() {
                let _ = writeln!(
                    text,
                    "Saved in the camera as {user_set}, its power-on default."
                );
            }
            Ok(Report::json(renamed(result)).text(String::from_utf8_lossy(&text)))
        }
    }
}

/// Check that the camera can save its settings in `user_set` and start
/// with it: UserSetSave, a UserSetSelector entry and a power-on default.
/// What `--persist` will write.
fn check_persist(client: &Client, user_set: &str) -> Result<Value> {
    let list = client.call(SessionCommand::FeatureList {
        filter: crate::controls::FeatureFilter {
            pattern: Some("UserSet".into()),
            all: true,
            category: None,
        },
    })?;
    let rows = list["features"].as_array().cloned().unwrap_or_default();
    let row = |name: &str| rows.iter().find(|row| row["name"] == name);
    let choices = |row: &Value| -> Vec<String> {
        row["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|choice| choice.as_str().map(str::to_owned))
            .collect()
    };
    let unsupported = |what: String| -> anyhow::Error {
        Categorized::new(
            ErrorKind::Unsupported,
            format!("{what}, so --persist cannot keep the preset in it"),
        )
        .hint("apply without --persist; the preset stays saved on this computer")
        .into()
    };
    if row("UserSetSave").is_none() {
        return Err(unsupported("this camera has no UserSetSave".into()));
    }
    let Some(selector) = row("UserSetSelector") else {
        return Err(unsupported("this camera has no UserSetSelector".into()));
    };
    let sets = choices(selector);
    if !sets.iter().any(|set| set == user_set) {
        return Err(Categorized::new(
            ErrorKind::InvalidChoice,
            format!(
                "{user_set} is not one of this camera's user sets: {}",
                sets.join(", ")
            ),
        )
        .details(json!({"feature": "UserSetSelector", "value": user_set, "choices": sets}))
        .into());
    }
    let default = ["UserSetDefault", "UserSetDefaultSelector"]
        .into_iter()
        .find(|name| row(name).is_some_and(|row| choices(row).iter().any(|c| c == user_set)));
    match default {
        Some(default) => Ok(json!({"user_set": user_set, "default": default})),
        None => Err(unsupported(format!(
            "this camera cannot start with {user_set} (no UserSetDefault entry for it)"
        ))),
    }
}

/// Save the camera's settings in `user_set` and make it the power-on
/// default: UserSetDefault, or UserSetDefaultSelector on older cameras.
fn persist_user_set(client: &Client, user_set: &str) -> Result<Value> {
    let save = crate::controls::Name::parse(&format!("UserSetSave[UserSetSelector={user_set}]"))?;
    client.call(SessionCommand::Exec { name: save })?;
    let mut last = None;
    for feature in ["UserSetDefault", "UserSetDefaultSelector"] {
        let value = crate::controls::Assignment::parse(&format!("{feature}={user_set}"))?;
        match client.call(SessionCommand::SetValues {
            values: vec![value],
            dry_run: false,
        }) {
            Ok(_) => return Ok(json!({"user_set": user_set, "default": feature})),
            Err(error) if crate::types::classify(&error).kind == ErrorKind::FeatureNotFound => {
                last = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("no user set default"))
        .context(format!(
            "saved {user_set}, but cannot make it the power-on default"
        )))
}

fn list_text(value: &Value) -> String {
    let mut out = String::new();
    let presets = value["presets"].as_array().cloned().unwrap_or_default();
    if presets.is_empty() {
        out.push_str("No saved presets. Save one with: capturefab preset save NAME -c CAMERA\n");
    } else {
        let rows = presets
            .iter()
            .map(|preset| {
                let name = preset["name"].as_str().unwrap_or_default().to_owned();
                if let Some(status) = preset["status"].as_str() {
                    let note = if status == "newer_version" {
                        "saved by a newer Capturefab; update Capturefab to use it".to_owned()
                    } else {
                        format!(
                            "cannot be read: {}. Remove it with: capturefab preset rm {}",
                            preset["reason"].as_str().unwrap_or_default(),
                            crate::hint::shell_quote(&name)
                        )
                    };
                    return vec![name, "-".into(), "-".into(), note];
                }
                let saved = preset["saved_at"].as_str().unwrap_or_default();
                vec![
                    name,
                    saved.get(..10).unwrap_or(saved).to_owned(),
                    preset["values"].to_string(),
                    format!(
                        "{}{}",
                        camera_label(preset),
                        preset["note"]
                            .as_str()
                            .map(|note| format!(" · {note}"))
                            .unwrap_or_default()
                    ),
                ]
            })
            .collect();
        out.push_str(&super::camera::table(
            &["NAME", "SAVED", "SETTINGS", "CAMERA"],
            rows,
        ));
    }
    out.push_str(&format!("Folder: {}\n", shown(&value["dir"])));
    out
}

fn show_text(value: &Value) -> String {
    let mut out = format!(
        "Preset {} · {} · saved {}\n",
        value["name"].as_str().unwrap_or_default(),
        camera_label(&value["camera"]),
        value["saved_at"].as_str().unwrap_or_default()
    );
    if let Some(note) = value["note"].as_str() {
        out.push_str(&format!("Note: {note}\n"));
    }
    if value["auto"].is_object() {
        out.push_str("Auto exposure on\n");
    }
    for row in value["values"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {}{} = {}\n",
            row["feature"].as_str().unwrap_or_default(),
            banks(row),
            shown(&row["value"])
        ));
    }
    let skipped = value["skipped"].as_array().cloned().unwrap_or_default();
    if !skipped.is_empty() {
        out.push_str("Not saved:\n");
        for row in &skipped {
            out.push_str(&format!(
                "  {}{}: {}\n",
                row["feature"].as_str().unwrap_or_default(),
                banks(row),
                row["reason"].as_str().unwrap_or_default()
            ));
        }
    }
    out
}

fn save_text(value: &Value, camera: Option<&str>) -> String {
    let preset = &value["preset"];
    let name = preset["name"].as_str().unwrap_or_default();
    let mut out = format!(
        "Saved preset {name} with {} settings from {}.\n",
        preset["values"],
        camera_label(&preset["camera"])
    );
    let skipped = preset["skipped"].as_array().cloned().unwrap_or_default();
    if !skipped.is_empty() {
        out.push_str(&format!("Not saved ({}):\n", skipped.len()));
        for row in skipped.iter().take(10) {
            out.push_str(&format!(
                "  {}{}: {}\n",
                row["feature"].as_str().unwrap_or_default(),
                banks(row),
                row["reason"].as_str().unwrap_or_default()
            ));
        }
        if skipped.len() > 10 {
            out.push_str(&format!("  and {} more (see --json)\n", skipped.len() - 10));
        }
    }
    out.push_str(&format!(
        "Network, user-set and file settings are never saved. Restore with: capturefab preset apply {} -c {}\n",
        crate::hint::shell_quote(name),
        crate::hint::shell_quote(camera.unwrap_or("CAMERA"))
    ));
    out
}

/// Result fields, for `schema`.
pub const APPLY_FIELDS: &[(&str, &str)] = &[
    ("preset", "string — the preset's name"),
    ("camera", "{id,vendor,model,serial} — the camera compared"),
    ("match", "same_camera|same_model|other_model|unknown_source"),
    (
        "changes",
        "[{feature,at?,saved,current,risk:reversible|disruptive,stream_locked,warning?}] — disruptive changes may change triggering, I/O lines or acquisition rate",
    ),
    ("unchanged", "int — values that already match"),
    ("matched", "[{feature,at?,value}]"),
    ("not_applied", "[{feature,at?,status,saved,reason}]"),
    ("risk", "reversible|disruptive — the whole change"),
    (
        "restarts_stream",
        "bool — a running stream stops and starts once",
    ),
    (
        "auto_change",
        "{current:bool,saved:bool,action:enter|release|none,risk} — Capturefab auto mode now and in the preset",
    ),
    ("warnings", "[string]"),
    ("dry_run", "true — with --dry-run: nothing was written"),
    ("applied", "int — values written"),
    ("auto_changed", "bool"),
    ("restarted_stream", "bool"),
    ("persisted", "{user_set,default} — with --persist"),
    (
        "persist",
        "{user_set,default} — with --dry-run --persist: the user set it would save",
    ),
];

/// A setup file given on the command line: a newer document is
/// unsupported, anything else wrong with it is usage.
fn document_input<T>(result: Result<T>) -> Result<T> {
    match result {
        Err(error)
            if error
                .chain()
                .any(|cause| cause.is::<setups::NewerVersion>()) =>
        {
            Err(crate::types::Categorized::error(
                crate::types::ErrorKind::Unsupported,
                format!("{error:#}"),
            ))
        }
        result => result.map_err(|error| Usage(format!("{error:#}")).into()),
    }
}

fn taken(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<setups::NameTaken>() {
        Some(taken) => {
            crate::types::Categorized::new(crate::types::ErrorKind::Exists, taken.to_string())
                .hint("pass --replace to overwrite it")
                .details(json!({"name": taken.name, "path": taken.path}))
                .into()
        }
        None => error,
    }
}

fn summary(setup: &Setup, path: &Path) -> Value {
    json!({"preset":{
        "name":setup.name,
        "camera":setup.camera,
        "values":setup.values.len(),
        "skipped":setup.skipped,
        "saved_at":setup.saved_at,
        "path":path,
    }})
}

/// The setup an apply names: a saved one, from this command's state
/// folder, or a file.
fn load(name: &Option<String>, file: &Option<PathBuf>) -> Result<Setup> {
    match file {
        Some(file) => document_input(setups::read_file(file)),
        None => {
            let name = name.as_deref().context("name a preset or pass --file")?;
            Store::session()?.get(name)
        }
    }
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
                "Preset {} cannot be applied strictly; nothing changed. Not applicable: {}",
                diff["setup"].as_str().unwrap_or_default(),
                failing.join(", ")
            ),
        )
    })
}

/// Software auto exposure's change in a diff, as people read it.
fn auto_row(auto: &Value) -> Option<String> {
    match auto["action"].as_str()? {
        "enter" => Some("Auto exposure: off → on".into()),
        "release" => Some("Auto exposure: on → off (manual)".into()),
        _ => None,
    }
}

fn changes(diff: &Value) -> impl Iterator<Item = &Value> {
    diff["changes"].as_array().into_iter().flatten()
}

/// A value as people read it; whole floats without ".0".
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
        let setup = diff["setup"].as_str().unwrap_or("this preset");
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
            "  ! not a known image setting: may change triggering, I/O lines or acquisition rate"
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

/// A restore's result: what changed and what did not apply.
fn write_applied(out: &mut impl Write, value: &Value) -> Result<()> {
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
        writeln!(out, "  Not known image settings: {}", disruptive.join(", "))?;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_and_results_read_as_prose() {
        let mut out = Vec::new();
        write_applied(
            &mut out,
            &json!({"setup":"bench","camera":{"id":"sim:0"},"applied":1,"unchanged":2}),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Restored bench on sim:0: 1 changed, 2 already matched.\n"
        );
        let value = json!({"setup":"line","camera":{"id":"sim:line:0"},"applied":1,"unchanged":0,
            "changes":[{"feature":"AcquisitionLineRate","current":700,"saved":480,"risk":"disruptive"}]});
        let mut out = Vec::new();
        write_diff(&mut out, &value, "Would change").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("! AcquisitionLineRate: 700 → 480"), "{text}");
        assert!(
            text.contains("may change triggering, I/O lines or acquisition rate"),
            "{text}"
        );
        let diff = json!({
            "setup":"bench","camera":{"id":"sim:0"},"changes":[],"unchanged":3,
            "auto_change":{"current":false,"saved":true,"action":"enter","risk":"reversible"},
        });
        let mut out = Vec::new();
        write_diff(&mut out, &diff, "Would change").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("Would change 1 on sim:0, 3 already match."),
            "{text}"
        );
        assert_eq!(
            auto_row(&json!({"action":"release","current":true,"saved":false})).as_deref(),
            Some("Auto exposure: on → off (manual)")
        );
        assert_eq!(
            renamed(json!({"setup":"x","a":1})),
            json!({"preset":"x","a":1})
        );
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
            "Preset s0x cannot be applied strictly; nothing changed. Not applicable: Scan3dOutputMode (not on this camera)"
        );
        assert!(
            strict_refusal(
                &json!({"setup":"x","not_applied":[{"feature":"A","status":"skipped"}]})
            )
            .is_none()
        );
    }
}
