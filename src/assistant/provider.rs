//! Provider metadata and pure validation. Credentials never appear in an
//! endpoint, and only user settings or their documented environment variables
//! can choose an origin.
use super::vision::{ImagePlacement, VisionMode};
use serde::{Deserialize, Serialize};
use std::{fmt, net::IpAddr, str::FromStr};

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    #[serde(rename = "openai")]
    OpenAi,
    #[default]
    Anthropic,
    Gemini,
    Vertex,
    Bedrock,
    Azure,
    #[serde(rename = "openrouter")]
    OpenRouter,
    Groq,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Cloudflare,
    Mistral,
    Xai,
    Together,
    Fireworks,
    Cerebras,
    #[serde(rename = "huggingface")]
    HuggingFace,
    Moonshot,
    #[serde(rename = "dashscope")]
    DashScope,
    Perplexity,
    Ollama,
    #[serde(rename = "lmstudio")]
    LmStudio,
    Custom,
    AppleFoundationModels,
}

impl ProviderId {
    pub const ALL: [Self; 23] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
        Self::Vertex,
        Self::Bedrock,
        Self::Azure,
        Self::OpenRouter,
        Self::Groq,
        Self::DeepSeek,
        Self::Cloudflare,
        Self::Mistral,
        Self::Xai,
        Self::Together,
        Self::Fireworks,
        Self::Cerebras,
        Self::HuggingFace,
        Self::Moonshot,
        Self::DashScope,
        Self::Perplexity,
        Self::Ollama,
        Self::LmStudio,
        Self::Custom,
        Self::AppleFoundationModels,
    ];

    pub fn id(self) -> &'static str {
        self.info().id
    }

    pub fn info(self) -> &'static ProviderInfo {
        &PROVIDERS[self as usize]
    }

    pub fn protocol(self) -> Protocol {
        self.info().protocol
    }

    /// OS inference runs in a bundled native helper, never an HTTP endpoint.
    pub fn is_on_device(self) -> bool {
        self == Self::AppleFoundationModels
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.info().name)
    }
}

impl FromStr for ProviderId {
    type Err = SettingsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|id| id.id() == value)
            .ok_or_else(|| SettingsError::new("provider", "Choose a provider from the list."))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Chat,
    Responses,
    Messages,
    Generate,
    Converse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    Bearer,
    ApiKey,
    GoogleApiKey,
    GoogleAdc,
    Bedrock,
    OptionalBearer,
    Ollama,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    #[default]
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortField {
    ReasoningEffort,
    Reasoning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Dialect {
    pub token_field: &'static str,
    pub send_tool_choice: bool,
    pub send_parallel_tool_calls: bool,
    pub send_strict: bool,
    /// xAI enforces strict schemas without an explicit flag.
    pub strict_implicit: bool,
    pub effort: Option<[&'static str; 3]>,
    pub effort_field: EffortField,
    pub replay_keys: &'static [&'static str],
    pub name_on_tool_messages: bool,
    pub image_placement: ImagePlacement,
    pub max_images_per_request: usize,
    pub max_image_bytes: usize,
    pub extra_headers: &'static [(&'static str, &'static str)],
    pub openrouter_provider: bool,
}

impl Dialect {
    /// Effort is model-dependent. A non-default model gets it only when the
    /// user explicitly chose a level, and a conversation may suppress it after
    /// a provider rejection.
    pub fn mapped_effort(
        self,
        id: ProviderId,
        model: &str,
        chosen: Option<Effort>,
    ) -> Option<&'static str> {
        if chosen.is_none() && model != id.info().default_model {
            return None;
        }
        self.effort
            .map(|levels| levels[chosen.unwrap_or_default().index()])
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    pub auth: AuthScheme,
    pub key_envs: &'static [&'static str],
    pub settings: &'static [&'static str],
    pub default_model: &'static str,
    pub suggested_models: &'static [&'static str],
    pub suggested_vision_model: Option<&'static str>,
    pub keychain_account: &'static str,
    /// None means live metadata or a model choice is needed.
    pub vision_default: Option<bool>,
    pub dialect: Option<Dialect>,
}

const REPLAY: &[&str] = &["role", "content", "tool_calls"];
const REPLAY_CONTENT: &[&str] = &["role", "content", "tool_calls", "reasoning_content"];
const REPLAY_BOTH: &[&str] = &[
    "role",
    "content",
    "tool_calls",
    "reasoning",
    "reasoning_content",
];
const REPLAY_ROUTER: &[&str] = &[
    "role",
    "content",
    "tool_calls",
    "reasoning",
    "reasoning_details",
];
const CHAT: Dialect = Dialect {
    token_field: "max_tokens",
    send_tool_choice: true,
    send_parallel_tool_calls: false,
    send_strict: false,
    strict_implicit: false,
    effort: None,
    effort_field: EffortField::ReasoningEffort,
    replay_keys: REPLAY,
    name_on_tool_messages: false,
    image_placement: ImagePlacement::FollowUpUser,
    max_images_per_request: 8,
    max_image_bytes: 8 * 1024 * 1024,
    extra_headers: &[],
    openrouter_provider: false,
};
const NORMAL_EFFORT: Option<[&str; 3]> = Some(["low", "medium", "high"]);
const HIGH_EFFORT: Option<[&str; 3]> = Some(["low", "high", "high"]);

