# Self-Healing Runbook

This document records component health and recovery hooks that the repair agent can use.

## Repair agent overview

The dashboard's **Fix Issues** button spawns a dedicated Claude Opus 4.6 session with:

- Working directory: the Continuum install folder.
- System prompt: [`prompts/repair-agent-system.md`](../prompts/repair-agent-system.md).
- Input context: written to `~/.continuum-dev/repair-context.md` before the spawn and sent inline after secret fields are redacted. It contains the user-reported problem (if any), component statuses, the live config snapshot, and the last 500 log lines.
- A dedicated Continuum MCP server restricted to component tests. Built-in Claude tools, project/user settings, slash commands, and session persistence are disabled. Manual next steps are reported directly in the streamed assistant output.

Output streams live to the Health tab via `continuum:repair` events (text deltas, tool calls, tool results, stderr, final status).

## Permission gateway

Component: `permissions`

- Health check: `PermissionGateway::health()` validates bundled defaults and
  user overrides and counts malformed request/grant records.
- Logs: structured `component = "permissions"` warnings plus durable
  permission decisions in `<data_dir>/logs/actions.jsonl`.
- Recovery: a malformed `permissions.toml` causes fail-closed denial. Restore
  it from the latest Continuum backup or remove only the malformed override
  after preserving a copy; bundled defaults then take effect. Malformed
  request/grant JSON can be quarantined individually without changing policy.
- Restart rule: `PermissionHealth::should_restart()` is true when effective
  policy cannot be parsed. Restarting alone does not weaken or bypass policy.

## Git checkpoint broker

Component: `git_tools`

- Health check: run `git --version`, then a read-only `git rev-parse
  --show-toplevel` in an allowlisted confirmed project.
- Logs: MCP tool audit plus permission decisions in `logs/actions.jsonl`.
- Recovery: orphaned files in `.git/continuum-tmp/` are temporary indexes and
  can be removed when no MCP Git call is active. Never delete
  `.git/continuum-recovery/`; it contains rollback recovery copies.
- Restart rule: restart the MCP process after a command timeout. Git
  subprocesses use `git_context.command_timeout_secs` and are killed on drop.

## Filesystem action broker

Component: `file_actions`

- Health check: verify `<data_dir>/recovery/files/` is creatable and perform an
  atomic create/rename/delete probe inside that directory only.
- Logs: tool and permission audit records in `logs/actions.jsonl`; mutation
  responses include their recovery path.
- Recovery: originals from patches and recoverable deletes live below
  `<data_dir>/recovery/files/<date>/`. Move them back only after confirming the
  destination does not exist.
- Restart rule: retry after filesystem locks are released. Temporary sibling
  files named `.continuum-write-*.tmp` or `.continuum-patch-*.tmp` may be
  quarantined after verifying no MCP file action is active.

## Terminal broker

Component: `terminal_tools`

- Health check: run an allowed program's version command through
  `terminal_verify` in an allowlisted project and verify its evidence JSON can
  be read back.
- Logs: MCP/permission audit plus immutable per-run JSON under
  `<data_dir>/evidence/terminal/`.
- Recovery: a timed-out child is killed on drop. If evidence persistence fails,
  the verifier call fails instead of claiming success without proof.
- Restart rule: restart the MCP process after repeated spawn failures; first
  verify PATH and `mcp.terminal.allowed_programs` configuration.

## Native IDE bridge

Component: `ide`

- Health check: `ide_status` resolves configured editor aliases to native
  executables without opening files or inspecting editor state.
- Logs: every MCP handoff is audited with its editor and canonical target path.
- Recovery: ensure VS Code, VS Code Insiders, or VSCodium is installed and its
  command is on PATH; adjust `mcp.ide.allowed_editors` if needed, then restart
  the MCP process and call `ide_status` again.
- Restart rule: restart after repeated spawn/timeouts. A missing executable or
  denied path is a configuration/permission issue and should not be retried.

## Browser DOM bridge

Component: `browser`

