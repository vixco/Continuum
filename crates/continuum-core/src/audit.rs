//! # Minimal action audit log (spec §4.13)
//!
//! An append-only JSONL record of the things Continuum did *on its own* or
//! *to the user's data*: orchestrator wakes, honest-toggle changes,
//! corrections, and deletions. One JSON object per line:
//!
//! ```json
//! {"ts":"2026-08-05T10:11:12.131Z","kind":"toggle_change","actor":"user",
//!  "summary":"mic observation disabled","details":{"name":"mic","value":false}}
//! ```
//!
//! ## Where it lives
//!
//! `<data_dir>/logs/actions.jsonl` — the same `logs/` subdirectory the
//! runtime already creates in its data dir (`continuum setup` seeds it).
//! In development that resolves to `~/.continuum-dev/logs/actions.jsonl`;
//! a runtime started against the production data dir writes the spec's
//! `~/.continuum/logs/actions.jsonl`. The path follows the process's data
//! dir rather than being hardcoded, so the dev and production runtimes
//! never write into each other's audit trail.
//!
//! ## Discipline
//!
//! - **Never fails a caller.** Every write is best-effort: a full disk or
//!   a locked file logs at `warn` once per failure and returns.
//! - **Cheap.** Open-append-close per line (`OpenOptions::append`), no
//!   background task, no buffering to lose on a crash. Audit lines are
//!   rare — a wake, a click — not a hot loop.
//! - **Bounded.** Summaries are capped at [`MAX_SUMMARY_CHARS`] so a
//!   pathological input cannot write a megabyte-long line, and the file is
//!   truncated-from-the-front once it exceeds [`MAX_FILE_BYTES`].
//!
//! The *full* audit UI is NEXT-tier work (spec §2); this is the record it
//! will read.
//!
//! # Layer
//!
//! Cross-layer. Pure `std::fs` + serde — ungated, so the runtime, the
//! desktop process and (later) the MCP server can all append.

use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Summaries longer than this are truncated on a char boundary.
pub const MAX_SUMMARY_CHARS: usize = 400;

/// Once the file grows past this, the oldest half is dropped on the next
/// append. 4 MiB is thousands of actions — long enough to be useful, small
/// enough that the rotation read never hurts.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// File name inside `<data_dir>/logs/`.
pub const AUDIT_FILE_NAME: &str = "actions.jsonl";

/// Who caused the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// A deliberate human action (Context page click, voice command).
    User,
    /// Continuum acting on its own (a triage-driven wake, a maintenance
    /// pass, an MCP tool call made by a model).
    Agent,
}

impl Actor {
    /// Stable snake_case token.
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::Agent => "agent",
        }
    }
}

/// One audit line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// When it happened.
    pub ts: DateTime<Utc>,
    /// Action family (`wake_start`, `wake_result`, `toggle_change`,
    /// `correction`, `deletion`, …). Free-form but stable per call site.
    pub kind: String,
    /// Who caused it.
    pub actor: Actor,
    /// One human-readable line.
    pub summary: String,
    /// Optional structured payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Append-only JSONL audit writer.
///
/// Cheap to clone (`PathBuf` only) and safe to share: every append opens,
/// writes one line, and closes, so concurrent writers interleave whole
/// lines rather than corrupting each other (`O_APPEND` semantics on both
/// Windows and Unix for writes below the pipe-buffer size, which a single
/// audit line always is).
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Audit log inside a Continuum data dir (`<data_dir>/logs/actions.jsonl`).
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("logs").join(AUDIT_FILE_NAME),
        }
    }

    /// Audit log at an explicit path (tests, tooling).
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// The file this log appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry. Never returns an error — see the module docs.
    pub fn append(&self, entry: &AuditEntry) {
        if let Err(e) = self.try_append(entry) {
            tracing::warn!(
                layer = "system",
                component = "audit",
                path = %self.path.display(),
                error = %e,
                "Failed to append audit entry"
            );
        }
    }

    /// Convenience constructor + append in one call.
    pub fn record(
        &self,
        kind: &str,
        actor: Actor,
        summary: impl Into<String>,
        details: Option<serde_json::Value>,
    ) {
        self.append(&AuditEntry {
            ts: Utc::now(),
            kind: kind.to_string(),
            actor,
            summary: truncate_chars(&summary.into(), MAX_SUMMARY_CHARS),
            details,
        });
    }

    fn try_append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.rotate_if_needed();
        let mut line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // A serialized entry can never contain a raw newline (serde_json
        // escapes them), so one line per entry is guaranteed.
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Drops the oldest half of the file once it exceeds
    /// [`MAX_FILE_BYTES`]. Best-effort: a failure here never blocks the
    /// append that triggered it.
    fn rotate_if_needed(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() <= MAX_FILE_BYTES {
            return;
        }
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let lines: Vec<&str> = contents.lines().collect();
        let keep_from = lines.len() / 2;
        let kept = lines[keep_from..].join("\n");
        let tmp = self.path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, format!("{kept}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
            tracing::info!(
                layer = "system",
                component = "audit",
                dropped_lines = keep_from,
                "Rotated the action audit log"
            );
        }
    }

    /// Reads every well-formed entry back, oldest first. Malformed lines
    /// are skipped (a torn tail from a crash must not hide the history in
    /// front of it). Used by tests and, later, the audit UI.
    pub fn read_all(&self) -> Vec<AuditEntry> {
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
            .collect()
    }
}

