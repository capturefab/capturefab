//! `capturefab hub ...`: save, check, list and remove camera hubs. These run
//! in the CLI process, like `destination`; secrets are read from stdin or
//! the environment and never accepted as arguments.
use super::{
    Hub, Kind, SecretSource, Store, registry,
    secret::{self, Secret},
};
use anyhow::Result;
use clap::{ArgGroup, Args, Subcommand};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Subcommand, Debug, Clone)]
pub enum HubCommand {
    /// List saved hubs
    List,
    /// List cameras on one saved hub, or on all of them; each ID is a --camera selector
    Cameras {
        /// A saved hub; every saved hub when omitted
        name: Option<String>,
    },
    /// Check that a saved hub answers and accepts its secret
    Test {
        /// A saved hub
        name: String,
    },
    /// Save a Home Assistant server; its cameras open as hub:NAME/camera.ENTITY
    #[command(name = "add-home-assistant", visible_alias = "add-ha")]
    AddHomeAssistant {
        /// A name for the hub: letters, digits, '.', '-' and '_'
        name: String,
        #[command(flatten)]
        server: Server,
    },
    /// Save a Frigate NVR; use http://HOST:5000, or https://HOST:8971 with --user
    AddFrigate {
        /// A name for the hub: letters, digits, '.', '-' and '_'
        name: String,
        /// The Frigate user to sign in as (port 8971)
        #[arg(long)]
        user: Option<String>,
        /// The port of Frigate's RTSP restream
        #[arg(long, default_value_t = 8554)]
        rtsp_port: u16,
        #[command(flatten)]
        server: Server,
    },
    /// Save a go2rtc server; its streams open as hub:NAME/STREAM
    AddGo2rtc {
        /// A name for the hub: letters, digits, '.', '-' and '_'
        name: String,
        /// The go2rtc API user name, when it has one
        #[arg(long)]
        user: Option<String>,
        #[command(flatten)]
        server: Server,
    },
    /// Save a UniFi Protect console (snapshots); create an API key under Settings > Control Plane > Integrations
    #[command(name = "add-unifi-protect", visible_alias = "add-unifi")]
    AddUnifiProtect {
        /// A name for the hub: letters, digits, '.', '-' and '_'
        name: String,
        #[command(flatten)]
        server: Server,
    },
    /// Remove a saved hub; --forget-secret also deletes its saved secret
    Remove {
        /// A saved hub
        name: String,
        /// Also delete the hub's secret from the OS credential store
        #[arg(long)]
        forget_secret: bool,
    },
}

/// Where a hub is and how it is trusted.
#[derive(Args, Debug, Clone)]
#[command(group(ArgGroup::new("trust").args(["certificate", "accept_certificate", "tls_server_name"])))]
pub struct Server {
    /// The server's address, such as http://homeassistant.local:8123
    #[arg(long)]
    pub url: String,
    /// Read the secret from stdin and save it in the OS credential store: a long-lived access token (Home Assistant: Profile > Security), a password (Frigate, go2rtc) or an API key (UniFi Protect)
    #[arg(long, conflicts_with = "secret_env")]
    pub secret_stdin: bool,
    /// Read the secret from environment variable VAR (CAPTUREFAB_HUB_...) in the process that opens the camera: a GUI or serve session, or this CLI
    #[arg(long, value_name = "VAR")]
    pub secret_env: Option<String>,
    /// The server's TLS certificate fingerprint, SHA256:AB:CD:... (openssl x509 -noout -fingerprint -sha256); a different certificate is refused
    #[arg(long, value_name = "FINGERPRINT")]
    pub certificate: Option<String>,
    /// Trust the certificate the server presents now; confirm its fingerprint with the administrator
    #[arg(long, conflicts_with = "no_check")]
    pub accept_certificate: bool,
    /// Check the certificate against NAME instead of the URL's host (for a certificate issued to a domain name reached by IP)
    #[arg(long, value_name = "NAME")]
    pub tls_server_name: Option<String>,
    /// Save without checking that the server answers and accepts the secret
    #[arg(long)]
    pub no_check: bool,
}

