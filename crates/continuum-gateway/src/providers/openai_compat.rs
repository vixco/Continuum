//! OpenAI-compatible adapter. Covers LM Studio, Ollama, OpenRouter,
//! OpenAI, DeepSeek, Kimi, GLM, xAI, StepFun, NVIDIA, HF, Gemini-compat,
//! DashScope-compat, and custom endpoints — same wire shape, different
//! base URL. API key is optional (local servers run keyless).
//!
//! When a [`ChatRequest`] carries tools *and* an executor, this adapter
//! also runs the internal function-calling loop: streamed
//! `finish_reason == "tool_calls"` rounds are executed through the
//! [`ToolExecutor`], each result is appended as a `role:"tool"` message,
//! and a fresh streaming request is issued — bounded by
//! [`ChatRequest::tool_max_rounds`]. Endpoints that reject tool
//! definitions with a 4xx get exactly one retry without tools, so plain
//! chat keeps working against servers without function-calling support.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::error::GatewayError;
use crate::providers::{map_status, retry_after};
use crate::sse::SseParser;
use crate::types::*;
use crate::ChatProvider;

/// Talks to any server exposing the OpenAI `/v1/chat/completions` and
/// `/v1/models` wire shape.
pub struct OpenAiCompatAdapter {
    ctx: HttpCtx,
}

/// Shared HTTP plumbing for one connection. Cloned into the tool-loop
/// task so follow-up rounds can issue their own requests after
/// `stream_chat` has returned.
#[derive(Clone)]
struct HttpCtx {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    idle_timeout: Duration,
}

impl HttpCtx {
    /// Attaches the bearer token when this connection has one configured.
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => rb.bearer_auth(k),
            None => rb,
        }
    }

    fn map_send_error(&self, e: reqwest::Error) -> GatewayError {
        if e.is_timeout() {
            GatewayError::Timeout
        } else {
            GatewayError::Unreachable {
                url: self.base_url.clone(),
                source: Some(e),
            }
        }
    }

    /// Sends a request, bounding the whole round-trip (not just the TCP
    /// connect) by `idle_timeout`. Without this, a server that accepts the
    /// connection but never writes a response byte would hang `send()`
    /// forever — `connect_timeout` alone does not cover that case.
    async fn send_with_timeout(
        &self,
        rb: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GatewayError> {
        match tokio::time::timeout(self.idle_timeout, rb.send()).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(self.map_send_error(e)),
            Err(_) => Err(GatewayError::Timeout),
        }
    }

    /// Reads an error response's body for the `BadResponse`/status-mapped
    /// error detail, bounded by `idle_timeout` — a body read can stall the
    /// same way an initial `send()` can, so this must not block forever
    /// either. Any failure (timeout or read error) degrades to an empty
    /// string; the body text is diagnostic-only, never load-bearing.
    async fn read_body_text(&self, resp: reqwest::Response) -> String {
        tokio::time::timeout(self.idle_timeout, resp.text())
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default()
    }

    /// Builds an authenticated `POST /chat/completions` carrying `body`.
    fn chat_request(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.auth(
            self.client
                .post(format!("{}/chat/completions", self.base_url)),
        )
        .json(body)
    }

    /// Turns a non-2xx response into the mapped [`GatewayError`].
    async fn status_error(&self, resp: reqwest::Response) -> GatewayError {
        let ra = retry_after(&resp);
        let status = resp.status();
        map_status(status, ra, self.read_body_text(resp).await)
    }
}

impl OpenAiCompatAdapter {
    /// Builds an adapter. `base_url` should include the `/v1` prefix
    /// (e.g. `http://localhost:1234/v1`); a trailing slash is stripped.
    pub fn new(
        base_url: String,
        api_key: Option<String>,
        connect_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<Self, GatewayError> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .build()
            .map_err(|e| GatewayError::BadResponse {
                detail: e.to_string(),
            })?;
        Ok(Self {
            ctx: HttpCtx {
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key,
                client,
                idle_timeout,
            },
        })
    }
}

