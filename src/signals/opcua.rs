//! OPC UA with the async-opcua client on a current-thread tokio runtime that
//! lives only in the helper. Security policy None only; polls `Read` on the
//! log interval and browses breadth-first for `signals browse`.
use super::{BrowseItem, Mode, Quality, Sample, TagInfo, Target, Value, helper::Ctx};
use anyhow::{Context, Result, anyhow, bail};
use opcua::{
    client::{ClientBuilder, IdentityToken, Session},
    crypto::SecurityPolicy,
    types::{
        AttributeId, BrowseDescription, BrowseDirection, BrowseResultMask, DataValue,
        MessageSecurityMode, NodeClass, NodeId, ReadValueId, ReferenceTypeId, StatusCode,
        TimestampsToReturn, Variant,
    },
};
use std::{
    collections::{BTreeSet, VecDeque},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Deepest level browsed below the root.
const BROWSE_DEPTH: usize = 4;
/// Decoding limits. The library defaults (1000-element arrays, five 64 KiB
/// chunks) make a browse of a folder with more than 1000 children, or a read
/// of a larger array, fail with a closed connection, and servers may ignore
/// the requested maximum references per node. The helper process bounds the
/// damage, so these are generous but still finite.
const MAX_ARRAY_LENGTH: usize = 100_000;
const MAX_CHUNK_COUNT: usize = 256;
const MAX_MESSAGE_SIZE: usize = 16 << 20;

fn target(ctx: &Ctx) -> (&str, &[String], Option<&str>) {
    match &ctx.uri.target {
        Target::Opcua {
            endpoint,
            nodes,
            root,
        } => (endpoint, nodes, root.as_deref()),
        _ => unreachable!(),
    }
}

fn node_ids(nodes: &[String]) -> Result<Vec<NodeId>> {
    nodes
        .iter()
        .map(|n| NodeId::from_str(n).map_err(|_| anyhow!("OPC UA node ID '{n}' is malformed")))
        .collect()
}

/// `host:port` of an `opc.tcp://host:port[/path]` endpoint.
fn endpoint_addr(endpoint: &str) -> &str {
    let rest = endpoint.strip_prefix("opc.tcp://").unwrap_or(endpoint);
    rest.split('/').next().unwrap_or(rest)
}

/// Fail fast, and classifiably, when nothing listens on the endpoint: the
/// OPC UA client would otherwise retry a refused connection until the
/// timeout and report it as a timeout.
async fn probe(addr: &str, timeout: Duration) -> Result<()> {
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
        Err(_) => bail!(
            "OPC UA connection to {addr} timed out after {} ms",
            timeout.as_millis()
        ),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            bail!("OPC UA connection refused by {addr}")
        }
        Ok(Err(e)) => bail!("OPC UA connection to {addr} failed: {e}"),
        Ok(Ok(_)) => Ok(()),
    }
}

type Connection = (Arc<Session>, tokio::task::JoinHandle<StatusCode>);

async fn connect(ctx: &Ctx) -> Result<Connection> {
    let (endpoint, _, _) = target(ctx);
    probe(endpoint_addr(endpoint), ctx.timeout()).await?;
    let mut builder = ClientBuilder::new()
        .application_name("Capturefab")
        .application_uri("urn:capturefab")
        .product_uri("urn:capturefab")
        .create_sample_keypair(false)
        .trust_server_certs(false)
        .session_retry_limit(0)
        .max_array_length(MAX_ARRAY_LENGTH)
        .max_chunk_count(MAX_CHUNK_COUNT)
        .max_message_size(MAX_MESSAGE_SIZE)
        .request_timeout(ctx.timeout());
    if let Some(pki) = &ctx.spec.pki_dir {
        builder = builder.pki_dir(pki.clone());
    }
    let mut client = builder
        .client()
        .map_err(|errors| anyhow!("OPC UA client setup failed: {}", errors.join("; ")))?;
    let identity = match &ctx.uri.user {
        Some(user) => {
            eprintln!(
                "capturefab signal helper: OPC UA security policy None sends the password unencrypted"
            );
            IdentityToken::new_user_name(user.clone(), ctx.uri.password.clone().unwrap_or_default())
        }
        None => IdentityToken::new_anonymous(),
    };
    let connect = client.connect_to_matching_endpoint(
        (
            endpoint,
            SecurityPolicy::None.to_str(),
            MessageSecurityMode::None,
        ),
        identity,
    );
    let (session, event_loop) = tokio::time::timeout(ctx.timeout(), connect)
        .await
        .map_err(|_| {
            anyhow!(
                "OPC UA connection timed out after {} ms",
                ctx.spec.timeout_ms
            )
        })?
        .map_err(|e| anyhow!("OPC UA connection failed: {e}"))?;
    // With no session retries, a rejected session (for example
    // BadUserAccessDenied) ends the event loop at once with its status
    // instead of retrying until the timeout; `log` reconnects itself.
    let mut handle = event_loop.spawn();
    let deadline = Instant::now() + ctx.timeout();
    loop {
        let step =
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now()));
        if let Ok(true) = tokio::time::timeout(step, session.wait_for_connection()).await {
            return Ok((session, handle));
        }
        if handle.is_finished() {
            let status = (&mut handle)
                .await
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "event loop failed".into());
            bail!("OPC UA session could not be established: {status}");
        }
        if Instant::now() >= deadline {
            handle.abort();
            bail!(
                "OPC UA session could not be established: timed out after {} ms",
                ctx.spec.timeout_ms
            );
        }
    }
}

