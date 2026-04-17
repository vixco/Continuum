# Kairo install.ps1
# Installer for Kairo — the AI that knows when to act.
#
# Usage:
#   # Install from GitHub release (default):
#   irm https://raw.githubusercontent.com/PrincNL/kairo-ai/main/scripts/install.ps1 | iex
#
#   # Or clone and run locally:
#   .\scripts\install.ps1
#   .\scripts\install.ps1 -FromSource          # build from source (requires Rust toolchain)
#   .\scripts\install.ps1 -SkipModels          # skip the model download step
#   .\scripts\install.ps1 -AutoStart           # also register Kairo to start with Windows
#   .\scripts\install.ps1 -DesktopShortcut     # also create a desktop shortcut
#   .\scripts\install.ps1 -Version v0.1.0-alpha.1   # pin a specific release tag
#
# The installer is idempotent — rerunning it upgrades / repairs without
# losing any config, memory, or models already on disk.

[CmdletBinding()]
param(
    [switch]$FromSource,
    [switch]$SkipModels,
    [switch]$AutoStart,
    [switch]$DesktopShortcut,
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\Kairo"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"  # Invoke-WebRequest is ~10x faster without the progress bar

# ---- Cosmetics --------------------------------------------------------------

function Write-Header($text) {
    Write-Host ""
    Write-Host "=== $text ===" -ForegroundColor Cyan
}

function Write-Ok($text)   { Write-Host "  [OK]   $text" -ForegroundColor Green }
function Write-Info($text) { Write-Host "  [..]   $text" -ForegroundColor Gray }
function Write-Warn($text) { Write-Host "  [WARN] $text" -ForegroundColor Yellow }
function Write-Err($text)  { Write-Host "  [FAIL] $text" -ForegroundColor Red }
function Write-Step($text) { Write-Host "`n-> $text" -ForegroundColor White }

Write-Host ""
Write-Host "       K" -NoNewline -ForegroundColor White
Write-Host "AI" -NoNewline -ForegroundColor Magenta
Write-Host "ro" -ForegroundColor White
Write-Host "       the AI that knows when to act" -ForegroundColor DarkGray
Write-Host ""

# ---- Step 1: Windows version check ------------------------------------------

Write-Header "Checking Windows version"

$winVersion = [System.Environment]::OSVersion.Version
$buildNumber = (Get-CimInstance Win32_OperatingSystem).BuildNumber
Write-Info "Windows $($winVersion.Major).$($winVersion.Minor) build $buildNumber"

if ($winVersion.Major -lt 10) {
    Write-Err "Kairo requires Windows 10 or 11. Detected: $($winVersion.Major).$($winVersion.Minor)"
    exit 1
}
if ($winVersion.Major -eq 10 -and [int]$buildNumber -lt 18362) {
    Write-Err "Kairo requires Windows 10 1903+ (build 18362) for Graphics Capture API. Detected build: $buildNumber"
    exit 1
}
Write-Ok "Windows version supported"

# ---- Step 2: Prerequisite checks --------------------------------------------

Write-Header "Checking prerequisites"

function Test-Command($cmd) {
    try { $null = & $cmd --version 2>&1; return $true } catch { return $false }
}

# Node.js
$nodeOk = $false
try {
    $nodeVer = node --version 2>&1
    if ($LASTEXITCODE -eq 0) {
        $major = [int]($nodeVer -replace '^v(\d+)\..*', '$1')
        if ($major -ge 18) {
            Write-Ok "Node.js $nodeVer"
            $nodeOk = $true
        } else {
            Write-Warn "Node.js $nodeVer found, but Kairo needs 18+. Install from https://nodejs.org"
        }
    }
} catch {
    Write-Warn "Node.js not found. Claude Code CLI needs it."
}
if (-not $nodeOk) {
    Write-Host "    -> Install Node.js 18 or newer from https://nodejs.org" -ForegroundColor Yellow
    Write-Host "    -> Rerun this installer after installing Node.js" -ForegroundColor Yellow
    exit 1
}

# Claude Code CLI
$claudeOk = $false
try {
    $claudeVer = claude --version 2>&1
    if ($LASTEXITCODE -eq 0) {
        Write-Ok "Claude Code CLI: $claudeVer"
        $claudeOk = $true
    }
} catch {
    # Fall through
}
if (-not $claudeOk) {
    Write-Warn "Claude Code CLI not found."
    Write-Host "    -> Kairo drives Claude Code as a subprocess, so this is required." -ForegroundColor Yellow
    $response = Read-Host "    -> Install now via 'npm install -g @anthropic-ai/claude-code'? [Y/n]"
    if ($response -ne "n" -and $response -ne "N") {
        Write-Info "Installing @anthropic-ai/claude-code globally..."
        npm install -g @anthropic-ai/claude-code
        if ($LASTEXITCODE -ne 0) {
            Write-Err "npm install failed. Install manually: npm install -g @anthropic-ai/claude-code"
            exit 1
        }
        Write-Ok "Claude Code CLI installed"
    } else {
        Write-Err "Claude Code CLI is required. Aborting."
        exit 1
    }
}

# Claude Code auth status
Write-Info "Checking Claude Code login status..."
$authOk = $false
$authOutput = & claude config get 2>&1 | Out-String
if ($LASTEXITCODE -eq 0 -and $authOutput -match "Anthropic|account|logged in|auth" -and $authOutput -notmatch "not logged in|no account") {
    # The `claude config get` surface is a bit chatty; rely on a real `claude -p` ping below if it ever becomes unclear.
    Write-Ok "Claude Code appears to be configured"
    $authOk = $true
} else {
    Write-Warn "Could not confirm Claude Code is logged in."
    Write-Host "    -> Run 'claude login' in a separate terminal and follow the prompts." -ForegroundColor Yellow
    $response = Read-Host "    -> Continue anyway? [y/N]"
    if ($response -ne "y" -and $response -ne "Y") {
        Write-Info "Aborting. Run 'claude login' and then rerun this installer."
        exit 1
    }
}

# Rust toolchain (only needed for source builds)
if ($FromSource) {
    Write-Info "Source build requested — checking build toolchain..."
    $rustOk = Test-Command "rustc"
    $cargoOk = Test-Command "cargo"
    $cmakeOk = Test-Command "cmake"
    if (-not $rustOk -or -not $cargoOk) {
        Write-Err "Rust toolchain missing. Install from https://rustup.rs and rerun."
        exit 1
    }
    Write-Ok "Rust $(rustc --version)"
    if (-not $cmakeOk) {
        Write-Err "CMake not on PATH. Source builds need CMake + Ninja + LLVM. See scripts/dev-setup.ps1 for the full list."
        exit 1
    }
    Write-Ok "CMake present"
    if (-not $env:LIBCLANG_PATH -or -not (Test-Path $env:LIBCLANG_PATH)) {
        Write-Warn "LIBCLANG_PATH not set. whisper-rs bindgen will likely fail."
        Write-Host "    -> Install LLVM and set LIBCLANG_PATH to <llvm>\bin (default: C:\LLVM\bin)" -ForegroundColor Yellow
    }
}

# ---- Step 3: Create ~/.kairo/ directory layout ------------------------------

Write-Header "Preparing ~/.kairo/ data directory"

$KairoData = Join-Path $env:USERPROFILE ".kairo"
$subdirs = @("config", "models", "models\vision", "models\triage", "models\stt", "models\tts",
             "logs", "memory", "backups", "bin", "worker-intents", "workers", "repair-intents")
foreach ($sd in $subdirs) {
    $full = Join-Path $KairoData $sd
    if (-not (Test-Path $full)) {
        New-Item -ItemType Directory -Force -Path $full | Out-Null
    }
}
Write-Ok "Created $KairoData"

# Copy default config files (only if they don't already exist — we never clobber user config)
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptRoot
$defaultConfigDir = Join-Path $repoRoot "config"
$userConfigDir = Join-Path $KairoData "config"

if (Test-Path $defaultConfigDir) {
    foreach ($cfg in Get-ChildItem $defaultConfigDir -File) {
        $dest = Join-Path $userConfigDir $cfg.Name
        if (-not (Test-Path $dest)) {
            Copy-Item $cfg.FullName $dest
            Write-Ok "Seeded $($cfg.Name)"
        } else {
            Write-Info "$($cfg.Name) already exists — keeping yours"
        }
    }
}

# ---- Step 4: Install Kairo binary -------------------------------------------

Write-Header "Installing Kairo binary"

if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
}

