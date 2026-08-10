//! # Live runtime-service control
//!
//! Observation privacy toggles answer whether a source may be observed. This
//! module answers a different question: whether an optional runtime service is
//! requested to run at all. Keeping the two controls separate prevents a UI
//! switch from accidentally weakening a privacy policy.
//!
//! The control is process-local and lock-free. Durable changes travel through
//! `ContextAction::SetRuntimeService`, which applies the atomic change and then
//! surgically persists the matching config key.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Optional runtime services that can start and stop without restarting the
/// Continuum process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeServiceName {
    /// Project-scoped filesystem activity observation.
    FileActivity,
    /// Bounded background-process lifecycle/resource observation.
    BackgroundActivity,
    /// Interactive per-frame triage evaluation. The loaded model may still be
    /// reused by other local maintenance tasks when this is off.
    TriageEvaluation,
}

impl RuntimeServiceName {
    /// Stable public token used by intents, config writers and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileActivity => "file_activity",
            Self::BackgroundActivity => "background_activity",
            Self::TriageEvaluation => "triage_evaluation",
        }
    }

    /// Parses a stable public token without guessing.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "file_activity" | "file_watcher" => Some(Self::FileActivity),
            "background_activity" | "process_watcher" => Some(Self::BackgroundActivity),
            "triage_evaluation" | "triage" => Some(Self::TriageEvaluation),
            _ => None,
        }
    }
}

/// Serializable snapshot published for the desktop settings surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeServiceSnapshot {
    pub file_activity: bool,
    pub background_activity: bool,
    pub triage_evaluation: bool,
    /// Monotonic counter bumped only when an effective value changes.
    pub version: u64,
}

impl Default for RuntimeServiceSnapshot {
    fn default() -> Self {
        Self {
            file_activity: false,
            background_activity: false,
            triage_evaluation: true,
            version: 0,
        }
    }
}

/// Shared control plane for optional runtime services.
#[derive(Debug, Clone)]
pub struct RuntimeServiceControl {
    inner: Arc<RuntimeServiceInner>,
}

#[derive(Debug)]
struct RuntimeServiceInner {
    file_activity: AtomicBool,
    background_activity: AtomicBool,
    triage_evaluation: AtomicBool,
    version: AtomicU64,
    file_restart_generation: AtomicU64,
    process_restart_generation: AtomicU64,
    triage_restart_generation: AtomicU64,
}

impl Default for RuntimeServiceControl {
    fn default() -> Self {
        let defaults = RuntimeServiceSnapshot::default();
        Self::new(
            defaults.file_activity,
            defaults.background_activity,
            defaults.triage_evaluation,
        )
    }
}

impl RuntimeServiceControl {
    /// Seeds live service state from boot configuration.
    pub fn new(file_activity: bool, background_activity: bool, triage_evaluation: bool) -> Self {
        Self {
            inner: Arc::new(RuntimeServiceInner {
                file_activity: AtomicBool::new(file_activity),
                background_activity: AtomicBool::new(background_activity),
                triage_evaluation: AtomicBool::new(triage_evaluation),
                version: AtomicU64::new(0),
                file_restart_generation: AtomicU64::new(0),
                process_restart_generation: AtomicU64::new(0),
                triage_restart_generation: AtomicU64::new(0),
            }),
        }
    }

    /// Whether one service is requested to run right now.
    pub fn enabled(&self, service: RuntimeServiceName) -> bool {
        self.cell(service).load(Ordering::Acquire)
    }

    /// Applies one requested state. Returns `true` only for a real transition.
    pub fn set(&self, service: RuntimeServiceName, enabled: bool) -> bool {
        let previous = self.cell(service).swap(enabled, Ordering::AcqRel);
        if previous == enabled {
            return false;
        }
        // A state transition starts a new lifecycle generation. Consumers can
        // reject stale in-flight results from the previous generation.
        self.restart_cell(service).fetch_add(1, Ordering::AcqRel);
        self.inner.version.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Requests a bounded in-process restart. Consumers compare generations;
    /// no process is spawned and duplicate watcher instances cannot be created.
    pub fn request_restart(&self, service: RuntimeServiceName) -> u64 {
        let generation = self
            .restart_cell(service)
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        self.inner.version.fetch_add(1, Ordering::AcqRel);
        generation
    }

    /// Current restart generation for one service.
    pub fn restart_generation(&self, service: RuntimeServiceName) -> u64 {
        self.restart_cell(service).load(Ordering::Acquire)
    }

    /// Monotonic control-plane change counter.
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }

    /// Cheap serializable snapshot.
    pub fn snapshot(&self) -> RuntimeServiceSnapshot {
        RuntimeServiceSnapshot {
            file_activity: self.enabled(RuntimeServiceName::FileActivity),
            background_activity: self.enabled(RuntimeServiceName::BackgroundActivity),
            triage_evaluation: self.enabled(RuntimeServiceName::TriageEvaluation),
            version: self.version(),
        }
    }

    fn cell(&self, service: RuntimeServiceName) -> &AtomicBool {
        match service {
            RuntimeServiceName::FileActivity => &self.inner.file_activity,
            RuntimeServiceName::BackgroundActivity => &self.inner.background_activity,
            RuntimeServiceName::TriageEvaluation => &self.inner.triage_evaluation,
        }
    }

    fn restart_cell(&self, service: RuntimeServiceName) -> &AtomicU64 {
        match service {
            RuntimeServiceName::FileActivity => &self.inner.file_restart_generation,
            RuntimeServiceName::BackgroundActivity => &self.inner.process_restart_generation,
            RuntimeServiceName::TriageEvaluation => &self.inner.triage_restart_generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_and_triage_remains_available() {
        let control = RuntimeServiceControl::default();
        assert!(!control.enabled(RuntimeServiceName::FileActivity));
        assert!(!control.enabled(RuntimeServiceName::BackgroundActivity));
        assert!(control.enabled(RuntimeServiceName::TriageEvaluation));
    }

    #[test]
    fn duplicate_set_does_not_create_a_transition() {
        let control = RuntimeServiceControl::default();
        assert!(!control.set(RuntimeServiceName::FileActivity, false));
        assert_eq!(control.version(), 0);
        assert!(control.set(RuntimeServiceName::FileActivity, true));
        assert_eq!(control.version(), 1);
        assert!(!control.set(RuntimeServiceName::FileActivity, true));
        assert_eq!(control.version(), 1);
    }

    #[test]
    fn restart_requests_are_service_scoped_and_monotonic() {
        let control = RuntimeServiceControl::default();
        assert_eq!(control.request_restart(RuntimeServiceName::FileActivity), 1);
        assert_eq!(control.request_restart(RuntimeServiceName::FileActivity), 2);
        assert_eq!(
            control.restart_generation(RuntimeServiceName::BackgroundActivity),
            0
        );
    }

    #[test]
    fn clones_share_state() {
        let a = RuntimeServiceControl::default();
        let b = a.clone();
        a.set(RuntimeServiceName::BackgroundActivity, true);
        assert!(b.enabled(RuntimeServiceName::BackgroundActivity));
    }
}
