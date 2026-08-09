# Voice pipeline

Phase 5 is Continuum's full bidirectional voice loop. It is designed to be
local-first, low-latency, interruptible, and to feel like a presence
rather than a tool. This document explains the pipeline end to end, every
configurable knob, and how to diagnose things when they go wrong.

## Pipeline diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│  LAYER 1 — senses::audio (always on)                                 │
│  mic capture → adaptive VAD → whisper.cpp transcription              │
│                                                                      │
│  Language auto-detected per segment so the user can speak any        │
│  language whisper understands. Segments under ~300 ms of trailing    │
│  silence stay glued together.                                        │
└─────────────────┬────────────────────────────────────────────────────┘
                  │ AudioObservation { transcript, language }
                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│  voice::wake::TranscriptWakeDetector                                 │
│  Scans each transcript for "hey continuum" (configurable). Extracts the  │
│  text after the wake phrase as the initial utterance.                │
│                                                                      │
│  No access-key dependency, no extra model. Runs on the same whisper  │
│  output the triage layer consumes.                                   │
└─────────────────┬────────────────────────────────────────────────────┘
                  │ on wake match → FeedbackCue::Wake chime
                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│  voice::stt::VoiceSession + SemanticEndpointDetector                 │
│  Accumulates post-wake transcripts. Closes the session when:         │
│   • silence exceeds endpoint_silence_ms                              │
│   • the utterance "looks complete" (ends with .!? or matches a short │
│     imperative like "stop"/"cancel"/"bedankt")                       │
│   • listen_timeout_ms expires                                        │
│                                                                      │
│  Deliberately heuristic — a second LLM call on the hot path would    │
│  blow the latency budget. See ROADMAP Phase 5.                       │
└─────────────────┬────────────────────────────────────────────────────┘
                  │ WakeOrchestrator { reason: "Voice command (nl): …" }
                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│  LAYER 2 — triage::llm (Qwen 3 8B on GPU)                            │
│  The voice command is wrapped as a high-salience perception frame    │
│  and handed to the existing triage layer. Triage decides between     │
│  whisper (short local answer) and wake_orchestrator (Opus).          │
└─────────────────┬────────────────────────────────────────────────────┘
                  │
          ┌───────┴───────┐
          ▼               ▼
  triage whisper     orchestrator wake
          │               │
          │               │ stream_event → TextDelta
          ▼               ▼
┌──────────────────────────────────────────────────────────────────────┐
│  voice::streaming::SpeechController                                  │
│  Sentence chunker: buffers text until . ? ! : ; or \n\n arrives,     │
│  then hands the sentence to a dedicated synthesis worker thread.     │
│                                                                      │
│  Generation counter: interrupt() bumps it. Jobs holding a stale      │
│  generation are dropped before Piper is called, so barge-in takes    │
│  effect within one synth worker poll.                                │
└─────────────────┬────────────────────────────────────────────────────┘
                  │ SynthesizedAudio { samples, sample_rate }
                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│  voice::tts::PiperVoiceBank → voice::tts::PiperEngine                │
│  Piper invoked as a subprocess via stdin/stdout (UTF-8 in, 16-bit    │
│  PCM out). Keeps ONNX Runtime out of the Rust dep graph, which       │
│  would otherwise conflict with continuum-vision's `ort` build.           │
│                                                                      │
│  Binary resolution: CONTINUUM_PIPER_BIN → ~/.continuum-dev/bin/piper/ →      │
│  system PATH.                                                        │
└─────────────────┬────────────────────────────────────────────────────┘
                  │ Vec<f32>
                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│  voice::playback::PlaybackStream (cpal)                              │
