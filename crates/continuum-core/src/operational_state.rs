//! # Public-safe operational state and diagnostics
//!
//! These types are the stable contract between the headless runtime, desktop
//! diagnostics and end-to-end tests. They intentionally avoid private paths,
//! process command lines and raw errors. Detailed local logs may retain richer
//! evidence behind the existing privacy boundary.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Unambiguous lifecycle state for a runtime component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OperationalState {
    Starting,
    Running,
    Idle,
    Degraded,
    DisabledByUser,
    DisabledByPolicy,
    PermissionRequired,
    #[default]
    Unavailable,
    Stopping,
    Failed,
}

impl OperationalState {
    /// Whether the component is actively enabled rather than intentionally off.
    pub const fn enabled(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Idle | Self::Degraded | Self::Stopping
        )
    }

    /// Whether the state represents a genuine fault requiring attention.
    pub const fn faulted(self) -> bool {
        matches!(
            self,
            Self::Degraded | Self::PermissionRequired | Self::Unavailable | Self::Failed
        )
    }
}

/// Broad, non-sensitive root-cause category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RootCauseCategory {
    UserChoice,
    Configuration,
    Policy,
    Permission,
    Dependency,
    Resource,
    Data,
    Internal,
    #[default]
    Unknown,
}

/// Permission class for a proposed repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepairPolicyClass {
    AutomaticallySafe,
    RequiresUserApproval,
    DeniedDestructive,
    ManualOnly,
    #[default]
    Unavailable,
}

/// Public description of a repair option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepairDescriptor {
    pub class: RepairPolicyClass,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub available: bool,
}

/// One public-safe evidence pointer. `reference` names a metric or state key,
/// never a user path or raw log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceReference {
    pub kind: String,
    pub reference: String,
    pub observed_at: DateTime<Utc>,
}

/// Structured component diagnosis carried alongside legacy health fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentDiagnostic {
    pub component: String,
    pub capability_affected: String,
    pub state: OperationalState,
    pub reason_code: String,
    pub explanation: String,
    pub root_cause: RootCauseCategory,
    #[serde(default)]
    pub evidence: Vec<EvidenceReference>,
    pub observed_at: DateTime<Utc>,
    pub retryable: bool,
    #[serde(default)]
    pub recommended_action: Option<String>,
    #[serde(default)]
    pub repair: RepairDescriptor,
}

impl Default for ComponentDiagnostic {
    fn default() -> Self {
        Self {
            component: "unknown".to_string(),
            capability_affected: "unknown".to_string(),
            state: OperationalState::Unavailable,
            reason_code: "diagnostic_unavailable".to_string(),
            explanation: "No structured diagnostic was published.".to_string(),
            root_cause: RootCauseCategory::Unknown,
            evidence: Vec::new(),
            observed_at: Utc::now(),
            retryable: false,
            recommended_action: None,
            repair: RepairDescriptor::default(),
        }
    }
}

impl ComponentDiagnostic {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        component: impl Into<String>,
        capability_affected: impl Into<String>,
        state: OperationalState,
        reason_code: impl Into<String>,
        explanation: impl Into<String>,
        root_cause: RootCauseCategory,
        retryable: bool,
    ) -> Self {
        Self {
            component: component.into(),
            capability_affected: capability_affected.into(),
            state,
            reason_code: reason_code.into(),
            explanation: explanation.into(),
            root_cause,
            evidence: Vec::new(),
            observed_at: Utc::now(),
            retryable,
            recommended_action: None,
            repair: RepairDescriptor::default(),
        }
    }

    pub fn with_evidence(mut self, kind: &str, reference: &str) -> Self {
        self.evidence.push(EvidenceReference {
            kind: kind.to_string(),
            reference: reference.to_string(),
            observed_at: self.observed_at,
        });
        self
    }

    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.recommended_action = Some(action.into());
        self
    }

    pub fn with_repair(
        mut self,
        class: RepairPolicyClass,
        available: bool,
        action: Option<&str>,
    ) -> Self {
        self.repair = RepairDescriptor {
            class,
            available,
            action: action.map(ToOwned::to_owned),
        };
        self
    }
}

/// Structured runtime/repair event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationalEventKind {
    HealthTransition,
    WatcherStateTransition,
    RepairStarted,
    RepairCompleted,
    RepairFailed,
    VerificationResult,
}

/// Bounded public-safe event suitable for the desktop diagnostics timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalEvent {
    pub sequence: u64,
    pub kind: OperationalEventKind,
    pub component: String,
    #[serde(default)]
    pub from: Option<OperationalState>,
    pub to: OperationalState,
    pub reason_code: String,
    pub explanation: String,
    pub ts: DateTime<Utc>,
}

/// Deduplicates health snapshots into bounded transition events. Repeated
/// healthy polls produce no event, preventing noisy infinite loops.
#[derive(Debug, Clone)]
pub struct OperationalEventBuffer {
    inner: Arc<Mutex<OperationalEventInner>>,
}

#[derive(Debug)]
struct OperationalEventInner {
    capacity: usize,
    sequence: u64,
    last: HashMap<String, (OperationalState, String)>,
    events: VecDeque<OperationalEvent>,
}

impl Default for OperationalEventBuffer {
    fn default() -> Self {
        Self::new(64)
    }
}

