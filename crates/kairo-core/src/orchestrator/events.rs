//! # Claude Code event types
//!
//! Strongly-typed representations of the JSON events that Claude Code emits
//! on stdout when running in `--output-format stream-json` mode.
//!
//! ## Event sequence (observed from CLI v2.1.100)
//!
//! A typical single-turn interaction produces events in this order:
//!
//! 1. `system` (subtype `init`) — session id, tools, model, version info
//! 2. `stream_event` (`message_start`) — raw API message start
//! 3. `stream_event` (`content_block_start`) — beginning of a text block
//! 4. `stream_event` (`content_block_delta`) — incremental text chunks
//! 5. `assistant` — partial/full message snapshot (when `--include-partial-messages`)
//! 6. `stream_event` (`content_block_stop`) — end of content block
//! 7. `stream_event` (`message_delta`) — stop reason, final usage
//! 8. `stream_event` (`message_stop`) — message complete
//! 9. `rate_limit_event` — rate limit status (undocumented, added ~2026)
//! 10. `result` — final summary with cost, duration, session id
//!
//! Uses [`serde`]'s `#[serde(tag = "type")]` for top-level dispatch.
//! Unknown event types deserialize into [`ClaudeEvent::Unknown`] to stay
//! forward-compatible with future CLI changes.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Top-level event enum
// ---------------------------------------------------------------------------

/// A single JSON event emitted by the Claude Code CLI on stdout.
///
/// Each line of stdout is one of these events. Parse with
/// [`serde_json::from_str`] or [`serde_json::from_slice`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClaudeEvent {
    /// System initialization event emitted at the start of a session.
    #[serde(rename = "system")]
    System(SystemEvent),

    /// A raw Anthropic API streaming event, wrapped with session metadata.
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEvent),

    /// An assistant message snapshot (partial or complete).
    ///
    /// Emitted when `--include-partial-messages` is set. Contains the
    /// accumulated message content up to this point.
    #[serde(rename = "assistant")]
    Assistant(AssistantEvent),

    /// A user message (seen in multi-turn interactions).
    #[serde(rename = "user")]
    User(UserEvent),

    /// Rate limit status information.
    ///
    /// Not documented in the official CLI docs as of 2026-04. Emitted
    /// after the message completes, before the `result` event.
    #[serde(rename = "rate_limit_event")]
    RateLimit(RateLimitEvent),

    /// The final event in a session turn, containing cost and timing.
    #[serde(rename = "result")]
    Result(ResultEvent),

    /// Catch-all for event types not yet modeled.
    ///
    /// This ensures forward compatibility — new event types added by
    /// Anthropic will be captured as raw JSON instead of causing parse errors.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// System event
// ---------------------------------------------------------------------------

/// System init event with session metadata.
///
/// Emitted once at the start of each session. Contains the session id,
/// available tools, model name, and version information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemEvent {
    /// The sub-type of system event (e.g. `"init"`).
    #[serde(default)]
    pub subtype: Option<String>,

    /// The working directory the CLI was invoked from.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Unique session identifier.
    #[serde(default)]
    pub session_id: Option<String>,

    /// List of available tool names.
    #[serde(default)]
    pub tools: Vec<String>,

    /// Connected MCP servers.
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,

    /// The model being used (e.g. `"claude-opus-4-6"`).
    #[serde(default)]
    pub model: Option<String>,

    /// Claude Code CLI version.
    #[serde(default)]
    pub claude_code_version: Option<String>,

    /// Permission mode (e.g. `"default"`).
    #[serde(default, rename = "permissionMode")]
    pub permission_mode: Option<String>,

    /// How the API key is sourced (e.g. `"none"` for subscription auth).
    #[serde(default, rename = "apiKeySource")]
    pub api_key_source: Option<String>,

    /// Unique event identifier.
    #[serde(default)]
    pub uuid: Option<String>,

    /// All other fields we don't explicitly model.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Stream event (wraps raw Anthropic API events)
// ---------------------------------------------------------------------------

