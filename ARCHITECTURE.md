# Kairo Architecture

This document is the authoritative technical blueprint for Kairo. It describes every layer of the system, how data flows between them, how tools are exposed, how memory is stored, and how the self-healing subsystem works. Read this before writing any code, and update it before changing any major design decision.

## Contents

1. [Design philosophy](#design-philosophy)
2. [The four cognitive layers](#the-four-cognitive-layers)
3. [Data flow](#data-flow)
4. [Layer 1 — Senses](#layer-1--senses)
5. [Layer 2 — Triage](#layer-2--triage)
6. [Layer 3 — Orchestrator](#layer-3--orchestrator)
7. [Layer 4 — Workers](#layer-4--workers)
8. [The MCP tool layer](#the-mcp-tool-layer)
9. [Memory system](#memory-system)
10. [Voice pipeline](#voice-pipeline)
11. [Dashboard](#dashboard)
12. [Self-healing subsystem](#self-healing-subsystem)
13. [Security and permissions](#security-and-permissions)
14. [Directory layout](#directory-layout)
15. [Key design decisions](#key-design-decisions)

---

## Design philosophy

Kairo follows five architectural rules that drive every decision below them.

**Rule 1 — Cost scales with intelligence.** Every task should be handled by the cheapest layer that can do it correctly. A screenshot of your browser is processed by a 0.23B vision model, not by Claude Opus. A question like "what time is it?" is answered by a 3B local LLM, not by a round trip to the Anthropic API. Opus only wakes up when a task genuinely requires reasoning, planning, or multi-step tool use.

**Rule 2 — Perception is first-class.** Kairo is not a chatbot that happens to have eyes. It is an observation system that happens to speak. The perception layer runs 24/7 and produces a continuous stream of structured frames. Every other layer is a consumer of that stream.

**Rule 3 — Official subprocess over custom integration.** Kairo does not call the Anthropic API directly. It does not scrape OAuth tokens. It invokes the officially supported `claude` CLI in headless mode as a child process and communicates via stdin/stdout. This is the only approach that is legal, stable, and will keep working as Claude evolves.

**Rule 4 — Configuration beats assumption.** Every model, every sample rate, every retention policy, every voice, every tool permission is exposed in the dashboard. Kairo ships with sensible defaults but assumes nothing.

**Rule 5 — The system must be able to repair itself.** A cognitive assistant that breaks and requires terminal debugging has failed its users. Kairo includes a Repair Agent — a dedicated Claude Code session with access to its own installation — that can diagnose and fix component failures on demand.

---

## The four cognitive layers

```
Layer 1  SENSES          Always on, local, ~free
Layer 2  TRIAGE          Local LLM, hundreds of ms per decision
Layer 3  ORCHESTRATOR    Claude Opus 4.6, ~seconds, ~cents per call
Layer 4  WORKERS         Claude Opus or Sonnet, seconds to hours
```

Each layer has a distinct job, a distinct latency budget, and a distinct cost profile. Data flows upward from senses to orchestrator. Commands flow downward from orchestrator to workers and tools. The triage layer is a gate — it decides what bubbles up.

---

## Data flow

```
┌───────────────────────────────────────────────────────────────┐
│  SENSES (always on)                                           │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐         │
│  │  Vision      │  │  Audio       │  │  Context     │         │
│  │  Moondream   │  │  whisper.cpp │  │  Windows API │         │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘         │
│         └─────────────────┼─────────────────┘                 │
│                           ▼                                   │
│                 ┌─────────────────┐                           │
│                 │ Perception      │                           │
│                 │ Frame Builder   │                           │
│                 └────────┬────────┘                           │
└──────────────────────────┼────────────────────────────────────┘
                           │ frame (2–5s interval)
                           ▼
┌───────────────────────────────────────────────────────────────┐
│  TRIAGE (local 3–4B LLM)                                      │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │  Prompt: You are Kairo's triage layer...                │  │
│  │  Decision: ignore | remember | whisper | wake | exec    │  │
│  └────────────────────────┬────────────────────────────────┘  │
└───────────────────────────┼───────────────────────────────────┘
                            │
        ┌───────────┬───────┴───────┬──────────────┐
        ▼           ▼               ▼              ▼
     ignore    store in memory   speak via      WAKE
               (no action)       local TTS      orchestrator
                                 (no Opus)
                                                  │
                                                  ▼
┌───────────────────────────────────────────────────────────────┐
│  ORCHESTRATOR (Claude Opus 4.6 via claude -p stream-json)     │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │  Inputs:                                                │  │
│  │  · Current perception frame                             │  │
│  │  · Last N frames summary                                │  │
│  │  · Top 3 relevant episodic memories (vector retrieval)  │  │
│  │  · Active project context                               │  │
│  │  · Available tools (via MCP server)                     │  │
│  │                                                         │  │
│  │  Decides: speak, spawn worker, schedule, ignore         │  │
│  └────────────────────────┬────────────────────────────────┘  │
└───────────────────────────┼───────────────────────────────────┘
                            │
        ┌───────────────────┼───────────────────┐
        ▼                   ▼                   ▼
    speak (TTS)         spawn worker 1      spawn worker 2
                        (Sonnet 4.6)        (Opus 4.6)
                        │                   │
                        ▼                   ▼
              ┌─────────────────────────────────────┐
              │  WORKERS (headless Claude Code)     │
              │  · own working directory            │
              │  · own tool allowlist               │
              │  · own session id                   │
              │  · report back via stream-json      │
              └─────────────────────────────────────┘
```

The key insight is that **the orchestrator rarely runs**. In normal use it wakes up 20 to 50 times per day. Workers run even less often and finish in seconds or minutes. The bulk of Kairo's activity — 99.9% of all tokens processed — happens in Layer 1 and Layer 2, which are free and local.

---

## Layer 1 — Senses

The senses layer runs as a dedicated subprocess inside Kairo Core. It has one job: produce a steady stream of `PerceptionFrame` objects and push them to the triage layer via an internal queue.

### Vision watcher

Takes a screenshot every *N* seconds (default 3, configurable 1–10) using the Windows Graphics Capture API via the `windows` Rust crate. The screenshot is downscaled to 1280×720 and sent to a small local vision model.

**Default model:** Moondream 2 (1.8B parameters, runs on CPU at ~1 second per image, ~300 MB RAM).
**Alternatives user can select:**
- Florence-2 base (0.23B) — even smaller, faster, slightly worse at text
- Florence-2 large (0.77B)
- MiniCPM-V 2.6 (8B) — much better but requires GPU

The vision model produces a one-sentence description of the screen, focused on *what is the user looking at* rather than exhaustive detail:

```json
{
  "description": "VS Code open, editing ProcedureLayer.tsx, terminal shows error",
  "app_foreground": "Code.exe",
  "has_error_visible": true,
  "confidence": 0.87
}
```

### Audio watcher

Continuously captures microphone audio with voice activity detection (Silero VAD). Only when speech is detected does the audio get sent through whisper.cpp for transcription. This avoids transcribing silence.

**Default model:** whisper.cpp `small` (244 MB, near-realtime on CPU).
**Alternatives:** `tiny`, `base`, `medium`, `large-v3`.

Output:

```json
{
  "transcript": "kut waarom werkt dit nou niet",
  "language": "nl",
  "speaker_confidence": 0.91,
  "duration_ms": 2300
}
```

### Context watcher

Pure Rust code that polls Windows APIs once per second. It captures:

- Foreground window title and process name
- Active file path from editors that expose it via UI Automation (VS Code, JetBrains, Sublime, etc.)
- Currently playing media (via Windows Media Session)
- Idle time (last user input)
- Whether the user is in a call (detects Discord, Teams, Zoom, Meet)
- Active Chrome/Edge tab URL (via accessibility tree)

This layer uses **no AI**. It is just structured polling. It is cheap, fast, and deterministic.

### Perception frame

The three watchers feed into a single `PerceptionFrame` builder that emits one frame every 2–5 seconds:

```rust
pub struct PerceptionFrame {
    pub ts: DateTime<Utc>,
    pub screen: ScreenObservation,
    pub audio: Option<AudioObservation>,
    pub context: ContextObservation,
    pub salience_hint: f32,  // 0.0 to 1.0, from simple heuristics
}
```

The `salience_hint` is a rough pre-filter that prevents the triage LLM from being called for totally uninteresting frames (e.g. nothing changed since last frame, user is idle, no audio). This is a classical rule-based calculation, not an ML model. It uses heuristics like:

- Frame identical to previous? salience = 0.0, skip triage
- New error visible on screen? salience += 0.3
- User spoke within the last 5 seconds? salience += 0.4
- New window focused? salience += 0.2
- Calendar event within 15 minutes? salience += 0.3

Only frames above a threshold (default 0.15) reach the triage layer. Everything else gets stored in the raw log for later retrieval and dropped.

---

## Layer 2 — Triage

The triage layer is a small local LLM (3–4B parameters) that reads every salient perception frame and decides what to do. It is the gatekeeper that decides whether to spend money on Opus or not.

**Default model:** Qwen 2.5 3B Instruct (Q4 quantization, ~2 GB RAM, 40 tokens/sec CPU, 150+ GPU).

**Alternatives:**
- Qwen 3 4B
- Gemma 3 4B (better Dutch)
- Phi-4 mini (3.8B)
- Llama 3.2 3B
- Any GGUF-compatible model via local file path

### Triage prompt

The triage LLM gets a short structured prompt on every call:

```
You are the triage layer of Kairo, {user}'s personal AI assistant.
You are not Kairo. You are the part of Kairo that decides whether Kairo should act.

You will receive a perception frame describing what is happening on {user}'s computer.
Your job is to output exactly one of these decisions, as JSON:

{ "decision": "ignore" }
  — nothing worth doing, discard the frame

{ "decision": "remember", "summary": "..." }
  — worth remembering but no action needed

{ "decision": "whisper", "text": "..." }
  — say a short sentence aloud via local TTS, do not wake the orchestrator

{ "decision": "execute_simple", "action": "..." }
  — perform a simple pre-approved action (start app, toggle mute, etc.)

{ "decision": "wake_orchestrator", "reason": "..." }
  — the situation genuinely needs Claude Opus to think about it

Be extremely conservative about waking the orchestrator. It costs money and 
interrupts the user. Only wake it when genuine reasoning or multi-step action 
is needed, or when {user} has explicitly asked for something.

{SOUL.md excerpt — who Kairo is and how Kairo behaves}

Current frame:
{perception_frame_json}

Recent memory summary:
{short summary of last 15 minutes}

Output (one JSON object, nothing else):
```

The triage LLM must respond in under 500 ms. If it takes longer than 2 seconds, Kairo logs a warning and considers quantization adjustment.

### Voice fast path

When the user speaks directly to Kairo (wake word detected or in active conversation mode), the triage LLM also handles **fast conversational responses**. A simple question like "what time is it" or "turn off the music" gets answered or executed by triage itself without ever waking Opus. This is what keeps voice latency under 500 ms for routine interactions.

Opus only gets called for voice input when the request involves reasoning, memory recall, or multi-step action.

---

## Layer 3 — Orchestrator

The orchestrator is Claude Opus 4.6, invoked via the official Claude Code CLI in headless mode. This is the only cloud component of Kairo.

### How Kairo spawns the orchestrator

Kairo Core spawns the orchestrator as a child process using Rust's `tokio::process::Command`. The command is:

```bash
claude \
  --print \
  --output-format stream-json \
  --input-format stream-json \
  --verbose \
  --include-partial-messages \
  --model claude-opus-4-6 \
  --append-system-prompt-file ~/.kairo/orchestrator-prompt.md \
  --mcp-config ~/.kairo/mcp-servers.json \
  --allowedTools "Read,Write,Edit,Bash,Task,mcp__kairo__*"
```

Kairo writes a single JSON message to the orchestrator's stdin:

```json
{"type": "user", "message": {"role": "user", "content": "<wake context>"}}
```

Where `<wake context>` is a structured payload containing the current perception frame, relevant memory recall results, active project info, and the reason the triage layer decided to wake up Opus. Opus then streams events back on stdout, one JSON object per line. Kairo Core parses those events in real time and can:

- Display the orchestrator's thinking in the dashboard live
- Stream text to the TTS pipeline as it arrives (so Kairo starts speaking before the full response is done)
- Capture tool calls and display them in the "watch mode" panel
- Spawn workers based on the orchestrator's instructions

### Orchestrator prompt

The orchestrator's system prompt is built by concatenating:

1. `SOUL.md` — Kairo's personality
2. `TOOLS.md` — documentation of every MCP tool and when to use it
3. A runtime header with current time, active user, and available workers

The orchestrator is instructed to **never do long tasks itself**. Its job is to plan, decide, and delegate. If a task takes more than a few tool calls, it must spawn a worker.

### Resume vs fresh sessions

Every wake-up is a fresh Claude Code session by default. This is intentional — it keeps context clean and costs low. For conversation continuity (e.g. the user and Kairo are in an ongoing back-and-forth), Kairo Core persists the session ID from the first wake-up and passes `--resume <session_id>` for follow-ups.

---

## Layer 4 — Workers

Workers are independent Claude Code sessions spawned by the orchestrator to do actual work. Each worker:

- Gets its own working directory (usually a project folder)
- Gets its own tool allowlist (narrower than the orchestrator's)
- Gets its own model (Sonnet 4.6 by default, Opus 4.6 for heavy tasks, user-configurable)
- Gets its own session ID and log file
- Reports progress back to the orchestrator via a structured status file or MCP callback

The orchestrator does not spawn workers directly via `tokio::process`. Instead, it calls the `mcp__kairo__spawn_worker` tool exposed by the Kairo MCP server. The MCP server then spawns the Claude Code process, captures its output, and streams progress back to the orchestrator and to the dashboard.

### Worker model selection

The user configures in the dashboard:

- **Budget mode** — all workers use Sonnet 4.6
- **Power mode** — all workers use Opus 4.6
- **Auto mode** — orchestrator decides per task (default)

In Auto mode, the orchestrator uses Sonnet for mechanical tasks (file organization, boilerplate code, summaries, email drafts) and Opus for reasoning-heavy tasks (architecture decisions, debugging, complex refactors).

### Worker concurrency

The dashboard exposes a `max_concurrent_workers` setting (default 3, max 10). Workers are queued when the limit is reached. Each worker's status is visible in the dashboard Home tab with live progress.

---

## The MCP tool layer

This is the heart of what makes Kairo more than a Claude Code wrapper. Kairo ships with a bundled **MCP server** written in Rust using the [rmcp](https://github.com/modelcontextprotocol/rust-sdk) crate. The orchestrator and workers get this MCP server registered automatically via the `--mcp-config` flag.

The Kairo MCP server exposes the following tool namespaces:

### `mcp__kairo__memory`

- `memory_query(query: string, limit: int)` — semantic search over episodic memory, returns top results
- `memory_write(content: string, tags: string[], importance: float)` — store something new
- `memory_fact_get(key: string)` — read a semantic fact
- `memory_fact_set(key: string, value: string)` — write a semantic fact
- `memory_recent(minutes: int)` — get raw log for last N minutes

### `mcp__kairo__perception`

- `perception_current()` — get the latest perception frame
- `perception_screenshot()` — force a fresh screenshot and return it as an image
- `perception_transcribe(seconds: int)` — transcribe the last N seconds of audio

### `mcp__kairo__voice`

- `voice_speak(text: string, interrupt: bool)` — speak text via TTS, optionally interrupt current speech
- `voice_listen(timeout_seconds: int)` — actively listen for user input and return transcription

### `mcp__kairo__windows`

- `windows_list_apps()` — list all open windows with titles and PIDs
- `windows_focus_app(pid: int)` — bring a window to foreground
- `windows_launch(path: string, args: string[])` — launch an application
- `windows_close_app(pid: int)` — close an application
- `windows_ui_click(element_ref: string)` — click a UI element by accessibility reference
- `windows_ui_type(text: string)` — type text into the active window
- `windows_clipboard_get()` — read clipboard
- `windows_clipboard_set(content: string)` — write clipboard
- `windows_notification(title: string, body: string)` — post a system notification

### `mcp__kairo__shell`

- `shell_run(command: string, cwd: string, elevated: bool)` — run a PowerShell or cmd command
  - `elevated: true` requires explicit user confirmation unless pre-approved for that session
- `shell_background(command: string, cwd: string)` — run a long-running command in the background

### `mcp__kairo__workers`

- `spawn_worker(task: string, cwd: string, model: string, tools: string[])` — spawn a new Claude Code worker
- `worker_status(worker_id: string)` — get current status of a worker
- `worker_cancel(worker_id: string)` — stop a worker

### `mcp__kairo__schedule`

- `schedule_once(when: datetime, task: string)` — schedule a one-shot wake-up
- `schedule_recurring(cron: string, task: string)` — schedule a recurring task
- `schedule_list()` — list all scheduled tasks
- `schedule_cancel(id: string)` — cancel a scheduled task

### `mcp__kairo__system`

- `system_health()` — get current component statuses
- `system_config_get(key: string)` — read a config value
- `system_config_set(key: string, value: any)` — write a config value (may require confirmation)

Every tool has a strict permission model. Destructive operations (shell commands, file deletes, elevated operations, config changes) require either a pre-approved allowlist rule or live user confirmation via the dashboard.

---

## Memory system

Memory is arguably the hardest part of Kairo and what separates it from every chatbot. Kairo uses three stores for three kinds of memory, mirroring the cognitive science distinction between raw experience, episodic memory, and semantic knowledge.

### Raw log (SQLite)

Everything the senses produce, stored verbatim. One row per perception frame. Includes screenshot thumbnails (base64 or filepaths), transcripts, and context. Used for forensic retrieval ("what was I doing at 14:23 yesterday").

**Retention:** default 30 days, configurable 1–365. Rotated nightly.

**Schema (simplified):**

```sql
CREATE TABLE perception_frames (
  id INTEGER PRIMARY KEY,
  ts TIMESTAMP NOT NULL,
  screen_description TEXT,
  screen_screenshot_path TEXT,
  audio_transcript TEXT,
  context_json TEXT,
  salience REAL,
  triage_decision TEXT
);
CREATE INDEX idx_frames_ts ON perception_frames(ts);
```

### Episodic memory (LanceDB)

Every 10–15 minutes, a small local LLM reads the last window of raw log entries and distills them into 1–5 episodic memory entries. These are the "things worth remembering" from the user's day.

**Entry shape:**

```rust
pub struct EpisodicMemory {
    pub id: Uuid,
    pub ts_start: DateTime<Utc>,
    pub ts_end: DateTime<Utc>,
    pub content: String,           // natural language summary
    pub embedding: Vec<f32>,       // fastembed vector
    pub tags: Vec<String>,         // ["simcharts", "debugging", "ProcedureLayer"]
    pub importance: f32,           // 0.0 to 1.0
    pub linked_facts: Vec<String>, // keys into semantic memory
}
```

Stored in a LanceDB table with vector indexing. Retrieved via semantic similarity when the orchestrator wakes up.

**Retention:** no automatic deletion. The user can delete entries manually or apply retention rules.

### Semantic memory (structured JSON/graph)

Stable facts about the user, their projects, their relationships, their preferences. This is the equivalent of "things Kairo just knows about you."

Stored as a single SQLite table plus a graph structure for relationships:

```sql
CREATE TABLE semantic_facts (
  key TEXT PRIMARY KEY,         -- "user.name", "project.simcharts.stack"
  value TEXT NOT NULL,          -- JSON-encoded value
  confidence REAL,              -- how sure Kairo is (learned facts have lower confidence than user-provided)
  source TEXT,                  -- "user_stated" | "observed" | "inferred"
  updated_at TIMESTAMP
);

CREATE TABLE semantic_edges (
  from_key TEXT,
  to_key TEXT,
  relation TEXT,                -- "owns", "works_on", "prefers", "dislikes"
  PRIMARY KEY (from_key, to_key, relation)
);
```

Examples of keys: `user.name`, `user.location`, `project.simcharts.repo_path`, `project.simcharts.stack`, `contact.jan.email`, `routine.morning_start_time`.

### Memory retrieval flow

When the orchestrator wakes up, Kairo Core runs a two-step retrieval:

1. **Vector search** in episodic memory using the current perception frame as the query (embedded via fastembed). Returns top 10.
2. **Re-rank** via the triage LLM: "Which of these 10 memories are most relevant to the current situation?" Returns top 3.

The top 3 episodic memories, plus all relevant semantic facts (selected by tag and key matching), are added to the orchestrator's context. The orchestrator never sees the raw log directly unless it explicitly queries `memory_recent`.

### Memory writing

The orchestrator can write to memory via the MCP tools (`memory_write`, `memory_fact_set`). When something important is discussed or decided, it writes a note. Over time, Kairo's memory grows organically without manual curation.

---

## Voice pipeline

Voice is what makes Kairo feel alive. It has to be low-latency, interruptible, and natural. The pipeline is:

```
Wake word detected  ──▶  Start streaming transcription (whisper.cpp)
                              │
                              ▼
                    Partial transcripts every ~300ms
                              │
                              ▼
                    Semantic endpoint detection
                    (triage LLM: "is this sentence complete?")
                              │
                              ▼
              ┌────────────────┴────────────────┐
              ▼                                 ▼
    Simple query                      Complex query
    (triage answers)                  (orchestrator answers)
              │                                 │
              ▼                                 ▼
    Stream text to TTS        Stream first tokens to TTS
    (Piper local)             as they arrive from Opus
              │                                 │
              └────────────────┬────────────────┘
                               ▼
                     Piper synthesizes audio
                     Audio playback starts
                               │
                               ▼
                     If user speaks again → interrupt
                     (stop playback <50ms, restart loop)
```

### Wake word

Picovoice Porcupine runs continuously on CPU with ~1% usage. Default wake word is "Hey Kairo" (custom-trained). Users can train their own or disable wake word entirely (always-listening mode where the triage LLM decides whether speech is addressed to Kairo).

### TTS options

- **Piper** (default) — local, fast, free, Dutch and English voices included
- **Kokoro TTS** — local, better quality than Piper, English only
- **ElevenLabs streaming** — best quality, cloud, requires API key, costs per character

### Interrupt handling

The microphone keeps listening while Kairo speaks. If the user starts talking, playback is cut within 50 ms and the new input goes into the pipeline. This is what makes it feel like a conversation instead of a walkie-talkie.

### Ambient mute

When Kairo detects that the user is in a Discord, Teams, Zoom, or Meet call (via the context watcher), it switches to a quiet mode: no spontaneous speech, only on-screen text responses, and any voice output happens at reduced volume through a secondary audio channel.

---

## Dashboard

The dashboard is a Tauri window opened from the system tray. It is the single place where the user configures, monitors, and repairs Kairo. It has the following tabs:

### Home

Real-time status: the status orb, current perception frame (text + thumbnail), live audio waveform, active workers, recent actions timeline, and resource usage.

### Brain

Model configuration for all four layers with dropdowns, test buttons, and a visual diagram showing how the layers connect.

### Memory

Three tabs for raw log, episodic memory, and semantic facts. Browsable, searchable, editable. Includes a prominent "wipe all memory" button.

### Tools

Lists all MCP tools and skills with toggles. Allows installing new MCP servers from a URL or npm package. Allows adding new skill files.

### Voice

Voice selection, speech rate, interrupt sensitivity, ambient mute rules, wake word configuration.

### Automations

List of scheduled tasks and trigger rules. Simple form-based creation.

### Logs

Searchable event log with filters by layer, severity, and time range.

### Health

Component status grid, recent errors, and the **Fix Issues** button that triggers the Repair Agent.

All tabs are live-updating via a WebSocket connection from the Kairo Core to the dashboard frontend.

---

## Self-healing subsystem

This is the feature that takes Kairo from "ambitious but fragile" to "a system you can trust."

### How it works

When the user clicks **Fix Issues** (or says "Kairo, something is broken"), Kairo Core:

1. Collects the last 500 log lines, all current component statuses, any stacktraces, and the current config snapshot.
2. Writes this context to `~/.kairo/repair-context.md`.
3. Spawns a dedicated Claude Code session with:
   - Working directory set to the Kairo install folder
   - Model forced to Claude Opus 4.6
   - A custom system prompt from `~/.kairo/repair-agent-prompt.md`
   - Full file system access to the Kairo installation
   - Access to a dedicated MCP tool set: `repair_restart_component`, `repair_reinstall_component`, `repair_rollback_config`, `repair_test_component`, `repair_escalate`
4. Streams the repair agent's output live to the dashboard Health tab.

The repair agent is instructed to:

- Diagnose the root cause from the logs
- Propose a fix
- Apply non-destructive fixes immediately (restart a process, reload config, clear a cache)
- Ask for confirmation before destructive fixes (reinstall a model, modify core config, rollback to backup)
- Test the fix by calling `repair_test_component`
- Report what it did and whether the issue is resolved

### Backup rotation

Every morning at 04:00, Kairo Core snapshots the entire `~/.kairo` directory (excluding the raw log and memory stores) to `~/.kairo-backups/<date>/`. The repair agent has read access to the last 7 backups and can rollback to any of them if a fix goes wrong.

### Predictive maintenance

The self-diagnose routine runs nightly: checks every component's response time, error rate, and resource usage. If a component is degrading, it logs a warning and offers (via the Health tab) to have the repair agent investigate preemptively.

### Voice-activated repair

The user can trigger the repair agent with voice: *"Kairo, something isn't right, can you check?"* The orchestrator routes this to the repair subsystem and reports back by voice when done.

---

## Security and permissions

Kairo has access to your computer. Trust is earned through transparency and explicit permissions.

### Tool permission tiers

Every MCP tool has one of four permission levels:

- **Auto** — can be called without confirmation (read operations, voice output, memory reads)
- **Session-approved** — requires confirmation once per session, then allowed (most shell commands, file writes in specified directories)
- **Always-confirm** — requires confirmation every single call (elevated shell, file deletes, sending messages, financial actions)
- **Blocked** — cannot be called at all unless the user explicitly enables it (modifying Kairo's own installation except via repair agent, accessing password stores, anything touching credentials)

The defaults are set in `~/.kairo/permissions.toml` and visible in the dashboard.

### Per-folder policies

The user configures which folders are read-write, read-only, or off-limits to the orchestrator and workers. By default, `~/.kairo`, system folders, and credential stores are off-limits.

### Audit log

Every tool call is logged with timestamp, caller (orchestrator or specific worker id), arguments (redacted for sensitive fields), and result. The audit log is append-only and never automatically deleted.

### No telemetry

Kairo does not phone home. Ever. There is no usage tracking, no crash reporting to third parties, no "anonymous analytics." The only network calls Kairo makes are to the Anthropic API (via Claude Code) and optionally to ElevenLabs (if the user enables premium TTS).

---

## Directory layout

The repository is a monorepo using pnpm workspaces for JavaScript/TypeScript parts and a Cargo workspace for Rust parts.

```
kairo-ai/
├── README.md
├── ARCHITECTURE.md
├── CLAUDE.md
├── SOUL.md
├── ROADMAP.md
├── CONTRIBUTING.md
├── LICENSE
├── CHANGELOG.md
│
├── Cargo.toml                    # Rust workspace root
├── package.json                  # pnpm workspace root
├── pnpm-workspace.yaml
├── rust-toolchain.toml
│
├── apps/
│   └── desktop/                  # Tauri desktop app
│       ├── src-tauri/            # Rust backend
│       │   ├── Cargo.toml
│       │   └── src/
│       │       ├── main.rs
│       │       ├── commands.rs   # Tauri command handlers
│       │       └── tray.rs       # System tray integration
│       ├── src/                  # Next.js frontend
│       │   ├── app/
│       │   ├── components/
│       │   └── styles/
│       ├── package.json
│       └── tailwind.config.ts
│
├── crates/
│   ├── kairo-core/               # Main orchestration runtime
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── senses/           # Layer 1
│   │       │   ├── vision.rs
│   │       │   ├── audio.rs
│   │       │   └── context.rs
│   │       ├── triage/           # Layer 2
│   │       │   ├── mod.rs
│   │       │   ├── llm.rs
│   │       │   └── prompts.rs
│   │       ├── orchestrator/     # Layer 3
│   │       │   ├── mod.rs
│   │       │   ├── spawn.rs
│   │       │   └── stream.rs
│   │       ├── workers/          # Layer 4
│   │       │   ├── mod.rs
│   │       │   ├── pool.rs
│   │       │   └── supervisor.rs
│   │       ├── memory/
│   │       │   ├── raw_log.rs
│   │       │   ├── episodic.rs
│   │       │   └── semantic.rs
│   │       ├── voice/
│   │       │   ├── wake.rs
│   │       │   ├── stt.rs
│   │       │   └── tts.rs
│   │       ├── health/
│   │       │   └── repair.rs
│   │       └── config.rs
│   │
│   ├── kairo-mcp/                # MCP server exposing Windows tools
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── tools/
│   │       │   ├── memory.rs
│   │       │   ├── perception.rs
│   │       │   ├── voice.rs
│   │       │   ├── windows.rs
│   │       │   ├── shell.rs
│   │       │   ├── workers.rs
│   │       │   ├── schedule.rs
│   │       │   └── system.rs
│   │       └── permissions.rs
│   │
│   ├── kairo-llm/                # Local LLM runtime (llama.cpp wrapper)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   │
│   └── kairo-vision/             # Local vision model runtime
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
│
├── skills/                       # Bundled Kairo skills (SKILL.md files)
│   ├── README.md
│   ├── simcharts-dev/
│   │   └── SKILL.md
│   ├── tovix-client-onboarding/
│   │   └── SKILL.md
│   └── daily-briefing/
│       └── SKILL.md
│
├── docs/
│   ├── getting-started.md
│   ├── configuration.md
│   ├── voice.md
│   ├── memory.md
│   ├── mcp-tools.md
│   ├── skills.md
│   ├── self-healing.md
│   └── images/
│
├── prompts/                      # Orchestrator and triage prompts
│   ├── orchestrator-system.md
│   ├── triage-system.md
│   ├── repair-agent-system.md
│   └── salience-heuristics.md
│
├── config/
│   ├── default-permissions.toml
│   ├── default-models.toml
│   └── default-mcp-servers.json
│
├── scripts/
│   ├── install.ps1
│   ├── download-models.ps1
│   └── dev-setup.ps1
│
└── .github/
    └── workflows/
        ├── ci.yml
        ├── build-desktop.yml
        └── release.yml
```

---

## Key design decisions

These are the decisions that shape everything else. Change them only with deliberate cause.

**Tauri over Electron.** Smaller binary, native performance, Rust backend gives us direct access to Windows APIs without FFI gymnastics. Electron would work but would double our install size and lose us native UI Automation access.

**Rust for Kairo Core and the MCP server.** Performance matters because Layer 1 runs 24/7. Rust gives us zero-cost abstractions over Windows APIs and clean integration with llama.cpp, whisper.cpp, and LanceDB. TypeScript would be faster to prototype but slower at runtime and messier for Windows COM interop.

**Claude Code as subprocess, not SDK.** The Agent SDK exists in Python and TypeScript but tying Kairo to it would mean bundling a language runtime and fighting version drift. The `claude` CLI is the official, stable, language-agnostic contract. We spawn it and talk JSON.

**Stream-json for both directions.** We use `--input-format stream-json --output-format stream-json` for all orchestrator calls. This gives us bidirectional structured communication, live tool call visibility, and the ability to feed follow-up messages into a running agent loop.

**MCP for extending Claude Code, not replacing it.** Claude Code already handles the hard parts (tool loop, file editing, sub-agents, context management). We add Windows-specific capabilities via MCP and let Claude Code drive. This also means anyone else's MCP server works with Kairo out of the box.

**LanceDB over Chroma/Qdrant.** LanceDB is embedded (no server), Rust-native, and designed for on-device use. Chroma is Python-first. Qdrant is a server product. LanceDB is the right choice for a local-first desktop app.

**Piper as default TTS.** Free, fast, local, Dutch support, actively maintained. ElevenLabs is better-sounding but we refuse to require an API key for core functionality.

**Apache 2.0 over MIT.** Explicit patent grant protects contributors. Slightly more enterprise-friendly. No practical downsides for a permissively licensed project.

**Monorepo over multi-repo.** Single clone, single CI, single version, Claude Code can see everything. We can split later if the project grows large enough to warrant it. Right now it would just add friction.

---

Last updated: 2026-04-10. This document is authoritative. If code and this document disagree, fix the document first, then the code, or fix the code first and update the document in the same commit.