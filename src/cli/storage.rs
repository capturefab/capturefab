//! The storage budget captures and recordings count against, kept until
//! lean storage replaces the ledger: `storage` shows it, `storage
//! configure` raises it. Hidden from help and the schema.
use super::output::Report;
use crate::types::human_bytes;
use anyhow::{Result, ensure};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Owned files, the budget and reservations
    Status,
    /// Save the budget for all cameras and sessions
    Configure(StorageArgs),
}

#[derive(Debug, Args)]
pub struct StorageArgs {
    /// Maximum owned bytes across all outputs, such as 50GiB
    #[arg(long, value_parser = parse_bytes)]
    max_space: Option<u64>,
    /// Maximum owned files
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    max_files: Option<u32>,
    /// Delete the oldest owned files when the budget is reached, instead of stopping
    #[arg(long)]
    delete_oldest: bool,
    /// Stop when the budget is reached (the default)
    #[arg(long, conflicts_with = "delete_oldest")]
    stop_when_full: bool,
    /// Delete owned files older than this, such as 168h; needs --delete-oldest
    #[arg(long, conflicts_with = "no_max_age", value_name = "DUR", value_parser = super::args::duration)]
    max_age: Option<Duration>,
    /// Keep files however old they are
    #[arg(long)]
    no_max_age: bool,
}

fn parse_bytes(text: &str) -> std::result::Result<u64, String> {
    crate::storage::parse_bytes(text).map_err(|error| error.to_string())
}

impl StorageArgs {
    fn apply(
        &self,
        mut policy: crate::storage::StoragePolicy,
    ) -> Result<crate::storage::StoragePolicy> {
        if let Some(space) = self.max_space {
            policy.max_bytes = space;
        }
        if let Some(files) = self.max_files {
            policy.max_files = files;
        }
        if self.delete_oldest {
            policy.on_full = "delete-oldest".into();
        }
        if self.stop_when_full {
            policy.on_full = "stop".into();
        }
        if self.no_max_age {
            policy.max_age_seconds = None;
        }
        if let Some(age) = self.max_age {
            ensure!(
                age.subsec_nanos() == 0 && age.as_secs() > 0,
                "--max-age must be a whole number of seconds (at least 1s)"
            );
            policy.max_age_seconds = Some(age.as_secs());
        }
        ensure!(
            policy.max_age_seconds.is_none() || policy.on_full == "delete-oldest",
            "--max-age needs --delete-oldest"
        );
        policy.validate()?;
        Ok(policy)
    }
}

pub fn run(command: &StorageCommand) -> Result<Report> {
    let value = match command {
        StorageCommand::Status => crate::storage::quota_status()?,
        StorageCommand::Configure(args) => {
            let policy = args
                .apply(crate::storage::configured_policy()?)
                .map_err(super::input)?;
            crate::storage::configure_policy(&policy)?
        }
    };
    let text = summary(&value);
    Ok(Report::json(value).text(text))
}

/// How much of the budget is used, and what needs attention.
fn summary(value: &Value) -> String {
    let quota = if value["quota"].is_object() {
        &value["quota"]
    } else {
        value
    };
    let policy = &quota["policy"];
    let number = |value: &Value| value.as_u64().unwrap_or(0);
    let mut out = String::new();
    if value["quota"].is_object() {
        out.push_str("Saved the storage budget.\n");
    }
    out.push_str(&format!(
        "Using {} of {} ({} of {} files); when full: {}\n",
        human_bytes(number(&quota["allocated_bytes"])),
        human_bytes(number(&policy["max_bytes"])),
        number(&quota["files"]),
        number(&policy["max_files"]),
        policy["on_full"].as_str().unwrap_or("stop"),
    ));
    if quota["over_limit"] == true {
        out.push_str("Over the budget: new files stop until files are removed or it is raised (storage configure).\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_take_durations_and_need_delete_oldest() {
        use clap::Parser;
        #[derive(Parser)]
        struct Probe {
            #[command(flatten)]
            storage: StorageArgs,
        }
        let probe = Probe::try_parse_from(["x", "--max-age", "168h", "--delete-oldest"]).unwrap();
        let policy = probe
            .storage
            .apply(crate::storage::StoragePolicy::default())
            .unwrap();
        assert_eq!(policy.max_age_seconds, Some(168 * 3600));
        assert!(Probe::try_parse_from(["x", "--max-age", "7"]).is_err());
        let probe = Probe::try_parse_from(["x", "--max-age", "1h"]).unwrap();
        assert!(
            probe
                .storage
                .apply(crate::storage::StoragePolicy::default())
                .is_err()
        );
    }
}
