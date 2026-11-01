//! `capturefab onvif resolve`: an ONVIF camera's RTSP streams.
use super::{Global, output::Report};
use crate::logins::{Logins, Remembered};
use crate::types::Usage;
use anyhow::Result;
use clap::Subcommand;
use serde_json::json;
use std::fmt::Write as _;
use std::time::Duration;

#[derive(Debug, Subcommand)]
pub enum OnvifCommand {
    /// List an ONVIF camera's profiles and their RTSP stream URIs
    #[command(
        after_help = "Examples:\n  capturefab onvif resolve cam.local\n  capturefab onvif resolve http://10.0.0.9/onvif/device_service --profile Profile_1\n  capturefab login add onvif://cam.local --user admin --password-stdin   # save its sign-in first"
    )]
    Resolve {
        /// HOST[:PORT], http://HOST/onvif/device_service, or onvif://HOST
        host: String,
        /// Only this profile token
        #[arg(long, value_name = "TOKEN")]
        profile: Option<String>,
        /// How long the camera may take to answer
        #[arg(long, default_value = "5s", value_name = "DUR", value_parser = super::args::duration)]
        timeout: Duration,
    },
}

/// The `onvif resolve` result.
pub const FIELDS: &[(&str, &str)] = &[
    ("endpoint", "string — the ONVIF device service asked"),
    (
        "streams",
        "[{token,name,encoding,width,height,uri}] — uri opens with -c (credentials are never shown)",
    ),
];

/// The ONVIF source a host names.
fn source(host: &str) -> String {
    let host = host.trim();
    if crate::onvif::is_source(host) {
        host.to_owned()
    } else if host.contains("://") {
        format!("onvif:{host}")
    } else {
        format!("onvif://{host}")
    }
}

pub fn run(_global: &Global, command: OnvifCommand) -> Result<Report> {
    let OnvifCommand::Resolve {
        host,
        profile,
        timeout,
    } = command;
    if let Some(profile) = &profile
        && (profile.is_empty() || profile.len() > 1024 || profile.chars().any(char::is_control))
    {
        return Err(Usage(
            "ONVIF profile must be nonempty and contain no control characters (maximum 1024 bytes)"
                .into(),
        )
        .into());
    }
    let source = source(&host);
    let (endpoint, mut credentials) =
        crate::onvif::source_endpoint(&source).map_err(|error| Usage(format!("{error:#}")))?;
    if credentials.is_none()
        && let Ok(key) = crate::logins::key(&source)
        && let Remembered::Login(login) = Logins::system().remembered(&key.address)?
    {
        credentials = Some(crate::onvif::Credentials {
            username: login.user.clone(),
            password: login.expose().to_owned(),
        });
    }
    let streams =
        crate::onvif::resolve(&endpoint, credentials.as_ref(), profile.as_deref(), timeout)?;
    let streams = streams
        .into_iter()
        .map(|stream| {
            let mut value = json!(stream);
            value["uri"] = json!(crate::media::redact_url(&stream.uri));
            value
        })
        .collect::<Vec<_>>();
    let mut text = String::new();
    for stream in &streams {
        let _ = writeln!(
            text,
            "{}  {}  {}{}",
            stream["token"].as_str().unwrap_or_default(),
            stream["uri"].as_str().unwrap_or_default(),
            stream["encoding"].as_str().unwrap_or_default(),
            match (stream["width"].as_u64(), stream["height"].as_u64()) {
                (Some(w), Some(h)) => format!(" {w}x{h}"),
                _ => String::new(),
            }
        );
    }
    if streams.is_empty() {
        text.push_str("The camera reported no streams.\n");
    }
    let endpoint = crate::media::redact_url(&endpoint);
    Ok(Report::json(json!({"endpoint": endpoint, "streams": streams})).text(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_become_onvif_sources() {
        assert_eq!(source("cam.local"), "onvif://cam.local");
        assert_eq!(
            source("http://cam.local/onvif/device_service"),
            "onvif:http://cam.local/onvif/device_service"
        );
        assert_eq!(source("onvif://cam.local:8080"), "onvif://cam.local:8080");
        let (endpoint, _) = crate::onvif::source_endpoint(&source("cam.local")).unwrap();
        assert!(endpoint.starts_with("http://cam.local"), "{endpoint}");
    }
}
