//! MTConnect agents: polls `/current` over HTTP(S) with ureq and parses the
//! streams with roxmltree. Unchanged items (same sequence) are skipped.
use super::{BrowseItem, Mode, Quality, Sample, TagInfo, Target, Value, helper::Ctx};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

/// Largest `/current` document read.
const MAX_BODY: u64 = 4 << 20;

/// One data item from a `/current` document.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub sample: Sample,
    /// samples, events or condition.
    pub category: String,
    /// The element name, e.g. Position or Execution.
    pub element: String,
    pub name: Option<String>,
    pub sequence: Option<String>,
}

/// Parse every data item in a streams document.
pub fn parse_items(xml: &str, now_ms: u64) -> Result<Vec<Item>> {
    let document = roxmltree::Document::parse(xml).context("MTConnect response is not XML")?;
    let root = document.root_element();
    if root.tag_name().name() == "MTConnectError" {
        let text = root
            .descendants()
            .find(|n| n.tag_name().name() == "Error")
            .and_then(|n| n.text())
            .unwrap_or("unknown error");
        bail!(
            "MTConnect agent error: {}",
            super::truncate(text.trim(), 256)
        );
    }
    let mut items = Vec::new();
    for device in root
        .descendants()
        .filter(|n| n.tag_name().name() == "DeviceStream")
    {
        let device_name = device
            .attribute("name")
            .or_else(|| device.attribute("uuid"))
            .unwrap_or("device");
        for group in device.descendants().filter(|n| {
            n.is_element() && matches!(n.tag_name().name(), "Samples" | "Events" | "Condition")
        }) {
            let category = group.tag_name().name().to_ascii_lowercase();
            for node in group.children().filter(|n| n.is_element()) {
                let Some(id) = node.attribute("dataItemId") else {
                    continue;
                };
                let element = node.tag_name().name().to_string();
                let text = node.text().unwrap_or("").trim();
                let condition = category == "condition";
                let (value, quality) = if text == "UNAVAILABLE" || element == "Unavailable" {
                    (Value::Text("UNAVAILABLE".into()), Quality::Bad)
                } else if condition {
                    (Value::Text(element.to_ascii_uppercase()), Quality::Good)
                } else if let Ok(x) = text.parse::<f64>() {
                    super::sanitize_number(x)
                } else {
                    (Value::text(text), Quality::Good)
                };
                let source_ts_ms = node.attribute("timestamp").and_then(parse_time);
                items.push(Item {
                    sample: Sample {
                        ts_ms: now_ms,
                        source_ts_ms,
                        tag: super::truncate(&format!("{device_name}/{id}"), super::MAX_TAG),
                        value,
                        quality,
                    },
                    category: category.clone(),
                    element,
                    name: node.attribute("name").map(str::to_string),
                    sequence: node.attribute("sequence").map(str::to_string),
                });
            }
        }
    }
    Ok(items)
}

fn parse_time(text: &str) -> Option<u64> {
    let ms = chrono::DateTime::parse_from_rfc3339(text)
        .map(|t| t.timestamp_millis())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|t| t.and_utc().timestamp_millis())
        })
        .ok()?;
    u64::try_from(ms).ok()
}

/// New or changed samples, deduplicated by each item's sequence number.
pub fn parse_streams(
    xml: &str,
    now_ms: u64,
    seen: &mut BTreeMap<String, String>,
) -> Result<Vec<Sample>> {
    Ok(parse_items(xml, now_ms)?
        .into_iter()
        .filter(|item| match &item.sequence {
            Some(sequence) => seen
                .insert(item.sample.tag.clone(), sequence.clone())
                .is_none_or(|old| old != *sequence),
            None => true,
        })
        .map(|item| item.sample)
        .collect())
}

pub struct Agent {
    agent: ureq::Agent,
    url: String,
}
impl Agent {
    fn new(ctx: &Ctx) -> Self {
        let Target::Mtconnect { url, .. } = &ctx.uri.target else {
            unreachable!()
        };
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(ctx.timeout()))
            .max_redirects(0)
            .tls_config(tls)
            .build()
            .new_agent();
        Self {
            agent,
            url: url.clone(),
        }
    }
    fn current(&self) -> Result<String> {
        let mut response = self
            .agent
            .get(&self.url)
            .call()
            .context("MTConnect request failed")?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .context("read MTConnect response")?;
        if !(200..300).contains(&status) {
            bail!("MTConnect agent answered HTTP {status}");
        }
        Ok(body)
    }
}

