//! Existing-only WO configuration; never opens/connects a camera as a fallback.
use super::{Global, Report, SessionCommand, SetArgs};
use crate::types::Usage;
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

fn command<'a>(
    global: &'a Global,
    args: &'a SetArgs,
) -> Result<(&'a str, &'a str, SessionCommand)> {
    ensure!(args.write_only && !args.dry_run && args.values.len()==1 && (args.keep.keep.is_none() || global.keep_from_env) && !global.no_session && !global.session_from_env && args.camera_explicit,
        Usage("--write-only requires exactly ActionDeviceKey=U32, an explicit existing --session and -c; no --dry-run, explicit --keep, environment-only session/camera or --no-session".into()));
    let session = global
        .session
        .as_deref()
        .ok_or_else(|| Usage("--write-only requires an explicit existing --session".into()))?;
    let camera = args
        .camera
        .camera
        .as_deref()
        .ok_or_else(|| Usage("--write-only requires an explicit existing camera".into()))?;
    let (name, value) = args.values[0]
        .split_once('=')
        .ok_or_else(|| Usage("expected ActionDeviceKey=U32".into()))?;
    ensure!(
        name == "ActionDeviceKey",
        Usage("--write-only supports exactly ActionDeviceKey without banks or aliases".into())
    );
    #[derive(Deserialize)]
    struct Key {
        #[serde(deserialize_with = "crate::config_number::unsigned")]
        key: u32,
    }
    let key_value: Value = serde_json::from_str(value).map_err(|_| {
        Usage("ActionDeviceKey must be an exact unsigned 32-bit JSON number".into())
    })?;
    let key: Key = serde_json::from_value(serde_json::json!({"key":key_value}))
        .map_err(|error| Usage(error.to_string()))?;
    let timeout = args.write_timeout.unwrap_or(Duration::from_secs(3));
    ensure!(
        !timeout.is_zero() && timeout <= Duration::from_secs(30),
        Usage("write timeout must be greater than 0 and at most 30s".into())
    );
    ensure!(
        timeout.subsec_nanos().is_multiple_of(1_000_000),
        Usage("write timeout must be an exact number of milliseconds".into())
    );
    let timeout_ms = u64::try_from(timeout.as_millis())?;
    let command = SessionCommand::WriteActionDeviceKey {
        key: key.key,
        timeout_ms,
    };
    command.validate()?;
    crate::session::validate_selector(camera)?;
    crate::ipc::validate_name(session)?;
    Ok((session, camera, command))
}

pub(super) fn set(global: &Global, args: &SetArgs) -> Result<Report> {
    let (session, camera, command) = command(global, args)?;
    let timeout = crate::session::default_request_timeout(&command);
    let value = crate::ipc::call_to(session, Some(camera), command, timeout).map_err(|error| {
        if matches!(
            crate::types::classify(&error).kind,
            crate::types::ErrorKind::SessionNotFound | crate::types::ErrorKind::Usage
        ) {
            error
        } else {
            crate::session::action_key::uncertain(error)
        }
    })?;
    Ok(Report::json(value).text("Assigned ActionDeviceKey in the current control epoch; no readback or rollback is available.\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        Cli, Command,
        args::{CameraArg, KeepArg},
    };
    use clap::Parser as _;

    fn input() -> (Global, SetArgs) {
        (
            Global {
                json: false,
                quiet: false,
                verbose: false,
                session: Some("existing".into()),
                no_session: false,
                session_from_env: false,
                keep_from_env: false,
            },
            SetArgs {
                camera: CameraArg {
                    camera: Some("sim:0".into()),
                },
                values: vec!["ActionDeviceKey=4294967295".into()],
                dry_run: false,
                write_only: true,
                write_timeout: None,
                camera_explicit: true,
                keep: KeepArg::default(),
            },
        )
    }

    #[test]
    fn exact_key_and_explicit_existing_routing_are_admitted_without_opening_a_client() {
        let (global, args) = input();
        let (session, camera, c) = command(&global, &args).unwrap();
        assert_eq!((session, camera), ("existing", "sim:0"));
        assert!(matches!(
            c,
            SessionCommand::WriteActionDeviceKey {
                key: u32::MAX,
                timeout_ms: 3000
            }
        ));
        for invalid in [
            "actiondevicekey=1",
            "ActionDeviceKey[1]=1",
            "ActionGroupKey=1",
            "exposure=1",
            "ActionDeviceKey=-1",
            "ActionDeviceKey=4294967296",
            "ActionDeviceKey=1.00000000000000000001",
            "ActionDeviceKey=true",
            "ActionDeviceKey=\"1\"",
        ] {
            let (g, mut a) = input();
            a.values = vec![invalid.into()];
            command(&g, &a).expect_err(invalid);
        }
        for case in 0..10 {
            let (mut g, mut a) = input();
            match case {
                0 => g.session = None,
                1 => g.session_from_env = true,
                2 => g.no_session = true,
                3 => a.camera.camera = None,
                4 => a.camera_explicit = false,
                5 => a.dry_run = true,
                6 => a.keep.keep = Some(Duration::ZERO),
                7 => g.session = Some("invalid/name".into()),
                8 => a.values.push("ActionDeviceKey=2".into()),
                9 => a.write_only = false,
                _ => unreachable!(),
            }
            command(&g, &a).unwrap_err();
        }
        // A dedicated existing-owner call never consumes ambient keep.
        let (mut g, mut a) = input();
        g.keep_from_env = true;
        a.keep.keep = Some(Duration::ZERO);
        command(&g, &a).unwrap();
        for timeout in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_millis(30_001),
        ] {
            let (g, mut a) = input();
            a.write_timeout = Some(timeout);
            command(&g, &a).unwrap_err();
        }
    }

    #[test]
    fn clap_keeps_rw_assignments_and_requires_wo_for_the_new_timeout() {
        let cli =
            Cli::try_parse_from(["capturefab", "set", "--dry-run", "ExposureTime=1"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Set(SetArgs {
                write_only: false,
                dry_run: true,
                ..
            }))
        ));
        Cli::try_parse_from([
            "capturefab",
            "set",
            "--write-timeout",
            "1s",
            "ExposureTime=1",
        ])
        .unwrap_err();
        let cli = Cli::try_parse_from([
            "capturefab",
            "--session",
            "existing",
            "set",
            "--write-only",
            "-c",
            "sim:0",
            "ActionDeviceKey=1",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Set(SetArgs {
                write_only: true,
                ..
            }))
        ));
    }
}
