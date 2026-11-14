//! Bounded OPC-UA Read services using the in-process Rust client.
use super::{Reading, Source};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use opcua::{
    client::{Client, ClientBuilder, IdentityToken, Session},
    crypto::SecurityPolicy,
    types::{DataValue, MessageSecurityMode, ReadValueId, StatusCode, TimestampsToReturn},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};
use tokio::{runtime::Runtime, task::JoinHandle, time::Instant};

#[cfg(test)]
mod depth_tests;
mod namespace;
mod security;
mod subscription;
mod value;

const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub endpoint: String,
    pub nodes: Vec<Node>,
    #[serde(default, skip_serializing_if = "crate::types::is_default")]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<subscription::Config>,
    #[serde(
        default = "default_sampling",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub sampling_ms: u64,
    #[serde(
        default = "default_timeout",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub request_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<Security>,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    #[default]
    Read,
    Subscription,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Node {
    pub name: String,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_uri: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Security {
    pub policy: Policy,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub server_certificate: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_password_env: Option<String>,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Policy {
    Basic256Sha256,
    Aes128Sha256RsaOaep,
    Aes256Sha256RsaPss,
}
const fn default_sampling() -> u64 {
    100
}
const fn default_timeout() -> u64 {
    1000
}

fn env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        && name.len() <= 256
}
pub(super) fn validate(config: &Config) -> Result<()> {
    match (config.mode, &config.subscription) {
        (Mode::Read, None) => (),
        (Mode::Subscription, Some(subscription)) => subscription.validate()?,
        _ => return Err(Usage("OPC-UA subscription mode requires its subscription block; Read mode rejects that block".into()).into()),
    }
    let authority = config
        .endpoint
        .strip_prefix("opc.tcp://")
        .and_then(|tail| tail.split('/').next());
    if authority.is_none_or(str::is_empty)
        || config.endpoint.len() > 8192
        || config.endpoint.chars().any(char::is_control)
        || authority.is_some_and(|authority| authority.contains('@'))
    {
        return Err(Usage(
            "OPC-UA endpoint must be an opc.tcp:// URL without embedded credentials".into(),
        )
        .into());
    }
    if config.nodes.is_empty() || config.nodes.len() > 256 {
        return Err(Usage("OPC-UA requires 1–256 named nodes".into()).into());
    }
    let mut names = BTreeSet::new();
    for node in &config.nodes {
        if node.name.is_empty()
            || node.name.len() > 1024
            || node.name.chars().any(char::is_control)
            || node.node_id.is_empty()
            || node.node_id.len() > 4096
            || node.node_id.chars().any(char::is_control)
            || !names.insert(&node.name)
        {
            return Err(Usage("OPC-UA nodes need unique nonempty names and nonempty node_ids without control characters".into()).into());
        }
    }
    if !(1..=3_600_000).contains(&config.sampling_ms)
        || !(1..=30_000).contains(&config.request_timeout_ms)
    {
        return Err(Usage(
            "OPC-UA sampling_ms must be 1–3600000 and request_timeout_ms 1–30000".into(),
        )
        .into());
    }
    if config.username_env.is_some() != config.password_env.is_some() {
        return Err(
            Usage("OPC-UA username_env and password_env must be supplied together".into()).into(),
        );
    }
    if config.username_env.is_some() && config.security.is_none() {
        return Err(Usage("OPC-UA username/password authentication requires SignAndEncrypt security configuration".into()).into());
    }
    for name in config
        .username_env
        .iter()
        .chain(&config.password_env)
        .chain(
            config
                .security
                .iter()
                .filter_map(|security| security.private_key_password_env.as_ref()),
        )
    {
        if !env_name(name) {
            return Err(Usage("OPC-UA credential environment names must be shell identifiers of at most 256 bytes".into()).into());
        }
    }
    if let Some(security) = &config.security
        && [
            &security.certificate,
            &security.private_key,
            &security.server_certificate,
        ]
        .iter()
        .any(|path| path.as_os_str().is_empty())
    {
        return Err(Usage(
            "OPC-UA security requires certificate, private_key and pinned server_certificate paths"
                .into(),
        )
        .into());
    }
    Ok(())
}

pub(crate) fn open(config: &Config) -> Result<Box<dyn Source>> {
    validate(config)?;
    let prepared = namespace::prepare(&config.nodes)?;
    // URI-addressed requests remain unavailable until this session's table
    // validates and every requested URI resolves. No namespace-zero fallback.
    let nodes = if namespace::required(&config.nodes) {
        Vec::new()
    } else {
        prepared
    };
    let identity = match (&config.username_env, &config.password_env) {
        (Some(username), Some(password)) => {
            IdentityToken::new_user_name(credential(username)?, credential(password)?)
        }
        _ => IdentityToken::Anonymous,
    };
    let request_timeout = Duration::from_millis(config.request_timeout_ms);
    let builder = ClientBuilder::new()
        .application_name("Capturefab")
        .application_uri("urn:capturefab:opcua")
        .product_uri("urn:capturefab")
        .create_sample_keypair(false)
        .trust_server_certs(false)
        .verify_server_certs(true)
        .session_retry_limit(0)
        .request_timeout(request_timeout)
        .max_array_length(65_536)
        .max_string_length(MAX_MESSAGE)
        .max_byte_string_length(MAX_MESSAGE)
        .max_message_size(MAX_MESSAGE)
        .max_chunk_size(65_536)
        .max_incoming_chunk_size(65_536)
        .max_chunk_count(64);
    let builder = if config.mode == Mode::Subscription {
        builder
            .recreate_subscriptions(false)
            .ignore_clock_skew(false)
    } else {
        builder
    };
    let (builder, pki, pin) = if let Some(security) = &config.security {
        let (builder, pki, pin) = security::configure(builder, security)?;
        (builder, pki, Some(pin))
    } else {
        let mut temporary = tempfile::Builder::new();
        temporary.prefix("capturefab-opcua-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            temporary.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let pki = temporary.tempdir().map_err(|_| connection_error())?;
        (builder.pki_dir(pki.path()), pki, None)
    };
    let client = builder.client().map_err(|_| connection_error())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| {
            Categorized::error(ErrorKind::Unavailable, "OPC-UA runtime could not start")
        })?;
    let state = State {
        client,
        config: config.clone(),
        nodes,
        identity: Some(identity),
        pin,
        session: None,
        task: None,
        ready: false,
        next_poll: None,
        request_timeout,
        _pki: pki,
    };
    if config.mode == Mode::Subscription {
        return Ok(subscription::open(runtime, state));
    }
    Ok(Box::new(OpcUa {
        runtime: Some(runtime),
        state,
    }))
}

fn credential(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Categorized::error(
                ErrorKind::AuthRequired,
                "OPC-UA credential environment variable is missing or empty",
            )
        })
}

