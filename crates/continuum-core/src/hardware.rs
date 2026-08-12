//! # Hardware detection and adaptive resource policy
//!
//! Continuum runs several local models continuously — a triage LLM
//! (Qwen3-8B), Whisper STT, and an ONNX vision model — plus screen/context
//! pollers and a worker pool. On a laptop these can eat the whole machine if
//! each picks "all cores" / "full GPU" naively. This module probes the host
//! once at boot and resolves a [`ResolvedResourcePlan`] that tunes every
//! resource-affecting knob to the detected specs.
//!
//! This is **not** a cognitive layer. It sits *outside* the four-layer
//! architecture (Senses → Triage → Orchestrator → Workers): it never feeds
//! perception frames upward and never makes triage decisions. It only tunes
//! downward-facing knobs (threads, GPU offload, poll intervals, worker
//! concurrency) that the runtime applies before components spawn. Data still
//! flows up, commands still flow down — the layer hierarchy is untouched.
//!
//! Default policy (`BarelyNotice`): barely-noticeable CPU/RAM footprint, but
//! GPU/VRAM used freely for quality. Every value is overridable via
//! `[resources]` in `config.toml` / the dashboard (non-negotiable #3).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{ProfileMode, ResourceConfig};

/// Detected host capabilities. Probed once at boot by [`probe_hardware`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareSpecs {
    /// Physical CPU cores (best effort; falls back to logical/2 if unknown).
    pub physical_cores: u32,
    /// Logical CPU cores (SMT threads).
    pub logical_cores: u32,
    /// Total system RAM in megabytes.
    pub total_ram_mb: u32,
    /// CPU brand string (for display in the dashboard / repair context).
    pub cpu_brand: String,
    /// True when an NVIDIA CUDA runtime DLL is present on the system.
    pub has_cuda: bool,
    /// Detected VRAM in megabytes, when queryable. `None` when CUDA is present
    /// but VRAM could not be queried (treated as "assume enough").
    pub vram_mb: Option<u32>,
    /// True when the machine is currently running on battery power.
    pub on_battery: bool,
    /// True when the machine has a battery (i.e. is a laptop).
    pub is_laptop: bool,
}

/// Concrete resource knobs, resolved from [`HardwareSpecs`] + [`ResourceConfig`]
/// by [`resolve_resource_policy`]. Applied once at boot; serialised into the
/// runtime snapshot so the dashboard can display it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedResourcePlan {
    /// Threads for the triage LLM (llama.cpp `n_threads`).
    pub triage_threads: u32,
    /// llama.cpp GPU layer offload (`n_gpu_layers`); 999 = full offload, 0 = CPU.
    pub triage_gpu_layers: u32,
    /// Whether the vision model should be loaded at all.
    pub vision_enabled: bool,
    /// Whether the vision model should request an available GPU backend.
    pub vision_gpu: bool,
    /// Threads for Whisper STT.
    pub whisper_threads: u32,
    /// Max concurrent worker sessions (clamped to 1-10).
    pub workers_max_concurrent: u32,
    /// Screen-capture interval in seconds.
    pub screen_interval_secs: u64,
    /// Context-poller interval in seconds.
    pub context_interval_secs: u64,
}

/// A preset baseline policy. [`ProfileMode::Custom`] builds one from the
/// user's [`ResourceConfig`] fields; the other profiles use the constants below.
struct Preset {
    cpu_frac: f32,
    battery_frac: f32,
    workers: u32,
    screen_ac: u64,
    screen_batt: u64,
    ctx_ac: u64,
    ctx_batt: u64,
}

const BARELY_NOTICE: Preset = Preset {
    cpu_frac: 0.30,
    battery_frac: 0.20,
    workers: 2,
    screen_ac: 3,
    screen_batt: 5,
    ctx_ac: 1,
    ctx_batt: 2,
};

const BALANCED: Preset = Preset {
    cpu_frac: 0.55,
    battery_frac: 0.30,
    workers: 4,
    screen_ac: 2,
    screen_batt: 4,
    ctx_ac: 1,
    ctx_batt: 2,
};

const PERFORMANCE: Preset = Preset {
    cpu_frac: 0.80,
    battery_frac: 0.80,
    workers: 6,
    screen_ac: 1,
    screen_batt: 2,
    ctx_ac: 1,
    ctx_batt: 1,
};

