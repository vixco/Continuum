//! Full audio pipeline implementation (requires `audio` Cargo feature).
//!
//! Captures microphone audio via `cpal`, detects speech segments with an
//! energy-based VAD, resamples to 16 kHz with `rubato`, and transcribes
//! via `whisper-rs`.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use rubato::audioadapter::Adapter;
use rubato::audioadapter_buffers::direct::SequentialSliceOfSlices;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use tokio::sync::mpsc as tokio_mpsc;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::config::AudioConfig;
use crate::senses::types::AudioObservation;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const TARGET_SAMPLE_RATE: u32 = 16_000;
const VAD_CHUNK_SAMPLES: usize = 512;
const RESAMPLER_CHUNK_SIZE: usize = 1024;

/// Rolling window (in VAD chunks) over which the adaptive VAD averages
/// silence RMS to compute the noise floor. 312 chunks × 32 ms ≈ 10 seconds.
const VAD_NOISE_WINDOW_CHUNKS: u64 = 312;

// ---------------------------------------------------------------------------
// Adaptive energy-based VAD
// ---------------------------------------------------------------------------

/// Self-calibrating voice activity detector.
///
/// Tracks a rolling 10-second window of RMS values from chunks that were
/// classified as silence, and sets the speech threshold to
/// `max(floor, noise_floor_mean × multiplier)`. A quiet laptop mic
/// (ambient ≈ 0.0005) uses the floor (0.005); a hot USB mic lifts the
/// threshold naturally as its ambient chunks flow into the ring.
///
/// **No warmup bootstrap.** Only chunks that were actually classified as
/// silence feed the noise floor. A previous revision had a 1-second warmup
/// that unconditionally pushed every chunk — this meant a user who spoke
/// during the first second contaminated the noise floor with speech RMS,
/// and the threshold would stay poisoned at ~0.2 for the next 10 seconds
/// (locking real speech out). Without the warmup, speech RMS never enters
/// the ring, so the threshold can only rise when genuine silence does.
struct AdaptiveVad {
    /// Absolute minimum threshold, regardless of noise floor. Protects against
    /// pathologically quiet rooms where noise_floor × multiplier would fall
    /// below typical speech RMS.
    floor: f32,
    /// Multiplier applied to the rolling noise floor. Speech must exceed
    /// `noise_floor × multiplier` to be detected.
    multiplier: f32,
    /// Number of consecutive silence chunks required to end a speech segment.
    silence_chunks_needed: usize,
    /// Maximum number of samples in a single speech segment before forced split.
    max_segment_samples: usize,
    /// Rolling window of `(chunk_index, rms)` entries classified as silence.
    /// Older entries are evicted when they fall outside the noise window.
    silence_ring: VecDeque<(u64, f32)>,
    /// Monotonically increasing chunk counter used for ring-eviction timing.
    current_chunk: u64,
    /// Most recently computed threshold. Cached here so the outer loop can
    /// include it in trace logs without re-running the mean.
    last_threshold: f32,
}

/// Result of feeding a chunk to the VAD.
#[derive(Debug, Clone, PartialEq)]
enum VadDecision {
    /// No speech detected yet, or still accumulating silence.
    Silence,
    /// Speech is active; keep accumulating.
    Speech,
    /// A speech segment just ended; the accumulated buffer is ready for
    /// transcription.
    SegmentComplete,
    /// The speech buffer hit the maximum length and was force-split.
    SegmentForceSplit,
}

/// Tracks the running state of the VAD across consecutive chunks.
struct VadState {
    /// Whether speech is currently active.
    speech_active: bool,
    /// Number of consecutive silence chunks since the last speech chunk.
    consecutive_silence_chunks: usize,
    /// Accumulated speech samples for the current segment.
    speech_buffer: Vec<f32>,
}

impl VadState {
    /// Creates a new, empty VAD state.
    fn new() -> Self {
        Self {
            speech_active: false,
            consecutive_silence_chunks: 0,
            speech_buffer: Vec::new(),
        }
    }

    /// Takes the accumulated speech buffer and resets the state for the next
    /// segment. Returns the buffer.
    fn take_segment(&mut self) -> Vec<f32> {
        self.speech_active = false;
        self.consecutive_silence_chunks = 0;
        std::mem::take(&mut self.speech_buffer)
    }
}

impl AdaptiveVad {
    /// Creates a new adaptive VAD. No warmup — see the struct doc for why.
    fn new(floor: f32, multiplier: f32, silence_duration_ms: u64, max_segment_secs: u64) -> Self {
        let chunk_duration_ms = (VAD_CHUNK_SAMPLES as u64 * 1000) / u64::from(TARGET_SAMPLE_RATE);
        let silence_chunks_needed = if chunk_duration_ms > 0 {
            (silence_duration_ms / chunk_duration_ms).max(1) as usize
        } else {
            1
        };
        let max_segment_samples = (max_segment_secs * u64::from(TARGET_SAMPLE_RATE)) as usize;
        // Floor below ~0.0001 is below the noise floor of any real microphone
        // and would make the VAD trigger constantly. Clamp defensively.
        let floor = floor.max(0.0001);
        // Multiplier < 1 means threshold drops below the noise floor — the
        // VAD would classify its own silence as speech. Clamp to 1.
        let multiplier = multiplier.max(1.0);

        Self {
            floor,
            multiplier,
            silence_chunks_needed,
            max_segment_samples,
            silence_ring: VecDeque::with_capacity(VAD_NOISE_WINDOW_CHUNKS as usize),
            current_chunk: 0,
            last_threshold: floor,
        }
    }

    /// Computes the RMS energy of a slice of f32 samples. Returns 0.0 for
    /// empty slices.
    fn rms_energy(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }

    /// Mean RMS of the chunks currently in the noise window. 0.0 when the
    /// window is empty (fresh VAD before any silence chunk has been observed).
    fn noise_floor(&self) -> f32 {
        if self.silence_ring.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.silence_ring.iter().map(|(_, rms)| *rms).sum();
        sum / self.silence_ring.len() as f32
    }

    /// Effective speech threshold at the current moment. Public for the outer
    /// loop's periodic stats log.
    fn current_threshold(&self) -> f32 {
        (self.noise_floor() * self.multiplier).max(self.floor)
    }

