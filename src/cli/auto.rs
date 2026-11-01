//! `capturefab auto`: settle exposure and gain, then hold them; or turn the
//! camera's auto exposure off, or leave it on.
use super::{Client, Global, args::CameraArg, args::KeepArg, output::Report, signals};
use crate::{
    controls::{Assignment, FeatureFilter},
    session::SessionCommand,
    types::{Categorized, ErrorKind, Usage},
};
use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Debug, Args)]
pub struct AutoArgs {
    #[command(flatten)]
    pub camera: CameraArg,
    /// Turn the camera's auto exposure, gain and white balance off, keeping their values
    #[arg(long, conflicts_with = "continuous")]
    pub off: bool,
    /// Keep adjusting: the camera's continuous auto exposure, or Capturefab's in a session
    #[arg(long)]
    pub continuous: bool,
    /// How long settling may take
    #[arg(long, default_value = "5s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Duration,
    #[command(flatten)]
    pub keep: KeepArg,
}

/// How often settling looks at the camera's auto state.
const POLL: Duration = Duration::from_millis(100);

pub fn run(global: &Global, args: AutoArgs) -> Result<Report> {
    signals::take_over();
    let client = Client::open_with(global, args.camera.camera.as_deref(), args.keep.route())?;
    let value = if args.off {
        off(&client)?
    } else if args.continuous {
        continuous(&client)?
    } else {
        settle(&client, args.timeout)?
    };
    let text = text(&value);
    Ok(Report::json(value).text(text))
}

fn text(value: &Value) -> String {
    let camera = value["camera"].as_str().unwrap_or("the camera");
    let exposure = value["exposure_us"]
        .as_f64()
        .map(|us| format!(" at {} µs", us.round()))
        .unwrap_or_default();
    match value["mode"].as_str() {
        Some("off") => format!("Auto exposure is off on {camera}{exposure}.\n"),
        Some(_) => format!(
            "Auto exposure stays on for {camera} ({}).\n",
            value["by"].as_str().unwrap_or("camera")
        ),
        None if value["settled"] == true => format!(
            "Settled {camera}{exposure} after {} step(s){}; holding it.\n",
            value["iterations"],
            value["limited_by"]
                .as_str()
                .map(|limit| format!(", limited by {limit}"))
                .unwrap_or_default()
        ),
        None => format!(
            "{camera} had not settled{exposure}; holding the last values. Try a longer --timeout or more light.\n"
        ),
    }
}

/// Refuse a camera whose exposure cannot be adjusted, or that sends frames
/// only when triggered: there is nothing to meter between triggers.
fn adjustable(info: &Value) -> Result<()> {
    let camera = info["camera"].as_str().unwrap_or("the camera");
    if info["controls"].get("exposure").is_none() && info["controls"].get("gain").is_none() {
        return Err(Categorized::new(
            ErrorKind::Unsupported,
            format!("{camera} has no exposure or gain to adjust"),
        )
        .into());
    }
    if info["trigger"]["mode"] == "On" {
        return Err(Categorized::new(
            ErrorKind::Unsupported,
            format!(
                "{camera} sends frames only when triggered ({}), so auto exposure has nothing to meter",
                info["trigger"]["source"].as_str().unwrap_or("trigger")
            ),
        )
        .hint("set trigger=off first, or set exposure by hand")
        .details(json!({"trigger": info["trigger"]}))
        .into());
    }
    Ok(())
}

