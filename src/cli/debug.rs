//! `capturefab debug`: protocol tools for people debugging a camera.
use super::{Client, Global, args::CameraArg, output::Report};
use crate::session::{SessionCommand, SessionHandle};
use crate::types::Usage;
use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Subcommand)]
pub(crate) enum DebugCommand {
    /// Print a camera's `GenICam` XML, or write it to a file
    #[command(
        about = "Print a camera's GenICam XML, or write it to a file",
        after_help = "Examples:\n  capturefab debug xml -c sim:0 > camera.xml\n  capturefab debug xml -c gige:00c0df123456 -o camera.xml"
    )]
    Xml {
        #[command(flatten)]
        camera: CameraArg,
        /// The file to write; it must not exist
        #[arg(short = 'o', long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Read raw camera memory
    #[command(
        after_help = "Examples:\n  capturefab debug read -c sim:0 0x100 4\n  capturefab --json debug read -c gige:00c0df123456 0x0938\n  capturefab debug read -c CAMERA --feature 'LUTValueAll[LUTSelector=LUT1]' --timeout 5s"
    )]
    Read {
        #[command(flatten)]
        camera: CameraArg,
        /// Address: decimal or 0x hexadecimal
        #[arg(value_parser = parse_address, required_unless_present = "feature", conflicts_with = "feature")]
        address: Option<u64>,
        /// Bytes to read, 1..65536 (default 4 for numeric addresses)
        #[arg(value_parser = clap::value_parser!(u32).range(1..=65536), conflicts_with = "feature")]
        length: Option<u32>,
        /// XML-declared opaque Register, optionally at a declared selector bank
        #[arg(long, value_name = "NAME", value_parser = parse_feature, conflicts_with_all = ["address", "length"])]
        feature: Option<crate::controls::Name>,
        /// One budget for selection, metadata and byte transfer (named form only)
        #[arg(long, value_name = "DUR", value_parser = super::args::duration, requires = "feature", conflicts_with_all = ["address", "length"])]
        timeout: Option<Duration>,
    },
    /// Write raw camera memory
    #[command(
        after_help = "Examples:\n  capturefab debug write -c sim:0 0x100 80020000\n  capturefab --json debug write -c gige:00c0df123456 0x0a00 00000002\n  capturefab debug write -c CAMERA --feature FileAccessBuffer --hex 0080ff"
    )]
    Write {
        #[command(flatten)]
        camera: CameraArg,
        /// Address: decimal or 0x hexadecimal
        #[arg(value_parser = parse_address, required_unless_present = "feature", conflicts_with = "feature")]
        address: Option<u64>,
        /// Hex bytes, such as 00000001
        #[arg(required_unless_present = "feature", conflicts_with = "hex")]
        data: Option<String>,
        /// XML-declared opaque Register, optionally at a declared selector bank
        #[arg(long, value_name = "NAME", value_parser = parse_feature, requires = "hex", conflicts_with_all = ["address", "data"])]
        feature: Option<crate::controls::Name>,
        /// Exact raw bytes for the named register; no implicit commit or rollback.
        /// For large payloads, use debug rpc with JSON on stdin to avoid command-line limits.
        #[arg(long, value_name = "HEX", requires = "feature")]
        hex: Option<String>,
        /// One budget for selection, metadata and byte transfer (named form only)
        #[arg(long, value_name = "DUR", value_parser = super::args::duration, requires = "feature", conflicts_with_all = ["address", "data"])]
        timeout: Option<Duration>,
    },
    /// Send one session command, a JSON object on stdin, and print its reply
    #[command(
        after_help = "Examples:\n  echo '{\"op\":\"get_values\",\"names\":[{\"name\":\"Width\"}]}' | capturefab debug rpc -c sim:0\n  echo '{\"op\":\"status\"}' | capturefab --session bench debug rpc"
    )]
    Rpc {
        #[command(flatten)]
        camera: CameraArg,
    },
}

fn parse_address(s: &str) -> std::result::Result<u64, String> {
    s.strip_prefix("0x")
        .map_or_else(|| s.parse(), |v| u64::from_str_radix(v, 16))
        .map_err(|e| e.to_string())
}

