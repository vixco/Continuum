//! # Authorized runtime repair intents
//!
//! The desktop repair flow may grant a short-lived capability to a dedicated
//! MCP process. That process can queue only a small, typed in-process restart
//! request. This module validates the capability again inside the runtime,
//! restarts the existing single supervisor (never a second process/task), and
//! verifies recovery from a fresh post-restart health observation.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::operational_state::{
    ComponentDiagnostic, OperationalEventBuffer, OperationalEventKind, OperationalState,
    RepairPolicyClass,
};
use crate::runtime_control::{RuntimeServiceControl, RuntimeServiceName};

use super::verified::VerifiedRepairOutcome;

const REPAIR_INTENTS_DIR: &str = "repair-intents";
const MAX_INTENT_BYTES: u64 = 16 * 1024;
const DEFAULT_DRAIN_LIMIT: usize = 32;
const DEFAULT_VERIFY_TIMEOUT_SECS: i64 = 20;

/// Safe runtime components that support a single-supervisor in-process restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRepairTarget {
    FileWatcher,
    ProcessWatcher,
}

impl RuntimeRepairTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileWatcher => "file_watcher",
            Self::ProcessWatcher => "process_watcher",
        }
    }

    pub const fn service(self) -> RuntimeServiceName {
        match self {
            Self::FileWatcher => RuntimeServiceName::FileActivity,
            Self::ProcessWatcher => RuntimeServiceName::BackgroundActivity,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "file_watcher" => Some(Self::FileWatcher),
            "process_watcher" => Some(Self::ProcessWatcher),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RepairIntentHeader {
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RepairIntentEnvelope {
    kind: String,
    queued_at: DateTime<Utc>,
    authorization: Option<String>,
    body: RepairIntentBody,
}

#[derive(Debug, Deserialize)]
struct RepairIntentBody {
    component: String,
}

/// One capability-validated restart request ready for the runtime coordinator.
#[derive(Debug, Clone)]
pub struct AuthorizedRuntimeRepair {
    pub target: RuntimeRepairTarget,
    pub queued_at: DateTime<Utc>,
}

/// Public-safe explanation for an intent that was consumed but rejected.
#[derive(Debug, Clone, Serialize)]
pub struct RejectedRuntimeRepair {
    pub reason_code: String,
    pub explanation: String,
}

/// One bounded drain result. Raw file contents, tokens and paths are never
/// returned or logged through this public structure.
#[derive(Debug, Clone, Default)]
pub struct RuntimeRepairDrain {
    pub accepted: Vec<AuthorizedRuntimeRepair>,
    pub rejected: Vec<RejectedRuntimeRepair>,
}

/// Bounded, duplicate-resistant repair-intent reader.
pub struct RuntimeRepairIntentDrainer {
    max_files: usize,
    seen: HashSet<String>,
}

impl Default for RuntimeRepairIntentDrainer {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_DRAIN_LIMIT,
            seen: HashSet::new(),
        }
    }
}

impl RuntimeRepairIntentDrainer {
    pub fn with_limit(max_files: usize) -> Self {
        Self {
            max_files: max_files.max(1),
            seen: HashSet::new(),
        }
    }

    /// Consumes at most `max_files` complete JSON intents. Hidden temporary
    /// files are ignored. Every capability is revalidated against the live
    /// grant file and explicit restart allowlist.
    pub fn drain(&mut self, data_dir: &Path) -> RuntimeRepairDrain {
        let mut result = RuntimeRepairDrain::default();
        let dir = data_dir.join(REPAIR_INTENTS_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return result;
        };
        let mut entries = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.starts_with('.') && name.ends_with(".json") && name.contains("-restart-")
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries.into_iter().take(self.max_files) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !self.seen.insert(name.clone()) {
                continue;
            }
            if self.seen.len() > 256 {
                self.seen.clear();
                self.seen.insert(name);
            }

            let path = entry.path();
            match parse_and_authorize(data_dir, &path) {
                ParsedRuntimeRepair::Accepted(intent) => {
                    // The runtime owns restart intents. A consumed request is
                    // never retried implicitly; a later attempt needs a fresh
                    // authorized capability and a new verification cycle.
                    let _ = std::fs::remove_file(&path);
                    result.accepted.push(intent);
                }
                ParsedRuntimeRepair::Rejected(rejected) => {
                    let _ = std::fs::remove_file(&path);
                    result.rejected.push(rejected);
                }
                ParsedRuntimeRepair::Ignored => {
                    // Reinstall/escalation compatibility intents have distinct
                    // policy consumers. This restart consumer must not steal
                    // or delete them.
                }
            }
        }
        result
    }
}

