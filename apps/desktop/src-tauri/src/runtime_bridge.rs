//! Bridge to the separately-running `kairo` runtime binary.
//!
//! The headless `kairo` binary owns the llama-cpp-backed triage loop and
//! all perception watchers. In Phase 6 we route dashboard reads through
//! the state store (populated by a small JSON file the binary writes to
//! `~/.kairo-dev/state.json` on every meaningful update) and through a
//! named pipe for control messages.
//!
//! For this initial landing, the bridge implements the **file-tail path
//! only** — the dashboard's own state store stays authoritative for the
//! in-process bits (automations, backups, config edits), while runtime
//! flags (models loaded, current voice mode, etc.) are read from disk.
//! When the `kairo` binary is not running, nothing is read, nothing
//! breaks, and the dashboard falls back to `Unknown`/`Degrading` statuses.

use std::path::PathBuf;

use serde::Deserialize;
use tauri::AppHandle;

use kairo_core::runtime::KairoRuntime;
use kairo_core::state::{StateHandle, VoiceMode};

/// The JSON shape the `kairo` runtime writes.
#[derive(Debug, Default, Deserialize)]
struct RuntimeSnapshot {
    #[serde(default)]
    triage_model_loaded: bool,
    #[serde(default)]
    vision_model_loaded: bool,
    #[serde(default)]
    tts_loaded: bool,
    #[serde(default)]
    stt_loaded: bool,
    #[serde(default)]
    orchestrator_ready: bool,
    #[serde(default)]
    voice_mode: Option<String>,
    #[serde(default)]
    partial_transcript: Option<String>,
}

/// Spawn a ticker that reads `~/.kairo-dev/state.json` every 2 seconds
/// and pushes the flags into the state store. Harmless when the file
/// doesn't exist.
pub fn spawn_ipc_listener(runtime: KairoRuntime, _app: AppHandle) {
    let path = runtime.dev_dir().join("state.json");
    let state = runtime.state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            ticker.tick().await;
            if let Err(e) = tick_once(&path, &state).await {
                tracing::trace!(
                    layer = "dashboard",
                    component = "runtime_bridge",
                    error = %e,
                    "runtime state.json read failed"
                );
            }
        }
    });
}

async fn tick_once(path: &PathBuf, state: &StateHandle) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = tokio::fs::read_to_string(path).await?;
    let snap: RuntimeSnapshot = serde_json::from_str(&contents)?;
    state
        .set_system_flag(|s| {
            s.triage_model_loaded = snap.triage_model_loaded;
            s.vision_model_loaded = snap.vision_model_loaded;
            s.tts_loaded = snap.tts_loaded;
            s.stt_loaded = snap.stt_loaded;
            s.orchestrator_ready = snap.orchestrator_ready;
        })
        .await;
    if let Some(mode) = snap.voice_mode.as_deref() {
        let parsed = match mode {
            "idle" => VoiceMode::Idle,
            "listening" => VoiceMode::Listening,
            "thinking" => VoiceMode::Thinking,
            "speaking" => VoiceMode::Speaking,
            "muted" => VoiceMode::Muted,
            "error" => VoiceMode::Error,
            _ => VoiceMode::Idle,
        };
        state.set_voice_mode(parsed).await;
    }
    if let Some(ref t) = snap.partial_transcript {
        state.set_partial_transcript(t).await;
    }
    Ok(())
}
