//! Local source setup. User names and passwords enter through protected stdin,
//! never command arguments, model context or public profile metadata.
use crate::{
    session::SessionCommand,
    signals::auth::{ProfileDraft, ProfileInfo, ProfileRef, Secret, SecretSource, Store},
};
use anyhow::{Context as _, Result, ensure};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{IsTerminal, Read};
use zeroize::Zeroizing;

#[derive(Subcommand)]
pub enum Command {
    /// List saved profiles without reading credentials or contacting a source
    List,
    /// Save a credential-free endpoint; credentials are accepted only on stdin
    Save {
        name: String,
        #[arg(long)]
        uri: String,
        /// Read JSON {"username":"…","password":"…"} from stdin, up to 16 KiB
        #[arg(long)]
        credentials_stdin: bool,
        /// Read the password from this CAPTUREFAB_SIGNAL_ variable in the workbench
        #[arg(long, requires = "credentials_stdin")]
        password_env: Option<String>,
        /// Update this exact saved revision; omit when creating a new profile
        #[arg(long)]
        expected_revision: Option<String>,
    },
    /// Remove this exact revision of a saved profile
    Remove {
        name: String,
        #[arg(long)]
        expected_revision: String,
    },
    /// Explicitly resolve credentials locally and bind a profile to this workbench
    Bind { name: String },
}
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::List => "Profiles::List",
            Self::Save { .. } => "Profiles::Save",
            Self::Remove { .. } => "Profiles::Remove",
            Self::Bind { .. } => "Profiles::Bind",
        })
    }
}
impl Command {
    pub fn needs_session(&self) -> bool {
        matches!(self, Self::Bind { .. })
    }
    pub fn prepare_binding(&self) -> Result<SessionCommand> {
        let Self::Bind { name } = self else {
            anyhow::bail!("local profile management does not use a workbench")
        };
        let profile = find(&Store::session(), name)?.profile;
        Ok(SessionCommand::SignalBind { profile })
    }
}
fn find(store: &Store, name: &str) -> Result<ProfileInfo> {
    store
        .list()?
        .into_iter()
        .find(|profile| profile.name.eq_ignore_ascii_case(name))
        .context("no saved source has this name; run capturefab signals profiles list")
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(deserialize_with = "protected_text")]
    username: Zeroizing<String>,
    #[serde(default, deserialize_with = "protected_optional_text")]
    password: Option<Zeroizing<String>>,
}
fn protected_text<'de, D: serde::Deserializer<'de>>(
    input: D,
) -> std::result::Result<Zeroizing<String>, D::Error> {
    String::deserialize(input).map(Zeroizing::new)
}
fn protected_optional_text<'de, D: serde::Deserializer<'de>>(
    input: D,
) -> std::result::Result<Option<Zeroizing<String>>, D::Error> {
    Option::<String>::deserialize(input).map(|value| value.map(Zeroizing::new))
}
fn credentials(reader: impl Read) -> Result<Credentials> {
    let mut bytes = Zeroizing::new(Vec::new());
    reader.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "source credentials on stdin exceed 16 KiB"
    );
    serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!(
            "source credentials must be a JSON object with username and optional password fields"
        )
    })
}
pub fn run_local(command: &Command) -> Result<Value> {
    ensure!(
        !matches!(
            command,
            Command::Save {
                credentials_stdin: true,
                ..
            }
        ) || !std::io::stdin().is_terminal(),
        "Provide source credentials through a pipe or private file on stdin; use the workbench source editor for masked entry."
    );
    run_with_store(command, &Store::session(), std::io::stdin().lock())
}
fn run_with_store(command: &Command, store: &Store, reader: impl Read) -> Result<Value> {
    match command {
        Command::List => Ok(json!({"profiles":store.list()?})),
        Command::Save {
            name,
            uri,
            credentials_stdin,
            password_env,
            expected_revision,
        } => {
            // Shape checks precede stdin or credential access and do not echo endpoint data.
            crate::signals::auth::validate_endpoint(uri)?;
            crate::ipc::validate_name(name)?;
            let expected = expected_revision
                .as_ref()
                .map(|revision| -> Result<ProfileRef> {
                    let current = find(store, name)?.profile;
                    ensure!(
                        &current.revision == revision,
                        "source profile changed; list profiles and use its current revision"
                    );
                    Ok(current)
                })
                .transpose()?;
            let mut input = credentials_stdin.then(|| credentials(reader)).transpose()?;
            let secret = match (credentials_stdin, password_env) {
                (false, _) => SecretSource::None,
                (true, Some(variable)) => SecretSource::Environment {
                    variable: variable.clone(),
                },
                (true, None) => SecretSource::Keychain,
            };
            ensure!(
                password_env.is_none()
                    || input.as_ref().is_none_or(|value| value.password.is_none()),
                "omit password from stdin when using --password-env"
            );
            let password = input
                .as_mut()
                .and_then(|input| input.password.take())
                .map(Secret::new)
                .transpose()?;
            let draft = ProfileDraft {
                name: name.clone(),
                endpoint: uri.clone(),
                username: input.as_ref().map(|input| input.username.to_string()),
                secret,
            };
            let profile = store.save(expected.as_ref(), draft, password)?;
            Ok(json!({"saved":profile}))
        }
        Command::Remove {
            name,
            expected_revision,
        } => {
            let current = find(store, name)?.profile;
            ensure!(
                &current.revision == expected_revision,
                "source profile changed; list profiles and use its current revision"
            );
            store.delete(&current)?;
            Ok(json!({"removed":name}))
        }
        Command::Bind { .. } => anyhow::bail!("binding a source needs a running workbench"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Secrets(std::sync::atomic::AtomicUsize);
    impl crate::signals::auth::SecretStore for Secrets {
        fn environment(&self, _: &str) -> Result<Option<Secret>> {
            panic!("saving/listing must not read environment values")
        }
        fn read_keychain(&self, _: &str) -> Result<Option<Secret>> {
            panic!("saving a supplied password must not read it back")
        }
        fn write_keychain(&self, _: &str, password: &Secret) -> Result<()> {
            assert_eq!(password.expose(), "hidden-password");
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn delete_keychain(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stdin_password_goes_only_to_the_store_and_never_profile_output() {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-profile-secret-cli-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        let secrets = std::sync::Arc::new(Secrets::default());
        let store = Store::at(&dir).with_secret_store(secrets.clone());
        let command = Command::Save {
            name: "protected-line".into(),
            uri: "mqtts://localhost/line/#".into(),
            credentials_stdin: true,
            password_env: None,
            expected_revision: None,
        };
        let output = run_with_store(
            &command,
            &store,
            br#"{"username":"hidden-user","password":"hidden-password"}"#.as_slice(),
        )
        .unwrap();
        let listed = run_with_store(&Command::List, &store, &b""[..]).unwrap();
        for text in [
            output.to_string(),
            listed.to_string(),
            format!("{command:?}"),
        ] {
            assert!(!text.contains("hidden-user"));
            assert!(!text.contains("hidden-password"));
        }
        assert_eq!(secrets.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        let metadata = std::fs::read_to_string(dir.join("signal-profiles.json")).unwrap();
        assert!(!metadata.contains("hidden-password"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stdin_credentials_are_bounded_strict_and_do_not_echo_values() {
        for bytes in [
            br#"{"username":"hidden-user","password":"hidden-password","unexpected":true}"#
                .as_slice(),
            br#"{"username":"hidden-user","password":42}"#,
        ] {
            let error = credentials(bytes).err().unwrap().to_string();
            assert!(!error.contains("hidden-user"));
            assert!(!error.contains("hidden-password"));
        }
        assert!(credentials(vec![b' '; 16 * 1024 + 1].as_slice()).is_err());
    }
    #[test]
    fn local_profile_commands_save_list_compare_revision_and_remove() {
        let dir = std::env::temp_dir().join(format!(
            "capturefab-profile-cli-{}-{}",
            std::process::id(),
            crate::scheduling::now_ms()
        ));
        let store = Store::at(&dir);
        let save = Command::Save {
            name: "line-one".into(),
            uri: "mqtt://localhost/line/#".into(),
            credentials_stdin: false,
            password_env: None,
            expected_revision: None,
        };
        let saved = run_with_store(&save, &store, &b""[..]).unwrap();
        let revision = saved["saved"]["profile"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();
        let list = run_with_store(&Command::List, &store, &b""[..]).unwrap();
        assert_eq!(list["profiles"].as_array().unwrap().len(), 1);
        assert!(run_with_store(&save, &store, &b""[..]).is_err());
        assert!(
            run_with_store(
                &Command::Remove {
                    name: "line-one".into(),
                    expected_revision: "stale".into()
                },
                &store,
                &b""[..]
            )
            .is_err()
        );
        run_with_store(
            &Command::Remove {
                name: "line-one".into(),
                expected_revision: revision,
            },
            &store,
            &b""[..],
        )
        .unwrap();
        assert!(store.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