#[async_trait::async_trait]
impl ChatProvider for OpenAiCompatAdapter {
    async fn test_connection(&self) -> Result<ConnectionTestReport, GatewayError> {
        let started = std::time::Instant::now();
        let models = self.list_models().await?;
        Ok(ConnectionTestReport {
            ok: true,
            latency_ms: started.elapsed().as_millis() as u64,
            detail: format!("{} models available", models.len()),
            models,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>, GatewayError> {
        let resp = self
            .ctx
            .send_with_timeout(
                self.ctx
                    .auth(self.ctx.client.get(format!("{}/models", self.ctx.base_url))),
            )
            .await?;
        if !resp.status().is_success() {
            return Err(self.ctx.status_error(resp).await);
        }
        #[derive(serde::Deserialize)]
        struct ModelEntry {
            id: String,
        }
        #[derive(serde::Deserialize)]
        struct ModelList {
            data: Vec<ModelEntry>,
        }
        let list: ModelList = match tokio::time::timeout(self.ctx.idle_timeout, resp.json()).await {
            Ok(Ok(list)) => list,
            Ok(Err(e)) => {
                return Err(GatewayError::BadResponse {
                    detail: e.to_string(),
                })
            }
            Err(_) => return Err(GatewayError::Timeout),
        };
        Ok(list.data.into_iter().map(|m| m.id).collect())
    }

    async fn stream_chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, ChatEvent>, GatewayError> {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in &req.messages {
            let role = match m.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
            };
            messages.push(json!({"role": role, "content": m.content}));
        }
        // Tools require an executor to be actionable; without one the body
        // stays byte-identical to the tool-free shape.
        let tools_enabled = !req.tools.is_empty() && req.executor.is_some();
        let tools = tools_enabled.then_some(req.tools.as_slice());
        let body = build_body(
            &req.model,
            &messages,
            req.max_tokens,
            req.temperature,
            tools,
        );
        let mut resp = self
            .ctx
            .send_with_timeout(self.ctx.chat_request(&body))
            .await?;

        // Containment: an endpoint without function-calling support may
        // reject the tool definitions outright. Retry the FIRST request
        // once without tools (same messages) so plain chat still works.
        let mut tools_active = tools_enabled;
        if tools_enabled && resp.status().is_client_error() {
            tracing::warn!(
                layer = "gateway",
                component = "openai_compat",
                status = %resp.status(),
                "endpoint rejected request with tool definitions; retrying once without tools"
            );
            let body = build_body(&req.model, &messages, req.max_tokens, req.temperature, None);
            resp = self
                .ctx
                .send_with_timeout(self.ctx.chat_request(&body))
                .await?;
            tools_active = false;
        }
        if !resp.status().is_success() {
            return Err(self.ctx.status_error(resp).await);
        }

        match (tools_active, req.executor) {
            (true, Some(executor)) => {
                let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
                let tool_loop = ToolLoop {
                    ctx: self.ctx.clone(),
                    model: req.model,
                    max_tokens: req.max_tokens,
                    temperature: req.temperature,
                    tools: req.tools,
                    executor,
                    max_rounds: req.tool_max_rounds,
                    messages,
                    tx,
                    cancel,
                };
                tokio::spawn(tool_loop.run(resp));
                Ok(super::mpsc_stream(rx))
            }
            _ => Ok(stream_events(resp, cancel, self.ctx.idle_timeout)),
        }
    }
}

/// Builds the `/chat/completions` request body. `tools: Some(..)` adds the
/// OpenAI function-calling `tools` array; `None` keeps the body identical
/// to the pre-tool-loop shape.
fn build_body(
    model: &str,
    messages: &[serde_json::Value],
    max_tokens: u32,
    temperature: Option<f32>,
    tools: Option<&[ToolDef]>,
) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "max_tokens": max_tokens,
    });
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    if let Some(tools) = tools {
        body["tools"] = json!(tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect::<Vec<_>>());
    }
    body
}

/// A tool call fully assembled from streamed fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletedToolCall {
    /// Provider-assigned call id; synthesized from the index if absent.
    id: String,
    /// Tool (function) name.
    name: String,
    /// Raw JSON-encoded arguments string, exactly as streamed.
    arguments: String,
}