impl OperationalEventBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OperationalEventInner {
                capacity: capacity.max(1),
                sequence: 0,
                last: HashMap::new(),
                events: VecDeque::new(),
            })),
        }
    }

    /// Observes one diagnosis. Returns the emitted transition, if any.
    pub fn observe(
        &self,
        kind: OperationalEventKind,
        diagnostic: &ComponentDiagnostic,
    ) -> Option<OperationalEvent> {
        let mut inner = self.inner.lock();
        let key = diagnostic.component.clone();
        let current = (diagnostic.state, diagnostic.reason_code.clone());
        let previous = inner.last.insert(key.clone(), current.clone());
        if previous.as_ref() == Some(&current) {
            return None;
        }
        Some(push_event(
            &mut inner,
            kind,
            key,
            previous.map(|(state, _)| state),
            diagnostic.state,
            diagnostic.reason_code.clone(),
            diagnostic.explanation.clone(),
            diagnostic.observed_at,
        ))
    }

    /// Records a bounded one-shot event such as a repair attempt or
    /// verification result. Unlike [`Self::observe`], these events are not
    /// deduplicated because every authorized attempt must remain visible.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        kind: OperationalEventKind,
        component: impl Into<String>,
        from: Option<OperationalState>,
        to: OperationalState,
        reason_code: impl Into<String>,
        explanation: impl Into<String>,
    ) -> OperationalEvent {
        let mut inner = self.inner.lock();
        push_event(
            &mut inner,
            kind,
            component.into(),
            from,
            to,
            reason_code.into(),
            explanation.into(),
            Utc::now(),
        )
    }

    pub fn recent(&self) -> Vec<OperationalEvent> {
        self.inner.lock().events.iter().cloned().collect()
    }
}

fn push_event(
    inner: &mut OperationalEventInner,
    kind: OperationalEventKind,
    component: String,
    from: Option<OperationalState>,
    to: OperationalState,
    reason_code: String,
    explanation: String,
    ts: DateTime<Utc>,
) -> OperationalEvent {
    inner.sequence = inner.sequence.saturating_add(1);
    let event = OperationalEvent {
        sequence: inner.sequence,
        kind,
        component,
        from,
        to,
        reason_code,
        explanation,
        ts,
    };
    inner.events.push_back(event.clone());
    while inner.events.len() > inner.capacity {
        inner.events.pop_front();
    }
    event
}

/// Removes obvious path-like fragments from a public diagnostic string. Raw
/// errors remain available in local structured logs; UI/state surfaces get a
/// stable explanation instead.
pub fn public_safe_message(message: &str, fallback: &str) -> String {
    let looks_private = message.contains("\\\\")
        || message.contains(":\\")
        || message.contains("/home/")
        || message.contains("/Users/")
        || message.contains("/.continuum")
        || message.contains("file://");
    if looks_private {
        fallback.to_string()
    } else {
        message.chars().take(240).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(state: OperationalState, reason: &str) -> ComponentDiagnostic {
        ComponentDiagnostic::new(
            "file_watcher",
            "file_activity",
            state,
            reason,
            "synthetic explanation",
            RootCauseCategory::Unknown,
            true,
        )
    }

    #[test]
    fn idle_is_enabled_but_not_disabled() {
        assert!(OperationalState::Idle.enabled());
        assert!(!OperationalState::DisabledByUser.enabled());
    }

    #[test]
    fn transition_buffer_deduplicates_and_is_bounded() {
        let buffer = OperationalEventBuffer::new(2);
        assert!(buffer
            .observe(
                OperationalEventKind::WatcherStateTransition,
                &diagnostic(OperationalState::Idle, "idle")
            )
            .is_some());
        assert!(buffer
            .observe(
                OperationalEventKind::WatcherStateTransition,
                &diagnostic(OperationalState::Idle, "idle")
            )
            .is_none());
        buffer.observe(
            OperationalEventKind::WatcherStateTransition,
            &diagnostic(OperationalState::Running, "watching"),
        );
        buffer.observe(
            OperationalEventKind::WatcherStateTransition,
            &diagnostic(OperationalState::Degraded, "root_unavailable"),
        );
        let events = buffer.recent();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].to, OperationalState::Running);
        assert_eq!(events[1].to, OperationalState::Degraded);
    }

    #[test]
    fn one_shot_repair_events_are_not_deduplicated() {
        let buffer = OperationalEventBuffer::new(4);
        buffer.record(
            OperationalEventKind::RepairStarted,
            "file_watcher",
            Some(OperationalState::Failed),
            OperationalState::Starting,
            "restart_requested",
            "Synthetic restart requested.",
        );
        buffer.record(
            OperationalEventKind::RepairStarted,
            "file_watcher",
            Some(OperationalState::Failed),
            OperationalState::Starting,
            "restart_requested",
            "Synthetic restart requested.",
        );
        assert_eq!(buffer.recent().len(), 2);
    }

    #[test]
    fn private_paths_are_redacted_from_public_messages() {
        assert_eq!(
            public_safe_message(
                r#"failed to open C:\\Users\\person\\private\\state.json"#,
                "state publication failed"
            ),
            "state publication failed"
        );
        assert_eq!(
            public_safe_message("permission denied", "fallback"),
            "permission denied"
        );
    }
}
