# Senses Layer Guide

Layer 1 of Kairo's cognitive architecture. Captures screen, audio, and Windows context continuously, producing `PerceptionFrame` objects that flow upward to the triage layer.

## Quick Start

```bash
# Without audio (no LLVM required):
cargo run --bin kairo-perception

# With audio (requires LLVM/libclang):
cargo run --bin kairo-perception --features audio
```

The binary loads config from `~/.kairo-dev/config.toml`. If no config file exists, defaults are used. Press Ctrl+C to stop.

## Architecture Overview

```
VisionWatcher ──> ScreenObservation ──┐
AudioWatcher  ──> AudioObservation  ──┤──> PerceptionFrameBuilder ──> PerceptionFrame ──> RawLog
ContextWatcher -> ContextObservation ─┘
```

Each watcher runs as an independent tokio task. The frame builder combines the latest observation from each source at a fixed interval, computes a salience score, and emits a unified `PerceptionFrame`. Frames are written to the SQLite raw log.

## The Three Watchers

### Vision Watcher

Captures the primary monitor using `xcap` (GDI backend, no yellow border), downscales to the configured resolution, and runs the local vision model to produce a one-sentence description.

- **Crate:** `kairo-core::senses::vision`
- **Model:** SmolVLM-256M via `ort` (ONNX Runtime). Falls back to a stub model if model files are missing.
- **Interval:** Every `screen.interval_secs` seconds (default 3).
- **Screenshots:** Saved to `~/.kairo-dev/screenshots/<YYYY-MM-DD>/<HH-MM-SS>.jpg` when `screen.save_screenshots = true`.

### Audio Watcher

Captures microphone audio via `cpal` (WASAPI), detects speech with energy-based VAD, resamples with `rubato`, and transcribes via `whisper-rs`.

- **Crate:** `kairo-core::senses::audio`
- **Model:** Whisper small (244M params). Path configurable.
- **Feature gate:** Requires `--features audio` at compile time (needs LLVM for whisper-rs bindgen).
- **Without feature:** A stub watcher parks until shutdown, producing no observations.
- **Segment cap:** 8 seconds max, forced split.

### Context Watcher

Polls Windows APIs once per second. No AI, no models -- pure structured data.

- **Crate:** `kairo-core::senses::context`
- **Data:** Foreground window title, process name, idle time, in-call detection.
- **Call detection:** Checks for Discord, Teams, Zoom, Slack processes or browser tabs with "meet"/"zoom" in the title.
- **Platform:** Windows-only via `#[cfg(windows)]`. Non-Windows gets stub observations.

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

All settings are in `~/.kairo-dev/config.toml`. Missing keys fall back to defaults. Every value is overridable via the dashboard (when built).

```toml
[vision]
name = "SmolVLM-256M"
model_path = "~/.kairo-dev/models/vision/smolvlm-256m"
gpu_enabled = false
input_width = 384
input_height = 384

[screen]
interval_secs = 3          # Capture interval (1-10)
capture_width = 1280        # Downscale width
capture_height = 720        # Downscale height
save_screenshots = true     # Save JPEGs to disk

[audio]
enabled = true
whisper_model_path = "~/.kairo-dev/models/stt/whisper-small.bin"
vad_threshold = 0.5         # RMS energy threshold (0.0-1.0)
silence_duration_ms = 500   # Silence before segment ends
max_segment_secs = 8        # Forced split at this length

[context]
poll_interval_secs = 1      # Windows API poll rate

[frame]
interval_secs = 3           # Frame assembly interval (2-10)
salience_threshold = 0.10   # Minimum salience to reach triage (0.0-1.0)

[storage]
db_path = "~/.kairo-dev/raw_log.sqlite"
screenshots_dir = "~/.kairo-dev/screenshots"
retention_days = 30          # Frames older than this are rotated (1-365)
```

## Required Models

Download models before first run:

```powershell
.\scripts\download-models.ps1
```

This places:

| Model | Path | Size |
|---|---|---|
| SmolVLM-256M (ONNX) | `~/.kairo-dev/models/vision/smolvlm-256m/` | ~500 MB |
| Whisper small | `~/.kairo-dev/models/stt/whisper-small.bin` | ~466 MB |

Without models, the vision watcher falls back to a stub that returns `"(no vision model loaded)"`. The audio watcher requires its model to function (or disable audio via config).

## Development Directory

All runtime data lives in `~/.kairo-dev/` during development:

```
~/.kairo-dev/
  config.toml              # Runtime configuration
  raw_log.sqlite           # Perception frame database
  screenshots/             # Saved screenshot JPEGs
    2026-04-10/
      14-30-00.jpg
      14-30-03.jpg
  models/
    vision/smolvlm-256m/   # ONNX model files
    stt/whisper-small.bin   # Whisper model
```

This path is in `.gitignore`. Never committed.

## Raw Log

SQLite database at `~/.kairo-dev/raw_log.sqlite`. One row per `PerceptionFrame`.

- **Retention:** 30 days by default, configurable via `storage.retention_days`.
- **Rotation:** Nightly. Frames older than the retention period are deleted.
- **Screenshots:** Stored as JPEG files on disk. The database stores file paths, not blobs.
- **Browsing:** Open with any SQLite browser (DB Browser for SQLite, DBeaver, `sqlite3` CLI).

## Debugging

Set `RUST_LOG` for verbose output:

```bash
# Default (info for most, debug for kairo crates):
cargo run --bin kairo-perception

# Full debug output:
RUST_LOG=debug cargo run --bin kairo-perception

# Trace-level for frame builder only:
RUST_LOG=info,kairo_core::senses::frame=trace cargo run --bin kairo-perception
```

Each log line includes `layer=senses` and `component=<name>` fields for filtering.

To inspect stored frames, open `~/.kairo-dev/raw_log.sqlite`:

```sql
SELECT id, ts, salience_hint, screen_description, audio_transcript
FROM perception_frames
ORDER BY ts DESC
LIMIT 20;
```