/// Accumulates streamed `choices[0].delta.tool_calls[]` fragments into
/// complete calls, keyed by fragment `index`. Argument fragments are
/// concatenated; `id`/`name` fragments replace (providers send them whole
/// in the first fragment, and re-sends must not duplicate them).
#[derive(Debug, Default)]
struct ToolCallAccumulator {
    calls: BTreeMap<u64, PartialToolCall>,
}

/// One in-flight tool call being assembled from fragments.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    /// Feeds one `choices[0].delta` object; ignores anything that is not a
    /// well-formed tool-call fragment.
    fn push_delta(&mut self, delta: &serde_json::Value) {
        let Some(fragments) = delta.get("tool_calls").and_then(|t| t.as_array()) else {
            return;
        };
        for frag in fragments {
            let index = frag.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
            let entry = self.calls.entry(index).or_default();
            if let Some(id) = frag.get("id").and_then(|s| s.as_str()) {
                if !id.is_empty() {
                    entry.id = Some(id.to_string());
                }
            }
            if let Some(name) = frag.pointer("/function/name").and_then(|s| s.as_str()) {
                if !name.is_empty() {
                    entry.name = Some(name.to_string());
                }
            }
            if let Some(args) = frag.pointer("/function/arguments").and_then(|s| s.as_str()) {
                entry.arguments.push_str(args);
            }
        }
    }

    /// Returns the completed calls in index order. Calls that never
    /// received a name are dropped (nothing executable to run).
    fn finish(self) -> Vec<CompletedToolCall> {
        self.calls
            .into_iter()
            .filter_map(|(index, partial)| {
                Some(CompletedToolCall {
                    id: partial.id.unwrap_or_else(|| format!("call_{index}")),
                    name: partial.name?,
                    arguments: partial.arguments,
                })
            })
            .collect()
    }
}

/// Parses a tool call's streamed `arguments` JSON. An empty string counts
/// as `{}` — some servers stream no-argument calls that way.
fn parse_arguments(raw: &str) -> Result<serde_json::Value, String> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|e| e.to_string())
}

/// How one streamed response round ended.
enum RoundOutcome {
    /// Terminal round: emit `Done` with this round's usage/stop_reason.
    Finished {
        usage: TokenUsage,
        stop_reason: Option<String>,
    },
    /// The model requested tools. Usage/stop_reason are kept so a caller
    /// without a tool loop can still finish the turn normally.
    ToolCalls {
        calls: Vec<CompletedToolCall>,
        usage: TokenUsage,
        stop_reason: Option<String>,
    },
    /// A stream-level error event was already emitted; end the turn.
    Aborted,
}

/// Emits the standard cancelled-turn error event.
async fn send_cancelled(tx: &Sender<ChatEvent>) {
    let _ = tx
        .send(ChatEvent::Error {
            message: GatewayError::Cancelled.user_message(),
            retryable: false,
        })
        .await;
}

/// Emits a [`GatewayError`] as a stream event — used mid-loop, where the
/// `Result` return of `stream_chat` is no longer available.
async fn send_error(tx: &Sender<ChatEvent>, err: &GatewayError) {
    let _ = tx
        .send(ChatEvent::Error {
            message: err.user_message(),
            retryable: err.retryable(),
        })
        .await;
}

