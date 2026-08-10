# A2 — efficient ambient perception and vision context

- Added dimension-aware exact + perceptual frame fingerprints for cheap duplicate/change detection without retaining pixels.
- Added bounded per-display/per-region adaptive semantic gating with deterministic oldest-key eviction and a time-based fallback sample.
- Added text-free, encoder-revision-aware semantic cache key material for the bounded shared cache; A2 does not introduce a second production cache.
- Added compact privacy-classified ambient observation records that never contain raw pixels or screenshot paths.
- Added explicit perception health states distinguishing observing, disabled, paused, stale, permission-required, encoder-unavailable, unavailable, degraded, processing, error, and historical-capture-disabled conditions.
- Added strict privacy helpers: `never_observe` cannot be semantically processed or persisted, while `local_only` recent history is explicitly ineligible for automatic memory-candidate promotion.
- Added monotonic perception counters/latencies for capture, deduplication, cache reuse, inference, buffering, and time-to-searchable observation.
- Added synthetic tests for duplicate collapse, meaningful changes, shape changes, multi-monitor isolation, bounded gate state, fallback sampling, encoder-revision identity, privacy-safe history/memory boundaries, observation timestamp/confidence, health truth states, and metrics.
- Added a reproducible synthetic `perception_gate_bench` example. Its output measures only fingerprint/change-gate overhead and makes no Windows/native capture or ONNX performance claim.

Integration note: fold this entry into the root `CHANGELOG.md` once when the swarm branches are integrated.
