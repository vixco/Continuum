//! # Runtime-adjustable cadences + idle state machine
//!
//! **This module is the sanctioned pattern for runtime-adjustable cadences**
//! (context engine spec §3): shared `AtomicU64` values that long-running
//! loops read *each iteration* instead of capturing at spawn time. No
//! config hot-reload machinery, no restart — a controller stores a new
//! value, the loop picks it up on its next pass. Anything beyond simple
//! numeric cadences (model paths, toggle sets, …) stays NEXT-tier work
//! (spec §11) and must not imitate this pattern.
//!
//! Two pieces live here:
//!
//! - [`CadenceControl`] — the shared atomics: per-monitor capture interval
//!   and the per-monitor vision minimum interval, plus the wake-nudge
//!   sequence the capture/vision loops use to force one immediate pass
//!   after an idle exit.
//! - [`IdleController`] — the mechanical idle state machine (spec §4.11):
//!   enter when `idle_seconds` exceeds `[performance].idle_pause_after_secs`,
//!   restore on input activity, voice wake, hotkey, or any orchestrator
//!   wake. Pure logic; the runtime binary maps its transitions onto
//!   `idle_start`/`idle_end` system events and the live-context hub.
//!
//! # Layer
//!
//! Layer 1 — Senses (control plane). No AI involvement.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::config::PerformanceConfig;

/// Shared, cheap-to-clone cadence handle (the spec §3 sanctioned
/// `AtomicU64` pattern). Producers of new cadences (the idle controller)
/// store; consumers (per-monitor capture OS-threads, the vision
/// consumer) load every iteration.
#[derive(Debug, Clone)]
pub struct CadenceControl {
    inner: Arc<CadenceInner>,
}

#[derive(Debug)]
struct CadenceInner {
    /// Current per-monitor capture interval (ms). Capture loops clamp to
    /// a 20 ms floor themselves.
    capture_interval_ms: AtomicU64,
    /// Current minimum delay between local vision calls per monitor
    /// (ms). `0` means vision is fully paused (spec §4.11).
    vision_min_interval_ms: AtomicU64,
    /// Boot-time (normal) values restored on idle exit.
    normal_capture_interval_ms: u64,
    normal_vision_min_interval_ms: u64,
    /// Whether the idle controller currently has idle cadences applied.
    idle: AtomicBool,
    /// Wake-nudge sequence (spec §4.11 wake-during-pause). Bumped once
    /// per idle exit; every capture worker and vision cache remembers the
    /// last value it saw and forces one immediate meaningful
    /// capture+vision pass when it changes. A sequence (not a flag) so
    /// each per-monitor loop observes the nudge exactly once without
    /// racing the others for a single one-shot bool.
    nudge_seq: AtomicU64,
}

impl CadenceControl {
    /// Creates a control seeded with the normal (non-idle) cadences from
    /// `[screen]` config.
    pub fn new(capture_interval_ms: u64, vision_min_interval_ms: u64) -> Self {
        Self {
            inner: Arc::new(CadenceInner {
                capture_interval_ms: AtomicU64::new(capture_interval_ms),
                vision_min_interval_ms: AtomicU64::new(vision_min_interval_ms),
                normal_capture_interval_ms: capture_interval_ms,
                normal_vision_min_interval_ms: vision_min_interval_ms,
                idle: AtomicBool::new(false),
                nudge_seq: AtomicU64::new(0),
            }),
        }
    }

    /// Current per-monitor capture interval in milliseconds.
    pub fn capture_interval_ms(&self) -> u64 {
        self.inner.capture_interval_ms.load(Ordering::Acquire)
    }

    /// Current per-monitor vision minimum interval in milliseconds.
    /// `0` = vision fully paused (spec §4.11).
    pub fn vision_min_interval_ms(&self) -> u64 {
        self.inner.vision_min_interval_ms.load(Ordering::Acquire)
    }

    /// Whether idle cadences are currently applied.
    pub fn is_idle(&self) -> bool {
        self.inner.idle.load(Ordering::Acquire)
    }

    /// Current wake-nudge sequence. Loops that remember the last value
    /// they saw force one immediate pass when it changes.
    pub fn nudge_seq(&self) -> u64 {
        self.inner.nudge_seq.load(Ordering::Acquire)
    }

    /// Applies the `[performance]` idle cadences.
    pub fn enter_idle(&self, idle_capture_interval_ms: u64, idle_vision_interval_ms: u64) {
        self.inner
            .capture_interval_ms
            .store(idle_capture_interval_ms, Ordering::Release);
        self.inner
            .vision_min_interval_ms
            .store(idle_vision_interval_ms, Ordering::Release);
        self.inner.idle.store(true, Ordering::Release);
    }

