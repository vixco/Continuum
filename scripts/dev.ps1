# Continuum local dev - one command to run the dashboard locally.
# No CI, no push, no release artifacts. Pure local loop.
#
# Usage:
#   ./scripts/dev.ps1               # Tauri desktop app (default) - the real frameless UI
#   ./scripts/dev.ps1 -FrontendOnly  # Next.js only, auto free port (no Rust/Tauri)
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

# Find the first free TCP port starting at $Start. Next.js and the Tauri
# devUrl are both pointed at this port, so a stale dev server (or any other
# app) on the default 3000 never breaks `dev.ps1` — it just rolls on to the
# next free port. We probe via Get-NetTCPConnection (the OS listener table)
# rather than opening a TcpListener on 127.0.0.1, because Next binds to `::`
# (IPv6, which also covers IPv4 on Windows) and an IPv4-only probe would miss
# it and hand back a port that is actually in use.
function Find-FreePort {
  param([int]$Start = 3000)
  for ($p = $Start; $p -lt ($Start + 200); $p++) {
    if (Get-NetTCPConnection -LocalPort $p -State Listen -ErrorAction SilentlyContinue) { continue }
    return $p
  }
  throw "No free dev port found in $Start..$($Start + 199). Free a port and retry."
}

function Test-Preqs {
  param([switch]$NeedRust)
  $ok = $true
  if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    # pnpm often lives behind corepack instead of on PATH. Try the local
    # corepack shim dir first, then materialize it via `corepack enable`.
    # Session-scoped PATH change only; nothing is installed globally.
    $shims = Join-Path $env:USERPROFILE ".local-tools\corepack-shims"
    if (-not (Test-Path (Join-Path $shims "pnpm.CMD"))) {
      if (Get-Command corepack -ErrorAction SilentlyContinue) {
        New-Item -ItemType Directory -Force $shims | Out-Null
        corepack enable --install-directory $shims 2>$null
      }
    }
    if (Test-Path (Join-Path $shims "pnpm.CMD")) {
      $env:Path = "$shims;$env:Path"
    }
  }
  if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    Write-Err "pnpm not on PATH - install Node 20+ then 'npm i -g pnpm' (or 'corepack enable')"
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

# --- Frontend-only: Next.js, no Rust/Tauri ------------------------------
if ($FrontendOnly) {
  $port = Find-FreePort
  $env:PORT = $port
  Write-Step "Frontend-only mode -> http://localhost:$port"
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
  # Pick a free port and point both Next.js (via $env:PORT) and Tauri's devUrl
  # (via $env:TAURI_CONFIG) at it, so a stale dev server or any other app on
  # 3000 can never wedge the dashboard — it just uses the next free port.
  $port = Find-FreePort
  $env:PORT = $port
  $env:TAURI_CONFIG = '{"build":{"devUrl":"http://localhost:' + $port + '"}}'
  Write-Step "Launching Tauri dev on http://localhost:$port (compiles the Rust backend, then opens the window)"
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