│  Linear-interpolation resample Piper 22050 Hz → device rate          │
│  (commonly 48000 on Windows WASAPI). Mono is fanned out to stereo    │
│  or more by simple channel replication. Master volume is applied in  │
│  the cpal callback.                                                  │
└──────────────────────────────────────────────────────────────────────┘
```

On top of the pipeline:

- **Barge-in** — while TTS is playing, fresh speech from the user arrives
  through the senses audio watcher. `update_voice_session` calls
  `SpeechController::interrupt()` before processing the new input, which
  clears the sentence buffer, bumps the generation counter, and drops
  queued PCM within 50 ms.

- **Ambient mute** — when the context watcher reports an active call
  (Discord, Teams, Zoom, Meet), orchestrator responses and whisper
  decisions are logged instead of spoken. Continuum goes silent; the user
  stays in control of their call.

- **Multilingual input, English output** — whisper transcribes any
  language (`audio.whisper_language = "auto"`) so the user can speak
  Dutch, English, German, or anything else whisper covers. Continuum
  *understands* the transcript in its native language and *responds*
  through the English Piper voice. This is a conscious 2026-04 choice:
  the Dutch Piper voice (`nl_NL-mls-medium`) produces barely-intelligible
  speech, so shipping English-only hurts the user less than bad Dutch
  TTS would. When better voices land, flip
  `voice.language_detection_enabled = true` and add a
  `[tts.voices.<lang>]` section per language.

- **Conversation follow-up** — after `do_wake` completes, Continuum sets a
  `followup_until` deadline. Speech within that window starts a new voice
  session without re-triggering the wake word, so a back-and-forth
  conversation feels natural.

- **Global hotkey** — `Ctrl+Shift+K` (configurable) toggles listening
  from anywhere on Windows via `RegisterHotKey`. Pressing it skips the
  wake phrase on the next transcript, which is the push-to-talk story.

- **Feedback cues** — procedurally-generated sine-wave cues announce
  state transitions: wake chime (880 → 1320 Hz), listen click (1200 Hz),
  done double-click (660 Hz), error double-beep (220 → 165 Hz). Disable
  with `voice.feedback_sounds = false`.

## Installing models

Phase 5 requires Piper voices, the Piper Windows binary, espeak-ng-data,
and a Whisper STT model. Use **Settings → Local model storage** to select the
directory and download missing models, or run the bundled script directly:

```powershell
powershell scripts/download-models.ps1
```

The script is idempotent — it will skip anything already in place and
prints a summary at the end. Files land at:

| Artifact | Path |
|---|---|
| Piper binary | `~/.continuum-dev/bin/piper/piper.exe` |
| Piper EN voice | `~/.continuum-dev/models/tts/en_US-norman-medium.onnx` (+ `.json`) |
| Piper NL voice | `~/.continuum-dev/models/tts/nl_NL-mls-medium.onnx` (+ `.json`) |
| espeak-ng-data | `~/.continuum-dev/models/tts/espeak-ng-data/` |
| Whisper medium | `~/.continuum-dev/models/stt/whisper-medium.bin` |

Both the Piper binary and espeak-ng-data come from the official Piper
Windows release bundle (`piper_windows_amd64.zip`). That avoids the
defunct `rhasspy/espeak-ng-data` repo and guarantees version
compatibility.

## Smoke testing

The three bundled examples let you verify each layer independently.

```bash
# 5A: TTS only — synthesises Dutch and English, plays through speakers.
cargo run --example voice_test -p continuum-core

# 5C: end-to-end demo with typed transcripts.
cargo run --example voice_demo -p continuum-core

# 5C: latency benchmark against ARCHITECTURE.md targets.
cargo run --example voice_latency_bench -p continuum-core
```

`voice_demo` is the fastest path to "does my wake detection + endpoint +
TTS + conversation mode work end to end" because it doesn't depend on
the microphone — you type transcripts at the prompt.

For the real thing:

```bash
cargo run --bin continuum
```

which runs perception + triage + orchestrator + voice in one process.

## Configuration

Voice configuration lives in `~/.continuum-dev/config.toml` (user override)
with defaults in `config/default-models.toml`. Most fields are under
`[voice]` and `[tts]`.

### `[voice]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Master switch for the voice-input path. |
| `wake_word_enabled` | `true` | Require the wake phrase before a command is accepted. |
| `wake_keyword` | `"hey continuum"` | Phrase Continuum listens for in whisper transcripts. Case-insensitive. |
| `wake_sensitivity` | `0.5` | Reserved for the future native Porcupine-style backend. |
| `custom_keyword_path` | `""` | Reserved — path to a `.ppn` file for the native backend. |
| `listen_timeout_ms` | `12000` | Max time a post-wake session stays open. |
| `endpoint_silence_ms` | `700` | Silence after the last transcript fragment before endpoint fires. |
| `min_utterance_chars` | `3` | Lower bound on utterance length before endpoint can fire. |
| `barge_in_enabled` | `true` | Stop playback when fresh user speech arrives. |
| `ambient_mute_enabled` | `true` | Stay silent while the user is in a call. |
| `language_detection_enabled` | `false` | Route TTS voice by detected speech language. Disabled by default so Continuum always responds via the English voice. |
| `default_language` | `"en"` | Language used when detection is unavailable. |
| `volume` | `0.8` | Master playback gain in `[0.0, 1.0]`. Applied in the cpal callback. |
| `feedback_sounds` | `true` | Play chime/click/beep cues on state transitions. |
| `hotkey` | `"Ctrl+Shift+K"` | Global toggle-listen shortcut (Windows). Empty disables. |
| `conversation_followup_seconds` | `5` | Wake-free window after Continuum finishes speaking. `0` requires wake word every time. |

