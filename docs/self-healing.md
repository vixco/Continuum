# Self-Healing Runbook

This document records component health and recovery hooks that the repair agent can use.

## Repair agent overview

The dashboard's **Fix Issues** button spawns a dedicated Claude Opus 4.6 session with:

- Working directory: the Continuum install folder.
- System prompt: [`prompts/repair-agent-system.md`](../prompts/repair-agent-system.md).
- Input context: written to `~/.continuum-dev/repair-context.md` before the spawn and sent inline after secret fields are redacted. It contains the user-reported problem (if any), component statuses, the live config snapshot, and the last 500 log lines.
- A dedicated Continuum MCP server restricted to component tests. Built-in Claude tools, project/user settings, slash commands, and session persistence are disabled. Manual next steps are reported directly in the streamed assistant output.

Output streams live to the Health tab via `continuum:repair` events (text deltas, tool calls, tool results, stderr, final status).

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