/// A wrapper around a raw Anthropic API streaming event.
///
/// The inner [`ApiEvent`] contains the actual event data (message start,
/// content block deltas, etc.). The outer fields provide Claude Code
/// session context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    /// The raw Anthropic API event.
    pub event: ApiEvent,

    /// Session id this event belongs to.
    #[serde(default)]
    pub session_id: Option<String>,

    /// If this event is inside a tool use, the parent tool_use id.
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,

    /// Unique event identifier.
    #[serde(default)]
    pub uuid: Option<String>,
}

/// A raw Anthropic API streaming event.
///
/// These map directly to the Anthropic Messages API streaming protocol.
/// See <https://docs.anthropic.com/en/api/messages-streaming>.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ApiEvent {
    /// Start of a new message.
    #[serde(rename = "message_start")]
    MessageStart {
        /// The (partial) message object.
        #[serde(default)]
        message: serde_json::Value,
    },

    /// Start of a new content block within a message.
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        /// Zero-based index of this content block.
        #[serde(default)]
        index: u32,
        /// The content block metadata.
        #[serde(default)]
        content_block: Option<ContentBlock>,
    },

    /// An incremental update to a content block.
    ///
    /// For text blocks, this contains [`Delta::TextDelta`] with the new text.
    /// For tool use blocks, this contains [`Delta::InputJsonDelta`].
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        /// Zero-based index of the content block being updated.
        #[serde(default)]
        index: u32,
        /// The incremental update.
        #[serde(default)]
        delta: Option<Delta>,
    },

    /// A content block has finished.
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        /// Zero-based index of the completed content block.
        #[serde(default)]
        index: u32,
    },

    /// Message-level metadata update (stop reason, final usage).
    #[serde(rename = "message_delta")]
    MessageDelta {
        /// The message-level delta.
        #[serde(default)]
        delta: Option<MessageDeltaBody>,
        /// Updated usage statistics.
        #[serde(default)]
        usage: Option<serde_json::Value>,
    },

    /// The message is fully complete.
    #[serde(rename = "message_stop")]
    MessageStop,

    /// Catch-all for unrecognized API event types.
    #[serde(other)]
    Unknown,
}

/// A content block within an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// A text content block.
    #[serde(rename = "text")]
    Text {
        /// The text content (empty string at block start, full text in snapshots).
        #[serde(default)]
        text: String,
    },

    /// A tool use content block.
    #[serde(rename = "tool_use")]
    ToolUse {
        /// The unique id for this tool invocation.
        #[serde(default)]
        id: String,
        /// The tool name.
        #[serde(default)]
        name: String,
        /// The (partial) JSON input for the tool.
        #[serde(default)]
        input: serde_json::Value,
    },

    /// Catch-all for unknown content block types.
    #[serde(other)]
    Unknown,
}

/// An incremental delta within a content block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Delta {
    /// A text delta — the primary event for live streaming output.
    #[serde(rename = "text_delta")]
    TextDelta {
        /// The incremental text to append.
        #[serde(default)]
        text: String,
    },

    /// A JSON delta for tool input streaming.
    #[serde(rename = "input_json_delta")]
    InputJsonDelta {
        /// Partial JSON string to append to the tool input.
        #[serde(default)]
        partial_json: String,
    },

    /// Catch-all for unknown delta types.
    #[serde(other)]
    Unknown,
}

/// The delta body inside a `message_delta` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    /// Why the message stopped (e.g. `"end_turn"`, `"tool_use"`).
    #[serde(default)]
    pub stop_reason: Option<String>,

    /// All other fields.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Assistant event
// ---------------------------------------------------------------------------

/// An assistant message snapshot.
///
/// With `--include-partial-messages`, the CLI emits these as the message
/// accumulates. The `message` field contains the full content up to this point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantEvent {
    /// The assistant message object.
    pub message: AssistantMessage,

    /// Session id.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Parent tool use id (if inside a tool call context).
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,

    /// Unique event identifier.
    #[serde(default)]
    pub uuid: Option<String>,
}

