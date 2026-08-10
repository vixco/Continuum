# A8 — persistent-intelligence evaluation and CI

- Added a deterministic seven-scenario synthetic evaluation contract spanning temporal context, retrieval, provenance, memory precision, privacy, cache correctness, failure explanation, repair verification, UI truthfulness, and regression safety.
- Added 18 adversarial tests covering runtime-fixture spoofing, malformed exporter shapes, mixed/empty provenance, secret leakage, canonical `cloud_allowed` / `local_only` / `never_observe` behavior, local-only egress and cache boundaries, stale-cache reuse, meaningful-change loss, unverified repair success, and chat-scroll regressions.
- Added a dedicated GitHub Actions workflow and a hard trust boundary: schema v1 accepts contract fixtures only, reports runtime evidence as unsupported, and keeps `--require-runtime` red until structurally separate production adapters exist.
- Rebased the A8 evaluation-only diff onto the current coordinator integration base; the branch does not duplicate or claim ownership of baseline formatting changes.
