//! # Worker intent-file protocol
//!
//! The kairo-mcp server runs as a subprocess of the claude CLI, which runs as a
//! subprocess of the main kairo runtime. There is no tokio channel reaching
//! across that boundary, so MCP→runtime commands go through the filesystem —
//! the same pattern the repair subsystem uses.
//!
//! ## Directory layout
//!
//! ```text
//! ~/.kairo-dev/
//! ├── worker-intents/          ← MCP writes, runtime drains on each tick
//! │   ├── 20260414T130102123-spawn.json
//! │   └── 20260414T130108456-cancel.json
//! └── workers/                 ← runtime writes, MCP + dashboard read
//!     ├── <worker-id>.json     (current snapshot; atomic-renamed)
//!     └── ...
//! ```
//!
//! Both directories are created on demand by this module's helpers.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::types::{WorkerId, WorkerSnapshot, WorkerSpec};

/// Subdirectory under the Kairo data dir where MCP writes spawn/cancel intents.
pub const INTENTS_SUBDIR: &str = "worker-intents";
/// Subdirectory under the Kairo data dir where the runtime publishes snapshots.
pub const SNAPSHOTS_SUBDIR: &str = "workers";

/// Top-level envelope for a worker intent. One file = one intent.
///
/// `Spawn` boxes the `WorkerSpec` so the variant-size delta stays small —
/// specs are ~400 bytes while `Cancel` is a single UUID string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerIntent {
    /// Queue a new worker.
    Spawn {
        /// Pre-assigned worker id. The MCP server generates it so it can
        /// return it to the orchestrator immediately without waiting for the
        /// runtime to pick up the intent.
        id: WorkerId,
        spec: Box<WorkerSpec>,
    },
    /// Stop a running or queued worker.
    Cancel { id: WorkerId },
}

impl WorkerIntent {
    /// Convenience: build a `Spawn` without manual boxing.
    pub fn spawn(id: WorkerId, spec: WorkerSpec) -> Self {
        Self::Spawn {
            id,
            spec: Box::new(spec),
        }
    }
}

/// Returns the intents directory for a given Kairo data dir, creating it on
/// first call.
pub fn ensure_intents_dir(data_dir: &Path) -> Result<PathBuf> {
    let p = data_dir.join(INTENTS_SUBDIR);
    std::fs::create_dir_all(&p)
        .with_context(|| format!("Failed to create worker intents dir at {}", p.display()))?;
    Ok(p)
}

/// Returns the snapshots directory for a given Kairo data dir, creating it on
/// first call.
pub fn ensure_snapshots_dir(data_dir: &Path) -> Result<PathBuf> {
    let p = data_dir.join(SNAPSHOTS_SUBDIR);
    std::fs::create_dir_all(&p)
        .with_context(|| format!("Failed to create worker snapshots dir at {}", p.display()))?;
    Ok(p)
}

/// Writes a new intent file. Returns the path it was written to.
pub fn write_intent(data_dir: &Path, intent: &WorkerIntent) -> Result<PathBuf> {
    let dir = ensure_intents_dir(data_dir)?;
    let kind = match intent {
        WorkerIntent::Spawn { .. } => "spawn",
        WorkerIntent::Cancel { .. } => "cancel",
    };
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
    let path = dir.join(format!("{ts}-{kind}.json"));
    let payload =
        serde_json::to_string_pretty(intent).context("Failed to serialize worker intent")?;
    std::fs::write(&path, payload)
        .with_context(|| format!("Failed to write worker intent at {}", path.display()))?;
    Ok(path)
}

/// Reads and removes every intent file from the directory. Intents that fail
/// to parse are renamed with a `.bad` suffix so they don't starve the loop
/// but can still be inspected for debugging.
pub fn drain_intents(data_dir: &Path) -> Result<Vec<(PathBuf, WorkerIntent)>> {
    let dir = ensure_intents_dir(data_dir)?;
    let mut out = Vec::new();
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };

    // Collect first so we don't hold the iterator across mutations.
    let mut entries: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "json")
                .unwrap_or(false)
        })
        .collect();
    entries.sort();

    for path in entries {
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match serde_json::from_str::<WorkerIntent>(&contents) {
            Ok(intent) => {
                // Best-effort cleanup: failure to remove is non-fatal because
                // the next drain will simply process an already-consumed file
                // as a duplicate, which the pool can de-dupe via worker id.
                let _ = std::fs::remove_file(&path);
                out.push((path, intent));
            }
            Err(e) => {
                tracing::warn!(
                    layer = "workers",
                    component = "intent",
                    path = %path.display(),
                    error = %e,
                    "Skipping unparseable worker intent"
                );
                let _ = std::fs::rename(&path, path.with_extension("bad"));
            }
        }
    }
    Ok(out)
}

/// Returns the snapshot path for a given worker id under `data_dir`.
pub fn snapshot_path(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join(SNAPSHOTS_SUBDIR).join(format!("{id}.json"))
}