/// Build a [`Preset`] from the user's [`ResourceConfig`] fields (used for
/// [`ProfileMode::Custom`] and as the source of overrides that survive across
/// profiles — e.g. `workers_max_concurrent` always overrides).
fn custom_preset(cfg: &ResourceConfig) -> Preset {
    Preset {
        cpu_frac: cfg.cpu_core_fraction,
        battery_frac: cfg.battery_core_fraction,
        workers: cfg.workers_max_concurrent.unwrap_or(2),
        screen_ac: cfg.screen_interval_secs.unwrap_or(3),
        screen_batt: cfg.screen_interval_secs.unwrap_or(5),
        ctx_ac: cfg.context_interval_secs.unwrap_or(1),
        ctx_batt: cfg.context_interval_secs.unwrap_or(2),
    }
}

/// Resolve the concrete resource plan from detected specs and user policy.
///
/// Pure: no I/O, no globals. The unit-test target. The caller applies the
/// returned knobs to the triage LLM, vision model, whisper, pollers, and
/// worker pool before spawning them.
pub fn resolve_resource_policy(
    specs: &HardwareSpecs,
    cfg: &ResourceConfig,
) -> ResolvedResourcePlan {
    // Auto picks a profile from the specs: laptops and GPU-less boxes stay
    // conservative; a desktop with a GPU gets Balanced.
    let profile = match cfg.profile {
        ProfileMode::Auto => {
            if specs.is_laptop || !specs.has_cuda {
                ProfileMode::BarelyNotice
            } else {
                ProfileMode::Balanced
            }
        }
        p => p,
    };

    let preset = match profile {
        ProfileMode::BarelyNotice => &BARELY_NOTICE,
        ProfileMode::Balanced => &BALANCED,
        ProfileMode::Performance => &PERFORMANCE,
        ProfileMode::Custom => return resolve_with_preset(specs, cfg, &custom_preset(cfg)),
        // Auto is resolved above; this branch is unreachable but the
        // exhaustiveness check wants it.
        ProfileMode::Auto => &BARELY_NOTICE,
    };
    resolve_with_preset(specs, cfg, preset)
}

fn resolve_with_preset(
    specs: &HardwareSpecs,
    cfg: &ResourceConfig,
    preset: &Preset,
) -> ResolvedResourcePlan {
    let on_battery = cfg.battery_throttle && specs.on_battery;

    // CPU threads: fraction of logical cores, floored at cpu_min_threads and
    // capped at cpu_max_threads so even a 32-core box leaves the OS headroom.
    let frac = if on_battery {
        preset.battery_frac
    } else {
        preset.cpu_frac
    };
    let raw = (specs.logical_cores as f32 * frac).round().max(1.0) as u32;
    let triage_threads = raw.clamp(cfg.cpu_min_threads, cfg.cpu_max_threads);

    // GPU: explicit override wins; otherwise auto-detect using the VRAM floor.
    let use_gpu = match cfg.gpu_enabled {
        Some(b) => b,
        None => specs.has_cuda && specs.vram_mb.is_none_or(|v| v >= cfg.gpu_min_vram_mb),
    };
    let triage_gpu_layers = if use_gpu { 999 } else { 0 };

    // Vision: load when RAM is sufficient (or the user forces it on). On a
    // very low-RAM box, perception falls back to text-only context.
    let vision_enabled = cfg
        .vision_enabled
        .unwrap_or(specs.total_ram_mb >= cfg.vision_min_ram_mb);

    // Workers: derive from the preset, halve on battery, cap by RAM (one
    // worker per 4 GB), floor 1. An explicit override replaces the derived
    // value entirely (clamped to the pool's 1-10 range).
    let workers = match cfg.workers_max_concurrent {
        Some(o) => o.clamp(1, 10),
        None => {
            let mut w = preset.workers;
            if on_battery {
                w = (w / 2).max(1);
            }
            let ram_cap = (specs.total_ram_mb / 1024 / 4).max(1);
            w.min(ram_cap).clamp(1, 10)
        }
    };

    // Poll intervals: an explicit override wins (applies on both AC and
    // battery); otherwise use the preset's AC/battery pair.
    let screen_interval = match cfg.screen_interval_secs {
        Some(s) => s.max(1),
        None => {
            if on_battery {
                preset.screen_batt
            } else {
                preset.screen_ac
            }
        }
    };
    let context_interval = match cfg.context_interval_secs {
        Some(s) => s.max(1),
        None => {
            if on_battery {
                preset.ctx_batt
            } else {
                preset.ctx_ac
            }
        }
    };

    ResolvedResourcePlan {
        triage_threads,
        triage_gpu_layers,
        vision_enabled,
        vision_gpu: use_gpu,
        whisper_threads: triage_threads,
        workers_max_concurrent: workers,
        screen_interval_secs: screen_interval,
        context_interval_secs: context_interval,
    }
}

