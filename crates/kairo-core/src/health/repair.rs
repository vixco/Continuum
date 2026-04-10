//! # Repair agent
//!
//! A dedicated Claude Code session with access to Kairo's own installation.
//! Spawned when the user clicks "Fix Issues" in the Health tab or says
//! "Kairo, something isn't right."
//!
//! The repair agent:
//! 1. Reads the last 500 log lines and component statuses
//! 2. Diagnoses the root cause
//! 3. Applies non-destructive fixes immediately
//! 4. Asks confirmation for destructive fixes
//! 5. Tests the fix and reports results
