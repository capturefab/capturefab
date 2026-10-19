//! Append-only conversations. Wire turns are encoded once; a sent prefix is
//! sealed so accidental edits fail before any subsequent provider request.
use super::{
    http::{ProviderError, ProviderErrorKind},
    provider::{Protocol, ProviderId},
    usage::Usage,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fmt, sync::Arc};

pub const MAX_CALLS: usize = 8;
pub const TOO_MANY_CALLS: &str = "Too many tool calls in one round. Call at most 8 tools at once.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub strict: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Clone)]
pub struct ImagePart {
    pub mime: &'static str,
    pub data: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub label: String,
}
impl fmt::Debug for ImagePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImagePart")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct ResultRecord {
    pub id: String,
    pub name: String,
    pub content: Value,
    pub is_error: bool,
    pub images: Vec<ImagePart>,
}
impl ResultRecord {
    pub fn error(call: &CallRecord, message: &str) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            content: json!({"error":message}),
            is_error: true,
            images: Vec::new(),
        }
    }
    /// Bound and redact before the first encoding. Already-sent results are
    /// never revisited, shortened or re-encoded.
    pub fn bounded(mut self) -> Self {
        self.content = crate::tools::redact::for_model(self.content);
        if serde_json::to_vec(&self.content)
            .map_or(true, |bytes| bytes.len() > crate::tools::MAX_OUTPUT_BYTES)
            || super::protocol::result_text(&self).len() > crate::tools::MAX_OUTPUT_BYTES
        {
            // Keep the runtime's small, typed budget receipt when a large
            // observation is omitted. Arbitrary result fields stay omitted.
            let remaining = self.content.get("limits_remaining").and_then(|value| {
                serde_json::from_value::<super::limits::Remaining>(value.clone()).ok()
            });
            self.content = json!({"truncated":true,"summary":"The tool result exceeded 24 KiB. Ask for a narrower observation."});
            if let Some(remaining) = remaining {
                self.content["limits_remaining"] = json!(remaining);
            }
        }
        for image in &mut self.images {
            image.label = crate::tools::redact::for_model_text(&image.label);
        }
        self
    }
}

#[derive(Clone, Debug)]
pub enum Turn {
    User {
        wire: Value,
    },
    Assistant {
        text: String,
        calls: Vec<CallRecord>,
        raw: Value,
    },
    ToolResults {
        results: Vec<ResultRecord>,
        wire: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal(String),
    Blocked(String),
    Malformed(String),
    PauseTurn,
}

#[derive(Clone, Debug)]
pub struct ModelReply {
    pub text: String,
    pub calls: Vec<CallRecord>,
    pub raw: Value,
    pub stop: Stop,
    pub usage: Usage,
}

#[derive(Clone)]
pub struct Conversation {
    pub provider: ProviderId,
    pub model: String,
    pub system: String,
    pub tools: Vec<ToolDef>,
    pub turns: Vec<Turn>,
    identity: [u8; 32],
    sent: Vec<[u8; 32]>,
    pub effort_suppressed: bool,
    pub vision_rejected: bool,
    /// History assembled with permission to include source values and dataset results.
    pub(crate) data_permission: bool,
}
impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conversation")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("tools", &self.tools.len())
            .field("turns", &self.turns.len())
            .field("sent_turns", &self.sent.len())
            .finish_non_exhaustive()
    }
}

fn hash(value: &Value) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(value).expect("JSON values serialize")).into()
}
fn turn_hash(turn: &Turn) -> [u8; 32] {
    hash(&match turn {
        Turn::User { wire } => json!({"user":wire}),
        Turn::Assistant { text, calls, raw } => json!({"assistant":raw,"text":text,"calls":calls}),
        Turn::ToolResults { results, wire } => {
            json!({"results":results.iter().map(|result|json!({"id":result.id,"name":result.name,"content":result.content,"error":result.is_error,"images":result.images.iter().map(|image|json!({"mime":image.mime,"sha256":format!("{:x}",Sha256::digest(&image.data)),"width":image.width,"height":image.height,"label":image.label})).collect::<Vec<_>>()})).collect::<Vec<_>>(),"wire":wire})
        }
    })
}

