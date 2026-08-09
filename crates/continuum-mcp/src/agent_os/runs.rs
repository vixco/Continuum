use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::types::RunRecord;

const RUN_LEASE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExecutionOwner {
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}

#[derive(Clone, Copy, Debug)]
struct ActiveExecution {
    owner: ExecutionOwner,
    last_seen: Instant,
}

#[derive(Default)]
struct RunStoreState {
    active: BTreeMap<String, ActiveExecution>,
}

#[derive(Debug)]
struct LeaseTransition {
    owner: ExecutionOwner,
    previous: Option<ActiveExecution>,
    touched: bool,
    release_after_commit: bool,
}

pub struct RunStore {
    runs_dir: PathBuf,
    gate: Mutex<RunStoreState>,
}

impl RunStore {
    pub fn new(root: &Path) -> Result<Self> {
        let runs_dir = root.join("runs");
        std::fs::create_dir_all(&runs_dir)
            .with_context(|| format!("Failed to create run directory {}", runs_dir.display()))?;
        Ok(Self {
            runs_dir,
            gate: Mutex::new(RunStoreState::default()),
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
        let mut state = self.lock()?;
        self.recover_interrupted_replace(&record.run_id)?;
        let transition = prepare_execution_transition(&mut state, record)?;

        match self.save_record_locked(record) {
            Ok(()) => {
                finish_execution_transition(&mut state, &record.run_id, &transition);
                Ok(())
            }
            Err(error) => {
                rollback_execution_transition(&mut state, &record.run_id, &transition);
                Err(error)
            }
        }
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

    fn save_record_locked(&self, record: &RunRecord) -> Result<()> {
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

        // A backup that survived with no canonical record is recovered before
        // this method is called. If both exist and the stale backup cannot be
        // removed, stop before touching the known-good canonical record.
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

    fn lock(&self) -> Result<MutexGuard<'_, RunStoreState>> {
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

fn current_execution_owner() -> ExecutionOwner {
    tokio::task::try_id()
        .map(ExecutionOwner::Task)
        .unwrap_or_else(|| ExecutionOwner::Thread(std::thread::current().id()))
}

fn prepare_execution_transition(
    state: &mut RunStoreState,
    record: &RunRecord,
) -> Result<LeaseTransition> {
    let owner = current_execution_owner();
    let now = Instant::now();
    let previous = state.active.get(&record.run_id).copied();

    if record.status == "running" {
        if let Some(active) = previous {
            let age = now.saturating_duration_since(active.last_seen);
            if active.owner != owner && age < RUN_LEASE_TTL {
                bail!(
                    "run_id {} is already executing in another task; inspect the existing run and evidence before retrying",
                    record.run_id
                );
            }
            if active.owner != owner {
                tracing::warn!(
                    layer = "mcp",
                    component = "agent_os_runs",
                    run_id = %record.run_id,
                    lease_age_ms = age.as_millis(),
                    "Taking over an expired Agent OS execution lease"
                );
            }
        }
        state.active.insert(
            record.run_id.clone(),
            ActiveExecution {
                owner,
                last_seen: now,
            },
        );
        Ok(LeaseTransition {
            owner,
            previous,
            touched: true,
            release_after_commit: false,
        })
    } else {
        if let Some(active) = previous {
            let age = now.saturating_duration_since(active.last_seen);
            if active.owner != owner && age < RUN_LEASE_TTL {
                bail!(
                    "run_id {} is still owned by another active task and cannot transition to status {:?}",
                    record.run_id,
                    record.status
                );
            }
        }
        Ok(LeaseTransition {
            owner,
            previous,
            touched: false,
            release_after_commit: true,
        })
    }
}

fn finish_execution_transition(
    state: &mut RunStoreState,
    run_id: &str,
    transition: &LeaseTransition,
) {
    if transition.release_after_commit {
        state.active.remove(run_id);
    } else if let Some(active) = state.active.get_mut(run_id) {
        if active.owner == transition.owner {
            active.last_seen = Instant::now();
        }
    }
}

fn rollback_execution_transition(
    state: &mut RunStoreState,
    run_id: &str,
    transition: &LeaseTransition,
) {
    if !transition.touched {
        return;
    }
    match transition.previous {
        Some(previous) => {
            state.active.insert(run_id.to_string(), previous);
        }
        None => {
            state.active.remove(run_id);
        }
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
    use std::sync::Arc;

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

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_task_cannot_reenter_or_finish_active_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(RunStore::new(temp.path()).expect("store"));
        let mut active = record("run-shared", "one side effect at a time");
        store.save(&active).expect("claim active run");

        let contender = Arc::clone(&store);
        let running_result = tokio::spawn(async move {
            contender.save(&record("run-shared", "one side effect at a time"))
        })
        .await
        .expect("join contender");
        assert!(running_result
            .expect_err("parallel running save must be rejected")
            .to_string()
            .contains("already executing"));

        let contender = Arc::clone(&store);
        let terminal_result = tokio::spawn(async move {
            let mut terminal = record("run-shared", "one side effect at a time");
            terminal.status = "completed".into();
            contender.save(&terminal)
        })
        .await
        .expect("join terminal contender");
        assert!(terminal_result
            .expect_err("foreign terminal save must be rejected")
            .to_string()
            .contains("still owned"));

        active.status = "completed".into();
        active.updated_at = chrono::Utc::now();
        store.save(&active).expect("owner releases run");

        let contender = Arc::clone(&store);
        let resumed_result = tokio::spawn(async move {
            contender.save(&record("run-shared", "one side effect at a time"))
        })
        .await
        .expect("join resumed contender");
        assert!(resumed_result.is_ok());
    }

    #[test]
    fn expired_lease_can_be_reclaimed() {
        let other_thread = std::thread::spawn(|| std::thread::current().id())
            .join()
            .expect("thread id");
        let mut state = RunStoreState::default();
        state.active.insert(
            "run-stale".into(),
            ActiveExecution {
                owner: ExecutionOwner::Thread(other_thread),
                last_seen: Instant::now()
                    .checked_sub(RUN_LEASE_TTL + Duration::from_secs(1))
                    .expect("past instant"),
            },
        );
        let transition = prepare_execution_transition(
            &mut state,
            &record("run-stale", "recover abandoned execution"),
        )
        .expect("reclaim stale lease");
        assert!(transition.touched);
        assert_eq!(
            state.active.get("run-stale").expect("active").owner,
            current_execution_owner()
        );
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_run_id("../escape").is_err());
    }
}