enum ParsedRuntimeRepair {
    Accepted(AuthorizedRuntimeRepair),
    Rejected(RejectedRuntimeRepair),
    Ignored,
}

fn parse_and_authorize(data_dir: &Path, path: &Path) -> ParsedRuntimeRepair {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            return ParsedRuntimeRepair::Rejected(rejected(
                "intent_unreadable",
                "A repair request could not be read and was discarded.",
            ))
        }
    };
    if metadata.len() > MAX_INTENT_BYTES {
        return ParsedRuntimeRepair::Rejected(rejected(
            "intent_too_large",
            "A repair request exceeded the bounded size limit and was discarded.",
        ));
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return ParsedRuntimeRepair::Rejected(rejected(
                "intent_unreadable",
                "A repair request could not be read and was discarded.",
            ))
        }
    };
    let header: RepairIntentHeader = match serde_json::from_slice(&bytes) {
        Ok(header) => header,
        Err(_) => {
            return ParsedRuntimeRepair::Rejected(rejected(
                "intent_invalid",
                "A malformed repair request was discarded.",
            ))
        }
    };
    if header.kind != "restart" {
        return ParsedRuntimeRepair::Ignored;
    }
    let envelope: RepairIntentEnvelope = match serde_json::from_slice(&bytes) {
        Ok(envelope) => envelope,
        Err(_) => {
            return ParsedRuntimeRepair::Rejected(rejected(
                "intent_invalid",
                "A malformed restart request was discarded.",
            ))
        }
    };
    let Some(target) = RuntimeRepairTarget::parse(&envelope.body.component) else {
        return ParsedRuntimeRepair::Rejected(rejected(
            "repair_target_denied",
            "The requested component does not support safe in-process restart.",
        ));
    };
    let Some(authorization) = envelope.authorization.as_deref() else {
        return ParsedRuntimeRepair::Rejected(rejected(
            "repair_capability_missing",
            "The restart request did not carry an authorized capability.",
        ));
    };
    let grant = match super::repair::authorize_repair_session(data_dir, authorization) {
        Ok(grant) => grant,
        Err(_) => {
            return ParsedRuntimeRepair::Rejected(rejected(
                "repair_capability_invalid",
                "The repair request did not carry a live authorized capability.",
            ))
        }
    };
    if !grant
        .allowed_restart_components
        .iter()
        .any(|component| component == target.as_str())
    {
        return ParsedRuntimeRepair::Rejected(rejected(
            "repair_not_allowlisted",
            "The requested restart was not authorized by the user's live repair preview.",
        ));
    }
    if envelope.queued_at > Utc::now() + chrono::Duration::minutes(1) {
        return ParsedRuntimeRepair::Rejected(rejected(
            "repair_timestamp_invalid",
            "The repair request timestamp was invalid and the request was discarded.",
        ));
    }
    ParsedRuntimeRepair::Accepted(AuthorizedRuntimeRepair {
        target,
        queued_at: envelope.queued_at,
    })
}

fn rejected(reason_code: &str, explanation: &str) -> RejectedRuntimeRepair {
    RejectedRuntimeRepair {
        reason_code: reason_code.to_string(),
        explanation: explanation.to_string(),
    }
}

/// One fresh health observation used to verify a restart.
#[derive(Debug, Clone)]
pub struct RuntimeRepairObservation {
    pub diagnostic: ComponentDiagnostic,
    pub activation_count: u64,
}

#[derive(Debug, Clone)]
struct PendingRuntimeRepair {
    target: RuntimeRepairTarget,
    before: RuntimeRepairObservation,
    requested_generation: u64,
    started_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

/// Final before/after proof for one runtime repair attempt.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeRepairVerification {
    pub component: String,
    pub outcome: VerifiedRepairOutcome,
    pub before: ComponentDiagnostic,
    pub after: ComponentDiagnostic,
    pub requested_generation: u64,
    pub started_at: DateTime<Utc>,
    pub verified_at: DateTime<Utc>,
    pub explanation: String,
}

/// Coordinates safe restarts and refuses duplicate attempts for a component
/// until the prior attempt has produced a verification result.
pub struct RuntimeRepairCoordinator {
    pending: HashMap<RuntimeRepairTarget, PendingRuntimeRepair>,
    events: OperationalEventBuffer,
    verify_timeout: chrono::Duration,
}

