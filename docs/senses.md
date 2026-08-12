# Senses Layer Guide

Layer 1 of Continuum's cognitive architecture. Captures screen, audio, and Windows context continuously, producing `PerceptionFrame` objects that flow upward to the triage layer.

## Quick Start

```bash
# Without audio (no LLVM required):
cargo run --bin continuum-perception

# With audio (requires LLVM/libclang):
cargo run --bin continuum-perception --features audio
```

The binary loads config from `~/.continuum-dev/config.toml`. If no config file exists, defaults are used. Press Ctrl+C to stop.

## Architecture Overview

```
Monitor workers ──> OrderedBuffer ──> change-selected local vision ──┐
ContextWatcher ──────────────────────────────────────────────────────┼──> LiveContextHub
                                                                    │        │
AudioWatcher ──> AudioObservation ──────────────────────────────────┼────────┼──> PerceptionFrameBuilder
                                                                    │        └──> live-context.json / MCP
                                                                    └──> ScreenObservation
```

Each connected monitor has an independent capture task. Capture continuity is
decoupled from local vision inference by a bounded ordered buffer, so slow
inference cannot silently pause sensing. The frame builder still combines the
latest observations at a fixed interval and writes `PerceptionFrame` rows to
the SQLite raw log. The separate live-context projection gives every agent role
the same compact current world-state without exposing raw screenshots.

## Watchers

### Vision Watcher

Captures all connected monitors concurrently. `xcap` supplies enumeration and
stable `display-<xcap id>` identities; on Windows, direct GDI `StretchBlt`
captures the scaled hot-path samples and model keyframes without a yellow
border. A failed direct capture automatically falls back to `xcap`. Each
display also carries geometry, a per-monitor capture sequence, and a timestamp.

- **Crate:** `continuum-core::senses::vision`
- **Model:** SmolVLM2-2.2B Q4_K_M through llama.cpp MTMD by default. The
  official SmolVLM-500M ONNX pipeline is loaded automatically if the preferred
  model or its warmup fails; SmolVLM-256M remains a configurable low-resource
  option. If both configured models fail, vision fails closed to the stub.
- **Capture cadence:** `screen.capture_interval_ms` (default 20 ms per monitor,
  a best-effort 50-captures/second target). This loop is mechanical; it does not
  call AI. GDI and display timing can miss that deadline, which is counted in
  live-context health rather than presented as a false 50/s guarantee.
- **Backpressure:** `screen.buffer_capacity` pending captures; oldest pending
  keyframes are dropped under load and reported in health/event counters.
  Unchanged samples never occupy this queue or the semantic event ring.
- **Change selection:** a 64×36 luma signature gates local vision using
  `screen.meaningful_change_threshold`. It compares against the last selected
  keyframe, allowing gradual changes to accumulate. Inference is independently
  scheduled by `screen.vision_min_interval_ms`; the 20 ms default removes an
  artificial pause but actual throughput remains bounded by model latency.
- **Context fusion:** the model describes the visible main content. Foreground
  application and window-title identity come from `ContextWatcher`, so
  Continuum can combine both signals without forcing the vision model to guess
  an application from weak visual evidence.
- **Screenshots:** disabled by default. When explicitly enabled, only selected
  changed frames are saved under a per-monitor local directory.
- **Privacy:** sensitive foreground applications or titles are redacted before
  local vision; startup fails closed until context classification is available.

### Audio Watcher

Captures microphone audio via `cpal` (WASAPI), detects speech with energy-based VAD, resamples with `rubato`, and transcribes via `whisper-rs`.

- **Crate:** `continuum-core::senses::audio`
- **Model:** Whisper small (244M params). Path configurable.
- **Feature gate:** Requires `--features audio` at compile time (needs LLVM for whisper-rs bindgen).
- **Without feature:** A stub watcher parks until shutdown, producing no observations.
- **Segment cap:** 8 seconds max, forced split.

### Context Watcher

Polls Windows APIs once per second. No AI, no models -- pure structured data.

