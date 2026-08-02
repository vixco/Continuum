//! Anthropic Messages API adapter (API-key auth). Subscription users go
//! through the ClaudeCliAdapter instead — never OAuth (non-negotiable #1).

use std::time::Duration;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::json;
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
    base_url: String,
    api_key: String,
    client: reqwest::Client,
    idle_timeout: Duration,
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
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
            idle_timeout,
        })
    }

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
            .send_with_timeout(
                self.headers(self.client.get(format!("{}/v1/models", self.base_url))),
            )
            .await?;
        if !resp.status().is_success() {
            let ra = retry_after(&resp);
            let status = resp.status();
            return Err(map_status(status, ra, self.read_body_text(resp).await));
        }
        #[derive(serde::Deserialize)]
        struct ModelEntry {
            id: String,
        }
        #[derive(serde::Deserialize)]
        struct ModelList {
            data: Vec<ModelEntry>,
        }
        let list: ModelList = match tokio::time::timeout(self.idle_timeout, resp.json()).await {
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
        // Note: no `temperature` — the Anthropic Messages API rejects it
        // on the newest models when combined with these other params.
        let body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "system": req.system,
            "stream": true,
            "messages": messages,
        });
        let rb = self
            .headers(self.client.post(format!("{}/v1/messages", self.base_url)))
            .json(&body);
        let resp = self.send_with_timeout(rb).await?;
        if !resp.status().is_success() {
            let ra = retry_after(&resp);
            let status = resp.status();
            return Err(map_status(status, ra, self.read_body_text(resp).await));
        }

        let idle = self.idle_timeout;
        let stream = stream_events(resp, cancel, idle);
        Ok(stream)
    }
}

/// Turns the SSE byte stream into ChatEvents. Ends after Done/Error.
fn stream_events(
    resp: reqwest::Response,
    cancel: CancellationToken,
    idle_timeout: Duration,
) -> BoxStream<'static, ChatEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
    tokio::spawn(async move {
        let mut parser = SseParser::new();
        let mut bytes = resp.bytes_stream();
        let mut usage = TokenUsage::default();
        let mut stop_reason: Option<String> = None;
        loop {
            // Eager check: if the token was already cancelled before this
            // iteration started, don't race it against `bytes.next()` in
            // the `select!` below — a fully-buffered response (small SSE
            // bodies typically arrive in one TCP read) can make that read
            // resolve synchronously too, which would non-deterministically
            // let a stale chunk win over an already-requested cancellation.
            if cancel.is_cancelled() {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: GatewayError::Cancelled.user_message(),
                        retryable: false,
                    })
                    .await;
                return;
            }
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tx.send(ChatEvent::Error { message: GatewayError::Cancelled.user_message(), retryable: false }).await;
                    return;
                }
                res = tokio::time::timeout(idle_timeout, bytes.next()) => match res {
                    Err(_) => {
                        let _ = tx.send(ChatEvent::Error { message: GatewayError::Timeout.user_message(), retryable: true }).await;
                        return;
                    }
                    Ok(None) => break, // connection closed early — finish with what we have
                    Ok(Some(Err(e))) => {
                        let _ = tx.send(ChatEvent::Error { message: format!("Stream error: {e}"), retryable: true }).await;
                        return;
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
                    Some("content_block_delta") => {
                        if v.pointer("/delta/type").and_then(|t| t.as_str()) == Some("text_delta") {
                            if let Some(text) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                                let _ = tx
                                    .send(ChatEvent::Delta {
                                        text: text.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                    Some("message_delta") => {
                        if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                            stop_reason = Some(sr.to_string());
                        }
                        if let Some(t) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64())
                        {
                            usage.output_tokens = Some(t);
                        }
                    }
                    Some("message_stop") => {
                        if stop_reason.as_deref() == Some("refusal") {
                            let _ = tx
                                .send(ChatEvent::Error {
                                    message: "The model declined this request (safety refusal). Rephrase or try another model.".into(),
                                    retryable: false,
                                })
                                .await;
                        } else {
                            let _ = tx
                                .send(ChatEvent::Done {
                                    usage: usage.clone(),
                                    stop_reason: stop_reason.clone(),
                                })
                                .await;
                        }
                        return;
                    }
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
                        return;
                    }
                    _ => {}
                }
            }
        }
        let _ = tx.send(ChatEvent::Done { usage, stop_reason }).await;
    });
    super::mpsc_stream(rx)
}
