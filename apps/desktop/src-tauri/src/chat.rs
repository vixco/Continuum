//! Chat commands: conversation CRUD plus send/cancel with streaming via the
//! `continuum:chat` event.
//!
//! `chat_send_message` returns as soon as the provider stream has started;
//! the actual delta/done/error events are pushed to the frontend over the
//! `continuum:chat` Tauri event so the UI can render tokens as they arrive
//! instead of waiting for the whole response (see CLAUDE.md "How to run
//! Claude Code" for why streaming matters — same principle applies here).

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

/// Creates a new empty conversation bound to a provider + model.
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

/// Deletes a conversation. Not an error if it's already gone.
#[tauri::command]
pub fn chat_delete_conversation(
    conversation_id: String,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<(), String> {
    ChatStore::new(state.runtime.dev_dir())
        .delete(&conversation_id)
        .map_err(|e| e.to_string())
}

/// Renames a conversation (overrides the auto-derived title).
#[tauri::command]
pub fn chat_rename_conversation(
    conversation_id: String,
    title: String,
    state: tauri::State<'_, Arc<crate::AppState>>,
) -> Result<(), String> {
    let store = ChatStore::new(state.runtime.dev_dir());
    let mut conv = store.get(&conversation_id).ok_or("Unknown conversation")?;
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("Title can't be empty.".into());
    }
    conv.title = trimmed.to_string();
    store.save(&conv).map_err(|e| e.to_string())
}

/// Switches a conversation to a different provider/model for future turns.
/// Past messages keep whatever `model` they were generated with.
#[tauri::command]
pub fn chat_set_conversation_model(
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
    // One in-flight stream per conversation.
    {
        let inflight = chat_state.inflight.lock().expect("inflight lock");
        if inflight.contains_key(&conversation_id) {
            return Err("Already responding — stop the current answer first.".into());
        }
    }

    // `state`/`chat_state` are `tauri::State<'_, _>` and not `'static`, so
    // everything the spawned consumer task needs is pulled out as owned
    // data before the task is created (see the doc comment on the spawn
    // below for the full list).
    let dev_dir = state.runtime.dev_dir();
    let store = ChatStore::new(dev_dir.clone());
    let mut conv = store.get(&conversation_id).ok_or("Unknown conversation")?;
    let providers = chat_state.providers.lock().expect("providers lock").load();
    let conn = providers
        .iter()
        .find(|p| p.id == conv.provider_id)
        .ok_or("This conversation's provider was removed. Pick a model again.")?
        .clone();

    conv.messages.push(StoredMessage::user(text.trim()));
    conv.derive_title();
    store.save(&conv).map_err(|e| e.to_string())?;

    let chat_cfg = state.runtime.config_snapshot().chat.clone();
    // No lock is held across this call — `build_adapter` itself does not
    // await, and the two `.lock()`s above have already been dropped.
    let adapter = build_adapter(&conn, chat_state.secrets.as_ref(), &chat_cfg)?;

    let runtime_running = crate::commands::runtime_state_fresh(&dev_dir);
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

    let cancel = CancellationToken::new();
    chat_state
        .inflight
        .lock()
        .expect("inflight lock")
        .insert(conversation_id.clone(), cancel.clone());

    let mut stream = match adapter.stream_chat(req, cancel).await {
        Ok(s) => s,
        Err(e) => {
            chat_state
                .inflight
                .lock()
                .expect("inflight lock")
                .remove(&conversation_id);
            return Err(e.user_message());
        }
    };

    // Consume the stream in a detached task so the command itself returns
    // immediately (the frontend gets deltas via the event, not the command
    // result). Only owned, 'static data crosses into the task: the
    // `Arc<ChatState>` (via `.inner().clone()`), the dev dir (to build a
    // fresh `ChatStore` — it's just a `PathBuf` wrapper), and plain values.
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
                    let _ = store.save(&conv);
                }
                ChatEvent::Error { .. } => {
                    finished = true;
                    if !acc.is_empty() {
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
                            let _ = store.save(&conv);
                        }
                    }
                }
            }
        }
        if !finished && !acc.is_empty() {
            // Stream ended without a Done/Error event (shouldn't happen,
            // but keep the partial reply rather than silently drop it).
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
                let _ = store.save(&conv);
            }
        }
        chat_state
            .inflight
            .lock()
            .expect("inflight lock")
            .remove(&conversation_id);
    });
    Ok(())
}

/// Cancels the in-flight stream for a conversation, if any. A no-op when
/// nothing is streaming (e.g. the user double-clicks Stop).
#[tauri::command]
pub fn chat_cancel(conversation_id: String, chat_state: tauri::State<'_, Arc<ChatState>>) {
    if let Some(tok) = chat_state
        .inflight
        .lock()
        .expect("inflight lock")
        .remove(&conversation_id)
    {
        tok.cancel();
    }
}
