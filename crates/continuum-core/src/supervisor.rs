//! # Runtime component supervisor
//!
//! The Continuum runtime (`crates/continuum-core/src/bin/continuum.rs`) runs
//! its senses, triage, and supporting loops as independent tokio tasks within
//! a single process. Historically each task was `tokio::spawn`ed and its
//! `JoinHandle` discarded: if the task panicked, hit a fatal error, or
//! returned early, it died **silently** and nothing brought it back. The
//! `should_restart` flags the context-engine snapshot publishes into
//! `state.json` were inert — they surfaced in the Health tab but no code path
//! acted on them.
//!
//! The supervisor closes that gap. It owns the lifecycle of supervised
//! component tasks:
//!
//! 1. **Auto-respawn.** A watch loop reaps finished `JoinHandle`s every few
//!    seconds. A task that exited (panic, error, or unexpected clean return)
//!    is respawned via its registered `restarter` closure, up to a backstop
//!    so a hard-failing component cannot restart-loop forever.
//! 2. **Repair-intent consumption.** The `mcp__continuum__repair_restart_component`
//!    MCP tool writes a `restart` intent JSON file under
//!    `~/.continuum-dev/repair-intents/`. The supervisor drains that queue:
//!    on a `restart` intent for a supervised component it aborts the live task
//!    and respawns it, then moves the intent file to `processed/` so the run
//!    is idempotent. This turns the previously-dead repair wire into a live
//!    diagnose→intent→restart loop.
//!
//! The supervisor is **not** a cognitive layer and never feeds perception
//! upward. It only manages downward-facing task lifecycles. Data still flows
//! up through the channels the supervised tasks write into; the supervisor
//! never inspects that data.
//!
//! Restarters are `Fn() -> JoinHandle<()>` closures supplied at registration
//! time. They capture clones of the shared state each task needs (privacy
//! filter, toggle/cadence controls, channel senders, config snapshots) so a
//! respawn faithfully reconstructs the task without re-running one-shot boot
//! logic. The restart count is exposed for the health surface.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often the watch loop reaps finished tasks and drains the intent queue.
const WATCH_TICK_SECS: u64 = 5;

/// Components the supervisor can actually restart on a `repair_restart_component`
/// intent, identified by the same snake_case keys `continuum-mcp`
/// `RepairTarget::as_str()` emits. The repair session grant copies this list
/// into `allowed_restart_components` so an authorised repair agent can queue a
/// restart that the supervisor then consumes. Components not in this list
/// (e.g. `triage`, `orchestrator`) are not yet supervisor-managed and remain
/// denied — fail-closed rather than promising a restart nothing acts on.
pub const SUPERVISED_REPAIR_TARGETS: &[&str] = &["vision", "audio", "context_watcher"];

/// Hard backstop: a component that has restarted this many times without
/// stabilising is left dead and surfaced as unhealthy, so a broken component
/// cannot spin the CPU forever. Large enough that transient panics recover,
/// small enough to fail visibly.
const MAX_RESTARTS_PER_HOUR: u64 = 12;

/// A closure that (re)spawns a supervised task and returns its `JoinHandle`.
/// Must be callable many times; captured shared state is re-cloned per call.
type Restarter = Box<dyn Fn() -> JoinHandle<()> + Send + Sync + 'static>;

struct SupervisedTask {
    name: &'static str,
    handle: Option<JoinHandle<()>>,
    restarter: Restarter,
    restarts: u64,
    /// Mirrored `RepairTarget::as_str()` for components the repair agent can
    /// target (e.g. `"vision"`, `"audio"`, `"context_watcher"`). `None` for
    /// components with no repair target (git/file/process) — they still
    /// auto-respawn on death but cannot be restart-intent driven.
    repair_key: Option<&'static str>,
    first_spawn_at: chrono::DateTime<chrono::Utc>,
}

/// Public, serialisable snapshot of one supervised task's state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SupervisorStat {
    pub name: &'static str,
    pub alive: bool,
    pub restarts: u64,
    pub repair_key: Option<&'static str>,
}

#[derive(Clone)]
pub struct Supervisor {
    tasks: Arc<Mutex<Vec<SupervisedTask>>>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a supervised task. The `restarter` is invoked immediately to
    /// spawn the first instance. `repair_key`, when `Some`, must match the
    /// `component` string a `repair_restart_component` intent carries (see
    /// `continuum-mcp` `RepairTarget::as_str`) so intents can target it.
    pub fn register(
        &self,
        name: &'static str,
        repair_key: Option<&'static str>,
        restarter: Restarter,
    ) {
        let handle = restarter();
        tracing::info!(
            layer = "system",
            component = "supervisor",
            task = name,
            "supervised task spawned"
        );
        self.tasks.lock().push(SupervisedTask {
            name,
            handle: Some(handle),
            restarter,
            restarts: 0,
            repair_key,
            first_spawn_at: chrono::Utc::now(),
        });
    }