impl Conversation {
    pub fn new(
        provider: ProviderId,
        model: impl Into<String>,
        system: impl Into<String>,
        tools: Vec<ToolDef>,
    ) -> Self {
        let model = model.into();
        let system = system.into();
        let identity =
            hash(&json!({"provider":provider,"model":model,"system":system,"tools":tools}));
        Self {
            provider,
            model,
            system,
            tools,
            turns: Vec::new(),
            identity,
            sent: Vec::new(),
            effort_suppressed: false,
            vision_rejected: false,
            data_permission: false,
        }
    }

    fn check_sealed(&self) -> Result<()> {
        ensure!(
            self.identity
                == hash(
                    &json!({"provider":self.provider,"model":self.model,"system":self.system,"tools":self.tools})
                ),
            "Start a new conversation after changing its provider, model, system prompt or tools."
        );
        ensure!(
            self.turns.len() >= self.sent.len()
                && self
                    .turns
                    .iter()
                    .zip(&self.sent)
                    .all(|(turn, sent)| turn_hash(turn) == *sent),
            "Earlier conversation turns changed. Start a new conversation; your plan is kept."
        );
        Ok(())
    }

    pub fn check_invariant(&self) -> Result<()> {
        self.check_sealed()?;
        let gemini = self.protocol() == Protocol::Generate;
        let mut index = 0;
        while index < self.turns.len() {
            match &self.turns[index] {
                Turn::Assistant { calls, .. } if !calls.is_empty() => {
                    let Some(Turn::ToolResults { results, .. }) = self.turns.get(index + 1) else {
                        anyhow::bail!("Answer every tool call before sending another request.");
                    };
                    ensure!(
                        results.len() == calls.len(),
                        "Answer every tool call exactly once."
                    );
                    let mut seen = std::collections::BTreeSet::new();
                    for (call, result) in calls.iter().zip(results) {
                        ensure!(
                            call.id == result.id && call.name == result.name,
                            "Answer tool calls in their original order and with their original IDs and names."
                        );
                        ensure!(
                            (gemini && call.id.is_empty())
                                || (!call.id.is_empty() && seen.insert(&call.id)),
                            "Tool call IDs must be present and unique."
                        );
                    }
                    index += 2;
                    continue;
                }
                Turn::ToolResults { .. } => {
                    anyhow::bail!("Tool results need an immediately preceding assistant tool call.")
                }
                _ => {}
            }
            index += 1;
        }
        Ok(())
    }

    pub fn protocol(&self) -> Protocol {
        super::protocol::protocol_for(self.provider, &self.model)
    }

    /// Seal immediately before dispatch, including requests whose reply may
    /// fail or arrive after cancellation. A retried request uses the same wire.
    pub fn mark_sent(&mut self) -> Result<()> {
        self.check_invariant()?;
        self.sent = self.turns.iter().map(turn_hash).collect();
        Ok(())
    }

