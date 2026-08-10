//! Efficient ambient-perception primitives shared by capture and context consumers.
//!
//! Platform capture stays in `continuum_core::senses::vision`. This module only
//! defines deterministic fingerprints/change gating plus compact privacy-aware
//! contracts. The existing 64x36 watcher prefilter remains the cheapest first
//! stage; strong fingerprints are suitable after that prefilter and for semantic
//! cache correctness. A7's bounded shared cache remains the authoritative cache.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use image::{imageops::FilterType, DynamicImage, RgbaImage};
use serde::{Deserialize, Serialize};

use crate::digest::Sha256;

/// Stable schema version for compact ambient screen observations.
pub const OBSERVATION_SCHEMA_VERSION: u16 = 1;

/// Canonical screen-observation sensitivity before any memory-domain mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// Eligible for ordinary downstream processing subject to user policy.
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

    /// Whether this sensitivity may enter automatic memory-candidate construction.
    pub fn automatic_memory_candidate_allowed(self) -> bool {
        self == Self::CloudAllowed
    }
}

/// Retention decision for an observation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    /// Current-context evidence only; it must not be persisted.
    Ephemeral,
    /// Metadata may enter bounded recent-history storage owned by the context layer.
    RecentHistory,
    /// Persistence is prohibited.
    DoNotPersist,
}

/// Runtime truth state for perception health and desktop status surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PerceptionHealth {
    /// Capture is running and the semantic path is available.
    Observing,
    /// Source is disabled by configuration rather than failed.
    Disabled,
    /// Observation is intentionally paused by user/privacy control.
    Paused,
    /// Last known state exists but is no longer fresh enough to present as live.
    Stale { reason: String },
    /// OS capture authorization is required.
    PermissionRequired { reason: String },
    /// Capture works but semantic encoding is unavailable.
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

/// Pixel layout covered by a content digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    /// Eight-bit red, green, blue, alpha channels in RGBA byte order.
    Rgba8,
}

/// Strong and perceptual identity for one frame without retaining its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameFingerprint {
    /// Original frame width.
    pub width: u32,
    /// Original frame height.
    pub height: u32,
    /// Pixel format bound into the strong digest.
    pub pixel_format: PixelFormat,
    /// SHA-256 over a domain separator, format, dimensions, and frame bytes.
    pub content_digest: [u8; 32],
    /// 144-bit 16x9 average-luma structural hash packed into three words.
    pub perceptual_hash: [u64; 3],
    /// Mean frame luminance used to catch global brightness/theme transitions.
    pub mean_luma: u8,
}

impl FrameFingerprint {
    /// Build a deterministic fingerprint from an RGBA frame.
    pub fn from_rgba(image: &RgbaImage) -> Self {
        let width = image.width();
        let height = image.height();
        let mut digest = Sha256::new();
        digest.update(b"continuum-frame-v1\0rgba8\0");
        digest.update(&width.to_le_bytes());
        digest.update(&height.to_le_bytes());
        digest.update(image.as_raw());
        let (perceptual_hash, mean_luma) = perceptual_hash(image);
        Self {
            width,
            height,
            pixel_format: PixelFormat::Rgba8,
            content_digest: digest.finalize(),
            perceptual_hash,
            mean_luma,
        }
    }