pub fn run(ctx: &Ctx) -> Result<bool> {
    let agent = Agent::new(ctx);
    let infos = |items: &[Item]| -> Vec<TagInfo> {
        items
            .iter()
            .map(|i| TagInfo {
                tag: i.sample.tag.clone(),
                unit: None,
                kind: Some(i.sample.value.kind().into()),
            })
            .collect()
    };
    match ctx.mode() {
        Mode::Read => {
            let items = parse_items(&agent.current()?, crate::scheduling::now_ms())?;
            ctx.ready(infos(&items));
            for item in items {
                ctx.sample(item.sample);
            }
            Ok(false)
        }
        Mode::Browse { limit, .. } => {
            let items = parse_items(&agent.current()?, crate::scheduling::now_ms())?;
            let truncated = items.len() > limit as usize;
            ctx.browse(
                items
                    .into_iter()
                    .take(limit as usize)
                    .map(|i| BrowseItem {
                        id: i.sample.tag.clone(),
                        name: i.name.unwrap_or_else(|| i.sample.tag.clone()),
                        kind: i.category,
                        data_type: Some(i.element),
                        unit: None,
                        value: Some(i.sample.value),
                    })
                    .collect(),
            );
            Ok(truncated)
        }
        Mode::Log => {
            let mut seen = BTreeMap::new();
            let mut known = std::collections::BTreeSet::new();
            ctx.poll_loop(
                || agent.current().map(|_| ()),
                |()| {
                    let items = parse_items(&agent.current()?, crate::scheduling::now_ms())?;
                    let fresh: Vec<Item> = items
                        .iter()
                        .filter(|i| {
                            known.len() < super::MAX_TAGS && known.insert(i.sample.tag.clone())
                        })
                        .cloned()
                        .collect();
                    if !fresh.is_empty() {
                        ctx.ready(infos(&fresh));
                    }
                    for item in items {
                        let changed = match &item.sequence {
                            Some(sequence) => seen
                                .insert(item.sample.tag.clone(), sequence.clone())
                                .is_none_or(|old: String| old != *sequence),
                            None => true,
                        };
                        if changed && known.contains(&item.sample.tag) {
                            ctx.sample(item.sample);
                        }
                    }
                    Ok(())
                },
            )?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0"?>
<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:1.3">
  <Header creationTime="2026-10-17T10:00:00Z" sender="agent"/>
  <Streams>
    <DeviceStream name="mill" uuid="m1">
      <ComponentStream component="Linear" name="X" componentId="x">
        <Samples>
          <Position dataItemId="xpos" name="Xpos" sequence="10" timestamp="2026-10-17T10:00:00.250Z">12.5</Position>
          <Load dataItemId="xload" sequence="11" timestamp="2026-10-17T10:00:00Z">UNAVAILABLE</Load>
        </Samples>
        <Events>
          <Execution dataItemId="exec" sequence="12" timestamp="2026-10-17T10:00:00.5">ACTIVE</Execution>
        </Events>
        <Condition>
          <Warning dataItemId="temp_cond" sequence="13" timestamp="2026-10-17T10:00:00Z" type="TEMPERATURE">hot</Warning>
        </Condition>
      </ComponentStream>
    </DeviceStream>
  </Streams>
</MTConnectStreams>"#;

    #[test]
    fn parses_and_dedupes_streams() {
        let mut seen = BTreeMap::new();
        let samples = parse_streams(XML, 5, &mut seen).unwrap();
        assert_eq!(samples.len(), 4);
        assert_eq!(samples[0].tag, "mill/xpos");
        assert_eq!(samples[0].value, Value::Number(12.5));
        assert_eq!(samples[0].source_ts_ms, Some(1_792_231_200_250));
        assert_eq!(samples[1].quality, Quality::Bad);
        assert_eq!(samples[2].value, Value::Text("ACTIVE".into()));
        assert_eq!(samples[2].source_ts_ms, Some(1_792_231_200_500));
        assert_eq!(samples[3].value, Value::Text("WARNING".into()));
        assert!(parse_streams(XML, 6, &mut seen).unwrap().is_empty());
        let changed = XML.replace("sequence=\"10\"", "sequence=\"20\"");
        assert_eq!(parse_streams(&changed, 7, &mut seen).unwrap().len(), 1);
        let error = parse_items(
            r#"<MTConnectError><Errors><Error errorCode="NO_DEVICE">Could not find device</Error></Errors></MTConnectError>"#,
            0,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Could not find device"));
    }
}