- Health check: `browser_status` probes only the configured loopback port.
- Logs: MCP permission and action audits; fill content is redacted.
- Recovery: verify `mcp.browser.enabled`, the dedicated Chromium
  `--remote-debugging-port`, and exact host allowlist. Restart the browser with
  its dedicated profile if CDP is unavailable, then restart the MCP process.
- Restart rule: protocol disconnects/timeouts may be retried after one status
  check. Host denials and password-field blocks require configuration/user
  action and are never auto-retried.

## GitHub CLI bridge

Component: `github`

- Health check: `github_status` verifies `gh.exe`, active github.com auth, and
  OS-keyring token storage without showing a token or making repository calls.
- Logs: connect/disconnect and MCP permission/tool audits; tokens are excluded.
- Recovery: install/update the official GitHub CLI, then reconnect from
  Settings. Plaintext-file token storage is rejected. Disconnect locally and
  revoke the GitHub CLI OAuth grant remotely if compromise is suspected.
- Restart rule: restart the MCP process after repeated CLI spawn/timeouts; auth
  failures require reconnect rather than restart.
- Mutation failures are non-retriable by default: inspect the returned GitHub
  response, correct the request or permissions, and ask for a new confirmation.
  Continuum never automatically repeats an issue, comment, or pull-request
  creation because duplicate external writes are not safely recoverable.

## Repair MCP tools

All under the `repair_*` namespace (routed via `continuum-mcp`):

| Tool                       | Effect                                                                 |
|----------------------------|------------------------------------------------------------------------|
| `repair_restart_component` | Published MCP compatibility boundary, but denied in the safe Health session because restart intents have no runtime consumer. |
| `repair_reinstall_model`   | Published MCP compatibility boundary; denied in the safe Health session. |
| `repair_rollback_config`   | Published MCP compatibility boundary; denied in the safe Health session. The separate desktop rollback command is guarded and reversible. |
| `repair_test_component`    | Lightweight file-presence probe. Returns `healthy / degrading / error / unknown`; it is not live recovery proof. |
| `repair_escalate`          | Published MCP compatibility boundary; denied because escalation intents have no dashboard consumer. |

The one automatic Health fix that is implemented end-to-end is starting an offline runtime. The desktop verifies the one-time preview, creates and re-verifies a backup, starts the packaged runtime once, and waits for a fresh `state.json` heartbeat up to `health.runtime_start_timeout_secs` (90 seconds by default, clamped to 10–300 seconds). A timeout stops the process and reports failure. Component restart intent files are not consumed in this release and must never be presented as successful repair.

## Backup rotation

Every night at 04:00 local time, the dashboard zips these files into versioned archives under `~/.continuum-backups/<YYYY-MM-DD>/`:

- `config.toml`
- `automations.json`
- `permissions.toml`
- `semantic.sqlite`
- `orchestrator-system.md`

Deliberately excluded: raw log (`raw_log.sqlite`), episodic LanceDB folder, screenshots, model weights, logs.

Retention defaults to the most recent 7 archives and is configurable. The Health tab's **Backup now** button triggers an immediate archive.

## Predictive maintenance

The health registry runs every 30 s. A component is marked `degrading` when:

- It currently probes `healthy` but the 24 h error rate across the last 20 probes is > 5 %, OR
- Its file-presence probe reports the subsystem never signalled "loaded" (seen as a boot-time "awaiting start" state).

Components marked `degrading` show amber in the Health tab. Predictive auto-repair (automatically firing the repair agent on a degradation trend) is off by default.

## Voice-activated repair

Voice activation is not wired to the one-time desktop preview in this release. Start repair from Advanced → Health so the authorization and preview cannot be bypassed.

---

## Continuous Live Context

Component: `live_context`

Logs:

- `layer = "senses"`, `component = "vision"` for monitor discovery, capture,
  change selection, and local vision failures.
- `layer = "senses"`, `component = "live_context"` for projection publication.
- `~/.continuum-dev/live-context.json` contains bounded health counters and the
  last capture timestamp; it never contains raw screenshot bytes or raw input.

Health check:

- Healthy when capture is enabled, connected monitors are represented, the last
  capture is recent, and there is no sustained buffer loss.
