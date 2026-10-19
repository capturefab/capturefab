//! Bedrock Converse. Model-specific additions are enabled only for a
//! recognized family, including inference-profile region prefixes.
use super::{
    BuildCtx, MAX_ARGUMENT_BYTES, Wire, error_hint, finish_request, image_base64, result_text,
};
use crate::assistant::{
    conversation::{CallRecord, Conversation, ModelReply, ResultRecord, Stop, Turn},
    http::{Headers, HttpRequest, ProviderError, ProviderErrorKind},
    provider::{Effort, Protocol, ProviderId},
    usage,
    vision::VisionDecision,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct Converse {
    provider: ProviderId,
    model: String,
    next_id: AtomicU64,
}

impl Converse {
    pub fn new(provider: ProviderId, model: &str) -> Self {
        Self {
            provider,
            model: model.into(),
            next_id: AtomicU64::new(1),
        }
    }
}

fn model_family(model: &str) -> &str {
    let model = if model.starts_with("arn:") {
        model.rsplit('/').next().unwrap_or(model)
    } else {
        model
    };
    ["global.", "us.", "eu.", "apac.", "au.", "jp."]
        .iter()
        .find_map(|prefix| model.strip_prefix(prefix))
        .unwrap_or(model)
}

fn supports_status(model: &str) -> bool {
    let model = model_family(model);
    model.starts_with("anthropic.") || model.starts_with("amazon.nova")
}

fn native_images(model: &str) -> bool {
    let model = model_family(model);
    model.starts_with("anthropic.claude") || model.starts_with("amazon.nova")
}

fn image_block(image: &crate::assistant::conversation::ImagePart) -> Value {
    let format = match image.mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpeg",
    };
    json!({"image":{"format":format,"source":{"bytes":image_base64(image)}}})
}

impl Wire for Converse {
    fn protocol(&self) -> Protocol {
        Protocol::Converse
    }