/// The message body inside an assistant event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// The model that generated this message.
    #[serde(default)]
    pub model: Option<String>,

    /// The message id.
    #[serde(default)]
    pub id: Option<String>,

    /// Always `"assistant"`.
    #[serde(default)]
    pub role: Option<String>,

    /// The content blocks (text, tool_use, etc.).
    #[serde(default)]
    pub content: Vec<ContentBlock>,

    /// Why the message stopped.
    #[serde(default)]
    pub stop_reason: Option<String>,

    /// All other fields (usage, etc.).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// User event
// ---------------------------------------------------------------------------

/// A user message event (seen in multi-turn interactions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEvent {
    /// The user message object.
    #[serde(default)]
    pub message: serde_json::Value,

    /// Session id.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Unique event identifier.
    #[serde(default)]
    pub uuid: Option<String>,

    /// All other fields.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Rate limit event
// ---------------------------------------------------------------------------

/// Rate limit status information.
///
/// Undocumented as of Claude Code v2.1.100. Emitted after the message
/// completes, before the `result` event. Contains information about the
/// current rate limit window and overage status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitEvent {
    /// Detailed rate limit information.
    #[serde(default)]
    pub rate_limit_info: Option<RateLimitInfo>,

    /// Session id.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Unique event identifier.
    #[serde(default)]
    pub uuid: Option<String>,
}

/// Detailed rate limit information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    /// Current status (e.g. `"allowed"`).
    #[serde(default)]
    pub status: Option<String>,

    /// Unix timestamp when the rate limit resets.
    #[serde(default, rename = "resetsAt")]
    pub resets_at: Option<u64>,

    /// The type of rate limit (e.g. `"five_hour"`).
    #[serde(default, rename = "rateLimitType")]
    pub rate_limit_type: Option<String>,

    /// Overage status (e.g. `"allowed"`).
    #[serde(default, rename = "overageStatus")]
    pub overage_status: Option<String>,

    /// Unix timestamp when overage resets.
    #[serde(default, rename = "overageResetsAt")]
    pub overage_resets_at: Option<u64>,

    /// Whether the request is using overage capacity.
    #[serde(default, rename = "isUsingOverage")]
    pub is_using_overage: bool,
}

// ---------------------------------------------------------------------------
// Result event
// ---------------------------------------------------------------------------

/// The final event in a session turn.
///
/// Contains the complete result text, total cost, timing, and detailed
/// usage statistics. This is the signal that the turn is complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultEvent {
    /// Sub-type (e.g. `"success"`, `"error"`).
    #[serde(default)]
    pub subtype: Option<String>,

    /// Whether this result represents an error.
    #[serde(default)]
    pub is_error: bool,

    /// Wall-clock duration in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,

    /// API-side duration in milliseconds.
    #[serde(default)]
    pub duration_api_ms: Option<u64>,

    /// Number of conversational turns in this session.
    #[serde(default)]
    pub num_turns: Option<u32>,

    /// The final result text.
    #[serde(default)]
    pub result: Option<String>,

    /// Why the session stopped (e.g. `"end_turn"`).
    #[serde(default)]
    pub stop_reason: Option<String>,

    /// Session id.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Total cost in USD for this session turn.
    #[serde(default)]
    pub total_cost_usd: Option<f64>,

    /// Detailed usage statistics.
    #[serde(default)]
    pub usage: Option<serde_json::Value>,

    /// Per-model usage breakdown.
    #[serde(default, rename = "modelUsage")]
    pub model_usage: Option<serde_json::Value>,

    /// Unique event identifier.
    #[serde(default)]
    pub uuid: Option<String>,

    /// All other fields (permission_denials, terminal_reason, etc.).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Convenience methods
// ---------------------------------------------------------------------------

