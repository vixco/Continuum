//! # continuum-core
//!
//! The main orchestration runtime for Continuum — a desktop-native, local-first,
//! ambient AI assistant for Windows.
//!
//! This crate implements the four-layer cognitive architecture:
//!
//! - **Layer 1 — Senses**: Local vision (SmolVLM-256M), audio (whisper.cpp), and
//!   context polling. Produces a continuous stream of [`senses::types::PerceptionFrame`] objects.
//! - **Layer 2 — Triage**: A small local LLM (Qwen 2.5 3B default) that
//!   evaluates perception frames and decides what deserves attention.
//! - **Layer 3 — Orchestrator**: Claude Opus 4.6 via the official `claude` CLI
//!   in headless mode, woken only when genuine reasoning is needed.
//! - **Layer 4 — Workers**: Headless Claude Code sessions spawned by the
//!   orchestrator to perform actual work.
//!
//! Data flows upward from senses to orchestrator. Commands flow downward from
//! orchestrator to workers and tools. The triage layer acts as the gate.

// Always available — these modules power the desktop dashboard and don't
// need llama-cpp / whisper / lancedb.
pub mod audit;
pub mod automations;
pub mod config;
pub mod config_edit;
pub mod context;
pub mod curator;
pub mod github_cli;
pub mod guard;
pub mod hardware;
pub mod health;
pub mod llm_gate;
pub mod logs;
pub mod mcp_registry;
pub mod permissions;
pub mod runtime;
pub mod runtime_publish;
pub mod senses;
pub mod skills;
pub mod state;
pub mod triage;
pub mod workers;

// Heavy runtime modules — gated on the `runtime` feature so the desktop
// crate can depend on continuum-core without pulling C++ build deps.
/// Evaluation harness for the context-engine benches (spec §9): the
/// `--record` JSONL format, the committed synthetic fixture, the fake-clock
/// replay, and the shared metrics. Gated with `runtime` because it drives
/// the events writer and the raw log.
#[cfg(feature = "runtime")]
pub mod bench;
#[cfg(feature = "runtime")]
pub mod memory;
#[cfg(feature = "runtime")]
pub mod orchestrator;
// `voice` itself is always compiled — the intent submodule is pure
// serde/std and the desktop crate calls into it. Heavy submodules
// (TTS, STT, playback, etc.) are gated inside `voice/mod.rs`.
pub mod voice;
