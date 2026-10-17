//! MTConnect agents: polls `/current` over HTTP(S) with ureq and parses the
//! streams with roxmltree. Unchanged items (same sequence) are skipped.
use super::{BrowseItem, Mode, Quality, Sample, TagInfo, Target, Value, helper::Ctx};
use anyhow::{Context, Result, bail, ensure};
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
        return Err(agent_error(root));
    }
    ensure!(
        root.tag_name().name() == "MTConnectStreams",
        "MTConnect response is a <{}> document instead of MTConnectStreams; check the agent address",
        super::truncate(root.tag_name().name(), 64)
    );
    let mut items: Vec<Item> = Vec::new();
    // Index of each condition tag's item: a data item can have several
    // active conditions at once, which merge into one sample.
    let mut conditions: BTreeMap<String, usize> = BTreeMap::new();
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
                } else if node.children().any(|c| c.is_element()) {
                    // DATA_SET and TABLE representations carry <Entry> children.
                    (Value::text(&entries(node).to_string()), Quality::Good)
                } else if let Ok(x) = text.parse::<f64>() {
                    super::sanitize_number(x)
                } else {
                    (Value::text(text), Quality::Good)
                };
                let source_ts_ms = node.attribute("timestamp").and_then(parse_time);
                let tag = super::truncate(&format!("{device_name}/{id}"), super::MAX_TAG);
                let sequence = node.attribute("sequence").map(str::to_string);
                if condition && let Some(&index) = conditions.get(&tag) {
                    let kept = &mut items[index];
                    if severity(&element) > severity(&kept.element) {
                        kept.sample.value = value;
                        kept.sample.quality = quality;
                        kept.sample.source_ts_ms = source_ts_ms;
                        kept.element = element;
                        kept.name = node.attribute("name").map(str::to_string);
                    }
                    // Any change in the set of active conditions is a change.
                    let joined = [kept.sequence.take(), sequence]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(",");
                    kept.sequence = (!joined.is_empty()).then_some(joined);
                    continue;
                }
                if condition {
                    conditions.insert(tag.clone(), items.len());
                }
                items.push(Item {
                    sample: Sample {
                        ts_ms: now_ms,
                        source_ts_ms,
                        tag,
                        value,
                        quality,
                    },
                    category: category.clone(),
                    element,
                    name: node.attribute("name").map(str::to_string),
                    sequence,
                });
            }
        }
    }
    Ok(items)
}

/// An `MTConnectError` document as an error. 1.x agents put the message in
/// `<Error errorCode="...">text</Error>`; 2.x agents use an element per code
/// (e.g. `<NoDevice errorCode="NO_DEVICE"><ErrorMessage>`).
fn agent_error(root: roxmltree::Node) -> anyhow::Error {
    let error = root
        .descendants()
        .find(|n| n.is_element() && n.attribute("errorCode").is_some());
    let code = error.and_then(|n| n.attribute("errorCode")).unwrap_or("");
    let message = error
        .and_then(|n| {
            n.children()
                .find(|c| c.tag_name().name() == "ErrorMessage")
                .and_then(|c| c.text())
                .or_else(|| n.text())
        })
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("unknown error");
    let message = super::truncate(message, 256);
    let code = super::truncate(code, 64);
    if code == "NO_DEVICE" {
        // "not found" makes this exit 3 (unavailable), like a missing camera.
        anyhow::anyhow!("MTConnect device not found: {message}")
    } else if code.is_empty() {
        anyhow::anyhow!("MTConnect agent error: {message}")
    } else {
        anyhow::anyhow!("MTConnect agent error {code}: {message}")
    }
}

/// Condition levels, least to most severe.
fn severity(element: &str) -> u8 {
    match element {
        "Fault" => 3,
        "Warning" => 2,
        "Normal" => 1,
        _ => 0,
    }
}

