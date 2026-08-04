use std::sync::{Arc, Mutex};
use std::time::Duration;

use continuum_gateway::providers::OpenAiCompatAdapter;
use continuum_gateway::{
    ChatEvent, ChatMessage, ChatProvider, ChatRequest, ChatRole, GatewayError, ToolDef,
    ToolExecutor, DEFAULT_MAX_TOOL_ROUNDS,
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn req() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        system: "You are Continuum.".into(),
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
        }],
        max_tokens: 64,
        temperature: None,
        tools: vec![],
        executor: None,
        mcp: None,
        tool_max_rounds: continuum_gateway::DEFAULT_MAX_TOOL_ROUNDS,
    }
}

fn adapter(server: &MockServer, key: Option<&str>) -> OpenAiCompatAdapter {
    OpenAiCompatAdapter::new(
        format!("{}/v1", server.uri()),
        key.map(String::from),
        Duration::from_secs(2),
        Duration::from_secs(2),
    )
    .expect("adapter")
}

#[tokio::test]
async fn list_models_parses_ids_and_sends_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer k1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "m1"}, {"id": "m2"}]
        })))
        .mount(&server)
        .await;
    let models = adapter(&server, Some("k1"))
        .list_models()
        .await
        .expect("models");
    assert_eq!(models, vec!["m1", "m2"]);
}

#[tokio::test]
async fn stream_chat_yields_deltas_then_done() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut stream = adapter(&server, None)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text: t } => text.push_str(&t),
            continuum_gateway::ChatEvent::Done { usage, .. } => {
                assert_eq!(usage.output_tokens, Some(2));
                done = true;
            }
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("error: {message}"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(text, "Hello");
    assert!(done);
}

#[tokio::test]
async fn maps_401_and_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = adapter(&server, Some("bad"))
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(err, GatewayError::Unauthorized));

    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server2)
        .await;
    let err2 = adapter(&server2, None)
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(
        err2,
        GatewayError::RateLimited {
            retry_after_secs: Some(7)
        }
    ));
}

#[tokio::test]
async fn stream_end_without_done_still_yields_done() {
    let server = MockServer::start().await;
    // No `data: [DONE]` terminator — the mock server closes the connection
    // after writing this body, so the stream must end on its own.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut stream = adapter(&server, None)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = stream.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text: t } => text.push_str(&t),
            continuum_gateway::ChatEvent::Done { .. } => done = true,
            continuum_gateway::ChatEvent::Error { message, .. } => panic!("error: {message}"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(text, "Hi");
    assert!(
        done,
        "connection close without [DONE] must still yield Done"
    );
}

#[tokio::test]
async fn cancellation_yields_cancelled_error() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;

    let cancel = CancellationToken::new();
    let mut stream = adapter(&server, None)
        .stream_chat(req(), cancel.clone())
        .await
        .expect("stream");
    // Cancel before ever polling the stream: the eager `is_cancelled()`
    // check at the top of the stream task's loop guarantees the Cancelled
    // error wins deterministically, rather than racing an already-cancelled
    // token against a possibly-already-buffered first chunk.
    cancel.cancel();
    let ev = stream.next().await.expect("event");
    match ev {
        continuum_gateway::ChatEvent::Error { message, retryable } => {
            assert_eq!(message, GatewayError::Cancelled.user_message());
            assert!(!retryable);
        }
        other => panic!("expected cancelled error as first event, got {other:?}"),
    }
}

#[tokio::test]
async fn idle_timeout_yields_timeout_error() {
    // Coverage note: wiremock can only delay a response as a whole (there's
    // no way to start writing a body and then stall mid-stream), so this
    // exercises the *outer* request timeout added for the "no overall
    // request timeout" finding — the initial POST's `send()` is bounded by
    // `idle_timeout` — rather than the mid-stream idle-timeout branch inside
    // `stream_events`'s `tokio::select!` loop. A mid-stream stall isn't
    // constructible with wiremock; see `mid_stream_stall_yields_timeout_error`
    // below, which drives that branch via a hand-rolled raw-TCP fixture.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw("data: [DONE]\n\n", "text/event-stream")
                .set_delay(Duration::from_secs(1)),
        )
        .mount(&server)
        .await;

    let a = OpenAiCompatAdapter::new(
        format!("{}/v1", server.uri()),
        None,
        Duration::from_secs(2),
        Duration::from_millis(200),
    )
    .expect("adapter");
    let err = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .err()
        .expect("err");
    assert!(matches!(err, GatewayError::Timeout));
}

