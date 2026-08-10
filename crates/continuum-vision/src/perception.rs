//! Efficient ambient-perception primitives shared by capture and context consumers.
//!
//! The screen watcher owns platform capture. This module deliberately does not
//! capture pixels itself; it provides deterministic fingerprints, per-display
//! change gating, semantic-result caching, compact observation contracts, and
//! health/metrics types so high-frequency acquisition does not imply high-frequency
//! model inference.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use image::{imageops::FilterType, DynamicImage, RgbaImage};
use serde::{Deserialize, Serialize};

/// Stable schema version for compact ambient screen observations.
pub const OBSERVATION_SCHEMA_VERSION: u16 = 1;

/// Privacy classification attached before an observation leaves the senses layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// Safe for normal local processing and eligible downstream use.
    CloudAllowed,
    /// Must remain local and be redacted before any cloud prompt use.
    LocalOnly,
    /// Content must not be semantically processed or durably persisted.
    NeverObserve,
}

/// Retention decision for an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    /// Ephemeral current-context evidence only.
    Ephemeral,
    /// Metadata may enter bounded recent-history storage.
    RecentHistory,
    /// Explicitly excluded from persistence.
    DoNotPersist,
}

/// Runtime status exposed to health and desktop surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PerceptionHealth {
    /// Capture is active and semantic processing is available.
    Observing,
    /// Observation is intentionally paused by user/privacy control.
    Paused,
    /// OS permission or capture authorization is missing.
    PermissionRequired { reason: String },
    /// Capture works, but the semantic encoder is unavailable.
    EncoderUnavailable { reason: String },
    /// The subsystem is operating with reduced capability.
    Degraded { reason: String },
    /// A frame is currently undergoing semantic processing.
    Processing,
    /// Recent-history persistence is intentionally disabled.
    HistoricalCaptureDisabled,
}

/// Deterministic, privacy-safe identity for one frame/region.
///
/// `exact_hash` detects byte-identical content. `perceptual_hash` is a compact
/// 16×9 average-luma bitset that remains stable across small pixel-level changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameFingerprint {
    /// FNV-1a hash over RGBA bytes.
    pub exact_hash: u64,
    /// 144-bit average-luma hash packed into three `u64`s.
    pub perceptual_hash: [u64; 3],
}

impl FrameFingerprint {
    /// Build a deterministic fingerprint from an RGBA frame.
    pub fn from_rgba(image: &RgbaImage) -> Self {
        Self {
            exact_hash: fnv1a64(image.as_raw()),
            perceptual_hash: perceptual_hash(image),
        }
    }

    /// Normalized Hamming distance in `[0, 1]` between perceptual hashes.
    pub fn perceptual_distance(self, other: Self) -> f32 {
        let differing: u32 = self
            .perceptual_hash
            .iter()
            .zip(other.perceptual_hash)
            .map(|(left, right)| (left ^ right).count_ones())
            .sum();
        differing as f32 / 144.0
    }
}

/// Unique key for a display or bounded dirty region.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObservationKey {
    /// Stable display identity supplied by the platform capture layer.
    pub display_id: String,
    /// Optional region in display coordinates `(x, y, width, height)`.
    pub region: Option<(u32, u32, u32, u32)>,
}

impl ObservationKey {
    /// Construct a key for a whole display.
    pub fn display(display_id: impl Into<String>) -> Self {
        Self {
            display_id: display_id.into(),
            region: None,
        }
    }

    /// Construct a key for one region on a display.
    pub fn region(
        display_id: impl Into<String>,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            display_id: display_id.into(),
            region: Some((x, y, width, height)),
        }
    }
}

/// Why the change gate did or did not request semantic processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateReason {
    /// First frame for this key.
    FirstObservation,
    /// Exact pixel content is unchanged.
    ExactDuplicate,
    /// Perceptual change is below threshold and fallback sampling is not due.
    BelowThreshold,
    /// Perceptual change crossed the configured threshold.
    MeaningfulChange,
    /// Time-based refresh is due despite low visual change.
    FallbackSample,
}

/// Result of evaluating one frame fingerprint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateDecision {
    /// Whether local semantic inference should run.
    pub should_encode: bool,
    /// Normalized perceptual change score.
    pub change_score: f32,
    /// Decision reason for metrics/diagnostics.
    pub reason: GateReason,
}

#[derive(Debug, Clone, Copy)]
struct GateState {
    fingerprint: FrameFingerprint,
    last_semantic_at: Instant,
}