- Degrading when no monitor is represented, more than 10% of capture events
  were dropped, or more than 10% exceeded the configured capture deadline.
  Drops and cadence misses are explicit overload evidence, not hidden pauses.
- Error/restart-required when `LiveContextHealth::should_restart()` detects a
  sustained capture stall or repeated capture failures.

Recovery:

1. Confirm **Brain → Continuous local context** is enabled and the intended
   monitor IDs are not in `screen.excluded_monitor_ids`.
2. Inspect `live-context.json` health counters and recent source-attributed
   events. If drops rise, increase `screen.buffer_capacity`, increase
   `screen.vision_min_interval_ms`, or relax capture cadence.
3. Restart the Continuum runtime to re-enumerate displays and restart all
   monitor workers. Hot-plug discovery normally repairs topology within 2 s.
4. Persistent xcap failures require checking the interactive Windows desktop
   session and display drivers; do not delete local user data.

Runtime proof boundary: unit tests cover event ordering, bounded oldest-drop
behavior, compact projection limits, privacy classification, and restart logic.
CI compilation/integration tests validate wiring. Actual simultaneous cadence
across multiple physical Windows monitors requires a live machine with multiple
displays and is not established by unit tests alone.

---

## Context Engine Components

The `continuum` runtime process has **no `HealthRegistry`** — its health
surface is `RuntimeSnapshot.context_engine` in `~/.continuum-dev/state.json`,
refreshed on the same 2 s publish loop as every other runtime counter (the
curator precedent). Each entry is a uniform summary:
`{ healthy, enabled, should_restart, detail }`. Per spec §7,
disabled-with-reason states report `healthy: true, enabled: false` with the
reason in `detail` and never request a restart. `RuntimeSnapshot.paused`
mirrors `[privacy.toggles].pause_all`; when the runtime *booted* paused, the
three watcher entries (`context_watcher`, `git_watcher`, `file_watcher`) report
`"not running (pause_all set in [privacy.toggles])"` because those tasks were
never spawned — that is deliberate, not a failure. `live_context` and
`events_writer` always have a real handle.

The seven published keys are `idle` (a plain bool from the cadence
controller), `context_watcher`, `live_context`, `git_watcher`,
`file_watcher`, `events_writer` and `triage`. Components without a key —
the privacy filter, the project resolver, session-state inference and the
intent drainer — are covered below with the reason each has nothing to
probe.

### Context watcher (`context_watcher`)

- Logs: `layer = "senses"`, `component = "context"`.
- Health: the 1 Hz poll loop stamps `last_poll_at` every tick. Healthy while
  the last poll is within 3 poll intervals (min 5 s); `should_restart` only
  after a sustained stall of 10 intervals (min 30 s) of an *enabled* loop.
- Recovery: restart the `continuum` runtime (the poller is stateless). A
  stall almost always means the tokio runtime itself is wedged — check the
  log tail for a panic in another senses task first. `pause_all` parks the
  poller disabled-with-reason; clear the toggle and restart.

### Project resolver

- Logs: `layer = "context"`, `component = "project"`.
- Health: no entry, and deliberately so — it is a pure in-frame-loop
  computation over the Projects table with no task, channel or subprocess.
  It cannot stall independently of the frame loop, and every failure mode
  is a *wrong answer*, not a dead component. A resolution it is unsure of
  simply returns a low confidence.
- Recovery: the Projects table (raw-log DB) is authoritative and
  `config.toml`'s `[[projects.known]]` entries are only the boot seed, so
  the fixes are data fixes, not restarts. Wrong project: use **Correct**
  or **Not this project** on the Context page — both write a persistent
  override rule at the highest resolver tier. Flapping between two
  projects: raise `[projects] switch_min_secs` (the hysteresis window).
  Nothing being resolved at all: check that at least one project is
  **confirmed** — a `discovered` candidate is never collected from, by
  design. Auto-discovery can be switched off with
  `[projects] auto_discover = false`.

### Cadence controller / idle mode (`context_engine.idle`)

- Logs: `layer = "senses"`, `component = "idle"`; `idle_start` / `idle_end`
  system events in `context_events`.
