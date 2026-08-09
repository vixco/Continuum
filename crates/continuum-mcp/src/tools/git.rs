//! # Safe Git checkpoint tools (`mcp__continuum__git_*`)
//!
//! Checkpoints are commits stored below `refs/continuum/checkpoints/`. A
//! temporary index captures tracked and untracked non-secret files without
//! touching the user's real index or working tree. Rollback first creates a
//! safety checkpoint and moves untracked files into `.git/continuum-recovery`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

use crate::allowlist::{is_path_allowed, is_relative_path_denied, AllowlistConfig};

/// Default maximum diff payload returned to an agent.
pub const DEFAULT_DIFF_MAX_BYTES: usize = 200 * 1024;
/// Hard upper bound for a diff response.
pub const DIFF_MAX_BYTES: usize = 1024 * 1024;

/// Input for `git_checkpoint`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitCheckpointRequest {
    /// Allowlisted path inside the repository.
    pub repo: String,
    /// Human-readable reason, capped before it becomes a commit message.
    pub message: Option<String>,
}

/// Input for `git_diff`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitDiffRequest {
    /// Allowlisted path inside the repository.
    pub repo: String,
    /// Optional Continuum checkpoint id to compare against.
    pub checkpoint_id: Option<String>,
    /// Maximum UTF-8 bytes returned, capped at 1 MiB.
    pub max_bytes: Option<usize>,
}

/// Input for `git_checkpoint_list`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitCheckpointListRequest {
    /// Allowlisted path inside the repository.
    pub repo: String,
    /// Maximum rows, clamped to 100.
    pub limit: Option<usize>,
}

/// Input for `git_rollback`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GitRollbackRequest {
    /// Allowlisted path inside the repository.
    pub repo: String,
    /// Exact Continuum checkpoint id to restore.
    pub checkpoint_id: String,
}

/// A durable Continuum checkpoint.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitCheckpoint {
    /// Stable checkpoint id used by diff and rollback.
    pub id: String,
    /// Full Git ref.
    pub reference: String,
    /// Commit object id.
    pub commit: String,
    /// Canonical repository root.
    pub repo: String,
    /// UTC creation timestamp.
    pub created_at: String,
    /// Number of secret/denied paths deliberately excluded.
    pub excluded_sensitive_paths: usize,
}

/// Diff and status snapshot.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitDiffResponse {
    /// Canonical repository root.
    pub repo: String,
    /// Porcelain status, including untracked names.
    pub status: String,
    /// Unified diff for tracked content.
    pub diff: String,
    /// Whether the diff was truncated.
    pub truncated: bool,
}

/// Result of a recoverable rollback.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GitRollbackResponse {
    /// Checkpoint that was restored.
    pub restored_checkpoint_id: String,
    /// Automatic checkpoint of the pre-rollback state.
    pub safety_checkpoint: GitCheckpoint,
    /// Recovery directory for moved/copied files.
    pub recovery_dir: String,
    /// Number of untracked files moved out of the worktree.
    pub recovered_untracked_files: usize,
    /// Number of modified sensitive tracked files copied before reset.
    pub recovered_sensitive_files: usize,
}

/// Creates a checkpoint without changing the real index or working tree.
pub async fn checkpoint(
    request: &GitCheckpointRequest,
    allowlist: &AllowlistConfig,
    timeout_ms: u64,
) -> Result<GitCheckpoint, GitToolError> {
    let runner = GitRunner::resolve(&request.repo, allowlist, timeout_ms).await?;
    create_checkpoint(&runner, request.message.as_deref()).await
}

