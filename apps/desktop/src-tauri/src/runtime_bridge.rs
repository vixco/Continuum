//! Bridge to the separately-running `continuum` runtime binary.
//!
//! The headless `continuum` binary owns the llama-cpp-backed triage loop and
//! all perception watchers. In Phase 6 we route dashboard reads through
//! the state store (populated by a small JSON file the binary writes to
//! `~/.continuum-dev/state.json` on every meaningful update) and through a
//! named pipe for control messages.
//!
//! The bridge runs **two** listeners in parallel:
//!
//! 1. A 2 s file-tail over `state.json` — the durable, portable path. Harmless
//!    when the file doesn't exist; parse errors are emitted as a
//!    `continuum:runtime_error` event so the dashboard can surface
//!    "state.json is corrupt" rather than silently showing stale flags.
//! 2. A Windows named-pipe server at `\\.\pipe\continuum-runtime` that the
//!    runtime process opens and writes NDJSON snapshots into. On non-Windows
//!    builds the pipe path compiles to a no-op and only the file-tail runs.
//!
//! When the `continuum` binary is not running, nothing is read, nothing
//! breaks, and the dashboard falls back to `Unknown`/`Degrading` statuses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter};

use continuum_core::runtime::ContinuumRuntime;
use continuum_core::runtime_publish::RuntimeSnapshot;
use continuum_core::state::{StateHandle, VoiceMode};

/// Shared health-state slot for the `pipe_health` Tauri command. Written by
/// the pipe server thread, read by the dashboard command. Module-level so
/// the `pipe_health` command in `commands.rs` can read it without us
/// threading it through `AppState`. Gated to Windows because the producer
/// (`set_pipe_connected` / `set_pipe_last_error`) lives in the
/// Windows-only pipe server; on other platforms the field is always
/// `connected = false` and `last_error = None`.
#[cfg(windows)]
static PIPE_CONNECTED: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static PIPE_LAST_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Spawn both bridge listeners. Returns immediately; runs on the tokio
/// runtime the Tauri app already owns.
pub fn spawn_ipc_listener(runtime: ContinuumRuntime, app: AppHandle) {
    let dev_dir = runtime.dev_dir();
    let state_path = dev_dir.join("state.json");
    let state = runtime.state.clone();

    // --- File-tail (the portable path) ----------------------------------
    let last_was_err = Arc::new(AtomicBool::new(false));
    let state_for_tail = state.clone();
    let path_for_tail = state_path.clone();
    let app_for_tail = app.clone();
    let err_latch_for_tail = last_was_err.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        loop {
            ticker.tick().await;
            match tick_once(&path_for_tail, &state_for_tail).await {
                Ok(()) => {
                    if err_latch_for_tail.swap(false, Ordering::AcqRel) {
                        let _ =
                            app_for_tail.emit("continuum:runtime_error", serde_json::Value::Null);
                    }
                }
                Err(e) => {
                    if !err_latch_for_tail.swap(true, Ordering::AcqRel) {
                        let _ = app_for_tail.emit(
                            "continuum:runtime_error",
                            serde_json::json!({
                                "path": path_for_tail.display().to_string(),
                                "error": e.to_string(),
                            }),
                        );
                    }
                    tracing::debug!(
                        layer = "dashboard",
                        component = "runtime_bridge",
                        error = %e,
                        "runtime state.json read failed"
                    );
                }
            }
        }
    });

    // --- Named-pipe server (Windows only) -------------------------------
    // Best-effort: a failure here (e.g. the runtime never connects) must
    // not break the file-tail. We launch it on a dedicated OS thread
    // because the std-only Win32 pipe API blocks on connect/read. To
    // reach `StateHandle` (which is async) without spawning a second
    // tokio runtime, we hand the dedicated thread a clone of the
    // Tauri runtime's `Handle` and call `handle.block_on(...)` from
    // the OS thread. AGENTS.md non-negotiable #4 forbids a second
    // runtime; this is the canonical alternative.
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("continuum-pipe-server".into())
        .spawn(move || {
            pipe::run_pipe_server(state, app, handle);
        })
        .expect("spawn named-pipe thread");
}

