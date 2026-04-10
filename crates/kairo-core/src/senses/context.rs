//! # Context watcher
//!
//! Pure Rust code that polls Windows APIs once per second. Captures:
//! - Foreground window title and process name
//! - Active file path from editors that expose it via UI Automation
//! - Currently playing media (via Windows Media Session)
//! - Idle time since last user input
//! - Whether the user is in a call (Discord, Teams, Zoom, Meet)
//! - Active Chrome/Edge tab URL (via accessibility tree)
//!
//! This layer uses no AI. It is structured polling — cheap, fast, deterministic.
//! Produces [`ContextObservation`] structs.
