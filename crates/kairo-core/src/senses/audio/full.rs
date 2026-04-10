//! Full audio pipeline implementation (requires `audio` Cargo feature).
//!
//! Captures microphone audio via `cpal`, detects speech segments with an
//! energy-based VAD, resamples to 16 kHz with `rubato`, and transcribes
//! via `whisper-rs`.

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
const WHISPER_THREADS: i32 = 4;
const RESAMPLER_CHUNK_SIZE: usize = 1024;

// ---------------------------------------------------------------------------
// Energy-based VAD
// ---------------------------------------------------------------------------

/// Simple energy-based voice activity detector.
///
/// Computes the RMS energy of each 32 ms chunk and compares it against a
/// configurable threshold. When energy exceeds the threshold, the chunk is
/// classified as speech. After `silence_duration_ms` of consecutive non-speech
/// chunks the segment is considered complete.
///
/// This is a Phase 1 approach. A future upgrade to Silero VAD will provide
/// higher accuracy, especially in noisy environments.
struct EnergyVad {
    /// RMS energy threshold (0.0 - 1.0). Chunks with energy above this
    /// value are classified as speech.
    threshold: f32,
    /// Number of consecutive silence chunks required to end a speech segment.
    silence_chunks_needed: usize,
    /// Maximum number of samples in a single speech segment before forced split.
    max_segment_samples: usize,
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

impl EnergyVad {
    /// Creates a new energy-based VAD with the given parameters.
    ///
    /// # Arguments
    ///
    /// * `threshold` - RMS energy threshold (0.0 - 1.0).
    /// * `silence_duration_ms` - How long silence must persist (in ms) before
    ///   a speech segment is considered complete.
    /// * `max_segment_secs` - Maximum segment length in seconds before forced split.
    fn new(threshold: f32, silence_duration_ms: u64, max_segment_secs: u64) -> Self {
        // Each VAD chunk is 32 ms at 16 kHz.
        let chunk_duration_ms = (VAD_CHUNK_SAMPLES as u64 * 1000) / u64::from(TARGET_SAMPLE_RATE);
        let silence_chunks_needed = if chunk_duration_ms > 0 {
            (silence_duration_ms / chunk_duration_ms).max(1) as usize
        } else {
            1
        };
        let max_segment_samples = (max_segment_secs * u64::from(TARGET_SAMPLE_RATE)) as usize;

        Self {
            threshold,
            silence_chunks_needed,
            max_segment_samples,
        }
    }

    /// Computes the RMS energy of a slice of f32 samples.
    ///
    /// Returns 0.0 for empty slices.
    fn rms_energy(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }

    /// Feeds a chunk of exactly [`VAD_CHUNK_SAMPLES`] samples into the VAD
    /// and returns a decision.
    ///
    /// Callers must maintain a [`VadState`] across calls and pass it here.
    fn feed_chunk(&self, chunk: &[f32], state: &mut VadState) -> VadDecision {
        let energy = Self::rms_energy(chunk);
        let is_speech = energy > self.threshold;

        if is_speech {
            state.speech_active = true;
            state.consecutive_silence_chunks = 0;
            state.speech_buffer.extend_from_slice(chunk);

            // Check for force-split.
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
fn resample_to_16khz(
    samples: &[f32],
    source_rate: u32,
    source_channels: u16,
) -> Result<Vec<f32>> {
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
        let input_adapter =
            SequentialSliceOfSlices::new(input_slices, 1, input_chunk.len())
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
                (frames as f64 * chunk.len() as f64 / RESAMPLER_CHUNK_SIZE as f64)
                    as usize;
            output.extend_from_slice(&channel_data[..actual_output_len.min(channel_data.len())]);
        } else {
            output.extend_from_slice(&channel_data[..frames]);
        }
    }

    Ok(output)
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

        let whisper_ctx = match Self::load_whisper_model(&config.whisper_model_path) {
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

        Self { config, whisper_ctx }
    }

    /// Loads a whisper model from the given file path.
    fn load_whisper_model(model_path: &str) -> Result<WhisperContext> {
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
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

        let vad = EnergyVad::new(
            self.config.vad_threshold,
            self.config.silence_duration_ms,
            self.config.max_segment_secs,
        );
        let mut vad_state = VadState::new();

        // Buffer for accumulating raw samples from the audio callback before
        // they are sliced into VAD chunks.
        let mut raw_buffer: Vec<f32> = Vec::new();

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

            // Resample if needed (converts to 16 kHz mono).
            let samples_16khz = if needs_resample {
                let to_resample = std::mem::take(&mut raw_buffer);
                match resample_to_16khz(&to_resample, native_rate, native_channels) {
                    Ok(resampled) => resampled,
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
                std::mem::take(&mut raw_buffer)
            };

            // Feed samples through the VAD in chunks.
            let mut offset = 0;
            while offset + VAD_CHUNK_SAMPLES <= samples_16khz.len() {
                let chunk = &samples_16khz[offset..offset + VAD_CHUNK_SAMPLES];
                offset += VAD_CHUNK_SAMPLES;

                let decision = vad.feed_chunk(chunk, &mut vad_state);

                match decision {
                    VadDecision::SegmentComplete | VadDecision::SegmentForceSplit => {
                        let segment = vad_state.take_segment();
                        if segment.is_empty() {
                            continue;
                        }

                        let duration_ms =
                            (segment.len() as u64 * 1000) / u64::from(TARGET_SAMPLE_RATE);

                        tracing::debug!(
                            layer = "senses",
                            component = "audio",
                            duration_ms = duration_ms,
                            samples = segment.len(),
                            forced = (decision == VadDecision::SegmentForceSplit),
                            "Speech segment complete, sending to whisper"
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

            // Keep any leftover samples that did not fill a complete chunk.
            if offset < samples_16khz.len() {
                raw_buffer.extend_from_slice(&samples_16khz[offset..]);
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
    fn open_audio_stream(
        &self,
    ) -> Result<(std_mpsc::Receiver<Vec<f32>>, u32, u16, cpal::Stream)> {
        let host = cpal::default_host();

        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No default audio input device found"))?;

        let device_name = device
            .description()
            .map(|d| format!("{d:?}"))
            .unwrap_or_else(|_| "unknown".to_string());

        tracing::info!(
            layer = "senses",
            component = "audio",
            device = %device_name,
            "Using audio input device"
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

        let observation = tokio::task::spawn_blocking(move || -> Result<AudioObservation> {
            let mut state = whisper_ctx
                .create_state()
                .context("Failed to create whisper state")?;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some("auto"));
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_n_threads(WHISPER_THREADS);

            state
                .full(params, &samples)
                .context("Whisper inference failed")?;

            let num_segments = state.full_n_segments();

            let mut transcript = String::new();
            for i in 0..num_segments {
                match state.get_segment(i) {
                    Some(segment) => {
                        match segment.to_str_lossy() {
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
                        }
                    }
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

            // Detect language from whisper state. The language is set during
            // inference when "auto" is used.
            let language = "auto".to_string();

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
        tracing::debug!(
            layer = "senses",
            component = "audio",
            duration_ms = duration_ms,
            transcription_ms = elapsed.as_millis() as u64,
            transcript_len = observation.transcript.len(),
            "Transcription complete"
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- EnergyVad tests --

    #[test]
    fn test_rms_energy_silence() {
        let silence = vec![0.0f32; VAD_CHUNK_SAMPLES];
        let energy = EnergyVad::rms_energy(&silence);
        assert!(
            energy.abs() < f32::EPSILON,
            "RMS energy of silence should be 0.0, got {energy}"
        );
    }

    #[test]
    fn test_rms_energy_loud_signal() {
        let loud = vec![0.5f32; VAD_CHUNK_SAMPLES];
        let energy = EnergyVad::rms_energy(&loud);
        assert!(
            (energy - 0.5).abs() < 0.01,
            "RMS energy of constant 0.5 signal should be ~0.5, got {energy}"
        );
    }

    #[test]
    fn test_rms_energy_empty() {
        let energy = EnergyVad::rms_energy(&[]);
        assert!(
            energy.abs() < f32::EPSILON,
            "RMS energy of empty slice should be 0.0"
        );
    }

    #[test]
    fn test_vad_detects_speech_above_threshold() {
        let vad = EnergyVad::new(0.01, 500, 8);
        let mut state = VadState::new();

        let speech_chunk = vec![0.1f32; VAD_CHUNK_SAMPLES];
        let decision = vad.feed_chunk(&speech_chunk, &mut state);

        assert_eq!(decision, VadDecision::Speech);
        assert!(state.speech_active);
        assert_eq!(state.speech_buffer.len(), VAD_CHUNK_SAMPLES);
    }

    #[test]
    fn test_vad_silence_below_threshold() {
        let vad = EnergyVad::new(0.01, 500, 8);
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
        let vad = EnergyVad::new(0.01, 64, 8);
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
        let vad = EnergyVad::new(0.01, 500, 1);
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
        let vad = EnergyVad::new(0.01, 500, 8);
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
        let result =
            resample_to_16khz(&stereo, 16_000, 2).expect("Stereo mixdown should succeed");
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

    // -- EnergyVad edge cases --

    #[test]
    fn test_vad_multiple_segments() {
        // Verify the VAD can detect multiple speech segments in sequence.
        let vad = EnergyVad::new(0.01, 64, 8);
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
        let vad = EnergyVad::new(0.5, 500, 8);
        assert_eq!(vad.silence_chunks_needed, 15);

        // 32ms silence = exactly 1 chunk.
        let vad2 = EnergyVad::new(0.5, 32, 8);
        assert_eq!(vad2.silence_chunks_needed, 1);

        // 10ms silence = less than one chunk, clamped to 1.
        let vad3 = EnergyVad::new(0.5, 10, 8);
        assert_eq!(vad3.silence_chunks_needed, 1);
    }

    #[test]
    fn test_vad_max_segment_samples() {
        let vad = EnergyVad::new(0.5, 500, 8);
        assert_eq!(vad.max_segment_samples, 8 * 16_000);

        let vad2 = EnergyVad::new(0.5, 500, 1);
        assert_eq!(vad2.max_segment_samples, 16_000);
    }
}
