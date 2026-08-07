//! # Shared types for the senses layer
//!
//! Defines the observation structs produced by each watcher and the unified
//! [`PerceptionFrame`] that the frame builder emits. These types are consumed
//! by the triage layer (Layer 2) and stored in the raw log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::senses::live_context::PrivacyDisposition;

/// A single observation from the vision watcher.
///
/// Contains a one-sentence description of what the user is looking at,
/// produced by the local vision model (SmolVLM-256M by default), plus the
/// optional multi-monitor world-state blob.
///
/// # Description vs. world_compact (context engine spec §4.10)
///
/// These are two different things and must not be conflated again:
///
/// - [`description`](ScreenObservation::description) is the **caption** —
///   the one-sentence vision-model output for the monitor this observation
///   came from (privacy-scrubbed, or the redaction marker / empty string
///   for redacted / excluded monitors). Short. This is what the triage
///   prompt, the distiller, the retrieval query builder and the wake
///   "current moment" section render.
/// - [`world_compact`](ScreenObservation::world_compact) is the **compact
///   world-state blob** — `LiveWorldState::compact_for_agents`, ~1.4 kB of
///   every monitor + window + input + project + degradation lines. It is
///   for the context packager (spec §4.9) and **never** goes into the
///   triage prompt: it alone was ~400 tokens of the triage budget (§4.7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenObservation {
    /// One-sentence caption of the screen contents from the vision model.
    pub description: String,
    /// Compact multi-monitor world-state text (`compact_for_agents`).
    ///
    /// Additive and serde-defaulted (context engine spec §4.10, Task B4)
    /// so frames written by older builds — and the benchmark JSONL —
    /// still deserialize. Skipped on serialize when absent. Consumed by
    /// the context packager; excluded from the triage prompt by
    /// construction (`triage::prompts::TriagePromptFrame`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_compact: Option<String>,
    /// Name of the foreground application (e.g., "Code.exe").
    pub foreground_app: String,
    /// Whether the vision model detected an error dialog, stack trace, or similar.
    pub has_error_visible: bool,
    /// Model's confidence in the description (0.0 to 1.0).
    pub confidence: f32,
    /// Path to the saved screenshot JPEG, if screenshots are enabled.
    pub screenshot_path: Option<String>,
    /// When this observation was captured.
    pub ts: DateTime<Utc>,
}

/// A single observation from the audio watcher.
///
/// Contains the transcript of a speech segment detected by VAD and
/// transcribed by whisper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioObservation {
    /// Transcribed text from the speech segment.
    pub transcript: String,
    /// Detected language code (e.g., "en", "nl").
    pub language: String,
    /// Duration of the speech segment in milliseconds.
    pub duration_ms: u64,
    /// Whisper's confidence in the transcription (0.0 to 1.0).
    pub confidence: f32,
    /// When this observation was captured.
    pub ts: DateTime<Utc>,
}

/// A single observation from the context watcher.
///
/// Captures system state via Windows APIs — no AI involved.
///
/// The enrichment fields (`pid`, `exe_path`, `active_since_secs`,
/// `monitor_id` — context engine spec §4.2) are additive and
/// serde-defaulted so observations serialized by older builds still
/// deserialize. `Option` fields are skipped when absent to keep the
/// triage frame JSON compact (token budget, spec §4.7).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextObservation {
    /// Title of the foreground window.
    pub foreground_window_title: String,
    /// Process name of the foreground window (e.g., "Code.exe").
    pub foreground_process_name: String,
    /// Seconds since the user last interacted with the system.
    pub idle_seconds: u64,
    /// Whether the user appears to be in a call (Discord, Teams, Zoom, Meet).
    pub in_call: bool,
    /// Process id of the foreground window's owning process. `None` when
    /// the lookup failed or the observation is the `never_observe`
    /// sentinel (identity of excluded apps must not leak).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Full executable path of the foreground process, scrubbed through
    /// `PrivacyFilter::scrub_path` at collector emit (home prefix → `~`,
    /// username components redacted). `None` for the sentinel or when the
    /// lookup failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
    /// Whole seconds the current (process, title) focus target has been
    /// active, from the sentinel-aware dwell tracker. Resets on every
    /// focus switch and on sentinel↔real transitions.
    #[serde(default)]
    pub active_since_secs: u64,
    /// Hub monitor id (`display-N`) the foreground window sits on, via
    /// `MonitorFromWindow` joined to the live-context monitor geometry.
    /// `None` when mapping fails, no hub is attached, or the observation
    /// is the sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_id: Option<String>,
    /// The privacy zone this observation was emitted under (spec §4.1),
    /// as resolved by the collector — the **authoritative** zone tag.
    ///
    /// Before fixwave 2 the cloud gate re-derived the zone by sniffing two
    /// string literals (`[excluded]`, the redacted-title literal). The
    /// second of those only exists when `[context].redact_sensitive_titles`
    /// is on, so turning that legacy knob off silently un-gated
    /// `local_only` windows. The disposition is computed anyway at collector
    /// emit; carrying it is strictly cheaper and cannot be switched off.
    ///
    /// `None` means "emitted by a build that did not tag" — consumers fall
    /// back to the literal sniffing, so this field is purely additive and
    /// only ever makes the gate *stricter*. See
    /// [`ContextObservation::is_privacy_restricted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<PrivacyDisposition>,
    /// When this observation was captured.
    pub ts: DateTime<Utc>,
}

