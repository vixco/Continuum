# Dashboard

The Continuum dashboard is the Tauri desktop window that surfaces and
configures everything the four-layer runtime is doing. It's not the
runtime itself — the `continuum` binary owns senses/triage/orchestrator —
but it's where you spend time as a user.

## Architecture

Two processes cooperate:

```
┌────────────────────┐          ┌──────────────────────────┐
│  continuum.exe          │          │  continuum-desktop.exe        │
│  (release only)     │          │  (Tauri 2 + Next.js 15)   │
│                     │  writes  │                           │
│  senses → triage →  │ ───────▶ │  reads state.json         │
│  orchestrator →     │   2 s    │  runs own health probes   │
│  workers            │          │  runs repair agent        │
│                     │  reads   │  manages automations,     │
│                     │ ◀─────── │  backups                  │
│                     │ intents  │                           │
└────────────────────┘          └──────────────────────────┘
        ~/.continuum-dev/                   ~/.continuum-dev/
          state.json                      repair-intents/
          logs/                           automations.json
          config.toml                     config.toml
```

- `continuum.exe` writes a small [`RuntimeSnapshot`](../crates/continuum-core/src/runtime_publish.rs)
  JSON file every 2 s describing which models are loaded, the current
  voice mode, frame count and wake count.
- `continuum-desktop.exe` embeds `continuum-core` (without the `runtime` feature),
  so it gets the state store, log buffer, automations, health registry,
  backup rotation, and repair agent — but none of the llama-cpp / whisper
  C++ build dependencies.
- Tauri commands cover CRUD for config, automations, and memory. Live
  streams use Tauri's `emit` / `listen` IPC — no custom WebSocket server.

## Tabs

| Tab           | What it shows                                                                   |
|---------------|----------------------------------------------------------------------------------|
| Home          | Status orb, current perception frame + thumbnail, audio waveform, active workers, recent actions, quick stats (Opus wakes today, cost, memories, uptime). |
| Brain         | 4-layer pipeline diagram. Per-layer model selector, capture-interval slider, GPU toggle, test buttons. |
| Memory        | Three sub-tabs: raw perception log, episodic memories (vector search), semantic facts. Wipe-all confirmation requires typing "DELETE". |
| Tools         | MCP namespaces + per-tool permission level (auto / session / confirm / blocked). Skills list with toggles. |
| Voice         | Live voice state, wake word config, TTS engine + voice selector, volume / speed sliders, ambient mute settings. |
| Automations   | List of scheduled tasks with last/next run. Create / edit / delete / toggle. |
| Logs          | Real-time ring buffer (10k entries) with level / layer / component / text filters. NDJSON export. |
| Health        | Live component probes, per-component detail, one-time repair preview, verified backup status, and guarded repair output. |

## Event topics

| Topic           | Payload type                  | Purpose                                    |
|-----------------|-------------------------------|--------------------------------------------|
| `continuum:state`   | `ContinuumState` snapshot         | Any change to perception, triage, voice, etc. Debounced 150 ms. |
| `continuum:log`     | `LogEntry`                    | Every tracing event the backend emits.     |
| `continuum:repair`  | `RepairEvent`                 | Repair agent stream: text deltas, tool calls, stderr, completion. |
| `continuum:control` | `{action: "pause" \| "resume" \| "voice-on" \| "voice-off"}` | Tray menu → frontend. |

## Status orb colors

- **Grey** — idle
- **Blue** — listening (voice session open)
- **Purple** — thinking (orchestrator active)
- **Green** — speaking (TTS producing audio)
- **Amber** — muted
- **Red** — error

## Running the dashboard

```bash
# Frontend dev loop (pnpm)
cd apps/desktop
pnpm install

# Start the runtime separately in another terminal
cargo run --release --bin continuum

# Tauri dev — the backend embeds continuum-core without the runtime feature
# so debug builds work without llama-cpp hassles.
pnpm tauri dev
```

## Data files the dashboard touches

| Path                                     | Owner               | Purpose |
|------------------------------------------|---------------------|---------|
| `~/.continuum-dev/config.toml`               | shared              | Runtime configuration |
| `~/.continuum-dev/automations.json`          | dashboard           | Scheduled tasks |
| `~/.continuum-dev/state.json`                | continuum runtime       | Live flags + voice mode (published every 2 s) |
| `~/.continuum-dev/repair-intents/*.json`     | legacy MCP output   | Not consumed in this release; the guarded Health flow does not authorize these restart intents |
| `~/.continuum-dev/repair-context.md`         | dashboard           | Written at the start of each repair session |
| `~/.continuum-backups/<date>/continuum-<timestamp>-<id>.zip` | dashboard          | Versioned, manifest-verified config backup with configurable retention |

## Limitations

- The episodic and semantic search panels in the Memory tab return empty
  results unless the runtime is writing memory — they don't own the stores
  directly. Full coverage ships with Phase 8 when the runtime gains an
  HTTP control channel.
- Automation scheduling is persisted but not yet executed by the runtime;
  the data model is ready for Phase 8's scheduler.
- Workers tab data is populated by the orchestrator via state events —
  until Phase 8 spawns real workers, the list stays empty.
