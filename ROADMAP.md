# Roadmap

This is the build plan for Kairo, from empty repo to v1.0. Each phase has a clear goal, a concrete deliverable, and a "done when" checklist. The phases build on each other, so do not skip ahead — a broken foundation breaks everything above it.

Timelines are rough estimates assuming one focused developer (you + Claude Code) working on this seriously. Adjust as reality dictates.

## Overview

```
Phase 0  Hello world               — prove Claude Code can be spawned         (2–3 days)
Phase 1  Perception                 — senses layer produces frames             (1 week)
Phase 2  Triage                     — local LLM gates decisions                (1 week)
Phase 3  Orchestrator               — Opus wakes up, speaks, remembers         (1–2 weeks)
Phase 4  MCP tools                  — Windows capabilities exposed to Claude   (2 weeks)
Phase 5  Voice                      — full speech pipeline                     (1–2 weeks)
Phase 6  Dashboard                  — UI for everything                        (2 weeks)
Phase 7  Self-healing               — repair agent                             (1 week)
Phase 8  Workers + skills           — multi-agent workflows                    (1–2 weeks)
Phase 9  Polish + alpha release     — first public version                    (2 weeks)
```

Total: 12–15 weeks to public alpha, assuming consistent focus.

---

## Phase 0 — Hello world

**Goal:** Prove that a Rust process can spawn `claude` in headless mode, send it a prompt, and parse the streamed JSON response.

**Deliverable:** A minimal Rust binary in a scratch crate that runs Claude Code, asks it "what is 2+2", and prints the streamed text response to the terminal in real time.

**Done when:**

- [x] Cargo workspace is initialized with `crates/kairo-core` as a library crate
- [x] `crates/kairo-core/examples/hello_world.rs` spawns `claude --print --output-format stream-json --input-format stream-json --verbose --include-partial-messages`
- [x] It writes a JSON user message to stdin: `{"type":"user","message":{"role":"user","content":"What is 2+2?"}}`
- [x] It reads stdout line-by-line, parses each line as JSON, and prints any `text_delta` events
- [x] It exits cleanly when the `result` event arrives
- [x] README has a one-paragraph note about how to run it locally

**Why this matters:** Everything else depends on this working reliably. If Claude Code's CLI surface changes, we find out now, not in week 10.

**What to watch out for:** Claude Code's stream-json format is not fully documented. Be prepared to experiment with the event types and add defensive parsing. The CLI may require `ANTHROPIC_API_KEY` or an active `claude login` session — document this clearly.

---

## Phase 1 — Perception

**Goal:** The senses layer produces a continuous stream of `PerceptionFrame` objects from screen, audio, and context.

**Deliverable:** A running Kairo Core binary that captures screenshots every 3 seconds, runs them through a local vision model, captures audio via VAD and transcribes it with whisper, polls Windows for context, and writes the frames to a SQLite raw log.

**Done when:**

- [x] `crates/kairo-vision` wraps SmolVLM-256M via ONNX Runtime with a `describe(image) -> VisionOutput` API (encoder pipeline validated, decoder stubbed for Phase 1.5)
- [x] `crates/kairo-core/src/senses/vision.rs` captures screenshots via `xcap` (GDI), downscales to 1280×720, and calls the vision crate
- [x] `crates/kairo-core/src/senses/audio.rs` wraps `whisper.cpp` via `whisper-rs`, captures mic audio with energy VAD, and emits transcripts (behind `audio` feature flag, requires LLVM)
- [x] `crates/kairo-core/src/senses/context.rs` polls foreground window, idle time, and call state once per second
- [x] A `PerceptionFrameBuilder` combines the three into frames at a configurable interval
- [x] Frames are written to `~/.kairo-dev/raw_log.sqlite` with proper indexing
- [x] The salience heuristic function is implemented and tested (12 unit tests)
- [x] `cargo run --bin kairo-perception` shows the perception stream live in the terminal

**Why this matters:** Without reliable perception, nothing else can work. This is the foundation of Kairo's uniqueness.

**What to watch out for:** Windows screen capture has multiple APIs and they all have quirks. Graphics Capture API is modern but requires Windows 10 1903+. GDI is older but works everywhere. Pick one and document the tradeoff. Also, whisper.cpp with realtime streaming is finicky — test on actual hardware early.

---

## Phase 2 — Triage

**Goal:** A small local LLM reads perception frames and outputs triage decisions.

**Deliverable:** The triage layer runs the Qwen 2.5 3B model via llama.cpp, evaluates each salient perception frame, and produces a JSON decision within 500 ms.

**Done when:**