/// Map a Variant to a sample value; arrays and structures become text.
fn value(variant: &Variant) -> (Value, &'static str) {
    let number = |x: f64| super::sanitize_number(x).0;
    match variant {
        Variant::Boolean(b) => (Value::Bool(*b), "bool"),
        Variant::SByte(x) => (number(*x as f64), "number"),
        Variant::Byte(x) => (number(*x as f64), "number"),
        Variant::Int16(x) => (number(*x as f64), "number"),
        Variant::UInt16(x) => (number(*x as f64), "number"),
        Variant::Int32(x) => (number(*x as f64), "number"),
        Variant::UInt32(x) => (number(*x as f64), "number"),
        Variant::Int64(x) => (number(*x as f64), "number"),
        Variant::UInt64(x) => (number(*x as f64), "number"),
        Variant::Float(x) => (number(*x as f64), "number"),
        Variant::Double(x) => (number(*x), "number"),
        Variant::String(s) => (Value::text(s.as_ref()), "text"),
        Variant::LocalizedText(t) => (Value::text(t.text.as_ref()), "text"),
        Variant::DateTime(t) => (Value::text(&t.as_chrono().to_rfc3339()), "text"),
        Variant::Empty => (Value::text(""), "text"),
        Variant::Array(array) => {
            let items: Vec<String> = array
                .values
                .iter()
                .map(|v| match value(v).0 {
                    Value::Text(t) => serde_json::Value::String(t).to_string(),
                    other => other.to_string(),
                })
                .collect();
            (Value::text(&format!("[{}]", items.join(","))), "text")
        }
        other => (Value::text(&format!("{other:?}")), "text"),
    }
}

fn quality(status: Option<StatusCode>) -> Quality {
    match status {
        None => Quality::Good,
        Some(s) if s.is_good() => Quality::Good,
        Some(s) if s.is_uncertain() => Quality::Uncertain,
        Some(_) => Quality::Bad,
    }
}

fn sample(tag: &str, data: &DataValue) -> Sample {
    let (v, _) = data
        .value
        .as_ref()
        .map(value)
        .unwrap_or((Value::text(""), "text"));
    let non_finite = matches!(&v, Value::Text(t) if t == "NaN" || t == "inf" || t == "-inf")
        && matches!(&data.value, Some(Variant::Float(_) | Variant::Double(_)));
    let empty = matches!(data.value, None | Some(Variant::Empty));
    let mut sample = Sample::now(tag, v, quality(data.status));
    if non_finite || empty {
        sample.quality = Quality::Bad;
    }
    if empty && let Some(status) = data.status.filter(|s| !s.is_good()) {
        // A read with no value, such as BadNodeIdUnknown, says why.
        sample.value = Value::text(&status.to_string());
    }
    sample.source_ts_ms = data
        .source_timestamp
        .as_ref()
        .and_then(|t| u64::try_from(t.as_chrono().timestamp_millis()).ok());
    sample
}

async fn read(ctx: &Ctx, session: &Session, ids: &[ReadValueId], tags: &[String]) -> Result<()> {
    let values = session
        .read(ids, TimestampsToReturn::Both, 0.0)
        .await
        .map_err(|e| anyhow!("OPC UA read failed: {e}"))?;
    for (tag, data) in tags.iter().zip(values.iter()) {
        ctx.sample(sample(tag, data));
    }
    Ok(())
}