/// The kind, name, server and user an add command saves.
fn add_parts(command: &HubCommand) -> Option<(Kind, &str, &Server, Option<&str>)> {
    match command {
        HubCommand::AddHomeAssistant { name, server } => {
            Some((Kind::HomeAssistant, name, server, None))
        }
        HubCommand::AddFrigate {
            name, user, server, ..
        } => Some((Kind::Frigate, name, server, user.as_deref())),
        HubCommand::AddGo2rtc { name, user, server } => {
            Some((Kind::Go2rtc, name, server, user.as_deref()))
        }
        HubCommand::AddUnifiProtect { name, server } => {
            Some((Kind::UnifiProtect, name, server, None))
        }
        _ => None,
    }
}

/// The record an add command saves. With neither `--secret-stdin` nor
/// `--secret-env`, a kind that needs a secret reuses the saved one.
fn record(command: &HubCommand) -> Result<Option<Hub>> {
    let Some((kind, name, server, user)) = add_parts(command) else {
        return Ok(None);
    };
    let needs_secret = match kind {
        Kind::HomeAssistant | Kind::UnifiProtect => true,
        Kind::Frigate | Kind::Go2rtc => user.is_some(),
    };
    let secret = if server.secret_env.is_some() {
        SecretSource::Environment
    } else if server.secret_stdin || needs_secret {
        SecretSource::Keychain
    } else {
        SecretSource::None
    };
    let hub = Hub {
        name: name.into(),
        kind,
        url: server.url.trim_end_matches('/').to_owned(),
        user: user.map(str::to_owned),
        secret,
        secret_env: server.secret_env.clone(),
        certificate: server
            .certificate
            .as_deref()
            .map(registry::parse_fingerprint)
            .transpose()?
            .map(|pin| registry::format_fingerprint(&pin)),
        tls_server_name: server.tls_server_name.clone(),
        rtsp_port: match command {
            HubCommand::AddFrigate { rtsp_port, .. } => Some(*rtsp_port),
            _ => None,
        },
        rejected: None,
        revision: None,
    };
    if server.accept_certificate {
        anyhow::ensure!(
            hub.parsed_url()?.scheme == registry::Scheme::Https,
            "--accept-certificate needs an https:// hub URL"
        );
    }
    hub.validate()?;
    Ok(Some(hub))
}

/// Check arguments before touching any file (usage errors, exit 2).
pub fn prepare(command: &HubCommand) -> Result<()> {
    match command {
        HubCommand::Cameras { name: Some(name) }
        | HubCommand::Test { name }
        | HubCommand::Remove { name, .. } => registry::validate_name(name),
        HubCommand::List | HubCommand::Cameras { name: None } => Ok(()),
        _ => record(command).map(|_| ()),
    }
}

/// The JSON for a saved hub; never the secret.
fn describe(hub: &Hub) -> Value {
    let mut value = serde_json::to_value(hub).expect("hub records serialize");
    value.as_object_mut().unwrap().remove("revision");
    value["insecure"] = json!(hub.insecure());
    if hub.secret == SecretSource::Keychain
        && let Ok(account) = hub.account()
        && let Some(saved) = secret::keychain_has(&account)
    {
        value["secret_saved"] = json!(saved);
    }
    value
}

fn require_compiled() -> Result<()> {
    if super::compiled() {
        Ok(())
    } else {
        Err(registry::unsupported())
    }
}

/// Run a hub command; `timeout` is `--timeout-ms`, a floor for every
/// request's limits.
pub fn run(command: &HubCommand, timeout: Duration) -> Result<Value> {
    run_in(&Store::session(), command, timeout)
}

