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
//!   per-conversation `tokio::sync::Mutex` held across the save. In
//!   `chat_send_message` this means the lock is acquired *before* the load,
//!   not just around the save, so a concurrent rename/model-switch/delete
//!   can't land between our load and save.
//!
//! One deliberate asymmetry in failure handling: if sending fails *before*
//! the stream starts (bad adapter config, unreachable provider, CLI not
//! found), the just-saved user message is popped back out — otherwise every
//! failed Retry would leave a duplicate user turn behind. If it fails
//! *after* the stream starts (mid-generation error), the user message is
//! kept — the model may have partially answered, and popping the question
//! that produced a partial answer would be more confusing than leaving it.

use std::sync::Arc;

use continuum_gateway::{
    ChatEvent, ChatMessage, ChatRequest, ChatRole, ProviderKind, ToolExecutor,
};
use tauri::Emitter;
use tokio_util::sync::CancellationToken;

use crate::chat_store::{
    ChatStore, Conversation, ConversationSummary, StoredMessage, StoredToolCall,
};
use crate::chat_tools;
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

/// Undoes the user-message save from `chat_send_message` when a pre-stream
/// failure (adapter build, stream start) happens after that save already
/// landed. Without this, every failed Retry re-sends the same text and
/// appends another duplicate user turn.
///
/// Re-acquires `conv_lock` (it was dropped after the initial save — see the
/// comment at that call site), reloads the conversation, and pops the
/// trailing message only if it is still exactly the one just appended (last
/// message, user role, matching content) — defensive against a concurrent
/// edit landing in between. Mid-stream failures (inside the consumer task
/// spawned further down) deliberately do NOT call this — see the module
/// doc's note on that asymmetry.
async fn unwind_pending_user_message(
    conv_lock: &tokio::sync::Mutex<()>,
    store: &ChatStore,
    conversation_id: &str,
    text: &str,
) {
    let _guard = conv_lock.lock().await;
    let Some(mut conv) = store.get(conversation_id) else {
        return;
    };
    let is_our_pending_message = conv
        .messages
        .last()
        .is_some_and(|m| m.role == ChatRole::User && m.content == text);
    if !is_our_pending_message {
        return;
    }
    conv.messages.pop();
    if let Err(e) = store.save(&conv) {
        tracing::error!(
            layer = "desktop",
            component = "chat",
            conversation_id = %conversation_id,
            error = %e,
            "failed to unwind pending user message after pre-stream failure"
        );
    }
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
    preferred_language: &str,
) -> String {
    let base = cfg
        .system_prompt_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    format!(
        "{base}\n\n## Live status\n- Continuum version: {version}\n- Background runtime: {}\n- You are: {model} via {provider}\n- Selected response language: {preferred_language}\n\nRespond in the selected response language, even when the user's message is written in another language. A direct request to use another language may override it for that turn only.\n",
        if runtime_running { "running" } else { "not running" }
    )
}

/// "## Memory tools" system-prompt section, naming the tools that are
/// actually active for this provider kind and when to use them. Appended
/// by `chat_send_message` ONLY on turns where memory tools are wired into
/// the request (executor for HTTP providers, MCP for the Claude CLI).
fn memory_tools_section(kind: ProviderKind) -> String {
    let tools_line = match kind {
        ProviderKind::ClaudeCli => {
            "Your memory tools are the `mcp__continuum__memory_vault_*` family: \
             `memory_vault_search`, `memory_vault_get`, `memory_vault_save`, \
             `memory_vault_resolve`, and `memory_vault_delete`. Use `memory_vault_delete` \
             to remove a memory."
        }
        ProviderKind::OpenAiCompat | ProviderKind::Anthropic => {
            "Your memory tools are `memory_search`, `memory_get`, `memory_save`, and \
             `memory_delete`."
        }
    };
    format!(
        "## Memory tools\n\
         {tools_line}\n\
         \n\
         - Save a memory when the user explicitly asks you to remember something, and \
         when they state a durable personal fact (their name, a preference, a decision).\n\
         - Search memory before answering questions about the user or their past work.\n\
         - Update a memory by saving with the SAME title as the existing one; delete a \
         memory when the user asks you to forget it.\n\
         - Keep memory titles short and stable.\n"
    )
}

/// Whether an assistant turn that terminated WITHOUT a clean `Done` (error
/// event, cancel/Stop, tool-round limit, or a stream that just ended) still
/// deserves a persisted `StoredMessage`: yes when any text streamed OR any
/// tool call ran. Tool calls can mutate the vault (e.g. `memory_delete`)
/// before the first text delta, so dropping a text-less turn with tool
/// activity would erase the only transcript record of a permanent mutation.
fn should_persist_assistant_turn(acc: &str, tool_calls: &[StoredToolCall]) -> bool {
    !acc.is_empty() || !tool_calls.is_empty()
}