### `[tts]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Master switch for speech output. |
| `engine` | `"piper"` | `"piper"` (local, shipping) or `"elevenlabs"` (stub — falls back to Piper). |
| `espeak_data_dir` | `~/.continuum-dev/models/tts/espeak-ng-data` | Phoneme dictionary installed by the download script. |
| `primary` | `"en"` | BCP-47 short code of the default voice. |
| `voices.<lang>` | see defaults | Model + config paths per language, plus optional `speaker_id` for multi-speaker models. |
| `length_scale` | unset | `<1.0` speeds up speech; `>1.0` slows it. `unset` uses the voice's native value. |

### `[tts.elevenlabs]`

ElevenLabs is an intentional extension point. Phase 5 ships with the
Piper local path only; the ROADMAP keeps cloud TTS as a future plugin.
The config surface is stable so user configs don't break when the plugin
lands.

| Key | Default | Meaning |
|---|---|---|
| `api_key` | `""` | User-provided ElevenLabs API key. |
| `voice_id` | `""` | ElevenLabs voice ID. |
| `model_id` | `"eleven_turbo_v2_5"` | Model — turbo is the lowest-latency option. |
| `stability` | `0.5` | Prosody stability parameter. |
| `similarity_boost` | `0.75` | Reference-voice similarity parameter. |

Setting `tts.engine = "elevenlabs"` today logs a warning and falls back
to Piper — it does **not** crash or produce silence.

## Latency budget

| Stage | P95 target | Typical (CPU, medium voice) |
|---|---|---|
| Wake detection | ≤ 10 ms | 1–2 ms |
| Endpoint decision | ≤ 20 ms | sub-ms |
| TTS synthesis (short phrase) | ≤ 400 ms | 150–350 ms |
| Playback start | ≤ 50 ms | 10–30 ms |
| Full pipeline (wake → first audio queued) | ≤ 500 ms | 250–450 ms |
| Orchestrator wake (first word) | ≤ 2 s | 800–1600 ms |
| Interrupt (user speech → playback stopped) | ≤ 50 ms | 10–40 ms |

Run the latency benchmark to check your machine against these numbers:

```bash
cargo run --example voice_latency_bench -p continuum-core
```

Set `CONTINUUM_BENCH_N=50` for a longer run, or `CONTINUUM_BENCH_LANG=nl` to
benchmark the Dutch voice.

## Self-healing hooks

Every voice sub-component exposes a coarse health status used by the
Phase 7 repair agent. The probes are cheap file checks rather than
end-to-end synthesis calls, so they are safe to call often.

| Component | Health source |
|---|---|
| TTS | `voice::health::tts_health_from_paths` — model + config + espeak dir exist |
| STT | `voice::health::stt_health_from_paths` — whisper model exists |
| Wake | `voice::health::wake_health` — wake word enabled + keyword non-empty |
| Playback | `voice::health::playback_health` — cpal stream opened |

`VoiceHealth::snapshot(components)` combines them into a
`VoiceHealthReport` with an `overall()` status. Anything at
`HealthStatus::Unhealthy` signals the repair agent to restart the
component.

## Troubleshooting

**Continuum doesn't speak.** Check `~/.continuum-dev/config.toml` — `tts.enabled`
must be `true`, `voice.enabled` must be `true`, and `--no-tts` must not
be set. Then verify the Piper binary at `~/.continuum-dev/bin/piper/piper.exe`
and the voice `.onnx` + `.onnx.json` files under
`~/.continuum-dev/models/tts/`. Re-run `scripts/download-models.ps1` if any
are missing.

**Piper fails with "phonemizer error".** The `espeak-ng-data` directory
is missing or empty. Re-run the download script — it installs the
dictionary alongside `piper.exe` from the same release bundle.

**Wrong language on output.** Continuum is English-only by design right now
(see the "Multilingual input, English output" note above). The user's
speech is transcribed in whatever language they used, but the orchestrator
and triage prompts instruct Continuum to respond in English so the English
TTS voice sounds natural. If you have a quality Dutch/other voice and
want multilingual output, uncomment the `[tts.voices.<lang>]` section in
the config, flip `voice.language_detection_enabled = true`, and drop the
"always respond in English" rule from
`prompts/orchestrator-system.md`.

