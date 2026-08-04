use std::sync::{Arc, Mutex};
use std::time::Duration;

use continuum_gateway::providers::AnthropicAdapter;
use continuum_gateway::{
    ChatEvent, ChatMessage, ChatProvider, ChatRequest, ChatRole, GatewayError, ToolDef,
    ToolExecutor, DEFAULT_MAX_TOOL_ROUNDS,
};
use futures_util::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> ChatRequest {
    ChatRequest {
        model: "claude-opus-4-6".into(),
        system: "You are Continuum.".into(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
        }],
        max_tokens: 64,
        temperature: Some(0.7), // must NOT be forwarded
        tools: vec![],
        executor: None,
        mcp: None,
        tool_max_rounds: continuum_gateway::DEFAULT_MAX_TOOL_ROUNDS,
    }
}

fn adapter(server: &MockServer) -> AnthropicAdapter {
    AnthropicAdapter::new(
        server.uri(),
        "sk-ant-test".into(),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .expect("adapter")
}

#[tokio::test]
async fn stream_parses_anthropic_events() {
    let server = MockServer::start().await;
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hoi\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut stream = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut text = String::new();
    let mut usage = None;
    while let Some(ev) = stream.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text: t } => text.push_str(&t),
            continuum_gateway::ChatEvent::Done {
                usage: u,
                stop_reason,
            } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                usage = Some(u);
            }
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("{message}"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(text, "Hoi");
    let u = usage.expect("usage");
    assert_eq!(u.input_tokens, Some(9));
    assert_eq!(u.output_tokens, Some(5));
}

#[tokio::test]
async fn request_body_omits_temperature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains(
            "\"max_tokens\":64",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;
    // Guard: any body containing "temperature" would not match a second mock and 404 — assert via received requests.
    let mut s = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    while s.next().await.is_some() {}
    let reqs = server.received_requests().await.expect("requests");
    assert!(!String::from_utf8_lossy(&reqs[0].body).contains("temperature"));
}

#[tokio::test]
async fn refusal_stop_reason_surfaces_error() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\"usage\":{\"output_tokens\":0}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;
    let mut s = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut saw_refusal_error = false;
    while let Some(ev) = s.next().await {
        if let continuum_gateway::ChatEvent::Error { message, .. } = ev {
            assert!(message.to_lowercase().contains("declined"), "{message}");
            saw_refusal_error = true;
        }
    }
    assert!(saw_refusal_error);
}

// ---------------------------------------------------------------------------
// Tool-calling loop
// ---------------------------------------------------------------------------

/// Test executor: records every call and replies with a fixed result.
struct RecordingExecutor {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
    response: Result<String, String>,
}

impl RecordingExecutor {
    fn ok(output: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Ok(output.into()),
        })
    }

    fn err(message: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            response: Err(message.into()),
        })
    }
}

#[async_trait::async_trait]
impl ToolExecutor for RecordingExecutor {
    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        self.calls
            .lock()
            .expect("lock")
            .push((name.to_string(), input.clone()));
        self.response.clone()
    }
}

/// Test executor that never finishes — for cancellation coverage.
struct StallingExecutor;

#[async_trait::async_trait]
impl ToolExecutor for StallingExecutor {
    async fn execute(&self, _name: &str, _input: &serde_json::Value) -> Result<String, String> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok("too late".into())
    }
}

/// `req()` plus one tool definition and an executor.
fn tool_req(executor: Arc<dyn ToolExecutor>, max_rounds: u32) -> ChatRequest {
    ChatRequest {
        tools: vec![ToolDef {
            name: "memory_vault_search".into(),
            description: "Search the memory vault".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }),
        }],
        executor: Some(executor),
        tool_max_rounds: max_rounds,
        ..req()
    }
}

/// Renders Anthropic SSE events, each with its `event:` name line.
fn sse_body(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for ev in events {
        let name = ev["type"].as_str().expect("event type");
        body.push_str(&format!("event: {name}\ndata: {ev}\n\n"));
    }
    body
}

