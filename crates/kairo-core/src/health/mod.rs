//! # Health and self-healing
//!
//! Every Kairo component exposes a health check and can be restarted by
//! the repair agent. The health module provides:
//!
//! - Component status monitoring via the `system_health` MCP tool
//! - The repair agent spawning logic (a dedicated Claude Code session)
//! - Nightly backup rotation and self-diagnosis routines
//!
//! See [`repair`] for the repair agent implementation.

pub mod repair;
