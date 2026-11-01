//! `capturefab network`: GigE interface checks, and with `--tune` the
//! per-NIC bandwidth sharing applied to cameras.
use super::{Global, args::CamerasArg, output::Report};
use crate::session::{SessionCommand, SessionHandle};
use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Args)]
pub struct NetworkArgs {
    /// Apply bandwidth sharing (packet delay, throughput limits) to the cameras given with -c, or a session's cameras; they keep it until power-off
    #[arg(long)]
    pub tune: bool,
    #[command(flatten)]
    pub cameras: CamerasArg,
    /// How long GigE discovery collects answers
    #[arg(long, default_value = "1s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Duration,
}

pub const FIELDS: &[(&str, &str)] = &[
    (
        "interfaces",
        "[{name,ip,netmask,mtu?,speed_mbps?,kind?,up?}] — IPv4 adapters",
    ),
    (
        "cameras",
        "[{camera,interface,reachable,health,summary,details,tuning?}] — GigE cameras and how each is reached",
    ),
    ("warnings", "[string]"),
    ("time", "string — RFC 3339 UTC time of the scan"),
];

pub fn run(global: &Global, args: NetworkArgs) -> Result<Report> {
    if !args.tune && !args.cameras.cameras.is_empty() {
        return Err(crate::types::Usage(
            "-c names cameras to tune; add --tune, or leave -c out for read-only checks".into(),
        )
        .into());
    }
    let command = SessionCommand::NetworkScan {
        timeout_ms: u64::try_from(args.timeout.as_millis()).unwrap_or(u64::MAX),
        tune: args.tune,
    };
    command.validate()?;
    let value = match (&global.session, global.no_session) {
        (Some(session), false) => crate::ipc::call(session, command, Duration::from_secs(120))?,
        _ => {
            let local = SessionHandle::new();
            let result = (|| -> Result<Value> {
                for camera in &args.cameras.cameras {
                    let camera = super::caller_selector(camera)?;
                    local.request(SessionCommand::connect(camera, 2000))?;
                }
                local.request(command)
            })();
            local.shutdown();
            result?
        }
    };
    let mut scan = value["network_scan"].clone();
    let text = serde_json::from_value::<crate::network::NetworkScan>(scan.clone())
        .ok()
        .map(|scan| {
            let mut out = Vec::new();
            let _ = crate::network::write_human(&scan, &mut out);
            String::from_utf8_lossy(&out).into_owned()
        });
    if let Some(object) = scan.as_object_mut()
        && let Some(ms) = object.remove("scanned_at_ms").and_then(|ms| ms.as_i64())
        && let Some(time) = chrono::DateTime::from_timestamp_millis(ms)
    {
        object.insert(
            "time".into(),
            json!(time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
    }
    let mut report = Report::json(scan);
    if let Some(text) = text {
        report = report.text(text);
    }
    Ok(report)
}
