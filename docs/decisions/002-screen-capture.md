# ADR-002: Scaled GDI Capture with xcap Fallback

**Status:** Accepted for capture backend; cadence/topology superseded 2026-08-12
**Date:** 2026-04-10
**Layer:** 1 (Senses)
**Crate:** `continuum-core` (senses::vision)

## Context

Continuum originally captured only the primary monitor every 3 seconds through
`xcap`. The continuous live-context foundation now gives every connected
monitor a worker with a configurable, best-effort 20 ms target cadence. On
Windows, direct GDI `StretchBlt` captures a 64x36 mechanical change sample and
captures selected keyframes directly at model-facing resolution. `xcap`
remains responsible for enumeration and is the automatic capture fallback.
Unchanged samples bypass the bounded queue; only change-selected keyframes
reach local vision.

The capture must:

- Work on Windows 10 and 11.
- Produce no visible indicator (border, overlay, notification).
- Return an `image::RgbaImage` for downstream processing.
- Target 20ms when the GDI/desktop path can sustain it; record deadline misses
  honestly when it cannot.
- Support stable identities and concurrent capture for all connected monitors.

## The Yellow Border Problem

The Windows Graphics Capture (WGC) API shows a **yellow border** around the captured region as a privacy indicator. On Windows 11, applications can set `IsBorderRequired = false` to suppress it, but this flag does not exist on Windows 10.

For an ambient assistant that captures every 3 seconds, a persistent yellow border is unacceptable. It disrupts the user's workflow and defeats the purpose of ambient, invisible observation.

## Options Considered

### 1. windows-capture (WGC wrapper)

- **Pro:** Modern API, hardware-accelerated.
- **Con:** Yellow border on Windows 10. Requires Win11-only API to suppress.
- **Verdict:** Rejected for Phase 1 due to border issue.

### 2. xcap v0.8 (GDI / BitBlt backend)

- **Pro:** No border indicator, ever. GDI capture is silent.
- **Pro:** Returns `image::RgbaImage` directly, no conversion needed.
- **Pro:** Multi-monitor via `Monitor::all()` + `is_primary()`.
- **Pro:** Simple 3-line API: enumerate monitors, find primary, capture.
- **Pro:** Actively maintained, cross-platform (Linux/macOS support too).
- **Con:** GDI is slower than DXGI (~15-30ms vs ~5-10ms per capture), so 20 ms
  is not achievable on every monitor/machine.
- **Verdict:** Retained for compatibility and invisible capture. The runtime
  treats 20 ms as a best-effort deadline and exposes misses through health.

### 3. screenshots (older crate)

- **Pro:** Simple API.
- **Con:** Less maintained, fewer features than xcap.
- **Verdict:** Superseded by xcap.

### 4. dxgi-capture-rs

- **Pro:** No border (uses DXGI Desktop Duplication, not WGC).
- **Pro:** Faster than GDI (~5-10ms).
- **Con:** DXGI Desktop Duplication has quirks: requires the calling thread to own a desktop, fails under certain RDP/remote scenarios, needs careful COM initialization.
- **Verdict:** Reserved for future optimization if sub-10ms captures are needed.

### 5. windows crate direct (GDI)

- **Pro:** Scales inside GDI, so the 20 ms hot path never copies a full desktop
  bitmap into Rust memory.
- **Pro:** Allows a fast non-interpolating mode for the change signature and a
  higher-quality mode for model-facing keyframes.
- **Con:** Windows-only and more lifecycle code than `xcap`.
- **Verdict:** Selected for the Windows hot path, with `xcap` retained as the
  cross-platform fallback.

## Decision

Use direct GDI `StretchBlt` for scaled Windows captures. Use `COLORONCOLOR` for
the 64x36 change signature and `HALFTONE` for model-facing keyframes. Retain
`xcap` for monitor enumeration, stable identities, non-Windows capture, and an
automatic fallback when direct GDI capture fails.

## Tradeoffs

- **Speed:** GDI capture can take roughly 15-30ms depending on compositor,
  display, and host load. The 20ms target can therefore saturate one capture
  thread or miss deadlines; this is measured rather than hidden, and users can
  configure a slower cadence.
- **DirectX fullscreen:** GDI cannot capture DirectX exclusive fullscreen applications. This is not a concern for an ambient assistant -- users in exclusive fullscreen games are not expecting Continuum to describe their screen.
- **HDR:** GDI captures in SDR. HDR content will appear tone-mapped. Acceptable for Phase 1.

## Upgrade Path

If consistently meeting the 20ms target becomes important, switch the
per-monitor worker to `dxgi-capture-rs` or direct DXGI Desktop Duplication.
The current watcher isolates capture inside `run_monitor_capture_loop`, so the
queue, privacy, change detection, and vision contracts can remain intact.

## References

- `crates/continuum-core/src/senses/vision.rs` -- `capture_scaled_monitor()`, `run_monitor_capture_loop()`, `save_screenshot()`
- `crates/continuum-core/src/config.rs` -- `ScreenConfig` (interval, resolution, save flag)
