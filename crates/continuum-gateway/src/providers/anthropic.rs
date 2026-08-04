//! Anthropic Messages API adapter (API-key auth). Subscription users go
//! through the ClaudeCliAdapter instead — never OAuth (non-negotiable #1).
//!
//! When a [`ChatRequest`] carries tools *and* an executor, this adapter
//! also runs the internal tool_use loop: streamed `stop_reason ==
//! "tool_use"` rounds are executed through the [`ToolExecutor`], each
//! result goes back to the model as a `tool_result` user message, and a
//! fresh streaming request is issued — bounded by
//! [`ChatRequest::tool_max_rounds`]. Endpoints that reject tool
//! definitions with a 4xx get exactly one retry without tools, so plain
//! chat keeps working against proxies without tool support. The loop
//! mirrors the OpenAI-compat adapter's — same semantics, Anthropic wire
//! shapes.

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

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Talks to the Anthropic `/v1/messages` and `/v1/models` API using an
/// API key (`x-api-key` header). Never sends `temperature` — rejected by
/// the newest models when combined with other sampling params here.
pub struct AnthropicAdapter {
    ctx: HttpCtx,
}

/// Shared HTTP plumbing for one connection. Cloned into the tool-loop
/// task so follow-up rounds can issue their own requests after
/// `stream_chat` has returned.
#[derive(Clone)]
struct HttpCtx {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
    idle_timeout: Duration,
}

impl HttpCtx {
    /// Attaches the API key and required version header. Never logged —
    /// the key only ever lives inside this header value.
    fn headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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

    /// Builds an authenticated `POST /v1/messages` carrying `body`.
    fn chat_request(&self, body: &serde_json::Value) -> reqwest::RequestBuilder {
        self.headers(self.client.post(format!("{}/v1/messages", self.base_url)))
            .json(body)
    }

    /// Turns a non-2xx response into the mapped [`GatewayError`].
    async fn status_error(&self, resp: reqwest::Response) -> GatewayError {
        let ra = retry_after(&resp);
        let status = resp.status();
        map_status(status, ra, self.read_body_text(resp).await)
    }
}

impl AnthropicAdapter {
    /// Builds an adapter. `base_url` defaults to `https://api.anthropic.com`
    /// in production but is overridable (e.g. for tests); a trailing slash
    /// is stripped.
    pub fn new(
        base_url: String,
        api_key: String,
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
impl ChatProvider for AnthropicAdapter {
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
                self.ctx.headers(
                    self.ctx
                        .client
                        .get(format!("{}/v1/models", self.ctx.base_url)),
                ),
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
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                };
                json!({"role": role, "content": m.content})
            })
            .collect();
        // Tools require an executor to be actionable; without one the body
        // stays byte-identical to the tool-free shape.
        let tools_enabled = !req.tools.is_empty() && req.executor.is_some();
        let tools = tools_enabled.then_some(req.tools.as_slice());
        let body = build_body(&req.model, &req.system, &messages, req.max_tokens, tools);
        let mut resp = self
            .ctx
            .send_with_timeout(self.ctx.chat_request(&body))
            .await?;

        // Containment: a proxy or gateway without tool support may reject
        // the tool definitions outright. Retry the FIRST request once
        // without tools (same messages) so plain chat still works.
        let mut tools_active = tools_enabled;
        if tools_enabled && resp.status().is_client_error() {
            tracing::warn!(
                layer = "gateway",
                component = "anthropic",
                status = %resp.status(),
                "endpoint rejected request with tool definitions; retrying once without tools"
            );
            let body = build_body(&req.model, &req.system, &messages, req.max_tokens, None);
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
                    system: req.system,
                    max_tokens: req.max_tokens,
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

/// Builds the `/v1/messages` request body. `tools: Some(..)` adds the
/// Anthropic `tools` array (`name`/`description`/`input_schema`, no
/// wrapper object); `None` keeps the body identical to the pre-tool-loop
/// shape. Never includes `temperature` — the Anthropic Messages API
/// rejects it on the newest models when combined with these other params.
fn build_body(
    model: &str,
    system: &str,
    messages: &[serde_json::Value],
    max_tokens: u32,
    tools: Option<&[ToolDef]>,
) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "stream": true,
        "messages": messages,
    });
    if let Some(tools) = tools {
        body["tools"] = json!(tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect::<Vec<_>>());
    }
    body
}

