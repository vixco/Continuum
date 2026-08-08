//! # Recoverable filesystem mutation tools
//!
//! Every target passes the shared filesystem allowlist. Existing content is
//! moved to a recovery area before replacement/deletion, destinations never
//! overwrite, and writes use a temporary sibling followed by rename.

use std::path::{Path, PathBuf};

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::allowlist::{is_path_allowed, resolve_new_path_allowed, AllowlistConfig};

/// Input for `fs_create_file`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FsCreateFileRequest {
    /// New file path. Its direct parent must already exist and be allowlisted.
    pub path: String,
    /// UTF-8 file content.
    pub content: String,
}

/// Input for `fs_apply_patch`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FsApplyPatchRequest {
    /// Existing allowlisted UTF-8 file.
    pub path: String,
    /// Exact text that must currently exist.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
    /// Replace every occurrence instead of requiring exactly one.
    #[serde(default)]
    pub replace_all: bool,
}

/// Input for `fs_move`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FsMoveRequest {
    /// Existing allowlisted file or directory.
    pub source: String,
    /// New non-existing destination with an allowlisted existing parent.
    pub destination: String,
}

/// Input for `fs_delete_to_trash`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FsDeleteRequest {
    /// Existing allowlisted file or directory to move into recovery storage.
    pub path: String,
}

/// Result of creating a new file.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FsCreateFileResponse {
    /// Canonical destination.
    pub path: String,
    /// Bytes written.
    pub bytes: usize,
}

/// Result of an exact-text patch.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FsApplyPatchResponse {
    /// Canonical file path.
    pub path: String,
    /// Number of replacements made.
    pub replacements: usize,
    /// Original file preserved here.
    pub recovery_path: String,
    /// Final file size.
    pub bytes: usize,
}

/// Result of a move.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FsMoveResponse {
    /// Original canonical path.
    pub source: String,
    /// Final canonical path.
    pub destination: String,
}

/// Result of a recoverable delete.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FsDeleteResponse {
    /// Original canonical path.
    pub original_path: String,
    /// Recovery location containing the moved item.
    pub recovery_path: String,
}

/// Atomically creates a file and refuses overwrite.
pub async fn create_file(
    request: &FsCreateFileRequest,
    allowlist: &AllowlistConfig,
    max_bytes: usize,
) -> Result<FsCreateFileResponse, FileActionError> {
    enforce_size(request.content.len(), max_bytes)?;
    let destination = resolve_new_path_allowed(Path::new(&request.path), allowlist)
        .map_err(|error| FileActionError::Denied(error.to_string()))?;
    write_new_atomic(&destination, request.content.as_bytes()).await?;
    Ok(FsCreateFileResponse {
        path: destination.to_string_lossy().into_owned(),
        bytes: request.content.len(),
    })
}

/// Applies an exact-text patch after verifying the precondition.
pub async fn apply_patch(
    request: &FsApplyPatchRequest,
    allowlist: &AllowlistConfig,
    recovery_root: &Path,
    max_bytes: usize,
    max_replacements: usize,
) -> Result<FsApplyPatchResponse, FileActionError> {
    if request.old_text.is_empty() {
        return Err(FileActionError::EmptyPatchNeedle);
    }
    let path = is_path_allowed(Path::new(&request.path), allowlist)
        .map_err(|error| FileActionError::Denied(error.to_string()))?;
    if !path.is_file() {
        return Err(FileActionError::NotAFile);
    }
    let original = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| FileActionError::Io(error.to_string()))?;
    let occurrences = original.matches(&request.old_text).count();
    if occurrences == 0 {
        return Err(FileActionError::PreconditionMissing);
    }
    if !request.replace_all && occurrences != 1 {
        return Err(FileActionError::AmbiguousPatch(occurrences));
    }
    if occurrences > max_replacements.max(1) {
        return Err(FileActionError::TooManyReplacements(occurrences));
    }
    let updated = if request.replace_all {
        original.replace(&request.old_text, &request.new_text)
    } else {
        original.replacen(&request.old_text, &request.new_text, 1)
    };
    enforce_size(updated.len(), max_bytes)?;
    let recovery = recovery_path(recovery_root, &path);
    replace_with_recovery(&path, updated.as_bytes(), &recovery).await?;
    Ok(FsApplyPatchResponse {
        path: path.to_string_lossy().into_owned(),
        replacements: if request.replace_all { occurrences } else { 1 },
        recovery_path: recovery.to_string_lossy().into_owned(),
        bytes: updated.len(),
    })
}