/// A round that streams optional text, then one `tool_use` block whose
/// input arrives split across two `input_json_delta` fragments.
fn tool_round_sse(text: Option<&str>) -> String {
    let mut events = vec![json!({"type":"message_start","message":{"usage":{"input_tokens":9}}})];
    if let Some(t) = text {
        events.push(json!({"type":"content_block_start","index":0,
            "content_block":{"type":"text","text":""}}));
        events.push(json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":t}}));
        events.push(json!({"type":"content_block_stop","index":0}));
    }
    events.push(json!({"type":"content_block_start","index":1,
        "content_block":{"type":"tool_use","id":"toolu_1","name":"memory_vault_search","input":{}}}));
    events.push(json!({"type":"content_block_delta","index":1,
        "delta":{"type":"input_json_delta","partial_json":"{\"que"}}));
    events.push(json!({"type":"content_block_delta","index":1,
        "delta":{"type":"input_json_delta","partial_json":"ry\":\"rust\"}"}}));
    events.push(json!({"type":"content_block_stop","index":1}));
    events.push(
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
        "usage":{"output_tokens":7}}),
    );
    events.push(json!({"type":"message_stop"}));
    sse_body(&events)
}

/// A round with two `tool_use` blocks; the second streams no input
/// deltas (a no-argument call that must surface as `{}`).
fn parallel_tool_round_sse() -> String {
    sse_body(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":9}}}),
        json!({"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_a","name":"memory_vault_search","input":{}}}),
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":"{\"query\":\"a\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"toolu_b","name":"memory_vault_search","input":{}}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
            "usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ])
}

/// A plain text round finishing with `end_turn` and usage.
fn text_round_sse(text: &str) -> String {
    sse_body(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":text}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
            "usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ])
}

/// Mounts an SSE response on `/v1/messages`. `times: Some(n)` expires
/// the mock after `n` uses so a later-mounted mock takes over.
async fn mount_sse(server: &MockServer, body: String, times: Option<u64>) {
    let mut mock = Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        );
    if let Some(n) = times {
        mock = mock.up_to_n_times(n);
    }
    mock.mount(server).await;
}

async fn collect(
    mut stream: futures_util::stream::BoxStream<'static, ChatEvent>,
) -> Vec<ChatEvent> {
    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev);
    }
    events
}

#[tokio::test]
async fn tool_use_round_trip_executes_and_continues() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_round_sse(Some("Let me check")), Some(1)).await;
    mount_sse(&server, text_round_sse("Found it"), None).await;

    let exec = RecordingExecutor::ok("3 results");
    let stream = adapter(&server)
        .stream_chat(
            tool_req(exec.clone(), DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 5, "events: {events:?}");
    assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "Let me check"));
    match &events[1] {
        ChatEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "toolu_1");
            assert_eq!(name, "memory_vault_search");
            // fragments "{\"que" + "ry\":\"rust\"}" merged and parsed
            assert_eq!(input, &json!({"query": "rust"}));
        }
        other => panic!("expected ToolCall second, got {other:?}"),
    }
    match &events[2] {
        ChatEvent::ToolResult {
            id,
            output,
            is_error,
            ..
        } => {
            assert_eq!(id, "toolu_1");
            assert_eq!(output, "3 results");
            assert!(!(*is_error));
        }
        other => panic!("expected ToolResult third, got {other:?}"),
    }
    assert!(matches!(&events[3], ChatEvent::Delta { text } if text == "Found it"));
    match &events[4] {
        ChatEvent::Done { usage, stop_reason } => {
            // Done carries the LAST round's usage/stop_reason
            assert_eq!(usage.input_tokens, Some(3));
            assert_eq!(usage.output_tokens, Some(2));
            assert_eq!(stop_reason.as_deref(), Some("end_turn"));
        }
        other => panic!("expected Done last, got {other:?}"),
    }

    let recorded = exec.calls.lock().expect("lock").clone();
    assert_eq!(
        recorded,
        vec![("memory_vault_search".to_string(), json!({"query": "rust"}))]
    );

    let reqs = server.received_requests().await.expect("recorded");
    assert_eq!(reqs.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("json");
    // Anthropic tool wire shape: no wrapper object
    assert_eq!(
        first["tools"][0],
        json!({
            "name": "memory_vault_search",
            "description": "Search the memory vault",
            "input_schema": {
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }
        })
    );
    assert_eq!(first["system"], "You are Continuum.");
    assert_eq!(first["messages"].as_array().expect("messages").len(), 1);

    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
    let msgs = second["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 3, "user, assistant tool_use, user tool_result");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "hi");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(
        msgs[1]["content"],
        json!([
            {"type": "text", "text": "Let me check"},
            {"type": "tool_use", "id": "toolu_1", "name": "memory_vault_search",
             "input": {"query": "rust"}}
        ])
    );
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(
        msgs[2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_1",
             "content": "3 results", "is_error": false}
        ])
    );
    // the follow-up request keeps the same body shape, tools included
    assert_eq!(second["tools"][0]["name"], "memory_vault_search");
}