async fn tick_once(path: &PathBuf, state: &StateHandle) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents = tokio::fs::read_to_string(path).await?;
    let snap: RuntimeSnapshot = serde_json::from_str(&contents)?;
    apply_snapshot(&snap, state).await;
    Ok(())
}

/// Push a parsed runtime snapshot into the dashboard's state store.
/// Shared by the file-tail and the named-pipe paths so both produce
/// identical on-screen behaviour.
async fn apply_snapshot(snap: &RuntimeSnapshot, state: &StateHandle) {
    let paused = snap.paused;
    state
        .set_system_flag(|s| {
            s.triage_model_loaded = snap.triage_model_loaded;
            s.vision_model_loaded = snap.vision_model_loaded;
            s.tts_loaded = snap.tts_loaded;
            s.stt_loaded = snap.stt_loaded;
            s.orchestrator_ready = snap.orchestrator_ready;
            // Task A8 (closing the A2 seam): mirror the runtime's
            // pause_all flag. `None` = pre-A8 state.json — leave the
            // stored value untouched rather than inventing one.
            if let Some(paused) = paused {
                s.paused = paused;
            }
        })
        .await;
    // Task 11: mirror the runtime's curator (Plan B memory-vault) status
    // into the dashboard's own state store, alongside the memory stats
    // it already renders. `snap.curator` is only `None` for a `state.json`
    // written before this field existed — everything since then is `Some`
    // (with `enabled: false` and zeroed counters when the curator itself
    // hasn't spawned), which the dashboard needs to tell apart from
    // "haven't heard from the runtime at all yet".
    state.set_curator_snapshot(snap.curator.clone()).await;
    // Task C1 (spec §4.8 consumers, §7): mirror the runtime's published
    // session state and per-source context-engine health into the
    // dashboard store, which is what the Context page renders. The A8
    // `context_engine` section was published-but-ignored until now; it
    // rides the same setter so both always come from one snapshot.
    // Both are stored verbatim including `None` — see
    // `StateHandle::set_context_snapshot`.
    // Task C5 adds the third section: the Context page's list data
    // (projects, override rules, pins, recent events, live toggles,
    // continuation candidates). This process cannot open the raw-log
    // database, so `state.json` is the only path those lists take.
    state
        .set_context_snapshot(
            snap.session_state.clone(),
            snap.context_engine.clone(),
            snap.context_page.clone(),
        )
        .await;
    if let Some(mode) = snap.voice_mode.as_deref() {
        let parsed = match mode {
            "idle" => VoiceMode::Idle,
            "listening" => VoiceMode::Listening,
            "thinking" => VoiceMode::Thinking,
            "speaking" => VoiceMode::Speaking,
            "muted" => VoiceMode::Muted,
            "error" => VoiceMode::Error,
            _ => VoiceMode::Idle,
        };
        state.set_voice_mode(parsed).await;
    }
    if let Some(ref t) = snap.partial_transcript {
        state.set_partial_transcript(t).await;
    }
    state
        .set_live_context_status(
            snap.monitor_count,
            snap.capture_event_count,
            snap.dropped_capture_event_count,
            snap.last_capture_at,
        )
        .await;
    state
        .apply_voice_runtime_snapshot(
            snap.voice_volume,
            snap.mic_input_level,
            snap.wake_word_enabled,
            snap.tts_queue_len,
            snap.ambient_mute_active,
            snap.detected_call_app.clone(),
        )
        .await;
    state
        .set_voice_frontend_status(snap.voice_frontend_mode.clone(), snap.moshi_loaded)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applies_live_voice_telemetry_from_runtime_snapshot() {
        let state = StateHandle::new();
        let snapshot = RuntimeSnapshot {
            voice_mode: Some("speaking".into()),
            partial_transcript: Some("hello".into()),
            mic_input_level: Some(0.64),
            voice_volume: Some(0.72),
            tts_queue_len: Some(2),
            ambient_mute_active: Some(true),
            detected_call_app: Some("Teams.exe".into()),
            wake_word_enabled: Some(true),
            ..RuntimeSnapshot::default()
        };

        apply_snapshot(&snapshot, &state).await;

        let applied = state.snapshot().await;
        assert_eq!(applied.voice.mode, VoiceMode::Speaking);
        assert_eq!(applied.voice.partial_transcript, "hello");
        assert_eq!(applied.voice.mic_input_level, 0.64);
        assert_eq!(applied.voice.volume, 0.72);
        assert_eq!(applied.voice.tts_queue_len, 2);
        assert!(applied.voice.ambient_mute_active);
        assert_eq!(
            applied.voice.detected_call_app.as_deref(),
            Some("Teams.exe")
        );
        assert!(applied.voice.wake_word_enabled);
    }

    /// Task A8 (A2 seam closed): the runtime's pause_all flag reaches
    /// `SystemState.paused`; a pre-A8 snapshot (`paused: None`) leaves
    /// the stored value untouched.
    #[tokio::test]
    async fn applies_paused_flag_from_runtime_snapshot() {
        let state = StateHandle::new();
        apply_snapshot(
            &RuntimeSnapshot {
                paused: Some(true),
                ..RuntimeSnapshot::default()
            },
            &state,
        )
        .await;
        assert!(state.snapshot().await.system.paused);

        // None (old state.json) must not reset it.
        apply_snapshot(&RuntimeSnapshot::default(), &state).await;
        assert!(state.snapshot().await.system.paused);

        apply_snapshot(
            &RuntimeSnapshot {
                paused: Some(false),
                ..RuntimeSnapshot::default()
            },
            &state,
        )
        .await;
        assert!(!state.snapshot().await.system.paused);
    }

    /// Task C1: the published session state and the (until now ignored)
    /// A8 context-engine health both reach the dashboard store.
    #[tokio::test]
    async fn applies_session_state_and_context_engine_from_runtime_snapshot() {
        use continuum_core::context::session_state::{SessionState, StampedText};
        use continuum_core::runtime_publish::{ComponentHealthSummary, ContextEngineSnapshot};

        let state = StateHandle::new();
        let now = chrono::Utc::now();
        apply_snapshot(
            &RuntimeSnapshot {
                session_state: Some(SessionState {
                    active_project: Some("continuum".into()),
                    current_goal: Some("ship the context engine".into()),
                    current_task: Some("publish session state".into()),
                    active_app: Some("Code.exe".into()),
                    open_files: vec!["runtime_bridge.rs".into()],
                    last_error: Some(StampedText::new("cargo build failed", now)),
                    confidence: 0.7,
                    ..SessionState::default()
                }),
                context_engine: Some(ContextEngineSnapshot {
                    idle: false,
                    file_watcher: Some(ComponentHealthSummary {
                        healthy: true,
                        enabled: false,
                        should_restart: false,
                        detail: Some("disabled by [file_watcher].enabled".into()),
                    }),
                    ..ContextEngineSnapshot::default()
                }),
                ..RuntimeSnapshot::default()
            },
            &state,
        )
        .await;

        let applied = state.snapshot().await;
        let session = applied.context.session.expect("session state applied");
        assert_eq!(session.active_project.as_deref(), Some("continuum"));
        assert_eq!(
            session.current_task.as_deref(),
            Some("publish session state")
        );
        assert_eq!(session.open_files, vec!["runtime_bridge.rs".to_string()]);
        assert_eq!(
            session.last_error.map(|e| e.text).as_deref(),
            Some("cargo build failed")
        );
        let engine = applied.context.engine.expect("context engine applied");
        let file_watcher = engine.file_watcher.expect("file watcher summary");
        assert!(file_watcher.healthy && !file_watcher.enabled);
    }

    /// Task C5: the Context page's list data (projects, rules, pins, the
    /// recent-events strip, live toggles, continuation candidates) reaches
    /// the dashboard store. This process cannot open the raw-log database,
    /// so a regression here silently empties the whole page.
    #[tokio::test]
    async fn applies_context_page_from_runtime_snapshot() {
        use continuum_core::runtime_publish::{
            ContextEventView, ContextPageSnapshot, ContinuationCandidateView,
            ObservationTogglesView, OverrideRuleView, ProjectSummaryView, SessionPinView,
        };

        let state = StateHandle::new();
        apply_snapshot(
            &RuntimeSnapshot {
                context_page: Some(ContextPageSnapshot {
                    projects: vec![
                        ProjectSummaryView {
                            id: "continuum".into(),
                            name: "Continuum".into(),
                            status: "configured".into(),
                            root_paths: vec!["~/code/continuum".into()],
                            frames_count: 42,
                            active: true,
                            ..ProjectSummaryView::default()
                        },
                        ProjectSummaryView {
                            id: "simcharts".into(),
                            name: "SimCharts".into(),
                            status: "discovered".into(),
                            ..ProjectSummaryView::default()
                        },
                    ],
                    rules: vec![OverrideRuleView {
                        match_process: Some("Code.exe".into()),
                        match_title_substring: None,
                        action: "force_project".into(),
                        project_id: "continuum".into(),
                    }],
                    pins: vec![SessionPinView {
                        field: "project".into(),
                        value: Some("continuum".into()),
                    }],
                    recent_events: vec![ContextEventView {
                        id: 7,
                        ts: "2026-08-05T10:00:00Z".into(),
                        source: "screen".into(),
                        event_type: "error".into(),
                        application: "Code.exe".into(),
                        window_title: "main.rs — continuum".into(),
                        summary: "build failed".into(),
                        count: 14,
                        project_id: Some("continuum".into()),
                        raw_reference: Some("frame-1".into()),
                    }],
                    activity_trace: Vec::new(),
                    toggles: ObservationTogglesView {
                        mic: false,
                        ..ObservationTogglesView::default()
                    },
                    continuation: vec![ContinuationCandidateView {
                        kind: "session_task".into(),
                        label: "current task".into(),
                        text: "finish C5".into(),
                        confidence: 0.8,
                    }],
                }),
                ..RuntimeSnapshot::default()
            },
            &state,
        )
        .await;

        let page = state
            .snapshot()
            .await
            .context
            .page
            .expect("context page applied");
        assert_eq!(page.projects.len(), 2);
        assert!(page.projects[0].active);
        assert_eq!(page.projects[1].status, "discovered");
        assert_eq!(page.rules[0].action, "force_project");
        assert_eq!(page.pins[0].field, "project");
        assert_eq!(page.recent_events[0].count, 14);
        assert_eq!(page.recent_events[0].id, 7);
        assert!(!page.toggles.mic, "the live toggle value survives");
        assert!(page.toggles.screen);
        assert_eq!(page.continuation[0].kind, "session_task");

        // A runtime that stops reporting must clear the page rather than
        // leave a stale ghost (the C1 verbatim-store contract).
        apply_snapshot(&RuntimeSnapshot::default(), &state).await;
        assert!(state.snapshot().await.context.page.is_none());
    }

    /// Task C1 legacy compat: a `state.json` written by a pre-C1 runtime
    /// (no `session_state`, no `context_engine`) still parses and applies
    /// through the real file-tail path — the dashboard just shows nothing
    /// for the Context page instead of failing to read the file at all.
    #[tokio::test]
    async fn legacy_state_json_without_context_fields_still_applies() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{
                "triage_model_loaded": true,
                "frame_count": 7,
                "wake_count": 1,
                "last_update": "2026-04-14T10:00:00Z"
            }"#,
        )
        .expect("write");

        let state = StateHandle::new();
        tick_once(&path, &state)
            .await
            .expect("legacy snapshot reads");

        let applied = state.snapshot().await;
        assert!(applied.system.triage_model_loaded);
        assert!(applied.context.session.is_none());
        assert!(applied.context.engine.is_none());
        assert!(applied.context.page.is_none());
    }

    /// Task C1 end-to-end: the bytes the runtime publisher actually writes
    /// (`runtime_publish::write_snapshot`) are read back by the bridge's
    /// own file-tail. This is the contract C3's `context_session` tool and
    /// B5's rehydration reader also depend on.
    #[tokio::test]
    async fn published_state_json_round_trips_through_the_file_tail() {
        use continuum_core::context::session_state::{read_persisted_state, SessionState};
        use continuum_core::runtime_publish::write_snapshot;

        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state.json");
        let session = SessionState {
            active_project: Some("continuum".into()),
            current_task: Some("close the C1 seam".into()),
            confidence: 0.65,
            ..SessionState::default()
        };
        write_snapshot(
            &path,
            &RuntimeSnapshot {
                session_state: Some(session.clone()),
                ..RuntimeSnapshot::default()
            },
        )
        .expect("publish");

        let state = StateHandle::new();
        tick_once(&path, &state)
            .await
            .expect("published snapshot reads");
        assert_eq!(
            state
                .snapshot()
                .await
                .context
                .session
                .and_then(|s| s.current_task)
                .as_deref(),
            Some("close the C1 seam")
        );
        // Same file, the core-side rehydration reader.
        assert_eq!(read_persisted_state(dir.path()), Some(session));
    }
}