    /// Snapshot of every supervised task — for the health surface / state.json.
    pub fn stats(&self) -> Vec<SupervisorStat> {
        self.tasks
            .lock()
            .iter()
            .map(|t| SupervisorStat {
                name: t.name,
                alive: t.handle.as_ref().is_some_and(|h| !h.is_finished()),
                restarts: t.restarts,
                repair_key: t.repair_key,
            })
            .collect()
    }

    /// Restart a named task immediately. Aborts the live handle (if any) and
    /// respawns. Returns `true` if a task with that name (or `repair_key`)
    /// was found and restarted.
    pub fn restart_named(&self, key: &str) -> bool {
        {
            let mut tasks = self.tasks.lock();
            let Some(task) = tasks
                .iter_mut()
                .find(|t| t.name == key || t.repair_key == Some(key))
            else {
                return false;
            };
            if let Some(h) = task.handle.take() {
                h.abort();
            }
        }
        self.respawn_at(usize::MAX, key, true)
    }

    /// Respawn the task at `idx` (or located by `key` when `idx == usize::MAX`).
    /// Honours the per-hour restart backstop: a task over the cap is left dead
    /// and logged. Returns `true` if a new handle was installed.
    fn respawn_at(&self, idx: usize, key: &str, explicit: bool) -> bool {
        let mut tasks = self.tasks.lock();
        let Some(pos) = (if idx == usize::MAX {
            tasks
                .iter()
                .position(|t| t.name == key || t.repair_key == Some(key))
        } else {
            Some(idx)
        }) else {
            return false;
        };
        let task = &mut tasks[pos];
        // Per-hour backstop: if the task has been alive over an hour since its
        // first spawn, reset the restart counter so transient later failures
        // still recover.
        let now = chrono::Utc::now();
        if now - task.first_spawn_at > chrono::Duration::hours(1) {
            task.restarts = 0;
            task.first_spawn_at = now;
        }
        if !explicit && task.restarts >= MAX_RESTARTS_PER_HOUR {
            if task.handle.as_ref().is_some_and(|h| !h.is_finished()) {
                return true;
            }
            tracing::error!(
                layer = "system",
                component = "supervisor",
                task = task.name,
                restarts = task.restarts,
                "task exceeded restart backstop; leaving dead (surface as unhealthy)"
            );
            return false;
        }
        let new_handle = (task.restarter)();
        task.restarts += 1;
        task.handle = Some(new_handle);
        tracing::warn!(
            layer = "system",
            component = "supervisor",
            task = task.name,
            restarts = task.restarts,
            explicit,
            "task restarted"
        );
        true
    }

    /// Reap any finished handles and respawn them. Called once per watch tick.
    ///
    /// Async so we can `.await` each finished `JoinHandle` to surface its
    /// panic/JoinError payload in the log before respawning — no
    /// `block_in_place` (that would need the multi-threaded runtime).
    async fn reap_and_respawn(&self) {
        // Collect indices + handles of finished tasks under a brief lock, then
        // await each handle outside the lock to surface panic payloads.
        let finished: Vec<(usize, Option<JoinHandle<()>>)> = {
            let mut tasks = self.tasks.lock();
            tasks
                .iter_mut()
                .enumerate()
                .filter_map(|(i, t)| {
                    if t.handle.as_ref().is_some_and(|h| h.is_finished()) {
                        Some((i, t.handle.take()))
                    } else {
                        None
                    }
                })
                .collect()
        };
        for (idx, handle) in finished {
            if let Some(h) = handle {
                // Join to surface the panic/JoinError in the log; the result is
                // informational only.
                match h.await {
                    Ok(()) => tracing::warn!(
                        layer = "system",
                        component = "supervisor",
                        idx,
                        "supervised task returned cleanly (unexpected); respawning"
                    ),
                    Err(e) => tracing::error!(
                        layer = "system",
                        component = "supervisor",
                        idx,
                        error = %e,
                        "supervised task ended with error; respawning"
                    ),
                }
            }
            let name = self.tasks.lock().get(idx).map(|t| t.name).unwrap_or("?");
            self.respawn_at(idx, name, false);
        }
    }