/// Returns tracked diff plus porcelain status.
pub async fn diff(
    request: &GitDiffRequest,
    allowlist: &AllowlistConfig,
    timeout_ms: u64,
) -> Result<GitDiffResponse, GitToolError> {
    let runner = GitRunner::resolve(&request.repo, allowlist, timeout_ms).await?;
    let status_raw = runner.run(&["status", "--porcelain=v1"], &[]).await?;
    let status = filter_status(&status_raw);
    let mut args = vec![
        "diff".to_string(),
        "--no-ext-diff".to_string(),
        "--no-color".to_string(),
        "--unified=3".to_string(),
    ];
    let reference = if let Some(id) = request.checkpoint_id.as_deref() {
        validate_checkpoint_id(id)?;
        let value = checkpoint_ref(id);
        runner.verify_ref(&value).await?;
        Some(value)
    } else if runner
        .run(&["rev-parse", "--verify", "--quiet", "HEAD"], &[])
        .await
        .is_ok()
    {
        Some("HEAD".to_string())
    } else {
        None
    };
    let paths = if let Some(value) = reference.as_deref() {
        changed_since(&runner, value).await?
    } else {
        BTreeSet::new()
    };
    if let Some(value) = reference {
        args.push(value);
    }
    let safe_paths: Vec<String> = paths
        .into_iter()
        .filter(|path| is_relative_path_denied(path).is_ok())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let raw = if safe_paths.is_empty() {
        String::new()
    } else {
        args.push("--".to_string());
        args.extend(safe_paths);
        runner.run_owned(&args, &[]).await?
    };
    let max = request
        .max_bytes
        .unwrap_or(DEFAULT_DIFF_MAX_BYTES)
        .clamp(1024, DIFF_MAX_BYTES);
    let (diff, truncated) = truncate_utf8(&raw, max);
    Ok(GitDiffResponse {
        repo: runner.repo.to_string_lossy().into_owned(),
        status,
        diff,
        truncated,
    })
}

/// Lists Continuum checkpoint refs, newest first.
pub async fn list_checkpoints(
    request: &GitCheckpointListRequest,
    allowlist: &AllowlistConfig,
    timeout_ms: u64,
) -> Result<Vec<GitCheckpoint>, GitToolError> {
    let runner = GitRunner::resolve(&request.repo, allowlist, timeout_ms).await?;
    let limit = request.limit.unwrap_or(25).clamp(1, 100);
    let format = "%(refname:short)%00%(objectname)%00%(creatordate:iso-strict)";
    let raw = runner
        .run(
            &[
                "for-each-ref",
                "--sort=-creatordate",
                &format!("--count={limit}"),
                &format!("--format={format}"),
                "refs/continuum/checkpoints/",
            ],
            &[],
        )
        .await?;
    Ok(raw
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let short = fields.next()?;
            let commit = fields.next()?;
            let created_at = fields.next()?.to_string();
            let id = short.rsplit('/').next()?.to_string();
            Some(GitCheckpoint {
                id,
                reference: format!("refs/{short}"),
                commit: commit.to_string(),
                repo: runner.repo.to_string_lossy().into_owned(),
                created_at,
                excluded_sensitive_paths: 0,
            })
        })
        .collect())
}

/// Restores a checkpoint after preserving the current state and loose files.
pub async fn rollback(
    request: &GitRollbackRequest,
    allowlist: &AllowlistConfig,
    timeout_ms: u64,
) -> Result<GitRollbackResponse, GitToolError> {
    validate_checkpoint_id(&request.checkpoint_id)?;
    let runner = GitRunner::resolve(&request.repo, allowlist, timeout_ms).await?;
    let reference = checkpoint_ref(&request.checkpoint_id);
    runner.verify_ref(&reference).await?;
    let safety =
        create_checkpoint(&runner, Some("automatic pre-rollback safety checkpoint")).await?;
    let recovery_dir = runner.git_dir.join("continuum-recovery").join(&safety.id);
    tokio::fs::create_dir_all(&recovery_dir)
        .await
        .map_err(|error| GitToolError::Io(error.to_string()))?;

    let untracked = nul_paths(
        &runner
            .run_bytes(&["ls-files", "--others", "--exclude-standard", "-z"], &[])
            .await?,
    );
    let mut moved = 0;
    for relative in untracked {
        validate_relative(&relative)?;
        let source = runner.repo.join(&relative);
        if source.is_file() {
            let target = recovery_dir.join("untracked").join(&relative);
            move_file(&source, &target).await?;
            moved += 1;
        }
    }

    let changed = changed_paths(&runner).await?;
    let mut copied_sensitive = 0;
    for relative in changed {
        if is_relative_path_denied(&relative).is_ok() {
            continue;
        }
        let source = runner.repo.join(&relative);
        if source.is_file() {
            let target = recovery_dir.join("sensitive-tracked").join(&relative);
            copy_file(&source, &target).await?;
            copied_sensitive += 1;
        }
    }

    runner.run(&["reset", "--hard", &reference], &[]).await?;
    Ok(GitRollbackResponse {
        restored_checkpoint_id: request.checkpoint_id.clone(),
        safety_checkpoint: safety,
        recovery_dir: recovery_dir.to_string_lossy().into_owned(),
        recovered_untracked_files: moved,
        recovered_sensitive_files: copied_sensitive,
    })
}

