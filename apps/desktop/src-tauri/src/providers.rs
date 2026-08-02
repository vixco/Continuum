//! Provider connections: JSON store (no secrets) + OS credential store.
//!
//! `providers.json` lives in the Continuum dev dir and is written atomically
//! (tmp + rename, same pattern as voice intents). API keys go exclusively
//! into the OS credential store via `keyring` (service "Continuum").
#![allow(dead_code)] // TODO(task-9): remove — commands wire these up

use std::path::PathBuf;

use anyhow::{Context, Result};
use continuum_gateway::ProviderConnection;

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
}