struct OpcUa {
    runtime: Option<Runtime>,
    state: State,
}
struct State {
    client: Client,
    config: Config,
    nodes: Vec<ReadValueId>,
    identity: Option<IdentityToken>,
    pin: Option<Vec<u8>>,
    session: Option<Arc<Session>>,
    task: Option<JoinHandle<StatusCode>>,
    ready: bool,
    next_poll: Option<Instant>,
    request_timeout: Duration,
    // The private normalized key and exact certificate trust store live only
    // as long as this source, including its disconnect.
    _pki: tempfile::TempDir,
}
impl Source for OpcUa {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let runtime = self.runtime.as_ref().ok_or_else(connection_error)?;
        runtime.block_on(self.state.read(timeout))
    }
    fn ready(&self) -> bool {
        self.state.ready
    }
    fn end_timeout_is_idle(&self) -> bool {
        false
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        if let Some(runtime) = self.runtime.take() {
            runtime.block_on(async {
                if let Some(session) = self.state.session.take() {
                    session.disable_reconnects();
                    let _ = tokio::time::timeout(
                        timeout.min(self.state.request_timeout),
                        session.disconnect(),
                    )
                    .await;
                }
                self.state.abort();
            });
            // DNS resolution uses Tokio's blocking pool. Do not let a system
            // resolver outlive the caller's bounded shutdown budget.
            runtime.shutdown_timeout(Duration::ZERO);
        }
        Ok(())
    }
}
impl Drop for OpcUa {
    fn drop(&mut self) {
        self.state.abort();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::ZERO);
        }
    }
}
impl State {
    fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
    async fn connect(&mut self, deadline: Instant) -> Result<()> {
        if namespace::required(&self.config.nodes) {
            self.nodes.clear();
        }
        let endpoints = self
            .client
            .get_server_endpoints_from_url(&self.config.endpoint)
            .await
            .map_err(|error| status_error(error.status(), false))?;
        let (policy, mode) = self.config.security.as_ref().map_or(
            (SecurityPolicy::None, MessageSecurityMode::None),
            |security| {
                (
                    match security.policy {
                        Policy::Basic256Sha256 => SecurityPolicy::Basic256Sha256,
                        Policy::Aes128Sha256RsaOaep => SecurityPolicy::Aes128Sha256RsaOaep,
                        Policy::Aes256Sha256RsaPss => SecurityPolicy::Aes256Sha256RsaPss,
                    },
                    MessageSecurityMode::SignAndEncrypt,
                )
            },
        );
        let endpoint =
            Client::find_matching_endpoint(&endpoints, &self.config.endpoint, policy, mode)
                .ok_or_else(connection_error)?;
        if let Some(pin) = &self.pin
            && endpoint.server_certificate.as_ref() != pin.as_slice()
        {
            return Err(connection_error());
        }
        let identity = self.identity.take().ok_or_else(connection_error)?;
        let (session, event_loop) = self
            .client
            .connect_to_endpoint_directly(endpoint, identity)
            .map_err(|error| status_error(error.status(), false))?;
        if self.config.mode == Mode::Subscription {
            session.disable_reconnects();
        }
        self.task = Some(event_loop.spawn());
        self.session = Some(Arc::clone(&session));
        let task = self.task.as_mut().ok_or_else(connection_error)?;
        tokio::select! {
            biased;
            status = task => Err(task_error(&status)),
            connected = session.wait_for_connection() => {
                if connected { Ok(()) } else { Err(connection_error()) }
            }
        }?;
        if namespace::required(&self.config.nodes) {
            let task = self.task.as_mut().ok_or_else(connection_error)?;
            let request = [namespace::array_request()];
            let values = tokio::select! {
                biased;
                status = task => return Err(task_error(&status)),
                values = session.read(&request, TimestampsToReturn::Neither, 0.0) => {
                    values.map_err(|error| status_error(error.status(), true))?
                }
            };
            self.nodes = namespace::resolve(&self.config.nodes, &values, deadline)?;
        }
        Ok(())
    }
    async fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now() + timeout;
        if self.session.is_none() {
            let setup_deadline = deadline.min(Instant::now() + self.request_timeout);
            match tokio::time::timeout_at(setup_deadline, self.connect(setup_deadline)).await {
                Ok(Ok(())) => (),
                result => {
                    self.abort();
                    return match result {
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(timeout_error()),
                        Ok(Ok(())) => unreachable!(),
                    };
                }
            }
        }
        let task = self.task.as_mut().ok_or_else(connection_error)?;
        if let Some(next) = self.next_poll {
            let until = next.min(deadline);
            tokio::select! {
                biased;
                status = &mut *task => return Err(task_error(&status)),
                () = tokio::time::sleep_until(until) => (),
            }
            if Instant::now() >= deadline {
                return Ok(Vec::new());
            }
        }
        if self.nodes.len() != self.config.nodes.len() {
            return Err(connection_error());
        }
        if Instant::now() >= deadline {
            return Err(timeout_error());
        }
        let session = self.session.as_ref().ok_or_else(connection_error)?;
        let limit = self
            .request_timeout
            .min(deadline.saturating_duration_since(Instant::now()));
        let values = tokio::select! {
            biased;
            status = task => return Err(task_error(&status)),
            result = tokio::time::timeout(limit, session.read(&self.nodes, TimestampsToReturn::Both, 0.0)) => {
                result.map_err(|_| timeout_error())?.map_err(|error| status_error(error.status(), true))?
            }
        };
        let readings = readings(&self.config.nodes, &values)?;
        self.next_poll = Some(Instant::now() + Duration::from_millis(self.config.sampling_ms));
        self.ready = true;
        Ok(readings)
    }
}

