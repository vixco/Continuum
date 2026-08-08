use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use super::types::{
    ComposioConfigureRequest, ComposioCreateSessionRequest, ComposioExecuteRequest,
    ComposioMetaExecuteRequest, ComposioSearchRequest, RiskLevel,
};

const DEFAULT_BASE_URL: &str = "https://backend.composio.dev";
const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposioConfig {
    pub version: u32,
    pub base_url: String,
    pub user_id: String,
    #[serde(default)]
    pub enabled_toolkits: Vec<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub session_mcp_url: Option<String>,
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for ComposioConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            base_url: std::env::var("COMPOSIO_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            user_id: String::new(),
            enabled_toolkits: Vec::new(),
            session_id: None,
            session_mcp_url: None,
            updated_at: None,
        }
    }
}

pub struct ComposioClient {
    root: PathBuf,
    config_path: PathBuf,
    config: RwLock<ComposioConfig>,
    http: reqwest::Client,
}

impl ComposioClient {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("Failed to create {}", root.display()))?;
        let config_path = root.join("composio.json");
        let config = load_config(&config_path)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("continuum-agent-os/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to build Composio HTTP client")?;
        Ok(Self {
            root: root.to_path_buf(),
            config_path,
            config: RwLock::new(config),
            http,
        })
    }

    pub async fn status(&self) -> Value {
        let config = self.config.read().await.clone();
        let (key_available, key_source) = match self.resolve_api_key().await {
            Ok((_, source)) => (true, Some(source)),
            Err(_) => (false, None),
        };
        serde_json::json!({
            "configured": !config.user_id.trim().is_empty(),
            "api_key_available": key_available,
            "api_key_source": key_source,
            "base_url": config.base_url,
            "user_id": config.user_id,
            "enabled_toolkits": config.enabled_toolkits,
            "session_id": config.session_id,
            "session_mcp_url_available": config.session_mcp_url.is_some(),
            "supports": {
                "tool_discovery": true,
                "managed_oauth": true,
                "direct_tool_execution": true,
                "meta_tools": true,
                "multi_account": true,
                "workbench": true
            }
        })
    }

    pub async fn configure(&self, request: &ComposioConfigureRequest) -> Result<Value> {
        validate_user_id(&request.user_id)?;
        let base_url = request
            .base_url
            .as_deref()
            .map(validate_base_url)
            .transpose()?
            .unwrap_or_else(|| {
                std::env::var("COMPOSIO_BASE_URL")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            });
        let mut toolkits = request
            .enabled_toolkits
            .iter()
            .map(|toolkit| toolkit.trim().to_ascii_lowercase())
            .filter(|toolkit| !toolkit.is_empty())
            .collect::<Vec<_>>();
        toolkits.sort();
        toolkits.dedup();
        for toolkit in &toolkits {
            if !toolkit.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            }) {
                bail!("Invalid Composio toolkit slug {toolkit:?}");
            }
        }

        let mut config = self.config.write().await;
        let identity_changed = config.user_id != request.user_id.trim()
            || config.enabled_toolkits != toolkits
            || config.base_url != base_url;
        config.version = CONFIG_VERSION;
        config.user_id = request.user_id.trim().to_string();
        config.enabled_toolkits = toolkits;
        config.base_url = base_url;
        if request.reset_session || identity_changed {
            config.session_id = None;
            config.session_mcp_url = None;
        }
        config.updated_at = Some(chrono::Utc::now());
        persist_config(&self.config_path, &config)?;
        Ok(serde_json::to_value(&*config)?)
    }

    pub async fn create_session(&self, request: &ComposioCreateSessionRequest) -> Result<Value> {
        let existing = self.config.read().await.session_id.clone();
        if !request.force_new {
            if let Some(session_id) = existing {
                return Ok(serde_json::json!({
                    "created": false,
                    "session_id": session_id,
                    "message": "Reusing the persisted Composio Tool Router session"
                }));
            }
        }
        self.create_session_inner().await
    }

    pub async fn search(&self, request: &ComposioSearchRequest) -> Result<Value> {
        if request.queries.is_empty() || request.queries.len() > 7 {
            bail!("queries must contain between 1 and 7 use cases");
        }
        let mut queries = Vec::with_capacity(request.queries.len());
        for query in &request.queries {
            let query = query.trim();
            if query.is_empty() || query.chars().count() > 2_000 {
                bail!("each Composio search query must contain 1 to 2,000 characters");
            }
            queries.push(serde_json::json!({ "use_case": query }));
        }
        let session_id = self.ensure_session().await?;
        let mut body = serde_json::json!({ "queries": queries });
        if let Some(model) = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            body["model"] = Value::String(model.to_string());
        }
        let path = format!("/api/v3/tool_router/session/{session_id}/search");
        let result = self.request_json("POST", &path, Some(body)).await?;
        Ok(serde_json::json!({ "session_id": session_id, "response": result }))
    }

    pub async fn execute(&self, request: &ComposioExecuteRequest) -> Result<Value> {
        validate_tool_slug(&request.tool_slug)?;
        validate_object_or_null(&request.arguments, "arguments")?;
        let session_id = self.ensure_session().await?;
        let mut body = serde_json::json!({
            "tool_slug": request.tool_slug.trim().to_ascii_uppercase(),
            "arguments": request.arguments.clone(),
            "enable_auto_workbench_offload": request.enable_auto_workbench_offload
        });
        if let Some(account) = request
            .account
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            body["account"] = Value::String(account.to_string());
        }
        let path = format!("/api/v3/tool_router/session/{session_id}/execute");
        let result = self.request_json("POST", &path, Some(body)).await?;
        ensure_tool_response_success(&result)?;
        Ok(serde_json::json!({
            "session_id": session_id,
            "tool_slug": request.tool_slug.trim().to_ascii_uppercase(),
            "intent": request.intent,
            "verification": { "source": "composio_tool_response", "accepted": true },
            "response": result
        }))
    }

    pub async fn execute_meta(&self, request: &ComposioMetaExecuteRequest) -> Result<Value> {
        let slug = request.meta_tool.trim().to_ascii_uppercase();
        if !is_supported_meta_tool(&slug) {
            bail!("Unsupported Composio meta tool {slug:?}");
        }
        validate_object_or_null(&request.arguments, "arguments")?;
        let session_id = self.ensure_session().await?;
        let body = serde_json::json!({
            "slug": slug,
            "arguments": request.arguments.clone()
        });
        let path = format!("/api/v3/tool_router/session/{session_id}/execute_meta");
        let result = self.request_json("POST", &path, Some(body)).await?;
        ensure_tool_response_success(&result)?;
        Ok(serde_json::json!({
            "session_id": session_id,
            "meta_tool": request.meta_tool.trim().to_ascii_uppercase(),
            "intent": request.intent,
            "verification": { "source": "composio_tool_response", "accepted": true },
            "response": result
        }))
    }

    async fn ensure_session(&self) -> Result<String> {
        if let Some(session_id) = self.config.read().await.session_id.clone() {
            validate_session_id(&session_id)?;
            return Ok(session_id);
        }
        let created = self.create_session_inner().await?;
        created
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Composio did not return a session_id"))
    }

    async fn create_session_inner(&self) -> Result<Value> {
        let snapshot = self.config.read().await.clone();
        validate_user_id(&snapshot.user_id)?;
        let mut body = serde_json::json!({
            "user_id": snapshot.user_id.clone(),
            "manage_connections": {
                "enable": true,
                "enable_wait_for_connections": true,
                "enable_connection_removal": true
            },
            "workbench": {
                "enable": true,
                "enable_proxy_execution": true
            }
        });
        if !snapshot.enabled_toolkits.is_empty() {
            body["toolkits"] = serde_json::json!({ "enabled": snapshot.enabled_toolkits.clone() });
        }
        let response = self
            .request_json("POST", "/api/v3/tool_router/session", Some(body))
            .await?;
        let session_id = response
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Composio session response has no session_id"))?
            .to_string();
        validate_session_id(&session_id)?;
        // The hosted MCP URL is a parallel execution surface that would bypass
        // Continuum's local allow/ask/deny broker. Record only whether upstream
        // offered it; never persist or expose the URL to the agent.
        let mcp_url_available = response
            .pointer("/mcp/url")
            .and_then(Value::as_str)
            .is_some();

        let mut config = self.config.write().await;
        config.session_id = Some(session_id.clone());
        config.session_mcp_url = None;
        config.updated_at = Some(chrono::Utc::now());
        persist_config(&self.config_path, &config)?;
        Ok(serde_json::json!({
            "created": true,
            "session_id": session_id,
            "mcp_url_available": mcp_url_available,
            "execution_surface": "continuum_broker_only"
        }))
    }

    async fn request_json(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let config = self.config.read().await.clone();
        let base_url = validate_base_url(&config.base_url)?;
        let url = format!("{}{}", base_url.trim_end_matches('/'), path);
        let (api_key, _) = self.resolve_api_key().await?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&api_key).context("Composio API key contains invalid bytes")?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let mut builder = match method {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            "PATCH" => self.http.patch(&url),
            other => bail!("Unsupported Composio HTTP method {other}"),
        }
        .headers(headers);
        if let Some(body) = body {
            builder = builder.body(serde_json::to_vec(&body)?);
        }
        let response = builder
            .send()
            .await
            .with_context(|| format!("Composio request failed: {method} {path}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("Failed to read Composio response body")?;
        let payload = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                Value::String(
                    String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(8_000)
                        .collect(),
                )
            })
        };
        if !status.is_success() {
            bail!(
                "Composio returned HTTP {} for {} {}: {}",
                status.as_u16(),
                method,
                path,
                compact_error(&payload)
            );
        }
        Ok(payload)
    }

    async fn resolve_api_key(&self) -> Result<(String, String)> {
        if let Ok(key) = std::env::var("COMPOSIO_API_KEY") {
            let key = key.trim().to_string();
            validate_api_key(&key)?;
            return Ok((key, "environment".to_string()));
        }

        #[cfg(windows)]
        {
            let encrypted_path = self.root.join("composio-api-key.dpapi");
            if encrypted_path.exists() {
                let key = decrypt_windows_key(&encrypted_path).await?;
                validate_api_key(&key)?;
                return Ok((key, "windows_dpapi".to_string()));
            }
        }

        #[cfg(not(windows))]
        {
            let key_path = self.root.join("composio-api-key");
            if key_path.exists() {
                let key = std::fs::read_to_string(&key_path)
                    .with_context(|| format!("Failed to read {}", key_path.display()))?;
                let key = key.trim().to_string();
                validate_api_key(&key)?;
                return Ok((key, "local_key_file".to_string()));
            }
        }

        bail!(
            "Composio API key is unavailable. Set COMPOSIO_API_KEY or run scripts/install-agent-os.ps1 to store it with Windows DPAPI"
        )
    }
}

