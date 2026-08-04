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

/// Default cap on tool-call rounds within a single chat turn, used when a
/// [`ChatRequest`] does not need a different value. See
/// [`ChatRequest::tool_max_rounds`].
pub const DEFAULT_MAX_TOOL_ROUNDS: u32 = 8;

/// A tool the model may call, in provider-neutral form.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub input_schema: serde_json::Value,
}

/// Executes tool calls on behalf of an adapter's internal loop.
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Returns the tool result payload as a string (JSON or prose).
    /// Err(String) is delivered to the model as an error tool result —
    /// it must NOT abort the chat turn.
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String>;
}

/// MCP attachment for the Claude CLI adapter (ignored by HTTP adapters).
#[derive(Debug, Clone)]
pub struct McpSpec {
    /// Path to the MCP server binary (continuum-mcp).
    pub server_command: std::path::PathBuf,
    /// Extra env vars for the server process (e.g. CONTINUUM_VAULT_DIR).
    pub env: Vec<(String, String)>,
    /// Values for --allowedTools (joined with ","), e.g. "mcp__continuum__memory_vault_search".
    pub allowed_tools: Vec<String>,
}

/// A fully-assembled request handed to an adapter.
#[derive(Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    /// Only forwarded by the OpenAI-compatible adapter in v1.
    pub temperature: Option<f32>,
    /// Tools offered to the model (HTTP adapters). Empty = no tool calling.
    pub tools: Vec<ToolDef>,
    /// Executes tool calls mid-stream. None = tools ignored even if non-empty.
    pub executor: Option<std::sync::Arc<dyn ToolExecutor>>,
    /// MCP config for the Claude CLI adapter. None = no MCP.
    pub mcp: Option<McpSpec>,
    /// Cap on tool-call rounds within this chat turn, to bound runaway
    /// tool-calling loops. Defaults to [`DEFAULT_MAX_TOOL_ROUNDS`].
    pub tool_max_rounds: u32,
}

/// Placeholder `Debug` payload for `ChatRequest::executor`, since
/// `dyn ToolExecutor` has no meaningful `Debug` of its own. Renders as
/// `Some(<dyn ToolExecutor>)` or `None` with no surrounding quotes.
struct ExecutorDebug(bool);

impl std::fmt::Debug for ExecutorDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 {
            write!(f, "Some(<dyn ToolExecutor>)")
        } else {
            write!(f, "None")
        }
    }
}

impl std::fmt::Debug for ChatRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatRequest")
            .field("model", &self.model)
            .field("system", &self.system)
            .field("messages", &self.messages)
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("tools", &self.tools)
            .field("executor", &ExecutorDebug(self.executor.is_some()))
            .field("mcp", &self.mcp)
            .field("tool_max_rounds", &self.tool_max_rounds)
            .finish()
    }
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
    /// The model invoked a tool; emitted when the call is fully known.
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A tool finished; `output` is the string given back to the model.
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    struct StubExecutor;

    #[async_trait::async_trait]
    impl ToolExecutor for StubExecutor {
        async fn execute(&self, _name: &str, _input: &serde_json::Value) -> Result<String, String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn tool_call_event_serializes_with_snake_case_tag() {
        let ev = ChatEvent::ToolCall {
            id: "call_1".into(),
            name: "memory_vault_search".into(),
            input: serde_json::json!({"query": "foo"}),
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["id"], "call_1");
        assert_eq!(json["name"], "memory_vault_search");
        assert_eq!(json["input"]["query"], "foo");
    }

    #[test]
    fn tool_result_event_serializes_with_snake_case_tag() {
        let ev = ChatEvent::ToolResult {
            id: "call_1".into(),
            output: "42 results".into(),
            is_error: false,
            duration_ms: 12,
        };
        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["id"], "call_1");
        assert_eq!(json["output"], "42 results");
        assert_eq!(json["is_error"], false);
        assert_eq!(json["duration_ms"], 12);
    }

    #[test]
    fn chat_request_debug_with_executor_present_does_not_panic_and_mentions_field() {
        let req = ChatRequest {
            model: "m".into(),
            system: "s".into(),
            messages: vec![],
            max_tokens: 1,
            temperature: None,
            tools: vec![],
            executor: Some(std::sync::Arc::new(StubExecutor)),
            mcp: None,
            tool_max_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        };
        let debug_str = format!("{req:?}");
        assert!(
            debug_str.contains("executor: Some(<dyn ToolExecutor>)"),
            "debug output missing executor field: {debug_str}"
        );
    }

    #[test]
    fn chat_request_debug_without_executor_prints_none() {
        let req = ChatRequest {
            model: "m".into(),
            system: "s".into(),
            messages: vec![],
            max_tokens: 1,
            temperature: None,
            tools: vec![],
            executor: None,
            mcp: None,
            tool_max_rounds: DEFAULT_MAX_TOOL_ROUNDS,
        };
        let debug_str = format!("{req:?}");
        assert!(
            debug_str.contains("executor: None"),
            "debug output missing executor field: {debug_str}"
        );
    }
}
