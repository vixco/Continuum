# Continuum install.ps1
# Idempotent Windows installer for the Continuum desktop, runtime and MCP servers.
#
# Usage:
#   irm https://raw.githubusercontent.com/vixco/Continuum/main/scripts/install.ps1 | iex
#   .\scripts\install.ps1 -FromSource
#   .\scripts\install.ps1 -SkipModels
#   .\scripts\install.ps1 -AutoStart -DesktopShortcut
#   .\scripts\install.ps1 -Version v0.1.0-alpha.12

[CmdletBinding()]
param(
    [switch]$FromSource,
    [switch]$SkipModels,
    [switch]$AutoStart,
    [switch]$DesktopShortcut,
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\Continuum"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repository = "vixco/Continuum"
$DefaultInstallDir = "$env:LOCALAPPDATA\Continuum"
$ContinuumData = Join-Path $env:USERPROFILE ".continuum"
$ContinuumDev = Join-Path $env:USERPROFILE ".continuum-dev"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptRoot

function Write-Header($text) { Write-Host "`n=== $text ===" -ForegroundColor Cyan }
function Write-Ok($text) { Write-Host "  [OK]   $text" -ForegroundColor Green }
function Write-Info($text) { Write-Host "  [..]   $text" -ForegroundColor Gray }
function Write-Warn($text) { Write-Host "  [WARN] $text" -ForegroundColor Yellow }
function Write-Err($text) { Write-Host "  [FAIL] $text" -ForegroundColor Red }
function Write-Step($text) { Write-Host "`n-> $text" -ForegroundColor White }

function Test-Command([string]$Command) {
    try {
        $null = & $Command --version 2>&1
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function Invoke-GitHubJson([string]$Uri) {
    $headers = @{
        "Accept" = "application/vnd.github+json"
        "User-Agent" = "Continuum-Installer"
        "X-GitHub-Api-Version" = "2022-11-28"
    }
    try {
        return Invoke-RestMethod -Uri $Uri -Headers $headers
    } catch {
        throw "GitHub API request failed for $Uri. $($_.Exception.Message)"
    }
}

function Get-ReleaseAsset($Release, [string]$Pattern, [string]$Description) {
    $matches = @($Release.assets | Where-Object { $_.name -match $Pattern })
    if ($matches.Count -ne 1) {
        $names = @($Release.assets | ForEach-Object { $_.name }) -join ", "
        throw "Expected exactly one $Description asset matching '$Pattern'; found $($matches.Count). Assets: $names"
    }
    if ([int64]$matches[0].size -le 0) {
        throw "$Description asset '$($matches[0].name)' is empty"
    }
    return $matches[0]
}

function Save-ReleaseAsset($Asset, [string]$Destination) {
    Write-Info "Downloading $($Asset.name)..."
    Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $Destination -Headers @{ "User-Agent" = "Continuum-Installer" }
    if (-not (Test-Path $Destination) -or (Get-Item $Destination).Length -le 0) {
        throw "Downloaded asset is missing or empty: $Destination"
    }
}

function Assert-Checksum([string]$Path, [string]$ChecksumFile) {
    $name = Split-Path -Leaf $Path
    $escaped = [Regex]::Escape($name)
    $line = Get-Content $ChecksumFile | Where-Object { $_ -match "^([0-9a-fA-F]{64})\s+\*?$escaped$" } | Select-Object -First 1
    if (-not $line) {
        throw "SHA256SUMS.txt has no checksum for $name"
    }
    $expected = ([Regex]::Match($line, "^[0-9a-fA-F]{64}")).Value.ToLowerInvariant()
    $actual = (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "Checksum mismatch for $name (expected $expected, got $actual)"
    }
    Write-Ok "Verified SHA-256 for $name"
}

function Install-AgentOsRegistration([string]$AgentOsPath) {
    $mcpDir = Join-Path $ContinuumDev "mcp-servers"
    New-Item -ItemType Directory -Force -Path $mcpDir | Out-Null
    $registration = [ordered]@{
        name = "agent-os"
        command = [System.IO.Path]::GetFullPath($AgentOsPath)
        args = @("--data-dir", [System.IO.Path]::GetFullPath($ContinuumDev))
        env = @{}
        enabled = $true
        installed_at = [DateTime]::UtcNow.ToString("o")
    }
    $target = Join-Path $mcpDir "agent-os.json"
    $temporary = "$target.$PID.tmp"
    $registration | ConvertTo-Json -Depth 6 | Set-Content -Path $temporary -Encoding UTF8
    Move-Item -Path $temporary -Destination $target -Force
    Write-Ok "Registered policy-gated Agent OS MCP server"
}

Write-Host ""
Write-Host "  CONTINUUM" -ForegroundColor Cyan
Write-Host "  Persistent context and action layer for AI agents" -ForegroundColor DarkGray

# ---- Platform and prerequisites ---------------------------------------------

Write-Header "Checking Windows"
$winVersion = [System.Environment]::OSVersion.Version
$buildNumber = [int](Get-CimInstance Win32_OperatingSystem).BuildNumber
if ($winVersion.Major -lt 10 -or ($winVersion.Major -eq 10 -and $buildNumber -lt 18362)) {
    Write-Err "Continuum requires Windows 10 1903+ or Windows 11. Detected build $buildNumber."
    exit 1
}
Write-Ok "Windows build $buildNumber is supported"

Write-Header "Checking prerequisites"
$nodeOk = $false
try {
    $nodeVer = node --version 2>&1
    if ($LASTEXITCODE -eq 0) {
        $major = [int]($nodeVer -replace '^v(\d+)\..*', '$1')
        if ($major -ge 22) {
            Write-Ok "Node.js $nodeVer"
            $nodeOk = $true
        }
    }
} catch {}
if (-not $nodeOk) {
    Write-Err "Continuum requires Node.js 22 or newer. Install it from https://nodejs.org and rerun."
    exit 1
}

if (-not (Test-Command "claude")) {
    Write-Warn "Claude Code CLI is not installed. Continuum can use other providers, but the Claude adapter will be unavailable."
    $response = Read-Host "Install @anthropic-ai/claude-code now? [Y/n]"
    if ($response -ne "n" -and $response -ne "N") {
        npm install -g @anthropic-ai/claude-code
        if ($LASTEXITCODE -ne 0) {
            throw "npm install -g @anthropic-ai/claude-code failed"
        }
        Write-Ok "Claude Code CLI installed"
    }
} else {
    Write-Ok "Claude Code CLI available"
}

if ($FromSource) {
    foreach ($tool in @("rustc", "cargo", "cmake")) {
        if (-not (Test-Command $tool)) {
            Write-Err "$tool is required for -FromSource. Install the Rust/native toolchain and rerun."
            exit 1
        }
    }
    if (-not (Test-Command "pnpm")) {
        Write-Info "Installing pnpm 10.11.1..."
        npm install -g pnpm@10.11.1
        if ($LASTEXITCODE -ne 0) { throw "pnpm installation failed" }
    }
}

# ---- Data and install directories -------------------------------------------

Write-Header "Preparing local data"
$continuumSubdirs = @(
    "config", "models", "models\vision", "models\triage", "models\stt", "models\tts",
    "logs", "memory", "backups", "bin", "worker-intents", "workers", "repair-intents"
)
foreach ($subdir in $continuumSubdirs) {
    New-Item -ItemType Directory -Force -Path (Join-Path $ContinuumData $subdir) | Out-Null
}
foreach ($subdir in @("mcp-servers", "logs", "chats", "backups")) {
    New-Item -ItemType Directory -Force -Path (Join-Path $ContinuumDev $subdir) | Out-Null
}
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Write-Ok "Prepared $ContinuumData and $ContinuumDev"

$defaultConfigDir = Join-Path $repoRoot "config"
$userConfigDir = Join-Path $ContinuumData "config"
if (Test-Path $defaultConfigDir) {
    foreach ($configFile in Get-ChildItem $defaultConfigDir -File) {
        $destination = Join-Path $userConfigDir $configFile.Name
        if (-not (Test-Path $destination)) {
            Copy-Item $configFile.FullName $destination
            Write-Ok "Seeded $($configFile.Name)"
        }
    }
}

# ---- Install binaries --------------------------------------------------------

Write-Header "Installing Continuum"
$desktopInstalledByNsis = $false

if ($FromSource) {
    Write-Step "Building runtime, context MCP and Agent OS from source..."
    Push-Location $repoRoot
    try {
        cargo build --release --locked --bin continuum --bin continuum-mcp --bin continuum-agent-os
        if ($LASTEXITCODE -ne 0) { throw "Rust runtime build failed" }

        foreach ($binary in @("continuum.exe", "continuum-mcp.exe", "continuum-agent-os.exe")) {
            Copy-Item (Join-Path $repoRoot "target\release\$binary") (Join-Path $InstallDir $binary) -Force
        }

        $resourceBin = Join-Path $repoRoot "apps\desktop\src-tauri\resources\bin"
        New-Item -ItemType Directory -Force -Path $resourceBin | Out-Null
        Copy-Item (Join-Path $repoRoot "target\release\continuum.exe") (Join-Path $resourceBin "continuum.exe") -Force
        Copy-Item (Join-Path $repoRoot "target\release\continuum-mcp.exe") (Join-Path $resourceBin "continuum-mcp.exe") -Force
        Copy-Item (Join-Path $repoRoot "target\release\continuum-agent-os.exe") (Join-Path $resourceBin "continuum-agent-os.exe") -Force

        Write-Step "Building desktop frontend and Tauri executable..."
        Push-Location (Join-Path $repoRoot "apps\desktop")
        try {
            pnpm install --frozen-lockfile
            if ($LASTEXITCODE -ne 0) { throw "pnpm install failed" }
            pnpm build
            if ($LASTEXITCODE -ne 0) { throw "desktop frontend build failed" }
        } finally {
            Pop-Location
        }
        cargo build --release --locked -p continuum-desktop
        if ($LASTEXITCODE -ne 0) { throw "continuum-desktop build failed" }
        Copy-Item (Join-Path $repoRoot "target\release\continuum-desktop.exe") (Join-Path $InstallDir "continuum-desktop.exe") -Force
        Write-Ok "Installed desktop, runtime, context MCP and Agent OS"
    } finally {
        Pop-Location
    }
} else {
    Write-Step "Resolving release assets from GitHub..."
    $releaseUri = if ($Version -eq "latest") {
        "https://api.github.com/repos/$Repository/releases/latest"
    } else {
        $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
        "https://api.github.com/repos/$Repository/releases/tags/$tag"
    }
    $release = Invoke-GitHubJson $releaseUri
    if ($release.draft -or $release.prerelease) {
        throw "Release $($release.tag_name) is not a complete latest-channel release"
    }

    $portableAsset = Get-ReleaseAsset $release '^continuum-.+-windows-x64\.zip$' "Windows portable"
    $installerAsset = Get-ReleaseAsset $release '^continuum-.+-windows-x64-setup\.exe$' "Windows installer"
    $checksumAsset = Get-ReleaseAsset $release '^SHA256SUMS\.txt$' "checksum manifest"

    $tempRoot = Join-Path $env:TEMP "continuum-install-$PID-$([Guid]::NewGuid().ToString('N'))"
    $zipPath = Join-Path $tempRoot $portableAsset.name
    $setupPath = Join-Path $tempRoot $installerAsset.name
    $checksumPath = Join-Path $tempRoot "SHA256SUMS.txt"
    $extractDir = Join-Path $tempRoot "portable"
    New-Item -ItemType Directory -Force -Path $tempRoot,$extractDir | Out-Null
    try {
        Save-ReleaseAsset $portableAsset $zipPath
        Save-ReleaseAsset $installerAsset $setupPath
        Save-ReleaseAsset $checksumAsset $checksumPath
        Assert-Checksum $zipPath $checksumPath
        Assert-Checksum $setupPath $checksumPath

        Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
        foreach ($binary in @("continuum.exe", "continuum-mcp.exe", "continuum-agent-os.exe")) {
            $source = Get-ChildItem $extractDir -Recurse -File -Filter $binary | Select-Object -First 1
            if (-not $source) { throw "Release ZIP does not contain required binary $binary" }
            Copy-Item $source.FullName (Join-Path $InstallDir $binary) -Force
        }

        foreach ($directory in @("config", "prompts", "skills")) {
            $sourceDir = Get-ChildItem $extractDir -Recurse -Directory -Filter $directory | Select-Object -First 1
            if ($sourceDir) {
                Copy-Item $sourceDir.FullName (Join-Path $InstallDir $directory) -Recurse -Force
            }
        }

        Write-Step "Installing the Continuum desktop app..."
        $nsisArgs = @("/S")
        if ([System.IO.Path]::GetFullPath($InstallDir) -ne [System.IO.Path]::GetFullPath($DefaultInstallDir)) {
            $nsisArgs += "/D=$([System.IO.Path]::GetFullPath($InstallDir))"
        }
        $process = Start-Process -FilePath $setupPath -ArgumentList $nsisArgs -Wait -PassThru
        if ($process.ExitCode -ne 0) {
            throw "Continuum desktop installer exited with code $($process.ExitCode)"
        }
        $desktopInstalledByNsis = $true
        Write-Ok "Installed release $($release.tag_name)"
    } finally {
        Remove-Item $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$runtimeExe = Join-Path $InstallDir "continuum.exe"
$mcpExe = Join-Path $InstallDir "continuum-mcp.exe"
$agentOsExe = Join-Path $InstallDir "continuum-agent-os.exe"
foreach ($required in @($runtimeExe, $mcpExe, $agentOsExe)) {
    if (-not (Test-Path $required)) { throw "Required installed binary is missing: $required" }
}
Install-AgentOsRegistration $agentOsExe

# ---- PATH and shortcuts ------------------------------------------------------

Write-Header "Configuring launch integration"
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if (($userPath -split ';') -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("PATH", (($userPath.TrimEnd(';') + ";" + $InstallDir).TrimStart(';')), "User")
    Write-Ok "Added $InstallDir to user PATH"
}
$env:PATH = "$env:PATH;$InstallDir"

$desktopExe = Join-Path $InstallDir "continuum-desktop.exe"
$shortcutTarget = if (Test-Path $desktopExe) { $desktopExe } else { $runtimeExe }
$shortcutArgs = if (Test-Path $desktopExe) { "" } else { "run" }
$shell = New-Object -ComObject WScript.Shell
$startMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Continuum"
New-Item -ItemType Directory -Force -Path $startMenuDir | Out-Null
$shortcut = $shell.CreateShortcut((Join-Path $startMenuDir "Continuum.lnk"))
$shortcut.TargetPath = $shortcutTarget
$shortcut.Arguments = $shortcutArgs
$shortcut.WorkingDirectory = $InstallDir
$shortcut.Description = "Continuum AI context and action layer"
$shortcut.Save()
Write-Ok "Created Start Menu shortcut"

if ($DesktopShortcut) {
    $desktopPath = [Environment]::GetFolderPath("Desktop")
    $shortcut = $shell.CreateShortcut((Join-Path $desktopPath "Continuum.lnk"))
    $shortcut.TargetPath = $shortcutTarget
    $shortcut.Arguments = $shortcutArgs
    $shortcut.WorkingDirectory = $InstallDir
    $shortcut.Description = "Continuum AI context and action layer"
    $shortcut.Save()
    Write-Ok "Created Desktop shortcut"
}

if ($AutoStart) {
    $startupDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Startup"
    $shortcut = $shell.CreateShortcut((Join-Path $startupDir "Continuum.lnk"))
    $shortcut.TargetPath = $shortcutTarget
    $shortcut.Arguments = $shortcutArgs
    $shortcut.WorkingDirectory = $InstallDir
    $shortcut.Description = "Start Continuum with Windows"
    $shortcut.Save()
    Write-Ok "Registered Windows startup shortcut"
}

# ---- Optional local model download ------------------------------------------

if (-not $SkipModels) {
    Write-Header "Local models"
    $configPath = Join-Path $repoRoot "config\config.example.toml"
    $downloadScript = Join-Path $repoRoot "scripts\download-models.ps1"
    $visionPreflightScript = Join-Path $repoRoot "scripts\ensure-vision-model.ps1"
    if (Test-Path $visionPreflightScript) {
        Write-Info "Checking the local vision model before optional model downloads"
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $visionPreflightScript -Mode Check -Variant smolvlm2-2.2b-q4
        $preferredVisionReady = $LASTEXITCODE -eq 0
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $visionPreflightScript -Mode Check -Variant smolvlm-500m
        $fallbackVisionReady = $LASTEXITCODE -eq 0
        if (-not $preferredVisionReady -or -not $fallbackVisionReady) {
            Write-Info "Vision model is unavailable or incomplete. Run download-models.ps1 or repair the reported variant."
        }
    }
    if (Test-Path $downloadScript) {
        $response = Read-Host "Download recommended local models now? This can require several GB. [y/N]"
        if ($response -eq "y" -or $response -eq "Y") {
            $oldContinuumHome = $env:CONTINUUM_HOME
            $env:CONTINUUM_HOME = $ContinuumData
            try {
                & $downloadScript
            } finally {
                $env:CONTINUUM_HOME = $oldContinuumHome
            }
        } else {
            Write-Info "Skipped model downloads"
        }
    } else {
        Write-Info "Model download script is not present in this installation"
    }
}

# ---- First-run config and verification --------------------------------------

Write-Header "Finalising configuration"
$configFile = Join-Path $ContinuumDev "config.toml"
if (-not (Test-Path $configFile)) {
    $example = Join-Path $repoRoot "config\config.example.toml"
    if (Test-Path $example) {
        Copy-Item $example $configFile
        Write-Ok "Created $configFile"
    }
}

try {
    $versionOutput = & $runtimeExe --version 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "continuum --version failed" }
    Write-Ok $versionOutput.Trim()
} catch {
    Write-Warn "Runtime verification failed: $($_.Exception.Message)"
}

try {
    $agentHelp = & $agentOsExe --help 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0 -or $agentHelp -notmatch "Agent OS") {
        throw "continuum-agent-os --help did not return the expected contract"
    }
    Write-Ok "Agent OS executable verified"
} catch {
    throw "Agent OS verification failed: $($_.Exception.Message)"
}

Write-Host ""
Write-Host "Continuum installation complete." -ForegroundColor Green
Write-Host ""
Write-Host "Installed components:" -ForegroundColor White
Write-Host "  Desktop:       $shortcutTarget" -ForegroundColor Gray
Write-Host "  Runtime:       $runtimeExe" -ForegroundColor Gray
Write-Host "  Context MCP:   $mcpExe" -ForegroundColor Gray
Write-Host "  Agent OS MCP:  $agentOsExe" -ForegroundColor Gray
Write-Host "  Data:          $ContinuumDev" -ForegroundColor Gray
Write-Host ""
Write-Host "Open Continuum from the Start Menu. A new terminal is required before the updated PATH is visible." -ForegroundColor Cyan
