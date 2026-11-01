//! `capturefab debug`: protocol tools for people debugging a camera.
use super::{Client, Global, args::CameraArg, output::Report};
use crate::session::{SessionCommand, SessionHandle};
use crate::types::Usage;
use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::json;
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// Print a camera's GenICam XML, or write it to a file
    #[command(
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
        after_help = "Examples:\n  capturefab debug read -c sim:0 0x100 4\n  capturefab --json debug read -c gige:00c0df123456 0x0938"
    )]
    Read {
        #[command(flatten)]
        camera: CameraArg,
        /// Address: decimal or 0x hexadecimal
        #[arg(value_parser = parse_address)]
        address: u64,
        /// Bytes to read, 1..65536
        #[arg(default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=65536))]
        length: u32,
    },
    /// Write raw camera memory
    #[command(
        after_help = "Examples:\n  capturefab debug write -c sim:0 0x100 80020000\n  capturefab --json debug write -c gige:00c0df123456 0x0a00 00000002"
    )]
    Write {
        #[command(flatten)]
        camera: CameraArg,
        /// Address: decimal or 0x hexadecimal
        #[arg(value_parser = parse_address)]
        address: u64,
        /// Hex bytes, such as 00000001
        data: String,
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
    if let Some(v) = s.strip_prefix("0x") {
        u64::from_str_radix(v, 16)
    } else {
        s.parse()
    }
    .map_err(|e| e.to_string())
}

fn hex(s: &str) -> Result<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(Usage("hex data needs an even number of ASCII digits".into()).into());
    }
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            u8::from_str_radix(std::str::from_utf8(p)?, 16)
                .map_err(|_| Usage(format!("'{s}' is not hexadecimal")).into())
        })
        .collect()
}

/// Commands `debug rpc` refuses: worker-internal ones.
fn public(command: &SessionCommand) -> bool {
    !matches!(
        command,
        SessionCommand::Grab { .. } | SessionCommand::OptimizeNetwork { .. }
    )
}

pub fn run(global: &Global, command: DebugCommand) -> Result<Option<Report>> {
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
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map_err(|error| {
                            if error.kind() == std::io::ErrorKind::AlreadyExists {
                                crate::storage::OutputExists(path.clone()).into()
                            } else {
                                anyhow::Error::from(error)
                            }
                        })?;
                    file.write_all(xml.as_bytes())?;
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
        } => {
            let command = SessionCommand::ReadMemory {
                address,
                length: length as usize,
            };
            command.validate().map_err(super::input)?;
            let client = Client::open(global, camera.camera.as_deref())?;
            let value = client.call(command)?;
            let bytes = value["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b.as_u64())
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            Report::json(json!({"address": address, "hex": bytes}))
                .text(format!("0x{address:x}: {bytes}\n"))
        }
        DebugCommand::Write {
            camera,
            address,
            data,
        } => {
            let command = SessionCommand::WriteMemory {
                address,
                data: hex(&data)?,
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
            if !public(&command) {
                return Err(Usage("that command belongs to camera workers".into()).into());
            }
            if matches!(&command, SessionCommand::Capture { output, .. } if output == "-") {
                return Err(Usage("debug rpc captures need an output file".into()).into());
            }
            if let SessionCommand::Connect { camera, .. } = &mut command {
                *camera = super::caller_selector(camera)?;
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
        assert!(hex("0").is_err());
        assert!(hex("💡").is_err());
        assert_eq!(parse_address("0x100").unwrap(), 256);
        assert!(!public(&SessionCommand::Grab { timeout_ms: 1 }));
        assert!(public(&SessionCommand::Status));
    }
}
