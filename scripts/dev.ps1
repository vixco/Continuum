# Continuum local dev - one command to run the dashboard locally.
# No CI, no push, no release artifacts. Pure local loop.
#
# Usage:
#   ./scripts/dev.ps1               # Tauri desktop app (default) - the real frameless UI
#   ./scripts/dev.ps1 -FrontendOnly  # Next.js only on http://localhost:3000 (no Rust/Tauri)
#   ./scripts/dev.ps1 -WithRuntime   # also start continuum.exe (release) for live data
#   ./scripts/dev.ps1 -Check         # just verify prerequisites, don't run anything
#
# The default opens the real frameless dashboard with working window controls.
# Live perception/triage/voice data only flows when the runtime (continuum.exe)
# is running - pass -WithRuntime for that, or click "Start runtime" in the titlebar.

[CmdletBinding()]
param(
  [switch]$FrontendOnly,
  [switch]$WithRuntime,
  [switch]$Check
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path $PSScriptRoot -Parent
$desktop = Join-Path $repoRoot "apps/desktop"

function Write-Step($msg) { Write-Host "`n== $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "  OK  $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  !   $msg" -ForegroundColor Yellow }
function Write-Err($msg) { Write-Host "  X   $msg" -ForegroundColor Red }

function Test-Preqs {
  param([switch]$NeedRust)
  $ok = $true
  if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    Write-Err "pnpm not on PATH - install Node 20+ then 'npm i -g pnpm'"
    $ok = $false
  } else {
    Write-Ok "pnpm found"
  }
  if ($NeedRust) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
      Write-Err "cargo not on PATH - install Rust stable from https://rustup.rs"
      $ok = $false
    } else {
      Write-Ok "cargo found"
    }
  }
  return $ok
}

if ($Check) {
  Write-Step "Checking prerequisites"
  $null = Test-Preqs
  $null = Test-Preqs -NeedRust
  Write-Host "`nRun without flags to launch the Tauri dashboard." -ForegroundColor DarkGray
  return
}

# --- Frontend-only: Next.js on :3000, no Rust/Tauri ---------------------
if ($FrontendOnly) {
  Write-Step "Frontend-only mode -> http://localhost:3000"
  if (-not (Test-Preqs)) { exit 1 }
  Push-Location $desktop
  try { pnpm dev } finally { Pop-Location }
  return
}

# --- Default + -WithRuntime: Tauri desktop app --------------------------
Write-Step "Tauri desktop app (frameless dashboard)"
if (-not (Test-Preqs -NeedRust)) { exit 1 }

if (-not (Test-Path (Join-Path $desktop "node_modules"))) {
  Write-Step "Installing desktop dependencies (first run only)"
  Push-Location $desktop
  try { pnpm install } finally { Pop-Location }
}

$runtimeProc = $null
if ($WithRuntime) {
  Write-Step "Starting Continuum runtime (continuum.exe, release)"
  $bin = Join-Path $repoRoot "target/release/continuum.exe"
  if (-not (Test-Path $bin)) {
    Write-Warn "Runtime binary not built - building (release; ~9 min first time)..."
    Push-Location $repoRoot
    try { cargo build --release --bin continuum } finally { Pop-Location }
    $bin = Join-Path $repoRoot "target/release/continuum.exe"
  }
  if (Test-Path $bin) {
    $runtimeProc = Start-Process -FilePath $bin -PassThru -WindowStyle Normal
    Write-Ok "Runtime started (PID $($runtimeProc.Id))"
  } else {
    Write-Err "Runtime build failed - continuing without it (dashboard still runs)."
  }
}

try {
  Write-Step "Launching Tauri dev (compiles the Rust backend, then opens the window)"
  Write-Host "  Ctrl+C to stop." -ForegroundColor DarkGray
  Push-Location $desktop
  try { pnpm tauri dev } finally { Pop-Location }
}
finally {
  if ($runtimeProc -and -not $runtimeProc.HasExited) {
    Write-Step "Stopping runtime"
    Stop-Process -Id $runtimeProc.Id -Force -ErrorAction SilentlyContinue
  }
}