pub fn classify_tool_slug(tool_slug: &str) -> RiskLevel {
    let upper = tool_slug.trim().to_ascii_uppercase();
    let tokens: Vec<_> = upper
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.iter().any(|token| {
        matches!(
            *token,
            "DELETE"
                | "REMOVE"
                | "REVOKE"
                | "CANCEL"
                | "TERMINATE"
                | "PURGE"
                | "DESTROY"
                | "DROP"
                | "ARCHIVE"
                | "DISCONNECT"
                | "UNSUBSCRIBE"
        )
    }) {
        return RiskLevel::Destructive;
    }
    if tokens.iter().any(|token| {
        matches!(
            *token,
            "CREATE"
                | "ADD"
                | "UPDATE"
                | "EDIT"
                | "SEND"
                | "POST"
                | "PUT"
                | "PATCH"
                | "UPLOAD"
                | "MOVE"
                | "COPY"
                | "RENAME"
                | "INVITE"
                | "PUBLISH"
                | "EXECUTE"
                | "RUN"
                | "TRIGGER"
                | "START"
                | "STOP"
                | "ENABLE"
                | "DISABLE"
                | "MARK"
                | "REPLY"
                | "FORWARD"
        )
    }) {
        return RiskLevel::Write;
    }
    if tokens.iter().any(|token| {
        matches!(
            *token,
            "GET"
                | "LIST"
                | "SEARCH"
                | "READ"
                | "FETCH"
                | "QUERY"
                | "LOOKUP"
                | "FIND"
                | "DOWNLOAD"
                | "PREVIEW"
                | "CHECK"
                | "STATUS"
                | "DESCRIBE"
        )
    }) {
        return RiskLevel::Read;
    }
    RiskLevel::Write
}

