# Self-Healing Runbook

This document records component health and recovery hooks that the repair agent can use.

## Repair agent overview

The dashboard's **Fix Issues** button spawns a dedicated Claude Opus 4.6 session with:

- Working directory: the Continuum install folder.
- System prompt: [`prompts/repair-agent-system.md`](../prompts/repair-agent-system.md).
- Input context: written to `~/.continuum-dev/repair-context.md` before the spawn, containing the user-reported problem (if any), component statuses, the live config snapshot, and the last 500 log lines.
- Access to the Continuum MCP server plus the repair-specific tools listed below.

Output streams live to the Health tab via `continuum:repair` events (text deltas, tool calls, tool results, stderr, final status).

## Repair MCP tools

All under the `repair_*` namespace (routed via `continuum-mcp`):

| Tool                       | Effect                                                                 |
|----------------------------|------------------------------------------------------------------------|
| `repair_restart_component` | Queues a restart intent for the running runtime in `~/.continuum-dev/repair-intents/`. Targets: vision, triage, audio, stt, tts, orchestrator, mcp, memory, context_watcher. |
| `repair_reinstall_model`   | Queues a model-reinstall intent (non-destructive — the runtime re-downloads the file and restarts the subsystem). |
| `repair_rollback_config`   | Restores `config.toml` from a dated backup under `~/.continuum-backups/`. Destructive: requires confirmation. |
| `repair_test_component`    | Lightweight file-presence probe. Returns `healthy / degrading / error / unknown`. Complement to `repair_restart_component` — test first, restart if needed. |
| `repair_escalate`          | Posts a dashboard banner asking the user to take manual action.        |

Intent files have the shape `{kind, queued_at, body}` — the runtime polls `repair-intents/` every 2 s and moves consumed intents to `.done` siblings.

## Backup rotation

Every night at 04:00 local time (configurable via `health::backup::spawn_nightly`), the dashboard zips these files into `~/.continuum-backups/<YYYY-MM-DD>/continuum-<date>.zip`:

- `config.toml`
- `automations.json`
- `permissions.toml`
- `semantic.sqlite`
- `orchestrator-system.md`

Deliberately excluded: raw log (`raw_log.sqlite`), episodic LanceDB folder, screenshots, model weights, logs.

Rotation retains the most recent 7 backups; older dated folders are removed. The Health tab's **Backup now** button triggers an immediate rotation.

## Predictive maintenance

The health registry runs every 30 s. A component is marked `degrading` when:

- It currently probes `healthy` but the 24 h error rate across the last 20 probes is > 5 %, OR
- Its file-presence probe reports the subsystem never signalled "loaded" (seen as a boot-time "awaiting start" state).

Components marked `degrading` show amber in the Health tab. Predictive auto-repair (automatically firing the repair agent on a degradation trend) is off by default.

## Voice-activated repair

The triage layer recognises spoken phrases like "Continuum, something is broken" and "Continuum, check your health" as `execute_simple` decisions that forward to the repair subsystem. Implementation detail: the voice command maps to a `trigger_repair` Tauri command with the spoken text as the user reason, so the repair context surfaces what the user said.

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