    /// Drain `~/.continuum-dev/repair-intents/`: parse each new intent file,
    /// act on `restart` intents whose `component` matches a supervised task,
    /// then move the file to `processed/`. Already-seen filenames are skipped.
    fn consume_intents(&self, dev_dir: &Path, seen: &mut HashSet<String>) {
        let intents_dir = dev_dir.join("repair-intents");
        let processed_dir = intents_dir.join("processed");
        let entries = match std::fs::read_dir(&intents_dir) {
            Ok(e) => e,
            Err(_) => return, // no intents dir yet — healthy, nothing to do
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !seen.insert(fname.to_string()) {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        layer = "system",
                        component = "supervisor",
                        path = %path.display(),
                        error = %e,
                        "could not read repair intent; will retry"
                    );
                    seen.remove(fname);
                    continue;
                }
            };
            let parsed: Result<RepairIntent, _> = serde_json::from_str(&text);
            match parsed {
                Ok(intent) if intent.kind == "restart" => {
                    let component = intent.body.component.as_deref().unwrap_or("");
                    tracing::info!(
                        layer = "system",
                        component = "supervisor",
                        intent = %path.display(),
                        target = component,
                        "consuming restart intent"
                    );
                    let acted = self.restart_named(component);
                    tracing::info!(
                        layer = "system",
                        component = "supervisor",
                        target = component,
                        acted,
                        "restart intent handled"
                    );
                }
                Ok(intent) => {
                    tracing::info!(
                        layer = "system",
                        component = "supervisor",
                        kind = %intent.kind,
                        "non-restart intent observed (no consumer yet); archiving"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        layer = "system",
                        component = "supervisor",
                        path = %path.display(),
                        error = %e,
                        "malformed repair intent; archiving"
                    );
                }
            }
            // Archive the intent regardless of outcome so we don't re-process.
            let _ = std::fs::create_dir_all(&processed_dir);
            let dest = processed_dir.join(fname);
            if std::fs::rename(&path, &dest).is_err() {
                let _ = std::fs::copy(&path, &dest);
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    /// Run the watch loop until `shutdown` flips to true. Owns the intent
    /// queue drain. Spawns nothing itself beyond what restarters create.
    pub async fn run(self, dev_dir: PathBuf, mut shutdown: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(WATCH_TICK_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen: HashSet<String> = HashSet::new();
        // Drain once at boot so a pending intent from a crashed prior run is
        // honoured immediately rather than after the first tick.
        self.consume_intents(&dev_dir, &mut seen);
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!(
                            layer = "system",
                            component = "supervisor",
                            "shutdown requested; supervisor exiting"
                        );
                        break;
                    }
                }
                _ = tick.tick() => {
                    self.reap_and_respawn().await;
                    self.consume_intents(&dev_dir, &mut seen);
                }
            }
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// On-disk repair intent written by `continuum-mcp` `queue_intent`.
#[derive(Debug, Deserialize)]
struct RepairIntent {
    kind: String,
    /// Timestamp the MCP tool wrote the intent. Part of the on-disk format;
    /// not consumed by the supervisor, but retained for audit/debugging.
    #[serde(default)]
    #[allow(dead_code)]
    queued_at: Option<String>,
    body: IntentBody,
}

#[derive(Debug, Deserialize)]
struct IntentBody {
    /// snake_case component key matching `RepairTarget::as_str()`.
    #[serde(default)]
    component: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    fn counting_restarter(counter: Arc<AtomicU32>) -> Restarter {
        Box::new(move || {
            let c = counter.clone();
            tokio::spawn(async move {
                // Touch the counter from within the task so the test can
                // observe that the restarter actually ran. The task then
                // exits immediately to simulate a component that dies.
                c.fetch_add(1, Ordering::SeqCst);
            })
        })
    }

    /// Let any just-spawned task get scheduled so its body runs before we
    /// assert on its side effects.
    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }

    fn long_restarter() -> Restarter {
        Box::new(move || {
            tokio::spawn(async {
                // Park "forever" until the runtime drops it.
                std::future::pending::<()>().await;
            })
        })
    }

    #[tokio::test]
    async fn restart_named_aborts_and_respawns() {
        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        sup.register("vision", Some("vision"), counting_restarter(counts.clone()));
        settle().await;
        // initial spawn ran once
        assert_eq!(counts.load(Ordering::SeqCst), 1);
        // give the spawned task a chance to exit
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(sup.restart_named("vision"));
        settle().await;
        // restart spawns a second time
        assert_eq!(counts.load(Ordering::SeqCst), 2);
        let stat = &sup.stats()[0];
        assert_eq!(stat.restarts, 1);
    }

    #[tokio::test]
    async fn restart_named_unknown_returns_false() {
        let sup = Supervisor::new();
        sup.register("vision", Some("vision"), long_restarter());
        assert!(!sup.restart_named("nope"));
    }

