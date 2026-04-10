//! # Orchestrator process spawning
//!
//! Manages the lifecycle of Claude Code child processes for the orchestrator.
//! Handles spawning `claude` with the correct flags, writing JSON messages to
//! stdin, and setting up stdout/stderr readers for event streaming.
//!
//! The command template follows the pattern from CLAUDE.md:
//! ```text
//! claude --print --output-format stream-json --input-format stream-json
//!        --verbose --include-partial-messages --model claude-opus-4-6
//!        --append-system-prompt-file <prompt> --mcp-config <config>
//!        --allowedTools <tools>
//! ```
