# Vision evaluation

Continuum evaluates local vision as a Layer 1 screen sense. Raw screenshots
remain local; only a compact caption and health metadata flow upward to triage.

## Reproducible benchmark

The evaluation manifest is
`crates/continuum-vision/tests/fixtures/vision-eval.json`. It contains one
natural browser scene and four deterministic synthetic desktop screens: a code
editor build failure, runtime dashboard, privacy settings, and team calendar.
The synthetic fixtures contain no user data and can be regenerated with:

```powershell
.\scripts\generate-vision-eval-fixtures.ps1
```

Run a locally installed backend with:

```powershell
cargo run --release -p continuum-vision --bin continuum-vision-bench -- `
  "$env:USERPROFILE\.continuum-dev\models\vision\smolvlm2-2.2b-q4"
```

The scorer checks groups of required semantic concepts and separately counts
forbidden hallucinations. This is a small regression benchmark, not a claim of
general vision accuracy.

## Results on the development machine

Measured on 2026-08-12 with 8 logical CPU threads and an RTX 4060 Ti whose CUDA
and Vulkan development SDKs were not installed. Both measured runs therefore
used CPU inference.

| Backend | Concepts | Forbidden hallucinations | Mean latency |
|---|---:|---:|---:|
| SmolVLM-500M ONNX | 7/17 (41.2%) | 0 | 23.58 s |
| SmolVLM2-2.2B Q4_K_M MTMD | 12/17 (70.6%) | 0 | 9.81 s |

The 2.2B backend is the preferred default because it covered five more tested
concepts and was about 2.4 times faster in the same CPU-only environment. The
500M ONNX backend remains the automatic load/warmup fallback.

## Honest limitations

- The natural scene was understood as a man pointing at a picture, but the
  model described Elf as a man in a green suit; fine-grained identity is not
  consistently reliable.
- Small UI text and exact state values are still missed. Structured Windows
  context remains primary evidence; vision is corroborating evidence.
- GGUF confidence is reported as `0.0` (unavailable). The MTMD helper does not
  safely expose calibrated post-prompt token probabilities through the current
  Rust wrapper, and Continuum does not fabricate a confidence value.
- CUDA and Vulkan code paths compile only when their development SDK is
  installed. They were not runtime-verified in this evaluation.
