//! Camera commands that read or configure: list, info, features, get, set,
//! exec.
#![forbid(unsafe_code)]
use super::{Client, Global, args::CameraArg, args::CamerasArg, args::KeepArg, output::Report};
use crate::controls::{Assignment, FeatureFilter, Name};
use crate::session::SessionCommand;
use crate::types::CameraInfo;
use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::fmt::Write as _;
mod action_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Kind {
    Gige,
    U3v,
    Gentl,
    Webcam,
    Onvif,
    Screen,
    Sim,
}

#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    /// Only these kinds (comma-separated); screen captures and sim:0 are listed only when named
    #[arg(long, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<Kind>,
    /// How long `GigE` and ONVIF broadcasts collect answers
    #[arg(help = "How long GigE and ONVIF broadcasts collect answers", long, default_value = "1s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: std::time::Duration,
}

/// One `list` row.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Row {
    pub id: String,
    pub kind: &'static str,
    pub vendor: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// The session that holds it open: `{session, idle_releases_in_s?}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_by: Option<Value>,
}

/// The `list` result.
#[derive(Debug, Serialize)]
pub(super) struct ListResult {
    pub cameras: Vec<Row>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
impl ListResult {
    pub(super) const FIELDS: &[(&str, &str)] = &[
        (
            "cameras",
            "[{id,kind:gige|u3v|gentl|webcam|screen|onvif|stream|file|sim,vendor,model,serial,address,held_by?:{session,idle_releases_in_s?}}] — pass id to -c; held_by: a session keeps it open, the auto session for idle_releases_in_s more seconds unless used",
        ),
        (
            "warnings",
            "[string] — transports that could not be searched",
        ),
    ];
}

pub(super) fn row(info: &CameraInfo) -> Row {
    let media = info.transport == crate::types::Transport::Media;
    Row {
        id: crate::media::redact_url(&info.id),
        kind: crate::camera::kind(info),
        vendor: info.vendor.clone(),
        model: info.model.clone(),
        serial: (!media && !info.serial.is_empty() && !info.id.ends_with(&info.serial))
            .then(|| info.serial.clone()),
        address: info
            .address
            .as_ref()
            .filter(|address| **address != info.id && !media)
            .map(|address| crate::media::redact_url(address)),
        held_by: None,
    }
}

/// Say which session holds each of `rows` open, and when it lets go of it
/// unused; the sessions' descriptors list what they hold.
fn held(rows: &mut [Row]) {
    let sessions = crate::ipc::sessions();
    for row in rows.iter_mut() {
        let Some((session, camera)) = sessions.iter().find_map(|session| {
            let camera = session
                .cameras
                .iter()
                .find(|camera| camera.named_by(&row.id).is_some())?;
            Some((session, camera))
        }) else {
            continue;
        };
        let idle = crate::ipc::idle_left(&session.name, &camera.id);
        let mut held_by = json!({"session": session.name});
        if let Some(idle) = idle {
            held_by["idle_releases_in_s"] = json!(idle.as_millis().div_ceil(1000));
        }
        row.held_by = Some(held_by);
    }
}

pub(super) fn list(global: &Global, args: &ListArgs) -> Result<Report> {
    let named = |kind| args.kind.contains(&kind);
    let all = args.kind.is_empty();
    let kinds = crate::camera::Kinds {
        gige: all || named(Kind::Gige),
        u3v: all || named(Kind::U3v),
        gentl: all || named(Kind::Gentl),
        host: all || named(Kind::Webcam) || named(Kind::Screen),
        onvif: all || named(Kind::Onvif),
        sim: named(Kind::Sim),
    };
    let searching = super::term::activity("searching for cameras…");
    let (devices, warnings) = crate::camera::discover_kinds(args.timeout, kinds);
    drop(searching);
    let wanted = |kind: &str| match kind {
        "screen" => named(Kind::Screen),
        "sim" => named(Kind::Sim),
        "webcam" => all || named(Kind::Webcam),
        "gige" => all || named(Kind::Gige),
        "u3v" => all || named(Kind::U3v),
        "gentl" => all || named(Kind::Gentl),
        "onvif" => all || named(Kind::Onvif),
        _ => all,
    };
    let mut rows = devices
        .iter()
        .map(row)
        .filter(|row| wanted(row.kind))
        .collect::<Vec<_>>();
    held(&mut rows);
    let result = ListResult {
        cameras: rows,
        warnings: warnings
            .into_iter()
            .map(|warning| crate::scrub::redact_urls(&warning))
            .collect(),
    };
    let mut text = String::new();
    if result.cameras.is_empty() {
        text.push_str(
            "No cameras found. Try `capturefab list --kind sim` for the simulator, or `capturefab network` and `capturefab doctor` for GigE.\n",
        );
    } else {
        text.push_str(&table(
            &["ID", "KIND", "MODEL", "SERIAL", "ADDRESS", "HELD BY"],
            &result
                .cameras
                .iter()
                .map(|row| {
                    vec![
                        row.id.clone(),
                        row.kind.into(),
                        row.model.clone(),
                        row.serial.clone().unwrap_or_default(),
                        row.address.clone().unwrap_or_default(),
                        row.held_by
                            .as_ref()
                            .and_then(|held| held["session"].as_str())
                            .map(|session| format!("session {session}"))
                            .unwrap_or_default(),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
    }
    if !global.json {
        for warning in &result.warnings {
            global.output().note(format_args!("capturefab: {warning}"));
        }
    }
    Ok(Report::json(serde_json::to_value(&result)?).text(text))
}

/// Columns sized to their cells; a column empty in every row is left out.
pub(super) fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = (0..headers.len())
        .filter(|&column| column == 0 || rows.iter().any(|row| !row[column].is_empty()))
        .map(|column| {
            let width = rows
                .iter()
                .map(|row| row[column].chars().count())
                .chain([headers[column].len()])
                .max()
                .unwrap_or(0);
            (column, width)
        })
        .collect::<Vec<_>>();
    #[allow(
        single_use_lifetimes,
        reason = "Rust 1.89 requires a named lifetime for references in impl Trait"
    )]
    fn line<'a>(
        text: &mut String,
        columns: &[(usize, usize)],
        cells: impl IntoIterator<Item = &'a str>,
    ) {
        let start = text.len();
        let mut cells = cells.into_iter().enumerate();
        for (position, &(column, width)) in columns.iter().enumerate() {
            let (_, cell) = cells
                .find(|(index, _)| *index == column)
                .expect("table rows match their headers");
            text.push_str(cell);
            if position + 1 < columns.len() {
                text.extend(std::iter::repeat_n(' ', width - cell.chars().count() + 2));
            }
        }
        let length = text
            .get(start..)
            .expect("appended table cells start on a character boundary")
            .trim_end()
            .len();
        text.truncate(start + length);
        text.push('\n');
    }
    let mut text = String::new();
    line(&mut text, &columns, headers.iter().copied());
    for row in rows {
        line(&mut text, &columns, row.iter().map(String::as_str));
    }
    text
}

#[derive(Debug, Args)]
pub(crate) struct InfoArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    #[command(flatten)]
    pub keep: KeepArg,
}

pub(super) fn info(global: &Global, args: &InfoArgs) -> Result<Report> {
    let client = Client::open_with(global, args.camera.camera.as_deref(), args.keep.route())?;
    let value = client.call(SessionCommand::Info)?;
    let text = info_text(&value);
    Ok(Report::json(value).text(text))
}

fn shown(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => Cow::Borrowed(""),
        Value::String(text) => Cow::Borrowed(text),
        Value::Number(number) => {
            let mut text = number.to_string();
            if text.ends_with(".0") {
                text.truncate(text.len() - 2);
            }
            Cow::Owned(text)
        }
        other => Cow::Owned(other.to_string()),
    }
}

