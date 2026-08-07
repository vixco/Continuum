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
| Home          | Status orb, current perception frame + thumbnail, audio waveform, active workers, recent actions, quick stats (Opus wakes today, cost, memories, uptime), a Curator status row (see [Curator row](#curator-row) below). |
| Brain         | 4-layer pipeline diagram. Per-layer model selector, capture-interval slider, GPU toggle, test buttons. |
| Memory        | Graph-centric memory vault browser/editor. See [Memory tab](#memory-tab) below and `docs/memory.md` for the vault's data model. |
| Tools         | MCP namespaces + per-tool permission level (auto / session / confirm / blocked). Skills list with toggles. |
| Voice         | Live voice state, wake word config, TTS engine + voice selector, volume / speed sliders, ambient mute settings. |
| Automations   | List of scheduled tasks with last/next run. Create / edit / delete / toggle. |
| Logs          | Real-time ring buffer (10k entries) with level / layer / component / text filters. NDJSON export. |
| Health        | Live component probes, per-component detail, one-time repair preview, verified backup status, and guarded repair output. |

## Curator row

A full-width strip on the Home tab, right below the header stats
(`HomeTab.tsx`'s `CuratorRow`), showing the background memory-curator
pipeline's status (`docs/memory.md`'s "The curator" section covers what it
does). Reads `state.memory.curator`, mirrored from the runtime's
`RuntimeSnapshot.curator` via `state.json` every 2 s.

- **Curator: off** — rendered whenever `curator` is `null`/`undefined`
  (dashboard hasn't heard from the runtime yet, or the runtime predates this
  field) or `curator.enabled` is `false` (config-disabled, or no triage
  model loaded at boot — the curator never spawns without one). Both cases
  render identically; there's no user-facing reason to distinguish them.
- **Running** — a `StatusBadge`: `healthy` normally, `degrading` once
  `consecutive_failures >= 3`, plus "last pass HH:MM" (from
  `last_pass_at`, blank until the first pass completes), the current
  `pending_count`, the lifetime `candidates_written_total`, and — only while
  failing — a small warning badge with the failure count.
- There is no dedicated curator health probe or repair-agent restart target
  (`docs/self-healing.md` covers this gap and the actual recovery path:
  restart the `continuum` runtime).

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
  topbar has full-text search (Enter or the **Search** button sets a graph
  query filter), type filter chips, and a **Show hidden** toggle that
  reveals rejected/superseded/archived notes (hidden by default); a
  quarantine warning chip appears when the vault has unparsable files
  (`docs/self-healing.md` covers recovery). An always-on legend (dot color
  per type) sits bottom-left over the graph. Saved views and the vault
  actions ("…" menu) round out the topbar — see the bullets below.
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
  Populated once the curator pipeline (`docs/memory.md`'s "The curator")
  writes its first `status: candidate` note — empty until then, but no
  longer permanently empty by design.
- **Timeline strip** (`TimelineStrip.tsx`) — a bottom scrub bar bucketing
  the vault's `events` table into density bars for the visible window
  (default: today). Drag across bars to scrub; the graph dims every node
  whose `created`/`updated` falls outside the scrubbed window. Populated by
  the memory distiller's `distilled` events and the curator's own pipeline
  activity — empty only until the runtime has produced its first event.
- **Saved views** — name + persist the current filter set (types, status,
  project, query) to a local Zustand store (`lib/memoryViews.ts`); reapply
  from the topbar's Views dropdown. Local-only, not synced anywhere.
- **Vault actions ("…" menu)** — the topbar's overflow menu: **Rebuild
  index** (forces a full reindex of the derived SQLite database from the
  markdown — safe any time, see `docs/memory.md`'s troubleshooting section),
  **Import legacy memory** (shown only when a pre-vault `semantic.sqlite`
  still exists; runs the idempotent migration and shows a result banner),
  and **Wipe derived data** (danger row, requires typing `DELETE`). It
  never touches vault markdown. Confirming writes
  `<dev_dir>/wipe-request.json` and immediately prunes vault timeline events
  + rebuilds the index (the dashboard already holds a vault handle for
  that); the raw log and episodic memory are wiped by the `continuum`
  runtime at its next boot or its next daily hygiene tick, since those
  stores live only in that separate process. See `docs/memory.md`'s "The
  curator" section (wipe flow) for the full contract, shared with the MCP
  `memory_wipe_all` tool. A separate **Open vault** button in the topbar
  opens the vault folder in the OS file explorer; it works even when the
  index has failed to open, since it's the primary recovery affordance for
  a broken index.

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
| `~/.continuum-dev/vault/**/*.md`             | shared, user-owned  | Memory vault notes — opened directly (in-process) by both the dashboard and the runtime; see `docs/memory.md` |
| `~/.continuum-dev/vault/.continuum/index.db` | shared, derived     | Memory vault's SQLite index — always rebuildable from the markdown |
| `~/.continuum-dev/wipe-request.json`         | dashboard/MCP → runtime | Pending derived-data wipe request; drained (and deleted) by the runtime's boot drain or daily hygiene tick — see `docs/memory.md`'s "The curator" section |
| `~/.continuum-dev/voice-intents/*.json`      | dashboard → runtime | Push-to-talk intents; drained every 250 ms. Stale intents (>30 s) are dropped |
| `~/.continuum-dev/context-intents/*.json`    | dashboard → runtime | Context-page actions (add/confirm project, correct, not-this-project, pin, forget, delete-range, set-toggle); drained every 250 ms. **No TTL** — a correction issued while the runtime is stopped still applies at its next boot. Unparseable files are renamed `.bad` |
| `~/.continuum-dev/logs/actions.jsonl`        | runtime             | Append-only audit of wakes, toggle changes, corrections and deletions (one JSON object per line: `{ts, kind, actor, summary, details?}`); rotated at 4 MiB by dropping the oldest half |

## The Context page

The Context tab renders what Continuum currently believes about your work
(session state, per-source health, recent events, projects) and is the
place to correct it. It is a **read-only view of `state.json`** plus a
**write-only intent queue**: the dashboard process links `continuum-core`
with `default-features = false`, so it can never open the raw-log
database, the vault index or the episodic store itself.

That means two things in practice:

- everything the page lists (projects and discovery candidates, override
  rules, pins, the recent-events strip, the live toggle values,
  continuation candidates) arrives in `RuntimeSnapshot.context_page`,
  refreshed by the runtime every 5 s and published every 2 s;
- every action is fire-and-forget. Clicking writes one intent file and
  returns; the page updates when the runtime republishes, roughly a
  second later. Nothing is optimistically mutated in the store.

Honest-toggle caveats worth knowing (spec §4.1):

- flipping `screen`, `git` or `files` off takes effect within one loop
  iteration and genuinely stops the capture / subprocess / watch;
- flipping `mic` off stops the data path immediately (nothing is
  transcribed, persisted or sent), but the cpal input stream itself was
  opened at start, so the OS "microphone in use" indicator stays lit
  until the runtime restarts;
- `pause_all` can be engaged live, but a runtime that *booted* with
  `pause_all = true` never spawned its watchers at all — unpausing that
  process requires a restart.

## Limitations

- The curator has no dedicated health probe or repair-agent restart target
  — the Home tab's Curator row (above) and `docs/self-healing.md` are the
  only signals today; recovery is a full `continuum` runtime restart.
- Automation scheduling is persisted but not yet executed by the runtime;
  the data model is ready for Phase 8's scheduler.
- Workers tab data is populated by the orchestrator via state events —
  until Phase 8 spawns real workers, the list stays empty.
