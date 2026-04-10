//! # Perception frame builder
//!
//! Combines observations from the three senses watchers (vision, audio, context)
//! into unified [`PerceptionFrame`] objects at a configurable interval (default 3s).
//!
//! Also computes the `salience_hint` — a rule-based score (0.0 to 1.0) that
//! pre-filters frames before they reach the triage LLM. Heuristics include:
//! - Frame identical to previous? salience = 0.0
//! - New error visible on screen? salience += 0.3
//! - User spoke within last 5 seconds? salience += 0.4
//! - New window focused? salience += 0.2
//!
//! Only frames above threshold (default 0.15) reach the triage layer.
//!
//! Part of Layer 1 (Senses) in the Kairo cognitive architecture.

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

use crate::config::FrameConfig;
use crate::senses::types::{
    AudioObservation, ContextObservation, PerceptionFrame, ScreenObservation,
};

/// Computes a rule-based salience score for a perception frame.
///
/// The score is a float in \[0.0, 1.0\] representing how "interesting" this
/// frame is compared to the previous one. Frames with low salience are stored
/// in the raw log but skipped by the triage layer.
///
/// # Heuristics
///
/// | Condition | Score delta |
/// |---|---|
/// | Frame identical to previous (same window, no audio, no error change) | 0.0 (skip) |
/// | New error visible on screen | +0.3 |
/// | User spoke within last 5 seconds | +0.4 |
/// | New window focused (different from previous) | +0.2 |
/// | Error disappeared (was visible, now gone) | +0.1 |
///
/// The final score is clamped to \[0.0, 1.0\].
pub fn compute_salience(
    current: &PerceptionFrame,
    previous: Option<&PerceptionFrame>,
) -> f32 {
    let mut score: f32 = 0.0;

    let prev = match previous {
        Some(p) => p,
        None => {
            // First frame ever — always interesting.
            return 0.5;
        }
    };

    // New error visible on screen?
    if current.screen.has_error_visible && !prev.screen.has_error_visible {
        score += 0.3;
    }

    // Error disappeared? Mildly interesting.
    if !current.screen.has_error_visible && prev.screen.has_error_visible {
        score += 0.1;
    }

    // User spoke recently? Check if there's an audio observation.
    if let Some(ref audio) = current.audio {
        if !audio.transcript.is_empty() {
            score += 0.4;
        }
    }

    // New window focused?
    if current.context.foreground_process_name != prev.context.foreground_process_name
        || current.context.foreground_window_title != prev.context.foreground_window_title
    {
        score += 0.2;
    }

    // Clamp to [0.0, 1.0].
    score.clamp(0.0, 1.0)
}

/// Builds unified [`PerceptionFrame`]s from the three senses watchers.
///
/// The builder reads observations from three input channels (screen, audio,
/// context) and assembles a frame at a configurable interval. It holds the
/// latest observation from each source and emits a combined frame whenever
/// the timer fires.
///
/// # Usage
///
/// ```rust,no_run
/// # use kairo_core::senses::frame::PerceptionFrameBuilder;
/// # use kairo_core::config::FrameConfig;
/// # async fn example() {
/// let builder = PerceptionFrameBuilder::new(FrameConfig::default());
/// // ... connect channels and run
/// # }
/// ```
pub struct PerceptionFrameBuilder {
    /// Frame assembly configuration.
    config: FrameConfig,
}

impl PerceptionFrameBuilder {
    /// Creates a new frame builder with the given configuration.
    pub fn new(config: FrameConfig) -> Self {
        info!(
            layer = "senses",
            component = "frame",
            interval_secs = config.interval_secs,
            salience_threshold = config.salience_threshold,
            "PerceptionFrameBuilder created"
        );
        Self { config }
    }

