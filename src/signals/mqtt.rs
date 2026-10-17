//! MQTT and MQTT over TLS with rumqttc's blocking client. Payloads become
//! samples through the pure `decode_payload`, so decoding is tested without
//! a broker.
use super::{
    BrowseItem, MAX_PAYLOAD, MAX_TAGS, Mode, Quality, Sample, TagInfo, Target, Value, helper::Ctx,
    sanitize_number,
};
use anyhow::{Result, anyhow, bail};
use rumqttc::{Client, ConnectionError, Event, MqttOptions, Packet, QoS, RecvTimeoutError};
use serde_json::Value as Json;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// Most leaves taken from one JSON payload.
const MAX_LEAVES: usize = 256;
/// Deepest JSON object level flattened.
const MAX_DEPTH: usize = 4;

/// Decode one message: a number, `true`/`false`, a JSON object flattened to
/// `topic/field/sub`, or text. Top-level `ts`, `timestamp` or `time` fields
/// become `source_ts_ms` (values below 1e11 are seconds).
pub fn decode_payload(topic: &str, payload: &[u8], now_ms: u64) -> Vec<Sample> {
    let stamp = |tag: &str, value: Value, quality: Quality, source: Option<u64>| Sample {
        ts_ms: now_ms,
        source_ts_ms: source,
        tag: super::truncate(tag, super::MAX_TAG),
        value,
        quality,
    };
    if payload.len() > MAX_PAYLOAD {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(payload);
    let trimmed = text.trim();
    if trimmed == "true" || trimmed == "false" {
        return vec![stamp(
            topic,
            Value::Bool(trimmed == "true"),
            Quality::Good,
            None,
        )];
    }
    if let Ok(x) = trimmed.parse::<f64>() {
        let (value, quality) = sanitize_number(x);
        return vec![stamp(topic, value, quality, None)];
    }
    if trimmed.starts_with('{')
        && let Ok(Json::Object(map)) = serde_json::from_str::<Json>(trimmed)
    {
        let source = ["ts", "timestamp", "time"].iter().find_map(|k| {
            let x = map.get(*k)?.as_f64()?;
            (x.is_finite() && x > 0.0).then_some(if x < 1e11 {
                (x * 1000.0) as u64
            } else {
                x as u64
            })
        });
        let mut out = Vec::new();
        flatten(topic, &Json::Object(map), 0, &mut |tag, value| {
            let (value, quality) = match value {
                Json::Bool(b) => (Value::Bool(*b), Quality::Good),
                Json::Number(n) => sanitize_number(n.as_f64().unwrap_or(f64::NAN)),
                Json::String(s) => (Value::text(s), Quality::Good),
                _ => return,
            };
            if out.len() < MAX_LEAVES {
                out.push(stamp(&tag, value, quality, source));
            }
        });
        return out;
    }
    vec![stamp(topic, Value::text(&text), Quality::Good, None)]
}

fn flatten(prefix: &str, value: &Json, depth: usize, emit: &mut impl FnMut(String, &Json)) {
    match value {
        Json::Object(map) if depth < MAX_DEPTH => {
            for (key, value) in map {
                if depth == 0 && matches!(key.as_str(), "ts" | "timestamp" | "time") {
                    continue;
                }
                flatten(&format!("{prefix}/{key}"), value, depth + 1, emit);
            }
        }
        // Arrays, nulls and objects past the depth limit are skipped.
        Json::Object(_) | Json::Array(_) | Json::Null => {}
        leaf => emit(prefix.to_string(), leaf),
    }
}

fn options(ctx: &Ctx) -> Result<(MqttOptions, String, QoS)> {
    let Target::Mqtt {
        tls,
        filter,
        client_id,
        qos,
    } = &ctx.uri.target
    else {
        unreachable!()
    };
    let id = client_id.clone().unwrap_or_else(|| {
        let mut bytes = [0u8; 2];
        let _ = getrandom::fill(&mut bytes);
        format!(
            "capturefab-{}-{:02x}{:02x}",
            std::process::id(),
            bytes[0],
            bytes[1]
        )
    });
    let mut options = MqttOptions::new(id, ctx.uri.host.clone(), ctx.uri.port);
    options
        .set_keep_alive(Duration::from_secs(15))
        .set_max_packet_size(MAX_PAYLOAD + 8 * 1024, 16 * 1024)
        .set_clean_session(true);
    if let Some(user) = &ctx.uri.user {
        options.set_credentials(user.clone(), ctx.uri.password.clone().unwrap_or_default());
    }
    if *tls {
        use rustls_platform_verifier::BuilderVerifierExt;
        let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth();
        options.set_transport(rumqttc::Transport::tls_with_config(
            rumqttc::TlsConfiguration::Rustls(std::sync::Arc::new(config)),
        ));
    }
    let qos = if *qos == 1 {
        QoS::AtLeastOnce
    } else {
        QoS::AtMostOnce
    };
    Ok((options, filter.clone(), qos))
}

fn connection_error(error: ConnectionError) -> anyhow::Error {
    match error {
        // Not "connection refused": a rejected login is not an unreachable broker.
        ConnectionError::ConnectionRefused(code) => {
            anyhow!("MQTT broker rejected the login ({code:?})")
        }
        ConnectionError::NetworkTimeout => anyhow!("MQTT broker connection timed out"),
        other => anyhow!("MQTT connection failed: {other}"),
    }
}

/// Collect messages until `until`, keeping the last sample per tag. Fails if
/// the broker cannot be reached or the subscription is not acknowledged.
fn collect(
    ctx: &Ctx,
    until_after_suback: Duration,
    limit: usize,
) -> Result<(BTreeMap<String, Sample>, bool)> {
    let (options, filter, qos) = options(ctx)?;
    let (client, mut connection) = Client::new(options, 64);
    client.subscribe(filter, qos)?;
    let connect_deadline = Instant::now() + ctx.timeout();
    let mut deadline = None;
    let mut latest = BTreeMap::new();
    let mut truncated = false;
    loop {
        let now = Instant::now();
        let end = deadline.unwrap_or(connect_deadline);
        if now >= end || ctx.stopped() {
            if deadline.is_none() && !ctx.stopped() {
                bail!(
                    "MQTT subscription timed out after {} ms",
                    ctx.spec.timeout_ms
                );
            }
            break;
        }
        match connection.recv_timeout((end - now).min(Duration::from_millis(100))) {
            Ok(Ok(Event::Incoming(Packet::SubAck(_)))) => {
                deadline = Some(Instant::now() + until_after_suback);
            }
            Ok(Ok(Event::Incoming(Packet::Publish(message)))) => {
                for sample in decode_payload(
                    &message.topic,
                    &message.payload,
                    crate::scheduling::now_ms(),
                ) {
                    if latest.len() >= limit && !latest.contains_key(&sample.tag) {
                        truncated = true;
                        continue;
                    }
                    latest.insert(sample.tag.clone(), sample);
                }
            }
            Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => return Err(connection_error(error)),
            Err(RecvTimeoutError::Disconnected) => bail!("MQTT connection closed"),
        }
    }
    let _ = client.disconnect();
    Ok((latest, truncated))
}

pub fn run(ctx: &Ctx) -> Result<bool> {
    match ctx.mode() {
        Mode::Read => {
            let window = ctx.timeout().min(Duration::from_secs(2));
            let (latest, _) = collect(ctx, window, MAX_TAGS)?;
            ctx.ready(tag_infos(latest.values()));
            for sample in latest.into_values() {
                ctx.sample(sample);
            }
            Ok(false)
        }
        Mode::Browse { duration_ms, limit } => {
            let (latest, truncated) =
                collect(ctx, Duration::from_millis(duration_ms), limit as usize)?;
            ctx.browse(
                latest
                    .into_values()
                    .map(|sample| BrowseItem {
                        id: sample.tag.clone(),
                        name: sample.tag,
                        kind: "topic".into(),
                        data_type: Some(sample.value.kind().into()),
                        unit: None,
                        value: Some(sample.value),
                    })
                    .collect(),
            );
            Ok(truncated)
        }
        Mode::Log => log(ctx).map(|()| false),
    }
}

fn tag_infos<'a>(samples: impl Iterator<Item = &'a Sample>) -> Vec<TagInfo> {
    samples
        .map(|s| TagInfo {
            tag: s.tag.clone(),
            unit: None,
            kind: Some(s.value.kind().into()),
        })
        .collect()
}