impl RuntimeRepairCoordinator {
    pub fn new(events: OperationalEventBuffer) -> Self {
        Self {
            pending: HashMap::new(),
            events,
            verify_timeout: chrono::Duration::seconds(DEFAULT_VERIFY_TIMEOUT_SECS),
        }
    }

    #[cfg(test)]
    fn with_timeout(events: OperationalEventBuffer, verify_timeout: chrono::Duration) -> Self {
        Self {
            pending: HashMap::new(),
            events,
            verify_timeout,
        }
    }

    /// Starts one repair after re-checking that the service is enabled and no
    /// attempt for the same supervisor is already pending.
    pub fn start(
        &mut self,
        intent: AuthorizedRuntimeRepair,
        control: &RuntimeServiceControl,
        before: RuntimeRepairObservation,
    ) -> bool {
        if self.pending.contains_key(&intent.target) {
            self.events.record(
                OperationalEventKind::RepairFailed,
                intent.target.as_str(),
                Some(before.diagnostic.state),
                before.diagnostic.state,
                "repair_already_pending",
                "A repair for this component is already awaiting verification.",
            );
            return false;
        }
        if !control.enabled(intent.target.service()) {
            self.events.record(
                OperationalEventKind::RepairFailed,
                intent.target.as_str(),
                Some(before.diagnostic.state),
                OperationalState::DisabledByUser,
                "repair_target_disabled",
                "The component is disabled by the user, so an automatic restart was not executed.",
            );
            return false;
        }
        if before.diagnostic.repair.class != RepairPolicyClass::AutomaticallySafe
            || !before.diagnostic.repair.available
        {
            self.events.record(
                OperationalEventKind::RepairFailed,
                intent.target.as_str(),
                Some(before.diagnostic.state),
                before.diagnostic.state,
                "repair_no_longer_applicable",
                "The latest health observation no longer permits an automatic restart.",
            );
            return false;
        }

        let requested_generation = control.request_restart(intent.target.service());
        let started_at = Utc::now();
        self.events.record(
            OperationalEventKind::RepairStarted,
            intent.target.as_str(),
            Some(before.diagnostic.state),
            OperationalState::Starting,
            "verified_restart_started",
            "An authorized in-process restart started; recovery is not claimed until a fresh health probe passes.",
        );
        self.pending.insert(
            intent.target,
            PendingRuntimeRepair {
                target: intent.target,
                before,
                requested_generation,
                started_at,
                deadline: started_at + self.verify_timeout,
            },
        );
        true
    }