fn info_text(value: &Value) -> String {
    let get = |key: &str| value.get(key).map(shown).unwrap_or_default();
    let mut text = format!(
        "{}  {} {} ({})\n",
        get("camera"),
        get("vendor"),
        get("model"),
        get("kind")
    );
    if value.get("serial").is_some() {
        let _ = writeln!(text, "  serial      {}", get("serial"));
    }
    if value.get("camera_access").is_some() {
        let _ = writeln!(
            text,
            "  access      {} (acquisition: {})",
            get("camera_access"),
            get("acquisition_control")
        );
    }
    if value.get("width").is_some() {
        let _ = writeln!(
            text,
            "  image       {}x{} {} ({} bits)",
            get("width"),
            get("height"),
            get("pixel_format"),
            get("bits")
        );
    }
    if value.get("exposure_us").is_some() {
        let _ = writeln!(text, "  exposure    {} us", get("exposure_us"));
    }
    if value.get("gain_db").is_some() {
        let _ = writeln!(text, "  gain        {} dB", get("gain_db"));
    } else if value.get("gain").is_some() {
        let _ = writeln!(text, "  gain        {} {}", get("gain"), get("gain_unit"));
    }
    if value.get("fps").is_some() || value.get("fps_limit").is_some() {
        let _ = write!(
            text,
            "  fps         {}",
            value.get("fps").map_or_else(|| "?".into(), shown)
        );
        if let Some(limit) = value.get("fps_limit") {
            let _ = write!(text, " (limit {})", shown(limit));
        }
        text.push('\n');
    }
    if let Some(trigger) = value.get("trigger") {
        let _ = write!(
            text,
            "  trigger     {}",
            trigger["mode"].as_str().unwrap_or("Off")
        );
        if let Some(source) = trigger["source"].as_str() {
            let _ = write!(text, " from {source}");
        }
        if let Some(activation) = trigger["activation"]
            .as_str()
            .filter(|_| trigger["mode"] == "On")
        {
            let _ = write!(text, " on {activation}");
        }
        text.push('\n');
    }
    if let Some(controls) = value["controls"].as_object().filter(|c| !c.is_empty()) {
        text.push_str("  controls    ");
        for (index, (name, control)) in controls.iter().enumerate() {
            if index > 0 {
                text.push(' ');
            }
            let _ = write!(
                text,
                "{name}={}",
                control.get("value").map_or_else(|| "?".into(), shown)
            );
        }
        text.push('\n');
    }
    text
}