/// A `tool_use` content block fully assembled from streamed events.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletedToolUse {
    /// Provider-assigned block id; synthesized from the index if absent.
    id: String,
    /// Tool name.
    name: String,
    /// Raw JSON input accumulated from `input_json_delta` fragments,
    /// exactly as streamed. Empty when the block streamed no fragments
    /// (a no-argument call — parses as `{}`).
    input_json: String,
}

/// Accumulates streamed `tool_use` content blocks into complete calls,
/// keyed by content-block `index`. A `content_block_start` opens a block
/// with its `id`/`name`; `input_json_delta` fragments append to its raw
/// input buffer; the block is complete at `content_block_stop` (no state
/// change needed — it simply stops growing) and is drained in index
/// order by [`finish`](Self::finish).
#[derive(Debug, Default)]
struct ToolUseAccumulator {
    blocks: BTreeMap<u64, PartialToolUse>,
}

/// One in-flight `tool_use` block being assembled from events.
#[derive(Debug, Default)]
struct PartialToolUse {
    id: Option<String>,
    name: Option<String>,
    input_json: String,
}

impl ToolUseAccumulator {
    /// Feeds a `content_block_start` event; ignored unless it opens a
    /// `tool_use` block.
    fn push_block_start(&mut self, v: &serde_json::Value) {
        if v.pointer("/content_block/type").and_then(|t| t.as_str()) != Some("tool_use") {
            return;
        }
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        let entry = self.blocks.entry(index).or_default();
        if let Some(id) = v.pointer("/content_block/id").and_then(|s| s.as_str()) {
            if !id.is_empty() {
                entry.id = Some(id.to_string());
            }
        }
        if let Some(name) = v.pointer("/content_block/name").and_then(|s| s.as_str()) {
            if !name.is_empty() {
                entry.name = Some(name.to_string());
            }
        }
    }

    /// Feeds a `content_block_delta` event; ignored unless it carries an
    /// `input_json_delta`. A fragment for an index that never got a
    /// `content_block_start` accumulates into a nameless block, which
    /// [`finish`](Self::finish) drops.
    fn push_delta(&mut self, v: &serde_json::Value) {
        if v.pointer("/delta/type").and_then(|t| t.as_str()) != Some("input_json_delta") {
            return;
        }
        let Some(fragment) = v.pointer("/delta/partial_json").and_then(|s| s.as_str()) else {
            return;
        };
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
        self.blocks
            .entry(index)
            .or_default()
            .input_json
            .push_str(fragment);
    }

    /// Returns the completed calls in index order. Blocks that never
    /// received a name are dropped (nothing executable to run).
    fn finish(self) -> Vec<CompletedToolUse> {
        self.blocks
            .into_iter()
            .filter_map(|(index, partial)| {
                Some(CompletedToolUse {
                    id: partial.id.unwrap_or_else(|| format!("toolu_{index}")),
                    name: partial.name?,
                    input_json: partial.input_json,
                })
            })
            .collect()
    }
}

/// Parses a `tool_use` block's accumulated input JSON. An empty string
/// counts as `{}` — a no-argument call streams zero `input_json_delta`
/// fragments.
fn parse_input(raw: &str) -> Result<serde_json::Value, String> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|e| e.to_string())
}