    fn build<'a>(
        &self,
        ctx: &BuildCtx<'a>,
        conv: &Conversation,
    ) -> Result<HttpRequest<'a>, ProviderError> {
        if self.provider != conv.provider
            || self.model != conv.model
            || ctx.endpoint.protocol != Protocol::Converse
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
                json!({"toolSpec":{"name":tool.name,"description":tool.description,
                    "inputSchema":{"json":tool.parameters}}})
            })
            .collect();
        let mut body = json!({
            "system":[{"text":conv.system}],
            "messages":messages,
            "inferenceConfig":{"maxTokens":8192},
            "toolConfig":{"tools":tools},
        });
        if model_family(&self.model).starts_with("anthropic.") {
            body["additionalModelRequestFields"] = json!({"output_config":{
                "effort":ctx.settings.effort.unwrap_or(Effort::Low).as_str(),
            }});
        }
        finish_request(ctx, conv, body)
    }

    fn parse(&self, body: &Value, _headers: &Headers) -> Result<ModelReply, ProviderError> {
        if body.get("error").is_some_and(|error| !error.is_null()) {
            return Err(ProviderError::new(ProviderErrorKind::Rejected));
        }
        let mut raw = body["output"]["message"].clone();
        if raw.is_null() {
            let stop = match body["stopReason"].as_str() {
                Some("guardrail_intervened" | "content_filtered") => {
                    Some(Stop::Blocked("The provider blocked this response.".into()))
                }
                Some("max_tokens" | "model_context_window_exceeded") => Some(Stop::MaxTokens),
                Some("malformed_tool_use" | "malformed_model_output") => Some(Stop::Malformed(
                    "The model returned an invalid tool call.".into(),
                )),
                _ => None,
            };
            if let Some(stop) = stop {
                return Ok(ModelReply {
                    text: String::new(),
                    calls: Vec::new(),
                    raw,
                    stop,
                    usage: usage::converse(body, 0, 0),
                });
            }
        }
        if raw["role"].as_str() != Some("assistant") {
            return Err(ProviderError::new(ProviderErrorKind::Malformed));
        }
        let content = raw["content"]
            .as_array_mut()
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::Malformed))?;
        let mut ids: BTreeSet<String> = content
            .iter()
            .filter_map(|block| block["toolUse"]["toolUseId"].as_str())
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        let mut parsed_ids = BTreeSet::new();
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut malformed = false;
        for block in content {
            if let Some(part) = block["text"].as_str() {
                text.push_str(part);
            }
            if let Some(call) = block.get_mut("toolUse") {
                if !call.is_object() || call.get("toolUseId").is_some_and(|id| !id.is_string()) {
                    malformed = true;
                    continue;
                }
                let id = call["toolUseId"].as_str().filter(|id| !id.is_empty());
                let id = if let Some(id) = id {
                    id.to_owned()
                } else {
                    let id = loop {
                        let next_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                        let candidate = format!("call_{next_id}");
                        if ids.insert(candidate.clone()) {
                            break candidate;
                        }
                    };
                    call["toolUseId"] = json!(id);
                    id
                };
                if let (Some(name), Some(args)) = (
                    call["name"].as_str().filter(|name| {
                        !name.is_empty()
                            && name.len() <= 64
                            && name
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                    }),
                    call.get("input").filter(|args| {
                        args.is_object() && args.to_string().len() <= MAX_ARGUMENT_BYTES
                    }),
                ) && id.len() <= 64
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
                    && parsed_ids.insert(id.clone())
                {
                    calls.push(CallRecord {
                        id,
                        name: name.into(),
                        args: args.clone(),
                    });
                } else {
                    malformed = true;
                }
            }
        }
        let stop = match body["stopReason"].as_str() {
            Some("guardrail_intervened") => {
                Stop::Blocked("The provider's guardrail blocked this response.".into())
            }
            Some("content_filtered") => {
                Stop::Blocked("The provider filtered this response.".into())
            }
            Some("max_tokens" | "model_context_window_exceeded") => Stop::MaxTokens,
            Some("malformed_tool_use" | "malformed_model_output") => {
                Stop::Malformed("The model returned an invalid tool call.".into())
            }
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
            raw,
            stop,
            usage: usage::converse(body, 0, output_bytes),
        })
    }

    fn encode_results(&self, results: &[ResultRecord], vision: &VisionDecision) -> Value {
        let native = native_images(&self.model);
        let mut blocks = Vec::new();
        let mut siblings = Vec::new();
        for result in results {
            let mut content = if vision.enabled && native && !result.images.is_empty() {
                vec![json!({"json":if result.is_error {
                    json!({"error":error_hint(result)})
                } else {
                    result.content.clone()
                }})]
            } else {
                vec![json!({"text":result_text(result)})]
            };
            if vision.enabled {
                for image in &result.images {
                    let target = if native { &mut content } else { &mut siblings };
                    target.push(json!({"text":image.label}));
                    target.push(image_block(image));
                }
            }
            let mut result_block = json!({"toolResult":{
                "toolUseId":result.id,"content":content,
            }});
            if result.is_error && supports_status(&self.model) {
                result_block["toolResult"]["status"] = json!("error");
            }
            blocks.push(result_block);
        }
        blocks.extend(siblings);
        json!({"role":"user","content":blocks})
    }

    fn encode_user(&self, text: &str) -> Value {
        json!({"role":"user","content":[{"text":text}]})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{
        credentials::Auth,
        protocol::{ImageBudget, Mode, assistant_tools, test_image, test_vision},
        provider::{Endpoint, ProviderSettings},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use zeroize::Zeroizing;

    fn context<'a>(
        endpoint: &'a Endpoint,
        auth: &'a Auth,
        settings: &'a ProviderSettings,
    ) -> BuildCtx<'a> {
        BuildCtx {
            endpoint,
            auth,
            settings,
            mode: Mode::Run,
            effort_suppressed: false,
            image_budget: ImageBudget::default(),
        }
    }

    #[test]
    fn converse_two_round_fixture_preserves_reasoning_and_synthesizes_matching_ids() {
        let provider = ProviderId::Bedrock;
        let model = "global.anthropic.claude-sonnet-5-5";
        let wire = Converse::new(provider, model);
        let mut conv = Conversation::new(
            provider,
            model,
            "Frozen system.",
            assistant_tools(Mode::Run),
        );
        conv.append_user("Local time: 2026-10-18 08:00 (UTC-04:00). Inspect.", &wire)
            .unwrap();
        let endpoint=Endpoint {url:"https://bedrock-runtime.us-east-1.amazonaws.com/model/global.anthropic.claude-sonnet-5-5/converse".into(),protocol:Protocol::Converse};
        let secret = "test-bedrock-secret-never-in-body";
        let auth = Auth::Header {
            name: "Authorization",
            value: Zeroizing::new(format!("Bearer {secret}")),
            quota_project: None,
        };
        let settings = ProviderSettings::default();
        let ctx = context(&endpoint, &auth, &settings);
        let first = wire.build(&ctx, &conv).unwrap();
        let first_body: Value = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(first_body["system"], json!([{"text":"Frozen system."}]));
        assert_eq!(first_body["inferenceConfig"], json!({"maxTokens":8192}));
        assert_eq!(
            first_body["additionalModelRequestFields"],
            json!({"output_config":{"effort":"low"}})
        );
        assert!(first_body["toolConfig"].get("toolChoice").is_none());
        assert!(
            first_body["toolConfig"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["toolSpec"].get("strict").is_none())
        );
        for field in [
            "model",
            "temperature",
            "top_p",
            "thinking",
            "response_format",
        ] {
            assert!(first_body.get(field).is_none());
        }
        assert!(!String::from_utf8_lossy(&first.body).contains(secret));
        conv.mark_sent().unwrap();
        let response = json!({"output":{"message":{"role":"assistant","content":[
            {"reasoningContent":{"reasoningText":{"text":"Inspect before changing.","signature":"opaque-reasoning-signature=="}}},
            {"text":"Checking the frame."},
            {"toolUse":{"name":"inspect_frame","input":{"camera":"sim:0","url":"https://model-chosen.invalid"}}},
            {"toolUse":{"toolUseId":"call_1","name":"get_overview","input":{}}},
        ]}},"stopReason":"tool_use","usage":{"inputTokens":100,"outputTokens":20}});
        let reply = wire.parse(&response, &Headers::new()).unwrap();
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.calls[0].id, "call_2");
        assert_eq!(reply.raw["content"][2]["toolUse"]["toolUseId"], "call_2");
        assert_eq!(
            reply.raw["content"][0],
            response["output"]["message"]["content"][0]
        );
        let raw = reply.raw.clone();
        let results = reply
            .calls
            .iter()
            .enumerate()
            .map(|(i, call)| ResultRecord {
                id: call.id.clone(),
                name: call.name.clone(),
                content: json!({"camera":"sim:0","seq":42}),
                is_error: false,
                images: if i == 0 {
                    vec![test_image()]
                } else {
                    Vec::new()
                },
            })
            .collect();
        conv.push_reply(reply).unwrap();
        conv.append_results(results, &wire, &test_vision(provider, model))
            .unwrap();
        conv.append_user("Continue.", &wire).unwrap();
        let second = wire.build(&ctx, &conv).unwrap();
        let second_body: Value = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(second.url, endpoint.url);
        assert_eq!(second_body["toolConfig"], first_body["toolConfig"]);
        assert_eq!(
            serde_json::to_vec(&second_body["messages"][0]).unwrap(),
            serde_json::to_vec(&first_body["messages"][0]).unwrap()
        );
        assert_eq!(second_body["messages"][1], raw);
        let blocks = second_body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["toolResult"]["toolUseId"], "call_2");
        assert_eq!(blocks[1]["toolResult"]["toolUseId"], "call_1");
        assert_eq!(blocks[2], json!({"text":"Continue."}));
        let content = &blocks[0]["toolResult"]["content"];
        assert_eq!(content[0], json!({"json":{"camera":"sim:0","seq":42}}));
        assert_eq!(content[1], json!({"text":test_image().label}));
        assert_eq!(content[2]["image"]["format"], "jpeg");
        assert_eq!(
            STANDARD
                .decode(content[2]["image"]["source"]["bytes"].as_str().unwrap())
                .unwrap()
                .as_slice(),
            test_image().data.as_ref()
        );
        conv.mark_sent().unwrap();
        conv.check_invariant().unwrap();
    }

    #[test]
    fn image_and_error_status_follow_recognized_model_families() {
        for (model, native, status) in [
            ("global.anthropic.claude-sonnet-5-5", true, true),
            ("us.amazon.nova-pro-v1:0", true, true),
            (
                "arn:aws:bedrock:us-east-1:123456789012:foundation-model/eu.anthropic.claude-sonnet-5-5",
                true,
                true,
            ),
            ("apac.meta.llama-4", false, false),
            (
                "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc",
                false,
                false,
            ),
        ] {
            let wire = Converse::new(ProviderId::Bedrock, model);
            let result = |id: &str, images| ResultRecord {
                id: id.into(),
                name: "inspect_frame".into(),
                content: json!({"error":"No fresh frame."}),
                is_error: true,
                images,
            };
            let encoded = wire.encode_results(
                &[result("one", vec![test_image()]), result("two", Vec::new())],
                &test_vision(ProviderId::Bedrock, model),
            );
            let blocks = encoded["content"].as_array().unwrap();
            for block in &blocks[..2] {
                assert!(block.get("toolResult").is_some());
                assert_eq!(
                    block["toolResult"].get("status").is_some(),
                    status,
                    "{model}"
                );
            }
            if native {
                assert_eq!(blocks.len(), 2);
                assert_eq!(
                    blocks[0]["toolResult"]["content"][0],
                    json!({"json":{"error":"No fresh frame."}})
                );
                assert!(blocks[0]["toolResult"]["content"][2].get("image").is_some());
            } else {
                assert_eq!(blocks.len(), 4);
                assert_eq!(blocks[2], json!({"text":test_image().label}));
                assert!(blocks[3].get("image").is_some());
                assert!(blocks[0]["toolResult"]["content"][0].get("text").is_some());
            }
        }
    }

    #[test]
    fn converse_effort_is_low_by_default_and_only_anthropic_gets_the_extension() {
        for (model, effort, expected) in [
            ("global.anthropic.claude-sonnet-5-5", None, Some("low")),
            (
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-5-5",
                Some(Effort::High),
                Some("high"),
            ),
            ("us.amazon.nova-pro-v1:0", Some(Effort::High), None),
        ] {
            let wire = Converse::new(ProviderId::Bedrock, model);
            let mut conv = Conversation::new(
                ProviderId::Bedrock,
                model,
                "Fixed.",
                assistant_tools(Mode::Run),
            );
            conv.append_user("Inspect.", &wire).unwrap();
            let endpoint = Endpoint {
                url: "https://example.test/converse".into(),
                protocol: Protocol::Converse,
            };
            let auth = Auth::None;
            let settings = ProviderSettings {
                effort,
                ..Default::default()
            };
            let request = wire
                .build(&context(&endpoint, &auth, &settings), &conv)
                .unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                body["additionalModelRequestFields"]["output_config"]["effort"].as_str(),
                expected
            );
            assert!(body["inferenceConfig"].get("temperature").is_none());
        }
    }

    #[test]
    fn malformed_blocked_and_cutoff_replies_never_return_executable_calls() {
        let wire = Converse::new(ProviderId::Bedrock, "global.anthropic.claude-sonnet-5-5");
        for reason in [
            "malformed_tool_use",
            "malformed_model_output",
            "guardrail_intervened",
            "content_filtered",
            "max_tokens",
        ] {
            let reply = wire
                .parse(&json!({"stopReason":reason}), &Headers::new())
                .unwrap();
            assert!(reply.calls.is_empty());
            assert!(matches!(
                reply.stop,
                Stop::Malformed(_) | Stop::Blocked(_) | Stop::MaxTokens
            ));
        }
        for content in [
            json!([{"reasoningContent":{"reasoningText":{"text":"hidden","signature":"keep"}}}]),
            json!([{"toolUse":42}]),
            json!([{"toolUse":{"name":"inspect_frame","input":{},"toolUseId":42}}]),
            json!([{"toolUse":{"name":"inspect_frame","input":[],"toolUseId":"a"}}]),
            json!([{"toolUse":{"name":"inspect_frame","input":{},"toolUseId":"a"}},{"toolUse":{"name":"inspect_frame","input":{},"toolUseId":"a"}}]),
        ] {
            let reply=wire.parse(&json!({"output":{"message":{"role":"assistant","content":content}},"stopReason":"end_turn"}),&Headers::new()).unwrap();
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
        let wire = Converse::new(ProviderId::Bedrock, "global.anthropic.claude-sonnet-5-5");
        for (bytes, valid) in [
            (MAX_ARGUMENT_BYTES - 8, true),
            (MAX_ARGUMENT_BYTES - 7, false),
        ] {
            let args = json!({"p":"x".repeat(bytes)});
            let reply = wire.parse(&json!({"output":{"message":{"role":"assistant","content":[{"toolUse":{"toolUseId":"a","name":"inspect_frame","input":args}}]}},"stopReason":"tool_use"}), &Headers::new()).unwrap();
            assert_eq!(matches!(reply.stop, Stop::ToolUse), valid);
            assert_eq!(reply.calls.len(), usize::from(valid));
        }
    }

    #[test]
    fn synthesized_ids_are_distinct_across_rounds_and_written_into_raw() {
        let wire = Converse::new(ProviderId::Bedrock, "global.anthropic.claude-sonnet-5-5");
        let response = json!({"output":{"message":{"role":"assistant","content":[{"toolUse":{"name":"inspect_frame","input":{}}}]}},"stopReason":"tool_use"});
        let first = wire.parse(&response, &Headers::new()).unwrap();
        let second = wire.parse(&response, &Headers::new()).unwrap();
        assert_ne!(first.calls[0].id, second.calls[0].id);
        for reply in [first, second] {
            assert_eq!(
                reply.raw["content"][0]["toolUse"]["toolUseId"],
                reply.calls[0].id
            );
        }
    }
}