- **Crate:** `continuum-core::senses::context`
- **Data:** privacy-filtered foreground title/process, idle time, in-call
  detection, current Git project/root/head, and terminal-process presence.
- **Input boundary:** only coarse idle/active state is recorded. Key values,
  pointer coordinates, click targets, clipboard contents, and terminal text are
  never collected by live context.
- **Call detection:** Checks for Discord, Teams, Zoom, Slack processes or browser tabs with "meet"/"zoom" in the title.
- **Platform:** Windows-only via `#[cfg(windows)]`. Non-Windows gets stub observations.

### Background Process Watcher (opt-in)

Samples the process table and publishes only configured developer/model
lifecycles and sustained CPU or memory pressure. It writes a bounded current
snapshot to `~/.continuum-dev/processes.json` and change events to the deduped
context-event log. It never reads command lines, environment variables,
process memory, or hidden-window contents. Enable with
`[process_watcher].enabled = true`; it is off by default.

## Frame Builder

Combines observations into `PerceptionFrame` objects. Holds the latest observation from each watcher and emits a frame every `frame.interval_secs` seconds (default 3).

**Salience heuristic** (0.0 to 1.0):

| Condition | Score |
|---|---|
| First frame ever | 0.5 |
| Identical to previous frame | 0.0 |
| New error visible on screen | +0.3 |
| User spoke (non-empty transcript) | +0.4 |
| New window focused | +0.2 |
| Error disappeared | +0.1 |

Only frames above `frame.salience_threshold` (default 0.10) are forwarded to the triage layer. All frames are written to the raw log regardless of salience.

## Configuration

All settings are in `~/.continuum-dev/config.toml`. Missing keys fall back to defaults. Every value is overridable via the dashboard (when built).

```toml
[vision]
name = "SmolVLM2-2.2B Q4_K_M"
backend = "auto"
model_path = "~/.continuum-dev/models/vision/smolvlm2-2.2b-q4"
fallback_model_path = "~/.continuum-dev/models/vision/smolvlm-500m"
gpu_enabled = true             # Safe CPU fallback without a GPU-enabled build
input_width = 384
input_height = 384
prompt = "Describe the visible computer screen accurately. Include the application or scene, the main action or status, important readable text, and any visible error. Use one concise factual sentence."
max_new_tokens = 64
processor_max_edge = 1536
image_splitting = true

[screen]
enabled = true              # Visible user consent boundary
capture_interval_ms = 20    # Per-monitor best-effort target cadence (50/s)
capture_width = 1280        # Downscale width
capture_height = 720        # Downscale height
save_screenshots = false    # Explicit opt-in for selected local JPEGs
all_monitors = true          # Capture every connected monitor
excluded_monitor_ids = []    # Optional stable display IDs to exclude
buffer_capacity = 64         # Oldest-drop pending capture FIFO
meaningful_change_threshold = 0.025
vision_min_interval_ms = 20 # Continuously consume changed keyframes at model throughput

[audio]
enabled = true
whisper_model_path = "~/.continuum-dev/models/stt/whisper-small.bin"
vad_threshold = 0.5         # RMS energy threshold (0.0-1.0)
silence_duration_ms = 500   # Silence before segment ends
max_segment_secs = 8        # Forced split at this length

[context]
poll_interval_secs = 1      # Windows API poll rate
redact_sensitive_titles = true
sensitive_process_names = ["1password.exe", "bitwarden.exe", "keepass.exe", "keepassxc.exe", "credentialuibroker.exe"]
sensitive_title_keywords = ["password", "passkey", "two-factor", "2fa", "private key", "seed phrase"]
terminal_process_names = ["windowsterminal.exe", "powershell.exe", "pwsh.exe", "cmd.exe", "bash.exe", "wsl.exe"]

[frame]
interval_secs = 3           # Frame assembly interval (2-10)
salience_threshold = 0.10   # Minimum salience to reach triage (0.0-1.0)

[storage]
db_path = "~/.continuum-dev/raw_log.sqlite"
screenshots_dir = "~/.continuum-dev/screenshots"
retention_days = 30          # Frames older than this are rotated (1-365)
delete_screenshots_with_rotation = true   # Delete JPEGs with their rows + run the mtime sweep
screenshot_max_age_hours = 720            # mtime backstop for orphaned JPEGs (0 = off)
```

