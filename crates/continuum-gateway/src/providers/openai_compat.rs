//! OpenAI-compatible adapter. Covers LM Studio, Ollama, OpenRouter,
//! OpenAI, DeepSeek, Kimi, GLM, xAI, StepFun, NVIDIA, HF, Gemini-compat,
//! DashScope-compat, and custom endpoints — same wire shape, different
//! base URL. API key is optional (local servers run keyless).

use std::time::Duration;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::error::GatewayError;
use crate::sse::SseParser;
use crate::types::*;
use crate::ChatProvider;

/// Talks to any server exposing the OpenAI `/v1/chat/completions` and
/// `/v1/models` wire shape.
pub struct OpenAiCompatAdapter {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    idle_timeout: Duration,
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
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client,
            idle_timeout,
        })
    }

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
}

fn map_status(status: reqwest::StatusCode, retry_after: Option<u64>, body: String) -> GatewayError {
    match status.as_u16() {
        401 | 403 => GatewayError::Unauthorized,
        429 => GatewayError::RateLimited {
            retry_after_secs: retry_after,
        },
        _ => GatewayError::BadResponse {
            detail: format!(
                "HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            ),
        },
    }
}

fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse()
        .ok()
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
            .send_with_timeout(self.auth(self.client.get(format!("{}/models", self.base_url))))
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
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        for m in &req.messages {
            let role = match m.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
            };
            messages.push(json!({"role": role, "content": m.content}));
        }
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens,
        });
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        let rb = self
            .auth(
                self.client
                    .post(format!("{}/chat/completions", self.base_url)),
            )
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
                    Ok(None) => break, // connection closed without [DONE] — finish with what we have
                    Ok(Some(Err(e))) => {
                        let _ = tx.send(ChatEvent::Error { message: format!("Stream error: {e}"), retryable: true }).await;
                        return;
                    }
                    Ok(Some(Ok(c))) => c,
                }
            };
            for payload in parser.push(&chunk) {
                if payload == "[DONE]" {
                    let _ = tx
                        .send(ChatEvent::Done {
                            usage: usage.clone(),
                            stop_reason: stop_reason.clone(),
                        })
                        .await;
                    return;
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
                    if let Some(text) = choice.pointer("/delta/content").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            let _ = tx
                                .send(ChatEvent::Delta {
                                    text: text.to_string(),
                                })
                                .await;
                        }
                    }
                }
            }
        }
        let _ = tx.send(ChatEvent::Done { usage, stop_reason }).await;
    });
    super::mpsc_stream(rx)
}
