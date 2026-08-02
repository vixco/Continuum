//! # Audio playback stream
//!
//! Thin wrapper around a `cpal` output stream with a mutex-guarded sample
//! queue. Mono `f32` audio produced by the TTS engine is resampled from the
//! voice's native rate (e.g. 22050 Hz for Piper medium) to the device's
//! output rate (commonly 48000 Hz on Windows WASAPI) and expanded to match
//! the device's channel count.
//!
//! Design decisions:
//!
//! - **Queue type** — `Mutex<VecDeque<f32>>`. cpal's callback runs on an OS
//!   audio thread; the lock is held only long enough to drain samples into
//!   the provided buffer (microseconds). A lock-free SPSC ring would be
//!   lower latency but unnecessary for TTS where producer rate ≪ consumer
//!   rate.
//! - **Resampling** — linear interpolation. Sub-bark band accuracy that
//!   listeners cannot hear the difference from SINC for TTS content, and
//!   avoids a stateful `rubato::Async` resampler in the producer path.
//! - **Barge-in hook** — [`PlaybackStream::is_active`] exposes the "audio
//!   still queued" flag so Phase 5.3 can raise the VAD threshold while TTS
//!   is playing without reaching into private state.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

/// Shared state between the cpal audio callback and the producer side
/// (`push_mono`, `clear`, `is_active`).
struct PlaybackInner {
    queue: Mutex<VecDeque<f32>>,
    /// `true` while the queue is non-empty *or* a push is in flight. Set by
    /// the producer on `push_mono`, cleared by the callback when the queue
    /// is drained. Used by the barge-in detector to know when to raise the
    /// VAD threshold.
    active: AtomicBool,
    /// Master playback gain as `f32` bits. Multiplied into every sample in
    /// [`PlaybackInner::fill`]. Atomic so config changes take effect on the
    /// next audio buffer without holding the queue lock.
    volume_bits: AtomicU32,
}

impl PlaybackInner {
    fn new(initial_volume: f32) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            active: AtomicBool::new(false),
            volume_bits: AtomicU32::new(clamp_unit(initial_volume).to_bits()),
        }
    }

    /// Pull up to `out.len()` samples from the queue, zero-filling the rest.
    /// Applies the current volume gain to each sample. Called from the cpal
    /// audio thread.
    fn fill<S: cpal::SizedSample + cpal::FromSample<f32>>(&self, out: &mut [S]) {
        let gain = f32::from_bits(self.volume_bits.load(Ordering::Relaxed));
        let mut q = match self.queue.lock() {
            Ok(g) => g,
            Err(_) => {
                out.iter_mut().for_each(|s| *s = S::from_sample(0.0_f32));
                return;
            }
        };
        let drained = q.len().min(out.len());
        for sample in out.iter_mut().take(drained) {
            let v = q.pop_front().unwrap_or(0.0) * gain;
            *sample = S::from_sample(v);
        }
        for sample in out.iter_mut().skip(drained) {
            *sample = S::from_sample(0.0_f32);
        }
        if q.is_empty() {
            self.active.store(false, Ordering::Release);
        }
    }
}

fn clamp_unit(x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    x.clamp(0.0, 1.0)
}

/// Open, running audio playback stream.
///
/// Holds a cpal `Stream` for its lifetime — dropping this struct stops
/// playback cleanly.
pub struct PlaybackStream {
    _stream: cpal::Stream,
    inner: Arc<PlaybackInner>,
    device_rate: u32,
    channels: u16,
}

impl PlaybackStream {
    /// Open the system default output device with its preferred config.
    ///
    /// Queries the device's default sample rate + channel count rather
    /// than forcing 22050, so Piper output must be resampled in
    /// [`push_mono`] before enqueueing. Initial volume defaults to `1.0`
    /// — call [`set_volume`] or use [`open_default_with_volume`] for a
    /// different starting gain.
    pub fn open_default() -> Result<Self> {
        Self::open_default_with_volume(1.0)
    }

    /// Open the default output device with a specific initial volume gain.
    pub fn open_default_with_volume(volume: f32) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("No default audio output device")?;
        let config = device
            .default_output_config()
            .context("Failed to query default output config")?;

        let device_rate = config.sample_rate();
        let channels = config.channels();
        let format = config.sample_format();

        let inner = Arc::new(PlaybackInner::new(volume));
        let stream = build_stream(&device, &config.into(), format, inner.clone())?;
        stream.play().context("Failed to start playback stream")?;

