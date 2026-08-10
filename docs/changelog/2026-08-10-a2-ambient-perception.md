# A2 — efficient ambient perception and vision context

- Added privacy-gated SHA-256 frame identity plus brightness-sensitive perceptual change detection; `never_observe` returns before hashing or semantic selection.
- Added a bounded per-display/per-region second-stage gate behind the existing 64×36 luma prefilter, with deterministic LRU eviction and time-based fallback refresh.
- Wired the gate into the real `xcap` watcher without creating a second capture loop; selected compact observation provenance and real perception metrics/health now ride the existing `world_compact` channel and therefore persist with raw-frame/context-event provenance without storing pixels.
- Added stable occurrence ids distinct from content identity, capture timestamps, display keys, strong content digest, change score, semantic-processing flag, confidence, source, sensitivity and retention metadata.
- Added explicit observation→event sensitivity conversion: `cloud_allowed` and `local_only` map exhaustively; `never_observe` has no event representation.
- Added explicit health states for observing, disabled, paused, stale, permission-required, encoder-unavailable, unavailable, degraded, processing, error and historical-capture-disabled conditions.
- Added strict retention helpers: `Ephemeral` and `DoNotPersist` are non-persistable; `local_only` is not an automatic durable-memory candidate; `never_observe` is neither semantically cacheable nor persistable.
- Added runtime counters for capture/change/inference/emission latency, redundant selected packets, inference frequency, buffer drops and gate-state evictions. CPU/GPU/memory remain owned by the runtime-health subsystem.
- Added synthetic tests for privacy-before-processing spies, duplicate/fallback behavior, uniform full-screen brightness changes, frame-shape identity, multi-monitor isolation, bounded state, cache-key sensitivity/revision identity, all privacy×retention combinations, occurrence provenance, event sensitivity mapping and health states.
- Added a reproducible synthetic `perception_gate_bench` example. Its output measures only fingerprint/change-gate overhead and makes no Windows/native capture or ONNX performance claim.

A7 coordination: A2 exports only inner, text-free encoder/content identity. It is not a safe cross-scope cache by itself; privacy generation, profile/project scope, TTL/byte bounds and invalidation remain the shared cache owner's responsibility.

Integration note: fold this entry into the root `CHANGELOG.md` once when the swarm branches are integrated.