if ($FromSource) {
    Write-Step "Building from source (release, this may take ~10 minutes)..."
    Push-Location $repoRoot
    try {
        cargo build --release --bin kairo
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        cargo build --release --bin kairo-mcp
        if ($LASTEXITCODE -ne 0) { throw "cargo build kairo-mcp failed" }
        Copy-Item "target\release\kairo.exe" (Join-Path $InstallDir "kairo.exe") -Force
        Copy-Item "target\release\kairo-mcp.exe" (Join-Path $InstallDir "kairo-mcp.exe") -Force
        Write-Ok "Built and installed kairo.exe + kairo-mcp.exe"

        Write-Step "Building desktop app (cargo tauri build)..."
        Push-Location (Join-Path $repoRoot "apps\desktop")
        try {
            pnpm install --frozen-lockfile
            pnpm tauri build
            if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed" }
            $bundled = Get-ChildItem "src-tauri\target\release" -Filter "kairo-desktop.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($bundled) {
                Copy-Item $bundled.FullName (Join-Path $InstallDir "kairo-desktop.exe") -Force
                Write-Ok "Installed kairo-desktop.exe"
            }
        } finally {
            Pop-Location
        }
    } finally {
        Pop-Location
    }
} else {
    # Download from GitHub releases
    Write-Step "Downloading release binary..."
    $repo = "PrincNL/kairo-ai"
    try {
        if ($Version -eq "latest") {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers @{ "User-Agent" = "kairo-installer" }
        } else {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/tags/$Version" -Headers @{ "User-Agent" = "kairo-installer" }
        }
        Write-Ok "Found release $($release.tag_name)"

        $asset = $release.assets | Where-Object { $_.name -match 'kairo-.*-windows.*\.zip$' } | Select-Object -First 1
        if (-not $asset) {
            Write-Err "No Windows .zip asset in release $($release.tag_name)."
            Write-Host "    -> Fall back to: .\scripts\install.ps1 -FromSource" -ForegroundColor Yellow
            exit 1
        }
        $sumsAsset = $release.assets | Where-Object { $_.name -eq "SHA256SUMS.txt" } | Select-Object -First 1

        $tmpZip = Join-Path $env:TEMP "kairo-release.zip"
        Write-Info "Downloading $($asset.name) (~$([math]::Round($asset.size / 1MB)) MB)..."
        Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $tmpZip

        # Verify SHA256 if the release ships a SHA256SUMS.txt (all releases
        # from v0.1.0-alpha.2 onward). If the file is missing (older alpha)
        # we warn rather than abort so existing install flows don't break.
        if ($sumsAsset) {
            $tmpSums = Join-Path $env:TEMP "kairo-SHA256SUMS.txt"
            Invoke-WebRequest -Uri $sumsAsset.browser_download_url -OutFile $tmpSums
            $expected = $null
            foreach ($line in Get-Content $tmpSums) {
                $parts = $line -split '\s+', 2
                if ($parts.Count -eq 2 -and $parts[1].Trim() -eq $asset.name) {
                    $expected = $parts[0].ToLower()
                    break
                }
            }
            Remove-Item $tmpSums -Force
            if (-not $expected) {
                Write-Err "SHA256SUMS.txt did not list $($asset.name). Refusing to install an unverified binary."
                Remove-Item $tmpZip -Force
                exit 1
            }
            $actual = (Get-FileHash $tmpZip -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $expected) {
                Write-Err "Checksum mismatch! expected $expected, got $actual"
                Write-Err "Do NOT run the downloaded binary. Report via SECURITY.md."
                Remove-Item $tmpZip -Force
                exit 1
            }
            Write-Ok "SHA256 verified ($($expected.Substring(0,12))...)"
        } else {
            Write-Warn "Release is missing SHA256SUMS.txt — skipping integrity check."
        }

        Write-Info "Extracting to $InstallDir..."
        Expand-Archive -Path $tmpZip -DestinationPath $InstallDir -Force
        Remove-Item $tmpZip -Force
        Write-Ok "Kairo binary installed"
    } catch {
        Write-Err "Release download failed: $($_.Exception.Message)"
        Write-Host "    -> Fall back to building from source: .\scripts\install.ps1 -FromSource" -ForegroundColor Yellow
        exit 1
    }
}

