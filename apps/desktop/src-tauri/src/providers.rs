//! Provider connections: JSON store (no secrets) + OS credential store.
//!
//! `providers.json` lives in the Continuum dev dir and is written atomically
//! (tmp + rename, same pattern as voice intents). API keys go exclusively
//! into the OS credential store via `keyring` (service "Continuum").

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use continuum_core::config::ChatConfig;
use continuum_gateway::providers::{AnthropicAdapter, ClaudeCliAdapter, OpenAiCompatAdapter};
use continuum_gateway::{
    catalog, ChatProvider, ConnectionTestReport, ProviderConnection, ProviderKind,
};

pub const PROVIDERS_FILE: &str = "providers.json";
pub const KEYRING_SERVICE: &str = "Continuum";

pub struct ProviderStore {
    dev_dir: PathBuf,
}

impl ProviderStore {
    pub fn new(dev_dir: PathBuf) -> Self {
        Self { dev_dir }
    }

    fn path(&self) -> PathBuf {
        self.dev_dir.join(PROVIDERS_FILE)
    }

    /// Missing or corrupt file → empty list (warn, never crash the dashboard).
    pub fn load(&self) -> Vec<ProviderConnection> {
        let path = self.path();
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!(layer = "desktop", component = "providers", error = %e, "providers.json unparseable — starting empty");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    }

    pub fn save(&self, connections: &[ProviderConnection]) -> Result<()> {
        std::fs::create_dir_all(&self.dev_dir).context("create dev dir")?;
        let path = self.path();
        let tmp = path.with_extension("json.tmp");
        let payload = serde_json::to_string_pretty(connections).context("serialize providers")?;
        std::fs::write(&tmp, payload).context("write providers tmp")?;
        std::fs::rename(&tmp, &path).context("rename providers.json")?;
        Ok(())
    }
}

pub trait SecretStore: Send + Sync {
    fn set(&self, id: &str, secret: &str) -> Result<()>;
    fn get(&self, id: &str) -> Result<Option<String>>;
    fn delete(&self, id: &str) -> Result<()>;
}

/// Windows Credential Manager (via keyring). Account name is namespaced so
/// other Continuum secrets can share the service later.
pub struct KeyringSecretStore;

impl KeyringSecretStore {
    fn entry(id: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("provider:{id}"))
            .context("open keyring entry")
    }
}

impl SecretStore for KeyringSecretStore {
    fn set(&self, id: &str, secret: &str) -> Result<()> {
        Self::entry(id)?
            .set_password(secret)
            .context("store secret")
    }
    fn get(&self, id: &str) -> Result<Option<String>> {
        match Self::entry(id)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("read secret: {e}")),
        }
    }
    fn delete(&self, id: &str) -> Result<()> {
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("delete secret: {e}")),
        }
    }
}

/// In-memory store for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemorySecretStore(std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemorySecretStore {
    fn set(&self, id: &str, secret: &str) -> Result<()> {
        self.0
            .lock()
            .expect("lock")
            .insert(id.into(), secret.into());
        Ok(())
    }
    fn get(&self, id: &str) -> Result<Option<String>> {
        Ok(self.0.lock().expect("lock").get(id).cloned())
    }
    fn delete(&self, id: &str) -> Result<()> {
        self.0.lock().expect("lock").remove(id);
        Ok(())
    }
}

/// Shared Tauri-managed state for the chat feature: the provider store, the
/// OS credential store, and in-flight stream cancellation tokens (the last
/// is populated starting with Task 10's chat-send command).
pub struct ChatState {
    pub providers: std::sync::Mutex<ProviderStore>,
    pub secrets: Box<dyn SecretStore>,
    // Not read yet: populated and consumed by Task 10's chat-send / cancel
    // commands, which don't exist in this task.
    #[allow(dead_code)]
    pub inflight:
        std::sync::Mutex<std::collections::HashMap<String, tokio_util::sync::CancellationToken>>,
}

/// A catalog preset, shaped for the frontend "Add Provider" form.
#[derive(serde::Serialize)]
pub struct CatalogEntryDto {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: ProviderKind,
    pub default_base_url: Option<&'static str>,
    pub needs_key: bool,
    pub key_hint: &'static str,
}

/// Input for [`provider_add`]. `api_key` is written straight to the OS
/// credential store and never touches `providers.json`.
#[derive(serde::Deserialize)]
pub struct ProviderAddInput {
    pub catalog_id: Option<String>,
    pub display_name: String,
    /// Required for custom endpoints; presets fall back to the catalog default.
    pub base_url: Option<String>,
    /// Stored in the credential store, never persisted to JSON.
    pub api_key: Option<String>,
    /// When true, save even if the connection test fails.
    pub save_anyway: bool,
}

