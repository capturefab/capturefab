//! Owned NI input with continuous or software timing. The SDK lives only in a child.
use anyhow::Result;

mod config;
pub(crate) use config::Config;
#[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
use config::{Measurement, TerminalConfig};

use super::Source;
use crate::types::{Categorized, ErrorKind};

#[cfg(any(feature = "ni-daqmx", all(test, any(target_os = "linux", windows))))]
pub(crate) const WORKER_ARG: &str = "__nidaqmx-worker";
pub(super) const MAX_CHANNELS: usize = 64;
#[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
pub(super) const MAX_WORDS: usize = 4096;
#[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
pub(super) const MAX_ROW: usize = 512 * 1024;

pub(super) fn channel_key(name: &str) -> Option<(String, u32)> {
    if name.len() > 256 || !name.is_ascii() {
        return None;
    }
    let (device, channel) = name.split_once('/')?;
    if device.is_empty()
        || !device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return None;
    }
    let digits = channel.strip_prefix("ai")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((device.to_ascii_lowercase(), digits.parse().ok()?))
}

pub(super) fn input_channel_key(name: &str, prefix: &str) -> Option<(String, u32)> {
    if name.len() > 256 || !name.is_ascii() {
        return None;
    }
    let (device, channel) = name.split_once('/')?;
    if device.is_empty()
        || !device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return None;
    }
    let digits = channel.strip_prefix(prefix)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((device.to_ascii_lowercase(), digits.parse().ok()?))
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config)?;
    #[cfg(all(feature = "ni-daqmx", any(target_os = "linux", windows)))]
    {
        client::open(config)
    }
    #[cfg(not(all(feature = "ni-daqmx", any(target_os = "linux", windows))))]
    {
        Err(unsupported())
    }
}

fn unsupported() -> anyhow::Error {
    Categorized::new(
        ErrorKind::Unsupported,
        "NI-DAQmx input requires the ni-daqmx build feature and a supported Linux or Windows driver host",
    )
    .hint("Install the NI driver separately; macOS and NI-DAQmx Base use different interfaces")
    .into()
}

pub(crate) fn validate(config: &Config) -> Result<()> {
    config.validate()?;
    if !cfg!(all(feature = "ni-daqmx", any(target_os = "linux", windows))) {
        return Err(unsupported());
    }
    Ok(())
}

#[cfg(feature = "ni-daqmx")]
pub(crate) fn run_worker() -> Result<()> {
    #[cfg(any(target_os = "linux", windows))]
    {
        worker::run()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Err(Categorized::error(
            ErrorKind::Unsupported,
            "NI-DAQmx requires Linux or Windows",
        ))
    }
}

#[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
mod client;
#[cfg(all(feature = "ni-daqmx", any(target_os = "linux", windows)))]
mod native;
#[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
mod process;
#[cfg(test)]
mod tests;
#[cfg(any(all(feature = "ni-daqmx", any(target_os = "linux", windows)), test))]
mod wire;
#[cfg(all(feature = "ni-daqmx", any(target_os = "linux", windows)))]
mod worker;
