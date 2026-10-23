//! Model vision capabilities. All hooks accept already parsed metadata;
//! fetching belongs to the worker that owns a conversation and its credentials.
use super::provider::{Protocol, ProviderId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, sync::Mutex};

pub const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_CACHE_ENTRIES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionMode {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePlacement {
    Native,
    FollowUpUser,
    ToolMessageParts,
    Sibling,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum VisionSource {
    #[serde(rename = "user override")]
    UserOverride,
    #[serde(rename = "provider models API")]
    ProviderModelsApi,
    #[serde(rename = "built-in table")]
    BuiltInTable,
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "provider rejected images")]
    RuntimeRejection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VisionDecision {
    pub mode: VisionMode,
    pub resolved: Option<bool>,
    pub enabled: bool,
    pub source: VisionSource,
    pub placement: ImagePlacement,
}

/// The endpoint is part of the identity: two local/custom servers can load
/// different capabilities under the same model name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VisionKey {
    pub provider: ProviderId,
    pub model: String,
    pub endpoint: String,
}

impl VisionKey {
    pub fn new(provider: ProviderId, model: &str, endpoint_identity: &str) -> Self {
        Self {
            provider,
            model: model.into(),
            endpoint: endpoint_identity.trim_end_matches('/').into(),
        }
    }
}

#[derive(Clone, Copy)]
struct Entry {
    supported: bool,
    expires_at_ms: u64,
}

#[derive(Default)]
pub struct VisionCache {
    entries: Mutex<HashMap<VisionKey, Entry>>,
}

impl VisionCache {
    pub fn get(&self, key: &VisionKey, now_ms: u64) -> Option<bool> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries
            .get(key)
            .is_some_and(|entry| now_ms >= entry.expires_at_ms)
        {
            entries.remove(key);
        }
        entries.get(key).map(|entry| entry.supported)
    }

    pub fn insert(&self, key: VisionKey, supported: bool, now_ms: u64) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, entry| now_ms < entry.expires_at_ms);
        if entries.len() >= MAX_CACHE_ENTRIES
            && !entries.contains_key(&key)
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at_ms)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            Entry {
                supported,
                expires_at_ms: now_ms.saturating_add(CACHE_TTL_MS),
            },
        );
    }

    /// Remember a negative capability for later conversations. The caller also
    /// pins `runtime_rejected` for this conversation, so an explicit On cannot
    /// cause the same rejected request to loop.
    pub fn mark_rejected(&self, key: &VisionKey, now_ms: u64) {
        self.insert(key.clone(), false, now_ms);
    }

    pub fn resolve(
        &self,
        key: &VisionKey,
        mode: VisionMode,
        metadata: Option<&Value>,
        runtime_rejected: bool,
        now_ms: u64,
    ) -> VisionDecision {
        let live = metadata.and_then(|value| metadata_vision(key.provider, &key.model, value));
        if let Some(supported) = live {
            self.insert(key.clone(), supported, now_ms);
        }
        resolve(
            key.provider,
            &key.model,
            mode,
            live.or_else(|| self.get(key, now_ms)),
            runtime_rejected,
        )
    }
}

pub fn resolve(
    provider: ProviderId,
    model: &str,
    mode: VisionMode,
    live: Option<bool>,
    runtime_rejected: bool,
) -> VisionDecision {
    let (resolved, source) = if provider.is_on_device() {
        (Some(false), VisionSource::BuiltInTable)
    } else if runtime_rejected {
        (Some(false), VisionSource::RuntimeRejection)
    } else {
        match mode {
            VisionMode::On => (Some(true), VisionSource::UserOverride),
            VisionMode::Off => (Some(false), VisionSource::UserOverride),
            VisionMode::Auto => match live {
                Some(supported) => (Some(supported), VisionSource::ProviderModelsApi),
                None => match vision_by_id(provider, model) {
                    Some(supported) => (Some(supported), VisionSource::BuiltInTable),
                    None => (None, VisionSource::Unknown),
                },
            },
        }
    };
    let enabled = resolved == Some(true);
    VisionDecision {
        mode,
        resolved,
        enabled,
        source,
        placement: if enabled {
            image_placement(provider, model)
        } else {
            ImagePlacement::Unsupported
        },
    }
}