# Add InstallDir to the user PATH if it isn't already there
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Ok "Added $InstallDir to user PATH (restart your shell to pick it up)"
} else {
    Write-Info "PATH already contains $InstallDir"
}

# ---- Step 5: Download default models ----------------------------------------

if (-not $SkipModels) {
    Write-Header "Downloading default models"
    $modelScript = Join-Path $repoRoot "scripts\download-models.ps1"
    if (Test-Path $modelScript) {
        # The shipped download script writes to ~/.kairo-dev/models/; redirect via env.
        $env:KAIRO_MODELS_DIR = Join-Path $KairoData "models"
        & $modelScript
        if ($LASTEXITCODE -ne 0) {
            Write-Warn "Some models failed to download. You can rerun 'kairo setup' later to retry."
        }
    } else {
        Write-Warn "scripts/download-models.ps1 not found. Run 'kairo setup' after install to fetch models."
    }
} else {
    Write-Info "Skipping model download (-SkipModels)."
}

# ---- Step 6: Register shortcuts ---------------------------------------------

Write-Header "Registering shortcuts"

$wshell = New-Object -ComObject WScript.Shell

# Start Menu shortcut
$startMenu = [Environment]::GetFolderPath("Programs")
$startLnk = Join-Path $startMenu "Kairo.lnk"
$targetExe = Join-Path $InstallDir "kairo-desktop.exe"
if (-not (Test-Path $targetExe)) {
    # Fall back to kairo.exe if the dashboard wasn't bundled in this install
    $targetExe = Join-Path $InstallDir "kairo.exe"
}
if (Test-Path $targetExe) {
    $s = $wshell.CreateShortcut($startLnk)
    $s.TargetPath = $targetExe
    $s.WorkingDirectory = $InstallDir
    $s.Description = "Kairo — the AI that knows when to act"
    $s.Save()
    Write-Ok "Start Menu shortcut created"
} else {
    Write-Warn "Kairo executable not found at $targetExe — skipping Start Menu shortcut"
}