#[derive(Debug)]
struct GitRunner {
    repo: PathBuf,
    git_dir: PathBuf,
    timeout: Duration,
}

impl GitRunner {
    async fn resolve(
        requested: &str,
        allowlist: &AllowlistConfig,
        timeout_ms: u64,
    ) -> Result<Self, GitToolError> {
        let allowed = is_path_allowed(Path::new(requested), allowlist)
            .map_err(|error| GitToolError::Denied(error.to_string()))?;
        let timeout_duration = Duration::from_millis(timeout_ms.clamp(250, 30_000));
        let root = run_git_at(
            &allowed,
            &["rev-parse", "--show-toplevel"],
            &[],
            timeout_duration,
        )
        .await?;
        let repo = is_path_allowed(Path::new(root.trim()), allowlist)
            .map_err(|error| GitToolError::Denied(error.to_string()))?;
        let git_dir_raw = run_git_at(
            &repo,
            &["rev-parse", "--absolute-git-dir"],
            &[],
            timeout_duration,
        )
        .await?;
        Ok(Self {
            repo,
            git_dir: PathBuf::from(git_dir_raw.trim()),
            timeout: timeout_duration,
        })
    }

    async fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String, GitToolError> {
        run_git_at(&self.repo, args, env, self.timeout).await
    }

    async fn run_bytes(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<Vec<u8>, GitToolError> {
        run_git_bytes_at(&self.repo, args, env, self.timeout).await
    }

    async fn run_owned(
        &self,
        args: &[String],
        env: &[(&str, &str)],
    ) -> Result<String, GitToolError> {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&borrowed, env).await
    }

    async fn verify_ref(&self, reference: &str) -> Result<(), GitToolError> {
        self.run(&["rev-parse", "--verify", "--quiet", reference], &[])
            .await
            .map(|_| ())
            .map_err(|_| GitToolError::UnknownCheckpoint)
    }
}

async fn create_checkpoint(
    runner: &GitRunner,
    message: Option<&str>,
) -> Result<GitCheckpoint, GitToolError> {
    let id = format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        Uuid::new_v4().simple()
    );
    let reference = checkpoint_ref(&id);
    let index = runner
        .git_dir
        .join("continuum-tmp")
        .join(format!("{id}.index"));
    if let Some(parent) = index.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| GitToolError::Io(error.to_string()))?;
    }
    let index_value = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index_value.as_str())];
    let has_head = runner
        .run(&["rev-parse", "--verify", "--quiet", "HEAD"], &[])
        .await
        .is_ok();
    if has_head {
        runner.run(&["read-tree", "HEAD"], &env).await?;
    }
    runner.run(&["add", "-A"], &env).await?;

    let paths = nul_paths(
        &runner
            .run_bytes(
                &[
                    "ls-files",
                    "--cached",
                    "--others",
                    "--deleted",
                    "--exclude-standard",
                    "-z",
                ],
                &[],
            )
            .await?,
    );
    let denied: Vec<PathBuf> = paths
        .into_iter()
        .filter(|path| is_relative_path_denied(path).is_err())
        .collect();
    for path in &denied {
        let value = path.to_string_lossy();
        let tracked_in_head = has_head
            && runner
                .run(&["cat-file", "-e", &format!("HEAD:{value}")], &[])
                .await
                .is_ok();
        if tracked_in_head {
            runner
                .run(&["reset", "-q", "HEAD", "--", &value], &env)
                .await?;
        } else {
            runner
                .run(
                    &["rm", "--cached", "-q", "--ignore-unmatch", "--", &value],
                    &env,
                )
                .await?;
        }
    }

    let tree = runner.run(&["write-tree"], &env).await?;
    let message: String = message
        .unwrap_or("Continuum checkpoint")
        .chars()
        .take(240)
        .collect();
    let mut commit_args = vec!["commit-tree", tree.trim()];
    if has_head {
        commit_args.extend(["-p", "HEAD"]);
    }
    commit_args.extend(["-m", &message]);
    let identity = [
        ("GIT_AUTHOR_NAME", "Continuum"),
        ("GIT_AUTHOR_EMAIL", "continuum@local"),
        ("GIT_COMMITTER_NAME", "Continuum"),
        ("GIT_COMMITTER_EMAIL", "continuum@local"),
    ];
    let commit = runner.run(&commit_args, &identity).await?;
    runner
        .run(&["update-ref", &reference, commit.trim()], &[])
        .await?;
    let _ = tokio::fs::remove_file(&index).await;
    Ok(GitCheckpoint {
        id,
        reference,
        commit: commit.trim().to_string(),
        repo: runner.repo.to_string_lossy().into_owned(),
        created_at: Utc::now().to_rfc3339(),
        excluded_sensitive_paths: denied.len(),
    })
}