- Health: no probe of its own — it is pure shared atomics
  (`senses/cadence.rs`, the spec §3 sanctioned pattern). `idle: true` in the
  snapshot plus relaxed cadences is normal after
  `[performance].idle_pause_after_secs` of no input.
- Recovery: if capture seems stuck at the idle cadence while the user is
  active, any restore trigger fixes it (input activity, voice wake, hotkey,
  any wake); persistent wrong state means the frame loop stopped delivering
  frames — see the context watcher / live context entries above. Set
  `[performance].idle_pause_after_secs = 0` to disable idle mode entirely.

### Session-state inference (`session_state`)

- Logs: `layer = "context"`, `component = "session_state"`.
- Health: no probe of its own, and deliberately so — it is a *best-effort
  enrichment*, not a collector. Every failure mode is already a healthy
  state: no triage model loaded means the task is never spawned (mechanical
  fields still update); a gate timeout, a model error, or a reply that is
  not JSON all leave the previous goal/task in place and log at
  `debug`/`warn`. Nothing downstream blocks on it — consumers render
  `"unknown"`.
- Recovery: nothing to restart in isolation. If goal/task stay `unknown`
  while the user is clearly working, check in order: (1) is a triage model
  loaded (no model → no curator, no inference); (2) is the machine idle
  (`context_engine.idle: true` pauses inference by design, spec §4.11);
  (3) is `[session_state].confidence_floor` set too high — the model may be
  answering with a low confidence that is being discarded correctly;
  (4) grep the log for `"Session-state inference reply was not JSON"`,
  which means the local model is ignoring the JSON instruction in
  `prompts/session-state.md`. Lowering `infer_min_interval_secs` makes it
  try more often at a proportional GPU cost; it will never preempt
  interactive triage regardless.
- Rehydration: a wrong-looking project/task right after a restart is the
  boot rehydration (spec §4.8) surfacing the *previous* session,
  staleness-discounted. It is corrected by the first inference pass. Delete
  `~/.continuum-dev/state.json` to start from a blank state.

### Git collector (`git_watcher`)

- Logs: `layer = "senses"`, `component = "git_watch"`.
- Health: always `healthy: true` — a missing `git` binary, config-off, or
  toggle-off parks it disabled-with-reason; transient probe failures keep
  the last known state (`consecutive_failures` in `detail`).
  `should_restart` is always `false`: a restart cannot install git.
- Recovery: install git (or fix `PATH`) and restart the runtime — the
  binary is probed once at collector start. Confirm `[git_context].enabled`
  and `[privacy.toggles].git`. Repeated probe timeouts on a healthy repo:
  raise `[git_context].command_timeout_secs`.

### File watcher (`file_watcher`)

- Logs: `layer = "senses"`, `component = "file_watch"`.
- Health: disabled-with-reason by default (`[file_watcher].enabled = false`
  — opt-in). Per-root unavailability (deleted/unmounted root) is healthy:
  the root is retried every `[file_watcher].rearm_secs` and listed in
  `detail`; other roots are unaffected. `should_restart` is `true` ONLY
  when the notify event channel itself died.
- Recovery: for `channel_dead`, restart the runtime — watches are re-armed
  from the Projects table, nothing is lost. For an unavailable root,
  restore the directory (or remove the project root from config) and wait
  one rearm tick; no restart needed.

### Background-process collector (`process_watcher`)

- Logs: `layer = "senses"`, `component = "process_watch"`.
- Health: disabled-with-reason by default because `[process_watcher].enabled`
  is an opt-in consent boundary. While enabled, `detail` reports poll count,
  active significant processes, emitted events, and `last_poll_at`.
  `should_restart` is always `false`: an individual process-table refresh or
  snapshot write is retried on the next poll, so restart-thrashing cannot help.
- Recovery: confirm the data directory is writable when `processes.json`
  publication fails. Adjust `[process_watcher]` thresholds/include/exclude
  names when the signal is too noisy or too sparse, then restart the runtime;
  configuration is read at startup.

### Events writer (`events_writer`)