#[tokio::test]
async fn mid_stream_stall_yields_timeout_error() {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        // Read the request bytes (don't parse; just drain a bit) then answer
        let mut buf = [0u8; 4096];
        use tokio::io::AsyncReadExt;
        let _ = sock.read(&mut buf).await;
        let resp =
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
        sock.write_all(resp.as_bytes()).await.expect("headers");
        // one complete SSE event as a chunked body piece
        let payload = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n";
        let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
        sock.write_all(chunk.as_bytes()).await.expect("chunk");
        sock.flush().await.expect("flush");
        // then stall forever (never send terminating chunk)
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    });
    let a = OpenAiCompatAdapter::new(
        format!("http://{addr}/v1"),
        None,
        Duration::from_secs(2),
        Duration::from_millis(200),
    )
    .expect("adapter");
    let mut s = a
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    let mut got_delta = false;
    let mut got_timeout = false;
    while let Some(ev) = s.next().await {
        match ev {
            continuum_gateway::ChatEvent::Delta { text } => {
                assert_eq!(text, "Hi");
                got_delta = true;
            }
            continuum_gateway::ChatEvent::Error { message, retryable } => {
                assert!(retryable);
                assert_eq!(message, GatewayError::Timeout.user_message());
                got_timeout = true;
            }
            continuum_gateway::ChatEvent::Done { .. } => panic!("should not complete"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(got_delta && got_timeout);
}

#[tokio::test]
async fn unreachable_maps_to_unreachable() {
    let a = OpenAiCompatAdapter::new(
        "http://127.0.0.1:9".into(), // port 9 (discard) — nothing listens
        None,
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .expect("adapter");
    let err = a.list_models().await.expect_err("err");
    assert!(matches!(
        err,
        GatewayError::Unreachable { .. } | GatewayError::Timeout
    ));
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
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }),
        }],
        executor: Some(executor),
        tool_max_rounds: max_rounds,
        ..req()
    }
}

/// Renders JSON events as an SSE body terminated with `[DONE]`.
fn sse_body(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for ev in events {
        body.push_str(&format!("data: {ev}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// A round that requests one tool call, with `arguments` split across two
/// deltas (exercises fragment accumulation end-to-end).
fn tool_call_sse() -> String {
    sse_body(&[
        serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","type":"function",
             "function":{"name":"memory_vault_search","arguments":"{\"que"}}
        ]}}]}),
        serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"ry\":\"rust\"}"}}
        ]}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ])
}

/// A round that requests a tool call with unparseable JSON arguments.
fn bad_args_sse() -> String {
    sse_body(&[
        serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_9",
             "function":{"name":"memory_vault_search","arguments":"{not json"}}
        ]}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ])
}

