//! Efficient ambient-perception primitives shared by capture and context consumers.
//!
//! Platform capture stays in `continuum_core::senses::vision`. This module only
//! defines cheap deterministic fingerprints/change gating plus compact, privacy-
//! classified contracts for downstream temporal context, health, UI, and memory.
//! It intentionally does **not** own a production semantic cache; A7's bounded
//! shared cache is the authoritative reuse layer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use image::{imageops::FilterType, DynamicImage, RgbaImage};
use serde::{Deserialize, Serialize};

/// Stable schema version for compact ambient screen observations.
pub const OBSERVATION_SCHEMA_VERSION: u16 = 1;

/// Canonical screen-observation sensitivity before any memory-domain mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// Eligible for normal downstream processing subject to user policy.
    CloudAllowed,
    /// May remain in bounded local history, but is not an automatic memory candidate.
    LocalOnly,
    /// Must not receive semantic processing, reusable caching, or persistence.
    NeverObserve,
}

impl Sensitivity {
    /// Whether reusable semantic cache material may be produced for this content.
    pub fn reusable_semantic_cache_allowed(self) -> bool {
        self == Self::CloudAllowed
    }

    /// Whether an observation may be proposed to the memory consolidation layer.
    ///
    /// This is intentionally stricter than recent-history persistence: local-only
    /// screen evidence stays local/session-scoped unless a separately authorized
    /// higher-level flow explicitly handles it.
    pub fn automatic_memory_candidate_allowed(self) -> bool {
        self == Self::CloudAllowed
    }
}

/// Retention decision for an observation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    /// Current-context evidence only.
    Ephemeral,
    /// Metadata may enter bounded recent-history storage.
    RecentHistory,
    /// Persistence is prohibited.
    DoNotPersist,
}

/// Runtime truth state for perception health and desktop status surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PerceptionHealth {
    /// Capture is running and the encoder path is available.
    Observing,
    /// Source is disabled by configuration rather than failed.
    Disabled,
    /// Observation is intentionally paused by user/privacy control.
    Paused,
    /// Last known state exists but is no longer fresh enough to present as live.
    Stale { reason: String },
    /// OS capture authorization is required.
    PermissionRequired { reason: String },
    /// Capture can continue but semantic encoding is unavailable.
    EncoderUnavailable { reason: String },
    /// The perception subsystem itself is unavailable.
    Unavailable { reason: String },
    /// Some capability is working with reduced quality/cadence.
    Degraded { reason: String },
    /// A selected frame is undergoing semantic processing.
    Processing,
    /// A concrete runtime error prevents normal observation.
    Error { reason: String },
    /// Live observation may run, but historical observation storage is disabled.
    HistoricalCaptureDisabled,
}

/// Deterministic identity for one frame without retaining its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameFingerprint {
    /// Original frame width; part of exact identity to avoid shape aliasing.
    pub width: u32,
    /// Original frame height; part of exact identity to avoid shape aliasing.
    pub height: u32,
    /// FNV-1a over dimensions followed by RGBA bytes.
    pub exact_hash: u64,
    /// 144-bit 16x9 average-luma hash packed into three words.
    pub perceptual_hash: [u64; 3],
}

impl FrameFingerprint {
    /// Build a deterministic fingerprint from an RGBA frame.
    pub fn from_rgba(image: &RgbaImage) -> Self {
        let width = image.width();
        let height = image.height();
        let mut hash = Fnv1a64::new();
        hash.update(&width.to_le_bytes());
        hash.update(&height.to_le_bytes());
        hash.update(image.as_raw());
        Self {
            width,
            height,
            exact_hash: hash.finish(),
            perceptual_hash: perceptual_hash(image),
        }
    }

    /// Normalized perceptual Hamming distance in `[0, 1]`.
    ///
    /// A dimension change is always maximal change because a capture target was
    /// reconfigured even if its downsampled luminance happens to look identical.
    pub fn perceptual_distance(self, other: Self) -> f32 {
        if self.width != other.width || self.height != other.height {
            return 1.0;
        }
        let differing: u32 = self
            .perceptual_hash
            .iter()
            .zip(other.perceptual_hash)
            .map(|(left, right)| (left ^ right).count_ones())
            .sum();
        differing as f32 / 144.0
    }
}

