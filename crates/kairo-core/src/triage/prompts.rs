//! # Triage prompt management
//!
//! Loads and formats the triage system prompt from `prompts/triage-system.md`.
//! Injects runtime context (user name, SOUL.md excerpt, current frame) into
//! the prompt template before each triage call.
