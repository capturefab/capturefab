//! Reject explicit monitor operations before opening a worker or preparing artifacts.
use super::{Command, debug::DebugCommand, preset::PresetCommand};
use crate::session::monitor::{is_selector, reject_selector, unsupported};
use anyhow::Result;
use std::time::Duration;

fn acquisition(
    selector: &str,
    settings: bool,
    auto: bool,
    warmup: Option<Duration>,
    fresh: u32,
) -> Result<()> {
    if is_selector(selector)?
        && (settings || auto || warmup.is_some_and(|warmup| !warmup.is_zero()) || fresh > 0)
    {
        return Err(unsupported());
    }
    Ok(())
}

pub(super) fn preflight(command: &Command) -> Result<()> {
    match command {
        Command::Set(args) => reject_selector(args.camera.camera.as_deref()),
        Command::Exec(args) => reject_selector(args.camera.camera.as_deref()),
        Command::Auto(args) => reject_selector(args.camera.camera.as_deref()),
        Command::Get(args) => {
            let banked = args.names.iter().try_fold(false, |banked, text| {
                crate::controls::Name::parse(text).map(|name| banked || !name.bank.is_empty())
            })?;
            for selector in &args.cameras.cameras {
                if is_selector(selector)? && banked {
                    return Err(unsupported());
                }
            }
            Ok(())
        }
        Command::Capture(args) => {
            for selector in &args.cameras.cameras {
                acquisition(
                    selector,
                    !args.settings.is_empty(),
                    args.auto,
                    args.warmup,
                    args.fresh,
                )?;
            }
            Ok(())
        }
        Command::Record(args) => {
            if let Some(selector) = &args.camera.camera {
                acquisition(
                    selector,
                    !args.settings.is_empty(),
                    args.auto,
                    args.warmup,
                    0,
                )?;
            }
            Ok(())
        }
        Command::Inspect(args) => {
            for selector in &args.cameras.cameras {
                acquisition(selector, !args.settings.is_empty(), false, args.warmup, 0)?;
            }
            Ok(())
        }
        Command::Preset(
            PresetCommand::Save { camera, .. } | PresetCommand::Apply { camera, .. },
        )
        | Command::Debug(DebugCommand::Write { camera, .. }) => {
            reject_selector(camera.camera.as_deref())
        }
        Command::Debug(DebugCommand::Read {
            camera,
            feature: Some(name),
            ..
        }) => {
            if !name.bank.is_empty() {
                reject_selector(camera.camera.as_deref())?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    const CAMERA: &str = "gige-monitor://127.0.0.1?interface=127.0.0.1&mac=020000000001";

    #[test]
    fn explicit_monitor_mutations_are_rejected_without_opening_any_camera() {
        for words in [
            vec!["capture", "--set", "Width=4", "-o", "image.raw"],
            vec!["capture", "--auto", "-o", "image.png"],
            vec!["capture", "--warmup", "1ms", "-o", "image.png"],
            vec!["capture", "--fresh", "1", "-o", "image.png"],
            vec!["record", "--auto", "-o", "video.mkv"],
            vec!["record", "--warmup", "2s", "-o", "video.mkv"],
            vec!["inspect", "--set", "Width=4"],
            vec!["set", "Width=4", "--dry-run"],
            vec!["exec", "AcquisitionStart"],
            vec!["auto", "--off"],
            vec!["get", "TriggerMode[FrameStart]"],
            vec!["debug", "read", "--feature", "LUTValueAll[LUT1]"],
            vec!["debug", "write", "0x100", "00000001"],
            vec!["preset", "save", "named"],
            vec!["preset", "apply", "named", "--dry-run"],
        ] {
            let argv = [vec!["capturefab"], words, vec!["-c", CAMERA]].concat();
            let cli = super::super::Cli::try_parse_from(argv).unwrap();
            assert_eq!(
                crate::types::classify(&preflight(&cli.command.unwrap()).unwrap_err()).kind,
                crate::types::ErrorKind::Unsupported
            );
        }
        for words in [
            vec!["capture", "-o", "image.raw"],
            vec!["capture", "--warmup", "0", "-o", "image.raw"],
            vec!["record", "-o", "video.mkv", "--fps", "12"],
            vec!["inspect"],
            vec!["get", "Width"],
            vec!["debug", "read", "0x100", "4"],
        ] {
            let argv = [vec!["capturefab"], words, vec!["-c", CAMERA]].concat();
            let cli = super::super::Cli::try_parse_from(argv).unwrap();
            preflight(&cli.command.unwrap()).unwrap();
        }
    }
}
