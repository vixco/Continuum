# ADR-003: Audio Pipeline Architecture

**Status:** Accepted
**Date:** 2026-04-10
**Layer:** 1 (Senses)
**Crate:** `kairo-core` (senses::audio)

## Context

Kairo continuously captures microphone audio, detects when the user is speaking, and transcribes speech segments. The pipeline must:

- Run locally with no cloud API calls.
- Detect voice activity to avoid transcribing silence.
- Transcribe speech with reasonable accuracy in English and Dutch.
- Stay within a per-segment latency budget of ~2 seconds.
- Be optional at compile time (not every developer has LLVM installed).

## Components

### 1. cpal 0.17 -- Audio Capture

WASAPI-based audio capture on Windows. Reads the default input device at its native sample rate (typically 48kHz). Runs in a dedicated thread via cpal's callback model, pushing audio chunks into a ring buffer.

### 2. Energy-Based VAD -- Voice Activity Detection

A simple RMS (root mean square) energy threshold detector. When the audio energy exceeds `vad_threshold` (default 0.5), the segment is marked as speech. Speech ends after `silence_duration_ms` (default 500ms) of sub-threshold energy.

### 3. rubato 2.0 -- Resampling

Whisper expects 16kHz mono audio. Most microphones capture at 48kHz. Rubato handles the 48kHz-to-16kHz resampling with high-quality sinc interpolation.

### 4. whisper-rs 0.16 -- Transcription

Rust bindings to whisper.cpp. Uses the `whisper-small` model (244M parameters) by default. Transcribes completed speech segments in batch mode (not streaming).

## Why Energy VAD Over Silero

The `voice_activity_detector` crate (Silero VAD) is more accurate than energy-based detection, but it uses `ort` (ONNX Runtime) internally. Kairo already pins a specific `ort` version for the vision model in `kairo-vision`. Having two crates depend on potentially different `ort` versions causes version conflicts and linker errors.

Energy-based VAD is simpler, has zero external dependencies, and is reliable enough for Phase 1 where the primary use case is detecting clear speech directed at the assistant. Upgrading to Silero VAD is planned for a later phase once the `ort` version situation is resolved.

## Feature Gate

whisper-rs uses `bindgen` to generate Rust bindings for whisper.cpp, which requires LLVM/libclang to be installed. Not every development machine has this.

The entire audio pipeline is gated behind the `audio` Cargo feature:

- **With feature:** `crates/kairo-core/src/senses/audio/full.rs` provides the real `AudioWatcher`.
- **Without feature:** `crates/kairo-core/src/senses/audio/stub.rs` provides a stub `AudioWatcher` that parks until shutdown, producing no observations.

Build with the audio feature:

```bash
cargo build -p kairo-core --features audio
cargo run --bin kairo-perception --features audio
```

## Performance

| Metric | Value |
|---|---|
| Whisper model | small (244M params) |
| Transcription latency | ~800ms-1.5s for a 5-second segment on CPU |
| Max segment length | 8 seconds (forced split via `max_segment_secs`) |
| Resampling overhead | <10ms per segment |
| VAD overhead | Negligible (simple arithmetic) |

## Language Support

Language detection is set to auto via `set_language(Some("auto"))`. Whisper's small model handles English and Dutch well. The detected language code (e.g., "en", "nl") is included in each `AudioObservation`.

## Configuration

All audio settings are in `[audio]` in `config.toml`:

```toml
[audio]
enabled = true
whisper_model_path = "~/.kairo-dev/models/stt/whisper-small.bin"
vad_threshold = 0.5
silence_duration_ms = 500
max_segment_secs = 8
```

## References

- `crates/kairo-core/src/senses/audio/mod.rs` -- Feature gate and module structure
- `crates/kairo-core/src/senses/audio/full.rs` -- Full audio pipeline (behind `audio` feature)
- `crates/kairo-core/src/senses/audio/stub.rs` -- Stub when audio feature is disabled
- `crates/kairo-core/src/senses/types.rs` -- `AudioObservation` struct
- `crates/kairo-core/src/config.rs` -- `AudioConfig` with defaults
