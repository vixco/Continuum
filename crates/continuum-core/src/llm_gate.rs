//! # LlmGate — two-priority acquisition for the shared local LLM
//!
//! Task B2 of the context engine (spec §4.7 "LocalLlm discipline"). The
//! local model (`continuum-llm`'s [`LocalLlm`]) serializes generation on an
//! internal blocking mutex. That mutex is priority-blind: a background
//! caller (curator extraction, session summaries, conflict detection,
//! future session-state inference) that queues on it can hold an
//! interactive triage call — and with it the perception frame pipeline —
//! hostage for the full length of its generation.
//!
//! `LlmGate` serializes *intent* in async land, BEFORE anyone touches the
//! blocking lock, with two tiers:
//!
//! - **Interactive** (triage): [`LlmGate::acquire_interactive`] queues on
//!   the gate's semaphore directly. Because background callers never queue
//!   (they only `try_acquire`), the semaphore's FIFO contains interactive
//!   waiters exclusively — an interactive caller waits at most for the one
//!   generation currently in flight, never behind a parked background call.
//! - **Background** (curator, session inference — Plan B seam B5):
//!   [`LlmGate::acquire_background`] loops `try_acquire` + backoff, and
//!   defers outright (no `try_acquire` at all) while any interactive caller
//!   is waiting, so a freed permit always goes to the interactive tier
//!   first. Background callers additionally have their per-call token
//!   budget clamped to [`BACKGROUND_MAX_TOKENS`] — long outputs must be
//!   chunked into multiple gated calls so an interactive caller can slot in
//!   between chunks.
//!
//! The gate does not replace the mutex inside `continuum-llm` — that lock
//! still guards the llama.cpp context. The gate only guarantees the *order*
//! in which callers reach it: with every generation call gated, at most one
//! caller holds a permit at a time, so nobody ever actually blocks on the
//! inner mutex.
//!
//! This module is deliberately NOT gated behind the `runtime` feature: it
//! is pure `tokio::sync` + atomics (no llama.cpp), so its logic is testable
//! under `--no-default-features` too. The runtime-gated
//! [`crate::triage::llm::TriageLayer`] owns the process-wide instance.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Hard cap on `max_tokens` for a single background generation call
/// (spec §4.7: "background callers … cap `max_tokens ≤ 256` per chunked
/// call"). [`LlmGate::acquire_background`] clamps requests above this and
/// logs a warning — a background caller that wants more output must chunk
/// it across multiple gated calls.
pub const BACKGROUND_MAX_TOKENS: u32 = 256;

/// Backoff between `try_acquire` attempts in
/// [`LlmGate::acquire_background`]. 250 ms keeps background latency
/// negligible against curator cadences (minutes) while polling far less
/// often than a generation takes.
pub const BACKGROUND_RETRY_MS: u64 = 250;

/// Maximum total time a background caller waits for the gate before giving
/// up with [`GateTimeout`]. 30 s comfortably covers an interactive triage
/// burst (a triage evaluation is 1–3 s; even a pathological 3-retry
/// evaluation stays under 10 s) without letting a curator pass hang
/// forever; every background caller (curator tick, session flush) already
/// treats a completion error as "retry on the next scheduled pass", so a
/// timeout here degrades to exactly that.
pub const BACKGROUND_MAX_WAIT_MS: u64 = 30_000;

/// Returned by [`LlmGate::acquire_background`] when the gate stayed
/// contended for the whole [`BACKGROUND_MAX_WAIT_MS`] window. Callers treat
/// this like any other completion failure: log, skip the pass, retry on
/// their next tick.
#[derive(Debug, thiserror::Error)]
#[error(
    "LLM gate busy with interactive work for {BACKGROUND_MAX_WAIT_MS} ms — background call skipped"
)]
pub struct GateTimeout;

/// Exclusive right to run one generation on the shared local model.
/// Released on drop.
#[derive(Debug)]
pub struct LlmPermit {
    _permit: OwnedSemaphorePermit,
}

