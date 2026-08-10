//! # Verified repair orchestration
//!
//! A command exit is evidence that an action ran, not evidence that the
//! affected capability recovered. This module enforces the full repair
//! sequence: inspect, authorize, execute, re-probe, compare, and report a
//! verified outcome. It is intentionally independent of any one component so
//! desktop health checks, runtime watchers and synthetic end-to-end tests can
//! share the same semantics.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::operational_state::{
    ComponentDiagnostic, OperationalState, RepairPolicyClass, RootCauseCategory,
};

/// Bounded exponential retry configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 250,
            max_delay_ms: 4_000,
        }
    }
}

/// Stateful bounded exponential backoff. Once attempts are exhausted it
/// returns `None` and the caller must wait for an explicit user/config change.
#[derive(Debug, Clone)]
pub struct RetryBackoff {
    policy: RetryPolicy,
    attempts: u32,
}

impl RetryBackoff {
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy: RetryPolicy {
                max_attempts: policy.max_attempts.max(1),
                base_delay_ms: policy.base_delay_ms.max(1),
                max_delay_ms: policy.max_delay_ms.max(policy.base_delay_ms.max(1)),
            },
            attempts: 0,
        }
    }

    /// Delay before the next attempt, or `None` once the bounded budget is
    /// exhausted. The first failed attempt waits `base_delay_ms`.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempts >= self.policy.max_attempts {
            return None;
        }
        let exponent = self.attempts.min(31);
        let multiplier = 1u64 << exponent;
        let delay = self
            .policy
            .base_delay_ms
            .saturating_mul(multiplier)
            .min(self.policy.max_delay_ms);
        self.attempts = self.attempts.saturating_add(1);
        Some(Duration::from_millis(delay))
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// One policy-checked repair plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairPlan {
    pub component: String,
    pub action: String,
    pub policy: RepairPolicyClass,
    /// Set only by an explicit user approval path. An agent cannot set this
    /// merely because it wants to perform the action.
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub retry: RetryPolicy,
    /// States that prove recovery for this action.
    #[serde(default)]
    pub success_states: Vec<OperationalState>,
}

impl RepairPlan {
    pub fn automatically_safe(component: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            action: action.into(),
            policy: RepairPolicyClass::AutomaticallySafe,
            approved: false,
            retry: RetryPolicy::default(),
            success_states: vec![OperationalState::Running, OperationalState::Idle],
        }
    }
}

/// Evidence returned by the action executor itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairExecution {
    pub exited_successfully: bool,
    pub detail: String,
    #[serde(default)]
    pub evidence_reference: Option<String>,
    #[serde(default)]
    pub retryable: bool,
}

/// Final verified outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifiedRepairOutcome {
    VerifiedSuccess,
    VerifiedFailure,
    BlockedByPolicy,
    ApprovalRequired,
    ManualOnly,
}

/// Full before/action/after proof returned to callers and audit sinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedRepairResult {
    pub component: String,
    pub action: String,
    pub outcome: VerifiedRepairOutcome,
    pub before: ComponentDiagnostic,
    pub after: ComponentDiagnostic,
    #[serde(default)]
    pub execution: Option<RepairExecution>,
    pub attempts: u32,
    pub started_at: DateTime<Utc>,
    pub verified_at: DateTime<Utc>,
    pub explanation: String,
}

impl VerifiedRepairResult {
    pub fn verified_success(&self) -> bool {
        self.outcome == VerifiedRepairOutcome::VerifiedSuccess
    }
}

#[async_trait]
pub trait RepairProbe: Send {
    async fn inspect(&mut self) -> ComponentDiagnostic;
}

#[async_trait]
pub trait RepairExecutor: Send {
    async fn execute(&mut self, plan: &RepairPlan) -> RepairExecution;
}