    /// Feeds a chunk of exactly [`VAD_CHUNK_SAMPLES`] samples into the VAD,
    /// updates the rolling noise floor, and returns a decision.
    ///
    /// Callers must maintain a [`VadState`] across calls and pass it here.
    fn feed_chunk(&mut self, chunk: &[f32], state: &mut VadState) -> VadDecision {
        let rms = Self::rms_energy(chunk);
        self.current_chunk += 1;

        // Evict silence samples that have fallen outside the rolling window.
        let oldest_allowed = self.current_chunk.saturating_sub(VAD_NOISE_WINDOW_CHUNKS);
        while let Some(&(idx, _)) = self.silence_ring.front() {
            if idx < oldest_allowed {
                self.silence_ring.pop_front();
            } else {
                break;
            }
        }

        let threshold = self.current_threshold();
        self.last_threshold = threshold;
        let is_speech = rms > threshold;

        // Only chunks classified as silence contribute to the noise floor —
        // this is the invariant that keeps speech RMS out of the estimate.
        // On the very first chunks the ring is empty (noise_floor = 0 →
        // threshold = floor); if audio is genuine silence the ring fills
        // naturally, if audio is loud speech from the start nothing is
        // pushed and the threshold stays at the floor, which is correct.
        if !is_speech {
            self.silence_ring.push_back((self.current_chunk, rms));
        }

        if is_speech {
            let just_started = !state.speech_active;
            state.speech_active = true;
            state.consecutive_silence_chunks = 0;
            state.speech_buffer.extend_from_slice(chunk);

            if just_started {
                tracing::info!(
                    layer = "senses",
                    component = "audio",
                    rms = rms,
                    threshold = threshold,
                    noise_floor = self.noise_floor(),
                    "Speech segment started"
                );
            }

            if state.speech_buffer.len() >= self.max_segment_samples {
                tracing::debug!(
                    layer = "senses",
                    component = "audio",
                    buffer_samples = state.speech_buffer.len(),
                    max_samples = self.max_segment_samples,
                    "Force-splitting speech segment at max length"
                );
                return VadDecision::SegmentForceSplit;
            }

            VadDecision::Speech
        } else if state.speech_active {
            // Still include the silence chunk in the buffer so whisper gets
            // the trailing context.
            state.speech_buffer.extend_from_slice(chunk);
            state.consecutive_silence_chunks += 1;

            // Also check force-split during silence accumulation.
            if state.speech_buffer.len() >= self.max_segment_samples {
                tracing::debug!(
                    layer = "senses",
                    component = "audio",
                    buffer_samples = state.speech_buffer.len(),
                    max_samples = self.max_segment_samples,
                    "Force-splitting speech segment at max length (during silence)"
                );
                return VadDecision::SegmentForceSplit;
            }

            if state.consecutive_silence_chunks >= self.silence_chunks_needed {
                VadDecision::SegmentComplete
            } else {
                VadDecision::Speech
            }
        } else {
            VadDecision::Silence
        }
    }
}

// ---------------------------------------------------------------------------
// Resampling
// ---------------------------------------------------------------------------

/// Resamples a buffer of interleaved f32 samples from `source_rate` to
/// [`TARGET_SAMPLE_RATE`].
///
/// If the source is stereo (or more), channels are first mixed down to mono
/// by averaging. Then `rubato::SincFixedIn` performs high-quality sinc
/// interpolation.
///
/// # Errors
///
/// Returns an error if the resampler cannot be created or if processing fails.
fn resample_to_16khz(samples: &[f32], source_rate: u32, source_channels: u16) -> Result<Vec<f32>> {
    if source_rate == TARGET_SAMPLE_RATE && source_channels == 1 {
        return Ok(samples.to_vec());
    }

    // Mix down to mono if needed.
    let mono: Vec<f32> = if source_channels > 1 {
        let ch = source_channels as usize;
        samples
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    } else {
        samples.to_vec()
    };

    // If already at the target rate after mono mixdown, return directly.
    if source_rate == TARGET_SAMPLE_RATE {
        return Ok(mono);
    }

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let ratio = f64::from(TARGET_SAMPLE_RATE) / f64::from(source_rate);

    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        2.0,
        &params,
        RESAMPLER_CHUNK_SIZE,
        1, // mono
        FixedAsync::Input,
    )
    .context("Failed to create resampler")?;

    let mut output = Vec::with_capacity((mono.len() as f64 * ratio) as usize + 1024);

    // Process in chunks of RESAMPLER_CHUNK_SIZE.
    for chunk in mono.chunks(RESAMPLER_CHUNK_SIZE) {
        let input_chunk = if chunk.len() < RESAMPLER_CHUNK_SIZE {
            // Pad the last chunk with zeros to meet the resampler's expected size.
            let mut padded = chunk.to_vec();
            padded.resize(RESAMPLER_CHUNK_SIZE, 0.0);
            padded
        } else {
            chunk.to_vec()
        };

        let input_slice: &[f32] = &input_chunk;
        let input_slices: &[&[f32]] = &[input_slice];
        let input_adapter = SequentialSliceOfSlices::new(input_slices, 1, input_chunk.len())
            .context("Failed to create input adapter")?;
        let result = resampler
            .process(&input_adapter, 0, None)
            .context("Resampler processing failed")?;

        let frames = result.frames();
        let channel_data = result.take_data();
        // Only keep output proportional to the actual input length
        // (not the zero-padded portion) for the last chunk.
        if chunk.len() < RESAMPLER_CHUNK_SIZE {
            let actual_output_len =
                (frames as f64 * chunk.len() as f64 / RESAMPLER_CHUNK_SIZE as f64) as usize;
            output.extend_from_slice(&channel_data[..actual_output_len.min(channel_data.len())]);
        } else {
            output.extend_from_slice(&channel_data[..frames]);
        }
    }

    Ok(output)
}

/// Returns a human-readable name for a device via the non-deprecated
/// `description()` API, falling back to `<unnamed>` on error.
fn device_display_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".to_string())
}

// ---------------------------------------------------------------------------
// AudioWatcher
// ---------------------------------------------------------------------------

/// Watches the microphone and produces transcripts of speech segments.
///
/// Holds the audio configuration and a loaded whisper context. The whisper
/// model is loaded once at construction time and reused for every segment.
///
/// # Layer
///
/// Layer 1 -- Senses. This component captures raw audio, detects speech via
/// energy-based VAD, and transcribes via whisper. It pushes
/// [`AudioObservation`] values upward to the frame builder.
///
/// # Self-healing
///
/// The watcher logs all events with `layer = "senses"` and
/// `component = "audio"`. If the microphone or whisper model fails, the error
/// is logged and the watcher continues (or disables itself gracefully). The
/// repair agent can detect prolonged failures and restart the component.
pub struct AudioWatcher {
    /// Audio pipeline configuration.
    config: AudioConfig,
    /// Loaded whisper context, wrapped in `Arc` so it can be sent to blocking
    /// tasks. `None` if the model failed to load (degraded mode).
    whisper_ctx: Option<Arc<WhisperContext>>,
}

