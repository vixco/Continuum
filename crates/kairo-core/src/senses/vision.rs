//! # Vision watcher
//!
//! Takes a screenshot every N seconds (default 3, configurable 1–10) using the
//! Windows Graphics Capture API. The screenshot is downscaled to 1280x720 and
//! sent to the local vision model (Moondream 2 by default) for description.
//!
//! Produces [`ScreenObservation`] structs containing a one-sentence description
//! of what the user is looking at, the foreground app name, and a confidence score.