/// Per-display/region adaptive semantic gate.
#[derive(Debug)]
pub struct ChangeGate {
    threshold: f32,
    fallback_after: Duration,
    state: HashMap<ObservationKey, GateState>,
}

impl ChangeGate {
    /// Create a gate. `threshold` is clamped to `[0, 1]`.
    pub fn new(threshold: f32, fallback_after: Duration) -> Self {
        Self {
            threshold: threshold.clamp(0.0, 1.0),
            fallback_after,
            state: HashMap::new(),
        }
    }

    /// Evaluate a fingerprint at a caller-supplied monotonic time.
    ///
    /// Passing time explicitly makes the gate deterministic in tests and allows
    /// capture workers to share one clock source without sleeping.
    pub fn evaluate_at(
        &mut self,
        key: ObservationKey,
        fingerprint: FrameFingerprint,
        now: Instant,
    ) -> GateDecision {
        let Some(previous) = self.state.get_mut(&key) else {
            self.state.insert(
                key,
                GateState {
                    fingerprint,
                    last_semantic_at: now,
                },
            );
            return GateDecision {
                should_encode: true,
                change_score: 1.0,
                reason: GateReason::FirstObservation,
            };
        };

        if previous.fingerprint.exact_hash == fingerprint.exact_hash {
            previous.fingerprint = fingerprint;
            return GateDecision {
                should_encode: false,
                change_score: 0.0,
                reason: GateReason::ExactDuplicate,
            };
        }

        let change_score = previous.fingerprint.perceptual_distance(fingerprint);
        previous.fingerprint = fingerprint;
        if change_score >= self.threshold {
            previous.last_semantic_at = now;
            GateDecision {
                should_encode: true,
                change_score,
                reason: GateReason::MeaningfulChange,
            }
        } else if now.saturating_duration_since(previous.last_semantic_at) >= self.fallback_after {
            previous.last_semantic_at = now;
            GateDecision {
                should_encode: true,
                change_score,
                reason: GateReason::FallbackSample,
            }
        } else {
            GateDecision {
                should_encode: false,
                change_score,
                reason: GateReason::BelowThreshold,
            }
        }
    }

    /// Drop all state for one display, e.g. after reconnect/resolution change.
    pub fn invalidate_display(&mut self, display_id: &str) {
        self.state.retain(|key, _| key.display_id != display_id);
    }

    /// Number of independent display/region keys currently tracked.
    pub fn tracked_keys(&self) -> usize {
        self.state.len()
    }
}

/// Cache key for semantic encoder output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SemanticCacheKey {
    /// Display or region identity.
    pub observation_key: ObservationKey,
    /// Exact frame hash.
    pub exact_hash: u64,
    /// Encoder/model identity; changing it invalidates cached semantics.
    pub encoder_revision: String,
}

/// Compact cached semantic result. Raw screenshots are intentionally absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticResult {
    /// Privacy-scrubbed one-line summary.
    pub summary: String,
    /// Optional privacy-scrubbed relevant visible text, bounded by the caller.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub visible_text: Vec<String>,
    /// Optional opaque embedding-store reference rather than an inline vector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_ref: Option<String>,
    /// Encoder confidence in `[0, 1]`.
    pub confidence: f32,
}

/// In-memory semantic cache keyed by frame bytes and encoder revision.
#[derive(Debug, Default)]
pub struct SemanticCache {
    entries: HashMap<SemanticCacheKey, SemanticResult>,
}

impl SemanticCache {
    /// Read a cached semantic result.
    pub fn get(&self, key: &SemanticCacheKey) -> Option<&SemanticResult> {
        self.entries.get(key)
    }

    /// Insert or replace a semantic result.
    pub fn insert(&mut self, key: SemanticCacheKey, value: SemanticResult) {
        self.entries.insert(key, value);
    }

    /// Invalidate all cached results created by a previous encoder revision.
    pub fn retain_revision(&mut self, encoder_revision: &str) {
        self.entries
            .retain(|key, _| key.encoder_revision == encoder_revision);
    }

    /// Number of cached semantic results.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Compact typed observation emitted after privacy classification and change gating.
///
/// It intentionally contains no raw pixels and no screenshot path. Screenshot
/// persistence remains a separate explicit opt-in mechanism.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbientObservation {
    /// Schema version for compatible downstream evolution.
    pub schema_version: u16,
    /// Capture timestamp as Unix milliseconds, preserving source time exactly.
    pub timestamp_unix_ms: i64,
    /// Display/region identity.
    pub key: ObservationKey,
    /// Exact content hash for deduplication/provenance.
    pub content_hash: u64,
    /// Normalized visual change score.
    pub change_score: f32,
    /// Privacy-scrubbed semantic summary, when semantic processing ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_summary: Option<String>,
    /// Opaque embedding-store reference, never the vector itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_ref: Option<String>,
    /// Confidence in the semantic fields.
    pub confidence: f32,
    /// Producer identity, e.g. `screen:xcap` or `screen:synthetic-test`.
    pub source: String,
    /// Privacy classification applied before publication.
    pub sensitivity: Sensitivity,
    /// Explicit retention decision.
    pub retention: RetentionClass,
    /// Whether semantic inference ran for this observation.
    pub semantic_processed: bool,
}

