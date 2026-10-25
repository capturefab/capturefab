//! `capturefab login`: camera sign-ins saved in the OS credential store.
use crate::logins::{self, CameraLogin, Logins};
use crate::session::{SessionCommand, SessionHandle};
use anyhow::Result;
use clap::Subcommand;
use serde_json::{Value, json};
use std::time::Duration;

pub const PASSWORD_ENV: &str = "CAPTUREFAB_CAMERA_PASSWORD";

#[derive(Debug, Subcommand)]
pub enum LoginCommand {
    /// Check a camera sign-in, then save it in the OS credential store
    Add {
        /// A credential-free rtsp://, rtsps://, onvif:http://… or onvif://HOST address, or with --session the ID or serial of a discovered ONVIF camera
        address: String,
        /// Camera user name
        #[arg(long)]
        user: Option<String>,
        /// Read the password from stdin (an empty line is an empty password); otherwise CAPTUREFAB_CAMERA_PASSWORD
        #[arg(long)]
        password_stdin: bool,
        /// Save without connecting to the camera first
        #[arg(long)]
        no_check: bool,
    },
    /// List saved camera sign-ins (never their passwords)
    List,
    /// Forget a saved camera sign-in and delete its password
    Remove { address: String },
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    crate::dataset::Usage(message.into()).into()
}

/// Checks that need neither stdin, the credential store nor a camera.
pub fn prepare(command: &LoginCommand) -> Result<()> {
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
    if let LoginCommand::Remove { address } = command
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

pub fn run(command: &LoginCommand, session: Option<&str>, timeout: Duration) -> Result<Value> {
    prepare(command)?;
    match command {
        LoginCommand::List => {
            let logins = Logins::system().list()?;
            Ok(json!({"logins": logins, "supported": logins::can_remember()}))
        }
        LoginCommand::Remove { address } => {
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
        timeout_ms: timeout.as_millis().clamp(1, u64::MAX as u128) as u64,
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
pub fn human(command: &LoginCommand, value: &Value) -> String {
    match command {
        LoginCommand::List => {
            let logins = value["logins"].as_array().cloned().unwrap_or_default();
            if logins.is_empty() {
                return if value["supported"] == true {
                    "No saved camera sign-ins. Add one with `capturefab login add ADDRESS --user USER --password-stdin`.\n".into()
                } else {
                    "Saving camera sign-ins is unsupported in this build.\n".into()
                };
            }
            logins
                .iter()
                .map(|entry| {
                    format!(
                        "{}  {}  saved {}\n",
                        entry["address"].as_str().unwrap_or_default(),
                        entry["user"].as_str().unwrap_or_default(),
                        entry["saved_at"].as_str().unwrap_or_default()
                    )
                })
                .collect()
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
        LoginCommand::Remove { .. } => format!(
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
        assert!(target("http://cam.local/x", None, timeout).is_err());
    }
}