fn log(ctx: &Ctx) -> Result<()> {
    let (options, filter, qos) = options(ctx)?;
    let (client, mut connection) = Client::new(options, 64);
    client.subscribe(filter.clone(), qos)?;
    let interval_ms = ctx.spec.interval_ms;
    // Per-tag throttle: the last time each tag was emitted.
    let mut last: BTreeMap<String, u64> = BTreeMap::new();
    let mut connected = false;
    let mut ever_connected = false;
    let connect_deadline = Instant::now() + ctx.timeout();
    let mut backoff = Duration::from_secs(1);
    while !ctx.stopped() {
        if !ever_connected && Instant::now() >= connect_deadline {
            bail!(
                "MQTT broker connection timed out after {} ms",
                ctx.spec.timeout_ms
            );
        }
        match connection.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                // A clean session forgets subscriptions; renew after reconnects.
                if ever_connected {
                    client.subscribe(filter.clone(), qos)?;
                }
            }
            Ok(Ok(Event::Incoming(Packet::SubAck(_)))) => {
                if !connected {
                    connected = true;
                    ever_connected = true;
                    backoff = Duration::from_secs(1);
                    ctx.connected();
                }
            }
            Ok(Ok(Event::Incoming(Packet::Publish(message)))) => {
                if message.payload.len() > MAX_PAYLOAD {
                    ctx.drop_sample();
                    continue;
                }
                let now = crate::scheduling::now_ms();
                let mut new_tags = Vec::new();
                let mut keep = Vec::new();
                for sample in decode_payload(&message.topic, &message.payload, now) {
                    let full = last.len() >= MAX_TAGS;
                    match last.get_mut(&sample.tag) {
                        Some(at) if now.saturating_sub(*at) < interval_ms => continue,
                        Some(at) => *at = now,
                        None if full => {
                            ctx.drop_sample();
                            continue;
                        }
                        None => {
                            last.insert(sample.tag.clone(), now);
                            new_tags.push(TagInfo {
                                tag: sample.tag.clone(),
                                unit: None,
                                kind: Some(sample.value.kind().into()),
                            });
                        }
                    }
                    keep.push(sample);
                }
                if !new_tags.is_empty() {
                    ctx.ready(new_tags);
                }
                for sample in keep {
                    ctx.sample(sample);
                }
                ctx.flush_dropped();
            }
            Ok(Ok(_)) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => {
                let error = connection_error(error);
                if !ever_connected {
                    return Err(error);
                }
                connected = false;
                ctx.reconnecting(&error);
                if !ctx.sleep_until(Instant::now() + backoff) {
                    break;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Err(RecvTimeoutError::Disconnected) => bail!("MQTT connection closed"),
        }
    }
    let _ = client.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_payloads() {
        let s = decode_payload("a/t", b" 21.5 ", 7);
        assert_eq!(s[0].value, Value::Number(21.5));
        assert_eq!(s[0].ts_ms, 7);
        assert_eq!(decode_payload("a", b"true", 0)[0].value, Value::Bool(true));
        let s = decode_payload("a", b"NaN", 0);
        assert_eq!(
            (s[0].value.clone(), s[0].quality),
            (Value::Text("NaN".into()), Quality::Bad)
        );
        let s = decode_payload(
            "m",
            br#"{"ts":1700000000,"temp":20,"ok":true,"name":"x","n":null,"arr":[1],"a":{"b":{"c":{"d":{"e":1}}}}}"#,
            0,
        );
        let tags: Vec<_> = s.iter().map(|s| s.tag.as_str()).collect();
        assert_eq!(tags, ["m/name", "m/ok", "m/temp"]);
        assert!(s.iter().all(|s| s.source_ts_ms == Some(1_700_000_000_000)));
        let s = decode_payload("m", br#"{"timestamp":1700000000123,"a":{"b":2}}"#, 0);
        assert_eq!(s[0].tag, "m/a/b");
        assert_eq!(s[0].source_ts_ms, Some(1_700_000_000_123));
        let many: serde_json::Map<String, Json> =
            (0..400).map(|i| (format!("k{i}"), Json::from(i))).collect();
        let body = serde_json::to_vec(&many).unwrap();
        assert_eq!(decode_payload("m", &body, 0).len(), MAX_LEAVES);
        assert!(decode_payload("m", &vec![b'1'; MAX_PAYLOAD + 1], 0).is_empty());
        let text = "é".repeat(1000);
        let s = decode_payload("m", text.as_bytes(), 0);
        let Value::Text(t) = &s[0].value else {
            panic!()
        };
        assert!(t.len() <= 1024 && t.ends_with('…'));
    }
}