/// A round that requests two parallel tool calls (index 0 and 1).
fn parallel_tool_call_sse() -> String {
    sse_body(&[
        serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a",
             "function":{"name":"memory_vault_search","arguments":"{\"query\":\"a\"}"}}
        ]}}]}),
        serde_json::json!({"choices":[{"delta":{"tool_calls":[
            {"index":1,"id":"call_b",
             "function":{"name":"memory_vault_search","arguments":"{\"query\":\"b\"}"}}
        ]}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ])
}

/// A plain text round finishing with `stop` and usage.
fn text_sse(text: &str) -> String {
    sse_body(&[
        serde_json::json!({"choices":[{"delta":{"content":text}}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2}}),
    ])
}

/// Mounts an SSE response on `/v1/chat/completions`. `times: Some(n)`
/// expires the mock after `n` uses so a later-mounted mock takes over.
async fn mount_sse(server: &MockServer, body: String, times: Option<u64>) {
    let mut mock = Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
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
async fn tool_call_round_trip_executes_and_continues() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_call_sse(), Some(1)).await;
    mount_sse(&server, text_sse("Found it"), None).await;

    let exec = RecordingExecutor::ok("3 results");
    let stream = adapter(&server, None)
        .stream_chat(
            tool_req(exec.clone(), DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 4, "events: {events:?}");
    match &events[0] {
        ChatEvent::ToolCall { id, name, input } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "memory_vault_search");
            // fragments "{\"que" + "ry\":\"rust\"}" merged and parsed
            assert_eq!(input, &serde_json::json!({"query": "rust"}));
        }
        other => panic!("expected ToolCall first, got {other:?}"),
    }
    match &events[1] {
        ChatEvent::ToolResult {
            id,
            output,
            is_error,
            ..
        } => {
            assert_eq!(id, "call_1");
            assert_eq!(output, "3 results");
            assert!(!(*is_error));
        }
        other => panic!("expected ToolResult second, got {other:?}"),
    }
    assert!(matches!(&events[2], ChatEvent::Delta { text } if text == "Found it"));
    match &events[3] {
        ChatEvent::Done { usage, stop_reason } => {
            // Done carries the LAST round's usage/stop_reason
            assert_eq!(usage.output_tokens, Some(2));
            assert_eq!(stop_reason.as_deref(), Some("stop"));
        }
        other => panic!("expected Done last, got {other:?}"),
    }

    let recorded = exec.calls.lock().expect("lock").clone();
    assert_eq!(
        recorded,
        vec![(
            "memory_vault_search".to_string(),
            serde_json::json!({"query": "rust"})
        )]
    );

    let reqs = server.received_requests().await.expect("recorded");
    assert_eq!(reqs.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("json");
    assert_eq!(first["tools"][0]["type"], "function");
    assert_eq!(first["tools"][0]["function"]["name"], "memory_vault_search");
    assert_eq!(
        first["tools"][0]["function"]["description"],
        "Search the memory vault"
    );
    assert_eq!(
        first["tools"][0]["function"]["parameters"]["properties"]["query"]["type"],
        "string"
    );
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
    let msgs = second["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 4, "system, user, assistant tool_calls, tool");
    assert_eq!(msgs[2]["role"], "assistant");
    assert!(msgs[2]["content"].is_null());
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[2]["tool_calls"][0]["type"], "function");
    assert_eq!(
        msgs[2]["tool_calls"][0]["function"]["name"],
        "memory_vault_search"
    );
    assert_eq!(
        msgs[2]["tool_calls"][0]["function"]["arguments"],
        "{\"query\":\"rust\"}"
    );
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_1");
    assert_eq!(msgs[3]["content"], "3 results");
    // the follow-up request keeps the same body shape, tools included
    assert_eq!(
        second["tools"][0]["function"]["name"],
        "memory_vault_search"
    );
}

#[tokio::test]
async fn executor_error_becomes_error_tool_result_and_loop_continues() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_call_sse(), Some(1)).await;
    mount_sse(&server, text_sse("Recovered"), None).await;

    let exec = RecordingExecutor::err("vault offline");
    let stream = adapter(&server, None)
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
    assert_eq!(second["messages"][3]["role"], "tool");
    assert_eq!(second["messages"][3]["content"], "vault offline");
}

