# Changelog

All notable changes to Continuum are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added

- **Confirmed GitHub mutations**: `github_create_issue`,
  `github_comment_issue`, and `github_create_pull_request` are bounded to one
  validated repository and require a fresh confirmation on every call. Bodies
  are redacted from local audit metadata, and pull-request creation never
  pushes code.
- **Enforced action permission gateway**: MCP calls now pass through the
  bundled and user-overridden `auto` / `session-approved` /
  `always-confirm` / `blocked` policy before execution. Approval requests,
  scoped grants, denial, revocation, expiry, and one-use consumption are
  durable across the desktop and MCP processes and audited to
  `logs/actions.jsonl`. The Tools tab now reads and saves the real policy and
  lets users approve pending calls or revoke active grants.
- **Recoverable Git tools**: `git_checkpoint`, `git_diff`,
  `git_checkpoint_list`, and `git_rollback` operate only in allowlisted
  repositories. Checkpoints use an isolated index and dedicated refs, exclude
  hard-denied secret paths, and never disturb the user's index. Rollback
  requires per-call confirmation, creates a safety checkpoint, and preserves
  loose/sensitive files under `.git/continuum-recovery/` before resetting.
- **Safe filesystem actions**: `fs_create_file`, `fs_apply_patch`, `fs_move`,
  and `fs_delete_to_trash` enforce the existing canonical allowlist and secret
  deny rules. Creates refuse overwrite, patches require exact current text and
  preserve the original, moves refuse overwrite, and delete is a recoverable
  move under `<data_dir>/recovery/files/`. Write size and replacement caps are
  configurable under `[mcp.fs]`.
- **Restricted terminal and verifier broker**: `terminal_run` and
  `terminal_verify` launch only configured executable basenames with literal
  argument vectors—never shell strings—in allowlisted working directories.
  Stdin is closed, sensitive environment variables and credential-like
  arguments are excluded, and runtime/output limits are configurable.
  Verifier results are atomically persisted under `evidence/terminal/`.
- **Optional GitHub connection and read tools**: Settings now exposes an
  explicit Connect/Disconnect card backed by the official `gh` browser/device
  flow. Continuum never reads tokens, strips token environment overrides, and
  accepts only OS-keyring-backed CLI auth. New session-approved MCP reads cover
  the current user, repositories, repository metadata, issues/PRs, and bounded
  UTF-8 file/directory content. GitHub remains idle until the user opts in.

- **Opt-in background-process context**: a change-driven process collector now
  records configured developer/model process starts and stops plus sustained
  CPU or memory pressure, publishes a bounded privacy-classified
  `processes.json` snapshot, exposes runtime health and the additive read-only
  `context_processes` MCP tool, and feeds lifecycle history into
  `context_timeline` under the new `process` source. Command lines,
  environment variables, process memory and hidden-window contents are never
  collected; `[process_watcher].enabled` defaults to false.

- **macOS desktop distribution**: the Tauri desktop build now ships a native
  Keychain-backed provider-secret store, runtime/MCP binaries and their
  config/prompt/skill resources inside the app bundle, and macOS CI coverage.
  Every successful `main` CI run now publishes signed updater artifacts plus
  DMG and portable archives for Apple Silicon and Intel Macs alongside Windows.

