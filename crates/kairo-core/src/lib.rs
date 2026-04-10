//! # kairo-core
//!
//! The main orchestration runtime for Kairo — a desktop-native, local-first,
//! ambient AI assistant for Windows.
//!
//! This crate implements the four-layer cognitive architecture:
//!
//! - **Layer 1 — Senses**: Local vision (Moondream), audio (whisper.cpp), and
//!   context polling. Produces a continuous stream of [`PerceptionFrame`] objects.
//! - **Layer 2 — Triage**: A small local LLM (Qwen 2.5 3B default) that
//!   evaluates perception frames and decides what deserves attention.
//! - **Layer 3 — Orchestrator**: Claude Opus 4.6 via the official `claude` CLI
//!   in headless mode, woken only when genuine reasoning is needed.
//! - **Layer 4 — Workers**: Headless Claude Code sessions spawned by the
//!   orchestrator to perform actual work.
//!
//! Data flows upward from senses to orchestrator. Commands flow downward from
//! orchestrator to workers and tools. The triage layer acts as the gate.

pub mod config;
pub mod health;
pub mod memory;
pub mod orchestrator;
pub mod senses;
pub mod triage;
pub mod voice;
pub mod workers;
