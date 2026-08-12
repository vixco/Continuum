# The Context Engine

Continuum's context engine is the part that continuously knows *what you are
doing* — which app, which project, which goal, what just broke, what worked —
and hands that same picture to every AI that needs it: the orchestrator when it
wakes, the desktop chat, and any tool call.

The pipeline, end to end:

```
capture → dedupe → privacy filter → classification → project/goal → session state
        → curator/memory → context package → Main AI
```

This document is the user-facing guide: what is observed, what is never
observed, how to control it, what the AI can ask for, and what is still missing.
The technical blueprint is in `ARCHITECTURE.md` ("The context engine"); the tool
schemas are in `docs/mcp-tools.md`; the failure runbook is in
`docs/self-healing.md`.

---

## What Continuum observes

| Source | What is collected | Default |
|---|---|---|
| Window / process | Foreground process name, window title, pid, executable path (home/username scrubbed), which monitor, how long it has been focused, and focus-switch events with dwell time | on |
| Screen | One-sentence caption per monitor from the local vision model, plus a compact world-state render | on |
| Audio | Transcripts of speech detected by voice-activity detection, transcribed locally by whisper.cpp | on |
| Files | Create / modify / delete / rename events under confirmed project roots, path-scrubbed and debounced | **off** (`[file_watcher] enabled = false`) |
| Git | Branch, dirty/staged/untracked counts, ahead/behind, conflicts, last commit id + subject — for the **active, confirmed** project only | on |
| Projects | Which project you are in, resolved per frame from window titles, editor patterns, git roots and keywords | on |

Everything above is scrubbed before it is stored, before any local model sees
it, and again before anything leaves the machine.

### No keyboard capture. Ever.

Continuum does not and will not read keystrokes — **a keylogger cannot tell the
difference between a commit message and a password, so the only safe amount of
keyboard capture is none.** Typed text only ever reaches Continuum indirectly:
through the screen caption, through file contents you asked it to watch, or
through a window title. Window titles routinely contain typed text (address
bars, terminal command lines), so they are treated as **content**, not metadata:
they pass the same scrubbers and the same zone rules as everything else.

This is a permanent constraint, not a current limitation. A future UI Automation
reader (not shipped) will have to filter text-field value-change events for the
same reason.

---

## Privacy zones

Every observation is resolved against a list of zone rules, and the strictest
match wins. There are three zones:

| Zone | What happens |
|---|---|
| `never_observe` | The collector emits a **sentinel**: process becomes the literal `[excluded]`, the title is empty. No screenshot file is written, no caption is produced, no event row is stored, focus-dwell resets, and switches involving the window collapse to a single synthetic switch to/from the `[excluded]` bucket. Session state shows `[private]`. |
| `local_only` | Observed, scrubbed and stored, and local models may read it — but it is **stripped from everything that leaves the machine**. Cloud-bound renders show `[redacted by local privacy policy]`, and an inferred goal or task tainted by a `local_only` window generalizes to "working in a private context". |
| `cloud_allowed` | The default. |

Rules live in `config.toml`:

```toml
[[privacy.zones]]
match_process = "1password.exe"
zone = "never_observe"

[[privacy.zones]]
match_title_keyword = "Incognito"    # case-insensitive substring
zone = "local_only"
```

Either `match_process` or `match_title_keyword` (or both) may be given; `zone` is
required.

**Existing config keeps working.** The older `[context] sensitive_process_names`
list is synthesized into `never_observe` rules at load time and
`[context] sensitive_title_keywords` into `local_only` rules, preserving today's
"observed but redacted" behaviour. If both the old keys and `[privacy.zones]`
are present, the union applies and the stricter zone wins. `InPrivate`,
`Incognito` and `Private Browsing` are treated as `local_only` out of the box.

**Zones propagate.** Anything *derived* from an observation inherits the
strictest zone of its inputs: event rows carry a mandatory sensitivity tag, a
session-state inference over a window containing `local_only` events is itself
tagged `local_only`, a wake triggered by a `local_only` frame gets the generic
reason "user activity in a private context", and curator notes covering such a
span are written with vault `sensitivity: Sensitive`.

