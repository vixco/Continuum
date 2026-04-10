//! # Layer 2 — Triage
//!
//! The triage layer is a small local LLM (3–4B parameters) that reads every
//! salient perception frame and decides what to do. It is the gatekeeper that
//! decides whether to spend money on Opus or not.
//!
//! Default model: Qwen 2.5 3B Instruct (Q4 quantization).
//!
//! Decisions:
//! - `ignore` — nothing worth doing, discard the frame
//! - `remember` — worth remembering but no action needed
//! - `whisper` — say a short sentence via local TTS (no orchestrator)
//! - `execute_simple` — perform a pre-approved simple action
//! - `wake_orchestrator` — wake Claude Opus for genuine reasoning

pub mod llm;
pub mod prompts;
