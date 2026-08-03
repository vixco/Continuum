//! # Curator — memory vault curation pipeline (Plan B)
//!
//! The curator periodically reviews recent activity (perception events,
//! session transcripts) and extracts durable memories worth writing into the
//! [`continuum_memory`] vault: facts, preferences, decisions, goals, errors,
//! and the like. It leans on the triage layer's already-loaded local model
//! ([`crate::triage::llm::TriageLayer`]) for extraction completions rather
//! than waking the orchestrator for routine memory bookkeeping — see
//! non-negotiable #4 (data flows up, commands flow down; workers/curator
//! never trigger the orchestrator).
//!
//! Only the [`CuratorLlm`] impl for `TriageLayer` needs the heavy llama.cpp
//! runtime, so it is gated behind the `runtime` feature. The trait itself,
//! [`CuratorStatus`], and the extraction logic in [`extract`] are plain
//! serde/std and compile unconditionally — the desktop crate (which builds
//! with `default-features = false`) links this module to share types with
//! the runtime binary.

pub mod extract;
pub mod run;

use std::sync::{Arc, Mutex};

/// One-shot text completion backend for the curator.
///
/// Implemented by [`crate::triage::llm::TriageLayer`] (runtime-gated, backed
/// by the shared local model) and by test doubles.
#[async_trait::async_trait]
pub trait CuratorLlm: Send + Sync {
    /// One-shot completion. Implementations serialize internally (e.g. via
    /// a mutex around the underlying model context) so concurrent callers
    /// queue rather than race.
    async fn complete(&self, prompt: &str, max_tokens: u32) -> anyhow::Result<String>;
}

/// Rolling curator status for the runtime snapshot (Task 11) and the
/// dashboard's health surface.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CuratorStatus {
    /// When the most recent curator pass completed, successfully or not.
    pub last_pass_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Consecutive failed passes. A self-healing signal: the repair agent
    /// escalates once this crosses a threshold, mirroring the triage
    /// layer's `consecutive_failures` pattern.
    pub consecutive_failures: u32,
    /// Lifetime count of candidate/confirmed notes the curator has written.
    pub candidates_written_total: u64,
    /// Current count of notes awaiting human review (`NodeStatus::Candidate`).
    pub pending_count: u64,
}

/// Shared handle to [`CuratorStatus`], updated by the curator pass loop and
/// read by the runtime snapshot / dashboard / `system_health` MCP tool.
pub type SharedCuratorStatus = Arc<Mutex<CuratorStatus>>;

#[cfg(feature = "runtime")]
#[async_trait::async_trait]
impl CuratorLlm for crate::triage::llm::TriageLayer {
    async fn complete(&self, prompt: &str, max_tokens: u32) -> anyhow::Result<String> {
        // `Self::complete` here resolves to `TriageLayer`'s own inherent
        // `complete` method (see `triage/llm.rs`), not this trait method —
        // if that inherent method were ever renamed to `complete` colliding
        // ambiguously with this trait fn, this call becomes a silent
        // infinite recursion instead of a compile error.
        Self::complete(self, prompt, max_tokens).await
    }
}

/// Scripted [`CuratorLlm`] test double: replies are consumed in the order
/// given to [`MockLlm::new`], and `complete()` errors once the script is
/// exhausted. `pub(crate)` so extraction/run/conflict/session tests across
/// the curator module can all share one implementation.
#[cfg(test)]
pub(crate) struct MockLlm {
    replies: std::sync::Mutex<Vec<String>>,
    calls: std::sync::atomic::AtomicU32,
}

#[cfg(test)]
impl MockLlm {
    /// Build a mock that returns `replies` in order, one per `complete()` call.
    pub(crate) fn new(replies: Vec<String>) -> Self {
        let mut queue = replies;
        queue.reverse(); // pop() from the back = first-in-first-out
        Self {
            replies: std::sync::Mutex::new(queue),
            calls: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Alias for [`MockLlm::new`] — reads better at extraction/run-loop
    /// call sites that are literally scripting a sequence of LLM replies.
    pub(crate) fn scripted(replies: Vec<String>) -> Self {
        Self::new(replies)
    }

    /// Number of times `complete()` has been called so far.
    pub(crate) fn calls(&self) -> u32 {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl CuratorLlm for MockLlm {
    async fn complete(&self, _prompt: &str, _max_tokens: u32) -> anyhow::Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("MockLlm: scripted replies exhausted"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_llm_pops_scripted_replies_in_order_and_errors_when_empty() {
        let mock = MockLlm::new(vec!["one".to_string(), "two".to_string()]);
        assert_eq!(mock.complete("p", 10).await.unwrap(), "one");
        assert_eq!(mock.complete("p", 10).await.unwrap(), "two");
        assert!(mock.complete("p", 10).await.is_err());
        assert_eq!(mock.calls(), 3);
    }
}
