//! # kairo-mcp
//!
//! The Kairo MCP server — exposes Windows-specific tools to Claude Code so the
//! orchestrator and workers can interact with the user's desktop.
//!
//! This binary runs as a standalone MCP server, registered with Claude Code via
//! the `--mcp-config` flag. It uses the `rmcp` crate for the MCP protocol.
//!
//! Tool namespaces: memory, perception, voice, windows, shell, workers, schedule, system.

mod tools;

fn main() {
    eprintln!("kairo-mcp: not yet implemented — scaffolding only");
}