#[derive(Debug, Args)]
pub(crate) struct FeaturesArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// Only names containing this text, or matching a glob such as 'Exposure*'
    #[arg(value_name = "PATTERN")]
    pub pattern: Option<String>,
    /// Every visibility level (Expert, Guru); a PATTERN or --category also searches every level
    #[arg(long)]
    pub all: bool,
    /// Only this category, such as `ImageFormatControl`
    #[arg(
        help = "Only this category, such as ImageFormatControl",
        long,
        value_name = "NAME"
    )]
    pub category: Option<String>,
    #[command(flatten)]
    pub keep: KeepArg,
}

pub(super) fn features(global: &Global, args: FeaturesArgs) -> Result<Report> {
    let filter = FeatureFilter {
        pattern: args.pattern,
        all: args.all,
        category: args.category,
    };
    filter.validate()?;
    let client = Client::open_with(global, args.camera.camera.as_deref(), args.keep.route())?;
    let value = client.call(SessionCommand::FeatureList { filter })?;
    let rows = value["features"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            let range = match (row.get("min"), row.get("max")) {
                (Some(min), Some(max)) => format!("{}..{}", shown(min), shown(max)),
                _ => row["choices"]
                    .as_array()
                    .map(|choices| choices.iter().map(shown).collect::<Vec<_>>().join("|"))
                    .unwrap_or_default(),
            };
            vec![
                row["name"].as_str().unwrap_or_default().to_owned(),
                row.get("value")
                    .map(|value| shown(value).into_owned())
                    .unwrap_or_default(),
                row.get("unit")
                    .map(|value| shown(value).into_owned())
                    .unwrap_or_default(),
                row["access"].as_str().unwrap_or_default().to_owned(),
                range,
                if row["writable"] == false {
                    "read-only monitor; writes disabled".to_owned()
                } else {
                    row.get("locked_by")
                        .map(|owner| format!("locked by {}", shown(owner)))
                        .unwrap_or_default()
                },
            ]
        })
        .collect::<Vec<_>>();
    let mut text = if rows.is_empty() {
        "No features match.\n".to_owned()
    } else {
        table(&["NAME", "VALUE", "UNIT", "ACCESS", "RANGE", "NOTE"], &rows)
    };
    if let Some(truncated) = value.get("truncated") {
        let _ = writeln!(
            text,
            "{truncated} more not shown; narrow with a PATTERN or --category"
        );
    }
    Ok(Report::json(value).text(text))
}

