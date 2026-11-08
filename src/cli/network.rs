//! `capturefab network`: `GigE` interface checks, and with `--tune` the
//! per-NIC bandwidth sharing applied to cameras.
#![forbid(unsafe_code)]
use super::{Global, output::Report};
use crate::session::{SessionCommand, SessionHandle};
use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Args)]
pub(crate) struct NetworkArgs {
    /// Apply bandwidth sharing (packet delay, throughput limits) to the cameras given with -c, or a session's cameras; they keep it until power-off
    #[arg(long)]
    pub tune: bool,
    /// `GigE` camera to tune, repeatable (with --tune; not read from `CAPTUREFAB_CAMERA`, so the read-only checks stay read-only)
    #[arg(
        help = "GigE camera to tune, repeatable (with --tune; not read from CAPTUREFAB_CAMERA, so the read-only checks stay read-only)",
        short = 'c',
        long = "camera",
        value_name = "CAM",
        requires = "tune"
    )]
    pub cameras: Vec<String>,
    /// How long `GigE` discovery collects answers
    #[arg(help = "How long GigE discovery collects answers", long, default_value = "1s", value_name = "DUR", value_parser = super::args::duration)]
    pub timeout: Duration,
}

pub(super) const FIELDS: &[(&str, &str)] = &[
    (
        "interfaces",
        "[{name,address,netmask,mtu?,speed_mbps?,kind:wired|wireless|loopback|other|unknown,up?}] — IPv4 adapters",
    ),
    (
        "cameras",
        "[{camera:{id,kind,vendor,model,serial?,address?},active,interface?,host_address?,reachable,health:unreachable|limited|unverified|ready|unstable,summary,details,tuning?}] — GigE cameras and how each is reached; active: a session holds it; host_address: this computer's address toward it",
    ),
    ("warnings", "[string]"),
    ("time", "string — RFC 3339 UTC time of the scan"),
];

pub(super) fn run(global: &Global, args: &NetworkArgs) -> Result<Report> {
    let command = SessionCommand::NetworkScan {
        timeout_ms: u64::try_from(args.timeout.as_millis()).unwrap_or(u64::MAX),
        tune: args.tune,
    };
    command.validate()?;
    let value = if let (Some(session), false) = (&global.session, global.no_session) {
        crate::ipc::call(session, command, Duration::from_secs(120))?
    } else {
        let local = SessionHandle::new();
        let result = (|| -> Result<Value> {
            for camera in &args.cameras {
                let camera = super::caller_selector(camera)?;
                local.request(SessionCommand::connect(camera, 2000))?;
            }
            local.request(command)
        })();
        local.shutdown();
        result?
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
    // Cameras in the shape `list` prints them.
    for camera in scan["cameras"].as_array_mut().into_iter().flatten() {
        if let Ok(info) =
            serde_json::from_value::<crate::types::CameraInfo>(camera["camera"].clone())
        {
            camera["camera"] = serde_json::to_value(super::camera::row(&info))?;
        }
    }
    let mut report = Report::json(scan);
    if let Some(text) = text {
        report = report.text(text);
    }
    Ok(report)
}
