# Changelog

All notable changes to Kairo are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **Phase 1 — Perception layer**: full senses subsystem producing continuous PerceptionFrame stream
- `kairo-vision` crate: VisionModel trait with OnnxVisionModel (SmolVLM-256M via ort), image preprocessing pipeline, warmup support
- Screen capture via `xcap` (GDI/BitBlt, no yellow border): primary monitor capture, 1280x720 downscaling, JPEG screenshot saving
- Audio pipeline: cpal mic capture, energy-based VAD, whisper-rs batch transcription, rubato resampling (behind `audio` feature flag)
- Context poller: foreground window title/process via Windows APIs, idle time detection, call detection (Discord/Teams/Zoom/Meet/Slack)
- PerceptionFrameBuilder: assembles frames from three senses channels, computes salience heuristic (5 rules)
- SQLite raw log via sqlx: schema creation, write/query frames, nightly rotation with configurable retention
- `kairo-perception` binary: standalone perception runner with Ctrl+C graceful shutdown
- Shared observation types: ScreenObservation, AudioObservation, ContextObservation, PerceptionFrame
- KairoConfig with TOML loading from `~/.kairo-dev/config.toml`, sensible defaults for all senses
- Decision documents: 001-vision-model, 002-screen-capture, 003-audio-pipeline
- Updated ARCHITECTURE.md: SmolVLM-256M as default vision model, rate_limit_event documentation
- Updated download-models.ps1 with actual model download URLs
- 79+ unit and integration tests across kairo-vision and kairo-core
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
