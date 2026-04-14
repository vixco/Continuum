# Changelog

All notable changes to Kairo are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added — Phase 8 workers + skills

- **Worker pool** (`crates/kairo-core/src/workers/pool.rs`): queue with priority ordering (user_requested > orchestrator_spawned > scheduled), concurrency cap (`max_concurrent`, default 3, max 10), failure-streak refusal, per-worker snapshot publishing, and dashboard/MCP/audit hooks. Cancellation signals propagate from queued and running workers; pool shutdown gracefully cancels everything.
- **Worker supervisor** (`crates/kairo-core/src/workers/supervisor.rs`): spawns a fresh `claude --print --output-format stream-json` subprocess per worker, streams events (`SessionReady`, `TextDelta`, `ToolCall`, `Progress`, `Log`, `Finished`), enforces wall-clock timeouts with `tokio::time::timeout_at`, and returns a terminal `WorkerOutcome`. A dry-run mode (`KAIRO_WORKER_DRY_RUN=1`) synthesises a transcript for tests + the `worker_demo` example.
- **Worker types** (`crates/kairo-core/src/workers/types.rs`): `WorkerSpec`, `WorkerSnapshot`, `WorkerPriority`, `WorkerModelTier`, `WorkerStatus`, `WorkerPoolStats`, `WorkerOutcome`. All serde + non-runtime-gated so the dashboard can read them without llama-cpp.
- **Intent file protocol** (`crates/kairo-core/src/workers/intent.rs`): MCP writes JSON intents to `~/.kairo-dev/worker-intents/`; kairo-core drains, processes, and writes per-worker snapshots to `~/.kairo-dev/workers/<id>.json` atomically (`.tmp` + rename). Malformed intents are renamed to `.bad` so the loop never starves.
- **Model selection heuristic** (`crates/kairo-core/src/workers/model_select.rs`): Auto mode picks Opus for refactor/architect/debug-complex/migration work and Sonnet for rename/format/summary/boilerplate; tie goes to Opus. Explicit `"power"`/`"budget"`/`"claude-*"` tiers override; config `mode = "budget"|"power"` beats everything. Every choice is logged with a one-line reason in the worker snapshot.
- **Worker MCP tools** (`crates/kairo-mcp/src/tools/workers.rs`): `workers_spawn_worker`, `workers_worker_status`, `workers_worker_cancel`, `workers_worker_wait`, `workers_worker_list` — all registered in `KairoMcpServer` under the `mcp__kairo__workers_*` namespace, with full audit coverage.
- **Skills module** (`crates/kairo-core/src/skills/`):
  - `frontmatter.rs`: hand-rolled YAML parser for the narrow skill frontmatter (`name`, `description`, `triggers`, `source`, `manual_only`), tolerant of CRLF, inline and list trigger styles, unknown keys.
  - `loader.rs`: `SkillLoader` scans `skills/`, parses each `SKILL.md`, caches by name, hot-reloads on `mtime` change, surfaces parse errors for the dashboard.
  - `matcher.rs`: `SkillMatcher` scores skills by trigger substring hits against a `MatchContext` (wake reason, task, project, audio, foreground app, tags, forced). Multi-match with a token budget; forced skills bypass the budget and rank first.
  - `installer.rs`: `create_skill`, `save_skill`, `delete_skill` with name validation (`[a-zA-Z0-9_-]`) and safe frontmatter serialisation.
