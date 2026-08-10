//! # Context engine
//!
//! Cross-layer context state that turns raw perception into a coherent
//! "what is the user doing" picture (context engine spec §3/§4). Unlike the
//! senses watchers, everything in this module is **pure logic + shared
//! state** — no polling, no models — and is deliberately **not gated behind
//! the `runtime` feature: the runtime, the MCP server, and the desktop app
//! all link it (the `--no-default-features` build proves it).
//!
//! Modules land task by task (Plan A):
//! - [`project`] — the project resolver, the single source of truth for
//!   "current project" (spec §4.3, Task A4)
//! - [`session_state`] — session-state tracker (spec §4.8, Task B5)
//! - [`temporal`] — bounded historical session synthesis with explicit
//!   provenance, uncertainty and privacy scope
//! - [`package`] — context package: one struct, three consumer profiles
//!   (spec §4.9, Task B7)
//! - [`continuation`] — continuation resolver (spec §4.12, Task B8)
//! - [`intents`] — the Context-page intent-file protocol (spec §4.13,
//!   Task C5); ungated so the dashboard can write what the runtime drains
//! - [`apply`] — the runtime-side effects of those intents (runtime-gated:
//!   it touches the raw log, the vault and the episodic store)

pub mod continuation;
pub mod intents;
pub mod package;
pub mod project;
pub mod sensitivity;
pub mod session_state;
pub mod temporal;

#[cfg(feature = "runtime")]
pub mod apply;