async fn browse(ctx: &Ctx, session: &Session, duration: Duration, limit: usize) -> Result<bool> {
    let (_, _, root) = target(ctx);
    let root = node_ids(&[root.unwrap_or("i=85").to_string()])?.remove(0);
    let deadline = Instant::now() + duration;
    let mut queue = VecDeque::from([(root, 0usize)]);
    let mut seen = BTreeSet::new();
    let mut items: Vec<BrowseItem> = Vec::new();
    let mut variables = Vec::new();
    let mut truncated = false;
    while let Some((node, depth)) = queue.pop_front() {
        if Instant::now() >= deadline || ctx.stopped() {
            truncated = true;
            break;
        }
        let description = BrowseDescription {
            node_id: node,
            browse_direction: BrowseDirection::Forward,
            reference_type_id: ReferenceTypeId::HierarchicalReferences.into(),
            include_subtypes: true,
            node_class_mask: 0,
            result_mask: BrowseResultMask::All as u32,
        };
        let mut results = session
            .browse(
                &[description],
                limit.saturating_sub(items.len()).clamp(1, 1000) as u32,
                None,
            )
            .await
            .map_err(|e| anyhow!("OPC UA browse failed: {e}"))?;
        let mut references = Vec::new();
        while let Some(result) = results.pop() {
            references.extend(result.references.unwrap_or_default());
            let point = result.continuation_point;
            if point.is_null() || items.len() + references.len() >= limit {
                if !point.is_null() {
                    let _ = session.browse_next(true, &[point]).await;
                }
                break;
            }
            results = session
                .browse_next(false, &[point])
                .await
                .map_err(|e| anyhow!("OPC UA browse failed: {e}"))?;
        }
        for reference in references {
            let id = reference.node_id.node_id.clone();
            if !seen.insert(id.to_string()) {
                continue;
            }
            if items.len() >= limit {
                truncated = true;
                break;
            }
            let class = reference.node_class;
            if class == NodeClass::Variable {
                variables.push((items.len(), id.clone()));
            }
            items.push(BrowseItem {
                id: id.to_string(),
                name: reference.display_name.text.as_ref().to_string(),
                kind: format!("{class:?}").to_ascii_lowercase(),
                data_type: None,
                unit: None,
                value: None,
            });
            if depth + 1 < BROWSE_DEPTH && matches!(class, NodeClass::Object | NodeClass::Variable)
            {
                queue.push_back((id, depth + 1));
            }
        }
        if truncated {
            break;
        }
    }
    // One batched read of the variables' values and data types.
    for chunk in variables.chunks(500) {
        let ids: Vec<ReadValueId> = chunk
            .iter()
            .flat_map(|(_, id)| {
                [AttributeId::Value, AttributeId::DataType].map(|attribute| ReadValueId {
                    node_id: id.clone(),
                    attribute_id: attribute as u32,
                    ..Default::default()
                })
            })
            .collect();
        let Ok(values) = session.read(&ids, TimestampsToReturn::Neither, 0.0).await else {
            break;
        };
        for ((index, _), pair) in chunk.iter().zip(values.chunks(2)) {
            let item = &mut items[*index];
            if let Some(v) = pair
                .first()
                .and_then(|d| d.value.as_ref())
                .filter(|v| !matches!(v, Variant::Empty))
            {
                item.value = Some(value(v).0);
            }
            if let Some(Variant::NodeId(t)) = pair.get(1).and_then(|d| d.value.as_ref()) {
                item.data_type = Some(t.to_string());
            }
        }
    }
    ctx.browse(items);
    Ok(truncated)
}

async fn disconnect((session, handle): Connection) {
    let _ = tokio::time::timeout(Duration::from_secs(2), session.disconnect()).await;
    handle.abort();
}

/// Sleep until `until` in short steps; false when stopped.
async fn sleep_until(ctx: &Ctx, until: Instant) -> bool {
    loop {
        if ctx.stopped() {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50).min(until - now)).await;
    }
}