impl AmbientObservation {
    /// Returns whether this record is allowed to enter any persistent history.
    pub fn persistence_allowed(&self) -> bool {
        self.sensitivity != Sensitivity::NeverObserve
            && self.retention != RetentionClass::DoNotPersist
    }
}

/// Monotonic counters/latencies suitable for runtime diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptionMetrics {
    /// Frames acquired from capture APIs.
    pub frames_captured: u64,
    /// Byte-identical frames discarded before semantic work.
    pub exact_duplicates_discarded: u64,
    /// Low-change frames skipped before semantic work.
    pub low_change_discarded: u64,
    /// Semantic encoder invocations.
    pub semantic_inferences: u64,
    /// Semantic cache hits.
    pub semantic_cache_hits: u64,
    /// Oldest-buffered frames dropped under backpressure.
    pub buffered_frames_dropped: u64,
    /// Last measured capture latency in microseconds.
    pub last_capture_latency_us: u64,
    /// Last measured change-detection latency in microseconds.
    pub last_change_detection_latency_us: u64,
    /// Last measured semantic inference latency in microseconds.
    pub last_inference_latency_us: u64,
    /// Last measured capture-to-searchable-observation latency in microseconds.
    pub last_searchable_latency_us: u64,
}

impl PerceptionMetrics {
    /// Fraction of captured frames discarded before semantic inference.
    pub fn deduplication_rate(&self) -> f32 {
        if self.frames_captured == 0 {
            return 0.0;
        }
        (self.exact_duplicates_discarded + self.low_change_discarded) as f32
            / self.frames_captured as f32
    }