/// Streams one model response: forwards text deltas as
/// [`ChatEvent::Delta`] the moment they arrive, accumulates tool-call
/// fragments, and tracks usage/finish_reason. Stream-level errors are
/// emitted here (returning [`RoundOutcome::Aborted`]); `Done` is never
/// emitted — the caller decides how the round ends the turn.
async fn run_round(
    resp: reqwest::Response,
    tx: &Sender<ChatEvent>,
    cancel: &CancellationToken,
    idle_timeout: Duration,
) -> RoundOutcome {
    let mut parser = SseParser::new();
    let mut bytes = resp.bytes_stream();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<String> = None;
    let mut acc = ToolCallAccumulator::default();
    loop {
        // Eager check: if the token was already cancelled before this
        // iteration started, don't race it against `bytes.next()` in
        // the `select!` below — a fully-buffered response (small SSE
        // bodies typically arrive in one TCP read) can make that read
        // resolve synchronously too, which would non-deterministically
        // let a stale chunk win over an already-requested cancellation.
        if cancel.is_cancelled() {
            send_cancelled(tx).await;
            return RoundOutcome::Aborted;
        }
        let chunk = tokio::select! {
            _ = cancel.cancelled() => {
                send_cancelled(tx).await;
                return RoundOutcome::Aborted;
            }
            res = tokio::time::timeout(idle_timeout, bytes.next()) => match res {
                Err(_) => {
                    send_error(tx, &GatewayError::Timeout).await;
                    return RoundOutcome::Aborted;
                }
                Ok(None) => break, // connection closed without [DONE] — finish with what we have
                Ok(Some(Err(e))) => {
                    let _ = tx.send(ChatEvent::Error { message: format!("Stream error: {e}"), retryable: true }).await;
                    return RoundOutcome::Aborted;
                }
                Ok(Some(Ok(c))) => c,
            }
        };
        for payload in parser.push(&chunk) {
            if payload == "[DONE]" {
                return finish_round(acc, usage, stop_reason);
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            if let Some(u) = v.get("usage") {
                usage.input_tokens = u.get("prompt_tokens").and_then(|x| x.as_u64());
                usage.output_tokens = u.get("completion_tokens").and_then(|x| x.as_u64());
            }
            if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    stop_reason = Some(fr.to_string());
                }
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            let _ = tx
                                .send(ChatEvent::Delta {
                                    text: text.to_string(),
                                })
                                .await;
                        }
                    }
                    acc.push_delta(delta);
                }
            }
        }
    }
    finish_round(acc, usage, stop_reason)
}

/// Decides how a completed response ends the round: a `tool_calls` finish
/// with at least one fully-assembled call hands control to the tool loop;
/// anything else finishes the turn.
fn finish_round(
    acc: ToolCallAccumulator,
    usage: TokenUsage,
    stop_reason: Option<String>,
) -> RoundOutcome {
    if stop_reason.as_deref() == Some("tool_calls") {
        let calls = acc.finish();
        if !calls.is_empty() {
            return RoundOutcome::ToolCalls {
                calls,
                usage,
                stop_reason,
            };
        }
    }
    RoundOutcome::Finished { usage, stop_reason }
}

/// Turns a single SSE response into ChatEvents (no tool loop). Ends after
/// Done/Error. A stray `tool_calls` finish degrades to `Done`, exactly as
/// before the tool loop existed.
fn stream_events(
    resp: reqwest::Response,
    cancel: CancellationToken,
    idle_timeout: Duration,
) -> BoxStream<'static, ChatEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
    tokio::spawn(async move {
        match run_round(resp, &tx, &cancel, idle_timeout).await {
            RoundOutcome::Aborted => {}
            RoundOutcome::Finished { usage, stop_reason }
            | RoundOutcome::ToolCalls {
                usage, stop_reason, ..
            } => {
                let _ = tx.send(ChatEvent::Done { usage, stop_reason }).await;
            }
        }
    });
    super::mpsc_stream(rx)
}

/// One chat turn's function-calling loop: streams rounds, executes
/// `tool_calls` finishes through the [`ToolExecutor`], extends the
/// conversation with the OpenAI-shape assistant/tool messages, and issues
/// follow-up requests until the model finishes with text, an error
/// surfaces, `cancel` fires, or `max_rounds` is exhausted.
struct ToolLoop {
    ctx: HttpCtx,
    model: String,
    max_tokens: u32,
    temperature: Option<f32>,
    tools: Vec<ToolDef>,
    executor: Arc<dyn ToolExecutor>,
    max_rounds: u32,
    messages: Vec<serde_json::Value>,
    tx: Sender<ChatEvent>,
    cancel: CancellationToken,
}

