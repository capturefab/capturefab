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
    /// Meter only this part of the frame: X,Y,W,H as fractions 0..1
    #[arg(long, value_name = "X,Y,W,H", value_parser = super::inspect::parse_region, conflicts_with = "off")]
    pub region: Option<[f64; 4]>,
    /// The longest exposure, as against motion blur: gain rises past it
    #[arg(long, value_name = "DUR", value_parser = super::args::duration, conflicts_with = "off")]
    pub max_exposure: Option<Duration>,
    #[command(flatten)]
    pub keep: KeepArg,
}

/// How often settling looks at the camera's auto state: within a frame of
/// a fast camera, so the command ends soon after exposure settles; a look
/// is one short request, or a register read or two.
const POLL: Duration = Duration::from_millis(25);

pub fn run(global: &Global, args: AutoArgs) -> Result<Report> {
    signals::take_over();
    let client = Client::open_with(global, args.camera.camera.as_deref(), args.keep.route())?;
    let value = if args.off {
        off(&client)?
    } else if args.continuous {
        if args.region.is_some() || args.max_exposure.is_some() {
            return Err(Usage(
                "--region and --max-exposure settle exposure once; leave out --continuous".into(),
            )
            .into());
        }
        continuous(&client)?
    } else {
        let limits = Limits {
            region: args.region,
            max_exposure_us: args.max_exposure.map(|max| max.as_secs_f64() * 1e6),
        };
        settle(&client, args.timeout, &limits)?
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

/// What settling exposure keeps to.
#[derive(Debug, Default)]
pub(super) struct Limits {
    /// Meter only this part of the frame.
    pub region: Option<[f64; 4]>,
    /// The longest exposure, in microseconds.
    pub max_exposure_us: Option<f64>,
}

/// Settle exposure and gain on `client`'s camera within `timeout`, then
/// hold them: with the camera's own auto exposure run once when it has one
/// and nothing limits it, else with Capturefab's, which runs on the stream
/// and is left with the values it reached. A camera this started streams
/// no longer.
pub(super) fn settle(client: &Client, timeout: Duration, limits: &Limits) -> Result<Value> {
    let info = client.call(SessionCommand::Info)?;
    adjustable(&info)?;
    let once = once_features(client, &info)?;
    if limits.region.is_none() && limits.max_exposure_us.is_none() && !once.is_empty() {
        return settle_once(client, &info, &once, timeout);
    }
    let streaming = client.status()?["streaming"] == true;
    client.call(SessionCommand::Auto {
        balance: None,
        continuous: false,
        region: limits.region,
        max_exposure_us: limits.max_exposure_us,
        job: None,
    })?;
    let started = !streaming && client.call(SessionCommand::Start).is_ok();
    let deadline = Instant::now() + timeout;
    let _settling = super::term::activity("settling exposure…");
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
            signals::sleep(POLL);
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
    value["by"] = json!("capturefab");
    Ok(crate::types::without_nulls(value))
}

/// The camera's own auto functions with a Once mode: ExposureAuto, and
/// GainAuto beside it, and BalanceWhiteAuto on a color camera, which the
/// image then needs balanced as much as exposed.
fn once_features(client: &Client, info: &Value) -> Result<Vec<String>> {
    let listed = client.call(SessionCommand::FeatureList {
        filter: FeatureFilter {
            pattern: Some("*Auto".into()),
            all: true,
            category: None,
        },
    })?;
    Ok(with_once(&listed, info))
}

fn with_once(listed: &Value, info: &Value) -> Vec<String> {
    let once = |name: &str| {
        listed["features"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| {
                row["name"] == name
                    && row["access"] == "rw"
                    && row["choices"]
                        .as_array()
                        .is_some_and(|choices| choices.iter().any(|choice| choice == "Once"))
            })
    };
    if !once("ExposureAuto") {
        return Vec::new();
    }
    let color = info["pixel_format"]
        .as_str()
        .is_some_and(|format| !format.starts_with("Mono"));
    ["ExposureAuto", "GainAuto", "BalanceWhiteAuto"]
        .into_iter()
        .filter(|name| (color || *name != "BalanceWhiteAuto") && once(name))
        .map(String::from)
        .collect()
}

/// Settle with the camera's own auto functions set to Once, which return
/// to Off when the image is right: the camera streams until they do or
/// `timeout` passes, when they are turned off holding what they reached.
fn settle_once(
    client: &Client,
    info: &Value,
    features: &[String],
    timeout: Duration,
) -> Result<Value> {
    let streaming = client.status()?["streaming"] == true;
    let names = features
        .iter()
        .map(|name| crate::controls::Name::parse(name))
        .collect::<Result<Vec<_>>>()?;
    let set = |mode: &str| -> Result<Value> {
        client.call(SessionCommand::SetValues {
            values: features
                .iter()
                .map(|name| Assignment::parse(&format!("{name}={mode}")))
                .collect::<Result<_>>()?,
            dry_run: false,
        })
    };
    set("Once")?;
    let started = !streaming && client.call(SessionCommand::Start).is_ok();
    let deadline = Instant::now() + timeout;
    let _settling = super::term::activity("settling exposure…");
    let mut polls = 0u32;
    let watched = (|| -> Result<bool> {
        loop {
            let values = client.call(SessionCommand::GetValues {
                names: names.clone(),
            })?;
            if values["values"]
                .as_object()
                .is_some_and(|values| values.values().all(|mode| mode == "Off"))
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            if signals::interrupted() {
                return Err(Categorized::error(
                    ErrorKind::Interrupted,
                    "interrupted while settling exposure",
                ));
            }
            polls += 1;
            signals::sleep(POLL);
        }
    })();
    // Measured on the settled stream, before it stops.
    let measured = match &watched {
        Ok(true) => client
            .call(SessionCommand::Frame {
                fresh: 1,
                timeout_ms: 0,
                max_dimension: 0,
                format: Default::default(),
                region: None,
                preview: None,
                warmup_ms: Some(0),
            })
            .ok(),
        _ => None,
    };
    // Cut short: hold what the camera reached.
    let held = match &watched {
        Ok(true) => Ok(Value::Null),
        _ => set("Off"),
    };
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
    let settled = watched?;
    held?;
    let after = client.call(SessionCommand::Info)?;
    let mut value = json!({
        "camera": info["camera"],
        "settled": settled,
        "by": "camera",
        "exposure_us": after["exposure_us"],
        "gain_db": after["gain_db"],
        "iterations": polls,
        "mean": measured.as_ref().and_then(|reply| reply["exposure"]["mean"].as_f64()),
    });
    let at_max = |control: &str, value: &Value| {
        let max = after["controls"][control]["max"].as_f64();
        value
            .as_f64()
            .zip(max)
            .is_some_and(|(value, max)| value >= max * 0.999)
    };
    if !settled {
        value["limited_by"] = json!("timeout");
    } else if at_max("exposure", &after["exposure_us"]) {
        value["limited_by"] = json!("exposure_max");
    } else if at_max("gain", &after["gain_db"]) {
        value["limited_by"] = json!("gain_max");
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
            region: None,
            max_exposure_us: None,
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
        "exposure_max|exposure_min|gain_max|camera|timeout — what stopped it short of the target",
    ),
    ("exposure_us", "float — the exposure held"),
    ("gain_db", "float — the gain held"),
    (
        "iterations",
        "int — exposure and gain changes made; by the camera, how often it was asked whether it was done",
    ),
    ("mean", "float — mean brightness 0..1 at the end"),
    ("mode", "off|continuous — with --off or --continuous"),
    (
        "by",
        "camera|capturefab — whose auto exposure settled or runs: the camera's own (ExposureAuto Once or Continuous), or Capturefab's (with --region, --max-exposure, or when the camera has none)",
    ),
    (
        "job",
        concat!(
            super::jobs::job_row!(),
            " — with --continuous by capturefab: the session job it runs as, which jobs stop ends"
        ),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_color_camera_balances_white_once_beside_exposure() {
        let row = |name: &str, choices: &[&str]| json!({"name": name, "access": "rw", "choices": choices});
        let listed = json!({"features": [
            row("ExposureAuto", &["Off", "Once", "Continuous"]),
            row("GainAuto", &["Off", "Once"]),
            row("BalanceWhiteAuto", &["Off", "Once", "Continuous"]),
        ]});
        let color = json!({"pixel_format": "BayerRG8"});
        assert_eq!(
            with_once(&listed, &color),
            ["ExposureAuto", "GainAuto", "BalanceWhiteAuto"]
        );
        let mono = json!({"pixel_format": "Mono8"});
        assert_eq!(with_once(&listed, &mono), ["ExposureAuto", "GainAuto"]);
        // White balance alone is not exposure.
        let balance_only = json!({"features": [row("ExposureAuto", &["Off", "Continuous"]), row("BalanceWhiteAuto", &["Off", "Once"])]});
        assert!(with_once(&balance_only, &color).is_empty());
    }
}
