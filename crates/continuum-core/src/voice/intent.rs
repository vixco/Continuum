//! # Voice intent-file protocol
//!
//! The dashboard process and the daemon process are separate, so dashboard
//! commands that need to influence live daemon behavior (like push-to-talk)
//! travel through the filesystem — the same pattern [`workers::intent`] uses
//! for MCP→runtime spawn/cancel calls.
//!
//! ## Directory layout
//!
//! ```text
//! ~/.continuum-dev/
//! └── voice-intents/                ← dashboard writes, daemon drains on each tick
//!     ├── 20260416T123456789-talk_now.json
//!     └── ...
//! ```
//!
//! The directory is created on demand by [`ensure_intents_dir`].

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Subdirectory under the Continuum data dir where the dashboard writes intents.
pub const VOICE_INTENTS_SUBDIR: &str = "voice-intents";

/// Intents older than this many milliseconds are silently dropped on drain.
/// Self-healing for crashes that leave stale files behind: a stale `talk_now`
/// firing on next launch would be surprising.
const STALE_INTENT_TTL_MS: u64 = 30_000;

/// Top-level envelope for a voice intent. One file = one intent.
///
/// Future variants (`Cancel`, `Mute`) slot in without touching the on-disk
/// schema thanks to serde's tagged enum representation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceIntent {
    /// Open a listening session right now. Equivalent to a hotkey press.
    TalkNow {
        /// Wall-clock timestamp (ms since epoch) when the dashboard issued
        /// the click. Used to drop stale intents from before a crash.
        ts_ms: u64,
    },
}

impl VoiceIntent {
    /// Convenience constructor that stamps the current wall-clock time.
    pub fn talk_now() -> Self {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self::TalkNow { ts_ms }
    }

    fn ts_ms(&self) -> u64 {
        match self {
            Self::TalkNow { ts_ms } => *ts_ms,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::TalkNow { .. } => "talk_now",
        }
    }
}

/// Returns the voice-intents directory for a given Continuum data dir, creating
/// it on first call.
pub fn ensure_intents_dir(data_dir: &Path) -> Result<PathBuf> {
    let p = data_dir.join(VOICE_INTENTS_SUBDIR);
    std::fs::create_dir_all(&p)
        .with_context(|| format!("Failed to create voice intents dir at {}", p.display()))?;
    Ok(p)
}

/// Writes a new intent file. Returns the path it was written to.
///
/// Atomic via `.tmp` + rename so a partially-written file can never be
/// observed by the daemon mid-write.
pub fn write_intent(data_dir: &Path, intent: &VoiceIntent) -> Result<PathBuf> {
    let dir = ensure_intents_dir(data_dir)?;
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
    let path = dir.join(format!("{ts}-{}.json", intent.kind_label()));
    let tmp = path.with_extension("json.tmp");
    let payload =
        serde_json::to_string_pretty(intent).context("Failed to serialize voice intent")?;
    std::fs::write(&tmp, payload)
        .with_context(|| format!("Failed to write voice intent tmp at {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        format!(
            "Failed to rename voice intent {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(path)
}

/// Reads and removes every intent file from the directory. Stale intents
/// (older than `STALE_INTENT_TTL_MS`) are dropped silently. Unparseable files
/// are renamed with a `.bad` suffix so they don't starve the loop but can
/// still be inspected.
pub fn drain_intents(data_dir: &Path) -> Result<Vec<VoiceIntent>> {
    let dir = ensure_intents_dir(data_dir)?;
    let mut out = Vec::new();

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };

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

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    for path in entries {
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        match serde_json::from_str::<VoiceIntent>(&contents) {
            Ok(intent) => {
                let _ = std::fs::remove_file(&path);
                let age = now_ms.saturating_sub(intent.ts_ms());
                if age > STALE_INTENT_TTL_MS {
                    tracing::debug!(
                        layer = "voice",
                        component = "intent",
                        age_ms = age,
                        "Dropping stale voice intent"
                    );
                    continue;
                }
                out.push(intent);
            }
            Err(e) => {
                tracing::warn!(
                    layer = "voice",
                    component = "intent",
                    path = %path.display(),
                    error = %e,
                    "Skipping unparseable voice intent"
                );
                let _ = std::fs::rename(&path, path.with_extension("bad"));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_and_drain_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let intent = VoiceIntent::talk_now();
        let p = write_intent(tmp.path(), &intent).unwrap();
        assert!(p.exists());

        let intents = drain_intents(tmp.path()).unwrap();
        assert_eq!(intents.len(), 1);
        assert!(matches!(intents[0], VoiceIntent::TalkNow { .. }));
        // File should be gone after drain.
        assert!(!p.exists());
    }

    #[test]
    fn bad_json_is_renamed_not_reread() {
        let tmp = TempDir::new().unwrap();
        let dir = ensure_intents_dir(tmp.path()).unwrap();
        let bad = dir.join("20260416T000000000-oops.json");
        std::fs::write(&bad, "{ not json").unwrap();

        let intents = drain_intents(tmp.path()).unwrap();
        assert!(intents.is_empty());
        assert!(!bad.exists());
        assert!(bad.with_extension("bad").exists());
    }

    #[test]
    fn stale_intent_is_dropped() {
        let tmp = TempDir::new().unwrap();
        // Write an intent stamped 5 minutes in the past.
        let stale_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 300_000;
        let intent = VoiceIntent::TalkNow { ts_ms: stale_ts };
        let p = write_intent(tmp.path(), &intent).unwrap();
        assert!(p.exists());

        let intents = drain_intents(tmp.path()).unwrap();
        assert!(intents.is_empty(), "stale intent should be dropped");
        // File still consumed (removed), just not surfaced.
        assert!(!p.exists());
    }

    #[test]
    fn drain_on_missing_dir_creates_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(VOICE_INTENTS_SUBDIR);
        assert!(!dir.exists());
        let intents = drain_intents(tmp.path()).unwrap();
        assert!(intents.is_empty());
        assert!(dir.exists(), "drain should ensure_dir as a side effect");
    }
}