    /// Restores the normal cadences and bumps the wake-nudge sequence so
    /// every capture worker takes one immediate meaningful pass (the
    /// spec §4.11 wake-during-pause guarantee).
    pub fn exit_idle(&self) {
        self.inner
            .capture_interval_ms
            .store(self.inner.normal_capture_interval_ms, Ordering::Release);
        self.inner
            .vision_min_interval_ms
            .store(self.inner.normal_vision_min_interval_ms, Ordering::Release);
        self.inner.idle.store(false, Ordering::Release);
        self.inner.nudge_seq.fetch_add(1, Ordering::AcqRel);
    }
}

/// A transition reported by [`IdleController`]. The runtime binary maps
/// these onto `idle_start` / `idle_end` system events and
/// `LiveContextHub::set_idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleTransition {
    /// Idle threshold crossed; idle cadences are now applied.
    EnteredIdle,
    /// A restore trigger fired; normal cadences are back and the wake
    /// nudge has been issued.
    ExitedIdle,
}

/// Mechanical idle state machine (spec §4.11): no model output feeds it,
/// only `idle_seconds` from the context watcher and the explicit wake
/// triggers. Owns no clock — callers feed it observations.
///
/// Session-inference pause is Plan B work: when the session-state
/// inference task lands, it reads [`CadenceControl::is_idle`] (or an
/// equivalent flag published from this controller) and skips its LLM
/// calls while idle. Seam only — nothing to wire here yet.
#[derive(Debug)]
pub struct IdleController {
    pause_after_secs: u64,
    idle_capture_interval_ms: u64,
    idle_vision_interval_ms: u64,
    idle: bool,
}

impl IdleController {
    /// Creates a controller from the `[performance]` knobs.
    pub fn new(performance: &PerformanceConfig) -> Self {
        Self {
            pause_after_secs: performance.idle_pause_after_secs,
            idle_capture_interval_ms: performance.idle_capture_interval_ms,
            idle_vision_interval_ms: performance.idle_vision_interval_ms,
            idle: false,
        }
    }

    /// Whether the controller is currently in idle mode.
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// Feeds one frame's `idle_seconds`. Crossing the threshold enters
    /// idle; a drop back under it (input activity) exits. A
    /// `pause_after_secs` of `0` disables idle mode entirely.
    pub fn on_frame(
        &mut self,
        idle_seconds: u64,
        cadence: &CadenceControl,
    ) -> Option<IdleTransition> {
        if self.pause_after_secs == 0 {
            return None;
        }
        if !self.idle && idle_seconds > self.pause_after_secs {
            self.idle = true;
            cadence.enter_idle(self.idle_capture_interval_ms, self.idle_vision_interval_ms);
            return Some(IdleTransition::EnteredIdle);
        }
        if self.idle && idle_seconds <= self.pause_after_secs {
            self.idle = false;
            cadence.exit_idle();
            return Some(IdleTransition::ExitedIdle);
        }
        None
    }

