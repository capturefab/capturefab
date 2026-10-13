//! Commands for the scanner APIs installed on each desktop platform.
//!
//! Image bytes and device IDs stay on pipes/arguments, never in shell syntax.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::process::Command;

pub(super) fn command(
    operation: &str,
    device: Option<&str>,
    dpi: u32,
    mode: &str,
    source: &str,
) -> Result<Command> {
    ensure!(
        matches!(operation, "discover" | "scan"),
        "unknown scanner operation {operation}"
    );
    let mode = match mode.to_ascii_lowercase().as_str() {
        "color" => "Color",
        "gray" => "Gray",
        "lineart" => "Lineart",
        _ => bail!("unsupported scanner mode {mode}; choose Color, Gray, or Lineart"),
    };
    let source = match source.to_ascii_lowercase().as_str() {
        "default" => "Default",
        "flatbed" => "Flatbed",
        "feeder" => "Feeder",
        _ => bail!("unsupported scanner source {source}; choose Default, Flatbed, or Feeder"),
    };
    if operation == "scan" {
        ensure!(dpi > 0, "scanner resolution must be greater than zero");
        ensure!(
            device.is_some_and(|id| !id.is_empty()),
            "a scanner device ID is required"
        );
    }
    platform_command(operation, device, dpi, mode, source)
}

#[cfg(target_os = "linux")]
fn platform_command(
    operation: &str,
    device: Option<&str>,
    dpi: u32,
    mode: &str,
    source: &str,
) -> Result<Command> {
    Ok(sane_command(
        scanimage_program(),
        operation,
        device,
        dpi,
        mode,
        source,
    ))
}

// Compiled in tests on other hosts too, so the SANE protocol can be verified
// without installing a scanner driver or touching hardware.
#[cfg(any(target_os = "linux", test))]
fn sane_command(
    program: impl AsRef<std::ffi::OsStr>,
    operation: &str,
    device: Option<&str>,
    dpi: u32,
    mode: &str,
    source: &str,
) -> Command {
    let mut command = Command::new(program);
    if operation == "discover" {
        // SANE's %n is an actual newline; tabs delimit ID/vendor/model/type.
        command.arg("--formatted-device-list=%d\t%v\t%m\t%t%n");
    } else {
        command.args([
            "--device-name",
            device.unwrap_or_default(),
            "--format=pnm",
            "--resolution",
        ]);
        command.arg(dpi.to_string()).args(["--mode", mode]);
        match source {
            "Flatbed" => {
                command.args(["--source", "Flatbed"]);
            }
            "Feeder" => {
                command.args(["--source", "Automatic Document Feeder"]);
            }
            // Driver defaults also work with scanners whose backend has no
            // source option (for example a scanner that only has a flatbed).
            _ => {}
        }
    }
    command
}

#[cfg(target_os = "linux")]
fn scanimage_program() -> std::ffi::OsString {
    std::env::var_os("CAPTUREFAB_SCANIMAGE").unwrap_or_else(|| "scanimage".into())
}

#[cfg(target_os = "windows")]
fn platform_command(
    operation: &str,
    device: Option<&str>,
    dpi: u32,
    mode: &str,
    source: &str,
) -> Result<Command> {
    Ok(wia_command(
        powershell_program(),
        operation,
        device,
        dpi,
        mode,
        source,
    ))
}

#[cfg(any(target_os = "windows", test))]
fn wia_command(
    program: impl AsRef<std::ffi::OsStr>,
    operation: &str,
    device: Option<&str>,
    dpi: u32,
    mode: &str,
    source: &str,
) -> Command {
    use base64::Engine;
    let script: Vec<u8> = include_str!("wia.ps1")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut command = Command::new(program);
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Sta",
        "-ExecutionPolicy",
        "Bypass",
        "-EncodedCommand",
    ]);
    command.arg(base64::engine::general_purpose::STANDARD.encode(script));
    // Values are data, including IDs containing quotes or PowerShell syntax.
    command.env("CAPTUREFAB_WIA_OPERATION", operation);
    command.env("CAPTUREFAB_WIA_DEVICE", device.unwrap_or_default());
    command.env("CAPTUREFAB_WIA_DPI", dpi.to_string());
    command.env("CAPTUREFAB_WIA_MODE", mode);
    command.env("CAPTUREFAB_WIA_SOURCE", source);
    command
}

#[cfg(target_os = "windows")]
fn powershell_program() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
        .unwrap_or_else(|| "powershell.exe".into())
}