#[derive(Debug, Args)]
pub(crate) struct GetArgs {
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// Features, banks such as `TriggerMode[FrameStart]`, or canonical controls.
    #[arg(
        required = true,
        value_name = "NAME",
        help = "Features, banks (TriggerMode[FrameStart]) or controls (exposure, gain, fps, roi, format, trigger, auto_exposure, auto_wb)"
    )]
    pub names: Vec<String>,
    #[command(flatten)]
    pub keep: KeepArg,
}

pub(super) fn get(global: &Global, args: &GetArgs) -> Result<Report> {
    let names = args
        .names
        .iter()
        .map(|name| Name::parse(name))
        .collect::<Result<Vec<_>>>()?;
    let read = |camera: Option<&str>| -> Result<Value> {
        let client = Client::open_with(global, camera, args.keep.route())?;
        client.call(SessionCommand::GetValues {
            names: names.clone(),
        })
    };
    let lines = |value: &Value| -> String {
        let mut text = String::new();
        if let Some(values) = value["values"].as_object() {
            for (name, value) in values {
                let _ = writeln!(text, "{name} = {}", shown(value));
            }
        }
        text
    };
    if args.cameras.cameras.len() <= 1 {
        let value = read(args.cameras.cameras.first().map(String::as_str))?;
        let text = lines(&value);
        return Ok(Report::json(value).text(text));
    }
    let mut cameras = Vec::new();
    let mut text = String::new();
    for camera in &args.cameras.cameras {
        let value = match read(Some(camera)) {
            Ok(value) => value,
            // The cameras already read go with the error.
            Err(error) if !cameras.is_empty() => {
                return Err(crate::types::with_details(
                    error,
                    json!({ "cameras": cameras }),
                ));
            }
            Err(error) => return Err(error),
        };
        let _ = writeln!(text, "{}:", value["camera"].as_str().unwrap_or(camera));
        for line in lines(&value).lines() {
            let _ = writeln!(text, "  {line}");
        }
        cameras.push(value);
    }
    Ok(Report::json(json!({ "cameras": cameras })).text(text))
}

#[derive(Debug, Args)]
pub(crate) struct SetArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// `NAME=VALUE` assignments for features, banks and canonical controls.
    #[arg(
        required = true,
        value_name = "NAME=VALUE",
        help = "NAME=VALUE: a feature (ExposureTime=10ms), a bank (TriggerMode[FrameStart]=On) or a control (exposure=5ms, roi=0,0,640,480)"
    )]
    pub values: Vec<String>,
    /// Check every value against the camera and show the writes in order; change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Assign only `ActionDeviceKey` without readback or rollback in an existing session.
    #[arg(long)]
    pub write_only: bool,
    /// Bound the WO key operation, including ordering and worker queue waits.
    #[arg(long, requires = "write_only", value_name = "DUR", value_parser = super::args::duration)]
    pub write_timeout: Option<std::time::Duration>,
    /// Populated from clap `ValueSource` by the root CLI parser.
    #[arg(skip)]
    pub camera_explicit: bool,
    #[command(flatten)]
    pub keep: KeepArg,
}

pub(super) const ACTION_KEY_FIELDS: &[(&str, &str)] = &[
    ("camera", "string — exact owned camera binding"),
    ("feature", "ActionDeviceKey"),
    (
        "write_completed",
        "true — reported completion, without readable verification",
    ),
    ("completion", "native_transport_ack|producer_reported"),
    ("readback", "unavailable_write_only"),
    ("rollback", "not_available"),
    (
        "scope",
        "current_control_epoch — not persisted or reapplied after reconnect",
    ),
];

pub(super) fn set(global: &Global, args: &SetArgs) -> Result<Report> {
    if args.write_only {
        return action_key::set(global, args);
    }
    let values = args
        .values
        .iter()
        .map(|value| Assignment::parse(value))
        .collect::<Result<Vec<_>>>()?;
    let client = Client::open_with(global, args.camera.camera.as_deref(), args.keep.route())?;
    let value = client.call(SessionCommand::SetValues {
        values,
        dry_run: args.dry_run,
    })?;
    let rows = value
        .get(if args.dry_run { "plan" } else { "changed" })
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let camera = value["camera"].as_str().unwrap_or("the camera");
    let mut text = if rows.is_empty() {
        format!("Nothing to change on {camera}.\n")
    } else if args.dry_run {
        format!("Would change on {camera}, in this order:\n")
    } else {
        String::new()
    };
    for row in &rows {
        let _ = writeln!(
            text,
            "{}{}: {} → {}",
            if args.dry_run { "  " } else { "" },
            row["name"].as_str().unwrap_or_default(),
            row.get("from").map_or_else(|| "?".into(), shown),
            row.get("to").map_or_else(|| "?".into(), shown)
        );
    }
    for warning in value["warnings"].as_array().into_iter().flatten() {
        let _ = writeln!(text, "Note: {}", shown(warning));
    }
    Ok(Report::json(value).text(text))
}

