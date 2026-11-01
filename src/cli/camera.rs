//! Camera commands that read or configure: list, info, features, get, set,
//! exec.
use super::{Client, Global, args::CameraArg, args::CamerasArg, output::Report};
use crate::controls::{Assignment, FeatureFilter, Name};
use crate::session::SessionCommand;
use crate::types::CameraInfo;
use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    Gige,
    U3v,
    Webcam,
    Onvif,
    Screen,
    Sim,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Only these kinds (comma-separated); screen captures and sim:0 are listed only when named
    #[arg(long, value_delimiter = ',', value_name = "KIND")]
    pub kind: Vec<Kind>,
    /// How long GigE and ONVIF broadcasts collect answers
    #[arg(long, default_value = "1s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: std::time::Duration,
}

/// One `list` row.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub id: String,
    pub kind: &'static str,
    pub vendor: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// The `list` result.
#[derive(Debug, Serialize)]
pub struct ListResult {
    pub cameras: Vec<Row>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
impl ListResult {
    pub const FIELDS: &[(&str, &str)] = &[
        (
            "cameras",
            "[{id,kind:gige|u3v|webcam|screen|onvif|stream|file|sim,vendor,model,serial,address}] — pass id to -c",
        ),
        (
            "warnings",
            "[string] — transports that could not be searched",
        ),
    ];
}

pub fn row(info: &CameraInfo) -> Row {
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
    }
}

pub fn list(global: &Global, args: ListArgs) -> Result<Report> {
    let named = |kind| args.kind.contains(&kind);
    let all = args.kind.is_empty();
    let kinds = crate::camera::Kinds {
        gige: all || named(Kind::Gige),
        u3v: all || named(Kind::U3v),
        host: all || named(Kind::Webcam) || named(Kind::Screen),
        onvif: all || named(Kind::Onvif),
        sim: named(Kind::Sim),
    };
    let (devices, warnings) = crate::camera::discover_kinds(args.timeout, kinds);
    let wanted = |kind: &str| match kind {
        "screen" => named(Kind::Screen),
        "sim" => named(Kind::Sim),
        "webcam" => all || named(Kind::Webcam),
        "gige" => all || named(Kind::Gige),
        "u3v" => all || named(Kind::U3v),
        "onvif" => all || named(Kind::Onvif),
        _ => all,
    };
    let rows = devices
        .iter()
        .map(row)
        .filter(|row| wanted(row.kind))
        .collect::<Vec<_>>();
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
            &["ID", "KIND", "MODEL", "SERIAL", "ADDRESS"],
            result
                .cameras
                .iter()
                .map(|row| {
                    vec![
                        row.id.clone(),
                        row.kind.into(),
                        row.model.clone(),
                        row.serial.clone().unwrap_or_default(),
                        row.address.clone().unwrap_or_default(),
                    ]
                })
                .collect(),
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
pub(super) fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let shown = (0..headers.len())
        .filter(|&column| column == 0 || rows.iter().any(|row| !row[column].is_empty()))
        .collect::<Vec<_>>();
    let width = |column: usize| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .chain([headers[column].len()])
            .max()
            .unwrap_or(0)
    };
    let widths = shown
        .iter()
        .map(|&column| width(column))
        .collect::<Vec<_>>();
    let line = |cells: Vec<&str>| {
        let mut line = String::new();
        for (position, &column) in shown.iter().enumerate() {
            let cell = cells[column];
            line.push_str(cell);
            if position + 1 < shown.len() {
                line.push_str(&" ".repeat(widths[position] - cell.chars().count() + 2));
            }
        }
        line.trim_end().to_owned() + "\n"
    };
    let mut text = line(headers.to_vec());
    for row in &rows {
        text.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    text
}

#[derive(Debug, Args)]
pub struct InfoArgs {
    #[command(flatten)]
    pub camera: CameraArg,
}

pub fn info(global: &Global, args: InfoArgs) -> Result<Report> {
    let client = Client::open(global, args.camera.camera.as_deref())?;
    let value = client.call(SessionCommand::Info)?;
    let text = info_text(&value);
    Ok(Report::json(value).text(text))
}