/// Executes a repair only when policy allows it and never reports success
/// until a fresh probe reaches an explicitly accepted state.
pub async fn run_verified_repair<P, E>(
    plan: &RepairPlan,
    probe: &mut P,
    executor: &mut E,
) -> VerifiedRepairResult
where
    P: RepairProbe,
    E: RepairExecutor,
{
    let started_at = Utc::now();
    let before = probe.inspect().await;

    let blocked = match plan.policy {
        RepairPolicyClass::AutomaticallySafe => None,
        RepairPolicyClass::RequiresUserApproval if plan.approved => None,
        RepairPolicyClass::RequiresUserApproval => Some((
            VerifiedRepairOutcome::ApprovalRequired,
            "Repair requires explicit user approval and was not executed.",
        )),
        RepairPolicyClass::DeniedDestructive => Some((
            VerifiedRepairOutcome::BlockedByPolicy,
            "Repair is denied by policy and was not executed.",
        )),
        RepairPolicyClass::ManualOnly => Some((
            VerifiedRepairOutcome::ManualOnly,
            "Repair is manual-only and was not executed.",
        )),
        RepairPolicyClass::Unavailable => Some((
            VerifiedRepairOutcome::BlockedByPolicy,
            "No executable repair is available for this diagnosis.",
        )),
    };

    if let Some((outcome, explanation)) = blocked {
        return VerifiedRepairResult {
            component: plan.component.clone(),
            action: plan.action.clone(),
            outcome,
            before: before.clone(),
            after: before,
            execution: None,
            attempts: 0,
            started_at,
            verified_at: Utc::now(),
            explanation: explanation.to_string(),
        };
    }

    let mut backoff = RetryBackoff::new(plan.retry);
    let mut attempts = 0;
    let mut last_execution = None;
    let mut after = before.clone();

    loop {
        attempts += 1;
        let execution = executor.execute(plan).await;
        let retryable = execution.retryable;
        last_execution = Some(execution);
        after = probe.inspect().await;

        let command_ok = last_execution
            .as_ref()
            .is_some_and(|execution| execution.exited_successfully);
        let state_ok = plan.success_states.contains(&after.state);
        if command_ok && state_ok {
            return VerifiedRepairResult {
                component: plan.component.clone(),
                action: plan.action.clone(),
                outcome: VerifiedRepairOutcome::VerifiedSuccess,
                before,
                after,
                execution: last_execution,
                attempts,
                started_at,
                verified_at: Utc::now(),
                explanation: "The action completed and a fresh health probe verified recovery."
                    .to_string(),
            };
        }

        if !retryable {
            break;
        }
        let Some(delay) = backoff.next_delay() else {
            break;
        };
        tokio::time::sleep(delay).await;
    }

    VerifiedRepairResult {
        component: plan.component.clone(),
        action: plan.action.clone(),
        outcome: VerifiedRepairOutcome::VerifiedFailure,
        before,
        after,
        execution: last_execution,
        attempts,
        started_at,
        verified_at: Utc::now(),
        explanation: "The action did not produce an accepted post-repair health state.".to_string(),
    }
}