/// Probe the host once. Cheap (a few sysinfo calls + one `nvidia-smi` round
/// trip, 2 s timeout). Logs the detected specs via `tracing`.
pub fn probe_hardware() -> HardwareSpecs {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();

    let logical_cores = sys.cpus().len().max(1) as u32;
    let physical_cores = match sys.physical_core_count() {
        Some(c) => c as u32,
        None => (logical_cores / 2).max(1),
    };
    let total_ram_mb = (sys.total_memory() / 1024 / 1024).max(1) as u32;
    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_default();

    let has_cuda = cuda_runtime_present();
    let vram_mb = if has_cuda { nvidia_smi_vram_mb() } else { None };

    let (on_battery, is_laptop) = power_status();

    let specs = HardwareSpecs {
        physical_cores,
        logical_cores,
        total_ram_mb,
        cpu_brand,
        has_cuda,
        vram_mb,
        on_battery,
        is_laptop,
    };
    tracing::info!(
        layer = "hardware",
        component = "continuum",
        specs = ?specs,
        "Detected host hardware"
    );
    specs
}

/// Query `nvidia-smi` for total VRAM in MB. Returns `None` when nvidia-smi is
/// absent or did not respond within 2 s — the caller treats that as "VRAM
/// unknown, assume enough" when CUDA is present.
fn nvidia_smi_vram_mb() -> Option<u32> {
    let mut child = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Bound the wait so a wedged nvidia-smi cannot stall boot: ~2 s max.
    for _ in 0..40 {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let out = child.wait_with_output().ok()?.stdout;
                let text = String::from_utf8_lossy(&out);
                return text
                    .lines()
                    .next()
                    .and_then(|l| l.trim().parse::<u32>().ok());
            }
            Ok(Some(_)) => return None, // exited non-success
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let _ = child.kill();
    None
}

#[cfg(windows)]
fn cuda_runtime_present() -> bool {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;

    // SAFETY: LoadLibraryW with a constant wide string is safe to call. We do
    // not FreeLibrary — the daemon is long-lived and nvcuda.dll is shared
    // with llama.cpp / ort anyway, so one extra reference is negligible.
    unsafe { LoadLibraryW(w!("nvcuda.dll")).is_ok() }
}

#[cfg(not(windows))]
fn cuda_runtime_present() -> bool {
    false
}

#[cfg(windows)]
fn power_status() -> (bool, bool) {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: GetSystemPowerStatus writes into our local struct; the pointer
    // is valid for the call.
    let ok = unsafe { GetSystemPowerStatus(&mut status) }.is_ok();
    if !ok {
        return (false, false);
    }
    // ACLineStatus: 0 = offline (battery), 1 = online (AC).
    let on_battery = status.ACLineStatus == 0;
    // BatteryFlag bit 7 (0x80) = "no system battery"; 0xFF = unknown.
    let no_battery = (status.BatteryFlag & 0x80) != 0 || status.BatteryFlag == 0xFF;
    let is_laptop = !no_battery;
    (on_battery, is_laptop)
}

#[cfg(not(windows))]
fn power_status() -> (bool, bool) {
    (false, false)
}

/// Outcome of classifying live system load. Used by the
/// `system_resources` health probe (desktop) to pick a `ComponentStatus`,
/// and by the repair agent to reason about model-load failures. Pure so it
/// can be unit-tested without a real CPU sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStatus {
    /// Load is within comfortable headroom.
    Ok,
    /// CPU sustained > 90 % across the last ~60 s, or RAM > 90 % — heavy, but
    /// recoverable by lowering `cpu_core_fraction` / worker concurrency.
    Degrading,
    /// RAM > 95 % — imminent OOM risk; triage/vision model loads will fail.
    Critical,
}