    /// Re-checks every pending repair against fresh watcher health. Success
    /// requires both a new activation and an accepted running/idle state.
    pub fn verify<F>(
        &mut self,
        now: DateTime<Utc>,
        mut observe: F,
    ) -> Vec<RuntimeRepairVerification>
    where
        F: FnMut(RuntimeRepairTarget) -> Option<RuntimeRepairObservation>,
    {
        let targets = self.pending.keys().copied().collect::<Vec<_>>();
        let mut completed = Vec::new();
        for target in targets {
            let Some(current) = observe(target) else {
                continue;
            };
            let Some(pending) = self.pending.get(&target).cloned() else {
                continue;
            };
            let activated = current.activation_count > pending.before.activation_count;
            let accepted_state = matches!(
                current.diagnostic.state,
                OperationalState::Running | OperationalState::Idle
            );
            let terminal_failure = activated
                && matches!(
                    current.diagnostic.state,
                    OperationalState::Failed
                        | OperationalState::PermissionRequired
                        | OperationalState::Unavailable
                        | OperationalState::DisabledByPolicy
                        | OperationalState::DisabledByUser
                );
            if !activated || (!accepted_state && !terminal_failure && now < pending.deadline) {
                if now < pending.deadline {
                    continue;
                }
            }

            let success = activated && accepted_state;
            let outcome = if success {
                VerifiedRepairOutcome::VerifiedSuccess
            } else {
                VerifiedRepairOutcome::VerifiedFailure
            };
            let explanation = if success {
                "A fresh post-restart health observation verified a new activation in an accepted state."
                    .to_string()
            } else if !activated {
                "The restart request did not produce a new component activation before the verification deadline."
                    .to_string()
            } else {
                "The component reactivated but did not reach an accepted running or idle state."
                    .to_string()
            };
            self.events.record(
                OperationalEventKind::VerificationResult,
                target.as_str(),
                Some(pending.before.diagnostic.state),
                current.diagnostic.state,
                if success {
                    "repair_verified_success"
                } else {
                    "repair_verified_failure"
                },
                explanation.clone(),
            );
            self.events.record(
                if success {
                    OperationalEventKind::RepairCompleted
                } else {
                    OperationalEventKind::RepairFailed
                },
                target.as_str(),
                Some(pending.before.diagnostic.state),
                current.diagnostic.state,
                if success {
                    "repair_completed"
                } else {
                    "repair_failed"
                },
                explanation.clone(),
            );
            self.pending.remove(&target);
            completed.push(RuntimeRepairVerification {
                component: pending.target.as_str().to_string(),
                outcome,
                before: pending.before.diagnostic,
                after: current.diagnostic,
                requested_generation: pending.requested_generation,
                started_at: pending.started_at,
                verified_at: now,
                explanation,
            });
        }
        completed
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operational_state::{RepairPolicyClass, RootCauseCategory};
    use crate::runtime_control::RuntimeServiceControl;
    use tempfile::TempDir;

    fn diagnostic(component: &str, state: OperationalState) -> ComponentDiagnostic {
        ComponentDiagnostic::new(
            component,
            component,
            state,
            "synthetic",
            "Synthetic public-safe health state.",
            RootCauseCategory::Unknown,
            true,
        )
        .with_repair(RepairPolicyClass::AutomaticallySafe, true, Some("restart"))
    }

    fn observation(
        target: RuntimeRepairTarget,
        state: OperationalState,
        activation_count: u64,
    ) -> RuntimeRepairObservation {
        RuntimeRepairObservation {
            diagnostic: diagnostic(target.as_str(), state),
            activation_count,
        }
    }

    fn write_grant_and_intent(dir: &Path, component: &str, allow_restart: bool) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let grant = super::super::repair::RepairSessionGrant {
            token: token.clone(),
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::minutes(5),
            allowed_components: vec![component.to_string()],
            allowed_restart_components: if allow_restart {
                vec![component.to_string()]
            } else {
                Vec::new()
            },
            allow_escalation_intent: false,
            allow_model_reinstall: false,
            allow_config_rollback: false,
        };
        let grants = dir.join("repair-grants");
        std::fs::create_dir_all(&grants).unwrap();
        std::fs::write(
            grants.join(format!("{token}.json")),
            serde_json::to_vec(&grant).unwrap(),
        )
        .unwrap();
        let intents = dir.join(REPAIR_INTENTS_DIR);
        std::fs::create_dir_all(&intents).unwrap();
        std::fs::write(
            intents.join("20260810T000000000-restart-000000.json"),
            serde_json::to_vec(&serde_json::json!({
                "kind": "restart",
                "queued_at": Utc::now(),
                "authorization": token.clone(),
                "body": { "component": component },
            }))
            .unwrap(),
        )
        .unwrap();
        token
    }

    #[test]
    fn restart_intent_without_authorization_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let intents = tmp.path().join(REPAIR_INTENTS_DIR);
        std::fs::create_dir_all(&intents).unwrap();
        std::fs::write(
            intents.join("20260810T000000000-restart-000000.json"),
            serde_json::to_vec(&serde_json::json!({
                "kind": "restart",
                "queued_at": Utc::now(),
                "authorization": null,
                "body": { "component": "file_watcher" },
            }))
            .unwrap(),
        )
        .unwrap();

