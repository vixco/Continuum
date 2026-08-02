//! Chat commands: conversation CRUD plus send/cancel with streaming via the
//! `continuum:chat` event.
//!
//! `chat_send_message` returns as soon as the provider stream has started;
//! the actual delta/done/error events are pushed to the frontend over the
//! `continuum:chat` Tauri event so the UI can render tokens as they arrive
//! instead of waiting for the whole response (see CLAUDE.md "How to run
//! Claude Code" for why streaming matters — same principle applies here).
//!
//! Two concurrency hazards this module guards against:
//!
//! - **Stop-then-resend race**: `chat_send_message`'s "already responding"
//!   check and its reservation of `conversation_id` in `ChatState.inflight`
//!   happen under a single lock acquisition (see [`chat_send_message`]) so
//!   two overlapping sends can't both observe "not busy". `chat_cancel`
//!   only cancels the token — it never removes the reservation — so a
//!   resend immediately after Stop is rejected until the still-draining
//!   consumer task's own unconditional cleanup runs.
//! - **Concurrent writers to one conversation file**: every load-mutate-save
//!   sequence on a conversation (the user-message save here, the three
//!   assistant-message saves in the consumer task, and the mutating CRUD
//!   commands) is serialized through `ChatState::conv_lock`, a
//!   per-conversation `tokio::sync::Mutex` held across the save.

use std::sync::Arc;

use continuum_gateway::{ChatEvent, ChatMessage, ChatRequest, ChatRole};
use tauri::Emitter;
use tokio_util::sync::CancellationToken;

use crate::chat_store::{ChatStore, Conversation, ConversationSummary, StoredMessage};
use crate::providers::{build_adapter, ChatState};

/// Built-in system prompt, embedded at compile time. Overridden per-config
/// via `[chat].system_prompt_path`.
const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../assets/chat-system-prompt.md");

/// Payload for the `continuum:chat` Tauri event.
#[derive(serde::Serialize, Clone)]
pub struct ChatEventPayload {
    pub conversation_id: String,
    pub event: ChatEvent,
}

/// Removes the in-flight reservation for a conversation. `chat_send_message`
/// calls this on every early-return error path once the reservation has
/// been taken — the reservation is otherwise only released by the consumer
/// task's own unconditional end-of-task cleanup (never by `chat_cancel`,
/// which only cancels the token).
fn release_inflight(
    inflight: &std::sync::Mutex<
        std::collections::HashMap<String, tokio_util::sync::CancellationToken>,
    >,
    conversation_id: &str,
) {
    inflight
        .lock()
        .expect("inflight lock")
        .remove(conversation_id);
}

/// Assembles the system prompt: the built-in (or user-overridden) base text
/// plus a short "live status" footer so the assistant can answer questions
/// like "is the background runtime running?" honestly instead of guessing.
fn system_prompt(
    cfg: &continuum_core::config::ChatConfig,
    runtime_running: bool,
    version: &str,
    provider: &str,
    model: &str,
) -> String {
    let base = cfg
        .system_prompt_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    format!(
        "{base}\n\n## Live status\n- Continuum version: {version}\n- Background runtime: {}\n- You are: {model} via {provider}\n",
        if runtime_running { "running" } else { "not running" }
    )
}