pub fn classify_execute_request(request: &ComposioExecuteRequest) -> RiskLevel {
    if request.enable_auto_workbench_offload {
        RiskLevel::Destructive
    } else {
        classify_tool_slug(&request.tool_slug)
    }
}

pub fn classify_meta_tool(meta_tool: &str, arguments: &Value) -> RiskLevel {
    let slug = meta_tool.trim().to_ascii_uppercase();
    match slug.as_str() {
        "COMPOSIO_SEARCH_TOOLS" | "COMPOSIO_GET_TOOL_SCHEMAS" | "COMPOSIO_WAIT_FOR_CONNECTIONS" => {
            RiskLevel::Read
        }
        "COMPOSIO_REMOTE_WORKBENCH" | "COMPOSIO_REMOTE_BASH_TOOL" => RiskLevel::Destructive,
        "COMPOSIO_MANAGE_CONNECTIONS" => {
            let action = arguments
                .get("action")
                .or_else(|| arguments.get("operation"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ["remove", "delete", "disconnect", "revoke"]
                .iter()
                .any(|candidate| action.contains(candidate))
            {
                RiskLevel::Destructive
            } else if ["list", "get", "status"]
                .iter()
                .any(|candidate| action.contains(candidate))
            {
                RiskLevel::Read
            } else {
                RiskLevel::Write
            }
        }
        "COMPOSIO_MULTI_EXECUTE_TOOL" => arguments
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        tool.get("tool_slug")
                            .or_else(|| tool.get("slug"))
                            .and_then(Value::as_str)
                    })
                    .map(classify_tool_slug)
                    .max()
                    .unwrap_or(RiskLevel::Write)
            })
            .unwrap_or(RiskLevel::Write),
        _ => RiskLevel::Write,
    }
}

