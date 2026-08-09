//! # Opt-in loopback Chromium DOM bridge
//!
//! Talks only to a user-started Chromium DevTools endpoint on 127.0.0.1. Page
//! hosts are allowlisted, JavaScript expressions are fixed by Continuum, and
//! password inputs are never read or written.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::config::McpBrowserConfig;

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct BrowserEmptyRequest {}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserTargetRequest {
    pub target_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserNavigateRequest {
    pub target_id: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserClickRequest {
    pub target_id: String,
    pub selector: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BrowserFillRequest {
    pub target_id: String,
    pub selector: String,
    /// Text to enter; redacted by the common audit sanitizer.
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CdpTarget {
    id: String,
    title: String,
    url: String,
    #[serde(default)]
    web_socket_debugger_url: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowserStatusResponse {
    pub enabled: bool,
    pub reachable: bool,
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowserTab {
    pub target_id: String,
    pub title: String,
    pub url: String,
}

pub async fn status(client: &reqwest::Client, cfg: &McpBrowserConfig) -> BrowserStatusResponse {
    let reachable = cfg.enabled && fetch_targets(client, cfg).await.is_ok();
    BrowserStatusResponse {
        enabled: cfg.enabled,
        reachable,
        allowed_hosts: cfg.allowed_hosts.clone(),
    }
}

pub async fn list_tabs(
    client: &reqwest::Client,
    cfg: &McpBrowserConfig,
) -> Result<Vec<BrowserTab>, BrowserError> {
    Ok(fetch_targets(client, cfg)
        .await?
        .into_iter()
        .filter(|t| t.kind == "page" && allowed_page_url(&t.url, cfg))
        .map(|t| BrowserTab {
            target_id: t.id,
            title: t.title.chars().take(500).collect(),
            url: t.url,
        })
        .collect())
}

pub async fn snapshot(
    client: &reqwest::Client,
    req: &BrowserTargetRequest,
    cfg: &McpBrowserConfig,
) -> Result<Value, BrowserError> {
    let target = target(client, &req.target_id, cfg).await?;
    let limit = cfg.max_text_bytes.clamp(1024, 1024 * 1024);
    let expression = format!(
        r#"(()=>{{const clean=s=>(s||'').slice(0,200);const forms=[...document.forms].slice(0,50).map(f=>({{ariaLabel:clean(f.getAttribute('aria-label')),fields:[...f.elements].slice(0,100).filter(e=>e.type!=='password').map(e=>({{tag:e.tagName.toLowerCase(),type:clean(e.type),name:clean(e.name),label:clean(e.getAttribute('aria-label'))}}))}}));return {{title:clean(document.title),url:location.href,text:(document.body?.innerText||'').slice(0,{limit}),forms}}}})()"#
    );
    evaluate(&target, &expression, cfg).await
}

pub async fn navigate(
    client: &reqwest::Client,
    req: &BrowserNavigateRequest,
    cfg: &McpBrowserConfig,
) -> Result<Value, BrowserError> {
    validate_url(&req.url, cfg)?;
    let target = target_allow_blank(client, &req.target_id, cfg).await?;
    command(&target, "Page.navigate", json!({"url": req.url}), cfg).await
}

pub async fn click(
    client: &reqwest::Client,
    req: &BrowserClickRequest,
    cfg: &McpBrowserConfig,
) -> Result<Value, BrowserError> {
    validate_selector(&req.selector)?;
    let target = target(client, &req.target_id, cfg).await?;
    let selector =
        serde_json::to_string(&req.selector).map_err(|e| BrowserError::Protocol(e.to_string()))?;
    evaluate(&target, &format!(r#"(()=>{{const e=document.querySelector({selector});if(!e)return {{ok:false,reason:'not_found'}};if(e.matches('input[type=password]'))return {{ok:false,reason:'password_blocked'}};e.click();return {{ok:true}}}})()"#), cfg).await
}

pub async fn fill(
    client: &reqwest::Client,
    req: &BrowserFillRequest,
    cfg: &McpBrowserConfig,
) -> Result<Value, BrowserError> {
    validate_selector(&req.selector)?;
    if req.content.len() > 64 * 1024 {
        return Err(BrowserError::Invalid("content too large".into()));
    }
    let target = target(client, &req.target_id, cfg).await?;
    let selector =
        serde_json::to_string(&req.selector).map_err(|e| BrowserError::Protocol(e.to_string()))?;
    let content =
        serde_json::to_string(&req.content).map_err(|e| BrowserError::Protocol(e.to_string()))?;
    evaluate(&target, &format!(r#"(()=>{{const e=document.querySelector({selector});if(!e)return {{ok:false,reason:'not_found'}};if(e.matches('input[type=password]'))return {{ok:false,reason:'password_blocked'}};if(!('value' in e))return {{ok:false,reason:'not_fillable'}};e.value={content};e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return {{ok:true}}}})()"#), cfg).await
}

async fn fetch_targets(
    client: &reqwest::Client,
    cfg: &McpBrowserConfig,
) -> Result<Vec<CdpTarget>, BrowserError> {
    if !cfg.enabled {
        return Err(BrowserError::Disabled);
    }
    client
        .get(format!("http://127.0.0.1:{}/json/list", cfg.port))
        .send()
        .await
        .map_err(|e| BrowserError::Unavailable(e.to_string()))?
        .error_for_status()
        .map_err(|e| BrowserError::Unavailable(e.to_string()))?
        .json()
        .await
        .map_err(|e| BrowserError::Protocol(e.to_string()))
}

async fn target(
    client: &reqwest::Client,
    id: &str,
    cfg: &McpBrowserConfig,
) -> Result<CdpTarget, BrowserError> {
    let t = target_raw(client, id, cfg).await?;
    if !allowed_page_url(&t.url, cfg) {
        return Err(BrowserError::HostDenied);
    }
    Ok(t)
}
async fn target_allow_blank(
    client: &reqwest::Client,
    id: &str,
    cfg: &McpBrowserConfig,
) -> Result<CdpTarget, BrowserError> {
    let t = target_raw(client, id, cfg).await?;
    if t.url != "about:blank" && !allowed_page_url(&t.url, cfg) {
        return Err(BrowserError::HostDenied);
    }
    Ok(t)
}
async fn target_raw(
    client: &reqwest::Client,
    id: &str,
    cfg: &McpBrowserConfig,
) -> Result<CdpTarget, BrowserError> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(BrowserError::Invalid("invalid target_id".into()));
    }
    fetch_targets(client, cfg)
        .await?
        .into_iter()
        .find(|t| t.id == id && t.kind == "page" && !t.web_socket_debugger_url.is_empty())
        .ok_or(BrowserError::TargetNotFound)
}

fn allowed_page_url(raw: &str, cfg: &McpBrowserConfig) -> bool {
    validate_url(raw, cfg).is_ok()
}
fn validate_url(raw: &str, cfg: &McpBrowserConfig) -> Result<(), BrowserError> {
    let url = Url::parse(raw).map_err(|_| BrowserError::HostDenied)?;
    if !matches!(url.scheme(), "http" | "https") || url.username() != "" || url.password().is_some()
    {
        return Err(BrowserError::HostDenied);
    }
    let host = url.host_str().ok_or(BrowserError::HostDenied)?;
    if !cfg
        .allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host))
    {
        return Err(BrowserError::HostDenied);
    }
    Ok(())
}
fn validate_selector(s: &str) -> Result<(), BrowserError> {
    if s.is_empty() || s.len() > 1024 || s.contains(['\r', '\n']) {
        Err(BrowserError::Invalid("invalid selector".into()))
    } else {
        Ok(())
    }
}

async fn evaluate(
    target: &CdpTarget,
    expression: &str,
    cfg: &McpBrowserConfig,
) -> Result<Value, BrowserError> {
    let response = command(
        target,
        "Runtime.evaluate",
        json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
        cfg,
    )
    .await?;
    if response.get("exceptionDetails").is_some() {
        return Err(BrowserError::Protocol("DOM expression failed".into()));
    }
    Ok(response
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}
async fn command(
    target: &CdpTarget,
    method: &str,
    params: Value,
    cfg: &McpBrowserConfig,
) -> Result<Value, BrowserError> {
    let ws = Url::parse(&target.web_socket_debugger_url)
        .map_err(|e| BrowserError::Protocol(e.to_string()))?;
    if !matches!(ws.scheme(), "ws") || !matches!(ws.host_str(), Some("127.0.0.1" | "localhost")) {
        return Err(BrowserError::EndpointDenied);
    }
    let (mut socket, _) = timeout(
        Duration::from_secs(cfg.timeout_secs.clamp(1, 30)),
        connect_async(ws.as_str()),
    )
    .await
    .map_err(|_| BrowserError::Timeout)?
    .map_err(|e| BrowserError::Unavailable(e.to_string()))?;
    socket
        .send(Message::Text(
            json!({"id":1,"method":method,"params":params}).to_string(),
        ))
        .await
        .map_err(|e| BrowserError::Protocol(e.to_string()))?;
    let value = timeout(Duration::from_secs(cfg.timeout_secs.clamp(1, 30)), async {
        while let Some(message) = socket.next().await {
            let text = message
                .map_err(|e| BrowserError::Protocol(e.to_string()))?
                .into_text()
                .map_err(|e| BrowserError::Protocol(e.to_string()))?;
            let v: Value =
                serde_json::from_str(&text).map_err(|e| BrowserError::Protocol(e.to_string()))?;
            if v.get("id") == Some(&json!(1)) {
                return if let Some(error) = v.get("error") {
                    Err(BrowserError::Protocol(error.to_string()))
                } else {
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                };
            }
        }
        Err(BrowserError::Protocol("CDP connection closed".into()))
    })
    .await
    .map_err(|_| BrowserError::Timeout)??;
    Ok(value)
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("browser DOM bridge is disabled")]
    Disabled,
    #[error("browser bridge unavailable: {0}")]
    Unavailable(String),
    #[error("browser target not found")]
    TargetNotFound,
    #[error("page host is not allowlisted")]
    HostDenied,
    #[error("non-loopback CDP endpoint denied")]
    EndpointDenied,
    #[error("invalid browser request: {0}")]
    Invalid(String),
    #[error("browser protocol error: {0}")]
    Protocol(String),
    #[error("browser bridge timed out")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_and_selector_validation_fail_closed() {
        let cfg = McpBrowserConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(validate_url("http://localhost:3000/a", &cfg).is_ok());
        assert!(validate_url("https://bank.example/", &cfg).is_err());
        assert!(validate_url("file:///secret", &cfg).is_err());
        assert!(validate_selector("#save").is_ok());
        assert!(validate_selector("").is_err());
    }
}