fn readings(nodes: &[Node], values: &[DataValue]) -> Result<Vec<Reading>> {
    if nodes.len() != values.len() {
        return Err(Categorized::error(
            ErrorKind::Incomplete,
            "OPC-UA Read returned an unexpected node count",
        ));
    }
    let mut readings = Vec::with_capacity(nodes.len());
    for (index, (node, data)) in nodes.iter().zip(values).enumerate() {
        if !data.status().is_good() {
            return Err(Categorized::new(
                ErrorKind::Unavailable,
                "OPC-UA node returned a non-Good status; the poll was not published",
            )
            .details(serde_json::json!({"node_index":index,"opcua_status":data.status().bits()}))
            .into());
        }
        readings.push(Reading {
            signal: node.name.clone(),
            value: data
                .value
                .as_ref()
                .map(value::convert)
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
            source_time: data
                .source_timestamp
                .filter(|time| !time.is_null())
                .map(|time| {
                    time.as_chrono()
                        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
                }),
        });
    }
    let mut size = JsonLimit(0);
    serde_json::to_writer(&mut size, &readings).map_err(|_| {
        Categorized::error(
            ErrorKind::Incomplete,
            "OPC-UA poll exceeds the 1 MiB JSON limit",
        )
    })?;
    Ok(readings)
}
struct JsonLimit(usize);
impl std::io::Write for JsonLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_MESSAGE.saturating_sub(self.0) {
            return Err(std::io::Error::other("OPC-UA JSON limit"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn connection_error() -> anyhow::Error {
    Categorized::new(
        ErrorKind::Unavailable,
        "OPC-UA connection or security setup failed",
    )
    .hint("check the endpoint, credentials and configured certificate files")
    .into()
}
fn timeout_error() -> anyhow::Error {
    Categorized::error(
        ErrorKind::Timeout,
        "OPC-UA operation exceeded its request or source deadline",
    )
}
fn status_error(status: StatusCode, reading: bool) -> anyhow::Error {
    let (kind, message) = match status {
        StatusCode::BadTimeout | StatusCode::BadRequestTimeout => (
            ErrorKind::Timeout,
            "OPC-UA operation exceeded its request or source deadline",
        ),
        StatusCode::BadUserAccessDenied
        | StatusCode::BadIdentityTokenInvalid
        | StatusCode::BadIdentityTokenRejected => (
            ErrorKind::AuthRejected,
            "OPC-UA server rejected the configured identity",
        ),
        StatusCode::BadEncodingLimitsExceeded
        | StatusCode::BadResponseTooLarge
        | StatusCode::BadTcpMessageTooLarge => (
            ErrorKind::Incomplete,
            "OPC-UA response exceeds its decoding limits",
        ),
        _ if reading => (ErrorKind::Unavailable, "OPC-UA Read service failed"),
        _ => (
            ErrorKind::Unavailable,
            "OPC-UA connection or security setup failed",
        ),
    };
    Categorized::new(kind, message)
        .details(serde_json::json!({"opcua_status":status.bits()}))
        .into()
}
fn task_error(status: &std::result::Result<StatusCode, tokio::task::JoinError>) -> anyhow::Error {
    match status {
        Ok(status) if status.is_good() => Categorized::new(
            ErrorKind::Unavailable,
            "OPC-UA SDK session event loop ended while an operation was pending",
        )
        .details(
            serde_json::json!({"sdk_status_raw":status.bits(),"sdk_error_status_present":false}),
        )
        .into(),
        Ok(status) => status_error(*status, false),
        Err(_) => connection_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Node, Policy, Security, readings, status_error, validate};
    use opcua::types::{DataValue, StatusCode};
    #[test]
    fn integer_configuration_survives_the_serialized_helper_boundary() {
        let configured: Config = serde_json::from_str(
            r#"{"endpoint":"opc.tcp://127.0.0.1:4840","nodes":[{"name":"pressure","node_id":"ns=2;s=P"}],"sampling_ms":1e2,"request_timeout_ms":1.5e3}"#,
        )
        .unwrap();
        validate(&configured).unwrap();
        let encoded = serde_json::to_value(&configured).unwrap();
        assert_eq!(encoded["sampling_ms"].as_u64(), Some(100));
        assert_eq!(encoded["request_timeout_ms"].as_u64(), Some(1500));
        let decoded: Config = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.sampling_ms, 100);
        assert_eq!(decoded.request_timeout_ms, 1500);
        validate(&decoded).unwrap();
        let defaulted = config();
        assert_eq!(defaulted.sampling_ms, 100);
        assert_eq!(defaulted.request_timeout_ms, 1000);
    }

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({"endpoint":"opc.tcp://127.0.0.1:4840", "nodes":[{"name":"pressure","node_id":"ns=2;s=P"}]})).unwrap()
    }
    #[test]
    fn validates_nodes_credentials_and_security() {
        let mut config = config();
        validate(&config).unwrap();
        config.nodes.push(Node {
            name: "pressure".into(),
            node_id: "i=1".into(),
            namespace_uri: None,
        });
        validate(&config).unwrap_err();
        config.nodes.pop();
        config.endpoint = "opc.tcp://user:secret@127.0.0.1:4840".into();
        validate(&config).unwrap_err();
        config.endpoint = "opc.tcp://127.0.0.1:4840".into();
        config.username_env = Some("OPCUA_USER".into());
        config.password_env = Some("OPCUA_PASSWORD".into());
        validate(&config).unwrap_err();
        config.security = Some(Security {
            policy: Policy::Basic256Sha256,
            certificate: "client.der".into(),
            private_key: "client.pem".into(),
            server_certificate: "server.der".into(),
            private_key_password_env: None,
        });
        validate(&config).unwrap();
    }
    #[test]
    fn old_python_configuration_is_rejected() {
        let mut config = serde_json::to_value(config()).unwrap();
        config["python"] = "python3".into();
        serde_json::from_value::<Config>(config).unwrap_err();
    }
    #[test]
    fn status_errors_keep_only_safe_numeric_details() {
        let error = status_error(StatusCode::BadIdentityTokenRejected, false);
        let classified = crate::types::classify(&error);
        assert_eq!(classified.kind, crate::types::ErrorKind::AuthRejected);
        assert_eq!(
            classified.details.unwrap()["opcua_status"],
            StatusCode::BadIdentityTokenRejected.bits()
        );
    }
    #[test]
    fn poll_is_atomic_and_absent_timestamps_are_not_invented() {
        let mut nodes = config().nodes;
        nodes.push(Node {
            name: "ready".into(),
            node_id: "i=2".into(),
            namespace_uri: None,
        });
        let good = DataValue::value_only(42_u64);
        let invalid = DataValue::from((1_i32.into(), StatusCode::Uncertain));
        let error = readings(&nodes, &[good.clone(), invalid]).unwrap_err();
        assert_eq!(
            crate::types::classify(&error).details.unwrap()["node_index"],
            1
        );
        readings(&nodes, std::slice::from_ref(&good)).unwrap_err();
        let values = readings(&nodes, &[good.clone(), good]).unwrap();
        assert!(values.iter().all(|reading| reading.source_time.is_none()));
    }
}

#[cfg(test)]
mod completion_tests {
    use super::task_error;
    use crate::types::{ErrorKind, classify};
    use opcua::types::StatusCode;
    #[test]
    fn completed_loop_omits_invented_native_error_but_preserves_actual_bad_status() {
        let ended = task_error(&Ok(StatusCode::Good));
        let classified = classify(&ended);
        assert_eq!(classified.kind, ErrorKind::Unavailable);
        let details = classified.details.unwrap();
        assert!(details.get("opcua_status").is_none());
        assert_eq!(details["sdk_status_raw"], 0);
        assert_eq!(details["sdk_error_status_present"], false);
        let failed = task_error(&Ok(StatusCode::BadDecodingError));
        let details = classify(&failed).details.unwrap();
        assert_eq!(details["opcua_status"], StatusCode::BadDecodingError.bits());
        assert!(details.get("sdk_error_status_present").is_none());
    }
}
