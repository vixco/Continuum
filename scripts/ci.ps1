param(
    [ValidateSet(
        "All",
        "Full",
        "Format",
        "ReleaseContract",
        "ClippyLight",
        "ClippyFull",
        "Tests",
        "TestsFast",
        "Desktop",
        "Docs",
        "Evaluations",
        "ReleaseWindows"
    )]
    [string]$Stage = "All",
    [switch]$SkipInstall,
    [switch]$SkipToolchainCheck,
    [switch]$StrictToolchain
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$desktopRoot = Join-Path $repoRoot "apps\desktop"
$docsRoot = Join-Path $repoRoot "apps\docs"
$runningOnWindows = $env:OS -eq "Windows_NT"
$script:dependenciesReady = $SkipInstall.IsPresent
if ([string]::IsNullOrWhiteSpace($env:GITHUB_ACTIONS) -and [string]::IsNullOrWhiteSpace($env:CARGO_TARGET_DIR)) {
    $env:CARGO_TARGET_DIR = Join-Path $repoRoot "target\ci-local"
}
$cargoTargetRoot = if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
    $env:CARGO_TARGET_DIR
} else {
    Join-Path $repoRoot $env:CARGO_TARGET_DIR
}
if ([string]::IsNullOrWhiteSpace($env:CARGO_BUILD_JOBS)) {
    $env:CARGO_BUILD_JOBS = "4"
}

function Write-Stage([string]$Name) {
    Write-Host ""
    Write-Host "==> $Name" -ForegroundColor Cyan
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [string]$WorkingDirectory = $repoRoot,
        [int[]]$ExpectedExitCodes = @(0)
    )

    Push-Location $WorkingDirectory
    try {
        Write-Host "> $Command $($Arguments -join ' ')" -ForegroundColor DarkGray
        & $Command @Arguments
        $exitCode = $LASTEXITCODE
        if ($exitCode -notin $ExpectedExitCodes) {
            throw "Command failed with exit code ${exitCode}: $Command $($Arguments -join ' ')"
        }
    }
    finally {
        Pop-Location
    }
}

function Assert-Command([string]$Name) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "Required command '$Name' is missing. Run scripts/dev-setup.ps1 first."
    }
}