#[tokio::test]
async fn executor_error_becomes_error_tool_result_and_loop_continues() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_round_sse(None), Some(1)).await;
    mount_sse(&server, text_round_sse("Recovered"), None).await;

    let exec = RecordingExecutor::err("vault offline");
    let stream = adapter(&server)
        .stream_chat(
            tool_req(exec.clone(), DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 4, "events: {events:?}");
    match &events[1] {
        ChatEvent::ToolResult {
            output, is_error, ..
        } => {
            assert_eq!(output, "vault offline");
            assert!(*is_error);
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    assert!(matches!(&events[2], ChatEvent::Delta { text } if text == "Recovered"));
    assert!(matches!(&events[3], ChatEvent::Done { .. }));

    let reqs = server.received_requests().await.expect("recorded");
    assert_eq!(reqs.len(), 2, "executor error must not abort the turn");
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
    // no text this round → no leading text block
    let assistant = second["messages"][1]["content"]
        .as_array()
        .expect("assistant content");
    assert_eq!(assistant.len(), 1);
    assert_eq!(assistant[0]["type"], "tool_use");
    assert_eq!(
        second["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_1",
             "content": "vault offline", "is_error": true}
        ])
    );
}

#[tokio::test]
async fn parallel_tool_use_blocks_share_one_result_message() {
    let server = MockServer::start().await;
    mount_sse(&server, parallel_tool_round_sse(), Some(1)).await;
    mount_sse(&server, text_round_sse("Both done"), None).await;

    let exec = RecordingExecutor::ok("hit");
    let stream = adapter(&server)
        .stream_chat(
            tool_req(exec.clone(), DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 6, "events: {events:?}");
    assert!(matches!(&events[0], ChatEvent::ToolCall { id, .. } if id == "toolu_a"));
    assert!(matches!(&events[1], ChatEvent::ToolResult { id, .. } if id == "toolu_a"));
    match &events[2] {
        ChatEvent::ToolCall { id, input, .. } => {
            assert_eq!(id, "toolu_b");
            // zero input_json_delta fragments → empty input object
            assert_eq!(input, &json!({}));
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
    assert!(matches!(&events[3], ChatEvent::ToolResult { id, .. } if id == "toolu_b"));

    let reqs = server.received_requests().await.expect("recorded");
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
    let assistant = second["messages"][1]["content"]
        .as_array()
        .expect("assistant content");
    assert_eq!(assistant.len(), 2, "both tool_use blocks, no text block");
    assert_eq!(assistant[0]["id"], "toolu_a");
    assert_eq!(assistant[0]["input"], json!({"query": "a"}));
    assert_eq!(assistant[1]["id"], "toolu_b");
    assert_eq!(assistant[1]["input"], json!({}));
    let results = second["messages"][2]["content"]
        .as_array()
        .expect("tool_result content");
    assert_eq!(results.len(), 2, "one user message with all tool_results");
    assert_eq!(results[0]["tool_use_id"], "toolu_a");
    assert_eq!(results[1]["tool_use_id"], "toolu_b");
}

#[tokio::test]
async fn tools_omitted_when_empty_or_executor_missing() {
    // Empty tools → no "tools" key in the body.
    let server = MockServer::start().await;
    mount_sse(&server, text_round_sse("Hi"), None).await;
    let stream = adapter(&server)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    collect(stream).await;
    let reqs = server.received_requests().await.expect("recorded");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("json");
    assert!(body.get("tools").is_none(), "body: {body}");

    // Tools present but no executor → tools ignored, no "tools" key.
    let server2 = MockServer::start().await;
    mount_sse(&server2, text_round_sse("Hi"), None).await;
    let no_exec = ChatRequest {
        tools: vec![ToolDef {
            name: "t".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }],
        ..req()
    };
    let stream2 = adapter(&server2)
        .stream_chat(no_exec, CancellationToken::new())
        .await
        .expect("stream");
    collect(stream2).await;
    let reqs2 = server2.received_requests().await.expect("recorded");
    let body2: serde_json::Value = serde_json::from_slice(&reqs2[0].body).expect("json");
    assert!(body2.get("tools").is_none(), "body: {body2}");
}

#[tokio::test]
async fn tool_round_limit_emits_error_event() {
    let server = MockServer::start().await;
    // Every round wants more tools — the limit must cut the loop.
    mount_sse(&server, tool_round_sse(None), None).await;

    let exec = RecordingExecutor::ok("3 results");
    let stream = adapter(&server)
        .stream_chat(tool_req(exec.clone(), 1), CancellationToken::new())
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 3, "events: {events:?}");
    assert!(matches!(&events[0], ChatEvent::ToolCall { .. }));
    assert!(matches!(&events[1], ChatEvent::ToolResult { .. }));
    match &events[2] {
        ChatEvent::Error { message, retryable } => {
            assert_eq!(message, "Tool-call limit reached");
            assert!(!retryable);
        }
        other => panic!("expected limit error, got {other:?}"),
    }
    assert_eq!(exec.calls.lock().expect("lock").len(), 1);
    assert_eq!(server.received_requests().await.expect("recorded").len(), 2);
}

#[tokio::test]
async fn zero_tool_rounds_means_no_tool_execution() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_round_sse(None), None).await;

    let exec = RecordingExecutor::ok("never");
    let stream = adapter(&server)
        .stream_chat(tool_req(exec.clone(), 0), CancellationToken::new())
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 1, "events: {events:?}");
    match &events[0] {
        ChatEvent::Error { message, retryable } => {
            assert_eq!(message, "Tool-call limit reached");
            assert!(!retryable);
        }
        other => panic!("expected limit error, got {other:?}"),
    }
    assert!(exec.calls.lock().expect("lock").is_empty());
    assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
}