- [ ] `crates/kairo-llm` wraps `llama.cpp` via `llama-cpp-rs` with a simple streaming chat API
- [ ] Model files are downloadable via `scripts/download-models.ps1`, stored in `~/.kairo/models/`
- [ ] `crates/kairo-core/src/triage/mod.rs` implements the decision loop
- [ ] `prompts/triage-system.md` is written and loaded at startup
- [ ] The triage layer receives frames from the perception stream and emits decisions to a decision queue
- [ ] `ignore`, `remember`, `whisper`, `execute_simple`, and `wake_orchestrator` decisions are all handled
- [ ] Decision latency is measured and logged; if > 2 seconds, a warning is raised
- [ ] Integration tests cover at least 20 distinct frame scenarios with expected decisions

**Why this matters:** The triage layer is what makes Kairo economically viable. If triage is wrong, Opus gets woken up for nonsense and costs explode.

**What to watch out for:** Small LLMs are unreliable at structured JSON output without heavy prompting. Use llama.cpp's grammar mode or JSON Schema constraints if the model supports it. Test extensively with edge cases — idle screens, foreign language audio, multi-monitor setups, partial transcripts.

---

## Phase 3 — Orchestrator

**Goal:** When the triage layer decides to wake up the orchestrator, Kairo Core spawns Claude Code, sends it context, streams the response to a TTS (or stdout for now), and stores the outcome in memory.

**Deliverable:** A full wake-up cycle works end-to-end: user triggers something → perception captures it → triage wakes orchestrator → Opus thinks → response is spoken → memory is updated.

**Done when:**

- [ ] `crates/kairo-core/src/orchestrator/mod.rs` implements the full spawn + stream loop
- [ ] `prompts/orchestrator-system.md` is written (imports SOUL.md and adds runtime context)
- [ ] The orchestrator receives a wake context (current frame + memory recall + active project)
- [ ] Opus's response is streamed token-by-token and displayed in real time
- [ ] Memory system is implemented:
  - [ ] `crates/kairo-core/src/memory/raw_log.rs` (SQLite, already from Phase 1)
  - [ ] `crates/kairo-core/src/memory/episodic.rs` with LanceDB and fastembed
  - [ ] `crates/kairo-core/src/memory/semantic.rs` with SQLite
- [ ] A background task distills episodic memories from raw log every 15 minutes
- [ ] Memory retrieval (vector search + triage re-rank) runs before every wake-up
- [ ] A simple stdout-only voice proxy (no real TTS yet) prints Opus's text as speech placeholder

**Why this matters:** This is the first moment Kairo is actually doing what Kairo is supposed to do. You will feel the system come alive here.

**What to watch out for:** Context window management. Do not dump the full raw log into Opus's context. Keep wake contexts under 4000 tokens. Also: Opus occasionally produces very long responses — be ready to truncate or summarize.

---

## Phase 4 — MCP tools

**Goal:** The Kairo MCP server exposes Windows capabilities to Claude Code so the orchestrator can actually *do* things, not just talk about them.

**Deliverable:** `crates/kairo-mcp` runs as a standalone MCP server, registers with Claude Code via `--mcp-config`, and exposes the full tool set described in `ARCHITECTURE.md`.

**Done when:**

- [ ] `crates/kairo-mcp` is set up with `rmcp` crate and compiles as a binary
- [ ] Tool implementations for all namespaces exist:
  - [ ] `mcp__kairo__memory` (query, write, fact get/set, recent)
  - [ ] `mcp__kairo__perception` (current, screenshot, transcribe)
  - [ ] `mcp__kairo__voice` (speak, listen)
  - [ ] `mcp__kairo__windows` (list_apps, focus_app, launch, close, ui_click, ui_type, clipboard, notification)
  - [ ] `mcp__kairo__shell` (run, background, with elevation support)
  - [ ] `mcp__kairo__workers` (spawn, status, cancel) — stub until Phase 8
  - [ ] `mcp__kairo__schedule` (once, recurring, list, cancel)
  - [ ] `mcp__kairo__system` (health, config_get, config_set)
- [ ] Permission tiers are enforced: auto, session-approved, always-confirm, blocked
- [ ] `config/default-permissions.toml` defines sensible defaults
- [ ] Every tool has an integration test that calls it via MCP protocol
- [ ] `docs/mcp-tools.md` documents every tool with examples

**Why this matters:** Without tools, Kairo is just a chatbot. Tools are how Kairo takes action in the real world.

**What to watch out for:** Windows UI Automation is genuinely hard. Start with the simple tools (clipboard, notification, launch) and build up. Save `ui_click` for last. Also: elevated shell commands are dangerous — be paranoid about confirmation.