/// Classify a rolling window of CPU% samples plus the current RAM fraction.
///
/// `cpu_samples` is newest-at-back (any length; only the last two are
/// considered for the sustained-CPU rule, matching the ~60 s window at a
/// 30 s poll cadence). `ram_fraction` is `used / total` in `0.0..=1.0`.
pub fn classify_system_load(cpu_samples: &[f32], ram_fraction: f32) -> LoadStatus {
    if ram_fraction > 0.95 {
        return LoadStatus::Critical;
    }
    if ram_fraction > 0.90 {
        return LoadStatus::Degrading;
    }
    let last_two: Vec<f32> = cpu_samples.iter().rev().take(2).copied().collect();
    if last_two.len() == 2 && last_two.iter().all(|&v| v > 90.0) {
        return LoadStatus::Degrading;
    }
    LoadStatus::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(
        logical: u32,
        ram_mb: u32,
        has_cuda: bool,
        vram: Option<u32>,
        on_battery: bool,
        is_laptop: bool,
    ) -> HardwareSpecs {
        HardwareSpecs {
            physical_cores: (logical / 2).max(1),
            logical_cores: logical,
            total_ram_mb: ram_mb,
            cpu_brand: "Test CPU".into(),
            has_cuda,
            vram_mb: vram,
            on_battery,
            is_laptop,
        }
    }

    fn cfg_for(profile: ProfileMode) -> ResourceConfig {
        ResourceConfig {
            profile,
            ..ResourceConfig::default()
        }
    }

    #[test]
    fn desktop_with_gpu_full_offloads_and_keeps_headroom() {
        // 16 logical cores, 32 GB, 8 GB VRAM, AC, desktop.
        let s = specs(16, 32 * 1024, true, Some(8192), false, false);
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::BarelyNotice));
        // 30% of 16 = 4.8 → round 5, clamp [2,8] → 5
        assert_eq!(plan.triage_threads, 5);
        assert_eq!(plan.triage_gpu_layers, 999, "full GPU offload");
        assert!(plan.vision_gpu, "vision uses CUDA");
        assert!(plan.vision_enabled, "RAM well above floor");
        assert_eq!(plan.workers_max_concurrent, 2, "barely-notice preset");
        assert_eq!(plan.screen_interval_secs, 3);
        assert_eq!(plan.context_interval_secs, 1);
    }

    #[test]
    fn laptop_on_battery_no_gpu_throttles_hard() {
        // 4 logical cores, 8 GB, no GPU, battery, laptop.
        let s = specs(4, 8 * 1024, false, None, true, true);
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::BarelyNotice));
        // 20% of 4 = 0.8 → round 1, clamp [2,8] → floored at 2
        assert_eq!(plan.triage_threads, 2);
        assert_eq!(plan.triage_gpu_layers, 0, "no GPU → CPU");
        assert!(!plan.vision_gpu);
        assert!(plan.vision_enabled, "8 GB >= 6 GB floor");
        // battery halves workers 2→1, ram_cap = 8/4 = 2 → min(1,2) = 1
        assert_eq!(plan.workers_max_concurrent, 1);
        assert_eq!(plan.screen_interval_secs, 5, "battery screen interval");
        assert_eq!(plan.context_interval_secs, 2, "battery context interval");
    }

    #[test]
    fn very_low_ram_disables_vision() {
        // 4 cores, 4 GB — below the 6 GB vision floor.
        let s = specs(4, 4 * 1024, false, None, false, true);
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::BarelyNotice));
        assert!(!plan.vision_enabled, "RAM below vision floor → text-only");
    }

    #[test]
    fn gpu_override_forces_cpu_even_when_cuda_present() {
        let s = specs(16, 32 * 1024, true, Some(8192), false, false);
        let cfg = ResourceConfig {
            gpu_enabled: Some(false),
            ..cfg_for(ProfileMode::BarelyNotice)
        };
        let plan = resolve_resource_policy(&s, &cfg);
        assert_eq!(plan.triage_gpu_layers, 0);
        assert!(!plan.vision_gpu);
    }

    #[test]
    fn vram_below_floor_falls_back_to_cpu_when_auto() {
        let s = specs(8, 16 * 1024, true, Some(2048), false, false); // 2 GB VRAM
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::BarelyNotice));
        // has_cuda true but vram 2048 < 3000 → auto picks CPU
        assert_eq!(plan.triage_gpu_layers, 0);
        assert!(!plan.vision_gpu);
    }

    #[test]
    fn unknown_vram_with_cuda_allows_gpu() {
        let s = specs(8, 16 * 1024, true, None, false, false);
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::BarelyNotice));
        assert_eq!(plan.triage_gpu_layers, 999, "unknown VRAM assumes enough");
    }

    #[test]
    fn custom_profile_honours_explicit_fraction() {
        let s = specs(16, 32 * 1024, false, None, false, false);
        let cfg = ResourceConfig {
            profile: ProfileMode::Custom,
            cpu_core_fraction: 0.50,
            ..ResourceConfig::default()
        };
        let plan = resolve_resource_policy(&s, &cfg);
        // 50% of 16 = 8, clamp [2,8] → 8
        assert_eq!(plan.triage_threads, 8);
    }

    #[test]
    fn workers_override_replaces_derived_value_and_clamps_to_10() {
        let s = specs(16, 32 * 1024, true, Some(8192), false, false);
        let cfg = ResourceConfig {
            workers_max_concurrent: Some(20),
            ..cfg_for(ProfileMode::BarelyNotice)
        };
        let plan = resolve_resource_policy(&s, &cfg);
        assert_eq!(plan.workers_max_concurrent, 10, "clamped to pool max");
    }

    #[test]
    fn auto_profile_picks_balanced_for_gpu_desktop() {
        let s = specs(16, 32 * 1024, true, Some(8192), false, false);
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::Auto));
        // Balanced: 55% of 16 = 8.8 → 9, clamp [2,8] → 8
        assert_eq!(plan.triage_threads, 8);
        assert_eq!(plan.workers_max_concurrent, 4, "balanced preset workers");
        assert_eq!(plan.screen_interval_secs, 2);
    }

    #[test]
    fn auto_profile_falls_back_to_barely_notice_for_laptop() {
        let s = specs(8, 16 * 1024, false, None, true, true);
        let plan = resolve_resource_policy(&s, &cfg_for(ProfileMode::Auto));
        // battery barely-notice: 20% of 8 = 1.6 → 2, screen 5
        assert_eq!(plan.triage_threads, 2);
        assert_eq!(plan.screen_interval_secs, 5);
    }

    #[test]
    fn battery_throttle_disabled_uses_ac_values_on_battery() {
        let s = specs(8, 16 * 1024, false, None, true, true);
        let cfg = ResourceConfig {
            battery_throttle: false,
            ..cfg_for(ProfileMode::BarelyNotice)
        };
        let plan = resolve_resource_policy(&s, &cfg);
        assert_eq!(
            plan.screen_interval_secs, 3,
            "no battery throttle → AC interval"
        );
        assert_eq!(plan.workers_max_concurrent, 2, "no battery halving");
    }

    #[test]
    fn probe_hardware_runs_and_reports_cores() {
        let s = probe_hardware();
        assert!(s.logical_cores >= 1, "must detect at least one core");
        assert!(s.total_ram_mb >= 1, "must detect some RAM");
    }

    #[test]
    fn classify_load_ok_under_nominal() {
        assert_eq!(classify_system_load(&[40.0, 55.0], 0.50), LoadStatus::Ok);
        // A single high sample isn't "sustained" — needs two in a row.
        assert_eq!(classify_system_load(&[95.0], 0.40), LoadStatus::Ok);
    }

    #[test]
    fn classify_load_degrading_on_sustained_cpu() {
        assert_eq!(
            classify_system_load(&[92.0, 95.0], 0.40),
            LoadStatus::Degrading
        );
        // Most-recent two are what matter; an old spike doesn't count.
        assert_eq!(
            classify_system_load(&[95.0, 95.0, 30.0], 0.40),
            LoadStatus::Ok,
            "only the last two samples are considered"
        );
    }

    #[test]
    fn classify_load_degrading_on_high_ram() {
        assert_eq!(classify_system_load(&[20.0], 0.92), LoadStatus::Degrading);
    }

    #[test]
    fn classify_load_critical_on_ram_exhaustion() {
        assert_eq!(
            classify_system_load(&[20.0, 20.0], 0.97),
            LoadStatus::Critical,
            "RAM >95% is critical even with low CPU"
        );
        // RAM critical takes precedence over CPU degrading.
        assert_eq!(
            classify_system_load(&[99.0, 99.0], 0.99),
            LoadStatus::Critical
        );
    }
}
