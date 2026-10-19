pub mod claude;
pub mod gemini;
pub mod openai;

use std::pin::Pin;
use std::str::FromStr;

use async_trait::async_trait;
use clap::ValueEnum;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

pub use claude::ClaudeProvider;
pub use gemini::GeminiProvider;
pub use openai::ChatGptProvider;

use crate::store::ProviderCreds;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ValueEnum,
)]
pub enum ProviderId {
    #[value(name = "claude")]
    Claude,
    #[value(name = "chatgpt")]
    ChatGpt,
    #[value(name = "gemini")]
    Gemini,
}

impl ProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderId::Claude => "claude",
            ProviderId::ChatGpt => "chatgpt",
            ProviderId::Gemini => "gemini",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            ProviderId::Claude => "Claude (claude.ai subscription)",
            ProviderId::ChatGpt => "ChatGPT (Plus/Pro subscription)",
            ProviderId::Gemini => "Gemini (Google Antigravity subscription)",
        }
    }

    /// Fallback model used when the user has no preference and the server
    /// catalog is unreachable.
    pub fn builtin_default_model(&self) -> Option<&'static str> {
        match self {
            ProviderId::Claude => Some("claude-sonnet-5"),
            ProviderId::ChatGpt => Some("gpt-5.3-codex"),
            ProviderId::Gemini => Some("gemini-3.8-flash-high"),
        }
    }
}

impl FromStr for ProviderId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "anthropic" => Ok(ProviderId::Claude),
            "chatgpt" | "openai" | "codex" => Ok(ProviderId::ChatGpt),
            "gemini" | "google" | "antigravity" | "agy" => Ok(ProviderId::Gemini),
            other => {
                anyhow::bail!("unknown provider {other:?}; expected claude | chatgpt | gemini")
            }
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Conversation wire types (provider-agnostic)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// Gemini thought signature echoed back with the call.
        #[serde(default)]
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub model: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        args_json: String,
        thought_signature: Option<String>,
    },
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    TurnEnd,
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct UsageInfo {
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub quota: Option<String>,
}

/// A provider instance bound to credentials and an HTTP client.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>>;

    /// Stream one model turn (assistant reply, possibly with tool calls).
    /// Implementations must return `Err` on non-retryable failures and
    /// `Ok(stream)` whose `ProviderEvent::Error` reports mid-stream problems.
    fn stream_turn(
        &self,
        req: TurnRequest,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<ProviderEvent>> + Send + '_>>;

    async fn usage(&self) -> anyhow::Result<UsageInfo>;
}

/// Build a provider from stored credentials (refreshing if needed is the
/// caller's job; providers consume fresh tokens).
pub fn create(
    creds: ProviderCreds,
    http: reqwest::Client,
    debug: bool,
) -> anyhow::Result<Box<dyn Provider>> {
    Ok(match creds {
        ProviderCreds::Claude(c) => Box::new(ClaudeProvider::new(c, http, debug)?),
        ProviderCreds::ChatGpt(c) => Box::new(ChatGptProvider::new(c, http, debug)?),
        ProviderCreds::Gemini(c) => Box::new(GeminiProvider::new(c, http, debug)?),
    })
}
