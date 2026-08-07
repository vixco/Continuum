//! # Drop guards for spawned-task invariants
//!
//! Every long-lived flag in the runtime that is *claimed* by one task and
//! *released* by another has the same failure mode: if the claiming task
//! dies between the two — a panic inside a `tokio::spawn`, or the task
//! being dropped mid-await — the release never runs and the flag latches
//! forever. Two such flags exist in `bin/continuum.rs`:
//!
//! - `TriageCoalescer::busy` (released by the `triage_result_rx` arm when
//!   the evaluation task sends its result), and
//! - `orchestrator_busy` (released at the tail of the spawned wake task).
//!
//! A latched flag is silent: triage simply stops evaluating frames and the
//! orchestrator simply stops waking, with no error anywhere. [`FallbackGuard`]
//! is the mechanical fix — the release is registered as a drop action the
//! moment the task starts, so it runs on *every* exit path including an
//! unwind. Use [`FallbackGuard::disarm`] when the normal path already
//! performed the release and the fallback would double it.
//!
//! # Layer
//!
//! Infrastructure — used by the runtime binary's Layer 2 and Layer 3 spawn
//! sites. No layer of its own.
//!
//! # Example
//!
//! ```
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use std::sync::Arc;
//! use continuum_core::guard::FallbackGuard;
//!
//! let busy = Arc::new(AtomicBool::new(true));
//! {
//!     let flag = busy.clone();
//!     let _guard = FallbackGuard::new(move || flag.store(false, Ordering::Release));
//!     // ... work that may panic ...
//! }
//! assert!(!busy.load(Ordering::Acquire));
//! ```

/// Runs `action` when dropped, unless [`disarm`](Self::disarm)ed first.
///
/// The action runs on a normal scope exit **and** on an unwind, which is
/// the point: it is how a spawned task guarantees a shared flag is
/// released even when the task panics.
///
/// The action must not itself panic — a panic during unwind aborts the
/// process. Keep it to atomic stores, channel sends and `tracing` calls.
pub struct FallbackGuard<F: FnOnce()> {
    action: Option<F>,
}

impl<F: FnOnce()> FallbackGuard<F> {
    /// Arms a guard that will run `action` on drop.
    pub fn new(action: F) -> Self {
        Self {
            action: Some(action),
        }
    }

    /// Cancels the guard: the action will not run. Call this on the
    /// success path when the normal code already did the release.
    pub fn disarm(mut self) {
        self.action = None;
    }
}

impl<F: FnOnce()> Drop for FallbackGuard<F> {
    fn drop(&mut self) {
        if let Some(action) = self.action.take() {
            action();
        }
    }
}

impl<F: FnOnce()> std::fmt::Debug for FallbackGuard<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackGuard")
            .field("armed", &self.action.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn runs_on_normal_scope_exit() {
        let fired = Arc::new(AtomicBool::new(false));
        {
            let f = fired.clone();
            let _guard = FallbackGuard::new(move || f.store(true, Ordering::Release));
            assert!(!fired.load(Ordering::Acquire));
        }
        assert!(fired.load(Ordering::Acquire));
    }

    #[test]
    fn disarm_suppresses_the_action() {
        let fired = Arc::new(AtomicBool::new(false));
        {
            let f = fired.clone();
            let guard = FallbackGuard::new(move || f.store(true, Ordering::Release));
            guard.disarm();
        }
        assert!(!fired.load(Ordering::Acquire));
    }

    #[test]
    fn runs_exactly_once_on_unwind() {
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = FallbackGuard::new(move || {
                c.fetch_add(1, Ordering::AcqRel);
            });
            panic!("boom");
        }));
        assert!(result.is_err(), "the panic must propagate");
        assert_eq!(count.load(Ordering::Acquire), 1);
    }

    /// The C1/I4 shape: a panicking `tokio::spawn` body still releases the
    /// busy flag it claimed.
    #[tokio::test]
    async fn releases_a_busy_flag_when_a_spawned_task_panics() {
        let busy = Arc::new(AtomicBool::new(true));
        let flag = busy.clone();
        let joined = tokio::spawn(async move {
            let _guard = FallbackGuard::new(move || flag.store(false, Ordering::Release));
            panic!("triage eval blew up");
        })
        .await;
        assert!(joined.is_err(), "the task panicked");
        assert!(
            !busy.load(Ordering::Acquire),
            "the guard must have cleared the flag"
        );
    }

    /// The guard also fires when the task is *cancelled* (future dropped
    /// mid-await), which is the other way a release can be skipped.
    #[tokio::test]
    async fn releases_a_busy_flag_when_a_spawned_task_is_aborted() {
        let busy = Arc::new(AtomicBool::new(true));
        let flag = busy.clone();
        let handle = tokio::spawn(async move {
            let _guard = FallbackGuard::new(move || flag.store(false, Ordering::Release));
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        handle.abort();
        let _ = handle.await;
        assert!(!busy.load(Ordering::Acquire));
    }
}
