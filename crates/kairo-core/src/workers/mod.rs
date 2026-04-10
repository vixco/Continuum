//! # Layer 4 — Workers
//!
//! Workers are independent Claude Code sessions spawned by the orchestrator
//! to perform actual work. Each worker gets its own working directory, tool
//! allowlist, model selection, session ID, and log file.
//!
//! The orchestrator does not spawn workers directly — it calls the
//! `mcp__kairo__workers__spawn_worker` MCP tool, which is handled by the
//! Kairo MCP server. This keeps the worker lifecycle management out of the
//! orchestrator's context.

pub mod pool;
pub mod supervisor;