        let mut drainer = RuntimeRepairIntentDrainer::default();
        let result = drainer.drain(tmp.path());
        assert!(result.accepted.is_empty());
        assert_eq!(result.rejected[0].reason_code, "repair_capability_missing");
    }

    #[test]
    fn intent_requires_live_allowlisted_capability() {
        let tmp = TempDir::new().unwrap();
        write_grant_and_intent(tmp.path(), "file_watcher", false);
        let mut drainer = RuntimeRepairIntentDrainer::default();
        let result = drainer.drain(tmp.path());
        assert!(result.accepted.is_empty());
        assert_eq!(result.rejected[0].reason_code, "repair_not_allowlisted");
    }

    #[test]
    fn valid_intent_is_consumed_once() {
        let tmp = TempDir::new().unwrap();
        write_grant_and_intent(tmp.path(), "file_watcher", true);
        let mut drainer = RuntimeRepairIntentDrainer::default();
        let first = drainer.drain(tmp.path());
        assert_eq!(first.accepted.len(), 1);
        assert!(drainer.drain(tmp.path()).accepted.is_empty());
    }

    #[test]
    fn unrelated_repair_intents_are_not_consumed() {
        let tmp = TempDir::new().unwrap();
        let intents = tmp.path().join(REPAIR_INTENTS_DIR);
        std::fs::create_dir_all(&intents).unwrap();
        let path = intents.join("legacy.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "kind": "escalate",
                "queued_at": Utc::now(),
                "authorization": null,
                "body": { "message": "synthetic" },
            }))
            .unwrap(),
        )
        .unwrap();

        let mut drainer = RuntimeRepairIntentDrainer::default();
        let result = drainer.drain(tmp.path());
        assert!(result.accepted.is_empty());
        assert!(result.rejected.is_empty());
        assert!(path.exists(), "another consumer still owns this intent");
    }

    #[test]
    fn disabled_target_is_never_restarted() {
        let events = OperationalEventBuffer::default();
        let mut coordinator = RuntimeRepairCoordinator::new(events.clone());
        let control = RuntimeServiceControl::default();
        let started = coordinator.start(
            AuthorizedRuntimeRepair {
                target: RuntimeRepairTarget::FileWatcher,
                queued_at: Utc::now(),
            },
            &control,
            observation(
                RuntimeRepairTarget::FileWatcher,
                OperationalState::DisabledByUser,
                0,
            ),
        );
        assert!(!started);
        assert_eq!(
            control.restart_generation(RuntimeServiceName::FileActivity),
            0
        );
        assert_eq!(events.recent()[0].reason_code, "repair_target_disabled");
    }

    #[test]
    fn command_request_alone_is_not_success() {
        let events = OperationalEventBuffer::default();
        let mut coordinator =
            RuntimeRepairCoordinator::with_timeout(events, chrono::Duration::milliseconds(1));
        let control = RuntimeServiceControl::new(true, false, true);
        assert!(coordinator.start(
            AuthorizedRuntimeRepair {
                target: RuntimeRepairTarget::FileWatcher,
                queued_at: Utc::now(),
            },
            &control,
            observation(
                RuntimeRepairTarget::FileWatcher,
                OperationalState::Failed,
                2
            ),
        ));
        let results = coordinator.verify(Utc::now() + chrono::Duration::seconds(1), |_| {
            Some(observation(
                RuntimeRepairTarget::FileWatcher,
                OperationalState::Starting,
                2,
            ))
        });
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, VerifiedRepairOutcome::VerifiedFailure);
    }

    #[test]
    fn new_activation_in_idle_state_is_verified_success() {
        let events = OperationalEventBuffer::default();
        let mut coordinator = RuntimeRepairCoordinator::new(events);
        let control = RuntimeServiceControl::new(false, true, true);
        assert!(coordinator.start(
            AuthorizedRuntimeRepair {
                target: RuntimeRepairTarget::ProcessWatcher,
                queued_at: Utc::now(),
            },
            &control,
            observation(
                RuntimeRepairTarget::ProcessWatcher,
                OperationalState::Failed,
                3
            ),
        ));
        let results = coordinator.verify(Utc::now(), |_| {
            Some(observation(
                RuntimeRepairTarget::ProcessWatcher,
                OperationalState::Idle,
                4,
            ))
        });
        assert_eq!(results[0].outcome, VerifiedRepairOutcome::VerifiedSuccess);
        assert_eq!(coordinator.pending_count(), 0);
    }

    #[test]
    fn duplicate_pending_restart_is_prevented() {
        let events = OperationalEventBuffer::default();
        let mut coordinator = RuntimeRepairCoordinator::new(events);
        let control = RuntimeServiceControl::new(true, false, true);
        let intent = AuthorizedRuntimeRepair {
            target: RuntimeRepairTarget::FileWatcher,
            queued_at: Utc::now(),
        };
        assert!(coordinator.start(
            intent.clone(),
            &control,
            observation(
                RuntimeRepairTarget::FileWatcher,
                OperationalState::Failed,
                0
            ),
        ));
        assert!(!coordinator.start(
            intent,
            &control,
            observation(
                RuntimeRepairTarget::FileWatcher,
                OperationalState::Failed,
                0
            ),
        ));
        assert_eq!(
            control.restart_generation(RuntimeServiceName::FileActivity),
            1
        );
    }
}