/// Moves a file or directory without overwriting a destination.
pub async fn move_path(
    request: &FsMoveRequest,
    allowlist: &AllowlistConfig,
) -> Result<FsMoveResponse, FileActionError> {
    let source = is_path_allowed(Path::new(&request.source), allowlist)
        .map_err(|error| FileActionError::Denied(error.to_string()))?;
    let destination = resolve_new_path_allowed(Path::new(&request.destination), allowlist)
        .map_err(|error| FileActionError::Denied(error.to_string()))?;
    move_without_overwrite(&source, &destination).await?;
    Ok(FsMoveResponse {
        source: source.to_string_lossy().into_owned(),
        destination: destination.to_string_lossy().into_owned(),
    })
}

/// Moves a file or directory into Continuum recovery storage.
pub async fn delete_to_trash(
    request: &FsDeleteRequest,
    allowlist: &AllowlistConfig,
    recovery_root: &Path,
) -> Result<FsDeleteResponse, FileActionError> {
    let source = is_path_allowed(Path::new(&request.path), allowlist)
        .map_err(|error| FileActionError::Denied(error.to_string()))?;
    let recovery = recovery_path(recovery_root, &source);
    if let Some(parent) = recovery.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| FileActionError::Io(error.to_string()))?;
    }
    move_without_overwrite(&source, &recovery).await?;
    Ok(FsDeleteResponse {
        original_path: source.to_string_lossy().into_owned(),
        recovery_path: recovery.to_string_lossy().into_owned(),
    })
}

fn enforce_size(size: usize, max_bytes: usize) -> Result<(), FileActionError> {
    if size > max_bytes.max(1) {
        Err(FileActionError::TooLarge { size, max_bytes })
    } else {
        Ok(())
    }
}

fn recovery_path(root: &Path, original: &Path) -> PathBuf {
    let name = original
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("item");
    root.join(Utc::now().format("%Y-%m-%d").to_string())
        .join(format!("{}-{name}", Uuid::new_v4().simple()))
}

async fn write_new_atomic(path: &Path, content: &[u8]) -> Result<(), FileActionError> {
    let parent = path.parent().ok_or(FileActionError::UnsafeDestination)?;
    let temporary = parent.join(format!(".continuum-write-{}.tmp", Uuid::new_v4().simple()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(&temporary)
        .await
        .map_err(|error| FileActionError::Io(error.to_string()))?;
    use tokio::io::AsyncWriteExt;
    if let Err(error) = file.write_all(content).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(FileActionError::Io(error.to_string()));
    }
    file.sync_all()
        .await
        .map_err(|error| FileActionError::Io(error.to_string()))?;
    drop(file);
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|error| FileActionError::Io(error.to_string()))
}

async fn replace_with_recovery(
    path: &Path,
    content: &[u8],
    recovery: &Path,
) -> Result<(), FileActionError> {
    let parent = path.parent().ok_or(FileActionError::UnsafeDestination)?;
    let temporary = parent.join(format!(".continuum-patch-{}.tmp", Uuid::new_v4().simple()));
    tokio::fs::write(&temporary, content)
        .await
        .map_err(|error| FileActionError::Io(error.to_string()))?;
    if let Some(parent) = recovery.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| FileActionError::Io(error.to_string()))?;
    }
    move_without_overwrite(path, recovery).await?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = move_without_overwrite(recovery, path).await;
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(FileActionError::Io(error.to_string()));
    }
    Ok(())
}

