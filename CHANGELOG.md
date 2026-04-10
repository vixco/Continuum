# Changelog

All notable changes to Kairo are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added
- Phase 0 Hello World: example binary that spawns Claude Code CLI, streams JSON events, and prints live text output (`crates/kairo-core/examples/hello_world.rs`)
- Strongly-typed Claude Code event parser in `crates/kairo-core/src/orchestrator/events.rs` with full coverage of system, stream_event, assistant, user, rate_limit_event, and result event types
- Unit tests for event parser using real JSON captured from Claude Code CLI v2.1.100
- Updated CLAUDE.md event type documentation to match actual CLI behavior (discovered `rate_limit_event`, `total_cost_usd` field name, detailed `system` init fields)
- Initial repository scaffolding
- Architecture, soul, roadmap, and Claude Code instructions
- Cargo workspace with kairo-core, kairo-mcp, kairo-llm, kairo-vision crates
- pnpm workspace with desktop app
- Tauri 2 desktop app skeleton with Next.js 15 frontend
- Full module tree for kairo-core matching the four-layer architecture
- MCP server skeleton with all tool namespace modules
- Prompt templates for triage, orchestrator, repair agent, and salience heuristics
- Default config files for models, permissions, and MCP servers
- Bundled skill placeholders (daily-briefing, code-review, project-context)
- Dev setup, model download, and install PowerShell scripts
- CI workflow for Rust and Next.js builds
- Apache 2.0 license
- Contributing guidelines