/// Unique identity for a whole display or bounded dirty region.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObservationKey {
    /// Stable capture-layer display id such as `display-1`.
    pub display_id: String,
    /// Optional `(x, y, width, height)` region in display-local coordinates.
    pub region: Option<(u32, u32, u32, u32)>,
}

impl ObservationKey {
    /// Construct a whole-display key.
    pub fn display(display_id: impl Into<String>) -> Self {
        Self {
            display_id: display_id.into(),
            region: None,
        }
    }

    /// Construct a bounded region key.
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

/// Reason the lightweight gate selected or rejected semantic work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    /// No previous sample exists for this display/region.
    FirstObservation,
    /// Exact content and dimensions are unchanged.
    ExactDuplicate,
    /// Change exists but remains below the configured semantic threshold.
    BelowThreshold,
    /// Perceptual change crossed the configured threshold.
    MeaningfulChange,
    /// Low-change content was refreshed because the fallback deadline elapsed.
    FallbackSample,
}

/// Result of evaluating one frame fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GateDecision {
    /// Whether semantic encoding should run.
    pub should_encode: bool,
    /// Normalized perceptual change score.
    pub change_score: f32,
    /// Explainable selection reason.
    pub reason: GateReason,
}

#[derive(Debug, Clone, Copy)]
struct GateState {
    fingerprint: FrameFingerprint,
    last_semantic_at: Instant,
    last_seen_sequence: u64,
}

/// Bounded per-display/region adaptive semantic gate.
#[derive(Debug)]
pub struct ChangeGate {
    threshold: f32,
    fallback_after: Duration,
    max_keys: usize,
    sequence: u64,
    state: HashMap<ObservationKey, GateState>,
}

impl ChangeGate {
    /// Create a gate with a conservative 64-key bound.
    pub fn new(threshold: f32, fallback_after: Duration) -> Self {
        Self::with_max_keys(threshold, fallback_after, 64)
    }

    /// Create a gate with an explicit maximum number of display/region states.
    pub fn with_max_keys(threshold: f32, fallback_after: Duration, max_keys: usize) -> Self {
        Self {
            threshold: threshold.clamp(0.0, 1.0),
            fallback_after,
            max_keys: max_keys.max(1),
            sequence: 0,
            state: HashMap::new(),
        }
    }

    /// Evaluate at a caller-supplied monotonic time for deterministic tests.
    pub fn evaluate_at(
        &mut self,
        key: ObservationKey,
        fingerprint: FrameFingerprint,
        now: Instant,
    ) -> GateDecision {
        self.sequence = self.sequence.saturating_add(1);
        let sequence = self.sequence;

        if !self.state.contains_key(&key) {
            self.evict_oldest_if_full();
            self.state.insert(
                key,
                GateState {
                    fingerprint,
                    last_semantic_at: now,
                    last_seen_sequence: sequence,
                },
            );
            return GateDecision {
                should_encode: true,
                change_score: 1.0,
                reason: GateReason::FirstObservation,
            };
        }

        let previous = self
            .state
            .get_mut(&key)
            .expect("key presence checked immediately above");
        previous.last_seen_sequence = sequence;

        if previous.fingerprint.exact_hash == fingerprint.exact_hash
            && previous.fingerprint.width == fingerprint.width
            && previous.fingerprint.height == fingerprint.height
        {
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

    /// Drop all history for one monitor after disconnect/reconfiguration.
    pub fn invalidate_display(&mut self, display_id: &str) {
        self.state.retain(|key, _| key.display_id != display_id);
    }

    /// Number of display/region keys currently tracked.
    pub fn tracked_keys(&self) -> usize {
        self.state.len()
    }

    fn evict_oldest_if_full(&mut self) {
        if self.state.len() < self.max_keys {
            return;
        }
        if let Some(oldest) = self
            .state
            .iter()
            .min_by_key(|(_, state)| state.last_seen_sequence)
            .map(|(key, _)| key.clone())
        {
            self.state.remove(&oldest);
        }
    }
}

/// Text-free key material A7's bounded shared cache can use for semantic reuse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SemanticCacheKey {
    /// Display/region scope.
    pub observation_key: ObservationKey,
    /// Exact frame identity.
    pub exact_hash: u64,
    /// Encoder/model/config generation; changing it prevents stale reuse.
    pub encoder_revision: String,
}

/// Compact typed observation after privacy classification and change gating.
///
/// Raw pixels and screenshot paths are intentionally absent. This record is
/// evidence for recent context; it is not itself durable memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbientObservation {
    /// Schema version for additive downstream evolution.
    pub schema_version: u16,
    /// Original capture timestamp in Unix milliseconds.
    pub timestamp_unix_ms: i64,
    /// Display or changed-region identity.
    pub key: ObservationKey,
    /// Exact frame content hash for deduplication/provenance.
    pub content_hash: u64,
    /// Normalized perceptual change score.
    pub change_score: f32,
    /// Privacy-scrubbed semantic summary, when semantic processing ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_summary: Option<String>,
    /// Opaque embedding-store reference rather than an inline vector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_ref: Option<String>,
    /// Confidence in semantic fields in `[0, 1]`.
    pub confidence: f32,
    /// Producer identity such as `screen:xcap`.
    pub source: String,
    /// Privacy classification applied before publication.
    pub sensitivity: Sensitivity,
    /// Explicit local retention disposition.
    pub retention: RetentionClass,
    /// Whether semantic inference actually ran for this observation.
    pub semantic_processed: bool,
}

