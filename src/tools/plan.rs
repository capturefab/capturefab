//! Portable, credential-free plan documents. Reading never executes a call.
use super::{MAX_PLAN_CALLS, PLAN_VERSION, ToolCall, ToolKind};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs::OpenOptions, io::Write, path::Path};

pub const MAX_PLAN_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDocument {
    pub version: u32,
    pub calls: Vec<ToolCall>,
}

impl PlanDocument {
    pub fn new(calls: Vec<ToolCall>) -> Self {
        Self {
            version: PLAN_VERSION,
            calls,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == PLAN_VERSION,
            "Use plan version {PLAN_VERSION}."
        );
        ensure!(
            (1..=MAX_PLAN_CALLS).contains(&self.calls.len()),
            "A plan must contain 1 to {MAX_PLAN_CALLS} calls."
        );
        for (index, call) in self.calls.iter().enumerate() {
            let spec = super::spec(&call.tool).with_context(|| {
                format!("Step {} names an unknown tool; use tools list.", index + 1)
            })?;
            ensure!(
                spec.kind != ToolKind::Read,
                "Step {} is an observation; plans contain control, capture or configure tools only.",
                index + 1
            );
            let (_, issues) = super::schema::check(spec.name, &call.args, &(spec.schema)());
            if let Some(issue) = issues.first() {
                bail!("Step {}: {}", index + 1, issue.message);
            }
        }
        Ok(())
    }
}

/// Prepare a separate local export without changing the in-app/model plan.
/// Anonymous source references need their exact currently approved endpoint so
/// another process can check this version-1 document. Protected account epochs
/// and cached display locations must never become portable connection grants.
pub fn for_local_export(
    document: &PlanDocument,
    approved_sources: &[super::context::DataSourceRef],
) -> Result<PlanDocument> {
    document.validate()?;
    let mut exported = document.clone();
    for (index, call) in exported.calls.iter_mut().enumerate() {
        if !matches!(
            call.tool.as_str(),
            "browse_data_source"
                | "read_data_source"
                | "start_signal_capture"
                | "stop_signal_capture"
                | "wait_for_data"
        ) {
            continue;
        }
        let selected = call.args["source"]
            .as_str()
            .context("Choose a source before exporting this plan.")?;
        let matches = approved_sources
            .iter()
            .filter(|binding| binding.reference == selected || binding.uri == selected)
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "The source in step {} is no longer unambiguously approved for export. Select its current endpoint in Sources before exporting, or run the plan in the app.",
            index + 1
        );
        let binding = matches[0];
        ensure!(
            binding.auth.is_none(),
            "Protected source bindings cannot be exported. Run this plan in the app, or bind the saved profile locally in the target workbench and review the calls there."
        );
        ensure!(
            !binding.uri.is_empty(),
            "A cached source does not grant a fresh connection. Add its complete credential-free endpoint in Sources before exporting, or run the plan in the app."
        );
        binding.validate_local().context(
            "The selected source binding changed. Add its current credential-free endpoint in Sources before exporting.",
        )?;
        call.args["source"] = binding.uri.clone().into();
    }
    // Keep credential and complete-document byte checks on the converted copy.
    to_json(&exported)?;
    Ok(exported)
}

pub fn parse(text: &str) -> Result<PlanDocument> {
    ensure!(
        text.len() <= MAX_PLAN_BYTES,
        "Use a plan no larger than 256 KiB."
    );
    let document: PlanDocument = serde_json::from_value(parse_value(text)?)
        .context("Read a plan object with version and calls.")?;
    document.validate()?;
    Ok(document)
}

/// The same bounded, duplicate-refusing JSON reader serves command-line
/// arguments and complete plans before either can contact a session.
pub(crate) fn parse_value(text: &str) -> Result<serde_json::Value> {
    ensure!(
        text.len() <= MAX_PLAN_BYTES,
        "Use JSON no larger than 256 KiB."
    );
    let strict: StrictValue =
        serde_json::from_str(text).context("Read JSON without duplicate object fields.")?;
    Ok(strict.0)
}