impl AudioWatcher {
    /// Creates a new audio watcher, loading the whisper model from disk.
    ///
    /// If the whisper model file does not exist or fails to load, the watcher
    /// is created in degraded mode (no transcription). A warning is logged and
    /// the `run` loop will still perform VAD but skip transcription.
    ///
    /// # Arguments
    ///
    /// * `config` - Audio pipeline settings (thresholds, model path, etc.).
    pub fn new(config: AudioConfig) -> Self {
        if !config.enabled {
            tracing::info!(
                layer = "senses",
                component = "audio",
                "Audio watcher disabled by configuration"
            );
            return Self {
                config,
                whisper_ctx: None,
            };
        }

        let whisper_ctx =
            match Self::load_whisper_model(&config.whisper_model_path, config.whisper_use_gpu) {
                Ok(ctx) => {
                    tracing::info!(
                        layer = "senses",
                        component = "audio",
                        model_path = %config.whisper_model_path,
                        "Whisper model loaded successfully"
                    );
                    Some(Arc::new(ctx))
                }
                Err(err) => {
                    tracing::warn!(
                        layer = "senses",
                        component = "audio",
                        model_path = %config.whisper_model_path,
                        error = %err,
                        "Failed to load whisper model, running in degraded mode (no transcription)"
                    );
                    None
                }
            };

        Self {
            config,
            whisper_ctx,
        }
    }

    /// Loads a whisper model from the given file path.
    fn load_whisper_model(model_path: &str, use_gpu: bool) -> Result<WhisperContext> {
        let mut params = WhisperContextParameters::default();
        // Only effective when Continuum is built with the `cuda` feature;
        // otherwise whisper.cpp falls back to CPU. Honours the resolved
        // resource plan's GPU decision.
        params.use_gpu = use_gpu;
        let ctx = WhisperContext::new_with_params(model_path, params)
            .context("Failed to initialize WhisperContext")?;
        Ok(ctx)
    }