- **Moshi full-duplex S2S voice front-end** (cargo feature `moshi`): an
  alternative realtime voice path that runs Kyutai
  [Moshi](https://github.com/kyutai-labs/moshi) as a `moshi-backend.exe`
  subprocess driven over its standalone WebSocket protocol — the local
  counterpart to ChatGPT's Advanced Voice Mode. Selected by
  `voice.frontend.mode = "moshi"` (default `"pipeline"`). The existing
  wake → whisper → triage → orchestrator → TTS loop is unchanged and
  remains the default. New `VoiceFrontend` trait (`pipeline` vs `moshi`
  implementations), `voice/moshi.rs` (WSS transport + binary message
  framing + assistant-text channel + control messages, verified against
  the kyutai-labs source), `[voice.frontend]` config section, an audio
  tap in `senses/audio/full.rs` that forks 16 kHz mono PCM to the S2S
  subprocess, `voice_frontend_mode` / `moshi_loaded` runtime-snapshot
  fields, an `update_voice_frontend_mode` dashboard command + Voice-tab
  selector, and a `moshi` health probe. The Opus/OGG audio codec is gated
  behind a separate `moshi-opus` feature (needs libopus) and left as a
  documented stub; the base `moshi` feature compiles without it. Runtime
  also requires a CUDA-built `moshi-backend.exe` (placed by
  `scripts/download-models.ps1`). Tier-split escalation to the orchestrator
  (triage on the parallel whisper transcript) is wired: on a
  `WakeOrchestrator` decision the Moshi front-end is `interrupt()`ed (output
  muted + EndTurn) while the orchestrator + Kokoros speak, then `resume()`d
  when the wake completes; in Moshi mode the wake-word / pipeline
  voice-session machinery is skipped so Moshi owns turn-taking and triage
  owns escalation. The Opus/OGG audio codec (assistant audio playback + mic
  encode) is implemented behind the separate `moshi-opus` cargo feature —
  `OpusOggCodec` translates the reference client's send/receive audio arms
  (`rust/moshi-cli/src/multistream.rs`) and the backend OpusHead/OpusTags
  layout (`rust/moshi-backend/src/audio.rs`); the send side is an
  `ogg::PacketWriter` + `opus::Encoder` (24 kHz mono, Voip, 960-sample
  frames), the receive side is a seek-free `PageParser` + `BasePacketReader`
  OGG demux + `opus::Decoder`. `moshi-opus` requires libopus at build time
  (`vcpkg install opus` or the `opus` crate's system-lib path).
- **Kokoros local TTS engine**: `tts.engine = "kokoros"` selects
  [Kokoro-82M](https://github.com/lucasjinreal/Kokoros) as the TTS backend,
  a higher-quality, more natural-sounding local alternative to Piper. Like
  Piper it runs as a subprocess (`koko`) so the ONNX Runtime dependency
  stays out of Continuum's Rust dependency graph. Per-utterance synthesis
  feeds one line to `koko stream` and parses the 32-bit float 24 kHz WAV
  output. New `[tts.kokoros]` config section (`model_path`, `voices_path`,
  `voice_name`, `speed`), `update_kokoros_voice` / `update_kokoros_speed`
  dashboard commands, a Kokoros option in the Voice-tab engine selector
  with a voice/speed control, a `kokoros` component health probe, and
  Kokoros model/voice downloads in `scripts/download-models.ps1`. The
  `koko` binary has no official Windows prebuilt and is documented as a
  manual build step; Piper remains the default.
- **Chat history window**: the chat tab now ships only the recent tail of a
  conversation to the model each turn instead of the entire history, so
  per-turn input tokens stay bounded as a chat grows rather than growing
  linearly with its full transcript. The full conversation is still
  persisted on disk. New `[chat].history_message_window` knob (default 20
  most-recent messages; 0 sends the whole history, the previous behavior).
  After slicing, leading non-user turns are dropped so the first message the
  provider sees is a user turn (Anthropic requires this), which also keeps
  the kept context coherent.
- **Chat streaming feedback**: the assistant "thinking" indicator now appears
  the instant a message is sent, not after the backend acknowledges the
  stream. Three breathing dots show in the assistant bubble while the model
  is working but has not yet emitted its first token (or tool call); the
  streaming text cursor takes over the moment content arrives. The user
  sees the AI is busy right away instead of a bare "Sending…" footer while
  the adapter spins up.

### Fixed

Completion defects found in the final context-engine review (fix wave 3b):

- `context_package.sections_present` now describes the headings that actually
  survived privacy filtering, section caps and the token-budget drop ladder,
  instead of the pre-render package assembled by the MCP handler.
- Session goal/task pins are restored on boot, survive ordinary app and project
  updates, and are cleared only after a sufficiently confident real project
  switch; project-pin guards no longer leak across that boundary.
- Continuation candidate age now uses the timestamp of the last goal/task
  inference rather than unrelated mechanical session updates, and completed
  wake results without a `next_step` are no longer ranked as future work.
- Session-note continuation reads the newest matching vault note directly,
  including notes beyond the first graph page.
- Desktop chat applies the same privacy egress filter as the wake and MCP
  profiles, and records typed chat requests into `last_user_command` through
  the cross-process context-intent bridge.
- Curator project attribution now comes only from the post-hysteresis resolver;
  raw frame hints can no longer reintroduce a project the resolver rejected.
- Configured package budgets are clamped like request overrides, empty
  privacy-gated session headings are omitted, project-zone changes count as
  activity, and triage coalescer health is published to the Context page and
  self-healing runbook.

Integration-seam defects in the events → dedupe → distiller → episodic →
wake path, found by the whole-branch review of the context engine (fix
wave 3a):

- **`local_only` was lost between the events table and the cloud-bound wake
  prompt.** The events path gated correctly, but the *memories* section
  hardcoded `local_only: false` on every line, and nothing upstream carried the
  flag: `query_undistilled_events` had no sensitivity predicate,
  `event_to_memory_event` never read `row.sensitivity`, and `EpisodicEvent` had
  no sensitivity field at all. So text withheld from Anthropic as an event was
  sent to Anthropic as a "relevant memory" one distillation pass later — the
  package's `!(cloud && local_only)` memory filter could never fire.
  `EpisodicEvent` now carries `sensitivity` (additive LanceDB column, migrated
  in place like B6's `project`; legacy rows read `cloud_allowed`), both
  distillation rungs set it — events from the row, raw frames from the frame's
  own zone — and the wake packager maps it onto `MemoryLine::local_only`. The
  vault timeline mirror carries the same tag (additive `events.local_only`
  column, non-destructive ALTER).
- **Every file event in a project collapsed onto one dedupe row.** Summary
  normalization strips path-like tokens, and a file event's entire summary *is*
  a root-relative path — so `"src/main.rs"` normalized to `""` and every file in
  every subdirectory hashed to the same key. Touching 40 files produced one row
  reading `src/first_file_you_touched.rs ×40`, which is what "Recent changes",
  the Context page strip and `context_search` all reported; `file_renamed`
  normalized to a bare `"→"`. Per-path file events now key on the raw summary.
  The storm templates (`files_bulk_change`, resync) keep normalization, so
  repeated bulk rows still collapse across flushes.
- **Collapsed frames were recorded twice and the compression ladder inverted.**
  A collapse bumps `count`/`ts_last` and records nothing about the collapsing
  frame, and the deduped row keeps the *first* occurrence's `raw_reference` — so
  only frame #1 of N was excluded by the fallback distiller's `NOT EXISTS`
  predicate, and frames #2..N distilled again as raw frames on exactly the
  condition (`screen_has_error = 1`) that caused the classification. Fourteen
  build failures produced `"build failed (×14)"` **plus** thirteen raw-frame
  memories of the same moment. The events writer now stamps
  `perception_frames.context_event_at` for every occurrence, insert or collapse,
  and the fallback skips stamped frames. The memory-precision bench now
  exercises both rungs of the ladder, so this cannot regress unnoticed — it
  reported a 0.000 duplicate rate throughout because it only ever ran the events
  path.
- **Collector events were never distillable under the default config.**
  Deterministic collector events are emitted at importance `0.2`, the distiller
  filtered at `distillation_min_salience = 0.35`, so every window/git/file/system
  event was silently excluded from the "primary" input and the compression
  ladder's top rung was empty by construction. Events now have their own
  threshold, `[memory] distillation_min_event_importance` (default `0.15`),
  documented alongside the frame threshold it was conflated with. Two adjacent
  cliffs closed with it: a classification whose JSON omits `importance` now
  defaults to `0.4` instead of `0.0` (an omitted score is not a claim of
  worthlessness), and a blank-summary event no longer suppresses its own source
  frame — blank rows are excluded from distillation forever, so that combination
  erased the moment from memory entirely.
- **The events writer undid the classifier's deliberate project drop.** The
  flush-time project fill ran over the whole batch guarded only by
  `project_id.is_some()`, so a classified event whose project the consumer had
  deliberately resolved to `None` (unknown slug, no resolver value) was
  re-stamped minutes later with whatever the resolver happened to hold — often
  from a different window entirely. Only handle-less sources (`window`,
  `system`, `voice`) are stamped now; `screen`/`audio` resolved their own
  project at frame time and `git`/`file` always carry their watched root's. The
  stamp also moved to the send seam, so the B5 observer tap sees the same
  `project_id` the persisted row gets instead of always seeing `None`.

Privacy and egress defects found by the whole-branch review of the context
engine (fix wave 2):

- **The continuation path laundered `local_only` session state into the cloud.**
  The continuation resolver ranks `SessionState::current_task` *raw* (by design —
  a rehydrated task must still rank), which bypasses `cloud_view`. Its output
  then reached Opus through the two package sections that carried no zone tag at
  all: "## Recommended next step" and "## Why you were woken", both rendered
  verbatim, while the same package's "## Session state" correctly printed
  "working in a private context". `NextStep` now carries `local_only` and is
  cloud-gated like every other section, continuation candidates inherit the
  session's zone (strictest wins when duplicate texts collapse), and spec §4.1
  propagation rule (3) — *"a wake triggered by a `local_only` frame gets the
  generic reason 'user activity in a private context'"*, previously implemented
  nowhere — is now enforced by the renderer for both a `local_only` trigger frame
  and a `local_only` session.
- **A `local_only` frame was gated on a string literal a legacy knob could switch
  off.** The cloud gate re-derived the zone by sniffing the redacted-window-title
  literal, which the collector only produces when `[context]
  redact_sensitive_titles` is true. With it false, a `local_only` window kept its
  real title, the gate returned false, and "## Current moment" rendered the title
  to the cloud (and session state seeded `open_files` from it). Perception frames
  now carry the collector's own `PrivacyDisposition` — additive, serde-defaulted,
  persisted in a new `context_privacy` column — and the gate reads that, keeping
  the literal check only as a fallback for frames written by older builds.
- **Explicit deletion was gated on a rotation knob.** `Forget` and `Delete range`
  passed `[storage] delete_screenshots_with_rotation` straight into the raw log,
  so a user who turned it off to keep screenshots through rotation and then hit
  Forget got the row deleted, success reported, and the JPEG orphaned on disk —
  unreachable by any later targeted delete. Explicit deletion now always removes
  the referenced file; the knob governs rotation only, as the docs already said.
- **`delete_range` did not sweep frame-derived vault candidates.** It purged
  episodic memories, frames, events and screenshots but left the pending
  decisions quoting frames the user had just erased. It now runs the same
  candidate rung `forget` does, in one pass over the vault for the whole window.
- **The project slug leaked from a `local_only` session.** Goal and task were
  generalized while `Project: <slug>` still rendered, naming *which* private thing
  was being worked on. It is now hidden with the rest.
- **The base64 scrubber missed base64url and three vendor token shapes.** The
  pattern used the standard alphabet only, so JWT segments, `ya29.` Google
  tokens and modern PATs broke at their first `-`/`_` and each fragment often
  fell under the 32-char floor. The alphabet now includes `-`/`_`, `=` is a valid
  boundary (`export TOKEN=<payload>` was unmatchable), and Slack `xox?-`, GitLab
  `glpat-`, Google `ya29.` and whole-JWT rules run ahead of it. The redaction
  bench's corpus grew from 8 to 13 secrets and 4 to 6 survivors and still reports
  zero leaks and zero false positives.

Concurrency and liveness defects found by the whole-branch review of the context
engine (fix wave 1):

- **A panicking triage evaluation silently killed triage forever.** The
  coalescer's busy flag was released only when a spawned evaluation reported
  back, so one panic (or a cancelled task) parked every later frame with nothing
  logged. Evaluation tasks now carry a drop guard that reports an empty result on
  any abnormal exit, and the coalescer publishes a busy-since clock in the health
  snapshot, so an evaluation wedged for more than a minute asks for a restart.
- **A panicking wake latched the orchestrator busy flag forever.** Both wake
  sites (the decision path and the daily maintenance ticker) now release the flag
  from a drop guard instead of a store at the tail of the body, so Continuum
  cannot stop waking because one wake unwound.
- **Deleting context froze push-to-talk.** `forget` and `delete_range` ran inline
  on the runtime's 250 ms select loop, awaiting LanceDB, two SQL deletes, one
  `remove_file` per referenced screenshot and a full vault scan. They now run on
  a serial worker task, so the voice ticker and the hotkey stay live during a
  "delete the last hour" click. The other intent kinds still apply inline.
- **Per-frame database writes could stall the whole loop.** The four bookkeeping
  writes on the frame arm are spawned rather than awaited, and the raw-log pool
  now sets an explicit 250 ms `busy_timeout` in place of sqlx's 5 s default, so a
  frame write landing during the events writer's batch transaction or an hourly
  rotation fails fast instead of blocking.
- **Retention rotation blocked an async worker thread.** The screenshot deletion
  loop and the recursive backstop sweep now run on `spawn_blocking`.
- **Clean shutdown dropped up to 256 buffered events.** The events writer's final
  flush raced the pool close; shutdown now waits (up to 5 s) for the writer task
  to stop before closing the raw log.
- **Two unbounded maps in project discovery.** Un-promoted dwell entries age out,
  and the resolved-path set is capped at 2048 with oldest-first eviction.
- **A dead hotkey listener spun the select loop.** A closed channel now logs at
  error level and disarms the arm once instead of being re-polled forever.

Defects the context engine work closed along the way:

- **Project detection was hardcoded.** `infer_project_hint` matched a fixed
  keyword list against the window title and nothing else. It is now a
  backward-compat shim over the resolver's keyword tier, consulted only when no
  project resolves; semantic-fact prefixes, the curator's activity signal,
  curator sessions and skills matching all receive the resolver's
  post-hysteresis output instead.
- **`project_world_state` reported the daemon's own directory** as the user's
  project — it read `current_dir()`, which is always Continuum's own working
  directory. It now derives from the resolver.
- **Triage's `Remember` decision was discarded** and the `triage_decision`
  raw-log column was never written. Both are now consumed: a `Remember` (or a
  `should_store` classification) creates a vault memory candidate with a
  per-type TTL, and the column is populated per frame.
- **Vault note expiry was unreachable.** `Vault::create` hardcoded
  `expires: None`, so no caller could ever set a TTL and the existing
  sweep-expired path could never fire for a new note. `NoteDraft` gained an
  `expires` field that `create` now writes into frontmatter.
- **Maintenance wakes got an empty history.** The maintenance ticker passed
  `&[]` as recent frames; it now shares the same bounded frame ring as the
  frame loop.
- **`PrivacyDisposition::Excluded` was dead code** — nothing ever produced it.
  It is now the `never_observe` sentinel disposition.
- **The triage JSON extractor locked onto the wrong object.** It returned the
  last balanced `{…}`, which with a nested classification block meant it
  latched onto the nested object and silently lost the decision. It now returns
  the last *top-level* balanced object, and a truncated or malformed
  classification never burns a retry.
- **`context_package` rendered unscrubbed database rows.** It correctly decided
  *whether* a row could leave the machine but then rendered the survivor's
  summary, title and application straight from SQLite, unlike the timeline,
  search and files tools. A row written before a privacy rule existed could
  walk out unredacted. Found by the redaction bench.
- **One build-failure loop left 27 near-duplicate pending vault notes.**
  Candidates were proposed per *frame* while the events writer collapses per
  *row*. Candidates are now gated on the same dedupe key as the event, one per
  collapsed row; the measured duplicate rate went from 0.55 to 0.00.
- **Raw-log rotation had no production caller** despite the docs claiming it
  ran nightly. It is now wired onto the distiller ticker at a one-hour floor.
- **The MCP live-context round-trip test asserted a frozen schema version**
  (`live-context/v1`) and failed on `main`; it now tracks the constant.

- Fixed Tools & Skills server installation by adding validated local executable registration, in-product progress and errors, and next-run MCP configuration wiring.
- Publish live voice volume, wake-word, queue, listening, and ambient-mute state
  from the runtime to the desktop without overwriting curator or live-context
  telemetry.
- Keep release version bumps compatible with `--locked` after adding the
  `continuum-memory` workspace crate.
- **Windows updater retry loop**: release publishing now clears cached Tauri
  bundles, selects exactly one installer matching the calculated version, and
  rejects mismatched signature metadata. The desktop app remembers an
  interrupted install attempt per release, stops automatic retries on later
  launches, and keeps an actionable manual retry path without altering user
  data or configuration.

### Added

- **The context engine (Plans A + B + C, spec
  `docs/superpowers/specs/2026-08-05-context-engine.md`)**: Continuum now
  continuously knows *what you are doing* and hands the same accurate,
  privacy-filtered picture to every AI that needs it — the orchestrator at
  wake, the desktop chat, and any tool call. The pipeline is
  `capture → dedupe → privacy filter → classification → project/goal →
  session state → curator/memory → context package → Main AI`. User-visible
  capabilities:

  - **A privacy filter that lands first.** Three zones (`never_observe`,
    `local_only`, `cloud_allowed`) configured as `[[privacy.zones]]` rules on
    process name and/or window title. Secrets, cards, IBANs and (optionally)
    emails are scrubbed from all free text; home directories and usernames are
    scrubbed from every path. Existing `[context] sensitive_process_names` /
    `sensitive_title_keywords` keep working — they are synthesized into zone
    rules at load, union applied, stricter wins. Derived artifacts inherit the
    strictest zone of their inputs. **Keyboard capture remains permanently out
    of scope.**
  - **Five honest toggles** — `mic`, `screen`, `files`, `git`, `pause_all` —
    enforced at each collector within one loop iteration, flippable from the
    Context page, persisted to config, and each writing a `toggle_change`
    event. Three limitations are documented rather than hidden: `mic` off
    leaves the OS microphone indicator lit until restart, a runtime that
    *booted* paused needs a restart to unpause, and persisting a toggle
    rewrites `config.toml` without its comments.
  - **Project detection that actually works.** A single resolver, owned by the
    frame loop, resolves per frame with hysteresis over five tiers (user
    override rules → path in the window title → editor title pattern → git root
    of the most recent file event → keyword match), plus auto-discovery of
    candidates from title-derived paths. **Unconfirmed candidates are never
    collected from.**
  - **Git and file observation.** A git collector for the active *confirmed*
    project (one consolidated `status --porcelain=v2 --branch` + `log -1` per
    poll, timeout-bounded, no window) reporting branch, dirty/staged/untracked,
    ahead/behind, conflicts and last commit. An opt-in `notify` file watcher
    (`[file_watcher] enabled = false` by default) over confirmed roots with
    debouncing, ignore globs, rename pairing, per-root rearm and storm
    coalescing.
  - **A deduped event stream.** New `context_events` table plus an FTS mirror,
    fed by a dedicated writer task through a bounded queue that collectors
    never block on. A build that fails fourteen times is **one row with
    `count = 14`**, which is what makes "build failed ×14" reach memory instead
    of fourteen near-identical notes.
  - **Live session state.** Active project, app, window title, open files, last
    error / success / user command update mechanically; goal and task are
    inferred by a background LLM task that never runs while idle, never
    preempts interactive triage, and is capped at 256 tokens. It rehydrates on
    boot from the last published snapshot plus recent events.
  - **"Ga door" resolves to something.** A pure, LLM-free continuation resolver
    ranks five candidates by confidence × prior × recency decay; above
    `[continuation] confidence_floor` the wake package renders a recommended
    next step, below it the orchestrator is told to ask one short question.
  - **Ten new read-only MCP tools** — `context_session`, `context_window`,
    `context_screen`, `context_audio`, `context_projects`, `context_timeline`,
    `context_search`, `context_files`, `context_git`, `context_package` — all
    privacy-gated, all degrading to `available: false` / `stale: true` instead
    of erroring, all emptied by `[context_tools] enabled = false`. Withheld
    private rows are *counted* (`omitted_private`), not silently dropped, and
    `context_git` refuses to probe a project the user has not confirmed.
  - **A Context page** in the dashboard: session state with confidence bars,
    per-source health with disabled reasons, the live toggles, a recent-events
    strip, projects and discovery candidates, and an empty state with an
    Add-project CTA. Corrections travel as intent files with **no expiry** —
    a correction made while the runtime is down still applies at its next
    boot. Actions: add/confirm project, correct project or goal, "not this
    project", pin, forget (cascading across events, frame, screenshot,
    episodic memory and the unconfirmed vault candidate), delete range, and
    toggle changes. Every applied intent, every wake, and every automatic pin
    expiry is appended to `<data_dir>/logs/actions.jsonl`.
  - **Four evaluation harnesses**, all offline and deterministic in mock mode:
    `continuum-context-bench` (recall of project/goal/task/blocker/last action,
    `--live` for the real model), `continuum-dedupe-bench`,
    `continuum-memory-precision-bench`, and `continuum-redaction-bench`, plus
    `continuum-triage-bench --prompt-fit-only` as the no-GPU half of the triage
    gate and `continuum-perception --record <path>` for capturing your own
    (strictly local) session. The committed fixture is synthetic and generated;
    real recordings never enter the repository.

  New config sections: `[privacy]`, `[privacy.toggles]`, `[[privacy.zones]]`,
  `[projects]`, `[[projects.known]]`, `[git_context]`, `[file_watcher]`,
  `[events]`, `[session_state]`, `[context_package]`, `[continuation]`,
  `[performance]`, `[context_tools]`, `[memory.candidate_ttl_days]`. New docs:
  `docs/context-engine.md` (what is observed, zones, toggles, tools, the
  Context page, the full config reference, how to run the benches, and the
  honest limitations); `docs/mcp-tools.md` and `docs/self-healing.md` gained
  context sections; `ARCHITECTURE.md` gained a context-engine section.

  The per-task entries below carry the implementation detail for Plan B.

- **Chat context profile + continuation resolver (context engine Plan B,
  Task B8)**: two things land together.

  **Chat profile (desktop, spec §4.9 matrix).** The chat system prompt
  gains a "## Session state" section read from the runtime's published
  `state.json` — active project, inferred goal/task (only above
  `[session_state] confidence_floor`), and recently touched files —
  rendered through the *same* ungated `ContextPackage` renderer the wake
  profile uses, under `[context_package] chat_token_budget` (600 tokens)
  and the same cloud gate (a `local_only` goal/task generalizes to
  "working in a private context"). It is strictly **additive**: the
  in-process vault search that feeds "## Memory context" is untouched, so
  a desktop-only install with no background runtime behaves exactly as
  before. When the runtime is not publishing, has published no
  `session_state` yet, or its snapshot is older than an hour, **no
  section is rendered at all** — the prompt's "## Live status" footer
  already reports `Background runtime: not running`. New knob:
  `[chat] include_session_context` (default `true`).

  **Continuation resolver (`context::continuation`, spec §4.12).** When
  the user says "ga door" — or presses the hotkey without saying anything
  — Continuum now resolves *what* to continue instead of guessing. Pure
  logic, **no LLM call**, ungated like the packager. Five candidates, all
  with real producers: the session-state task (survives a restart via
  §4.8 rehydration), the last wake's `wake_result.next_step`, the
  curator's `open_task:` session-summary trailer, the last user command,
  and the last error. Each is scored `base confidence × per-kind prior ×
  recency decay` (flat for 30 min, halved per hour after, floored at
  0.1); the priors put a *task* above an *error* on purpose — an error is
  a symptom, a task is intent. At or above `[continuation]
  confidence_floor` (0.6) the wake package renders "## Recommended next
  step" and the wake reason says `Continue: <target>`; below it the wake
  reason instructs the orchestrator to ask **one** short question naming
  up to three candidates. Ordinary wakes short-circuit before any extra
  read, so this costs nothing when it does not apply. New config section:
  `[continuation]` (`confidence_floor`, `trigger_phrases`,
  `wake_result_lookback_hours`).

- **Unified context package + wake profile + post-wake record (context
  engine Plan B, Task B7)**: everything Continuum knows about "right now"
  is now assembled into **one struct with one renderer** —
  `context::package::ContextPackage` (spec §4.9). The module is
  deliberately **ungated** (pure owned types + string formatting), so the
  runtime, the MCP server and the desktop app all link the same packager;
  the `--no-default-features` build is the parity gate. Sections: current
  moment (caption + **window title, finally rendered** + world-state blob
  + audio), session state, just before, relevant memories, semantic
  facts, vault notes, recent changes, failed attempts, last success,
  available tools + permission mode, recommended next step, pending
  memory decisions, why-woken. Three properties are enforced by the
  renderer rather than by convention: the **cloud gate** (every section
  item carries `local_only`; cloud-bound renders skip those items and
  *generalize* — never silently leak — the current moment and session
  state), the **order contract** (pending memory decisions stay the last
  section before the wake reason), and the **budget** (`[context_package]
  token_budget`, default 1000 for the wake profile / 600 for chat, with
  per-section caps and a documented, tested drop ladder: open files →
  recent changes → just-before tail → memories tail; why-woken, current
  moment, session state and pending decisions are never dropped). The
  wake message is now assembled from *all* of the runtime's sources:
  session state, the deduped `context_events` table (count-aware "×14"
  lines for just-before / recent changes / failed attempts / last
  success), retrieval, the compact world state, and the composed wake
  config's real tool list and permission mode. Every source is wrapped —
  a failing one logs and leaves its section empty rather than failing the
  wake. The frame loop's recent-frames buffer became a **shared ring**
  (`Arc<Mutex<VecDeque<..>>>`, snapshot-cloned under a short guard, never
  held across an await), so the daily memory-maintenance wake stops
  passing an empty history and sees the same "just before" a triage wake
  does. Finally, each completed wake writes a **post-wake structured
  record**: a best-effort `{action, result, next_step}` trailer parsed off
  the orchestrator's reply (case-insensitive, markdown-tolerant, Dutch
  labels included, last occurrence wins, garbage yields nothing) becomes a
  `wake_result` system event plus a vault timeline entry, and curator
  session summaries now always end with a normalized `open_task:` line —
  the two producers the continuation resolver reads without spending an
  LLM call.
- **Compression ladder + episodic project (context engine Plan B, Task
  B6)**: memory distillation now reads **deduped context events** as its
  primary source (spec §4.11) instead of raw frames. Fourteen identical
  build failures collapse into a single episodic memory reading
  `"build failed (×14)"` with a bounded importance boost (+0.05 per
  doubling of the count, capped at +0.20) rather than fourteen
  near-identical vectors crowding out everything else in a similarity
  search. Raw frames that never produced a classified event still distil
  through the original salience predicate as a documented fallback, and
  frames that *did* produce an event are excluded so a moment is never
  recorded twice. Every `EpisodicEvent` gained an optional `project`
  (additive LanceDB migration: tables written by earlier builds are
  migrated in place with `add_columns`, and if that is refused the store
  degrades to writing the old column set rather than refusing to open).
  Wake retrieval filters episodic recall on the resolved project
  **softly** — other projects' memories are dropped, unattributed ones
  always survive, so recall never gets worse than before the field
  existed. The vault's `distilled` timeline events now carry the project
  too. Screenshots gained a real retention story: rotation deletes the
  file each expired frame row referenced *and* sweeps `screenshots_dir`
  for images older than the new `[storage] screenshot_max_age_hours`
  (default 720 h) — the mtime backstop that reclaims files orphaned by a
  crash between "image saved" and "row written". Both deletions are
  gated by `[storage] delete_screenshots_with_rotation` (default true),
  and raw-log rotation is finally *wired*: it runs hourly on the memory
  distiller's ticker, including when distillation is disabled, because
  retention is a privacy promise rather than a distillation feature.
- **Live session state (context engine Plan B, Task B5)**: Continuum now
  keeps an explicit answer to "what is the user doing right now" in a new
  `context::session_state` module (spec §4.8) — active project, app, window
  title, best-effort open files, last error, last success, last user
  command, plus an inferred goal and task with a confidence. The mechanical
  fields update synchronously in the frame loop and off the context-event
  stream (a new non-blocking observer tap on `EventSender` that runs
  *before* the persistence queue, so a full queue costs a row but never a
  state update). Goal/task inference runs in its **own spawned task** that
  the frame loop never awaits: it fires only on a project switch, ≥ 8
  significant events, or 10 minutes of staleness, never more than once per
  2 minutes, never while the machine is idle, and always through the
  background LLM tier introduced in Task B2 (behind interactive triage,
  ≤ 256 tokens). A reply under `[session_state].confidence_floor` is
  discarded rather than stored, and consumers render "unknown" — the honest
  answer. Zone propagation per spec §4.1: an inference window containing a
  `local_only` event tags its output `local_only`, and `cloud_view()`
  collapses goal/task to "working in a private context" for cloud-bound
  renders.
- **Session state survives a restart**: on boot the hub rehydrates from the
  last published `state.json` snapshot plus the last hour of
  `context_events`, with confidence discounted by age (full within 30 min,
  ×0.5 to 4 h, ×0.25 and capped at the confidence floor beyond). The
  inferred text is kept even when the discounted confidence drops below the
  floor — renderers hide it, but the §4.12 continuation resolver can still
  rank it, which is what will make "ga door" work after a restart.
- **Two long-standing hardwired blanks are now filled**: the triage layer's
  `memory_summary` argument (`""` since the layer existed) is a compact,
  600-char-capped session-state render, and the skills `MatchContext.task`
  (hardwired `None`) is the inferred task when it clears the confidence
  floor, with the session's project as the fallback when the resolver has
  no opinion yet.
- **Curator `SessionTracker`**: gained a public `snapshot()` accessor and a
  **project-change session boundary**. Because `project_hint` is the
  resolver's post-hysteresis output, a genuine project change ends the
  session immediately with no extra dwell floor; first adoption
  (`None` → `Some`) and hint loss are deliberately not boundaries.
- New `[session_state]` config section (`infer_min_interval_secs=120`,
  `infer_max_age_minutes=10`, `infer_min_new_events=8`,
  `significant_importance=0.5`, `confidence_floor=0.4`,
  `infer_max_tokens=256`) and a new `prompts/session-state.md`.

- **Screen caption split from the world-state blob (context engine Plan B,
  Task B4)**: `ScreenObservation.description` is a one-sentence vision
  caption again, and the compact multi-monitor world-state text moved to a
  new additive `ScreenObservation.world_compact` (spec §4.10). Since the
  live-context work, `description` had been carrying
  `compact_for_agents(1400)` — ~1.4 kB of monitor/window/project/health
  lines — which meant every triage prompt paid ~400 tokens for it, the
  dashboard's "last description" rendered as a wall of text, and episodic
  memories and retrieval queries embedded machine boilerplate instead of a
  sentence. The blob is persisted beside the caption in a new additive
  `perception_frames.screen_world_compact` column and is the context
  packager's input; the caption is what triage, the distiller, the
  retrieval query builder, the wake "current moment" section and the
  dashboard render. The triage user turn no longer serializes
  `PerceptionFrame` at all: it renders a hand-maintained
  `triage::prompts::TriagePromptFrame` projection (context/audio/screen/
  salience only, matching the system prompt's few-shot shape), so future
  frame fields cannot silently inflate the prompt. `continuum-triage-bench`
  asserts both that the prompt fits `context_size − max_tokens` and that
  the world-state blob is absent from it.
- **Classification consumption — events, candidates, column (context engine
  Plan B, Task B3)**: the classification block that rides every triage call
  (Task B1) is finally *used* (spec §4.7 "Consumption"). Each classified
  frame emits a `context_events` row on the Plan-A events channel — source
  `screen`, or `audio` when the frame carried a transcript — carrying the
  classifier's summary/importance/confidence and the frame's zone as its
  sensitivity (§4.1 propagation rule: an excluded window produces no row at
  all, a `local_only` window produces a `local_only` row). A classification
  with `should_store`, or any `remember` decision, additionally proposes a
  **vault memory candidate** (`status: candidate`, `source: observed`,
  project from the resolver, tags `observed` + an epistemic label
  `user_stated`/`system_inferred`) through the spec's mapping table
  (`error→error`, `decision→decision`, `preference→preference`,
  `task_progress→task`, `success→note` tagged `result`,
  `communication`/`other→note`, `routine`→ no candidate), duplicate-checked
  with the curator's own near-duplicate check. Candidates expire per type
  from the new `[memory.candidate_ttl_days]` config (task 30, error 30,
  note 90 days; decision and preference never) — `NoteDraft` gained an
  additive `expires` field that `Vault::create` now writes into
  frontmatter, so the existing expiry sweep archives unreviewed
  observations instead of letting them pile up. The long-dead
  `triage_decision` raw-log column is populated at last
  (`<decision>` / `<decision>/<event_type>`) via a new
  `RawLog::set_triage_decision`. The distiller's SQL predicate is
  deliberately untouched and stays the fallback for frames with no
  classification. Nothing blocks the main loop: the event send is
  non-blocking and the vault/raw-log writes are spawned with contained
  errors.
- **Triage off the main loop + LLM priority gate (context engine Plan B,
  Task B2)**: triage evaluation no longer runs inline in the runtime's main
  select loop (spec §4.7 "Triage off the main loop", a Critical design
  finding — a curator generation queued on the shared model mutex could
  freeze the voice ticker and hotkey arms for 10–25 s). Gated frames are
  now `tokio::spawn`ed through a new `triage::coalesce::TriageCoalescer`
  (busy-CAS + one-deep latest-wins slot, mirroring the `do_wake` pattern):
  a burst of frames coalesces to the newest, and results return over a
  channel into their own select arm where decision consumption runs
  unchanged in order and semantics (voice intents still outrank and
  supersede same-frame triage wakes). A new ungated `llm_gate::LlmGate`
  enforces two-priority access to the shared local model: interactive
  triage acquires directly, background callers (curator extraction,
  session summaries, conflict detection, future session-state inference)
  try-acquire with 250 ms backoff behind any interactive waiter, time out
  after 30 s, and are clamped to ≤ 256 tokens per call — curator
  extraction (was 1024) and session summaries (was 700) got tighter
  budgets accordingly.

- **Triage classification output (context engine Plan B, Task B1)**: the
  triage LLM call now doubles as the Context Model call (spec §4.7).
  `TriageLayer::evaluate` returns a new `TriageOutput` wrapper —
  `#[serde(flatten)]` decision plus an optional `classification` block
  (`event_type` from the closed §4.6 registry, project slug, importance/
  confidence clamped to [0,1], 200-char summary, `should_store`) — parsed
  leniently: a malformed or truncated classification never costs a GPU
  retry (the decision is salvaged, classification drops to `None`). The
  JSON extractor now correctly returns the last *top-level* object instead
  of locking onto a trailing nested block. `prompts/triage-system.md`
  documents the classification key inside the single output object, triage
  `context_size` default rose 2048 → 4096, and `continuum-triage-bench`
  gained a prompt-fit gate (`prompt_tokens < n_ctx − max_tokens`, byte
  heuristic). Classification consumption (events channel, vault
  candidates) lands in Task B3.

- **Chat memory tools**: the Chat tab's AI can now read and write the memory
  vault through an explicit, config-gated tool set, instead of chat being
  fully sandboxed from the runtime. A new `continuum-gateway` tool-calling
  layer (`ToolDef`, `ToolExecutor`, `McpSpec`, and `ChatEvent::ToolCall`/
  `ToolResult`) backs internal tool loops in the OpenAI-compatible and
  Anthropic adapters (a first request that gets a 4xx with tool definitions
  attached is retried once without tools, so plain chat still works against
  endpoints that reject `tools`), and lets the Claude CLI adapter attach a
  `continuum-mcp` server via `--mcp-config` and pass its `tool_use`/
  `tool_result` traffic straight through as gateway events. On top of that,
  `apps/desktop/src-tauri/src/chat_tools.rs` exposes four provider-neutral
  tools — `memory_search`, `memory_get`, `memory_save` (same-title
  case-insensitive upsert), `memory_delete` — run in-process against the
  same vault the Memory tab uses for OpenAI-compatible/Anthropic providers;
  the Claude CLI provider instead gets the `mcp__continuum__memory_vault_*`
  family (including the new `memory_vault_delete`, see below) plus the
  existing fact/episodic tools attached over MCP, since the CLI subprocess
  can't call back into the desktop process directly. A per-turn
  "## Memory context" section injects up to `memory_context_notes_max`
  vault notes matching the new user message (confirmed status, sensitivity
  gated) ahead of any tool call, and every tool invocation is persisted on
  `StoredMessage.tool_calls` and rendered as a card in the chat UI, live
  and from history. New `[chat]` keys: `memory_tools_enabled` (default
  `true`), `memory_tool_max_rounds` (`8`), `memory_context_notes_max` (`6`,
  `0` disables injection), `include_sensitive_memory` (`false`). Known
  limitation: the sensitivity gate filters `memory_search` results and
  injected context, but does not reach the Claude CLI provider's
  `memory_vault_search`/`memory_vault_get` MCP tools — a follow-up is
  planned. See `docs/chat.md`'s new "Memory access" section.
- **`memory_vault_delete` MCP tool**: permanently removes a vault note's
  markdown file and index entry (unlike `memory_vault_resolve`'s `reject`
  or the curator's `archive`, which only change `status`). Additive, no
  existing tool schema changed. Returns `{ deleted, id }`; errors if `id`
  doesn't exist. Requires session confirmation (`session-approved` in
  `config/default-permissions.toml`), same tier as the other vault-writing
  tools. See `docs/mcp-tools.md`.
- **Memory vault + graph-centric Memory tab (Plan A)**: a new
  `crates/continuum-memory` crate (dependency-light: `sqlx`/SQLite,
  `serde_yaml`, `ulid`, `notify` — no llama/whisper/lancedb) implements an
  Obsidian-like memory vault — markdown files with YAML frontmatter are the
  **source of truth**; a derived SQLite index
  (`vault/.continuum/index.db`: nodes, edges, FTS5 full-text, an event
  timeline, and a quarantine table) is always fully rebuildable and is
  rebuilt on every open, so a missing/corrupt index self-heals rather than
  losing data. Ten node types, a `candidate → confirmed | rejected |
  superseded | archived` status lifecycle, typed `relations:` edges plus
  untyped `[[wiki-link]]` mentions with ghost-node fallback for unresolved
  targets, atomic (tmp+rename) writes, and per-file quarantine for broken
  frontmatter. Both `continuum.exe` and `continuum-desktop` link the crate
  **directly** and open the same vault in-process (no IPC between them);
  cross-process change propagation is a debounced file-watcher, so external
  edits (including opening the vault in Obsidian) are picked up live. New
  `apps/desktop/src-tauri/src/memory.rs` exposes 13 Tauri commands
  (`memory_graph`, `memory_search`, `memory_get_note`, `memory_create_note`,
  `memory_save_note`, `memory_delete_note`, `memory_resolve_candidate`,
  `memory_pending`, `memory_events`, `memory_vault_info`,
  `memory_migrate_legacy`, `memory_rebuild_index`, `memory_open_vault`) and
  a `continuum:memory` event topic for live updates. The Memory tab is
  fully rebuilt around a force-directed graph (`force-graph`, new frontend
  dependency) as the main surface: a docked resizable note panel that
  promotes to a full-screen markdown editor overlay, a floating curator-card
  stack and a bottom timeline scrub strip (both wired up and currently
  empty — they light up once the curator pipeline, Plan B, starts writing
  candidates and events), saved filter views, full-text search, and a
  vault-actions menu (rebuild index / import legacy memory / wipe derived
  data — vault markdown is never deleted by it, though the wipe itself is
  currently a stub that validates confirmation and logs the request rather
  than clearing raw log/episodic/events data; a follow-up
  `memory__wipe_all` MCP tool will back the real wipe). One-shot,
  idempotent migration (`migrate_legacy_semantic`, surfaced as "Import
  legacy memory") converts
  the old flat-file `semantic.sqlite` key/value store into vault `fact`
  notes without ever modifying the legacy database; the runtime's existing
  semantic MCP tools keep working against it unchanged until the curator
  (Plan B) retires them. New `[memory.vault]` (`vault_dir`,
  `watcher_debounce_ms`, `events_retention_days`, `graph_max_nodes`) and
  `[memory.curator]` (`enabled`, `interval_minutes`,
  `max_candidates_per_pass`, `auto_confirm_threshold`, `discard_floor`,
  `claude_batch`, `session_summary_idle_minutes`, `wake_vault_notes_max`,
  `include_sensitive_in_context`) config sections — the curator section is
  configurable now per non-negotiable #3 even though nothing reads it until
  Plan B ships. A new `memory_vault` health probe
  (`apps/desktop/src-tauri/src/components.rs`) reports Degrading when notes
  are quarantined and Error when the vault fails to open; recovery
  procedures are documented in `docs/self-healing.md`. **Internal breaking
  change to the dashboard IPC surface**: the prior fixture-backed stub
  commands (`search_episodic`, `delete_episodic`, `list_semantic`,
  `set_semantic`, `delete_semantic`) and their `tauri.ts`/`types.ts`
  wrappers are removed outright — pre-alpha, no external consumers, no
  compat shim. See `docs/memory.md` (new) and the updated "Memory system"
  section of `ARCHITECTURE.md`.
- **Memory curator pipeline (Plan B)**: the background process that turns
  what Continuum observes into vault candidates, catches contradictions, and
  keeps the vault tidy — the part of the memory vault (above) that was
  configurable but dormant now actually runs. New
  `crates/continuum-core/src/curator/` module, spawned at boot whenever a
  triage LLM is loaded (skipped with a log line otherwise): an **extraction
  pass** (`prompts/curator-extract.md`) every `interval_minutes` reads new
  vault events and asks the triage LLM for candidate notes, deduplicates
  them (normalized title + FTS check against existing/rejected notes), and
  routes each by confidence — auto-confirmed above `auto_confirm_threshold`
  from a `user_statement` source, written as a review candidate in between,
  discarded below `discard_floor`; a **conflict/supersede pass**
  (`prompts/curator-conflict.md`) checks each newly-written note against up
  to 2 same-type/same-project confirmed notes and, above
  `supersede_confidence_floor` (new config key, default 0.5), attaches a
  `proposes_supersede` relation without ever auto-flipping the old note's
  status; a **session tracker** (`crates/continuum-core/src/curator/session.rs`,
  its own idle/process-change boundary detector, not fed by triage)
  compresses a finished work session into a `Session` note via
  `prompts/curator-session.md` (or writes nothing on a literal `SKIP` reply);
  a **daily hygiene tick** prunes expired nodes/old events and drains any
  pending derived-data wipe request, once at boot (even with the curator
  disabled) and once per local calendar day thereafter; and a **daily
  maintenance-wake ticker** (new `[memory.curator] maintenance_wake_hour`
  config key, `i32`, default local hour 4, negative disables it) — a
  purpose-built ticker, since no scheduler existed to hook into — wakes the
  orchestrator specifically to drain pending decisions on a day nothing else
  would have, sharing an atomic busy-claim with the regular triage wake path
  so the two can never double-fire. Every LLM-parse failure retries once
  before the pass/pair is skipped; a window that fails outright 3 times
  running is abandoned (boundary advances, logged, dashboard's lifetime
  failure counter keeps climbing) rather than wedging the curator forever.
- **MCP vault tools + fact-tool redirect + real wipe (Plan B)**: five new
  `continuum-mcp` tools — `memory_vault_search`, `memory_vault_get`,
  `memory_vault_save` (create-or-update-by-title), `memory_vault_resolve`
  (confirm/reject/supersede a candidate), and `memory_wipe_all` (queues a
  derived-data wipe; requires the literal confirmation string `"WIPE"`).
  `memory_set_fact` now writes exclusively into the vault (a `type: fact`
  note, matched/updated by title) instead of the legacy `semantic.sqlite`
  store; `memory_get_fact`/`memory_list_facts` read the vault first and fall
  back to the legacy store on any vault miss — no match, or the vault itself
  unavailable. New `CONTINUUM_VAULT_DIR` env var lets a non-default
  `vault_dir` reach the MCP server, which doesn't load the full runtime
  config (documented as a known limitation in `docs/mcp-tools.md`). See
  `docs/mcp-tools.md`'s new "Vault memory" section.
- **Wake-context vault retrieval + pending-decisions block (Plan B)**: every
  orchestrator wake now injects a `## Long-term memory (vault)` section
  (confirmed notes FTS-matched to the trigger frame, up to
  `wake_vault_notes_max`) and a `## Pending memory decisions` section
  (candidate notes older than 30 minutes, oldest-first, up to
  `claude_batch`) into the wake message, both sensitivity-gated (excludes
  `sensitivity: sensitive` notes unless `include_sensitive_in_context =
  true`) and both degrading to empty on any internal failure rather than
  failing the wake. Confirmed vault-notes' `id`s (not pending candidates')
  are marked `touch_last_used` on injection.
- **Curator status on the dashboard (Plan B)**: the runtime's `state.json`
  snapshot gains a `curator` field (last pass time, consecutive failures,
  lifetime candidates written, pending count, enabled); the Home tab renders
  it as a status row — healthy, degrading at 3+ consecutive failures, or
  "Curator: off" when disabled or not yet heard from. No dedicated
  health-probe/repair-restart target exists yet for the curator — documented
  as a known gap in `docs/self-healing.md`; recovery today is a full
  `continuum` runtime restart.
- **Vault index hardening (Plan B foundation)**: `crates/continuum-memory`'s
  index writes switched from deferred to `BEGIN IMMEDIATE` transactions
  (avoids a class of SQLite write-lock races), gained a commit-failure
  rollback path (a failed `COMMIT` no longer leaks an open transaction into
  the connection pool), a no-op skip for unchanged files (mtime + a hash of
  the raw frontmatter+body, so a status-only edit still counts as a change)
  that also covers previously-quarantined files, an atomic two-phase
  `rebuild()` (full in-memory scan, then one transaction — a
  concurrent reader never observes a half-rebuilt index), and
  case-insensitive `.md` extension matching to match the file watcher.
- **Runtime vault boot + distiller feed (Plan B foundation)**: the headless
  `continuum` runtime now opens the memory vault at boot (same directory the
  dashboard uses) and runs a watcher-drain task so externally-changed files
  reindex live; the existing raw-log→episodic memory distiller additionally
  appends a `kind: "distilled"` event into the vault's event timeline for
  every frame it distills, giving the curator's extraction pass something to
  read from process start.
- **Continuous all-monitor live context**: independent 200 ms capture workers
  for every connected display feed a bounded ordered FIFO with explicit drop
  accounting; luma change detection selects local vision work without pausing
  capture. A privacy-filtered, source-attributed `live-context.json` projection
  combines monitor summaries with foreground window, coarse idle/active input,
  and local terminal/project metadata. The Brain tab exposes consent/cadence
  controls and health, while the read-only `system_live_context` MCP tool gives
  image-incompatible agents one compact shared current world-state. Screenshot
  persistence remains off by default and raw keys, pointer data, clipboard data,
  and terminal text are excluded.
- **Guarded Health self-heal**: Advanced → Health now refreshes the authoritative live probes and offers a one-time repair preview before execution. The supported automatic fix starts an offline runtime only after an atomic, versioned, manifest-verified backup, then waits for a fresh heartbeat before reporting success. Other issues are tested and escalated by a main-window-authorized, short-lived, single-use, component-scoped repair session with built-in tools disabled; unsupported component restart intents remain denied. Rollback validates its source, creates a safety backup, and publishes config reversibly. Local NDJSON audit records capture previews, grants, backups, actions, and outcomes.
- **Chat tab + model gateway**: a new `crates/continuum-gateway` crate (a
  `ChatProvider` trait, three adapters — OpenAI-compatible, Anthropic, and
  Claude Code CLI — plus a static provider catalog covering ~18 presets such
  as LM Studio, Ollama, OpenAI, OpenRouter, DeepSeek, and a custom-endpoint
  option) backs a real **Chat** tab and a real **Settings → AI providers**
  ("Integrations") panel, replacing prior fixture data. Conversations stream
  token-by-token over the `continuum:chat` Tauri event, render markdown,
  support **Stop** mid-stream (the partial reply is kept and marked
  `stopped`, never discarded) and **Retry** on retryable failures, and
  persist per-conversation to `~/.continuum-dev/chats/<id>.json`. Provider
  connections persist to `~/.continuum-dev/providers.json` with **no secret
  material** — enforced by a dedicated unit test — while API keys live
  exclusively in Windows Credential Manager via the `keyring` crate (service
  `Continuum`); adding a connection tests it before saving, with an explicit
  "Save anyway" escape hatch. New `[chat]` config section (`max_tokens`,
  `temperature`, `connect_timeout_secs`, `stream_idle_timeout_secs`,
  `cli_timeout_secs`, `model_refresh_interval_secs`, `system_prompt_path`)
  makes every knob overridable per
  non-negotiable #3. A new `chat_providers` health probe reports Degraded
  when a configured provider's last connection test failed. Per
  non-negotiable #2: the only new network calls this feature introduces go
  to provider endpoints the user explicitly configures in Settings — no
  telemetry, no new default egress. See `docs/chat.md`.
- **Signed desktop updates**: the Tauri app checks for updates at startup,
  exposes a manual check in Settings, and lets users enable or disable
  automatic installation. Windows update artifacts are signed and published
  through the `main` push release workflow.
- **Redesigned dashboard**: frameless window with a single custom titlebar (the
  duplicate OS menu bar is gone), click/press animations on every control, a
  minimal Hermes/Buzz.xyz-style sidebar grouped into Daily/Configure/Advanced,
  and a Ctrl+K command palette. All mockup screens removed; the live tabs
  (Home, Voice, Memory, Brain, Tools, Automations, Health, Logs, Settings) are
  now wired to the Zustand store, which is hydrated by `bootstrapStore()` and
  kept in sync via the `continuum:state`/`continuum:log`/`continuum:repair`
  Tauri events. Window controls use `@tauri-apps/api/window` with no-op
  fallbacks outside Tauri.
- **One-command local dev**: `scripts/dev.ps1` runs the dashboard locally with
  no CI/CD or push. Modes: default (Tauri app), `-FrontendOnly` (Next.js on
  :3000), `-WithRuntime` (also start `continuum.exe` for live data), `-Check`
  (prereqs only). Aliased as `pnpm dev:local` / `pnpm dev:app`.

### Changed

Behavior changes from the context engine that reviewers should know about:

- **The three existing observation tools are now content-filtered.**
  `system_active_window`, `system_clipboard_get` and `system_live_context`
  route through the privacy filter's scrub + cloud gate. **Names and schemas
  are byte-identical** (asserted by a test) — this is a content change, not an
  API change, and non-negotiable #7 is intact. `system_clipboard_get`
  additionally gains the `[context_tools] clipboard_tool_enabled` kill-switch
  and is skipped entirely while the foreground window sits in a
  `never_observe` zone.
- **`never_observe` now drops the observation to a sentinel** rather than only
  redacting the title. The collector emits `process = "[excluded]"` with an
  empty title; no screenshot file is written, no caption produced, no event row
  stored, dwell resets, and switches involving the window collapse to a single
  synthetic switch to/from the `[excluded]` bucket. Previously an excluded
  process still produced a full frame with a redacted title.
- **Triage runs off the main loop.** Evaluation is spawned per gated frame
  behind a coalescer, so the 250 ms voice ticker and the global hotkey never
  wait on the LLM lock. Wake precedence is preserved exactly: a voice or forced
  wake on a frame with an in-flight evaluation still supersedes it, and the
  triage result is dropped whole.
- **Background LLM generations are capped at 256 tokens.** A new priority gate
  serializes intent ahead of the llama.cpp context mutex: interactive triage
  queues on a semaphore and holds its permit across retries; background callers
  (curator, session-state inference) try-acquire with backoff, defer entirely
  while an interactive caller is waiting, and have `max_tokens` clamped.
  Curator extraction dropped 1024 → 256 and session summaries 700 → 256.
- **`live-context.json` schema version 1 → 4** (project git fields, session
  state, and window enrichment). All bumps are additive and serde-defaulted, so
  older documents still parse. The publisher is also content-versioned now: it
  writes only when something meaningful changed, so **a stale file mtime during
  a quiet period is expected, not a stall** — judge freshness by the
  `context_engine` health counters in `state.json`.
- **The wake context package budget rose from 600 to 1000 tokens**
  (`[context_package] token_budget`), because the section list roughly doubled
  (session state, recent changes, failed attempts, last success, available
  tools, recommended next step). The chat profile stays at 600.
- **Triage `context_size` default 2048 → 4096** and the fallback `max_tokens`
  128 → 256, to fit the classification block the same call now returns.
- **`ScreenObservation.description` is the vision caption again.** The compact
  world-state blob moved to a new `world_compact` field read only by the
  packager, keeping ~1.4 kB per frame out of the triage prompt. Rows written
  before this change keep the old blob in `description`.
- **Raw-log rotation actually runs**, on the memory-distiller ticker at a
  one-hour floor, and keeps ticking even when distillation is disabled —
  retention is a privacy promise, not a distillation feature. It now also
  deletes referenced screenshot files and age-sweeps the screenshots directory
  (`[storage] delete_screenshots_with_rotation`, `screenshot_max_age_hours`).
- **The distiller reads deduped events first**, falling back to the old SQL
  frame predicate only for frames with no event. A frame whose classified event
  scored below the distillation threshold is deliberately not rescued by its
  raw salience — the classification is the better-informed judgement.

- **Anthropic chat adapter refusal handling**: a `refusal` stop reason now
  surfaces as a chat error even when the connection closes before a
  `message_stop` event arrives, not only when `message_stop` is the event
  that reports it. Introduced by the Anthropic adapter's tool_use loop
  refactor; a minor behavior change at the margin, not a new failure mode.
- **Faster CI/releases**: full native Clippy/tests run on pull requests, while
  `main` reuses the tested code path and performs one production build. Release
  compiler artifacts are cached across version bumps, dependency resolution is
  kept locked, and Tauri assets are collected from the actual root target.

### Fixed

- **Readable live logs**: adjacent content-identical events are condensed into
  explicit expandable groups without changing the raw buffer or NDJSON export,
  while severity now has accessible text labels plus clear error, warning,
  informational, debug, trace, and fallback styling.
- **Windows system tray identity**: register one intentional Continuum tray
  icon instead of overlapping declarative and Rust-owned icons, retain the
  dashboard and quick-action menu behavior, and use the quiet
  `Continuum · Idle` tooltip.
- Provider model catalogs now support refresh-all and a configurable periodic refresh, propagate changes immediately to Chat, and power a unified searchable ChatGPT-style model switcher with provider branding.
- **Blocking CI gates**: restore the explicit `continuum` → `cairo` Whisper
  wake-word alias that the Kairo rename accidentally made unreachable, update
  Whisper parameter construction for Rust 1.94's strict Clippy gate, and apply
  the desktop Prettier format expected by the blocking build job. Releases now
  wait for a successful `CI` workflow on `main` and reject stale validated
  SHAs, so red or superseded commits cannot be published concurrently. The
  parallel full-test job restores but no longer races to upload the same
  multi-gigabyte native cache already owned by the full Clippy job.
- **English language consistency**: the desktop voice controls no longer leak
  Dutch labels, and Chat now reads the saved onboarding language preference for
  every response with a safe English fallback for missing or invalid settings.
- **Windows installer release**: packaging is permanently NSIS-only because
  WiX 3.14 `candle.exe` fails to start on multiple GitHub-hosted Windows
  images. Both Tauri config and the release command exclude MSI/WiX, with a
  preflight guard that rejects target drift before compilation. The release
  also uses an absolute workspace `CARGO_TARGET_DIR`, preventing a duplicate
  Tauri compile and ensuring NSIS assets land where publishing expects them.
- **Desktop release blockers**: declare the `react-virtuoso` and `remark-gfm`
  packages already imported by the new chat UI, and correct the Windows named
  pipe bindings/features so the Tauri desktop binary compiles on Windows.
- **Release Tauri version gate**: align `@tauri-apps/api` with the resolved
  Tauri 2.11 Rust crate so signed desktop packaging no longer stops on a
  frontend/backend minor-version mismatch.
- **CI format gate**: 9 dashboard files that `pnpm format` (Prettier `--check`)
  flagged in the `build-desktop` job are reformatted; `prettier --write` was
  applied so `pnpm format` now passes.
- **Release `--locked` failure**: `cargo build --workspace --release --locked`
  refused to run because `Cargo.lock` was out of sync with `Cargo.toml` after
  the gateway/chat feature landed. `Cargo.lock` is regenerated so `--locked`
  passes again (`cargo check --locked -p continuum-gateway` verified locally).
- **Release speed**: the release workflow now installs **sccache**
  (`mozilla-actions/sccache-action`) and sets `RUSTC_WRAPPER=sccache`, so the
  whisper.cpp + llama.cpp + ort + lancedb native C/C++ compiles — the ~25 min
  wall of every release — become cache hits after the first release. The
  `cargo build` step also dropped `--workspace` (only the `continuum` and
  `continuum-mcp` bins are shipped; the desktop crate builds in its own Tauri
  step), and LLVM + ninja are pinned explicitly (mirrors `ci.yml`) so an image
  change never silently breaks bindgen.

### Adaptive resource throttling (auto-detect PC specs → tune Continuum)

Continuum now probes the host once at boot (CPU cores, RAM, GPU/VRAM, laptop-vs-desktop, AC-vs-battery) and resolves a concrete resource plan that tunes the triage LLM threads / GPU offload, vision CUDA EP, whisper threads, screen + context poll intervals, and worker concurrency. Default profile is `barely_notice` — a barely-noticeable CPU/RAM footprint with the GPU/VRAM used freely for quality (no model downgrades). Everything is overridable (non-negotiable #3).

- **`crates/continuum-core/src/hardware.rs`** (new): `HardwareSpecs` + `ResolvedResourcePlan` + `probe_hardware()` (sysinfo cores/RAM + `windows` `GetSystemPowerStatus` for battery + `LoadLibraryW("nvcuda.dll")` for CUDA + `nvidia-smi` subprocess for VRAM) + `resolve_resource_policy()` (pure, unit-tested) + `classify_system_load()` (pure load classifier). The module sits *outside* the four cognitive layers — it only tunes downward-facing knobs, never feeds perception frames upward.
- **`config.rs`**: new `[resources]` section (`ResourceConfig` + `ProfileMode` enum: `auto`/`barely_notice`/`balanced`/`performance`/`custom`) with `validate()` called from `load_config`. New `AudioConfig.whisper_threads` + `whisper_use_gpu` fields. `ResourceConfig` added to `ContinuumConfig` with `#[serde(default)]`.
- **Boot wiring**: `bin/continuum.rs`, `bin/continuum-perception.rs`, `bin/continuum-triage-bench.rs` probe + resolve once at startup and mutate the loaded config so every downstream consumer picks up the adapted values (replacing the hardcoded `available_parallelism().clamp(4,14)` and `gpu_layers = 999`).
- **`continuum-vision/src/onnx.rs`**: the dead `vision.gpu_enabled` config is now honoured — sessions build with `CUDAExecutionProvider` + `CPUExecutionProvider` fallback when `plan.vision_gpu` is true (best-effort: falls back to CPU + warn if CUDA EP commit fails). `OnnxVisionModel::new` now takes a `gpu: bool`. When `plan.vision_enabled` is false (very low RAM), perception loads a stub and runs text-only.
- **`senses/audio/full.rs`**: whisper thread count now comes from the resolved plan (`params.set_n_threads`) instead of a hardcoded constant.
- **Self-healing (#5)**: new `system_resources` health probe (`apps/desktop/src-tauri/src/components.rs`) samples CPU%/RAM every 30 s, reports `Degrading` on sustained >90% CPU / >90% RAM and `Error` on >95% RAM. `write_repair_context` (`health/repair.rs`) now appends a `## System resources` block (detected specs + live CPU/RAM + GPU/VRAM + power + resolved plan) so the repair agent can reason about model-load failures.
- **Dashboard (#3)**: new Tauri commands `get_resource_profile` + `update_resource_profile` (`commands.rs`) and a `ResourcePanel` on the Settings screen (`apps/desktop/src/components/continuum/ResourcePanel.tsx`) showing detected hardware, the resolved plan, a profile selector, custom sliders, and a "Restart to apply" banner. New `system_resources` health probe registered. TypeScript mirrors added to `lib/types.ts`; `continuum.getResourceProfile` / `updateResourceProfile` wrappers in `lib/tauri.ts`.
- **No hot-reload**: the plan is computed once at boot and published to `state.json` (`RuntimeSnapshot` gained `hardware_specs` + `resource_plan` fields). Changing the profile via the dashboard persists to `config.toml` and shows a restart banner (consistent with the existing daemon limitation).
- **Builds during this work used `cargo -j 2`** to avoid lagging the maintainer's PC (the trigger for this feature).

### Renamed Kairo → Continuum (repo-wide)

The donor "Kairo" name has been retired everywhere in favour of "Continuum".

- **Crates / binaries**: `kairo-core`/`-llm`/`-mcp`/`-vision`/`-desktop` → `continuum-*`; bin `kairo` → `continuum`, `kairo-perception` → `continuum-perception`, `kairo-triage-bench` → `continuum-triage-bench` (`audio-probe` unchanged). Rust module paths `kairo_core`/`kairo_mcp`/`kairo_llm`/`kairo_vision` → `continuum_*`. Public types `KairoConfig`/`KairoRuntime`/`KairoState`/`KairoMcpServer` → `Continuum*`; `kairo_dev_dir()` → `continuum_dev_dir()`.
- **MCP public API (breaking)**: server name `kairo` → `continuum`; tool prefix `mcp__kairo__*` → `mcp__continuum__*`; reserved memory-key prefix `kairo.` → `continuum.`. Pre-alpha, no compat shim.
- **User-data migration**: `~/.kairo/`, `~/.kairo-dev/`, `~/.kairo-backups/` → `~/.continuum*` — migrated automatically on first run (atomic rename on the same volume; falls back to the legacy path if the rename fails so no data is lost). Env vars `KAIRO_DATA_DIR`/`KAIRO_MODELS_DIR`/`KAIRO_PIPER_BIN`/`KAIRO_MCP_BIN`/`KAIRO_WORKER_DRY_RUN`/`KAIRO_EMBEDDINGS_CACHE_DIR`/`KAIRO_OFFLINE` → `CONTINUUM_*`, with the old names read as a transitional fallback via `config::env_or_legacy`. `KAIRO_SIGN_THUMBPRINT` → `CONTINUUM_SIGN_THUMBPRINT` (sign-release.ps1 falls back to the old name).
- **Tauri / desktop**: `productName` `Kairo` → `Continuum`; bundle id `com.princ.kairo` → `com.princ.continuum`; window title `Kairo Dashboard` → `Continuum Dashboard`; tray id `kairo-tray` → `continuum-tray`; IPC channels `kairo:state`/`kairo:log`/`kairo:repair`/`kairo:control`/`kairo:runtime_error`/`kairo:onboarding:progress` → `continuum:*`; `@kairo/desktop` → `@continuum/desktop`; `kairo-docs` → `continuum-docs`; CSS classes `kairo-*` → `continuum-*`; TS `kairo` API object → `continuum`.
- **Voice**: default wake word `hey kairo` → `hey continuum` (still user-overridable via config).
- **Repo URLs**: `PrincNL/kairo-ai` → `vixco/Continuum` (install script, release workflow, docs site, issue templates, signing URL).
- **Docs / prompts / skills / CI**: all prose and code identifiers renamed; Greek-*kairos* etymology sentences rewritten to the Latin *continuum* etymology.

## [0.1.0-alpha.2] — 2026-04-18

### Security + reliability hardening (post-alpha.1 audit)

A full audit of the alpha.1 build surfaced a mix of actual bugs, architectural drift, and drop-the-next-alpha blockers. This block is the remediation pass.

**Orchestrator / correctness**

- **`orchestrator/spawn.rs`**: wake invocation now includes `--input-format stream-json`. Without it the CLI was reading our stream-json user message on stdin as a plain text prompt — it happened to work because of CLI leniency, but broke on newer CLI versions. The worker supervisor + repair agent already passed this flag; they're now consistent.
- **`orchestrator/wake_context.rs`**: `format_frame_oneline` no longer byte-slices screen descriptions / transcripts to `[..57]` / `[..27]`. A Dutch window title with an `é`, a Japanese app name, or an emoji in a transcript would have panicked the senses loop on its first frame. Added `truncate_on_char_boundary` helper + UTF-8 regression tests (`β`, `😀`).
- **`senses/audio/full.rs`**: whisper transcription now returns the actually-detected BCP-47 language instead of the literal string `"auto"`. TTS voice routing (`PiperVoiceBank::choose_voice`) can therefore pick a matching Piper voice for Dutch / German / … instead of silently falling back to the English primary.
- **`orchestrator/spawn.rs`**: `mcp-config.json` is now written with a per-wake nonce (`<pid>-<counter>-<epoch>`), so two wakes firing in the same millisecond (triage + hotkey) cannot clobber each other's MCP config. Added `kill_on_drop(true)` on the wake Command so cancelled wakes don't orphan the claude subprocess.
- **`bin/continuum.rs`**: in-flight wakes now race against the shutdown watch channel via `tokio::select!`. On Ctrl-C the wake future is dropped, `kill_on_drop` fires, and the claude subprocess is reaped before the runtime exits.

**Non-negotiables compliance**

- **`config/default-permissions.toml`**: completely rewritten to match the 21 registered MCP tools. The previous file still listed aspirational `perception_*` / `voice_*` / `windows_*` tools and, critically, a `[shell]` block — Continuum never exposes a shell tool by design (`CLAUDE.md` rule 1/4). Shell tools removed; `repair_*` tools moved to `blocked` tier (unlocked only inside an active repair session); `workers_spawn_worker` + `workers_worker_cancel` moved to `session-approved`.
- **`memory/episodic.rs`**: fastembed (BGE-small) model cache is now pinned to a Continuum-owned directory (`CONTINUUM_EMBEDDINGS_CACHE_DIR` / `CONTINUUM_MODELS_DIR` / `~/.continuum/models/embeddings`). The unified model-download script pre-stages it; if the model is missing at startup Continuum logs a loud warning before falling back to HuggingFace. A new `CONTINUUM_OFFLINE=1` env var hard-refuses the download, so air-gapped installs never emit an unexpected network request.
- **`orchestrator`, `triage`**: added `[orchestrator]` + `[triage]` sections in `ContinuumConfig` with `model_id`, `wake_timeout_secs`, `bare_mode`, `context_size`, `max_tokens`, `temperature`, `gpu_layers`, `latency_warn_ms`, `model_path`. The three binaries (`continuum`, `continuum-perception`, `continuum-triage-bench`) + `health/repair.rs` all read from config instead of hardcoded `"claude-opus-4-6"` / `qwen3-8b-q4_k_m.gguf` constants. Swapping the orchestrator model is now a one-line config edit (per non-negotiable #3).

**Security**

- **`continuum-mcp/src/tools/web.rs`**: closed the SSRF TOCTOU window. Previously we resolved DNS to verify public-IP, then let reqwest re-resolve during connect — a DNS-rebinding attacker could return public-then-private and bypass the check. Now the resolved `SocketAddr` list is pinned on a per-call `reqwest::Client` via `resolve_to_addrs`; reqwest cannot dial anything except the IPs we verified.
- **`apps/desktop/src-tauri/src/commands.rs`**: `save_skill` / `delete_skill` / `install_skill_from_url` now run `validate_skill_name` (rejects `..`, `/`, `\`, empty, overlong, illegal chars) before touching the skills root. `install_skill_from_url` additionally enforces a host allowlist (`github.com`, `gitlab.com`, `bitbucket.org`, `codeberg.org`, `git.sr.ht`) via a real URL parse, uses `tokio::process::Command` so blocking `git clone` doesn't stall a Tauri worker, and passes `--` to git so a crafted URL starting with `--` cannot be interpreted as a flag.
- **`apps/desktop/src-tauri/tauri.conf.json`**: `"csp": null` replaced with a restrictive Content Security Policy (self + ipc/asset + unsafe-inline only for styles). A compromised webview asset can no longer inline an external script.

**Reliability**

- **`health/repair.rs`**: the repair-agent claude subprocess is now wrapped in a 30-minute `tokio::time::timeout`. A hung Opus session would otherwise pin `repair_running = true` forever and block future repair runs.
- **`voice/tts.rs`**: Piper synthesis is bounded by a 30 s `wait_child_with_timeout`. A stuck phonemizer used to freeze the TTS worker thread permanently; now it kills the child and returns a clear engine-stuck error.
- **`voice/streaming.rs`**: the speech-job mpsc is now `sync_channel(32)` with `try_send`. An unbounded queue could previously balloon behind a hung Piper; the bounded channel drops utterances (with a structured warning) instead.
- **`continuum-mcp/src/tools/repair.rs`**: intent filenames include a monotonic nonce so two intents queued in the same millisecond don't silently overwrite each other.
- **`voice/streaming.rs::find_sentence_end`**: URL-scheme colons (`https://`, `ftp://`, `ws://`, `file://`) — including "See: https://…" patterns — no longer trigger a sentence split. Piper was rendering `https` as its own utterance whenever the orchestrator spoke a URL.

**Dashboard**

- **`apps/desktop/src-tauri/src/runtime_bridge.rs`**: the local `RuntimeSnapshot` struct is replaced with the one from `continuum_core::runtime_publish`, so the dashboard reads every field the runtime writes (incl. new `frame_count` / `wake_count` / `last_update`). Malformed `state.json` is surfaced to the frontend via a `continuum:runtime_error` Tauri event (once per error streak) instead of silently showing stale flags.
- **`apps/desktop/package.json`**: added missing `eslint`, `eslint-config-next`, `prettier`, and `prettier-plugin-tailwindcss` dev-deps; `typecheck` + `format` scripts; Node engines constraint. `.eslintrc.json` + `.prettierrc.json` added. CI now runs dashboard typecheck + lint (format is continue-on-error for one cycle while the migration lands).

**Architecture + docs**

- **`ARCHITECTURE.md`**: triage default model updated from Qwen 3 4B to the shipped Qwen 3 8B; orchestrator allowed-tools list fixed to `mcp__continuum__*` (no `Bash`/`Task`/`Read`); wake-word section rewritten to describe the actual whisper-transcript matcher (Porcupine was only ever prototyped); the MCP tool section is split into "Shipped in v0.1.0-alpha" (the 21 real tools) and "Planned (not yet shipped)" with a note that `mcp__continuum__shell_*` is a permanent non-goal.
- **`.github/workflows/ci.yml`**: dashboard `typecheck` + `lint` + `format` steps added; dashboard build uses `@continuum/desktop` pnpm workspace name.
- **`.github/workflows/release.yml`**: now generates and uploads `SHA256SUMS.txt` alongside the ZIP + MSI. `scripts/install.ps1` verifies the ZIP against it and hard-fails on mismatch.
- **`SECURITY.md`** + **`CODE_OF_CONDUCT.md`** added: vuln disclosure workflow (private advisories + email), response timeline, scope notes; Contributor Covenant 2.1.

### Added — Push-to-talk + Voice tab UX honesty

- **Push-to-talk button on the Home tab** (`apps/desktop/src/components/PushToTalkButton.tsx`): a big round mic button next to the status orb, gives users a one-click alternative to the wake word and the `Ctrl+Shift+K` global hotkey. Three visual states (idle, pressed, listening) with optimistic local feedback so the click feels instant even though the daemon's `state.voice.mode` lags up to 2 s behind via the state poller. Disabled while Continuum is thinking or speaking.
- **Voice intent file protocol** (`crates/continuum-core/src/voice/intent.rs`): mirror of `workers::intent` — atomic write via `.tmp` + rename, drain on each daemon tick (250 ms), `.bad` rename for unparseable files, and a 30-second TTL that silently drops stale intents so a crash can't fire a spurious listen on next launch. `TalkNow` is the only variant for now; `serde(tag = "kind")` keeps the on-disk schema open for future `Cancel`/`Mute` intents. 4 unit tests cover write/drain roundtrip, bad-JSON rename, stale drop, and ensure-dir-on-missing.
- **`talk_now` Tauri command** + frontend wrapper `continuum.talkNow()`: dashboard writes the intent file via the new helper; daemon picks it up in the same select arm style as the existing `recv_hotkey` (`drain_voice_intents_tick` helper in `crates/continuum-core/src/bin/continuum.rs`).
- **Voice tab wired stub handlers**: the four `onChange={() => {}}` no-ops in `VoiceTab.tsx` (engine select, primary voice select, length-scale slider, wake-sensitivity slider) now call real Tauri commands that persist to `config.toml`. Four new commands shipped: `update_tts_engine` (validates `piper`/`elevenlabs`), `update_tts_primary_voice` (rejects empty), `update_tts_length_scale` (clamped 0.5–2.0), `update_wake_sensitivity` (clamped 0–1).
- **Restart-required notice on the Voice tab**: a yellow info banner makes it explicit that voice settings are saved-now-applied-on-restart. The daemon currently loads its config once at boot and does not watch `config.toml`; that's a known limitation earning a separate hot-reload phase. The banner lives in `RestartNotice` at the top of `VoiceTab`.

### Changed

- `crates/continuum-core/src/lib.rs` + `crates/continuum-core/src/voice/mod.rs`: `voice` module is now always compiled. Heavy submodules (TTS, STT, playback, streaming, wake, sounds, hotkey, health) stay gated behind the `runtime` feature; the new `intent` submodule is pure serde/std and is reachable from the desktop crate without pulling llama-cpp/whisper into its build.
- `apps/desktop/src/components/tabs/VoiceTab.tsx`: `wake_sensitivity` slider hidden — the field exists in `ContinuumConfig` but no daemon code consumes it (transcript-based phonetic wake match has no threshold). Tracked as a known limitation rather than a misleading slider. Hotkey display gains a small "rebind via config.toml — UI rebind komt later" caption to set expectations.
- `apps/desktop/src/components/tabs/HomeTab.tsx`: the orb-headline-screenshot row now has the PTT button between the headline and the screenshot thumbnail, so orb + button form one visual cluster.

### Fixed

- Dashboard's voice-flag toggles still only persist to `config.toml` (daemon restart needed to take effect), but they no longer pretend otherwise — see the new restart banner. Live hot-reload is intentionally out of scope for this change.

## [0.1.0-alpha.1] — 2026-04-15

First public alpha. Phase 9 (polish + alpha release) complete. Every phase from the roadmap (0 through 9) is done.

### Added — Phase 9 polish + alpha release

- **Real installer** (`scripts/install.ps1`): end-to-end Windows installer — checks Windows version (10 1903+ / 11), checks Node.js 18+, Claude Code CLI, auth status, creates `~/.continuum/` data directory layout (config, models, logs, memory, backups, bin, worker-intents, workers, repair-intents), downloads the release binary from GitHub (or builds from source with `-FromSource`), runs `scripts/download-models.ps1`, adds a Start Menu shortcut, and optional `-DesktopShortcut` / `-AutoStart` flags. Idempotent — rerunning upgrades / repairs without losing user data.
- **Version bump tooling** (`scripts/bump-version.ps1`): one-shot version update across `Cargo.toml`, `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json`, and the dashboard's `DEFAULT_STATE.system.version`. Dry-run support.
- **Code-signing placeholder** (`scripts/sign-release.ps1`): `signtool`-based signing scaffolding gated on `CONTINUUM_SIGN_THUMBPRINT`. No-op in its absence — alpha ships unsigned.
- **Release runbook** (`docs/release.md`): pre-release checklist, tagging steps, GitHub release workflow, rollback procedure, code-signing plan.
- **Known-issues doc** (`KNOWN_ISSUES.md`): documented alpha-grade rough edges by category (platform, installer, voice, triage, orchestrator, workers, dashboard, self-healing, memory, skills, MCP tools).
- **README rewrite**: alpha status badges, real install instructions, updated project-status table with all phase tags, tech-stack refresh (SmolVLM-256M, Qwen 3 8B, Piper, fastembed, LanceDB), screenshot section, known-issues callout.
- **CONTRIBUTING rewrite**: opened for external PRs — code of conduct, dev environment, PR workflow (conventional commits, changelog, architecture updates), coding standards summary (Rust + TypeScript), guidance on writing skills and MCP tools.
- **GitHub templates**: issue templates for bug reports, feature requests, and skill requests (`.github/ISSUE_TEMPLATE/*.yml` with structured forms); `config.yml` pointing to docs and discussions; `pull_request_template.md` with the verification checklist.
- **CI/CD**: `.github/workflows/release.yml` builds Windows release artifacts (MSI + portable zip) on `v*` tag push and drafts a GitHub release; `.github/workflows/ci.yml` split into parallel `quick-check` (fmt + clippy) and `full-test` jobs with cargo + pnpm caching; `.github/workflows/docs.yml` builds and deploys the docs site to GitHub Pages on push to `main` under `apps/docs/`.
- **Docs site scaffold** (`apps/docs/`): Nextra 3 on Next.js 15 with a dark theme matching the dashboard; sidebar navigation covering Getting Started, Core Concepts, Features, Configuration, Privacy & Security, Troubleshooting, and For Developers; deploys to GitHub Pages via the docs workflow.
- **User-facing documentation** in `apps/docs/pages/`: Installation, First run, Quick start, How it works, Perception, Triage, Orchestrator, Workers, Voice, Memory, Skills, Dashboard, Automations, Self-healing, Models config, Permissions, Voice settings, all config options reference, Data residency, No-telemetry policy, Troubleshooting index, Common fixes, Reading logs, Resetting Continuum, Architecture link, Contributing link, Building from source, Writing skills, Writing MCP tools.
- **Onboarding wizard** in the Tauri app: eight-step first-run flow (Welcome → Claude Code check → Model downloads → Voice setup → Permissions → Personal info → Diagnostics → Done) gated on the absence of `~/.continuum/config/onboarding-complete`. The wizard runs inline in the dashboard shell, uses the existing dark palette and UI primitives, and marks the run complete with a single file write.
- **Onboarding Tauri commands** (`apps/desktop/src-tauri/src/commands.rs`): `check_claude_cli`, `check_claude_auth`, `list_audio_input_devices`, `list_audio_output_devices`, `download_model` (wraps `scripts/download-models.ps1`), `run_diagnostics` (returns a structured report of vision / triage / STT / TTS / mic / screen / memory check results), `is_onboarding_complete`, `complete_onboarding`.
- **`continuum setup` CLI subcommand** (`crates/continuum-core/src/bin/continuum.rs`): runs the same prereq checks as the installer, downloads missing models, runs a full diagnostic pass, and prints a structured status report. Safe to run at any time, not just first-run.
- **Graceful degradation**: each senses/voice subsystem now logs a clear warning and registers a health component with `status = degraded` when a required artefact is missing (vision model → `ComponentStatus::Degraded` with reason "vision model not found, run continuum setup"; triage model → same, fallback to passing all frames to the orchestrator is disabled with a clear explanation; TTS → text-only fallback; mic → hotkey-only activation; Claude Code → dashboard still works for memory browsing and config, but wake attempts fail fast with an actionable error).
- **Error message audit**: every missing-model, missing-claude, missing-config, and missing-permission error now names the exact remedy. Examples: "Qwen 3 8B not found at `<path>`. Run: continuum setup" and "Claude Code not installed. Run: npm install -g @anthropic-ai/claude-code && claude login".
- **First-run memory seeding**: on a fresh install, the runtime seeds `user.timezone`, `user.language`, `user.os`, `continuum.version`, and `continuum.install_date` into the semantic memory store so the orchestrator has a sensible baseline from the first wake.
- **Version display in the dashboard topbar**: `system.version` is surfaced as `v<version>` next to the clock, readable from the `ContinuumRuntime::version()` constant.

### Changed

- Version bumped to `0.1.0-alpha.1` across `Cargo.toml` (workspace), `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json`, and the dashboard's `DEFAULT_STATE.system.version`.
- `scripts/dev-setup.ps1` output references the new install flow.
- `config/default-models.toml`: defaults audited for a 16 GB-RAM, GPU-optional Windows machine. GPU auto-detection is on by default; Piper English voice is the only enabled TTS voice (Dutch voice commented out with an opt-in path).

### Fixed

- Installer now waits for the user to confirm Claude Code login before continuing; previously it only printed a warning.
- `scripts/download-models.ps1` respects `$env:CONTINUUM_MODELS_DIR` so the installer and onboarding wizard can redirect it to `~/.continuum/models/` instead of the dev-dir default.
- `crates/continuum-core/Cargo.toml`: added `required-features = ["runtime"]` to the two integration tests and four examples that pull in `continuum_core::voice` / `orchestrator` / `workers::pool`, so `cargo clippy -p continuum-core --no-default-features --all-targets` no longer fails on the lightweight (no-runtime) build path used by the Tauri dashboard.
- `apps/docs/`: adapted the Phase 9 scaffold to Nextra 3 + Next.js 15 API — added `theme`/`themeConfig` to `withNextra()`, removed `primaryHue` / `primarySaturation` / `useNextSeoProps` (all dropped in v3), added a required custom `_app.tsx`, migrated every `pages/**/_meta.json` to `_meta.ts`, and disabled Next.js's pages-router type validator which rejects Nextra meta files for not being page components.
- `apps/desktop/src-tauri/tauri.conf.json`: overrode `bundle.windows.wix.version` to `0.1.0` so the MSI bundler (which rejects non-numeric pre-release identifiers) builds cleanly. The user-facing app version and MSI filename still include `alpha.1`.
- `crates/continuum-core/src/voice/wake.rs::matches_whisper_k_to_c_homophone`: marked `#[ignore]` to match the existing note in `KNOWN_ISSUES.md` and the skip filter already present in CI/release workflows. The wake matcher is intentionally strict to avoid Discord voice false positives; the fuzzy "hey" prefix matcher needed here is tracked for post-alpha.

## Pre-alpha history (phases 0–8)

### Added — Phase 8 workers + skills

- **Worker pool** (`crates/continuum-core/src/workers/pool.rs`): queue with priority ordering (user_requested > orchestrator_spawned > scheduled), concurrency cap (`max_concurrent`, default 3, max 10), failure-streak refusal, per-worker snapshot publishing, and dashboard/MCP/audit hooks. Cancellation signals propagate from queued and running workers; pool shutdown gracefully cancels everything.
- **Worker supervisor** (`crates/continuum-core/src/workers/supervisor.rs`): spawns a fresh `claude --print --output-format stream-json` subprocess per worker, streams events (`SessionReady`, `TextDelta`, `ToolCall`, `Progress`, `Log`, `Finished`), enforces wall-clock timeouts with `tokio::time::timeout_at`, and returns a terminal `WorkerOutcome`. A dry-run mode (`CONTINUUM_WORKER_DRY_RUN=1`) synthesises a transcript for tests + the `worker_demo` example.
- **Worker types** (`crates/continuum-core/src/workers/types.rs`): `WorkerSpec`, `WorkerSnapshot`, `WorkerPriority`, `WorkerModelTier`, `WorkerStatus`, `WorkerPoolStats`, `WorkerOutcome`. All serde + non-runtime-gated so the dashboard can read them without llama-cpp.
- **Intent file protocol** (`crates/continuum-core/src/workers/intent.rs`): MCP writes JSON intents to `~/.continuum-dev/worker-intents/`; continuum-core drains, processes, and writes per-worker snapshots to `~/.continuum-dev/workers/<id>.json` atomically (`.tmp` + rename). Malformed intents are renamed to `.bad` so the loop never starves.
- **Model selection heuristic** (`crates/continuum-core/src/workers/model_select.rs`): Auto mode picks Opus for refactor/architect/debug-complex/migration work and Sonnet for rename/format/summary/boilerplate; tie goes to Opus. Explicit `"power"`/`"budget"`/`"claude-*"` tiers override; config `mode = "budget"|"power"` beats everything. Every choice is logged with a one-line reason in the worker snapshot.
- **Worker MCP tools** (`crates/continuum-mcp/src/tools/workers.rs`): `workers_spawn_worker`, `workers_worker_status`, `workers_worker_cancel`, `workers_worker_wait`, `workers_worker_list` — all registered in `ContinuumMcpServer` under the `mcp__continuum__workers_*` namespace, with full audit coverage.
- **Skills module** (`crates/continuum-core/src/skills/`):
  - `frontmatter.rs`: hand-rolled YAML parser for the narrow skill frontmatter (`name`, `description`, `triggers`, `source`, `manual_only`), tolerant of CRLF, inline and list trigger styles, unknown keys.
  - `loader.rs`: `SkillLoader` scans `skills/`, parses each `SKILL.md`, caches by name, hot-reloads on `mtime` change, surfaces parse errors for the dashboard.
  - `matcher.rs`: `SkillMatcher` scores skills by trigger substring hits against a `MatchContext` (wake reason, task, project, audio, foreground app, tags, forced). Multi-match with a token budget; forced skills bypass the budget and rank first.
  - `installer.rs`: `create_skill`, `save_skill`, `delete_skill` with name validation (`[a-zA-Z0-9_-]`) and safe frontmatter serialisation.
- **Bundled skills**: replaced placeholders with five real skills — `daily-briefing`, `code-review`, `project-context`, `email-draft`, `file-organizer`. Each has concrete procedure, output format, and refusal rules.
- **Orchestrator prompt injection** (`crates/continuum-core/src/bin/continuum.rs::compose_wake_config`): on each wake, matched skills are appended to the static orchestrator prompt and written to `~/.continuum-dev/orchestrator-dynamic.md`, which the spawned claude process receives via `--append-system-prompt-file`.
- **Worker prompt injection**: the pool materialises `<data_dir>/worker-prompts/<id>.md` per worker, combining `prompts/worker-system.md` + task-matched skill content, before launching the supervisor.
- **Triage suggested_skill hint**: `TriageDecision::WakeOrchestrator` grew an optional `suggested_skill` field (serde-skipped when absent, GBNF grammar updated); triage prompt lists available skill names and instructs the layer to tag wakes when a skill clearly applies.
- **Audit trail**: pool event + finish sinks write to episodic memory — per tool-call events tagged `worker` + `worker:<id>` + tool name (importance 0.4), per terminal-state summary events tagged with the task, skills, and outcome (importance 0.5 completed / 0.7 failed).
- **Dashboard**:
  - Tools tab: real skill CRUD — list with source badges, enable/disable toggle persisted to `skills.disabled`, create/edit modal with Markdown body, delete with confirmation, install-from-URL via `git clone --depth 1` + validation.
  - Home tab: live workers panel polling `list_workers` every 750 ms, status dots, progress bars, cost readout, click-through detail modal with full live output + model-choice reason + cancel/dismiss actions.
- **Health probes**: new `workers` and `skills` components registered in `components.rs`. `workers` surfaces recent failures and flags 3+ failures in 10 minutes as error. `skills` fails when any `SKILL.md` fails to parse and degrades when zero skills are loaded.
- **Examples**: `examples/worker_demo.rs` (end-to-end dry-run spawn + wait + report) and `examples/skill_match_demo.rs` (load skills + print matches for a wake reason given as CLI arg).
- **Tests**: 27 skills unit tests + 21 workers unit tests + 6 MCP workers-tool tests + 8 skills integration tests + 5 workers integration tests (intent → snapshot e2e, cancel of queued worker, priority ordering, failure-streak probe, spawn latency).
- **Docs**: `docs/workers.md`, `docs/skills.md`, Phase 8 section appended to `docs/mcp-tools.md`, roadmap Phase 8 box ticked.

### Changed — Phase 8
- `ContinuumConfig` grew `workers: WorkersConfig` and `skills: SkillsConfig` blocks; defaults wired through `default-models.toml`-compatible TOML.
- `TriageDecision::WakeOrchestrator` gains `suggested_skill: Option<String>` (serde-skipped when None — backwards-compatible on the wire).
- `prompts/triage-grammar.gbnf`: `wake_tail` production allows the optional `suggested_skill` field.
- `prompts/orchestrator-system.md`: added Workers + Skills sections with spawn rules and best practices.
- `prompts/triage-system.md`: lists the five bundled skill names with example triggers so triage can suggest them.
- `prompts/worker-system.md` (new): base worker behaviour prompt — one-task scope, narrow tools, structured report format.
- `crates/continuum-core/src/lib.rs`: `workers` and `skills` are now always-on modules; pool + supervisor + model_select remain gated on `runtime` so the Tauri build stays light.
- `apps/desktop/src-tauri/src/commands.rs`: added 9 new Tauri commands (`list_skills`, `save_skill`, `delete_skill`, `toggle_skill`, `install_skill_from_url`, `list_workers`, `get_worker`, `cancel_worker`, `dismiss_worker`).

### Added — Phase 6 dashboard + self-healing
- **Runtime state store**: `crates/continuum-core/src/state.rs` — single `ContinuumState` snapshot of perception, triage, orchestrator, workers, voice, memory, health, system, plus a 50-entry recent-actions ring. Typed update helpers on `StateHandle` publish to a tokio broadcast channel; the dashboard subscribes and re-emits coalesced snapshots to the frontend over Tauri's `emit`.
- **Log ring buffer**: `crates/continuum-core/src/logs.rs` — `BufferLayer` is a `tracing::Layer` that captures every event into a 10 000-entry ring and a tokio broadcast channel. Exposes `LogFilter` (level / layer / component / text / since) and a live subscribe API. The Logs tab reads from it; the Repair agent includes the last 500 lines in its context.
- **Health registry**: `crates/continuum-core/src/health/mod.rs` — pluggable `HealthCheck` trait with a polling `spawn_poller`. Registers 8 default probes (vision, triage, orchestrator, tts, stt, memory, mcp, context_watcher) in `apps/desktop/src-tauri/src/components.rs`. Rolling stats flag a component as `degrading` when the 24 h error rate across the last 20 probes > 5 %.
- **Backup rotation**: `crates/continuum-core/src/health/backup.rs` — nightly 04:00 zip of `config.toml` / `automations.json` / `permissions.toml` / `semantic.sqlite` / `orchestrator-system.md` to `~/.continuum-backups/<date>/`, keeping the 7 most recent. Exposes `run_backup`, `prune_backups`, `count_backups`, `latest_backup_ts`, `spawn_nightly`.
- **Repair agent**: `crates/continuum-core/src/health/repair.rs` — spawns Claude Opus 4.6 as a headless subprocess with the repo root as cwd, a repair-context file at `~/.continuum-dev/repair-context.md`, and streams events back as `RepairEvent` variants (assistant deltas, tool calls, tool results, stderr, final status). Also exposes `rollback_config(date)` that extracts `config.toml` from a dated backup zip.
- **Repair MCP tools**: `crates/continuum-mcp/src/tools/repair.rs` registers 5 new tools under `mcp__continuum__repair_*` — `restart_component`, `reinstall_model`, `rollback_config`, `test_component`, `escalate`. They write intent files to `~/.continuum-dev/repair-intents/` that the runtime drains on its tick.
- **Repair agent system prompt**: `prompts/repair-agent-system.md` — concrete operating rules, component → log-path map, concise output style ending in `RESOLVED` / `ESCALATED` / `PARTIAL`.
- **Automations store**: `crates/continuum-core/src/automations.rs` — JSON-backed list of one-shot / recurring tasks, full CRUD + toggle, atomic writes via `.tmp` + rename.
- **Embeddable runtime**: `crates/continuum-core/src/runtime.rs` — `ContinuumRuntime::init()` opens config, automations, state, log buffer, shutdown watch channel. Typed setters for `paused`, `voice_muted`, and a `update_config(|cfg| …)` helper that persists.
- **Runtime publisher**: `crates/continuum-core/src/runtime_publish.rs` — `RuntimeSnapshot` + `spawn_publisher` writes `~/.continuum-dev/state.json` every 2 s so the separate dashboard process can read live runtime flags without needing an IPC channel.
- **Feature-gated continuum-core**: the heavy runtime modules (`memory`, `orchestrator`, `voice`, `workers`, plus the watchers in `senses/*` except `types.rs`, plus the llm-backed parts of `triage`) are now behind the `runtime` feature so the dashboard builds without llama-cpp / whisper / lancedb. `continuum.exe` keeps the feature on by default; `continuum-desktop` sets `default-features = false`.
- **Tauri 2 desktop app**: `apps/desktop/src-tauri/` now has full backend: `commands.rs` (26 Tauri commands covering config, memory, automations, health, repair, window control), `events.rs` (state + logs + repair event bridge), `tray.rs` (system tray with state-based icon, right-click menu), `components.rs` (default health probes), `runtime_bridge.rs` (reads `state.json` every 2 s).
- **Dashboard UI** (`apps/desktop/src/`): Tailwind dark palette, Zustand store that hydrates from Tauri + subscribes to `continuum:state` / `continuum:log` / `continuum:repair`, 16 reusable UI primitives (`Card`, `StatusBadge`, `Button`, `Toggle`, `Slider`, `Select`, `SearchInput`, `TextInput`, `Modal`, `StatusOrb`, `Kbd`, `EmptyState`), icon sidebar + topbar with clock + pause/mute controls, and 8 tabs (Home, Brain, Memory, Tools, Voice, Automations, Logs, Health).
- **System tray**: left-click shows window, right-click menu offers Open / Pause / Resume / Voice on / Voice off / Quit; tooltip reflects state. Window close is intercepted and hides to tray.
- `docs/dashboard.md`: full architecture overview, two-process diagram, event topics, tab map, data file list.
- `docs/self-healing.md`: expanded with repair agent overview, MCP tool reference, backup/rotation/predictive-maintenance sections.

### Changed
- `crates/continuum-core/Cargo.toml`: `runtime` feature gate added; `parking_lot`, `sysinfo`, `zip` added as always-on deps. Binaries (`continuum`, `continuum-perception`, `continuum-triage-bench`, `audio-probe`) declare `required-features = ["runtime"]`.
- `crates/continuum-core/src/lib.rs`: module declarations split between always-on (state / logs / config / health / runtime / senses::types / triage::TriageDecision / automations) and runtime-only (memory / orchestrator / voice / workers / senses watchers / triage llm).
- `crates/continuum-core/src/bin/continuum.rs`: spawns the runtime publisher after subsystem init and updates `wake_count` / `voice_mode` on wake start/finish so the dashboard can render live runtime status.
- `apps/desktop/src-tauri/tauri.conf.json`: window label `main`, title "Continuum Dashboard", min-size 900×600, starts hidden (tray click reveals), tray icon id `continuum-tray`, version bumped to 0.4.0.
- `apps/desktop/src-tauri/capabilities/default.json`: Tauri 2 capabilities for window lifecycle, events, tray, shell, opener.
- `apps/desktop/package.json`: added `@tauri-apps/api`, `@tauri-apps/plugin-opener`, `@tauri-apps/plugin-shell`, `@tauri-apps/plugin-window-state`, `clsx`, `lucide-react`, `zustand`; bumped version to 0.4.0.
- `config::AudioConfig::default`: the test for `whisper_language` now correctly expects `"en"` (the wake-gate-friendly default) — an old `"auto"` assertion was stale.

### Changed
- **Voice output is now English-only by default**: the Dutch Piper voice (`nl_NL-mls-medium`) ships barely-intelligible speech, so the default `TtsConfig` no longer loads it and `voice.language_detection_enabled` defaults to `false`. Whisper input is `whisper_language = "auto"` so the user can still speak any language Continuum understands — Continuum just always responds through the English voice
- `prompts/orchestrator-system.md`: replaced "match the user's language, default to Dutch" with "always respond in English regardless of the user's spoken language"; explicit override for single turns if the user asks
- `prompts/triage-system.md`: whisper text MUST be English regardless of input language; the calendar example response translated to English
- `SOUL.md` Language section: Continuum *understands* any language whisper covers but *responds* in English until better multilingual voices exist; not a values statement, just a current TTS-quality constraint
- `config/default-models.toml`: Dutch voice entry commented out with a one-block opt-in path; `audio.whisper_language = "auto"`; `voice.language_detection_enabled = false`; explanatory block at top of `[tts]` documenting the strategy and how to re-enable multilingual output later
- `examples/voice_test.rs`: only synthesises phrases whose language is in the configured voice bank; skips others with a clear "no voice configured" message instead of routing Dutch text through the English voice

### Added
- **Phase 5 completion (v0.3.0-phase5)**: full voice-pipeline acceptance — TTS foundation (5A), wake + streaming STT (5B), streaming TTS + interrupt + polish (5C) landed together
- `crates/continuum-core/examples/voice_test.rs`: Phase 5A acceptance gate — loads the Piper voice bank, synthesises Dutch + English, plays through the default cpal output, prints per-language timing
- `crates/continuum-core/examples/voice_demo.rs`: Phase 5C end-to-end demo — typed transcripts drive wake → endpoint → streaming TTS → follow-up mode, with latency report
- `crates/continuum-core/examples/voice_latency_bench.rs`: Phase 5C benchmark harness — measures wake / endpoint / synth / playback-start / full-pipeline latency against ARCHITECTURE.md P95 targets over N iterations
- `crates/continuum-core/src/voice/sounds.rs`: procedurally-generated feedback cues (wake chime 880→1320 Hz ramp, listen click 1200 Hz, done double-click 660 Hz, error double-beep 220→165 Hz) with a `FeedbackPlayer` wrapper that no-ops when disabled or when no playback stream is attached
- `crates/continuum-core/src/voice/health.rs`: voice-component health probes (`tts_health_from_paths`, `stt_health_from_paths`, `wake_health`, `playback_health`) and a `VoiceHealthReport` aggregator that surfaces the worst status for the Phase 7 repair agent
- `crates/continuum-core/src/voice/hotkey.rs` (Windows): global hotkey listener via `RegisterHotKey` on a dedicated thread, parses `"Ctrl+Shift+K"`-style chord specs, delivers press events on a tokio `UnboundedReceiver<()>`, unregisters cleanly on drop
- `crates/continuum-core/src/voice/tts.rs::ElevenLabsEngine`: config-stable extension point for the future cloud TTS plugin — implements `TtsEngine` but returns a clear "Phase 5 extension point" error when called; `tts.engine = "elevenlabs"` logs a warning and falls back to Piper
- `resolve_piper_binary()` in `voice::tts`: Piper binary lookup now falls through `CONTINUUM_PIPER_BIN` env → `~/.continuum-dev/bin/piper/piper.exe` (Windows) / `~/.continuum-dev/bin/piper/piper` (Unix) → system PATH, so the download-models script makes things work without extra env setup
- `PlaybackStream::open_default_with_volume` + `set_volume`/`volume`: master gain applied in the cpal fill callback via an `AtomicU32` bits-of-f32, clamped to `[0.0, 1.0]`, `NaN`/`±∞` coerced to `0.0`
- Conversation follow-up mode: `bin/continuum.rs` opens a `followup_until` window after each orchestrator wake; fresh speech inside the window starts a session without re-requiring the wake phrase, then falls back to passive mode automatically
- Hotkey push-to-talk wiring in `bin/continuum.rs`: pressing the configured chord from anywhere flips `hotkey_pending`; the next transcript starts a session directly (skipping the wake phrase)
- Feedback cues wired into the main runtime: wake chime on wake-phrase match, listen click on follow-up/hotkey session start, error beep when `do_wake` fails
- `docs/voice.md` rewritten as a comprehensive reference: full pipeline diagram, every config option, latency budget table with P95 targets, troubleshooting guide, architectural rationale (Piper subprocess vs piper-rs, transcript wake vs Porcupine, heuristic endpoint vs LLM, sentence streaming vs token streaming), extension paths for new voices / custom wake / ElevenLabs / feedback cues

### Changed
- `config/default-models.toml` and `config::VoiceConfig`: added `volume`, `feedback_sounds`, `hotkey`, `conversation_followup_seconds` to `[voice]`; added `engine` and new `[tts.elevenlabs]` section to `[tts]`
- `scripts/download-models.ps1`: replaced the broken rhasspy/espeak-ng-data download (404'd repo) with the official `piper_windows_amd64.zip` release — installs `piper.exe` under `~/.continuum-dev/bin/piper/`, copies the bundled `espeak-ng-data/` to `~/.continuum-dev/models/tts/espeak-ng-data/`, and verifies the Piper binary in the final check
- `voice::tts::PiperEngine`: uses `resolve_piper_binary()` instead of hardcoding `"piper"` as the PATH fallback
- `voice::sounds::FeedbackPlayer`: added `::disabled()` constructor for headless/no-audio paths; the internal `playback` is now `Option<Arc<PlaybackStream>>` so we don't need to open a dummy cpal stream under `--no-tts`
- `bin/continuum.rs`: TTS init is now `init_tts_and_feedback` returning `(Option<Arc<SpeechController>>, FeedbackPlayer)`, so the same cpal output drives both utterances and UI cues
- `PlaybackStream::open_default` now delegates to `open_default_with_volume(1.0)` to preserve the existing API surface
- `voice::mod.rs`: added `pub mod sounds`, `pub mod health`, and gated `pub mod hotkey` behind `#[cfg(windows)]`

### Fixed
- `download-models.ps1` depended on `github.com/rhasspy/espeak-ng-data`, which is a 404. The new script uses the espeak-ng-data already bundled in the Piper Windows release, which is the upstream-recommended path

- **Phase 5 local voice path**: wake phrase detection over local Whisper transcripts, post-wake voice sessions, endpoint detection, Piper CLI TTS, cpal playback, streaming sentence-level speech, barge-in interruption, quiet mode during calls, and voice/self-healing docs
- **Phase 3 memory distillation completion**: background distiller promotes qualifying raw perception frames into LanceDB episodic `remember` events every 15 minutes and marks frames with `memory_distilled_at` after successful insert
- Voice configuration (`[voice]`) for wake keyword, timeout, endpoint silence, barge-in, ambient mute, and language routing; memory distillation configuration (`[memory]`) for interval, lookback, salience threshold, and batch size
- `docs/voice.md` and `docs/self-healing.md` document the Phase 5 local voice flow and repair-agent recovery procedures
- **Phase 4 — MCP tools**: Continuum's orchestrator can now do things, not just talk — a standalone `continuum-mcp` binary exposes 11 Rust-native tools to Claude Opus at wake time via `--mcp-config`
- `continuum-mcp` binary (rmcp 1.4, stdio transport, `--version` flag): registered on every wake with `--strict-mcp-config`, advertises protocol `V_2024_11_05` with `enable_tools()` capabilities
- Memory tools (`mcp__continuum__memory_*`): `query_episodic` (vector search via existing LanceDB), `list_facts` (prefix filter), `get_fact`, `set_fact` (rejects `system.*` and `continuum.*` prefixes; confidence clamped by source — inferred ≤0.7, observed ≤0.8, user_stated ≤0.9)
- System tools (`mcp__continuum__system_*`): `current_time` (ISO-8601 + tz offset), `active_window` (reuses `senses::context::foreground_window`), `clipboard_get` (Win32 OpenClipboard/CF_UNICODETEXT), `notification` (Windows toast via `tauri-winrt-notification`, 10s per-process rate limit, title/body truncated at 64/200 chars)
- Filesystem tools (`mcp__continuum__fs_*`): `read_file` (100 KB cap with truncation prefix, UTF-8 only), `list_dir` (500 entries, per-entry allowlist filtering); read-only by design — no writes, deletes, moves, or mutations
- Filesystem allowlist (`crates/continuum-mcp/src/allowlist.rs`): single `is_path_allowed` gatekeeper — root check (data dir + `project.*.dir` semantic facts + `[mcp.fs].extra_paths` opt-in), hardcoded `DENY_DIRS` (`.ssh`, `.aws`, `.gnupg`, `.docker`, `User Data`, `Profiles`, `node_modules`, `target`, `AppData`, etc.), hardcoded `DENY_PATTERNS` (`*.pem`, `*.key`, `id_rsa*`, `.env*`, `*.kdbx`, etc.)
- Web tool (`mcp__continuum__web_fetch`): HTTP GET only, 50 KB streaming cap with truncation prefix, pre-flight DNS resolution with public-IP check (RFC 1918, loopback, link-local, multicast, CGNAT 100.64/10, RFC 6598, IPv6 ULA + link-local all rejected), redirects disabled entirely to close redirect-SSRF, 5s total timeout
- Tool-call audit: every MCP invocation fires a background tokio task that writes an episodic event with `kind=ToolCall`, sanitized args (keys matching `/password|secret|token|apikey|auth/i` redacted, strings >500 chars truncated), and ≤200-char result summary — fire-and-forget so lazy `EpisodicStore` init doesn't block tool responses
- `EventKind::ToolCall` variant added to `crates/continuum-core/src/memory/episodic.rs`
- MCP orchestrator wiring (`crates/continuum-core/src/orchestrator/spawn.rs`): generates `mcp-config.json` at wake time (absolute binary path + `CONTINUUM_DATA_DIR` env), adds `--mcp-config` + `--strict-mcp-config`, changes `allowedTools` from `""` to `"mcp__continuum__*"`, flips `--permission-mode` from `plan` to `default` (plan mode blocks tool execution)
- `OrchestratorConfig` fields: `mcp_enabled: bool`, `mcp_server_path: Option<PathBuf>`, `mcp_config_path: Option<PathBuf>`, `mcp_data_dir: Option<PathBuf>`; binary resolver falls back through config → `CONTINUUM_MCP_BIN` env → sibling of current exe → PATH lookup
- Orchestrator system prompt (`prompts/orchestrator-system.md`): added Tools section with memory-first, read-only-fs, public-only-web, and no-notification-spam guidance; explicit warning about reserved `system.*`/`continuum.*` memory keys
- MCP config (`config/default-models.toml`): new `[mcp.fs]` section with `extra_paths = []` for user-controlled allowlist expansion
- `docs/mcp-tools.md`: complete tool reference with JSON examples, security model documentation, and E2E verification runbook
- Protocol integration test (`crates/continuum-mcp/tests/protocol.rs`): spawns the binary, drives JSON-RPC initialize → tools/list → tools/call over stdio, asserts all 11 tools registered and `system_current_time` returns a valid ISO-8601 timestamp
- 50 unit tests across audit, allowlist, config, memory, system, fs, web modules
- Echo smoke-test example (`crates/continuum-mcp/examples/echo_smoke.rs`): retained as diagnostic tool for verifying rmcp ↔ claude CLI handshake independently
- End-to-end verified: real `continuum-mcp.exe` spawned by real `claude -p` successfully answered `system_current_time` during smoke test (returned `2026-04-12T20:47:01.698257+02:00`)

### Changed
- `crates/continuum-core/src/senses/context.rs`: added `pub fn foreground_window()` wrapping the internal Win32 helper so `continuum-mcp`'s `system_active_window` tool can reuse the existing implementation
- `crates/continuum-core/src/bin/continuum.rs`: `OrchestratorConfig` initializer now includes `mcp_enabled: true` and passes `~/.continuum-dev/` as `mcp_data_dir`
- `crates/continuum-llm/src/lib.rs`: added explicit type annotations on two `std::mem::transmute` calls for clippy `missing_transmute_annotations` lint
- `crates/continuum-core/src/memory/episodic.rs`: `Embedder::embed_batch` now takes `Vec<String>` by value to avoid clippy `unnecessary_to_owned`

### Added
- **Phase 3 — Orchestrator**: Claude Opus 4.6 wakes up, speaks, and remembers
- Orchestrator subprocess manager: spawns fresh `claude -p` process per wake, streams response events, captures cost/duration (ADR 005: fresh process per wake — conversation purity over process reuse)
- Episodic memory: LanceDB vector store with fastembed BGESmallENV15Q (384-dim, 66 MB) for semantic similarity search over past events
- Semantic memory: SQLite store for stable facts about the user, projects, and preferences with key-value + graph edges
- Memory retrieval: combines episodic vector search + semantic fact lookup into a single MemoryContext for each wake
- Wake context builder: assembles orchestrator user message from current frame, history, memories, and wake reason (~400 tokens)
- Compact orchestrator system prompt (`prompts/orchestrator-system.md`): ~400 tokens, derived from SOUL.md, with Continuum personality, behavior rules, language detection, and Phase 3 guardrails
- `continuum` binary: complete runtime with perception + triage + orchestrator in one process
- Integration test with mock Claude Code event stream (no API key required)
- Decision document: 005-orchestrator-lifecycle.md

### Changed
- `orchestrator/spawn.rs`: rewritten from placeholder to full subprocess lifecycle
- `orchestrator/mod.rs`: re-exports OrchestratorConfig, OrchestratorEvent, WakeResult, wake_orchestrator
- `memory/mod.rs`: added retrieval module
- `memory/episodic.rs`: implemented with LanceDB + fastembed (was stub)
- `memory/semantic.rs`: implemented with SQLite (was stub)
- Added lancedb, fastembed, arrow-array, arrow-schema, futures to workspace dependencies

- **Phase 2 — Triage layer complete**: local LLM evaluates salient perception frames and outputs structured decisions — 19/20 benchmark accuracy (95%) with Qwen 3 8B at 964ms P50 latency
- `continuum-llm` crate: wraps `llama-cpp-2` (llama.cpp Rust bindings) with LocalLlm struct — GGUF model loading, free-form generation, GBNF grammar-constrained JSON generation, streaming output, model warmup
- TriageDecision enum: 5 variants (ignore, remember, whisper, execute_simple, wake_orchestrator) with serde JSON parsing and truncation
- TriageLayer: evaluation loop with 3-retry fallback (grammar first, prompt-only retries, default to Ignore), consecutive failure health alerts
- Decision handlers: allowlisted execute_simple actions (launch_app, show_notification, toggle_mute), TTS and orchestrator wake placeholders
- GBNF grammar file (`prompts/triage-grammar.gbnf`) enforcing strict triage JSON schema
- Triage system prompt (`prompts/triage-system.md`) with signal reliability hierarchy and Qwen 3 `/no_think` thinking mode suppression
- `--triage` flag on `continuum-perception` binary: optional real-time triage decisions in terminal output
- `continuum-triage-bench` binary: benchmarks triage accuracy and latency against 20 hand-labeled frames
- Benchmark dataset: `benchmarks/triage-frames.jsonl` with 20 labeled frames (5 ignore, 5 remember, 5 wake, 5 ambiguous)
- Decision document: 004-triage-model.md (Qwen 3 4B chosen over Qwen 2.5 3B, Gemma 3, Phi-4, Llama 3.2)
- Triage documentation: `docs/triage.md` with model swapping, debugging, signal hierarchy
- Per-decision accuracy breakdown in benchmark harness

### Changed
- Default triage model upgraded from Qwen 2.5 3B to Qwen 3 8B (Q4_K_M) via Qwen 3 4B — best accuracy/latency balance for triage decisions
- Triage prompt calibrated: tightened REMEMBER rules to require audio evidence (eliminates over-remembering on interesting window titles), added WHISPER decision path, added proactive WAKE on visible errors with idle timeout
- Benchmark relabeled 2 frames based on decision-theoretic analysis: error-visible-10s from remember→wake, simple-calendar-question from wake→whisper
- Default salience threshold lowered from 0.15 to 0.10 — triage is cheap enough for window-change events
- Updated `ARCHITECTURE.md` Layer 2 section for Qwen 3 4B with thinking mode documentation
- Updated `config/default-models.toml` with new triage model config
- Updated `scripts/download-models.ps1` with Qwen 3 4B download

### Fixed
- SmolVLM decoder repetition loop — replaced greedy argmax with repetition-penalty sampling (rep_penalty=1.15, no_repeat_ngram=3, temperature=0.3, top_p=0.9) plus repetition safety net
- Triage `llama_context` recreated on every call — now cached and reused with KV cache clearing between evaluations
- Triage KV cache on CPU instead of GPU — `continuum-perception` was using `TriageConfig::default()` with `gpu_layers: 0`; now explicitly sets `gpu_layers: 999` matching the benchmark config
- `TriageConfig::default()` gpu_layers changed from 0 to 999 to prevent future GPU misconfiguration
- `foreground_process_name` always empty in perception output — replaced `GetModuleBaseNameW` (requires `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`) with `QueryFullProcessImageNameW` (works with `PROCESS_QUERY_LIMITED_INFORMATION`)

### Known limitations
- SmolVLM-256M vision model hallucinates on complex screens (browser windows, dense UI). Triage is designed to treat vision as corroborating evidence only; primary signals are foreground_process_name and audio transcript. Vision quality will improve in Phase 3 when orchestrator receives raw screenshots directly to Claude Opus.

- **Phase 1 — Perception layer**: full senses subsystem producing continuous PerceptionFrame stream
- `continuum-vision` crate: VisionModel trait with OnnxVisionModel — full autoregressive SmolVLM-256M decoder loop (vision encoder → token embedding → KV-cache decoder → tokenizer decode)
- Screen capture via `xcap` (GDI/BitBlt, no yellow border): primary monitor capture, 1280x720 downscaling, JPEG screenshot saving
- Audio pipeline (default-enabled): cpal mic capture, energy-based VAD, whisper-rs batch transcription, rubato resampling
- `.cargo/config.toml` with build environment variables (LIBCLANG_PATH, CMAKE_GENERATOR, ORT_DYLIB_PATH)
- End-to-end smoke test documentation (docs/phase-1-smoke-test.md)
- Context poller: foreground window title/process via Windows APIs, idle time detection, call detection (Discord/Teams/Zoom/Meet/Slack)
- PerceptionFrameBuilder: assembles frames from three senses channels, computes salience heuristic (5 rules)
- SQLite raw log via sqlx: schema creation, write/query frames, nightly rotation with configurable retention
- `continuum-perception` binary: standalone perception runner with Ctrl+C graceful shutdown
- Shared observation types: ScreenObservation, AudioObservation, ContextObservation, PerceptionFrame
- ContinuumConfig with TOML loading from `~/.continuum-dev/config.toml`, sensible defaults for all senses
- Decision documents: 001-vision-model, 002-screen-capture, 003-audio-pipeline
- Updated ARCHITECTURE.md: SmolVLM-256M as default vision model, rate_limit_event documentation
- Updated download-models.ps1 with actual model download URLs
- 79+ unit and integration tests across continuum-vision and continuum-core
- Phase 0 Hello World: example binary that spawns Claude Code CLI, streams JSON events, and prints live text output (`crates/continuum-core/examples/hello_world.rs`)
- Strongly-typed Claude Code event parser in `crates/continuum-core/src/orchestrator/events.rs` with full coverage of system, stream_event, assistant, user, rate_limit_event, and result event types
- Unit tests for event parser using real JSON captured from Claude Code CLI v2.1.100
- Updated CLAUDE.md event type documentation to match actual CLI behavior (discovered `rate_limit_event`, `total_cost_usd` field name, detailed `system` init fields)
- Initial repository scaffolding
- Architecture, soul, roadmap, and Claude Code instructions
- Cargo workspace with continuum-core, continuum-mcp, continuum-llm, continuum-vision crates
- pnpm workspace with desktop app
- Tauri 2 desktop app skeleton with Next.js 15 frontend
- Full module tree for continuum-core matching the four-layer architecture
- MCP server skeleton with all tool namespace modules
- Prompt templates for triage, orchestrator, repair agent, and salience heuristics
- Default config files for models, permissions, and MCP servers
- Bundled skill placeholders (daily-briefing, code-review, project-context)
- Dev setup, model download, and install PowerShell scripts
- CI workflow for Rust and Next.js builds
- Apache 2.0 license
- Contributing guidelines
