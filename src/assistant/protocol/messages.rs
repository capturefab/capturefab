//! Anthropic Messages, including Vertex's rawPredict body. The stored
//! assistant content includes every thinking block and signature unchanged.
use super::{BuildCtx, MAX_ARGUMENT_BYTES, Wire, finish_request, image_base64, result_text};
use crate::assistant::{
    conversation::{CallRecord, Conversation, ModelReply, ResultRecord, Stop, ToolDef, Turn},
    http::{Headers, HttpRequest, ProviderError, ProviderErrorKind},
    provider::{Protocol, ProviderId},
    usage,
    vision::VisionDecision,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub struct Messages {
    provider: ProviderId,
    model: String,
}

impl Messages {
    pub fn new(provider: ProviderId, model: &str) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

/// Strictness is frozen with the catalog, rather than depending on the
/// history or on which tools the model has used. A tool that does not fit
/// leaves room for later tools with smaller schemas.
pub fn strict_flags(tools: &[ToolDef]) -> Vec<bool> {
    let mut unions = 0;
    let mut strict = 0;
    tools
        .iter()
        .map(|tool| {
            let added = union_properties(&tool.parameters);
            if strict < 20 && unions + added <= 16 {
                unions += added;
                strict += 1;
                true
            } else {
                false
            }
        })
        .collect()
}

fn union_properties(schema: &Value) -> usize {
    match schema {
        Value::Object(object) => {
            let here = usize::from(
                object.get("type").is_some_and(Value::is_array)
                    || object.get("anyOf").is_some_and(Value::is_array),
            );
            here + object.values().map(union_properties).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(union_properties).sum(),
        _ => 0,
    }
}

impl Wire for Messages {
    fn protocol(&self) -> Protocol {
        Protocol::Messages
    }

    fn build<'a>(
        &self,
        ctx: &BuildCtx<'a>,
        conv: &Conversation,
    ) -> Result<HttpRequest<'a>, ProviderError> {
        if self.provider != conv.provider
            || self.model != conv.model
            || ctx.endpoint.protocol != Protocol::Messages
        {
            return Err(ProviderError::new(ProviderErrorKind::Rejected));
        }
        let mut messages = Vec::new();
        for turn in &conv.turns {
            let raw = match turn {
                Turn::User { wire } | Turn::ToolResults { wire, .. } => wire,
                Turn::Assistant { raw, .. } => raw,
            };
            if let Some(parts) = raw.as_array() {
                messages.extend(parts.iter().cloned());
            } else {
                messages.push(raw.clone());
            }
        }
        let tools: Vec<Value> = conv
            .tools
            .iter()
            .map(|tool| {
                let mut wire = json!({
                    "name":tool.name,
                    "description":tool.description,
                    "input_schema":tool.parameters,
                });
                if tool.strict {
                    wire["strict"] = json!(true);
                }
                wire
            })
            .collect();
        let mut body = json!({
            "system":conv.system,
            "messages":messages,
            "max_tokens":8192,
            "tools":tools,
            "tool_choice":{"type":"auto"},
            "output_config":{"effort":ctx.effort().as_str()},
        });
        if self.provider == ProviderId::Vertex {
            body["anthropic_version"] = json!("vertex-2023-10-16");
        } else {
            body["model"] = json!(conv.model);
        }
        if self.provider == ProviderId::Anthropic {
            body["cache_control"] = json!({"type":"ephemeral"});
        }
        let mut request = finish_request(ctx, conv, body)?;
        if self.provider == ProviderId::Anthropic {
            request
                .headers
                .push(("anthropic-version".into(), "2023-06-01".into()));
        }
        Ok(request)
    }

    fn parse(&self, body: &Value, _headers: &Headers) -> Result<ModelReply, ProviderError> {
        if body.get("error").is_some_and(|error| !error.is_null()) {
            return Err(ProviderError::new(ProviderErrorKind::Rejected));
        }
        let empty = Vec::new();
        let content = match body["content"].as_array() {
            Some(content) => content,
            None if matches!(
                body["stop_reason"].as_str(),
                Some("refusal" | "max_tokens" | "model_context_window_exceeded" | "pause_turn")
            ) =>
            {
                &empty
            }
            None => return Err(ProviderError::new(ProviderErrorKind::Malformed)),
        };
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut ids = BTreeSet::new();
        let mut malformed = false;
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(part) = block["text"].as_str() {
                        text.push_str(part);
                    }
                }
                Some("tool_use") => {
                    let id = block["id"].as_str().filter(|id| !id.is_empty());
                    let name = block["name"].as_str().filter(|name| {
                        !name.is_empty()
                            && name.len() <= 128
                            && name
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                    });
                    let args = block.get("input").filter(|args| {
                        args.is_object() && args.to_string().len() <= MAX_ARGUMENT_BYTES
                    });
                    if let (Some(id), Some(name), Some(args)) = (id, name, args)
                        && ids.insert(id.to_owned())
                    {
                        calls.push(CallRecord {
                            id: id.into(),
                            name: name.into(),
                            args: args.clone(),
                        });
                    } else {
                        malformed = true;
                    }
                }
                _ => {}
            }
        }
        let stop = match body["stop_reason"].as_str() {
            Some("refusal") => Stop::Refusal(
                body["stop_details"]["explanation"]
                    .as_str()
                    .or_else(|| body["stop_details"]["category"].as_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or(if text.is_empty() {
                        "The model declined this request."
                    } else {
                        &text
                    })
                    .into(),
            ),
            Some("max_tokens" | "model_context_window_exceeded") => Stop::MaxTokens,
            Some("pause_turn") => Stop::PauseTurn,
            _ if malformed => Stop::Malformed("The model returned an invalid tool call.".into()),
            Some("tool_use") if !calls.is_empty() => Stop::ToolUse,
            Some("end_turn" | "stop_sequence") if !calls.is_empty() => Stop::ToolUse,
            Some("end_turn" | "stop_sequence") if !text.trim().is_empty() => Stop::EndTurn,
            _ => Stop::Malformed("The model returned no usable answer or tool call.".into()),
        };
        let output_bytes = text.len()
            + calls
                .iter()
                .map(|call| call.args.to_string().len())
                .sum::<usize>();
        if !matches!(stop, Stop::ToolUse) {
            calls.clear();
        }
        Ok(ModelReply {
            text,
            calls,
            raw: json!({"role":"assistant","content":content}),
            stop,
            usage: usage::messages(body, 0, output_bytes),
        })
    }

    fn encode_results(&self, results: &[ResultRecord], vision: &VisionDecision) -> Value {
        let blocks: Vec<Value> = results
            .iter()
            .map(|result| {
                let text = result_text(result);
                let content = if vision.enabled && !result.images.is_empty() {
                    let mut blocks = Vec::new();
                    for (index, image) in result.images.iter().enumerate() {
                        blocks.push(json!({"type":"text","text":if index == 0 {
                            format!("{}\n{text}", image.label)
                        } else {
                            image.label.clone()
                        }}));
                        blocks.push(json!({"type":"image","source":{
                            "type":"base64","media_type":image.mime,"data":image_base64(image),
                        }}));
                    }
                    Value::Array(blocks)
                } else {
                    Value::String(text)
                };
                json!({"type":"tool_result","tool_use_id":result.id,
                    "content":content,"is_error":result.is_error})
            })
            .collect();
        json!({"role":"user","content":blocks})
    }

    fn encode_user(&self, text: &str) -> Value {
        json!({"role":"user","content":[{"type":"text","text":text}]})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{
        credentials::Auth,
        protocol::{ImageBudget, Mode, assistant_tools, test_image, test_vision},
        provider::{Effort, Endpoint, ProviderSettings},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use zeroize::Zeroizing;

    fn context<'a>(
        endpoint: &'a Endpoint,
        auth: &'a Auth,
        settings: &'a ProviderSettings,
        mode: Mode,
    ) -> BuildCtx<'a> {
        BuildCtx {
            endpoint,
            auth,
            settings,
            mode,
            effort_suppressed: false,
            image_budget: ImageBudget::default(),
        }
    }

    #[test]
    fn messages_two_round_fixture_preserves_thinking_images_and_sent_prefix() {
        let provider = ProviderId::Anthropic;
        let model = provider.info().default_model;
        let wire = Messages::new(provider, model);
        let mut conv = Conversation::new(
            provider,
            model,
            "Frozen system.",
            assistant_tools(Mode::Plan),
        );
        conv.append_user(
            "Local time: 2026-10-18 08:00 (UTC-04:00). Inspect the camera.",
            &wire,
        )
        .unwrap();
        let endpoint = Endpoint {
            url: "https://api.anthropic.com/v1/messages".into(),
            protocol: Protocol::Messages,
        };
        let planted = "test-secret-in-headers-only";
        let auth = Auth::Header {
            name: "x-api-key",
            value: Zeroizing::new(planted.into()),
            quota_project: None,
        };
        let settings = ProviderSettings::default();
        let ctx = context(&endpoint, &auth, &settings, Mode::Plan);
        let first = wire.build(&ctx, &conv).unwrap();
        let first_body: Value = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(first.url, endpoint.url);
        assert!(!String::from_utf8_lossy(&first.body).contains(planted));
        assert_eq!(first_body["max_tokens"], 8192);
        assert_eq!(first_body["output_config"]["effort"], "low");
        assert_eq!(first_body["tool_choice"], json!({"type":"auto"}));
        assert_eq!(first_body["cache_control"], json!({"type":"ephemeral"}));
        assert!(
            first
                .headers
                .iter()
                .any(|(name, value)| name == "anthropic-version" && value == "2023-06-01")
        );
        for field in [
            "temperature",
            "top_p",
            "top_k",
            "thinking",
            "budget_tokens",
            "response_format",
        ] {
            assert!(first_body.get(field).is_none(), "{field}");
        }
        conv.mark_sent().unwrap();
        let response = json!({
            "content":[
                {"type":"thinking","thinking":"Inspect before adjusting.","signature":"opaque-signature=="},
                {"type":"text","text":"Checking the frame."},
                {"type":"tool_use","id":"toolu_frame","name":"inspect_frame","input":{"camera":"sim:0","url":"https://model-controlled.invalid/"}},
                {"type":"tool_use","id":"toolu_overview","name":"get_overview","input":{}},
            ],
            "stop_reason":"tool_use",
            "usage":{"input_tokens":200,"cache_read_input_tokens":50,"output_tokens":30},
        });
        let reply = wire.parse(&response, &Headers::new()).unwrap();
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.raw["content"], response["content"]);
        assert_eq!(reply.usage.input, 200);
        let results = reply
            .calls
            .iter()
            .enumerate()
            .map(|(index, call)| ResultRecord {
                id: call.id.clone(),
                name: call.name.clone(),
                content: json!({"camera":"sim:0","seq":42}),
                is_error: false,
                images: if index == 0 {
                    vec![test_image()]
                } else {
                    Vec::new()
                },
            })
            .collect();
        conv.push_reply(reply).unwrap();
        conv.append_results(results, &wire, &test_vision(provider, model))
            .unwrap();
        conv.append_user("Continue with these measurements.", &wire)
            .unwrap();
        let second = wire.build(&ctx, &conv).unwrap();
        let second_body: Value = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(second.url, first.url);
        assert_eq!(second_body["system"], first_body["system"]);
        assert_eq!(second_body["tools"], first_body["tools"]);
        assert_eq!(
            serde_json::to_vec(&second_body["messages"][0]).unwrap(),
            serde_json::to_vec(&first_body["messages"][0]).unwrap()
        );
        assert_eq!(second_body["messages"][1]["content"], response["content"]);
        let blocks = second_body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[2]["type"], "text");
        let image = &blocks[0]["content"][1]["source"];
        assert_eq!(image["type"], "base64");
        assert_eq!(image["media_type"], "image/jpeg");
        assert_eq!(
            STANDARD
                .decode(image["data"].as_str().unwrap())
                .unwrap()
                .as_slice(),
            test_image().data.as_ref()
        );
        assert!(
            blocks[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with(&test_image().label)
        );
        conv.mark_sent().unwrap();
        conv.check_invariant().unwrap();
    }

    #[test]
    fn vertex_body_and_effort_defaults_are_platform_specific() {
        for (provider, mode, chosen, expected) in [
            (ProviderId::Anthropic, Mode::Run, None, "medium"),
            (ProviderId::Anthropic, Mode::Run, Some(Effort::High), "high"),
            (ProviderId::Vertex, Mode::Plan, None, "low"),
        ] {
            let model = "claude-sonnet-5-5";
            let wire = Messages::new(provider, model);
            let mut conv = Conversation::new(provider, model, "Fixed.", assistant_tools(mode));
            conv.append_user("Inspect.", &wire).unwrap();
            let endpoint = Endpoint {
                url: "https://example.test/messages".into(),
                protocol: Protocol::Messages,
            };
            let auth = Auth::None;
            let settings = ProviderSettings {
                effort: chosen,
                ..Default::default()
            };
            let request = wire
                .build(&context(&endpoint, &auth, &settings, mode), &conv)
                .unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["output_config"]["effort"], expected);
            if provider == ProviderId::Vertex {
                assert!(body.get("model").is_none());
                assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
                assert!(
                    !request
                        .headers
                        .iter()
                        .any(|(name, _)| name == "anthropic-version")
                );
                assert!(body.get("cache_control").is_none());
            } else {
                assert_eq!(body["model"], model);
                assert!(body.get("anthropic_version").is_none());
            }
        }
    }

    #[test]
    fn strict_budget_is_frozen_against_the_real_plan_and_run_catalogs() {
        for mode in [Mode::Plan, Mode::Run] {
            let tools = assistant_tools(mode);
            let flags = strict_flags(&tools);
            assert_eq!(
                flags,
                tools.iter().map(|tool| tool.strict).collect::<Vec<_>>()
            );
            assert!(flags.iter().filter(|strict| **strict).count() <= 20);
            assert!(
                tools
                    .iter()
                    .zip(&flags)
                    .filter(|(_, strict)| **strict)
                    .map(|(tool, _)| union_properties(&tool.parameters))
                    .sum::<usize>()
                    <= 16
            );
            assert!(flags.iter().any(|strict| !*strict));
            for tool in &tools {
                let properties = tool.parameters["properties"].as_object().unwrap();
                let required = tool.parameters["required"].as_array().unwrap();
                assert!(
                    properties
                        .keys()
                        .all(|key| required.iter().any(|value| value.as_str() == Some(key)))
                );
            }
            let names: Vec<_> = tools.iter().map(|tool| tool.name.as_str()).collect();
            if mode == Mode::Plan {
                assert!(names.contains(&"remove_step"));
            } else {
                assert!(names.contains(&"remember"));
                assert!(names.contains(&"finish_task"));
            }
        }
        let tools = (0..22)
            .map(|index| ToolDef {
                name: format!("tool_{index}"),
                description: String::new(),
                parameters: json!({"type":"object","properties":{},"required":[]}),
                strict: false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            strict_flags(&tools).iter().filter(|flag| **flag).count(),
            20
        );
        let tool = |name: &str, count: usize| ToolDef {
            name: name.into(),
            description: String::new(),
            parameters: json!({"type":"object","properties":(0..count).map(|i|(format!("p{i}"),json!({"type":["string","null"]}))).collect::<serde_json::Map<_,_>>()}),
            strict: false,
        };
        assert_eq!(
            strict_flags(&[tool("full", 16), tool("too_large", 1), tool("fits", 0)]),
            [true, false, true]
        );
    }

    #[test]
    fn refusals_pause_cutoff_and_malformed_calls_are_not_executable() {
        let wire = Messages::new(ProviderId::Anthropic, "claude-sonnet-5-5");
        for (body, expected) in [
            (
                json!({"stop_reason":"refusal","stop_details":{"category":"safety"}}),
                Stop::Refusal("safety".into()),
            ),
            (json!({"stop_reason":"pause_turn"}), Stop::PauseTurn),
            (
                json!({"stop_reason":"max_tokens","content":[{"type":"tool_use","id":"a","name":"inspect_frame","input":{}}]}),
                Stop::MaxTokens,
            ),
        ] {
            let reply = wire.parse(&body, &Headers::new()).unwrap();
            assert_eq!(reply.stop, expected);
            assert!(reply.calls.is_empty());
        }
        for content in [
            json!([{"type":"thinking","thinking":"hidden","signature":"keep"}]),
            json!([{"type":"tool_use","id":"a","name":"inspect_frame","input":[]}]),
            json!([{"type":"tool_use","id":"a","name":"inspect_frame","input":{}},{"type":"tool_use","id":"a","name":"inspect_frame","input":{}}]),
        ] {
            let reply = wire
                .parse(
                    &json!({"content":content,"stop_reason":"end_turn"}),
                    &Headers::new(),
                )
                .unwrap();
            assert!(matches!(reply.stop, Stop::Malformed(_)));
            assert!(reply.calls.is_empty());
        }
        assert_eq!(
            wire.parse(
                &json!({"error":{"message":"provider-specific"}}),
                &Headers::new()
            )
            .unwrap_err()
            .kind,
            ProviderErrorKind::Rejected
        );
    }

    #[test]
    fn native_tool_inputs_obey_the_same_argument_byte_cap_as_string_protocols() {
        let wire = Messages::new(ProviderId::Anthropic, "claude-sonnet-5-5");
        for (bytes, valid) in [
            (MAX_ARGUMENT_BYTES - 8, true),
            (MAX_ARGUMENT_BYTES - 7, false),
        ] {
            let args = json!({"p":"x".repeat(bytes)});
            let reply = wire.parse(&json!({"content":[{"type":"tool_use","id":"a","name":"inspect_frame","input":args}],"stop_reason":"tool_use"}), &Headers::new()).unwrap();
            assert_eq!(matches!(reply.stop, Stop::ToolUse), valid);
            assert_eq!(reply.calls.len(), usize::from(valid));
        }
    }
}
