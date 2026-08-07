# Context Engine — Specification v2

Status: design-reviewed (5-lens adversarial panel, 41 serious findings adjudicated and incorporated) · 2026-08-05
Substrate: 7-reader codebase synthesis (`context-engine-map.md`); Continuum.md §4, §6, §7, §12, §13, §17, §18, §21–23, §32.

## 1. Goal

Context is the product's heart: Continuum must continuously know *what the user is doing* (app, window, project, goal, task, last error/success), remember what matters, and hand any AI — orchestrator wake, chat, or tool call — the same accurate, privacy-filtered, compact context. This spec turns the current partial pipeline (capture → salience → triage → distill) into the canonical Continuum.md pipeline:

```
capture → dedupe → privacy filter → classification → project/goal → session state
        → curator/memory → context package → Main AI
```

**Hard requirements from the maintainer:**
1. **No keyboard capture, permanently.** Typed text reaches Continuum only indirectly (screen, files, accessibility later), always after the privacy filter. Window-title text is treated as *content*, not metadata — it passes scrub + zones (titles routinely contain typed text: address bars, terminal command lines). A later UIA reader must filter Edit/password control value-change events (keylogging by the back door).
2. **Every context source is also a tool** (§5), additive to continuum-mcp (non-negotiable #7), each with a permission entry (§12 observation rights).
3. **No half work**: the NOW tier ships completely — collectors, privacy, session state, packager, tools, Context page, evals, health hooks, config knobs, docs.

## 2. Scope

**NOW (this spec, three plans, §10):** everything below. **NEXT (separate spec):** UI Automation reader (dedicated COM worker), local OCR, §17 process split + config hot-reload, clipboard *watcher* (default OFF), model gateway/router, full epistemic-label vocabulary UI, full audit-log UI. **LATER (external):** browser extension, VS Code/IDE plugin, terminal plugin.

**Fixed defects (part of NOW):** hardcoded `infer_project_hint` (retrieval.rs:261); `project_world_state` reading daemon CWD (context.rs:549); triage `Remember` discarded + `triage_decision` column never written; `ScreenObservation.description` overload; maintenance wakes get real history; `PrivacyDisposition::Excluded` dead code wired; **existing MCP observation tools bypassing privacy** (§5.1).

## 3. Architecture

One tokio runtime. Existing patterns: **watcher** (async task + config clone + shutdown watch + health hooks; spawn block continuum.rs:409-460), **hub** (LiveContextHub `record_*` + `LIVE_CONTEXT_SCHEMA_VERSION` bump), **never-fail-the-wake** (log + degrade), **additive schema** (`ensure_optional_column`). One **new pattern** (sanctioned here): shared `AtomicU64`/watch values for runtime-adjustable cadences (idle controller adjusts capture/vision intervals without restart; loops read the value each iteration).

```
ContextWatcher (1 Hz, enriched)──┐  scrub AT COLLECTOR EMIT (before hub fork!)
VisionWatcher (unchanged)────────┤
AudioWatcher (unchanged)─────────┼─► PerceptionFrameBuilder ─► frame loop (select!)
GitCollector (active project)────┤            │                    │ spawns triage
FileWatcher (opt-in roots)───────┘            │                    │ per gated frame
        │ (scrubbed payloads)                 ▼                    ▼
   LiveContextHub ──► live-context.json   RawLog.write_frame   TriageOutput
        │            (content-versioned)      │                    │
        ▼                                     ▼                    ▼
 SessionStateHub ◄──────────── events-writer task ◄── mpsc<ContextEvent> (all collectors)
        │                       (dedupe + batch → context_events + FTS)
        ▼
 ContextPackage (UNGATED pure module) ─ 3 assembler profiles:
   runtime-full (wake) │ mcp-published (context_package) │ desktop-published (chat)
```

**Event transport (explicit):** one `mpsc::Sender<ContextEvent>` cloned into every collector; a dedicated **events-writer task** (not the frame loop, not per-collector writes) owns the receiver, applies dedupe, and batch-inserts. Bounded queue, drop-oldest with an overflow-coalesce row ("N events dropped from <source>"). Collectors never block on SQLite; the frame loop never touches the events DB inline. Switch events additionally ride the frame builder's accumulation path *for salience only*, not persistence.

**Single source of truth for "current project":** the resolver (§4.3) is owned by the frame loop, resolves once per frame post-hysteresis; its single output feeds SessionStateHub, ActivitySignal (now explicitly derived), ProjectWorldState, and every event's `project_id`.

New Rust modules: `senses/privacy.rs`, `senses/git_watch.rs`, `senses/file_watch.rs`, `context/{project,session_state,package,continuation}.rs`, `memory/events.rs`. `context/package.rs` and the `ContextPackage`/section types are **NOT gated behind the `runtime` feature** (pure types + string rendering; `regex` moves to continuum-core's unconditional deps for privacy.rs) so all three processes link them; the desktop `--no-default-features` parity gate proves it.

## 4. Components

### 4.1 Privacy filter (`senses/privacy.rs`) — lands FIRST

The choke point every byte passes **at collector emit** — before the hub, before persistence, before any model (local included), before any tool response.

**Scrubbers** (`PrivacyFilter::scrub_text`), each togglable in `[privacy]`: API keys/bearer tokens (`sk-…`, `ghp_…`, `AKIA…`, `Bearer …`), generic high-entropy hex/base64 ≥ 32 chars, credit cards (Luhn), IBAN, emails (default OFF). Replacement `[REDACTED]`. **Scope rule:** entropy/secret scrubbers apply to *free-text fields only* (titles, captions, transcripts, summaries, commit subjects); *structured fields from trusted collectors* (git commit id, branch, frame ids, dedupe keys) are exempt by construction — a 40-char git OID must survive end-to-end (false-positive corpus in tests: commit hashes, UUIDs, sha256 digests). **Path scrubber** (separate, always on for cloud-bound + persisted paths): home-dir prefix → `~`, username token redacted anywhere in paths; structure otherwise preserved. Applies to exe_path, file-event paths, git paths. Scrub is idempotent.

**Zones** per rule `{match_process?, match_title_keyword?, zone}` in `[privacy].zones`:
- `never_observe` → the collector emits a **sentinel observation** (`process="[excluded]"`, empty title, `PrivacyDisposition::Excluded` — finally produced). Sentinel semantics are defined, not implied: no screenshot file is written, no caption, no events row; dwell accumulation resets; switch events with an excluded endpoint are suppressed and replaced by a single synthetic switch from/to the literal `[excluded]` bucket; session state shows "[private]"; salience treats sentinel↔real transitions as a process change. This prevents the stale-frame bug (frame.context is non-optional; without a sentinel, latest-wins would persist the *previous* window as current).
- `local_only` → observed, scrubbed, persisted, local models OK; **stripped from everything cloud-bound**.
- `cloud_allowed` (default).

**Legacy migration (load-time synthesis, old keys keep working):** `sensitive_process_names` → `never_observe`; `sensitive_title_keywords` → `local_only` (preserves today's "observed but redacted" spirit). If both old keys and `[privacy].zones` exist, the union applies, stricter zone wins. New default title keywords: `InPrivate`, `Incognito`, `Private Browsing` → `local_only`.

**Zone-derivation propagation rule (closes the laundering hole):** every *derived* artifact inherits the strictest zone of its inputs, carried as a sensitivity tag the cloud gate enforces: (1) frame/collector zone → `context_events.sensitivity` is mandatory at insert; (2) session-state inference over a window containing `local_only` events tags its output fields `local_only` (cloud renders "working in a private context"); (3) a wake triggered by a `local_only` frame gets the generic reason "user activity in a private context"; (4) curator session notes and correction-derived preference notes covering `local_only` spans get vault `sensitivity: Sensitive`.

**Foreground-only limitation, mitigated:** zone matching sources from the foreground poll AND a per-monitor visible-window sweep (`EnumWindows` + `GetWindowRect` joined to monitor geometry, same Win32 block §4.2 extends): a monitor showing any `never_observe`/`local_only` top-level window inherits that zone for capture/caption purposes. `excluded_monitor_ids` remains the hard fallback; the limitation is stated on the Context page sources panel.

**Cloud egress points enumerated** (the gate applies at ALL of them): wake message render, chat prompt injection, every MCP tool response (a cloud model may be the caller), correction-intent contents echoed into notes. Chat providers count as cloud unless the connection sets a new per-connection `local_endpoint=true` flag.

**Honest toggles (Continuum.md §13):** per-source observation switches — `mic`, `screen`, `files`, `git`, `pause_all` — in config + RuntimeSnapshot, rendered on the Context page, enforced *at each collector* (a disabled source emits nothing; pause_all gates the frame loop). Every toggle change writes a `system` event. No dishonest mute: toggles are independent.

### 4.2 Window/process enrichment

`ContextObservation` gains `pid`, `exe_path` (path-scrubbed), `active_since_secs`, `monitor_id` (via `MonitorFromWindow`, not manual rect intersection — DPI-safe; add the Win32_Graphics_Gdi feature). Raw-log columns additive. ContextWatcher diffs consecutive polls → **switch events** `{from_app, to_app, from_title, to_title, dwell_secs}` (source `window`, type `focus_switch`) into the events channel + a new salience signal via the frame builder's accumulation path. Switch events are **NOT serialized into the triage frame JSON** (token budget §4.7). Background-process enumeration stays out; the §4.1 visible-window sweep is per-monitor zone detection only.

### 4.3 Project resolver (`context/project.rs`)

Config `[[projects]]`: `{ id, name, root_paths, repo?, keywords, zone? }`. **Project id format:** lowercase slug `[a-z0-9-]{1,64}`, no dots (feeds semantic-fact prefixes, dedupe keys, vault relations). Resolution per frame, strength order:
0. **User override rules** (persisted; from Context-page corrections): `{match_process, match_title_substring, action: force_project|exclude_project}`.
1. A path visible in the window title matching a configured/confirmed root_path.
2. Editor title patterns ("file — folder — app").
3. Git root of the most recent file event.
4. Keyword match (legacy behavior, config-driven).

Result `{project_id, confidence}` with hysteresis (`switch_min_secs=20`). Replaces `infer_project_hint` (all four consumers) and fixes `project_world_state` (never `current_dir()`).

**Auto-discovery (non-circular):** the discovery source is **title-derived paths** — a resolvable absolute path or editor-pattern folder observed ≥ `discover_min_secs=30`, validated by dir-exists + `find_git_root`, proposes a candidate (id = slugified folder name + `-2` collision suffix). Candidates appear on the Context page; **unconfirmed candidates are never collected from** — no git polling, no file watching, no per-project stats; only the proposal row exists. Confirming (or "Add project" manually, §4.13) writes the **Projects table** immediately (runtime picks it up live) and appends `[[projects]]` to config.toml for next boot; at boot, config is seed data reconciled into the table, config wins on id conflict. The Projects table (raw-log DB) holds: config mirror + discovered candidates + override rules + pins + per-project stats + `zone`.

**Project zones:** the `zone` field on a project applies to its git/file events and title matches (closes "zones can't express file/git sources"); the resolver, collectors, events inserts, and cloud gate enforce it.

### 4.4 Git collector (`senses/git_watch.rs`)

Watches the **active, confirmed** project's repo (resolver output; never daemon CWD; nothing for unconfirmed candidates). Reports branch, dirty/staged counts, ahead/behind, last commit (id+subject — subject is free text → scrubbed; id exempt), conflicts, untracked count. **One consolidated invocation** per poll: `git status --porcelain=v2 --branch` + `git log -1 --format=…`, `CREATE_NO_WINDOW`, timeout `[git_context] command_timeout_secs=10`. Cadence `poll_secs=30`; the `.git` mtime gate applies **only to ref-derived facts** (branch/head/ahead-behind/last-commit); working-tree dirtiness always polls (an edited file changes neither HEAD nor index). Project-switch probe fires post-hysteresis, min-spawn-interval knob. Sink: extended `ProjectWorldState` (hub, schema bump) + events **on changes only** (`commit`, `branch_switch`, `conflict`, `dirty_change`).

**Failure modes:** `git --version` probed once at startup — absent → disabled-with-reason (healthy=true, enabled=false, surfaced in health + Context page; `should_restart()=false`; re-probed on config change). `.git`-as-file (worktrees) → resolve the gitdir pointer. Zero-commit/detached repos → `branch: null`, no error.

### 4.5 File watcher (`senses/file_watch.rs`) — opt-in, default OFF

`notify`-based on confirmed roots only. Debounce `debounce_ms=1000`, ignore-globs (node_modules, target, .git internals except HEAD/index, build dirs; config). Events `{path (scrubbed), kind, project_id, ts}`, types `file_modified|file_created|file_deleted|file_renamed` (From/To rename pairing specified). **Storm rule:** > K events for one project within the collapse window → single coalesced "N files changed in <project>" event (branch switches touch thousands of *tracked* files; debounce alone is per-path). **Failure modes:** per-root watch state — an erroring/vanished root is marked unavailable (event + Context page badge), retried on `rearm_secs=60` backoff, other roots unaffected; `should_restart()` only when notify's channel itself is dead. On overflow/rescan → structured health event + ignore-aware directory resync.

### 4.6 Events table + dedupe (`memory/events.rs`)

Table `context_events` (raw-log DB, additive): `id, ts_first, ts_last, count, source, application, window_title, project_id, event_type, summary, importance, confidence, sensitivity, raw_reference, dedupe_key`. Index on `(dedupe_key, ts_last)` + in-memory LRU of open collapse keys. **FTS:** `context_events_fts` (FTS5, external-content over `summary, window_title`, triggers on insert AND dedupe-UPDATE restricted to text-bearing changes) — created by the runtime in Plan A; Plan C only reads. **All DDL for this DB lives in raw_log.rs, single-writer (runtime).**

**event_type registry (closed, additive-only, same stability policy as MCP schemas):**

| source | event_type values |
|---|---|
| window | `focus_switch`, `project_switch` |
| git | `commit`, `branch_switch`, `conflict`, `dirty_change` |
| file | `file_modified`, `file_created`, `file_deleted`, `file_renamed`, `files_bulk_change` |
| screen, audio | the §4.7 classification enum (`error`, `success`, `decision`, `preference`, `task_progress`, `communication`, `routine`, `other`) |
| system | `idle_start`, `idle_end`, `wake`, `wake_result`, `voice_command`, `toggle_change`, `source_unavailable`, `events_dropped` |

`sensitivity` ∈ {`local_only`, `cloud_allowed`} (never_observe rows cannot exist). A classifier-emitted project not present in the Projects table is dropped to the resolver's value (log-only).

**Dedupe (defined, not hand-waved):**
- Template sources (window/git/file/system): `dedupe_key = hash(source, event_type, project_id, normalized_summary)`; normalization = lowercase, strip digits/hex/paths/quoted strings, collapse whitespace, first 12 tokens.
- **Classified screen/audio events: summary is NOT in the key** — key = `hash(source, event_type, project_id, application)`. LLM summaries are never byte-stable; the flagship "build failed ×14" case must collapse regardless of summary variance (first summary kept as display text, count bumped).
- Window anchor: `ts_last` (ongoing repetition keeps collapsing). Caps: `count ≤ 500`, span ≤ 24 h — then a fresh row.
- Retention `retention_days=30`, rotated with the raw log.

### 4.7 Classification riding triage

The triage call becomes the Context Model call — no second GPU pass. **Parse container (serde-proven):** `TriageOutput { #[serde(flatten)] decision: TriageDecision, classification: Option<Classification> }` — the existing internally-tagged enum ignores unknown keys (`test_parse_extra_keys_accepted`), so the flatten wrapper is required or the block silently dies. `TriageLayer::evaluate` returns `TriageOutput`; touched sites enumerated: continuum.rs decision match, triage handlers, continuum-triage-bench. The output is ONE top-level JSON object (classification is a nested key inside it — brace-depth early-stop safe); compact keys; a truncated/malformed classification block must still yield the decision (never burn retries on the sub-object).

```json
{ "decision": "ignore", "classification": { "event_type": "error", "project": "continuum", "importance": 0.8, "confidence": 0.9, "summary": "one line", "should_store": false } }
```

**Token budget (measured, not asserted):** triage `context_size` default 2048 → **4096** (prompt is ~2,030 tokens worst-case today — already at the edge). System-prompt byte cap asserted in test; `memory_summary` (now fed by session state) char-capped at 600; switch events excluded from the frame JSON; description slimming per §4.10. Output grows ~60-90 decode tokens (~0.5-1 s on Qwen 3 8B): the bench (§9) is **re-baselined with classification enabled**, asserts `prompt_tokens < n_ctx − max_tokens`, and keeps p95 < 1500 ms as the target.

**Consumption:** (a) every triaged frame's classification → events channel (source screen/audio), zone-tagged; (b) `should_store` OR `Remember` → **vault memory candidate** (status Candidate, source Observed, project from resolver, **expiry** per-type TTL from config, epistemic label from source: `user_stated` (audio/voice) | `system_inferred` (screen)); mapping table event_type → vault type: `error→error, decision→decision, preference→preference, task_progress→task, success→note(tag result), communication/other→note, routine/project_switch→no candidate` (11-type audit: `attempt/result/file` vault types deferred to NEXT, stated); (c) `triage_decision` raw-log column populated; (d) SQL distiller predicate stays as fallback when classification is absent.

**Triage off the main loop (fixes the freeze):** triage evaluation is `tokio::spawn`ed per gated frame with a coalesce/busy flag (mirroring the do_wake pattern) — the 250 ms voice ticker and hotkey arm never wait on the LLM lock. **LocalLlm discipline:** a two-priority acquisition (interactive triage first; background callers — curator, session inference — use try-acquire/backoff and cap `max_tokens ≤ 256` per chunked call). Honest claim: "no additional per-frame GPU pass, plus a bounded background budget (session inference ≤ 1 call/2 min, ≤256 tokens)". Bench asserts voice-ticker delay p99 < 500 ms under concurrent curator+triage load.

### 4.8 Session-state tracker (`context/session_state.rs`)

`SessionStateHub` (Arc<RwLock>, hub pattern):

```json
{ "active_project": "continuum", "current_goal": "…", "current_task": "…", "active_app": "Code.exe", "window_title": "…", "open_files": ["…"], "last_error": "…", "last_success": "…", "last_user_command": "…", "confidence": 0.0, "since": "…", "updated": "…" }
```

- Mechanical fields update synchronously in the frame loop (project/app/title; open_files best-effort from titles + file events; last_error/last_success from classified events; `last_user_command` from voice/chat/hotkey intents + ts).
- **Goal/task inference: event-driven, own spawned task** (never awaited in the frame loop): triggered by project switch, ≥`infer_min_new_events=8` significant events (importance ≥ `significant_importance=0.5`), or staleness > `infer_max_age_minutes=10`; min interval `infer_min_interval_secs=120`; `max_tokens=256`; lenient JSON + clamps; below `confidence_floor=0.4` fields render "unknown". Zone rule: windows containing local_only events tag outputs local_only (§4.1).
- **Boot rehydration:** on start, seed from the persisted state.json snapshot + most recent context_events (lowered confidence, staleness-discounted) — the continuation resolver must survive restarts.
- `SessionTracker` (curator) gains a public snapshot accessor + a project-change boundary (post-hysteresis).
- Consumers: Plan B wires triage `memory_summary`, skills `MatchContext`, packager; Plan C wires publishing (state.json + live-context.json), Context page, MCP tools.

### 4.9 Context package — one struct, three assembler profiles

`ContextPackage` struct + pure renderer live UNGATED (§3). Sections: current moment (caption + window title + audio), session state, just before (deduped events, count-aware), relevant memories/facts/vault notes + pending decisions (order contract preserved: pending last-before-reason), recent changes (file/git), failed attempts (`error` events with counts), last success, open files, available tools + permission mode, recommended next step (resolver, only when confident), why woken. Budget `token_budget` default **1000** (wake) / 600 (chat preset); per-section caps in config; drop order under pressure: open files → recent changes → just-before tail → memories tail (stated in config docs). Every section render is cloud-gated.

**Per-consumer matrix (explicit — the three processes differ):**

| Consumer | Process | Sections | Sources |
|---|---|---|---|
| Wake (`build_wake_message`) | runtime | ALL | in-process hubs, recent_frames ring (becomes `Arc<std::sync::Mutex<VecDeque<PerceptionFrame>>>`, snapshot-clone under short guard, never held across await; maintenance ticker stops passing `&[]`), episodic/semantic retrieval |
| `context_package` MCP tool | continuum-mcp | session/events/git/projects/screen from published files + read-only SQLite; memories via its OWN lazy store opens (existing precedent server.rs:59-61) with live-context compact text as the query; **omitted:** why-woken, trigger-frame moment (replaced by live-context current moment); per-section `stale` flags | state.json, live-context.json, context_events (read-only), vault, its own episodic store |
| Chat system prompt | desktop (Tauri) | **keeps today's in-process vault search as the memory section (no regression when runtime is off)**; ADDS session-state section read from state.json (absent/stale → "runtime not running"); episodic section explicitly unavailable | vault in-process, state.json, context_events read-only |

**Post-wake structured record (feeds continuation):** after each wake, parse a best-effort trailer `{action, result, next_step}` from the orchestrator result (absent if unparseable) → stored as a `wake_result` system event + fields on the wake vault event. The curator session-summary contract gains a structured `open_task:` trailer line the resolver reads without an LLM.

### 4.10 Frame/description disentanglement

`ScreenObservation.description` returns to the one-sentence vision caption; the compact world-state blob moves to `ScreenObservation.world_compact: Option<String>` (additive everywhere). Triage prompt uses caption + slim context; packager uses world_compact.

### 4.11 Compression ladder + idle

Distiller reads from `context_events` (deduped, count-aware: "build failed ×14"), writes **project** onto every EpisodicEvent (additive Lance migration; retrieval gains an optional project filter). Screenshot policy: rotation deletes referenced files with rows PLUS an mtime backstop sweep of `screenshots_dir` older than `retention_days`; never_observe captures write no files at all.

**Idle (mechanical, not circular):** when `idle_seconds > idle_pause_after_secs=300`: capture cadence → `idle_capture_interval_ms=2000` (via the shared AtomicU64 pattern, §3), vision consumer switches to `idle_vision_interval_ms=15000` (0 = fully paused) so **unattended-error detection stays alive** (a build failing while you're at lunch still produces events/wakes — documented trade-off + knob), triage runs only for frames with visible error or audio (the existing mechanical skip-gate inputs, not the classification's own output), session inference pauses. **Restore triggers:** input activity OR voice wake OR hotkey OR any do_wake entry; a wake during pause forces one immediate capture+vision pass (or stamps responses `stale: true`). Publisher: live-context.json writes are keyed on a **content-version counter** (bumped only by meaningful-change captures, vision/privacy updates, window/project changes — NOT by unchanged-capture bookkeeping or the 1 Hz no-change poll; no "unchanged" ring events during idle).

### 4.12 Continuation resolver v1 (`context/continuation.rs`)

Trigger set: config `[continuation] trigger_phrases=["ga door","continue",…]` + empty-ask hotkey/chat. Candidates (all with real producers): current session-state task (survives restart via §4.8 rehydration), `open_task` from the last session summary (structured trailer, §4.9), last `error` event, last `wake_result.next_step` (§4.9), `last_user_command`. Ranked recency × confidence; ≥ `confidence_floor=0.6` → packager renders "recommended next step"; below → the wake reason instructs a one-line disambiguation question. Pure logic + unit tests; no LLM call.

### 4.13 Context page + correction loop

Dashboard tab: active project (+ source of the belief), goal, task, activity, blocker/last error, confidence bars, per-source health + toggles (§4.1), recent events strip, discovered-project candidates. **Empty state** (fresh install): discovery candidates + "Add project" CTA + explanation of what will/won't be observed. Actions (intent files `~/.continuum-dev/context-intents/`, drained in the main loop):
- **Add/Confirm project** → Projects table now + config append for next boot (§4.3).
- **Correct** (project/goal) → session state + persisted resolver override rule + Confirmed preference note (zone-inherited).
- **Not this project** → persisted `exclude_project` override rule (tier 0).
- **Pin** → persisted in `session_pins`; blocks *session-state overwrite only* (not resolution); cleared when the resolver reports a different project above confidence C for `switch_min_secs` — no pin/resolver deadlock.
- **Forget** → **cascade** keyed on `raw_reference`: context_events row + FTS entry, referenced perception_frames row + screenshot file, episodic events derived from that frame, unconfirmed vault candidate.
- **Delete range** (start/end ts) → purges frames, events, episodic, screenshots in the window (Continuum.md §13).
All overrides/pins survive restart and are listed + deletable on the page. **Minimal audit log (NOW):** append-only JSONL `~/.continuum/logs/actions.jsonl` of wake/tool-call/toggle/correction/delete actions; full UI deferred to NEXT.

## 5 MCP context tools

### 5.1 Existing tools brought inside the privacy boundary (Critical fix)

`system_active_window`, `system_clipboard_get`, and `system_live_context` (BOTH the `compact` and full `state` fields) route through `PrivacyFilter` scrub + cloud gate. Content filtering changes no tool name or schema — non-negotiable #7 is not violated (stated explicitly). `system_clipboard_get` additionally gets scrub_text on returned text + a `[context_tools]` kill-switch (consistent with the deferred clipboard watcher). "Schema-stable" replaces "untouched" in all docs.

### 5.2 New tool family (additive; each: schema struct, permission entry, docs, protocol-test entry)

All read-only; all privacy-gated; all degrade gracefully (`stale: true` / empty, never a wake-killing error). SQLite access from the MCP process: **read-only open path** (no `RawLog::new` DDL — a dedicated read-only constructor; `PRAGMA query_only=ON`, busy_timeout 2000 ms, WAL-recovery tolerant); missing DB → `{stale: true, events: []}`.

| Tool | Returns | Source |
|---|---|---|
| `context_session` | session-state JSON + staleness | state.json |
| `context_window` | active process/window/monitor, focus duration, last N switches | live-context.json + events |
| `context_screen` | per-monitor caption + world_compact + staleness | live-context.json |
| `context_audio` | latest privacy-filtered transcript + ts + in_call/mute state | live-context.json |
| `context_timeline` | `{since?, until?, types?, project?, source?, limit≤200}` deduped events with counts | context_events RO |
| `context_search` | FTS over summaries + titles `{query, limit≤50}` | context_events_fts RO |
| `context_files` | recent file events `{project?, limit}` | context_events RO |
| `context_git` | active project git state; `{project?}` for a **confirmed** named project runs an on-demand timeout-bounded probe against its root (scrubbed) | live-context.json / direct probe |
| `context_projects` | configured + discovered projects, active first | Projects table RO |
| `context_package` | assembled package per the §4.9 mcp-published profile, `{token_budget?}` | published state + own stores |

Session-summary search is served by existing `memory_vault_search` (`types:["session"]`) — stated in docs so the tool surface is visibly complete. The chat CLI provider's allowed-tools list gains the context family; HTTP chat providers get packager context passively (active tools = later deliberate decision).

## 6 Config surface (every knob, with defaults)

`[privacy]` scrub_api_keys/cards/iban=true, scrub_emails=false, zones=[], toggles {mic,screen,files,git,pause_all}; `[projects]` auto_discover=true, switch_min_secs=20, discover_min_secs=30; `[[projects]]` id/name/root_paths/repo/keywords/zone; `[git_context]` enabled=true (confirmed projects only), poll_secs=30, command_timeout_secs=10, min_spawn_interval_secs=5; `[file_watcher]` enabled=false, debounce_ms=1000, ignore_globs, rearm_secs=60, storm_threshold=50; `[events]` collapse_window_minutes=10, retention_days=30, count_cap=500, span_cap_hours=24, queue_cap=1024; `[session_state]` infer_min_interval_secs=120, infer_max_age_minutes=10, infer_min_new_events=8, significant_importance=0.5, confidence_floor=0.4, infer_max_tokens=256; `[context_package]` token_budget=1000, chat_token_budget=600, per-section caps; `[continuation]` confidence_floor=0.6, trigger_phrases; `[performance]` idle_pause_after_secs=300, idle_capture_interval_ms=2000, idle_vision_interval_ms=15000; `[context_tools]` enabled=true, clipboard_tool_enabled=true, per-tool caps; `[storage]` delete_screenshots_with_rotation=true, screenshot_max_age_hours=720; `[memory]` candidate TTLs per type (task/error 30 d, decision/preference none); triage `context_size` default 4096.

## 7 Self-healing

Every new watcher: real `is_healthy()`/`should_restart()` (per the failure-mode paragraphs — degraded-permanent states report healthy+disabled, never restart-thrash), structured logs (layer="senses"/"context"), health snapshot registration, recovery procedure in docs/self-healing.md. Existing dead hooks (LiveContextHub, ContextWatcher) wired into the health loop in Plan A.

## 8 Performance budgets

No additional per-frame GPU pass (classification rides triage); bounded background budget (session inference ≤ 1/2 min ≤ 256 tokens; curator chunked ≤ 256/call); triage spawned off the main loop — voice ticker p99 delay < 500 ms under load (bench-asserted); triage context 4096, p95 < 1500 ms re-baselined with classification; events writes via dedicated writer task, rate-capped, storm-coalesced, indexed dedupe lookups; publisher content-versioned; idle ≈ near-zero (reduced capture, 15 s vision, no triage without error/audio, no session inference, no publisher churn).

## 9 Testing contract

- Unit: scrubbers (secret corpus AND false-positive corpus: git OIDs/UUIDs/sha256 survive), zone routing incl. sentinel semantics + excluded-boundary switch events, propagation rule, dedupe (template + classified variants, anchor, caps), project resolution matrix + hysteresis + override tiers + discovery, session-state mechanical updates + rehydration, continuation ranking incl. post-restart, packager section caps/order/drop-order per profile, TriageOutput parsing (flatten wrapper, truncated classification still yields decision, clamps).
- Integration: core full + `--no-default-features` (packager must link ungated) green; events migrations idempotent; events-writer overflow-coalesce; frame loop with all collectors under fake clock.
- Benches: **`continuum-record`** flag (perception bin) serializes post-privacy frames + collector events to JSONL; fixture = synthetic hand-authored 20-min JSONL in `crates/continuum-core/benches/data/` (2 projects, build-failure loop, success, idle gap) + labels sidecar `{ts, expected:{project, goal?, task?, blocker?}}`; real recordings local-only, never committed. Four harnesses: context-recall (project ≥ 0.9, goal/task ≥ 0.6, blocker/last-action ≥ 0.8), safety-redaction (zero secrets end-to-end incl. every MCP tool response; commit ids survive), dedupe-precision (≥ 90% collapse on the defined algorithm, no distinct-event loss), **memory-precision** (post-distill+curation: duplicate rate ≤ 10% by embedding similarity, precision vs labels ≥ 70%, later-used reporting via last_used). Triage bench: prompt-fit assert + re-baselined p95 + voice-ticker-delay p99.
- Acceptance: Context page live within 5 s of a project switch (after ≥1 confirmed project); "ga door" after restart resolves the rehydrated last task or asks.

## 10 Plan split

- **Plan A — Waarneming & fundament:** privacy filter + zones + sentinel + toggles enforcement FIRST; window/process enrichment + switch events; project resolver + Projects table + discovery + defect fixes; git collector; file watcher; events-writer task + context_events + FTS + dedupe; idle controls + publisher content-versioning; health hooks. (No LLM changes.)
- **Plan B — Begrip & pakket:** TriageOutput classification + Remember routing + triage_decision column + context_size 4096 + triage off-loop + LocalLlm priority discipline; description disentanglement; SessionStateHub + inference + rehydration; curator boundaries + structured session trailer; distiller merge + episodic project field; post-wake structured record; packager (ungated) + wake profile + chat profile + shared ring; continuation resolver.
- **Plan C — Ontsluiting:** session-state publishing (state.json + live-context.json, 4-touch contract); §5.1 privacy retrofit of existing tools; the 10 new MCP tools + read-only DB path + permissions + docs; Context page + correction/toggle/forget/delete-range intents + audit JSONL; `continuum-record` + fixture + 4 benches + triage re-baseline; self-healing docs; CHANGELOG/ARCHITECTURE (+ documented-drift fixes).

A → B → C. Each independently shippable; interfaces at the seams are named above (events channel, Projects table, TriageOutput, SessionStateHub snapshot, package profiles, published files).

## 11 Explicitly out / guarded

Keyboard capture: never, at any tier; UIA (NEXT) filters text-field value events. Background-process enumeration, browser/IDE/terminal content, autonomy levels, §17 process split, config hot-reload beyond the sanctioned AtomicU64 cadence pattern: NEXT/LATER. No second tokio runtime. MCP additive-only; `LIVE_CONTEXT_SCHEMA_VERSION` bumps on live-context shape additions.