/// Writes a snapshot atomically by writing to a `.tmp` sibling and renaming.
pub fn write_snapshot(data_dir: &Path, snapshot: &WorkerSnapshot) -> Result<()> {
    ensure_snapshots_dir(data_dir)?;
    let path = snapshot_path(data_dir, &snapshot.id);
    let tmp = path.with_extension("json.tmp");
    let payload =
        serde_json::to_string_pretty(snapshot).context("Failed to serialize worker snapshot")?;
    std::fs::write(&tmp, payload)
        .with_context(|| format!("Failed to write worker snapshot tmp at {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        format!(
            "Failed to rename worker snapshot {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Reads a snapshot back by id. Returns `None` if the file is missing.
pub fn read_snapshot(data_dir: &Path, id: &str) -> Result<Option<WorkerSnapshot>> {
    let path = snapshot_path(data_dir, id);
    match std::fs::read_to_string(&path) {
        Ok(body) => {
            let snap = serde_json::from_str(&body).with_context(|| {
                format!("Failed to parse worker snapshot at {}", path.display())
            })?;
            Ok(Some(snap))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("Failed to read worker snapshot"),
    }
}

/// Lists all snapshot files under `data_dir`. Unreadable entries are skipped.
pub fn list_snapshots(data_dir: &Path) -> Result<Vec<WorkerSnapshot>> {
    let dir = ensure_snapshots_dir(data_dir)?;
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&path) {
            if let Ok(snap) = serde_json::from_str::<WorkerSnapshot>(&body) {
                out.push(snap);
            }
        }
    }
    out.sort_by(|a, b| b.queued_at.cmp(&a.queued_at));
    Ok(out)
}

/// Removes a snapshot file. Useful when the dashboard dismisses a completed
/// worker or the runtime prunes old snapshots.
pub fn delete_snapshot(data_dir: &Path, id: &str) -> Result<()> {
    let path = snapshot_path(data_dir, id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("Failed to delete worker snapshot"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::types::{WorkerSnapshot, WorkerSpec, WorkerStatus};
    use tempfile::TempDir;

    fn sample_spec() -> WorkerSpec {
        WorkerSpec::new("test task", std::env::temp_dir())
    }

    #[test]
    fn write_and_drain_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let intent = WorkerIntent::spawn("abc-123".into(), sample_spec());
        let p = write_intent(tmp.path(), &intent).unwrap();
        assert!(p.exists());

        let intents = drain_intents(tmp.path()).unwrap();
        assert_eq!(intents.len(), 1);
        match &intents[0].1 {
            WorkerIntent::Spawn { id, .. } => assert_eq!(id, "abc-123"),
            other => panic!("expected Spawn, got {other:?}"),
        }
        // File should be gone after drain.
        assert!(!p.exists());
    }

    #[test]
    fn cancel_intent_roundtrip() {
        let tmp = TempDir::new().unwrap();
        write_intent(
            tmp.path(),
            &WorkerIntent::Cancel {
                id: "to-kill".into(),
            },
        )
        .unwrap();
        let intents = drain_intents(tmp.path()).unwrap();
        assert_eq!(intents.len(), 1);
        assert!(matches!(&intents[0].1, WorkerIntent::Cancel { id } if id == "to-kill"));
    }

    #[test]
    fn bad_json_is_renamed_not_reread() {
        let tmp = TempDir::new().unwrap();
        let dir = ensure_intents_dir(tmp.path()).unwrap();
        let bad = dir.join("20260414T000000000-oops.json");
        std::fs::write(&bad, "{ not json").unwrap();

        let intents = drain_intents(tmp.path()).unwrap();
        assert!(intents.is_empty());
        assert!(!bad.exists());
        assert!(bad.with_extension("bad").exists());
    }

    #[test]
    fn snapshot_write_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let spec = sample_spec();
        let mut snap = WorkerSnapshot::queued(
            "worker-1".into(),
            &spec,
            "claude-sonnet-4-6".into(),
            "test".into(),
        );
        snap.status = WorkerStatus::Running;
        write_snapshot(tmp.path(), &snap).unwrap();

        let readback = read_snapshot(tmp.path(), "worker-1").unwrap().unwrap();
        assert_eq!(readback.id, "worker-1");
        assert_eq!(readback.status, WorkerStatus::Running);
    }

    #[test]
    fn list_snapshots_returns_all_json_files() {
        let tmp = TempDir::new().unwrap();
        let spec = sample_spec();
        for id in ["a", "b", "c"] {
            let snap =
                WorkerSnapshot::queued(id.into(), &spec, "claude-sonnet-4-6".into(), "test".into());
            write_snapshot(tmp.path(), &snap).unwrap();
        }
        let list = list_snapshots(tmp.path()).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn delete_missing_snapshot_is_ok() {
        let tmp = TempDir::new().unwrap();
        delete_snapshot(tmp.path(), "never-existed").unwrap();
    }
}
