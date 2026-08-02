//! # Gateway types
//!
//! Shared request/response/connection types for the Model Gateway
//! (`Continuum.md` §16). No secrets ever live in these structs — API keys
//! are stored in the OS credential store, referenced by connection id.

use serde::{Deserialize, Serialize};

/// Which adapter a connection uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Any OpenAI-compatible endpoint (LM Studio, Ollama, OpenRouter, …).
    OpenAiCompat,
    /// Anthropic Messages API with an API key.
    Anthropic,
    /// The official `claude` CLI (subscription auth, no key).
    ClaudeCli,
}

/// A configured provider connection. Persisted to `providers.json` by the
/// desktop app. NOTE: no secret material — `requires_key` only records
/// whether a credential entry exists in the OS credential store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConnection {
    pub id: String,
    pub display_name: String,
    pub kind: ProviderKind,
    /// Base URL, e.g. `http://localhost:1234/v1`. `None` for ClaudeCli.
    pub base_url: Option<String>,
    /// Catalog preset this was created from, if any.
    pub catalog_id: Option<String>,
    /// Model ids cached from the last refresh.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    /// Reserved for Phase 3 role assignment (e.g. "main_ai").
    #[serde(default)]
    pub roles: Vec<String>,
    pub requires_key: bool,
    #[serde(default)]
    pub last_tested_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub last_test_ok: Option<bool>,
}

/// Role of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
}

/// One message in a conversation (system prompt travels separately).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

/// A fully-assembled request handed to an adapter.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    /// Only forwarded by the OpenAI-compatible adapter in v1.
    pub temperature: Option<f32>,
}

/// Token usage reported by a provider, when available.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
}

/// Streaming event emitted by `stream_chat`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Delta {
        text: String,
    },
    Done {
        usage: TokenUsage,
        stop_reason: Option<String>,
    },
    Error {
        message: String,
        retryable: bool,
    },
}

/// Result of a connection test.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionTestReport {
    pub ok: bool,
    pub latency_ms: u64,
    pub models: Vec<String>,
    pub detail: String,
}