/// Lists every conversation, newest-updated first.
#[tauri::command]
pub fn chat_list_conversations(
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Vec<ConversationSummary> {
    ChatStore::new(state.runtime.dev_dir()).list()
}

/// Loads a single conversation, including its full message history.
#[tauri::command]
pub fn chat_get_conversation(
    conversation_id: String,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<Conversation, String> {
    ChatStore::new(state.runtime.dev_dir())
        .get(&conversation_id)
        .ok_or_else(|| "Unknown conversation".to_string())
}

/// Creates a new empty conversation bound to a provider + model. No
/// per-conversation lock needed — the id is freshly minted by `store.create`
/// and can't yet be referenced by any concurrent writer.
#[tauri::command]
pub fn chat_create_conversation(
    provider_id: String,
    model: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<Conversation, String> {
    let providers = chat_state.providers.lock().expect("providers lock").load();
    if !providers.iter().any(|p| p.id == provider_id) {
        return Err("Unknown provider".into());
    }
    ChatStore::new(state.runtime.dev_dir())
        .create(&provider_id, &model)
        .map_err(|e| e.to_string())
}

/// Deletes a conversation. Not an error if it's already gone. Takes the
/// per-conversation lock so a delete can't race a concurrent
/// `chat_send_message` save.
#[tauri::command]
pub async fn chat_delete_conversation(
    conversation_id: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<(), String> {
    let store = ChatStore::new(state.runtime.dev_dir());
    let conv_lock = chat_state.conv_lock(&conversation_id);
    let _guard = conv_lock.lock().await;
    store.delete(&conversation_id).map_err(|e| e.to_string())
}

/// Renames a conversation (overrides the auto-derived title).
#[tauri::command]
pub async fn chat_rename_conversation(
    conversation_id: String,
    title: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<(), String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("Title can't be empty.".into());
    }
    let store = ChatStore::new(state.runtime.dev_dir());
    let conv_lock = chat_state.conv_lock(&conversation_id);
    let _guard = conv_lock.lock().await;
    let mut conv = store.get(&conversation_id).ok_or("Unknown conversation")?;
    conv.title = trimmed.to_string();
    store.save(&conv).map_err(|e| e.to_string())
}

/// Switches a conversation to a different provider/model for future turns.
/// Past messages keep whatever `model` they were generated with.
#[tauri::command]
pub async fn chat_set_conversation_model(
    conversation_id: String,
    provider_id: String,
    model: String,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<(), String> {
    let providers = chat_state.providers.lock().expect("providers lock").load();
    if !providers.iter().any(|p| p.id == provider_id) {
        return Err("Unknown provider".into());
    }
    let store = ChatStore::new(state.runtime.dev_dir());
    let conv_lock = chat_state.conv_lock(&conversation_id);
    let _guard = conv_lock.lock().await;
    let mut conv = store.get(&conversation_id).ok_or("Unknown conversation")?;
    conv.provider_id = provider_id;
    conv.model = model;
    store.save(&conv).map_err(|e| e.to_string())
}

/// Sends a user message and streams the assistant's reply back over the
/// `continuum:chat` event. Returns as soon as the stream has started —
/// the caller should not await a "done" from this command, only from the
/// event stream.
#[tauri::command]
pub async fn chat_send_message(
    conversation_id: String,
    text: String,
    app: tauri::AppHandle,
    chat_state: tauri::State<'_, Arc<ChatState>>,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("Empty message.".into());
    }

    // Atomic reserve: the "is something already streaming" check and the
    // reservation of `conversation_id` happen under one lock acquisition.
    // Two `chat_send_message` calls racing each other therefore can't both
    // observe "not busy" — the loser gets the "already responding" error
    // deterministically instead of one message silently clobbering the
    // other's write. From here on, EVERY early return must call
    // `release_inflight` first; only the consumer task's unconditional
    // end-of-task cleanup and this reservation itself touch the map after
    // this point (`chat_cancel` deliberately does not).
    let cancel = CancellationToken::new();
    {
        let mut inflight = chat_state.inflight.lock().expect("inflight lock");
        if inflight.contains_key(&conversation_id) {
            return Err("Already responding — stop the current answer first.".into());
        }
        inflight.insert(conversation_id.clone(), cancel.clone());
    }

    // `state`/`chat_state` are `tauri::State<'_, _>` and not `'static`, so
    // everything the spawned consumer task needs is pulled out as owned
    // data before the task is created (see the doc comment on the spawn
    // below for the full list).
    let dev_dir = state.runtime.dev_dir();
    let store = ChatStore::new(dev_dir.clone());
    let conv_lock = chat_state.conv_lock(&conversation_id);

    let mut conv = match store.get(&conversation_id) {
        Some(c) => c,
        None => {
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err("Unknown conversation".into());
        }
    };
    let providers = chat_state.providers.lock().expect("providers lock").load();
    let conn = match providers.iter().find(|p| p.id == conv.provider_id) {
        Some(c) => c.clone(),
        None => {
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err("This conversation's provider was removed. Pick a model again.".into());
        }
    };

    conv.messages.push(StoredMessage::user(text.trim()));
    conv.derive_title();
    {
        let _guard = conv_lock.lock().await;
        if let Err(e) = store.save(&conv) {
            drop(_guard);
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err(e.to_string());
        }
    }

    let chat_cfg = state.runtime.config_snapshot().chat.clone();
    // No std lock is held across this call — `build_adapter` itself does
    // not await, and the `.lock()`s above have already been dropped.
    let adapter = match build_adapter(&conn, chat_state.secrets.as_ref(), &chat_cfg) {
        Ok(a) => a,
        Err(e) => {
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err(e);
        }
    };

    let runtime_running = crate::components::runtime_alive(&dev_dir);
    let req = ChatRequest {
        model: conv.model.clone(),
        system: system_prompt(
            &chat_cfg,
            runtime_running,
            env!("CARGO_PKG_VERSION"),
            &conn.display_name,
            &conv.model,
        ),
        messages: conv
            .messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role,
                content: m.content.clone(),
            })
            .collect(),
        max_tokens: chat_cfg.max_tokens,
        temperature: chat_cfg.temperature,
    };

    let mut stream = match adapter.stream_chat(req, cancel).await {
        Ok(s) => s,
        Err(e) => {
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err(e.user_message());
        }
    };

    // Consume the stream in a detached task so the command itself returns
    // immediately (the frontend gets deltas via the event, not the command
    // result). Only owned, 'static data crosses into the task: the
    // `Arc<ChatState>` (via `.inner().clone()`, also used to reach the
    // shared `conv_lock`'s inflight-cleanup at the end), the per-conversation
    // lock `Arc` itself, the dev dir (to build a fresh `ChatStore` — it's
    // just a `PathBuf` wrapper), and plain values.
    let chat_state = chat_state.inner().clone();
    let dev_dir_task = dev_dir.clone();
    let model = conv.model.clone();
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let store = ChatStore::new(dev_dir_task);
        let mut acc = String::new();
        let mut finished = false;
        while let Some(ev) = stream.next().await {
            let _ = app.emit(
                "continuum:chat",
                ChatEventPayload {
                    conversation_id: conversation_id.clone(),
                    event: ev.clone(),
                },
            );
            match ev {
                ChatEvent::Delta { text } => acc.push_str(&text),
                ChatEvent::Done { usage, .. } => {
                    finished = true;
                    let _guard = conv_lock.lock().await;
                    let Some(mut conv) = store.get(&conversation_id) else {
                        break;
                    };
                    conv.messages.push(StoredMessage {
                        role: ChatRole::Assistant,
                        content: acc.clone(),
                        ts: chrono::Utc::now(),
                        model: Some(model.clone()),
                        duration_ms: Some(started.elapsed().as_millis() as u64),
                        usage: Some(usage),
                        aborted: false,
                    });
                    if let Err(e) = store.save(&conv) {
                        tracing::error!(
                            layer = "desktop",
                            component = "chat",
                            conversation_id = %conversation_id,
                            error = %e,
                            "failed to persist chat message"
                        );
                    }
                }
                ChatEvent::Error { .. } => {
                    finished = true;
                    if !acc.is_empty() {
                        let _guard = conv_lock.lock().await;
                        if let Some(mut conv) = store.get(&conversation_id) {
                            conv.messages.push(StoredMessage {
                                role: ChatRole::Assistant,
                                content: acc.clone(),
                                ts: chrono::Utc::now(),
                                model: Some(model.clone()),
                                duration_ms: Some(started.elapsed().as_millis() as u64),
                                usage: None,
                                aborted: true,
                            });
                            if let Err(e) = store.save(&conv) {
                                tracing::error!(
                                    layer = "desktop",
                                    component = "chat",
                                    conversation_id = %conversation_id,
                                    error = %e,
                                    "failed to persist chat message"
                                );
                            }
                        }
                    }
                }
            }
        }
        if !finished && !acc.is_empty() {
            // Stream ended without a Done/Error event (shouldn't happen,
            // but keep the partial reply rather than silently drop it).
            let _guard = conv_lock.lock().await;
            if let Some(mut conv) = store.get(&conversation_id) {
                conv.messages.push(StoredMessage {
                    role: ChatRole::Assistant,
                    content: acc,
                    ts: chrono::Utc::now(),
                    model: Some(model),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                    usage: None,
                    aborted: true,
                });
                if let Err(e) = store.save(&conv) {
                    tracing::error!(
                        layer = "desktop",
                        component = "chat",
                        conversation_id = %conversation_id,
                        error = %e,
                        "failed to persist chat message"
                    );
                }
            }
        }
        // Single place the reservation is released on the success/streaming
        // path — the mirror of the atomic reserve at the top of this
        // function. `chat_cancel` never removes this entry itself.
        chat_state
            .inflight
            .lock()
            .expect("inflight lock")
            .remove(&conversation_id);
    });
    Ok(())
}

/// Cancels the in-flight stream for a conversation, if any, by triggering
/// its `CancellationToken`. Deliberately does NOT remove the `inflight`
/// reservation — only the consumer task's own end-of-task cleanup does
/// that, once it has actually finished draining the stream and persisting
/// whatever partial reply resulted. Removing it here would open a window
/// where an immediate resend races the still-draining old consumer task.
#[tauri::command]
pub fn chat_cancel(conversation_id: String, chat_state: tauri::State<'_, Arc<ChatState>>) {
    if let Some(tok) = chat_state
        .inflight
        .lock()
        .expect("inflight lock")
        .get(&conversation_id)
    {
        tok.cancel();
    }
}
