//! `capturefab login`: camera sign-ins saved in the OS credential store.
use super::{Global, output::Report};
use crate::logins::{self, CameraLogin, Logins};
use crate::session::{SessionCommand, SessionHandle};
use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::Duration;

pub(crate) const PASSWORD_ENV: &str = "CAPTUREFAB_CAMERA_PASSWORD";

#[derive(Debug, Subcommand)]
pub(crate) enum LoginCommand {
    /// Check a camera sign-in, then save it in the OS credential store
    #[command(
        after_help = "Examples:\n  printf '%s\\n' \"$PASS\" | capturefab login add rtsp://cam.local/stream --user admin --password-stdin\n  printf '%s\\n' \"$PASS\" | capturefab login add onvif://cam.local --user admin --password-stdin"
    )]
    Add {
        /// A credential-free `rtsp://`, `rtsps://`, `onvif:http://…` or `onvif://HOST` address
        #[arg(
            help = "A credential-free rtsp://, rtsps://, onvif:http://… or onvif://HOST address"
        )]
        address: String,
        /// Camera user name
        #[arg(long)]
        user: Option<String>,
        /// Read the password from stdin (an empty line is an empty password); otherwise `CAPTUREFAB_CAMERA_PASSWORD`
        #[arg(
            help = "Read the password from stdin (an empty line is an empty password); otherwise CAPTUREFAB_CAMERA_PASSWORD",
            long
        )]
        password_stdin: bool,
        /// Save without connecting to the camera first
        #[arg(long)]
        no_check: bool,
    },
    /// List saved camera sign-ins (never their passwords)
    #[command(after_help = "Examples:\n  capturefab login list\n  capturefab --json login list")]
    List,
    /// Forget a saved camera sign-in and delete its password
    #[command(
        visible_alias = "remove",
        after_help = "Examples:\n  capturefab login rm rtsp://cam.local/stream\n  capturefab login rm onvif://cam.local"
    )]
    Rm { address: String },
}

/// How long `login add` waits for the camera it checks.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

fn usage(message: impl Into<String>) -> anyhow::Error {
    crate::types::Usage(message.into()).into()
}

/// Checks that need neither stdin, the credential store nor a camera.
fn prepare(command: &LoginCommand) -> Result<()> {
    if let LoginCommand::Add {
        address,
        user,
        password_stdin,
        ..
    } = command
    {
        if address.contains("://") || crate::onvif::is_source(address) {
            logins::key(address)?;
        }
        if user.as_deref().is_none_or(str::is_empty) {
            return Err(usage("give the camera user name with --user USER"));
        }
        let env = std::env::var_os(PASSWORD_ENV).is_some();
        if *password_stdin && env {
            return Err(usage(format!(
                "give the password with --password-stdin or {PASSWORD_ENV}, not both"
            )));
        }
        if !*password_stdin && !env {
            return Err(usage(format!(
                "give the password with --password-stdin or {PASSWORD_ENV}"
            )));
        }
    }
    if let LoginCommand::Rm { address } = command
        && (address.contains("://") || crate::onvif::is_source(address))
    {
        logins::key(address)?;
    }
    Ok(())
}

/// The address a sign-in is saved under; with a session, a bare name may be
/// the ID or serial of an ONVIF camera it discovered or connected.
fn resolve(address: &str, session: Option<&str>, timeout: Duration) -> Result<String> {
    Ok(logins::key(&target(address, session, timeout)?)?.address)
}

/// The credential-free address to connect, as given (query and all), or the
/// ID of the discovered ONVIF camera it names.
fn target(address: &str, session: Option<&str>, timeout: Duration) -> Result<String> {
    if logins::key(address).is_ok() {
        return Ok(address.trim().to_owned());
    }
    if let Some(name) = session
        && !address.contains("://")
    {
        let state = crate::ipc::call(name, SessionCommand::Status, timeout)?;
        let devices = state["devices"].as_array().into_iter().flatten().chain(
            state["cameras"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|camera| &camera["info"]),
        );
        for device in devices {
            let id = device["id"].as_str().unwrap_or_default();
            if crate::onvif::is_source(id)
                && (id == address || device["serial"].as_str() == Some(address))
            {
                logins::key(id)?;
                return Ok(id.to_owned());
            }
        }
    }
    logins::key(address)?;
    Ok(address.to_owned())
}

fn password(stdin: bool) -> Result<zeroize::Zeroizing<String>> {
    if stdin {
        return crate::secret_input::read_stdin_password(
            std::io::stdin().lock(),
            "camera password",
        );
    }
    let value =
        std::env::var(PASSWORD_ENV).map_err(|_| usage(format!("{PASSWORD_ENV} must be UTF-8")))?;
    Ok(zeroize::Zeroizing::new(value))
}