/// How one streamed response round ended.
enum RoundOutcome {
    /// Terminal round: finish the turn with this round's usage/stop_reason.
    Finished {
        usage: TokenUsage,
        stop_reason: Option<String>,
    },
    /// The model requested tools. `text` carries this round's streamed
    /// text so the follow-up assistant message can replay it; usage and
    /// stop_reason are kept so a caller without a tool loop can still
    /// finish the turn normally.
    ToolCalls {
        calls: Vec<CompletedToolUse>,
        text: String,
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

/// Ends the turn after a terminal round: a `refusal` stop_reason
/// surfaces as a non-retryable error (exactly as the pre-tool-loop
/// adapter did at `message_stop`); anything else emits `Done`.
async fn finish_turn(tx: &Sender<ChatEvent>, usage: TokenUsage, stop_reason: Option<String>) {
    if stop_reason.as_deref() == Some("refusal") {
        let _ = tx
            .send(ChatEvent::Error {
                message:
                    "The model declined this request (safety refusal). Rephrase or try another model."
                        .into(),
                retryable: false,
            })
            .await;
    } else {
        let _ = tx.send(ChatEvent::Done { usage, stop_reason }).await;
    }
}

/// Streams one model response: forwards `text_delta`s as
/// [`ChatEvent::Delta`] the moment they arrive, accumulates `tool_use`
/// blocks and their `input_json_delta` fragments, and tracks
/// usage/stop_reason. Stream-level errors are emitted here (returning
/// [`RoundOutcome::Aborted`]); `Done` is never emitted — the caller
/// decides how the round ends the turn.
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
    let mut acc = ToolUseAccumulator::default();
    let mut text = String::new();
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
                Ok(None) => break, // connection closed early — finish with what we have
                Ok(Some(Err(e))) => {
                    let _ = tx.send(ChatEvent::Error { message: format!("Stream error: {e}"), retryable: true }).await;
                    return RoundOutcome::Aborted;
                }
                Ok(Some(Ok(c))) => c,
            }
        };
        for payload in parser.push(&chunk) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("message_start") => {
                    if let Some(t) = v
                        .pointer("/message/usage/input_tokens")
                        .and_then(|x| x.as_u64())
                    {
                        usage.input_tokens = Some(t);
                    }
                }
                Some("content_block_start") => acc.push_block_start(&v),
                Some("content_block_delta") => {
                    if v.pointer("/delta/type").and_then(|t| t.as_str()) == Some("text_delta") {
                        if let Some(t) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                            let _ = tx
                                .send(ChatEvent::Delta {
                                    text: t.to_string(),
                                })
                                .await;
                        }
                    } else {
                        acc.push_delta(&v);
                    }
                }
                Some("message_delta") => {
                    if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                        stop_reason = Some(sr.to_string());
                    }
                    if let Some(t) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                        usage.output_tokens = Some(t);
                    }
                }
                Some("message_stop") => return finish_round(acc, text, usage, stop_reason),
                Some("error") => {
                    let detail = v
                        .pointer("/error/message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: format!("Anthropic API error: {detail}"),
                            retryable: true,
                        })
                        .await;
                    return RoundOutcome::Aborted;
                }
                // content_block_stop needs no handling: the block's
                // accumulator entry is already complete and just stops
                // growing.
                _ => {}
            }
        }
    }
    finish_round(acc, text, usage, stop_reason)
}

/// Decides how a completed response ends the round: a `tool_use` stop
/// with at least one fully-assembled block hands control to the tool
/// loop; anything else finishes the turn.
fn finish_round(
    acc: ToolUseAccumulator,
    text: String,
    usage: TokenUsage,
    stop_reason: Option<String>,
) -> RoundOutcome {
    if stop_reason.as_deref() == Some("tool_use") {
        let calls = acc.finish();
        if !calls.is_empty() {
            return RoundOutcome::ToolCalls {
                calls,
                text,
                usage,
                stop_reason,
            };
        }
    }
    RoundOutcome::Finished { usage, stop_reason }
}

/// Turns a single SSE response into ChatEvents (no tool loop). Ends after
/// Done/Error. A stray `tool_use` stop degrades to a normal turn end,
/// exactly as before the tool loop existed.
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
                finish_turn(&tx, usage, stop_reason).await;
            }
        }
    });
    super::mpsc_stream(rx)
}

/// One chat turn's tool_use loop: streams rounds, executes `tool_use`
/// stops through the [`ToolExecutor`], extends the conversation with the
/// Anthropic-shape assistant/`tool_result` messages, and issues
/// follow-up requests until the model finishes with text, an error
/// surfaces, `cancel` fires, or `max_rounds` is exhausted.
struct ToolLoop {
    ctx: HttpCtx,
    model: String,
    system: String,
    max_tokens: u32,
    tools: Vec<ToolDef>,
    executor: Arc<dyn ToolExecutor>,
    max_rounds: u32,
    messages: Vec<serde_json::Value>,
    tx: Sender<ChatEvent>,
    cancel: CancellationToken,
}