pub fn image_placement(provider: ProviderId, model: &str) -> ImagePlacement {
    if provider == ProviderId::Vertex && model.starts_with("claude-") {
        return ImagePlacement::Native;
    }
    match provider.protocol() {
        Protocol::Responses | Protocol::Messages => ImagePlacement::Native,
        Protocol::Generate => ImagePlacement::Sibling,
        Protocol::Converse => {
            if model.contains("anthropic.claude") || model.contains("amazon.nova") {
                ImagePlacement::Native
            } else {
                ImagePlacement::Sibling
            }
        }
        Protocol::Chat => provider
            .info()
            .dialect
            .map_or(ImagePlacement::FollowUpUser, |dialect| {
                dialect.image_placement
            }),
    }
}

/// Live metadata takes precedence over the maintained table. A missing hook or
/// field is unknown; an explicit false or an image-free modalities list is a
/// known negative, and must not fall through to a positive glob.
pub fn metadata_vision(provider: ProviderId, model: &str, metadata: &Value) -> Option<bool> {
    let metadata = model_entry(provider, model, metadata)?;
    match provider {
        ProviderId::OpenRouter | ProviderId::HuggingFace => metadata
            .pointer("/architecture/input_modalities")
            .and_then(Value::as_array)
            .map(|items| items.iter().any(|item| item.as_str() == Some("image"))),
        ProviderId::Anthropic => metadata
            .pointer("/capabilities/image_input/supported")
            .and_then(Value::as_bool),
        ProviderId::Mistral | ProviderId::LmStudio => metadata
            .pointer("/capabilities/vision")
            .and_then(Value::as_bool),
        ProviderId::Ollama => metadata
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|items| items.iter().any(|item| item.as_str() == Some("vision"))),
        _ => None,
    }
}

fn model_entry<'a>(provider: ProviderId, model: &str, metadata: &'a Value) -> Option<&'a Value> {
    let model = if provider == ProviderId::HuggingFace {
        model.split(':').next().unwrap_or(model)
    } else {
        model
    };
    if let Some(models) = metadata
        .get("data")
        .or_else(|| metadata.get("models"))
        .and_then(Value::as_array)
    {
        models.iter().find(|entry| {
            entry
                .get("id")
                .or_else(|| entry.get("key"))
                .and_then(Value::as_str)
                == Some(model)
        })
    } else {
        if let Some(id) = metadata.get("id").and_then(Value::as_str)
            && id != model
        {
            return None;
        }
        Some(metadata)
    }
}

