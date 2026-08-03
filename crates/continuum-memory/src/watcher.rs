//! Debounced file-watcher: external edits (Obsidian, editors, the other
//! Continuum process) surface as batches of changed paths.
//!
//! Layer: memory. This is the mechanism that lets the markdown-on-disk
//! source of truth stay the source of truth even when something other than
//! this process's own [`crate::vault::Vault`] methods touches the vault
//! directory — the derived index is kept in sync by reindexing exactly the
//! paths reported here, never by polling.

use std::path::PathBuf;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind};

use crate::error::{MemoryError, Result};

/// A live handle on a debounced watch of a vault directory.
///
/// Receive batches of changed absolute paths from `rx`; each batch is
/// already filtered down to markdown notes that live in the vault and
/// outside `.continuum/` (the index's own directory, whose churn must never
/// trigger a reindex). Drop this value to stop watching — the internal
/// debouncer thread is torn down when `_debouncer` drops.
pub struct VaultWatcher {
    /// Batches of changed absolute paths (already filtered to vault `.md`
    /// files outside `.continuum/`).
    pub rx: tokio::sync::mpsc::UnboundedReceiver<Vec<PathBuf>>,
    /// Keeps the debouncer (and its background thread) alive for as long as
    /// this [`VaultWatcher`] is held. Never read directly; its only job is
    /// to not be dropped early.
    _debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
}

/// Start a debounced recursive watch of `dir`, batching filesystem events
/// over `debounce_ms` and forwarding filtered batches to the returned
/// [`VaultWatcher`].
///
/// A path survives the filter only if all of the following hold:
/// - its extension is exactly `md`
/// - it lies inside `dir` (paths notify reports outside the watched root,
///   which should not happen but are defensively excluded, are dropped)
/// - its vault-relative path does not start with `.continuum` (the index
///   directory; the watcher must never react to its own index writes)
///
/// The debouncer callback runs on its own dedicated thread (spawned inside
/// `notify-debouncer-mini`), so the bridge into async code is an unbounded
/// channel: sending must never block that thread, and there is no async
/// runtime available to await on there anyway.
pub(crate) fn spawn(dir: &std::path::Path, debounce_ms: u64) -> Result<VaultWatcher> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let root = dir.to_path_buf();
    let mut debouncer = new_debouncer(
        Duration::from_millis(debounce_ms),
        move |res: DebounceEventResult| {
            let events = match res {
                Ok(events) => events,
                Err(e) => {
                    tracing::warn!(
                        layer = "memory",
                        component = "watcher",
                        error = %e,
                        "vault watcher error"
                    );
                    return;
                }
            };
            let paths: Vec<PathBuf> = events
                .into_iter()
                .filter(|e| matches!(e.kind, DebouncedEventKind::Any))
                .map(|e| e.path)
                .filter(|p| {
                    let is_md = p
                        .extension()
                        .map(|ext| ext.eq_ignore_ascii_case("md"))
                        .unwrap_or(false);
                    let inside_index_dir = p
                        .strip_prefix(&root)
                        .map(|rel| rel.starts_with(".continuum"))
                        .unwrap_or(true);
                    is_md && !inside_index_dir
                })
                .collect();
            if !paths.is_empty() {
                tracing::info!(
                    layer = "memory",
                    component = "watcher",
                    batch_size = paths.len(),
                    "vault watcher batch"
                );
                // The debouncer's callback runs on its own thread with no async
                // runtime; an unbounded send is the non-blocking bridge into
                // the consumer's async loop. The only failure mode is the
                // receiver having been dropped (VaultWatcher discarded), which
                // just means nobody is listening any more — nothing to do.
                let _ = tx.send(paths);
            }
        },
    )
    .map_err(|e| MemoryError::Watch(e.to_string()))?;
    debouncer
        .watcher()
        .watch(dir, RecursiveMode::Recursive)
        .map_err(|e| MemoryError::Watch(e.to_string()))?;
    Ok(VaultWatcher {
        rx,
        _debouncer: debouncer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `spawn` on a directory that does not exist must surface a
    /// [`MemoryError::Watch`] rather than panicking — `notify`'s
    /// `watch()` call fails cleanly for a missing path, and that failure
    /// must propagate as our error type, not be swallowed.
    #[test]
    fn spawn_on_missing_dir_errors() {
        let missing = std::env::temp_dir().join("continuum-watcher-test-does-not-exist-xyz");
        let _ = std::fs::remove_dir_all(&missing);
        let result = spawn(&missing, 500);
        assert!(matches!(result, Err(MemoryError::Watch(_))));
    }
}
