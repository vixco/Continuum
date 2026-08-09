use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
#[cfg(windows)]
use windows::{
    core::w,
    Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    },
};

use super::composio::{classify_execute_request, classify_meta_tool};
use super::types::{
    ComposioExecuteRequest, ComposioMetaExecuteRequest, PlanStep, RiskLevel, RunRecord,
};

const RUN_LEASE_TTL: Duration = Duration::from_secs(30 * 60);
#[cfg(windows)]
const DPAPI_MAGIC: &[u8] = b"CONTINUUM-RUN-DPAPI-V1\0";
#[cfg(windows)]
const DPAPI_ENTROPY: &[u8] = b"Continuum Agent OS run record v1";

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
        restrict_directory_permissions(&runs_dir)?;
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
            Ok(bytes) => decode_run_record(&path, &bytes).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    pub fn save(&self, record: &RunRecord) -> Result<()> {
        validate_run_record(record)?;
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
                .and_then(|bytes| decode_run_record(&path, &bytes).ok())
            {
                Some(record) => records.push(record),
                None => tracing::warn!(
                    layer = "mcp",
                    component = "agent_os_runs",
                    path = %path.display(),
                    "Skipping unreadable Agent OS run record"
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
        let payload = encode_run_record(record)?;
        if let Err(error) = write_synced_new_file(&temporary, &payload) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }

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

        // The canonical checkpoint is active. Cleanup failures must not turn a
        // committed side effect into a reported save failure and provoke a retry.
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

fn record_holds_execution_lease(record: &RunRecord) -> bool {
    if record.status == "running" {
        return true;
    }
    if record.status != "failed" {
        return false;
    }

    // execute_plan persists a failed step before inspecting continue_on_error.
    // Keep ownership while a continued plan still has later steps, otherwise a
    // second task could enter between those two checkpoints and repeat effects.
    record
        .results
        .iter()
        .filter(|result| result.status == "error")
        .max_by_key(|result| result.index)
        .and_then(|result| {
            record
                .steps
                .get(result.index)
                .map(|step| (result.index, step))
        })
        .is_some_and(|(index, step)| step.continue_on_error && index + 1 < record.steps.len())
}

fn prepare_execution_transition(
    state: &mut RunStoreState,
    record: &RunRecord,
) -> Result<LeaseTransition> {
    let owner = current_execution_owner();
    let now = Instant::now();
    let previous = state.active.get(&record.run_id).copied();

    if record_holds_execution_lease(record) {
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

fn validate_run_record(record: &RunRecord) -> Result<()> {
    validate_run_id(&record.run_id)?;
    if record.goal.trim().is_empty() || record.goal.chars().count() > 4_000 {
        bail!(
            "run_id {} goal must contain between 1 and 4,000 characters",
            record.run_id
        );
    }
    if !matches!(
        record.status.as_str(),
        "running" | "failed" | "completed" | "completed_with_errors"
    ) {
        bail!(
            "run_id {} has invalid status {:?}",
            record.run_id,
            record.status
        );
    }
    if record.updated_at < record.created_at {
        bail!("run_id {} has updated_at before created_at", record.run_id);
    }

    let mut step_ids = BTreeSet::new();
    for (index, step) in record.steps.iter().enumerate() {
        let step_id = effective_step_id(step, index);
        validate_step_id(&step_id)?;
        if !step_ids.insert(step_id.clone()) {
            bail!(
                "run_id {} contains duplicate step id {step_id:?}",
                record.run_id
            );
        }
        let action = step.action.trim();
        if action.is_empty() || action.len() > 128 {
            bail!("step {step_id:?} has an invalid action name");
        }
        if step
            .expectation
            .as_deref()
            .is_some_and(|value| value.chars().count() > 4_000)
        {
            bail!("step {step_id:?} expectation exceeds 4,000 characters");
        }
        if step.continue_on_error && step_risk(step)? == RiskLevel::Destructive {
            bail!("destructive step {step_id:?} cannot set continue_on_error=true");
        }
    }

    let mut result_indices = BTreeSet::new();
    for result in &record.results {
        let Some(step) = record.steps.get(result.index) else {
            bail!(
                "run_id {} contains a result for missing step index {}",
                record.run_id,
                result.index
            );
        };
        if !result_indices.insert(result.index) {
            bail!(
                "run_id {} contains duplicate results for step index {}",
                record.run_id,
                result.index
            );
        }
        let expected_id = effective_step_id(step, result.index);
        if result.id != expected_id || result.action != step.action {
            bail!(
                "run_id {} result metadata does not match immutable step {}",
                record.run_id,
                result.index
            );
        }
        match result.status.as_str() {
            "success" if result.error.is_none() => {}
            "error"
                if result
                    .error
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()) => {}
            "success" => bail!(
                "run_id {} success result {} unexpectedly contains an error",
                record.run_id,
                result.index
            ),
            "error" => bail!(
                "run_id {} error result {} has no error message",
                record.run_id,
                result.index
            ),
            other => bail!(
                "run_id {} result {} has invalid status {other:?}",
                record.run_id,
                result.index
            ),
        }
    }

    match record.status.as_str() {
        "completed" => {
            if record.results.len() != record.steps.len()
                || record
                    .results
                    .iter()
                    .any(|result| result.status != "success")
            {
                bail!(
                    "completed run_id {} must contain one successful result per step",
                    record.run_id
                );
            }
        }
        "completed_with_errors" => {
            if record.results.len() != record.steps.len()
                || !record.results.iter().any(|result| result.status == "error")
            {
                bail!(
                    "completed_with_errors run_id {} must contain all step results and at least one error",
                    record.run_id
                );
            }
        }
        "failed" if !record.results.iter().any(|result| result.status == "error") => {
            bail!("failed run_id {} has no failed step result", record.run_id);
        }
        _ => {}
    }
    Ok(())
}

fn effective_step_id(step: &PlanStep, index: usize) -> String {
    step.id
        .clone()
        .unwrap_or_else(|| format!("step_{}", index + 1))
}

fn validate_step_id(step_id: &str) -> Result<()> {
    if step_id.is_empty()
        || step_id.len() > 96
        || !step_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("step ids may contain only letters, numbers, '-' or '_' (maximum 96 characters)");
    }
    Ok(())
}

fn step_risk(step: &PlanStep) -> Result<RiskLevel> {
    match step.action.trim() {
        "composio_execute" => {
            let request: ComposioExecuteRequest = serde_json::from_value(step.arguments.clone())
                .context("Invalid composio_execute arguments in persisted plan")?;
            Ok(classify_execute_request(&request))
        }
        "composio_execute_meta" => {
            let request: ComposioMetaExecuteRequest =
                serde_json::from_value(step.arguments.clone())
                    .context("Invalid composio_execute_meta arguments in persisted plan")?;
            Ok(classify_meta_tool(&request.meta_tool, &request.arguments))
        }
        _ => Ok(RiskLevel::Write),
    }
}

fn encode_run_record(record: &RunRecord) -> Result<Vec<u8>> {
    let mut plaintext = serde_json::to_vec_pretty(record)?;
    let result = protect_run_payload(&mut plaintext);
    plaintext.fill(0);
    result
}

fn decode_run_record(path: &Path, payload: &[u8]) -> Result<RunRecord> {
    let mut plaintext = unprotect_run_payload(payload)
        .with_context(|| format!("Failed to decrypt Agent OS run record {}", path.display()))?;
    let parsed = serde_json::from_slice::<RunRecord>(&plaintext)
        .with_context(|| format!("Invalid run record at {}", path.display()));
    plaintext.fill(0);
    let record = parsed?;
    validate_run_record(&record)
        .with_context(|| format!("Invalid run invariants at {}", path.display()))?;
    Ok(record)
}

#[cfg(windows)]
fn protect_run_payload(plaintext: &mut [u8]) -> Result<Vec<u8>> {
    let input = crypto_blob(plaintext, "run record")?;
    let mut entropy_bytes = DPAPI_ENTROPY.to_vec();
    let entropy = crypto_blob(&mut entropy_bytes, "DPAPI entropy")?;
    let mut output = LocalCryptoBlob::default();

    unsafe {
        CryptProtectData(
            &input,
            w!("Continuum Agent OS run record"),
            Some(&entropy as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    }
    .context("Windows DPAPI could not protect the Agent OS run record")?;

    let protected = output.copy_bytes("protected run record")?;
    let mut envelope = Vec::with_capacity(DPAPI_MAGIC.len() + protected.len());
    envelope.extend_from_slice(DPAPI_MAGIC);
    envelope.extend_from_slice(&protected);
    Ok(envelope)
}

#[cfg(not(windows))]
fn protect_run_payload(plaintext: &mut [u8]) -> Result<Vec<u8>> {
    Ok(plaintext.to_vec())
}

#[cfg(windows)]
fn unprotect_run_payload(payload: &[u8]) -> Result<Vec<u8>> {
    let Some(ciphertext) = payload.strip_prefix(DPAPI_MAGIC) else {
        // Compatibility with records written before DPAPI protection. The next
        // successful save transparently migrates the record to the new envelope.
        return Ok(payload.to_vec());
    };
    if ciphertext.is_empty() {
        bail!("DPAPI run-record envelope contains no ciphertext");
    }

    let mut ciphertext = ciphertext.to_vec();
    let input = crypto_blob(&mut ciphertext, "encrypted run record")?;
    let mut entropy_bytes = DPAPI_ENTROPY.to_vec();
    let entropy = crypto_blob(&mut entropy_bytes, "DPAPI entropy")?;
    let mut output = LocalCryptoBlob::default();

    let result = unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output.0,
        )
    }
    .context("Windows DPAPI could not decrypt the Agent OS run record");
    ciphertext.fill(0);
    result?;
    output.copy_bytes("decrypted run record")
}

#[cfg(not(windows))]
fn unprotect_run_payload(payload: &[u8]) -> Result<Vec<u8>> {
    Ok(payload.to_vec())
}

#[cfg(windows)]
fn crypto_blob(bytes: &mut [u8], label: &str) -> Result<CRYPT_INTEGER_BLOB> {
    let length = u32::try_from(bytes.len())
        .with_context(|| format!("{label} is too large for Windows DPAPI"))?;
    if length == 0 {
        bail!("{label} is empty");
    }
    Ok(CRYPT_INTEGER_BLOB {
        cbData: length,
        pbData: bytes.as_mut_ptr(),
    })
}

#[cfg(windows)]
#[derive(Default)]
struct LocalCryptoBlob(CRYPT_INTEGER_BLOB);

#[cfg(windows)]
impl LocalCryptoBlob {
    fn copy_bytes(&self, label: &str) -> Result<Vec<u8>> {
        if self.0.cbData == 0 || self.0.pbData.is_null() {
            bail!("Windows DPAPI returned an empty {label}");
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(self.0.pbData.cast_const(), self.0.cbData as usize)
        };
        Ok(bytes.to_vec())
    }
}

#[cfg(windows)]
impl Drop for LocalCryptoBlob {
    fn drop(&mut self) {
        if self.0.pbData.is_null() {
            return;
        }
        let remaining = unsafe { LocalFree(HLOCAL(self.0.pbData.cast())) };
        if !remaining.0.is_null() {
            tracing::warn!(
                layer = "mcp",
                component = "agent_os_runs",
                "Windows LocalFree could not release a DPAPI output buffer"
            );
        }
        self.0.pbData = std::ptr::null_mut();
        self.0.cbData = 0;
    }
}

fn write_synced_new_file(path: &Path, payload: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    file.write_all(payload)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))
}

