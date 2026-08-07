//! # Triage coalescer — busy flag + latest-wins slot
//!
//! Task B2 of the context engine (spec §4.7 "Triage off the main loop").
//! The runtime's main select loop no longer awaits triage inline: a gated
//! frame is handed to a spawned evaluation task and its result comes back
//! over a channel into its own select arm. This type is the pure
//! coordination logic for that pattern — the same CAS-claim shape as the
//! `orchestrator_busy` / `try_claim_busy` wake gate in `bin/continuum.rs`,
//! plus a one-deep latest-wins slot:
//!
//! - While no evaluation is in flight, [`TriageCoalescer::submit`] claims
//!   the busy flag and tells the caller to evaluate the item now.
//! - While one IS in flight, the item is parked in the slot, displacing any
//!   older parked item — a burst of N gated frames coalesces to the newest
//!   one, mirroring the perception pipeline's latest-wins discipline.
//! - When an evaluation completes, [`TriageCoalescer::complete`] either
//!   hands back the parked item (busy stays claimed; caller spawns the next
//!   evaluation immediately) or releases the busy flag.
//!
//! Both methods are called only from the main select loop's thread (the
//! frame arm submits, the triage-result arm completes), so the two calls
//! never race each other; the atomics/mutex exist so the type is still
//! sound if that ever changes, not because the loop needs them today.
//!
//! ## Stuck detection
//!
//! `complete()` is the *only* thing that releases the busy flag, and it
//! runs in a different task from the evaluation. If an evaluation task
//! ever dies without reporting (panic, cancellation), the flag would latch
//! and every later frame would be parked forever — silently. Two
//! mechanisms cover that: the evaluation task carries a
//! [`crate::guard::FallbackGuard`] that reports on unwind, and
//! [`TriageCoalescer::health`] exposes a busy-since timestamp so the
//! runtime's health snapshot (and therefore the repair agent) can see a
//! triage layer that has been "in flight" for minutes.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

/// Outcome of [`TriageCoalescer::submit`].
#[derive(Debug, PartialEq)]
pub enum Submitted<T> {
    /// The busy flag was free and is now claimed by this item — the caller
    /// must start an evaluation for it (and later call
    /// [`TriageCoalescer::complete`]).
    Evaluate(T),
    /// An evaluation is already in flight — the item was parked in the
    /// latest-wins slot. `replaced` is the older parked item it displaced
    /// (never evaluated; the caller may need to clean up bookkeeping keyed
    /// on it, e.g. the voice-supersede set in `bin/continuum.rs`).
    Coalesced {
        /// The previously parked item this submission displaced, if any.
        replaced: Option<T>,
    },
}

/// Shared, cloneable view of "is a triage evaluation in flight, and since
/// when?" — the health surface of [`TriageCoalescer`] (spec §7).
///
/// Held by the runtime's publish closure, which folds it into
/// `RuntimeSnapshot.context_engine.triage`: a busy-since older than a few
/// evaluation budgets means an evaluation never reported back and triage
/// is wedged, which is exactly the state the repair agent needs to see.
#[derive(Debug, Clone, Default)]
pub struct TriageBusyHandle {
    /// Unix ms at which the current evaluation claimed the flag; `0` when
    /// idle.
    since_ms: Arc<AtomicI64>,
}

impl TriageBusyHandle {
    /// When the in-flight evaluation claimed the busy flag; `None` when no
    /// evaluation is in flight.
    pub fn busy_since(&self) -> Option<DateTime<Utc>> {
        let ms = self.since_ms.load(Ordering::Acquire);
        if ms <= 0 {
            return None;
        }
        DateTime::from_timestamp_millis(ms)
    }

    /// How long the current evaluation has been in flight, if any.
    pub fn busy_for(&self, now: DateTime<Utc>) -> Option<chrono::Duration> {
        self.busy_since().map(|since| now - since)
    }

    /// `true` when an evaluation has been in flight longer than
    /// `threshold` — i.e. it almost certainly never reported back.
    pub fn is_stuck(&self, now: DateTime<Utc>, threshold: chrono::Duration) -> bool {
        self.busy_for(now)
            .is_some_and(|elapsed| elapsed > threshold)
    }

    fn mark_busy(&self, now: DateTime<Utc>) {
        // `max(1)` keeps 0 reserved as the idle sentinel.
        self.since_ms
            .store(now.timestamp_millis().max(1), Ordering::Release);
    }

    fn mark_idle(&self) {
        self.since_ms.store(0, Ordering::Release);
    }
}

/// Coalescing busy-gate for off-loop triage evaluation. See the module doc.
#[derive(Debug, Default)]
pub struct TriageCoalescer<T> {
    busy: AtomicBool,
    pending: Mutex<Option<T>>,
    busy_since: TriageBusyHandle,
}

