# Triage Layer (Layer 2)

The triage layer evaluates every salient perception frame using a small local LLM and outputs a structured decision. It is the gatekeeper that determines whether the orchestrator (Claude Opus) should wake up.

## How it works

1. The perception layer (Layer 1) produces frames every 3 seconds
2. Frames with salience above 0.10 are forwarded to triage
3. The triage LLM receives the frame JSON + system prompt + memory summary
4. GBNF grammar constrains the output to exactly one of 5 decisions
5. The handler executes the decision (or logs a placeholder for Phase 2)

## Decisions

| Decision | When | Phase 2 Behavior |
|---|---|---|
| `ignore` | Nothing worth doing | Frame dropped |
| `remember` | Worth noting, no action | Logged for future memory distillation |
| `whisper` | Time-sensitive, low-complexity | Prints `[would say via TTS: ...]` |
| `execute_simple` | Pre-approved action needed | Allowlisted: launch_app, show_notification, toggle_mute |
| `wake_orchestrator` | Genuine reasoning needed | Prints `[WOULD WAKE ORCHESTRATOR: ...]` |

## Model

Default: Qwen 3 4B (Q4_K_M, ~2.5 GB). See [ADR 004](decisions/004-triage-model.md) for details.

### Swapping models

1. Download any GGUF model to `~/.kairo-dev/models/triage/`
2. Update `~/.kairo-dev/config.toml`:
   ```toml
   [triage]
   path = "~/.kairo-dev/models/triage/your-model.gguf"
   ```
3. Restart kairo-perception

Any GGUF-compatible instruct model should work. Smaller models (1-3B) may need lower temperature and more explicit prompting.

## Running

```bash
# Perception only (no triage)
cargo run --bin kairo-perception

# Perception + triage
cargo run --bin kairo-perception -- --triage

# Benchmark triage accuracy
cargo run --bin kairo-triage-bench
```

## Debugging wrong decisions

1. **Check the benchmark**: Run `kairo-triage-bench` to see accuracy across labeled frames
2. **Check latency**: If P95 > 1500ms, consider fewer GPU layers or a smaller model
3. **Check the prompt**: The system prompt is in `prompts/triage-system.md` — verify the signal hierarchy is clear
4. **Check the grammar**: The GBNF grammar is in `prompts/triage-grammar.gbnf` — verify it matches the `TriageDecision` enum
5. **Inspect raw output**: Set `RUST_LOG=kairo_core::triage=debug` to see raw model output on fallback attempts
6. **Common issues**:
   - Model keeps choosing `ignore`: Temperature may be too low, or the perception frame lacks distinguishing information
   - Model produces thinking tokens: Ensure `/no_think` is at the start of the system prompt
   - Grammar compilation fails: Verify the GBNF syntax with a llama.cpp grammar validator

## Signal reliability

The triage prompt documents which perception signals to trust:

1. **Context fields** (process name, window title) — most reliable
2. **Audio transcript** — strongest direct signal when present
3. **Screen description** — unreliable hint from SmolVLM, corroborating only
4. **Salience hint** — pre-computed heuristic, informational

## Configuration

| Setting | Default | Description |
|---|---|---|
| `triage.path` | `~/.kairo-dev/models/triage/qwen3-4b-q4_k_m.gguf` | Model path |
| `triage.gpu_layers` | `0` | GPU offload layers (0 = CPU only) |
| `frame.salience_threshold` | `0.10` | Minimum salience to reach triage |
