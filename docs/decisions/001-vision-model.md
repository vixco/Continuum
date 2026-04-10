# ADR-001: SmolVLM-256M as Default Vision Model

**Status:** Accepted
**Date:** 2026-04-10
**Layer:** 1 (Senses)
**Crate:** `kairo-vision`

## Context

Kairo's senses layer captures the primary monitor every 3 seconds at 1280x720, downscales to 384x384, and feeds the image to a local vision model that produces a one-sentence description. The model must:

- Run on CPU with inference under 3 seconds (the capture interval).
- Be small enough to coexist with the triage LLM (Qwen 2.5 3B) in RAM.
- Have reliable ONNX exports for use with the `ort` crate (ONNX Runtime).
- Be self-contained: no Python sidecar, no fragile conversion pipeline.

## Options Considered

### 1. Moondream 2 (1.8B)

- **Pro:** Best quality for its size class, good at UI understanding.
- **Con (ONNX):** The community ONNX export has shape mismatch issues between the vision encoder and text decoder. We hit dimension errors during our integration attempt.
- **Con (candle):** The candle (Rust-native) implementation is pinned to an old Moondream revision that predates the v2 architecture changes. Updating it requires significant effort.
- **Con (Python sidecar):** Running Moondream via a Python subprocess is possible but violates our self-contained deployment goal and adds startup latency.
- **Con (size):** At 1.8B parameters, CPU inference takes 5-8 seconds on mid-range hardware, exceeding the 3-second budget.

### 2. Florence-2 (base: 232M, large: 770M)

- **Pro:** Microsoft-maintained, good at captioning and OCR.
- **Con:** ONNX exports exist but are less battle-tested than HuggingFace models. The large variant is too slow for 3-second CPU cycles.
- **Verdict:** Viable alternative. May revisit for specialized tasks (OCR detection).

### 3. SmolVLM-256M

- **Pro:** HuggingFace-maintained ONNX exports, first-party support.
- **Pro:** 8x smaller than Moondream (256M vs 1.8B parameters).
- **Pro:** Fastest CPU inference of the three options (~2-3 seconds).
- **Pro:** Self-contained deployment via the `ort` crate.
- **Con:** Lower description quality than Moondream, especially for fine UI details.
- **Verdict:** Best fit for Phase 1 requirements.

## Decision

Use **SmolVLM-256M via the `ort` crate** as the default vision model.

The model files live in `~/.kairo-dev/models/vision/smolvlm-256m/` and are downloaded by `scripts/download-models.ps1`. The model name and path are configurable in `config.toml` under `[vision]`.

## Phase 1 Status

- Vision encoder: loaded and validated. Produces image embeddings.
- Autoregressive text decoder: **stubbed**. The full token-by-token decoding loop is planned for Phase 1.5. Until then, the stub model returns `"(no vision model loaded)"` as the description.
- The `kairo-perception` binary gracefully falls back to the stub when model files are missing.

## Alternatives Available

The vision model is pluggable via the `VisionModel` trait in `kairo-vision`. Users can switch by changing `[vision]` config:

- **Moondream 2** -- recommended for users with a CUDA GPU, where the 1.8B size is not a bottleneck.
- **Florence-2 base/large** -- available for OCR-heavy workflows.

## GPU Policy

GPU acceleration is disabled in Phase 1 (`gpu_enabled = false` in default config). The `ort` crate supports CUDA execution providers, and enabling GPU is a one-line config change for future phases.

## References

- `crates/kairo-vision/src/onnx.rs` -- ONNX model loader
- `crates/kairo-vision/src/lib.rs` -- `VisionModel` trait
- `crates/kairo-core/src/senses/vision.rs` -- `VisionWatcher` that calls the model
- `crates/kairo-core/src/config.rs` -- `VisionConfig` struct with defaults