    /// An explicit wake trigger fired: voice wake, hotkey / push-to-talk,
    /// or any orchestrator wake entry (spec §4.11). Exits idle if idle;
    /// note the state machine may legitimately re-enter on the next
    /// frame when the user is still physically away (`idle_seconds`
    /// stays above the threshold) — the exit's wake nudge has already
    /// forced the fresh capture+vision pass the wake needed.
    pub fn on_wake(&mut self, cadence: &CadenceControl) -> Option<IdleTransition> {
        if !self.idle {
            return None;
        }
        self.idle = false;
        cadence.exit_idle();
        Some(IdleTransition::ExitedIdle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perf(pause_after: u64) -> PerformanceConfig {
        PerformanceConfig {
            idle_pause_after_secs: pause_after,
            idle_capture_interval_ms: 2_000,
            idle_vision_interval_ms: 15_000,
        }
    }

    #[test]
    fn cadence_control_reads_and_writes_shared_values() {
        let cadence = CadenceControl::new(200, 2_000);
        let clone = cadence.clone();
        assert_eq!(clone.capture_interval_ms(), 200);
        assert_eq!(clone.vision_min_interval_ms(), 2_000);
        assert!(!clone.is_idle());

        cadence.enter_idle(2_000, 15_000);
        // The clone shares the same atomics — the whole point of the
        // sanctioned pattern.
        assert_eq!(clone.capture_interval_ms(), 2_000);
        assert_eq!(clone.vision_min_interval_ms(), 15_000);
        assert!(clone.is_idle());

        cadence.exit_idle();
        assert_eq!(clone.capture_interval_ms(), 200);
        assert_eq!(clone.vision_min_interval_ms(), 2_000);
        assert!(!clone.is_idle());
    }

    #[test]
    fn exit_idle_bumps_the_wake_nudge_exactly_once() {
        let cadence = CadenceControl::new(200, 2_000);
        let seen = cadence.nudge_seq();
        cadence.enter_idle(2_000, 0);
        assert_eq!(cadence.nudge_seq(), seen, "entering idle never nudges");
        cadence.exit_idle();
        assert_eq!(cadence.nudge_seq(), seen + 1);
        cadence.exit_idle();
        assert_eq!(
            cadence.nudge_seq(),
            seen + 2,
            "each exit is one nudge; loops diff against their last-seen value"
        );
    }

    #[test]
    fn idle_enters_on_threshold_and_exits_on_input_activity() {
        let cadence = CadenceControl::new(200, 2_000);
        let mut idle = IdleController::new(&perf(300));

        assert_eq!(idle.on_frame(0, &cadence), None);
        assert_eq!(idle.on_frame(300, &cadence), None, "threshold is exclusive");
        assert_eq!(
            idle.on_frame(301, &cadence),
            Some(IdleTransition::EnteredIdle)
        );
        assert!(idle.is_idle());
        assert_eq!(cadence.capture_interval_ms(), 2_000);
        assert_eq!(cadence.vision_min_interval_ms(), 15_000);
        // Staying idle produces no repeat transition.
        assert_eq!(idle.on_frame(500, &cadence), None);

        // Input activity: idle_seconds drops → restore.
        assert_eq!(idle.on_frame(1, &cadence), Some(IdleTransition::ExitedIdle));
        assert!(!idle.is_idle());
        assert_eq!(cadence.capture_interval_ms(), 200);
        assert_eq!(cadence.vision_min_interval_ms(), 2_000);
    }

    #[test]
    fn voice_wake_restores_cadences_and_nudges() {
        // Spec §4.11 restore triggers: voice wake / hotkey / do_wake all
        // route through on_wake.
        let cadence = CadenceControl::new(200, 2_000);
        let mut idle = IdleController::new(&perf(300));
        idle.on_frame(400, &cadence);
        assert!(cadence.is_idle());

        let nudge_before = cadence.nudge_seq();
        assert_eq!(idle.on_wake(&cadence), Some(IdleTransition::ExitedIdle));
        assert!(!idle.is_idle());
        assert_eq!(cadence.capture_interval_ms(), 200);
        assert_eq!(cadence.vision_min_interval_ms(), 2_000);
        assert_eq!(
            cadence.nudge_seq(),
            nudge_before + 1,
            "wake-during-pause forces one immediate capture+vision pass"
        );

        // A wake while already active is a no-op.
        assert_eq!(idle.on_wake(&cadence), None);
        assert_eq!(cadence.nudge_seq(), nudge_before + 1);
    }

    #[test]
    fn wake_during_pause_may_reenter_idle_on_the_next_frame() {
        // The user is physically away: a maintenance wake exits idle, the
        // next frame's idle_seconds is still above threshold → re-enter.
        // Documented behavior, not a bug (see on_wake's doc comment).
        let cadence = CadenceControl::new(200, 2_000);
        let mut idle = IdleController::new(&perf(300));
        idle.on_frame(400, &cadence);
        assert_eq!(idle.on_wake(&cadence), Some(IdleTransition::ExitedIdle));
        assert_eq!(
            idle.on_frame(400, &cadence),
            Some(IdleTransition::EnteredIdle)
        );
        assert!(cadence.is_idle());
    }

    #[test]
    fn zero_pause_after_disables_idle_mode() {
        let cadence = CadenceControl::new(200, 2_000);
        let mut idle = IdleController::new(&perf(0));
        assert_eq!(idle.on_frame(u64::MAX, &cadence), None);
        assert!(!idle.is_idle());
        assert_eq!(cadence.capture_interval_ms(), 200);
    }

    #[test]
    fn zero_vision_interval_pauses_vision_while_idle() {
        let cadence = CadenceControl::new(200, 2_000);
        let mut idle = IdleController::new(&PerformanceConfig {
            idle_pause_after_secs: 300,
            idle_capture_interval_ms: 2_000,
            idle_vision_interval_ms: 0,
        });
        idle.on_frame(301, &cadence);
        assert_eq!(
            cadence.vision_min_interval_ms(),
            0,
            "0 = vision fully paused (spec §4.11)"
        );
        idle.on_wake(&cadence);
        assert_eq!(cadence.vision_min_interval_ms(), 2_000);
    }
}