    pub fn push_reply(&mut self, reply: ModelReply) -> Result<()> {
        self.check_invariant()?;
        ensure!(
            matches!(reply.stop, Stop::EndTurn | Stop::ToolUse | Stop::PauseTurn),
            "Keep the last complete conversation after a terminal provider reply."
        );
        ensure!(
            matches!(reply.stop, Stop::ToolUse) == !reply.calls.is_empty(),
            "Only a complete tool-use reply may contain tool calls."
        );
        ensure!(
            reply.raw.is_object() || reply.raw.is_array(),
            "Do not append a model reply without a wire message."
        );
        let nonempty = |field: &str| {
            reply.raw[field]
                .as_array()
                .is_some_and(|parts| !parts.is_empty())
        };
        let replayable = match self.protocol() {
            Protocol::Messages | Protocol::Converse => nonempty("content"),
            Protocol::Generate => nonempty("parts"),
            Protocol::Responses => reply.raw.as_array().is_some_and(|items| !items.is_empty()),
            Protocol::Chat => {
                reply.raw["content"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
                    || nonempty("content")
                    || nonempty("tool_calls")
            }
        };
        ensure!(
            replayable,
            "Do not append a model reply without replayable assistant content."
        );
        self.turns.push(Turn::Assistant {
            text: reply.text,
            calls: reply.calls,
            raw: reply.raw,
        });
        Ok(())
    }

    pub fn append_user(&mut self, text: &str, wire: &dyn super::protocol::Wire) -> Result<()> {
        self.check_invariant()?;
        ensure!(
            wire.protocol() == self.protocol(),
            "Use the conversation's original wire protocol."
        );
        ensure!(
            self.protocol() != Protocol::Converse
                || !matches!(self.turns.last(), Some(Turn::User { .. })),
            "Start a new conversation before adding another user message after an unanswered request."
        );
        let user = wire.encode_user(text);
        if matches!(self.protocol(), Protocol::Messages | Protocol::Converse)
            && matches!(self.turns.last(), Some(Turn::ToolResults { .. }))
        {
            ensure!(
                self.turns.len() > self.sent.len(),
                "Start a new conversation before adding a message after sent tool results."
            );
            let Some(Turn::ToolResults { wire: pending, .. }) = self.turns.last_mut() else {
                unreachable!()
            };
            let extra = user["content"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("The user message has no content blocks."))?;
            pending["content"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("The tool message has no content blocks."))?
                .extend(extra.iter().cloned());
        } else {
            self.turns.push(Turn::User { wire: user });
        }
        Ok(())
    }

    pub fn append_results(
        &mut self,
        results: Vec<ResultRecord>,
        wire: &dyn super::protocol::Wire,
        vision: &super::vision::VisionDecision,
    ) -> Result<()> {
        self.check_sealed()?;
        ensure!(
            wire.protocol() == self.protocol(),
            "Use the conversation's original wire protocol."
        );
        let Some(Turn::Assistant { calls, .. }) = self.turns.last() else {
            anyhow::bail!("Tool results need an assistant tool call.");
        };
        ensure!(
            !calls.is_empty() && results.len() == calls.len().min(MAX_CALLS),
            "Return one result for each of the first eight tool calls."
        );
        let mut complete = Vec::with_capacity(calls.len());
        for (index, call) in calls.iter().enumerate() {
            let mut result = if index < MAX_CALLS {
                let result = results[index].clone();
                ensure!(
                    call.id == result.id && call.name == result.name,
                    "Answer tool calls in their original order."
                );
                result.bounded()
            } else {
                ResultRecord::error(call, TOO_MANY_CALLS)
            };
            if !vision.enabled {
                result.images.clear();
            }
            complete.push(result);
        }
        let encoded = wire.encode_results(&complete, vision);
        self.turns.push(Turn::ToolResults {
            results: complete,
            wire: encoded,
        });
        if let Err(error) = self.check_invariant() {
            self.turns.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Cancelled rounds are discarded, preserving the last complete prefix.
    pub fn discard_pending(&mut self) {
        if matches!(self.turns.last(),Some(Turn::Assistant{calls,..}) if !calls.is_empty())
            && self.turns.len() > self.sent.len()
        {
            self.turns.pop();
        }
    }

    pub fn images(&self) -> impl Iterator<Item = &ImagePart> {
        self.turns.iter().flat_map(|turn| match turn {
            Turn::ToolResults { results, .. } => results
                .iter()
                .flat_map(|result| result.images.iter())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
    }
    pub fn request_bytes(
        &self,
        ctx: &super::protocol::BuildCtx<'_>,
        wire: &dyn super::protocol::Wire,
    ) -> Result<usize, ProviderError> {
        Ok(wire.build(ctx, self)?.body.len())
    }
    pub(crate) fn validate_request(&self) -> Result<(), ProviderError> {
        self.check_invariant()
            .map_err(|_| ProviderError::new(ProviderErrorKind::Rejected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{
        protocol::{self, Mode},
        vision,
    };

    fn call(index: usize) -> CallRecord {
        CallRecord {
            id: format!("call_{index}"),
            name: "get_overview".into(),
            args: json!({}),
        }
    }
    fn reply(calls: Vec<CallRecord>) -> ModelReply {
        ModelReply {
            text: String::new(),
            raw: json!({"role":"assistant","content":[{"type":"text","text":"fixture"}],"parts":[{"text":"fixture"}]}),
            calls,
            stop: Stop::ToolUse,
            usage: Usage::default(),
        }
    }
    fn conv(provider: ProviderId) -> Conversation {
        Conversation::new(
            provider,
            provider.info().default_model,
            "A fixed system.",
            protocol::assistant_tools(Mode::Plan),
        )
    }

    #[test]
    fn sent_identity_and_every_turn_are_sealed() {
        let mut c = conv(ProviderId::Anthropic);
        let wire = protocol::wire(c.provider, &c.model);
        c.append_user("first", wire.as_ref()).unwrap();
        c.mark_sent().unwrap();
        let baseline = c.clone();
        for changed in [0, 1, 2, 3, 4] {
            let mut c = baseline.clone();
            match changed {
                0 => c.model.push_str("-new"),
                1 => c.system.push('!'),
                2 => c.tools[0].description.push('!'),
                3 => c.turns.clear(),
                _ => {
                    if let Turn::User { wire } = &mut c.turns[0] {
                        wire["content"][0]["text"] = json!("rewritten");
                    }
                }
            }
            assert!(c.check_invariant().is_err());
        }
        c.append_user("second", wire.as_ref()).unwrap();
        c.check_invariant().unwrap();
        c.mark_sent().unwrap();
        assert!(!format!("{c:?}").contains("A fixed system."));
    }

    #[test]
    fn every_call_gets_one_result_and_overflow_is_never_executed() {
        let mut c = conv(ProviderId::Anthropic);
        let wire = protocol::wire(c.provider, &c.model);
        let vision = vision::resolve(c.provider, &c.model, vision::VisionMode::Off, None, false);
        let calls = (0..10).map(call).collect::<Vec<_>>();
        c.push_reply(reply(calls.clone())).unwrap();
        assert!(c.check_invariant().is_err());
        let results = calls
            .iter()
            .take(8)
            .map(|call| ResultRecord {
                id: call.id.clone(),
                name: call.name.clone(),
                content: json!({"ok":true}),
                is_error: false,
                images: Vec::new(),
            })
            .collect();
        c.append_results(results, wire.as_ref(), &vision).unwrap();
        c.check_invariant().unwrap();
        let Turn::ToolResults { results, .. } = c.turns.last().unwrap() else {
            panic!()
        };
        assert_eq!(results.len(), 10);
        assert!(
            results[8..]
                .iter()
                .all(|result| result.is_error && result.content["error"] == TOO_MANY_CALLS)
        );
        c.mark_sent().unwrap();
    }

    #[test]
    fn pending_results_merge_user_text_once_before_sending() {
        for provider in [ProviderId::Anthropic, ProviderId::Bedrock] {
            let mut c = conv(provider);
            let wire = protocol::wire(c.provider, &c.model);
            let vision = protocol::test_vision(c.provider, &c.model);
            let call = call(1);
            c.push_reply(reply(vec![call.clone()])).unwrap();
            c.append_results(
                vec![ResultRecord::error(&call, "Try another observation.")],
                wire.as_ref(),
                &vision,
            )
            .unwrap();
            let len = c.turns.len();
            c.append_user("A follow-up", wire.as_ref()).unwrap();
            assert_eq!(c.turns.len(), len);
            let Turn::ToolResults { wire: stored, .. } = c.turns.last().unwrap() else {
                panic!()
            };
            assert_eq!(stored["role"], "user");
            assert_eq!(stored["content"].as_array().unwrap().len(), 2);
            c.mark_sent().unwrap();
            let previous = c.turns.len();
            assert!(c.append_user("too late", wire.as_ref()).is_err());
            assert_eq!(c.turns.len(), previous);
            c.check_invariant().unwrap();
        }
    }

    #[test]
    fn invalid_result_order_is_rejected_and_cancel_keeps_complete_prefix() {
        let mut c = conv(ProviderId::Anthropic);
        let wire = protocol::wire(c.provider, &c.model);
        let vision = protocol::test_vision(c.provider, &c.model);
        c.append_user("goal", wire.as_ref()).unwrap();
        c.mark_sent().unwrap();
        c.push_reply(reply(vec![call(1), call(2)])).unwrap();
        let wrong = vec![
            ResultRecord::error(&call(2), "wrong"),
            ResultRecord::error(&call(1), "wrong"),
        ];
        assert!(c.append_results(wrong, wire.as_ref(), &vision).is_err());
        c.discard_pending();
        assert_eq!(c.turns.len(), 1);
        c.check_invariant().unwrap();
    }

    #[test]
    fn gemini_absent_ids_match_by_order_and_name() {
        let mut c = conv(ProviderId::Gemini);
        let wire = protocol::wire(c.provider, &c.model);
        let vision = protocol::test_vision(c.provider, &c.model);
        let calls = vec![
            CallRecord {
                id: String::new(),
                name: "get_overview".into(),
                args: json!({}),
            },
            CallRecord {
                id: String::new(),
                name: "get_storage".into(),
                args: json!({}),
            },
        ];
        let results = calls
            .iter()
            .map(|call| ResultRecord::error(call, "fixture"))
            .collect();
        c.push_reply(reply(calls)).unwrap();
        c.append_results(results, wire.as_ref(), &vision).unwrap();
        c.check_invariant().unwrap();
        let Turn::ToolResults { wire, .. } = c.turns.last().unwrap() else {
            panic!()
        };
        assert!(
            wire["parts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|part| part["functionResponse"].get("id").is_none())
        );
    }

    #[test]
    fn results_are_bounded_redacted_and_images_are_not_in_debug() {
        let mut result = ResultRecord::error(&call(1), "rtsp://alice:planted-password@camera/live");
        result.images.push(protocol::test_image());
        let bounded = result.bounded();
        assert!(!bounded.content.to_string().contains("planted-password"));
        assert!(
            !format!("{:?}", bounded.images).contains(&protocol::image_base64(&bounded.images[0]))
        );
        let mut huge = ResultRecord::error(&call(1), "fixture");
        huge.content = json!({"value":"x".repeat(25*1024)});
        let huge = huge.bounded();
        assert_eq!(huge.content["truncated"], true);
        assert!(serde_json::to_vec(&huge.content).unwrap().len() < crate::tools::MAX_OUTPUT_BYTES);
        let mut escaped = ResultRecord::error(&call(1), "fixture");
        escaped.content = json!({"value":"\"".repeat(10*1024)});
        let escaped = escaped.bounded();
        assert!(protocol::result_text(&escaped).len() <= crate::tools::MAX_OUTPUT_BYTES);
    }

    #[test]
    fn oversized_observations_retain_typed_remaining_limits_without_extra_fields() {
        let limiter = super::super::limits::Limiter::new(
            super::super::limits::Limits::default(),
            100,
            u64::MAX,
            None,
        )
        .unwrap();
        let remaining = json!(limiter.remaining(200));
        let mut result = ResultRecord::error(&call(1), "fixture");
        result.content = json!({"rows":"x".repeat(25*1024),"limits_remaining":remaining});
        let bounded = result.clone().bounded();
        assert_eq!(bounded.content["truncated"], true);
        assert_eq!(bounded.content["limits_remaining"], remaining);
        assert!(bounded.content.get("rows").is_none());
        assert!(protocol::result_text(&bounded).len() <= crate::tools::MAX_OUTPUT_BYTES);

        result.content["limits_remaining"]["extra"] = json!("private-source-text");
        let bounded = result.bounded();
        assert!(bounded.content.get("limits_remaining").is_none());
        assert!(!bounded.content.to_string().contains("private-source-text"));
    }
    #[test]
    fn blocked_prompt_without_assistant_wire_keeps_a_usable_conversation() {
        let mut c = conv(ProviderId::Gemini);
        let wire = protocol::wire(c.provider, &c.model);
        c.append_user("goal", wire.as_ref()).unwrap();
        c.mark_sent().unwrap();
        let blocked = wire
            .parse(
                &json!({"promptFeedback":{"blockReason":"SAFETY"}}),
                &super::super::http::Headers::new(),
            )
            .unwrap();
        assert!(matches!(blocked.stop, Stop::Blocked(_)));
        assert!(c.push_reply(blocked).is_err());
        assert_eq!(c.turns.len(), 1);
        c.check_invariant().unwrap();
        let mut c = conv(ProviderId::Bedrock);
        let wire = protocol::wire(c.provider, &c.model);
        c.append_user("goal", wire.as_ref()).unwrap();
        c.mark_sent().unwrap();
        assert!(
            c.append_user("follow-up after a blocked request", wire.as_ref())
                .is_err()
        );
        c.check_invariant().unwrap();
        for provider in [
            ProviderId::Anthropic,
            ProviderId::Bedrock,
            ProviderId::Gemini,
        ] {
            let mut c = conv(provider);
            let wire = protocol::wire(c.provider, &c.model);
            c.append_user("goal", wire.as_ref()).unwrap();
            c.mark_sent().unwrap();
            let mut empty = reply(Vec::new());
            empty.raw = json!({"role":"assistant","content":[],"parts":[]});
            empty.stop = Stop::MaxTokens;
            assert!(c.push_reply(empty).is_err());
            assert_eq!(c.turns.len(), 1);
            c.check_invariant().unwrap();
        }
    }
}