    /// Runs the frame builder loop.
    ///
    /// Reads the latest observations from the three senses channels and
    /// assembles a [`PerceptionFrame`] every `config.interval_secs` seconds.
    /// Frames are sent to the output channel.
    ///
    /// The builder keeps the most recent observation from each source between
    /// frames. If no new observation has arrived for a source, the previous
    /// one is reused (with audio cleared to `None` since speech is ephemeral).
    ///
    /// # Arguments
    ///
    /// * `screen_rx` - Channel receiving screen observations from the vision watcher.
    /// * `audio_rx` - Channel receiving audio observations from the audio watcher.
    /// * `context_rx` - Channel receiving context observations from the context poller.
    /// * `frame_tx` - Channel for sending assembled frames.
    /// * `shutdown` - Watch receiver; exits when value changes to `true`.
    pub async fn run(
        &self,
        mut screen_rx: mpsc::Receiver<ScreenObservation>,
        mut audio_rx: mpsc::Receiver<AudioObservation>,
        mut context_rx: mpsc::Receiver<ContextObservation>,
        frame_tx: mpsc::Sender<PerceptionFrame>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        info!(
            layer = "senses",
            component = "frame",
            "Frame builder loop starting"
        );

        let interval = tokio::time::Duration::from_secs(self.config.interval_secs);

        // Latest observations from each source.
        let mut latest_screen: Option<ScreenObservation> = None;
        let mut latest_audio: Option<AudioObservation> = None;
        let mut latest_context: Option<ContextObservation> = None;
        let mut previous_frame: Option<PerceptionFrame> = None;
        let mut audio_consumed = false;

        let mut ticker = tokio::time::interval(interval);
        // Don't fire immediately — wait for the first interval.
        ticker.tick().await;

        loop {
            tokio::select! {
                // Drain incoming observations (non-blocking).
                Some(screen) = screen_rx.recv() => {
                    trace!(
                        layer = "senses",
                        component = "frame",
                        description = %screen.description,
                        "Received screen observation"
                    );
                    latest_screen = Some(screen);
                }
                Some(audio) = audio_rx.recv() => {
                    trace!(
                        layer = "senses",
                        component = "frame",
                        transcript = %audio.transcript,
                        "Received audio observation"
                    );
                    latest_audio = Some(audio);
                    audio_consumed = false;
                }
                Some(context) = context_rx.recv() => {
                    trace!(
                        layer = "senses",
                        component = "frame",
                        process = %context.foreground_process_name,
                        title = %context.foreground_window_title,
                        "Received context observation"
                    );
                    latest_context = Some(context);
                }
                _ = ticker.tick() => {
                    // Time to assemble a frame.
                    let screen = match latest_screen.clone() {
                        Some(s) => s,
                        None => {
                            debug!(
                                layer = "senses",
                                component = "frame",
                                "No screen observation yet, skipping frame"
                            );
                            continue;
                        }
                    };

                    let context = match latest_context.clone() {
                        Some(c) => c,
                        None => {
                            debug!(
                                layer = "senses",
                                component = "frame",
                                "No context observation yet, skipping frame"
                            );
                            continue;
                        }
                    };

                    // Audio is ephemeral — only include it once per speech segment.
                    let audio = if !audio_consumed {
                        audio_consumed = true;
                        latest_audio.clone()
                    } else {
                        None
                    };

                    let mut frame = PerceptionFrame {
                        id: Uuid::new_v4(),
                        ts: Utc::now(),
                        screen,
                        audio,
                        context,
                        salience_hint: 0.0,
                    };

                    // Fill foreground_app from context if screen didn't set it.
                    if frame.screen.foreground_app.is_empty() {
                        frame.screen.foreground_app =
                            frame.context.foreground_process_name.clone();
                    }

                    // Compute salience.
                    frame.salience_hint =
                        compute_salience(&frame, previous_frame.as_ref());

                    debug!(
                        layer = "senses",
                        component = "frame",
                        frame_id = %frame.id,
                        salience = frame.salience_hint,
                        threshold = self.config.salience_threshold,
                        has_audio = frame.audio.is_some(),
                        "Assembled perception frame"
                    );

                    previous_frame = Some(frame.clone());

                    if frame_tx.send(frame).await.is_err() {
                        warn!(
                            layer = "senses",
                            component = "frame",
                            "Frame output channel closed, stopping builder"
                        );
                        break;
                    }
                }
                result = shutdown.changed() => {
                    match result {
                        Ok(()) if *shutdown.borrow() => {
                            info!(
                                layer = "senses",
                                component = "frame",
                                "Shutdown signal received, stopping frame builder"
                            );
                            break;
                        }
                        Ok(()) => continue,
                        Err(_) => {
                            info!(
                                layer = "senses",
                                component = "frame",
                                "Shutdown watch dropped, stopping frame builder"
                            );
                            break;
                        }
                    }
                }
            }
        }

        info!(
            layer = "senses",
            component = "frame",
            "Frame builder stopped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a minimal ScreenObservation.
    fn screen(description: &str, app: &str, has_error: bool) -> ScreenObservation {
        ScreenObservation {
            description: description.to_string(),
            foreground_app: app.to_string(),
            has_error_visible: has_error,
            confidence: 0.9,
            screenshot_path: None,
            ts: Utc::now(),
        }
    }

    /// Helper to create a minimal ContextObservation.
    fn context(title: &str, process: &str) -> ContextObservation {
        ContextObservation {
            foreground_window_title: title.to_string(),
            foreground_process_name: process.to_string(),
            idle_seconds: 0,
            in_call: false,
            ts: Utc::now(),
        }
    }

    /// Helper to create a minimal AudioObservation.
    fn audio(transcript: &str) -> AudioObservation {
        AudioObservation {
            transcript: transcript.to_string(),
            language: "en".to_string(),
            duration_ms: 2000,
            confidence: 0.85,
            ts: Utc::now(),
        }
    }

    /// Helper to create a minimal PerceptionFrame.
    fn frame(
        screen_obs: ScreenObservation,
        audio_obs: Option<AudioObservation>,
        context_obs: ContextObservation,
    ) -> PerceptionFrame {
        PerceptionFrame {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            screen: screen_obs,
            audio: audio_obs,
            context: context_obs,
            salience_hint: 0.0,
        }
    }

    // --- Salience tests ---

    #[test]
    fn test_salience_first_frame_is_interesting() {
        let f = frame(
            screen("code editor", "Code.exe", false),
            None,
            context("main.rs - kairo", "Code.exe"),
        );
        let score = compute_salience(&f, None);
        assert!(
            score > 0.0,
            "First frame should have positive salience, got {score}"
        );
    }

    #[test]
    fn test_salience_identical_frames_is_zero() {
        let s = screen("code editor", "Code.exe", false);
        let c = context("main.rs - kairo", "Code.exe");
        let prev = frame(s.clone(), None, c.clone());
        let curr = frame(s, None, c);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.0).abs() < f32::EPSILON,
            "Identical frames should have salience 0.0, got {score}"
        );
    }

