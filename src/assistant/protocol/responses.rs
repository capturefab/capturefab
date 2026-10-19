//! Stateless Responses requests. Assistant output items are retained once on
//! parse and replayed without rebuilding messages, phases or opaque artifacts.
use super::{BuildCtx, Wire, finish_request, image_data_uri, parse_arguments, result_text};
use crate::assistant::{
    conversation::{CallRecord, Conversation, ModelReply, ResultRecord, Stop, Turn},
    http::{Headers, HttpRequest, ProviderError, ProviderErrorKind},
    provider::{Effort, Protocol, ProviderId},
    usage,
    vision::{ImagePlacement, VisionDecision},
};
use serde_json::{Value, json};
use std::collections::HashSet;

pub struct Responses {
    provider: ProviderId,
    model: String,
}

impl Responses {
    pub fn new(provider: ProviderId, model: &str) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }

    fn malformed(&self, body: &Value, raw: Value) -> ModelReply {
        ModelReply {
            text: String::new(),
            calls: Vec::new(),
            raw,
            stop: Stop::Malformed("The model returned an invalid tool call.".into()),
            usage: usage::responses(body, 0, 0),
        }
    }
}

impl Wire for Responses {
    fn protocol(&self) -> Protocol {
        Protocol::Responses
    }

    fn build<'a>(
        &self,
        ctx: &BuildCtx<'a>,
        conv: &Conversation,
    ) -> Result<HttpRequest<'a>, ProviderError> {
        if crate::assistant::provider::validate_model(&self.model).is_err()
            || conv.provider != self.provider
            || conv.model != self.model
            || self.provider.protocol() != Protocol::Responses
            || ctx.endpoint.protocol != Protocol::Responses
        {
            return Err(ProviderError::new(ProviderErrorKind::Rejected));
        }
        let mut input = Vec::new();
        for turn in &conv.turns {
            match turn {
                Turn::User { wire } | Turn::ToolResults { wire, .. } => {
                    append_wire(&mut input, wire)
                }
                Turn::Assistant { raw, .. } => append_wire(&mut input, raw),
            }
        }
        let tools=conv.tools.iter().map(|tool|json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":true})).collect::<Vec<_>>();
        let mut body = json!({"model":conv.model,"instructions":conv.system,"input":input,"tools":tools,"max_output_tokens":8192});
        if self.provider != ProviderId::Perplexity {
            body["store"] = json!(false);
            body["include"] = json!(["reasoning.encrypted_content"]);
            body["tool_choice"] = json!("auto");
            if !ctx.effort_suppressed {
                body["reasoning"] =
                    json!({"effort":ctx.settings.effort.unwrap_or(Effort::Low).as_str()});
            }
        }
        finish_request(ctx, conv, body)
    }

    fn parse(&self, body: &Value, _headers: &Headers) -> Result<ModelReply, ProviderError> {
        if !body["error"].is_null() {
            return Err(ProviderError::new(ProviderErrorKind::Rejected));
        }
        let output = body
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::Malformed))?;
        let preserve_reasoning = self.provider == ProviderId::Perplexity;
        let raw = Value::Array(
            output
                .iter()
                .filter(|item| {
                    preserve_reasoning
                        || item["type"].as_str() != Some("reasoning")
                        || item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                })
                .cloned()
                .collect(),
        );
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut refusal = None;
        let mut used = HashSet::new();
        let terminal = matches!(
            body["status"].as_str(),
            Some("incomplete" | "failed" | "cancelled")
        );
        for item in output {
            match item["type"].as_str() {
                Some("function_call") if !terminal => {
                    let Some(id) = item["call_id"].as_str().filter(|id| !id.is_empty()) else {
                        return Ok(self.malformed(body, raw));
                    };
                    let Some(name) = item["name"].as_str().filter(|name| valid_name(name)) else {
                        return Ok(self.malformed(body, raw));
                    };
                    let Some(arguments) = item["arguments"].as_str() else {
                        return Ok(self.malformed(body, raw));
                    };
                    if id.len() > 200
                        || id
                            .chars()
                            .any(|character| character.is_whitespace() || character.is_control())
                        || !used.insert(id.to_owned())
                    {
                        return Ok(self.malformed(body, raw));
                    }
                    let args = match parse_arguments(arguments) {
                        Ok(args) => args,
                        Err(_) => return Ok(self.malformed(body, raw)),
                    };
                    calls.push(CallRecord {
                        id: id.into(),
                        name: name.into(),
                        args,
                    });
                }
                Some("message") => {
                    if item
                        .get("role")
                        .is_some_and(|role| role.as_str() != Some("assistant"))
                    {
                        return Ok(self.malformed(body, raw));
                    }
                    let Some(parts) = item["content"].as_array() else {
                        return Ok(self.malformed(body, raw));
                    };
                    for part in parts {
                        match part["type"].as_str() {
                            Some("output_text") => {
                                let Some(content) = part["text"].as_str() else {
                                    return Ok(self.malformed(body, raw));
                                };
                                text.push_str(content);
                            }
                            Some("refusal") => {
                                refusal = Some(
                                    part["refusal"]
                                        .as_str()
                                        .unwrap_or("The model declined the request.")
                                        .to_owned(),
                                )
                            }
                            _ => {}
                        }
                    }
                }
                Some("refusal") => {
                    refusal = Some(
                        item["refusal"]
                            .as_str()
                            .unwrap_or("The model declined the request.")
                            .to_owned(),
                    )
                }
                _ => {}
            }
        }
        let stop = if let Some(refusal) = refusal {
            Stop::Refusal(refusal)
        } else if body["status"].as_str() == Some("incomplete") {
            match body
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
            {
                Some("content_filter") => Stop::Blocked("The provider blocked this reply.".into()),
                _ => Stop::MaxTokens,
            }
        } else if matches!(body["status"].as_str(), Some("failed" | "cancelled")) {
            Stop::Blocked("The provider did not complete this reply.".into())
        } else if !calls.is_empty() {
            Stop::ToolUse
        } else if text.trim().is_empty() {
            Stop::Malformed("The model returned an invalid tool call.".into())
        } else {
            Stop::EndTurn
        };
        if !matches!(stop, Stop::ToolUse) {
            calls.clear();
        }
        let usage = usage::responses(body, 0, text.len());
        Ok(ModelReply {
            text,
            calls,
            raw,
            stop,
            usage,
        })
    }

    fn encode_results(&self, results: &[ResultRecord], vision: &VisionDecision) -> Value {
        Value::Array(results.iter().map(|result| {
            let output=if vision.enabled && vision.placement!=ImagePlacement::Unsupported && !result.images.is_empty() {
                let mut parts=vec![json!({"type":"input_text","text":result_text(result)})];
                for image in &result.images {
                    parts.push(json!({"type":"input_text","text":image.label}));
                    parts.push(json!({"type":"input_image","image_url":image_data_uri(image),"detail":"high"}));
                }
                Value::Array(parts)
            } else {Value::String(result_text(result))};
            json!({"type":"function_call_output","call_id":result.id,"output":output})
        }).collect())
    }

    fn encode_user(&self, text: &str) -> Value {
        json!({"role":"user","content":[{"type":"input_text","text":text}]})
    }
}