/// Decrements `interactive_waiting` on drop so a cancelled
/// `acquire_interactive` future (e.g. its task was aborted mid-wait) can
/// never leave the counter stuck above zero — which would starve the
/// background tier permanently.
struct WaitingGuard(Arc<AtomicUsize>);

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Two-priority acquisition gate for the shared local LLM. Cheap to clone
/// (all handles); every clone gates the same underlying permit.
///
/// See the module doc for the full discipline. Plan B seam (B5): the
/// session-state inference task acquires through the background tier —
/// either directly via [`LlmGate::acquire_background`] or indirectly
/// through [`crate::triage::llm::TriageLayer::complete`], which wraps it.
#[derive(Clone)]
pub struct LlmGate {
    permit: Arc<Semaphore>,
    interactive_waiting: Arc<AtomicUsize>,
}

impl Default for LlmGate {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmGate {
    /// Creates a gate with a single generation permit.
    pub fn new() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
            interactive_waiting: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Acquires the gate at interactive priority (triage). Waits at most
    /// for the generation currently in flight — background callers never
    /// sit in this queue (they only `try_acquire`), so nothing can be
    /// scheduled ahead of an interactive waiter.
    pub async fn acquire_interactive(&self) -> LlmPermit {
        self.interactive_waiting.fetch_add(1, Ordering::AcqRel);
        let _guard = WaitingGuard(self.interactive_waiting.clone());
        let permit = self
            .permit
            .clone()
            .acquire_owned()
            .await
            .expect("LlmGate semaphore is never closed");
        LlmPermit { _permit: permit }
    }