impl ClaudeEvent {
    /// Try to extract a text delta from this event.
    ///
    /// Returns `Some(text)` if this is a `stream_event` containing a
    /// `content_block_delta` with a `text_delta`. Returns `None` otherwise.
    ///
    /// This is the primary method for extracting streaming text output.
    pub fn as_text_delta(&self) -> Option<&str> {
        match self {
            ClaudeEvent::StreamEvent(se) => match &se.event {
                ApiEvent::ContentBlockDelta {
                    delta: Some(Delta::TextDelta { text }),
                    ..
                } => Some(text.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Check if this is the final `result` event.
    pub fn is_result(&self) -> bool {
        matches!(self, ClaudeEvent::Result(_))
    }

    /// Check if this is a system init event.
    pub fn is_system_init(&self) -> bool {
        matches!(
            self,
            ClaudeEvent::System(SystemEvent {
                subtype: Some(ref s),
                ..
            }) if s == "init"
        )
    }
}

// ---------------------------------------------------------------------------
// Display for logging
// ---------------------------------------------------------------------------

impl std::fmt::Display for ClaudeEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeEvent::System(_) => write!(f, "[system]"),
            ClaudeEvent::StreamEvent(se) => match &se.event {
                ApiEvent::MessageStart { .. } => write!(f, "[stream:message_start]"),
                ApiEvent::ContentBlockStart { .. } => write!(f, "[stream:content_block_start]"),
                ApiEvent::ContentBlockDelta { .. } => write!(f, "[stream:content_block_delta]"),
                ApiEvent::ContentBlockStop { .. } => write!(f, "[stream:content_block_stop]"),
                ApiEvent::MessageDelta { .. } => write!(f, "[stream:message_delta]"),
                ApiEvent::MessageStop => write!(f, "[stream:message_stop]"),
                ApiEvent::Unknown => write!(f, "[stream:unknown]"),
            },
            ClaudeEvent::Assistant(_) => write!(f, "[assistant]"),
            ClaudeEvent::User(_) => write!(f, "[user]"),
            ClaudeEvent::RateLimit(_) => write!(f, "[rate_limit]"),
            ClaudeEvent::Result(_) => write!(f, "[result]"),
            ClaudeEvent::Unknown => write!(f, "[unknown]"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Real JSON captured from Claude Code CLI v2.1.100 on 2026-04-10.
    // These are the ground truth for our parser.

    const SYSTEM_INIT_JSON: &str = r#"{"type":"system","subtype":"init","cwd":"F:\\TRYORVIA\\kairo-ai","session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","tools":["Task","Bash","Edit","Glob","Grep","Read","Write"],"mcp_servers":[],"model":"claude-opus-4-6","permissionMode":"default","apiKeySource":"none","claude_code_version":"2.1.100","uuid":"6b5e8466-80b2-4e34-b671-e39e16c50254","fast_mode_state":"off"}"#;

    const STREAM_MESSAGE_START_JSON: &str = r#"{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-4-6","id":"msg_01Y57USL58KkstxVjFbKAX4W","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":1}}},"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","parent_tool_use_id":null,"uuid":"77e6b880-f0a6-471a-8010-ef024e233cc4"}"#;

    const STREAM_CONTENT_BLOCK_START_JSON: &str = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","parent_tool_use_id":null,"uuid":"4737bf93-16fb-49ca-8b5a-b0a46c1bcce6"}"#;

    const STREAM_TEXT_DELTA_JSON: &str = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"2+2 is 4."}},"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","parent_tool_use_id":null,"uuid":"85169869-084b-44e3-b5ca-9adf5f9d2abf"}"#;

    const ASSISTANT_JSON: &str = r#"{"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_01Y57USL58KkstxVjFbKAX4W","type":"message","role":"assistant","content":[{"type":"text","text":"2+2 is 4."}],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","uuid":"a45112f7-4cf3-48da-888c-fe50913f92db"}"#;

    const STREAM_CONTENT_BLOCK_STOP_JSON: &str = r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","parent_tool_use_id":null,"uuid":"72155bf9-0936-4f70-83d7-c175dbbb9a99"}"#;

    const STREAM_MESSAGE_DELTA_JSON: &str = r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":3,"output_tokens":12}},"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","parent_tool_use_id":null,"uuid":"381d7e6d-17b5-4026-a31d-7611a17226a1"}"#;

    const STREAM_MESSAGE_STOP_JSON: &str = r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","parent_tool_use_id":null,"uuid":"f997ba50-8928-438e-b8fc-5cc11ca92086"}"#;

    const RATE_LIMIT_JSON: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1775847600,"rateLimitType":"five_hour","overageStatus":"allowed","overageResetsAt":1775833200,"isUsingOverage":false},"uuid":"637e7c3b-4c4d-46af-aa7e-1109818778ad","session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d"}"#;

    const RESULT_JSON: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2324,"duration_api_ms":2311,"num_turns":1,"result":"2+2 is 4.","stop_reason":"end_turn","session_id":"f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d","total_cost_usd":0.0624165,"usage":{"input_tokens":3,"output_tokens":12},"modelUsage":{"claude-opus-4-6":{"inputTokens":3,"outputTokens":12,"costUSD":0.0624165}},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","uuid":"da518236-456b-4ffd-813a-d9835b4f4378"}"#;

    #[test]
    fn parse_system_init() {
        let event: ClaudeEvent = serde_json::from_str(SYSTEM_INIT_JSON).unwrap();
        match &event {
            ClaudeEvent::System(sys) => {
                assert_eq!(sys.subtype.as_deref(), Some("init"));
                assert_eq!(sys.model.as_deref(), Some("claude-opus-4-6"));
                assert_eq!(
                    sys.session_id.as_deref(),
                    Some("f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d")
                );
                assert_eq!(sys.claude_code_version.as_deref(), Some("2.1.100"));
                assert!(!sys.tools.is_empty());
            }
            other => panic!("expected System, got {other}"),
        }
        assert!(event.is_system_init());
    }

    #[test]
    fn parse_stream_message_start() {
        let event: ClaudeEvent = serde_json::from_str(STREAM_MESSAGE_START_JSON).unwrap();
        match &event {
            ClaudeEvent::StreamEvent(se) => {
                assert!(matches!(se.event, ApiEvent::MessageStart { .. }));
                assert_eq!(
                    se.session_id.as_deref(),
                    Some("f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d")
                );
            }
            other => panic!("expected StreamEvent, got {other}"),
        }
    }

    #[test]
    fn parse_stream_content_block_start() {
        let event: ClaudeEvent = serde_json::from_str(STREAM_CONTENT_BLOCK_START_JSON).unwrap();
        match &event {
            ClaudeEvent::StreamEvent(se) => match &se.event {
                ApiEvent::ContentBlockStart {
                    index,
                    content_block,
                } => {
                    assert_eq!(*index, 0);
                    assert!(matches!(content_block, Some(ContentBlock::Text { .. })));
                }
                other => panic!("expected ContentBlockStart, got {other:?}"),
            },
            other => panic!("expected StreamEvent, got {other}"),
        }
    }

    #[test]
    fn parse_stream_text_delta() {
        let event: ClaudeEvent = serde_json::from_str(STREAM_TEXT_DELTA_JSON).unwrap();
        assert_eq!(event.as_text_delta(), Some("2+2 is 4."));
    }

    #[test]
    fn parse_assistant_message() {
        let event: ClaudeEvent = serde_json::from_str(ASSISTANT_JSON).unwrap();
        match &event {
            ClaudeEvent::Assistant(a) => {
                assert_eq!(a.message.model.as_deref(), Some("claude-opus-4-6"));
                assert_eq!(a.message.role.as_deref(), Some("assistant"));
                assert_eq!(a.message.content.len(), 1);
                match &a.message.content[0] {
                    ContentBlock::Text { text } => {
                        assert_eq!(text, "2+2 is 4.");
                    }
                    other => panic!("expected Text content, got {other:?}"),
                }
            }
            other => panic!("expected Assistant, got {other}"),
        }
    }

    #[test]
    fn parse_stream_content_block_stop() {
        let event: ClaudeEvent = serde_json::from_str(STREAM_CONTENT_BLOCK_STOP_JSON).unwrap();
        match &event {
            ClaudeEvent::StreamEvent(se) => {
                assert!(matches!(se.event, ApiEvent::ContentBlockStop { index: 0 }));
            }
            other => panic!("expected StreamEvent, got {other}"),
        }
    }

    #[test]
    fn parse_stream_message_delta() {
        let event: ClaudeEvent = serde_json::from_str(STREAM_MESSAGE_DELTA_JSON).unwrap();
        match &event {
            ClaudeEvent::StreamEvent(se) => match &se.event {
                ApiEvent::MessageDelta { delta, .. } => {
                    let delta = delta.as_ref().expect("delta should be present");
                    assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                }
                other => panic!("expected MessageDelta, got {other:?}"),
            },
            other => panic!("expected StreamEvent, got {other}"),
        }
    }

    #[test]
    fn parse_stream_message_stop() {
        let event: ClaudeEvent = serde_json::from_str(STREAM_MESSAGE_STOP_JSON).unwrap();
        match &event {
            ClaudeEvent::StreamEvent(se) => {
                assert!(matches!(se.event, ApiEvent::MessageStop));
            }
            other => panic!("expected StreamEvent, got {other}"),
        }
    }

    #[test]
    fn parse_rate_limit_event() {
        let event: ClaudeEvent = serde_json::from_str(RATE_LIMIT_JSON).unwrap();
        match &event {
            ClaudeEvent::RateLimit(rl) => {
                let info = rl.rate_limit_info.as_ref().expect("info present");
                assert_eq!(info.status.as_deref(), Some("allowed"));
                assert_eq!(info.rate_limit_type.as_deref(), Some("five_hour"));
                assert!(!info.is_using_overage);
            }
            other => panic!("expected RateLimit, got {other}"),
        }
    }

    #[test]
    fn parse_result_event() {
        let event: ClaudeEvent = serde_json::from_str(RESULT_JSON).unwrap();
        match &event {
            ClaudeEvent::Result(r) => {
                assert_eq!(r.subtype.as_deref(), Some("success"));
                assert!(!r.is_error);
                assert_eq!(r.duration_ms, Some(2324));
                assert_eq!(r.duration_api_ms, Some(2311));
                assert_eq!(r.num_turns, Some(1));
                assert_eq!(r.result.as_deref(), Some("2+2 is 4."));
                assert_eq!(r.stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(
                    r.session_id.as_deref(),
                    Some("f33ddd7b-e6ed-4cdb-b4be-fb166b9a7a3d")
                );
                assert!(r.total_cost_usd.is_some());
                let cost = r.total_cost_usd.unwrap();
                assert!(cost > 0.0);
            }
            other => panic!("expected Result, got {other}"),
        }
        assert!(event.is_result());
    }

    #[test]
    fn parse_unknown_event_type_does_not_crash() {
        let json = r#"{"type":"some_future_event","data":"whatever"}"#;
        let event: ClaudeEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, ClaudeEvent::Unknown));
    }

    #[test]
    fn text_delta_returns_none_for_non_delta_events() {
        let event: ClaudeEvent = serde_json::from_str(SYSTEM_INIT_JSON).unwrap();
        assert!(event.as_text_delta().is_none());

        let event: ClaudeEvent = serde_json::from_str(RESULT_JSON).unwrap();
        assert!(event.as_text_delta().is_none());
    }

    #[test]
    fn display_format_covers_all_variants() {
        let system: ClaudeEvent = serde_json::from_str(SYSTEM_INIT_JSON).unwrap();
        assert_eq!(format!("{system}"), "[system]");

        let delta: ClaudeEvent = serde_json::from_str(STREAM_TEXT_DELTA_JSON).unwrap();
        assert_eq!(format!("{delta}"), "[stream:content_block_delta]");

        let result: ClaudeEvent = serde_json::from_str(RESULT_JSON).unwrap();
        assert_eq!(format!("{result}"), "[result]");
    }
}
