//! # Voice intent-file protocol
//!
//! The dashboard process and the daemon process are separate, so dashboard
//! commands that influence live voice behavior travel through the filesystem.
//! The protocol supports one-shot push-to-talk plus a persistent Chat voice
//! session and native TTS playback requests.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Subdirectory under the Continuum data dir where the dashboard writes intents.
pub const VOICE_INTENTS_SUBDIR: &str = "voice-intents";

/// Intents older than this many milliseconds are silently dropped on drain.
const STALE_INTENT_TTL_MS: u64 = 30_000;

/// Top-level envelope for a voice intent. One file = one intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VoiceIntent {
    /// Open a one-shot listening session right now. Equivalent to a hotkey press.
    TalkNow { ts_ms: u64 },
    /// Start persistent Chat voice mode. Spoken turns bypass wake-word routing
    /// and are published for the desktop Chat surface instead.
    ChatStart { ts_ms: u64 },
    /// Stop persistent Chat voice mode and discard any partial voice turn.
    ChatStop { ts_ms: u64 },
    /// Speak a completed Chat response through Continuum's native TTS engine.
    Speak { ts_ms: u64, text: String },
}

impl VoiceIntent {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn talk_now() -> Self {
        Self::TalkNow { ts_ms: Self::now_ms() }
    }

    pub fn chat_start() -> Self {
        Self::ChatStart { ts_ms: Self::now_ms() }
    }

    pub fn chat_stop() -> Self {
        Self::ChatStop { ts_ms: Self::now_ms() }
    }

    pub fn speak(text: impl Into<String>) -> Self {
        Self::Speak {
            ts_ms: Self::now_ms(),
            text: text.into(),
        }
    }

    fn ts_ms(&self) -> u64 {
        match self {
            Self::TalkNow { ts_ms }
            | Self::ChatStart { ts_ms }
            | Self::ChatStop { ts_ms }
            | Self::Speak { ts_ms, .. } => *ts_ms,
        }
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::TalkNow { .. } => "talk_now",
            Self::ChatStart { .. } => "chat_start",
            Self::ChatStop { .. } => "chat_stop",
            Self::Speak { .. } => "speak",
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

/// Writes a new intent file. Atomic via `.tmp` + rename.
pub fn write_intent(data_dir: &Path, intent: &VoiceIntent) -> Result<PathBuf> {
    let dir = ensure_intents_dir(data_dir)?;
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
    let path = dir.join(format!("{ts}-{}.json", intent.kind_label()));
    let tmp = path.with_extension("json.tmp");
    let payload = serde_json::to_string_pretty(intent).context("Failed to serialize voice intent")?;
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

/// Reads and removes every intent file from the directory. Stale intents are
/// dropped. Unparseable files are renamed with a `.bad` suffix.
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

    let now_ms = SelfTime::now_ms();

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

struct SelfTime;
impl SelfTime {
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn all_voice_intents_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let intents = [
            VoiceIntent::talk_now(),
            VoiceIntent::chat_start(),
            VoiceIntent::speak("hello"),
            VoiceIntent::chat_stop(),
        ];
        for intent in &intents {
            write_intent(tmp.path(), intent).unwrap();
        }
        let drained = drain_intents(tmp.path()).unwrap();
        assert_eq!(drained.len(), intents.len());
        assert_eq!(drained, intents);
    }

    #[test]
    fn speak_keeps_exact_text() {
        let tmp = TempDir::new().unwrap();
        let intent = VoiceIntent::speak("Hello, world — test 123.");
        write_intent(tmp.path(), &intent).unwrap();
        let drained = drain_intents(tmp.path()).unwrap();
        assert!(matches!(
            drained.as_slice(),
            [VoiceIntent::Speak { text, .. }] if text == "Hello, world — test 123."
        ));
    }

    #[test]
    fn bad_json_is_renamed_not_reread() {
        let tmp = TempDir::new().unwrap();
        let dir = ensure_intents_dir(tmp.path()).unwrap();
        let bad = dir.join("20260416T000000000-oops.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(drain_intents(tmp.path()).unwrap().is_empty());
        assert!(!bad.exists());
        assert!(bad.with_extension("bad").exists());
    }

    #[test]
    fn stale_intent_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let stale_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            - 300_000;
        let intent = VoiceIntent::TalkNow { ts_ms: stale_ts };
        let p = write_intent(tmp.path(), &intent).unwrap();
        assert!(drain_intents(tmp.path()).unwrap().is_empty());
        assert!(!p.exists());
    }

    #[test]
    fn drain_on_missing_dir_creates_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(VOICE_INTENTS_SUBDIR);
        assert!(!dir.exists());
        assert!(drain_intents(tmp.path()).unwrap().is_empty());
        assert!(dir.exists());
    }
}