struct StrictValue(serde_json::Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate object fields")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|value| StrictValue(value.into()))
                    .ok_or_else(|| E::custom("Use a finite JSON number."))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                value: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(value.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(serde_json::Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(values.into()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.contains_key(&key) {
                        return Err(serde::de::Error::custom("Remove the duplicate JSON field."));
                    }
                    fields.insert(key, map.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(fields.into()))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Render a plan for saving. Unlike display redaction, export refuses to
/// replace a secret with a nonfunctional placeholder.
pub fn to_json(document: &PlanDocument) -> Result<String> {
    document.validate()?;
    ensure!(
        !document
            .calls
            .iter()
            .any(|call| credential_value(&call.args)),
        "This plan file contains stream credentials. Remove them before exporting the plan."
    );
    let output = serde_json::to_string_pretty(document)?;
    ensure!(
        output.len() <= MAX_PLAN_BYTES,
        "Use a plan no larger than 256 KiB."
    );
    Ok(output)
}

/// Save a new private file, never replacing an existing plan.
pub fn write(path: &Path, document: &PlanDocument) -> Result<()> {
    let output = to_json(document)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("Create a new plan file.")?;
    file.write_all(output.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn credential_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => {
            if !text.contains("://") {
                return false;
            }
            let authority = text
                .split_once("://")
                .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
                .unwrap_or_default();
            authority.contains('@') || super::redact::for_output_text(text) != *text
        }
        serde_json::Value::Array(values) => values.iter().any(credential_value),
        serde_json::Value::Object(fields) => fields.iter().any(|(key, value)| {
            matches!(key.as_str(), "secret" | "password") || credential_value(value)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        context::DataSourceRef,
        tests::{fixture_context, fixture_snapshot},
    };
    use serde_json::json;

    fn document() -> PlanDocument {
        PlanDocument::new(vec![ToolCall {
            tool: "start_live_view".into(),
            args: json!({"camera":"sim:0"}),
        }])
    }

    #[test]
    fn plan_documents_round_trip_and_reject_wrong_shapes() {
        let document = document();
        assert_eq!(parse(&to_json(&document).unwrap()).unwrap(), document);
        for input in [
            r#"{"version":2,"calls":[]}"#,
            r#"{"version":1,"calls":[],"extra":true}"#,
            r#"{"version":1,"calls":[{"tool":"start_live_view","args":{"camera":"sim:0"},"extra":true}]}"#,
            r#"{"version":1,"calls":[{"tool":"get_overview","args":{}}]}"#,
            r#"{"version":1,"calls":[{"tool":"start_live_view","args":{}}]}"#,
            r#"{"version":1,"calls":[{"tool":"start_live_view","args":{"camera":"sim:0","camera":"sim:1"}}]}"#,
        ] {
            assert!(parse(input).is_err(), "{input}");
        }
        assert!(parse(&" ".repeat(MAX_PLAN_BYTES + 1)).is_err());
        let mut too_many = document.clone();
        too_many.calls = vec![document.calls[0].clone(); MAX_PLAN_CALLS + 1];
        assert!(to_json(&too_many).is_err());
    }

    #[test]
    fn exporting_refuses_credentials_in_every_argument() {
        for value in [
            "rtsp://u:p@h/s",
            "rtsp://u@h/s",
            "srt://h?passphrase=x",
            "rtmp://h/live2/ABCDEFGH",
            "rtsp://h/s?api_key=private",
            "https://h/s?%58-Amz-Signature=private",
            "https://h/s?sig=private",
        ] {
            let mut doc = document();
            doc.calls[0].args["camera"] = json!(value);
            let error = to_json(&doc).unwrap_err().to_string();
            assert!(
                error.contains("contains stream credentials"),
                "{value}: {error}"
            );
            assert!(!error.contains(value));
        }
    }

    fn source_document(source: &str) -> PlanDocument {
        PlanDocument::new(vec![ToolCall {
            tool: "read_data_source".into(),
            args: json!({"source":source,"timeout":"1s"}),
        }])
    }

    #[test]
    fn anonymous_source_export_round_trips_into_a_new_cli_context_without_contact() {
        let source = DataSourceRef::approved("sim-signal://portable-plan").unwrap();
        let document = source_document(&source.reference);
        let local = for_local_export(&document, std::slice::from_ref(&source)).unwrap();
        let parsed = parse(&to_json(&local).unwrap()).unwrap();
        assert_eq!(document.calls[0].args["source"], source.reference);
        assert_eq!(parsed.version, PLAN_VERSION);
        assert_eq!(parsed.calls[0].args["source"], source.uri);

        let mut context = fixture_context(fixture_snapshot());
        context.data_sources.clear();
        context.data_profiles.clear();
        context.snapshot.signals.clear();
        context.snapshot.signal_bindings.clear();
        let steps = parsed
            .calls
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, call)| (crate::tools::StepId(index as u64 + 1), call))
            .collect::<Vec<_>>();
        let checked = crate::tools::check_plan(&context, &steps, &|_, _| false);
        assert!(
            checked.iter().all(crate::tools::Checked::ready),
            "{checked:?}"
        );
        assert!(checked[0].ops.iter().any(|operation| matches!(operation,crate::tools::Op::Session { command:crate::session::SessionCommand::SignalRead {uri,auth:None,..},..} if uri==&source.uri)));
    }

    #[test]
    fn local_export_converts_only_known_source_arguments() {
        let source = DataSourceRef::approved("sim-signal://portable-plan").unwrap();
        let mut document = PlanDocument::new(vec![
            ToolCall {
                tool: "browse_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s","limit":10}),
            },
            ToolCall {
                tool: "read_data_source".into(),
                args: json!({"source":source.reference,"timeout":"1s"}),
            },
            ToolCall {
                tool: "start_signal_capture".into(),
                args: json!({"source":source.reference,"every":"100ms","duration":"1s","max_samples":10,"max_bytes":1024,"tags":null}),
            },
            ToolCall {
                tool: "wait_for_data".into(),
                args: json!({"source":source.reference,"timeout":"1s","samples":null}),
            },
            ToolCall {
                tool: "stop_signal_capture".into(),
                args: json!({"source":source.reference}),
            },
            ToolCall {
                tool: "capture_photos".into(),
                args: json!({"camera":"sim:0","count":1,"format":"png","file_name":"photo.png","destination":null,"tags":[{"key":"source","value":source.reference}]}),
            },
            ToolCall {
                tool: "annotate_dataset".into(),
                args: json!({"text":source.reference}),
            },
        ]);
        let original = document.clone();
        let local = for_local_export(&document, std::slice::from_ref(&source)).unwrap();
        for call in &local.calls[..5] {
            assert_eq!(call.args["source"], source.uri);
        }
        assert_eq!(local.calls[5..], document.calls[5..]);
        assert_eq!(document, original);
        document.calls[0].args["source"] = source.uri.clone().into();
        assert_eq!(for_local_export(&document, &[source]).unwrap(), local);
    }

    #[test]
    fn local_export_refuses_missing_stale_cached_ambiguous_and_protected_bindings() {
        let source = DataSourceRef::approved("sim-signal://portable-plan").unwrap();
        let document = source_document(&source.reference);
        let original = document.clone();
        assert!(for_local_export(&document, &[]).is_err());
        let replacement = DataSourceRef::approved("sim-signal://replacement-plan").unwrap();
        assert!(for_local_export(&document, &[replacement]).is_err());
        let mut stale = source.clone();
        stale.uri = "sim-signal://replacement-plan".into();
        assert!(for_local_export(&document, &[stale]).is_err());
        assert!(for_local_export(&document, &[source.clone(), source.clone()]).is_err());
        let cached = DataSourceRef::cached(source.reference.clone());
        assert!(
            for_local_export(&document, &[cached])
                .unwrap_err()
                .to_string()
                .contains("cached")
        );
        let mut protected = source;
        protected.auth = Some(crate::signals::auth::AuthBinding {
            profile: crate::signals::auth::ProfileRef {
                id: "a".repeat(32),
                revision: "b".repeat(32),
            },
            epoch: "c".repeat(32),
        });
        let error = for_local_export(&document, &[protected])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Protected") && error.contains("in the app") && error.contains("bind"),
            "{error}"
        );
        assert!(!error.contains(&"c".repeat(32)));
        assert_eq!(document, original);
    }
}
