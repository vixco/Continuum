# Continuum Architecture

This document is the authoritative technical blueprint for Continuum. It describes every layer of the system, how data flows between them, how tools are exposed, how memory is stored, and how the self-healing subsystem works. Read this before writing any code, and update it before changing any major design decision.

## Contents

1. [Design philosophy](#design-philosophy)
2. [The four cognitive layers](#the-four-cognitive-layers)
3. [Data flow](#data-flow)
4. [Layer 1 — Senses](#layer-1--senses)
5. [Layer 2 — Triage](#layer-2--triage)
6. [The context engine](#the-context-engine)
7. [Layer 3 — Orchestrator](#layer-3--orchestrator)
8. [Layer 4 — Workers](#layer-4--workers)
9. [The MCP tool layer](#the-mcp-tool-layer)
10. [Memory system](#memory-system)
11. [Voice pipeline](#voice-pipeline)
12. [Dashboard](#dashboard)
13. [Self-healing subsystem](#self-healing-subsystem)
14. [Security and permissions](#security-and-permissions)
15. [Directory layout](#directory-layout)
16. [Key design decisions](#key-design-decisions)

---

## Design philosophy

Continuum follows five architectural rules that drive every decision below them.

**Rule 1 — Cost scales with intelligence.** Every task should be handled by the cheapest layer that can do it correctly. A screenshot of your browser is processed by a 0.23B vision model, not by Claude Opus. A question like "what time is it?" is answered by a 3B local LLM, not by a round trip to the Anthropic API. Opus only wakes up when a task genuinely requires reasoning, planning, or multi-step tool use.

**Rule 2 — Perception is first-class.** Continuum is not a chatbot that happens to have eyes. It is an observation system that happens to speak. The perception layer runs 24/7 and produces a continuous stream of structured frames. Every other layer is a consumer of that stream.

**Rule 3 — Official subprocess over custom integration.** Continuum does not call the Anthropic API directly. It does not scrape OAuth tokens. It invokes the officially supported `claude` CLI in headless mode as a child process and communicates via stdin/stdout. This is the only approach that is legal, stable, and will keep working as Claude evolves.

**Rule 4 — Configuration beats assumption.** Every model, every sample rate, every retention policy, every voice, every tool permission is exposed in the dashboard. Continuum ships with sensible defaults but assumes nothing.

**Rule 5 — The system must be able to repair itself.** A cognitive assistant that breaks and requires terminal debugging has failed its users. Continuum includes a Repair Agent — a dedicated Claude Code session with access to its own installation — that can diagnose and fix component failures on demand.

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
│  │  Prompt: You are Continuum's triage layer...                │  │
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

The key insight is that **the orchestrator rarely runs**. In normal use it wakes up 20 to 50 times per day. Workers run even less often and finish in seconds or minutes. The bulk of Continuum's activity — 99.9% of all tokens processed — happens in Layer 1 and Layer 2, which are free and local.

---

## Layer 1 — Senses

The senses layer runs as a dedicated subprocess inside Continuum Core. It has one job: produce a steady stream of `PerceptionFrame` objects and push them to the triage layer via an internal queue.

### Vision watcher

Enumerates every connected monitor through `xcap` and gives each display an
independent capture worker with a stable `display-<xcap id>` identity. The
default target is one best-effort capture per monitor every 20 ms (50 captures
per second). Capture itself is mechanical and contains no AI. A bounded ordered
FIFO decouples it from local vision inference: unchanged samples update only
health/current-state metadata, while selected keyframes enter the queue;
overload drops the oldest pending keyframe, increments explicit degradation
counters, and never silently pauses capture. A cheap 64×36 luma difference
selects meaningful changes against the last selected keyframe so gradual change
accumulates. On Windows the hot path samples directly at 64x36 through GDI
`StretchBlt`; selected frames are captured directly at the configured keyframe
resolution before queue admission. `xcap` remains the display-enumeration and
capture-fallback boundary. The
single local vision consumer runs continuously at actual model throughput.
Twenty milliseconds is a target, not a claim that the OS capture API or VLM can
always sustain 50 completed captures or inferences per second.

Visual summaries join foreground-window, coarse idle/active input, and local
terminal/project events in the shared `live-context.json` projection. The
`ScreenObservation` carries both halves side by side: `description` is the
one-sentence vision caption that triage, memory and the dashboard read, and
`world_compact` is a compact version of that projection reserved for the
context packager (context engine spec §4.10 — the blob is deliberately kept
out of the triage prompt's token budget). `system_live_context` makes the same
source-attributed state available to agent roles that do not accept images. Sensitive foreground
applications/titles fail closed to redacted context, and screenshot persistence
is disabled unless the user explicitly enables it.

Windows capture uses direct GDI `StretchBlt` for scaled samples and keyframes,
with `xcap` as the automatic fallback. GDI was chosen over the Windows Graphics
Capture API because WGC shows a yellow border on Windows 10 (the
`IsBorderRequired = false` flag is Windows 11 only), which is unacceptable for
ambient polling.

**Default model:** SmolVLM2-2.2B Instruct, quantized to Q4_K_M, through
llama.cpp's MTMD pipeline. A verified SmolVLM-500M ONNX pipeline is the
automatic load/warmup fallback; SmolVLM-256M remains the low-resource option.
Both paths run fully locally and produce compact text observations rather than
exposing raw screenshots above the Senses layer.

**Alternatives user can select:**
- Moondream 2 (1.8B) — better captioning quality, recommended for GPU users
- Florence-2 base (0.23B) — even smaller, faster, better at structured extraction (OCR, object detection) than free-form description
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

Pure Rust code that polls the OS once per second. On Windows it uses the Win32
foreground-window, UI Automation, and Media Session APIs; on macOS it enumerates
on-screen windows via the Core Graphics `CGWindowList` API and resolves the
focused window's title through the Accessibility framework
(`AXFocusedWindow` / `AXTitle`). The macOS implementation is built from
lightweight system-framework bindings (not heavy native build deps), so it
compiles under `--no-default-features` on macOS and the desktop build gets a
working watcher rather than an empty stub. It captures:

- Foreground window title and process name
- Active file path from editors that expose it via UI Automation (VS Code, JetBrains, Sublime, etc.)
- Currently playing media (via Windows Media Session)
- Idle time (last user input)
- Whether the user is in a call (detects Discord, Teams, Zoom, Meet) — process-name and title-keyword matching is platform-aware (`Discord.exe` on Windows vs `Discord` on macOS, etc.)
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

The `salience_hint` is a rough pre-filter that prevents the triage LLM from being called for totally uninteresting frames. This is a classical rule-based calculation, not an ML model. The shipped rules (`senses/frame.rs::compute_salience`) are additive and clamped to `[0.0, 1.0]`:

- First frame ever? salience = 0.5 (nothing to compare against, always look)
- New error visible on screen? += 0.3
- Error that was visible has disappeared? += 0.1
- Frame carries a non-empty audio transcript? += 0.4
- Foreground process or window title changed since the previous frame? += 0.2
- A focus switch happened *between* frame ticks that the frame-to-frame comparison missed? += 0.2 (`accumulate_switch_salience`, never double-counted with the rule above)

A frame in which none of these fire scores 0.0. Only frames at or above `[frame] salience_threshold` (default **0.10**) reach the triage layer; everything else is stored in the raw log and dropped. See `prompts/salience-heuristics.md`.

---

## Layer 2 — Triage

The triage layer is a small local LLM (3–4B parameters) that reads every salient perception frame and decides what to do. It is the gatekeeper that decides whether to spend money on Opus or not.

**Default model:** Qwen 3 8B (Q4_K_M quantization, ~5 GB RAM, 15–20 tokens/sec CPU, 80+ GPU). Benchmarked at 95% accuracy on our perception-frame decision set; the 4B variant tops out around 82% on the same bench, and the extra RAM turned out to be worth it for the gatekeeper layer.

Qwen 3 has a "thinking" mode, and triage runs with it **disabled**: `build_triage_prompt` wraps the call in ChatML precisely so the `/no_think` directive at the start of the user turn suppresses thinking tokens, and a GBNF grammar constrains the output to the decision JSON. The 95% figure above is the benchmark on that shipped configuration — `continuum-triage-bench` calls the same `build_triage_prompt` and the same grammar, so it measures what the runtime actually does. Thinking is off because triage is a latency-critical gate on every salient frame, not because it never helps; re-enabling it would mean removing `/no_think`, re-baselining the bench, and paying the decode cost per frame.

**Low-RAM alternative:** Qwen 3 4B (Q4_K_M, ~2.5 GB) for systems with ≤6 GB available RAM; 30–35 tokens/sec on CPU but ~13 percentage points less accurate on the same triage benchmark. Qwen 2.5 3B Instruct is still loadable for legacy setups but not recommended.

**Other alternatives:**
- Gemma 3 4B (good Dutch, weaker at strict JSON grammar compliance)
- Phi-4 mini 3.8B (strong JSON, weak Dutch — not recommended)
- Llama 3.2 3B (fast, weak Dutch — not recommended)
- Any GGUF-compatible model via local file path

### Triage prompt

The triage LLM gets a short structured prompt on every call:

```
You are the triage layer of Continuum, {user}'s personal AI assistant.
You are not Continuum. You are the part of Continuum that decides whether Continuum should act.

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

{SOUL.md excerpt — who Continuum is and how Continuum behaves}

Current frame:
{perception_frame_json}

Recent memory summary:
{short summary of last 15 minutes}

Output (one JSON object, nothing else):
```

**GPU acceleration:** CUDA GPU offload is enabled by default and recommended for all users with NVIDIA GPUs. With GPU offload, prompt processing runs at 1000+ tokens/sec (vs ~45 tok/s on CPU), bringing triage latency from ~12 seconds to under 1 second. Users without compatible GPUs fall back to CPU automatically — llama.cpp detects GPU availability at runtime. For CPU-only users, consider the Qwen 2.5 3B model (smaller, faster on CPU).

The triage LLM must respond in under 500 ms. If it takes longer than 2 seconds, Continuum logs a warning and considers quantization adjustment.

### Session state — what the user is doing

`context::session_state` (context engine spec §4.8) holds Continuum's live
answer to "what is the user doing right now": active project, app, window
title, best-effort open files, last error, last success, last user command,
plus an inferred goal and task with a confidence and a `local_only` zone tag.

The two halves have very different costs and are built accordingly. The
mechanical fields update **synchronously** — from the frame loop, and from a
non-blocking observer tap on the context-events channel that runs *before*
the persistence queue, so a full queue costs a persisted row but never a
state update. Goal/task inference costs a local-LLM call and therefore runs
in its **own spawned task** the frame loop never awaits: it fires only on a
project switch, on ≥ `infer_min_new_events` significant events, or on
staleness, never more than once per `infer_min_interval_secs`, never while
the machine is idle (§4.11), and always through the background tier of
`LlmGate` (behind interactive triage, `max_tokens ≤ 256`). A reply under
`confidence_floor` is discarded rather than stored — consumers render
"unknown", which is the honest answer.

On boot the state rehydrates from the last published `state.json` snapshot
plus the most recent `context_events`, with confidence discounted by age, so
a restart does not erase what Continuum knew a minute ago.

Session state is what feeds the triage prompt's `memory_summary`, the skills
matcher's task/project, and (Plan B/C) the context packager, the continuation
resolver and the dashboard's Context page. Cloud-bound renders must go
through `cloud_view()`, which collapses goal/task to "working in a private
context" whenever the inference window contained a `local_only` event
(spec §4.1 zone propagation).

### Voice fast path

When the user speaks directly to Continuum (wake word detected or in active conversation mode), the triage LLM also handles **fast conversational responses**. A simple question like "what time is it" or "turn off the music" gets answered or executed by triage itself without ever waking Opus. This is what keeps voice latency under 500 ms for routine interactions.

Opus only gets called for voice input when the request involves reasoning, memory recall, or multi-step action.

---

## The context engine

The context engine is not a fifth layer. It is the spine that runs *through*
Layers 1–3: it takes what the senses produce, decides what it means, remembers
the part that matters, and hands every AI — orchestrator wake, desktop chat, or
tool call — the same privacy-filtered picture. The specification is
`docs/superpowers/specs/2026-08-05-context-engine.md`; the user-facing guide is
`docs/context-engine.md`.

### The pipeline

```
capture → dedupe → privacy filter → classification → project/goal → session state
        → curator/memory → context package → Main AI
```

| Stage | Where it lives | What it does |
|---|---|---|
| capture | `senses/{context,vision,audio}.rs`, `senses/git_watch.rs`, `senses/file_watch.rs` | Five collectors. The context watcher polls at 1 Hz and emits enriched observations (pid, exe path, monitor, dwell) plus focus-switch diffs; the git collector watches the active *confirmed* project only; the file watcher is opt-in and watches confirmed roots only. |
| privacy | `senses/privacy.rs` | The choke point, applied **at collector emit** — before the hub, before persistence, before any model, before any tool response. Scrubbers on free text, path scrubbing everywhere, and three zones (`never_observe` → sentinel observation, `local_only` → stored but never cloud-bound, `cloud_allowed`). Derived artifacts inherit the strictest zone of their inputs. |
| dedupe | `memory/events.rs` | Template sources key on `hash(source, event_type, project_id, normalized_summary)`; classified screen/audio events deliberately **exclude the summary** from the key so an LLM's varying phrasing still collapses; per-path file events key on the **raw** summary, because that summary *is* the path and normalization strips paths (the storm templates stay normalized). `count` is bumped, `ts_last` re-anchored, first summary kept as display text. |
| classification | `triage/{mod,prompts,consume}.rs` | Rides the existing triage call — no second GPU pass. `TriageOutput` flattens the decision and carries an optional classification block; a malformed block never costs a retry. Consumption writes an event row, optionally a vault candidate with a per-type TTL, and the `triage_decision` raw-log column. |
| project/goal | `context/project.rs` | One resolver, owned by the frame loop, resolved once per frame with hysteresis. Tiers: user override rules → path in the window title → editor title pattern → git root of the most recent file event → keyword match. Its single output feeds session state, the collectors and every event's `project_id`. |
| session state | `context/session_state.rs` | Mechanical fields update synchronously; goal/task inference runs in its own spawned task, gated on idle and behind interactive triage. Rehydrates on boot from the last snapshot plus recent events, confidence discounted by age. |
| memory | `memory/{distill,episodic}.rs` | The distiller reads deduped **events** first (count-aware: "build failed ×14", gated by `distillation_min_event_importance`) and falls back to the SQL frame predicate (gated by `distillation_min_salience`) for frames that produced no usable event — the writer stamps `perception_frames.context_event_at` on every occurrence, including collapses, so a moment is never recorded twice. Episodic memories carry a `project` **and** the §4.1 `sensitivity` of the text they summarize, so a `local_only` memory is withheld at every cloud egress. |
| package | `context/package.rs` | One struct, one renderer, three assembler profiles. |
| continuation | `context/continuation.rs` | Ranks what "ga door" should resume. Pure logic, no LLM call. |

### Event transport

One `mpsc::Sender<ContextEvent>` is cloned into every collector. A dedicated
**events-writer task** owns the receiver, applies dedupe, and batch-inserts —
not the frame loop, not per-collector writes. Collectors never block on SQLite.
The queue is bounded (`[events] queue_cap`); because tokio's channel cannot pop
the oldest, overflow policy is **drop-newest**, counted per source and coalesced
by the writer into `"N events dropped from <source>"` system rows, so loss is
visible in the events table itself. Switch events additionally ride the frame
builder's accumulation path *for salience only*, never for persistence.

Session state taps the same stream through a synchronous observer installed
*after* the registry check but *before* the queue, so a full queue costs a
persisted row but never a state update.

### The three package profiles

`context::package` is deliberately **ungated** (pure owned types plus string
rendering, no `runtime` feature), which is what lets all three processes link
the same renderer; the `--no-default-features` build is the parity gate.

| Profile | Process | Sections | Sources |
|---|---|---|---|
| Wake | `continuum` runtime | all | in-process hubs, the shared frame ring, episodic + semantic retrieval |
| `context_package` tool | `continuum-mcp` | all but why-woken, trigger-frame moment, tools, recommended next step, pending decisions and facts; adds per-section `stale` flags | `state.json`, `live-context.json`, `context_events` read-only, its own lazily-opened vault and episodic stores |
| Chat | desktop (Tauri) | session state, plus the existing in-process vault search | `state.json`; episodic is explicitly unavailable |

The renderer owns three guarantees: the **cloud gate** (every item carries a
`local_only` flag; a cloud-bound render skips those items and *generalizes* the
two sections that can never be empty), the **order contract** (pending decisions
are last before why-woken), and the **budget** (per-section caps first, then a
drop ladder — open files → recent changes → the tail of "just before" → the tail
of memories — with why-woken, current moment, session state and pending
decisions never dropped).

### Storage

All DDL for the raw-log database lives in `memory/raw_log.rs` and the runtime is
the single writer. Every other process opens it through a read-only constructor
that runs no migrations, sets `PRAGMA query_only = ON` and a 2 s busy timeout,
and reports a typed "not yet created" for a cold database rather than an error.

| Table | Holds |
|---|---|
| `perception_frames` | one row per frame, verbatim; gained `screen_world_compact`, a populated `triage_decision`, `context_privacy` (the collector's resolved zone, NULL for rows written before it existed) and `context_event_at` (when this frame first became a `context_events` row — the distiller's "already recorded" marker, which a collapse cannot express through `raw_reference`) |
| `context_events` | the deduped event stream: `id, ts_first, ts_last, count, source, application, window_title, project_id, event_type, summary, importance, confidence, sensitivity, raw_reference, dedupe_key` |
| `context_events_fts` | FTS5 external-content mirror over `summary, window_title`, maintained by triggers — never written directly |
| Projects table | config mirror, discovered candidates, override rules, session pins, per-project stats and zone |

The `event_type` registry is **closed and additive-only**, with the same
stability policy as the MCP schemas: `window` emits `focus_switch` /
`project_switch`; `git` emits `commit` / `branch_switch` / `conflict` /
`dirty_change`; `file` emits the four file kinds plus `files_bulk_change`;
`screen` and `audio` emit the eight classification types; `system` emits
`idle_start`, `idle_end`, `wake`, `wake_result`, `voice_command`,
`toggle_change`, `source_unavailable` and `events_dropped`.

Published files: `state.json` (every 2 s regardless of content — it carries
`session_state`, `context_engine` health and the Context page snapshot) and
`live-context.json` (written **only when its content-version counter moved**, so
a stale mtime during a quiet period is expected, not a stall).

### Benchmarks

`crates/continuum-core/benches/data/` holds the only committed fixture — a
synthetic, generated 20-minute narrative plus a labels sidecar. Real recordings
(`continuum-perception --record`) are post-privacy but are exactly the content
the privacy work protects: they stay local and never enter the repository.

---

## Layer 3 — Orchestrator

The orchestrator is Claude Opus 4.6, invoked via the official Claude Code CLI in headless mode. This is the only cloud component of Continuum.

### How Continuum spawns the orchestrator

Continuum Core spawns the orchestrator as a child process using Rust's `tokio::process::Command`. The command is:

```bash
claude \
  --print \
  --output-format stream-json \
  --input-format stream-json \
  --verbose \
  --include-partial-messages \
  --model claude-opus-4-6 \
  --append-system-prompt-file ~/.continuum/orchestrator-prompt.md \
  --mcp-config ~/.continuum-dev/mcp-config-<nonce>.json \
  --strict-mcp-config \
  --allowedTools "mcp__continuum__*" \
  --permission-mode default
```

The allowed-tools list is intentionally restricted to the Continuum MCP namespace — the orchestrator has **no access to `Bash`, `Read`, `Write`, `Edit`, or `Task`**. Every file read, code edit, and shell-adjacent action flows through an explicit Continuum MCP tool so it can be audited, permission-gated, and mocked during repair. Workers (Layer 4) get a broader allowlist that includes the Claude Code built-ins; orchestrators do not. The MCP config file name carries a per-wake nonce so parallel wakes cannot clobber each other's config. The model ID, wake timeout, and bare-mode flag come from `[orchestrator]` in `config.toml` — none of these are hardcoded in the runtime (non-negotiable #3).

Continuum writes a single JSON message to the orchestrator's stdin:

```json
{"type": "user", "message": {"role": "user", "content": "<wake context>"}}
```

Where `<wake context>` is a structured payload containing the current perception frame, relevant memory recall results, active project info, and the reason the triage layer decided to wake up Opus. Opus then streams events back on stdout, one JSON object per line. Continuum Core parses those events in real time and can:

- Display the orchestrator's thinking in the dashboard live
- Stream text to the TTS pipeline as it arrives (so Continuum starts speaking before the full response is done)
- Capture tool calls and display them in the "watch mode" panel
- Spawn workers based on the orchestrator's instructions

### Orchestrator prompt

The orchestrator's system prompt is built by concatenating:

1. `SOUL.md` — Continuum's personality
2. `TOOLS.md` — documentation of every MCP tool and when to use it
3. A runtime header with current time, active user, and available workers

The orchestrator is instructed to **never do long tasks itself**. Its job is to plan, decide, and delegate. If a task takes more than a few tool calls, it must spawn a worker.

### Rate limiting

The Claude Code CLI emits `rate_limit_event` objects in the stream-json output. These events contain `resetsAt` (ISO timestamp), `rateLimitType` (e.g. `"token"`, `"request"`), and `overageStatus` fields. As of CLI v2.1.100 (April 2026), this event type is undocumented but reliably emitted.

Continuum Core's stream parser already recognizes and deserializes `rate_limit_event`. Consumer logic (backoff, queue pausing, user notification) is deferred to Phase 3 (orchestrator) and Phase 7 (self-healing). For now, the event is logged at `warn` level and stored in the session metadata.

### Resume vs fresh sessions

Every wake-up is a fresh Claude Code session by default. This is intentional — it keeps context clean and costs low. For conversation continuity (e.g. the user and Continuum are in an ongoing back-and-forth), Continuum Core persists the session ID from the first wake-up and passes `--resume <session_id>` for follow-ups.

---

## Layer 4 — Workers

Workers are independent Claude Code sessions spawned by the orchestrator to do actual work. Each worker:

- Gets its own working directory (usually a project folder)
- Gets its own tool allowlist (narrower than the orchestrator's)
- Gets its own model (Sonnet 4.6 by default, Opus 4.6 for heavy tasks, user-configurable)
- Gets its own session ID and log file
- Reports progress back to the orchestrator via a structured status file or MCP callback

The orchestrator does not spawn workers directly via `tokio::process`. Instead, it calls the `mcp__continuum__spawn_worker` tool exposed by the Continuum MCP server. The MCP server then spawns the Claude Code process, captures its output, and streams progress back to the orchestrator and to the dashboard.

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

This is the heart of what makes Continuum more than a Claude Code wrapper. Continuum ships with a bundled **MCP server** written in Rust using the [rmcp](https://github.com/modelcontextprotocol/rust-sdk) crate. The orchestrator and workers get this MCP server registered automatically via the `--mcp-config` flag.

The Continuum MCP server exposes these tools. Permission tiers are defined in `config/default-permissions.toml` and overridable via `~/.continuum/permissions.toml`.

### Shipped in v0.1.0-alpha

These are registered by `crates/continuum-mcp/src/server.rs` today; CI keeps the list below in sync with `config/default-permissions.toml` via an integration test that loads both and diffs them.

**Memory (`mcp__continuum__memory_*`)**
- `memory_query_episodic(query, limit?)` — vector search over episodic memory
- `memory_list_facts(prefix?, limit?)` — list facts by dotted key prefix (vault-first, legacy `semantic.sqlite` fallback)
- `memory_get_fact(key)` — fetch one fact (vault-first, legacy fallback)
- `memory_set_fact(key, value, source?)` — upsert a fact; writes a `type: fact` vault note (confidence clamped by source)

**Memory vault (the four `mcp__continuum__memory_vault_*` tools, plus `memory_wipe_all`, Plan B)**
- `memory_vault_search(query, types?, project?, limit?)` — full-text search over vault notes
- `memory_vault_get(id)` — fetch one note's full frontmatter, body, and backlinks
- `memory_vault_save(type, title, body, project?, confidence?, importance?, tags?, relations?, source_ref?)` — create, or update-by-title, a confirmed vault note
- `memory_vault_resolve(id, action, replaces?)` — confirm / reject / supersede a candidate note
- `memory_wipe_all(confirm)` — queue a derived-data wipe (`confirm` must be the literal string `"WIPE"`); vault markdown is never deleted; not part of the `memory_vault_*` name family despite living in this section

**System (`mcp__continuum__system_*`)**
- `system_current_time()` — local wall-clock + tz + epoch ms
- `system_active_window()` — foreground window title + process name
- `system_live_context()` — the shared local live world-state (compact text + structured state)
- `system_clipboard_get()` — best-effort clipboard read (text only)
- `system_notification(title, body)` — Windows toast, rate-limited to 1 / 10 s per session

**Context (`mcp__continuum__context_*`, context engine spec §5.2)** — read-only, privacy-gated, degrade to empty/`stale` answers instead of erroring, and emptied entirely by `[context_tools] enabled = false` or the matching `[privacy.toggles]` source toggle
- `context_session()` — live session state (project, goal, task, confidence, last error/success/command) from `state.json`
- `context_window(limit?)` — foreground window with pid / exe path / monitor / dwell, plus recent focus switches from `context_events`
- `context_screen()` — per-monitor captions with zone markers + the compact world render, from `live-context.json`
- `context_audio()` — latest published transcript + timestamp + in-call / ambient-mute state
- `context_projects()` — configured, confirmed, and discovered projects from the Projects table (read-only), active first
- `context_timeline(since?, until?, types?, project?, source?, limit?)` — deduped events with counts from `context_events` (limit clamped to 200); withheld private rows are reported as `omitted_private`, never silently dropped
- `context_search(query, limit?)` — full-text search over event summaries and window titles (limit clamped to 50); the query is normalized in the store, so it can neither raise an FTS syntax error nor inject operators
- `context_files(project?, limit?)` — recent file events (limit clamped to 100)
- `context_git(project?)` — no argument reads the published project state with no subprocess; naming a project runs a timeout-bounded read-only probe, **refused with a reason (never an error) for any project the user has not confirmed**
- `context_package(token_budget?)` — the mcp-published assembler profile, with `sections_present` / `sections_omitted`, four independent per-section staleness flags, and the drop-ladder rungs that ran

The desktop chat provider lists all ten explicitly (adding a tool to the MCP server is deliberately not the same act as granting chat access to it). The orchestrator reaches them through its `mcp__continuum__*` wildcard. Workers do not get them under the default `[workers] default_allowed_tools`.

**Filesystem (`mcp__continuum__fs_*`, read-only, path-allowlisted)**
- `fs_read_file(path)` — up to 100 KB UTF-8 text; paths must fall inside Continuum data dir, `project.*.dir` facts, or `[mcp.fs].extra_paths`
- `fs_list_dir(path)` — up to 500 entries with kind / size / mtime

**Web**
- `web_fetch(url)` — HTTP GET only, public-IP enforcement with DNS pinning (no rebinding), 5 s timeout, 50 KB cap, no redirects

**Repair (`mcp__continuum__repair_*`, blocked by default, unlocked inside a repair session)**
- `repair_restart_component(component)` — queue a restart intent
- `repair_reinstall_model(component)` — queue a model reinstall intent
- `repair_rollback_config(date)` — restore a dated config backup
- `repair_test_component(component)` — quick health sanity check
- `repair_escalate(message)` — post a user-visible red banner on the Health tab

**Workers (`mcp__continuum__workers_*`)**
- `workers_spawn_worker(task, cwd, model?, tools?)` — queue a worker spawn intent
- `workers_worker_status(worker_id)` — snapshot
- `workers_worker_wait(worker_id, timeout_secs?)` — block until terminal state
- `workers_worker_list(status?, limit?)` — recent snapshots
- `workers_worker_cancel(worker_id)` — queue a cancellation intent

### Planned (not yet shipped)

These tool namespaces are reserved by the architecture but **not exposed in the alpha**. The MCP server does not register handlers for them; adding one requires updating this section, `config/default-permissions.toml`, and the server in the same PR. A missing handler would return a protocol-level "unknown tool" error — no silent fallback.

- **`mcp__continuum__perception_*`** — live frame / screenshot / transcription reads for the orchestrator (currently, the orchestrator sees the trigger frame embedded in the wake context; this namespace would expose on-demand reads)
- **`mcp__continuum__voice_*`** — programmatic TTS (`voice_speak`) and push-to-listen (`voice_listen`)
- **`mcp__continuum__windows_*`** — focus, launch, close, UI automation (ui_click / ui_type), clipboard_set
- **`mcp__continuum__schedule_*`** — cron-style scheduled wakes
- **`mcp__continuum__system_config_*`** — dynamic config read / write

**Deliberately excluded, no `mcp__continuum__shell_*` namespace ever.** Shell execution inside Continuum's permission boundary is a non-goal — workers that need a shell go through Claude Code's built-in `Bash` tool, which is gated by Continuum Core's worker-pool permission model rather than by Continuum's MCP server.

Every tool has a strict permission model. Destructive operations (repair intents, worker spawns, filesystem writes that aren't yet shipped) require either a pre-approved allowlist rule or live user confirmation via the dashboard.

---

## Memory system

Memory is arguably the hardest part of Continuum and what separates it from every chatbot. Continuum stores memory in four places, three of them unchanged since the early phases and one — the **memory vault** — that replaced the original flat key/value "semantic memory" store:

| Store | Kind of memory | Source of truth | Status |
|---|---|---|---|
| Raw log (SQLite) | Everything the senses produce, verbatim | itself | unchanged |
| Episodic memory (LanceDB) | Distilled "things worth remembering" from the day, vector-searchable | itself | unchanged |
| **Memory vault (markdown + derived SQLite index)** | Structured, linked, user-ownable knowledge (decisions, facts, people, projects, preferences, …), kept current by a background **curator** pipeline | **the markdown files** — the index is fully derived and disposable | current (Plan A + Plan B, shipped) |
| ~~Semantic memory (flat key/value SQLite)~~ | Legacy stable facts (`user.name`, `project.x.stack`, …) | itself | **legacy** — superseded by the vault; migrated on request; the orchestrator's fact tools (`memory_set_fact` et al.) write exclusively to the vault now, so this store is read-only from the runtime's own perspective, kept only as a fallback for facts never migrated |

The vault is described in full in `docs/memory.md`; this section covers how all four stores relate.

### Raw log (SQLite)

Everything the senses produce, stored verbatim. One row per perception frame. Includes screenshot thumbnails (base64 or filepaths), transcripts, and context. Used for forensic retrieval ("what was I doing at 14:23 yesterday").

**Retention:** default 30 days (`[storage] retention_days`, configurable 1–365). Rotation runs on the memory-distiller ticker at a one-hour floor — and keeps ticking even when distillation is disabled, because retention is a privacy promise, not a distillation feature. Rotation deletes screenshot files referenced by the rows it removes and additionally sweeps `screenshots_dir` by file age (`[storage] screenshot_max_age_hours`, default 720), which is why that value must stay at or above `retention_days * 24`: the sweep never consults the database.

The same database also holds `context_events`, its FTS mirror, and the Projects table — see "The context engine" above. All DDL lives in `memory/raw_log.rs`; the runtime is the single writer and every other process opens it read-only.

**Schema (simplified):**

```sql
CREATE TABLE perception_frames (
  id INTEGER PRIMARY KEY,
  ts TIMESTAMP NOT NULL,
  screen_description TEXT,        -- the one-sentence vision caption
  screen_world_compact TEXT,      -- the compact world-state render (packager only)
  screen_screenshot_path TEXT,
  audio_transcript TEXT,
  context_json TEXT,
  salience REAL,
  triage_decision TEXT,
  context_privacy TEXT            -- collector zone: visible / redacted / excluded
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

### Memory vault (markdown, source of truth)

The vault replaces the old flat key/value semantic store with an
Obsidian-like, user-ownable knowledge base: **markdown files on disk are the
source of truth**; a derived SQLite index (`vault/.continuum/index.db`,
FTS5 + graph tables + an event timeline) makes them searchable and
graph-shaped, and is fully rebuildable from the markdown at any time — a
missing or corrupt index is just deleted and regenerated, never a data-loss
event.

Ten node types (`project | goal | task | decision | person | preference |
fact | error | session | note`), a status lifecycle (`candidate → confirmed
| rejected | superseded | archived`), typed `relations:` frontmatter edges
plus untyped `[[wiki-link]]` mentions, atomic (tmp+rename) writes, and
per-file quarantine for unparsable frontmatter. Full schema, config, and
troubleshooting: `docs/memory.md`.

`crates/continuum-memory` is a new, dependency-light crate (`sqlx`,
`serde`/`serde_yaml`, `ulid`, `notify`, `chrono` — no llama/whisper/lancedb).
**Both processes link it directly**: the `continuum` runtime and
`continuum-desktop` each open the same vault directory in-process rather
than one talking to the other over IPC, so the dashboard's Memory tab is
fully functional (browse/search/edit/create/migrate) even when the runtime
isn't running. Cross-process change propagation is a debounced file-watcher
in each process — an edit made by one process (or by hand, or in Obsidian)
is picked up and reindexed by the other without polling.

The vault shipped as **Plan A**: crate, index, watcher, migration, the
Tauri command surface, and the graph-centric Memory tab rebuild (see
"Dashboard" below). The **curator** — a background pipeline (local triage
LLM + orchestrator review via MCP) that continuously turns perception into
proposed vault candidates, detects contradictions between existing notes,
summarizes finished work sessions, and keeps the vault tidy — shipped as
**Plan B** (`docs/superpowers/plans/2026-08-03-memory-vault-plan-b.md`) and
is described in full in `docs/memory.md`'s "The curator" section.

### The curator (Plan B)

`crates/continuum-core/src/curator/` implements the pipeline; it only spawns
when a triage LLM is loaded at boot, and runs its own background loop
independent of orchestrator wakes (a fixed-interval ticker, not something
the orchestrator drives):

1. **Extraction** — reads new vault events, asks the triage LLM to propose
   candidate notes, routes each by confidence (auto-confirm above
   `auto_confirm_threshold`, candidate in between, discarded below
   `discard_floor`).
2. **Conflict detection** — for each newly-written note, asks the LLM
   whether a same-type/same-project confirmed note it resembles is
   `unrelated`/`supersedes`/`contradicts`, attaching a `proposes_supersede`
   relation above `supersede_confidence_floor` — never auto-resolving it.
3. **Session summaries** — a curator-owned `SessionTracker` (idle timeout or
   a sustained foreground-process change) compresses a finished work session
   into a `Session` note.
4. **Daily hygiene** — event pruning, expired-node sweeping, and draining
   any pending derived-data wipe request, once per local calendar day.
5. **A daily maintenance wake** — a purpose-built ticker (`maintenance_wake_hour`)
   wakes the orchestrator specifically to drain pending decisions on a day
   nothing else would have; there was no pre-existing scheduler to hook this
   into, so `bin/continuum.rs` grew one.

Candidates surface to a human or the orchestrator three ways: the wake
context's "Pending memory decisions" section (below), the four
`mcp__continuum__memory_vault_*` MCP tools plus `memory_wipe_all`, and the
dashboard's Memory tab curator-card stack. Every LLM-parse failure retries
once before the pass/pair is skipped; a window that fails outright 3 times
running is abandoned rather than wedging the curator forever. Full config
keys, prompt locations, and the session-boundary algorithm: `docs/memory.md`.

### Semantic memory (legacy, superseded by the vault)

Before the vault existed, "things Continuum just knows about you" lived in
a flat SQLite key/value table plus a typed-edge table:

```sql
CREATE TABLE semantic_facts (
  key TEXT PRIMARY KEY,         -- "user.name", "project.simcharts.stack"
  value TEXT NOT NULL,          -- JSON-encoded value
  confidence REAL,              -- how sure Continuum is (learned facts have lower confidence than user-provided)
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

This store is **legacy**: `crates/continuum-memory::migrate_legacy_semantic`
(exposed as the Memory tab's "Import legacy memory" action) converts every
row into a vault `fact` note, idempotently, without ever modifying or
deleting the original database. `memory_set_fact` now writes exclusively
into the vault (a `type: fact` note, matched/updated by title); the legacy
database is never written to again. `memory_get_fact`/`memory_list_facts`
still read it, but only as a fallback when the vault has no matching note or
is itself unavailable — for facts written before this redirect shipped, or
never migrated.

### Memory retrieval flow

When the orchestrator wakes up, Continuum Core runs a two-step retrieval:

1. **Vector search** in episodic memory using the current perception frame as the query (embedded via fastembed). Returns top 10.
2. **Re-rank** via the triage LLM: "Which of these 10 memories are most relevant to the current situation?" Returns top 3.

The top 3 episodic memories, plus all relevant semantic facts (selected by
tag and key matching), are added to the orchestrator's context. The
orchestrator never sees the raw log directly — no MCP tool exposes it today;
the episodic top-3 (`memory_query_episodic`) and semantic facts
(`memory_list_facts` / `memory_get_fact`) reach the wake context this way.

**Vault notes are also injected on every wake**, independent of the
episodic/semantic flow above: `retrieve_vault_context` FTS-matches the
trigger frame against confirmed vault notes (up to `wake_vault_notes_max`,
sensitivity-gated) into a `## Long-term memory (vault)` wake-message
section, and pulls candidate notes older than 30 minutes (oldest-first, up
to `claude_batch`, also sensitivity-gated) into a `## Pending memory
decisions` section with an instruction to resolve them via
`memory_vault_resolve`/`memory_vault_save`. Every internal failure here
(a vault search error, an unparseable timestamp) degrades to an empty
section rather than failing the wake — a memory-retrieval hiccup must never
block the orchestrator from waking. See `docs/memory.md`'s curator section
and `docs/mcp-tools.md`.

### Memory writing

The orchestrator writes to memory two ways. `memory_set_fact` — the
simplest path, for a flat dotted-key fact (confidence clamped by source) —
now writes a `type: fact` vault note under the hood rather than the legacy
store (see `docs/mcp-tools.md`). For anything richer — a decision, a
person, a nuanced preference — `memory_vault_save` writes (or, matched by
title, updates) a full vault node directly: type, body, project, tags,
relations. `memory_vault_resolve` lets it act on candidates the curator
proposed (confirm / reject / supersede). Outside the orchestrator, the
vault also changes through the Memory tab (create/edit a note directly), the
legacy migration, and the curator pipeline itself (above) — the vault has no
single writer, by design; every writer goes through the same file-watcher
reindex.

---

## Voice pipeline

Voice is what makes Continuum feel alive. It has to be low-latency, interruptible, and natural. The pipeline is:

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

Wake detection runs on the continuous whisper transcript stream — the same whisper-medium pipeline that drives STT is the wake gate. A small `TranscriptWakeDetector` scans each new transcript fragment for "hey continuum" (configurable via `voice.wake_keyword`) with a narrow edit-distance tolerance tuned for the failure modes we see in practice (e.g. whisper hears "hey cairo" on short clips). No separate wake-word model ships — Porcupine was prototyped but dropped to keep the model-download footprint smaller and avoid a second audio-inference pipeline competing with whisper for the microphone. Users can disable the wake gate entirely (`voice.wake_word_enabled = false`), in which case every transcript is offered to triage and the LLM decides whether it's addressed to Continuum.

### TTS options

- **Piper** (default) — local, fast, free, Dutch and English voices included
- **Kokoro TTS** — local, better quality than Piper, English only
- **ElevenLabs streaming** — best quality, cloud, requires API key, costs per character

### Voice front-end: pipeline vs Moshi

Continuum has two realtime voice front-ends, selected by
`voice.frontend.mode` (cargo feature `moshi` gates the Moshi path):

- **`pipeline`** (default) — the segment-granular loop above:
  wake → whisper STT → triage → orchestrator → TTS. Works on CPU,
  interruptible, full tool/memory access. This is what shipped through
  Phase 5.
- **`moshi`** — Kyutai Moshi full-duplex speech-to-speech, run as a
  `moshi-backend.exe` subprocess driven over its standalone WebSocket
  protocol (`wss://127.0.0.1:<port>/api/chat`). Moshi owns turn-taking
  for short conversational exchanges (~200–400 ms, interruptible) the way
  ChatGPT's Advanced Voice Mode does. Requires a CUDA-built
  `moshi-backend.exe` and, for audio, libopus + the `moshi-opus` cargo
  feature (Opus-in-OGG, 24 kHz mono). Windows support is community-grade.

The two share a `VoiceFrontend` trait. The audio watcher forks 16 kHz mono
PCM into `MoshiFrontend::feed_pcm` when Moshi is active; assistant text
deltas flow back as `MoshiEvent`s. Because the standalone Moshi backend
does **not** transcribe user audio, the parallel whisper path stays the
source of user transcripts for triage — that is how the tier-split
escalates from a Moshi conversation to the orchestrator. In Moshi mode the
wake-word / pipeline voice-session machinery is skipped (Moshi is
always-listening), but triage still runs on every perception frame's
whisper transcript. On a `WakeOrchestrator` decision the Moshi front-end
is `interrupt()`ed (output muted + `EndTurn` control frame sent) before
the orchestrator wake is spawned; `do_wake` runs and its streamed answer
is spoken via Kokoros through the shared `SpeechController` / playback
path. When the wake completes, the spawned task calls `resume()` on the
Moshi front-end, unmuting its assistant output so S2S turn-taking
continues. The orchestrator only fires for reasoning/tool/memory turns;
chitchat stays local in Moshi. Barge-in during an orchestrator turn uses
the existing `SpeechController` interrupt path; Moshi is already muted
for the duration.

### Interrupt handling

The microphone keeps listening while Continuum speaks. If the user starts talking, playback is cut within 50 ms and the new input goes into the pipeline. This is what makes it feel like a conversation instead of a walkie-talkie.

### Ambient mute

When Continuum detects that the user is in a Discord, Teams, Zoom, or Meet call (via the context watcher), it switches to a quiet mode: no spontaneous speech, only on-screen text responses, and any voice output happens at reduced volume through a secondary audio channel.

---

## Dashboard

The dashboard is a Tauri window opened from the system tray. It is the single place where the user configures, monitors, and repairs Continuum. It has the following tabs:

### Home

Real-time status: the status orb, current perception frame (text + thumbnail), live audio waveform, active workers, recent actions timeline, and resource usage.

### Brain

Model configuration for all four layers with dropdowns, test buttons, and a visual diagram showing how the layers connect.

### Memory

Graph-centric: a full-bleed force-directed graph of the vault (`docs/memory.md`)
is the page itself, not a sub-tab. Click a node to open a docked, resizable
detail/edit panel; expand it to a full-screen markdown editor. Floating
curator cards (populated once the curator pipeline writes its first
candidate) and a bottom timeline scrub strip sit over the graph; the topbar
has search, type/status/project filters, saved views, and a "…"
vault-actions menu (rebuild the derived index, import the legacy semantic
store, wipe derived memory data). "Wipe" never deletes vault markdown: it
writes `<dev_dir>/wipe-request.json` (same contract the MCP `memory_wipe_all`
tool uses), immediately prunes vault events + rebuilds the index from the
dashboard process, and the `continuum` runtime clears raw log/episodic at
its next boot or daily hygiene tick. The Home tab also shows a Curator
status row (last pass, pending/written counts, a degrading badge at 3+
consecutive failures). See `docs/dashboard.md` for the full tab breakdown.

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

All tabs are live-updating via a WebSocket connection from the Continuum Core to the dashboard frontend.

---

## Self-healing subsystem

This is the feature that takes Continuum from "ambitious but fragile" to "a system you can trust."

### How it works

When the user clicks **Fix Issues** (or says "Continuum, something is broken"), Continuum Core:

1. Collects the last 500 log lines, all current component statuses, any stacktraces, and the current config snapshot.
2. Writes this context to `~/.continuum/repair-context.md`.
3. Spawns a dedicated Claude Code session with:
   - Working directory set to the Continuum install folder
   - Model forced to Claude Opus 4.6
   - A custom system prompt from `~/.continuum/repair-agent-prompt.md`
   - Full file system access to the Continuum installation
   - Access to a dedicated MCP tool set: `repair_restart_component`, `repair_reinstall_component`, `repair_rollback_config`, `repair_test_component`, `repair_escalate`
4. Streams the repair agent's output live to the dashboard Health tab.

The repair agent is instructed to:

- Diagnose the root cause from the logs
- Propose a fix
- Apply non-destructive fixes immediately (restart a process, reload config, clear a cache)
- Ask for confirmation before destructive fixes (reinstall a model, modify core config, rollback to backup)
- Test the fix by calling `repair_test_component`
- Report what it did and whether the issue is resolved

### Runtime supervisor

A `Supervisor` (`crates/continuum-core/src/supervisor.rs`) owns the long-lived sense tasks so a dead or stuck component is revived without a full runtime restart. It manages the three triage-relevant collectors — `vision`, `audio`, `context_watcher` (`SUPERVISED_REPAIR_TARGETS`) — plus the auto-heal-only `git`, `file`, and `process` collectors. On a watch tick it reaps any task whose `JoinHandle` has resolved (clean exit or panic) and respawns a faithful reconstruction through the restarter closure registered at boot. Each supervised component is constructed with a stable shared `Arc<RwLock<Health>>` (via a `with_health` builder), so a respawn reuses the same health handle the dashboard and `system_health` MCP tool read — a restart never orphans health state.

The supervisor also drains `~/.continuum-dev/repair-intents/`: each `restart` intent whose `component` matches a supervised target calls `restart_named`, then the file is archived to `processed/`. This is the wire that makes the repair agent's `repair_restart_component` call actually take effect — the repair agent writes an intent, the supervisor consumes it on the next tick and respawns the component in-process. The repair session's `allowed_restart_components` is seeded from `SUPERVISED_REPAIR_TARGETS`, so only those three components are restartable by the agent; the git/file/process collectors have no repair key and auto-heal on death only (no public API change to `continuum-mcp`).

Runtime structured logs tee to a fixed `~/.continuum-dev/logs/continuum.log` via a non-blocking `tracing-appender` writer; `runtime_log_tail` (`health/repair.rs`) reads its tail to build the diagnose context handed to the repair agent.

### Backup rotation

Every morning at 04:00, Continuum Core snapshots the entire `~/.continuum` directory (excluding the raw log and memory stores) to `~/.continuum-backups/<date>/`. The repair agent has read access to the last 7 backups and can rollback to any of them if a fix goes wrong.

### Predictive maintenance

The self-diagnose routine runs nightly: checks every component's response time, error rate, and resource usage. If a component is degrading, it logs a warning and offers (via the Health tab) to have the repair agent investigate preemptively.

### Voice-activated repair

The user can trigger the repair agent with voice: *"Continuum, something isn't right, can you check?"* The orchestrator routes this to the repair subsystem and reports back by voice when done.

---

## Resource policy

Vision now prefers SmolVLM2-2.2B Q4_K_M through llama.cpp MTMD and requests
GPU offload only in CUDA- or Vulkan-enabled builds. On CPU-only builds it stays
local on CPU. A failed primary load or warmup automatically retries the
SmolVLM-500M ONNX fallback before vision degrades to the stub.

Continuum runs several local models continuously (a triage LLM, Whisper STT, an ONNX vision model) plus screen/context pollers and a worker pool. On a laptop these can eat the whole machine if each picks "all cores / full GPU" naively. So the runtime probes the host once at boot and resolves a concrete resource plan that tunes every resource-affecting knob. This is **not a cognitive layer** — it sits *outside* the Senses → Triage → Orchestrator → Workers hierarchy: it never feeds perception frames upward and never makes triage decisions. It only tunes downward-facing knobs before components spawn. Data still flows up, commands still flow down.

### Detection

`crates/continuum-core/src/hardware.rs::probe_hardware()` runs once at startup and produces a `HardwareSpecs`:

- **sysinfo** — physical + logical core counts, total RAM, CPU brand string.
- **`windows::Win32::System::Power::GetSystemPowerStatus`** — AC vs battery, and whether the machine has a battery (laptop vs desktop).
- **`windows::Win32::System::LibraryLoader::LoadLibraryW("nvcuda.dll")`** — is an NVIDIA CUDA runtime present? (works even when `nvidia-smi` isn't on PATH).
- **`nvidia-smi --query-gpu=memory.total`** subprocess (short timeout) — VRAM in MB, when queryable. If `nvcuda.dll` loads but VRAM can't be read, Continuum assumes enough and lets the model loader fall back internally on failure.

### Resolution

`resolve_resource_policy(specs, &config.resources)` is a **pure** function (the unit-test target) that maps specs + the user's `[resources]` config to a `ResolvedResourcePlan`: triage threads / GPU layers, vision enabled + CUDA EP, whisper threads, worker concurrency, screen + context poll intervals. The default profile is `barely_notice` — a barely-noticeable CPU/RAM footprint (≤ ~30% of logical cores, ≥ 50% RAM free, halved on battery) with the GPU/VRAM used **freely** for quality (full offload when VRAM ≥ `gpu_min_vram_mb`). No model downgrades — Qwen3-8B Q4_K_M, whisper-medium, SmolVLM-256M stay; only the threads / GPU / intervals / concurrency move. Profiles: `auto` / `barely_notice` / `balanced` / `performance` / `custom` (custom honours every field verbatim).

The three binaries (`continuum`, `continuum-perception`, `continuum-triage-bench`) apply the resolved plan to the loaded config before components spawn, so every downstream consumer picks up the adapted values without each needing to know about hardware detection. The plan + specs are published to `state.json` (`RuntimeSnapshot` gained `hardware_specs` + `resource_plan` fields) for the dashboard.

### Self-healing

A `system_resources` health probe (registered in `apps/desktop/src-tauri/src/components.rs`) samples CPU%/RAM every 30 s via sysinfo and reports `Degrading` on sustained >90% CPU across ~60 s or >90% RAM, `Error` on >95% RAM (imminent OOM). `write_repair_context` (`health/repair.rs`) appends a `## System resources` block (detected specs + live CPU/RAM + GPU/VRAM + power + resolved plan) so the repair agent can reason about model-load failures — e.g. "triage OOM'd → 4 GB laptop → vision should be off → lower `cpu_core_fraction` / `workers_max_concurrent`".

### Override

Every knob is overridable via `[resources]` in `config.toml` or the dashboard Settings → Resources panel (Tauri commands `get_resource_profile` / `update_resource_profile`). There is no hot-reload: the plan is resolved once at boot, so a profile change persists to config and the dashboard shows a "Restart to apply" banner (consistent with the existing daemon limitation).

---

## Security and permissions

Continuum has access to your computer. Trust is earned through transparency and explicit permissions.

### Tool permission tiers

Every MCP tool has one of four permission levels:

- **Auto** — can be called without confirmation (read operations, voice output, memory reads)
- **Session-approved** — requires confirmation once per session, then allowed (most shell commands, file writes in specified directories)
- **Always-confirm** — requires confirmation every single call (elevated shell, file deletes, sending messages, financial actions)
- **Blocked** — cannot be called at all unless the user explicitly enables it (modifying Continuum's own installation except via repair agent, accessing password stores, anything touching credentials)

The defaults are set in `~/.continuum/permissions.toml` and visible in the dashboard.

### Per-folder policies

The user configures which folders are read-write, read-only, or off-limits to the orchestrator and workers. By default, `~/.continuum`, system folders, and credential stores are off-limits.

### Audit log

An append-only JSONL log at `<data_dir>/logs/actions.jsonl` records one object per line — `{ts, kind, actor, summary, details?}` — for **wakes and wake results** (`actor: agent`), **every applied Context-page intent including the ones that fail** (`actor: user`), and **automatic project-pin expiry** (`actor: agent`). It self-rotates at 4 MiB by dropping the oldest half, and writing to it can never return an error to a caller.

**MCP tool calls are not in this log yet.** They execute inside the `continuum-mcp` process, which would need its own audit wiring; the target design is for every tool call to be recorded with timestamp, caller, redacted arguments and result. Until that lands, the tool-call trail is the MCP server's own structured logging.

### No telemetry

Continuum does not phone home. Ever. There is no usage tracking, no crash reporting to third parties, no "anonymous analytics." The only network calls Continuum makes are to the Anthropic API (via Claude Code) and optionally to ElevenLabs (if the user enables premium TTS).

---

## Directory layout

The repository is a monorepo using pnpm workspaces for JavaScript/TypeScript parts and a Cargo workspace for Rust parts.

```
continuum-ai/
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
│   ├── continuum-core/               # Main orchestration runtime
│   │   ├── Cargo.toml
│   │   ├── benches/
│   │   │   └── data/             # the ONLY committed fixture (synthetic, generated)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── senses/           # Layer 1
│   │       │   ├── vision.rs
│   │       │   ├── audio/
│   │       │   ├── context.rs
│   │       │   ├── frame.rs
│   │       │   ├── privacy.rs        # the scrub + zone choke point
│   │       │   ├── git_watch.rs      # git collector (active confirmed project)
│   │       │   ├── file_watch.rs     # opt-in file watcher
│   │       │   ├── cadence.rs        # idle controller + shared-atomic cadences
│   │       │   ├── toggles.rs        # live observation toggles
│   │       │   ├── screenshots.rs    # age-based screenshot sweep
│   │       │   └── live_context.rs
│   │       ├── triage/           # Layer 2
│   │       │   ├── mod.rs
│   │       │   ├── llm.rs
│   │       │   ├── prompts.rs
│   │       │   ├── coalesce.rs       # off-loop triage submission
│   │       │   └── consume.rs        # classification → events / candidates
│   │       ├── context/          # the context engine (mostly UNGATED)
│   │       │   ├── project.rs        # resolver, discovery, override rules
│   │       │   ├── session_state.rs  # live "what is the user doing"
│   │       │   ├── package.rs        # one struct, one renderer, three profiles
│   │       │   ├── continuation.rs   # what "ga door" resumes
│   │       │   ├── intents.rs        # Context-page intent files
│   │       │   └── apply.rs          # applying them (incl. the Forget cascade)
│   │       ├── orchestrator/     # Layer 3
│   │       │   ├── mod.rs
│   │       │   ├── spawn.rs
│   │       │   └── stream.rs
│   │       ├── workers/          # Layer 4
│   │       │   ├── mod.rs
│   │       │   ├── pool.rs
│   │       │   └── supervisor.rs
│   │       ├── memory/
│   │       │   ├── raw_log.rs        # ALL DDL for the raw-log DB lives here
│   │       │   ├── events.rs         # events writer, dedupe, event registry
│   │       │   ├── episodic.rs
│   │       │   ├── distill.rs
│   │       │   └── semantic.rs       # legacy — see "Memory system"; migrates into the vault
│   │       ├── bench/            # replay + scoring for the four harnesses
│   │       ├── voice/
│   │       │   ├── wake.rs
│   │       │   ├── stt.rs
│   │       │   └── tts.rs
│   │       ├── health/
│   │       │   └── repair.rs
│   │       ├── audit.rs          # append-only actions.jsonl
│   │       ├── llm_gate.rs       # interactive-vs-background LLM priority
│   │       ├── runtime_publish.rs
│   │       ├── config_edit.rs
│   │       └── config.rs
│   │
│   ├── continuum-memory/             # Memory vault: markdown source of truth + derived SQLite index
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── vault.rs          # the Vault façade (create/get/save/delete/graph/search/…)
│   │       ├── index.rs          # derived SQLite index (nodes/edges/FTS5/events/quarantine)
│   │       ├── watcher.rs        # debounced file-watcher, cross-process change propagation
│   │       ├── migrate.rs        # one-shot legacy semantic.sqlite → vault migration
│   │       ├── frontmatter.rs    # YAML frontmatter parse/render, wiki-link extraction
│   │       ├── slug.rs
│   │       ├── model.rs          # wire types (Note, Frontmatter, GraphData, …)
│   │       └── error.rs
│   │
│   ├── continuum-mcp/                # MCP server exposing Windows tools
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── server.rs
│   │       ├── tools/
│   │       │   ├── memory.rs
│   │       │   ├── system.rs         # incl. the three content-filtered tools
│   │       │   ├── context.rs        # the ten context tools
│   │       │   ├── fs.rs
│   │       │   ├── web.rs
│   │       │   ├── repair.rs
│   │       │   └── workers.rs
│   │       ├── redaction.rs      # safety-redaction bench harness
│   │       ├── audit.rs
│   │       └── allowlist.rs
│   │
│   ├── continuum-llm/                # Local LLM runtime (llama.cpp wrapper)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── lib.rs
│   │
│   └── continuum-vision/             # Local vision model runtime
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs
│
├── skills/                       # Bundled Continuum skills (SKILL.md files)
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
│   ├── context-engine.md         # what is observed, zones, toggles, tools, config
│   ├── dashboard.md
│   ├── skills.md
│   ├── self-healing.md
│   └── images/
│
├── prompts/                      # Orchestrator and triage prompts
│   ├── orchestrator-system.md
│   ├── triage-system.md
│   ├── session-state.md
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

**Rust for Continuum Core and the MCP server.** Performance matters because Layer 1 runs 24/7. Rust gives us zero-cost abstractions over Windows APIs and clean integration with llama.cpp, whisper.cpp, and LanceDB. TypeScript would be faster to prototype but slower at runtime and messier for Windows COM interop.

**Claude Code as subprocess, not SDK.** The Agent SDK exists in Python and TypeScript but tying Continuum to it would mean bundling a language runtime and fighting version drift. The `claude` CLI is the official, stable, language-agnostic contract. We spawn it and talk JSON.

**Stream-json for both directions.** We use `--input-format stream-json --output-format stream-json` for all orchestrator calls. This gives us bidirectional structured communication, live tool call visibility, and the ability to feed follow-up messages into a running agent loop.

**MCP for extending Claude Code, not replacing it.** Claude Code already handles the hard parts (tool loop, file editing, sub-agents, context management). We add Windows-specific capabilities via MCP and let Claude Code drive. This also means anyone else's MCP server works with Continuum out of the box.

**LanceDB over Chroma/Qdrant.** LanceDB is embedded (no server), Rust-native, and designed for on-device use. Chroma is Python-first. Qdrant is a server product. LanceDB is the right choice for a local-first desktop app.

**Piper as default TTS.** Free, fast, local, Dutch support, actively maintained. ElevenLabs is better-sounding but we refuse to require an API key for core functionality.

**Apache 2.0 over MIT.** Explicit patent grant protects contributors. Slightly more enterprise-friendly. No practical downsides for a permissively licensed project.

**Monorepo over multi-repo.** Single clone, single CI, single version, Claude Code can see everything. We can split later if the project grows large enough to warrant it. Right now it would just add friction.

---

Last updated: 2026-04-10. This document is authoritative. If code and this document disagree, fix the document first, then the code, or fix the code first and update the document in the same commit.