/// Resolves the executable name for the Claude Code CLI subprocess.
///
/// On Windows, `claude` installs as an npm shim (`claude.cmd`), and
/// `std::process::Command`'s PATH search does not reliably resolve
/// extension-less shim names the way `cmd.exe` does — spawning `"claude"`
/// directly can fail to find it even though `claude --version` works fine
/// from an interactive shell. We probe once with `where claude.cmd` (a
/// synchronous, near-instant lookup; it never spawns the CLI itself) and
/// prefer the `.cmd` name when found. Everywhere else, and if the probe
/// fails, we fall back to the bare `"claude"` name, which resolves
/// normally via PATH.
fn resolve_claude_binary() -> String {
    #[cfg(windows)]
    {
        let found = std::process::Command::new("where")
            .arg("claude.cmd")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if found {
            return "claude.cmd".to_string();
        }
    }
    "claude".to_string()
}

/// Builds a [`ChatProvider`] adapter for a stored connection. This is the
/// single place that decides "which adapter for which connection kind" so
/// Task 10's chat-send commands can reuse it unchanged.
pub fn build_adapter(
    conn: &ProviderConnection,
    secrets: &dyn SecretStore,
    chat: &ChatConfig,
) -> Result<Box<dyn ChatProvider>, String> {
    let connect = Duration::from_secs(chat.connect_timeout_secs);
    let idle = Duration::from_secs(chat.stream_idle_timeout_secs);
    match conn.kind {
        ProviderKind::OpenAiCompat => {
            let base = conn.base_url.clone().ok_or("provider has no base URL")?;
            let key = if conn.requires_key {
                secrets.get(&conn.id).map_err(|e| e.to_string())?
            } else {
                None
            };
            Ok(Box::new(
                OpenAiCompatAdapter::new(base, key, connect, idle).map_err(|e| e.user_message())?,
            ))
        }
        ProviderKind::Anthropic => {
            let base = conn
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com".into());
            let key = secrets
                .get(&conn.id)
                .map_err(|e| e.to_string())?
                .ok_or("No API key stored for this provider. Re-add it in Settings.")?;
            Ok(Box::new(
                AnthropicAdapter::new(base, key, connect, idle).map_err(|e| e.user_message())?,
            ))
        }
        ProviderKind::ClaudeCli => Ok(Box::new(ClaudeCliAdapter::new(
            resolve_claude_binary(),
            Duration::from_secs(chat.cli_timeout_secs),
        ))),
    }
}

/// Lists provider presets the "Add Provider" form can prefill from.
#[tauri::command]
pub fn catalog_list() -> Vec<CatalogEntryDto> {
    catalog::catalog()
        .iter()
        .map(|e| CatalogEntryDto {
            id: e.id,
            label: e.label,
            kind: e.kind,
            default_base_url: e.default_base_url,
            needs_key: e.needs_key,
            key_hint: e.key_hint,
        })
        .collect()
}

/// Lists configured provider connections. Never includes secret material —
/// `providers.json` doesn't store any.
#[tauri::command]
pub fn providers_list(chat_state: tauri::State<'_, Arc<ChatState>>) -> Vec<ProviderConnection> {
    chat_state.providers.lock().expect("providers lock").load()
}

