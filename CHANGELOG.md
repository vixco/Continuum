# Changelog

All notable changes to Kairo are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added
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
