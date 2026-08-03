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
| Memory        | Graph-centric memory vault browser/editor. See [Memory tab](#memory-tab) below and `docs/memory.md` for the vault's data model. |
| Tools         | MCP namespaces + per-tool permission level (auto / session / confirm / blocked). Skills list with toggles. |
| Voice         | Live voice state, wake word config, TTS engine + voice selector, volume / speed sliders, ambient mute settings. |
| Automations   | List of scheduled tasks with last/next run. Create / edit / delete / toggle. |
| Logs          | Real-time ring buffer (10k entries) with level / layer / component / text filters. NDJSON export. |
| Health        | Component status grid, per-component detail modal, Fix Issues button, backup status, repair agent output stream. |

## Memory tab

The Memory tab is a rebuild around the memory vault (`docs/memory.md`) — a
full-bleed force-directed graph *is* the page, not one of several sub-tabs.
The Tauri backend (`apps/desktop/src-tauri/src/memory.rs`) links
`continuum-memory` directly and opens the vault lazily on first command, so
a broken/locked vault degrades only this tab, never app startup.

- **Graph view** (`MemoryGraph.tsx`, canvas via `force-graph`) — one dot per
  node, colored by type, radius scaled by importance. A pulsing halo marks
  `candidate` status, 40% opacity marks `superseded`/`archived` (only when
  the status filter shows them), a hollow ring marks a ghost node (an
  unresolved `[[wiki-link]]` target — clicking one offers "create note").
  Drag to pan, scroll to zoom, drag a node to pin it for the session. The
  topbar has full-text search (Enter centers the result), type/status/
  project filter chips, a legend toggle, and a quarantine warning chip when
  the vault has unparsable files (`docs/self-healing.md` covers recovery).
- **Note panel + editor** — clicking a node opens a resizable (320px–70%
  of viewport, width persisted) docked right panel: rendered markdown,
  metadata chips, editable confidence/importance sliders, tags, project,
  a typed-relations editor (add/remove `{to, rel, confidence}`), backlinks,
  and a delete row. The **⤢ Expand** button (or double-click a node)
  promotes to `NoteEditorOverlay.tsx`, a full-screen modal with a
  textarea + live markdown preview toggle; `Esc` returns to the graph with
  selection preserved, `Ctrl+S` / **Save** persists.
- **Curator stack** (`CuratorStack.tsx`) — floating cards top-right for
  candidate notes awaiting review (Confirm / Reject / "Later", which just
  hides a card behind a session-local counter badge). Reads
  `memory_pending()` and refreshes live off the `continuum:memory` event.
  **Empty today** — it renders correctly but has nothing to show until the
  curator pipeline (Plan B) starts writing `status: candidate` notes.
- **Timeline strip** (`TimelineStrip.tsx`) — a bottom scrub bar bucketing
  the vault's `events` table into density bars for the visible window
  (default: today). Drag across bars to scrub; the graph dims every node
  whose `created`/`updated` falls outside the scrubbed window. **Empty
  today** for the same reason as the curator stack — nothing writes vault
  events until Plan B's curator/distiller integration lands.
- **Saved views** — name + persist the current filter set (types, status,
  project, query) to a local Zustand store (`lib/memoryViews.ts`); reapply
  from the topbar's Views dropdown. Local-only, not synced anywhere.
- **Vault actions ("…" menu)** — the topbar's overflow menu: **Rebuild
  index** (forces a full reindex of the derived SQLite database from the
  markdown — safe any time, see `docs/memory.md`'s troubleshooting section),
  **Import legacy memory** (shown only when a pre-vault `semantic.sqlite`
  still exists; runs the idempotent migration and shows a result banner),
  and **Wipe derived memory data** (danger row, requires typing `DELETE`).
  It never touches vault markdown — but as of this writing `wipe_memory`
  (`apps/desktop/src-tauri/src/commands.rs`) is a stub: it validates the
  `DELETE` confirmation, logs the request, and marks the distiller for a
  re-pass, without yet actually clearing the raw log/episodic/events data.
  The real wipe is forwarded to the `continuum` runtime once
  `continuum-mcp` gains a `memory__wipe_all` tool (follow-up work); today
  the button only records that a wipe was requested. A separate **Open
  vault** button in the topbar opens the vault
  folder in the OS file explorer; it works even when the index has failed
  to open, since it's the primary recovery affordance for a broken index.

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
| `~/.continuum-dev/repair-intents/*.json`     | dashboard → runtime | Repair actions queued by the repair agent |
| `~/.continuum-dev/repair-context.md`         | dashboard           | Written at the start of each repair session |
| `~/.continuum-backups/<date>/continuum-<date>.zip` | dashboard          | Nightly config backup, 7-day rotation |
| `~/.continuum-dev/vault/**/*.md`             | shared, user-owned  | Memory vault notes — opened directly (in-process) by both the dashboard and the runtime; see `docs/memory.md` |
| `~/.continuum-dev/vault/.continuum/index.db` | shared, derived     | Memory vault's SQLite index — always rebuildable from the markdown |

## Limitations

- The Memory tab's curator stack and timeline strip render correctly but
  stay empty — nothing writes candidate notes or vault events until the
  curator pipeline (Plan B) ships. The graph, note panel/editor, search,
  and migration are fully functional today regardless (the dashboard owns
  the vault directly, no runtime dependency).
- Automation scheduling is persisted but not yet executed by the runtime;
  the data model is ready for Phase 8's scheduler.
- Workers tab data is populated by the orchestrator via state events —
  until Phase 8 spawns real workers, the list stays empty.
