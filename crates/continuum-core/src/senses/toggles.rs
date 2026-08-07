//! # Live observation toggles (spec §4.1 honest toggles, §4.13 set_toggle)
//!
//! Task A2 made the per-source toggles *spawn-time* reads: each watcher
//! captured an [`ObservationToggles`] copy and honoured it once, at start.
//! That is correct but not live — flipping "microphone off" on the Context
//! page had to wait for a restart, which is precisely the wrong latency for
//! a privacy control.
//!
//! [`ToggleControl`] fixes that by following the **sanctioned shared-atomics
//! pattern** established by [`crate::senses::cadence::CadenceControl`]
//! (spec §3): one `Arc` of `AtomicBool`s that the intent handler stores into
//! and every long-running loop loads *each iteration*. No config hot-reload
//! machinery, no restart, no locks.
//!
//! ## What "live" means per source
//!
//! | source | live behaviour when switched off |
//! |---|---|
//! | `screen` | the capture scheduler stops taking bitmaps and the vision consumer drops anything still in flight |
//! | `git` | the poll cycle stops spawning `git` subprocesses |
//! | `files` | the notify watches are dropped and buffered events are discarded |
//! | `mic` | transcripts are discarded before they reach the frame builder |
//! | `pause_all` | the frame loop builds and persists nothing, and every individual source gates off |
//!
//! **Documented limitation:** flipping `mic` off closes the *data path*, not
//! the audio device — cpal's input stream is opened once at watcher start.
//! Nothing is captured into a frame, persisted or sent anywhere, but the OS
//! "microphone in use" indicator stays lit until the runtime restarts. Turning
//! `screen` off *does* stop the capture itself. This asymmetry is stated on
//! the Context page rather than papered over (spec §4.1, "no dishonest mute").
//!
//! # Layer
//!
//! Layer 1 — Senses (control plane). Pure atomics, ungated.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::config::ObservationToggles;
use crate::context::intents::ToggleName;
use crate::senses::privacy::ObservedSource;

/// Shared, cheap-to-clone handle onto the live toggle values.
///
/// Producers (the Context-page `set_toggle` intent handler) store;
/// consumers (every watcher loop) load on each pass.
#[derive(Debug, Clone)]
pub struct ToggleControl {
    inner: Arc<ToggleInner>,
}

#[derive(Debug)]
struct ToggleInner {
    mic: AtomicBool,
    screen: AtomicBool,
    files: AtomicBool,
    git: AtomicBool,
    pause_all: AtomicBool,
    /// Bumped on every accepted change. Loops that want to log a
    /// transition (rather than re-log every iteration) remember the last
    /// value they saw — the [`crate::senses::cadence::CadenceControl`]
    /// nudge-sequence trick.
    version: AtomicU64,
}

impl Default for ToggleControl {
    fn default() -> Self {
        Self::new(&ObservationToggles::default())
    }
}

impl ToggleControl {
    /// Seeds a control from the boot configuration.
    pub fn new(toggles: &ObservationToggles) -> Self {
        Self {
            inner: Arc::new(ToggleInner {
                mic: AtomicBool::new(toggles.mic),
                screen: AtomicBool::new(toggles.screen),
                files: AtomicBool::new(toggles.files),
                git: AtomicBool::new(toggles.git),
                pause_all: AtomicBool::new(toggles.pause_all),
                version: AtomicU64::new(0),
            }),
        }
    }

    /// Current values as a plain struct — the shape
    /// [`crate::senses::privacy::source_enabled`] already speaks.
    pub fn snapshot(&self) -> ObservationToggles {
        ObservationToggles {
            mic: self.inner.mic.load(Ordering::Acquire),
            screen: self.inner.screen.load(Ordering::Acquire),
            files: self.inner.files.load(Ordering::Acquire),
            git: self.inner.git.load(Ordering::Acquire),
            pause_all: self.inner.pause_all.load(Ordering::Acquire),
        }
    }

    /// Whether a source may observe right now (`pause_all` wins).
    pub fn enabled(&self, source: ObservedSource) -> bool {
        crate::senses::privacy::source_enabled(&self.snapshot(), source)
    }