**Zone matching is foreground-first, with a per-monitor sweep.** The foreground
poll is the primary source, and a visible-window sweep additionally makes a
monitor inherit the zone of any `never_observe` / `local_only` top-level window
on it. A hard fallback exists for a monitor you never want captured at all:
`[screen] excluded_monitor_ids`.

**Scrubbers** run on free text (titles, captions, transcripts, summaries, commit
subjects) regardless of zone: API keys and bearer tokens, high-entropy hex/base64
runs, credit-card numbers (Luhn-checked), IBANs, and — if you switch it on —
email addresses. Paths are separately scrubbed everywhere: your home directory
becomes `~` and your username is redacted wherever it appears. Structured fields
from trusted collectors are deliberately exempt so a 40-character git commit id
survives end to end.

---

## The honest toggles

Five switches control observation, in `[privacy.toggles]` and on the Context
page. They are independent, they are enforced *at each collector*, and every
change writes a `toggle_change` system event.

| Toggle | Effect | Default |
|---|---|---|
| `mic` | Stops audio transcription | `true` |
| `screen` | Stops screen capture and captioning | `true` |
| `files` | Stops the file watcher | `true` |
| `git` | Stops the git collector (no subprocess is spawned) | `true` |
| `pause_all` | Gates every collector, buffered frame processing, and process activity | `false` |

A toggle flipped from the Context page takes effect within one loop iteration
(one poll cycle, one flush tick, one capture discovery tick) — the capture is
genuinely stopped, not filtered afterwards. The circular desktop power control
sets the same `pause_all` choke point. It offers 15 minutes, 1 hour, 4 hours,
until tomorrow at 08:00 local time, and indefinite. Its local
`observation-pause.json` lease survives a restart and timed leases resume
automatically. Buffered frames are discarded before persistence/triage and the
current live-context/process projections are cleared when pausing.

**Three limitations are stated rather than hidden:**

1. **`mic` off stops the data path, but not the OS indicator.** Nothing is
   transcribed, stored or sent, but the cpal input stream itself was opened at
   startup and stays open, so Windows keeps showing "microphone in use" until
   the runtime restarts.
2. **Persisting a toggle drops comments from `config.toml`.** Toggle changes are
   written back through the config editor, which rewrites the file and does not
   preserve comments.

### Background-process activity (opt-in)

`[process_watcher].enabled` is a separate consent boundary and defaults to
`false`. When enabled, Continuum samples the process table and persists only
meaningful changes: configured developer/model processes starting or stopping,
plus CPU or resident-memory pressure sustained across several samples. The
current bounded snapshot is published to `processes.json`; lifecycle history is
written through the same deduplicated `context_events` writer as window, Git,
and file activity.

```toml
[process_watcher]
enabled = false
poll_secs = 2
cpu_threshold_percent = 75.0
memory_threshold_mb = 2048
sustained_samples = 3
snapshot_limit = 50
include_names = ["cargo", "rustc", "node", "python", "ollama", "docker"]
exclude_names = ["system", "registry", "lsass", "svchost"]
```

The collector never reads command lines, environment variables, process
memory, or hidden-window contents. Generic OS polling can prove that a process
disappeared but cannot reliably distinguish a clean exit from a crash or recover
its exit code. Exact exit codes and stderr remain available only for processes
Continuum launched/supervised itself or applications that publish permitted
logs/Windows crash evidence.

---

## Classification and session state, in plain language

**Classification rides triage — there is no second GPU pass.** The same local
model call that decides "ignore / remember / whisper / wake" now also returns a
small classification block: an event type (`error`, `success`, `decision`,
`preference`, `task_progress`, `communication`, `routine`, `other`), a project,
an importance and confidence, a one-line summary, and whether it is worth
storing. A truncated or malformed classification never costs a retry — the
decision is still extracted.

Those classifications become rows in a `context_events` table alongside the
window, git and file events. **Repetitions collapse**: a build that fails
fourteen times is one row with `count = 14`, not fourteen rows, which is what
makes "build failed ×14" appear in memory instead of fourteen near-identical
notes. Rows collapse within `[events] collapse_window_minutes` and are capped at
`count_cap` occurrences and `span_cap_hours` of span before a fresh row starts.

