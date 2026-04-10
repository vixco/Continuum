# Kairo Build Prompts

This file contains the prompts you feed to Claude Code to build Kairo. There are two main prompts and a set of phase prompts. Use them in order.

## How to use this file

1. **Clone the repo locally** on your Windows machine.
2. **Install Claude Code** if you haven't yet: `npm install -g @anthropic-ai/claude-code` then `claude login`.
3. **Open a terminal in the repo root** and run `claude` to start an interactive session.
4. **Copy the Master Bootstrap Prompt** below and paste it as your first message. Let Claude Code work through it (this takes a while, maybe an hour, because it's setting up everything).
5. **Review what Claude Code produced**, commit it, push it.
6. **Then use the Phase 0 prompt** as your next message to start actual development.
7. **After Phase 0 works**, move to Phase 1, then Phase 2, etc.

For every phase, open a fresh Claude Code session in the repo and paste the corresponding phase prompt. Claude Code will read the repo, read `CLAUDE.md` and `ARCHITECTURE.md`, and continue from where the previous phase left off.

---

## MASTER BOOTSTRAP PROMPT

Use this exactly once, at the very beginning, to set up the full project scaffolding.

```
You are the primary developer of Kairo, an ambitious open-source desktop AI assistant project. I am Toshan, the architect and maintainer. You will do most of the actual coding.

Before you do anything else, read these files in this order:

1. README.md — the public pitch, tells you what Kairo is
2. ARCHITECTURE.md — the full technical blueprint, this is the source of truth for every design decision
3. CLAUDE.md — your operating manual for this repo, covers coding standards, non-negotiables, and how to call the claude CLI from within Kairo
4. SOUL.md — Kairo's personality, you'll need this for prompt files later
5. ROADMAP.md — the phased build plan, we are starting from Phase 0

Read them fully. Do not skim. If anything in these files contradicts anything I say in this prompt, the files win — tell me and we'll update the files together.

Your job in this first session is to scaffold the entire repository so that subsequent sessions can start writing actual code. You are not writing any Kairo logic yet. You are setting up the skeleton that ARCHITECTURE.md describes in the "Directory layout" section.

Use the TodoWrite tool to create a detailed plan with at least 15 items before you start. Track your progress as you go.

Here is what I want you to produce, in order:

## Part 1: Workspace setup

1. Create a Cargo workspace at the repo root. The root Cargo.toml should declare a workspace with members: `crates/kairo-core`, `crates/kairo-mcp`, `crates/kairo-llm`, `crates/kairo-vision`, and `apps/desktop/src-tauri`. Set resolver = "2".

2. Create a pnpm workspace. Add a root `package.json` with the workspace name `kairo-ai`, private: true, and a `pnpm-workspace.yaml` declaring `apps/*` as the workspace packages.

3. Create a `rust-toolchain.toml` pinned to Rust 1.82, components rustfmt and clippy, profile default.

4. Create a `.gitignore` at the root that covers Rust (target/, Cargo.lock should NOT be ignored since this is an application), Node (node_modules, .next, .turbo, dist, .DS_Store), Tauri (src-tauri/target, src-tauri/gen, *.dmg, *.msi, *.exe), editor files (.vscode/, .idea/, *.swp), and environment files (.env, .env.local). Also ignore `~/.kairo/` equivalents in the dev workspace: `.kairo-dev/`.

5. Create `.editorconfig` with UTF-8, LF line endings, 4 spaces for Rust, 2 spaces for TypeScript/JSON/YAML/Markdown, trim trailing whitespace, insert final newline.

## Part 2: Rust crates scaffolding

For each of the four Rust crates, create:

- `Cargo.toml` with name, version 0.0.0, edition 2021, license Apache-2.0, description
- `src/lib.rs` with a module-level doc comment explaining what this crate does and which layer it belongs to
- An empty `tests/` directory with a `.gitkeep`

The four crates and their purposes:

**crates/kairo-core** — The main orchestration runtime. Contains the four-layer logic: senses, triage, orchestrator, workers, memory, voice, health, config. Set up the module tree as described in ARCHITECTURE.md directory layout. Create empty module files (with doc comments but no implementation yet) for each submodule. Add these dependencies to Cargo.toml but do not implement anything: tokio (full features), serde, serde_json, anyhow, thiserror, tracing, tracing-subscriber, chrono (with serde), uuid (v4 and serde), async-trait, sqlx (sqlite and runtime-tokio-rustls).

**crates/kairo-mcp** — The MCP server exposing Windows-specific tools. This is a binary crate, not a library. Set it up with a main.rs that prints "kairo-mcp not yet implemented" and exits 0. Create the `src/tools/` directory with empty module files for memory, perception, voice, windows, shell, workers, schedule, system. Add dependencies: rmcp (the Rust MCP SDK), tokio, serde, serde_json, schemars, anyhow, thiserror, tracing.

**crates/kairo-llm** — The local LLM runtime wrapper. This is a library crate. Add dependencies: tokio, anyhow, thiserror, tracing, serde, serde_json. Leave llama-cpp-rs out for now since it needs native libs — we'll add it in Phase 2.

**crates/kairo-vision** — The local vision model runtime. Library crate. Same minimal dependencies as kairo-llm.

## Part 3: Tauri desktop app scaffolding

1. Create `apps/desktop/` as a Next.js 15 + Tauri 2 project. Use the App Router, TypeScript, and Tailwind CSS.

2. Frontend: in `apps/desktop/`, create a minimal Next.js setup with `package.json`, `next.config.mjs` (with `output: 'export'` because Tauri wants static files), `tsconfig.json` (strict: true), `tailwind.config.ts`, `postcss.config.mjs`, and `src/app/layout.tsx`, `src/app/page.tsx`, `src/app/globals.css`. The homepage should just show "Kairo — pre-alpha scaffolding" in a centered dark layout using Tailwind.

3. Tauri backend: in `apps/desktop/src-tauri/`, create a Tauri 2 project with `Cargo.toml`, `tauri.conf.json` (with app name "Kairo", bundle identifier "com.toshan.kairo", window title "Kairo"), `build.rs`, and `src/main.rs`. The main.rs should set up a basic Tauri app with a single command `greet` that returns "hello from kairo core". Add the `kairo-core` crate as a dependency via workspace path.

4. Make sure the Tauri Cargo.toml has the correct `tauri-build` in `[build-dependencies]` and `tauri` in `[dependencies]` with the right feature flags for Tauri 2.

## Part 4: Prompts directory

Create `prompts/` with these four files. Each file should have a clear header explaining which layer uses it, when it's loaded, and how to update it. The actual prompt content can be a single placeholder paragraph that says "TODO: fill in during Phase 2/3/7" — we'll write the real prompts in the appropriate phase. The four files:

- `prompts/triage-system.md` — used by Layer 2, loaded on every triage call
- `prompts/orchestrator-system.md` — used by Layer 3, imports SOUL.md and TOOLS.md
- `prompts/repair-agent-system.md` — used by the repair agent in Phase 7
- `prompts/salience-heuristics.md` — documentation of the salience scoring rules

## Part 5: Config directory

Create `config/` with these files. Fill them with sensible defaults matching ARCHITECTURE.md:

- `config/default-models.toml` — TOML file listing the default model choices for vision (Moondream 2), triage (Qwen 2.5 3B Q4_K_M), orchestrator (claude-opus-4-6), workers (auto mode), STT (whisper small), TTS (piper with a default voice). Include paths where models will be downloaded to (`~/.kairo/models/<component>/`).
- `config/default-permissions.toml` — TOML file defining the permission tiers for MCP tools. Organize by namespace (memory, perception, voice, windows, shell, workers, schedule, system). For each tool, set the tier: auto, session-approved, always-confirm, or blocked. Use the defaults I'd expect from reading ARCHITECTURE.md section "Security and permissions".
- `config/default-mcp-servers.json` — JSON file that Claude Code will read via `--mcp-config`. Initially it registers only the Kairo MCP server (pointing to the kairo-mcp binary) plus leaves room for user-added servers.

## Part 6: Skills directory

Create `skills/` with a `README.md` explaining what skills are (SKILL.md files that extend Kairo's knowledge for specific workflows) and three placeholder skill directories:

- `skills/daily-briefing/SKILL.md`
- `skills/code-review/SKILL.md`
- `skills/project-context/SKILL.md`

Each SKILL.md should have proper frontmatter with `name` and `description` fields, but the actual skill content can be a placeholder saying "TODO: implement in Phase 8."

## Part 7: Scripts directory

Create `scripts/` with these PowerShell scripts. Make each script idempotent and print clear status messages:

- `scripts/dev-setup.ps1` — checks for prerequisites (Rust, pnpm, Claude Code CLI), prints their versions, exits with clear error if anything is missing
- `scripts/download-models.ps1` — placeholder that prints "TODO: Phase 1/2 will implement model downloads" and exits 0
- `scripts/install.ps1` — placeholder for the future installer

## Part 8: GitHub actions

Create `.github/workflows/ci.yml` that runs on push and PR to main. It should:
- Set up Rust stable
- Run `cargo fmt --all -- --check`
- Run `cargo clippy --all-targets --all-features -- -D warnings`
- Run `cargo test --workspace`
- Set up pnpm and Node 20
- Run `pnpm install --frozen-lockfile`
- Run `pnpm --filter desktop build` to verify the Next.js frontend builds

For now the workflow can be Windows-only since Kairo is Windows-first.

## Part 9: Miscellaneous

1. Create a `LICENSE` file at the root with the full text of the Apache License 2.0. Use the canonical text from apache.org.

2. Create `CHANGELOG.md` at the root with this content:

    # Changelog
    
    All notable changes to Kairo are documented here. Format based on Keep a Changelog, versioning based on SemVer.
    
    ## [Unreleased]
    
    ### Added
    - Initial repository scaffolding
    - Architecture, soul, roadmap, and Claude Code instructions
    - Cargo and pnpm workspace setup
    - Tauri 2 desktop app skeleton
    - Placeholder Rust crates for kairo-core, kairo-mcp, kairo-llm, kairo-vision
    - CI workflow for Rust and Next.js builds

3. Create `CONTRIBUTING.md` with a short "Kairo is not accepting code contributions during the pre-alpha phase. Star the repo and watch for updates" message plus a longer section on how contributions will work after alpha (fork, branch, PR, follow the rules in CLAUDE.md).

4. Update the existing README.md "Project status" table if any of the checkmarks need adjusting based on what you just built.

## Part 10: Verification

Once everything is in place, verify:

1. Run `cargo build --workspace` from the repo root. It must succeed. If it fails, fix whatever is wrong until it passes.
2. Run `cargo fmt --all --check`. Fix any formatting issues.
3. Run `cargo clippy --all-targets` and fix any warnings.
4. Run `cd apps/desktop && pnpm install && pnpm build`. The frontend must build.
5. Run `tree -I 'target|node_modules|.next'` (or equivalent) and show me the final directory structure.

When all of this is done, write a summary of what you created, what works, what doesn't, and what I should check manually before moving to Phase 0.

Do not try to implement any actual Kairo features in this session. This is scaffolding only. The goal is that at the end, the repo compiles, looks right, and is ready for Phase 0. No more, no less.

If you hit something you're unsure about — a crate version, an API choice, a configuration detail — stop and ask me. Do not guess.

Start by reading the docs, creating your todo list, and confirming you understand the scope before you begin writing files.
```

---

## PHASE 0 PROMPT — Hello world

Use this after the master bootstrap has been committed and pushed.

```
We are now starting Phase 0 of the Kairo roadmap: Hello World.

Read ROADMAP.md section "Phase 0 — Hello world" in full. That is the goal for this session.

Then read CLAUDE.md section "How to run Claude Code from within Kairo". That pattern is exactly what you need to implement.

You also need to read these external docs before writing any code:
- https://code.claude.com/docs/en/headless — the headless mode documentation
- https://code.claude.com/docs/en/cli-reference — the full CLI reference

Your deliverable is an example binary at `crates/kairo-core/examples/hello_world.rs` that:

1. Spawns `claude` as a child process with these flags: `--print --output-format stream-json --input-format stream-json --verbose --include-partial-messages --model claude-opus-4-6`
2. Writes a single JSON user message to stdin: `{"type":"user","message":{"role":"user","content":"What is 2+2? Respond in one sentence."}}`
3. Reads stdout line by line, parses each line as JSON into a strongly-typed enum
4. Prints any `text_delta` content to the terminal as it streams in (so the user sees the answer appear live)
5. Exits cleanly when the `result` event arrives, printing the total cost and duration
6. Handles errors gracefully: missing claude binary, invalid JSON, stream termination, etc.

Define the event enum in `crates/kairo-core/src/orchestrator/events.rs` as a new module. Export it from lib.rs. The event types you need to handle include:
- system init events
- assistant messages with content blocks (text, tool_use)
- user messages with tool_result blocks
- stream_event events with content_block_delta for live text streaming
- result events with cost/duration/session metadata

Use serde's `#[serde(tag = "type")]` pattern for the top-level enum and handle unknown variants gracefully (don't crash on new event types).

Acceptance criteria:

1. `cargo run --example hello_world -p kairo-core` works from the repo root
2. The user sees text appear word-by-word as Claude streams it
3. At the end, the total cost and duration are printed
4. If `claude` is not installed, the error message says so clearly
5. If the user is not logged in to claude, the error is clear
6. No unwrap() calls in the example code — use proper error handling with anyhow
7. Add a test for the event parser that feeds it a few recorded JSON lines and verifies the correct enum variants are produced
8. Update CHANGELOG.md under Unreleased

Use TodoWrite to plan this before you start. Show me the plan. Then build it. Then run it and show me the output.

If Claude Code's stream-json format has changed since ARCHITECTURE.md was written, adjust the parser and update both ARCHITECTURE.md and CLAUDE.md to match reality.
```

---

## PHASE 1 PROMPT — Perception

Use this after Phase 0 is merged.

```
We are starting Phase 1 of the Kairo roadmap: Perception.

Read ROADMAP.md section "Phase 1 — Perception" and ARCHITECTURE.md section "Layer 1 — Senses" in full before you start.

The goal of this phase is to produce a running Kairo Core binary that continuously captures perception frames from screen, audio, and context, and writes them to a SQLite raw log.

This is a large phase. Break it into subtasks and use sub-agents via the Task tool for the independent pieces. Create a detailed TodoWrite plan with at least 20 items.

## Subtasks

### 1. The vision crate

In `crates/kairo-vision`, implement Moondream 2 inference via ONNX Runtime.

- Add dependencies: `ort` (the ONNX Runtime crate), `image`, `ndarray`, `tokenizers`, `anyhow`, `tracing`
- Create a `Moondream` struct that loads the model file from a path
- Expose an async `describe(image: DynamicImage) -> Result<String>` method
- The description should be one sentence focused on what the user is looking at, not exhaustive detail
- Write integration tests that feed a test image and verify a non-empty description

Before implementing, research the current state of Moondream 2 on ONNX and confirm the model file format and input/output tensor shapes. If ONNX Moondream is not stable yet, use the Candle Rust crate with Moondream weights instead. Pick whichever works and document the choice.

### 2. Screen capture

In `crates/kairo-core/src/senses/vision.rs`, implement screen capture.

- Use the `windows-capture` crate or the `windows` crate's Graphics Capture API
- Capture the primary monitor at full resolution
- Downscale to 1280x720 using the `image` crate
- Pass the downscaled image to the vision crate for description
- Return a `ScreenObservation` struct with description, foreground app, and a confidence score
- Configurable capture interval via config (default 3 seconds)
- Write a loop that runs in a tokio task and emits observations to a channel

### 3. Audio capture and transcription

In `crates/kairo-core/src/senses/audio.rs`, implement mic capture with VAD and Whisper transcription.

- Use `cpal` for cross-platform audio capture
- Use `webrtc-vad` or `silero-vad` for voice activity detection
- Use `whisper-rs` for transcription
- Only transcribe when VAD detects speech
- Return an `AudioObservation` with transcript, language, duration, and confidence
- Emit observations to a channel

### 4. Context polling

In `crates/kairo-core/src/senses/context.rs`, implement Windows context polling.

- Use the `windows` crate to query:
  - Foreground window handle, title, and process name
  - Idle time since last user input (`GetLastInputInfo`)
  - Whether a call app is in the foreground (Discord, Teams, Zoom, Meet) — check process name
- Try to get the active file path from VS Code via UI Automation (optional, can be stubbed for this phase)
- Emit `ContextObservation` every 1 second

### 5. Frame builder

In `crates/kairo-core/src/senses/frame.rs`, implement the perception frame builder.

- Define the `PerceptionFrame` struct with all observations plus timestamp and salience_hint
- Define a `PerceptionFrameBuilder` that reads from the three observation channels and assembles frames at a configurable interval
- Implement the `compute_salience` function using the heuristics from ARCHITECTURE.md (frame diff, new errors, recent audio, new window focus, upcoming calendar event)
- Emit frames to a final output channel

### 6. Raw log storage

In `crates/kairo-core/src/memory/raw_log.rs`, implement SQLite storage for perception frames.

- Use `sqlx` with SQLite and rustls
- Create the schema on first run (perception_frames table with all the fields from ARCHITECTURE.md)
- Expose `write_frame(frame: &PerceptionFrame)` and `query_frames(since, until)` methods
- Write screenshots to `~/.kairo/screenshots/<date>/` and store paths in the DB (not blobs)
- Implement nightly rotation that deletes frames older than the retention policy

### 7. The main perception binary

In `crates/kairo-core/src/bin/kairo-perception.rs`, create a binary that:

- Loads config from `~/.kairo/config.toml` (or a dev default)
- Initializes the three senses subsystems
- Starts the frame builder
- Writes every frame to the raw log
- Prints a live one-line summary of each frame to the terminal (e.g. `[14:32:11] VS Code | user spoke: "waarom werkt dit niet" | salience: 0.47`)
- Handles Ctrl+C gracefully

### 8. Tests

For each module, write unit tests for the pure logic (salience calculation, frame assembly, schema). Write one integration test that runs the full pipeline for 30 seconds with a mock screen/audio source and verifies frames are written to the raw log.

### 9. Documentation

Update `docs/senses.md` with how the senses layer works, what models it uses, how to configure it, and how to debug it.

### 10. CHANGELOG

Update CHANGELOG.md under Unreleased with everything you added.

## Acceptance criteria

1. `cargo run --bin kairo-perception` runs cleanly for at least 5 minutes without crashing
2. The terminal shows a continuous stream of frame summaries
3. The SQLite raw log has entries for every frame
4. Screenshots are saved to disk and paths match the DB
5. `cargo clippy --all-targets -- -D warnings` passes
6. `cargo test --workspace` passes
7. Resource usage on an idle screen is under 500 MB RAM and under 20% of a single CPU core
8. When you manually trigger changes (open an app, speak into the mic), the frames reflect them within 3 seconds

Before you start, research the current state of Moondream 2 Rust bindings, windows-capture vs Graphics Capture API, and whisper-rs with streaming. Summarize your findings and get my approval before committing to a specific approach.

Use sub-agents for the independent subtasks. The vision crate, audio crate, and context poller are three good candidates for parallel sub-agent work.
```

---

## PHASE 2 PROMPT — Triage

Use this after Phase 1 is merged.

```
We are starting Phase 2 of the Kairo roadmap: Triage.

Read ROADMAP.md section "Phase 2 — Triage" and ARCHITECTURE.md section "Layer 2 — Triage" in full.

The goal is to add a local LLM that evaluates every salient perception frame and outputs a structured triage decision.

## Subtasks

### 1. The LLM crate

In `crates/kairo-llm`, wrap llama.cpp via `llama-cpp-rs` (or the `llama-cpp-2` crate if that's more current — research which is actively maintained). Expose:

- `struct LocalLlm` that loads a GGUF model from a path
- Async `generate(prompt: &str, opts: GenerateOptions) -> Result<String>` method
- Async `generate_json<T: DeserializeOwned>(prompt: &str, schema: Value) -> Result<T>` method that uses llama.cpp's grammar mode to constrain output
- Streaming version `generate_stream(prompt, opts) -> impl Stream<Item = String>` for future use

Support GPU acceleration when CUDA is available but fall back to CPU gracefully.

### 2. Model download

In `scripts/download-models.ps1`, implement actual model downloads for:
- Qwen 2.5 3B Instruct Q4_K_M GGUF from Hugging Face
- Target path `~/.kairo/models/triage/qwen2.5-3b-q4.gguf`

Check if the file already exists before downloading. Verify the SHA256 after download.

### 3. The triage prompt

Fill in `prompts/triage-system.md` with the real prompt from ARCHITECTURE.md section "Triage prompt". Include the SOUL.md excerpt inline (or load it dynamically, your choice — document the choice).

### 4. The triage loop

In `crates/kairo-core/src/triage/mod.rs`, implement:

- `struct TriageLayer` that holds a LocalLlm instance
- `async fn evaluate(frame: PerceptionFrame) -> TriageDecision` method
- The `TriageDecision` enum with all the variants from ARCHITECTURE.md
- A loop that reads from the perception frame channel, calls evaluate, and emits decisions
- Latency measurement: if any call takes > 2 seconds, log a warning

### 5. Decision handlers

Implement handlers for the non-wake decisions:
- `ignore` — drop the frame
- `remember` — write to the raw log with a special flag, will be used by episodic memory in Phase 3
- `whisper` — placeholder that prints "would say: <text>" to the terminal (real TTS comes in Phase 5)
- `execute_simple` — a small allowlist of pre-approved actions (launch app, toggle mute, show notification)

The `wake_orchestrator` variant is deferred to Phase 3.

### 6. Triage benchmarking harness

Create `crates/kairo-core/src/bin/kairo-triage-bench.rs` that loads a set of hand-labeled perception frames from `benchmarks/triage-frames.jsonl`, runs each through the triage layer, and measures:
- Accuracy (matches expected decision)
- Latency (p50, p95, p99)
- Token usage

Write the first 20 benchmark frames by hand. Cover a mix of "obviously ignore", "obviously wake", "ambiguous" cases.

### 7. Integration

Update the main perception binary so that frames also flow through triage. Show the triage decision in the terminal output alongside the frame summary.

### 8. Tests and docs

Unit tests for the decision parser. Integration test that runs the full perception → triage pipeline for 30 seconds with mock data. Update `docs/triage.md`.

## Acceptance criteria

1. Qwen 2.5 3B is downloaded and loaded successfully
2. The triage benchmark runs with > 80% accuracy on the 20 frames
3. P95 latency is under 1.5 seconds
4. CPU-only inference works (users without GPUs must still be functional)
5. The main perception binary shows triage decisions for every frame
6. Memory usage remains stable over a 10-minute run (no leaks)
7. All tests pass, clippy clean, CHANGELOG updated

Before starting, research llama.cpp Rust bindings to pick the best crate. Also verify the Qwen 2.5 3B GGUF is available on Hugging Face with a Q4_K_M quant. If not, pick an equivalent model and document the choice.
```

---

## PHASE 3 PROMPT — Orchestrator

Use this after Phase 2 is merged.

```
We are starting Phase 3 of the Kairo roadmap: Orchestrator.

Read ROADMAP.md section "Phase 3 — Orchestrator" and ARCHITECTURE.md sections "Layer 3 — Orchestrator" and "Memory system" in full.

The goal is to make Claude Opus 4.6 wake up when triage decides, read relevant memory, think, and speak back.

This is the biggest phase so far. Plan carefully. Use sub-agents.

## Subtasks

### 1. Orchestrator spawner

Build on top of the Phase 0 hello_world to implement `crates/kairo-core/src/orchestrator/spawn.rs`:

- `struct Orchestrator` with config for model, prompt paths, allowed tools
- `async fn wake(ctx: WakeContext) -> OrchestratorResponse` method
- Spawns claude with all the flags from CLAUDE.md
- Streams events, parses them into the event enum
- Returns a structured response containing the final text, any tool calls made, session id, and cost

### 2. Wake context builder

In `crates/kairo-core/src/orchestrator/context.rs`, build the wake context that gets sent to Opus:
- Current perception frame
- Last 15 minutes of raw log summarized to under 500 tokens
- Top 3 relevant episodic memories (needs memory system first)
- Active project context from semantic memory
- The reason the triage layer decided to wake up
- Available tools (just read from the MCP config for now, since MCP server isn't built yet)

Must fit within 4000 tokens total.

### 3. Episodic memory

In `crates/kairo-core/src/memory/episodic.rs`, implement LanceDB-based episodic memory:
- Add dependencies: `lancedb`, `fastembed`
- Create the LanceDB table schema (id, ts_start, ts_end, content, embedding, tags, importance, linked_facts)
- Implement `write_memory`, `query_by_vector`, `query_by_tag` methods
- Implement the background distillation task that runs every 15 minutes: read the last window of raw log entries, ask the triage LLM to summarize into 1-5 episodic memory entries, embed them with fastembed, write to LanceDB

### 4. Semantic memory

In `crates/kairo-core/src/memory/semantic.rs`, implement SQLite-based semantic fact storage:
- The schema from ARCHITECTURE.md (semantic_facts and semantic_edges tables)
- `fact_get`, `fact_set`, `fact_query`, `edge_add`, `edge_query` methods
- Seed some initial facts from a `config/initial-facts.toml` file (user name, timezone, etc.)

### 5. Memory retrieval flow

In `crates/kairo-core/src/memory/retrieval.rs`, implement the two-step retrieval:
1. Vector search episodic memory with the current frame as query, return top 10
2. Use the triage LLM to re-rank: "which of these 10 memories are most relevant?" Return top 3.

### 6. Orchestrator prompt

Fill in `prompts/orchestrator-system.md` with a real prompt that:
- Inlines the full SOUL.md content
- Describes the four-layer architecture briefly so Opus understands its role
- Lists the tools it can call (even though tools aren't built yet, list the names that will exist)
- Sets the ground rules: plan and delegate, don't do long tasks yourself, be brief

### 7. The main Kairo binary

Create `crates/kairo-core/src/bin/kairo.rs` that ties everything together:
- Starts the perception layer
- Starts the triage layer
- When triage decides `wake_orchestrator`, builds the wake context and calls the orchestrator
- Streams Opus's response text to stdout as a placeholder for TTS
- Logs the orchestrator call with cost and duration

### 8. Tests and docs

Integration test that runs the full pipeline against a mocked claude CLI. Update `docs/orchestrator.md` and `docs/memory.md`.

## Acceptance criteria

1. `cargo run --bin kairo` runs the full stack
2. When you manually trigger a wake (e.g. via a test command), Opus responds within 5 seconds
3. Opus's response streams token-by-token to stdout
4. Episodic memory entries are created from raw log every 15 minutes
5. Semantic facts can be read and written via code
6. Wake context stays under 4000 tokens in all test scenarios
7. Cost per wake-up is logged and visible
8. All tests pass, clippy clean, CHANGELOG updated

Before starting, verify claude -p --output-format stream-json --input-format stream-json is still the correct invocation pattern and that the event schema hasn't changed since Phase 0. If it has, update everything.
```

---

## PHASE 4 THROUGH PHASE 9 — Shorter template

For phases 4 through 9, use this template and fill in the phase name:

```
We are starting Phase [N] of the Kairo roadmap: [Name].

Read:
1. ROADMAP.md section "Phase [N] — [Name]" — the done-when checklist is your acceptance criteria
2. ARCHITECTURE.md sections [list the relevant sections for this phase]
3. CLAUDE.md to refresh on coding standards and non-negotiables

Create a TodoWrite plan with all the items from the roadmap checklist plus any sub-items you need. Show me the plan.

Break the work into sub-agent tasks where possible. Good candidates for sub-agents are:
- Research tasks ("research the best crate for X and return a comparison")
- Independent module implementations ("implement the voice/tts.rs module")
- Test suite writing ("write integration tests for the MCP tool X")

Work through the plan, verifying as you go. After each major component is done, run:
- cargo build --workspace
- cargo clippy --all-targets -- -D warnings
- cargo test --workspace
- Relevant manual smoke test

At the end of the phase, update CHANGELOG.md under Unreleased with everything you added, and update ROADMAP.md by checking off the completed items.

If you hit architectural decisions that ARCHITECTURE.md doesn't cover, stop and ask me. Do not guess your way through a design decision.

Specific notes for Phase [N]:
[Add any phase-specific concerns here, like "Windows UI Automation is COM-heavy, research the `windows` crate's UIA module carefully" or "voice latency is the primary metric for this phase"]
```

---

## Tips for running Claude Code on this project

### Start with `/model opus` in interactive mode

For big scaffolding prompts like the Master Bootstrap and the larger phases, switch Claude Code to Opus 4.6 with the `/model` slash command before pasting the prompt. Sonnet is faster but Opus is better at holding a multi-step plan across long sessions.

### Use sessions and `--resume` across days

A single phase might take multiple days. After each work session, note the session id (printed in the output). The next day, start Claude Code with `claude --resume <session-id>` to continue from where you left off without losing context.

### Set generous tool allowlists

For development sessions on this project, run claude with `--allowedTools "Read,Write,Edit,Bash,Task,WebFetch,WebSearch,TodoWrite"` or use `--permission-mode acceptEdits` to avoid constant approval prompts. Kairo is a big project — too much friction kills momentum.

### Keep the dashboard open (once it exists)

After Phase 6, you can actually watch Kairo build Kairo from the dashboard. Open Home tab, watch the active workers panel. It's surreal but useful for catching runaway sessions.

### Commit after every phase, tag pre-releases

After each phase is done and tests pass, commit with `feat(phaseN): <what was built>` and tag a pre-release: `git tag v0.0.N-phaseN && git push origin v0.0.N-phaseN`. This makes rollback trivial if a later phase breaks something.

### When Claude Code gets stuck

If Claude Code is going in circles or making the same mistake repeatedly, stop the session, read what it tried, write a new prompt that clarifies the specific thing it got wrong, and start fresh. Do not let a confused session keep burning tokens.

---

Last updated: 2026-04-10.
