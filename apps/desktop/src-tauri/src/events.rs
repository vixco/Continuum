//! Event bridge — forwards state changes and log events to the frontend.
//!
//! Topic layout (match `dashboard/lib/events.ts`):
//!
//! - `kairo:state`  — full snapshot (emitted whenever any sub-state changes)
//! - `kairo:log`    — single `LogEntry` per emission (live tail)
//! - `kairo:repair` — `RepairEvent` per emission (repair agent stream)

use tauri::{AppHandle, Emitter};

use kairo_core::logs::LogBuffer;
use kairo_core::state::StateHandle;

/// Debounce window for state snapshots. The state store fires one event
/// per mutation; many mutations can happen in fast bursts (perception +
/// triage + state-of-voice). We coalesce within a short window so the
/// frontend doesn't re-render dozens of times per second.
const STATE_DEBOUNCE_MS: u64 = 150;

pub fn bridge_state(state: StateHandle, app: AppHandle) {
    let mut rx = state.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(_) => {
                    // Drain any events that arrived while we slept, then
                    // emit one coalesced snapshot.
                    tokio::time::sleep(std::time::Duration::from_millis(
                        STATE_DEBOUNCE_MS,
                    ))
                    .await;
                    while rx.try_recv().is_ok() {}
                    let snap = state.snapshot().await;
                    let _ = app.emit("kairo:state", snap);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(
                        layer = "dashboard",
                        component = "events",
                        lagged = n,
                        "State subscriber lagged"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

pub fn bridge_logs(logs: LogBuffer, app: AppHandle) {
    let mut rx = logs.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(entry) => {
                    let _ = app.emit("kairo:log", entry);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