**Session state** is Continuum's live answer to "what is the user doing". It has
two halves with very different costs:

- The **mechanical** half — active project, app, window title, recently touched
  files, last error, last success, last user command — updates synchronously and
  is always free. Voice records the command in-process; desktop chat writes a
  small context intent that the runtime drains, so typed requests also survive
  process boundaries and restarts. The chat prompt runs the resulting session
  state through the same privacy egress filter as wake and MCP packages.
- The **inferred** half — current goal, current task, concrete recent activity,
  a concise evidence-backed interpretation, an optional timely help suggestion,
  and confidence — costs a local-LLM call and therefore runs in its own
  background task. `activity` says what visibly happened inside apps;
  `interpretation` is a conclusion, never hidden chain-of-thought; and
  `suggested_help` stays empty when silence is more useful. It fires on a
  project switch, after enough significant events, after a short configurable
  focus-switch sequence, or on staleness; never more than once per
  `[session_state] infer_min_interval_secs`; never while the machine is idle;
  always behind interactive triage in the LLM priority gate and capped at 256
  tokens. Every inferred claim in a reply below `confidence_floor` is discarded
  rather than stored — consumers render "unknown", which is the honest answer.

The Context page also publishes an independent, bounded `activity_trace` from
recent privacy-filtered perception frames. Consecutive identical
app/title/caption observations collapse, but leaving and later returning stays
as a distinct step. Each row carries the app, title, concrete caption or
mechanical fallback, confidence, visible-error flag, and current dwell counter;
screenshot paths never enter the published view.

On boot, session state rehydrates from the last published snapshot plus the most
recent events with confidence discounted by age, so saying **"ga door"** after a
restart resolves to what you were actually doing rather than nothing. If nothing
clears the continuation confidence floor, Continuum asks one short question
naming up to three candidates instead of guessing.

---

## What the AI can ask for

Eleven read-only, privacy-gated tools. They read only what the runtime already
published, they degrade to `available: false` / `stale: true` instead of
erroring, and `[context_tools] enabled = false` empties the whole family without
changing a single schema. Full argument and response schemas are in
`docs/mcp-tools.md`.

| Tool | Purpose | What the model gets back |
|---|---|---|
| `context_session` | Live session state | Project, goal, task + confidence, app, window title, open files, last error / success / user command, `local_only` flag, staleness |
| `context_window` | Foreground window + recent focus | Process, title, zone, pid, exe path, monitor, dwell seconds, plus the last N focus switches (`limit`, default 10, max 50) |
| `context_screen` | What is on screen | Per-monitor caption with a zone marker, plus the compact world render |
| `context_audio` | What was heard | Latest published transcript + timestamp, in-call and mute state |
| `context_projects` | Which projects exist | Configured, confirmed and discovered projects, active first |
| `context_processes` | Meaningful background activity | Active build/runtime/AI/service processes with PID, CPU, memory, start time and scrubbed executable path; never command lines or environment variables |
| `context_timeline` | What happened | Deduped events with counts, filterable by `since` / `until` / `types` / `project` / `source`, `limit` default 50, max 200 |
| `context_search` | Find an event | Full-text search over event summaries and window titles (`query`, `limit` default 20, max 50) |
| `context_files` | What files changed | File events, optionally per project (`limit` default 20, max 100) |
| `context_git` | Repository state | Branch, dirty/staged/untracked, ahead/behind, conflicts, last commit id + subject. No argument reads published state; naming a project runs a timeout-bounded probe |
| `context_package` | Everything at once | The assembled context package as markdown, with token count, which sections actually rendered after privacy filtering and budget drops, per-section staleness, and which drop-ladder rungs ran |

Three cross-cutting rules are worth knowing:

- **Withheld is counted, not hidden.** When a private row is filtered out of
  `context_timeline` / `context_search` / `context_files`, the response reports
  `omitted_private: N`. The model learns that something exists without learning
  what it was.
- **An unconfirmed project is never probed.** `context_git` refuses a project
  whose status is still `discovered` with a one-line reason, not an error —
  Continuum does not run commands in a directory you have not confirmed. The
  instruction to the model is to ask you to confirm it, not to retry.