- **Bundled skills**: replaced placeholders with five real skills — `daily-briefing`, `code-review`, `project-context`, `email-draft`, `file-organizer`. Each has concrete procedure, output format, and refusal rules.
- **Orchestrator prompt injection** (`crates/kairo-core/src/bin/kairo.rs::compose_wake_config`): on each wake, matched skills are appended to the static orchestrator prompt and written to `~/.kairo-dev/orchestrator-dynamic.md`, which the spawned claude process receives via `--append-system-prompt-file`.
- **Worker prompt injection**: the pool materialises `<data_dir>/worker-prompts/<id>.md` per worker, combining `prompts/worker-system.md` + task-matched skill content, before launching the supervisor.
- **Triage suggested_skill hint**: `TriageDecision::WakeOrchestrator` grew an optional `suggested_skill` field (serde-skipped when absent, GBNF grammar updated); triage prompt lists available skill names and instructs the layer to tag wakes when a skill clearly applies.
- **Audit trail**: pool event + finish sinks write to episodic memory — per tool-call events tagged `worker` + `worker:<id>` + tool name (importance 0.4), per terminal-state summary events tagged with the task, skills, and outcome (importance 0.5 completed / 0.7 failed).
- **Dashboard**:
  - Tools tab: real skill CRUD — list with source badges, enable/disable toggle persisted to `skills.disabled`, create/edit modal with Markdown body, delete with confirmation, install-from-URL via `git clone --depth 1` + validation.
  - Home tab: live workers panel polling `list_workers` every 750 ms, status dots, progress bars, cost readout, click-through detail modal with full live output + model-choice reason + cancel/dismiss actions.
- **Health probes**: new `workers` and `skills` components registered in `components.rs`. `workers` surfaces recent failures and flags 3+ failures in 10 minutes as error. `skills` fails when any `SKILL.md` fails to parse and degrades when zero skills are loaded.
- **Examples**: `examples/worker_demo.rs` (end-to-end dry-run spawn + wait + report) and `examples/skill_match_demo.rs` (load skills + print matches for a wake reason given as CLI arg).
- **Tests**: 27 skills unit tests + 21 workers unit tests + 6 MCP workers-tool tests + 8 skills integration tests + 5 workers integration tests (intent → snapshot e2e, cancel of queued worker, priority ordering, failure-streak probe, spawn latency).
- **Docs**: `docs/workers.md`, `docs/skills.md`, Phase 8 section appended to `docs/mcp-tools.md`, roadmap Phase 8 box ticked.

### Changed — Phase 8
- `KairoConfig` grew `workers: WorkersConfig` and `skills: SkillsConfig` blocks; defaults wired through `default-models.toml`-compatible TOML.
- `TriageDecision::WakeOrchestrator` gains `suggested_skill: Option<String>` (serde-skipped when None — backwards-compatible on the wire).
- `prompts/triage-grammar.gbnf`: `wake_tail` production allows the optional `suggested_skill` field.
- `prompts/orchestrator-system.md`: added Workers + Skills sections with spawn rules and best practices.
- `prompts/triage-system.md`: lists the five bundled skill names with example triggers so triage can suggest them.
- `prompts/worker-system.md` (new): base worker behaviour prompt — one-task scope, narrow tools, structured report format.
- `crates/kairo-core/src/lib.rs`: `workers` and `skills` are now always-on modules; pool + supervisor + model_select remain gated on `runtime` so the Tauri build stays light.
- `apps/desktop/src-tauri/src/commands.rs`: added 9 new Tauri commands (`list_skills`, `save_skill`, `delete_skill`, `toggle_skill`, `install_skill_from_url`, `list_workers`, `get_worker`, `cancel_worker`, `dismiss_worker`).