    /// Runs the audio capture and transcription loop until shutdown.
    ///
    /// This is the main entry point. It:
    /// 1. Opens the default input device.
    /// 2. Starts an audio stream that pushes samples into a channel.
    /// 3. Runs the VAD processing loop, detecting speech segments.
    /// 4. Transcribes completed segments via whisper.
    /// 5. Sends [`AudioObservation`] values through `tx`.
    ///
    /// The loop exits when `shutdown` receives `true` or when the observation
    /// channel closes.
    ///
    /// # Arguments
    ///
    /// * `tx` - Channel sender for completed audio observations.
    /// * `shutdown` - Watch receiver; the loop exits when this becomes `true`.
    pub async fn run(
        &self,
        tx: tokio_mpsc::Sender<AudioObservation>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        if !self.config.enabled {
            tracing::info!(
                layer = "senses",
                component = "audio",
                "Audio watcher is disabled, exiting run loop"
            );
            // Park until shutdown so the task does not busy-loop.
            let _ = shutdown.changed().await;
            return;
        }

        // --- Open audio device ---
        let (sample_rx, native_rate, native_channels, _stream) = match self.open_audio_stream() {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    layer = "senses",
                    component = "audio",
                    error = %err,
                    "Failed to open audio input device, disabling audio watcher"
                );
                let _ = shutdown.changed().await;
                return;
            }
        };

        tracing::info!(
            layer = "senses",
            component = "audio",
            native_rate = native_rate,
            native_channels = native_channels,
            vad_threshold = self.config.vad_threshold,
            silence_duration_ms = self.config.silence_duration_ms,
            max_segment_secs = self.config.max_segment_secs,
            "Audio watcher started"
        );

        let needs_resample = native_rate != TARGET_SAMPLE_RATE || native_channels != 1;

        let mut vad = AdaptiveVad::new(
            self.config.vad_threshold,
            self.config.vad_noise_floor_multiplier,
            self.config.silence_duration_ms,
            self.config.max_segment_secs,
        );
        let mut vad_state = VadState::new();

        // Two separate buffers to prevent sample-format mixing:
        // - `raw_buffer` holds native-rate / native-channels samples from cpal
        //   (typically 48 kHz stereo on Windows) until we resample.
        // - `vad_buffer` holds post-resample 16 kHz mono samples waiting to
        //   be sliced into fixed-size VAD chunks. The leftover (<512 samples)
        //   from each resample pass is preserved HERE, not pushed back into
        //   `raw_buffer` — doing that corrupted subsequent resamples and
        //   caused ~75 % of the audio to be dropped.
        let mut raw_buffer: Vec<f32> = Vec::new();
        let mut vad_buffer: Vec<f32> = Vec::new();

        // --- DIAGNOSTIC: periodic RMS / stats tracking ---
        let mut diag_stats_last_log = Instant::now();
        let mut diag_buffers_seen: u64 = 0;
        let mut diag_samples_seen: u64 = 0;
        let mut diag_peak_rms: f32 = 0.0;
        let mut diag_speech_chunks: u64 = 0;
        let mut diag_silence_chunks: u64 = 0;

        loop {
            // Check shutdown.
            if *shutdown.borrow() {
                tracing::info!(
                    layer = "senses",
                    component = "audio",
                    "Shutdown signal received, stopping audio watcher"
                );
                break;
            }

            // Drain all available samples from the audio callback channel.
            // We use try_recv in a loop to avoid blocking the async runtime.
            let mut drained = false;
            loop {
                match sample_rx.try_recv() {
                    Ok(samples) => {
                        // --- DIAGNOSTIC: log RMS of every buffer that reaches us ---
                        let rms = if samples.is_empty() {
                            0.0
                        } else {
                            let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
                            (sum_sq / samples.len() as f32).sqrt()
                        };
                        diag_buffers_seen += 1;
                        diag_samples_seen += samples.len() as u64;
                        if rms > diag_peak_rms {
                            diag_peak_rms = rms;
                        }
                        tracing::trace!(
                            layer = "senses",
                            component = "audio",
                            buffer_samples = samples.len(),
                            rms = rms,
                            "Raw audio buffer received from cpal"
                        );
                        raw_buffer.extend(samples);
                        drained = true;
                    }
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => {
                        tracing::warn!(
                            layer = "senses",
                            component = "audio",
                            "Audio sample channel disconnected, stopping audio watcher"
                        );
                        return;
                    }
                }
            }

            // --- DIAGNOSTIC: every 2s, log aggregate audio stats ---
            if diag_stats_last_log.elapsed() >= Duration::from_secs(2) {
                tracing::info!(
                    layer = "senses",
                    component = "audio",
                    buffers = diag_buffers_seen,
                    samples = diag_samples_seen,
                    peak_rms = diag_peak_rms,
                    vad_floor = self.config.vad_threshold,
                    vad_threshold_now = vad.last_threshold,
                    noise_floor = vad.noise_floor(),
                    speech_chunks = diag_speech_chunks,
                    silence_chunks = diag_silence_chunks,
                    "Audio stats (last 2s window)"
                );
                diag_stats_last_log = Instant::now();
                diag_buffers_seen = 0;
                diag_samples_seen = 0;
                diag_peak_rms = 0.0;
                diag_speech_chunks = 0;
                diag_silence_chunks = 0;
            }

            // If we got no new samples, yield briefly and try again.
            if !drained {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    _ = shutdown.changed() => {
                        tracing::info!(
                            layer = "senses",
                            component = "audio",
                            "Shutdown signal received during idle, stopping audio watcher"
                        );
                        return;
                    }
                }
                continue;
            }

            // Resample if needed (converts to 16 kHz mono) and append the
            // output to the post-resample VAD buffer.
            if needs_resample {
                let to_resample = std::mem::take(&mut raw_buffer);
                match resample_to_16khz(&to_resample, native_rate, native_channels) {
                    Ok(resampled) => vad_buffer.extend(resampled),
                    Err(err) => {
                        tracing::warn!(
                            layer = "senses",
                            component = "audio",
                            error = %err,
                            "Resampling failed, skipping audio chunk"
                        );
                        continue;
                    }
                }
            } else {
                vad_buffer.append(&mut raw_buffer);
            }

            // Feed samples through the VAD in chunks. Processed chunks are
            // drained from `vad_buffer`; the tail remains for the next pass.
            let mut offset = 0;
            while offset + VAD_CHUNK_SAMPLES <= vad_buffer.len() {
                let chunk = &vad_buffer[offset..offset + VAD_CHUNK_SAMPLES];
                offset += VAD_CHUNK_SAMPLES;

                // --- DIAGNOSTIC: compute chunk energy before feeding VAD ---
                let chunk_energy = AdaptiveVad::rms_energy(chunk);
                let decision = vad.feed_chunk(chunk, &mut vad_state);
                if chunk_energy > vad.last_threshold {
                    diag_speech_chunks += 1;
                } else {
                    diag_silence_chunks += 1;
                }
                tracing::trace!(
                    layer = "senses",
                    component = "audio",
                    chunk_rms = chunk_energy,
                    floor = self.config.vad_threshold,
                    threshold = vad.last_threshold,
                    noise_floor = vad.noise_floor(),
                    decision = ?decision,
                    buffer_samples = vad_state.speech_buffer.len(),
                    "VAD chunk decision"
                );

                match decision {
                    VadDecision::SegmentComplete | VadDecision::SegmentForceSplit => {
                        let segment = vad_state.take_segment();
                        if segment.is_empty() {
                            continue;
                        }

                        let duration_ms =
                            (segment.len() as u64 * 1000) / u64::from(TARGET_SAMPLE_RATE);

                        tracing::info!(
                            layer = "senses",
                            component = "audio",
                            duration_ms = duration_ms,
                            samples = segment.len(),
                            forced = (decision == VadDecision::SegmentForceSplit),
                            "Speech segment ended"
                        );

                        // Transcribe the segment.
                        match self.transcribe_segment(segment, duration_ms).await {
                            Ok(obs) => {
                                if tx.send(obs).await.is_err() {
                                    tracing::warn!(
                                        layer = "senses",
                                        component = "audio",
                                        "Observation channel closed, stopping audio watcher"
                                    );
                                    return;
                                }
                            }
                            Err(err) => {
                                tracing::warn!(
                                    layer = "senses",
                                    component = "audio",
                                    error = %err,
                                    "Whisper transcription failed, skipping segment"
                                );
                            }
                        }
                    }
                    VadDecision::Speech | VadDecision::Silence => {
                        // Continue accumulating.
                    }
                }
            }

            // Drop the chunks we just processed. Any tail that didn't fill
            // a complete VAD chunk stays in `vad_buffer` for the next pass.
            if offset > 0 {
                vad_buffer.drain(..offset);
            }
        }

        tracing::info!(
            layer = "senses",
            component = "audio",
            "Audio watcher stopped"
        );
    }

    /// Opens the default audio input device and starts a capture stream.
    ///
    /// Returns a tuple of:
    /// - The `std::sync::mpsc::Receiver` for raw f32 sample chunks.
    /// - The native sample rate of the device.
    /// - The native channel count of the device.
    /// - The `cpal::Stream` handle (must be kept alive to continue capture).
    ///
    /// # Errors
    ///
    /// Returns an error if no input device is found, the device does not
    /// support a usable configuration, or the stream cannot be built.
    fn open_audio_stream(&self) -> Result<(std_mpsc::Receiver<Vec<f32>>, u32, u16, cpal::Stream)> {
        let host = cpal::default_host();

        // --- DIAGNOSTIC: enumerate every available input device ---
        let default_id = host
            .default_input_device()
            .and_then(|d| d.id().ok())
            .map(|id| id.to_string());

        match host.input_devices() {
            Ok(iter) => {
                for (i, d) in iter.enumerate() {
                    let name = device_display_name(&d);
                    let id_str = d.id().ok().map(|id| id.to_string());
                    let (cfg_rate, cfg_ch, cfg_fmt) = match d.default_input_config() {
                        Ok(c) => (
                            c.sample_rate(),
                            c.channels(),
                            format!("{:?}", c.sample_format()),
                        ),
                        Err(e) => (0, 0, format!("err: {e}")),
                    };
                    let is_default = match (&id_str, &default_id) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    };
                    tracing::info!(
                        layer = "senses",
                        component = "audio",
                        idx = i,
                        name = %name,
                        id = id_str.as_deref().unwrap_or("<no id>"),
                        default_rate = cfg_rate,
                        channels = cfg_ch,
                        format = %cfg_fmt,
                        is_default = is_default,
                        "Available audio input device"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    layer = "senses",
                    component = "audio",
                    error = %e,
                    "Failed to enumerate audio input devices"
                );
            }
        }

        // Device selection: always use whatever Windows has marked as the
        // default recording device. This deliberately ignores any saved
        // `device_index` / `device_name` from earlier picker runs — the
        // picker fought with Windows' own format negotiation and produced
        // quieter audio than the default path. Users set their preferred
        // mic via Windows Sound settings → Input → "Set as default".
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No default audio input device found"))?;

        let device_name = device_display_name(&device);
        tracing::info!(
            layer = "senses",
            component = "audio",
            device = %device_name,
            reason = "Windows default input device",
            "Selected audio input device"
        );

        // Try to get a config that matches 16 kHz mono f32 first.
        let config = self.select_input_config(&device)?;

        let native_rate = config.sample_rate;
        let native_channels = config.channels;

        tracing::info!(
            layer = "senses",
            component = "audio",
            sample_rate = native_rate,
            channels = native_channels,
            "Audio stream configured"
        );

        let (sample_tx, sample_rx) = std_mpsc::sync_channel::<Vec<f32>>(64);

        let err_callback = |err: cpal::StreamError| {
            tracing::error!(
                layer = "senses",
                component = "audio",
                error = %err,
                "Audio stream error"
            );
        };

        let stream_config: StreamConfig = config;

        let stream = device
            .build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Send a copy of the samples. If the channel is full, drop
                    // the oldest data to avoid blocking the audio thread.
                    let _ = sample_tx.try_send(data.to_vec());
                },
                err_callback,
                None, // no timeout
            )
            .context("Failed to build audio input stream")?;

        stream.play().context("Failed to start audio stream")?;

        Ok((sample_rx, native_rate, native_channels, stream))
    }

    /// Selects the best input stream configuration for the given device.
    ///
    /// Prefers 16 kHz mono f32 to avoid resampling. Falls back to the
    /// device's default input configuration.
    fn select_input_config(&self, device: &cpal::Device) -> Result<StreamConfig> {
        // Try the preferred config first: 16 kHz, mono, f32.
        let preferred = StreamConfig {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };

        // Check if the device supports our preferred config by looking at
        // supported configs. If we find one that matches, use it.
        if let Ok(supported_configs) = device.supported_input_configs() {
            for range in supported_configs {
                if range.sample_format() == SampleFormat::F32
                    && range.channels() == 1
                    && range.min_sample_rate() <= TARGET_SAMPLE_RATE
                    && range.max_sample_rate() >= TARGET_SAMPLE_RATE
                {
                    tracing::debug!(
                        layer = "senses",
                        component = "audio",
                        "Device supports preferred 16kHz mono f32 config"
                    );
                    return Ok(preferred);
                }
            }
        }

        // Fall back to the default input config.
        let default_config = device
            .default_input_config()
            .context("Failed to get default input config")?;

        tracing::info!(
            layer = "senses",
            component = "audio",
            sample_rate = default_config.sample_rate(),
            channels = default_config.channels(),
            format = ?default_config.sample_format(),
            "Using default input config (will resample to 16kHz mono)"
        );

        Ok(StreamConfig {
            channels: default_config.channels(),
            sample_rate: default_config.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        })
    }

    /// Transcribes a speech segment using whisper.
    ///
    /// Runs on `tokio::task::spawn_blocking` because `whisper-rs` is synchronous.
    /// Returns an [`AudioObservation`] with the transcript, detected language,
    /// duration, and confidence.
    ///
    /// If the whisper context is `None` (degraded mode), returns an observation
    /// with an empty transcript and zero confidence.
    ///
    /// # Errors
    ///
    /// Returns an error if the whisper inference or state creation fails.
    async fn transcribe_segment(
        &self,
        samples: Vec<f32>,
        duration_ms: u64,
    ) -> Result<AudioObservation> {
        let whisper_ctx = match &self.whisper_ctx {
            Some(ctx) => Arc::clone(ctx),
            None => {
                tracing::debug!(
                    layer = "senses",
                    component = "audio",
                    "No whisper model loaded, returning empty observation"
                );
                return Ok(AudioObservation {
                    transcript: String::new(),
                    language: "unknown".to_string(),
                    duration_ms,
                    confidence: 0.0,
                    ts: Utc::now(),
                });
            }
        };

        let start = Instant::now();

        // --- DIAGNOSTIC: log what Whisper is about to receive ---
        let input_rms = if samples.is_empty() {
            0.0
        } else {
            let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
            (sum_sq / samples.len() as f32).sqrt()
        };

        // Normalize the segment to peak ±0.5 before whisper. Typical USB-mic
        // input lands around 0.01–0.05 peak — whisper's feature extraction
        // is trained on audio closer to ±0.3, and too-quiet input comes back
        // with empty transcripts even though speech is audible. Scaling to a
        // known peak is the standard preprocessing step for whisper.
        let mut samples = samples;
        let peak_abs = samples.iter().map(|&s| s.abs()).fold(0.0_f32, f32::max);
        let norm_scale = if peak_abs > 0.0 { 0.5 / peak_abs } else { 1.0 };
        if (norm_scale - 1.0).abs() > f32::EPSILON {
            for s in samples.iter_mut() {
                *s *= norm_scale;
            }
        }

        tracing::info!(
            layer = "senses",
            component = "audio",
            input_samples = samples.len(),
            input_duration_ms = duration_ms,
            input_rms = input_rms,
            peak_abs = peak_abs,
            norm_scale = norm_scale,
            "Whisper invocation starting"
        );

        // Clone the language string for move into the blocking task.
        let language = self.config.whisper_language.clone();
        // Threads come from the resolved resource plan (stored on the audio
        // config at boot); a fraction of logical cores with headroom rather
        // than a hardcoded 4. Copy out (i32 is `Copy`) so the blocking task
        // doesn't borrow `self`.
        let n_threads = self.config.whisper_threads;

        let observation = tokio::task::spawn_blocking(move || -> Result<AudioObservation> {
            let mut state = whisper_ctx
                .create_state()
                .context("Failed to create whisper state")?;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            // Force language unless explicitly set to "auto". Short VAD
            // segments (<2 s) are unreliable for whisper's auto-detector
            // and routinely come back as `[BLANK_AUDIO]` when it can't
            // commit to a language.
            if language == "auto" {
                params.set_language(Some("auto"));
            } else {
                params.set_language(Some(&language));
            }
            // Suppress the `[BLANK_AUDIO]` and `[MUSIC]` marker tokens —
            // we only want real transcribed text out. Without this,
            // whisper-small cheerfully emits `[BLANK_AUDIO]` for any clip
            // it's not confident about, even ones that are clearly speech.
            params.set_suppress_blank(true);
            params.set_suppress_nst(true);
            // Don't carry context from the previous segment. Each VAD
            // segment is an independent utterance; prior context can bias
            // the decoder into repeating earlier tokens.
            params.set_no_context(true);
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            // Threads come from the resolved resource plan (stored on the
            // audio config at boot); a fraction of logical cores with headroom
            // rather than a hardcoded 4.
            params.set_n_threads(n_threads);

            state
                .full(params, &samples)
                .context("Whisper inference failed")?;

            let num_segments = state.full_n_segments();

            let mut transcript = String::new();
            for i in 0..num_segments {
                match state.get_segment(i) {
                    Some(segment) => match segment.to_str_lossy() {
                        Ok(text) => {
                            if !transcript.is_empty() {
                                transcript.push(' ');
                            }
                            transcript.push_str(text.trim());
                        }
                        Err(err) => {
                            tracing::warn!(
                                layer = "senses",
                                component = "audio",
                                segment = i,
                                error = %err,
                                "Failed to get whisper segment text"
                            );
                        }
                    },
                    None => {
                        tracing::warn!(
                            layer = "senses",
                            component = "audio",
                            segment = i,
                            "Whisper segment not found"
                        );
                    }
                }
            }

            // Whisper does not expose a per-segment confidence score directly
            // in the public API. We use a heuristic: if the transcript is
            // non-empty, assign a default confidence. A future version could
            // parse token-level probabilities.
            let confidence = if transcript.is_empty() { 0.0 } else { 0.7 };

            // Detect language from whisper state. When the caller asked for
            // "auto", whisper fills in the detected language id during
            // inference; otherwise we trust the requested language so voice
            // routing downstream always has a concrete ISO-639-1 code.
            let language = if language == "auto" {
                let id = state.full_lang_id_from_state();
                whisper_rs::get_lang_str(id)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "en".to_string())
            } else {
                language.clone()
            };

            Ok(AudioObservation {
                transcript,
                language,
                duration_ms,
                confidence,
                ts: Utc::now(),
            })
        })
        .await
        .context("Whisper transcription task panicked")??;

        let elapsed = start.elapsed();

        // Drop known whisper hallucinations. On silence, music, or
        // low-SNR ambient noise, whisper-small/medium reliably emits a
        // fixed set of phrases ("Thanks for watching!", "you",
        // "Subtitles by the Amara.org community", etc.) because those
        // tokens dominate its training set. Emitting them poisons the
        // triage/follow-up flow.
        let observation = if looks_like_hallucination(&observation.transcript) {
            tracing::debug!(
                layer = "senses",
                component = "audio",
                transcript = %observation.transcript,
                "Dropped whisper hallucination"
            );
            AudioObservation {
                transcript: String::new(),
                language: observation.language,
                duration_ms: observation.duration_ms,
                confidence: 0.0,
                ts: observation.ts,
            }
        } else {
            observation
        };

        tracing::info!(
            layer = "senses",
            component = "audio",
            duration_ms = duration_ms,
            transcription_ms = elapsed.as_millis() as u64,
            transcript_len = observation.transcript.len(),
            transcript = %observation.transcript,
            "Whisper transcription complete"
        );

        Ok(observation)
    }

    /// Returns `true` if the audio watcher appears healthy.
    ///
    /// Checks that the watcher is enabled and (if so) that the whisper model
    /// is loaded. A future version could also verify the audio stream is active.
    pub fn is_healthy(&self) -> bool {
        if !self.config.enabled {
            // Disabled is a valid state, not unhealthy.
            return true;
        }
        self.whisper_ctx.is_some()
    }

    /// Returns `true` if the repair agent should restart this component.
    ///
    /// Returns `true` when the watcher is enabled but the whisper model
    /// failed to load, suggesting a restart might fix a transient issue.
    pub fn should_restart(&self) -> bool {
        self.config.enabled && self.whisper_ctx.is_none()
    }
}