fn load_config(path: &Path) -> Result<ComposioConfig> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut config: ComposioConfig = serde_json::from_slice(&bytes)
                .with_context(|| format!("Invalid Composio config at {}", path.display()))?;
            config.base_url = validate_base_url(&config.base_url)?;
            if !config.user_id.is_empty() {
                validate_user_id(&config.user_id)?;
            }
            if let Some(session_id) = &config.session_id {
                validate_session_id(session_id)?;
            }
            Ok(config)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ComposioConfig::default()),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn persist_config(path: &Path, config: &ComposioConfig) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Composio config has no parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".composio-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temporary, serde_json::to_vec_pretty(config)?)
        .with_context(|| format!("Failed to write {}", temporary.display()))?;
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to replace {}", path.display()))?;
    }
    std::fs::rename(&temporary, path)
        .with_context(|| format!("Failed to activate {}", path.display()))?;
    Ok(())
}

fn validate_user_id(user_id: &str) -> Result<()> {
    let user_id = user_id.trim();
    if user_id.is_empty() || user_id.chars().count() > 256 {
        bail!("Composio user_id must contain between 1 and 256 characters");
    }
    if user_id
        .chars()
        .any(|character| matches!(character, '\r' | '\n' | '\0'))
    {
        bail!("Composio user_id contains unsupported control characters");
    }
    Ok(())
}

fn validate_api_key(key: &str) -> Result<()> {
    if key.len() < 10 || key.len() > 2_048 || key.chars().any(char::is_whitespace) {
        bail!("Composio API key has an invalid shape");
    }
    Ok(())
}

fn validate_base_url(value: &str) -> Result<String> {
    let parsed = url::Url::parse(value.trim()).context("Invalid Composio base_url")?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("Composio base_url cannot contain embedded credentials");
    }
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && is_loopback(&parsed)) {
        bail!("Composio base_url must use HTTPS (HTTP is allowed only for loopback development)");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("Composio base_url cannot contain query parameters or a fragment");
    }
    if !matches!(parsed.path(), "" | "/") {
        bail!("Composio base_url cannot contain a path prefix");
    }
    let official = parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("backend.composio.dev"));
    if !official && !is_loopback(&parsed) {
        bail!("Composio base_url must target backend.composio.dev (or loopback for local development)");
    }
    if official && parsed.port_or_known_default() != Some(443) {
        bail!("The official Composio base_url must use port 443");
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

fn is_loopback(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
}

fn validate_session_id(value: &str) -> Result<()> {
    if !value.starts_with("trs_")
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!("Invalid Composio Tool Router session id");
    }
    Ok(())
}

fn validate_tool_slug(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 256
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!("Invalid Composio tool slug");
    }
    Ok(())
}

