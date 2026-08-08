use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};

use super::types::RunRecord;

pub struct RunStore {
    runs_dir: PathBuf,
    gate: Mutex<()>,
}

impl RunStore {
    pub fn new(root: &Path) -> Result<Self> {
        let runs_dir = root.join("runs");
        std::fs::create_dir_all(&runs_dir)
            .with_context(|| format!("Failed to create run directory {}", runs_dir.display()))?;
        Ok(Self {
            runs_dir,
            gate: Mutex::new(()),
        })
    }

    pub fn load(&self, run_id: &str) -> Result<Option<RunRecord>> {
        validate_run_id(run_id)?;
        let _guard = self.lock()?;
        self.recover_interrupted_replace(run_id)?;
        let path = self.path(run_id);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("Invalid run record at {}", path.display()))
                .map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    pub fn save(&self, record: &RunRecord) -> Result<()> {
        validate_run_id(&record.run_id)?;
        let _guard = self.lock()?;
        self.recover_interrupted_replace(&record.run_id)?;

        let path = self.path(&record.run_id);
        let backup = self.backup_path(&record.run_id);
        let temporary = self.runs_dir.join(format!(
            ".{}-{}-{}.tmp",
            record.run_id,
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let payload = serde_json::to_vec_pretty(record)?;
        if let Err(error) = write_synced_new_file(&temporary, &payload) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }

        // A backup that survived with no canonical record is recovered above.
        // If both exist and the stale backup cannot be removed, stop before
        // touching the known-good canonical record.
        if backup.exists() {
            std::fs::remove_file(&backup)
                .with_context(|| format!("Failed to remove stale {}", backup.display()))?;
        }
        if path.exists() {
            std::fs::rename(&path, &backup).with_context(|| {
                format!(
                    "Failed to move current run record {} to recovery backup {}",
                    path.display(),
                    backup.display()
                )
            })?;
        }

        if let Err(error) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::remove_file(&temporary);
            if backup.exists() && !path.exists() {
                let _ = std::fs::rename(&backup, &path);
            }
            return Err(error).with_context(|| format!("Failed to activate {}", path.display()));
        }

        // From this point the new canonical record is active. Cleanup and a
        // directory fsync may improve durability, but they must not report the
        // already-committed save as failed: a caller retry could replay an
        // external side effect even though its checkpoint is safely readable.
        if backup.exists() {
            if let Err(error) = std::fs::remove_file(&backup) {
                tracing::warn!(
                    layer = "mcp",
                    component = "agent_os_runs",
                    path = %backup.display(),
                    error = %error,
                    "Run record committed; stale recovery backup will be retried later"
                );
            }
        }
        if let Err(error) = sync_directory(&self.runs_dir) {
            tracing::warn!(
                layer = "mcp",
                component = "agent_os_runs",
                path = %self.runs_dir.display(),
                error = %error,
                "Run record committed but directory sync could not be confirmed"
            );
        }
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<RunRecord>> {
        let _guard = self.lock()?;
        self.recover_all_interrupted_replaces()?;
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.runs_dir)
            .with_context(|| format!("Failed to read {}", self.runs_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RunRecord>(&bytes).ok())
            {
                Some(record) => records.push(record),
                None => tracing::warn!(
                    layer = "mcp",
                    component = "agent_os_runs",
                    path = %path.display(),
                    "Skipping unreadable agent run record"
                ),
            }
        }
        records.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        records.truncate(limit.clamp(1, 100));
        Ok(records)
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>> {
        self.gate
            .lock()
            .map_err(|_| anyhow::anyhow!("run store lock was poisoned by a previous panic"))
    }

    fn recover_all_interrupted_replaces(&self) -> Result<()> {
        for entry in std::fs::read_dir(&self.runs_dir)
            .with_context(|| format!("Failed to read {}", self.runs_dir.display()))?
        {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(run_id) = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".backup"))
            else {
                continue;
            };
            if validate_run_id(run_id).is_ok() {
                self.recover_interrupted_replace(run_id)?;
            }
        }
        Ok(())
    }

    fn recover_interrupted_replace(&self, run_id: &str) -> Result<()> {
        let path = self.path(run_id);
        let backup = self.backup_path(run_id);
        match (path.exists(), backup.exists()) {
            (true, true) => {
                if let Err(error) = std::fs::remove_file(&backup) {
                    tracing::warn!(
                        layer = "mcp",
                        component = "agent_os_runs",
                        path = %backup.display(),
                        error = %error,
                        "Canonical run record is valid; stale recovery backup could not be removed"
                    );
                }
            }
            (false, true) => {
                std::fs::rename(&backup, &path).with_context(|| {
                    format!(
                        "Failed to recover interrupted run update {} from {}",
                        path.display(),
                        backup.display()
                    )
                })?;
                if let Err(error) = sync_directory(&self.runs_dir) {
                    tracing::warn!(
                        layer = "mcp",
                        component = "agent_os_runs",
                        path = %self.runs_dir.display(),
                        error = %error,
                        "Recovered run record but directory sync could not be confirmed"
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn path(&self, run_id: &str) -> PathBuf {
        self.runs_dir.join(format!("{run_id}.json"))
    }

    fn backup_path(&self, run_id: &str) -> PathBuf {
        self.runs_dir.join(format!(".{run_id}.backup"))
    }
}

fn write_synced_new_file(path: &Path, payload: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    file.write_all(payload)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || run_id.len() > 96
        || !run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("run_id must contain only letters, numbers, '-' or '_' (maximum 96 characters)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_os::types::RunRecord;

    fn record(run_id: &str, goal: &str) -> RunRecord {
        let now = chrono::Utc::now();
        RunRecord {
            run_id: run_id.into(),
            goal: goal.into(),
            status: "running".into(),
            created_at: now,
            updated_at: now,
            steps: vec![],
            results: vec![],
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        store.save(&record("run-test", "test")).expect("save");
        assert_eq!(store.load("run-test").expect("load").unwrap().goal, "test");
    }

    #[test]
    fn repeated_save_replaces_record_without_leaving_backup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        store.save(&record("run-test", "before")).expect("first");
        store.save(&record("run-test", "after")).expect("second");
        assert_eq!(store.load("run-test").expect("load").unwrap().goal, "after");
        assert!(!store.backup_path("run-test").exists());
    }

    #[test]
    fn interrupted_replace_recovers_last_complete_record() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        store.save(&record("run-test", "durable")).expect("save");
        std::fs::rename(store.path("run-test"), store.backup_path("run-test"))
            .expect("simulate interrupted replace");
        assert_eq!(
            store.load("run-test").expect("load").unwrap().goal,
            "durable"
        );
        assert!(store.path("run-test").exists());
        assert!(!store.backup_path("run-test").exists());
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_run_id("../escape").is_err());
    }
}
