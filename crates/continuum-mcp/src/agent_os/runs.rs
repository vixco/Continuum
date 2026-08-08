use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
        let _guard = self.gate.lock().expect("run store lock poisoned");
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
        let _guard = self.gate.lock().expect("run store lock poisoned");
        let path = self.path(&record.run_id);
        let temporary = self.runs_dir.join(format!(
            ".{}-{}-{}.tmp",
            record.run_id,
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&temporary, serde_json::to_vec_pretty(record)?)
            .with_context(|| format!("Failed to write {}", temporary.display()))?;
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to replace {}", path.display()))?;
        }
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("Failed to activate {}", path.display()))?;
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<RunRecord>> {
        let _guard = self.gate.lock().expect("run store lock poisoned");
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

    fn path(&self, run_id: &str) -> PathBuf {
        self.runs_dir.join(format!("{run_id}.json"))
    }
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

    #[test]
    fn save_and_load_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        let now = chrono::Utc::now();
        let record = RunRecord {
            run_id: "run-test".into(),
            goal: "test".into(),
            status: "running".into(),
            created_at: now,
            updated_at: now,
            steps: vec![],
            results: vec![],
        };
        store.save(&record).expect("save");
        assert_eq!(store.load("run-test").expect("load").unwrap().goal, "test");
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_run_id("../escape").is_err());
    }
}