pub static PROVIDERS: [ProviderInfo; 23] = [
    ProviderInfo {
        id: "openai",
        name: "OpenAI",
        protocol: Protocol::Responses,
        base_url: "https://api.openai.com/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["OPENAI_API_KEY"],
        settings: &[],
        default_model: "gpt-6.1-sol",
        suggested_models: &["gpt-6.1-sol", "gpt-6-astra", "gpt-6-luna", "gpt-5.6-sol"],
        suggested_vision_model: Some("gpt-6.1-sol"),
        keychain_account: "openai",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "anthropic",
        name: "Anthropic",
        protocol: Protocol::Messages,
        base_url: "https://api.anthropic.com/v1",
        auth: AuthScheme::ApiKey,
        key_envs: &["ANTHROPIC_API_KEY"],
        settings: &[],
        default_model: "claude-sonnet-5-5",
        suggested_models: &["claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-5-5"],
        suggested_vision_model: Some("claude-sonnet-5-5"),
        keychain_account: "claude",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "gemini",
        name: "Google Gemini",
        protocol: Protocol::Generate,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        auth: AuthScheme::GoogleApiKey,
        key_envs: &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        settings: &[],
        default_model: "gemini-3.8-flash",
        suggested_models: &["gemini-3.8-flash", "gemini-4-argon"],
        suggested_vision_model: Some("gemini-3.8-flash"),
        keychain_account: "gemini",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "vertex",
        name: "Google Vertex AI",
        protocol: Protocol::Generate,
        base_url: "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}",
        auth: AuthScheme::GoogleAdc,
        key_envs: &["GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_API_KEY"],
        settings: &["project", "location"],
        default_model: "gemini-3.8-flash",
        suggested_models: &["gemini-3.8-flash", "gemini-4-argon", "claude-sonnet-5-5"],
        suggested_vision_model: Some("gemini-3.8-flash"),
        keychain_account: "vertex",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "bedrock",
        name: "Amazon Bedrock",
        protocol: Protocol::Converse,
        base_url: "https://bedrock-runtime.{region}.amazonaws.com",
        auth: AuthScheme::Bedrock,
        key_envs: &[
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ],
        settings: &["region", "profile"],
        default_model: "global.anthropic.claude-sonnet-5-5",
        suggested_models: &[
            "global.anthropic.claude-sonnet-5-5",
            "global.anthropic.claude-haiku-5-5",
            "amazon.nova-pro-v1:0",
        ],
        suggested_vision_model: Some("global.anthropic.claude-sonnet-5-5"),
        keychain_account: "bedrock",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "azure",
        name: "Azure OpenAI",
        protocol: Protocol::Responses,
        base_url: "{endpoint}/openai/v1",
        auth: AuthScheme::ApiKey,
        key_envs: &["AZURE_OPENAI_API_KEY"],
        settings: &["endpoint"],
        default_model: "gpt-6.1-sol",
        suggested_models: &["gpt-6.1-sol", "gpt-6-luna"],
        suggested_vision_model: Some("gpt-6.1-sol"),
        keychain_account: "azure",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "openrouter",
        name: "OpenRouter",
        protocol: Protocol::Chat,
        base_url: "https://openrouter.ai/api/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["OPENROUTER_API_KEY"],
        settings: &[],
        default_model: "anthropic/claude-sonnet-5.5",
        suggested_models: &[
            "anthropic/claude-sonnet-5.5",
            "openai/gpt-6.1-sol",
            "google/gemini-3.8-flash",
            "qwen/qwen3.8-27b",
        ],
        suggested_vision_model: Some("anthropic/claude-sonnet-5.5"),
        keychain_account: "openrouter",
        vision_default: None,
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            effort: NORMAL_EFFORT,
            effort_field: EffortField::Reasoning,
            replay_keys: REPLAY_ROUTER,
            extra_headers: &[("X-OpenRouter-Title", "Capturefab")],
            openrouter_provider: true,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "groq",
        name: "Groq",
        protocol: Protocol::Chat,
        base_url: "https://api.groq.com/openai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["GROQ_API_KEY"],
        settings: &[],
        default_model: "openai/gpt-oss-120b",
        suggested_models: &[
            "openai/gpt-oss-120b",
            "qwen/qwen3.8-27b",
            "llama-3.3-70b-versatile",
        ],
        suggested_vision_model: Some("qwen/qwen3.8-27b"),
        keychain_account: "groq",
        vision_default: Some(false),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            effort: NORMAL_EFFORT,
            name_on_tool_messages: true,
            max_images_per_request: 3,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "deepseek",
        name: "DeepSeek",
        protocol: Protocol::Chat,
        base_url: "https://api.deepseek.com",
        auth: AuthScheme::Bearer,
        key_envs: &["DEEPSEEK_API_KEY"],
        settings: &[],
        default_model: "deepseek-v4-pro",
        suggested_models: &["deepseek-v4-pro", "deepseek-flash"],
        suggested_vision_model: Some("deepseek-flash"),
        keychain_account: "deepseek",
        vision_default: Some(false),
        dialect: Some(Dialect {
            effort: HIGH_EFFORT,
            replay_keys: REPLAY_CONTENT,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "cloudflare",
        name: "Cloudflare Workers AI",
        protocol: Protocol::Chat,
        base_url: "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["CLOUDFLARE_API_TOKEN"],
        settings: &["account_id", "gateway_id"],
        default_model: "@cf/zai-org/glm-5.3",
        suggested_models: &[
            "@cf/zai-org/glm-5.3",
            "@cf/zai-org/glm-5.3-flash",
            "@cf/qwen/qwen3.8-27b",
        ],
        suggested_vision_model: Some("@cf/zai-org/glm-5.3-flash"),
        keychain_account: "cloudflare",
        vision_default: Some(false),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            effort: HIGH_EFFORT,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "mistral",
        name: "Mistral AI",
        protocol: Protocol::Chat,
        base_url: "https://api.mistral.ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["MISTRAL_API_KEY"],
        settings: &[],
        default_model: "mistral-medium-latest",
        suggested_models: &[
            "mistral-medium-latest",
            "mistral-large-latest",
            "mistral-small-latest",
        ],
        suggested_vision_model: Some("mistral-medium-latest"),
        keychain_account: "mistral",
        vision_default: Some(true),
        dialect: Some(Dialect {
            send_parallel_tool_calls: true,
            effort: NORMAL_EFFORT,
            name_on_tool_messages: true,
            image_placement: ImagePlacement::ToolMessageParts,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "xai",
        name: "xAI",
        protocol: Protocol::Chat,
        base_url: "https://api.x.ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["XAI_API_KEY"],
        settings: &[],
        default_model: "grok-4.7",
        suggested_models: &["grok-4.7", "grok-4.6"],
        suggested_vision_model: Some("grok-4.7"),
        keychain_account: "xai",
        vision_default: Some(true),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            strict_implicit: true,
            effort: NORMAL_EFFORT,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "together",
        name: "Together AI",
        protocol: Protocol::Chat,
        base_url: "https://api.together.ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["TOGETHER_API_KEY"],
        settings: &[],
        default_model: "zai-org/GLM-5.3",
        suggested_models: &[
            "zai-org/GLM-5.3",
            "zai-org/GLM-5.3-Flash",
            "moonshotai/Kimi-K3",
        ],
        suggested_vision_model: Some("zai-org/GLM-5.3-Flash"),
        keychain_account: "together",
        vision_default: Some(false),
        dialect: Some(Dialect {
            send_parallel_tool_calls: true,
            send_strict: true,
            effort: NORMAL_EFFORT,
            replay_keys: REPLAY_BOTH,
            name_on_tool_messages: true,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "fireworks",
        name: "Fireworks AI",
        protocol: Protocol::Chat,
        base_url: "https://api.fireworks.ai/inference/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["FIREWORKS_API_KEY"],
        settings: &[],
        default_model: "accounts/fireworks/models/glm-5p3",
        suggested_models: &[
            "accounts/fireworks/models/glm-5p3",
            "accounts/fireworks/models/glm-5p3-flash",
            "accounts/fireworks/models/kimi-k3",
        ],
        suggested_vision_model: Some("accounts/fireworks/models/glm-5p3-flash"),
        keychain_account: "fireworks",
        vision_default: Some(false),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            send_strict: true,
            effort: NORMAL_EFFORT,
            replay_keys: REPLAY_CONTENT,
            max_images_per_request: 30,
            max_image_bytes: 9_000_000,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "cerebras",
        name: "Cerebras",
        protocol: Protocol::Chat,
        base_url: "https://api.cerebras.ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["CEREBRAS_API_KEY"],
        settings: &[],
        default_model: "gpt-oss-120b",
        suggested_models: &["gpt-oss-120b", "qwen-3.8-27b"],
        suggested_vision_model: Some("qwen-3.8-27b"),
        keychain_account: "cerebras",
        vision_default: Some(false),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            send_strict: true,
            effort: NORMAL_EFFORT,
            max_images_per_request: 2,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "huggingface",
        name: "Hugging Face",
        protocol: Protocol::Chat,
        base_url: "https://router.huggingface.co/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["HF_TOKEN"],
        settings: &[],
        default_model: "zai-org/GLM-5.3",
        suggested_models: &[
            "zai-org/GLM-5.3",
            "zai-org/GLM-5.3-Flash",
            "Qwen/Qwen3.8-27B",
        ],
        suggested_vision_model: Some("zai-org/GLM-5.3-Flash"),
        keychain_account: "huggingface",
        vision_default: None,
        dialect: Some(Dialect {
            name_on_tool_messages: true,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "moonshot",
        name: "Moonshot AI",
        protocol: Protocol::Chat,
        base_url: "https://api.moonshot.ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["MOONSHOT_API_KEY"],
        settings: &["base_url"],
        default_model: "kimi-k3",
        suggested_models: &["kimi-k3", "kimi-k2.6", "kimi-k2.7-code"],
        suggested_vision_model: Some("kimi-k3"),
        keychain_account: "moonshot",
        vision_default: Some(true),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            effort: HIGH_EFFORT,
            replay_keys: REPLAY_CONTENT,
            name_on_tool_messages: true,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "dashscope",
        name: "Alibaba Qwen",
        protocol: Protocol::Chat,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["DASHSCOPE_API_KEY"],
        settings: &["base_url"],
        default_model: "qwen3.8-flash",
        suggested_models: &["qwen3.8-flash", "qwen3.8-max", "qwen3.7-plus"],
        suggested_vision_model: Some("qwen3.8-flash"),
        keychain_account: "dashscope",
        vision_default: Some(true),
        dialect: Some(Dialect {
            token_field: "max_completion_tokens",
            send_parallel_tool_calls: true,
            effort: Some(["low", "medium", "xhigh"]),
            replay_keys: REPLAY_CONTENT,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "perplexity",
        name: "Perplexity",
        protocol: Protocol::Responses,
        base_url: "https://api.perplexity.ai/v1",
        auth: AuthScheme::Bearer,
        key_envs: &["PERPLEXITY_API_KEY"],
        settings: &[],
        default_model: "anthropic/claude-sonnet-5-5",
        suggested_models: &[
            "anthropic/claude-sonnet-5-5",
            "openai/gpt-6.1-sol",
            "google/gemini-3.8-flash",
        ],
        suggested_vision_model: Some("anthropic/claude-sonnet-5-5"),
        keychain_account: "perplexity",
        vision_default: Some(true),
        dialect: None,
    },
    ProviderInfo {
        id: "ollama",
        name: "Ollama",
        protocol: Protocol::Chat,
        base_url: "http://localhost:11434/v1",
        auth: AuthScheme::Ollama,
        key_envs: &["OLLAMA_API_KEY"],
        settings: &["base_url"],
        default_model: "qwen3.8",
        suggested_models: &[
            "qwen3.8",
            "gpt-oss:20b",
            "gemma4:31b",
            "glm-5.3-flash:cloud",
        ],
        suggested_vision_model: Some("qwen3.8"),
        keychain_account: "ollama",
        vision_default: None,
        dialect: Some(Dialect {
            send_tool_choice: false,
            effort: NORMAL_EFFORT,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "lmstudio",
        name: "LM Studio",
        protocol: Protocol::Chat,
        base_url: "http://localhost:1234/v1",
        auth: AuthScheme::OptionalBearer,
        key_envs: &["LM_API_TOKEN"],
        settings: &["base_url"],
        default_model: "",
        suggested_models: &[],
        suggested_vision_model: None,
        keychain_account: "lmstudio",
        vision_default: None,
        dialect: Some(Dialect {
            send_tool_choice: false,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "custom",
        name: "OpenAI-compatible",
        protocol: Protocol::Chat,
        base_url: "",
        auth: AuthScheme::OptionalBearer,
        key_envs: &["CAPTUREFAB_AI_API_KEY"],
        settings: &["base_url"],
        default_model: "",
        suggested_models: &[],
        suggested_vision_model: None,
        keychain_account: "custom",
        vision_default: None,
        dialect: Some(Dialect {
            send_parallel_tool_calls: true,
            ..CHAT
        }),
    },
    ProviderInfo {
        id: "apple_foundation_models",
        name: "Apple Intelligence",
        // Chat is the internal conversation format; no HTTP request is sent.
        protocol: Protocol::Chat,
        base_url: "foundation-models://local",
        auth: AuthScheme::None,
        key_envs: &[],
        settings: &[],
        default_model: "on-device",
        suggested_models: &["on-device"],
        suggested_vision_model: None,
        keychain_account: "apple_foundation_models",
        vision_default: Some(false),
        dialect: Some(Dialect {
            max_images_per_request: 0,
            max_image_bytes: 0,
            ..CHAT
        }),
    },
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub account_id: Option<String>,
    pub gateway_id: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub project: Option<String>,
    pub location: Option<String>,
    pub endpoint: Option<String>,
    pub effort: Option<Effort>,
    pub vision: Option<VisionMode>,
    pub saved_key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SettingsError {
    pub field: String,
    pub message: String,
}

impl SettingsError {
    pub fn new(field: &str, message: &str) -> Self {
        let label = match field {
            "model" => "Model",
            "base_url" => "Base URL",
            "account_id" => "Account ID",
            "gateway_id" => "Gateway ID",
            "region" => "Region",
            "profile" => "Profile",
            "project" => "Project",
            "location" => "Location",
            "endpoint" => "Endpoint",
            _ => "Provider",
        };
        Self {
            field: field.into(),
            message: format!("{label}: {message}"),
        }
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for SettingsError {}

impl ProviderSettings {
    pub fn model(&self, id: ProviderId) -> &str {
        self.model.as_deref().unwrap_or(id.info().default_model)
    }

    /// Validate provided fields without requiring credentials or consulting the
    /// environment. Missing run-time fields are checked by endpoint construction.
    pub fn validate(&self, id: ProviderId) -> Result<(), Vec<SettingsError>> {
        let mut issues = Vec::new();
        if id.is_on_device() {
            if self
                .model
                .as_deref()
                .is_some_and(|model| model != id.info().default_model)
            {
                issues.push(SettingsError::new(
                    "model",
                    "Apple Intelligence uses the on-device system model.",
                ));
            }
            for (field, value) in [
                ("account_id", &self.account_id),
                ("gateway_id", &self.gateway_id),
                ("region", &self.region),
                ("profile", &self.profile),
                ("project", &self.project),
                ("location", &self.location),
            ] {
                if value.is_some() {
                    issues.push(SettingsError::new(
                        field,
                        "Apple Intelligence does not use cloud configuration.",
                    ));
                }
            }
            if self.effort.is_some() {
                issues.push(SettingsError::new(
                    "effort",
                    "Apple Intelligence does not expose reasoning effort.",
                ));
            }
            if self.vision == Some(VisionMode::On) {
                issues.push(SettingsError::new(
                    "vision",
                    "Apple Intelligence supports text only; choose Auto or Off.",
                ));
            }
            if self.saved_key {
                issues.push(SettingsError::new(
                    "saved_key",
                    "Apple Intelligence does not use an API key.",
                ));
            }
        }
        for (field, value) in [
            ("model", &self.model),
            ("base_url", &self.base_url),
            ("account_id", &self.account_id),
            ("gateway_id", &self.gateway_id),
            ("region", &self.region),
            ("profile", &self.profile),
            ("project", &self.project),
            ("location", &self.location),
            ("endpoint", &self.endpoint),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| has_credential_url(value))
            {
                issues.push(SettingsError::new(
                    field,
                    "Remove credentials from the URL. Store API keys in the credential form.",
                ));
            }
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        if let Some(model) = &self.model
            && let Err(issue) = validate_model(model)
        {
            issues.push(issue);
        }
        for (field, value) in [("base_url", &self.base_url), ("endpoint", &self.endpoint)] {
            if let Some(value) = value {
                if !id.info().settings.contains(&field) {
                    issues.push(SettingsError::new(
                        field,
                        "This provider uses its fixed endpoint.",
                    ));
                } else if let Err(issue) = validate_url(id, field, value) {
                    issues.push(issue);
                }
            }
        }
        if self
            .account_id
            .as_ref()
            .is_some_and(|v| v.len() != 32 || !v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            issues.push(SettingsError::new(
                "account_id",
                "Enter the 32 hexadecimal characters of your Cloudflare account ID.",
            ));
        }
        if self.gateway_id.as_ref().is_some_and(|v| {
            !(1..=64).contains(&v.len())
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }) {
            issues.push(SettingsError::new(
                "gateway_id",
                "Use 1 to 64 lowercase letters, digits or hyphens.",
            ));
        }
        if self.region.as_ref().is_some_and(|v| {
            !(2..=32).contains(&v.len())
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }) {
            issues.push(SettingsError::new(
                "region",
                "Use 2 to 32 lowercase letters, digits or hyphens.",
            ));
        }
        if self.profile.as_ref().is_some_and(|v| {
            v.is_empty()
                || v.len() > 128
                || v.chars().any(|c| c.is_control() || matches!(c, '[' | ']'))
        }) {
            issues.push(SettingsError::new(
                "profile",
                "Use a profile name of 1 to 128 characters without brackets or control characters.",
            ));
        }
        if self.project.as_ref().is_some_and(|v| {
            !(6..=30).contains(&v.len())
                || !v.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                || !v.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }) {
            issues.push(SettingsError::new("project","Use a Google Cloud project ID of 6 to 30 lowercase letters, digits or hyphens, starting with a letter and ending with a letter or digit."));
        }
        if self.location.as_ref().is_some_and(|v| !valid_location(v)) {
            issues.push(SettingsError::new(
                "location",
                "Use global, us, eu or a region such as us-central1.",
            ));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    /// Apply only the selected provider's documented configuration variables.
    /// Empty environment values are skipped; a nonempty invalid value is an
    /// error rather than a fallback to another origin.
    pub fn effective(
        &self,
        id: ProviderId,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, Vec<SettingsError>> {
        let mut result = self.clone();
        let pick = |names: &[&str]| {
            names
                .iter()
                .filter_map(|name| env(name))
                .find(|value| !value.is_empty())
        };
        let fill = |field: &mut Option<String>, names: &[&str]| {
            if field.is_none() {
                *field = pick(names);
            }
        };
        match id {
            ProviderId::Cloudflare => fill(&mut result.account_id, &["CLOUDFLARE_ACCOUNT_ID"]),
            ProviderId::Azure => fill(&mut result.endpoint, &["AZURE_OPENAI_ENDPOINT"]),
            ProviderId::Bedrock => {
                fill(&mut result.region, &["AWS_REGION", "AWS_DEFAULT_REGION"]);
                fill(&mut result.profile, &["AWS_PROFILE"]);
            }
            ProviderId::Vertex => {
                fill(&mut result.project, &["GOOGLE_CLOUD_PROJECT"]);
                fill(&mut result.location, &["GOOGLE_CLOUD_LOCATION"]);
                result.location.get_or_insert_with(|| "global".into());
            }
            ProviderId::Ollama if result.base_url.is_none() => {
                if let Some(host) = pick(&["OLLAMA_HOST"]) {
                    result.base_url = Some(ollama_host(&host)?);
                }
            }
            _ => {}
        }
        result.validate(id)?;
        for value in [&mut result.base_url, &mut result.endpoint]
            .into_iter()
            .flatten()
        {
            *value = normalize_url(value);
        }
        if id == ProviderId::Azure
            && let Some(endpoint) = &mut result.endpoint
        {
            for suffix in ["/openai/v1", "/openai"] {
                if let Some(base) = endpoint.strip_suffix(suffix) {
                    *endpoint = base.to_owned();
                    break;
                }
            }
        }
        if let Some(account) = &mut result.account_id {
            account.make_ascii_lowercase();
        }
        Ok(result)
    }

    pub fn connection_notes(&self, id: ProviderId) -> Vec<String> {
        let mut notes = Vec::new();
        let url = self
            .endpoint
            .as_deref()
            .or(self.base_url.as_deref())
            .unwrap_or(id.info().base_url);
        if let Ok(address) = parse_endpoint(url)
            && address.scheme == "http"
        {
            notes.push(format!("Unencrypted connection to {}", address.host));
        }
        if id == ProviderId::Vertex
            && self.model(id).starts_with("claude-")
            && !matches!(
                self.location.as_deref().unwrap_or("global"),
                "global" | "us" | "eu"
            )
        {
            notes.push("Claude on Vertex AI may need the global, us or eu location.".into());
        }
        notes
    }

    pub fn changed_host_note(&self, id: ProviderId, previous: &Self) -> Option<String> {
        if !previous.saved_key {
            return None;
        }
        let host = |settings: &Self| {
            parse_endpoint(
                settings
                    .endpoint
                    .as_deref()
                    .or(settings.base_url.as_deref())
                    .unwrap_or(id.info().base_url),
            )
            .ok()
            .map(|address| address.host)
        };
        let current = host(self)?;
        (Some(&current) != host(previous).as_ref())
            .then(|| format!("Your saved key will be sent to {current}."))
    }
}

pub fn validate_model(model: &str) -> Result<(), SettingsError> {
    if !(1..=200).contains(&model.len())
        || model.starts_with('/')
        || model.contains("..")
        || has_credential_url(model)
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b))
    {
        Err(SettingsError::new(
            "model",
            "Use 1 to 200 letters, digits or . _ : / @ -, without .. or a leading /.",
        ))
    } else {
        Ok(())
    }
}

fn has_credential_url(value: &str) -> bool {
    crate::media::redact_url(value) != value
}

fn valid_location(value: &str) -> bool {
    if matches!(value, "global" | "us" | "eu") {
        return true;
    }
    let Some((first, last)) = value.split_once('-') else {
        return false;
    };
    let letters = last.bytes().take_while(u8::is_ascii_lowercase).count();
    !first.is_empty()
        && first.bytes().all(|b| b.is_ascii_lowercase())
        && letters > 0
        && letters < last.len()
        && last.as_bytes()[letters..].iter().all(u8::is_ascii_digit)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointAddress {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub loopback: bool,
    pub private: bool,
}
impl EndpointAddress {
    pub fn uses_direct(&self) -> bool {
        self.loopback || self.private
    }
}

/// Strict, non-resolving URL parser for saved settings. Never includes input
/// in an error, as it may contain a credential URL pasted by the user.
pub fn parse_endpoint(value: &str) -> Result<EndpointAddress, SettingsError> {
    let invalid = || {
        SettingsError::new(
            "base_url",
            "Enter an HTTP or HTTPS URL without a username, password, query or fragment.",
        )
    };
    if value.is_empty()
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b == b'\\')
        || value.contains(['?', '#'])
    {
        return Err(invalid());
    }
    let uri = value.parse::<ureq::http::Uri>().map_err(|_| invalid())?;
    let scheme = uri.scheme_str().ok_or_else(invalid)?.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return Err(invalid());
    }
    let authority = uri.authority().ok_or_else(invalid)?;
    if authority.as_str().contains('@') {
        return Err(invalid());
    }
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host.is_empty() || host.contains('%') {
        return Err(invalid());
    }
    let port = authority.port_u16();
    let port_text = if authority.as_str().starts_with('[') {
        authority
            .as_str()
            .split_once(']')
            .and_then(|(_, tail)| tail.strip_prefix(':'))
    } else {
        authority.as_str().split_once(':').map(|(_, port)| port)
    };
    if port == Some(0)
        || port_text.is_some_and(|text| {
            text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) || port.is_none()
        })
    {
        return Err(invalid());
    }
    let ip = host.parse::<IpAddr>().ok();
    let loopback = host == "localhost" || ip.is_some_and(|ip| ip.is_loopback());
    let private = match ip {
        Some(IpAddr::V4(ip)) => {
            let [a, b, _, _] = ip.octets();
            a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
        }
        Some(IpAddr::V6(ip)) => (ip.segments()[0] & 0xfe00) == 0xfc00,
        None => false,
    };
    if uri.path().split('/').any(|part| matches!(part, "." | "..")) {
        return Err(invalid());
    }
    Ok(EndpointAddress {
        scheme,
        host,
        port,
        loopback,
        private,
    })
}

fn validate_url(id: ProviderId, field: &str, value: &str) -> Result<(), SettingsError> {
    let address = parse_endpoint(value).map_err(|issue| {
        SettingsError::new(
            field,
            issue
                .message
                .split_once(": ")
                .map_or(issue.message.as_str(), |(_, message)| message),
        )
    })?;
    let local = matches!(
        id,
        ProviderId::Custom | ProviderId::Ollama | ProviderId::LmStudio
    );
    if address.scheme != "https" && !(local && address.uses_direct()) {
        return Err(SettingsError::new(
            field,
            "Use HTTPS, or HTTP to a loopback or private address for a local provider.",
        ));
    }
    let allowed = match id {
        ProviderId::Moonshot => {
            matches!(address.host.as_str(), "api.moonshot.ai" | "api.moonshot.cn")
        }
        ProviderId::DashScope => address
            .host
            .strip_suffix(".aliyuncs.com")
            .is_some_and(|prefix| {
                prefix == "dashscope"
                    || prefix.strip_prefix("dashscope-").is_some_and(|region| {
                        !region.is_empty()
                            && region
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                    })
            }),
        ProviderId::Azure => [
            ".openai.azure.com",
            ".cognitiveservices.azure.com",
            ".services.ai.azure.com",
        ]
        .iter()
        .any(|suffix| {
            address
                .host
                .strip_suffix(suffix)
                .is_some_and(|prefix| !prefix.is_empty())
        }),
        _ => true,
    };
    if !allowed {
        return Err(SettingsError::new(
            field,
            "Choose an official host for this provider.",
        ));
    }
    if id == ProviderId::Azure {
        let uri = value
            .parse::<ureq::http::Uri>()
            .expect("URL was parsed above");
        if !matches!(
            uri.path().trim_end_matches('/'),
            "" | "/openai" | "/openai/v1"
        ) {
            return Err(SettingsError::new(
                field,
                "Use the endpoint origin, optionally ending in /openai or /openai/v1.",
            ));
        }
    }
    Ok(())
}

fn normalize_url(value: &str) -> String {
    let Ok(uri) = value.parse::<ureq::http::Uri>() else {
        return value.into();
    };
    format!(
        "{}://{}{}",
        uri.scheme_str().unwrap_or("https").to_ascii_lowercase(),
        uri.authority()
            .map_or("", |value| value.as_str())
            .to_ascii_lowercase(),
        uri.path().trim_end_matches('/')
    )
}

fn ollama_host(value: &str) -> Result<String, Vec<SettingsError>> {
    let explicit_scheme = value.contains("://");
    let value = if explicit_scheme {
        value.to_owned()
    } else if value == "::" {
        "http://[::]".into()
    } else {
        format!("http://{value}")
    };
    let uri = value.parse::<ureq::http::Uri>().map_err(|_| {
        vec![SettingsError::new(
            "base_url",
            "Enter OLLAMA_HOST as [scheme://]host[:port].",
        )]
    })?;
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() || value.contains('#') {
        return Err(vec![SettingsError::new(
            "base_url",
            "Enter OLLAMA_HOST as [scheme://]host[:port], without a path, query or fragment.",
        )]);
    }
    let address = parse_endpoint(&value).map_err(|issue| vec![issue])?;
    let host = match address.host.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        host => host,
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.into()
    };
    let port = address
        .port
        .or((!explicit_scheme).then_some(11434))
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!("{}://{host}{port}/v1", address.scheme))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub protocol: Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointSet {
    pub primary: Option<Endpoint>,
    pub express: Option<Endpoint>,
}

impl EndpointSet {
    pub fn select(&self, vertex_express: bool) -> Result<&Endpoint, SettingsError> {
        if vertex_express {
            self.express.as_ref().ok_or_else(||SettingsError::new("model","Claude on Vertex AI needs Google Cloud credentials (ADC). API keys work only for Gemini models."))
        } else {
            self.primary.as_ref().ok_or_else(|| {
                SettingsError::new("project", "Enter your Google Cloud project ID to use ADC.")
            })
        }
    }
}

/// Endpoints are credential-free. Vertex route selection happens only after
/// credentials decide between ADC and an express key.
pub fn endpoints(
    id: ProviderId,
    settings: &ProviderSettings,
    model: &str,
) -> Result<EndpointSet, Vec<SettingsError>> {
    settings.validate(id)?;
    validate_model(model).map_err(|issue| vec![issue])?;
    if id.is_on_device() && model != id.info().default_model {
        return Err(vec![SettingsError::new(
            "model",
            "Apple Intelligence uses the on-device system model.",
        )]);
    }
    let make = |url: String, protocol| Endpoint { url, protocol };
    if id == ProviderId::Vertex {
        let location = settings.location.as_deref().unwrap_or("global");
        let host = match location {
            "global" => "aiplatform.googleapis.com".into(),
            "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
            _ => format!("{location}-aiplatform.googleapis.com"),
        };
        let claude = model.starts_with("claude-");
        let (publisher, operation, protocol) = if claude {
            ("anthropic", "rawPredict", Protocol::Messages)
        } else {
            ("google", "generateContent", Protocol::Generate)
        };
        let model = vertex_encode(model);
        let primary=settings.project.as_deref().map(|project|make(format!("https://{host}/v1/projects/{project}/locations/{location}/publishers/{publisher}/models/{model}:{operation}"),protocol));
        if claude && primary.is_none() {
            return Err(vec![SettingsError::new(
                "project",
                "Enter your Google Cloud project ID to use Claude with ADC.",
            )]);
        }
        let express=(!claude).then(||make(format!("https://aiplatform.googleapis.com/v1/publishers/google/models/{model}:generateContent"),Protocol::Generate));
        return Ok(EndpointSet { primary, express });
    }
    let url = match id {
        ProviderId::AppleFoundationModels => {
            format!("{}/chat/completions", id.info().base_url)
        }
        ProviderId::Bedrock => {
            let region = settings.region.as_deref().ok_or_else(|| {
                vec![SettingsError::new(
                    "region",
                    "Choose an AWS region in settings, the environment or your AWS profile.",
                )]
            })?;
            format!(
                "https://bedrock-runtime.{region}.amazonaws.com/model/{}/converse",
                crate::aws::uri_encode(model, true)
            )
        }
        ProviderId::Azure => {
            let endpoint = settings.endpoint.as_deref().ok_or_else(|| {
                vec![SettingsError::new(
                    "endpoint",
                    "Enter your Azure OpenAI endpoint.",
                )]
            })?;
            let base = normalize_url(endpoint);
            let base = base
                .strip_suffix("/openai/v1")
                .or_else(|| base.strip_suffix("/openai"))
                .unwrap_or(&base);
            format!("{base}/openai/v1/responses")
        }
        ProviderId::Cloudflare => {
            let account = settings.account_id.as_deref().ok_or_else(|| {
                vec![SettingsError::new(
                    "account_id",
                    "Enter your Cloudflare account ID.",
                )]
            })?;
            format!(
                "https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1/chat/completions"
            )
        }
        _ => {
            let base = settings
                .base_url
                .as_deref()
                .unwrap_or(id.info().base_url)
                .trim_end_matches('/');
            if base.is_empty() {
                return Err(vec![SettingsError::new(
                    "base_url",
                    "Enter the provider's base URL.",
                )]);
            }
            match id.protocol() {
                Protocol::Responses => format!("{base}/responses"),
                Protocol::Messages => format!("{base}/messages"),
                Protocol::Generate => format!(
                    "{base}/models/{}:generateContent",
                    crate::aws::uri_encode(model, true)
                ),
                Protocol::Chat => format!("{base}/chat/completions"),
                Protocol::Converse => unreachable!("Bedrock handled above"),
            }
        }
    };
    Ok(EndpointSet {
        primary: Some(make(url, id.protocol())),
        express: None,
    })
}

fn vertex_encode(value: &str) -> String {
    // `@` in dated Vertex Claude names is expressly allowed by the API.
    crate::aws::uri_encode(value, true).replace("%40", "@")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn native_provider_has_a_fixed_local_model_and_rejects_remote_configuration() {
        let id = ProviderId::AppleFoundationModels;
        assert!(id.is_on_device());
        assert!(id.info().key_envs.is_empty());
        let endpoint = endpoints(id, &ProviderSettings::default(), "on-device").unwrap();
        assert_eq!(
            endpoint.select(false).unwrap().url,
            "foundation-models://local/chat/completions"
        );
        assert!(endpoints(id, &ProviderSettings::default(), "remote-model").is_err());
        for settings in [
            ProviderSettings {
                model: Some("cloud-model".into()),
                ..ProviderSettings::default()
            },
            ProviderSettings {
                base_url: Some("https://api.example/v1".into()),
                ..ProviderSettings::default()
            },
            ProviderSettings {
                endpoint: Some("https://api.example/v1".into()),
                ..ProviderSettings::default()
            },
            ProviderSettings {
                saved_key: true,
                ..ProviderSettings::default()
            },
            ProviderSettings {
                vision: Some(VisionMode::On),
                ..ProviderSettings::default()
            },
            ProviderSettings {
                effort: Some(Effort::High),
                ..ProviderSettings::default()
            },
        ] {
            assert!(settings.validate(id).is_err(), "{settings:?}");
        }
    }

    fn settings_url(field: &str, url: &str) -> ProviderSettings {
        let mut settings = ProviderSettings::default();
        match field {
            "endpoint" => settings.endpoint = Some(url.into()),
            _ => settings.base_url = Some(url.into()),
        }
        settings
    }

    #[test]
    fn catalog_ids_defaults_auth_and_environment_are_stable() {
        let expected = [
            (
                "openai",
                "gpt-6.1-sol",
                Protocol::Responses,
                &["OPENAI_API_KEY"][..],
            ),
            (
                "anthropic",
                "claude-sonnet-5-5",
                Protocol::Messages,
                &["ANTHROPIC_API_KEY"][..],
            ),
            (
                "gemini",
                "gemini-3.8-flash",
                Protocol::Generate,
                &["GOOGLE_API_KEY", "GEMINI_API_KEY"][..],
            ),
            (
                "vertex",
                "gemini-3.8-flash",
                Protocol::Generate,
                &["GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_API_KEY"][..],
            ),
            (
                "bedrock",
                "global.anthropic.claude-sonnet-5-5",
                Protocol::Converse,
                &[
                    "AWS_BEARER_TOKEN_BEDROCK",
                    "AWS_ACCESS_KEY_ID",
                    "AWS_SECRET_ACCESS_KEY",
                    "AWS_SESSION_TOKEN",
                ][..],
            ),
            (
                "azure",
                "gpt-6.1-sol",
                Protocol::Responses,
                &["AZURE_OPENAI_API_KEY"][..],
            ),
            (
                "openrouter",
                "anthropic/claude-sonnet-5.5",
                Protocol::Chat,
                &["OPENROUTER_API_KEY"][..],
            ),
            (
                "groq",
                "openai/gpt-oss-120b",
                Protocol::Chat,
                &["GROQ_API_KEY"][..],
            ),
            (
                "deepseek",
                "deepseek-v4-pro",
                Protocol::Chat,
                &["DEEPSEEK_API_KEY"][..],
            ),
            (
                "cloudflare",
                "@cf/zai-org/glm-5.3",
                Protocol::Chat,
                &["CLOUDFLARE_API_TOKEN"][..],
            ),
            (
                "mistral",
                "mistral-medium-latest",
                Protocol::Chat,
                &["MISTRAL_API_KEY"][..],
            ),
            ("xai", "grok-4.7", Protocol::Chat, &["XAI_API_KEY"][..]),
            (
                "together",
                "zai-org/GLM-5.3",
                Protocol::Chat,
                &["TOGETHER_API_KEY"][..],
            ),
            (
                "fireworks",
                "accounts/fireworks/models/glm-5p3",
                Protocol::Chat,
                &["FIREWORKS_API_KEY"][..],
            ),
            (
                "cerebras",
                "gpt-oss-120b",
                Protocol::Chat,
                &["CEREBRAS_API_KEY"][..],
            ),
            (
                "huggingface",
                "zai-org/GLM-5.3",
                Protocol::Chat,
                &["HF_TOKEN"][..],
            ),
            (
                "moonshot",
                "kimi-k3",
                Protocol::Chat,
                &["MOONSHOT_API_KEY"][..],
            ),
            (
                "dashscope",
                "qwen3.8-flash",
                Protocol::Chat,
                &["DASHSCOPE_API_KEY"][..],
            ),
            (
                "perplexity",
                "anthropic/claude-sonnet-5-5",
                Protocol::Responses,
                &["PERPLEXITY_API_KEY"][..],
            ),
            ("ollama", "qwen3.8", Protocol::Chat, &["OLLAMA_API_KEY"][..]),
            ("lmstudio", "", Protocol::Chat, &["LM_API_TOKEN"][..]),
            ("custom", "", Protocol::Chat, &["CAPTUREFAB_AI_API_KEY"][..]),
            (
                "apple_foundation_models",
                "on-device",
                Protocol::Chat,
                &[][..],
            ),
        ];
        let mut seen = BTreeSet::new();
        for (provider, (id, model, protocol, envs)) in ProviderId::ALL.into_iter().zip(expected) {
            let info = provider.info();
            assert!(seen.insert(provider));
            assert_eq!(provider.id(), id);
            assert_eq!(serde_json::to_value(provider).unwrap(), id);
            assert_eq!(
                serde_json::from_value::<ProviderId>(serde_json::json!(id)).unwrap(),
                provider
            );
            assert_eq!(id.parse::<ProviderId>().unwrap(), provider);
            assert_eq!(info.default_model, model);
            assert_eq!(info.protocol, protocol);
            assert_eq!(info.key_envs, envs);
            assert_eq!(provider.to_string(), info.name);
            assert_eq!(info.dialect.is_some(), protocol == Protocol::Chat);
            if !model.is_empty() {
                validate_model(model).unwrap();
                assert!(info.suggested_models.contains(&model));
            }
            for suggestion in info.suggested_models {
                validate_model(suggestion).unwrap();
            }
        }
        assert_eq!(seen.len(), ProviderId::ALL.len());
        assert_eq!(ProviderId::Anthropic.info().keychain_account, "claude");
        assert_eq!(ProviderId::OpenAi.info().keychain_account, "openai");
        assert_eq!(ProviderId::Gemini.info().keychain_account, "gemini");
        assert!("OpenAI".parse::<ProviderId>().is_err());
    }

    #[test]
    fn chat_dialects_use_only_the_pinned_fields_and_replay_allowlists() {
        for provider in ProviderId::ALL {
            let Some(dialect) = provider.info().dialect else {
                continue;
            };
            let completion = matches!(
                provider,
                ProviderId::OpenRouter
                    | ProviderId::Groq
                    | ProviderId::Xai
                    | ProviderId::Cloudflare
                    | ProviderId::Cerebras
                    | ProviderId::Fireworks
                    | ProviderId::Moonshot
                    | ProviderId::DashScope
            );
            assert_eq!(
                dialect.token_field,
                if completion {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                },
                "{provider}"
            );
            assert_eq!(
                dialect.send_tool_choice,
                !matches!(provider, ProviderId::Ollama | ProviderId::LmStudio),
                "{provider}"
            );
            assert_eq!(
                dialect.send_strict,
                matches!(
                    provider,
                    ProviderId::Cerebras | ProviderId::Together | ProviderId::Fireworks
                ),
                "{provider}"
            );
            assert_eq!(dialect.strict_implicit, provider == ProviderId::Xai);
            assert_eq!(&dialect.replay_keys[..3], REPLAY);
            let extra = match provider {
                ProviderId::DeepSeek
                | ProviderId::Moonshot
                | ProviderId::DashScope
                | ProviderId::Fireworks => &["reasoning_content"][..],
                ProviderId::Together => &["reasoning", "reasoning_content"][..],
                ProviderId::OpenRouter => &["reasoning", "reasoning_details"][..],
                _ => &[],
            };
            assert_eq!(&dialect.replay_keys[3..], extra, "{provider}");
            assert!(!dialect.replay_keys.contains(&"refusal"));
        }
        let router = ProviderId::OpenRouter.info().dialect.unwrap();
        assert_eq!(
            router.extra_headers,
            &[("X-OpenRouter-Title", "Capturefab")]
        );
        assert!(router.openrouter_provider);
        assert_eq!(router.effort_field, EffortField::Reasoning);
        assert_eq!(
            ProviderId::Groq
                .info()
                .dialect
                .unwrap()
                .max_images_per_request,
            3
        );
        assert_eq!(
            ProviderId::Cerebras
                .info()
                .dialect
                .unwrap()
                .max_images_per_request,
            2
        );
        assert_eq!(
            ProviderId::Fireworks
                .info()
                .dialect
                .unwrap()
                .max_images_per_request,
            30
        );
        assert_eq!(
            ProviderId::Fireworks
                .info()
                .dialect
                .unwrap()
                .max_image_bytes,
            9_000_000
        );
    }

    #[test]
    fn effort_mapping_avoids_unrequested_nondefault_model_parameters() {
        for provider in [
            ProviderId::DeepSeek,
            ProviderId::Moonshot,
            ProviderId::Cloudflare,
        ] {
            let dialect = provider.info().dialect.unwrap();
            assert_eq!(
                dialect.mapped_effort(provider, provider.info().default_model, None),
                Some("low")
            );
            assert_eq!(dialect.mapped_effort(provider, "another-model", None), None);
            assert_eq!(
                dialect.mapped_effort(provider, "another-model", Some(Effort::Medium)),
                Some("high")
            );
        }
        let dashscope = ProviderId::DashScope.info().dialect.unwrap();
        assert_eq!(
            dashscope.mapped_effort(ProviderId::DashScope, "qwen3.8-flash", Some(Effort::High)),
            Some("xhigh")
        );
        for provider in [
            ProviderId::HuggingFace,
            ProviderId::LmStudio,
            ProviderId::Custom,
        ] {
            assert_eq!(
                provider.info().dialect.unwrap().mapped_effort(
                    provider,
                    "model",
                    Some(Effort::High)
                ),
                None
            );
        }
    }

    #[test]
    fn model_validation_is_bounded_and_prevents_path_injection() {
        for good in [
            "x",
            "org/model:version@date",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc",
        ] {
            validate_model(good).unwrap();
        }
        validate_model(&"a".repeat(200)).unwrap();
        for bad in [
            "",
            "/model",
            "a..b",
            "model?key=secret",
            "model#fragment",
            "model%2Fother",
            "model\\path",
            "model name",
            "model\nnext",
            "模型",
        ] {
            let issue = validate_model(bad).unwrap_err();
            assert_eq!(issue.field, "model");
            assert!(issue.message.starts_with("Model: "));
            assert!(!issue.message.contains("key=secret"));
        }
        assert!(validate_model(&"a".repeat(201)).is_err());
    }

    #[test]
    fn urls_refuse_credentials_and_only_permit_private_plain_http_for_local_providers() {
        for url in [
            "https://localhost/v1",
            "http://localhost:8080/v1",
            "http://127.0.0.2:80/v1",
            "http://[::1]:8080/v1",
            "http://10.1.2.3/v1",
            "http://172.16.1.2/v1",
            "http://172.31.255.254/v1",
            "http://192.168.1.2/v1",
            "http://[fc00::1]/v1",
            "http://[fdff::1]/v1",
        ] {
            for provider in [ProviderId::Custom, ProviderId::Ollama, ProviderId::LmStudio] {
                settings_url("base_url", url).validate(provider).unwrap();
            }
        }
        for url in [
            "http://8.8.8.8/v1",
            "http://172.15.0.1/v1",
            "http://172.32.0.1/v1",
            "http://192.0.0.1/v1",
            "http://169.254.169.254/v1",
            "http://[fe80::1]/v1",
            "http://[::ffff:127.0.0.1]/v1",
            "http://plant.local/v1",
            "http://localhost.evil/v1",
            "ftp://localhost/v1",
            "https://user:DO-NOT-LEAK@example.com/v1",
            "https://user@example.com/v1",
            "https://example.com/v1?key=DO-NOT-LEAK",
            "https://example.com/v1#DO-NOT-LEAK",
            "https://example.com:0/v1",
            "https://example.com:99999/v1",
            "https://example.com:abc/v1",
            "https://example.com:/v1",
            "https://example.com/a/../v1",
            "https://example.com/a\\v1",
        ] {
            let issues = settings_url("base_url", url)
                .validate(ProviderId::Custom)
                .unwrap_err();
            assert!(
                issues
                    .iter()
                    .all(|issue| issue.field == "base_url"
                        && issue.message.starts_with("Base URL: "))
            );
            assert!(!format!("{issues:?}").contains("DO-NOT-LEAK"));
        }
        assert!(
            settings_url("base_url", "https://elsewhere.example/v1")
                .validate(ProviderId::OpenAi)
                .is_err()
        );
        assert!(
            parse_endpoint("https://[::1]:8443/v1")
                .unwrap()
                .uses_direct()
        );
        assert!(parse_endpoint("https://10.0.0.1/v1").unwrap().uses_direct());
        assert!(
            !parse_endpoint("https://example.com/v1")
                .unwrap()
                .uses_direct()
        );
    }

    #[test]
    fn pinned_hosts_apply_to_settings_and_environment_values() {
        for host in ["api.moonshot.ai", "api.moonshot.cn"] {
            settings_url("base_url", &format!("https://{host}/v1"))
                .validate(ProviderId::Moonshot)
                .unwrap();
        }
        for host in [
            "dashscope.aliyuncs.com",
            "dashscope-intl.aliyuncs.com",
            "dashscope-us1.aliyuncs.com",
        ] {
            settings_url("base_url", &format!("https://{host}/compatible-mode/v1"))
                .validate(ProviderId::DashScope)
                .unwrap();
        }
        for path in ["", "/", "/openai", "/openai/", "/openai/v1", "/openai/v1/"] {
            let settings = settings_url(
                "endpoint",
                &format!("https://factory.openai.azure.com{path}"),
            );
            settings.validate(ProviderId::Azure).unwrap();
            assert_eq!(
                endpoints(ProviderId::Azure, &settings, "deployment")
                    .unwrap()
                    .select(false)
                    .unwrap()
                    .url,
                "https://factory.openai.azure.com/openai/v1/responses"
            );
        }
        for path in [
            "/evil",
            "/v1",
            "/evil/openai/v1",
            "/openai/v1/responses",
            "/openai/%76%31",
        ] {
            let issues = settings_url(
                "endpoint",
                &format!("https://factory.openai.azure.com{path}"),
            )
            .validate(ProviderId::Azure)
            .unwrap_err();
            assert_eq!(issues[0].field, "endpoint");
        }
        for host in [
            "project.openai.azure.com",
            "project.cognitiveservices.azure.com",
            "project.services.ai.azure.com",
        ] {
            settings_url("endpoint", &format!("https://{host}/openai/v1/"))
                .validate(ProviderId::Azure)
                .unwrap();
        }
        for (provider, field, host) in [
            (
                ProviderId::Moonshot,
                "base_url",
                "api.moonshot.ai.evil.example",
            ),
            (ProviderId::Moonshot, "base_url", "evil.api.moonshot.ai"),
            (ProviderId::DashScope, "base_url", "dashscope-.aliyuncs.com"),
            (
                ProviderId::DashScope,
                "base_url",
                "dashscope-intl.aliyuncs.com.evil.example",
            ),
            (
                ProviderId::DashScope,
                "base_url",
                "dashscope-intl-us.aliyuncs.com",
            ),
            (ProviderId::Azure, "endpoint", "openai.azure.com"),
            (
                ProviderId::Azure,
                "endpoint",
                "project.openai.azure.com.evil.example",
            ),
        ] {
            assert!(
                settings_url(field, &format!("https://{host}/v1"))
                    .validate(provider)
                    .is_err(),
                "{provider} {host}"
            );
        }
        let settings = ProviderSettings::default();
        let issues = settings
            .effective(ProviderId::Azure, |name| {
                (name == "AZURE_OPENAI_ENDPOINT").then(|| "https://collector.example".into())
            })
            .unwrap_err();
        assert_eq!(issues[0].field, "endpoint");
        assert!(
            settings
                .effective(ProviderId::Bedrock, |name| (name == "AWS_REGION")
                    .then(|| "us-east-1/evil".into()))
                .is_err()
        );
        assert!(
            settings
                .effective(ProviderId::Vertex, |name| (name == "GOOGLE_CLOUD_LOCATION")
                    .then(|| "../../escape".into()))
                .is_err()
        );
    }

    #[test]
    fn effective_settings_use_selected_provider_env_precedence_and_canonical_urls() {
        let environment = BTreeMap::from([
            ("AWS_REGION", ""),
            ("AWS_DEFAULT_REGION", "eu-west-1"),
            ("AWS_PROFILE", "factory"),
            (
                "AZURE_OPENAI_ENDPOINT",
                "https://Factory.OPENAI.AZURE.COM/openai/v1/",
            ),
            ("CLOUDFLARE_ACCOUNT_ID", "ABCDEF0123456789ABCDEF0123456789"),
            ("GOOGLE_CLOUD_PROJECT", "factory-project"),
            ("GOOGLE_CLOUD_LOCATION", "us-central1"),
        ]);
        let env = |name: &str| environment.get(name).map(|value| value.to_string());
        let defaults = ProviderSettings::default();
        let bedrock = defaults.effective(ProviderId::Bedrock, env).unwrap();
        assert_eq!(bedrock.region.as_deref(), Some("eu-west-1"));
        assert_eq!(bedrock.profile.as_deref(), Some("factory"));
        let explicit = ProviderSettings {
            region: Some("us-west-2".into()),
            ..defaults.clone()
        }
        .effective(ProviderId::Bedrock, env)
        .unwrap();
        assert_eq!(explicit.region.as_deref(), Some("us-west-2"));
        assert_eq!(
            defaults
                .effective(ProviderId::Azure, env)
                .unwrap()
                .endpoint
                .as_deref(),
            Some("https://factory.openai.azure.com")
        );
        assert_eq!(
            defaults
                .effective(ProviderId::Cloudflare, env)
                .unwrap()
                .account_id
                .as_deref(),
            Some("abcdef0123456789abcdef0123456789")
        );
        let vertex = defaults.effective(ProviderId::Vertex, env).unwrap();
        assert_eq!(vertex.project.as_deref(), Some("factory-project"));
        assert_eq!(vertex.location.as_deref(), Some("us-central1"));
        assert_eq!(
            defaults
                .effective(ProviderId::Vertex, |_| None)
                .unwrap()
                .location
                .as_deref(),
            Some("global")
        );
        assert_eq!(
            defaults
                .effective(ProviderId::OpenAi, |_| panic!(
                    "fixed-provider settings must not read unrelated environment"
                ))
                .unwrap(),
            defaults
        );
    }

    #[test]
    fn ollama_host_parses_wildcards_ipv6_schemes_and_private_hosts() {
        for (host, expected) in [
            ("0.0.0.0:11434", "http://127.0.0.1:11434/v1"),
            ("::", "http://[::1]:11434/v1"),
            ("[::]:11434", "http://[::1]:11434/v1"),
            ("localhost", "http://localhost:11434/v1"),
            ("http://192.168.1.42:11434", "http://192.168.1.42:11434/v1"),
            ("https://ollama.com", "https://ollama.com/v1"),
        ] {
            let settings = ProviderSettings::default()
                .effective(ProviderId::Ollama, |name| {
                    (name == "OLLAMA_HOST").then(|| host.into())
                })
                .unwrap();
            assert_eq!(settings.base_url.as_deref(), Some(expected));
        }
        for host in [
            "http://example.com",
            "localhost:11434/v1",
            "http://user:DO-NOT-LEAK@localhost:11434",
            "https://ollama.com?key=DO-NOT-LEAK",
        ] {
            let issues = ProviderSettings::default()
                .effective(ProviderId::Ollama, |_| Some(host.into()))
                .unwrap_err();
            assert!(!format!("{issues:?}").contains("DO-NOT-LEAK"));
        }
    }

    #[test]
    fn every_identifier_error_is_mapped_to_its_field() {
        let settings = ProviderSettings {
            model: Some("bad model".into()),
            account_id: Some("x".into()),
            gateway_id: Some("UPPER".into()),
            region: Some("x".into()),
            profile: Some("bad\nprofile".into()),
            project: Some("5bad".into()),
            location: Some("us-central1-a".into()),
            ..ProviderSettings::default()
        };
        let issues = settings.validate(ProviderId::Vertex).unwrap_err();
        assert_eq!(
            issues
                .iter()
                .map(|issue| issue.field.as_str())
                .collect::<Vec<_>>(),
            [
                "model",
                "account_id",
                "gateway_id",
                "region",
                "profile",
                "project",
                "location"
            ]
        );
        for issue in issues {
            assert!(issue.message.contains(": "));
        }
        for project in ["abcdef", "factory-project-1", &"a".repeat(30)] {
            ProviderSettings {
                project: Some(project.into()),
                ..ProviderSettings::default()
            }
            .validate(ProviderId::Vertex)
            .unwrap();
        }
        for location in ["global", "us", "eu", "us-central1", "europe-west12"] {
            ProviderSettings {
                location: Some(location.into()),
                ..ProviderSettings::default()
            }
            .validate(ProviderId::Vertex)
            .unwrap();
        }
    }

    #[test]
    fn endpoints_encode_model_paths_and_handle_every_catalog_provider() {
        for provider in ProviderId::ALL {
            let mut settings = ProviderSettings::default();
            match provider {
                ProviderId::Vertex => settings.project = Some("factory-project".into()),
                ProviderId::Bedrock => settings.region = Some("us-east-1".into()),
                ProviderId::Azure => {
                    settings.endpoint = Some("https://factory.openai.azure.com/openai/v1/".into())
                }
                ProviderId::Cloudflare => {
                    settings.account_id = Some("0123456789abcdef0123456789abcdef".into())
                }
                ProviderId::Custom => {
                    settings.base_url = Some("https://inference.example/v1".into())
                }
                _ => {}
            }
            let model = if provider.info().default_model.is_empty() {
                "loaded-model"
            } else {
                provider.info().default_model
            };
            let set = endpoints(provider, &settings, model).unwrap();
            let endpoint = set.select(false).unwrap();
            assert_eq!(endpoint.protocol, provider.protocol());
            assert!(!endpoint.url.contains(['?', '#', '{', '}']));
            assert!(
                endpoint.url.ends_with(match provider.protocol() {
                    Protocol::Chat => "/chat/completions",
                    Protocol::Responses => "/responses",
                    Protocol::Messages => "/messages",
                    Protocol::Generate => ":generateContent",
                    Protocol::Converse => "/converse",
                }),
                "{}",
                endpoint.url
            );
        }
        let gemini = endpoints(
            ProviderId::Gemini,
            &ProviderSettings::default(),
            "org/model@version:1",
        )
        .unwrap();
        assert_eq!(
            gemini.select(false).unwrap().url,
            "https://generativelanguage.googleapis.com/v1beta/models/org%2Fmodel%40version%3A1:generateContent"
        );
        let bedrock = ProviderSettings {
            region: Some("us-east-1".into()),
            ..ProviderSettings::default()
        };
        let endpoint = endpoints(
            ProviderId::Bedrock,
            &bedrock,
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc",
        )
        .unwrap();
        assert_eq!(
            endpoint.select(false).unwrap().url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Aapplication-inference-profile%2Fabc/converse"
        );
        for (provider, field) in [
            (ProviderId::Azure, "endpoint"),
            (ProviderId::Cloudflare, "account_id"),
            (ProviderId::Bedrock, "region"),
            (ProviderId::Custom, "base_url"),
        ] {
            let issues = endpoints(provider, &ProviderSettings::default(), "model").unwrap_err();
            assert_eq!(issues[0].field, field);
        }
    }

    #[test]
    fn vertex_routes_distinguish_adc_express_and_claude_without_secret_urls() {
        for (location, host) in [
            ("global", "aiplatform.googleapis.com"),
            ("us", "aiplatform.us.rep.googleapis.com"),
            ("eu", "aiplatform.eu.rep.googleapis.com"),
            ("us-central1", "us-central1-aiplatform.googleapis.com"),
        ] {
            let settings = ProviderSettings {
                project: Some("factory-project".into()),
                location: Some(location.into()),
                ..ProviderSettings::default()
            };
            let set =
                endpoints(ProviderId::Vertex, &settings, "claude-sonnet-5-5@20261001").unwrap();
            assert_eq!(
                set.select(false).unwrap().url,
                format!(
                    "https://{host}/v1/projects/factory-project/locations/{location}/publishers/anthropic/models/claude-sonnet-5-5@20261001:rawPredict"
                )
            );
            assert_eq!(set.select(false).unwrap().protocol, Protocol::Messages);
            assert!(
                set.select(true)
                    .unwrap_err()
                    .message
                    .contains("API keys work only for Gemini")
            );
        }
        let set = endpoints(
            ProviderId::Vertex,
            &ProviderSettings::default(),
            "gemini-3.8-flash",
        )
        .unwrap();
        assert!(set.primary.is_none());
        assert!(set.select(false).is_err());
        assert_eq!(
            set.select(true).unwrap().url,
            "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3.8-flash:generateContent"
        );
        let settings = ProviderSettings {
            project: Some("factory-project".into()),
            ..ProviderSettings::default()
        };
        let set = endpoints(ProviderId::Vertex, &settings, "org/model@version:1").unwrap();
        assert!(
            set.select(false)
                .unwrap()
                .url
                .ends_with("/models/org%2Fmodel@version%3A1:generateContent")
        );
    }

    #[test]
    fn connection_notes_name_hosts_and_warn_about_regional_claude() {
        let previous = ProviderSettings {
            base_url: Some("https://first.example/v1".into()),
            saved_key: true,
            ..ProviderSettings::default()
        };
        let settings = ProviderSettings {
            base_url: Some("https://second.example/v1".into()),
            ..previous.clone()
        };
        assert_eq!(
            settings
                .changed_host_note(ProviderId::Custom, &previous)
                .as_deref(),
            Some("Your saved key will be sent to second.example.")
        );
        assert_eq!(
            previous.changed_host_note(ProviderId::Custom, &previous),
            None
        );
        assert_eq!(
            settings_url("base_url", "http://192.168.1.42/v1").connection_notes(ProviderId::Custom),
            ["Unencrypted connection to 192.168.1.42"]
        );
        let settings = ProviderSettings {
            model: Some("claude-sonnet-5-5".into()),
            location: Some("us-central1".into()),
            ..ProviderSettings::default()
        };
        assert_eq!(settings.connection_notes(ProviderId::Vertex).len(), 1);
        let settings = ProviderSettings {
            location: Some("global".into()),
            ..settings
        };
        assert!(settings.connection_notes(ProviderId::Vertex).is_empty());
    }

    #[test]
    fn settings_round_trip_only_nonsecret_fields() {
        let settings = ProviderSettings {
            model: Some("gpt-6.1-sol".into()),
            effort: Some(Effort::Medium),
            vision: Some(VisionMode::Auto),
            saved_key: true,
            ..ProviderSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["effort"], "medium");
        assert_eq!(value["vision"], "auto");
        assert_eq!(value["saved_key"], true);
        assert_eq!(
            serde_json::from_value::<ProviderSettings>(value).unwrap(),
            settings
        );
        assert_eq!(
            serde_json::from_str::<ProviderSettings>("{}").unwrap(),
            ProviderSettings::default()
        );
        for name in ["api_key", "secret", "password", "auth"] {
            assert!(
                serde_json::from_value::<ProviderSettings>(
                    serde_json::json!({name:"test-only-key"})
                )
                .is_err()
            );
        }
        for field in [
            "model",
            "base_url",
            "account_id",
            "gateway_id",
            "region",
            "profile",
            "project",
            "location",
            "endpoint",
        ] {
            let settings = serde_json::from_value::<ProviderSettings>(
                serde_json::json!({field:"rtsp://user:DO-NOT-LEAK@host/path"}),
            )
            .unwrap();
            let issues = settings.validate(ProviderId::Custom).unwrap_err();
            assert_eq!(issues[0].field, field);
            assert!(!format!("{issues:?}").contains("DO-NOT-LEAK"));
        }
    }
}