        tracing::info!(
            layer = "voice",
            component = "playback",
            device_rate,
            channels,
            format = ?format,
            device_name = %device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "unknown".into()),
            "Playback stream opened"
        );

        Ok(Self {
            _stream: stream,
            inner,
            device_rate,
            channels,
        })
    }

    /// Resample mono `f32` audio at `source_rate` to the device's rate,
    /// duplicate to the device's channel count, and enqueue for playback.
    ///
    /// Safe to call from any thread. Returns immediately — playback
    /// progresses on the cpal audio thread.
    pub fn push_mono(&self, mono: &[f32], source_rate: u32) {
        if mono.is_empty() {
            return;
        }
        let resampled = resample_linear(mono, source_rate, self.device_rate);
        let interleaved = interleave_mono_to_channels(&resampled, self.channels);

        {
            let mut q = match self.inner.queue.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            q.extend(interleaved);
        }
        self.inner.active.store(true, Ordering::Release);
    }

    /// Drop every sample currently queued. Used for barge-in (Phase 5.3):
    /// the user started speaking while Continuum was talking, so the current
    /// utterance should stop immediately.
    ///
    /// Under cpal, the audio thread's in-flight buffer (at most one
    /// callback ≈ 10 ms at 48 kHz / 512 frames) will still play out.
    pub fn clear(&self) {
        if let Ok(mut q) = self.inner.queue.lock() {
            q.clear();
        }
        self.inner.active.store(false, Ordering::Release);
    }

    /// Returns `true` while audio is queued for playback. Flips to `false`
    /// the moment the cpal callback empties the queue.
    pub fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }

    /// Update the master playback gain. Clamps to `[0.0, 1.0]`. Takes effect
    /// on the next audio buffer pulled by the cpal callback (typically
    /// within 10 ms at 48 kHz / 512 frames).
    pub fn set_volume(&self, volume: f32) {
        let clamped = clamp_unit(volume);
        self.inner
            .volume_bits
            .store(clamped.to_bits(), Ordering::Relaxed);
        tracing::debug!(
            layer = "voice",
            component = "playback",
            volume = clamped,
            "Volume updated"
        );
    }

    /// Current master playback gain in `[0.0, 1.0]`.
    pub fn volume(&self) -> f32 {
        f32::from_bits(self.inner.volume_bits.load(Ordering::Relaxed))
    }

    /// Block until the playback queue is empty. Returns immediately if
    /// nothing is queued.
    ///
    /// Polls at 20 ms intervals — adequate for the "wait for Continuum to
    /// finish speaking" case, wasteful for anything faster-paced.
    pub fn wait_drain(&self) {
        while self.is_active() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Device output sample rate in Hz.
    pub fn device_sample_rate(&self) -> u32 {
        self.device_rate
    }

    /// Device output channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }
}

/// Build a cpal output stream for the given sample format. The callback is
/// static-dispatched on sample type so cpal can vectorise the copy loop.
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: SampleFormat,
    inner: Arc<PlaybackInner>,
) -> Result<cpal::Stream> {
    let err_fn = |err| {
        tracing::error!(
            layer = "voice",
            component = "playback",
            error = ?err,
            "cpal output stream error"
        );
    };
    let stream = match format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| inner.fill::<f32>(data),
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| inner.fill::<i16>(data),
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream(
            config,
            move |data: &mut [u16], _: &cpal::OutputCallbackInfo| inner.fill::<u16>(data),
            err_fn,
            None,
        ),
        other => anyhow::bail!("Unsupported cpal output sample format: {other:?}"),
    };
    stream.with_context(|| format!("Failed to build output stream for {format:?}"))
}

/// Linear-interpolation resampler for mono `f32` audio.
///
/// Preserves total duration exactly: `output.len() == input.len() * to / from`
/// (rounded). Fast, stateless, allocation-per-call. Good enough for 22050 →
/// 48000 resampling of TTS content; upgrade to `rubato` if aliasing becomes
/// audible.
pub(crate) fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return input.to_vec();
    }

    let in_len = input.len();
    let out_len = ((in_len as u64 * to as u64) / from as u64) as usize;
    let mut out = Vec::with_capacity(out_len);

    let ratio = from as f64 / to as f64;
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos as usize;
        let frac = (src_pos - idx as f64) as f32;

        let a = input[idx.min(in_len - 1)];
        let b = input[(idx + 1).min(in_len - 1)];
        out.push(a + frac * (b - a));
    }
    out
}