- Logs: `layer = "memory"`, `component = "events"`.
- Health: `queue_depth`, `rows_written`, and `last_flush_at` in `detail`.
  `should_restart` is `true` only when the writer task died without a
  shutdown signal; clean shutdown never restarts. A persistently growing
  `queue_depth` means SQLite writes are stalling (disk full, DB locked by
  another process) — producers never block; overflow drops are coalesced
  into `events_dropped` rows, so data loss is visible in the events table
  itself.
- Recovery: restart the runtime to respawn the writer. If flushes keep
  failing, inspect `~/.continuum-dev/raw_log.sqlite` (disk space, WAL
  lock); the dedupe LRU is in-memory and rebuilds itself, and rows are
  only ever inserted transactionally, so a crash loses nothing committed.

### Triage evaluation (`triage`)

- Logs: `layer = "triage"`, `component = "coalesce"`.
- Health: `enabled` follows whether a triage model loaded (no model is a
  healthy, disabled-with-reason state). `should_restart` is `true` when a
  single evaluation has been "in flight" far longer than any plausible
  model latency — the signature of an evaluation task that died without
  releasing the coalescer, which silently parks every subsequent frame.
- Recovery: restart the runtime. Nothing is lost — frames skipped while
  the coalescer was wedged were, by definition, never evaluated, and the
  session state, event log and vault are all written by other tasks.

### Live-context publisher (content-versioned)

- The `live-context.json` publisher writes only when the hub's
  content-version counter moved (spec §4.11) — a stale file mtime during
  a quiet/idle period is **expected**, not a stall. Judge freshness by the
  `live_context` health entry (capture counters) instead of the file's
  mtime. Recovery for genuine capture stalls: see "Continuous Live
  Context" above.

### Context-intent drainer

- Logs: `layer = "context"`, `component = "intent"`.
- Health: no entry of its own — it is a directory scan that runs inside the
  same 250 ms ticker arm as push-to-talk voice intents, so if it stopped the
  whole main loop stopped and every other entry above would already be
  alarming. It never returns an error to the loop: an absent directory is an
  empty result, and each handler's failure is logged and audited while the
  next intent still runs.
- **The failure signal is a pile of `.bad` files.** Intent files live in
  `<data_dir>/context-intents/*.json`. An unparseable file or an unknown
  `kind` is renamed to `.bad` with a warning and never retried; a file that
  cannot be *read* is left in place for the next tick. Intents have no TTL
  by design (a correction made while the runtime was down is a durable
  decision, not a stale push-to-talk), so a stopped runtime accumulating
  `.json` files is normal and they all apply at the next boot.
- Recovery: `ls ~/.continuum-dev/context-intents/*.bad`. A few mean a
  dashboard/runtime version mismatch — the writer is emitting a shape this
  runtime does not know; upgrade whichever half is behind. They are inert
  and safe to delete. If actions appear to do nothing while no `.bad` files
  appear, confirm the runtime is publishing at all (`state.json` mtime) —
  the Context page is fire-and-forget and does not report delivery.
  Duplicate deliveries cannot happen: the drainer de-dupes by intent id over
  a bounded seen-set of 512.
- Verifying an action landed: `<data_dir>/logs/actions.jsonl` gets one line
  per applied intent **including failures**, with `actor: user`.

### Privacy filter

- No health entry: it is a pure in-process function (`senses/privacy.rs`)
  with no task, channel, or external dependency — there is nothing to
  restart. Misconfiguration surfaces as scrubbed/sentinel output, which is
  the filter working as designed.
- The one operational failure mode is a rule that is too broad: a
  `never_observe` process pattern matching your editor makes Continuum go
  silent about your actual work while every component still reports healthy.
  If session state is permanently `[private]`, read `[privacy.zones]` (and
  the legacy `[context] sensitive_process_names` list, which is synthesized
  into the same rule set) before suspecting a collector.

---

## Memory Distiller

Component: `memory/distiller`

Logs:

- `layer = "memory"`, `component = "distiller"`
- Startup logs include interval and lookback settings.
- Failed passes log the error and continue on the next interval.

Health check:

- Healthy when `memory.distillation_enabled = false`, or when the task can query `RawLog` and insert `Remember` events into `EpisodicStore`.
- A rising count of repeated `Memory distillation pass failed` warnings means the repair agent should inspect the raw log database and LanceDB directory.