/// Snapshot of the named-pipe health. Surfaced by the `pipe_health`
/// Tauri command and by the `continuum:runtime_pipe` Tauri event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipeHealth {
    pub connected: bool,
    pub last_error: Option<String>,
    /// Pipe name when running on Windows; `None` on platforms where the
    /// named-pipe path is a no-op.
    pub pipe_name: Option<&'static str>,
}

/// Read the current pipe-health state. Safe to call before the pipe
/// server has booted — the result will just be `connected = false`.
pub fn current_pipe_health() -> PipeHealth {
    #[cfg(windows)]
    {
        let connected = PIPE_CONNECTED.load(Ordering::Acquire);
        let last_error = PIPE_LAST_ERROR.lock().ok().and_then(|g| g.clone());
        PipeHealth {
            connected,
            last_error,
            pipe_name: Some(pipe::PIPE_NAME),
        }
    }
    #[cfg(not(windows))]
    {
        PipeHealth {
            connected: false,
            last_error: None,
            pipe_name: None,
        }
    }
}

#[cfg(windows)]
fn set_pipe_connected(v: bool) {
    PIPE_CONNECTED.store(v, Ordering::Release);
}

#[cfg(windows)]
fn set_pipe_last_error(msg: Option<String>) {
    if let Ok(mut g) = PIPE_LAST_ERROR.lock() {
        *g = msg;
    }
}

