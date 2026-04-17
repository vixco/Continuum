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
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Emitter};

use kairo_core::runtime::KairoRuntime;
use kairo_core::runtime_publish::RuntimeSnapshot;
use kairo_core::state::{StateHandle, VoiceMode};

/// Spawn a ticker that reads `~/.kairo-dev/state.json` every 2 seconds
/// and pushes the flags into the state store. Harmless when the file
/// doesn't exist; parse errors are emitted as a `kairo:runtime_error`
/// event so the dashboard can surface "state.json is corrupt" rather than
/// silently showing stale flags.
pub fn spawn_ipc_listener(runtime: KairoRuntime, app: AppHandle) {
    let path = runtime.dev_dir().join("state.json");
    let state = runtime.state.clone();
    // One-shot latch so we don't spam the frontend on every tick while the
    // runtime is writing a malformed file.
    let last_was_err = std::sync::Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            ticker.tick().await;
            match tick_once(&path, &state).await {
                Ok(()) => {
                    if last_was_err.swap(false, Ordering::AcqRel) {
                        let _ = app.emit("kairo:runtime_error", serde_json::Value::Null);
                    }
                }
                Err(e) => {
                    // Only emit once per error streak — Tauri IPC is cheap
                    // but the UI should only toast once.
                    if !last_was_err.swap(true, Ordering::AcqRel) {
                        let _ = app.emit(
                            "kairo:runtime_error",
                            serde_json::json!({
                                "path": path.display().to_string(),
                                "error": e.to_string(),
                            }),
                        );
                    }
                    tracing::debug!(
                        layer = "dashboard",
                        component = "runtime_bridge",
                        error = %e,
                        "runtime state.json read failed"
                    );
                }
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
