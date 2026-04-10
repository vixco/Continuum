# ADR 004: Triage Model Selection

**Date:** 2026-04-10
**Status:** Accepted
**Phase:** 2 — Triage

## Context

The triage layer (Layer 2) needs a small local LLM to evaluate perception frames and output structured JSON decisions. Requirements:

- **Structured JSON output**: Must reliably produce one of 5 decision variants
- **Bilingual**: User speaks Dutch and English — model must understand both
- **Fast on CPU**: P95 latency under 1.5 seconds for ~100 output tokens
- **Small footprint**: Under 3 GB RAM at Q4_K_M quantization
- **GGUF format**: Must work with llama.cpp via `llama-cpp-2` Rust bindings

## Decision

**Default model: Qwen 3 4B (Q4_K_M quantization)**

Source: `Qwen/Qwen3-4B-GGUF` on HuggingFace (official, no auth required)
File: `Qwen3-4B-Q4_K_M.gguf`
Size: ~2.5 GB

**Low-RAM alternative: Qwen 2.5 3B Instruct (Q4_K_M)** for systems with ≤4 GB available RAM.

## Alternatives Considered

| Model | JSON Quality | Dutch | Size | Speed | Verdict |
|---|---|---|---|---|---|
| **Qwen 3 4B** | Best at scale | Good (119 langs) | 2.5 GB | 30-35 tok/s | **Chosen** |
| Qwen 2.5 3B | Good | Functional | 1.9 GB | 40 tok/s | Low-RAM fallback |
| Gemma 3 4B | Weaker | Good | 2.5 GB | 30-35 tok/s | JSON grammar issues |
| Phi-4 mini 3.8B | Strong | Weak | 2.3 GB | 35-40 tok/s | Dutch disqualifies |
| Llama 3.2 3B | Decent | Weak | 1.8 GB | 45 tok/s | Dutch disqualifies |

## Key Implementation Details

1. **Thinking mode disabled**: Qwen 3 has a "thinking" mode that must be suppressed for triage. The system prompt starts with `/no_think` to prevent chain-of-thought reasoning, which adds unnecessary latency and tokens for a classification task.

2. **GBNF grammar constraint**: Output is constrained by a GBNF grammar that guarantees valid JSON matching the triage decision schema. This makes model choice less critical for JSON reliability.

3. **3-retry fallback**: Grammar-constrained generation first, then prompt-only retries with "JSON only" suffix, defaulting to Ignore on total failure.

## Consequences

- Model download adds ~2.5 GB to first-run setup time
- CPU inference at ~30-35 tok/s is within latency budget
- Qwen 3's improved Dutch comprehension means better triage decisions for Dutch audio transcripts like "kut waarom werkt dit nou niet"
- The `/no_think` suppression must be verified working — if thinking tokens leak, latency will exceed budget
