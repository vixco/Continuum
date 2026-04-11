# Changelog

All notable changes to Kairo are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **Phase 2 — Triage layer complete**: local LLM evaluates salient perception frames and outputs structured decisions — 19/20 benchmark accuracy (95%) with Qwen 3 8B at 964ms P50 latency
- `kairo-llm` crate: wraps `llama-cpp-2` (llama.cpp Rust bindings) with LocalLlm struct — GGUF model loading, free-form generation, GBNF grammar-constrained JSON generation, streaming output, model warmup
- TriageDecision enum: 5 variants (ignore, remember, whisper, execute_simple, wake_orchestrator) with serde JSON parsing and truncation
- TriageLayer: evaluation loop with 3-retry fallback (grammar first, prompt-only retries, default to Ignore), consecutive failure health alerts
- Decision handlers: allowlisted execute_simple actions (launch_app, show_notification, toggle_mute), TTS and orchestrator wake placeholders
- GBNF grammar file (`prompts/triage-grammar.gbnf`) enforcing strict triage JSON schema
- Triage system prompt (`prompts/triage-system.md`) with signal reliability hierarchy and Qwen 3 `/no_think` thinking mode suppression
- `--triage` flag on `kairo-perception` binary: optional real-time triage decisions in terminal output
- `kairo-triage-bench` binary: benchmarks triage accuracy and latency against 20 hand-labeled frames
- Benchmark dataset: `benchmarks/triage-frames.jsonl` with 20 labeled frames (5 ignore, 5 remember, 5 wake, 5 ambiguous)
- Decision document: 004-triage-model.md (Qwen 3 4B chosen over Qwen 2.5 3B, Gemma 3, Phi-4, Llama 3.2)
- Triage documentation: `docs/triage.md` with model swapping, debugging, signal hierarchy
- Per-decision accuracy breakdown in benchmark harness

### Changed
- Default triage model upgraded from Qwen 2.5 3B to Qwen 3 8B (Q4_K_M) via Qwen 3 4B — best accuracy/latency balance for triage decisions
- Triage prompt calibrated: tightened REMEMBER rules to require audio evidence (eliminates over-remembering on interesting window titles), added WHISPER decision path, added proactive WAKE on visible errors with idle timeout
- Benchmark relabeled 2 frames based on decision-theoretic analysis: error-visible-10s from remember→wake, simple-calendar-question from wake→whisper
- Default salience threshold lowered from 0.15 to 0.10 — triage is cheap enough for window-change events
- Updated `ARCHITECTURE.md` Layer 2 section for Qwen 3 4B with thinking mode documentation
- Updated `config/default-models.toml` with new triage model config
- Updated `scripts/download-models.ps1` with Qwen 3 4B download

### Fixed
- SmolVLM decoder repetition loop — replaced greedy argmax with repetition-penalty sampling (rep_penalty=1.15, no_repeat_ngram=3, temperature=0.3, top_p=0.9) plus repetition safety net
- Triage `llama_context` recreated on every call — now cached and reused with KV cache clearing between evaluations
- Triage KV cache on CPU instead of GPU — `kairo-perception` was using `TriageConfig::default()` with `gpu_layers: 0`; now explicitly sets `gpu_layers: 999` matching the benchmark config
- `TriageConfig::default()` gpu_layers changed from 0 to 999 to prevent future GPU misconfiguration
- `foreground_process_name` always empty in perception output — replaced `GetModuleBaseNameW` (requires `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`) with `QueryFullProcessImageNameW` (works with `PROCESS_QUERY_LIMITED_INFORMATION`)

### Known limitations
- SmolVLM-256M vision model hallucinates on complex screens (browser windows, dense UI). Triage is designed to treat vision as corroborating evidence only; primary signals are foreground_process_name and audio transcript. Vision quality will improve in Phase 3 when orchestrator receives raw screenshots directly to Claude Opus.

- **Phase 1 — Perception layer**: full senses subsystem producing continuous PerceptionFrame stream
- `kairo-vision` crate: VisionModel trait with OnnxVisionModel — full autoregressive SmolVLM-256M decoder loop (vision encoder → token embedding → KV-cache decoder → tokenizer decode)
- Screen capture via `xcap` (GDI/BitBlt, no yellow border): primary monitor capture, 1280x720 downscaling, JPEG screenshot saving
- Audio pipeline (default-enabled): cpal mic capture, energy-based VAD, whisper-rs batch transcription, rubato resampling
- `.cargo/config.toml` with build environment variables (LIBCLANG_PATH, CMAKE_GENERATOR, ORT_DYLIB_PATH)
- End-to-end smoke test documentation (docs/phase-1-smoke-test.md)
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