fn append_wire(input: &mut Vec<Value>, wire: &Value) {
    if let Some(items) = wire.as_array() {
        input.extend(items.iter().cloned());
    } else {
        input.push(wire.clone());
    }
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{
        conversation::ToolDef,
        credentials::Auth,
        protocol::{ImageBudget, Mode, test_image, test_vision},
        provider::{Endpoint, ProviderSettings},
        vision::{VisionMode, resolve},
    };
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    fn conversation(provider: ProviderId, wire: &Responses) -> Conversation {
        let mut conv = Conversation::new(
            provider,
            provider.info().default_model,
            "A fixed system prompt.",
            vec![ToolDef {
                name: "inspect_frame".into(),
                description: "Inspect a camera.".into(),
                parameters: json!({"type":"object","properties":{"camera":{"type":"string"}},"required":["camera"],"additionalProperties":false}),
                strict: false,
            }],
        );
        conv.append_user("Inspect sim:0.", wire).unwrap();
        conv
    }

    fn build(
        wire: &Responses,
        conv: &Conversation,
        settings: &ProviderSettings,
        mode: Mode,
        suppressed: bool,
    ) -> Value {
        let endpoint = Endpoint {
            url: "https://inference.example.test/v1/responses".into(),
            protocol: Protocol::Responses,
        };
        let auth = Auth::None;
        let ctx = BuildCtx {
            endpoint: &endpoint,
            auth: &auth,
            settings,
            mode,
            effort_suppressed: suppressed,
            image_budget: ImageBudget::default(),
        };
        let request = wire.build(&ctx, conv).unwrap();
        serde_json::from_slice(&request.body).unwrap()
    }

    fn response(arguments: &str) -> Value {
        json!({"status":"completed","output":[
            {"type":"reasoning","id":"rs_without_encryption","summary":[]},
            {"type":"reasoning","id":"rs_kept","encrypted_content":"opaque-encrypted-thinking","summary":[{"type":"summary_text","text":"Checking the frame."}]},
            {"type":"message","id":"msg_1","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Inspecting the camera."}]},
            {"type":"function_call","id":"fc_1","call_id":"call_a","name":"inspect_frame","arguments":arguments,"status":"completed"}
        ],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":25},"output_tokens":20,"output_tokens_details":{"reasoning_tokens":5}}})
    }

    fn result() -> ResultRecord {
        ResultRecord {
            id: "call_a".into(),
            name: "inspect_frame".into(),
            content: json!({"camera":"sim:0","exposure":{"mean":0.5}}),
            is_error: false,
            images: vec![test_image()],
        }
    }

    #[test]
    fn openai_two_round_fixture_preserves_phase_encrypted_reasoning_and_native_images() {
        let provider = ProviderId::OpenAi;
        let wire = Responses::new(provider, provider.info().default_model);
        let mut conv = conversation(provider, &wire);
        let settings = ProviderSettings::default();
        let first = build(&wire, &conv, &settings, Mode::Plan, false);
        assert_eq!(
            first,
            json!({
                "model":"gpt-6.1-sol","instructions":"A fixed system prompt.",
                "input":[{"role":"user","content":[{"type":"input_text","text":"Inspect sim:0."}]}],
                "tools":[{"type":"function","name":"inspect_frame","description":"Inspect a camera.","parameters":{"type":"object","properties":{"camera":{"type":"string"}},"required":["camera"],"additionalProperties":false},"strict":true}],
                "max_output_tokens":8192,"store":false,"include":["reasoning.encrypted_content"],"reasoning":{"effort":"low"},"tool_choice":"auto"
            })
        );
        conv.mark_sent().unwrap();
        let fixture = response("{\"camera\":\"sim:0\"}");
        let reply = wire.parse(&fixture, &Headers::new()).unwrap();
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.text, "Inspecting the camera.");
        assert_eq!(reply.calls[0].args, json!({"camera":"sim:0"}));
        assert_eq!(reply.usage.input, 75);
        assert_eq!(reply.usage.cache_read, 25);
        assert_eq!(reply.usage.reasoning, Some(5));
        let raw = reply.raw.clone();
        assert_eq!(raw.as_array().unwrap().len(), 3);
        assert_eq!(raw[0], fixture["output"][1]);
        assert_eq!(raw[1]["phase"], "commentary");
        conv.push_reply(reply).unwrap();
        let result = result();
        conv.append_results(
            vec![result.clone()],
            &wire,
            &test_vision(provider, &conv.model),
        )
        .unwrap();
        let second = build(&wire, &conv, &settings, Mode::Plan, false);
        assert_eq!(second["input"][0], first["input"][0]);
        for index in 0..3 {
            assert_eq!(second["input"][index + 1], raw[index]);
        }
        let output = &second["input"][4];
        assert_eq!(output["type"], "function_call_output");
        assert_eq!(output["call_id"], "call_a");
        assert_eq!(
            output["output"][0],
            json!({"type":"input_text","text":result_text(&result)})
        );
        assert_eq!(
            output["output"][1],
            json!({"type":"input_text","text":result.images[0].label})
        );
        assert_eq!(output["output"][2]["type"], "input_image");
        assert_eq!(output["output"][2]["detail"], "high");
        let uri = output["output"][2]["image_url"].as_str().unwrap();
        assert_eq!(
            STANDARD
                .decode(uri.strip_prefix("data:image/jpeg;base64,").unwrap())
                .unwrap()
                .as_slice(),
            result.images[0].data.as_ref()
        );
        conv.mark_sent().unwrap();
        let final_reply=wire.parse(&json!({"status":"completed","output":[{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"The frame is balanced."}]}]}),&Headers::new()).unwrap();
        assert_eq!(final_reply.stop, Stop::EndTurn);
        assert_eq!(final_reply.raw[0]["phase"], "final_answer");
        conv.push_reply(final_reply).unwrap();
        conv.check_invariant().unwrap();
    }

    #[test]
    fn azure_uses_the_same_stateless_contract_and_default_effort_is_low_in_runs() {
        for provider in [ProviderId::OpenAi, ProviderId::Azure] {
            let wire = Responses::new(provider, provider.info().default_model);
            let conv = conversation(provider, &wire);
            for mode in [Mode::Plan, Mode::Run] {
                let body = build(&wire, &conv, &ProviderSettings::default(), mode, false);
                assert_eq!(body["store"], false);
                assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
                assert_eq!(body["reasoning"]["effort"], "low");
                assert_eq!(body["max_output_tokens"], 8192);
                assert_eq!(body["tools"][0]["strict"], true);
                for forbidden in [
                    "temperature",
                    "top_p",
                    "top_k",
                    "stop",
                    "seed",
                    "n",
                    "text",
                    "response_format",
                    "max_tokens",
                    "max_completion_tokens",
                    "thinking",
                ] {
                    assert!(body.get(forbidden).is_none(), "{provider} {forbidden}");
                }
            }
            let explicit = ProviderSettings {
                effort: Some(Effort::High),
                ..ProviderSettings::default()
            };
            assert_eq!(
                build(&wire, &conv, &explicit, Mode::Run, false)["reasoning"]["effort"],
                "high"
            );
            assert!(
                build(&wire, &conv, &explicit, Mode::Run, true)
                    .get("reasoning")
                    .is_none()
            );
        }
    }

    #[test]
    fn perplexity_sends_only_its_documented_subset_and_keeps_signatures() {
        let provider = ProviderId::Perplexity;
        let wire = Responses::new(provider, provider.info().default_model);
        let mut conv = conversation(provider, &wire);
        let body = build(
            &wire,
            &conv,
            &ProviderSettings {
                effort: Some(Effort::High),
                ..ProviderSettings::default()
            },
            Mode::Run,
            false,
        );
        let keys = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "model",
                "instructions",
                "input",
                "tools",
                "max_output_tokens"
            ])
        );
        let mut fixture = response("{}");
        fixture["output"][3]["thought_signature"] = json!("perplexity-signature");
        let reply = wire.parse(&fixture, &Headers::new()).unwrap();
        assert_eq!(reply.raw, fixture["output"]);
        conv.push_reply(reply).unwrap();
        conv.append_results(vec![result()], &wire, &test_vision(provider, &conv.model))
            .unwrap();
        let next = build(&wire, &conv, &ProviderSettings::default(), Mode::Run, false);
        assert_eq!(
            next["input"][4]["thought_signature"],
            "perplexity-signature"
        );
        assert_eq!(next["input"][5]["output"][2]["type"], "input_image");
        assert!(
            next["input"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .all(|item| item["role"].as_str() != Some("user"))
        );
    }

    #[test]
    fn image_downgrade_and_errors_produce_compact_json_strings() {
        let provider = ProviderId::OpenAi;
        let wire = Responses::new(provider, "gpt-6.1-sol");
        let result = result();
        let off = resolve(provider, "gpt-6.1-sol", VisionMode::Off, None, false);
        let output = wire.encode_results(std::slice::from_ref(&result), &off);
        assert_eq!(
            output,
            json!([{"type":"function_call_output","call_id":"call_a","output":result_text(&result)}])
        );
        let failed = ResultRecord {
            is_error: true,
            content: json!("The camera is unavailable."),
            images: Vec::new(),
            ..result
        };
        assert_eq!(
            wire.encode_results(&[failed], &off)[0]["output"],
            "{\"error\":\"The camera is unavailable.\"}"
        );
    }

    #[test]
    fn missing_or_duplicate_call_ids_and_malformed_arguments_are_not_synthesized() {
        let wire = Responses::new(ProviderId::OpenAi, "gpt-6.1-sol");
        for args in [
            "{",
            "[]",
            "{\"camera\":1,\"camera\":2}",
            "{\"nested\":{\"value\":1,\"value\":2}}",
        ] {
            assert!(matches!(
                wire.parse(&response(args), &Headers::new()).unwrap().stop,
                Stop::Malformed(_)
            ));
        }
        for field in ["call_id", "name", "arguments"] {
            let mut fixture = response("{}");
            fixture["output"][3].as_object_mut().unwrap().remove(field);
            let reply = wire.parse(&fixture, &Headers::new()).unwrap();
            assert!(matches!(reply.stop, Stop::Malformed(_)));
            assert!(reply.calls.is_empty());
        }
        for name in ["", "bad.name", &"x".repeat(65)] {
            let mut fixture = response("{}");
            fixture["output"][3]["name"] = json!(name);
            assert!(matches!(
                wire.parse(&fixture, &Headers::new()).unwrap().stop,
                Stop::Malformed(_)
            ));
        }
        let mut fixture = response("{}");
        let call = fixture["output"][3].clone();
        fixture["output"].as_array_mut().unwrap().push(call);
        assert!(matches!(
            wire.parse(&fixture, &Headers::new()).unwrap().stop,
            Stop::Malformed(_)
        ));
        for role in ["user", "system", "developer"] {
            let mut fixture = response("{}");
            fixture["output"][2]["role"] = json!(role);
            let reply = wire.parse(&fixture, &Headers::new()).unwrap();
            assert!(matches!(reply.stop, Stop::Malformed(_)));
            assert!(reply.calls.is_empty());
        }
        assert_eq!(
            wire.parse(
                &json!({"status":"completed","output":null}),
                &Headers::new()
            )
            .unwrap_err()
            .kind,
            ProviderErrorKind::Malformed
        );
    }

    #[test]
    fn incomplete_refused_blocked_and_empty_replies_have_distinct_stops() {
        let wire = Responses::new(ProviderId::OpenAi, "gpt-6.1-sol");
        let mut partial = response("{\"partial\":");
        partial["status"] = json!("incomplete");
        partial["incomplete_details"] = json!({"reason":"max_output_tokens"});
        let reply = wire.parse(&partial, &Headers::new()).unwrap();
        assert_eq!(reply.stop, Stop::MaxTokens);
        assert!(reply.calls.is_empty());
        let fixture = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]});
        assert_eq!(
            wire.parse(&fixture, &Headers::new()).unwrap().stop,
            Stop::MaxTokens
        );
        let blocked = json!({"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]});
        assert!(matches!(
            wire.parse(&blocked, &Headers::new()).unwrap().stop,
            Stop::Blocked(_)
        ));
        let refused = json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I cannot help with that."}]}]});
        assert_eq!(
            wire.parse(&refused, &Headers::new()).unwrap().stop,
            Stop::Refusal("I cannot help with that.".into())
        );
        let mut mixed = response("{}");
        mixed["output"]
            .as_array_mut()
            .unwrap()
            .push(refused["output"][0].clone());
        let reply = wire.parse(&mixed, &Headers::new()).unwrap();
        assert!(matches!(reply.stop, Stop::Refusal(_)));
        assert!(reply.calls.is_empty());
        assert!(matches!(wire.parse(&json!({"status":"completed","output":[{"type":"reasoning","encrypted_content":"private"}]}),&Headers::new()).unwrap().stop,Stop::Malformed(_)));
        assert_eq!(
            wire.parse(
                &json!({"error":{"message":"request refused"}}),
                &Headers::new()
            )
            .unwrap_err()
            .kind,
            ProviderErrorKind::Rejected
        );
    }

    #[test]
    fn arguments_do_not_select_endpoints_and_auth_never_enters_the_body() {
        let provider = ProviderId::Azure;
        let wire = Responses::new(provider, provider.info().default_model);
        let mut conv = conversation(provider, &wire);
        let reply=wire.parse(&response("{\"endpoint\":\"https://attacker.example/openai/v1\",\"headers\":{\"authorization\":\"model-value\"}}"),&Headers::new()).unwrap();
        conv.push_reply(reply).unwrap();
        conv.append_results(
            vec![ResultRecord {
                images: Vec::new(),
                ..result()
            }],
            &wire,
            &test_vision(provider, &conv.model),
        )
        .unwrap();
        let endpoint = Endpoint {
            url: "https://factory.openai.azure.com/openai/v1/responses".into(),
            protocol: Protocol::Responses,
        };
        let auth = Auth::Header {
            name: "api-key",
            value: zeroize::Zeroizing::new("offline-header-only-key".into()),
            quota_project: None,
        };
        let settings = ProviderSettings::default();
        let ctx = BuildCtx {
            endpoint: &endpoint,
            auth: &auth,
            settings: &settings,
            mode: Mode::Plan,
            effort_suppressed: false,
            image_budget: ImageBudget::default(),
        };
        let request = wire.build(&ctx, &conv).unwrap();
        assert_eq!(request.url, endpoint.url);
        assert!(!request.url.contains("attacker"));
        assert!(!String::from_utf8_lossy(&request.body).contains("offline-header-only-key"));
    }
}
