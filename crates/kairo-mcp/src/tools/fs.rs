//! # Filesystem tools (`mcp__kairo__fs_*`)
//!
//! Read-only access to files within the runtime allowlist. All writes, deletes,
//! and moves are intentionally absent per CLAUDE.md non-negotiables.
//!
//! - [`read_file`] — up to 100 KB per call, UTF-8 only, with truncation prefix
//! - [`list_dir`] — up to 500 entries, per-entry allowlist filtering

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::allowlist::{is_path_allowed, AllowlistConfig, DenyReason};

// ---------------------------------------------------------------------------
// Config constants
// ---------------------------------------------------------------------------

/// Maximum bytes read from any single file before truncation.
pub const READ_FILE_MAX_BYTES: usize = 100 * 1024;

/// Maximum number of directory entries returned from a single list call.
pub const LIST_DIR_MAX_ENTRIES: usize = 500;

// ---------------------------------------------------------------------------
// fs_read_file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FsReadFileRequest {
    /// Absolute or relative filesystem path. Must be inside the allowlist
    /// after canonicalization. Symlinks are resolved.
    pub path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FsReadFileResponse {
    /// Canonicalized absolute path that was read.
    pub path: String,
    /// File contents, prefixed with a truncation marker if `truncated` is true.
    pub content: String,
    /// Whether the file was larger than the cap.
    pub truncated: bool,
    /// Total file size in bytes.
    pub total_bytes: u64,
}

pub fn read_file(path: &str, cfg: &AllowlistConfig) -> Result<FsReadFileResponse, FsError> {
    let canonical = is_path_allowed(Path::new(path), cfg).map_err(FsError::Denied)?;

    let metadata = std::fs::metadata(&canonical).map_err(|e| FsError::Io(e.to_string()))?;
    if !metadata.is_file() {
        return Err(FsError::NotAFile);
    }

    let total_bytes = metadata.len();
    let truncated = total_bytes as usize > READ_FILE_MAX_BYTES;

    let raw = if truncated {
        use std::io::Read;
        let mut buf = vec![0u8; READ_FILE_MAX_BYTES];
        let mut file = std::fs::File::open(&canonical).map_err(|e| FsError::Io(e.to_string()))?;
        let n = file
            .read(&mut buf)
            .map_err(|e| FsError::Io(e.to_string()))?;
        buf.truncate(n);
        buf
    } else {
        std::fs::read(&canonical).map_err(|e| FsError::Io(e.to_string()))?
    };

    // UTF-8 only. Binary files are rejected explicitly.
    let text = std::str::from_utf8(&raw)
        .map_err(|_| FsError::NonUtf8)?
        .to_string();

    let content = if truncated {
        let shown_kb = raw.len() / 1024;
        let total_kb = total_bytes / 1024;
        format!("[truncated, showing first {shown_kb}KB of {total_kb}KB total]\n\n{text}")
    } else {
        text
    };

    Ok(FsReadFileResponse {
        path: canonical.to_string_lossy().into_owned(),
        content,
        truncated,
        total_bytes,
    })
}