function Initialize-MsvcEnvironment {
    if (-not $runningOnWindows -or -not [string]::IsNullOrWhiteSpace($env:INCLUDE)) {
        return
    }

    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        throw "Visual Studio Build Tools were not found. Run scripts/dev-setup.ps1 first."
    }
    $installationPath = (& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath).Trim()
    if ([string]::IsNullOrWhiteSpace($installationPath)) {
        throw "Visual Studio C++ Build Tools were not found. Run scripts/dev-setup.ps1 first."
    }
    $devCommand = Join-Path $installationPath "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path $devCommand)) {
        throw "VsDevCmd.bat was not found under $installationPath."
    }

    $environmentLines = & cmd.exe /d /s /c "`"$devCommand`" -no_logo -arch=x64 -host_arch=x64 >nul && set"
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to initialize the Visual Studio C++ environment."
    }
    foreach ($line in $environmentLines) {
        $separator = $line.IndexOf('=')
        if ($separator -gt 0) {
            $name = $line.Substring(0, $separator)
            $value = $line.Substring($separator + 1)
            [Environment]::SetEnvironmentVariable($name, $value, "Process")
        }
    }
}

function Initialize-Toolchain {
    if ($SkipToolchainCheck) {
        return
    }

    Write-Stage "Toolchain preflight"
    foreach ($command in @("cargo", "rustc", "python", "node", "pnpm")) {
        Assert-Command $command
    }

    $rustVersion = (& rustc --version).Trim()
    if ($rustVersion -notmatch '^rustc 1\.94\.0\b') {
        throw "Rust 1.94.0 is required; found '$rustVersion'."
    }

    $pnpmVersion = (& pnpm --version).Trim()
    if ($pnpmVersion -ne "10.11.1") {
        throw "pnpm 10.11.1 is required; found '$pnpmVersion'."
    }

    $nodeVersion = (& node --version).Trim()
    if ($nodeVersion -notmatch '^v22\.') {
        $message = "GitHub CI uses Node 22; local preflight found '$nodeVersion'."
        if ($StrictToolchain) {
            throw $message
        }
        Write-Warning "$message Continuing because Node >=22 is supported; use -StrictToolchain for exact parity."
    }
}

function Initialize-NativeToolchain {
    Initialize-MsvcEnvironment
    foreach ($command in @("cmake", "ninja", "protoc")) {
        Assert-Command $command
    }

    if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH) -and $runningOnWindows) {
        foreach ($commonLlvm in @("C:\LLVM\bin", "C:\Program Files\LLVM\bin")) {
            if (Test-Path (Join-Path $commonLlvm "libclang.dll")) {
                $env:LIBCLANG_PATH = $commonLlvm
                break
            }
        }
    }
    if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
        throw "LIBCLANG_PATH is not configured. Run scripts/dev-setup.ps1 first."
    }
    if ($runningOnWindows -and -not (Test-Path (Join-Path $env:LIBCLANG_PATH "libclang.dll"))) {
        throw "libclang.dll was not found under LIBCLANG_PATH=$env:LIBCLANG_PATH"
    }

    if ([string]::IsNullOrWhiteSpace($env:PROTOC)) {
        $env:PROTOC = (Get-Command protoc).Source
    }
}

function Install-Dependencies {
    if ($script:dependenciesReady) {
        return
    }
    Write-Stage "Frozen JavaScript dependencies"
    Invoke-Checked "pnpm" @("install", "--frozen-lockfile")
    $script:dependenciesReady = $true
}

function Invoke-Format {
    Write-Stage "Rust formatting"
    Invoke-Checked "cargo" @("fmt", "--all", "--", "--check")
    Invoke-Checked "git" @("diff", "--check")
}

function Invoke-ReleaseContract {
    Write-Stage "Release contract"
    Invoke-Checked "python" @("-m", "unittest", "-v", "scripts/test_release_contract.py")
    Invoke-Checked "python" @("scripts/release_contract.py", "validate-config", "--repo-root", ".")
}

function Invoke-ClippyLight {
    Write-Stage "Clippy light path"
    Invoke-Checked "cargo" @("clippy", "-p", "continuum-core", "--no-default-features", "--lib", "--tests", "--", "-D", "warnings")
    Invoke-Checked "cargo" @("clippy", "-p", "continuum-desktop", "--no-deps", "--", "-D", "warnings")
}

function Invoke-ClippyFull {
    Write-Stage "Clippy full workspace"
    Initialize-NativeToolchain
    Invoke-Checked "cargo" @("clippy", "--workspace", "--all-targets", "--", "-D", "warnings")
}

function Invoke-Tests {
    Write-Stage "Rust test suite"
    Initialize-NativeToolchain
    Invoke-Checked "cargo" @("test", "-p", "continuum-core", "--no-default-features", "--lib")
    Invoke-Checked "cargo" @("test", "--workspace", "--", "--skip", "bench::score::tests::")
    Invoke-Checked "cargo" @("test", "-p", "continuum-core", "bench::score::tests::", "--", "--test-threads=1")
}

function Invoke-TestsFast {
    Write-Stage "Focused Rust tests"
    Invoke-Checked "cargo" @("test", "-p", "continuum-core", "--no-default-features", "--lib")
    Invoke-Checked "cargo" @("test", "-p", "continuum-desktop", "--bin", "continuum-desktop")
}

function Invoke-DesktopFrontend {
    Write-Stage "Desktop frontend"
    Install-Dependencies
    Invoke-Checked "pnpm" @("typecheck") $desktopRoot
    Invoke-Checked "pnpm" @("lint") $desktopRoot
    Invoke-Checked "pnpm" @("format") $desktopRoot
    Invoke-Checked "pnpm" @("build") $desktopRoot
}

function Invoke-Desktop {
    Invoke-DesktopFrontend
    Write-Stage "Tauri compile"
    Invoke-Checked "cargo" @("build", "-p", "continuum-desktop")
}

function Invoke-Docs {
    Write-Stage "Documentation build"
    Install-Dependencies
    Invoke-Checked "pnpm" @("build") $docsRoot
}

function Invoke-Evaluations {
    Write-Stage "Persistent-intelligence and autonomy contracts"
    Invoke-Checked "python" @("-m", "unittest", "-v", "scripts/test_persistent_intelligence_eval.py")
    Invoke-Checked "python" @("scripts/persistent_intelligence_eval.py", "--suite", "evals/persistent-intelligence/reference-suite.json", "--report", ".continuum-dev/ci/persistent-intelligence-report.json")
    Invoke-Checked "python" @("scripts/persistent_intelligence_eval.py", "--suite", "evals/persistent-intelligence/reference-suite.json", "--require-runtime", "--report", ".continuum-dev/ci/persistent-intelligence-runtime-gate-report.json") -ExpectedExitCodes @(1)
    Invoke-Checked "python" @("-m", "unittest", "-v", "scripts/test_autonomy_contract_eval.py")
    Invoke-Checked "python" @("scripts/autonomy_contract_eval.py", "--suite", "evals/autonomy/reference-suite.json", "--report", ".continuum-dev/ci/autonomy-contract-report.json")
    Invoke-Checked "python" @("scripts/autonomy_contract_eval.py", "--suite", "evals/autonomy/reference-suite.json", "--require-runtime", "--report", ".continuum-dev/ci/autonomy-runtime-gate-report.json") -ExpectedExitCodes @(1)
}

function Invoke-All {
    Install-Dependencies
    Invoke-Format
    Invoke-ReleaseContract
    Invoke-TestsFast
    # The desktop Rust test harness already compiles the native source. Avoid a
    # second executable link locally; the dedicated CI/release stages still do it.
    Invoke-DesktopFrontend
    Invoke-Docs
}

function Invoke-Full {
    Invoke-All
    $previousJobs = $env:CARGO_BUILD_JOBS
    $env:CARGO_BUILD_JOBS = "2"
    try {
        Invoke-ClippyLight
        Invoke-ClippyFull
        Invoke-Tests
        Invoke-Evaluations
    }
    finally {
        $env:CARGO_BUILD_JOBS = $previousJobs
    }
}

function Invoke-WindowsReleaseDryRun {
    if (-not $runningOnWindows) {
        throw "ReleaseWindows is a Windows-only NSIS dry-run. DMGs require native macOS runners."
    }

    Invoke-All
    Write-Stage "Windows NSIS release dry-run"
    $env:CARGO_BUILD_JOBS = "2"
    Initialize-NativeToolchain
    Invoke-Checked "cargo" @("build", "--release", "--locked", "--bin", "continuum", "--bin", "continuum-mcp", "--bin", "continuum-agent-os")

    $binDir = Join-Path $desktopRoot "src-tauri\resources\bin"
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Copy-Item (Join-Path $cargoTargetRoot "release\continuum.exe") (Join-Path $binDir "continuum.exe") -Force
    Copy-Item (Join-Path $cargoTargetRoot "release\continuum-mcp.exe") (Join-Path $binDir "continuum-mcp.exe") -Force
    Copy-Item (Join-Path $cargoTargetRoot "release\continuum-agent-os.exe") (Join-Path $binDir "continuum-agent-os.exe") -Force

    $bundleDir = Join-Path $cargoTargetRoot "release\bundle"
    $targetRelease = (Resolve-Path (Join-Path $cargoTargetRoot "release")).Path
    if (Test-Path $bundleDir) {
        $resolvedBundle = (Resolve-Path $bundleDir).Path
        if (-not $resolvedBundle.StartsWith($targetRelease, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to clean unexpected bundle path: $resolvedBundle"
        }
        Remove-Item -LiteralPath $resolvedBundle -Recurse -Force
    }

    $unsignedUpdaterOverride = '{"bundle":{"createUpdaterArtifacts":false}}'
    Invoke-Checked "pnpm" @("tauri", "build", "--bundles", "nsis", "--config", $unsignedUpdaterOverride) $desktopRoot

    $installers = @(Get-ChildItem (Join-Path $bundleDir "nsis") -File -Filter "*-setup.exe")
    if ($installers.Count -ne 1) {
        throw "Expected exactly one NSIS installer; found $($installers.Count)."
    }
    Write-Host "Local installer: $($installers[0].FullName)" -ForegroundColor Green
    Write-Warning "This local dry-run is not updater-signed or Authenticode-signed. GitHub builds signed updater artifacts."
}

Push-Location $repoRoot
try {
    Initialize-Toolchain
    switch ($Stage) {
        "All" { Invoke-All }
        "Full" { Invoke-Full }
        "Format" { Invoke-Format }
        "ReleaseContract" { Invoke-ReleaseContract }
        "ClippyLight" { Invoke-ClippyLight }
        "ClippyFull" { Invoke-ClippyFull }
        "Tests" { Invoke-Tests }
        "TestsFast" { Invoke-TestsFast }
        "Desktop" { Invoke-Desktop }
        "Docs" { Invoke-Docs }
        "Evaluations" { Invoke-Evaluations }
        "ReleaseWindows" { Invoke-WindowsReleaseDryRun }
    }
    Write-Host ""
    Write-Host "CI stage '$Stage' passed." -ForegroundColor Green
}
finally {
    Pop-Location
}