// ---------------------------------------------------------------------------
// Named-pipe transport (Windows).
// ---------------------------------------------------------------------------
//
// The runtime connects to `\\.\pipe\continuum-runtime` and writes one
// JSON-encoded `RuntimeSnapshot` per line. The server reads to EOF, parses
// the line, applies it, then loops to accept the next connection. A single
// connection at a time is plenty — the runtime reconnects after each write.

#[cfg(windows)]
mod pipe {
    use super::*;
    use std::io::{BufRead, BufReader, Read};

    /// Pipe name. Lives in the device namespace; visible only to the
    /// local user. `\\.\pipe\Global\…` would expose it to other sessions.
    pub(super) const PIPE_NAME: &str = r"\\.\pipe\continuum-runtime";

    pub(super) fn run_pipe_server(
        state: StateHandle,
        app: AppHandle,
        handle: tokio::runtime::Handle,
    ) {
        loop {
            if let Err(e) = serve_one_client(&state, &app, &handle) {
                let msg = e.to_string();
                set_pipe_last_error(Some(msg.clone()));
                tracing::warn!(
                    layer = "dashboard",
                    component = "runtime_bridge",
                    error = %msg,
                    "named-pipe client handler exited; will accept the next connection"
                );
                // Brief backoff so a misbehaving runtime can't spin us.
                std::thread::sleep(Duration::from_millis(250));
            } else {
                set_pipe_last_error(None);
            }
        }
    }