impl ToolLoop {
    /// Drives the loop starting from the already-successful first-round
    /// response. The turn ends exactly once — `Done` (or the refusal
    /// error) fires at the FINAL round's `message_stop` with that round's
    /// usage/stop_reason; text deltas from every round stream out as they
    /// arrive.
    async fn run(mut self, first_resp: reqwest::Response) {
        let mut resp = first_resp;
        let mut rounds_used: u32 = 0;
        loop {
            match run_round(resp, &self.tx, &self.cancel, self.ctx.idle_timeout).await {
                RoundOutcome::Aborted => return,
                RoundOutcome::Finished { usage, stop_reason } => {
                    finish_turn(&self.tx, usage, stop_reason).await;
                    return;
                }
                RoundOutcome::ToolCalls { calls, text, .. } => {
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
                    if !self.execute_round(&calls, &text).await {
                        return; // cancelled; event already emitted
                    }
                    let body = build_body(
                        &self.model,
                        &self.system,
                        &self.messages,
                        self.max_tokens,
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

    /// Executes one round's `tool_use` blocks in order: emits
    /// [`ChatEvent::ToolCall`] and [`ChatEvent::ToolResult`] per block,
    /// then appends one assistant message replaying the round's content
    /// (a leading text block only when the round streamed text, then the
    /// `tool_use` blocks in index order) and one user message carrying
    /// every `tool_result` block in the same order. Unparseable input and
    /// executor `Err`s become error tool results — never a crash. Returns
    /// `false` when cancelled (the cancelled error event is already
    /// emitted).
    async fn execute_round(&mut self, calls: &[CompletedToolUse], round_text: &str) -> bool {
        let mut assistant_content = Vec::with_capacity(calls.len() + 1);
        if !round_text.is_empty() {
            assistant_content.push(json!({"type": "text", "text": round_text}));
        }
        let mut result_blocks = Vec::with_capacity(calls.len());
        for call in calls {
            if self.cancel.is_cancelled() {
                send_cancelled(&self.tx).await;
                return false;
            }
            let parsed = parse_input(&call.input_json);
            let event_input = match &parsed {
                Ok(v) => v.clone(),
                // Surface the raw string so the dashboard can show what
                // the model actually produced.
                Err(_) => serde_json::Value::String(call.input_json.clone()),
            };
            let _ = self
                .tx
                .send(ChatEvent::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: event_input,
                })
                .await;
            // The replayed `tool_use` block must carry an object `input`;
            // an unparseable buffer degrades to `{}` on the wire (the
            // error tool_result tells the model what went wrong).
            let wire_input = match &parsed {
                Ok(v) => v.clone(),
                Err(_) => json!({}),
            };
            assistant_content.push(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": wire_input,
            }));
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
            result_blocks.push(json!({
                "type": "tool_result",
                "tool_use_id": call.id,
                "content": output,
                "is_error": is_error,
            }));
        }
        self.messages
            .push(json!({"role": "assistant", "content": assistant_content}));
        self.messages
            .push(json!({"role": "user", "content": result_blocks}));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_merges_input_json_fragments_across_deltas() {
        let mut acc = ToolUseAccumulator::default();
        acc.push_block_start(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_1","name":"search","input":{}}}));
        acc.push_delta(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}));
        acc.push_delta(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}));
        assert_eq!(
            acc.finish(),
            vec![CompletedToolUse {
                id: "toolu_1".into(),
                name: "search".into(),
                input_json: "{\"q\":\"rust\"}".into(),
            }]
        );
    }