async fn move_without_overwrite(source: &Path, destination: &Path) -> Result<(), FileActionError> {
    if destination.exists() {
        return Err(FileActionError::DestinationExists);
    }
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| FileActionError::Io(error.to_string()))?;
    }
    match tokio::fs::rename(source, destination).await {
        Ok(()) => Ok(()),
        Err(_rename_error) if source.is_file() => {
            let bytes = tokio::fs::copy(source, destination)
                .await
                .map_err(|error| FileActionError::Io(error.to_string()))?;
            let expected = tokio::fs::metadata(source)
                .await
                .map_err(|error| FileActionError::Io(error.to_string()))?
                .len();
            if bytes != expected {
                let _ = tokio::fs::remove_file(destination).await;
                return Err(FileActionError::Io(
                    "cross-volume copy verification failed".to_string(),
                ));
            }
            tokio::fs::remove_file(source)
                .await
                .map_err(|error| FileActionError::Io(error.to_string()))
        }
        Err(error) => Err(FileActionError::Io(format!(
            "move failed (directories must remain on one volume): {error}"
        ))),
    }
}

/// Filesystem mutation error.
#[derive(Debug, thiserror::Error)]
pub enum FileActionError {
    /// Path failed the allowlist or deny rules.
    #[error("path denied: {0}")]
    Denied(String),
    /// Destination appeared before the operation completed.
    #[error("destination already exists")]
    DestinationExists,
    /// Input exceeded the configured byte cap.
    #[error("content is {size} bytes; configured maximum is {max_bytes}")]
    TooLarge {
        /// Actual bytes.
        size: usize,
        /// Configured maximum.
        max_bytes: usize,
    },
    /// Patch old text was empty.
    #[error("old_text must not be empty")]
    EmptyPatchNeedle,
    /// Patch precondition was absent.
    #[error("old_text does not match current file content")]
    PreconditionMissing,
    /// Non-replace-all patch matched more than once.
    #[error("old_text matched {0} times; provide a unique context or set replace_all")]
    AmbiguousPatch(usize),
    /// Replacement count exceeded the configured cap.
    #[error("patch would make {0} replacements, above the configured limit")]
    TooManyReplacements(usize),
    /// Existing path was not a regular file.
    #[error("path is not a regular file")]
    NotAFile,
    /// Target had no safe parent.
    #[error("destination has no safe parent")]
    UnsafeDestination,
    /// Filesystem operation failed.
    #[error("io error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn create_refuses_overwrite_and_secret_names() {
        let root = tempdir().unwrap();
        let cfg = AllowlistConfig::from_roots([root.path()]);
        let request = FsCreateFileRequest {
            path: root.path().join("new.txt").to_string_lossy().into_owned(),
            content: "hello".into(),
        };
        create_file(&request, &cfg, 100).await.unwrap();
        assert!(create_file(&request, &cfg, 100).await.is_err());
        let secret = FsCreateFileRequest {
            path: root.path().join(".env").to_string_lossy().into_owned(),
            content: "secret".into(),
        };
        assert!(create_file(&secret, &cfg, 100).await.is_err());
    }

    #[tokio::test]
    async fn patch_requires_exact_context_and_preserves_original() {
        let root = tempdir().unwrap();
        let recovery = tempdir().unwrap();
        let file = root.path().join("file.txt");
        tokio::fs::write(&file, "before\n").await.unwrap();
        let cfg = AllowlistConfig::from_roots([root.path()]);
        let response = apply_patch(
            &FsApplyPatchRequest {
                path: file.to_string_lossy().into_owned(),
                old_text: "before".into(),
                new_text: "after".into(),
                replace_all: false,
            },
            &cfg,
            recovery.path(),
            100,
            10,
        )
        .await
        .unwrap();
        assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), "after\n");
        assert_eq!(
            tokio::fs::read_to_string(response.recovery_path)
                .await
                .unwrap(),
            "before\n"
        );
    }

    #[tokio::test]
    async fn delete_is_a_recoverable_move() {
        let root = tempdir().unwrap();
        let recovery = tempdir().unwrap();
        let file = root.path().join("remove.txt");
        tokio::fs::write(&file, "keep").await.unwrap();
        let cfg = AllowlistConfig::from_roots([root.path()]);
        let response = delete_to_trash(
            &FsDeleteRequest {
                path: file.to_string_lossy().into_owned(),
            },
            &cfg,
            recovery.path(),
        )
        .await
        .unwrap();
        assert!(!file.exists());
        assert_eq!(
            tokio::fs::read_to_string(response.recovery_path)
                .await
                .unwrap(),
            "keep"
        );
    }
}
