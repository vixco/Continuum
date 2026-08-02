//! # Shared types for the senses layer
//!
//! Defines the observation structs produced by each watcher and the unified
//! [`PerceptionFrame`] that the frame builder emits. These types are consumed
//! by the triage layer (Layer 2) and stored in the raw log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single observation from the vision watcher.
///
/// Contains a one-sentence description of what the user is looking at,
/// produced by the local vision model (SmolVLM-256M by default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenObservation {
    /// One-sentence description of the screen contents.
    pub description: String,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextObservation {
    /// Title of the foreground window.
    pub foreground_window_title: String,
    /// Process name of the foreground window (e.g., "Code.exe").
    pub foreground_process_name: String,
    /// Seconds since the user last interacted with the system.
    pub idle_seconds: u64,
    /// Whether the user appears to be in a call (Discord, Teams, Zoom, Meet).
    pub in_call: bool,
    /// When this observation was captured.
    pub ts: DateTime<Utc>,
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