    #[test]
    fn accumulator_keys_parallel_blocks_by_index() {
        let mut acc = ToolUseAccumulator::default();
        acc.push_block_start(&json!({"type":"content_block_start","index":2,
            "content_block":{"type":"tool_use","id":"toolu_b","name":"b","input":{}}}));
        acc.push_block_start(&json!({"type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"toolu_a","name":"a","input":{}}}));
        acc.push_delta(&json!({"type":"content_block_delta","index":2,
            "delta":{"type":"input_json_delta","partial_json":"{\"x\":2}"}}));
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        // index order, regardless of arrival order
        assert_eq!(calls[0].id, "toolu_a");
        assert_eq!(calls[0].input_json, "");
        assert_eq!(calls[1].id, "toolu_b");
        assert_eq!(calls[1].input_json, "{\"x\":2}");
    }

    #[test]
    fn accumulator_ignores_non_tool_blocks_and_drops_nameless_ones() {
        let mut acc = ToolUseAccumulator::default();
        // text block start must not open an accumulator entry
        acc.push_block_start(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"text","text":""}}));
        // text delta must not accumulate input
        acc.push_delta(&json!({"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"hello"}}));
        // an orphan fragment with no block start has no name → dropped
        acc.push_delta(&json!({"type":"content_block_delta","index":3,
            "delta":{"type":"input_json_delta","partial_json":"{\"orphan\":true}"}}));
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn accumulator_synthesizes_missing_id() {
        let mut acc = ToolUseAccumulator::default();
        acc.push_block_start(&json!({"type":"content_block_start","index":4,
            "content_block":{"type":"tool_use","name":"search","input":{}}}));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_4");
        assert_eq!(calls[0].name, "search");
    }

    #[test]
    fn parse_input_treats_empty_as_object_and_rejects_garbage() {
        assert_eq!(parse_input("").expect("empty"), json!({}));
        assert_eq!(parse_input("  ").expect("blank"), json!({}));
        assert_eq!(parse_input("{\"q\":1}").expect("valid"), json!({"q": 1}));
        assert!(parse_input("{not json").is_err());
    }

    #[test]
    fn build_body_includes_tools_only_when_given_and_never_temperature() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let plain = build_body("m", "sys", &messages, 64, None);
        assert!(plain.get("tools").is_none());
        assert!(plain.get("temperature").is_none());
        assert_eq!(plain["model"], "m");
        assert_eq!(plain["system"], "sys");
        assert_eq!(plain["stream"], true);
        assert_eq!(plain["max_tokens"], 64);

        let tools = vec![ToolDef {
            name: "search".into(),
            description: "find things".into(),
            input_schema: json!({"type": "object"}),
        }];
        let with_tools = build_body("m", "sys", &messages, 64, Some(&tools));
        assert!(with_tools.get("temperature").is_none());
        // Anthropic wire shape: bare objects, no {"type":"function"} wrapper
        assert_eq!(
            with_tools["tools"][0],
            json!({
                "name": "search",
                "description": "find things",
                "input_schema": {"type": "object"}
            })
        );
    }

    #[test]
    fn finish_round_requires_tool_use_stop_reason_and_calls() {
        // tool_use stop with no accumulated blocks degrades to Finished
        let outcome = finish_round(
            ToolUseAccumulator::default(),
            String::new(),
            TokenUsage::default(),
            Some("tool_use".into()),
        );
        assert!(matches!(outcome, RoundOutcome::Finished { .. }));

        // accumulated blocks with a plain end_turn stop also stay Finished
        let mut acc = ToolUseAccumulator::default();
        acc.push_block_start(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_1","name":"n","input":{}}}));
        let outcome = finish_round(
            acc,
            String::new(),
            TokenUsage::default(),
            Some("end_turn".into()),
        );
        assert!(matches!(outcome, RoundOutcome::Finished { .. }));

        // both together hand control to the loop
        let mut acc = ToolUseAccumulator::default();
        acc.push_block_start(&json!({"type":"content_block_start","index":0,
            "content_block":{"type":"tool_use","id":"toolu_1","name":"n","input":{}}}));
        let outcome = finish_round(
            acc,
            "so far".into(),
            TokenUsage::default(),
            Some("tool_use".into()),
        );
        match outcome {
            RoundOutcome::ToolCalls { calls, text, .. } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(text, "so far");
            }
            _ => panic!("expected ToolCalls"),
        }
    }
}