---

## Phase 5 — Voice

**Goal:** Full bidirectional voice: wake word → streaming STT → triage fast path → orchestrator slow path → streaming TTS → interrupt handling.

**Deliverable:** The user can say "Hey Kairo, what's on my schedule today?" and get a spoken response within a second, with the ability to interrupt mid-sentence.

**Done when:**

- [ ] Porcupine wake word detection is integrated
- [ ] `crates/kairo-core/src/voice/wake.rs` triggers the listening state on wake word
- [ ] Whisper streaming mode transcribes mic audio continuously after wake
- [ ] Semantic endpoint detection uses the triage LLM to decide when the user is done speaking
- [ ] Piper TTS is integrated and can speak text locally
- [ ] ElevenLabs streaming TTS is available as an optional premium backend
- [ ] TTS streaming: first tokens from orchestrator are spoken while the rest is still generating
- [ ] Interrupt handling: playback stops within 50 ms of detected user speech
- [ ] Ambient mute: Kairo detects calls (Discord/Teams/Zoom/Meet) and switches to quiet mode
- [ ] Language detection: Dutch input gets Dutch TTS, English input gets English TTS

**Why this matters:** Voice is the feature that makes Kairo feel like a presence instead of a tool. If voice doesn't feel natural, the whole product fails emotionally.

**What to watch out for:** Voice latency is the hardest engineering problem in the project. Budget: under 800 ms from end-of-user-speech to start-of-kairo-speech for fast-path queries. Under 1500 ms for orchestrator queries. Measure relentlessly.

---

## Phase 6 — Dashboard

**Goal:** The Tauri desktop app is fully functional with all tabs from `ARCHITECTURE.md` implemented.

**Deliverable:** The user can open the dashboard from the tray, see live status, configure every layer, browse memory, manage tools, and tune voice — all without touching config files.

**Done when:**

- [ ] `apps/desktop` is a Tauri 2 app with Next.js 15 frontend
- [ ] System tray integration works: left-click opens dashboard, right-click shows menu
- [ ] Home tab shows live perception, active workers, resource usage, recent actions
- [ ] Brain tab shows the 4-layer diagram and allows model selection + testing
- [ ] Memory tab has episodic, semantic, and raw log views with full CRUD
- [ ] Tools tab lists MCP tools and allows install/uninstall
- [ ] Voice tab exposes all voice configuration with preview
- [ ] Automations tab allows creating and managing scheduled tasks
- [ ] Logs tab is searchable and filterable
- [ ] Health tab shows component statuses (Fix Issues button comes in Phase 7)
- [ ] Frontend connects to Kairo Core via a WebSocket for live updates
- [ ] Dark mode is the default, styled to match the SimCharts navigraph aesthetic

**Why this matters:** Without a dashboard, Kairo is a backend project. The dashboard is what turns it into a product.

**What to watch out for:** Tauri + Next.js is straightforward but has gotchas around SSR (you want client-side rendering for this). Also: live updates via WebSocket across all tabs creates a lot of surface area for state management bugs. Use Zustand for shared state.

---

## Phase 7 — Self-healing

**Goal:** The repair agent can diagnose and fix component failures on demand and on schedule.

**Deliverable:** The "Fix Issues" button in the Health tab spawns a repair agent that reads logs, identifies problems, and applies fixes with user confirmation for destructive actions.

**Done when:**

- [ ] `crates/kairo-core/src/health/repair.rs` implements the repair agent spawning logic
- [ ] `prompts/repair-agent-system.md` is written
- [ ] A dedicated MCP tool set for the repair agent: `repair_restart_component`, `repair_reinstall_component`, `repair_rollback_config`, `repair_test_component`, `repair_escalate`
- [ ] Nightly backup rotation writes snapshots to `~/.kairo-backups/<date>/`
- [ ] Nightly self-diagnose routine checks all components and logs warnings
- [ ] The Health tab shows live repair agent output when a repair is running
- [ ] Voice-activated repair: "Kairo, something isn't right" triggers the repair agent

**Why this matters:** This is the feature that makes Kairo trustworthy. A system that can't fix itself becomes a maintenance nightmare the moment something goes wrong.

**What to watch out for:** The repair agent needs write access to Kairo's own installation, which is a scary permission. Make sure the backup + rollback is rock solid before enabling destructive repairs.

---

## Phase 8 — Workers and skills

**Goal:** The orchestrator can spawn multiple Claude Code workers in parallel, manage their lifecycle, and route tasks to the right model.

**Deliverable:** Complex multi-step workflows work end-to-end. Example: "Kairo, fix the small bugs in my GitHub repo tonight" spawns a worker that reads issues, creates branches, writes commits, and reports back with a briefing.

