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
    /// Master playback gain currently applied by the running voice process.
    /// `None` keeps older runtime snapshots backwards-compatible.
    #[serde(default)]
    pub voice_volume: Option<f32>,
    /// Number of TTS utterances waiting for synthesis/playback, including the
    /// utterance currently being processed.
    #[serde(default)]
    pub tts_queue_len: Option<usize>,
    /// Whether call detection is actively suppressing voice output.
    #[serde(default)]
    pub ambient_mute_active: Option<bool>,
    /// Foreground process that caused ambient mute to activate.
    #[serde(default)]
    pub detected_call_app: Option<String>,
    /// Wake-word setting applied by this runtime process at boot.
    #[serde(default)]
    pub wake_word_enabled: Option<bool>,
    /// Active voice front-end mode (`"pipeline"` / `"moshi"`). `None` on
    /// snapshots from runtimes that predate the Moshi front-end.
    #[serde(default)]
    pub voice_frontend_mode: Option<String>,
    /// Whether the Moshi S2S backend is loaded+connected. `None` on older
    /// snapshots; `Some(false)` means Moshi mode is selected but the backend
    /// isn't up yet (binary missing, CUDA unavailable, still connecting).
    #[serde(default)]
    pub moshi_loaded: Option<bool>,
    #[serde(default)]
    pub frame_count: u64,
    #[serde(default)]
    pub monitor_count: usize,
    #[serde(default)]
    pub capture_event_count: u64,
    #[serde(default)]
    pub dropped_capture_event_count: u64,
    #[serde(default)]
    pub last_capture_at: Option<chrono::DateTime<chrono::Utc>>,
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
    /// Memory-vault curator (Plan B) health — last pass time, consecutive
    /// failures, and pending/written counts. `None` only for snapshots
    /// written before this field existed (`#[serde(default)]` keeps old
    /// `state.json` files parsing); the runtime always publishes `Some`
    /// once it starts, using `enabled: false` and zeroed counters when the
    /// curator hasn't spawned (no triage model loaded). Read by the
    /// dashboard's Curator row.
    #[serde(default)]
    pub curator: Option<CuratorSnapshot>,
}

/// Curator (Plan B memory-vault) health surfaced to the dashboard. Mirrors
/// [`crate::curator::CuratorStatus`] plus an `enabled` flag derived from
/// config — see `build_curator_snapshot` in the `continuum` binary, which
/// fills this in on every publish tick from the curator's
/// [`crate::curator::SharedCuratorStatus`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorSnapshot {
    /// RFC3339 timestamp of the most recent curator pass, successful or
    /// not. `None` until the first pass completes.
    #[serde(default)]
    pub last_pass_at: Option<String>,
    /// Consecutive failed passes. The dashboard shows a warning badge once
    /// this crosses the same threshold (3) the repair agent escalates on.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Lifetime count of candidate/confirmed notes the curator has written.
    #[serde(default)]
    pub candidates_written_total: u64,
    /// Current count of notes awaiting human review.
    #[serde(default)]
    pub pending_count: u64,
    /// Whether the curator pipeline is actually running: both
    /// `[memory.curator] enabled = true` in config and a triage model
    /// loaded at boot. `false` (with zeroed counters above) means the
    /// dashboard should render "Curator: off".
    #[serde(default)]
    pub enabled: bool,
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
            voice_volume: Some(0.65),
            tts_queue_len: Some(2),
            last_update: "2026-04-14T10:00:00Z".into(),
            ..RuntimeSnapshot::default()
        };
        write_snapshot(&path, &snap).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"triage_model_loaded\": true"));
        assert!(contents.contains("\"voice_mode\": \"listening\""));
        assert!(contents.contains("\"voice_volume\": 0.65"));
        assert!(contents.contains("\"tts_queue_len\": 2"));
    }

    #[test]
    fn older_snapshot_without_voice_telemetry_still_deserializes() {
        let snapshot: RuntimeSnapshot =
            serde_json::from_str(r#"{"voice_mode":"idle","last_update":"2026-08-03T00:00:00Z"}"#)
                .unwrap();

        assert_eq!(snapshot.voice_mode.as_deref(), Some("idle"));
        assert_eq!(snapshot.voice_volume, None);
        assert_eq!(snapshot.tts_queue_len, None);
        assert_eq!(snapshot.ambient_mute_active, None);
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

    /// Task 11: a `state.json` written before the `curator` field existed
    /// must still parse — `#[serde(default)]` on both the field and every
    /// `CuratorSnapshot` member is what makes that true.
    #[test]
    fn snapshot_deserializes_without_curator_field() {
        let json = r#"{
            "triage_model_loaded": true,
            "vision_model_loaded": false,
            "tts_loaded": false,
            "stt_loaded": false,
            "orchestrator_ready": true,
            "frame_count": 12,
            "wake_count": 1,
            "last_update": "2026-04-14T10:00:00Z"
        }"#;
        let snap: RuntimeSnapshot = serde_json::from_str(json).unwrap();
        assert!(snap.curator.is_none());
    }

    /// Round trip with the field present: serialize, reparse, and confirm
    /// every `CuratorSnapshot` member survives — this is the shape the
    /// `continuum` binary's publisher actually writes once a curator status
    /// is available.
    #[test]
    fn snapshot_roundtrip_with_curator_field() {
        let snap = RuntimeSnapshot {
            curator: Some(CuratorSnapshot {
                last_pass_at: Some("2026-04-14T10:05:00+00:00".to_string()),
                consecutive_failures: 2,
                candidates_written_total: 7,
                pending_count: 3,
                enabled: true,
            }),
            last_update: "2026-04-14T10:05:02Z".into(),
            ..RuntimeSnapshot::default()
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        let curator = parsed.curator.expect("curator field should round-trip");
        assert_eq!(
            curator.last_pass_at.as_deref(),
            Some("2026-04-14T10:05:00+00:00")
        );
        assert_eq!(curator.consecutive_failures, 2);
        assert_eq!(curator.candidates_written_total, 7);
        assert_eq!(curator.pending_count, 3);
        assert!(curator.enabled);
    }

    /// The "curator never spawned" shape (Task 11 brief): `Some` with
    /// `enabled: false` and zeroed counters, not `None` — the dashboard
    /// tells "off" apart from "old state.json" this way.
    #[test]
    fn snapshot_roundtrip_curator_disabled_is_some_with_zeros() {
        let snap = RuntimeSnapshot {
            curator: Some(CuratorSnapshot::default()),
            ..RuntimeSnapshot::default()
        };
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: RuntimeSnapshot = serde_json::from_str(&json).unwrap();
        let curator = parsed.curator.expect("curator field should round-trip");
        assert!(!curator.enabled);
        assert_eq!(curator.consecutive_failures, 0);
        assert_eq!(curator.candidates_written_total, 0);
        assert_eq!(curator.pending_count, 0);
        assert!(curator.last_pass_at.is_none());
    }
}