- **Zones are re-checked at read time.** A rule you add today also hides rows
  that were written yesterday.

Access: the orchestrator reaches all ten through its `mcp__continuum__*`
wildcard; the desktop chat lists them explicitly. **Workers do not get them
under the default allowlist** — `[workers] default_allowed_tools` grants
`mcp__continuum__memory_*`, `system_*`, `fs_*` and `web_fetch` only. Add
`mcp__continuum__context_*` to that list if you want workers to have them.

There is no `context_sessions` tool and there does not need to be: past session
summaries are vault notes, so `memory_vault_search` with `types: ["session"]` is
the search for them.

### Three existing tools are now content-filtered

`system_active_window`, `system_clipboard_get` and `system_live_context` now
route through the same privacy filter. **Their names and schemas are unchanged**
— this is a content change, not an API change. What differs:

- `system_active_window` returns the sentinel for an excluded window and the
  redaction literal for a `local_only` one; otherwise the title is scrubbed and
  the process path is path-scrubbed.
- `system_clipboard_get` has three gates: the
  `[context_tools] clipboard_tool_enabled` kill-switch, a skip when the
  foreground window is in a `never_observe` zone (you excluded your password
  manager; the clipboard holds what you just copied out of it), and `scrub_text`
  on whatever survives.
- `system_live_context` runs the whole world state through the cloud gate, and
  renders its `compact` field *from the already-gated state* so the two halves
  cannot disagree.

---

## The Context page

The dashboard's **Context** tab shows what Continuum currently believes and is
the place to correct it. It is a read-only view of the runtime's published
`state.json` plus a write-only intent queue: the dashboard process cannot open
the raw-log database, the vault index or the episodic store itself, so
everything it lists arrives in the published snapshot (refreshed every 5 s,
published every 2 s) and every action is fire-and-forget — clicking writes one
intent file to `~/.continuum-dev/context-intents/` and returns; the page updates
when the runtime republishes. Intents have **no expiry**: a correction made
while the runtime is stopped still applies at its next boot.

| Action | What it does |
|---|---|
| **Add project** | Writes the project into the Projects table immediately (the runtime picks it up live) and appends `[[projects.known]]` to `config.toml` for the next boot |
| **Confirm project** | Promotes a discovered candidate to confirmed. Until then Continuum never collects from it — no git polling, no file watching |
| **Correct** (project or goal) | Updates session state, writes a persistent resolver override rule, and files a Confirmed preference note (zone-inherited: a `local_only` correction becomes a `Sensitive` note) |
| **Not this project** | Writes a persistent `exclude_project` override rule at the highest resolver tier |
| **Pin** | Freezes a session-state field and persists across restarts. A pin blocks *overwrite only* — the resolver still resolves, collectors still collect, and events are still stamped with the real project. It clears automatically when the resolver reports the same different project at git-root confidence or better for `[projects] switch_min_secs` |
| **Forget** | Cascade-deletes one observation (see below) |
| **Delete range** | Purges frames, events, episodic memories, screenshot files and frame-derived vault candidates inside a start/end window |
| **Toggles** | Flips the five observation switches live and persists them to `config.toml` |

Every one of these writes a line to the audit log at
`<data_dir>/logs/actions.jsonl` — including the ones that fail.

### What Forget reaches, and what it cannot

Forget is keyed on an observation's `raw_reference` and runs four independent
rungs; a rung that fails is logged and the rest still run, because a
half-succeeded deletion beats one that aborts on the first locked file.

It **removes**: every `context_events` row pointing at that reference (the
full-text mirror follows automatically), the referenced `perception_frames` row
and its screenshot file, episodic memories derived from that frame, and the
unconfirmed vault candidate whose source is that frame.

It **cannot reach**:

- **Anything that is not a frame.** If the reference is a git commit id or
  another synthetic key, only the event row is deleted — there is no frame,
  screenshot or derived memory to cascade to.
- **Confirmed vault notes.** Only the *unconfirmed candidate* is swept. Once you
  or the curator promoted a note, Forget leaves it alone; delete it from the
  Memory tab.