/// Selects which stored messages to send to the model this turn. The full
/// conversation is persisted on disk; only this tail goes into the request so
/// per-turn input tokens stay bounded as a chat grows.
///
/// `window == 0` sends every message (the pre-window behavior). Otherwise the
/// last `window` messages are taken, and any leading non-`user` messages are
/// then dropped so the first message the provider sees is a user turn — the
/// Anthropic Messages API rejects a request whose first message is not
/// `user`, and starting the kept context at a user turn is also the most
/// coherent slice for the model. The just-sent user message is always the
/// last entry of `messages`, so it is always retained.
fn window_messages(messages: &[StoredMessage], window: u32) -> &[StoredMessage] {
    if window == 0 {
        return messages;
    }
    let take = window as usize;
    let start = messages.len().saturating_sub(take);
    let mut slice = &messages[start..];
    // Drop leading assistant/tool turns so the first kept message is a user
    // turn. Stop at the first user message — never skip past it.
    while let Some(first) = slice.first() {
        if first.role == ChatRole::User {
            break;
        }
        slice = &slice[1..];
    }
    slice
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
    memory: tauri::State<'_, Arc<crate::memory::MemoryState>>,
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

    // The lock is acquired BEFORE the load, not just around the save, so
    // the whole load -> mutate -> save sequence below is one atomic unit
    // with respect to every other load-mutate-save on this conversation
    // (rename, model switch, delete) — see the module doc's "concurrent
    // writers" hazard. Loading outside the lock would let a concurrent
    // rename/model-switch land between our load and save and be silently
    // reverted, or let a concurrent delete's file be resurrected by our
    // stale save.
    let guard = conv_lock.lock().await;
    let mut conv = match store.get(&conversation_id) {
        Some(c) => c,
        None => {
            drop(guard);
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err("Unknown conversation".into());
        }
    };
    let providers = chat_state.providers.lock().expect("providers lock").load();
    let conn = match providers.iter().find(|p| p.id == conv.provider_id) {
        Some(c) => c.clone(),
        None => {
            drop(guard);
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err("This conversation's provider was removed. Pick a model again.".into());
        }
    };

    conv.messages.push(StoredMessage::user(text.trim()));
    conv.derive_title();
    if let Err(e) = store.save(&conv) {
        drop(guard);
        release_inflight(&chat_state.inflight, &conversation_id);
        return Err(e.to_string());
    }
    // Released here, before adapter construction/stream start — those can
    // block on network connects or a CLI spawn and must not hold up other
    // load-mutate-save ops on this conversation for that long. The
    // pre-stream error paths below that need to undo the save each
    // re-acquire it via `unwind_pending_user_message`.
    drop(guard);

    let chat_cfg = state.runtime.config_snapshot().chat.clone();
    // No std lock is held across this call — `build_adapter` itself does
    // not await, and the `.lock()`s above have already been dropped.
    let adapter = match build_adapter(&conn, chat_state.secrets.as_ref(), &chat_cfg) {
        Ok(a) => a,
        Err(e) => {
            unwind_pending_user_message(&conv_lock, &store, &conversation_id, text.trim()).await;
            release_inflight(&chat_state.inflight, &conversation_id);
            return Err(e);
        }
    };

    // Memory tools: when enabled and the vault opens, the chat AI can read
    // and write the memory vault — via the in-process executor for HTTP
    // providers, or via an attached continuum-mcp server for the Claude
    // CLI. A vault that fails to open degrades this send to a tool-less
    // chat (warn + continue), never to a failed send.
    let vault = if chat_cfg.memory_tools_enabled {
        match memory.vault().await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(
                    layer = "desktop",
                    component = "chat",
                    error = %e,
                    "memory vault unavailable — chat runs without memory tools"
                );
                None
            }
        }
    } else {
        None
    };

    let mut tools = Vec::new();
    let mut executor: Option<Arc<dyn ToolExecutor>> = None;
    let mut mcp = None;
    let mut tools_section = None;
    if let Some(vault) = &vault {
        match conn.kind {
            ProviderKind::ClaudeCli => {
                // CLI path: tool traffic travels over MCP; continuum-mcp
                // opens the same vault directory this process uses. A
                // missing binary means no tools this turn (logged once).
                if let Some(spec) = chat_tools::mcp_spec(memory.vault_dir(), &dev_dir) {
                    mcp = Some(spec);
                    tools_section = Some(memory_tools_section(conn.kind));
                }
            }
            ProviderKind::OpenAiCompat | ProviderKind::Anthropic => {
                tools = chat_tools::memory_tool_defs();
                executor = Some(Arc::new(chat_tools::VaultToolExecutor {
                    vault: vault.clone(),
                    include_sensitive: chat_cfg.include_sensitive_memory,
                }) as Arc<dyn ToolExecutor>);
                tools_section = Some(memory_tools_section(conn.kind));
            }
        }
    }

    // Prompt injection: retrieve vault notes matching the new user message
    // and inject them as a "## Memory context" section. Best-effort — a
    // search failure (or zero hits) just means no section this turn.
    let mut memory_context = String::new();
    if chat_cfg.memory_context_notes_max > 0 {
        if let Some(vault) = &vault {
            let query: String = text.trim().chars().take(200).collect();
            match vault
                .search(&query, chat_cfg.memory_context_notes_max.saturating_mul(2))
                .await
            {
                Ok(hits) => {
                    let mut kept: Vec<_> = hits
                        .into_iter()
                        .filter(|h| h.status == continuum_memory::NodeStatus::Confirmed)
                        .filter(|h| {
                            chat_cfg.include_sensitive_memory
                                || h.sensitivity != continuum_memory::Sensitivity::Sensitive
                        })
                        .collect();
                    kept.truncate(chat_cfg.memory_context_notes_max as usize);
                    memory_context = chat_tools::memory_context_section(&kept);
                }
                Err(e) => tracing::warn!(
                    layer = "desktop",
                    component = "chat",
                    error = %e,
                    "memory context search failed — continuing without context"
                ),
            }
        }
    }

    let runtime_running = crate::components::runtime_alive(&dev_dir);
    let mut system = system_prompt(
        &chat_cfg,
        runtime_running,
        env!("CARGO_PKG_VERSION"),
        &conn.display_name,
        &conv.model,
        &crate::onboarding::preferred_language(&dev_dir),
    );
    if let Some(section) = tools_section {
        system.push('\n');
        system.push_str(&section);
    }
    if !memory_context.is_empty() {
        system.push('\n');
        system.push_str(&memory_context);
    }

    let req = ChatRequest {
        model: conv.model.clone(),
        system,
        // Ship only the recent tail of the conversation, not the whole
        // history — see `window_messages`. The full transcript stays on
        // disk; this bounds per-turn input tokens as the chat grows.
        messages: window_messages(&conv.messages, chat_cfg.history_message_window)
            .iter()
            .map(|m| ChatMessage {
                role: m.role,
                content: m.content.clone(),
            })
            .collect(),
        max_tokens: chat_cfg.max_tokens,
        temperature: chat_cfg.temperature,
        tools,
        executor,
        mcp,
        tool_max_rounds: chat_cfg.memory_tool_max_rounds,
    };

    let mut stream = match adapter.stream_chat(req, cancel).await {
        Ok(s) => s,
        Err(e) => {
            unwind_pending_user_message(&conv_lock, &store, &conversation_id, text.trim()).await;
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
        let mut tool_calls: Vec<StoredToolCall> = Vec::new();
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
                ChatEvent::ToolCall { id, name, input } => {
                    tool_calls.push(StoredToolCall {
                        id,
                        name,
                        input,
                        output: None,
                        is_error: false,
                        duration_ms: 0,
                    });
                }
                ChatEvent::ToolResult {
                    id,
                    output,
                    is_error,
                    duration_ms,
                } => {
                    // Fill the matching (still-unresolved) call entry; an
                    // orphan result without a preceding call gets its own
                    // entry with an empty name, matching gateway behavior.
                    if let Some(entry) = tool_calls
                        .iter_mut()
                        .find(|t| t.id == id && t.output.is_none())
                    {
                        entry.output = Some(output);
                        entry.is_error = is_error;
                        entry.duration_ms = duration_ms;
                    } else {
                        tool_calls.push(StoredToolCall {
                            id,
                            name: String::new(),
                            input: serde_json::Value::Null,
                            output: Some(output),
                            is_error,
                            duration_ms,
                        });
                    }
                }
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
                        tool_calls: tool_calls.clone(),
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
                    // Skip persistence only when NOTHING streamed AND no
                    // tools ran — an executed tool call may have already
                    // mutated the vault, so its record must survive the
                    // error even with empty content.
                    if should_persist_assistant_turn(&acc, &tool_calls) {
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
                                tool_calls: tool_calls.clone(),
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
        if !finished && should_persist_assistant_turn(&acc, &tool_calls) {
            // Stream ended without a Done/Error event (shouldn't happen,
            // but keep the partial reply — streamed text and/or executed
            // tool calls — rather than silently drop it. Only a turn with
            // nothing streamed AND no tools run is skipped.
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
                    tool_calls,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_enforces_selected_english_language() {
        let prompt = system_prompt(
            &continuum_core::config::ChatConfig::default(),
            true,
            "0.1.0",
            "Local provider",
            "test-model",
            "en",
        );

        assert!(prompt.contains("Selected response language: en"));
        assert!(prompt.contains("Respond in the selected response language"));
    }

    /// The base system prompt never mentions memory tools — the section is
    /// appended by `chat_send_message` only on turns where tools are
    /// actually wired into the request.
    #[test]
    fn base_system_prompt_has_no_memory_tools_section() {
        let prompt = system_prompt(
            &continuum_core::config::ChatConfig::default(),
            true,
            "0.1.0",
            "Local provider",
            "test-model",
            "en",
        );
        assert!(!prompt.contains("## Memory tools"));
    }

    #[test]
    fn memory_tools_section_names_http_tools() {
        let s = memory_tools_section(ProviderKind::OpenAiCompat);
        assert!(s.starts_with("## Memory tools"));
        for tool in [
            "memory_search",
            "memory_get",
            "memory_save",
            "memory_delete",
        ] {
            assert!(s.contains(tool), "missing {tool}");
        }
        assert!(!s.contains("mcp__continuum__"));
        assert!(s.contains("SAME title"));
    }

    #[test]
    fn memory_tools_section_names_mcp_tools_for_cli() {
        let s = memory_tools_section(ProviderKind::ClaudeCli);
        assert!(s.starts_with("## Memory tools"));
        assert!(s.contains("mcp__continuum__memory_vault_"));
        for tool in [
            "memory_vault_search",
            "memory_vault_get",
            "memory_vault_save",
            "memory_vault_resolve",
            "memory_vault_delete",
        ] {
            assert!(s.contains(tool), "missing {tool}");
        }
    }

    /// Regression test for the whole-branch review's Finding I1: a turn
    /// that executed tools (the vault may already be mutated — e.g. a
    /// completed `memory_delete`) but streamed NO text before hitting an
    /// error/cancel must still be persisted, or the tool cards vanish on
    /// reload while the mutation is permanent.
    #[test]
    fn turn_with_tools_but_no_text_is_persisted() {
        let ran_tool = StoredToolCall {
            id: "call_1".into(),
            name: "memory_delete".into(),
            input: serde_json::json!({"id": "mem_1"}),
            output: Some(r#"{"deleted":true}"#.into()),
            is_error: false,
            duration_ms: 3,
        };
        assert!(should_persist_assistant_turn("", &[ran_tool]));
    }

    #[test]
    fn turn_with_text_is_persisted_and_empty_turn_is_not() {
        assert!(should_persist_assistant_turn("partial reply", &[]));
        assert!(
            !should_persist_assistant_turn("", &[]),
            "a turn with nothing streamed and no tools run stays unpersisted"
        );
    }

    /// `chat_send_message` itself takes `tauri::State`/`tauri::AppHandle`
    /// params that need a running Tauri app to construct, so it isn't
    /// exercised directly here. `unwind_pending_user_message` is the free
    /// function that contains all of Finding 3's actual logic (find the
    /// pending user message, pop it, save) and has no Tauri dependency, so
    /// these tests cover it directly against a real `ChatStore` on a temp
    /// dir — the same persistence path `chat_send_message` uses.
    fn conv_with_trailing_user_message(store: &ChatStore, text: &str) -> Conversation {
        let mut conv = store.create("prov-1", "model-1").expect("create");
        conv.messages.push(StoredMessage::user(text));
        store.save(&conv).expect("save");
        conv
    }

    #[tokio::test]
    async fn unwind_pops_matching_trailing_user_message() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = ChatStore::new(dir.path().to_path_buf());
        let conv = conv_with_trailing_user_message(&store, "hello");
        let lock = tokio::sync::Mutex::new(());

        unwind_pending_user_message(&lock, &store, &conv.id, "hello").await;

        let reloaded = store.get(&conv.id).expect("get");
        assert!(
            reloaded.messages.is_empty(),
            "the just-appended user message should have been popped, leaving no duplicate for a Retry to build on"
        );
    }

    #[tokio::test]
    async fn unwind_leaves_unrelated_trailing_message_untouched() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = ChatStore::new(dir.path().to_path_buf());
        // Trailing message belongs to a different send (different text) -
        // e.g. a concurrent send landed in between. Must not be popped.
        let conv = conv_with_trailing_user_message(&store, "the real last message");
        let lock = tokio::sync::Mutex::new(());

        unwind_pending_user_message(&lock, &store, &conv.id, "some other text").await;

        let reloaded = store.get(&conv.id).expect("get");
        assert_eq!(
            reloaded.messages.len(),
            1,
            "unrelated trailing message must survive unwind"
        );
        assert_eq!(reloaded.messages[0].content, "the real last message");
    }

    #[tokio::test]
    async fn unwind_ignores_non_trailing_match() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = ChatStore::new(dir.path().to_path_buf());
        let mut conv = store.create("prov-1", "model-1").expect("create");
        conv.messages.push(StoredMessage::user("question"));
        conv.messages.push(StoredMessage {
            role: ChatRole::Assistant,
            content: "answer".into(),
            ts: chrono::Utc::now(),
            model: None,
            duration_ms: None,
            usage: None,
            aborted: false,
            tool_calls: Vec::new(),
        });
        store.save(&conv).expect("save");
        let lock = tokio::sync::Mutex::new(());

        // "question" matches an earlier message's text, but it's no longer
        // the LAST message (the assistant reply is) - nothing should pop.
        unwind_pending_user_message(&lock, &store, &conv.id, "question").await;

        let reloaded = store.get(&conv.id).expect("get");
        assert_eq!(
            reloaded.messages.len(),
            2,
            "a text match that isn't the trailing message must not be popped"
        );
    }

    #[tokio::test]
    async fn unwind_is_noop_on_missing_conversation() {
        let dir = tempfile::tempdir().expect("tmp");
        let store = ChatStore::new(dir.path().to_path_buf());
        let lock = tokio::sync::Mutex::new(());
        // Must not panic when the conversation is gone (e.g. deleted
        // concurrently with the failed send) - just do nothing.
        unwind_pending_user_message(&lock, &store, "does-not-exist", "hello").await;
    }

    /// Helper to build a small alternating user/assistant transcript without
    /// touching disk — `window_messages` is pure and only needs the slice.
    fn transcript(n_turns: usize) -> Vec<StoredMessage> {
        let mut out = Vec::with_capacity(n_turns * 2);
        for i in 0..n_turns {
            out.push(StoredMessage::user(&format!("q{i}")));
            out.push(StoredMessage {
                role: ChatRole::Assistant,
                content: format!("a{i}"),
                ts: chrono::Utc::now(),
                model: None,
                duration_ms: None,
                usage: None,
                aborted: false,
                tool_calls: Vec::new(),
            });
        }
        out
    }

    #[test]
    fn window_zero_sends_entire_history() {
        let msgs = transcript(5);
        let win = window_messages(&msgs, 0);
        assert_eq!(win.len(), msgs.len(), "window 0 must opt out of windowing");
    }

    #[test]
    fn window_keeps_tail_and_last_is_user() {
        // 5 turns = 10 messages, plus a trailing user message simulating the
        // just-appended send -> 11 messages. Window of 4 starts at index 7
        // (an assistant turn), which the helper then drops, so 3 survive.
        // The invariant is: at most `window` kept, first is user, last is
        // the just-sent user message.
        let mut msgs = transcript(5);
        msgs.push(StoredMessage::user("current question"));
        let win = window_messages(&msgs, 4);
        assert!(win.len() <= 4, "window keeps at most `window` messages");
        assert_eq!(
            win[0].role,
            ChatRole::User,
            "first kept must be a user turn"
        );
        assert_eq!(win.last().unwrap().role, ChatRole::User);
        assert_eq!(win.last().unwrap().content, "current question");
    }

    #[test]
    fn window_drops_leading_assistant_so_first_is_user() {
        // Construct a slice boundary that lands on an assistant turn: 5
        // turns (10 msgs) + trailing user = 11; window 5 starts at index 6,
        // which is the assistant "a3". The helper must skip it.
        let mut msgs = transcript(5);
        msgs.push(StoredMessage::user("current question"));
        let win = window_messages(&msgs, 5);
        assert!(win.len() <= 5);
        assert_eq!(
            win[0].role,
            ChatRole::User,
            "leading assistant turns must be dropped so the first message is user"
        );
    }

    #[test]
    fn window_larger_than_history_returns_everything_starting_at_user() {
        let mut msgs = transcript(2);
        msgs.push(StoredMessage::user("current question"));
        let win = window_messages(&msgs, 999);
        // No truncation, and still starts at the first user turn.
        assert_eq!(win.len(), msgs.len());
        assert_eq!(win[0].role, ChatRole::User);
    }
}