impl ToolLoop {
    /// Drives the loop starting from the already-successful first-round
    /// response. `Done` fires exactly once, with the LAST round's
    /// usage/stop_reason; text deltas from every round stream out as they
    /// arrive.
    async fn run(mut self, first_resp: reqwest::Response) {
        let mut resp = first_resp;
        let mut rounds_used: u32 = 0;
        loop {
            match run_round(resp, &self.tx, &self.cancel, self.ctx.idle_timeout).await {
                RoundOutcome::Aborted => return,
                RoundOutcome::Finished { usage, stop_reason } => {
                    let _ = self.tx.send(ChatEvent::Done { usage, stop_reason }).await;
                    return;
                }
                RoundOutcome::ToolCalls { calls, .. } => {
                    if self.tx.is_closed() {
                        // Consumer dropped the stream — don't keep issuing
                        // HTTP requests nobody will see.
                        return;
                    }
                    if rounds_used >= self.max_rounds {
                        let _ = self
                            .tx
                            .send(ChatEvent::Error {
                                message: "Tool-call limit reached".into(),
                                retryable: false,
                            })
                            .await;
                        return;
                    }
                    rounds_used += 1;
                    if !self.execute_round(&calls).await {
                        return; // cancelled; event already emitted
                    }
                    let body = build_body(
                        &self.model,
                        &self.messages,
                        self.max_tokens,
                        self.temperature,
                        Some(&self.tools),
                    );
                    let next = tokio::select! {
                        _ = self.cancel.cancelled() => {
                            send_cancelled(&self.tx).await;
                            return;
                        }
                        res = self.ctx.send_with_timeout(self.ctx.chat_request(&body)) => res,
                    };
                    match next {
                        Ok(r) if r.status().is_success() => resp = r,
                        Ok(r) => {
                            send_error(&self.tx, &self.ctx.status_error(r).await).await;
                            return;
                        }
                        Err(e) => {
                            send_error(&self.tx, &e).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Executes one round's calls in order: emits [`ChatEvent::ToolCall`]
    /// and [`ChatEvent::ToolResult`] per call, then appends one assistant
    /// message carrying the `tool_calls` array (content null) and one
    /// `role:"tool"` message per call. Unparseable arguments and executor
    /// `Err`s become error tool results — never a crash. Returns `false`
    /// when cancelled (the cancelled error event is already emitted).
    async fn execute_round(&mut self, calls: &[CompletedToolCall]) -> bool {
        let mut call_specs = Vec::with_capacity(calls.len());
        let mut tool_messages = Vec::with_capacity(calls.len());
        for call in calls {
            if self.cancel.is_cancelled() {
                send_cancelled(&self.tx).await;
                return false;
            }
            let parsed = parse_arguments(&call.arguments);
            let event_input = match &parsed {
                Ok(v) => v.clone(),
                // Surface the raw string so the dashboard can show what
                // the model actually produced.
                Err(_) => serde_json::Value::String(call.arguments.clone()),
            };
            let _ = self
                .tx
                .send(ChatEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: event_input,
                })
                .await;
            let started = Instant::now();
            let result = match parsed {
                Ok(input) => {
                    tokio::select! {
                        _ = self.cancel.cancelled() => {
                            send_cancelled(&self.tx).await;
                            return false;
                        }
                        res = self.executor.execute(&call.name, &input) => res,
                    }
                }
                Err(e) => Err(format!("invalid tool arguments: {e}")),
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            let (output, is_error) = match result {
                Ok(o) => (o, false),
                Err(e) => (e, true),
            };
            let _ = self
                .tx
                .send(ChatEvent::ToolResult {
                    id: call.id.clone(),
                    output: output.clone(),
                    is_error,
                    duration_ms,
                })
                .await;
            call_specs.push(json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments},
            }));
            tool_messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": output,
            }));
        }
        self.messages.push(json!({
            "role": "assistant",
            "content": serde_json::Value::Null,
            "tool_calls": call_specs,
        }));
        self.messages.extend(tool_messages);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_merges_split_arguments_across_deltas() {
        let mut acc = ToolCallAccumulator::default();
        acc.push_delta(&json!({"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"search","arguments":"{\"q\":"}}
        ]}));
        acc.push_delta(&json!({"tool_calls":[
            {"index":0,"function":{"arguments":"\"rust\"}"}}
        ]}));
        assert_eq!(
            acc.finish(),
            vec![CompletedToolCall {
                id: "call_1".into(),
                name: "search".into(),
                arguments: "{\"q\":\"rust\"}".into(),
            }]
        );
    }

    #[test]
    fn accumulator_keys_parallel_calls_by_index() {
        let mut acc = ToolCallAccumulator::default();
        // interleaved fragments for two calls
        acc.push_delta(&json!({"tool_calls":[
            {"index":1,"id":"call_b","function":{"name":"b","arguments":"{\"x\":"}}
        ]}));
        acc.push_delta(&json!({"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"a","arguments":"{}"}}
        ]}));
        acc.push_delta(&json!({"tool_calls":[
            {"index":1,"function":{"arguments":"2}"}}
        ]}));
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        // index order, regardless of arrival order
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].arguments, "{\"x\":2}");
    }

    #[test]
    fn accumulator_synthesizes_missing_id_and_drops_nameless_calls() {
        let mut acc = ToolCallAccumulator::default();
        acc.push_delta(&json!({"tool_calls":[
            {"index":0,"function":{"name":"search","arguments":"{}"}},
            {"index":1,"function":{"arguments":"{\"orphan\":true}"}}
        ]}));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1, "nameless call must be dropped");
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].name, "search");
    }

    #[test]
    fn accumulator_ignores_deltas_without_tool_calls() {
        let mut acc = ToolCallAccumulator::default();
        acc.push_delta(&json!({"content": "plain text"}));
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn parse_arguments_treats_empty_as_object_and_rejects_garbage() {
        assert_eq!(parse_arguments("").expect("empty"), json!({}));
        assert_eq!(parse_arguments("  ").expect("blank"), json!({}));
        assert_eq!(
            parse_arguments("{\"q\":1}").expect("valid"),
            json!({"q": 1})
        );
        assert!(parse_arguments("{not json").is_err());
    }

    #[test]
    fn build_body_includes_tools_only_when_given() {
        let messages = vec![json!({"role": "system", "content": "s"})];
        let plain = build_body("m", &messages, 64, Some(0.5), None);
        assert!(plain.get("tools").is_none());
        assert_eq!(plain["model"], "m");
        assert_eq!(plain["stream"], true);
        assert_eq!(plain["max_tokens"], 64);
        assert_eq!(plain["temperature"], 0.5);

        let tools = vec![ToolDef {
            name: "search".into(),
            description: "find things".into(),
            input_schema: json!({"type": "object"}),
        }];
        let with_tools = build_body("m", &messages, 64, None, Some(&tools));
        assert!(with_tools.get("temperature").is_none());
        assert_eq!(with_tools["tools"][0]["type"], "function");
        assert_eq!(with_tools["tools"][0]["function"]["name"], "search");
        assert_eq!(
            with_tools["tools"][0]["function"]["description"],
            "find things"
        );
        assert_eq!(
            with_tools["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn finish_round_requires_tool_calls_finish_reason_and_calls() {
        // tool_calls finish with no accumulated calls degrades to Finished
        let outcome = finish_round(
            ToolCallAccumulator::default(),
            TokenUsage::default(),
            Some("tool_calls".into()),
        );
        assert!(matches!(outcome, RoundOutcome::Finished { .. }));

        // accumulated calls with a plain stop finish also stay Finished
        let mut acc = ToolCallAccumulator::default();
        acc.push_delta(&json!({"tool_calls":[
            {"index":0,"id":"c","function":{"name":"n","arguments":"{}"}}
        ]}));
        let outcome = finish_round(acc, TokenUsage::default(), Some("stop".into()));
        assert!(matches!(outcome, RoundOutcome::Finished { .. }));

        // both together hand control to the loop
        let mut acc = ToolCallAccumulator::default();
        acc.push_delta(&json!({"tool_calls":[
            {"index":0,"id":"c","function":{"name":"n","arguments":"{}"}}
        ]}));
        let outcome = finish_round(acc, TokenUsage::default(), Some("tool_calls".into()));
        assert!(matches!(outcome, RoundOutcome::ToolCalls { .. }));
    }
}