pub(crate) fn run(global: &Global, command: &LoginCommand) -> Result<Report> {
    let value = run_with(command, global.session.as_deref(), CHECK_TIMEOUT)?;
    let text = human(command, &value);
    Ok(Report::json(value).text(text))
}

fn run_with(command: &LoginCommand, session: Option<&str>, timeout: Duration) -> Result<Value> {
    prepare(command)?;
    match command {
        LoginCommand::List => {
            let logins = Logins::system().list()?;
            Ok(json!({"logins": logins, "supported": logins::can_remember()}))
        }
        LoginCommand::Rm { address } => {
            if !logins::can_remember() {
                return Err(unsupported());
            }
            let address = resolve(address, session, timeout)?;
            let forgotten = Logins::system().forget(&address)?;
            Ok(json!({"removed": address, "secret_forgotten": forgotten}))
        }
        LoginCommand::Add {
            address,
            user,
            password_stdin,
            no_check,
        } => {
            if !logins::can_remember() {
                return Err(unsupported());
            }
            // The check opens the address as given; the key only names
            // the saved entry.
            let target = target(address, session, timeout)?;
            let address = logins::key(&target)?.address;
            let password = password(*password_stdin)?;
            let login = CameraLogin::new(user.clone().unwrap_or_default(), password.as_str())?;
            drop(password);
            let check = if *no_check {
                None
            } else {
                let _checking = super::term::activity("checking the sign-in…");
                Some(check(&target, &login, timeout)?)
            };
            let saved = Logins::system().save(&address, &login)?;
            let mut result = json!({"login": saved.entry, "replaced": saved.replaced});
            if let Some(camera) = check {
                result["check"] = json!({"ok": true, "camera": camera});
            }
            Ok(result)
        }
    }
}

fn unsupported() -> anyhow::Error {
    crate::types::Categorized::error(
        crate::types::ErrorKind::Unsupported,
        "saving camera passwords in the OS credential store is unsupported in this build; put the credentials in the address instead",
    )
}

/// Connect once with the sign-in in a fresh local session, then disconnect.
fn check(address: &str, login: &CameraLogin, timeout: Duration) -> Result<Value> {
    let handle = SessionHandle::new();
    let result = handle.request(SessionCommand::Connect {
        camera: address.to_owned(),
        timeout_ms: super::millis(timeout).max(1),
        login: Some(login.clone()),
        saved_login: false,
    });
    let result = result.map(|connected| {
        let _ = handle.request(SessionCommand::Disconnect);
        connected["connected"].clone()
    });
    handle.shutdown();
    result
}

/// Human-readable output.
pub(crate) fn human(command: &LoginCommand, value: &Value) -> String {
    match command {
        LoginCommand::List => {
            let logins = value["logins"].as_array().map_or(&[][..], Vec::as_slice);
            if logins.is_empty() {
                return if value["supported"] == true {
                    "No saved camera sign-ins. Add one with `capturefab login add ADDRESS --user USER --password-stdin`.\n".into()
                } else {
                    "Saving camera sign-ins is unsupported in this build.\n".into()
                };
            }
            let mut text = String::new();
            for entry in logins {
                let _ = writeln!(
                    text,
                    "{}  {}  saved {}",
                    entry["address"].as_str().unwrap_or_default(),
                    entry["user"].as_str().unwrap_or_default(),
                    entry["saved_at"].as_str().unwrap_or_default()
                );
            }
            text
        }
        LoginCommand::Add { .. } => format!(
            "{} sign-in for {} as {}\n",
            if value["replaced"] == true {
                "Replaced the"
            } else {
                "Saved a"
            },
            value["login"]["address"].as_str().unwrap_or_default(),
            value["login"]["user"].as_str().unwrap_or_default()
        ),
        LoginCommand::Rm { .. } => format!(
            "Forgot the sign-in for {}\n",
            value["removed"].as_str().unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_opens_the_address_as_given_and_the_key_names_the_entry() {
        let timeout = Duration::from_secs(1);
        for (given, key) in [
            (
                "rtsp://cam.local/cam/realmonitor?channel=1&subtype=0",
                "rtsp://cam.local:554/cam/realmonitor",
            ),
            (
                " rtsps://Cam.local:8322/live#x ",
                "rtsps://cam.local:8322/live",
            ),
        ] {
            let target = target(given, None, timeout).unwrap();
            assert_eq!(target, given.trim(), "the query reaches the camera");
            assert_eq!(resolve(given, None, timeout).unwrap(), key);
        }
        let onvif = "onvif://cam.local?mode=media";
        assert_eq!(target(onvif, None, timeout).unwrap(), onvif);
        target("http://cam.local/x", None, timeout).unwrap_err();
    }
}
