//! # kairo-core
//!
//! The main orchestration runtime for Kairo — a desktop-native, local-first,
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
pub mod automations;
pub mod config;
pub mod health;
pub mod logs;
pub mod runtime;
pub mod runtime_publish;
pub mod senses;
pub mod state;
pub mod triage;

// Heavy runtime modules — gated on the `runtime` feature so the desktop
// crate can depend on kairo-core without pulling C++ build deps.
#[cfg(feature = "runtime")]
pub mod memory;
#[cfg(feature = "runtime")]
pub mod orchestrator;
#[cfg(feature = "runtime")]
pub mod voice;
#[cfg(feature = "runtime")]
pub mod workers;
