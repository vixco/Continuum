//! # Evaluation harness (context engine spec §9, Task C6)
//!
//! Everything the four context-engine benches share: the JSONL record
//! format the perception binary's `--record` flag writes, the committed
//! synthetic fixture and its label sidecar, the replay harness that drives
//! the pipeline under a fake clock, and the small metric helpers the bench
//! binaries print with.
//!
//! ## Where this sits in the layer architecture
//!
//! Nowhere — it is *test infrastructure*. It calls the same functions the
//! frame loop calls (`ProjectResolver::observe`, `SessionStateHub::apply_frame`,
//! `triage::consume::plan_consumption`, `EventSender::send`) but never spins
//! a watcher, never opens a capture device, and never reads the wall clock
//! for anything that ends up in a metric. Frames and events arrive from a
//! file; time is `base + t_ms`.
//!
//! ## The four harnesses (spec §9)
//!
//! | bin | asserts |
//! |---|---|
//! | `continuum-context-bench` | recall at the labeled checkpoints: project ≥ 0.9, goal/task ≥ 0.6, blocker/last-action ≥ 0.8 |
//! | `continuum-dedupe-bench` | ≥ 90 % collapse on the build-failure loop, zero distinct-event loss |
//! | `continuum-memory-precision-bench` | duplicate rate ≤ 10 %, precision vs labels ≥ 70 %, later-used report |
//! | `continuum-redaction-bench` | zero secrets end to end incl. every MCP tool response; git commit ids survive |
//!
//! The redaction bench lives in `continuum-mcp` (`crates/continuum-mcp/src/bin/`)
//! rather than here, because it must call the real MCP tool handlers and
//! `continuum-core` cannot depend on `continuum-mcp` without a dependency
//! cycle. Everything it replays comes from this module.
//!
//! ## Mock vs live
//!
//! Two pipeline steps need an LLM: triage classification (spec §4.7) and
//! session-state inference (spec §4.8). The replay supports both a
//! deterministic mock ([`replay::mock_classify`], [`replay::mock_infer_json`])
//! and the real Qwen model. **Mock is the default** — the benches must pass
//! on a machine with no GPU and no model file, in seconds, so they can gate
//! a change. Live mode (`--live`) is opt-in and is what actually measures
//! model quality; see the module docs on [`replay`] for what each mode
//! proves and what it does not.

pub mod fixture;
pub mod metrics;
pub mod record;
pub mod replay;
pub mod score;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic suffix so two [`BenchDir`]s created in the same millisecond
/// still differ.
static BENCH_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory for a bench run, removed on drop.
///
/// A bench must never write into the user's data directory: it opens
/// databases, writes vault notes and deletes rows. This is a three-line
/// stand-in for `tempfile::TempDir` so the *shipping* crate does not gain a
/// dependency for the sake of the benches (`tempfile` stays a
/// dev-dependency, used by unit tests).
///
/// Cleanup is best effort — a locked SQLite file on Windows must not make a
/// bench fail after it has already produced its verdict.
#[derive(Debug)]
pub struct BenchDir {
    path: PathBuf,
}

impl BenchDir {
    /// Creates `%TEMP%/continuum-bench-<pid>-<seq>/`.
    pub fn new(prefix: &str) -> std::io::Result<Self> {
        let seq = BENCH_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "continuum-bench-{prefix}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BenchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_dirs_are_unique_and_self_cleaning() {
        let first = BenchDir::new("unit").unwrap();
        let second = BenchDir::new("unit").unwrap();
        assert_ne!(first.path(), second.path());
        assert!(first.path().is_dir());
        std::fs::write(first.path().join("scratch"), b"x").unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists(), "the directory is removed on drop");
    }
}