**Whisper detects the wrong input language.** `audio.whisper_language`
defaults to `"auto"`, which relies on whisper's per-segment detection.
On short clips (< 1 second) detection can trip — force a concrete code
(`"en"`, `"nl"`, `"de"`) to bias toward your primary spoken language.
Forcing doesn't affect output language, only STT accuracy.

**Wake word never matches.** The current path is transcript-based — it
requires the wake phrase to survive whisper transcription. Very short
wake phrases (two syllables) work less reliably than three-word phrases.
If you need the wake word to work reliably in a noisy room, set
`voice.wake_word_enabled = false` and use the hotkey instead.

**Hotkey does nothing.** Another application may already own the chord.
Check the Continuum log for `"Hotkey registration failed"`. Change
`voice.hotkey` to an unused combination (e.g. `"Ctrl+Alt+K"`).

**Piper binary not found.** The runtime looks for piper in this order:
`CONTINUUM_PIPER_BIN` env var → `~/.continuum-dev/bin/piper/piper.exe` →
system PATH. Setting `CONTINUUM_PIPER_BIN` overrides everything.

**High synthesis latency.** The first synth call after startup is always
slower (Piper process spin-up, espeak-ng dictionary load, ONNX warmup).
Subsequent calls should be well under 400 ms for short sentences on
typical hardware. If they aren't, check CPU contention: Continuum plus
background builds plus a video call is enough to blow the budget.

**Audio crackling.** Happens when the PC can't keep up — usually the
cpal buffer is starving between refills. Raise `voice.volume` to force
the signal above the noise floor, or close background apps. If the
problem persists, log a note in `docs/self-healing.md` and file an
issue; the repair agent will eventually grow a dedicated playback probe.

**Continuum speaks at the wrong moment during a call.** `ambient_mute_enabled`
is a soft guard — the context watcher looks at foreground process names.
If your call app isn't on the hard-coded list (Discord, Teams, Zoom,
Meet), Continuum won't detect it. Add the process name in
`senses::context::CALL_APPS` if you want to contribute it upstream.

## Architectural choices (and why)

**Piper via subprocess, not `piper-rs`.** `piper-rs` pulls in its own
`ort`/`ndarray` tree that conflicts with `continuum-vision`'s ONNX Runtime
link. Calling `piper.exe` over stdin/stdout is slower by ~50 ms process
startup on the first call and 0 ms on the rest — well worth the clean
dependency graph.

**Transcript wake, not Porcupine.** Porcupine ships its own CPU-cheap
wake-word models but requires an access key, which is an external
dependency Continuum deliberately avoids per ROADMAP's local-first stance.
The transcript detector is accurate enough for three-word wake phrases
and adds zero new model/runtime overhead — whisper is running anyway.

**Heuristic endpoint, not LLM endpoint.** An LLM endpoint classifier
would catch nuances a regex cannot, but would add ~200 ms per
transcript fragment and another surface for failure. The heuristic
handles question starters (`what`, `wanneer`, `kun je`), sentence
punctuation, and short imperatives (`stop`, `cancel`, `bedankt`). Good
enough in practice; upgrade path is clean when we need it.

**Sentence-level streaming TTS, not token-level.** Piper cannot
synthesise a single token faster than the fixed process overhead (~100
ms), so token-level streaming would only add jitter. Sentence-level
streaming gives coherent prosody, a stable first-audio latency, and
chunks that align with natural human pauses.

## Extending

**Adding a new voice.** Drop the `.onnx` and `.onnx.json` files under
`~/.continuum-dev/models/tts/` and add an entry to `[tts.voices.<lang>]`.
The voice bank discovers any language present at startup and routes to
it when the language matches.

**Custom wake word.** For now, change `voice.wake_keyword` to anything
a couple syllables long. When the native Porcupine-style backend lands,
populate `voice.custom_keyword_path` with a `.ppn` file trained in
Picovoice Console.

**ElevenLabs.** Set `tts.elevenlabs.api_key`, `voice_id`, and flip
`tts.engine` to `"elevenlabs"`. Today that produces a warning and falls
back to Piper; the actual HTTP/WebSocket client lands in a future
point release without breaking your config.

**New feedback cue.** Add a variant to `FeedbackCue`, implement
`render()` with a waveform helper in `voice::sounds`, and the cue is
ready to play through the existing `FeedbackPlayer`.
