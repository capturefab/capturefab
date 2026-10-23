//! App-wide resource budgets are local settings, independent of session selection.
use crate::resources::{self, Policy};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::Value;

#[derive(Debug, Subcommand)]
pub enum ResourcesCommand {
    /// Show current limits, usage and remaining headroom
    Status,
    /// Change only the named budgets; use 'auto' to restore automatic protection
    Configure {
        /// Maximum image/codec threads per camera worker: auto or 1..64
        #[arg(long)]
        cpu_threads: Option<String>,
        /// Shared camera-buffer budget: auto or a size such as 1GiB
        #[arg(long)]
        camera_buffers: Option<String>,
    },
    /// Restore automatic CPU and camera-buffer safeguards
    Reset,
}

fn number(value: &str) -> Result<Option<u32>> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    Ok(Some(
        value
            .parse()
            .context("CPU thread limit must be auto or 1..64")?,
    ))
}
fn bytes(value: &str) -> Result<Option<u64>> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    Ok(Some(crate::storage::parse_bytes(value)?))
}
fn patch(command: &ResourcesCommand, policy: &mut Policy) -> Result<()> {
    let ResourcesCommand::Configure {
        cpu_threads,
        camera_buffers,
    } = command
    else {
        if matches!(command, ResourcesCommand::Reset) {
            *policy = Policy::default();
        }
        return Ok(());
    };
    ensure!(
        cpu_threads.is_some() || camera_buffers.is_some(),
        "Choose --cpu-threads, --camera-buffers, or use resources reset."
    );
    if let Some(value) = cpu_threads {
        policy.cpu_threads = number(value)?;
    }
    if let Some(value) = camera_buffers {
        policy.buffer_bytes = bytes(value)?;
    }
    policy.validate()
}
pub fn prepare(command: Option<&ResourcesCommand>) -> Result<()> {
    if let Some(command) = command {
        patch(command, &mut Policy::default())?;
    }
    Ok(())
}
pub fn run(command: Option<&ResourcesCommand>) -> Result<Value> {
    match command {
        Some(ResourcesCommand::Configure { .. }) => {
            resources::update(|policy| patch(command.expect("configure"), policy))?;
        }
        Some(ResourcesCommand::Reset) => {
            resources::configure(&Policy::default())?;
        }
        _ => {}
    }
    // A short-lived CLI needs two host CPU observations; the desktop and
    // server already have a continuing sampler. Never invent a zero load.
    if resources::host::sample().cpu_percent.is_none() {
        std::thread::sleep(std::time::Duration::from_millis(1010));
    }
    resources::snapshot()
}
pub fn human(value: &Value) -> String {
    let mut lines = vec!["Capturefab resources".to_owned()];
    let effective = &value["effective"];
    let host = &value["host"];
    let cpu = host["cpu_percent"].as_f64().map_or_else(
        || "Checking device load".into(),
        |n| {
            format!(
                "Device load {n:.0}% · {:.0}% CPU headroom",
                (100.0 - n).max(0.0)
            )
        },
    );
    lines.push(format!(
        "CPU: {cpu}; up to {} image/codec threads per camera",
        effective["cpu_threads"]
    ));
    if let Some(bytes) = value["buffers"]["used_bytes"].as_u64() {
        let limit = effective["buffer_bytes"].as_u64().unwrap_or(0);
        lines.push(format!(
            "Camera buffers: {} / {} MiB; {} MiB {}",
            bytes / resources::MIB,
            limit / resources::MIB,
            value["buffers"]["headroom_bytes"].as_u64().unwrap_or(0) / resources::MIB,
            if value["buffers"]["physical_headroom_bytes"].is_null() {
                "budget room (system memory unavailable)"
            } else {
                "headroom"
            }
        ));
    } else {
        lines.push("Camera buffers: usage unavailable".into());
    }
    if let Some(free) = host["memory_available"].as_u64() {
        lines.push(format!(
            "Device memory available: {} MiB",
            free / resources::MIB
        ));
    }
    let disk = &value["storage"];
    if let (Some(used), Some(limit)) = (
        disk["allocated_bytes"].as_u64(),
        disk["policy"]["max_bytes"].as_u64(),
    ) {
        let budget = limit.saturating_sub(used);
        let room = disk["free_bytes"]
            .as_u64()
            .map(|free| budget.min(free.saturating_sub(crate::storage::FREE_SPACE_FLOOR)));
        lines.push(format!(
            "Disk: {:.2} / {:.2} GiB reserved; {:.2} GiB {}",
            used as f64 / resources::GIB as f64,
            limit as f64 / resources::GIB as f64,
            room.unwrap_or(budget) as f64 / resources::GIB as f64,
            if room.is_some() {
                "headroom"
            } else {
                "budget room (disk free space unavailable)"
            }
        ));
    } else {
        lines.push("Disk: usage unavailable; see capturefab storage".into());
    }
    lines.join("\n") + "\n"
}