/// Converts a legacy health status into a public-safe diagnostic so the
/// existing desktop health registry can participate in before/after reports.
pub fn diagnostic_from_legacy(
    component: &str,
    status: crate::state::ComponentStatus,
    message: Option<&str>,
) -> ComponentDiagnostic {
    let (state, reason, root, retryable) = match status {
        crate::state::ComponentStatus::Healthy => (
            OperationalState::Running,
            "health_probe_passed",
            RootCauseCategory::Unknown,
            false,
        ),
        crate::state::ComponentStatus::Degrading => (
            OperationalState::Degraded,
            "health_probe_degraded",
            RootCauseCategory::Unknown,
            true,
        ),
        crate::state::ComponentStatus::Error => (
            OperationalState::Failed,
            "health_probe_failed",
            RootCauseCategory::Unknown,
            true,
        ),
        crate::state::ComponentStatus::Unknown => (
            OperationalState::Unavailable,
            "health_probe_unavailable",
            RootCauseCategory::Unknown,
            true,
        ),
    };
    ComponentDiagnostic::new(
        component,
        component,
        state,
        reason,
        message.unwrap_or("No additional public-safe probe detail was available."),
        root,
        retryable,
    )
    .with_evidence("health_probe", "desktop.health_registry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct SequenceProbe {
        values: VecDeque<ComponentDiagnostic>,
    }

    #[async_trait]
    impl RepairProbe for SequenceProbe {
        async fn inspect(&mut self) -> ComponentDiagnostic {
            self.values
                .pop_front()
                .or_else(|| self.values.back().cloned())
                .unwrap_or_else(ComponentDiagnostic::default)
        }
    }

    struct SequenceExecutor {
        values: VecDeque<RepairExecution>,
        calls: u32,
    }

    #[async_trait]
    impl RepairExecutor for SequenceExecutor {
        async fn execute(&mut self, _plan: &RepairPlan) -> RepairExecution {
            self.calls += 1;
            self.values.pop_front().unwrap_or(RepairExecution {
                exited_successfully: false,
                detail: "no synthetic execution".to_string(),
                evidence_reference: None,
                retryable: false,
            })
        }
    }

    fn diag(state: OperationalState) -> ComponentDiagnostic {
        ComponentDiagnostic::new(
            "file_watcher",
            "file_activity",
            state,
            "synthetic",
            "synthetic",
            RootCauseCategory::Internal,
            true,
        )
    }

    fn success_execution() -> RepairExecution {
        RepairExecution {
            exited_successfully: true,
            detail: "restart request accepted".to_string(),
            evidence_reference: Some("repair_intent:synthetic".to_string()),
            retryable: false,
        }
    }

    #[tokio::test]
    async fn command_exit_alone_is_not_success() {
        let mut probe = SequenceProbe {
            values: VecDeque::from([
                diag(OperationalState::Failed),
                diag(OperationalState::Failed),
            ]),
        };
        let mut executor = SequenceExecutor {
            values: VecDeque::from([success_execution()]),
            calls: 0,
        };
        let result = run_verified_repair(
            &RepairPlan::automatically_safe("file_watcher", "restart"),
            &mut probe,
            &mut executor,
        )
        .await;
        assert_eq!(result.outcome, VerifiedRepairOutcome::VerifiedFailure);
        assert!(!result.verified_success());
    }

    #[tokio::test]
    async fn recovery_requires_a_fresh_accepted_state() {
        let mut probe = SequenceProbe {
            values: VecDeque::from([
                diag(OperationalState::Failed),
                diag(OperationalState::Running),
            ]),
        };
        let mut executor = SequenceExecutor {
            values: VecDeque::from([success_execution()]),
            calls: 0,
        };
        let result = run_verified_repair(
            &RepairPlan::automatically_safe("file_watcher", "restart"),
            &mut probe,
            &mut executor,
        )
        .await;
        assert_eq!(result.outcome, VerifiedRepairOutcome::VerifiedSuccess);
        assert_eq!(result.attempts, 1);
    }

    #[tokio::test]
    async fn explicit_deny_cannot_be_bypassed() {
        let mut plan = RepairPlan::automatically_safe("memory", "delete_cache");
        plan.policy = RepairPolicyClass::DeniedDestructive;
        plan.approved = true;
        let mut probe = SequenceProbe {
            values: VecDeque::from([diag(OperationalState::Failed)]),
        };
        let mut executor = SequenceExecutor {
            values: VecDeque::from([success_execution()]),
            calls: 0,
        };
        let result = run_verified_repair(&plan, &mut probe, &mut executor).await;
        assert_eq!(result.outcome, VerifiedRepairOutcome::BlockedByPolicy);
        assert_eq!(executor.calls, 0);
    }

    #[test]
    fn retry_backoff_is_exponential_capped_and_bounded() {
        let mut backoff = RetryBackoff::new(RetryPolicy {
            max_attempts: 4,
            base_delay_ms: 100,
            max_delay_ms: 250,
        });
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(100)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(200)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(250)));
        assert_eq!(backoff.next_delay(), Some(Duration::from_millis(250)));
        assert_eq!(backoff.next_delay(), None);
    }
}