/// Char-boundary-safe truncation with an ellipsis marker.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn path_is_under_the_data_dirs_logs_folder() {
        let log = AuditLog::new(Path::new("C:/data"));
        assert!(log
            .path()
            .ends_with(Path::new("logs").join(AUDIT_FILE_NAME)));
    }

    #[test]
    fn appended_lines_are_one_json_object_each() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        log.record("toggle_change", Actor::User, "mic disabled", None);
        log.record(
            "deletion",
            Actor::User,
            "forget frame",
            Some(serde_json::json!({ "frames": 1 })),
        );

        let raw = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("well-formed JSON line");
            let obj = v.as_object().unwrap();
            assert!(obj.contains_key("ts"));
            assert!(obj.contains_key("kind"));
            assert!(obj.contains_key("actor"));
            assert!(obj.contains_key("summary"));
        }
        let parsed = log.read_all();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, "toggle_change");
        assert_eq!(parsed[0].actor, Actor::User);
        assert!(parsed[0].details.is_none());
        assert_eq!(parsed[1].details.as_ref().unwrap()["frames"], 1);
    }

    #[test]
    fn embedded_newlines_never_split_a_line() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        log.record("correction", Actor::User, "line one\nline two", None);
        let raw = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert_eq!(log.read_all()[0].summary, "line one\nline two");
    }

    #[test]
    fn long_summaries_are_truncated_on_a_char_boundary() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        let long = "é".repeat(MAX_SUMMARY_CHARS * 2);
        log.record("wake_start", Actor::Agent, long, None);
        let entry = &log.read_all()[0];
        assert_eq!(entry.summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(entry.summary.ends_with('…'));
    }

    #[test]
    fn missing_parent_directory_is_created() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(&tmp.path().join("nested").join("deeper"));
        log.record("wake_start", Actor::Agent, "hello", None);
        assert!(log.path().exists());
    }

    #[test]
    fn unwritable_path_is_survivable() {
        // A directory where the file should be: every open fails, and the
        // caller must not notice.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("actions.jsonl");
        std::fs::create_dir_all(&path).unwrap();
        let log = AuditLog::at(path);
        log.record("deletion", Actor::User, "nope", None);
        assert!(log.read_all().is_empty());
    }

    #[test]
    fn rotation_drops_the_oldest_half() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("actions.jsonl");
        // Write a file well over the cap by hand.
        let filler: String = (0..40_000)
            .map(|i| {
                format!(
                    "{}\n",
                    serde_json::to_string(&AuditEntry {
                        ts: Utc::now(),
                        kind: "wake_start".into(),
                        actor: Actor::Agent,
                        summary: format!("entry {i} {}", "x".repeat(80)),
                        details: None,
                    })
                    .unwrap()
                )
            })
            .collect();
        std::fs::write(&path, &filler).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > MAX_FILE_BYTES);

        let log = AuditLog::at(path.clone());
        log.record("toggle_change", Actor::User, "after rotation", None);

        let entries = log.read_all();
        assert!(entries.len() < 40_000, "old half should be gone");
        assert_eq!(entries.last().unwrap().summary, "after rotation");
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_FILE_BYTES);
    }

    #[test]
    fn malformed_lines_are_skipped_by_the_reader() {
        let tmp = TempDir::new().unwrap();
        let log = AuditLog::new(tmp.path());
        log.record("wake_start", Actor::Agent, "ok", None);
        // Simulate a torn write.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(log.path())
            .unwrap();
        f.write_all(b"{\"ts\":\"tru").unwrap();
        drop(f);
        assert_eq!(log.read_all().len(), 1);
    }
}
