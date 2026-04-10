# Phase 1 Smoke Test — 2026-04-10

5-minute end-to-end test of `kairo-perception` with real vision, audio, and context.

## Environment

- **OS**: Windows 11 Pro 10.0.26200
- **CPU**: Desktop (i7-class)
- **RAM**: 16 GB
- **Monitor**: PL2470H 1920x1080
- **Audio**: SteelSeries Sonar - Microphone (virtual audio device, 48kHz stereo)
- **Vision model**: SmolVLM-256M-Instruct q4 (64MB encoder, 109MB embed, 83MB decoder)
- **Whisper model**: ggml-small (465 MB)
- **ONNX Runtime**: 1.23.0 (CPU, no GPU)

## Test run

```
cargo run --bin kairo-perception
# ran for 5 minutes via timeout 300
```

## Results

### Vision (screen capture + SmolVLM description)

- **Status**: Working
- **Capture**: Primary monitor (PL2470H, 1920x1080) captured every ~6.5 seconds
- **Descriptions generated**: Real VLM output — examples:
  - `"The screen shows a search bar and a search icon."`
  - `"The screen shows a search result for a search query."`
  - `"The screen shows a dark background with a few glowing dots in the top left corner."`
- **Inference time**: ~4-6 seconds per frame (encoder ~1.5s, decoder loop ~2-4s)
- **Frame interval**: Effective ~6.5s (exceeds configured 3s due to CPU inference time)
- **Quality note**: 256M q4 model produces repetitive, low-detail descriptions. This is expected for the model size. Descriptions recognize UI elements (search bars, backgrounds) but lack specificity. Upgrading to the full-precision model or a larger model will improve quality.
- **Screenshots**: 49 JPEGs saved to `~/.kairo-dev/screenshots/2026-04-10/`, ~50 KB each

### Audio (microphone + VAD + whisper)

- **Status**: Partially working
- **Microphone**: Detected and streaming (SteelSeries Sonar, 48kHz stereo → resampled to 16kHz mono)
- **Whisper model**: Loaded successfully (ggml-small, 465 MB)
- **VAD**: Running, no speech segments detected during test (no one spoke into the mic)
- **Transcription**: Not triggered (expected — VAD correctly detected silence throughout)

### Context (Windows API polling)

- **Status**: Working
- **Poll interval**: 1 second
- **Foreground window**: Detected (process name populated via Windows API)
- **Idle time**: Tracked via GetLastInputInfo
- **Call detection**: No calls active during test

### Frame assembly

- **Status**: Working
- **Interval**: ~3 seconds (frame builder ticks at 3s regardless of vision cycle time)
- **Salience**: First frame = 0.5, subsequent identical frames = 0.0
- **Salience change detection**: Working (new window focus triggers salience increase)

### Raw log (SQLite)

- **Status**: Working
- **Database**: `~/.kairo-dev/raw_log.sqlite` (16 KB after 5 minutes)
- **Frames stored**: All frames written successfully with timestamps, descriptions, and screenshot paths

### Performance

- **CPU usage**: ~15-25% of one core (spikes during vision inference)
- **RAM**: ~400 MB (ONNX Runtime + models loaded)
- **Stability**: Ran for full 5 minutes without crash. Exit via SIGTERM (timeout) was clean.

## Known limitations

1. **Vision inference exceeds 3-second interval on CPU.** The q4 model takes ~4-6s per frame. This means effective capture rate is ~6.5s, not 3s. Acceptable for Phase 1; GPU acceleration or a smaller model will fix this in later phases.

2. **Description quality is limited.** SmolVLM-256M with q4 quantization produces generic descriptions ("search bar", "dark background") rather than specific ones ("VS Code editing main.rs"). This is a model capability limitation, not a pipeline issue. The full-precision model or Moondream 2B (for GPU users) will produce better results.

3. **Audio transcription not verified with live speech.** The test environment's virtual audio device detected no speech. The full pipeline (VAD → whisper) needs manual verification with a physical microphone.

## Conclusion

The Phase 1 perception layer is functional end-to-end:
- Screen capture → vision model → description: **working**
- Microphone → VAD → whisper → transcript: **pipeline complete, needs speech to verify**
- Windows context polling: **working**
- Frame assembly with salience: **working**
- SQLite raw log: **working**
- Screenshot storage: **working**
- Graceful shutdown: **working**

Phase 2 (triage) can begin. The triage LLM will receive real perception frames with actual screen descriptions and context data.
