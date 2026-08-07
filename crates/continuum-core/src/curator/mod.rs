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
//! [`CuratorStatus`], the extraction logic in [`extract`], and the
//! conflict/supersede detection in [`conflict`] are plain serde/std and
//! compile unconditionally — the desktop crate (which builds with
//! `default-features = false`) links this module to share types with the
//! runtime binary.

pub mod conflict;
pub mod extract;
pub mod run;
pub mod session;

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
    ///
    /// Task B2 (spec §4.7): callers of this trait are BACKGROUND consumers
    /// of the shared local model and must pass `max_tokens` ≤
    /// [`crate::llm_gate::BACKGROUND_MAX_TOKENS`] per call (chunk longer
    /// outputs across calls). The production implementation
    /// ([`crate::triage::llm::TriageLayer::complete`]) enforces both the
    /// clamp and the two-priority acquisition (try-acquire/backoff behind
    /// interactive triage); an `Err` can therefore also mean "gate stayed
    /// busy with interactive work" — treat it like any completion failure
    /// and retry on the next scheduled pass.
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

/// Wraps an arbitrary prompt in Qwen 3's ChatML format with a `/no_think`
/// directive. Qwen 3 defaults to emitting a `<think>...</think>` reasoning
/// block before its actual answer; `/no_think` suppresses it, but only
/// reliably takes effect inside a proper ChatML turn — see
/// `triage::prompts::build_triage_prompt` for the triage layer's own
/// (system-prompt-carrying) version of the same wrapper.
///
/// Used by [`crate::triage::llm::TriageLayer::complete`] (I2 fix), the
/// curator's shared one-shot completion path — extraction
/// ([`crate::curator::run::extract_pass`]), conflict detection
/// ([`crate::curator::conflict::detect_conflicts`]), and session summaries
/// ([`crate::curator::session::write_session_summary`]) all call it, and
/// without this wrapper every one of those prompts risked a stray
/// `<think>` block bleeding into `parse_candidates`/`parse_verdict`/the
/// session prompt's exact `"SKIP"` match, none of which expect one.
///
/// Kept here rather than in `triage::prompts` so it stays
/// featureless-testable: `triage::prompts` is gated behind the `runtime`
/// feature (it pulls in the llama.cpp-backed `TriageLayer`), but this
/// module is not (see this module's doc comment).
///
/// `#[allow(dead_code)]`: the only non-test caller
/// ([`crate::triage::llm::TriageLayer::complete`]) is itself gated behind
/// the `runtime` feature, so a plain `--no-default-features` library build
/// (no tests, no runtime) genuinely never calls this — that's expected,
/// not a bug; the desktop crate links this module for its plain types and
/// never needs the wrapper.
#[allow(dead_code)]
pub(crate) fn wrap_no_think(prompt: &str) -> String {
    format!("<|im_start|>user\n/no_think\n{prompt}<|im_end|>\n<|im_start|>assistant\n")
}

/// Strips a leading `<think>...</think>` reasoning block from a raw
/// completion, returning the trimmed remainder unchanged when there is no
/// such block. Mirrors `triage::extract_json_object`'s `</think>`-skip
/// logic (see that function's doc comment in `triage/mod.rs`), but is not
/// JSON-specific: the curator's session-summary replies are markdown, not
/// JSON, so this just returns plain trimmed text rather than hunting for a
/// brace-balanced object. Paired with [`wrap_no_think`] inside
/// [`crate::triage::llm::TriageLayer::complete`] (I2 fix).
///
/// `#[allow(dead_code)]`: see [`wrap_no_think`]'s doc comment — same
/// reasoning applies here.
#[allow(dead_code)]
pub(crate) fn strip_think_block(raw: &str) -> &str {
    let s = raw.trim();
    let after = match s.rfind("</think>") {
        Some(pos) => &s[pos + "</think>".len()..],
        None => s,
    };
    after.trim()
}

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
    /// Every prompt passed to `complete()`, in call order — lets tests
    /// inspect exactly what was sent (I1 fix regression coverage: the
    /// prompt-budget cap and the "same prompt + suffix" retry shape) without
    /// each test needing its own bespoke mock.
    prompts: std::sync::Mutex<Vec<String>>,
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
            prompts: std::sync::Mutex::new(Vec::new()),
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

    /// Every prompt seen by `complete()` so far, in call order.
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl CuratorLlm for MockLlm {
    async fn complete(&self, prompt: &str, _max_tokens: u32) -> anyhow::Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt.to_string());
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

    // --- I2: no_think wrap/strip helpers (featureless-testable) --------

    #[test]
    fn wrap_no_think_produces_chatml_with_no_think_directive() {
        let wrapped = wrap_no_think("extract these facts");
        assert!(wrapped.starts_with("<|im_start|>user\n/no_think\n"));
        assert!(wrapped.contains("extract these facts"));
        assert!(wrapped.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn strip_think_block_removes_leading_think_tag() {
        let raw = "<think>\nreasoning about the candidates\n</think>\n\n[{\"a\":1}]";
        assert_eq!(strip_think_block(raw), "[{\"a\":1}]");
    }

    #[test]
    fn strip_think_block_is_noop_without_think_tag() {
        assert_eq!(strip_think_block("  [{\"a\":1}]  "), "[{\"a\":1}]");
    }

    #[test]
    fn strip_think_block_handles_exact_skip_reply() {
        // Regression for the session-summary "SKIP" path (write_session_summary
        // matches the trimmed reply against the literal string "SKIP") — a
        // <think> block ahead of it must not survive into that comparison.
        let raw = "<think>this session looks trivial</think>\nSKIP";
        assert_eq!(strip_think_block(raw), "SKIP");
    }
}