#[tokio::test]
async fn first_request_4xx_with_tools_retries_once_without_tools() {
    let server = MockServer::start().await;
    // Reject any body carrying tool definitions, as a proxy without tool
    // support would.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"tools\""))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    mount_sse(&server, text_round_sse("Plain chat works"), None).await;

    let exec = RecordingExecutor::ok("unused");
    let stream = adapter(&server)
        .stream_chat(
            tool_req(exec, DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("retry without tools should succeed");
    let events = collect(stream).await;
    assert!(matches!(&events[0], ChatEvent::Delta { text } if text == "Plain chat works"));
    assert!(matches!(events.last(), Some(ChatEvent::Done { .. })));

    let reqs = server.received_requests().await.expect("recorded");
    assert_eq!(reqs.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("json");
    let retry: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
    assert!(first.get("tools").is_some());
    assert!(retry.get("tools").is_none(), "retry body: {retry}");
    // the retry reuses the same messages
    assert_eq!(first["messages"], retry["messages"]);
}

#[tokio::test]
async fn cancel_during_tool_execution_yields_cancelled_error() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_round_sse(None), None).await;

    let cancel = CancellationToken::new();
    let mut stream = adapter(&server)
        .stream_chat(
            tool_req(Arc::new(StallingExecutor), DEFAULT_MAX_TOOL_ROUNDS),
            cancel.clone(),
        )
        .await
        .expect("stream");
    let mut cancelled = false;
    while let Some(ev) = stream.next().await {
        match ev {
            ChatEvent::ToolCall { .. } => cancel.cancel(),
            ChatEvent::Error { message, retryable } => {
                assert_eq!(message, GatewayError::Cancelled.user_message());
                assert!(!retryable);
                cancelled = true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(cancelled, "cancel mid-execution must end the stream");
}

#[tokio::test]
async fn list_models_works() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "claude-opus-4-6"}, {"id": "claude-sonnet-4-6"}]
        })))
        .mount(&server)
        .await;
    let models = adapter(&server).list_models().await.expect("models");
    assert_eq!(models, vec!["claude-opus-4-6", "claude-sonnet-4-6"]);
}