    fn serve_one_client(
        state: &StateHandle,
        app: &AppHandle,
        handle: &tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        let server = create_pipe()?;
        // First-byte wait — blocks until a client connects or times out.
        // 5s is short enough to keep the latches fresh and long enough
        // that an idle runtime doesn't churn the thread.
        server.connect_blocking(Duration::from_secs(5))?;
        set_pipe_connected(true);
        let _ = app.emit(
            "continuum:runtime_pipe",
            serde_json::json!({"connected": true}),
        );

        // Read loop. Each line is one JSON snapshot. The runtime is
        // expected to disconnect after each write, but we keep reading
        // until EOF just in case it streams multiple.
        let mut reader = BufReader::new(server);
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader.read_line(&mut buf)?;
            if n == 0 {
                break;
            }
            let line = buf.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<RuntimeSnapshot>(line) {
                Ok(snap) => {
                    // Apply on the Tauri tokio runtime via the captured
                    // `Handle`. We deliberately avoid spawning a second
                    // runtime here (AGENTS.md non-negotiable #4) — the
                    // Tauri app's `new_multi_thread` runtime has many
                    // workers, so blocking one of them from a dedicated
                    // OS thread is fine.
                    let state = state.clone();
                    handle.block_on(apply_snapshot(&snap, &state));
                }
                Err(e) => {
                    let msg = format!("malformed pipe payload: {e}");
                    set_pipe_last_error(Some(msg.clone()));
                    let _ = app.emit(
                        "continuum:runtime_error",
                        serde_json::json!({"path": PIPE_NAME, "error": msg}),
                    );
                }
            }
        }