## Required Models

Download models before first run:

```powershell
.\scripts\download-models.ps1
```

This places:

| Model | Path | Size |
|---|---|---|
| SmolVLM2-2.2B Q4_K_M + projector | `~/.continuum-dev/models/vision/smolvlm2-2.2b-q4/` | ~1.9 GB |
| SmolVLM-500M fallback (ONNX) | `~/.continuum-dev/models/vision/smolvlm-500m/` | ~2.0 GB |
| SmolVLM-256M fallback (ONNX) | `~/.continuum-dev/models/vision/smolvlm-256m/` | ~1.0 GB |
| Whisper small | `~/.continuum-dev/models/stt/whisper-small.bin` | ~466 MB |

Without models, the vision watcher falls back to a stub that returns `"(no vision model loaded)"`. The audio watcher requires its model to function (or disable audio via config).

## Development Directory

All runtime data lives in `~/.continuum-dev/` during development:

```
~/.continuum-dev/
  config.toml              # Runtime configuration
  raw_log.sqlite           # Perception frame database
  live-context.json        # Compact versioned shared current world-state
  screenshots/             # Saved screenshot JPEGs
    2026-04-10/
      14-30-00.jpg
      14-30-03.jpg
  models/
    vision/smolvlm2-2.2b-q4/ # preferred GGUF model + projector
    vision/smolvlm-500m/     # automatic ONNX fallback
    stt/whisper-small.bin   # Whisper model
```

This path is in `.gitignore`. Never committed.

## Raw Log

SQLite database at `~/.continuum-dev/raw_log.sqlite`. One row per `PerceptionFrame`.

- **Retention:** 30 days by default, configurable via `storage.retention_days`.
- **Rotation:** Hourly, on the memory-distiller ticker (it runs even when
  distillation is disabled — retention is a privacy promise, not a
  distillation feature). Frames older than the retention period are deleted.
- **Screenshots:** Stored as JPEG files on disk. The database stores file
  paths, not blobs. Rotation deletes the file each deleted row referenced,
  and then sweeps `screenshots_dir` for any image whose mtime is older than
  `storage.screenshot_max_age_hours` (default 720 h = the 30-day retention).
  That backstop is what reclaims images orphaned by a crash between "file
  written" and "row written"; it is mtime-only and never consults the
  database, so keep `screenshot_max_age_hours >= retention_days * 24`.
  Setting `storage.delete_screenshots_with_rotation = false` disables both
  deletions (rows still rotate) and leaves the image cache entirely to you.
- **Browsing:** Open with any SQLite browser (DB Browser for SQLite, DBeaver, `sqlite3` CLI).

## Debugging

Set `RUST_LOG` for verbose output:

```bash
# Default (info for most, debug for continuum crates):
cargo run --bin continuum-perception

# Full debug output:
RUST_LOG=debug cargo run --bin continuum-perception

# Trace-level for frame builder only:
RUST_LOG=info,continuum_core::senses::frame=trace cargo run --bin continuum-perception
```

Each log line includes `layer=senses` and `component=<name>` fields for filtering.

To inspect stored frames, open `~/.continuum-dev/raw_log.sqlite`:

```sql
SELECT id, ts, salience_hint, screen_description, audio_transcript
FROM perception_frames
ORDER BY ts DESC
LIMIT 20;
```

`screen_description` is the one-sentence vision caption. The compact
multi-monitor world-state text lives in the separate `screen_world_compact`
column (context engine spec §4.10) — it is the context packager's input and is
never sent to the triage model. Frames written before that split still carry
the old combined blob in `screen_description`.
