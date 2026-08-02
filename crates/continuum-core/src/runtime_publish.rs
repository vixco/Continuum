//! # Runtime state publisher
//!
//! The `continuum` binary and the desktop dashboard run as separate processes
//! (because the runtime pulls in llama-cpp-sys-2, which has Windows build
//! quirks we don't want dragged into the Tauri build). Their shared
//! surface is this tiny JSON blob: the runtime writes it every few
//! seconds, the dashboard's `runtime_bridge` reads it.
//!
//! Keep this struct **small**. Every field the runtime writes here must
//! be cheap to compute on a hot loop. Bulk data (logs, memory) goes
//! through dedicated channels.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::hardware::{HardwareSpecs, ResolvedResourcePlan};

/// The shared shape the runtime writes and the dashboard reads. This is
/// the single source of truth for the state.json contract — both the
/// `continuum` binary and the `continuum-desktop` bridge serialise against this
/// struct. Any new runtime telemetry field goes here, not in a parallel
/// definition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub triage_model_loaded: bool,
    #[serde(default)]
    pub vision_model_loaded: bool,
    #[serde(default)]
    pub tts_loaded: bool,
    #[serde(default)]
    pub stt_loaded: bool,
    #[serde(default)]
    pub orchestrator_ready: bool,
    #[serde(default)]
    pub voice_mode: Option<String>,
    #[serde(default)]
    pub partial_transcript: Option<String>,
    #[serde(default)]
    pub frame_count: u64,
    #[serde(default)]
    pub wake_count: u64,
    #[serde(default)]
    pub last_update: String,
    /// Detected host hardware (probed once at boot). `None` until the runtime
    /// has run `probe_hardware`. Read by the dashboard's resource panel.
    #[serde(default)]
    pub hardware_specs: Option<HardwareSpecs>,
    /// Resolved adaptive resource plan (computed once at boot from
    /// `hardware_specs` + `[resources]` config). `None` until the runtime
    /// has resolved it. Read by the dashboard's resource panel.
    #[serde(default)]
    pub resource_plan: Option<ResolvedResourcePlan>,
}

pub fn write_snapshot(path: &Path, snapshot: &RuntimeSnapshot) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(snapshot)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Spawn a background ticker that polls the provided snapshot provider
/// every `interval_secs` and writes the result to `path`. Exits cleanly
/// on shutdown.
pub fn spawn_publisher<F>(
    path: std::path::PathBuf,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
    snapshot_fn: F,
) where
    F: Fn() -> RuntimeSnapshot + Send + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let snap = snapshot_fn();
                    if let Err(e) = write_snapshot(&path, &snap) {
                        tracing::trace!(
                            layer = "system",
                            component = "runtime_publish",
                            error = %e,
                            "snapshot write failed"
                        );
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_snapshot_creates_json_at_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        let snap = RuntimeSnapshot {
            triage_model_loaded: true,
            voice_mode: Some("listening".into()),
            last_update: "2026-04-14T10:00:00Z".into(),
            ..RuntimeSnapshot::default()
        };
        write_snapshot(&path, &snap).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"triage_model_loaded\": true"));
        assert!(contents.contains("\"voice_mode\": \"listening\""));
    }

    #[test]
    fn write_snapshot_is_atomic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        // First write.
        write_snapshot(
            &path,
            &RuntimeSnapshot {
                frame_count: 1,
                ..RuntimeSnapshot::default()
            },
        )
        .unwrap();
        // Second write — the old .tmp should not linger.
        write_snapshot(
            &path,
            &RuntimeSnapshot {
                frame_count: 2,
                ..RuntimeSnapshot::default()
            },
        )
        .unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }
}
