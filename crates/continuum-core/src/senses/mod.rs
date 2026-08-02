//! # Layer 1 — Senses
//!
//! The senses layer runs as a set of dedicated tasks inside Continuum Core. It has
//! one job: produce a steady stream of [`types::PerceptionFrame`] objects and
//! push them to the triage layer via an internal channel.
//!
//! Three watchers run concurrently:
//! - [`vision`] — captures screenshots and describes them via a local vision model
//! - [`audio`] — captures microphone audio with VAD and transcribes speech
//! - [`context`] — polls Windows APIs for foreground window, idle time, and call state
//!
//! A [`frame`] builder combines the three into unified perception frames at a
//! configurable interval.
//!
//! [`types`] is always compiled because the dashboard crate needs the frame
//! schema for its state store. The runtime watchers are only compiled with
//! the `runtime` feature.

pub mod types;

#[cfg(feature = "runtime")]
pub mod audio;
#[cfg(feature = "runtime")]
pub mod context;
#[cfg(feature = "runtime")]
pub mod frame;
#[cfg(feature = "runtime")]
pub mod vision;