    /// Whether the master pause is engaged.
    pub fn paused(&self) -> bool {
        self.inner.pause_all.load(Ordering::Acquire)
    }

    /// Monotonic change counter.
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }

    /// Applies one toggle. Returns `true` when the value actually changed
    /// (so the caller only emits a `toggle_change` event and an audit line
    /// for real transitions).
    pub fn set(&self, name: ToggleName, value: bool) -> bool {
        let cell = match name {
            ToggleName::Mic => &self.inner.mic,
            ToggleName::Screen => &self.inner.screen,
            ToggleName::Files => &self.inner.files,
            ToggleName::Git => &self.inner.git,
            ToggleName::PauseAll => &self.inner.pause_all,
        };
        let previous = cell.swap(value, Ordering::AcqRel);
        if previous != value {
            self.inner.version.fetch_add(1, Ordering::AcqRel);
            true
        } else {
            false
        }
    }

    /// Reads one toggle's own value, ignoring `pause_all`. The Context
    /// page renders these (the switches must show what the user set, not
    /// what the master pause is masking).
    pub fn value(&self, name: ToggleName) -> bool {
        let s = self.snapshot();
        match name {
            ToggleName::Mic => s.mic,
            ToggleName::Screen => s.screen,
            ToggleName::Files => s.files,
            ToggleName::Git => s.git,
            ToggleName::PauseAll => s.pause_all,
        }
    }
}

impl From<&ObservationToggles> for ToggleControl {
    fn from(value: &ObservationToggles) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_from_config_and_snapshots_back() {
        let cfg = ObservationToggles {
            mic: false,
            screen: true,
            files: false,
            git: true,
            pause_all: false,
        };
        let control = ToggleControl::new(&cfg);
        assert_eq!(control.snapshot(), cfg);
    }

    #[test]
    fn set_reports_only_real_transitions_and_bumps_the_version() {
        let control = ToggleControl::default();
        assert_eq!(control.version(), 0);
        assert!(control.set(ToggleName::Mic, false));
        assert_eq!(control.version(), 1);
        // Same value again: not a change.
        assert!(!control.set(ToggleName::Mic, false));
        assert_eq!(control.version(), 1);
        assert!(control.set(ToggleName::Mic, true));
        assert_eq!(control.version(), 2);
    }

    #[test]
    fn pause_all_gates_every_source_but_leaves_their_own_values_alone() {
        let control = ToggleControl::default();
        assert!(control.enabled(ObservedSource::Mic));
        assert!(control.enabled(ObservedSource::Window));
        control.set(ToggleName::PauseAll, true);
        assert!(control.paused());
        for source in [
            ObservedSource::Mic,
            ObservedSource::Screen,
            ObservedSource::Files,
            ObservedSource::Git,
            ObservedSource::Window,
        ] {
            assert!(!control.enabled(source), "{source:?} must gate off");
        }
        // The switches themselves still read "on" — the Context page shows
        // what the user set, plus a paused banner.
        assert!(control.value(ToggleName::Mic));
        assert!(control.value(ToggleName::Screen));
    }

    #[test]
    fn clones_share_one_state() {
        let a = ToggleControl::default();
        let b = a.clone();
        a.set(ToggleName::Git, false);
        assert!(!b.value(ToggleName::Git));
        assert_eq!(b.version(), 1);
    }

    #[test]
    fn every_toggle_name_maps_to_its_own_cell() {
        let control = ToggleControl::default();
        for name in [
            ToggleName::Mic,
            ToggleName::Screen,
            ToggleName::Files,
            ToggleName::Git,
            ToggleName::PauseAll,
        ] {
            let before = control.value(name);
            control.set(name, !before);
            assert_eq!(control.value(name), !before, "{name:?} did not flip");
            control.set(name, before);
        }
    }

    #[test]
    fn is_shared_across_threads() {
        let control = ToggleControl::default();
        let writer = control.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..1000 {
                writer.set(ToggleName::Screen, false);
                writer.set(ToggleName::Screen, true);
            }
        });
        for _ in 0..1000 {
            let _ = control.snapshot();
        }
        handle.join().unwrap();
        assert!(control.value(ToggleName::Screen));
    }
}