Recovery:

- Restart the `continuum` runtime to restart the distiller task.
- If SQLite schema migration failed, run `cargo check -p continuum-core` and inspect `~/.continuum-dev/raw_log.sqlite`.
- If LanceDB writes fail, inspect `~/.continuum-dev/episodic_db`.
- Raw frames are marked with `memory_distilled_at` only after successful episodic inserts, so restarting does not lose undistilled frames.

## Memory Vault

Component: `memory_vault`

Logs:

- `layer = "memory"`, `component = "vault"` for note read/write/delete and index rebuild events.
- `layer = "memory"`, `component = "watcher"` for file-watcher batches and watcher startup failures.
- Structured `tracing` events, same as every other component — no dedicated
  log file; visible via stdout and the dashboard's Logs tab (in-memory ring
  buffer, see "Tabs" in `docs/dashboard.md`).

Health check (`MemoryVaultCheck` in `apps/desktop/src-tauri/src/components.rs`, registered via `register_memory`):

- Talks to `MemoryState` directly (not `state.json`), so it stays meaningful even when the headless `continuum` runtime is offline — the vault has its own lifecycle independent of the runtime.
- **Healthy**: the vault opens and `Vault::info()` reports zero quarantined files.
- **Degrading**: the vault opens but one or more files are quarantined (unparsable frontmatter) — reported as `"N note(s) quarantined due to parse errors"`.
- **Error**: the vault fails to open at all (e.g. the index directory is locked or the disk is unwritable).

Recovery:

- **Quarantined file(s)** (degrading): open the vault folder (Memory tab → **Open vault**, or the probe's own recovery note) and fix the offending note's YAML frontmatter — indentation, a missing required field (`id`/`type`/`title`/`status`/`created`), or an invalid date. The file rejoins the index automatically on the next reindex (the file-watcher picks up the save); no restart needed.
- **Corrupt or schema-mismatched index** (surfaces as an error on open, or just "things look stale/wrong"): delete `vault/.continuum/index.db` and restart — `Vault::open` always performs a full rebuild from the markdown, so this is non-destructive by design. Equivalently, use the Memory tab's **"…" menu → Rebuild index**, which does the same thing without a restart.
- **Watcher dead** (live updates stop arriving, but every command still works against the vault directly): restart the dashboard (or the `continuum` runtime, whichever's watcher died) — the watcher bridge is stateless and simply re-opens on the next start; nothing is lost since the vault itself was never touched.
- The Memory tab's **Open vault folder** and **Rebuild index** actions (in the "…" vault-actions menu, "Open vault" also has its own topbar button) are the user-facing versions of the two recovery steps above and are always available, even when the vault has failed to open — `memory_open_vault` deliberately does not depend on a healthy index (see the comment on `MemoryState::vault_dir` in `memory.rs`).
- Full data model, config, and quarantine details: `docs/memory.md`.

## Memory Curator

Component: `memory/curator`

Logs:

- `layer = "memory"`, `component = "curator"` for extraction passes, conflict
  detection, session summaries, hygiene, and wipe-request draining.
- Every LLM parse failure logs a `warn` and retries once before the pass (or
  pair, for conflict detection) is skipped; a window that fails outright 3
  times in a row logs a `warn` and is abandoned (see `docs/memory.md`'s
  "Failure policy").

Health check — there is **no dedicated MCP health tool and no runtime
`HealthRegistry` entry** for the curator (unlike the repair-agent tools
above, which are a separate subsystem). The curator's health surfaces
through the same 2 s `state.json` publish loop every other runtime counter
uses:

- The runtime publishes a `CuratorSnapshot` (`last_pass_at`,
  `consecutive_failures`, `candidates_written_total`, `pending_count`,
  `enabled`) as part of `RuntimeSnapshot.curator` in
  `crates/continuum-core/src/runtime_publish.rs`. `None` only for a
  `state.json` written before this field existed; once the runtime is up it
  is always `Some` — `enabled: false` with zeroed counters is how a
  never-spawned or config-disabled curator reports, not `None`.
- The desktop dashboard mirrors this into `MemoryState.curator` and renders
  it as a "Curator" row on the Home tab (`docs/dashboard.md`): a
  `StatusBadge` showing `healthy` normally, `degrading` once
  `consecutive_failures >= 3` (the same threshold the doc-comment on
  `CuratorSnapshot::consecutive_failures` calls out), plus last-pass time,
  pending count, and written count. `!curator || !curator.enabled` renders
  "Curator: off" — this covers both "runtime never connected" and "runtime
  connected, curator genuinely off" identically, since there's no
  user-facing reason to distinguish them.
- The curator has **no** `should_restart()`/`repair_test_component` probe —
  it is not registered in `apps/desktop/src-tauri/src/components.rs`'s
  health-check grid, and there is no `repair_restart_component` target for
  it. This is a real gap against this doc's usual self-healing contract,
  not an oversight to paper over: the dashboard's Home-tab row is currently
  the only signal a degrading curator surfaces.

Recovery:

- **`consecutive_failures >= 3` (degrading)**: the underlying cause is
  almost always the local triage LLM being unreachable or producing
  consistently unparseable output — check the triage component's own health
  first (Brain tab / `docs/self-healing.md`'s triage entries), then restart
  the `continuum` runtime. The curator is stateless across restarts (its
  only persistent state is the vault itself, which is unaffected); a
  restart clears `consecutive_failures` back to 0 and picks up wherever the
  vault's events left off.
- **Curator shows "off" but should be running**: confirm
  `[memory.curator] enabled = true` in `config.toml` and that a triage model
  actually loaded at boot (the curator never spawns without one — see
  `docs/memory.md`'s curator section). Fix the triage model path/config and
  restart.
- **A pending wipe request never clears** (`~/.continuum-dev/wipe-request.json`
  still present after a restart or a full day): open the file and check it's
  valid JSON matching `{ requested_at, scopes: [...] }`. A malformed request
  file is **not** silently discarded — `process_wipe_request` returns an
  error, leaves the file in place, and logs the failure — so as of this
  writing a corrupt request file is retried (and re-logged) on every boot
  and every daily hygiene tick until a human fixes or deletes it by hand.
  Fixing the JSON (or deleting the file to abandon the request) resolves it
  on the next boot or tick, no restart-with-a-flag needed.
- Full pipeline, config keys, and prompt locations: `docs/memory.md`'s
  "The curator" section.

## Voice Wake And STT Session

Component: `voice/wake`, `voice/stt`

Logs:

- `layer = "voice"`, `component = "wake"` when the wake phrase is detected.
- `layer = "voice"`, `component = "stt"` when a voice command endpoint is detected.

Health check:

- `TranscriptWakeDetector::is_healthy()` returns true when the configured wake keyword is non-empty.
- `AudioWatcher::is_healthy()` must also be true for the voice input path to receive transcripts.

Recovery:

- If wake detection never fires, verify `[voice].wake_keyword` and the microphone path.
- Run `continuum --reset-audio` to re-pick the microphone if hardware changed.
- If Whisper is degraded, re-run `scripts/download-models.ps1` and check `audio.whisper_model_path`.

## TTS And Playback

Component: `voice/tts`, `voice/playback`, `voice/streaming`

Logs:

- `layer = "voice"`, `component = "tts"` for Piper routing and synthesis.
- `layer = "voice"`, `component = "playback"` for output device setup and stream errors.
- `layer = "voice"`, `component = "streaming"` for worker startup and interruption.

Health check:

- Piper voice files must exist under `[tts.voices.*]`.
- The `piper` binary must be available on `PATH`, or `CONTINUUM_PIPER_BIN` must point to it.
- `PlaybackStream::open_default()` must be able to open the default output device.

Recovery:

- Re-run `scripts/download-models.ps1` if Piper model/config files are missing.
- Install or repair the Piper CLI and set `CONTINUUM_PIPER_BIN` if it is not on `PATH`.
- Use `--no-tts` to keep Continuum running in log-only voice output mode while repairing audio output.

### Kokoros TTS

Component: `voice/tts` (engine = `kokoros`)

Logs: `layer = "voice"`, `component = "tts"`, engine `kokoros`.

Health check (`kokoros_health_from_paths`):

- `tts.kokoros.model_path` (`kokoro-v1.0.onnx`) and `tts.kokoros.voices_path`
  (`voices-v1.0.bin`) must exist.
- The `koko` binary must resolve (`CONTINUUM_KOKO_BIN` →
  `~/.continuum-dev/bin/kokoros/koko.exe` → `koko` on PATH).

Recovery:

- Re-run `scripts/download-models.ps1` to fetch the Kokoro model + voices.
- Build/install `koko` (no official Windows prebuilt — see download script)
  and set `CONTINUUM_KOKO_BIN`.
- Fall back to `tts.engine = "piper"` until Kokoros is repaired.

## Moshi S2S Front-end

Component: `moshi` (active only when `voice.frontend.mode = "moshi"`; requires
the `moshi` cargo feature).

Logs: `layer = "voice"`, `component = "moshi"`.

Health check (`moshi_health_from_paths`):

- Disabled when the active front-end is not `"moshi"`.
- The `moshi-backend` binary must resolve: `voice.frontend.moshi_bin` →
  `CONTINUUM_MOSHI_BIN` → `~/.continuum-dev/bin/moshi/moshi-backend.exe` →
  `moshi-backend` on PATH. A bare PATH fallback is reported healthy-but-
  unverified; an explicit missing path is `Unhealthy`.
- Live liveness is the runtime snapshot's `moshi_loaded` field (subprocess
  up + WebSocket handshake done), set by the event consumer in
  `bin/continuum.rs::build_moshi_voice`.

Recovery:

- If `moshi_loaded` flips to false mid-session, the `Disconnected` /
  `Error` event is logged; the repair agent may restart the component
  (re-spawn the subprocess + reconnect WebSocket). The standalone server
  also enforces a 360 s hard connection timeout, so long sessions must
  reconnect.
- Build `moshi-backend.exe` with CUDA (kyutai-labs/moshi `rust/`), or set
  `CONTINUUM_MOSHI_BIN` to an existing build. CUDA is required for realtime;
  CPU is documented-only.
- For audio I/O, enable the `moshi-opus` cargo feature and install libopus
  (vcpkg `opus`). Without it, the transport + text channel still work but
  no audio flows.
- Switch `voice.frontend.mode` back to `"pipeline"` to fall back to the
  existing whisper → triage → orchestrator → TTS loop while repairing Moshi.
  Mode changes take effect on the next daemon start.

## Chat Providers

Component: `chat_providers`

Logs:

- `layer = "desktop"`, `component = "providers"` when `providers.json` fails to parse (starts empty rather than crashing the dashboard).
- `layer = "desktop"`, `component = "chat"` / `component = "chat_store"` for send/stream/persist failures.

Health check:

- `ChatProvidersCheck` (`apps/desktop/src-tauri/src/components.rs`) reads `providers.json` directly — it runs from the health-poll loop, not a Tauri command, so it has no live app state to borrow from.
- Reports `Degrading` when any stored connection's `last_test_ok` is `false` (i.e. the last time it was tested, from Settings → Integrations or an automatic re-test, it failed). `Healthy` otherwise, including when there are no providers configured yet.

Recovery:

- Re-test the failing connection: Settings → Integrations → the provider row → **Test**. For a local provider (LM Studio/Ollama) this usually just needs the local server started.
- If the key was rotated or revoked upstream, remove the connection and re-add it with the new key — keys are never edited in place, only replaced (Remove deletes the Credential Manager entry; Add writes a fresh one).
- If the provider is fine but the probe still shows Degrading, restart the app — `chat_providers` is a pure file-read probe with no daemon of its own to restart, so a full app restart re-reads `providers.json` cleanly.
- This probe never blocks Chat itself: a Degrading status only means one *stored* connection's last test failed, not that sending messages is broken. Check the in-app error banner on the Chat tab for the actual `GatewayError::user_message()` text (see `docs/chat.md`'s error glossary) when a send fails.