async fn changed_paths(runner: &GitRunner) -> Result<BTreeSet<PathBuf>, GitToolError> {
    let mut paths = BTreeSet::new();
    for args in [
        ["diff", "--name-only", "-z", "HEAD"].as_slice(),
        ["diff", "--cached", "--name-only", "-z", "HEAD"].as_slice(),
    ] {
        paths.extend(nul_paths(&runner.run_bytes(args, &[]).await?));
    }
    Ok(paths)
}

async fn changed_since(
    runner: &GitRunner,
    reference: &str,
) -> Result<BTreeSet<PathBuf>, GitToolError> {
    Ok(nul_paths(
        &runner
            .run_bytes(&["diff", "--name-only", "-z", reference], &[])
            .await?,
    )
    .into_iter()
    .collect())
}

async fn run_git_at(
    repo: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    limit: Duration,
) -> Result<String, GitToolError> {
    let bytes = run_git_bytes_at(repo, args, env, limit).await?;
    String::from_utf8(bytes).map_err(|_| GitToolError::NonUtf8)
}

async fn run_git_bytes_at(
    repo: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    limit: Duration,
) -> Result<Vec<u8>, GitToolError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(limit, command.output())
        .await
        .map_err(|_| GitToolError::Timeout)?
        .map_err(|error| GitToolError::Io(error.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitToolError::Git(stderr.trim().chars().take(500).collect()));
    }
    Ok(output.stdout)
}

fn checkpoint_ref(id: &str) -> String {
    format!("refs/continuum/checkpoints/{id}")
}

fn validate_checkpoint_id(id: &str) -> Result<(), GitToolError> {
    if id.is_empty()
        || id.len() > 100
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(GitToolError::InvalidCheckpointId);
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), GitToolError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(GitToolError::UnsafePath);
    }
    Ok(())
}

fn nul_paths(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| PathBuf::from(String::from_utf8_lossy(part).into_owned()))
        .collect()
}

fn truncate_utf8(input: &str, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_string(), false);
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n[diff truncated]", &input[..end]), true)
}

fn filter_status(status: &str) -> String {
    status
        .lines()
        .filter(|line| {
            let path = line.get(3..).unwrap_or_default();
            path.split(" -> ")
                .all(|part| is_relative_path_denied(Path::new(part.trim_matches('"'))).is_ok())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn move_file(source: &Path, target: &Path) -> Result<(), GitToolError> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| GitToolError::Io(error.to_string()))?;
    }
    match tokio::fs::rename(source, target).await {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_file(source, target).await?;
            tokio::fs::remove_file(source)
                .await
                .map_err(|error| GitToolError::Io(error.to_string()))
        }
    }
}

async fn copy_file(source: &Path, target: &Path) -> Result<(), GitToolError> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| GitToolError::Io(error.to_string()))?;
    }
    tokio::fs::copy(source, target)
        .await
        .map(|_| ())
        .map_err(|error| GitToolError::Io(error.to_string()))
}