- **Prose that merely mentions the moment.** A curator session summary or a
  distilled memory that paraphrases several observations is not keyed on any one
  `raw_reference` and survives.
- **Episodic memories with no frame attribution.** Memories distilled from the
  fallback frame path carry no source frame id and are not matched.

Use **Delete range** when you want the whole window gone rather than one row.
It runs the same cascade as Forget over every frame in the window, including the
unconfirmed vault candidates derived from those frames.

Both deletion paths ignore `[storage] delete_screenshots_with_rotation`. That
knob governs **rotation** — whether Continuum tidies up screenshots on its own
schedule. A deletion you asked for by name always takes the JPEG with the row;
anything else would leave the file orphaned on disk, unreachable by any later
targeted delete.

### One thing the page cannot do yet

Override rules are **listed but not deletable** from the page. Pins can be
cleared there; removing a rule currently means editing the Projects database.
The page says so in place.

---

## Configuration reference

Every value below is the shipped default read from `config.rs`. All of these
sections are optional in `config.toml` — omit one and the defaults apply.

### `[privacy]`

| Key | Default | Meaning |
|---|---|---|
| `scrub_api_keys` | `true` | Redact API keys and bearer tokens |
| `scrub_cards` | `true` | Redact Luhn-valid card numbers |
| `scrub_iban` | `true` | Redact IBANs |
| `scrub_emails` | `false` | Redact email addresses |
| `zones` | `[]` | `[[privacy.zones]]` entries: `match_process?`, `match_title_keyword?`, `zone` (required) |

### `[privacy.toggles]`

`mic = true`, `screen = true`, `files = true`, `git = true`, `pause_all = false`.

### `[projects]`

| Key | Default |
|---|---|
| `auto_discover` | `true` |
| `switch_min_secs` | `20` (hysteresis before a project switch counts) |
| `discover_min_secs` | `30` (how long a path must be visible to be proposed) |

Project entries are `[[projects.known]]` (not `[[projects]]` — TOML cannot host
both a table and an array under one key): `id` (required, lowercase slug
`[a-z0-9-]`), `name`, `root_paths`, `repo`, `keywords`, `zone`.

### `[git_context]`

`enabled = true`, `poll_secs = 30`, `command_timeout_secs = 10`,
`min_spawn_interval_secs = 5`.

### `[file_watcher]`

| Key | Default |
|---|---|
| `enabled` | `false` — opt-in |
| `debounce_ms` | `1000` |
| `ignore_globs` | `["node_modules", "target", ".git", ".next", "dist", "build", "__pycache__", "*.tmp"]` |
| `rearm_secs` | `60` (retry an unavailable root) |
| `storm_threshold` | `50` (above this, one coalesced "N files changed" event) |

### `[events]`

`collapse_window_minutes = 10`, `retention_days = 30`, `count_cap = 500`,
`span_cap_hours = 24`, `queue_cap = 1024`.

**How repeats collapse onto one row.** Two events collapse when they hash
to the same dedupe key inside the collapse window. The key is
`hash(source, event_type, project_id, discriminator)`, and the
discriminator depends on where the event came from:

| Source | Discriminator | Why |
|---|---|---|
| `screen`, `audio` | the application | LLM summaries are never byte-stable — "build failed ×14" has to collapse regardless of wording |
| `file` (`file_modified` / `created` / `deleted` / `renamed`) | the **raw** summary | that summary *is* the path; normalizing it away made every file in a project share one row |
| everything else (incl. `files_bulk_change`) | the normalized summary | prose templates, where stripping digits/paths is what lets repeats collapse |

### `[memory]` distillation thresholds

The §4.11 compression ladder has two rungs and **two thresholds**, because
they measure different things:

| Key | Default | Gates |
|---|---|---|
| `distillation_min_event_importance` | `0.15` | deduped `context_events` rows — the primary rung |
| `distillation_min_salience` | `0.35` | raw perception frames — the fallback rung |

