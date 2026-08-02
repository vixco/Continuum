# Phase 3 Smoke Test

Manual test procedure for verifying the orchestrator end-to-end.

## Prerequisites

- Claude Code CLI installed and authenticated (`claude --version`, `claude login`)
- Triage model downloaded (`scripts/download-models.ps1`)
- ONNX Runtime DLL accessible (see `.cargo/config.toml` for ORT_DYLIB_PATH)
- BGESmallENV15Q embedding model will auto-download on first run (~66 MB)

## Test 1: Basic wake cycle

```bash
cargo run --release --bin continuum
```

Wait for "All layers running" message. Then trigger a wake by either:
- Speaking "Continuum, wat kan jij?" into the microphone
- Creating an error visible on screen (open a terminal, run a failing command) and wait ~10 seconds

**Expected:**
1. Triage output shows `triage=wake_orchestrator`
2. `--- CONTINUUM WAKING ---` appears
3. `CONTINUUM:` followed by streaming text (Opus's response)
4. Response is in Dutch (if triggered with Dutch audio)
5. Cost and duration shown: `--- [1500ms $0.0350] ---`

## Test 2: Memory storage

After test 1 completes, trigger a second wake (ask another question).

**Expected:**
- The second wake's "Relevant memories" section in debug logs should include the first interaction
- Verify by checking `~/.continuum-dev/episodic_db/` directory exists and has data

## Test 3: Personality check

Verify Opus responds as Continuum, not as "Claude" or a generic assistant:
- Response should be short, direct
- No "I'd be happy to help!" energy
- Should use Dutch if audio was Dutch
- No narration of internal reasoning

## Test 4: Graceful shutdown

Press Ctrl+C during operation.

**Expected:**
- "Ctrl+C received, shutting down..." log
- "Continuum stopped." log
- Process exits cleanly (no zombie claude processes)
- Verify: `tasklist | findstr claude` shows no lingering processes

## Test 5: Crash resilience

Kill the claude subprocess mid-response (Task Manager → kill node.exe or claude process).

**Expected:**
- `[ORCHESTRATOR ERROR: ...]` or `Stream ended without result event` warning
- Continuum continues running (does NOT crash)
- Next wake cycle works normally

## Test 6: No API key / auth failure

Run without being logged in: `claude logout` then `cargo run --release --bin continuum`

**Expected:**
- Wake attempt logs an error
- Continuum continues in observe-only mode
- No crash, no panic

## Verification checklist

- [ ] Triage wake decision triggers orchestrator
- [ ] Opus responds within 3-5 seconds
- [ ] Response text streams to terminal in real time
- [ ] Response is in the correct language
- [ ] Response reflects Continuum personality (not generic assistant)
- [ ] Cost per wake logged
- [ ] Wake stored in episodic memory
- [ ] Second wake retrieves first interaction
- [ ] Ctrl+C cleanly shuts down
- [ ] Subprocess crash doesn't crash Continuum
- [ ] All clippy/fmt pass: `cargo clippy -p continuum-core -- -D warnings`