        set_pipe_connected(false);
        let _ = app.emit(
            "continuum:runtime_pipe",
            serde_json::json!({"connected": false}),
        );
        Ok(())
    }

    // --- Raw Win32 pipe wrapper -----------------------------------------
    //
    // We use the `windows` crate (v0.59) for the four calls we need:
    // CreateNamedPipeW, ConnectNamedPipe, ReadFile, CloseHandle. A small
    // struct wraps the handle and implements Read so the BufRead above
    // works without further plumbing.

    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    struct PipeHandle {
        handle: windows::Win32::Foundation::HANDLE,
    }

    // SAFETY: the underlying HANDLE is owned exclusively by this thread
    // (the pipe server runs on a dedicated OS thread) and is not shared
    // across threads.
    unsafe impl Send for PipeHandle {}

    fn create_pipe() -> std::io::Result<PipeHandle> {
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
        };
        use windows::Win32::System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        };

        let wide: Vec<u16> = OsStr::new(PIPE_NAME)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: PIPE_NAME is a valid pipe path with a NUL terminator
        // appended by `encode_wide().chain(0)`. The OS allocates the
        // handle; we own it until CloseHandle in Drop.
        let handle = unsafe {
            CreateNamedPipeW(
                windows::core::PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1, // PIPE_UNLIMITED_INSTANCES would block re-create; 1 client at a time
                4096,
                4096,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(PipeHandle { handle })
    }

    impl PipeHandle {
        fn connect_blocking(&self, timeout: Duration) -> std::io::Result<()> {
            use windows::Win32::System::Pipes::ConnectNamedPipe;
            let start = std::time::Instant::now();
            loop {
                // SAFETY: self.handle is a valid pipe handle owned by us.
                let r = unsafe { ConnectNamedPipe(self.handle, None) };
                if r.is_ok() {
                    return Ok(());
                }
                let err = std::io::Error::last_os_error();
                // ERROR_PIPE_CONNECTED (535) = client connected between
                // CreateNamedPipe and ConnectNamedPipe; that's a success.
                let raw = err.raw_os_error().unwrap_or(0);
                if raw == 535 {
                    return Ok(());
                }
                if start.elapsed() >= timeout {
                    return Err(err);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Read for PipeHandle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            use windows::Win32::Storage::FileSystem::ReadFile;
            let mut bytes_read: u32 = 0;
            // SAFETY: buf is a valid slice; handle is open for reading.
            let r = unsafe { ReadFile(self.handle, Some(buf), Some(&mut bytes_read), None) };
            if r.is_err() {
                return Err(std::io::Error::last_os_error());
            }
            Ok(bytes_read as usize)
        }
    }

    impl Drop for PipeHandle {
        fn drop(&mut self) {
            // Disconnect first so a stuck client doesn't keep the pipe
            // alive after we've closed the handle.
            use windows::Win32::Foundation::CloseHandle;
            use windows::Win32::System::Pipes::DisconnectNamedPipe;
            unsafe {
                let _ = DisconnectNamedPipe(self.handle);
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(not(windows))]
mod pipe {
    use super::*;

    /// Non-Windows placeholder. The file-tail is the only source of
    /// state on Linux/macOS — those platforms aren't supported in alpha
    /// but we keep the build clean.
    pub(super) fn run_pipe_server(
        _state: StateHandle,
        _app: AppHandle,
        _handle: tokio::runtime::Handle,
    ) {
        tracing::debug!(
            layer = "dashboard",
            component = "runtime_bridge",
            "named-pipe transport not available on this platform; using file-tail only"
        );
    }
}