pub(crate) fn run_in(store: &Store, command: &HubCommand, timeout: Duration) -> Result<Value> {
    match command {
        HubCommand::List => Ok(json!({
            "hubs": store.list()?.iter().map(describe).collect::<Vec<_>>()
        })),
        HubCommand::Remove {
            name,
            forget_secret,
        } => {
            let hub = store.remove(name)?;
            let mut warnings = Vec::new();
            let forgotten = if *forget_secret && hub.secret == SecretSource::Keychain {
                let account = hub.account()?;
                match store.forget_unused_secret(&account, || secret::keychain_delete(&account)) {
                    Ok(forgotten) => forgotten,
                    Err(error) => {
                        warnings.push(format!("{error:#}"));
                        false
                    }
                }
            } else {
                false
            };
            Ok(json!({"removed": hub.name, "secret_forgotten": forgotten, "warnings": warnings}))
        }
        HubCommand::Test { name } => {
            require_compiled()?;
            network::test(store, &store.get(name)?, timeout)
        }
        HubCommand::Cameras { name } => {
            require_compiled()?;
            network::cameras(store, name.as_deref(), timeout)
        }
        _ => add(store, command, timeout),
    }
}

fn add(store: &Store, command: &HubCommand, timeout: Duration) -> Result<Value> {
    let mut hub = record(command)?.expect("an add command");
    let (_, _, server, _) = add_parts(command).expect("an add command");
    let mut warnings = Vec::new();
    if server.secret_stdin && !cfg!(feature = "keychain") {
        return Err(registry::keychain_unsupported());
    }
    // The secret to check with, and whether it is new to the keychain.
    let (secret, stored) = if server.secret_stdin {
        (Some(secret::read_stdin(hub.kind)?), true)
    } else if let Some(variable) = &server.secret_env {
        match std::env::var(variable) {
            Ok(value) if !value.is_empty() => (Some(Secret::new(hub.kind, value)?), false),
            _ if server.no_check => (None, false),
            _ => {
                return Err(crate::dataset::Usage(format!(
                    "{variable} is not set here; set it in this shell to check the hub, and in the process that opens its cameras, or save with --no-check"
                ))
                .into());
            }
        }
    } else if hub.secret == SecretSource::Keychain {
        // Reuse the secret saved for this hub and server.
        let reusable = store
            .list()?
            .into_iter()
            .find(|other| other.name == hub.name)
            .filter(|other| {
                other.secret == SecretSource::Keychain && other.account().ok() == hub.account().ok()
            })
            .is_some_and(|_| {
                hub.account().ok().and_then(|a| secret::keychain_has(&a)) == Some(true)
            });
        if !reusable {
            return Err(crate::dataset::Usage(format!(
                "hub {} needs --secret-stdin or --secret-env VAR",
                hub.kind.command()
            ))
            .into());
        }
        let secret = if server.no_check {
            None
        } else {
            secret::resolve(&hub)?
        };
        (secret, false)
    } else {
        (None, false)
    };
    let mut certificate = None;
    if server.accept_certificate {
        require_compiled()?;
        let fingerprint = network::accept(&hub, timeout)?;
        let text = registry::format_fingerprint(&fingerprint);
        eprintln!(
            "capturefab: trusting certificate {text} for {} on first use",
            hub.parsed_url()?.authority()
        );
        hub.certificate = Some(text.clone());
        certificate = Some(json!({"fingerprint": text, "trust": "accepted_on_first_use"}));
    } else if let Some(text) = &hub.certificate {
        certificate = Some(json!({"fingerprint": text, "trust": "pinned"}));
    }
    let check = if server.no_check {
        None
    } else {
        require_compiled()?;
        Some(network::check(&hub, secret.as_ref(), timeout)?)
    };
    if hub.insecure() {
        eprintln!(
            "capturefab: hub {} uses http://; its secret crosses the network unencrypted",
            hub.name
        );
    }
    let account = hub.account()?;
    let (previous, _) = store.save_with(
        &hub,
        |_| match (&secret, stored) {
            (Some(secret), true) => secret::keychain_replace(&account, secret).map(Some),
            _ => Ok(None),
        },
        |replaced| match replaced {
            Some(previous) => secret::keychain_restore(&account, previous.as_ref()),
            None => Ok(()),
        },
    )?;
    // A secret saved for this hub's previous server is no longer used.
    if let Some(previous) = &previous
        && previous.secret == SecretSource::Keychain
        && let Ok(old) = previous.account()
        && old != account
        && let Err(error) = store.forget_unused_secret(&old, || secret::keychain_delete(&old))
    {
        warnings.push(format!("{error:#}"));
    }
    let mut result = json!({
        "hub": describe(&hub),
        "replaced": previous.is_some(),
        "warnings": warnings,
    });
    if let Some(check) = check {
        result["check"] = check;
    }
    if let Some(certificate) = certificate {
        result["certificate"] = certificate;
    }
    Ok(result)
}