    /// Fraction of semantic lookups served from cache.
    pub fn semantic_cache_hit_rate(&self) -> f32 {
        let lookups = self.semantic_cache_hits + self.semantic_inferences;
        if lookups == 0 {
            return 0.0;
        }
        self.semantic_cache_hits as f32 / lookups as f32
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

fn perceptual_hash(image: &RgbaImage) -> [u64; 3] {
    let reduced = image::imageops::resize(image, 16, 9, FilterType::Triangle);
    let luma = DynamicImage::ImageRgba8(reduced).to_luma8();
    let pixels = luma.as_raw();
    let mean = pixels.iter().map(|value| u64::from(*value)).sum::<u64>() / pixels.len() as u64;
    let mut hash = [0u64; 3];
    for (index, value) in pixels.iter().enumerate() {
        if u64::from(*value) >= mean {
            hash[index / 64] |= 1u64 << (index % 64);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn solid(value: u8) -> RgbaImage {
        RgbaImage::from_pixel(32, 18, Rgba([value, value, value, 255]))
    }

    fn split(left: u8, right: u8) -> RgbaImage {
        let mut image = solid(left);
        for pixel in image.enumerate_pixels_mut() {
            if pixel.0 >= 16 {
                *pixel.2 = Rgba([right, right, right, 255]);
            }
        }
        image
    }

    #[test]
    fn exact_fingerprint_is_deterministic() {
        let image = solid(42);
        assert_eq!(
            FrameFingerprint::from_rgba(&image),
            FrameFingerprint::from_rgba(&image)
        );
    }

    #[test]
    fn unchanged_frames_are_deduplicated() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
        let key = ObservationKey::display("display-1");
        let fingerprint = FrameFingerprint::from_rgba(&solid(10));
        assert!(gate.evaluate_at(key.clone(), fingerprint, now).should_encode);
        let decision = gate.evaluate_at(key, fingerprint, now + Duration::from_millis(20));
        assert!(!decision.should_encode);
        assert_eq!(decision.reason, GateReason::ExactDuplicate);
    }

    #[test]
    fn meaningful_changes_trigger_semantic_processing() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
        let key = ObservationKey::display("display-1");
        gate.evaluate_at(key.clone(), FrameFingerprint::from_rgba(&split(0, 0)), now);
        let decision = gate.evaluate_at(
            key,
            FrameFingerprint::from_rgba(&split(0, 255)),
            now + Duration::from_millis(20),
        );
        assert!(decision.should_encode);
        assert_eq!(decision.reason, GateReason::MeaningfulChange);
        assert!(decision.change_score >= 0.05);
    }

    #[test]
    fn multi_monitor_keys_do_not_collide() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
        let fingerprint = FrameFingerprint::from_rgba(&solid(7));
        assert!(gate
            .evaluate_at(ObservationKey::display("display-1"), fingerprint, now)
            .should_encode);
        assert!(gate
            .evaluate_at(ObservationKey::display("display-2"), fingerprint, now)
            .should_encode);
        assert_eq!(gate.tracked_keys(), 2);
    }

    #[test]
    fn fallback_sampling_refreshes_low_change_content() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.90, Duration::from_secs(5));
        let key = ObservationKey::display("display-1");
        gate.evaluate_at(key.clone(), FrameFingerprint::from_rgba(&split(20, 20)), now);
        let decision = gate.evaluate_at(
            key,
            FrameFingerprint::from_rgba(&split(20, 21)),
            now + Duration::from_secs(6),
        );
        assert!(decision.should_encode);
        assert_eq!(decision.reason, GateReason::FallbackSample);
    }

    #[test]
    fn cached_encoder_results_invalidate_by_revision() {
        let key = ObservationKey::display("display-1");
        let mut cache = SemanticCache::default();
        for revision in ["encoder-v1", "encoder-v2"] {
            cache.insert(
                SemanticCacheKey {
                    observation_key: key.clone(),
                    exact_hash: 5,
                    encoder_revision: revision.to_string(),
                },
                SemanticResult {
                    summary: revision.to_string(),
                    visible_text: Vec::new(),
                    embedding_ref: None,
                    confidence: 0.8,
                },
            );
        }
        cache.retain_revision("encoder-v2");
        assert_eq!(cache.len(), 1);
        assert!(cache
            .get(&SemanticCacheKey {
                observation_key: key,
                exact_hash: 5,
                encoder_revision: "encoder-v2".into(),
            })
            .is_some());
    }

    #[test]
    fn never_observe_records_cannot_be_persisted() {
        let observation = AmbientObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            timestamp_unix_ms: 1_786_317_600_000,
            key: ObservationKey::display("display-1"),
            content_hash: 42,
            change_score: 1.0,
            semantic_summary: None,
            embedding_ref: None,
            confidence: 1.0,
            source: "screen:synthetic-test".into(),
            sensitivity: Sensitivity::NeverObserve,
            retention: RetentionClass::RecentHistory,
            semantic_processed: false,
        };
        assert!(!observation.persistence_allowed());
    }

    #[test]
    fn observation_roundtrip_preserves_timestamp_and_confidence() {
        let observation = AmbientObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            timestamp_unix_ms: 1_786_317_600_123,
            key: ObservationKey::region("display-2", 10, 20, 300, 200),
            content_hash: 9,
            change_score: 0.42,
            semantic_summary: Some("synthetic editor window".into()),
            embedding_ref: Some("embedding:synthetic:9".into()),
            confidence: 0.73,
            source: "screen:synthetic-test".into(),
            sensitivity: Sensitivity::CloudAllowed,
            retention: RetentionClass::Ephemeral,
            semantic_processed: true,
        };
        let encoded = serde_json::to_vec(&observation).expect("serialize observation");
        let decoded: AmbientObservation =
            serde_json::from_slice(&encoded).expect("deserialize observation");
        assert_eq!(decoded.timestamp_unix_ms, observation.timestamp_unix_ms);
        assert_eq!(decoded.confidence, observation.confidence);
        assert_eq!(decoded, observation);
    }

    #[test]
    fn health_contract_reports_degraded_encoder_state() {
        let health = PerceptionHealth::EncoderUnavailable {
            reason: "synthetic model not loaded".into(),
        };
        let json = serde_json::to_string(&health).expect("serialize health");
        assert!(json.contains("encoder_unavailable"));
        assert!(json.contains("synthetic model not loaded"));
    }

    #[test]
    fn metrics_report_dedup_and_cache_rates() {
        let metrics = PerceptionMetrics {
            frames_captured: 100,
            exact_duplicates_discarded: 70,
            low_change_discarded: 20,
            semantic_inferences: 8,
            semantic_cache_hits: 2,
            ..PerceptionMetrics::default()
        };
        assert!((metrics.deduplication_rate() - 0.9).abs() < f32::EPSILON);
        assert!((metrics.semantic_cache_hit_rate() - 0.2).abs() < f32::EPSILON);
    }
}