#[cfg(unix)]
fn restrict_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("Failed to restrict {} permissions", path.display()))
}

#[cfg(not(unix))]
fn restrict_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
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
    use crate::agent_os::types::{PlanStepResult, RunRecord};

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

    fn wait_step(id: &str, continue_on_error: bool) -> PlanStep {
        PlanStep {
            id: Some(id.into()),
            action: "computer_wait".into(),
            arguments: serde_json::json!({"milliseconds": 1}),
            expectation: None,
            continue_on_error,
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

    #[tokio::test(flavor = "current_thread")]
    async fn continued_error_keeps_lease_until_final_checkpoint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(RunStore::new(temp.path()).expect("store"));
        let mut active = record("run-continued", "continue independent work");
        active.steps = vec![wait_step("first", true), wait_step("second", false)];
        store.save(&active).expect("claim run");

        active.status = "failed".into();
        active.results.push(PlanStepResult {
            index: 0,
            id: "first".into(),
            action: "computer_wait".into(),
            status: "error".into(),
            evidence_id: None,
            result: serde_json::Value::Null,
            error: Some("independent wait failed".into()),
        });
        active.updated_at = chrono::Utc::now();
        store.save(&active).expect("persist continued error");

        let contender = Arc::clone(&store);
        let contender_record = active.clone();
        let result = tokio::spawn(async move { contender.save(&contender_record) })
            .await
            .expect("join contender");
        assert!(result
            .expect_err("continued plan must retain the lease")
            .to_string()
            .contains("already executing"));

        active.results.push(PlanStepResult {
            index: 1,
            id: "second".into(),
            action: "computer_wait".into(),
            status: "success".into(),
            evidence_id: None,
            result: serde_json::json!({"waited_ms": 1}),
            error: None,
        });
        active.status = "completed_with_errors".into();
        active.updated_at = chrono::Utc::now();
        store.save(&active).expect("release at final checkpoint");
    }

    #[test]
    fn destructive_steps_cannot_continue_after_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        let mut unsafe_record = record("run-unsafe", "delete and continue");
        unsafe_record.steps.push(PlanStep {
            id: Some("delete".into()),
            action: "composio_execute".into(),
            arguments: serde_json::json!({
                "tool_slug": "SLACK_DELETE_MESSAGE",
                "arguments": {}
            }),
            expectation: None,
            continue_on_error: true,
        });
        assert!(store
            .save(&unsafe_record)
            .expect_err("destructive continuation must fail")
            .to_string()
            .contains("destructive step"));
    }

    #[test]
    fn duplicate_step_ids_are_rejected() {
        let mut invalid = record("run-duplicate", "unique step ids");
        invalid.steps = vec![wait_step("same", false), wait_step("same", false)];
        assert!(validate_run_record(&invalid)
            .expect_err("duplicate ids must fail")
            .to_string()
            .contains("duplicate step id"));
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

    #[cfg(windows)]
    #[test]
    fn windows_run_records_are_dpapi_protected_at_rest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        let secret = "private email body for DPAPI test";
        store
            .save(&record("run-private", secret))
            .expect("save protected record");

        let raw = std::fs::read(store.path("run-private")).expect("read raw record");
        assert!(raw.starts_with(DPAPI_MAGIC));
        assert!(!raw
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
        assert_eq!(
            store.load("run-private").expect("load").unwrap().goal,
            secret
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_loads_legacy_plaintext_and_migrates_on_save() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        let legacy = record("run-legacy", "legacy private payload");
        std::fs::write(
            store.path("run-legacy"),
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy record"),
        )
        .expect("write legacy record");

        let loaded = store
            .load("run-legacy")
            .expect("load legacy")
            .expect("record");
        assert_eq!(loaded.goal, legacy.goal);
        store.save(&loaded).expect("migrate legacy record");
        let migrated = std::fs::read(store.path("run-legacy")).expect("read migrated record");
        assert!(migrated.starts_with(DPAPI_MAGIC));
    }

    #[cfg(unix)]
    #[test]
    fn unix_run_records_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let store = RunStore::new(temp.path()).expect("store");
        store
            .save(&record("run-private", "private"))
            .expect("save record");
        let directory_mode = std::fs::metadata(&store.runs_dir)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(store.path("run-private"))
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_run_id("../escape").is_err());
    }
}