/// Poll on the log schedule. When the session's event loop ends (it makes
/// one reconnect attempt after a lost connection), reconnect with a new
/// session, backing off from 1 s to 30 s. Errors are reported
/// when they start or change, not on every tick.
async fn log(ctx: &Ctx, connection: Connection, reads: &[ReadValueId], tags: &[String]) {
    let mut connection = Some(connection);
    let mut tick = Instant::now();
    let mut backoff = Duration::from_secs(1);
    let mut last_error: Option<String> = None;
    let report = |error: anyhow::Error, last_error: &mut Option<String>| {
        let message = format!("{error:#}");
        if last_error.as_deref() != Some(message.as_str()) {
            ctx.reconnecting(&error);
            *last_error = Some(message);
        }
    };
    loop {
        if ctx.stopped() {
            break;
        }
        if connection
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished())
        {
            // The event loop gave up; its session cannot recover.
            if let Some(dead) = connection.take() {
                disconnect(dead).await;
            }
        }
        match &connection {
            Some((session, _)) => match read(ctx, session, reads, tags).await {
                Ok(()) => {
                    if last_error.take().is_some() {
                        ctx.connected();
                    }
                    backoff = Duration::from_secs(1);
                }
                Err(error) => report(error, &mut last_error),
            },
            None => match connect(ctx).await {
                Ok(fresh) => {
                    connection = Some(fresh);
                    continue;
                }
                Err(error) => report(error, &mut last_error),
            },
        }
        ctx.flush_dropped();
        let until = if connection.is_some() {
            ctx.next_tick(&mut tick)
        } else {
            let until = Instant::now() + backoff;
            backoff = (backoff * 2).min(Duration::from_secs(30));
            until
        };
        if !sleep_until(ctx, until).await {
            break;
        }
    }
    if let Some(connection) = connection {
        disconnect(connection).await;
    }
}

pub fn run(ctx: &Ctx) -> Result<bool> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start OPC UA runtime")?;
    runtime.block_on(async {
        let (_, nodes, _) = target(ctx);
        let ids = node_ids(nodes)?;
        let reads: Vec<ReadValueId> = ids.iter().map(ReadValueId::from).collect();
        let infos: Vec<TagInfo> = nodes
            .iter()
            .map(|n| TagInfo {
                tag: n.clone(),
                unit: None,
                kind: None,
            })
            .collect();
        if !matches!(ctx.mode(), Mode::Browse { .. }) && nodes.is_empty() {
            bail!("opc.tcp:// read and start need at least one ?node=NODE_ID");
        }
        let connection = connect(ctx).await?;
        let session = connection.0.clone();
        let result = match ctx.mode() {
            Mode::Browse { duration_ms, limit } => {
                browse(
                    ctx,
                    &session,
                    Duration::from_millis(duration_ms),
                    limit as usize,
                )
                .await
            }
            Mode::Read => {
                ctx.ready(infos);
                read(ctx, &session, &reads, nodes).await.map(|()| false)
            }
            Mode::Log => {
                ctx.ready(infos);
                ctx.connected();
                drop(session);
                log(ctx, connection, &reads, nodes).await;
                return Ok(false);
            }
        };
        drop(session);
        disconnect(connection).await;
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use opcua::types::Array;

    #[test]
    fn endpoint_addresses() {
        assert_eq!(endpoint_addr("opc.tcp://h:4840/a/b"), "h:4840");
        assert_eq!(endpoint_addr("opc.tcp://[::1]:4841"), "[::1]:4841");
    }

    #[test]
    fn arrays_are_compact_text_and_empty_is_blank() {
        let numbers = Variant::Array(Box::new(
            Array::new(
                opcua::types::VariantScalarTypeId::Double,
                vec![Variant::Double(1.0), Variant::Double(2.5)],
            )
            .unwrap(),
        ));
        assert_eq!(value(&numbers).0, Value::text("[1,2.5]"));
        let words = Variant::Array(Box::new(
            Array::new(
                opcua::types::VariantScalarTypeId::String,
                vec![Variant::from("a\"b"), Variant::from("c")],
            )
            .unwrap(),
        ));
        assert_eq!(value(&words).0, Value::text(r#"["a\"b","c"]"#));
        assert_eq!(value(&Variant::Empty).0, Value::text(""));
    }

    #[test]
    fn missing_values_carry_their_status() {
        for data in [
            DataValue {
                value: Some(Variant::Empty),
                status: Some(StatusCode::BadNodeIdUnknown),
                ..Default::default()
            },
            DataValue {
                value: None,
                status: Some(StatusCode::BadNodeIdUnknown),
                ..Default::default()
            },
        ] {
            let s = sample("t", &data);
            assert_eq!(s.quality, Quality::Bad);
            assert_eq!(s.value, Value::text("BadNodeIdUnknown"));
        }
        let empty = sample("t", &DataValue::default());
        assert_eq!(empty.quality, Quality::Bad);
        assert_eq!(empty.value, Value::text(""));
    }

    #[test]
    fn refused_connections_are_unavailable_not_timeouts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            // Windows takes about two seconds to report a refused connection.
            .block_on(probe(&addr, Duration::from_secs(10)))
            .unwrap_err();
        assert_eq!(crate::cli::error_code(&error), ("unavailable", 3));
    }
}