/// Adds a provider connection. Tests the connection before saving unless
/// `save_anyway` is set; on a failed test with `save_anyway: false`, any
/// secret already written for this (not-yet-saved) connection is deleted
/// so it isn't orphaned in the credential store.
#[tauri::command]
pub async fn provider_add(
    input: ProviderAddInput,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<ProviderConnection, String> {
    let preset = input.catalog_id.as_deref().and_then(catalog::find);
    let kind = preset.map(|p| p.kind).unwrap_or(ProviderKind::OpenAiCompat);
    let base_url = input
        .base_url
        .clone()
        .or_else(|| preset.and_then(|p| p.default_base_url.map(String::from)));
    if kind != ProviderKind::ClaudeCli && base_url.is_none() {
        return Err("A base URL is required for this provider.".into());
    }
    let id = format!("prov-{}", uuid::Uuid::new_v4());
    let mut conn = ProviderConnection {
        id: id.clone(),
        display_name: input.display_name.trim().to_string(),
        kind,
        base_url,
        catalog_id: input.catalog_id.clone(),
        models: vec![],
        default_model: None,
        roles: vec![],
        requires_key: input.api_key.as_deref().is_some_and(|k| !k.is_empty()),
        last_tested_at: None,
        last_test_ok: None,
    };
    if conn.display_name.is_empty() {
        return Err("Give this provider a name.".into());
    }
    if let Some(key) = input.api_key.as_deref().filter(|k| !k.is_empty()) {
        chat_state
            .secrets
            .set(&id, key)
            .map_err(|e| e.to_string())?;
    }
    // Test before save (spec: test-before-save with explicit escape hatch).
    let chat_cfg = state.runtime.config_snapshot().chat.clone();
    match build_adapter(&conn, chat_state.secrets.as_ref(), &chat_cfg) {
        Ok(adapter) => match adapter.test_connection().await {
            Ok(report) => {
                conn.models = report.models;
                conn.default_model = conn.models.first().cloned();
                conn.last_tested_at = Some(chrono::Utc::now());
                conn.last_test_ok = Some(report.ok);
            }
            Err(e) if !input.save_anyway => {
                let _ = chat_state.secrets.delete(&id); // don't orphan the secret
                return Err(e.user_message());
            }
            Err(_) => conn.last_test_ok = Some(false),
        },
        Err(e) if !input.save_anyway => {
            let _ = chat_state.secrets.delete(&id);
            return Err(e);
        }
        Err(_) => conn.last_test_ok = Some(false),
    }
    // Lock held across the whole load -> mutate -> save sequence, but never
    // across an .await (the adapter call above already completed).
    let store = chat_state.providers.lock().expect("providers lock");
    let mut all = store.load();
    all.push(conn.clone());
    store.save(&all).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Re-tests an existing connection and refreshes its cached model list plus
/// last-tested status. The provider lock is taken twice — briefly to read
/// the connection, then again after the (unlocked) adapter call — so it is
/// never held across the `.await`.
#[tauri::command]
pub async fn provider_test(
    id: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<ConnectionTestReport, String> {
    let conn = {
        let store = chat_state.providers.lock().expect("providers lock");
        store
            .load()
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| "Unknown provider".to_string())?
    };
    let chat_cfg = state.runtime.config_snapshot().chat.clone();
    let adapter = build_adapter(&conn, chat_state.secrets.as_ref(), &chat_cfg)?;
    let result = adapter.test_connection().await;

    {
        let store = chat_state.providers.lock().expect("providers lock");
        let mut all = store.load();
        if let Some(c) = all.iter_mut().find(|c| c.id == id) {
            c.last_tested_at = Some(chrono::Utc::now());
            match &result {
                Ok(report) => {
                    c.models = report.models.clone();
                    let default_still_listed = c
                        .default_model
                        .as_deref()
                        .is_some_and(|d| c.models.iter().any(|m| m == d));
                    if !default_still_listed {
                        c.default_model = c.models.first().cloned();
                    }
                    c.last_test_ok = Some(report.ok);
                }
                Err(_) => c.last_test_ok = Some(false),
            }
            store.save(&all).map_err(|e| e.to_string())?;
        }
    }
    result.map_err(|e| e.user_message())
}

/// Refreshes the cached model list for an existing connection. Same
/// short-lock / await / re-lock shape as [`provider_test`].
#[tauri::command]
pub async fn provider_refresh_models(
    id: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<Vec<String>, String> {
    let conn = {
        let store = chat_state.providers.lock().expect("providers lock");
        store
            .load()
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| "Unknown provider".to_string())?
    };
    let chat_cfg = state.runtime.config_snapshot().chat.clone();
    let adapter = build_adapter(&conn, chat_state.secrets.as_ref(), &chat_cfg)?;
    let models = adapter.list_models().await.map_err(|e| e.user_message())?;

    let store = chat_state.providers.lock().expect("providers lock");
    let mut all = store.load();
    if let Some(c) = all.iter_mut().find(|c| c.id == id) {
        c.models = models.clone();
        let default_still_listed = c
            .default_model
            .as_deref()
            .is_some_and(|d| models.iter().any(|m| m == d));
        if !default_still_listed {
            c.default_model = models.first().cloned();
        }
        store.save(&all).map_err(|e| e.to_string())?;
    }
    Ok(models)
}

/// Deletes a provider connection and its stored secret, if any.
#[tauri::command]
pub fn provider_remove(
    id: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
) -> Result<(), String> {
    let store = chat_state.providers.lock().expect("providers lock");
    let mut all = store.load();
    let before = all.len();
    all.retain(|c| c.id != id);
    if all.len() == before {
        return Err("Unknown provider".into());
    }
    store.save(&all).map_err(|e| e.to_string())?;
    drop(store);
    let _ = chat_state.secrets.delete(&id);
    Ok(())
}

/// Sets the default model for a connection. Free text is accepted when the
/// connection is the Claude CLI (no fixed model list) or when it has no
/// cached models yet; otherwise the model must be one of `conn.models`.
#[tauri::command]
pub fn provider_set_default_model(
    id: String,
    model: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
) -> Result<(), String> {
    let store = chat_state.providers.lock().expect("providers lock");
    let mut all = store.load();
    let conn = all
        .iter_mut()
        .find(|c| c.id == id)
        .ok_or_else(|| "Unknown provider".to_string())?;
    let free_text_ok = conn.kind == ProviderKind::ClaudeCli || conn.models.is_empty();
    if !free_text_ok && !conn.models.contains(&model) {
        return Err(format!(
            "\"{model}\" is not a known model for this provider. Refresh models or pick from the list."
        ));
    }
    conn.default_model = Some(model);
    store.save(&all).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use continuum_gateway::{ProviderConnection, ProviderKind};

    fn conn(id: &str) -> ProviderConnection {
        ProviderConnection {
            id: id.into(),
            display_name: id.into(),
            kind: ProviderKind::OpenAiCompat,
            base_url: Some("http://localhost:1234/v1".into()),
            catalog_id: None,
            models: vec![],
            default_model: None,
            roles: vec![],
            requires_key: false,
            last_tested_at: None,
            last_test_ok: None,
        }
    }

    #[test]
    fn store_roundtrip_and_missing_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = ProviderStore::new(dir.path().to_path_buf());
        assert!(store.load().is_empty()); // missing file → empty, no error
        store.save(&[conn("a"), conn("b")]).expect("save");
        let loaded = store.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "a");
        // corrupt file → empty + no panic
        std::fs::write(dir.path().join("providers.json"), "{not json").expect("write");
        assert!(store.load().is_empty());
    }

    #[test]
    fn memory_secret_store_roundtrip() {
        let s = MemorySecretStore::default();
        assert!(s.get("x").expect("get").is_none());
        s.set("x", "sekrit").expect("set");
        assert_eq!(s.get("x").expect("get").as_deref(), Some("sekrit"));
        s.delete("x").expect("delete");
        assert!(s.get("x").expect("get").is_none());
    }

    // -- build_adapter -------------------------------------------------

    #[test]
    fn build_adapter_openai_compat_ok_without_key() {
        // conn("id") defaults to OpenAiCompat with a base_url and no key requirement.
        let c = conn("oai-1");
        let secrets = MemorySecretStore::default();
        let cfg = ChatConfig::default();
        assert!(build_adapter(&c, &secrets, &cfg).is_ok());
    }

    #[test]
    fn build_adapter_openai_compat_missing_base_url_errs() {
        let mut c = conn("oai-2");
        c.base_url = None;
        let secrets = MemorySecretStore::default();
        let cfg = ChatConfig::default();
        // build_adapter's Ok type (Box<dyn ChatProvider>) isn't Debug, so
        // map it away before expect_err rather than deriving Debug on it.
        let err = build_adapter(&c, &secrets, &cfg)
            .map(|_| ())
            .expect_err("missing base url");
        assert!(err.contains("base URL"), "{err}");
    }

    #[test]
    fn build_adapter_anthropic_missing_key_errs() {
        let mut c = conn("anthropic-1");
        c.kind = ProviderKind::Anthropic;
        c.base_url = Some("https://api.anthropic.com".into());
        c.requires_key = true;
        let secrets = MemorySecretStore::default(); // no key stored
        let cfg = ChatConfig::default();
        let err = build_adapter(&c, &secrets, &cfg)
            .map(|_| ())
            .expect_err("no key stored");
        assert!(err.contains("API key"), "{err}");
    }

    #[test]
    fn build_adapter_anthropic_with_key_ok() {
        let mut c = conn("anthropic-2");
        c.kind = ProviderKind::Anthropic;
        c.requires_key = true;
        let secrets = MemorySecretStore::default();
        secrets.set("anthropic-2", "sk-ant-test").expect("set");
        let cfg = ChatConfig::default();
        assert!(build_adapter(&c, &secrets, &cfg).is_ok());
    }

    #[test]
    fn build_adapter_claude_cli_ok() {
        // Constructing a ClaudeCliAdapter never spawns the CLI itself — the
        // resolve_claude_binary() probe (Windows only) is a cheap `where`
        // lookup, not the CLI. This must be Ok regardless of platform.
        let mut c = conn("cli-1");
        c.kind = ProviderKind::ClaudeCli;
        c.base_url = None;
        c.requires_key = false;
        let secrets = MemorySecretStore::default();
        let cfg = ChatConfig::default();
        assert!(build_adapter(&c, &secrets, &cfg).is_ok());
    }
}
