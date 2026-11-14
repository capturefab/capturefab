//! Admission for a camera whose acquisition and configuration belong to another controller.
use super::{SessionCommand, protocol::WorkerCommand};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;

pub(crate) fn unsupported() -> anyhow::Error {
    Categorized::new(
        ErrorKind::Unsupported,
        "GigE monitor access cannot change camera configuration or control acquisition",
    )
    .hint("configure and start multicast acquisition in the controlling application; omit settings, auto, device settling and capture freshness options")
    .into()
}

/// Validate a known monitor before a CLI or coordinator opens it or creates outputs.
pub(crate) fn validate_for_selector(selector: &str, command: &SessionCommand) -> Result<()> {
    if is_selector(selector)? {
        if let SessionCommand::Connect { camera, .. } = command {
            let expected = crate::transport::gige::monitor::MonitorSpec::parse(selector)?;
            if !is_selector(camera)?
                || crate::transport::gige::monitor::MonitorSpec::parse(camera)?.canonical()
                    != expected.canonical()
            {
                return Err(unsupported());
            }
        }
        validate(command)?;
    }
    Ok(())
}

pub(crate) fn is_selector(selector: &str) -> Result<bool> {
    if crate::transport::gige::monitor::is_selector(selector) {
        crate::transport::gige::monitor::MonitorSpec::parse(selector)?;
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn reject_selector(selector: Option<&str>) -> Result<()> {
    if let Some(selector) = selector
        && is_selector(selector)?
    {
        return Err(unsupported());
    }
    Ok(())
}

pub(crate) fn validate_worker(command: &WorkerCommand) -> Result<()> {
    match command {
        WorkerCommand::CheckConnection => Ok(()),
        WorkerCommand::OptimizeNetwork { .. } | WorkerCommand::WriteActionDeviceKeyUntil { .. } => {
            Err(unsupported())
        }
        WorkerCommand::Session(command)
        | WorkerCommand::SessionUntil { command, .. }
        | WorkerCommand::ForwardObservedUntil { command, .. } => validate(command),
    }
}

fn passive_frames(warmup: Option<u64>, fresh: u32) -> Result<()> {
    if warmup.is_some_and(|ms| ms > 0) || fresh > 0 {
        return Err(unsupported());
    }
    Ok(())
}

/// The exhaustive policy deliberately includes nominal reads which switch selectors.
pub(crate) fn validate(command: &SessionCommand) -> Result<()> {
    match command {
        SessionCommand::SetValues { .. }
        | SessionCommand::Exec { .. }
        | SessionCommand::ReadSetup
        | SessionCommand::SetupDiff { .. }
        | SessionCommand::ApplySetup { .. }
        | SessionCommand::Auto { .. }
        | SessionCommand::Manual
        | SessionCommand::WriteMemory { .. }
        | SessionCommand::WriteRegister { .. }
        | SessionCommand::WriteActionDeviceKey { .. } => Err(unsupported()),
        SessionCommand::GetValues { names } => {
            if names.iter().any(|name| !name.bank.is_empty()) {
                return Err(unsupported());
            }
            Ok(())
        }
        SessionCommand::ReadRegister { name, .. } => {
            if !name.bank.is_empty() {
                return Err(unsupported());
            }
            Ok(())
        }
        SessionCommand::Capture {
            warmup_ms, fresh, ..
        }
        | SessionCommand::Schedule {
            warmup_ms, fresh, ..
        } => passive_frames(*warmup_ms, *fresh),
        SessionCommand::Arm {
            warmup_ms,
            bytes_per_second,
        } => {
            if bytes_per_second.is_some() {
                return Err(unsupported());
            }
            passive_frames(*warmup_ms, 0)
        }
        SessionCommand::Frame { warmup_ms, .. } | SessionCommand::Forward { warmup_ms, .. } => {
            passive_frames(*warmup_ms, 0)
        }
        SessionCommand::NetworkScan { tune, .. } => {
            if *tune {
                return Err(unsupported());
            }
            Ok(())
        }
        SessionCommand::Connect { .. }
        | SessionCommand::Disconnect
        | SessionCommand::Info
        | SessionCommand::FeatureList { .. }
        | SessionCommand::Start
        | SessionCommand::Stop
        | SessionCommand::Jobs
        | SessionCommand::Wait { .. }
        | SessionCommand::StopJob { .. }
        | SessionCommand::Status
        | SessionCommand::Xml
        | SessionCommand::ReadMemory { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(value: &serde_json::Value) -> SessionCommand {
        SessionCommand::from_json_slice(value.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn nominal_reads_dry_runs_and_device_settling_cannot_bypass_monitor_admission() {
        for value in [
            json!({"op":"set_values","values":[{"name":"Width","value":"4"}],"dry_run":true}),
            json!({"op":"exec","name":{"name":"AcquisitionStart"}}),
            json!({"op":"get_values","names":[{"name":"TriggerMode","bank":[{"selector":"TriggerSelector","value":"FrameStart"}]}]}),
            json!({"op":"read_setup"}),
            json!({"op":"setup_diff","name":"saved"}),
            json!({"op":"write_memory","address":1,"data":[0]}),
            json!({"op":"read_register","name":{"name":"LUTValueAll","bank":[{"selector":"LUTSelector","value":"LUT1"}]},"timeout_ms":100}),
            json!({"op":"arm","warmup_ms":1}),
            json!({"op":"arm","bytes_per_second":1}),
            json!({"op":"capture","format":"raw","timeout_ms":100,"fresh":1}),
            json!({"op":"frame","warmup_ms":1}),
            json!({"op":"network_scan","timeout_ms":100,"tune":true}),
        ] {
            let command = request(&value);
            assert_eq!(
                crate::types::classify(&validate(&command).unwrap_err()).kind,
                ErrorKind::Unsupported
            );
            validate_worker(&WorkerCommand::Session(command)).unwrap_err();
        }
        for value in [
            json!({"op":"get_values","names":[{"name":"Width"}]}),
            json!({"op":"info"}),
            json!({"op":"xml"}),
            json!({"op":"read_memory","address":1,"length":4}),
            json!({"op":"read_register","name":{"name":"Opaque"},"timeout_ms":100}),
            json!({"op":"arm"}),
            json!({"op":"arm","warmup_ms":0}),
            json!({"op":"capture","format":"raw","timeout_ms":100}),
            json!({"op":"frame","fresh":3}),
            json!({"op":"stop"}),
        ] {
            validate(&request(&value)).unwrap();
        }
        validate_worker(&WorkerCommand::OptimizeNetwork {
            bytes_per_second: 1,
        })
        .unwrap_err();
        validate_worker(&WorkerCommand::WriteActionDeviceKeyUntil {
            key: 1,
            deadline_ns: 1,
        })
        .unwrap_err();
    }
}