    /// Acquires the gate at background priority: try-acquire with
    /// [`BACKGROUND_RETRY_MS`] backoff, deferring entirely while any
    /// interactive caller is waiting, timing out with [`GateTimeout`] after
    /// [`BACKGROUND_MAX_WAIT_MS`].
    ///
    /// `max_tokens` is the caller's requested generation budget; the
    /// returned budget is clamped to [`BACKGROUND_MAX_TOKENS`] (spec §4.7 —
    /// chunk longer outputs across multiple gated calls). The clamp lives
    /// here, on the acquisition path, so no background caller can bypass it
    /// without also bypassing the priority discipline.
    pub async fn acquire_background(
        &self,
        max_tokens: u32,
    ) -> Result<(LlmPermit, u32), GateTimeout> {
        let budget = if max_tokens > BACKGROUND_MAX_TOKENS {
            tracing::warn!(
                layer = "triage",
                component = "llm_gate",
                requested = max_tokens,
                clamped = BACKGROUND_MAX_TOKENS,
                "background LLM call requested more than the per-call token cap; clamping"
            );
            BACKGROUND_MAX_TOKENS
        } else {
            max_tokens
        };

        let deadline = tokio::time::Instant::now() + Duration::from_millis(BACKGROUND_MAX_WAIT_MS);
        loop {
            // Defer while an interactive caller is waiting: even if the
            // permit is momentarily free, it belongs to the interactive
            // queue. (An interactive caller that arrives in the instant
            // between this check and a successful try_acquire waits at
            // most one background generation — bounded by the ≤256-token
            // budget — which is the documented worst case.)
            if self.interactive_waiting.load(Ordering::Acquire) == 0 {
                if let Ok(permit) = self.permit.clone().try_acquire_owned() {
                    return Ok((LlmPermit { _permit: permit }, budget));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(GateTimeout);
            }
            tokio::time::sleep(Duration::from_millis(BACKGROUND_RETRY_MS)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test(start_paused = true)]
    async fn background_budget_clamped_to_cap() {
        let gate = LlmGate::new();
        let (_permit, budget) = gate
            .acquire_background(1024)
            .await
            .expect("free gate must grant immediately");
        assert_eq!(budget, BACKGROUND_MAX_TOKENS);
    }

    #[tokio::test(start_paused = true)]
    async fn background_budget_below_cap_passes_through() {
        let gate = LlmGate::new();
        let (_permit, budget) = gate
            .acquire_background(64)
            .await
            .expect("free gate must grant immediately");
        assert_eq!(budget, 64);
    }

    #[tokio::test(start_paused = true)]
    async fn background_waits_while_interactive_holds_then_acquires() {
        let gate = LlmGate::new();
        let held = gate.acquire_interactive().await;

        let bg_gate = gate.clone();
        let bg = tokio::spawn(async move { bg_gate.acquire_background(64).await });

        // Let the background task spin a few retries: it must not finish
        // while the interactive permit is held.
        tokio::time::sleep(Duration::from_millis(BACKGROUND_RETRY_MS * 4)).await;
        assert!(
            !bg.is_finished(),
            "background acquired while interactive held the gate"
        );

        drop(held);
        let (_permit, budget) = bg
            .await
            .expect("task panicked")
            .expect("background must acquire after interactive release");
        assert_eq!(budget, 64);
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_acquires_promptly_after_background_release() {
        let gate = LlmGate::new();
        let (held, _) = gate.acquire_background(64).await.expect("free gate");

        let int_gate = gate.clone();
        let interactive = tokio::spawn(async move {
            let _permit = int_gate.acquire_interactive().await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !interactive.is_finished(),
            "interactive acquired while background held the gate"
        );

        drop(held);
        tokio::time::timeout(Duration::from_millis(100), interactive)
            .await
            .expect("interactive did not acquire promptly after release")
            .expect("task panicked");
    }

    /// The ordering guarantee itself: with a background holder in place, a
    /// waiting interactive caller beats a backing-off background caller to
    /// the released permit — the background tier never queues ahead.
    #[tokio::test(start_paused = true)]
    async fn waiting_interactive_outranks_backing_off_background() {
        let gate = LlmGate::new();
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        let (holder, _) = gate.acquire_background(64).await.expect("free gate");

        let int_gate = gate.clone();
        let int_order = order.clone();
        let interactive = tokio::spawn(async move {
            let permit = int_gate.acquire_interactive().await;
            int_order.lock().unwrap().push("interactive");
            // Hold briefly so the released permit demonstrably went to us,
            // not to the background task racing on the same instant.
            tokio::time::sleep(Duration::from_millis(BACKGROUND_RETRY_MS * 2)).await;
            drop(permit);
        });

        // Give the interactive task time to be counted as waiting before
        // the second background caller starts polling.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let bg_gate = gate.clone();
        let bg_order = order.clone();
        let background = tokio::spawn(async move {
            let (_permit, _) = bg_gate
                .acquire_background(64)
                .await
                .expect("background must eventually acquire");
            bg_order.lock().unwrap().push("background");
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(holder);

        interactive.await.expect("interactive task panicked");
        background.await.expect("background task panicked");
        assert_eq!(
            *order.lock().unwrap(),
            vec!["interactive", "background"],
            "interactive waiter must win the released permit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn background_times_out_when_gate_never_frees() {
        let gate = LlmGate::new();
        let _held = gate.acquire_interactive().await;

        let err = gate
            .acquire_background(64)
            .await
            .expect_err("background must time out while the gate is held forever");
        assert!(err.to_string().contains("background call skipped"));
    }

    /// A cancelled interactive waiter must not leave `interactive_waiting`
    /// stuck — that would starve the background tier forever.
    #[tokio::test(start_paused = true)]
    async fn aborted_interactive_waiter_does_not_starve_background() {
        let gate = LlmGate::new();
        let (held, _) = gate.acquire_background(64).await.expect("free gate");

        let int_gate = gate.clone();
        let waiter = tokio::spawn(async move {
            let _permit = int_gate.acquire_interactive().await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        waiter.abort();
        let _ = waiter.await;

        drop(held);
        gate.acquire_background(64)
            .await
            .expect("background must acquire once the aborted waiter is gone");
    }
}
