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

async fn connect(ctx: &Ctx) -> Result<(Arc<Session>, tokio::task::JoinHandle<StatusCode>)> {
    let (endpoint, _, _) = target(ctx);
    let mut builder = ClientBuilder::new()
        .application_name("Capturefab")
        .application_uri("urn:capturefab")
        .product_uri("urn:capturefab")
        .create_sample_keypair(false)
        .trust_server_certs(false)
        .session_retry_limit(3)
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
    let handle = event_loop.spawn();
    let connected = tokio::time::timeout(ctx.timeout(), session.wait_for_connection())
        .await
        .unwrap_or(false);
    if !connected {
        handle.abort();
        bail!("OPC UA session could not be established");
    }
    Ok((session, handle))
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
    let mut sample = Sample::now(tag, v, quality(data.status));
    if non_finite || data.value.is_none() {
        sample.quality = Quality::Bad;
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
            .browse(&[description], 1000, None)
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
            if let Some(v) = pair.first().and_then(|d| d.value.as_ref()) {
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
        let (session, handle) = connect(ctx).await?;
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
                let mut tick = Instant::now();
                let mut failing = false;
                loop {
                    match read(ctx, &session, &reads, nodes).await {
                        Ok(()) if failing => {
                            failing = false;
                            ctx.connected();
                        }
                        Ok(()) => {}
                        Err(error) => {
                            // The session's event loop reconnects on its own.
                            failing = true;
                            ctx.reconnecting(&error);
                        }
                    }
                    ctx.flush_dropped();
                    let until = ctx.next_tick(&mut tick);
                    while Instant::now() < until {
                        if ctx.stopped() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50).min(until - Instant::now()))
                            .await;
                    }
                    if ctx.stopped() {
                        break Ok(false);
                    }
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), session.disconnect()).await;
        handle.abort();
        result
    })
}