### Added — Phase 6 dashboard + self-healing
- **Runtime state store**: `crates/kairo-core/src/state.rs` — single `KairoState` snapshot of perception, triage, orchestrator, workers, voice, memory, health, system, plus a 50-entry recent-actions ring. Typed update helpers on `StateHandle` publish to a tokio broadcast channel; the dashboard subscribes and re-emits coalesced snapshots to the frontend over Tauri's `emit`.
- **Log ring buffer**: `crates/kairo-core/src/logs.rs` — `BufferLayer` is a `tracing::Layer` that captures every event into a 10 000-entry ring and a tokio broadcast channel. Exposes `LogFilter` (level / layer / component / text / since) and a live subscribe API. The Logs tab reads from it; the Repair agent includes the last 500 lines in its context.
- **Health registry**: `crates/kairo-core/src/health/mod.rs` — pluggable `HealthCheck` trait with a polling `spawn_poller`. Registers 8 default probes (vision, triage, orchestrator, tts, stt, memory, mcp, context_watcher) in `apps/desktop/src-tauri/src/components.rs`. Rolling stats flag a component as `degrading` when the 24 h error rate across the last 20 probes > 5 %.
- **Backup rotation**: `crates/kairo-core/src/health/backup.rs` — nightly 04:00 zip of `config.toml` / `automations.json` / `permissions.toml` / `semantic.sqlite` / `orchestrator-system.md` to `~/.kairo-backups/<date>/`, keeping the 7 most recent. Exposes `run_backup`, `prune_backups`, `count_backups`, `latest_backup_ts`, `spawn_nightly`.
- **Repair agent**: `crates/kairo-core/src/health/repair.rs` — spawns Claude Opus 4.6 as a headless subprocess with the repo root as cwd, a repair-context file at `~/.kairo-dev/repair-context.md`, and streams events back as `RepairEvent` variants (assistant deltas, tool calls, tool results, stderr, final status). Also exposes `rollback_config(date)` that extracts `config.toml` from a dated backup zip.
- **Repair MCP tools**: `crates/kairo-mcp/src/tools/repair.rs` registers 5 new tools under `mcp__kairo__repair_*` — `restart_component`, `reinstall_model`, `rollback_config`, `test_component`, `escalate`. They write intent files to `~/.kairo-dev/repair-intents/` that the runtime drains on its tick.
- **Repair agent system prompt**: `prompts/repair-agent-system.md` — concrete operating rules, component → log-path map, concise output style ending in `RESOLVED` / `ESCALATED` / `PARTIAL`.
- **Automations store**: `crates/kairo-core/src/automations.rs` — JSON-backed list of one-shot / recurring tasks, full CRUD + toggle, atomic writes via `.tmp` + rename.
- **Embeddable runtime**: `crates/kairo-core/src/runtime.rs` — `KairoRuntime::init()` opens config, automations, state, log buffer, shutdown watch channel. Typed setters for `paused`, `voice_muted`, and a `update_config(|cfg| …)` helper that persists.
- **Runtime publisher**: `crates/kairo-core/src/runtime_publish.rs` — `RuntimeSnapshot` + `spawn_publisher` writes `~/.kairo-dev/state.json` every 2 s so the separate dashboard process can read live runtime flags without needing an IPC channel.
- **Feature-gated kairo-core**: the heavy runtime modules (`memory`, `orchestrator`, `voice`, `workers`, plus the watchers in `senses/*` except `types.rs`, plus the llm-backed parts of `triage`) are now behind the `runtime` feature so the dashboard builds without llama-cpp / whisper / lancedb. `kairo.exe` keeps the feature on by default; `kairo-desktop` sets `default-features = false`.
- **Tauri 2 desktop app**: `apps/desktop/src-tauri/` now has full backend: `commands.rs` (26 Tauri commands covering config, memory, automations, health, repair, window control), `events.rs` (state + logs + repair event bridge), `tray.rs` (system tray with state-based icon, right-click menu), `components.rs` (default health probes), `runtime_bridge.rs` (reads `state.json` every 2 s).
- **Dashboard UI** (`apps/desktop/src/`): Tailwind dark palette, Zustand store that hydrates from Tauri + subscribes to `kairo:state` / `kairo:log` / `kairo:repair`, 16 reusable UI primitives (`Card`, `StatusBadge`, `Button`, `Toggle`, `Slider`, `Select`, `SearchInput`, `TextInput`, `Modal`, `StatusOrb`, `Kbd`, `EmptyState`), icon sidebar + topbar with clock + pause/mute controls, and 8 tabs (Home, Brain, Memory, Tools, Voice, Automations, Logs, Health).
- **System tray**: left-click shows window, right-click menu offers Open / Pause / Resume / Voice on / Voice off / Quit; tooltip reflects state. Window close is intercepted and hides to tray.
- `docs/dashboard.md`: full architecture overview, two-process diagram, event topics, tab map, data file list.
- `docs/self-healing.md`: expanded with repair agent overview, MCP tool reference, backup/rotation/predictive-maintenance sections.