/// Git tool failure with sanitized model-facing messages.
#[derive(Debug, thiserror::Error)]
pub enum GitToolError {
    /// Repository path failed the filesystem allowlist.
    #[error("repository denied: {0}")]
    Denied(String),
    /// Git returned an error.
    #[error("git command failed: {0}")]
    Git(String),
    /// Git did not finish within the configured context timeout.
    #[error("git command timed out")]
    Timeout,
    /// Filesystem operation failed.
    #[error("io error: {0}")]
    Io(String),
    /// Git emitted non-UTF-8 output where text was required.
    #[error("git returned non-UTF-8 output")]
    NonUtf8,
    /// Checkpoint id is not safe to interpolate into a ref.
    #[error("invalid Continuum checkpoint id")]
    InvalidCheckpointId,
    /// The requested checkpoint ref does not exist.
    #[error("Continuum checkpoint does not exist")]
    UnknownCheckpoint,
    /// Git returned an unsafe relative path.
    #[error("git returned an unsafe repository path")]
    UnsafePath,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn repo() -> (TempDir, AllowlistConfig) {
        let temp = tempfile::tempdir().unwrap();
        run_git_at(temp.path(), &["init"], &[], Duration::from_secs(5))
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("tracked.txt"), "one")
            .await
            .unwrap();
        run_git_at(
            temp.path(),
            &["add", "tracked.txt"],
            &[],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let identity = [
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@local"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@local"),
        ];
        run_git_at(
            temp.path(),
            &["commit", "-m", "initial"],
            &identity,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let allowlist = AllowlistConfig::from_roots([temp.path()]);
        (temp, allowlist)
    }

    #[tokio::test]
    async fn checkpoint_preserves_real_index_and_excludes_secrets() {
        let (repo, allowlist) = repo().await;
        tokio::fs::write(repo.path().join("tracked.txt"), "two")
            .await
            .unwrap();
        tokio::fs::write(repo.path().join("new.txt"), "new")
            .await
            .unwrap();
        tokio::fs::write(repo.path().join(".env"), "TOKEN=secret")
            .await
            .unwrap();
        let result = checkpoint(
            &GitCheckpointRequest {
                repo: repo.path().to_string_lossy().into_owned(),
                message: Some("test".into()),
            },
            &allowlist,
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(result.excluded_sensitive_paths, 1);
        let staged = run_git_at(
            repo.path(),
            &["diff", "--cached", "--name-only"],
            &[],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(staged.is_empty());
        let tree = run_git_at(
            repo.path(),
            &["ls-tree", "-r", "--name-only", &result.reference],
            &[],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(tree.contains("tracked.txt"));
        assert!(tree.contains("new.txt"));
        assert!(!tree.contains(".env"));
        let snapshot = diff(
            &GitDiffRequest {
                repo: repo.path().to_string_lossy().into_owned(),
                checkpoint_id: Some(result.id),
                max_bytes: None,
            },
            &allowlist,
            5_000,
        )
        .await
        .unwrap();
        assert!(!snapshot.status.contains(".env"));
        assert!(!snapshot.diff.contains("TOKEN=secret"));
    }

    #[tokio::test]
    async fn rollback_creates_safety_checkpoint_and_recovers_untracked() {
        let (repo, allowlist) = repo().await;
        let original = checkpoint(
            &GitCheckpointRequest {
                repo: repo.path().to_string_lossy().into_owned(),
                message: None,
            },
            &allowlist,
            5_000,
        )
        .await
        .unwrap();
        tokio::fs::write(repo.path().join("tracked.txt"), "changed")
            .await
            .unwrap();
        tokio::fs::write(repo.path().join("loose.txt"), "recover me")
            .await
            .unwrap();
        let result = rollback(
            &GitRollbackRequest {
                repo: repo.path().to_string_lossy().into_owned(),
                checkpoint_id: original.id,
            },
            &allowlist,
            5_000,
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(repo.path().join("tracked.txt"))
                .await
                .unwrap(),
            "one"
        );
        assert!(!repo.path().join("loose.txt").exists());
        assert!(Path::new(&result.recovery_dir)
            .join("untracked/loose.txt")
            .exists());
    }

    #[test]
    fn checkpoint_ids_and_utf8_truncation_are_safe() {
        assert!(validate_checkpoint_id("20260808T120000Z-deadbeef").is_ok());
        assert!(validate_checkpoint_id("../../HEAD").is_err());
        let (value, truncated) = truncate_utf8("ééé", 3);
        assert!(truncated);
        assert!(value.starts_with('é'));
        assert!(!filter_status("?? .env\n M src/main.rs").contains(".env"));
    }
}