fn shown(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Number(number) => {
            let text = number.to_string();
            text.strip_suffix(".0").map(str::to_owned).unwrap_or(text)
        }
        other => other.to_string(),
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
        let _ = writeln!(
            text,
            "  fps         {}{}",
            value.get("fps").map(shown).unwrap_or_else(|| "?".into()),
            value
                .get("fps_limit")
                .map(|limit| format!(" (limit {})", shown(limit)))
                .unwrap_or_default()
        );
    }
    if let Some(trigger) = value.get("trigger") {
        let _ = writeln!(
            text,
            "  trigger     {}{}{}",
            trigger["mode"].as_str().unwrap_or("Off"),
            trigger["source"]
                .as_str()
                .map(|source| format!(" from {source}"))
                .unwrap_or_default(),
            trigger["activation"]
                .as_str()
                .filter(|_| trigger["mode"] == "On")
                .map(|activation| format!(" on {activation}"))
                .unwrap_or_default()
        );
    }
    if let Some(controls) = value["controls"].as_object().filter(|c| !c.is_empty()) {
        let _ = writeln!(
            text,
            "  controls    {}",
            controls
                .iter()
                .map(|(name, control)| format!(
                    "{name}={}",
                    control
                        .get("value")
                        .map(shown)
                        .unwrap_or_else(|| "?".into())
                ))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    text
}

#[derive(Debug, Args)]
pub struct FeaturesArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// Only names containing this text, or matching a glob such as 'Exposure*'
    #[arg(value_name = "PATTERN")]
    pub pattern: Option<String>,
    /// Every visibility level (Expert, Guru); a PATTERN or --category also searches every level
    #[arg(long)]
    pub all: bool,
    /// Only this category, such as ImageFormatControl
    #[arg(long, value_name = "NAME")]
    pub category: Option<String>,
}

pub fn features(global: &Global, args: FeaturesArgs) -> Result<Report> {
    let filter = FeatureFilter {
        pattern: args.pattern,
        all: args.all,
        category: args.category,
    };
    filter.validate()?;
    let client = Client::open(global, args.camera.camera.as_deref())?;
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
                row.get("value").map(shown).unwrap_or_default(),
                row.get("unit").map(shown).unwrap_or_default(),
                row["access"].as_str().unwrap_or_default().to_owned(),
                range,
                row.get("locked_by")
                    .map(|owner| format!("locked by {}", shown(owner)))
                    .unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();
    let mut text = if rows.is_empty() {
        "No features match.\n".to_owned()
    } else {
        table(&["NAME", "VALUE", "UNIT", "ACCESS", "RANGE", "NOTE"], rows)
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
pub struct GetArgs {
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// Features, banks (TriggerMode[FrameStart]) or controls (exposure, gain, fps, roi, format, trigger, auto_exposure, auto_wb)
    #[arg(required = true, value_name = "NAME")]
    pub names: Vec<String>,
}

pub fn get(global: &Global, args: GetArgs) -> Result<Report> {
    let names = args
        .names
        .iter()
        .map(|name| Name::parse(name))
        .collect::<Result<Vec<_>>>()?;
    let read = |camera: Option<&str>| -> Result<Value> {
        let client = Client::open(global, camera)?;
        client.call(SessionCommand::GetValues {
            names: names.clone(),
        })
    };
    let lines = |value: &Value| -> String {
        value["values"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, value)| format!("{name} = {}\n", shown(value)))
            .collect()
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
pub struct SetArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// NAME=VALUE: a feature (ExposureTime=10ms), a bank (TriggerMode[FrameStart]=On) or a control (exposure=5ms, roi=0,0,640,480)
    #[arg(required = true, value_name = "NAME=VALUE")]
    pub values: Vec<String>,
    /// Check every value against the camera and show the writes in order; change nothing
    #[arg(long)]
    pub dry_run: bool,
}

pub fn set(global: &Global, args: SetArgs) -> Result<Report> {
    let values = args
        .values
        .iter()
        .map(|value| Assignment::parse(value))
        .collect::<Result<Vec<_>>>()?;
    let client = Client::open(global, args.camera.camera.as_deref())?;
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
            row.get("from").map(shown).unwrap_or_else(|| "?".into()),
            row.get("to").map(shown).unwrap_or_else(|| "?".into())
        );
    }
    for warning in value["warnings"].as_array().into_iter().flatten() {
        let _ = writeln!(text, "Note: {}", shown(warning));
    }
    Ok(Report::json(value).text(text))
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// A command feature, optionally at a bank: TriggerSoftware, UserSetSave[UserSet1]
    #[arg(value_name = "COMMAND")]
    pub command: String,
}

pub fn exec(global: &Global, args: ExecArgs) -> Result<Report> {
    let name = Name::parse(&args.command)?;
    let client = Client::open(global, args.camera.camera.as_deref())?;
    let value = client.call(SessionCommand::Exec { name })?;
    let text = format!(
        "Ran {} on {}.\n",
        value["executed"].as_str().unwrap_or(&args.command),
        value["camera"].as_str().unwrap_or("the camera")
    );
    Ok(Report::json(value).text(text))
}

/// Result fields of these commands, for `schema`.
pub const GET_FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    (
        "values",
        "{name: value} — keys as named, with bank shorthand (TriggerMode[FrameStart]) where unambiguous; controls in the form set takes (exposure: \"10ms\")",
    ),
    ("cameras", "[{camera,values}] — with -c repeated"),
];
pub const EXEC_FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    ("executed", "string — the command run, with its bank"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align_and_drop_empty_columns() {
        let text = table(
            &["ID", "KIND", "SERIAL"],
            vec![
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