### Changed
- `crates/kairo-core/Cargo.toml`: `runtime` feature gate added; `parking_lot`, `sysinfo`, `zip` added as always-on deps. Binaries (`kairo`, `kairo-perception`, `kairo-triage-bench`, `audio-probe`) declare `required-features = ["runtime"]`.
- `crates/kairo-core/src/lib.rs`: module declarations split between always-on (state / logs / config / health / runtime / senses::types / triage::TriageDecision / automations) and runtime-only (memory / orchestrator / voice / workers / senses watchers / triage llm).
- `crates/kairo-core/src/bin/kairo.rs`: spawns the runtime publisher after subsystem init and updates `wake_count` / `voice_mode` on wake start/finish so the dashboard can render live runtime status.
- `apps/desktop/src-tauri/tauri.conf.json`: window label `main`, title "Kairo Dashboard", min-size 900×600, starts hidden (tray click reveals), tray icon id `kairo-tray`, version bumped to 0.4.0.
- `apps/desktop/src-tauri/capabilities/default.json`: Tauri 2 capabilities for window lifecycle, events, tray, shell, opener.
- `apps/desktop/package.json`: added `@tauri-apps/api`, `@tauri-apps/plugin-opener`, `@tauri-apps/plugin-shell`, `@tauri-apps/plugin-window-state`, `clsx`, `lucide-react`, `zustand`; bumped version to 0.4.0.
- `config::AudioConfig::default`: the test for `whisper_language` now correctly expects `"en"` (the wake-gate-friendly default) — an old `"auto"` assertion was stale.

### Changed
- **Voice output is now English-only by default**: the Dutch Piper voice (`nl_NL-mls-medium`) ships barely-intelligible speech, so the default `TtsConfig` no longer loads it and `voice.language_detection_enabled` defaults to `false`. Whisper input is `whisper_language = "auto"` so the user can still speak any language Kairo understands — Kairo just always responds through the English voice
- `prompts/orchestrator-system.md`: replaced "match the user's language, default to Dutch" with "always respond in English regardless of the user's spoken language"; explicit override for single turns if the user asks
- `prompts/triage-system.md`: whisper text MUST be English regardless of input language; the calendar example response translated to English
- `SOUL.md` Language section: Kairo *understands* any language whisper covers but *responds* in English until better multilingual voices exist; not a values statement, just a current TTS-quality constraint
- `config/default-models.toml`: Dutch voice entry commented out with a one-block opt-in path; `audio.whisper_language = "auto"`; `voice.language_detection_enabled = false`; explanatory block at top of `[tts]` documenting the strategy and how to re-enable multilingual output later
- `examples/voice_test.rs`: only synthesises phrases whose language is in the configured voice bank; skips others with a clear "no voice configured" message instead of routing Dutch text through the English voice

