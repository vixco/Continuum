//! # Layer 1 — Senses
//!
//! The senses layer runs as a set of dedicated tasks inside Kairo Core. It has
//! one job: produce a steady stream of [`PerceptionFrame`] objects and push
//! them to the triage layer via an internal channel.
//!
//! Three watchers run concurrently:
//! - [`vision`] — captures screenshots and describes them via a local vision model
//! - [`audio`] — captures microphone audio with VAD and transcribes speech
//! - [`context`] — polls Windows APIs for foreground window, idle time, and call state
//!
//! A [`frame`] builder combines the three into unified perception frames at a
//! configurable interval.

pub mod audio;
pub mod context;
pub mod frame;
pub mod vision;