/// The names here are capability hints, not hardware/provider compatibility
/// claims. More specific text-only exceptions precede broad vision families.
pub fn vision_by_id(provider: ProviderId, model: &str) -> Option<bool> {
    let mut model = model.to_ascii_lowercase();
    if matches!(provider, ProviderId::HuggingFace | ProviderId::Ollama) {
        model = model.split(':').next().unwrap_or(&model).to_owned();
    }
    let name = model.rsplit('/').next().unwrap_or(&model);
    let name = if provider == ProviderId::Fireworks {
        name.replace("5p3", "5.3")
            .replace("v4p1", "v4.1")
            .replace("3p8", "3.8")
            .replace("2p4t", "2.4t")
    } else {
        name.to_owned()
    };
    if name.starts_with("gpt-oss")
        || name.starts_with("deepseek-v4-pro")
        || name.starts_with("minimax-m2")
        || name == "qwen3.7-max"
        || name.starts_with("qwen3.8-2.4t")
    {
        return Some(false);
    }
    if name.starts_with("glm-5.3-flash") || name.starts_with("glm-5v") {
        return Some(true);
    }
    if matches!(name.as_str(), "glm-5" | "glm-5.1" | "glm-5.2" | "glm-5.3")
        || name.starts_with("glm-5.3-")
    {
        return Some(false);
    }
    if name.starts_with("gemini-3") || name.starts_with("gemini-4") {
        return Some(
            !["-tts", "-image", "-live"]
                .iter()
                .any(|suffix| name.contains(suffix)),
        );
    }
    if name.starts_with("claude-")
        || name.contains("anthropic.claude-")
        || name.starts_with("gpt-6")
        || name.starts_with("gpt-5.6")
        || name.starts_with("kimi-k3")
        || ["kimi-k2.5", "kimi-k2.6", "kimi-k2.7"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || name == "deepseek-flash"
        || name.starts_with("deepseek-v4.1-flash")
        || [
            "qwen3.5-",
            "qwen3.6-",
            "qwen3.7-",
            "qwen3.8-",
            "qwen-3.8-27b",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || (provider == ProviderId::Ollama && name == "qwen3.8")
        || name.starts_with("mistral-medium-3")
        || matches!(
            name.as_str(),
            "mistral-medium-latest"
                | "mistral-large-2512"
                | "mistral-large-latest"
                | "mistral-small-2603"
                | "mistral-small-latest"
        )
        || (name.starts_with("ministral-") && name.ends_with("-2512"))
        || name.starts_with("llama-4-")
        || name.starts_with("gemma-4")
        || (provider == ProviderId::Ollama && name.starts_with("gemma4"))
        || name.starts_with("muse-spark")
        || name.contains("amazon.nova-")
    {
        return Some(true);
    }
    if name
        .strip_prefix("grok-4.")
        .and_then(|version| version.bytes().next())
        .is_some_and(|digit| matches!(digit, b'5'..=b'9'))
    {
        return Some(true);
    }
    None
}

pub fn is_image_rejection(status: Option<u16>, excerpt: &str, had_images: bool) -> bool {
    had_images && matches!(status, Some(400 | 422)) && {
        let excerpt = excerpt.to_ascii_lowercase();
        [
            "image",
            "vision",
            "multimodal",
            "not supported for this model",
            "content type",
        ]
        .iter()
        .any(|word| excerpt.contains(word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_model_is_text_only_even_with_an_image_override_or_metadata() {
        for mode in [VisionMode::Auto, VisionMode::On, VisionMode::Off] {
            let decision = resolve(
                ProviderId::AppleFoundationModels,
                "on-device",
                mode,
                Some(true),
                false,
            );
            assert!(!decision.enabled);
            assert_eq!(decision.placement, ImagePlacement::Unsupported);
        }
    }
    use serde_json::json;

    #[test]
    fn builtin_table_handles_provider_prefixes_and_specific_exceptions() {
        for (provider, model, expected) in [
            (ProviderId::Anthropic, "claude-sonnet-5-5", true),
            (
                ProviderId::Bedrock,
                "global.anthropic.claude-sonnet-5-5",
                true,
            ),
            (
                ProviderId::Bedrock,
                "arn:aws:bedrock:us-east-1:123456789012:foundation-model/anthropic.claude-sonnet-5-5",
                true,
            ),
            (ProviderId::OpenAi, "gpt-6.1-sol", true),
            (ProviderId::OpenAi, "gpt-5.6-sol", true),
            (ProviderId::Gemini, "gemini-3.8-flash", true),
            (ProviderId::Gemini, "gemini-4-argon", true),
            (ProviderId::Gemini, "gemini-3.8-flash-tts", false),
            (ProviderId::Gemini, "gemini-4-image-preview", false),
            (ProviderId::Gemini, "gemini-3-live", false),
            (ProviderId::Groq, "openai/gpt-oss-120b", false),
            (ProviderId::Together, "zai-org/GLM-5.3", false),
            (ProviderId::Cloudflare, "@cf/zai-org/glm-5.3", false),
            (ProviderId::Cloudflare, "@cf/zai-org/glm-5.3-flash", true),
            (
                ProviderId::Fireworks,
                "accounts/fireworks/models/glm-5p3",
                false,
            ),
            (
                ProviderId::Fireworks,
                "accounts/fireworks/models/glm-5p3-flash",
                true,
            ),
            (
                ProviderId::Fireworks,
                "accounts/fireworks/models/deepseek-v4p1-flash",
                true,
            ),
            (
                ProviderId::Fireworks,
                "accounts/fireworks/models/qwen3p8-2p4t-a95b",
                false,
            ),
            (
                ProviderId::HuggingFace,
                "zai-org/GLM-5.3-Flash:fastest",
                true,
            ),
            (
                ProviderId::HuggingFace,
                "zai-org/GLM-5.3:fireworks-ai",
                false,
            ),
            (ProviderId::DeepSeek, "deepseek-v4-pro", false),
            (ProviderId::DeepSeek, "deepseek-flash", true),
            (ProviderId::OpenRouter, "deepseek/deepseek-v4.1-flash", true),
            (ProviderId::OpenRouter, "qwen/qwen3.7-max", false),
            (ProviderId::OpenRouter, "qwen/qwen3.8-2.4t-a95b", false),
            (ProviderId::Groq, "qwen/qwen3.8-27b", true),
            (ProviderId::Cerebras, "qwen-3.8-27b", true),
            (ProviderId::Ollama, "qwen3.8:latest", true),
            (ProviderId::Ollama, "gpt-oss:120b", false),
            (ProviderId::Ollama, "gemma4:31b", true),
            (ProviderId::Moonshot, "kimi-k2.7-code", true),
            (ProviderId::Xai, "grok-4.7", true),
            (ProviderId::Mistral, "mistral-medium-latest", true),
            (ProviderId::Mistral, "ministral-14b-2512", true),
            (ProviderId::OpenRouter, "minimax/minimax-m2.7", false),
            (ProviderId::Groq, "meta-models/muse-spark-open", true),
        ] {
            assert_eq!(
                vision_by_id(provider, model),
                Some(expected),
                "{provider} {model}"
            );
        }
        assert_eq!(vision_by_id(ProviderId::Custom, "camera-model"), None);
        assert_eq!(vision_by_id(ProviderId::OpenAi, "gpt-4"), None);
    }

    #[test]
    fn metadata_hooks_select_the_model_and_preserve_explicit_negative() {
        let models = json!({"data":[
            {"id":"zai-org/GLM-5.3","architecture":{"input_modalities":["text"]}},
            {"id":"zai-org/GLM-5.3-Flash","architecture":{"input_modalities":["text","image"]}}
        ]});
        assert_eq!(
            metadata_vision(ProviderId::OpenRouter, "zai-org/GLM-5.3", &models),
            Some(false)
        );
        assert_eq!(
            metadata_vision(
                ProviderId::HuggingFace,
                "zai-org/GLM-5.3-Flash:fastest",
                &models
            ),
            Some(true)
        );
        assert_eq!(
            metadata_vision(ProviderId::OpenRouter, "other-model", &models),
            None
        );
        assert_eq!(
            metadata_vision(
                ProviderId::Anthropic,
                "claude-sonnet-5-5",
                &json!({"capabilities":{"image_input":{"supported":false}}})
            ),
            Some(false)
        );
        assert_eq!(
            metadata_vision(
                ProviderId::Mistral,
                "mistral-medium-latest",
                &json!({"data":[{"id":"mistral-medium-latest","capabilities":{"vision":true}}]})
            ),
            Some(true)
        );
        assert_eq!(
            metadata_vision(
                ProviderId::Ollama,
                "qwen3.8",
                &json!({"capabilities":["tools","vision","thinking"]})
            ),
            Some(true)
        );
        assert_eq!(
            metadata_vision(
                ProviderId::Ollama,
                "qwen3.8",
                &json!({"capabilities":["tools"]})
            ),
            Some(false)
        );
        assert_eq!(
            metadata_vision(
                ProviderId::LmStudio,
                "loaded-model",
                &json!({"models":[{"key":"loaded-model","capabilities":{"vision":true}}]})
            ),
            Some(true)
        );
        assert_eq!(
            metadata_vision(
                ProviderId::Anthropic,
                "claude-sonnet-5-5",
                &json!({"id":"different","capabilities":{"image_input":{"supported":true}}})
            ),
            None
        );
        assert_eq!(
            metadata_vision(ProviderId::DeepSeek, "deepseek-flash", &models),
            None
        );
    }

    #[test]
    fn overrides_and_runtime_rejection_have_a_stable_resolution_order() {
        let provider = ProviderId::OpenAi;
        let model = "gpt-6.1-sol";
        assert_eq!(
            resolve(provider, model, VisionMode::Off, Some(true), false).source,
            VisionSource::UserOverride
        );
        assert!(!resolve(provider, model, VisionMode::Off, Some(true), false).enabled);
        assert!(resolve(provider, model, VisionMode::On, Some(false), false).enabled);
        assert!(!resolve(provider, model, VisionMode::Auto, Some(false), false).enabled);
        assert_eq!(
            resolve(provider, model, VisionMode::Auto, Some(false), false).source,
            VisionSource::ProviderModelsApi
        );
        assert_eq!(
            resolve(provider, model, VisionMode::Auto, None, false).source,
            VisionSource::BuiltInTable
        );
        let unknown = resolve(
            ProviderId::Custom,
            "new-model",
            VisionMode::Auto,
            None,
            false,
        );
        assert_eq!(unknown.resolved, None);
        assert!(!unknown.enabled);
        let rejected = resolve(provider, model, VisionMode::On, Some(true), true);
        assert_eq!(rejected.source, VisionSource::RuntimeRejection);
        assert!(!rejected.enabled);
        assert_eq!(rejected.placement, ImagePlacement::Unsupported);
    }

    #[test]
    fn cache_expires_at_twenty_four_hours_and_separates_endpoints() {
        let cache = VisionCache::default();
        let first = VisionKey::new(ProviderId::Custom, "loaded", "http://localhost:1234/v1/");
        let same = VisionKey::new(ProviderId::Custom, "loaded", "http://localhost:1234/v1");
        let other = VisionKey::new(ProviderId::Custom, "loaded", "http://localhost:4321/v1");
        cache.insert(first.clone(), true, 1000);
        assert_eq!(cache.get(&same, 1000 + CACHE_TTL_MS - 1), Some(true));
        assert_eq!(cache.get(&other, 1000), None);
        assert_eq!(cache.get(&first, 1000 + CACHE_TTL_MS), None);
        let key = VisionKey::new(ProviderId::Ollama, "qwen3.8", "http://localhost:11434/v1");
        assert!(
            cache
                .resolve(
                    &key,
                    VisionMode::Auto,
                    Some(&json!({"capabilities":["vision"]})),
                    false,
                    2000
                )
                .enabled
        );
        assert!(
            cache
                .resolve(&key, VisionMode::Auto, None, false, 2001)
                .enabled
        );
        cache.mark_rejected(&key, 2002);
        assert!(
            !cache
                .resolve(&key, VisionMode::On, None, true, 2003)
                .enabled
        );
        assert!(
            !cache
                .resolve(&key, VisionMode::Auto, None, false, 2003)
                .enabled
        );
        // An explicit override in a new conversation remains a deliberate probe.
        assert!(
            cache
                .resolve(&key, VisionMode::On, None, false, 2003)
                .enabled
        );
    }

    #[test]
    fn placement_matches_every_protocol_and_bedrock_family() {
        for provider in [
            ProviderId::OpenAi,
            ProviderId::Azure,
            ProviderId::Perplexity,
            ProviderId::Anthropic,
        ] {
            assert_eq!(
                image_placement(provider, provider.info().default_model),
                ImagePlacement::Native
            );
        }
        assert_eq!(
            image_placement(ProviderId::Gemini, "gemini-3.8-flash"),
            ImagePlacement::Sibling
        );
        assert_eq!(
            image_placement(ProviderId::Vertex, "gemini-4-argon"),
            ImagePlacement::Sibling
        );
        assert_eq!(
            image_placement(ProviderId::Vertex, "claude-sonnet-5-5"),
            ImagePlacement::Native
        );
        assert_eq!(
            image_placement(ProviderId::Bedrock, "global.anthropic.claude-sonnet-5-5"),
            ImagePlacement::Native
        );
        assert_eq!(
            image_placement(ProviderId::Bedrock, "amazon.nova-pro-v1:0"),
            ImagePlacement::Native
        );
        assert_eq!(
            image_placement(ProviderId::Bedrock, "other.model"),
            ImagePlacement::Sibling
        );
        assert_eq!(
            image_placement(ProviderId::Mistral, "mistral-medium-latest"),
            ImagePlacement::ToolMessageParts
        );
        assert_eq!(
            image_placement(ProviderId::Cerebras, "qwen-3.8-27b"),
            ImagePlacement::FollowUpUser
        );
    }

    #[test]
    fn only_an_image_request_with_a_matching_rejection_is_rebuilt() {
        assert!(is_image_rejection(
            Some(400),
            "This model has no VISION support",
            true
        ));
        assert!(is_image_rejection(Some(422), "Unsupported image_url", true));
        assert!(!is_image_rejection(Some(401), "vision", true));
        assert!(!is_image_rejection(Some(400), "vision", false));
        assert!(!is_image_rejection(
            Some(400),
            "Invalid reasoning_effort",
            true
        ));
    }
}