/// The commands that talk to hubs.
#[cfg(feature = "hubs")]
mod network {
    use super::*;
    use crate::hubs::{Selector, Stream, client, http, pump::Failure};

    /// Report a failure, recording a refused secret so discovery stops
    /// using it.
    fn fail(store: &Store, hub: &Hub, failure: Failure) -> anyhow::Error {
        if let Some(status) = failure.auth_status {
            let _ = store.mark_rejected(&hub.name, &hub.digest(), status);
        }
        failure.into_error()
    }

    pub(super) fn accept(hub: &Hub, timeout: Duration) -> Result<[u8; 32]> {
        http::fetch_fingerprint(
            &http::label(hub),
            &hub.parsed_url()?,
            timeout.max(Duration::from_secs(10)),
        )
    }

    pub(super) fn check(hub: &Hub, secret: Option<&Secret>, timeout: Duration) -> Result<Value> {
        match client::check(hub, secret, timeout) {
            Ok(check) => Ok(json!({
                "ok": true,
                "product": check.product,
                "version": check.version,
                "name": check.name,
                "cameras": check.cameras,
                "warnings": check.warnings,
            })),
            Err(failure) => {
                let unreachable = matches!(
                    failure.kind,
                    crate::types::ErrorKind::Unavailable | crate::types::ErrorKind::Timeout
                );
                let message = if unreachable {
                    format!("{}; fix the URL or save with --no-check", failure.message)
                } else {
                    format!("the hub check failed: {}", failure.message)
                };
                Err(crate::types::Categorized::error(failure.kind, message))
            }
        }
    }

    pub(super) fn test(store: &Store, hub: &Hub, timeout: Duration) -> Result<Value> {
        let secret = secret::resolve(hub)?;
        let check =
            client::check(hub, secret.as_ref(), timeout).map_err(|f| fail(store, hub, f))?;
        let _ = store.clear_rejected(&hub.name, &hub.digest());
        Ok(json!({
            "ok": true,
            "hub": hub.name,
            "kind": hub.kind,
            "product": check.product,
            "version": check.version,
            "name": check.name,
            "cameras": check.cameras,
            "warnings": check.warnings,
        }))
    }

    pub(super) fn listed(hub: &Hub, camera: client::Listed) -> Value {
        let id = Selector {
            hub: hub.name.clone(),
            camera: camera.camera.clone(),
            stream: Stream::Auto,
            fps: None,
        }
        .id();
        json!({
            "id": id,
            "hub": hub.name,
            "kind": hub.kind,
            "camera": camera.camera,
            "name": camera.name,
            "state": camera.state,
            "online": camera.online,
            "streams": camera.streams.iter().map(|s| s.name()).collect::<Vec<_>>(),
        })
    }

    pub(super) fn cameras(store: &Store, name: Option<&str>, timeout: Duration) -> Result<Value> {
        let hubs = match name {
            Some(name) => vec![store.get(name)?],
            None => store.list()?,
        };
        let mut cameras = Vec::new();
        let mut warnings = Vec::new();
        for hub in &hubs {
            let result = secret::resolve(hub).and_then(|secret| {
                client::cameras(hub, secret.as_ref(), timeout).map_err(|f| fail(store, hub, f))
            });
            match result {
                Ok(found) => cameras.extend(found.into_iter().map(|camera| listed(hub, camera))),
                Err(error) if name.is_some() => return Err(error),
                Err(error) => {
                    warnings.push(format!("{} hub {}: {error:#}", hub.kind.label(), hub.name))
                }
            }
        }
        Ok(json!({"cameras": cameras, "warnings": warnings}))
    }
}