#[tokio::test]
async fn invalid_tool_arguments_become_error_result_without_executor_call() {
    let server = MockServer::start().await;
    mount_sse(&server, bad_args_sse(), Some(1)).await;
    mount_sse(&server, text_sse("Continued"), None).await;

    let exec = RecordingExecutor::ok("never");
    let stream = adapter(&server, None)
        .stream_chat(
            tool_req(exec.clone(), DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 4, "events: {events:?}");
    match &events[0] {
        ChatEvent::ToolCall { id, input, .. } => {
            assert_eq!(id, "call_9");
            // unparseable arguments surface as the raw string
            assert_eq!(input, &serde_json::json!("{not json"));
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
    match &events[1] {
        ChatEvent::ToolResult {
            output, is_error, ..
        } => {
            assert!(*is_error);
            assert!(
                output.contains("invalid tool arguments"),
                "output: {output}"
            );
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    assert!(matches!(&events[2], ChatEvent::Delta { text } if text == "Continued"));
    assert!(matches!(&events[3], ChatEvent::Done { .. }));
    assert!(
        exec.calls.lock().expect("lock").is_empty(),
        "executor must not run on unparseable arguments"
    );
}

#[tokio::test]
async fn parallel_tool_calls_each_get_a_tool_message() {
    let server = MockServer::start().await;
    mount_sse(&server, parallel_tool_call_sse(), Some(1)).await;
    mount_sse(&server, text_sse("Both done"), None).await;

    let exec = RecordingExecutor::ok("hit");
    let stream = adapter(&server, None)
        .stream_chat(
            tool_req(exec.clone(), DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 6, "events: {events:?}");
    assert!(matches!(&events[0], ChatEvent::ToolCall { id, .. } if id == "call_a"));
    assert!(matches!(&events[1], ChatEvent::ToolResult { id, .. } if id == "call_a"));
    assert!(matches!(&events[2], ChatEvent::ToolCall { id, .. } if id == "call_b"));
    assert!(matches!(&events[3], ChatEvent::ToolResult { id, .. } if id == "call_b"));

    let reqs = server.received_requests().await.expect("recorded");
    let second: serde_json::Value = serde_json::from_slice(&reqs[1].body).expect("json");
    let msgs = second["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 5, "system, user, assistant, tool a, tool b");
    assert_eq!(
        msgs[2]["tool_calls"].as_array().expect("tool_calls").len(),
        2
    );
    assert_eq!(msgs[3]["tool_call_id"], "call_a");
    assert_eq!(msgs[4]["tool_call_id"], "call_b");
}

#[tokio::test]
async fn tools_omitted_when_empty_or_executor_missing() {
    // Empty tools → no "tools" key in the body.
    let server = MockServer::start().await;
    mount_sse(&server, text_sse("Hi"), None).await;
    let stream = adapter(&server, None)
        .stream_chat(req(), CancellationToken::new())
        .await
        .expect("stream");
    collect(stream).await;
    let reqs = server.received_requests().await.expect("recorded");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("json");
    assert!(body.get("tools").is_none(), "body: {body}");

    // Tools present but no executor → tools ignored, no "tools" key.
    let server2 = MockServer::start().await;
    mount_sse(&server2, text_sse("Hi"), None).await;
    let no_exec = ChatRequest {
        tools: vec![ToolDef {
            name: "t".into(),
            description: "d".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        ..req()
    };
    let stream2 = adapter(&server2, None)
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
    mount_sse(&server, tool_call_sse(), None).await;

    let exec = RecordingExecutor::ok("3 results");
    let stream = adapter(&server, None)
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
    mount_sse(&server, tool_call_sse(), None).await;

    let exec = RecordingExecutor::ok("never");
    let stream = adapter(&server, None)
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
    // Reject any body carrying tool definitions, as an endpoint without
    // function-calling support would.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"tools\""))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    mount_sse(&server, text_sse("Plain chat works"), None).await;

    let exec = RecordingExecutor::ok("unused");
    let stream = adapter(&server, None)
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
async fn mid_loop_request_failure_surfaces_as_error_event() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_call_sse(), Some(1)).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let exec = RecordingExecutor::ok("3 results");
    let stream = adapter(&server, None)
        .stream_chat(
            tool_req(exec, DEFAULT_MAX_TOOL_ROUNDS),
            CancellationToken::new(),
        )
        .await
        .expect("stream");
    let events = collect(stream).await;

    assert_eq!(events.len(), 3, "events: {events:?}");
    assert!(matches!(&events[0], ChatEvent::ToolCall { .. }));
    assert!(matches!(&events[1], ChatEvent::ToolResult { .. }));
    assert!(
        matches!(
            &events[2],
            ChatEvent::Error {
                retryable: true,
                ..
            }
        ),
        "expected retryable error, got {:?}",
        events[2]
    );
}

#[tokio::test]
async fn cancel_during_tool_execution_yields_cancelled_error() {
    let server = MockServer::start().await;
    mount_sse(&server, tool_call_sse(), None).await;

    let cancel = CancellationToken::new();
    let mut stream = adapter(&server, None)
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