// ---------------------------------------------------------------------------
// fs_list_dir
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct FsListDirRequest {
    /// Directory path. Must be inside the allowlist after canonicalization.
    pub path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FsEntry {
    pub name: String,
    /// "file" or "dir"; other types (symlinks to missing targets, etc.) are skipped.
    pub kind: String,
    pub size_bytes: Option<u64>,
    /// Last modified timestamp, ISO-8601 UTC.
    pub modified_iso: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FsListDirResponse {
    /// Canonicalized absolute path that was listed.
    pub path: String,
    pub entries: Vec<FsEntry>,
    /// True if there were more than [`LIST_DIR_MAX_ENTRIES`] in the directory.
    pub truncated: bool,
}

pub fn list_dir(path: &str, cfg: &AllowlistConfig) -> Result<FsListDirResponse, FsError> {
    let canonical = is_path_allowed(Path::new(path), cfg).map_err(FsError::Denied)?;

    let metadata = std::fs::metadata(&canonical).map_err(|e| FsError::Io(e.to_string()))?;
    if !metadata.is_dir() {
        return Err(FsError::NotADirectory);
    }

    let read_dir = std::fs::read_dir(&canonical).map_err(|e| FsError::Io(e.to_string()))?;

    let mut entries = Vec::new();
    let mut truncated = false;
    for entry_res in read_dir {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry_path = entry.path();

        // Per-entry allowlist check — skip silently on deny so the orchestrator
        // sees a clean list rather than an error. This enforces the hardcoded
        // deny list even for children of an allowlisted root (e.g. `.ssh`
        // inside the home dir).
        if is_path_allowed(&entry_path, cfg).is_err() {
            continue;
        }

        if entries.len() >= LIST_DIR_MAX_ENTRIES {
            truncated = true;
            break;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let kind = if md.is_dir() {
            "dir"
        } else if md.is_file() {
            "file"
        } else {
            continue;
        };

        let size = if md.is_file() { Some(md.len()) } else { None };
        let modified_iso = md
            .modified()
            .ok()
            .map(|t| DateTime::<Utc>::from(t).to_rfc3339());

        entries.push(FsEntry {
            name,
            kind: kind.to_string(),
            size_bytes: size,
            modified_iso,
        });
    }

    Ok(FsListDirResponse {
        path: canonical.to_string_lossy().into_owned(),
        entries,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("{0}")]
    Denied(DenyReason),
    #[error("path is not a file")]
    NotAFile,
    #[error("path is not a directory")]
    NotADirectory,
    #[error("file is not valid UTF-8 text (binary file read not supported)")]
    NonUtf8,
    #[error("io error: {0}")]
    Io(String),
}

/// Builds an `AllowlistConfig` from the runtime sources: data dir, config
/// extra_paths, and project.*.dir semantic facts. Used by the server on each
/// fs_* call so newly added facts take effect immediately.
pub fn build_allowlist(
    data_dir: &Path,
    extra_paths: &[PathBuf],
    project_dir_facts: &[String],
) -> AllowlistConfig {
    let mut roots: Vec<PathBuf> =
        Vec::with_capacity(1 + extra_paths.len() + project_dir_facts.len());
    roots.push(data_dir.to_path_buf());
    roots.extend(extra_paths.iter().cloned());
    for p in project_dir_facts {
        roots.push(PathBuf::from(p));
    }
    AllowlistConfig::from_roots(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_file_happy_path() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("hello.txt");
        std::fs::write(&f, "hello world").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let r = read_file(f.to_str().unwrap(), &cfg).unwrap();
        assert_eq!(r.content, "hello world");
        assert!(!r.truncated);
        assert_eq!(r.total_bytes, 11);
    }

    #[test]
    fn read_file_truncates_large_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("big.txt");
        std::fs::write(&f, "x".repeat(READ_FILE_MAX_BYTES + 100)).unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let r = read_file(f.to_str().unwrap(), &cfg).unwrap();
        assert!(r.truncated);
        assert!(r.content.starts_with("[truncated"));
        assert_eq!(r.total_bytes as usize, READ_FILE_MAX_BYTES + 100);
    }

    #[test]
    fn read_file_rejects_denied_path() {
        let dir = tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let f = ssh.join("id_rsa");
        std::fs::write(&f, "KEY").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let err = read_file(f.to_str().unwrap(), &cfg).unwrap_err();
        assert!(matches!(err, FsError::Denied(_)));
    }

    #[test]
    fn read_file_rejects_outside_root() {
        let dir = tempdir().unwrap();
        let other = tempdir().unwrap();
        let f = other.path().join("secret.txt");
        std::fs::write(&f, "x").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let err = read_file(f.to_str().unwrap(), &cfg).unwrap_err();
        assert!(matches!(err, FsError::Denied(_)));
    }

    #[test]
    fn read_file_rejects_binary_content() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("binary.bin");
        std::fs::write(&f, [0xff, 0xfe, 0x00, 0x80]).unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let err = read_file(f.to_str().unwrap(), &cfg).unwrap_err();
        assert!(matches!(err, FsError::NonUtf8));
    }

    #[test]
    fn list_dir_happy_path() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "A").unwrap();
        std::fs::write(dir.path().join("b.txt"), "BB").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let r = list_dir(dir.path().to_str().unwrap(), &cfg).unwrap();
        assert_eq!(r.entries.len(), 3);
        assert!(!r.truncated);
    }

    #[test]
    fn list_dir_skips_denied_entries() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("ok.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join(".ssh")).unwrap();
        std::fs::write(dir.path().join(".env"), "SECRET=x").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let r = list_dir(dir.path().to_str().unwrap(), &cfg).unwrap();
        let names: Vec<_> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"ok.txt"));
        assert!(!names.contains(&".ssh"));
        assert!(!names.contains(&".env"));
    }

    #[test]
    fn list_dir_rejects_file_target() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("notadir.txt");
        std::fs::write(&f, "x").unwrap();
        let cfg = AllowlistConfig::from_roots([dir.path()]);
        let err = list_dir(f.to_str().unwrap(), &cfg).unwrap_err();
        assert!(matches!(err, FsError::NotADirectory));
    }

    #[test]
    fn build_allowlist_includes_all_sources() {
        let data = tempdir().unwrap();
        let extra = tempdir().unwrap();
        let proj = tempdir().unwrap();

        let cfg = build_allowlist(
            data.path(),
            &[extra.path().to_path_buf()],
            &[proj.path().to_string_lossy().into_owned()],
        );
        // Each canonicalized root should be in the list
        assert_eq!(cfg.allowed_roots.len(), 3);
    }
}