/// Settle exposure and gain on `client`'s camera within `timeout`, then
/// hold them: Capturefab's auto mode runs on the stream and is left with
/// the values it reached. A camera this started streams no longer.
pub(super) fn settle(client: &Client, timeout: Duration) -> Result<Value> {
    let info = client.call(SessionCommand::Info)?;
    adjustable(&info)?;
    let streaming = client.status()?["streaming"] == true;
    client.call(SessionCommand::Auto {
        balance: None,
        continuous: false,
        job: None,
    })?;
    let started = !streaming && client.call(SessionCommand::Start).is_ok();
    let deadline = Instant::now() + timeout;
    let watched = (|| -> Result<(Value, bool)> {
        loop {
            let auto = client.status()?["auto"].clone();
            if matches!(auto["state"].as_str(), Some("stable" | "limited")) {
                return Ok((auto, true));
            }
            if Instant::now() >= deadline {
                return Ok((auto, false));
            }
            if signals::interrupted() {
                return Err(Categorized::error(
                    ErrorKind::Interrupted,
                    "interrupted while settling exposure",
                ));
            }
            std::thread::sleep(POLL);
        }
    })();
    // Hold what auto mode reached, also when it was cut short.
    let held = client.call(SessionCommand::Manual { revert: false });
    if started {
        let camera = client.status()?;
        let busy = camera["forwarding"].is_string()
            || super::jobs::of_camera(&camera)
                .iter()
                .any(|job| !super::jobs::ended(job));
        if !busy {
            client.call(SessionCommand::Stop)?;
        }
    }
    let (auto, settled) = watched?;
    held?;
    let mut value = json!({
        "camera": info["camera"],
        "settled": settled,
        "exposure_us": auto["exposure_us"],
        "gain_db": auto["gain_db"],
        "iterations": auto["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|change| !matches!(change["feature"].as_str(), Some("auto" | "balance")))
            .count(),
        "mean": auto["brightness"].as_f64().map(|mean| (mean * 10_000.0).round() / 10_000.0),
    });
    let notes = auto["notes"].as_array().cloned().unwrap_or_default();
    let noted = |start: &str| {
        notes
            .iter()
            .any(|note| note.as_str().is_some_and(|note| note.starts_with(start)))
    };
    if !settled {
        value["limited_by"] = json!("timeout");
    } else if auto["state"] == "limited" {
        value["limited_by"] = json!(if noted("too bright") {
            "exposure_min"
        } else if noted("too dark") {
            "exposure_max"
        } else {
            "camera"
        });
    }
    if value["exposure_us"].is_null() {
        value["exposure_us"] = info["exposure_us"].clone();
    }
    Ok(crate::types::without_nulls(value))
}

/// Turn auto exposure, gain and white balance off, keeping their values.
fn off(client: &Client) -> Result<Value> {
    client.call(SessionCommand::Manual { revert: false })?;
    let info = client.call(SessionCommand::Info)?;
    Ok(crate::types::without_nulls(json!({
        "camera": info["camera"],
        "mode": "off",
        "exposure_us": info["exposure_us"],
        "gain_db": info["gain_db"],
    })))
}

/// Leave auto exposure on: the camera's own continuous mode when it has
/// one, else Capturefab's, which runs only while a session holds the camera.
fn continuous(client: &Client) -> Result<Value> {
    let info = client.call(SessionCommand::Info)?;
    adjustable(&info)?;
    let features = client.call(SessionCommand::FeatureList {
        filter: FeatureFilter {
            pattern: Some("ExposureAuto".into()),
            all: true,
            category: None,
        },
    })?;
    let firmware = features["features"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|row| {
            row["name"] == "ExposureAuto"
                && row["choices"]
                    .as_array()
                    .is_some_and(|choices| choices.iter().any(|c| c == "Continuous"))
        });
    let by = if firmware {
        client.call(SessionCommand::SetValues {
            values: vec![Assignment::parse("ExposureAuto=Continuous")?],
            dry_run: false,
        })?;
        "camera"
    } else if client.session().is_some() {
        // A job of the session, which streams the camera until stopped.
        let reply = client.call(SessionCommand::Auto {
            balance: None,
            continuous: true,
            job: None,
        })?;
        return Ok(json!({
            "camera": info["camera"],
            "mode": "continuous",
            "by": "capturefab",
            "job": reply["job"],
        }));
    } else {
        return Err(Usage(format!(
            "{} has no continuous auto exposure of its own, and Capturefab's runs only in a session",
            info["camera"].as_str().unwrap_or("the camera")
        ))
        .into());
    };
    Ok(json!({"camera": info["camera"], "mode": "continuous", "by": by}))
}

/// Result fields, for `schema`.
pub const FIELDS: &[(&str, &str)] = &[
    ("camera", "string — camera ID"),
    (
        "settled",
        "bool — brightness reached its target, or a limit; false is still exit 0",
    ),
    (
        "limited_by",
        "exposure_max|exposure_min|camera|timeout — what stopped it short of the target",
    ),
    ("exposure_us", "float — the exposure held"),
    ("gain_db", "float — the gain held"),
    ("iterations", "int — exposure and gain changes made"),
    ("mean", "float — mean brightness 0..1 at the end"),
    ("mode", "off|continuous — with --off or --continuous"),
    (
        "by",
        "camera|capturefab — with --continuous: whose auto exposure runs",
    ),
    (
        "job",
        concat!(
            super::jobs::job_row!(),
            " — with --continuous by capturefab: the session job it runs as, which jobs stop ends"
        ),
    ),
];