impl<T> TriageCoalescer<T> {
    /// Creates an idle coalescer (busy flag free, empty slot).
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            pending: Mutex::new(None),
            busy_since: TriageBusyHandle::default(),
        }
    }

    /// Cloneable health view: how long the in-flight evaluation has been
    /// running. See [`TriageBusyHandle`].
    pub fn health(&self) -> TriageBusyHandle {
        self.busy_since.clone()
    }

    /// Offers an item for evaluation. Claims the busy flag when free
    /// (caller evaluates now); otherwise parks the item latest-wins.
    pub fn submit(&self, item: T) -> Submitted<T> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.busy_since.mark_busy(Utc::now());
            return Submitted::Evaluate(item);
        }
        let replaced = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(item);
        Submitted::Coalesced { replaced }
    }

    /// Marks the in-flight evaluation as finished. Returns the parked item
    /// when one is waiting — the busy flag then STAYS claimed and the
    /// caller must evaluate it (and call `complete` again after). Returns
    /// `None` when the slot is empty, releasing the busy flag.
    pub fn complete(&self) -> Option<T> {
        let next = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        match next.is_none() {
            // Nothing parked: the flag is released and triage is idle.
            true => {
                self.busy.store(false, Ordering::Release);
                self.busy_since.mark_idle();
            }
            // A parked item is handed back and starts evaluating now, so
            // the busy clock restarts rather than carrying the previous
            // evaluation's start time.
            false => self.busy_since.mark_busy(Utc::now()),
        }
        next
    }

    /// Whether an evaluation is currently claimed as in flight.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_submit_claims_and_evaluates() {
        let c = TriageCoalescer::new();
        assert!(!c.is_busy());
        assert_eq!(c.submit(1), Submitted::Evaluate(1));
        assert!(c.is_busy());
    }

    #[test]
    fn burst_coalesces_to_newest_only() {
        let c = TriageCoalescer::new();
        assert_eq!(c.submit(1), Submitted::Evaluate(1));
        // Burst while 1 is in flight: 2 parks, 3 displaces 2, 4 displaces 3.
        assert_eq!(c.submit(2), Submitted::Coalesced { replaced: None });
        assert_eq!(c.submit(3), Submitted::Coalesced { replaced: Some(2) });
        assert_eq!(c.submit(4), Submitted::Coalesced { replaced: Some(3) });
        // 1 finishes: only the newest (4) is handed back for evaluation.
        assert_eq!(c.complete(), Some(4));
        assert!(c.is_busy(), "busy stays claimed while the parked item runs");
        // 4 finishes with nothing parked: busy releases.
        assert_eq!(c.complete(), None);
        assert!(!c.is_busy());
    }

    #[test]
    fn complete_with_empty_slot_releases_busy_for_next_submit() {
        let c = TriageCoalescer::new();
        assert_eq!(c.submit("a"), Submitted::Evaluate("a"));
        assert_eq!(c.complete(), None);
        // Fully idle again: the next submission evaluates directly.
        assert_eq!(c.submit("b"), Submitted::Evaluate("b"));
    }

    #[test]
    fn busy_since_tracks_the_in_flight_evaluation() {
        let c = TriageCoalescer::new();
        let health = c.health();
        assert!(health.busy_since().is_none(), "idle at rest");

        let before = Utc::now();
        assert_eq!(c.submit(1), Submitted::Evaluate(1));
        let since = health.busy_since().expect("busy while evaluating");
        assert!(since >= before - chrono::Duration::seconds(1));

        assert_eq!(c.complete(), None);
        assert!(health.busy_since().is_none(), "idle again after complete");
    }

    #[test]
    fn busy_since_reports_a_wedged_evaluation() {
        let c = TriageCoalescer::new();
        let health = c.health();
        let far_future = Utc::now() + chrono::Duration::minutes(10);
        assert!(!health.is_stuck(far_future, chrono::Duration::seconds(30)));

        // An evaluation claims the flag and (the C1 failure) never
        // completes: ten minutes later the handle says so.
        assert_eq!(c.submit("frame"), Submitted::Evaluate("frame"));
        assert!(health.is_stuck(far_future, chrono::Duration::seconds(30)));
        assert!(health
            .busy_for(far_future)
            .is_some_and(|d| d >= chrono::Duration::minutes(9)));

        // Releasing clears the stuck state.
        assert_eq!(c.complete(), None);
        assert!(!health.is_stuck(far_future, chrono::Duration::seconds(30)));
    }

    #[test]
    fn busy_clock_restarts_when_a_parked_item_takes_over() {
        let c = TriageCoalescer::new();
        let health = c.health();
        assert_eq!(c.submit(1), Submitted::Evaluate(1));
        let first = health.busy_since().expect("busy");
        assert_eq!(c.submit(2), Submitted::Coalesced { replaced: None });
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert_eq!(c.complete(), Some(2));
        let second = health.busy_since().expect("still busy on the parked item");
        assert!(second >= first, "the busy clock restarts for item 2");
    }

    #[test]
    fn parked_item_chains_through_complete() {
        let c = TriageCoalescer::new();
        assert_eq!(c.submit(10), Submitted::Evaluate(10));
        assert_eq!(c.submit(11), Submitted::Coalesced { replaced: None });
        assert_eq!(c.complete(), Some(11));
        // While 11 runs, another frame parks.
        assert_eq!(c.submit(12), Submitted::Coalesced { replaced: None });
        assert_eq!(c.complete(), Some(12));
        assert_eq!(c.complete(), None);
        assert!(!c.is_busy());
    }
}
