//! Reproducible synthetic benchmark for the lightweight perception gate.
//!
//! This measures only fingerprint + change-gate overhead. It does not claim
//! Windows capture, ONNX, GPU, or end-to-end runtime performance.

use std::time::{Duration, Instant};

use continuum_vision::perception::{ChangeGate, FrameFingerprint, ObservationKey};
use image::{Rgba, RgbaImage};

fn frame(iteration: u64) -> RgbaImage {
    let mut image = RgbaImage::from_pixel(640, 360, Rgba([30, 30, 30, 255]));
    // Introduce a meaningful synthetic change every 100 frames; all other
    // frames are byte-identical and should be discarded cheaply.
    if iteration % 100 == 0 {
        for x in 240..400 {
            for y in 120..240 {
                image.put_pixel(x, y, Rgba([220, 220, 220, 255]));
            }
        }
    }
    image
}

fn main() {
    const ITERATIONS: u64 = 2_000;
    let key = ObservationKey::display("synthetic-display-1");
    let mut gate = ChangeGate::new(0.05, Duration::from_secs(30));
    let clock = Instant::now();
    let started = Instant::now();
    let mut semantic_requests = 0u64;

    for iteration in 0..ITERATIONS {
        let image = frame(iteration);
        let fingerprint = FrameFingerprint::from_rgba(&image);
        let decision = gate.evaluate_at(
            key.clone(),
            fingerprint,
            clock + Duration::from_millis(iteration * 20),
        );
        semantic_requests += u64::from(decision.should_encode);
    }

    let elapsed = started.elapsed();
    println!("synthetic frames: {ITERATIONS}");
    println!("semantic requests: {semantic_requests}");
    println!("semantic skip rate: {:.2}%", 100.0 * (1.0 - semantic_requests as f64 / ITERATIONS as f64));
    println!("fingerprint+gate total: {elapsed:?}");
    println!("average per frame: {:?}", elapsed / ITERATIONS as u32);
}