### Added
- **Phase 5 completion (v0.3.0-phase5)**: full voice-pipeline acceptance — TTS foundation (5A), wake + streaming STT (5B), streaming TTS + interrupt + polish (5C) landed together
- `crates/kairo-core/examples/voice_test.rs`: Phase 5A acceptance gate — loads the Piper voice bank, synthesises Dutch + English, plays through the default cpal output, prints per-language timing
- `crates/kairo-core/examples/voice_demo.rs`: Phase 5C end-to-end demo — typed transcripts drive wake → endpoint → streaming TTS → follow-up mode, with latency report
- `crates/kairo-core/examples/voice_latency_bench.rs`: Phase 5C benchmark harness — measures wake / endpoint / synth / playback-start / full-pipeline latency against ARCHITECTURE.md P95 targets over N iterations
- `crates/kairo-core/src/voice/sounds.rs`: procedurally-generated feedback cues (wake chime 880→1320 Hz ramp, listen click 1200 Hz, done double-click 660 Hz, error double-beep 220→165 Hz) with a `FeedbackPlayer` wrapper that no-ops when disabled or when no playback stream is attached
- `crates/kairo-core/src/voice/health.rs`: voice-component health probes (`tts_health_from_paths`, `stt_health_from_paths`, `wake_health`, `playback_health`) and a `VoiceHealthReport` aggregator that surfaces the worst status for the Phase 7 repair agent
- `crates/kairo-core/src/voice/hotkey.rs` (Windows): global hotkey listener via `RegisterHotKey` on a dedicated thread, parses `"Ctrl+Shift+K"`-style chord specs, delivers press events on a tokio `UnboundedReceiver<()>`, unregisters cleanly on drop
- `crates/kairo-core/src/voice/tts.rs::ElevenLabsEngine`: config-stable extension point for the future cloud TTS plugin — implements `TtsEngine` but returns a clear "Phase 5 extension point" error when called; `tts.engine = "elevenlabs"` logs a warning and falls back to Piper
- `resolve_piper_binary()` in `voice::tts`: Piper binary lookup now falls through `KAIRO_PIPER_BIN` env → `~/.kairo-dev/bin/piper/piper.exe` (Windows) / `~/.kairo-dev/bin/piper/piper` (Unix) → system PATH, so the download-models script makes things work without extra env setup
- `PlaybackStream::open_default_with_volume` + `set_volume`/`volume`: master gain applied in the cpal fill callback via an `AtomicU32` bits-of-f32, clamped to `[0.0, 1.0]`, `NaN`/`±∞` coerced to `0.0`
- Conversation follow-up mode: `bin/kairo.rs` opens a `followup_until` window after each orchestrator wake; fresh speech inside the window starts a session without re-requiring the wake phrase, then falls back to passive mode automatically
- Hotkey push-to-talk wiring in `bin/kairo.rs`: pressing the configured chord from anywhere flips `hotkey_pending`; the next transcript starts a session directly (skipping the wake phrase)
- Feedback cues wired into the main runtime: wake chime on wake-phrase match, listen click on follow-up/hotkey session start, error beep when `do_wake` fails
- `docs/voice.md` rewritten as a comprehensive reference: full pipeline diagram, every config option, latency budget table with P95 targets, troubleshooting guide, architectural rationale (Piper subprocess vs piper-rs, transcript wake vs Porcupine, heuristic endpoint vs LLM, sentence streaming vs token streaming), extension paths for new voices / custom wake / ElevenLabs / feedback cues