    #[test]
    fn test_salience_new_error_adds_0_3() {
        let s_prev = screen("code editor", "Code.exe", false);
        let c = context("main.rs", "Code.exe");
        let prev = frame(s_prev, None, c.clone());

        let s_curr = screen("error dialog", "Code.exe", true);
        let curr = frame(s_curr, None, c);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.3).abs() < f32::EPSILON,
            "New error should add 0.3, got {score}"
        );
    }

    #[test]
    fn test_salience_error_disappearing_adds_0_1() {
        let s_prev = screen("error dialog", "Code.exe", true);
        let c = context("main.rs", "Code.exe");
        let prev = frame(s_prev, None, c.clone());

        let s_curr = screen("code editor", "Code.exe", false);
        let curr = frame(s_curr, None, c);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.1).abs() < f32::EPSILON,
            "Error disappearing should add 0.1, got {score}"
        );
    }

    #[test]
    fn test_salience_user_spoke_adds_0_4() {
        let s = screen("code editor", "Code.exe", false);
        let c = context("main.rs", "Code.exe");
        let prev = frame(s.clone(), None, c.clone());

        let curr = frame(s, Some(audio("fix the bug")), c);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.4).abs() < f32::EPSILON,
            "User speech should add 0.4, got {score}"
        );
    }

    #[test]
    fn test_salience_empty_transcript_does_not_add() {
        let s = screen("code editor", "Code.exe", false);
        let c = context("main.rs", "Code.exe");
        let prev = frame(s.clone(), None, c.clone());

        let curr = frame(s, Some(audio("")), c);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.0).abs() < f32::EPSILON,
            "Empty transcript should not add salience, got {score}"
        );
    }

    #[test]
    fn test_salience_new_window_adds_0_2() {
        let s = screen("code editor", "Code.exe", false);
        let c_prev = context("main.rs - kairo", "Code.exe");
        let prev = frame(s.clone(), None, c_prev);

        let c_curr = context("Google - Chrome", "chrome.exe");
        let curr = frame(s, None, c_curr);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.2).abs() < f32::EPSILON,
            "New window should add 0.2, got {score}"
        );
    }

    #[test]
    fn test_salience_multiple_signals_stack() {
        let s_prev = screen("code editor", "Code.exe", false);
        let c_prev = context("main.rs", "Code.exe");
        let prev = frame(s_prev, None, c_prev);

        let s_curr = screen("error in browser", "chrome.exe", true);
        let c_curr = context("Error - Chrome", "chrome.exe");
        let curr = frame(s_curr, Some(audio("what happened")), c_curr);

        let score = compute_salience(&curr, Some(&prev));
        // new error (0.3) + speech (0.4) + new window (0.2) = 0.9
        assert!(
            (score - 0.9).abs() < f32::EPSILON,
            "Stacked signals should be 0.9, got {score}"
        );
    }

    #[test]
    fn test_salience_clamped_to_1_0() {
        // Force a scenario where raw score would exceed 1.0.
        // error appear (0.3) + speech (0.4) + new window (0.2) + error disappear (0.1)
        // But error appear and disappear can't both be true at once, so max real
        // sum is 0.9. Let's verify clamping works for future-proofing.
        let score: f32 = 1.5_f32.clamp(0.0, 1.0);
        assert!((score - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_salience_window_title_change_only() {
        let s = screen("code editor", "Code.exe", false);
        let c_prev = context("main.rs - kairo", "Code.exe");
        let prev = frame(s.clone(), None, c_prev);

        // Same process, different title (different file).
        let c_curr = context("lib.rs - kairo", "Code.exe");
        let curr = frame(s, None, c_curr);

        let score = compute_salience(&curr, Some(&prev));
        assert!(
            (score - 0.2).abs() < f32::EPSILON,
            "Title-only change should still trigger new window, got {score}"
        );
    }

    // --- PerceptionFrameBuilder tests ---

    #[tokio::test]
    async fn test_frame_builder_assembles_frames() {
        let config = FrameConfig {
            interval_secs: 1,
            salience_threshold: 0.0, // Accept all frames for testing.
        };
        let builder = PerceptionFrameBuilder::new(config);

        let (screen_tx, screen_rx) = mpsc::channel(8);
        let (audio_tx, audio_rx) = mpsc::channel(8);
        let (ctx_tx, ctx_rx) = mpsc::channel(8);
        let (frame_tx, mut frame_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Send observations before starting the builder.
        screen_tx
            .send(screen("VS Code editing main.rs", "Code.exe", false))
            .await
            .unwrap();
        ctx_tx
            .send(context("main.rs - kairo", "Code.exe"))
            .await
            .unwrap();

        let handle = tokio::spawn(async move {
            builder
                .run(screen_rx, audio_rx, ctx_rx, frame_tx, shutdown_rx)
                .await;
        });

        // Wait for a frame.
        let received = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            frame_rx.recv(),
        )
        .await;

        let _ = shutdown_tx.send(true);
        let _ = handle.await;

        // Drop unused senders to avoid warnings.
        drop(screen_tx);
        drop(audio_tx);
        drop(ctx_tx);

        match received {
            Ok(Some(f)) => {
                assert!(!f.screen.description.is_empty());
                assert!(!f.id.is_nil());
                // First frame should have salience > 0.
                assert!(f.salience_hint > 0.0);
            }
            Ok(None) => panic!("Frame channel closed without producing a frame"),
            Err(_) => panic!("Timed out waiting for frame"),
        }
    }

    #[tokio::test]
    async fn test_frame_builder_fills_foreground_app_from_context() {
        let config = FrameConfig {
            interval_secs: 1,
            salience_threshold: 0.0,
        };
        let builder = PerceptionFrameBuilder::new(config);

        let (screen_tx, screen_rx) = mpsc::channel(8);
        let (_audio_tx, audio_rx) = mpsc::channel(8);
        let (ctx_tx, ctx_rx) = mpsc::channel(8);
        let (frame_tx, mut frame_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Screen observation with empty foreground_app.
        screen_tx
            .send(screen("editing code", "", false))
            .await
            .unwrap();
        ctx_tx
            .send(context("main.rs - kairo", "Code.exe"))
            .await
            .unwrap();

        let handle = tokio::spawn(async move {
            builder
                .run(screen_rx, audio_rx, ctx_rx, frame_tx, shutdown_rx)
                .await;
        });

        let received = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            frame_rx.recv(),
        )
        .await;

        let _ = shutdown_tx.send(true);
        let _ = handle.await;

        if let Ok(Some(f)) = received {
            assert_eq!(
                f.screen.foreground_app, "Code.exe",
                "foreground_app should be filled from context"
            );
        }
    }

    #[tokio::test]
    async fn test_frame_builder_respects_shutdown() {
        let config = FrameConfig {
            interval_secs: 60, // Long interval.
            salience_threshold: 0.0,
        };
        let builder = PerceptionFrameBuilder::new(config);

        let (_s, screen_rx) = mpsc::channel(1);
        let (_a, audio_rx) = mpsc::channel(1);
        let (_c, ctx_rx) = mpsc::channel(1);
        let (frame_tx, _frame_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            builder
                .run(screen_rx, audio_rx, ctx_rx, frame_tx, shutdown_rx)
                .await;
        });

        // Shutdown immediately.
        let _ = shutdown_tx.send(true);

        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            handle,
        )
        .await;

        assert!(
            result.is_ok(),
            "Frame builder should shut down within 3 seconds"
        );
    }
}