impl AmbientObservation {
    /// Whether this record may enter bounded recent-history persistence.
    pub fn persistence_allowed(&self) -> bool {
        self.sensitivity != Sensitivity::NeverObserve
            && self.retention != RetentionClass::DoNotPersist
    }

    /// Whether this observation is eligible to become input to automatic memory consolidation.
    ///
    /// This does not promote anything. A3/A4 must still apply their evidence,
    /// recurrence, contradiction, and confirmation policy.
    pub fn automatic_memory_candidate_allowed(&self) -> bool {
        self.persistence_allowed() && self.sensitivity.automatic_memory_candidate_allowed()
    }

    /// Whether the record obeys the hard `never_observe` privacy invariant.
    pub fn privacy_consistent(&self) -> bool {
        self.sensitivity != Sensitivity::NeverObserve
            || (!self.semantic_processed
                && self.semantic_summary.is_none()
                && self.embedding_ref.is_none()
                && self.retention == RetentionClass::DoNotPersist)
    }
}

/// Monotonic runtime counters and latest measured latencies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptionMetrics {
    /// Frames acquired from capture APIs.
    pub frames_captured: u64,
    /// Byte-identical frames rejected before semantic work.
    pub exact_duplicates_discarded: u64,
    /// Perceptually low-change frames rejected before semantic work.
    pub low_change_discarded: u64,
    /// Semantic encoder invocations.
    pub semantic_inferences: u64,
    /// Reusable semantic lookups served by A7's cache layer.
    pub semantic_cache_hits: u64,
    /// Oldest buffered capture packets dropped under backpressure.
    pub buffered_frames_dropped: u64,
    /// Latest capture latency in microseconds.
    pub last_capture_latency_us: u64,
    /// Latest fingerprint/change-detection latency in microseconds.
    pub last_change_detection_latency_us: u64,
    /// Latest semantic inference latency in microseconds.
    pub last_inference_latency_us: u64,
    /// Latest capture-to-searchable-observation latency in microseconds.
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

    /// Fraction of semantic reuse lookups served from cache.
    pub fn semantic_cache_hit_rate(&self) -> f32 {
        let lookups = self.semantic_cache_hits + self.semantic_inferences;
        if lookups == 0 {
            return 0.0;
        }
        self.semantic_cache_hits as f32 / lookups as f32
    }
}