    #[tokio::test]
    async fn restart_key_matches_repair_key() {
        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        sup.register(
            "context_watcher",
            Some("context_watcher"),
            counting_restarter(counts.clone()),
        );
        settle().await;
        assert!(sup.restart_named("context_watcher"));
        settle().await;
        assert_eq!(counts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reap_and_respawn_revives_dead_task() {
        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        sup.register("audio", Some("audio"), counting_restarter(counts.clone()));
        settle().await;
        assert_eq!(counts.load(Ordering::SeqCst), 1);
        // let the first task exit
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        sup.reap_and_respawn().await;
        settle().await;
        assert_eq!(
            counts.load(Ordering::SeqCst),
            2,
            "dead task should have been respawned"
        );
    }

    #[tokio::test]
    async fn reap_does_not_touch_alive_task() {
        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        let c = counts.clone();
        sup.register(
            "git",
            None,
            Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async { std::future::pending::<()>().await })
            }),
        );
        sup.reap_and_respawn().await;
        assert_eq!(counts.load(Ordering::SeqCst), 1, "alive task not respawned");
    }

    #[tokio::test]
    async fn consume_intents_restarts_target_and_archives() {
        let tmp = TempDir::new().unwrap();
        let dev_dir = tmp.path();
        let intents = dev_dir.join("repair-intents");
        std::fs::create_dir_all(&intents).unwrap();
        let payload = serde_json::json!({
            "kind": "restart",
            "queued_at": "2026-08-10T00:00:00Z",
            "body": { "component": "vision" }
        });
        std::fs::write(
            intents.join("20260810T000000000-001.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();

        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        sup.register("vision", Some("vision"), counting_restarter(counts.clone()));
        settle().await;
        assert_eq!(counts.load(Ordering::SeqCst), 1);

        let mut seen = HashSet::new();
        sup.consume_intents(dev_dir, &mut seen);
        settle().await;

        assert_eq!(
            counts.load(Ordering::SeqCst),
            2,
            "intent should have restarted vision"
        );
        assert!(
            intents
                .join("processed/20260810T000000000-001.json")
                .exists(),
            "intent should be archived under processed/"
        );
        assert!(!intents.join("20260810T000000000-001.json").exists());
    }

    #[tokio::test]
    async fn consume_intents_ignores_non_restart_kind() {
        let tmp = TempDir::new().unwrap();
        let dev_dir = tmp.path();
        let intents = dev_dir.join("repair-intents");
        std::fs::create_dir_all(&intents).unwrap();
        std::fs::write(
            intents.join("escalate-001.json"),
            serde_json::json!({ "kind": "escalate", "body": { "message": "help" } }).to_string(),
        )
        .unwrap();

        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        sup.register("vision", Some("vision"), counting_restarter(counts.clone()));
        settle().await;
        let mut seen = HashSet::new();
        sup.consume_intents(dev_dir, &mut seen);
        settle().await;
        assert_eq!(
            counts.load(Ordering::SeqCst),
            1,
            "escalate intent must not restart"
        );
        assert!(intents.join("processed/escalate-001.json").exists());
    }

    #[tokio::test]
    async fn consume_intents_skips_already_seen() {
        let tmp = TempDir::new().unwrap();
        let dev_dir = tmp.path();
        let intents = dev_dir.join("repair-intents");
        std::fs::create_dir_all(&intents).unwrap();
        // Simulate a file that's already been seen but not archived (edge case):
        // the second call should not re-read it.
        std::fs::write(
            intents.join("x.json"),
            serde_json::json!({ "kind": "restart", "body": { "component": "vision" } }).to_string(),
        )
        .unwrap();
        let sup = Supervisor::new();
        let counts = Arc::new(AtomicU32::new(0));
        sup.register("vision", Some("vision"), counting_restarter(counts.clone()));
        settle().await;
        let mut seen = HashSet::new();
        sup.consume_intents(dev_dir, &mut seen);
        settle().await;
        let after_first = counts.load(Ordering::SeqCst);
        sup.consume_intents(dev_dir, &mut seen);
        assert_eq!(
            counts.load(Ordering::SeqCst),
            after_first,
            "seen file not re-processed"
        );
    }

    #[test]
    fn repair_intent_parses() {
        let s = r#"{"kind":"restart","queued_at":"2026-08-10T00:00:00Z","body":{"component":"context_watcher"}}"#;
        let i: RepairIntent = serde_json::from_str(s).unwrap();
        assert_eq!(i.kind, "restart");
        assert_eq!(i.body.component.as_deref(), Some("context_watcher"));
    }
}
