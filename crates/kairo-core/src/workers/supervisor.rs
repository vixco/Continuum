//! # Worker supervisor
//!
//! Monitors running workers for health and progress. Detects stuck or
//! crashed workers and reports them for cleanup. Streams worker output
//! to the dashboard's active workers panel.
