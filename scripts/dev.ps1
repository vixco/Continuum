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
. (Join-Path $PSScriptRoot "lib\onnx-runtime.ps1")

function Write-Step($msg) { Write-Host "`n== $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "  OK  $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "  !   $msg" -ForegroundColor Yellow }
function Write-Err($msg) { Write-Host "  X   $msg" -ForegroundColor Red }

# whisper-rs-sys runs bindgen while Tauri compiles the Rust backend. On a
# regular PowerShell session (rather than a Visual Studio Developer Prompt),
# clang cannot locate the Windows C headers such as stdbool.h unless we import
# the MSVC environment first. Keep this process-local: it must not mutate the
# maintainer's global shell configuration.
function Initialize-NativeToolchain {
  if ($env:OS -eq "Windows_NT" -and [string]::IsNullOrWhiteSpace($env:INCLUDE)) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
      throw "Visual Studio Build Tools were not found. Run scripts/dev-setup.ps1 first."
    }
    $installationPath = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if ([string]::IsNullOrWhiteSpace($installationPath)) {
      throw "Visual Studio C++ Build Tools were not found. Run scripts/dev-setup.ps1 first."
    }
    $devCommand = Join-Path $installationPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $devCommand -PathType Leaf)) {
      throw "VsDevCmd.bat was not found under $installationPath."
    }

    $environmentLines = & cmd.exe /d /s /c "`"$devCommand`" -no_logo -arch=x64 -host_arch=x64 >nul && set"
    if ($LASTEXITCODE -ne 0) {
      throw "Failed to initialize the Visual Studio C++ environment."
    }
    foreach ($line in $environmentLines) {
      $separator = $line.IndexOf('=')
      if ($separator -gt 0) {
        [Environment]::SetEnvironmentVariable($line.Substring(0, $separator), $line.Substring($separator + 1), "Process")
      }
    }
  }

  if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
    foreach ($candidate in @("C:\LLVM\bin", "C:\Program Files\LLVM\bin")) {
      if (Test-Path -LiteralPath (Join-Path $candidate "libclang.dll") -PathType Leaf) {
        $env:LIBCLANG_PATH = $candidate
        break
      }
    }
  }
  if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH) -or -not (Test-Path -LiteralPath (Join-Path $env:LIBCLANG_PATH "libclang.dll") -PathType Leaf)) {
    throw "LLVM/libclang is not configured. Install LLVM or set LIBCLANG_PATH, then rerun scripts/dev-setup.ps1."
  }

  Write-Ok "Native toolchain ready (MSVC + LLVM)"
}

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
  $ok = Test-Preqs -NeedRust
  try {
    $onnxRuntime = Resolve-ContinuumOnnxRuntime -RepoRoot $repoRoot
    Write-Ok "ONNX Runtime $($onnxRuntime.Version) found at $($onnxRuntime.Path)"
  } catch {
    Write-Err $_.Exception.Message
    $ok = $false
  }
  Write-Host "`nRun without flags to launch the Tauri dashboard." -ForegroundColor DarkGray
  if (-not $ok) { exit 1 }
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
try {
  Initialize-NativeToolchain
} catch {
  Write-Err $_.Exception.Message
  exit 1
}

# ort uses dynamic loading. Without an explicit path, Windows can silently pick
# its old System32 copy (currently 1.17 on some Windows builds), while ort rc.11
# requires 1.23 or newer. Resolve and validate before Tauri starts so both the
# dashboard and the runtime process spawned by its button inherit the safe DLL.
try {
  $onnxRuntime = Resolve-ContinuumOnnxRuntime -RepoRoot $repoRoot
  $env:ORT_DYLIB_PATH = $onnxRuntime.Path
  Write-Ok "ONNX Runtime $($onnxRuntime.Version) -> $($onnxRuntime.Path)"
} catch {
  Write-Err $_.Exception.Message
  exit 1
}

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
