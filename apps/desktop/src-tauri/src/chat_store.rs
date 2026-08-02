//! Conversation persistence: one JSON file per conversation under
//! `<dev_dir>/chats/`. No secrets; safe to back up.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use continuum_gateway::{ChatRole, TokenUsage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const CHATS_SUBDIR: &str = "chats";
const TITLE_MAX_CHARS: usize = 40;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: ChatRole,
    pub content: String,
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(default)]
    pub aborted: bool,
}

impl StoredMessage {
    /// Builds a user-authored message stamped with the current time.
    pub fn user(content: &str) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            ts: Utc::now(),
            model: None,
            duration_ms: None,
            usage: None,
            aborted: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub provider_id: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
}

/// Lightweight view of a [`Conversation`] for list rendering — everything
/// except the message bodies.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub provider_id: String,
    pub model: String,
    pub updated_at: DateTime<Utc>,
    pub message_count: usize,
}

impl Conversation {
    /// Title = first user message, truncated on a char boundary.
    pub fn derive_title(&mut self) {
        if let Some(first) = self.messages.iter().find(|m| m.role == ChatRole::User) {
            let mut t: String = first.content.chars().take(TITLE_MAX_CHARS).collect();
            if first.content.chars().count() > TITLE_MAX_CHARS {
                t.push('…');
            }
            if !t.trim().is_empty() {
                self.title = t.trim().to_string();
            }
        }
    }

    #[cfg(test)]
    fn new_for_test() -> Self {
        let now = Utc::now();
        Self {
            id: "chat-test".into(),
            title: "New chat".into(),
            provider_id: "prov-test".into(),
            model: "model-test".into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
        }
    }
}

/// Reads/writes conversation JSON files under `<dev_dir>/chats/`. Cheap to
/// construct — holds only a `PathBuf`, same shape as `ProviderStore`.
pub struct ChatStore {
    dev_dir: PathBuf,
}

impl ChatStore {
    pub fn new(dev_dir: PathBuf) -> Self {
        Self { dev_dir }
    }

    fn chats_dir(&self) -> PathBuf {
        self.dev_dir.join(CHATS_SUBDIR)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.chats_dir().join(format!("{id}.json"))
    }

    /// Lists every conversation, newest-updated first. Unparseable files are
    /// skipped (warn, never crash the dashboard) — same policy as
    /// `ProviderStore::load`.
    pub fn list(&self) -> Vec<ConversationSummary> {
        let dir = self.chats_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut summaries: Vec<ConversationSummary> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .filter_map(|e| {
                let path = e.path();
                let raw = std::fs::read_to_string(&path).ok()?;
                match serde_json::from_str::<Conversation>(&raw) {
                    Ok(c) => Some(ConversationSummary {
                        id: c.id,
                        title: c.title,
                        provider_id: c.provider_id,
                        model: c.model,
                        updated_at: c.updated_at,
                        message_count: c.messages.len(),
                    }),
                    Err(e) => {
                        tracing::warn!(
                            layer = "desktop",
                            component = "chat_store",
                            error = %e,
                            path = %path.display(),
                            "conversation file unparseable — skipping"
                        );
                        None
                    }
                }
            })
            .collect();
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        summaries
    }

    /// Loads one conversation. Missing or unparseable files both return
    /// `None` (the caller maps this to a user-facing "unknown conversation").
    pub fn get(&self, id: &str) -> Option<Conversation> {
        let raw = std::fs::read_to_string(self.path(id)).ok()?;
        match serde_json::from_str(&raw) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(
                    layer = "desktop",
                    component = "chat_store",
                    error = %e,
                    id,
                    "conversation file unparseable"
                );
                None
            }
        }
    }

    /// Creates a new empty conversation and persists it.
    pub fn create(&self, provider_id: &str, model: &str) -> Result<Conversation> {
        let now = Utc::now();
        let conv = Conversation {
            id: format!("chat-{}", uuid::Uuid::new_v4()),
            title: "New chat".into(),
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
        };
        self.save(&conv)?;
        Ok(conv)
    }

    /// Writes a conversation atomically (tmp + rename), stamping
    /// `updated_at` to now.
    pub fn save(&self, conv: &Conversation) -> Result<()> {
        let dir = self.chats_dir();
        std::fs::create_dir_all(&dir).context("create chats dir")?;
        let mut stamped = conv.clone();
        stamped.updated_at = Utc::now();
        let path = self.path(&conv.id);
        let tmp = path.with_extension("json.tmp");
        let payload = serde_json::to_string_pretty(&stamped).context("serialize conversation")?;
        std::fs::write(&tmp, payload).context("write conversation tmp")?;
        std::fs::rename(&tmp, &path).context("rename conversation file")?;
        Ok(())
    }

    /// Deletes a conversation file. Missing file is not an error.
    pub fn delete(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("delete conversation file"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_get_append_delete() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = ChatStore::new(dir.path().to_path_buf());
        assert!(store.list().is_empty());
        let conv = store.create("prov-1", "m1").expect("create");
        assert_eq!(conv.title, "New chat");
        let mut loaded = store.get(&conv.id).expect("get");
        loaded
            .messages
            .push(StoredMessage::user("hallo wereld dit is een test"));
        loaded.derive_title();
        store.save(&loaded).expect("save");
        assert_eq!(store.get(&conv.id).expect("get").messages.len(), 1);
        // title derived from first user message, truncated
        assert!(store.list()[0].title.starts_with("hallo wereld"));
        store.delete(&conv.id).expect("delete");
        assert!(store.get(&conv.id).is_none());
    }

    #[test]
    fn title_truncates_on_char_boundary() {
        let mut c = Conversation::new_for_test();
        c.messages.push(StoredMessage::user(&"é".repeat(100)));
        c.derive_title();
        assert!(c.title.chars().count() <= 41); // 40 + ellipsis
    }
}
