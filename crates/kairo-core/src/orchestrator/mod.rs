//! # Layer 3 — Orchestrator
//!
//! The orchestrator is Claude Opus 4.6, invoked via the official Claude Code CLI
//! in headless mode. This is the only cloud component of Kairo.
//!
//! ## Lifecycle (ADR 005: fresh process per wake)
//!
//! Each wake is independent — no conversation history carries between wakes.
//! Kairo's LanceDB episodic memory is the authoritative memory store.
//!
//! Per wake:
//! 1. Triage emits `WakeOrchestrator(reason)`
//! 2. Memory retrieval builds context (~100-200ms)
//! 3. Wake context builder produces user message (~400 tokens)
//! 4. Spawn fresh `claude -p` process
//! 5. Write user message to stdin, close stdin
//! 6. Read streamed events from stdout, pipe text deltas to terminal
//! 7. On `result` event: capture cost, full text, duration
//! 8. Process exits, store wake event + response in episodic memory
//!
//! See [`events`] for the event parsing types, [`spawn`] for the subprocess
//! logic, and [`wake_context`] for the message builder.

pub mod events;
pub mod spawn;
pub mod stream;
pub mod wake_context;

// Re-export the primary public types.
pub use spawn::{wake_orchestrator, OrchestratorConfig, OrchestratorEvent, WakeResult};
pub use stream::format_for_terminal;