#[cfg(target_os = "macos")]
fn platform_command(
    operation: &str,
    device: Option<&str>,
    dpi: u32,
    mode: &str,
    source: &str,
) -> Result<Command> {
    let mut command = Command::new(super::macos_executable()?);
    command.arg(operation);
    if operation == "scan" {
        command
            .arg(device.unwrap_or_default())
            .arg(dpi.to_string())
            .args([mode, source]);
    }
    Ok(command)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn platform_command(_: &str, _: Option<&str>, _: u32, _: &str, _: &str) -> Result<Command> {
    bail!(
        "document scanning is supported on Linux (SANE), Windows (WIA), and macOS (ImageCaptureCore)"
    )
}

pub(super) fn discovery(bytes: &[u8]) -> Result<Vec<super::Device>> {
    #[cfg(target_os = "linux")]
    {
        sane_discovery(bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        json_discovery(bytes)
    }
}

#[cfg(any(not(target_os = "linux"), test))]
fn json_discovery(bytes: &[u8]) -> Result<Vec<super::Device>> {
    // Windows' helper writes UTF-8 directly; accept a BOM from older helpers.
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    serde_json::from_slice(bytes).context("scanner helper returned an invalid device list")
}

#[cfg(any(target_os = "linux", test))]
fn sane_discovery(bytes: &[u8]) -> Result<Vec<super::Device>> {
    let text =
        std::str::from_utf8(bytes).context("SANE returned a device list that is not UTF-8")?;
    let mut devices = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        ensure!(
            fields.len() == 4 && !fields[0].is_empty(),
            "SANE returned an invalid device entry on line {}",
            index + 1
        );
        let name = [fields[1].trim(), fields[2].trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        devices.push(super::Device {
            id: fields[0].to_owned(),
            name: if name.is_empty() {
                fields[0].to_owned()
            } else {
                name
            },
            vendor: fields[1].trim().to_owned(),
            serial: String::new(),
        });
    }
    Ok(devices)
}

pub(super) fn available() -> Value {
    #[cfg(target_os = "linux")]
    {
        let present = program_exists(&scanimage_program());
        json!({"supported": true, "available": present, "backend": "SANE", "detail": if present {
            "SANE scanimage is installed; device support depends on the installed SANE backends"
        } else {
            "Install SANE's scanimage (usually the sane-utils package), or set CAPTUREFAB_SCANIMAGE to its executable"
        }})
    }
    #[cfg(target_os = "windows")]
    {
        let present = program_exists(powershell_program().as_os_str());
        json!({"supported": true, "available": present, "backend": "WIA", "detail": if present {
            "Windows Image Acquisition; install the scanner's WIA driver and enable the Windows Image Acquisition service"
        } else {
            "Windows PowerShell is required to access Windows Image Acquisition scanners"
        }})
    }
    #[cfg(target_os = "macos")]
    {
        json!({"supported": true, "available": true, "backend": "ImageCaptureCore", "detail":
            "Bundled ImageCaptureCore helper; device support depends on the installed macOS scanner drivers"})
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        json!({"supported": false, "available": false, "backend": null, "detail": "Document scanning is supported on Linux, Windows, and macOS"})
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn program_exists(program: &std::ffi::OsStr) -> bool {
    let path = std::path::Path::new(program);
    let usable = |path: &std::path::Path| {
        let Ok(metadata) = path.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    };
    if path.is_absolute() || path.components().count() > 1 {
        return usable(path);
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| usable(&directory.join(program)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sane_device_ids_are_preserved() {
        let devices = sane_discovery(b"airscan:e0:Office|Scanner\tACME\tScan 3000\tflatbed scanner\nnet:192.0.2.4:usb:1\t\t\tsheetfed scanner\n").unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "airscan:e0:Office|Scanner");
        assert_eq!(devices[0].name, "ACME Scan 3000");
        assert_eq!(devices[1].name, devices[1].id);
    }

    #[test]
    fn sane_rejects_ambiguous_or_invalid_records() {
        assert!(sane_discovery(b"a\tb\tc\td\textra\n").is_err());
        assert!(sane_discovery(b"\tvendor\tmodel\ttype\n").is_err());
        assert!(sane_discovery(b"\xff\t\t\t\n").is_err());
        assert!(sane_discovery(b"\n").unwrap().is_empty());
    }

    #[test]
    fn sane_command_keeps_device_ids_as_one_argument_and_default_source_unset() {
        let command = sane_command(
            "scanimage",
            "scan",
            Some("scanner ' ; $(foo)"),
            300,
            "Color",
            "Default",
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--device-name",
                "scanner ' ; $(foo)",
                "--format=pnm",
                "--resolution",
                "300",
                "--mode",
                "Color"
            ]
        );
        let feeder = sane_command("scanimage", "scan", Some("scanner"), 600, "Gray", "Feeder");
        assert_eq!(
            feeder.get_args().last().unwrap(),
            "Automatic Document Feeder"
        );
    }

    #[test]
    fn json_device_list_handles_empty_lists_and_bom() {
        assert!(json_discovery(b"[]").unwrap().is_empty());
        let devices =
            json_discovery(b"\xef\xbb\xbf[{\"id\":\"device\",\"name\":\"Scanner\"}]").unwrap();
        assert_eq!(devices[0].id, "device");
        assert!(devices[0].vendor.is_empty());
        assert!(json_discovery(b"not JSON").is_err());
    }

    #[test]
    fn wia_parameters_never_become_powershell_source() {
        use base64::Engine;
        let id = "scanner ' ; $(foo) / \u{30b9}\u{30ad}\u{30e3}\u{30ca}";
        let command = wia_command("powershell.exe", "scan", Some(id), 300, "Color", "Default");
        let args: Vec<_> = command.get_args().collect();
        let encoded = args.last().unwrap().to_str().unwrap();
        let script = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let utf16: Vec<_> = script
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16(&utf16).unwrap(), include_str!("wia.ps1"));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "CAPTUREFAB_WIA_DEVICE"
                    && value == Some(std::ffi::OsStr::new(id)))
        );
        // Windows CreateProcess limits the command line to 32,767 UTF-16 units.
        assert!(
            args.iter()
                .map(|arg| arg.to_string_lossy().encode_utf16().count() + 3)
                .sum::<usize>()
                < 32_000
        );
    }
}
