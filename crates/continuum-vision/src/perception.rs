//! Efficient ambient-perception primitives shared by capture and context consumers.
//!
//! Platform capture stays in `continuum_core::senses::vision`. This module only
//! defines cheap deterministic fingerprints/change gating plus compact, privacy-
//! classified contracts for downstream temporal context, health, UI, and memory.
//! It intentionally does **not** own a production cache; A7's bounded shared cache
//! is the authoritative reuse layer.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use image::{imageops::FilterType, DynamicImage, RgbaImage};
use serde::{Deserialize, Serialize};

/// Stable schema version for compact ambient screen observations.
pub const OBSERVATION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    CloudAllowed,
    LocalOnly,
    NeverObserve,
}

impl Sensitivity {
    pub fn reusable_semantic_cache_allowed(self) -> bool {
        self == Self::CloudAllowed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    Ephemeral,
    RecentHistory,
    DoNotPersist,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PerceptionHealth {
    Observing,
    Paused,
    PermissionRequired { reason: String },
    EncoderUnavailable { reason: String },
    Degraded { reason: String },
    Processing,
    HistoricalCaptureDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameFingerprint {
    pub exact_hash: u64,
    pub perceptual_hash: [u64; 3],
}

impl FrameFingerprint {
    pub fn from_rgba(image: &RgbaImage) -> Self {
        Self {
            exact_hash: fnv1a64(image.as_raw()),
            perceptual_hash: perceptual_hash(image),
        }
    }

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObservationKey {
    pub display_id: String,
    pub region: Option<(u32, u32, u32, u32)>,
}

impl ObservationKey {
    pub fn display(display_id: impl Into<String>) -> Self {
        Self {
            display_id: display_id.into(),
            region: None,
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    FirstObservation,
    ExactDuplicate,
    BelowThreshold,
    MeaningfulChange,
    FallbackSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GateDecision {
    pub should_encode: bool,
    pub change_score: f32,
    pub reason: GateReason,
}

#[derive(Debug, Clone, Copy)]
struct GateState {
    fingerprint: FrameFingerprint,
    last_semantic_at: Instant,
}

#[derive(Debug)]
pub struct ChangeGate {
    threshold: f32,
    fallback_after: Duration,
    state: HashMap<ObservationKey, GateState>,
}

impl ChangeGate {
    pub fn new(threshold: f32, fallback_after: Duration) -> Self {
        Self {
            threshold: threshold.clamp(0.0, 1.0),
            fallback_after,
            state: HashMap::new(),
        }
    }

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

    pub fn invalidate_display(&mut self, display_id: &str) {
        self.state.retain(|key, _| key.display_id != display_id);
    }

    pub fn tracked_keys(&self) -> usize {
        self.state.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SemanticCacheKey {
    pub observation_key: ObservationKey,
    pub exact_hash: u64,
    pub encoder_revision: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbientObservation {
    pub schema_version: u16,
    pub timestamp_unix_ms: i64,
    pub key: ObservationKey,
    pub content_hash: u64,
    pub change_score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_ref: Option<String>,
    pub confidence: f32,
    pub source: String,
    pub sensitivity: Sensitivity,
    pub retention: RetentionClass,
    pub semantic_processed: bool,
}

impl AmbientObservation {
    pub fn persistence_allowed(&self) -> bool {
        self.sensitivity != Sensitivity::NeverObserve
            && self.retention != RetentionClass::DoNotPersist
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerceptionMetrics {
    pub frames_captured: u64,
    pub exact_duplicates_discarded: u64,
    pub low_change_discarded: u64,
    pub semantic_inferences: u64,
    pub semantic_cache_hits: u64,
    pub buffered_frames_dropped: u64,
    pub last_capture_latency_us: u64,
    pub last_change_detection_latency_us: u64,
    pub last_inference_latency_us: u64,
    pub last_searchable_latency_us: u64,
}

impl PerceptionMetrics {
    pub fn deduplication_rate(&self) -> f32 {
        if self.frames_captured == 0 {
            return 0.0;
        }
        (self.exact_duplicates_discarded + self.low_change_discarded) as f32
            / self.frames_captured as f32
    }

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
    fn restricted_observations_are_not_reusable_cache_material() {
        assert!(Sensitivity::CloudAllowed.reusable_semantic_cache_allowed());
        assert!(!Sensitivity::LocalOnly.reusable_semantic_cache_allowed());
        assert!(!Sensitivity::NeverObserve.reusable_semantic_cache_allowed());
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
        assert_eq!(decoded, observation);
    }

    #[test]
    fn health_contract_reports_degraded_encoder_state() {
        let health = PerceptionHealth::EncoderUnavailable {
            reason: "synthetic model not loaded".into(),
        };
        let json = serde_json::to_string(&health).expect("serialize health");
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
}