/// Returns `true` if the transcript matches a known whisper hallucination.
///
/// Whisper models (especially whisper-small/medium) reliably emit a fixed
/// set of training-set phrases when fed silence, music, or low-SNR
/// ambient audio. These phrases are NOT what the user said — they're the
/// model's best guess when it has no real speech signal. Emitting them
/// to the triage layer poisons the follow-up voice session and causes
/// spurious orchestrator wakes.
///
/// This list captures the ones observed in production logs. Extend as
/// new ones show up. The check is intentionally narrow — we only drop
/// exact matches (case-insensitive, whitespace-trimmed) or tiny tokens
/// like bare "you"/"I" that can't carry meaningful commands.
pub(crate) fn looks_like_hallucination(transcript: &str) -> bool {
    let t = transcript.trim().to_lowercase();
    if t.is_empty() {
        return false; // empty isn't a hallucination; it's just silence
    }

    // Whisper's most common training-set parrot phrases.
    const EXACT_HALLUCINATIONS: &[&str] = &[
        "thanks for watching!",
        "thanks for watching.",
        "thanks for watching",
        "thank you for watching.",
        "thank you for watching",
        "thank you for watching!",
        "thank you.",
        "thank you!",
        "thank you",
        "bye!",
        "bye.",
        "bye",
        "please subscribe.",
        "please subscribe",
        "subscribe to my channel",
        "don't forget to subscribe",
        "subtitles by the amara.org community",
        "[music]",
        "♪ ♪",
        "♪♪",
    ];
    if EXACT_HALLUCINATIONS.iter().any(|h| t == *h) {
        return true;
    }

    // Extremely short single-word transcripts are usually whisper
    // guessing on silence. No real voice command is shorter than this.
    const TRIVIAL_WORDS: &[&str] = &["you", "i", "a", "the", "um", "uh", ".", "-", "!", "?"];
    if TRIVIAL_WORDS.iter().any(|w| t == *w) {
        return true;
    }

    // "Follow.", "Go.", single-verb dropouts — whisper stopping short.
    // We accept any utterance ≥ 4 characters OR ≥ 2 words; anything
    // shorter is too likely to be a mistranscription to act on.
    let words = t.split_whitespace().count();
    if t.chars().count() < 4 && words < 2 {
        return true;
    }

    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hallucination_filter_drops_known_parrot_phrases() {
        for phrase in &[
            "Thanks for watching!",
            "thanks for watching",
            "Thank you.",
            "you",
            "I",
            "Bye!",
            "Please subscribe.",
            "♪ ♪",
            "[Music]",
            "Subtitles by the Amara.org community",
        ] {
            assert!(looks_like_hallucination(phrase), "should drop: {phrase:?}");
        }
    }

    #[test]
    fn hallucination_filter_keeps_real_commands() {
        for phrase in &[
            "hey continuum what time is it",
            "open the build log",
            "what's on my screen",
            "tell me a joke",
            "wat staat er op mijn planning",
            "Hey Cairo, hello.",
        ] {
            assert!(!looks_like_hallucination(phrase), "should keep: {phrase:?}");
        }
    }

    #[test]
    fn hallucination_filter_keeps_empty() {
        // Empty isn't a hallucination — it's just "no speech found".
        // The calling code handles empty differently (no emit) so we
        // don't want the filter tagging it as suspicious.
        assert!(!looks_like_hallucination(""));
        assert!(!looks_like_hallucination("   "));
    }

    #[test]
    fn hallucination_filter_drops_tiny_fragments() {
        // 1-3 char single-word transcripts are noise.
        assert!(looks_like_hallucination("Hi."));
        assert!(looks_like_hallucination("Go"));
        assert!(looks_like_hallucination("."));
    }

    // -- AdaptiveVad tests --
    // `multiplier = 1.0` keeps behaviour close to a fixed-threshold VAD:
    // true-silence chunks have rms ~0, so the threshold stays at the floor
    // and the tests exercise the speech/silence state machine cleanly.

    fn test_vad(floor: f32, silence_ms: u64, max_secs: u64) -> AdaptiveVad {
        AdaptiveVad::new(floor, 1.0, silence_ms, max_secs)
    }

    #[test]
    fn test_rms_energy_silence() {
        let silence = vec![0.0f32; VAD_CHUNK_SAMPLES];
        let energy = AdaptiveVad::rms_energy(&silence);
        assert!(
            energy.abs() < f32::EPSILON,
            "RMS energy of silence should be 0.0, got {energy}"
        );
    }

    #[test]
    fn test_rms_energy_loud_signal() {
        let loud = vec![0.5f32; VAD_CHUNK_SAMPLES];
        let energy = AdaptiveVad::rms_energy(&loud);
        assert!(
            (energy - 0.5).abs() < 0.01,
            "RMS energy of constant 0.5 signal should be ~0.5, got {energy}"
        );
    }

    #[test]
    fn test_rms_energy_empty() {
        let energy = AdaptiveVad::rms_energy(&[]);
        assert!(
            energy.abs() < f32::EPSILON,
            "RMS energy of empty slice should be 0.0"
        );
    }

    #[test]
    fn test_vad_detects_speech_above_threshold() {
        let mut vad = test_vad(0.01, 500, 8);
        let mut state = VadState::new();

        let speech_chunk = vec![0.1f32; VAD_CHUNK_SAMPLES];
        let decision = vad.feed_chunk(&speech_chunk, &mut state);

        assert_eq!(decision, VadDecision::Speech);
        assert!(state.speech_active);
        assert_eq!(state.speech_buffer.len(), VAD_CHUNK_SAMPLES);
    }

    #[test]
    fn test_vad_silence_below_threshold() {
        let mut vad = test_vad(0.01, 500, 8);
        let mut state = VadState::new();

        let silence_chunk = vec![0.0f32; VAD_CHUNK_SAMPLES];
        let decision = vad.feed_chunk(&silence_chunk, &mut state);

        assert_eq!(decision, VadDecision::Silence);
        assert!(!state.speech_active);
        assert!(state.speech_buffer.is_empty());
    }

    #[test]
    fn test_vad_segment_complete_after_silence() {
        // silence_duration_ms = 64 ms, at 32 ms/chunk = 2 chunks needed.
        let mut vad = test_vad(0.01, 64, 8);
        let mut state = VadState::new();

        // Feed one speech chunk.
        let speech = vec![0.1f32; VAD_CHUNK_SAMPLES];
        let d = vad.feed_chunk(&speech, &mut state);
        assert_eq!(d, VadDecision::Speech);

        // Feed silence chunks until the segment completes.
        let silence = vec![0.0f32; VAD_CHUNK_SAMPLES];
        let d1 = vad.feed_chunk(&silence, &mut state);
        // First silence chunk: not enough yet (need 2).
        assert_eq!(d1, VadDecision::Speech);

        let d2 = vad.feed_chunk(&silence, &mut state);
        assert_eq!(d2, VadDecision::SegmentComplete);

        // The buffer should contain the speech chunk + 2 silence chunks.
        assert_eq!(state.speech_buffer.len(), VAD_CHUNK_SAMPLES * 3);
    }

    #[test]
    fn test_vad_force_split_at_max_segment() {
        // max_segment_secs = 1, so max samples = 16000.
        // Each chunk is 512 samples, so 32 chunks = 16384 >= 16000.
        let mut vad = test_vad(0.01, 500, 1);
        let mut state = VadState::new();

        let speech = vec![0.1f32; VAD_CHUNK_SAMPLES];
        let mut last_decision = VadDecision::Silence;

        for _ in 0..32 {
            last_decision = vad.feed_chunk(&speech, &mut state);
            if last_decision == VadDecision::SegmentForceSplit {
                break;
            }
        }

        assert_eq!(
            last_decision,
            VadDecision::SegmentForceSplit,
            "VAD should force-split at max segment length"
        );
    }

    #[test]
    fn test_vad_take_segment_resets_state() {
        let mut vad = test_vad(0.01, 500, 8);
        let mut state = VadState::new();

        let speech = vec![0.1f32; VAD_CHUNK_SAMPLES];
        vad.feed_chunk(&speech, &mut state);

        assert!(!state.speech_buffer.is_empty());
        assert!(state.speech_active);

        let segment = state.take_segment();
        assert_eq!(segment.len(), VAD_CHUNK_SAMPLES);
        assert!(state.speech_buffer.is_empty());
        assert!(!state.speech_active);
        assert_eq!(state.consecutive_silence_chunks, 0);
    }

    // -- Resampling tests --

    #[test]
    fn test_resample_passthrough_16khz_mono() {
        let samples = vec![0.5f32; 1600]; // 100ms at 16kHz
        let result = resample_to_16khz(&samples, 16_000, 1).expect("Passthrough should succeed");
        assert_eq!(result.len(), 1600);
        assert!((result[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resample_stereo_to_mono() {
        // Stereo signal: left = 0.4, right = 0.6. Mono average = 0.5.
        let mut stereo = Vec::new();
        for _ in 0..800 {
            stereo.push(0.4f32);
            stereo.push(0.6f32);
        }
        let result = resample_to_16khz(&stereo, 16_000, 2).expect("Stereo mixdown should succeed");
        assert_eq!(result.len(), 800);
        assert!(
            (result[0] - 0.5).abs() < 0.01,
            "Mono mixdown should average channels"
        );
    }

    #[test]
    fn test_resample_48khz_to_16khz() {
        // 48000 samples at 48kHz = 1 second. Should produce ~16000 samples.
        let samples = vec![0.1f32; 48000];
        let result =
            resample_to_16khz(&samples, 48_000, 1).expect("Resampling 48k->16k should succeed");
        // Allow some tolerance due to resampler edge effects.
        let expected = 16_000;
        let tolerance = 200;
        assert!(
            (result.len() as i64 - expected as i64).unsigned_abs() < tolerance,
            "Expected ~{expected} samples, got {}",
            result.len()
        );
    }

    // -- AudioWatcher tests --

    #[test]
    fn test_audio_watcher_disabled() {
        let config = AudioConfig {
            enabled: false,
            ..AudioConfig::default()
        };
        let watcher = AudioWatcher::new(config);
        assert!(watcher.is_healthy());
        assert!(!watcher.should_restart());
        assert!(watcher.whisper_ctx.is_none());
    }

    #[test]
    fn test_audio_watcher_missing_model_degrades_gracefully() {
        let config = AudioConfig {
            enabled: true,
            whisper_model_path: "/nonexistent/whisper-model.bin".to_string(),
            ..AudioConfig::default()
        };
        let watcher = AudioWatcher::new(config);
        // Should be created without panicking, but in degraded mode.
        assert!(watcher.whisper_ctx.is_none());
        assert!(!watcher.is_healthy());
        assert!(watcher.should_restart());
    }

    #[tokio::test]
    async fn test_audio_watcher_disabled_exits_on_shutdown() {
        let config = AudioConfig {
            enabled: false,
            ..AudioConfig::default()
        };
        let watcher = AudioWatcher::new(config);
        let (tx, _rx) = tokio_mpsc::channel::<AudioObservation>(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            watcher.run(tx, shutdown_rx).await;
        });

        // Signal shutdown. The disabled watcher should exit promptly.
        let _ = shutdown_tx.send(true);

        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;

        assert!(
            result.is_ok(),
            "Disabled audio watcher should exit within 5 seconds of shutdown"
        );
    }

    // -- AdaptiveVad edge cases --

    #[test]
    fn test_vad_multiple_segments() {
        // Verify the VAD can detect multiple speech segments in sequence.
        let mut vad = test_vad(0.01, 64, 8);
        let mut state = VadState::new();

        let speech = vec![0.1f32; VAD_CHUNK_SAMPLES];
        let silence = vec![0.0f32; VAD_CHUNK_SAMPLES];

        // First segment: one speech chunk, then silence until complete.
        vad.feed_chunk(&speech, &mut state);
        vad.feed_chunk(&silence, &mut state);
        let d = vad.feed_chunk(&silence, &mut state);
        assert_eq!(d, VadDecision::SegmentComplete);

        let seg1 = state.take_segment();
        assert!(!seg1.is_empty());

        // Second segment: should work after reset.
        let d2 = vad.feed_chunk(&speech, &mut state);
        assert_eq!(d2, VadDecision::Speech);
        assert!(state.speech_active);
    }

    #[test]
    fn test_vad_new_calculates_silence_chunks() {
        // 500ms silence at 32ms/chunk = floor(500/32) = 15 chunks, clamped to >= 1.
        let vad = test_vad(0.5, 500, 8);
        assert_eq!(vad.silence_chunks_needed, 15);

        // 32ms silence = exactly 1 chunk.
        let vad2 = test_vad(0.5, 32, 8);
        assert_eq!(vad2.silence_chunks_needed, 1);

        // 10ms silence = less than one chunk, clamped to 1.
        let vad3 = test_vad(0.5, 10, 8);
        assert_eq!(vad3.silence_chunks_needed, 1);
    }

    #[test]
    fn test_vad_max_segment_samples() {
        let vad = test_vad(0.5, 500, 8);
        assert_eq!(vad.max_segment_samples, 8 * 16_000);

        let vad2 = test_vad(0.5, 500, 1);
        assert_eq!(vad2.max_segment_samples, 16_000);
    }

    #[test]
    fn test_vad_noise_floor_tracks_silence_chunks() {
        // Starting with an empty ring, feeding truly-silent chunks should
        // accumulate them in the ring and leave noise_floor near zero.
        // Threshold then stays at the floor and subsequent loud chunks are
        // correctly classified as speech.
        let mut vad = AdaptiveVad::new(0.005, 5.0, 500, 8);
        let mut state = VadState::new();

        let silence = vec![0.0f32; VAD_CHUNK_SAMPLES];
        for _ in 0..100 {
            let d = vad.feed_chunk(&silence, &mut state);
            assert_eq!(d, VadDecision::Silence);
        }
        assert!(vad.noise_floor() < 0.001, "quiet silence → low noise floor");
        assert!(
            (vad.current_threshold() - 0.005).abs() < 1e-6,
            "threshold should still be at the floor"
        );
        assert!(!state.speech_active);

        let loud = vec![0.3f32; VAD_CHUNK_SAMPLES];
        let d = vad.feed_chunk(&loud, &mut state);
        assert_eq!(d, VadDecision::Speech);
        assert!(state.speech_active);
    }

    #[test]
    fn test_vad_speech_does_not_poison_noise_floor() {
        // Regression: a user who speaks immediately on startup must not
        // corrupt the adaptive threshold. Speech chunks are classified as
        // speech from chunk 1 (ring empty → threshold = floor) and NEVER
        // enter the silence ring, so the threshold stays at the floor
        // throughout and speech keeps being detected.
        let mut vad = AdaptiveVad::new(0.005, 5.0, 500, 8);
        let mut state = VadState::new();

        let loud = vec![0.3f32; VAD_CHUNK_SAMPLES];
        for _ in 0..200 {
            let _ = vad.feed_chunk(&loud, &mut state);
        }
        assert_eq!(
            vad.noise_floor(),
            0.0,
            "no silence chunks pushed → noise floor stays at 0"
        );
        assert!(
            (vad.current_threshold() - 0.005).abs() < 1e-6,
            "threshold must stay at floor when the ring is empty"
        );
    }
}