#[derive(Debug, Clone, Copy)]
struct Fnv1a64(u64);

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(Self::PRIME);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
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

    fn solid(width: u32, height: u32, value: u8) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([value, value, value, 255]))
    }

    fn split(left: u8, right: u8) -> RgbaImage {
        let mut image = solid(32, 18, left);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            if x >= 16 {
                *pixel = Rgba([right, right, right, 255]);
            }
        }
        image
    }

    #[test]
    fn unchanged_frames_are_deduplicated() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
        let key = ObservationKey::display("display-1");
        let fingerprint = FrameFingerprint::from_rgba(&solid(32, 18, 10));
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
    }

    #[test]
    fn frame_shape_change_cannot_alias_exact_content() {
        let landscape = FrameFingerprint::from_rgba(&solid(4, 2, 30));
        let portrait = FrameFingerprint::from_rgba(&solid(2, 4, 30));
        assert_ne!(landscape.exact_hash, portrait.exact_hash);
        assert_eq!(landscape.perceptual_distance(portrait), 1.0);
    }

    #[test]
    fn multi_monitor_keys_do_not_collide() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
        let fingerprint = FrameFingerprint::from_rgba(&solid(32, 18, 7));
        assert!(gate
            .evaluate_at(ObservationKey::display("display-1"), fingerprint, now)
            .should_encode);
        assert!(gate
            .evaluate_at(ObservationKey::display("display-2"), fingerprint, now)
            .should_encode);
        assert_eq!(gate.tracked_keys(), 2);
    }

    #[test]
    fn gate_state_is_bounded_and_evicts_oldest_key() {
        let now = Instant::now();
        let mut gate = ChangeGate::with_max_keys(0.05, Duration::from_secs(30), 2);
        let fingerprint = FrameFingerprint::from_rgba(&solid(32, 18, 7));
        gate.evaluate_at(ObservationKey::display("display-1"), fingerprint, now);
        gate.evaluate_at(
            ObservationKey::display("display-2"),
            fingerprint,
            now + Duration::from_millis(1),
        );
        gate.evaluate_at(
            ObservationKey::display("display-3"),
            fingerprint,
            now + Duration::from_millis(2),
        );
        assert_eq!(gate.tracked_keys(), 2);
        let decision = gate.evaluate_at(
            ObservationKey::display("display-1"),
            fingerprint,
            now + Duration::from_millis(3),
        );
        assert_eq!(decision.reason, GateReason::FirstObservation);
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
    fn encoder_revision_is_part_of_cache_identity() {
        let key = ObservationKey::display("display-1");
        let v1 = SemanticCacheKey {
            observation_key: key.clone(),
            exact_hash: 5,
            encoder_revision: "encoder-v1".into(),
        };
        let v2 = SemanticCacheKey {
            observation_key: key,
            exact_hash: 5,
            encoder_revision: "encoder-v2".into(),
        };
        assert_ne!(v1, v2);
    }

    #[test]
    fn local_only_history_does_not_become_automatic_memory_candidate() {
        assert!(Sensitivity::CloudAllowed.reusable_semantic_cache_allowed());
        assert!(!Sensitivity::LocalOnly.reusable_semantic_cache_allowed());
        assert!(!Sensitivity::NeverObserve.reusable_semantic_cache_allowed());
        assert!(Sensitivity::CloudAllowed.automatic_memory_candidate_allowed());
        assert!(!Sensitivity::LocalOnly.automatic_memory_candidate_allowed());
        assert!(!Sensitivity::NeverObserve.automatic_memory_candidate_allowed());
    }

    #[test]
    fn never_observe_requires_no_semantics_and_no_persistence() {
        let mut observation = synthetic_observation();
        observation.sensitivity = Sensitivity::NeverObserve;
        observation.retention = RetentionClass::DoNotPersist;
        observation.semantic_summary = None;
        observation.embedding_ref = None;
        observation.semantic_processed = false;
        assert!(!observation.persistence_allowed());
        assert!(!observation.automatic_memory_candidate_allowed());
        assert!(observation.privacy_consistent());

        observation.semantic_processed = true;
        assert!(!observation.privacy_consistent());
    }

    #[test]
    fn observation_roundtrip_preserves_timestamp_and_confidence() {
        let observation = synthetic_observation();
        let encoded = serde_json::to_vec(&observation).expect("serialize observation");
        let decoded: AmbientObservation =
            serde_json::from_slice(&encoded).expect("deserialize observation");
        assert_eq!(decoded.timestamp_unix_ms, observation.timestamp_unix_ms);
        assert_eq!(decoded.confidence, observation.confidence);
        assert_eq!(decoded, observation);
    }

    #[test]
    fn health_contract_distinguishes_stale_disabled_and_encoder_failure() {
        let states = [
            PerceptionHealth::Disabled,
            PerceptionHealth::Stale {
                reason: "synthetic stale snapshot".into(),
            },
            PerceptionHealth::EncoderUnavailable {
                reason: "synthetic model not loaded".into(),
            },
        ];
        let json = serde_json::to_string(&states).expect("serialize health states");
        assert!(json.contains("disabled"));
        assert!(json.contains("stale"));
        assert!(json.contains("encoder_unavailable"));
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

    fn synthetic_observation() -> AmbientObservation {
        AmbientObservation {
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
        }
    }
}
