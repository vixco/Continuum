# A2 — efficient ambient perception and vision context

- Added deterministic exact + perceptual frame fingerprints for cheap duplicate/change detection.
- Added per-display and per-region adaptive semantic gating with a time-based fallback sample.
- Added encoder-revision-aware semantic cache keys and invalidation.
- Added compact privacy-classified ambient observation records that never contain raw pixels or screenshot paths.
- Added explicit perception health states for observing, paused, permission-required, encoder-unavailable, degraded, processing, and historical-capture-disabled conditions.
- Added monotonic perception counters/latencies for capture, deduplication, cache, inference, buffering, and time-to-searchable observation.
- Added synthetic tests for duplicate collapse, meaningful change, multi-monitor isolation, fallback sampling, cache invalidation, privacy-safe persistence, observation provenance fields, degraded encoder state, and metrics.
- Added a reproducible synthetic `perception_gate_bench` example. Its output measures only fingerprint/change-gate overhead and makes no Windows/native capture or ONNX performance claim.

Integration note: fold this entry into the root `CHANGELOG.md` once when the swarm branches are integrated.