### Changed
- `config/default-models.toml` and `config::VoiceConfig`: added `volume`, `feedback_sounds`, `hotkey`, `conversation_followup_seconds` to `[voice]`; added `engine` and new `[tts.elevenlabs]` section to `[tts]`
- `scripts/download-models.ps1`: replaced the broken rhasspy/espeak-ng-data download (404'd repo) with the official `piper_windows_amd64.zip` release — installs `piper.exe` under `~/.kairo-dev/bin/piper/`, copies the bundled `espeak-ng-data/` to `~/.kairo-dev/models/tts/espeak-ng-data/`, and verifies the Piper binary in the final check
- `voice::tts::PiperEngine`: uses `resolve_piper_binary()` instead of hardcoding `"piper"` as the PATH fallback
- `voice::sounds::FeedbackPlayer`: added `::disabled()` constructor for headless/no-audio paths; the internal `playback` is now `Option<Arc<PlaybackStream>>` so we don't need to open a dummy cpal stream under `--no-tts`
- `bin/kairo.rs`: TTS init is now `init_tts_and_feedback` returning `(Option<Arc<SpeechController>>, FeedbackPlayer)`, so the same cpal output drives both utterances and UI cues
- `PlaybackStream::open_default` now delegates to `open_default_with_volume(1.0)` to preserve the existing API surface
- `voice::mod.rs`: added `pub mod sounds`, `pub mod health`, and gated `pub mod hotkey` behind `#[cfg(windows)]`

### Fixed
- `download-models.ps1` depended on `github.com/rhasspy/espeak-ng-data`, which is a 404. The new script uses the espeak-ng-data already bundled in the Piper Windows release, which is the upstream-recommended path

- **Phase 5 local voice path**: wake phrase detection over local Whisper transcripts, post-wake voice sessions, endpoint detection, Piper CLI TTS, cpal playback, streaming sentence-level speech, barge-in interruption, quiet mode during calls, and voice/self-healing docs
- **Phase 3 memory distillation completion**: background distiller promotes qualifying raw perception frames into LanceDB episodic `remember` events every 15 minutes and marks frames with `memory_distilled_at` after successful insert
- Voice configuration (`[voice]`) for wake keyword, timeout, endpoint silence, barge-in, ambient mute, and language routing; memory distillation configuration (`[memory]`) for interval, lookback, salience threshold, and batch size
- `docs/voice.md` and `docs/self-healing.md` document the Phase 5 local voice flow and repair-agent recovery procedures
- **Phase 4 — MCP tools**: Kairo's orchestrator can now do things, not just talk — a standalone `kairo-mcp` binary exposes 11 Rust-native tools to Claude Opus at wake time via `--mcp-config`
- `kairo-mcp` binary (rmcp 1.4, stdio transport, `--version` flag): registered on every wake with `--strict-mcp-config`, advertises protocol `V_2024_11_05` with `enable_tools()` capabilities
- Memory tools (`mcp__kairo__memory_*`): `query_episodic` (vector search via existing LanceDB), `list_facts` (prefix filter), `get_fact`, `set_fact` (rejects `system.*` and `kairo.*` prefixes; confidence clamped by source — inferred ≤0.7, observed ≤0.8, user_stated ≤0.9)
- System tools (`mcp__kairo__system_*`): `current_time` (ISO-8601 + tz offset), `active_window` (reuses `senses::context::foreground_window`), `clipboard_get` (Win32 OpenClipboard/CF_UNICODETEXT), `notification` (Windows toast via `tauri-winrt-notification`, 10s per-process rate limit, title/body truncated at 64/200 chars)
- Filesystem tools (`mcp__kairo__fs_*`): `read_file` (100 KB cap with truncation prefix, UTF-8 only), `list_dir` (500 entries, per-entry allowlist filtering); read-only by design — no writes, deletes, moves, or mutations
- Filesystem allowlist (`crates/kairo-mcp/src/allowlist.rs`): single `is_path_allowed` gatekeeper — root check (data dir + `project.*.dir` semantic facts + `[mcp.fs].extra_paths` opt-in), hardcoded `DENY_DIRS` (`.ssh`, `.aws`, `.gnupg`, `.docker`, `User Data`, `Profiles`, `node_modules`, `target`, `AppData`, etc.), hardcoded `DENY_PATTERNS` (`*.pem`, `*.key`, `id_rsa*`, `.env*`, `*.kdbx`, etc.)
- Web tool (`mcp__kairo__web_fetch`): HTTP GET only, 50 KB streaming cap with truncation prefix, pre-flight DNS resolution with public-IP check (RFC 1918, loopback, link-local, multicast, CGNAT 100.64/10, RFC 6598, IPv6 ULA + link-local all rejected), redirects disabled entirely to close redirect-SSRF, 5s total timeout
- Tool-call audit: every MCP invocation fires a background tokio task that writes an episodic event with `kind=ToolCall`, sanitized args (keys matching `/password|secret|token|apikey|auth/i` redacted, strings >500 chars truncated), and ≤200-char result summary — fire-and-forget so lazy `EpisodicStore` init doesn't block tool responses
- `EventKind::ToolCall` variant added to `crates/kairo-core/src/memory/episodic.rs`
- MCP orchestrator wiring (`crates/kairo-core/src/orchestrator/spawn.rs`): generates `mcp-config.json` at wake time (absolute binary path + `KAIRO_DATA_DIR` env), adds `--mcp-config` + `--strict-mcp-config`, changes `allowedTools` from `""` to `"mcp__kairo__*"`, flips `--permission-mode` from `plan` to `default` (plan mode blocks tool execution)
- `OrchestratorConfig` fields: `mcp_enabled: bool`, `mcp_server_path: Option<PathBuf>`, `mcp_config_path: Option<PathBuf>`, `mcp_data_dir: Option<PathBuf>`; binary resolver falls back through config → `KAIRO_MCP_BIN` env → sibling of current exe → PATH lookup
- Orchestrator system prompt (`prompts/orchestrator-system.md`): added Tools section with memory-first, read-only-fs, public-only-web, and no-notification-spam guidance; explicit warning about reserved `system.*`/`kairo.*` memory keys
- MCP config (`config/default-models.toml`): new `[mcp.fs]` section with `extra_paths = []` for user-controlled allowlist expansion
- `docs/mcp-tools.md`: complete tool reference with JSON examples, security model documentation, and E2E verification runbook
- Protocol integration test (`crates/kairo-mcp/tests/protocol.rs`): spawns the binary, drives JSON-RPC initialize → tools/list → tools/call over stdio, asserts all 11 tools registered and `system_current_time` returns a valid ISO-8601 timestamp
- 50 unit tests across audit, allowlist, config, memory, system, fs, web modules
- Echo smoke-test example (`crates/kairo-mcp/examples/echo_smoke.rs`): retained as diagnostic tool for verifying rmcp ↔ claude CLI handshake independently
- End-to-end verified: real `kairo-mcp.exe` spawned by real `claude -p` successfully answered `system_current_time` during smoke test (returned `2026-04-12T20:47:01.698257+02:00`)

### Changed
- `crates/kairo-core/src/senses/context.rs`: added `pub fn foreground_window()` wrapping the internal Win32 helper so `kairo-mcp`'s `system_active_window` tool can reuse the existing implementation
- `crates/kairo-core/src/bin/kairo.rs`: `OrchestratorConfig` initializer now includes `mcp_enabled: true` and passes `~/.kairo-dev/` as `mcp_data_dir`
- `crates/kairo-llm/src/lib.rs`: added explicit type annotations on two `std::mem::transmute` calls for clippy `missing_transmute_annotations` lint
- `crates/kairo-core/src/memory/episodic.rs`: `Embedder::embed_batch` now takes `Vec<String>` by value to avoid clippy `unnecessary_to_owned`

### Added
- **Phase 3 — Orchestrator**: Claude Opus 4.6 wakes up, speaks, and remembers
- Orchestrator subprocess manager: spawns fresh `claude -p` process per wake, streams response events, captures cost/duration (ADR 005: fresh process per wake — conversation purity over process reuse)
- Episodic memory: LanceDB vector store with fastembed BGESmallENV15Q (384-dim, 66 MB) for semantic similarity search over past events
- Semantic memory: SQLite store for stable facts about the user, projects, and preferences with key-value + graph edges
- Memory retrieval: combines episodic vector search + semantic fact lookup into a single MemoryContext for each wake
- Wake context builder: assembles orchestrator user message from current frame, history, memories, and wake reason (~400 tokens)
- Compact orchestrator system prompt (`prompts/orchestrator-system.md`): ~400 tokens, derived from SOUL.md, with Kairo personality, behavior rules, language detection, and Phase 3 guardrails
- `kairo` binary: complete runtime with perception + triage + orchestrator in one process
- Integration test with mock Claude Code event stream (no API key required)
- Decision document: 005-orchestrator-lifecycle.md

### Changed
- `orchestrator/spawn.rs`: rewritten from placeholder to full subprocess lifecycle
- `orchestrator/mod.rs`: re-exports OrchestratorConfig, OrchestratorEvent, WakeResult, wake_orchestrator
- `memory/mod.rs`: added retrieval module
- `memory/episodic.rs`: implemented with LanceDB + fastembed (was stub)
- `memory/semantic.rs`: implemented with SQLite (was stub)
- Added lancedb, fastembed, arrow-array, arrow-schema, futures to workspace dependencies

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