#[cfg(not(feature = "hubs"))]
mod network {
    use super::*;
    pub(super) fn accept(_: &Hub, _: Duration) -> Result<[u8; 32]> {
        Err(registry::unsupported())
    }
    pub(super) fn check(_: &Hub, _: Option<&Secret>, _: Duration) -> Result<Value> {
        Err(registry::unsupported())
    }
    pub(super) fn test(_: &Store, _: &Hub, _: Duration) -> Result<Value> {
        Err(registry::unsupported())
    }
    pub(super) fn cameras(_: &Store, _: Option<&str>, _: Duration) -> Result<Value> {
        Err(registry::unsupported())
    }
}

/// The text `capturefab hub ...` prints without `--json`.
pub fn human(command: &HubCommand, value: &Value) -> String {
    let text = |value: &Value| value.as_str().unwrap_or("").to_owned();
    let mut out = String::new();
    let mut table = |rows: Vec<Vec<String>>| {
        let widths = (0..rows[0].len())
            .map(|column| {
                rows.iter()
                    .map(|row| row[column].chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();
        for row in rows {
            let line = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect::<Vec<_>>()
                .join("  ");
            out.push_str(line.trim_end());
            out.push('\n');
        }
    };
    match command {
        HubCommand::List => {
            let hubs = value["hubs"].as_array().cloned().unwrap_or_default();
            if hubs.is_empty() {
                return "No saved hubs. Add one with: capturefab hub add-home-assistant NAME --url URL --secret-stdin < token.txt\n".into();
            }
            let mut rows = vec![vec![
                "NAME".into(),
                "KIND".into(),
                "URL".into(),
                "SECRET".into(),
            ]];
            for hub in hubs {
                let secret = if let Some(rejected) = hub["rejected"].as_object() {
                    format!("rejected (HTTP {})", rejected["status"])
                } else {
                    match hub["secret"].as_str() {
                        Some("keychain") if hub["secret_saved"] == false => {
                            "keychain (missing)".into()
                        }
                        Some("environment") => format!("env {}", text(&hub["secret_env"])),
                        Some(other) => other.into(),
                        None => String::new(),
                    }
                };
                rows.push(vec![
                    text(&hub["name"]),
                    text(&hub["kind"]),
                    text(&hub["url"]),
                    secret,
                ]);
            }
            table(rows);
        }
        HubCommand::Cameras { .. } => {
            let cameras = value["cameras"].as_array().cloned().unwrap_or_default();
            if cameras.is_empty() {
                out.push_str("No hub cameras.\n");
            } else {
                let mut rows = vec![vec!["ID".into(), "NAME".into(), "STATE".into()]];
                for camera in cameras {
                    let state = camera["state"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            if camera["online"] == true {
                                "online".into()
                            } else {
                                "offline".into()
                            }
                        });
                    rows.push(vec![text(&camera["id"]), text(&camera["name"]), state]);
                }
                table(rows);
            }
        }
        HubCommand::Test { .. } => {
            out.push_str(&format!(
                "Hub {}: {}{}{}, {} cameras\n",
                text(&value["hub"]),
                text(&value["product"]),
                value["version"]
                    .as_str()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default(),
                value["name"]
                    .as_str()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default(),
                value["cameras"]
            ));
        }
        HubCommand::Remove { name, .. } => out.push_str(&format!("Removed hub {name}\n")),
        _ => {
            let hub = &value["hub"];
            let verb = if value["replaced"] == true {
                "Replaced"
            } else {
                "Saved"
            };
            match value.get("check") {
                Some(check) => out.push_str(&format!(
                    "{verb} hub {}: {}{} at {} with {} cameras\nList them with: capturefab hub cameras {}\n",
                    text(&hub["name"]),
                    text(&check["product"]),
                    check["version"].as_str().map(|v| format!(" {v}")).unwrap_or_default(),
                    text(&hub["url"]),
                    check["cameras"],
                    text(&hub["name"])
                )),
                None => out.push_str(&format!(
                    "{verb} hub {} without checking it; check it with: capturefab hub test {}\n",
                    text(&hub["name"]),
                    text(&hub["name"])
                )),
            }
        }
    }
    out
}