impl ContextObservation {
    /// Whether this observation carries an authoritative zone tag saying it
    /// is **not** cloud-safe (`Redacted` = `local_only`, `Excluded` =
    /// `never_observe`).
    ///
    /// `false` for an untagged (legacy) observation — callers must still
    /// apply their own literal-based fallback, which is what keeps this
    /// additive.
    pub fn is_privacy_restricted(&self) -> bool {
        self.privacy
            .is_some_and(|d| d != PrivacyDisposition::Visible)
    }
}

/// A unified perception frame combining all three senses.
///
/// The frame builder emits one of these every N seconds (default 3).
/// It includes a [`salience_hint`](PerceptionFrame::salience_hint) score
/// computed from heuristics to pre-filter before reaching the triage LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerceptionFrame {
    /// Unique identifier for this frame.
    pub id: Uuid,
    /// When this frame was assembled.
    pub ts: DateTime<Utc>,
    /// Screen observation from the vision watcher.
    pub screen: ScreenObservation,
    /// Audio observation, if speech was detected in this interval.
    pub audio: Option<AudioObservation>,
    /// Context observation from the Windows API poller.
    pub context: ContextObservation,
    /// Rule-based salience score (0.0 to 1.0).
    ///
    /// Heuristics:
    /// - Frame identical to previous: 0.0
    /// - New error visible on screen: +0.3
    /// - User spoke within last 5s: +0.4
    /// - New window focused: +0.2
    /// - Clamped to \[0.0, 1.0\]
    pub salience_hint: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_observation_world_compact_is_additive() {
        // Legacy JSON (pre-B4, no world_compact key) must still deserialize —
        // the benchmark JSONL and older raw-log exports depend on it.
        let legacy = r#"{
            "description": "VS Code editing main.rs",
            "foreground_app": "Code.exe",
            "has_error_visible": false,
            "confidence": 0.8,
            "screenshot_path": null,
            "ts": "2026-08-05T12:00:00Z"
        }"#;
        let obs: ScreenObservation = serde_json::from_str(legacy).unwrap();
        assert_eq!(obs.description, "VS Code editing main.rs");
        assert_eq!(obs.world_compact, None);

        // Absent → skipped on serialize (nothing pays for the key).
        let json = serde_json::to_string(&obs).unwrap();
        assert!(!json.contains("world_compact"));

        // Present → roundtrips, and the caption is NOT overwritten by it
        // (spec §4.10: two independent fields, two independent consumers).
        let split = ScreenObservation {
            world_compact: Some("live-context/v2 seq=1 [monitor:display-1] …".to_string()),
            ..obs
        };
        let back: ScreenObservation =
            serde_json::from_str(&serde_json::to_string(&split).unwrap()).unwrap();
        assert_eq!(back.description, "VS Code editing main.rs");
        assert_eq!(
            back.world_compact.as_deref(),
            Some("live-context/v2 seq=1 [monitor:display-1] …")
        );
    }

    #[test]
    fn context_observation_enrichment_is_additive() {
        // Legacy JSON (pre-A3, no enrichment keys) must still deserialize.
        let legacy = r#"{
            "foreground_window_title": "main.rs - Continuum",
            "foreground_process_name": "Code.exe",
            "idle_seconds": 3,
            "in_call": false,
            "ts": "2026-08-05T12:00:00Z"
        }"#;
        let obs: ContextObservation = serde_json::from_str(legacy).unwrap();
        assert_eq!(obs.pid, None);
        assert_eq!(obs.exe_path, None);
        assert_eq!(obs.active_since_secs, 0);
        assert_eq!(obs.monitor_id, None);

        // Absent optional fields are skipped on serialize (frame JSON stays
        // compact for the triage token budget, spec §4.7).
        let json = serde_json::to_string(&obs).unwrap();
        assert!(!json.contains("\"pid\""));
        assert!(!json.contains("\"exe_path\""));
        assert!(!json.contains("\"monitor_id\""));
        assert!(json.contains("\"active_since_secs\":0"));

        // Present fields roundtrip.
        let enriched = ContextObservation {
            pid: Some(42),
            exe_path: Some("~\\bin\\Code.exe".to_string()),
            active_since_secs: 17,
            monitor_id: Some("display-1".to_string()),
            ..obs
        };
        let json = serde_json::to_string(&enriched).unwrap();
        let back: ContextObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, Some(42));
        assert_eq!(back.exe_path.as_deref(), Some("~\\bin\\Code.exe"));
        assert_eq!(back.active_since_secs, 17);
        assert_eq!(back.monitor_id.as_deref(), Some("display-1"));
    }
}
