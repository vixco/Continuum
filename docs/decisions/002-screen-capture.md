# ADR-002: xcap with GDI Backend for Screen Capture

**Status:** Accepted
**Date:** 2026-04-10
**Layer:** 1 (Senses)
**Crate:** `kairo-core` (senses::vision)

## Context

Kairo captures the primary monitor every 3 seconds to feed the vision model. The capture must:

- Work on Windows 10 and 11.
- Produce no visible indicator (border, overlay, notification).
- Return an `image::RgbaImage` for downstream processing.
- Complete in under 50ms (well within the 3-second budget).
- Support multi-monitor setups (capture primary only).

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
- **Con:** GDI is slower than DXGI (~15-30ms vs ~5-10ms per capture).
- **Verdict:** Best fit. Speed tradeoff is irrelevant at 3-second intervals.

### 3. screenshots (older crate)

- **Pro:** Simple API.
- **Con:** Less maintained, fewer features than xcap.
- **Verdict:** Superseded by xcap.

### 4. dxgi-capture-rs

- **Pro:** No border (uses DXGI Desktop Duplication, not WGC).
- **Pro:** Faster than GDI (~5-10ms).
- **Con:** DXGI Desktop Duplication has quirks: requires the calling thread to own a desktop, fails under certain RDP/remote scenarios, needs careful COM initialization.
- **Verdict:** Reserved for future optimization if sub-10ms captures are needed.

### 5. windows crate direct (raw COM)

- **Pro:** Maximum control.
- **Con:** Extremely verbose. Hundreds of lines for what xcap does in three.
- **Verdict:** Not worth the maintenance burden.

## Decision

Use **`xcap` v0.8** with its GDI (BitBlt) backend for all screen capture.

The capture code in `senses::vision::capture_primary_monitor()` is three lines:

```rust
let monitors = Monitor::all()?;
let primary = monitors.into_iter().find(|m| m.is_primary().unwrap_or(false))?;
let screenshot = primary.capture_image()?;
```

## Tradeoffs

- **Speed:** GDI capture takes ~15-30ms. At a 3-second interval this is <1% overhead. Not a concern.
- **DirectX fullscreen:** GDI cannot capture DirectX exclusive fullscreen applications. This is not a concern for an ambient assistant -- users in exclusive fullscreen games are not expecting Kairo to describe their screen.
- **HDR:** GDI captures in SDR. HDR content will appear tone-mapped. Acceptable for Phase 1.

## Upgrade Path

If sub-10ms capture latency becomes important (e.g., for real-time screen sharing features), switch to `dxgi-capture-rs` or a direct DXGI Desktop Duplication implementation. The `VisionWatcher` only calls `capture_primary_monitor()`, so the swap is isolated.

## References

- `crates/kairo-core/src/senses/vision.rs` -- `capture_primary_monitor()`, `downscale_screenshot()`, `save_screenshot()`
- `crates/kairo-core/src/config.rs` -- `ScreenConfig` (interval, resolution, save flag)