**Done when:**

- [ ] Worker pool in `crates/kairo-core/src/workers/pool.rs` manages concurrency
- [ ] Workers are spawned via `mcp__kairo__workers__spawn_worker` called by the orchestrator
- [ ] Each worker runs in its own working directory with its own session id
- [ ] Worker model selection honors user's budget/power/auto setting
- [ ] Workers report progress via a status file that the MCP server watches
- [ ] Worker output streams to the Home tab's active workers panel
- [ ] Workers can spawn sub-workers via Claude Code's built-in Task tool
- [ ] Skills directory (`skills/`) is loaded at startup
- [ ] Orchestrator prompt includes a list of available skills
- [ ] Three starter skills are implemented:
  - [ ] `skills/daily-briefing/` — generates a morning briefing from memory + calendar
  - [ ] `skills/simcharts-dev/` — personalized dev workflow for the SimCharts project
  - [ ] `skills/code-review/` — reviews a codebase or PR

**Why this matters:** Workers are how Kairo does long-running work without blocking the orchestrator. This is also where skills earn their keep.

**What to watch out for:** Process management across many concurrent Claude Code sessions is a resource hog. Cap the default concurrency at 3 and let power users tune it up.

---

## Phase 9 — Polish and alpha release

**Goal:** Ship a first public alpha release that other people can install and use without help from the maintainer.

**Deliverable:** A signed Windows installer, a GitHub release, and a public docs site. Someone who has never heard of Kairo can install it, run `kairo setup`, walk through onboarding, and start using it within 15 minutes.

**Done when:**

- [ ] Installer (`scripts/install.ps1` or a proper MSI) handles first-run setup
- [ ] Onboarding wizard in the desktop app:
  - [ ] Checks for Claude Code and guides installation if missing
  - [ ] Downloads default models (Moondream, Qwen 2.5 3B, Whisper small, Piper voices)
  - [ ] Asks for wake word preference
  - [ ] Asks for voice selection
  - [ ] Asks for per-folder permissions
  - [ ] Runs a diagnostic to verify everything works
- [ ] Docs site is live (maybe `docs/` built with Nextra or Mintlify)
- [ ] All `ARCHITECTURE.md` sections have corresponding user docs
- [ ] CI/CD pipeline builds Windows releases on tag push
- [ ] Code signing is set up for the desktop binary
- [ ] GitHub release v0.1.0-alpha is published with clear known-issues list
- [ ] Discord or Matrix community space is set up
- [ ] README is updated with proper screenshots and install instructions
- [ ] `CONTRIBUTING.md` is written and open for PRs

**Why this matters:** A project that never ships isn't a project. Getting to alpha forces every remaining rough edge to become visible and fixable.

**What to watch out for:** The gap between "works on my machine" and "works for strangers" is huge. Expect Phase 9 to take longer than you think. Test on a clean Windows VM before every release candidate.

---

## Post-1.0 ideas

These are not roadmap items — they are possibilities for after the alpha stabilizes and the community gives feedback.

- **macOS and Linux support.** Currently Kairo is Windows-only because Windows APIs are where the deepest integration lives. A cross-platform version would require reimplementing the Windows-specific MCP tools for each platform.
- **Mobile companion app.** An iOS/Android app that pairs with the desktop Kairo over Tailscale, letting the user talk to their PC's brain from anywhere.
- **Memory marketplace.** Users can share pre-built semantic memory packs for specific workflows (e.g. "Next.js developer starter pack").
- **Shared skills registry.** A community repository of skill files that users can install into their Kairo.
- **Multi-user mode.** A household with multiple Kairo users on different machines, with shared semantic facts (household calendar, shared grocery list) and private episodic memory.
- **LLM fine-tuning on personal memory.** An advanced feature where the triage LLM can be fine-tuned on the user's own raw log to get better at predicting salience for that specific person.
- **Voice cloning.** Train a custom TTS voice that matches something the user chooses — their own voice, a favorite narrator, anything.

---

## How to use this roadmap

- **Work phases in order.** Do not start Phase 3 before Phase 2 is stable.
- **Ship each phase to main.** Don't accumulate a giant branch. Merge small, merge often.
- **Update `CHANGELOG.md` as you go.** One entry per merged PR under `## [Unreleased]`.
- **Tag milestones.** After each phase, tag a pre-release: `v0.0.1-phase0`, `v0.0.2-phase1`, etc. This makes rollback easier.
- **Track progress in this file.** When a checkbox gets checked, check it here too and commit the update.

Last updated: 2026-04-10.