Deterministic collector events (focus switches, commits, file changes,
system events) are emitted at importance `0.2` on purpose: individually
they are bookkeeping, together they are "what happened today". The event
threshold therefore sits *below* that — with one shared threshold of
`0.35` the "primary" input was empty by construction and only classifier
output scoring above 0.35 ever reached episodic memory. Raise
`distillation_min_event_importance` toward `0.35` for a quieter episodic
store.

A classification whose JSON omits `importance` is scored `0.4`, not `0.0`
— an omitted score means "the model did not say", which is not the same
claim as "worthless".

**A moment is recorded once.** A frame whose classification produced a
usable event is stamped `perception_frames.context_event_at`, so the
fallback rung skips it — including the second through fourteenth
occurrence of a repeat, which collapse into an existing row and leave no
other trace. A blank-summary event does *not* claim its frame this way:
blank rows are excluded from the primary rung forever, so letting them
suppress the frame would erase the moment from memory entirely.

**Sensitivity follows the text.** A distilled memory inherits the §4.1
zone of whatever it was distilled from (the event's `sensitivity`, or the
frame's own zone on the fallback rung) and stores it alongside the
summary. Local-only memories are kept — they are memories of your own day
— and are rendered for local models, but the cloud egress gate withholds
them from the wake package exactly like the events they came from.

### `[session_state]`

`infer_min_interval_secs = 120`, `infer_max_age_minutes = 10`,
`infer_min_new_events = 8`, `significant_importance = 0.5`,
`confidence_floor = 0.4`, `infer_max_tokens = 256`.

### `[context_package]`

| Key | Default |
|---|---|
| `token_budget` | `1000` (wake profile) |
| `chat_token_budget` | `600` (desktop chat profile) |
| `events_window_minutes` | `20` |
| `max_just_before` | `5` |
| `max_memories` | `3` |
| `max_facts` | `8` |
| `max_vault_notes` | `5` |
| `max_pending_decisions` | `5` |
| `max_recent_changes` | `5` |
| `max_failed_attempts` | `3` |
| `max_open_files` | `5` |
| `max_tools` | `12` |
| `world_compact_max_chars` | `1200` |

Under budget pressure sections are dropped in this order: open files → recent
changes → the tail of "just before" → the tail of memories. Why-woken, the
current moment, session state and pending decisions are never dropped.

### `[continuation]`

`confidence_floor = 0.6`,
`trigger_phrases = ["ga door", "continue", "verder", "waar was ik", "pak weer op"]`,
`wake_result_lookback_hours = 12`.

### `[performance]`

`idle_pause_after_secs = 300` (`0` disables idle mode),
`idle_capture_interval_ms = 20`, `idle_vision_interval_ms = 20` (`0` pauses
vision entirely while idle).

Continuous visual context does not throttle by default when idle, so unattended
screen changes remain observable. Users can explicitly choose slower idle
cadences when resource or battery use matters more than timeline completeness.

### `[context_tools]`

`enabled = true` (master switch for all ten context tools),
`clipboard_tool_enabled = true`.

### `[storage]` (context-engine additions)

`delete_screenshots_with_rotation = true`, `screenshot_max_age_hours = 720`
(`0` disables the age sweep). Keep `screenshot_max_age_hours` at or above
`retention_days * 24` — the sweep is mtime-based and never consults the database.
Neither setting affects **explicit** deletion (Forget / Delete range), which
always removes the referenced files.

### `[memory.candidate_ttl_days]`

Expiry for observation-derived vault candidates: `task = 30`, `error = 30`,
`note = 90`; `decision` and `preference` have no TTL by default. `0` means
"never expires".

### `[frame]`

`salience_threshold = 0.10` — the minimum salience score for a frame to reach
triage. `interval_secs = 3`.

### `[triage]`

`context_size = 4096` (raised from 2048 to fit the classification block),
`max_tokens = 256`.

---

## Running the evaluation benches

All four harnesses run offline and deterministically in **mock mode** by
default: no GPU, no network, no model download. Mock mode gates the *plumbing*
(project resolution, hysteresis, zone propagation, dedupe keys, session
mechanics, distillation selection, privacy gates) — it does not measure model
quality, because the classification is scripted.

```bash
# Context recall: project / goal / task / blocker / last-action against labels
cargo run -p continuum-core --bin continuum-context-bench
cargo run -p continuum-core --bin continuum-context-bench -- --live   # real model

# Dedupe precision: collapse rate over the build-failure loop
cargo run -p continuum-core --bin continuum-dedupe-bench

# Memory precision: duplicate rate, precision vs labels, later-used report
cargo run -p continuum-core --bin continuum-memory-precision-bench

# Safety redaction: zero secrets across every MCP tool response path
cargo run -p continuum-mcp --bin continuum-redaction-bench

# Triage prompt-fit (the no-GPU half of the triage gate) — run from the repo root
cargo run -p continuum-core --bin continuum-triage-bench -- --prompt-fit-only
```

Threshold overrides: `--project-recall`, `--goal-recall`, `--action-recall` on
the context bench; `--collapse` on the dedupe bench; `--precision` and
`--duplicates` on the memory-precision bench. The redaction bench takes no
arguments at all.

On a memory-constrained machine, build the bench binaries with `-j 2`.

### The fixture

The committed fixture is **synthetic and generated**, never a real recording:

- `crates/continuum-core/benches/data/context-20min.jsonl`
- `crates/continuum-core/benches/data/context-20min.labels.json`

Regenerate it after changing the generator with:

```bash
cargo run -p continuum-core --bin continuum-context-bench -- --write-fixture
```

A test fails if the committed files drift from the generator.

### Recording your own session

```bash
cargo run -p continuum-core --bin continuum-perception -- --record ~/.continuum-dev/recordings/today.jsonl
```

**Recordings are local-only.** A recording is post-privacy (so it is safe to
keep) but it is exactly the content the privacy work exists to protect — it is
never safe to publish, and no real recording belongs in this repository.

---

## Honest limitations

Known gaps, stated plainly. None of these are silent.

**Retrieval**

- **Vault retrieval at wake time under-recalls.** `retrieve_vault_context`
  builds its query with prose labels ("App: …", "User said: …") and the
  full-text index joins every token with implicit AND, so the literal word
  "app" must appear in a note for *any* hit. In the memory-precision bench this
  makes the later-used rate structurally 0.0%; the bench reports "wake retrieval
  hits" beside it so the zero is diagnosable rather than mysterious. The fix
  (drop the labels, or OR the terms) is a recall-vs-precision decision that has
  not been made.

**Deletion and corrections**

- **The Forget cascade cannot reach confirmed vault notes, distilled prose, or
  memories without a frame attribution** — see "What Forget reaches" above.
- **Override rules are not deletable from the Context page.** The intent
  vocabulary is eight kinds; rule deletion needs a ninth plus a delete path in
  the store and the resolver.

**Auditing**

- **MCP tool calls are not in the audit log.** Wakes, toggle changes,
  corrections and deletions all are. Tool calls happen inside the
  `continuum-mcp` process, which would need its own audit wiring.

**Compression**

- **The dedupe key for classified events deliberately excludes the summary**, so
  "cargo build finished: 0 errors" and "cargo test finished: 214 passed" collapse
  into one row (same source, type, project and application) and only the first
  summary is kept as display text. This is correct per the design, and it is the
  first place the compression ladder's information loss is measurable rather
  than theoretical.

**Benchmarks**

- **The GPU triage re-baseline is still manual.** The prompt shrank by roughly
  1.4 kB per frame when the world-state blob moved out of it, so the old p95
  figure is stale, and the voice-ticker p99 assertion is not written yet. The
  prompt-fit gate runs without a GPU; the latency numbers need a machine with
  one.
- **The memory-precision bench measures duplicate similarity by token overlap,
  not embeddings**, because the embedding model downloads on first use and a
  bench that reaches for the network is neither offline-safe nor compatible with
  never-phoning-home.

**Not built yet**

- **UI Automation reading, local OCR, a clipboard *watcher*, and the full audit
  and epistemic-label UIs** are the next tier, not this one.
- **Browser extension, IDE plugin and terminal plugin** are external
  integrations, further out still.
- Consequently, project and activity detection is driven by window titles, editor
  title patterns, git roots and keywords. It does not read your editor's or
  browser's internal state.