    /// Normalized perceptual distance in `[0, 1]`.
    ///
    /// Dimension/format changes are maximal. Otherwise the score is the maximum
    /// of structural Hamming distance and absolute mean-luminance difference,
    /// so uniform black→white transitions cannot disappear in average hashing.
    pub fn perceptual_distance(self, other: Self) -> f32 {
        if self.width != other.width
            || self.height != other.height
            || self.pixel_format != other.pixel_format
        {
            return 1.0;
        }
        let differing: u32 = self
            .perceptual_hash
            .iter()
            .zip(other.perceptual_hash)
            .map(|(left, right)| (left ^ right).count_ones())
            .sum();
        let structural = differing as f32 / 144.0;
        let luminance = self.mean_luma.abs_diff(other.mean_luma) as f32 / 255.0;
        structural.max(luminance)
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
    /// Exact content is unchanged and a liveness refresh is not yet due.
    ExactDuplicate,
    /// Change exists but remains below the semantic threshold.
    BelowThreshold,
    /// Perceptual change crossed the configured threshold.
    MeaningfulChange,
    /// A periodic liveness/semantic refresh is due, even if bytes are unchanged.
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

/// Bounded least-recently-used per-display/region adaptive semantic gate.
#[derive(Debug)]
pub struct ChangeGate {
    threshold: f32,
    fallback_after: Duration,
    max_keys: usize,
    sequence: u64,
    evicted_keys: u64,
    state: HashMap<ObservationKey, GateState>,
}

impl ChangeGate {
    /// Create a gate with a conservative 64-key bound.
    pub fn new(threshold: f32, fallback_after: Duration) -> Self {
        Self::with_max_keys(threshold, fallback_after, 64)
    }

    /// Create a gate with an explicit maximum display/region cardinality.
    pub fn with_max_keys(threshold: f32, fallback_after: Duration, max_keys: usize) -> Self {
        Self {
            threshold: threshold.clamp(0.0, 1.0),
            fallback_after,
            max_keys: max_keys.max(1),
            sequence: 0,
            evicted_keys: 0,
            state: HashMap::new(),
        }
    }

    /// Evaluate at a caller-supplied monotonic time for deterministic testing.
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

        let fallback_due = now.saturating_duration_since(previous.last_semantic_at)
            >= self.fallback_after;
        if previous.fingerprint.content_digest == fingerprint.content_digest {
            previous.fingerprint = fingerprint;
            if fallback_due {
                previous.last_semantic_at = now;
                return GateDecision {
                    should_encode: true,
                    change_score: 0.0,
                    reason: GateReason::FallbackSample,
                };
            }
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
        } else if fallback_due {
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

    /// Total key states evicted because the configured cap was reached.
    pub fn evicted_keys(&self) -> u64 {
        self.evicted_keys
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
            self.evicted_keys = self.evicted_keys.saturating_add(1);
        }
    }
}

/// Collision-resistant, text-free key for A7's bounded semantic cache.
///
/// Fields are private so callers cannot bypass sensitivity admission by manually
/// assembling cache keys from restricted observations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SemanticCacheKey {
    observation_key: ObservationKey,
    content_digest: [u8; 32],
    frame_width: u32,
    frame_height: u32,
    pixel_format: PixelFormat,
    encoder_revision: String,
}

impl SemanticCacheKey {
    /// Build a pre-inference lookup key only when sensitivity permits reuse.
    pub fn for_frame(
        sensitivity: Sensitivity,
        observation_key: ObservationKey,
        fingerprint: FrameFingerprint,
        encoder_revision: impl Into<String>,
    ) -> Option<Self> {
        if !sensitivity.reusable_semantic_cache_allowed() {
            return None;
        }
        Some(Self {
            observation_key,
            content_digest: fingerprint.content_digest,
            frame_width: fingerprint.width,
            frame_height: fingerprint.height,
            pixel_format: fingerprint.pixel_format,
            encoder_revision: encoder_revision.into(),
        })
    }

    /// Encoder revision bound into this cache identity.
    pub fn encoder_revision(&self) -> &str {
        &self.encoder_revision
    }

    /// Strong content digest bound into this cache identity.
    pub fn content_digest(&self) -> &[u8; 32] {
        &self.content_digest
    }
}

/// Compact typed observation after privacy classification and change gating.
///
/// Raw pixels and screenshot paths are intentionally absent. `observation_id`
/// identifies this occurrence, while `content_digest` identifies its frame bytes;
/// identical content observed at two times therefore remains distinct evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbientObservation {
    /// Schema version for additive downstream evolution.
    pub schema_version: u16,
    /// Stable occurrence id generated by the runtime/event adapter.
    pub observation_id: String,
    /// Original observed/capture timestamp in Unix milliseconds.
    pub timestamp_unix_ms: i64,
    /// Display or changed-region identity.
    pub key: ObservationKey,
    /// Strong content identity; not an occurrence/evidence id.
    pub content_digest: [u8; 32],
    /// Captured frame width.
    pub frame_width: u32,
    /// Captured frame height.
    pub frame_height: u32,
    /// Captured pixel format.
    pub pixel_format: PixelFormat,
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
    /// Whether semantic inference actually ran for this occurrence.
    pub semantic_processed: bool,
}

impl AmbientObservation {
    /// Whether this record may enter bounded recent-history persistence.
    ///
    /// Only `RecentHistory` is persistable; `Ephemeral` is deliberately not an
    /// alias for short persistent storage.
    pub fn persistence_allowed(&self) -> bool {
        self.sensitivity != Sensitivity::NeverObserve
            && self.retention == RetentionClass::RecentHistory
    }

    /// Whether this occurrence may become input to automatic memory consolidation.
    ///
    /// This does not promote anything. A3/A4 must still enforce evidence,
    /// recurrence, contradiction, epistemic-strength, and confirmation policy.
    pub fn automatic_memory_candidate_allowed(&self) -> bool {
        self.persistence_allowed() && self.sensitivity.automatic_memory_candidate_allowed()
    }