fn validate_object_or_null(value: &Value, field: &str) -> Result<()> {
    if !(value.is_object() || value.is_null()) {
        bail!("{field} must be a JSON object");
    }
    Ok(())
}

fn is_supported_meta_tool(slug: &str) -> bool {
    matches!(
        slug,
        "COMPOSIO_SEARCH_TOOLS"
            | "COMPOSIO_GET_TOOL_SCHEMAS"
            | "COMPOSIO_MULTI_EXECUTE_TOOL"
            | "COMPOSIO_MANAGE_CONNECTIONS"
            | "COMPOSIO_WAIT_FOR_CONNECTIONS"
            | "COMPOSIO_REMOTE_WORKBENCH"
            | "COMPOSIO_REMOTE_BASH_TOOL"
    )
}

fn ensure_tool_response_success(value: &Value) -> Result<()> {
    let Some(error) = value.get("error") else {
        return Ok(());
    };
    let failed = match error {
        Value::Null => false,
        Value::String(message) => !message.trim().is_empty(),
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Bool(value) => *value,
        Value::Number(_) => true,
    };
    if failed {
        bail!("Composio tool execution failed: {}", compact_error(value));
    }
    Ok(())
}

fn compact_error(value: &Value) -> String {
    let candidate = value
        .pointer("/error/message")
        .or_else(|| value.get("error"))
        .or_else(|| value.get("message"));
    match candidate {
        Some(Value::String(message)) => message.chars().take(2_000).collect(),
        Some(other) => serde_json::to_string(other)
            .unwrap_or_else(|_| "unknown Composio error".to_string())
            .chars()
            .take(2_000)
            .collect(),
        None => serde_json::to_string(value)
            .unwrap_or_else(|_| "unknown Composio error".to_string())
            .chars()
            .take(2_000)
            .collect(),
    }
}

#[cfg(windows)]
async fn decrypt_windows_key(path: &Path) -> Result<String> {
    let script = r#"
$ErrorActionPreference = 'Stop'
$encrypted = [System.IO.File]::ReadAllText($env:CONTINUUM_COMPOSIO_KEY_PATH).Trim()
$secure = ConvertTo-SecureString $encrypted
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try { [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
"#;
    let output = tokio::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .env("CONTINUUM_COMPOSIO_KEY_PATH", path)
        .kill_on_drop(true)
        .output()
        .await
        .context("Failed to start PowerShell for Windows DPAPI decryption")?;
    if !output.status.success() {
        bail!(
            "Failed to decrypt the Composio API key with Windows DPAPI: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("Decrypted Composio API key was not UTF-8")?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_read_write_and_destructive_slugs() {
        assert_eq!(classify_tool_slug("GMAIL_LIST_THREADS"), RiskLevel::Read);
        assert_eq!(
            classify_tool_slug("GITHUB_CREATE_AN_ISSUE"),
            RiskLevel::Write
        );
        assert_eq!(
            classify_tool_slug("SLACK_DELETE_MESSAGE"),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn workbench_offload_is_destructive_even_for_read_slug() {
        let request = ComposioExecuteRequest {
            tool_slug: "GMAIL_LIST_THREADS".into(),
            arguments: serde_json::json!({}),
            account: None,
            intent: None,
            enable_auto_workbench_offload: true,
        };
        assert_eq!(classify_execute_request(&request), RiskLevel::Destructive);
    }

    #[test]
    fn multi_execute_uses_highest_risk() {
        let arguments = serde_json::json!({
            "tools": [
                {"tool_slug":"GMAIL_LIST_THREADS"},
                {"tool_slug":"GITHUB_CREATE_AN_ISSUE"},
                {"tool_slug":"SLACK_DELETE_MESSAGE"}
            ]
        });
        assert_eq!(
            classify_meta_tool("COMPOSIO_MULTI_EXECUTE_TOOL", &arguments),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn rejects_non_https_remote_base_url() {
        assert!(validate_base_url("http://example.com").is_err());
        assert!(validate_base_url("http://127.0.0.1:9999").is_ok());
        assert!(validate_base_url("https://backend.composio.dev").is_ok());
        assert!(validate_base_url("https://backend.composio.dev:444").is_err());
        assert!(validate_base_url("https://backend.composio.dev/prefix").is_err());
        assert!(validate_base_url("https://user:pass@backend.composio.dev").is_err());
        assert!(validate_base_url("https://evil.example").is_err());
    }
}