/// A DATA_SET or TABLE value as JSON: `{"key": value}`, where a table row's
/// value is an object of its cells. Removed entries are left out.
fn entries(node: roxmltree::Node) -> serde_json::Value {
    fn scalar(node: roxmltree::Node) -> serde_json::Value {
        let text = node.text().unwrap_or("").trim();
        match text.parse::<f64>().ok().filter(|x| x.is_finite()) {
            Some(x) => serde_json::json!(x),
            None => serde_json::Value::String(text.to_string()),
        }
    }
    let mut map = serde_json::Map::new();
    for entry in node.children().filter(|n| n.is_element()) {
        if entry.attribute("removed") == Some("true") {
            continue;
        }
        let key = entry.attribute("key").unwrap_or("").to_string();
        let value = if entry.children().any(|c| c.is_element()) {
            let cells = entry
                .children()
                .filter(|c| c.is_element())
                .map(|c| (c.attribute("key").unwrap_or("").to_string(), scalar(c)))
                .collect();
            serde_json::Value::Object(cells)
        } else {
            scalar(entry)
        };
        map.insert(key, value);
    }
    serde_json::Value::Object(map)
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
            .map_err(|error| match error {
                // ureq says "timeout: global", which `cli::error_code` would not
                // classify as a timeout.
                ureq::Error::Timeout(_) => {
                    anyhow::anyhow!(
                        "MTConnect request failed: the agent did not answer in time (timed out)"
                    )
                }
                error => anyhow::Error::new(error).context("MTConnect request failed"),
            })?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|error| match error {
                ureq::Error::Timeout(_) => anyhow::anyhow!(
                    "read MTConnect response: the agent did not answer in time (timed out)"
                ),
                error => anyhow::Error::new(error).context("read MTConnect response"),
            })?;
        if !(200..300).contains(&status) {
            // Agents answer errors such as an unknown device with an
            // MTConnectError document and a 4xx status; prefer its message.
            if let Ok(document) = roxmltree::Document::parse(&body)
                && document.root_element().tag_name().name() == "MTConnectError"
            {
                return Err(agent_error(document.root_element()));
            }
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

    #[test]
    fn agent_2x_errors_and_foreign_documents() {
        // As served by an MTConnect 2.8 agent (cppagent) for an unknown device.
        let error = parse_items(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MTConnectError xmlns="urn:mtconnect.org:MTConnectError:2.8"><Header creationTime="2026-10-17T18:38:19.228750Z" sender="agent" instanceId="1" version="2.8.0.4" bufferSize="131072"/><Errors><NoDevice errorCode="NO_DEVICE"><ErrorMessage>Could not find the device 'Nope'</ErrorMessage><Request>MTConnectStreams</Request><URI>/Nope/current</URI></NoDevice></Errors></MTConnectError>"#,
            0,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "MTConnect device not found: Could not find the device 'Nope'"
        );
        assert_eq!(crate::cli::error_code(&error).1, 3);
        let error = parse_items(
            r#"<MTConnectError><Errors><InvalidRequest errorCode="INVALID_REQUEST"><ErrorMessage>bad</ErrorMessage></InvalidRequest></Errors></MTConnectError>"#,
            0,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "MTConnect agent error INVALID_REQUEST: bad"
        );
        // A web page or probe document is not a streams document.
        let error = parse_items("<html><body>hi</body></html>", 0).unwrap_err();
        assert!(error.to_string().contains("<html>"), "{error}");
        assert!(parse_items("<MTConnectDevices/>", 0).is_err());
    }

    #[test]
    fn data_sets_tables_and_multiple_conditions() {
        let xml = r#"<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:2.3"><Streams>
<DeviceStream name="vmc" uuid="v"><ComponentStream component="Path" componentId="p">
<Events>
<VariableDataSet dataItemId="vars" count="3" sequence="5" timestamp="2026-10-17T09:30:00Z"><Entry key="100">55.2</Entry><Entry key="103">ON</Entry><Entry key="104" removed="true"/></VariableDataSet>
<WorkOffsetTable dataItemId="woff" count="1" sequence="6" timestamp="2026-10-17T08:00:00Z"><Entry key="G54"><Cell key="X">10.5</Cell><Cell key="Y">-2</Cell></Entry></WorkOffsetTable>
</Events>
<Condition>
<Warning dataItemId="temp" sequence="7" timestamp="2026-10-17T09:59:57Z" type="TEMPERATURE">warm</Warning>
<Fault dataItemId="temp" sequence="8" timestamp="2026-10-17T09:59:59Z" type="TEMPERATURE">hot</Fault>
<Normal dataItemId="load" sequence="9" timestamp="2026-10-17T08:00:00Z" type="LOAD"/>
</Condition>
</ComponentStream></DeviceStream></Streams></MTConnectStreams>"#;
        let items = parse_items(xml, 0).unwrap();
        let values: Vec<_> = items
            .iter()
            .map(|i| (i.sample.tag.as_str(), i.sample.value.clone()))
            .collect();
        assert_eq!(
            values,
            [
                ("vmc/vars", Value::Text(r#"{"100":55.2,"103":"ON"}"#.into())),
                (
                    "vmc/woff",
                    Value::Text(r#"{"G54":{"X":10.5,"Y":-2.0}}"#.into())
                ),
                ("vmc/temp", Value::Text("FAULT".into())),
                ("vmc/load", Value::Text("NORMAL".into())),
            ]
        );
        assert_eq!(items[2].sequence.as_deref(), Some("7,8"));
        // Two active conditions on one item must not re-log on every poll.
        let mut seen = BTreeMap::new();
        assert_eq!(parse_streams(xml, 1, &mut seen).unwrap().len(), 4);
        assert!(parse_streams(xml, 2, &mut seen).unwrap().is_empty());
        let cleared = xml.replace(
            r#"<Fault dataItemId="temp" sequence="8" timestamp="2026-10-17T09:59:59Z" type="TEMPERATURE">hot</Fault>"#,
            "",
        );
        let samples = parse_streams(&cleared, 3, &mut seen).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].value, Value::Text("WARNING".into()));
    }
}