#[derive(Debug, Args)]
pub(crate) struct ExecArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// A command feature, optionally at a bank.
    #[arg(
        value_name = "COMMAND",
        help = "A command feature, optionally at a bank: TriggerSoftware, UserSetSave[UserSet1]"
    )]
    pub command: String,
    #[command(flatten)]
    pub keep: KeepArg,
}

pub(super) fn exec(global: &Global, args: &ExecArgs) -> Result<Report> {
    let name = Name::parse(&args.command)?;
    let client = Client::open_with(global, args.camera.camera.as_deref(), args.keep.route())?;
    let value = client.call(SessionCommand::Exec { name })?;
    let text = format!(
        "Ran {} on {}.\n",
        value["executed"].as_str().unwrap_or(&args.command),
        value["camera"].as_str().unwrap_or("the camera")
    );
    Ok(Report::json(value).text(text))
}

/// Result fields of these commands, for `schema`.
pub(super) const GET_FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    (
        "values",
        "{name: value} — keys as named, with bank shorthand (TriggerMode[FrameStart]) where unambiguous; controls in the form set takes (exposure: \"10ms\")",
    ),
    ("cameras", "[{camera,values}] — with -c repeated"),
];
pub(super) const EXEC_FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    ("executed", "string — the command run, with its bank"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_text_preserves_optional_fields_and_spacing() {
        let value = json!({
            "camera": "sim:0", "vendor": "Capturefab", "model": "Test", "kind": "sim",
            "fps": 25.0, "fps_limit": 30.0,
            "trigger": {"mode": "On", "source": "Line0", "activation": "RisingEdge"},
            "controls": {"exposure": {"value": "5ms"}, "gain": {"value": 2.0}, "other": {}}
        });
        assert_eq!(
            info_text(&value),
            "sim:0  Capturefab Test (sim)\n  fps         25 (limit 30)\n  trigger     On from Line0 on RisingEdge\n  controls    exposure=5ms gain=2 other=?\n"
        );
        for (fields, expected) in [
            (json!({"fps_limit": 30}), "  fps         ? (limit 30)\n"),
            (json!({"fps": null}), "  fps         \n"),
            (json!({"trigger": null}), "  trigger     Off\n"),
            (
                json!({"trigger": {"mode": "Off", "source": "Software", "activation": "RisingEdge"}}),
                "  trigger     Off from Software\n",
            ),
            (json!({"controls": {}}), ""),
            (
                json!({"controls": {"gain": {"value": null}}}),
                "  controls    gain=\n",
            ),
        ] {
            assert_eq!(info_text(&fields), format!("    ()\n{expected}"));
        }
    }

    #[test]
    fn tables_keep_blank_rows_and_trim_only_the_current_row() {
        let rows = [
            vec!["one".into(), String::new(), "x   ".into()],
            vec![String::new(), String::new(), String::new()],
            vec!["two".into(), String::new(), String::new()],
            vec!["é".into(), String::new(), "v".into()],
        ];
        assert_eq!(
            table(&["ID", "HIDDEN", "VALUE"], &rows),
            "ID   VALUE\none  x\n\ntwo\né    v\n"
        );
        assert_eq!(table(&["ID", "HIDDEN"], &[]), "ID\n");
    }

    #[test]
    fn tables_align_and_drop_empty_columns() {
        let text = table(
            &["ID", "KIND", "SERIAL"],
            &[
                vec!["sim:0".into(), "sim".into(), String::new()],
                vec!["gige:00c0df".into(), "gige".into(), String::new()],
            ],
        );
        assert_eq!(
            text,
            "ID           KIND\nsim:0        sim\ngige:00c0df  gige\n"
        );
    }
}