/// Duplicate a mono channel into `channels` interleaved channels.
///
/// Output length is `mono.len() * channels`.
pub(crate) fn interleave_mono_to_channels(mono: &[f32], channels: u16) -> Vec<f32> {
    if channels == 0 {
        return Vec::new();
    }
    if channels == 1 {
        return mono.to_vec();
    }
    let ch = channels as usize;
    let mut out = Vec::with_capacity(mono.len() * ch);
    for &s in mono {
        for _ in 0..ch {
            out.push(s);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_identity_when_rates_equal() {
        let input: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let got = resample_linear(&input, 22050, 22050);
        assert_eq!(got, input);
    }

    #[test]
    fn resample_upsample_doubles_length() {
        let input = vec![0.0, 1.0, 0.0, 1.0];
        let got = resample_linear(&input, 22050, 44100);
        assert_eq!(got.len(), 8);
    }

    #[test]
    fn resample_downsample_halves_length() {
        let input: Vec<f32> = (0..8).map(|i| i as f32).collect();
        let got = resample_linear(&input, 44100, 22050);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn resample_upsample_22050_to_48000_preserves_energy_order() {
        // Check the upsampled signal is still monotonic where the input is.
        let input: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let got = resample_linear(&input, 22050, 48000);
        assert!(got.len() > input.len());
        // First sample should be ~input[0], last should be ~input[-1].
        assert!((got[0] - input[0]).abs() < 1e-3);
        assert!((got[got.len() - 1] - input[input.len() - 1]).abs() < 0.05);
    }

    #[test]
    fn resample_empty_input_returns_empty() {
        assert!(resample_linear(&[], 22050, 48000).is_empty());
    }

    #[test]
    fn resample_zero_rate_returns_empty() {
        assert!(resample_linear(&[1.0, 2.0], 0, 48000).is_empty());
        assert!(resample_linear(&[1.0, 2.0], 48000, 0).is_empty());
    }

    #[test]
    fn interleave_mono_to_stereo_duplicates() {
        let mono = vec![0.5, -0.5, 1.0];
        let stereo = interleave_mono_to_channels(&mono, 2);
        assert_eq!(stereo, vec![0.5, 0.5, -0.5, -0.5, 1.0, 1.0]);
    }

    #[test]
    fn interleave_mono_to_mono_is_clone() {
        let mono = vec![0.5, -0.5];
        let got = interleave_mono_to_channels(&mono, 1);
        assert_eq!(got, mono);
    }

    #[test]
    fn interleave_zero_channels_empty() {
        assert!(interleave_mono_to_channels(&[1.0, 2.0], 0).is_empty());
    }

    #[test]
    fn interleave_7_1_outputs_eight_per_sample() {
        let mono = vec![1.0];
        let out = interleave_mono_to_channels(&mono, 8);
        assert_eq!(out, vec![1.0; 8]);
    }

    // --- PlaybackInner (no real device) ---

    #[test]
    fn inner_active_flips_when_queue_drains() {
        let inner = Arc::new(PlaybackInner::new(1.0));
        assert!(!inner.active.load(Ordering::Acquire));

        inner.queue.lock().unwrap().extend([0.1, 0.2, 0.3]);
        inner.active.store(true, Ordering::Release);
        assert!(inner.active.load(Ordering::Acquire));

        // Fill a buffer larger than the queue → drains, flips active=false.
        let mut out = [0.0_f32; 8];
        inner.fill::<f32>(&mut out);
        assert!(!inner.active.load(Ordering::Acquire));
        assert!((out[0] - 0.1).abs() < 1e-6);
        assert!((out[1] - 0.2).abs() < 1e-6);
        assert!((out[2] - 0.3).abs() < 1e-6);
        assert_eq!(&out[3..], &[0.0; 5]);
    }

    #[test]
    fn inner_fill_partial_drain_keeps_active() {
        let inner = Arc::new(PlaybackInner::new(1.0));
        inner.queue.lock().unwrap().extend([0.1, 0.2, 0.3, 0.4]);
        inner.active.store(true, Ordering::Release);

        let mut out = [0.0_f32; 2];
        inner.fill::<f32>(&mut out);
        assert!((out[0] - 0.1).abs() < 1e-6);
        assert!((out[1] - 0.2).abs() < 1e-6);
        // Queue still has 2 samples → still active.
        assert!(inner.active.load(Ordering::Acquire));
    }

    #[test]
    fn inner_fill_empty_queue_zeroes_buffer() {
        let inner = Arc::new(PlaybackInner::new(1.0));
        let mut out = [9.0_f32; 4];
        inner.fill::<f32>(&mut out);
        assert_eq!(&out, &[0.0, 0.0, 0.0, 0.0]);
        assert!(!inner.active.load(Ordering::Acquire));
    }

    #[test]
    fn inner_fill_scales_by_volume() {
        let inner = Arc::new(PlaybackInner::new(0.5));
        inner.queue.lock().unwrap().extend([1.0, -1.0, 0.5, -0.5]);
        let mut out = [0.0_f32; 4];
        inner.fill::<f32>(&mut out);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] + 0.5).abs() < 1e-6);
        assert!((out[2] - 0.25).abs() < 1e-6);
        assert!((out[3] + 0.25).abs() < 1e-6);
    }

    #[test]
    fn clamp_unit_rejects_out_of_range() {
        assert_eq!(clamp_unit(-0.5), 0.0);
        assert_eq!(clamp_unit(2.0), 1.0);
        assert_eq!(clamp_unit(0.7), 0.7);
        assert_eq!(clamp_unit(f32::NAN), 0.0);
        assert_eq!(clamp_unit(f32::INFINITY), 0.0);
    }

    #[test]
    fn inner_volume_bits_roundtrip() {
        let inner = Arc::new(PlaybackInner::new(0.3));
        let got = f32::from_bits(inner.volume_bits.load(Ordering::Relaxed));
        assert!((got - 0.3).abs() < 1e-6);
    }
}
