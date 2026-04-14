//! # Automations (scheduled tasks)
//!
//! Persistent JSON-backed list of scheduled tasks. Each automation is a
//! wake-context template that fires at a given cron expression or at a
//! single absolute time.
//!
//! This module is the **data layer** — the execution side (spawning a wake
//! or worker when a timer fires) is wired into the runtime loop. For now we
//! expose CRUD + tick; full cron evaluation and wake routing land with
//! Phase 8.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One scheduled task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Automation {
    pub id: String,
    pub task: String,
    pub kind: AutomationKind,
    pub schedule: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationKind {
    OneShot,
    Recurring,
}

/// Serializable form posted by the dashboard when creating/updating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationInput {
    pub task: String,
    pub kind: AutomationKind,
    pub schedule: String,
    #[serde(default = "true_fn")]
    pub enabled: bool,
}

fn true_fn() -> bool {
    true
}

/// Shared automations store backed by a JSON file on disk.
#[derive(Clone)]
pub struct AutomationStore {
    items: Arc<RwLock<Vec<Automation>>>,
    path: PathBuf,
}

impl AutomationStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let items = if path.exists() {
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            if contents.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str::<Vec<Automation>>(&contents)
                    .with_context(|| format!("parse {}", path.display()))?
            }
        } else {
            Vec::new()
        };
        Ok(Self {
            items: Arc::new(RwLock::new(items)),
            path,
        })
    }

    pub fn list(&self) -> Vec<Automation> {
        self.items.read().clone()
    }

    pub fn create(&self, input: AutomationInput) -> Result<Automation> {
        let automation = Automation {
            id: Uuid::new_v4().to_string(),
            task: input.task,
            kind: input.kind,
            schedule: input.schedule,
            enabled: input.enabled,
            created_at: Utc::now(),
            last_run: None,
            next_run: None,
            last_status: None,
        };
        self.items.write().push(automation.clone());
        self.persist()?;
        Ok(automation)
    }

    pub fn update(&self, id: &str, input: AutomationInput) -> Result<Automation> {
        let mut guard = self.items.write();
        let item = guard
            .iter_mut()
            .find(|a| a.id == id)
            .context("automation not found")?;
        item.task = input.task;
        item.kind = input.kind;
        item.schedule = input.schedule;
        item.enabled = input.enabled;
        let cloned = item.clone();
        drop(guard);
        self.persist()?;
        Ok(cloned)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        {
            let mut guard = self.items.write();
            let before = guard.len();
            guard.retain(|a| a.id != id);
            if guard.len() == before {
                anyhow::bail!("automation {id} not found");
            }
        }
        self.persist()
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        {
            let mut guard = self.items.write();
            let item = guard
                .iter_mut()
                .find(|a| a.id == id)
                .context("automation not found")?;
            item.enabled = enabled;
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let guard = self.items.read();
        let json = serde_json::to_string_pretty(&*guard)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        write_atomic(&self.path, &json)
    }
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_and_list_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.json");
        let store = AutomationStore::open(&path).unwrap();
        let created = store
            .create(AutomationInput {
                task: "briefing".into(),
                kind: AutomationKind::Recurring,
                schedule: "0 8 * * *".into(),
                enabled: true,
            })
            .unwrap();

        let reopened = AutomationStore::open(&path).unwrap();
        let listed = reopened.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].task, "briefing");
    }

    #[test]
    fn delete_missing_errors() {
        let tmp = TempDir::new().unwrap();
        let store = AutomationStore::open(tmp.path().join("a.json")).unwrap();
        assert!(store.delete("no-such-id").is_err());
    }

    #[test]
    fn update_changes_fields() {
        let tmp = TempDir::new().unwrap();
        let store = AutomationStore::open(tmp.path().join("a.json")).unwrap();
        let a = store
            .create(AutomationInput {
                task: "t1".into(),
                kind: AutomationKind::OneShot,
                schedule: "2026-05-01T10:00:00Z".into(),
                enabled: true,
            })
            .unwrap();
        store
            .update(
                &a.id,
                AutomationInput {
                    task: "t2".into(),
                    kind: AutomationKind::Recurring,
                    schedule: "0 9 * * *".into(),
                    enabled: false,
                },
            )
            .unwrap();
        let listed = store.list();
        assert_eq!(listed[0].task, "t2");
        assert_eq!(listed[0].kind, AutomationKind::Recurring);
        assert!(!listed[0].enabled);
    }
}