    /// Build an insertion cache key only for semantically processed eligible observations.
    pub fn semantic_cache_key(&self, encoder_revision: impl Into<String>) -> Option<SemanticCacheKey> {
        if !self.semantic_processed || !self.sensitivity.reusable_semantic_cache_allowed() {
            return None;
        }
        Some(SemanticCacheKey {
            observation_key: self.key.clone(),
            content_digest: self.content_digest,
            frame_width: self.frame_width,
            frame_height: self.frame_height,
            pixel_format: self.pixel_format,
            encoder_revision: encoder_revision.into(),
        })
    }

    /// Whether the record obeys the hard `never_observe` privacy invariant.
    pub fn privacy_consistent(&self) -> bool {
        self.sensitivity != Sensitivity::NeverObserve
            || (!self.semantic_processed
                && self.semantic_summary.is_none()
                && self.embedding_ref.is_none()
                && self.retention == RetentionClass::DoNotPersist)
    }

    /// Whether provenance fields are complete enough for temporal evidence citation.
    pub fn provenance_complete(&self) -> bool {
        !self.observation_id.trim().is_empty()
            && !self.source.trim().is_empty()
            && self.timestamp_unix_ms > 0
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
    /// Change-gate display/region states evicted at the configured cardinality cap.
    pub gate_state_evictions: u64,
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

fn perceptual_hash(image: &RgbaImage) -> ([u64; 3], u8) {
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
    (hash, mean as u8)
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
    fn unchanged_frames_are_deduplicated_before_fallback() {
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
    fn unchanged_frame_is_refreshed_after_fallback_interval() {
        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(5));
        let key = ObservationKey::display("display-1");
        let fingerprint = FrameFingerprint::from_rgba(&solid(32, 18, 10));
        gate.evaluate_at(key.clone(), fingerprint, now);
        let decision = gate.evaluate_at(key, fingerprint, now + Duration::from_secs(6));
        assert!(decision.should_encode);
        assert_eq!(decision.reason, GateReason::FallbackSample);
        assert_eq!(decision.change_score, 0.0);
    }

    #[test]
    fn meaningful_structural_changes_trigger_semantic_processing() {
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
    fn uniform_global_luminance_change_is_meaningful() {
        let dark = FrameFingerprint::from_rgba(&solid(32, 18, 0));
        let bright = FrameFingerprint::from_rgba(&solid(32, 18, 255));
        assert_eq!(dark.perceptual_hash, bright.perceptual_hash);
        assert_eq!(dark.perceptual_distance(bright), 1.0);

        let now = Instant::now();
        let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
        let key = ObservationKey::display("display-1");
        gate.evaluate_at(key.clone(), dark, now);
        let decision = gate.evaluate_at(key, bright, now + Duration::from_millis(20));
        assert_eq!(decision.reason, GateReason::MeaningfulChange);
    }

    #[test]
    fn frame_shape_change_cannot_alias_content_identity() {
        let landscape = FrameFingerprint::from_rgba(&solid(4, 2, 30));
        let portrait = FrameFingerprint::from_rgba(&solid(2, 4, 30));
        assert_ne!(landscape.content_digest, portrait.content_digest);
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
    fn gate_state_is_bounded_lru_and_reports_eviction() {
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
        assert_eq!(gate.evicted_keys(), 1);
        let decision = gate.evaluate_at(
            ObservationKey::display("display-1"),
            fingerprint,
            now + Duration::from_millis(3),
        );
        assert_eq!(decision.reason, GateReason::FirstObservation);
        assert_eq!(gate.evicted_keys(), 2);
    }

    #[test]
    fn low_change_content_uses_fallback_sampling() {
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
    fn checked_cache_identity_binds_revision_shape_format_and_sensitivity() {
        let fingerprint = FrameFingerprint::from_rgba(&solid(32, 18, 7));
        let key = ObservationKey::display("display-1");
        let v1 = SemanticCacheKey::for_frame(
            Sensitivity::CloudAllowed,
            key.clone(),
            fingerprint,
            "encoder-v1",
        )
        .expect("cloud-allowed frame should be cacheable");
        let v2 = SemanticCacheKey::for_frame(
            Sensitivity::CloudAllowed,
            key.clone(),
            fingerprint,
            "encoder-v2",
        )
        .expect("cloud-allowed frame should be cacheable");
        assert_ne!(v1, v2);
        assert_eq!(v2.encoder_revision(), "encoder-v2");
        assert_eq!(v2.content_digest(), &fingerprint.content_digest);
        assert!(SemanticCacheKey::for_frame(Sensitivity::LocalOnly, key.clone(), fingerprint, "v").is_none());
        assert!(SemanticCacheKey::for_frame(Sensitivity::NeverObserve, key, fingerprint, "v").is_none());
    }

    #[test]
    fn persistence_matrix_is_explicit_for_all_sensitivity_and_retention_pairs() {
        for sensitivity in [
            Sensitivity::CloudAllowed,
            Sensitivity::LocalOnly,
            Sensitivity::NeverObserve,
        ] {
            for retention in [
                RetentionClass::Ephemeral,
                RetentionClass::RecentHistory,
                RetentionClass::DoNotPersist,
            ] {
                let mut observation = synthetic_observation("obs-matrix");
                observation.sensitivity = sensitivity;
                observation.retention = retention;
                let expected = sensitivity != Sensitivity::NeverObserve
                    && retention == RetentionClass::RecentHistory;
                assert_eq!(
                    observation.persistence_allowed(),
                    expected,
                    "unexpected persistence for {sensitivity:?}/{retention:?}"
                );
                let expected_memory = expected && sensitivity == Sensitivity::CloudAllowed;
                assert_eq!(
                    observation.automatic_memory_candidate_allowed(),
                    expected_memory,
                    "unexpected memory admission for {sensitivity:?}/{retention:?}"
                );
            }
        }
    }

    #[test]
    fn never_observe_requires_no_semantics_and_no_persistence() {
        let mut observation = synthetic_observation("obs-private");
        observation.sensitivity = Sensitivity::NeverObserve;
        observation.retention = RetentionClass::DoNotPersist;
        observation.semantic_summary = None;
        observation.embedding_ref = None;
        observation.semantic_processed = false;
        assert!(!observation.persistence_allowed());
        assert!(!observation.automatic_memory_candidate_allowed());
        assert!(observation.semantic_cache_key("encoder-v1").is_none());
        assert!(observation.privacy_consistent());

        observation.semantic_processed = true;
        assert!(!observation.privacy_consistent());
    }

    #[test]
    fn identical_content_occurrences_keep_distinct_evidence_ids_and_timestamps() {
        let mut first = synthetic_observation("obs-1");
        let mut second = first.clone();
        second.observation_id = "obs-2".into();
        second.timestamp_unix_ms += 1_000;
        assert_eq!(first.content_digest, second.content_digest);
        assert_ne!(first.observation_id, second.observation_id);
        assert_ne!(first.timestamp_unix_ms, second.timestamp_unix_ms);
        assert!(first.provenance_complete());
        assert!(second.provenance_complete());
    }

    #[test]
    fn observation_roundtrip_preserves_provenance_timestamp_and_confidence() {
        let observation = synthetic_observation("obs-roundtrip");
        let encoded = serde_json::to_vec(&observation).expect("serialize observation");
        let decoded: AmbientObservation =
            serde_json::from_slice(&encoded).expect("deserialize observation");
        assert_eq!(decoded.observation_id, observation.observation_id);
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
    fn metrics_report_dedup_cache_and_gate_eviction_rates() {
        let metrics = PerceptionMetrics {
            frames_captured: 100,
            exact_duplicates_discarded: 70,
            low_change_discarded: 20,
            semantic_inferences: 8,
            semantic_cache_hits: 2,
            gate_state_evictions: 3,
            ..PerceptionMetrics::default()
        };
        assert!((metrics.deduplication_rate() - 0.9).abs() < f32::EPSILON);
        assert!((metrics.semantic_cache_hit_rate() - 0.2).abs() < f32::EPSILON);
        assert_eq!(metrics.gate_state_evictions, 3);
    }

    fn synthetic_observation(observation_id: &str) -> AmbientObservation {
        let fingerprint = FrameFingerprint::from_rgba(&solid(32, 18, 42));
        AmbientObservation {
            schema_version: OBSERVATION_SCHEMA_VERSION,
            observation_id: observation_id.into(),
            timestamp_unix_ms: 1_786_317_600_123,
            key: ObservationKey::region("display-2", 10, 20, 300, 200),
            content_digest: fingerprint.content_digest,
            frame_width: fingerprint.width,
            frame_height: fingerprint.height,
            pixel_format: fingerprint.pixel_format,
            change_score: 0.42,
            semantic_summary: Some("synthetic editor window".into()),
            embedding_ref: Some("embedding:synthetic:9".into()),
            confidence: 0.73,
            source: "screen:synthetic-test".into(),
            sensitivity: Sensitivity::CloudAllowed,
            retention: RetentionClass::RecentHistory,
            semantic_processed: true,
        }
    }
}
