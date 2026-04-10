//! # Layer 3 — Orchestrator
//!
//! The orchestrator is Claude Opus 4.6, invoked via the official Claude Code CLI
//! in headless mode. This is the only cloud component of Kairo.
//!
//! When the triage layer decides to wake the orchestrator, Kairo Core:
//! 1. Builds a wake context (current frame + memory recall + active project)
//! 2. Spawns `claude` as a child process with `--output-format stream-json`
//! 3. Writes a JSON user message to stdin
//! 4. Reads streamed events from stdout, piping text to TTS in real time
//! 5. Captures tool calls and delegates to workers as needed
//!
//! See [`events`] for the event parsing types and [`spawn`] for the process
//! management logic.

pub mod events;
pub mod spawn;
pub mod stream;
