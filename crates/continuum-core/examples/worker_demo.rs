//! # worker_demo — Phase 8 end-to-end smoke test
//!
//! Spins up a [`WorkerPool`], queues one worker with a tiny task, and waits
//! for the result. Runs in dry-run mode by default so no real claude CLI is
//! spawned — set `CONTINUUM_WORKER_DRY_RUN=0` to exercise the real subprocess.
//!
//! ```bash
//! cargo run --release --example worker_demo -p continuum-core
//! ```

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use continuum_core::skills::SkillLoader;
use continuum_core::workers::{WorkerPool, WorkerPoolOptions, WorkerSpec};
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    // Default to dry-run so `cargo run --example worker_demo` doesn't hit the
    // Anthropic API. Override with `CONTINUUM_WORKER_DRY_RUN=0` for a live run.
    if std::env::var_os("CONTINUUM_WORKER_DRY_RUN").is_none() {
        std::env::set_var("CONTINUUM_WORKER_DRY_RUN", "1");
    }

    let tmp = tempfile::tempdir()?;
    let data_dir = tmp.path().to_path_buf();
    println!("using data dir: {}", data_dir.display());

    // Optional skill loader — loads from <repo>/skills if present so matched
    // skills get injected into the worker's prompt.
    let skill_root = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("skills");
    let skill_loader = SkillLoader::new(skill_root);
    skill_loader.reload().ok();

    let opts = WorkerPoolOptions {
        data_dir: data_dir.clone(),
        skill_loader: Some(skill_loader.clone()),
        ..WorkerPoolOptions::new(data_dir.clone())
    };
    let pool = WorkerPool::new(opts);
    let (tx, rx) = watch::channel(false);
    pool.spawn_background(rx);

    println!("\n---- Spawning worker ----");
    let id = pool
        .submit(WorkerSpec::new(
            "List the files in the current directory and summarise what this project is about",
            std::env::current_dir()?,
        ))
        .await?;
    println!("worker_id = {id}");

    println!("\n---- Waiting for completion ----");
    let snap = pool
        .wait(&id, Some(Duration::from_secs(30)))
        .await
        .ok_or_else(|| anyhow::anyhow!("worker disappeared"))?;

    println!("\n---- Result ----");
    println!("status:   {}", snap.status.as_str());
    println!("model:    {}", snap.model);
    println!("cost:     {:?}", snap.cost_usd);
    println!("duration: {} ms", snap.elapsed_ms);
    println!("skills:   {:?}", snap.skills);
    println!("tools:    {}", snap.tool_calls);
    if let Some(r) = snap.result.as_deref() {
        println!("\n{r}");
    }
    if let Some(e) = snap.error.as_deref() {
        println!("\n[error] {e}");
    }

    // Tidy up the background task.
    let _ = tx.send(true);
    Ok(())
}