fn parse_feature(text: &str) -> std::result::Result<crate::controls::Name, String> {
    crate::controls::Name::parse(text).map_err(|error| error.to_string())
}
pub(super) fn hex(s: &str) -> Result<Vec<u8>> {
    crate::types::decode_hex(s, 65536)
}
fn register_timeout(timeout: Option<Duration>) -> Result<u64> {
    let duration = timeout.unwrap_or(Duration::from_secs(5));
    if duration.is_zero() || duration > Duration::from_secs(30) {
        return Err(
            Usage("register timeout must be greater than zero and at most 30s".into()).into(),
        );
    }
    Ok(super::millis(duration).max(1))
}

pub(super) fn run(global: &Global, command: DebugCommand) -> Result<Option<Report>> {
    Ok(Some(match command {
        DebugCommand::Xml { camera, output } => {
            if output
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            {
                return Err(Usage("-o needs a file name".into()).into());
            }
            let client = Client::open(global, camera.camera.as_deref())?;
            let value = client.call(SessionCommand::Xml)?;
            let xml = value["xml"].as_str().context("missing XML")?;
            match output {
                Some(path) => {
                    let path = if path.is_absolute() {
                        path
                    } else {
                        std::env::current_dir()?.join(path)
                    };
                    crate::storage::save_one(xml.as_bytes(), &path, None)?;
                    let text = format!("Wrote {}\n", path.display());
                    Report::json(json!({"path": path, "bytes": xml.len()})).text(text)
                }
                None if !global.json => {
                    std::io::stdout().lock().write_all(xml.as_bytes())?;
                    return Ok(None);
                }
                None => Report::json(json!({"xml": xml})),
            }
        }
        DebugCommand::Read {
            camera,
            address,
            length,
            feature,
            timeout,
        } => {
            if let Some(name) = feature {
                let command = SessionCommand::ReadRegister {
                    name,
                    timeout_ms: register_timeout(timeout)?,
                };
                command.validate().map_err(super::input)?;
                let client = Client::open(global, camera.camera.as_deref())?;
                let value = client.call(command)?;
                return Ok(Some(Report::json(value.clone()).text(format!(
                    "{}: {}\n",
                    value["feature"].as_str().unwrap_or("register"),
                    value["hex"].as_str().unwrap_or("")
                ))));
            }
            let address = address.context("debug read needs an address or --feature")?;
            let command = SessionCommand::ReadMemory {
                address,
                length: length.unwrap_or(4) as usize,
            };
            command.validate().map_err(super::input)?;
            let client = Client::open(global, camera.camera.as_deref())?;
            let value = client.call(command)?;
            let bytes = crate::types::hex(
                value["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
                    .collect::<Vec<u8>>(),
            );
            Report::json(json!({"address": address, "hex": bytes}))
                .text(format!("0x{address:x}: {bytes}\n"))
        }
        DebugCommand::Write {
            camera,
            address,
            data,
            feature,
            hex: raw_hex,
            timeout,
        } => {
            if let Some(name) = feature {
                let command = SessionCommand::WriteRegister {
                    name,
                    hex: raw_hex.context("--feature write needs --hex")?,
                    timeout_ms: register_timeout(timeout)?,
                };
                command.validate().map_err(super::input)?;
                let client = Client::open(global, camera.camera.as_deref())?;
                let value = client.call(command)?;
                let text = format!(
                    "Wrote {} bytes to {}\n",
                    value["written"],
                    value["feature"].as_str().unwrap_or("register")
                );
                return Ok(Some(Report::json(value).text(text)));
            }
            let address = address.context("debug write needs an address or --feature")?;
            let command = SessionCommand::WriteMemory {
                address,
                data: hex(&data.context("debug write needs hexadecimal data")?)?,
            };
            command.validate().map_err(super::input)?;
            let client = Client::open(global, camera.camera.as_deref())?;
            let value = client.call(command)?;
            let text = format!("Wrote {} bytes at 0x{address:x}\n", value["written"]);
            Report::json(value).text(text)
        }
        DebugCommand::Rpc { camera } => {
            let mut data = Vec::new();
            std::io::stdin()
                .take(1024 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 1024 * 1024 {
                return Err(Usage("debug rpc reads at most 1 MiB from stdin".into()).into());
            }
            let mut command = SessionCommand::from_json_slice(&data).map_err(|error| {
                Usage(format!(
                    "stdin must be one session command JSON object ({error:#}); see capturefab schema debug rpc"
                ))
            })?;
            command
                .validate()
                .map_err(|error| Usage(format!("{error:#}")))?;
            if matches!(&command, SessionCommand::WriteActionDeviceKey { .. }) {
                return Err(Usage(
                    "debug rpc cannot assign ActionDeviceKey; use capturefab --session NAME set --write-only -c CAMERA ActionDeviceKey=U32 against an existing camera".into(),
                )
                .into());
            }
            if matches!(&command, SessionCommand::Capture { .. }) {
                return Err(Usage(
                    "debug rpc cannot send an image to stdout; save one with schedule".into(),
                )
                .into());
            }
            if let SessionCommand::Connect { camera, .. } = &mut command {
                *camera = super::caller_selector(camera)?;
            }
            if let Some(selector) = &camera.camera {
                crate::session::monitor::validate_for_selector(selector, &command)?;
            }
            super::paths::resolve_outputs(&mut command, &std::env::current_dir()?)
                .map_err(|error| Usage(format!("{error:#}")))?;
            let value = match (&camera.camera, &global.session) {
                (Some(_), _) | (None, Some(_))
                    if !matches!(command, SessionCommand::Connect { .. }) =>
                {
                    let client = Client::open(global, camera.camera.as_deref())?;
                    client.call(command)?
                }
                (_, Some(session)) => crate::ipc::call(
                    session,
                    command.clone(),
                    crate::session::default_request_timeout(&command),
                )?,
                _ => {
                    let local = SessionHandle::new();
                    let result = local.request(command);
                    local.shutdown();
                    result?
                }
            };
            Report::json(value)
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_addresses_parse() {
        assert_eq!(hex("00aaff").unwrap(), vec![0, 170, 255]);
        hex("0").unwrap_err();
        hex("💡").unwrap_err();
        assert_eq!(parse_address("0x100").unwrap(), 256);
    }
    #[test]
    fn named_debug_forms_are_explicit_and_numeric_forms_stay_unchanged() {
        use clap::Parser as _;
        #[derive(Debug, clap::Parser)]
        struct Args {
            #[command(subcommand)]
            command: DebugCommand,
        }
        let parsed = Args::try_parse_from(["debug", "read", "0x100"]).unwrap();
        assert!(matches!(
            parsed.command,
            DebugCommand::Read {
                address: Some(256),
                length: None,
                feature: None,
                ..
            }
        ));
        let parsed = Args::try_parse_from(["debug", "write", "0x100", "0080ff"]).unwrap();
        assert!(matches!(
            parsed.command,
            DebugCommand::Write {
                address: Some(256),
                feature: None,
                ..
            }
        ));
        for args in [
            vec![
                "debug",
                "read",
                "--feature",
                "R[Bank=B]",
                "--timeout",
                "50ms",
            ],
            vec!["debug", "write", "--feature", "R", "--hex", "0080ff"],
        ] {
            Args::try_parse_from(args).unwrap();
        }
        for args in [
            vec!["debug", "read"],
            vec!["debug", "read", "--feature", "R", "0x100"],
            vec!["debug", "read", "--timeout", "1s", "0x100"],
            vec!["debug", "write", "--timeout", "1s", "0x100", "ff"],
            vec!["debug", "write", "--feature", "R"],
            vec!["debug", "write", "--hex", "00", "0x100", "ff"],
            vec!["debug", "write", "--feature", "R", "--hex", "00", "0x100"],
        ] {
            assert_eq!(Args::try_parse_from(args).unwrap_err().exit_code(), 2);
        }
        assert_eq!(register_timeout(None).unwrap(), 5000);
        assert_eq!(register_timeout(Some(Duration::from_millis(1))).unwrap(), 1);
        register_timeout(Some(Duration::ZERO)).unwrap_err();
        register_timeout(Some(Duration::from_secs(31))).unwrap_err();
        hex(&"00".repeat(65537)).unwrap_err();
    }
}