# Desktop shortcut (optional)
if ($DesktopShortcut -and (Test-Path $targetExe)) {
    $desktop = [Environment]::GetFolderPath("Desktop")
    $dl = $wshell.CreateShortcut((Join-Path $desktop "Kairo.lnk"))
    $dl.TargetPath = $targetExe
    $dl.WorkingDirectory = $InstallDir
    $dl.Description = "Kairo — the AI that knows when to act"
    $dl.Save()
    Write-Ok "Desktop shortcut created"
}

# Auto-start (optional)
if ($AutoStart -and (Test-Path $targetExe)) {
    $runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
    New-ItemProperty -Path $runKey -Name "Kairo" -Value "`"$targetExe`"" -PropertyType String -Force | Out-Null
    Write-Ok "Registered to start with Windows"
}

# ---- Step 7: Mark install complete (not onboarding — that's the wizard) ----

$marker = Join-Path $userConfigDir "install-version"
$release_tag = if ($FromSource) { "source-$(git -C $repoRoot rev-parse --short HEAD 2>$null)" } else { $Version }
Set-Content -Path $marker -Value "kairo $release_tag installed $(Get-Date -Format o)"

# ---- Summary ----------------------------------------------------------------

Write-Host ""
Write-Host "=============================================" -ForegroundColor Green
Write-Host "  Kairo installed successfully." -ForegroundColor Green
Write-Host "=============================================" -ForegroundColor Green
Write-Host ""
Write-Host "  Install dir: $InstallDir"
Write-Host "  Data dir:    $KairoData"
Write-Host ""
Write-Host "  Next steps:" -ForegroundColor Cyan
Write-Host "    1. Launch Kairo from the Start Menu or run 'kairo' from a new shell."
Write-Host "    2. The dashboard will open and guide you through first-run setup."
Write-Host "    3. If anything needs a second look, run 'kairo setup' at any time."
Write-Host ""
Write-Host "  Docs: https://princnl.github.io/kairo-ai" -ForegroundColor Gray
Write-Host "  Issues: https://github.com/PrincNL/kairo-ai/issues" -ForegroundColor Gray
Write-Host ""

exit 0
