# Continuum development prerequisite check.
# Idempotent and read-only: it reports exact install/configuration actions.

$ErrorActionPreference = "Stop"
$requiredRustVersion = "1.94.0"
$allGood = $true

function Write-CheckResult {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][bool]$Ok,
        [string]$Detail = ""
    )

    Write-Host "Checking $Name..." -NoNewline
    if ($Ok) {
        Write-Host " OK $Detail" -ForegroundColor Green
    } else {
        Write-Host " MISSING $Detail" -ForegroundColor Red
        $script:allGood = $false
    }
}

function Get-CommandVersion {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [string[]]$Arguments = @("--version")
    )

    if (-not (Get-Command $Command -ErrorAction SilentlyContinue)) {
        return $null
    }

    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $output = & $Command @Arguments 2>&1
    $ErrorActionPreference = $previousErrorActionPreference
    if ($LASTEXITCODE -ne 0) {
        return $null
    }
    return ($output | Select-Object -First 1).ToString().Trim()
}

Write-Host "Continuum Development Setup" -ForegroundColor Cyan
Write-Host "========================" -ForegroundColor Cyan
Write-Host ""

$rustupVersion = Get-CommandVersion -Command "rustup"
Write-CheckResult -Name "rustup" -Ok ($null -ne $rustupVersion) -Detail $rustupVersion

$rustVersion = $null
if ($rustupVersion) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $rustOutput = & rustup run $requiredRustVersion rustc --version 2>&1
    $ErrorActionPreference = $previousErrorActionPreference
    if ($LASTEXITCODE -eq 0) {
        $rustVersion = ($rustOutput | Select-Object -First 1).ToString().Trim()
    }
}
Write-CheckResult -Name "Rust $requiredRustVersion" -Ok ($null -ne $rustVersion) -Detail $rustVersion
if (-not $rustVersion) {
    Write-Host "  Install: rustup toolchain install $requiredRustVersion --profile minimal --component clippy,rustfmt" -ForegroundColor Yellow
}

$cargoVersion = $null
if ($rustVersion) {
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $cargoOutput = & rustup run $requiredRustVersion cargo --version 2>&1
    $ErrorActionPreference = $previousErrorActionPreference
    if ($LASTEXITCODE -eq 0) {
        $cargoVersion = ($cargoOutput | Select-Object -First 1).ToString().Trim()
    }
}
Write-CheckResult -Name "Cargo for Rust $requiredRustVersion" -Ok ($null -ne $cargoVersion) -Detail $cargoVersion

$cmakeVersion = Get-CommandVersion -Command "cmake"
Write-CheckResult -Name "CMake" -Ok ($null -ne $cmakeVersion) -Detail $cmakeVersion
if (-not $cmakeVersion) {
    Write-Host "  Install: winget install --id Kitware.CMake --exact" -ForegroundColor Yellow
}

$ninjaVersion = Get-CommandVersion -Command "ninja"
Write-CheckResult -Name "Ninja" -Ok ($null -ne $ninjaVersion) -Detail $ninjaVersion
if (-not $ninjaVersion) {
    Write-Host "  Install: winget install --id Ninja-build.Ninja --exact" -ForegroundColor Yellow
}

$protocPath = $null
if ($env:PROTOC -and (Test-Path -LiteralPath $env:PROTOC -PathType Leaf)) {
    $protocPath = (Resolve-Path -LiteralPath $env:PROTOC).Path
} else {
    $protocCommand = Get-Command protoc -ErrorAction SilentlyContinue
    if ($protocCommand) {
        $protocPath = $protocCommand.Source
    }
}
Write-CheckResult -Name "protoc" -Ok ($null -ne $protocPath) -Detail $protocPath
if (-not $protocPath) {
    Write-Host "  Install protobuf, then set PROTOC to the full protoc.exe path." -ForegroundColor Yellow
}

$libclangPath = $null
$llvmCandidates = @(
    $env:LIBCLANG_PATH,
    "C:\LLVM\bin",
    "C:\Program Files\LLVM\bin"
) | Where-Object { $_ }
foreach ($candidate in $llvmCandidates) {
    if (Test-Path -LiteralPath (Join-Path $candidate "libclang.dll") -PathType Leaf) {
        $libclangPath = (Resolve-Path -LiteralPath $candidate).Path
        break
    }
}
Write-CheckResult -Name "LLVM/libclang" -Ok ($null -ne $libclangPath) -Detail $libclangPath
if ($libclangPath -and $env:LIBCLANG_PATH -ne $libclangPath) {
    Write-Host "  Current shell: `$env:LIBCLANG_PATH = '$libclangPath'" -ForegroundColor Yellow
} elseif (-not $libclangPath) {
    Write-Host "  Install: winget install --id LLVM.LLVM --exact" -ForegroundColor Yellow
    Write-Host "  Then set LIBCLANG_PATH to LLVM's bin directory." -ForegroundColor Yellow
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$msvcPath = $null
if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
    $msvcPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
Write-CheckResult -Name "MSVC C++ Build Tools" -Ok (-not [string]::IsNullOrWhiteSpace($msvcPath)) -Detail $msvcPath
if (-not $msvcPath) {
    Write-Host "  Install Visual Studio Build Tools with Desktop development with C++." -ForegroundColor Yellow
}

$nodeVersion = Get-CommandVersion -Command "node"
Write-CheckResult -Name "Node.js" -Ok ($null -ne $nodeVersion) -Detail $nodeVersion

$pnpmVersion = Get-CommandVersion -Command "pnpm"
Write-CheckResult -Name "pnpm" -Ok ($null -ne $pnpmVersion) -Detail $pnpmVersion

$claudeVersion = Get-CommandVersion -Command "claude"
Write-CheckResult -Name "Claude Code CLI" -Ok ($null -ne $claudeVersion) -Detail $claudeVersion

Write-Host ""
if (-not $allGood) {
    Write-Host "Some prerequisites are missing. Apply the actions above, then rerun this script." -ForegroundColor Red
    exit 1
}

Write-Host "All required development prerequisites were found." -ForegroundColor Green
Write-Host ""
Write-Host "Rust verification:" -ForegroundColor Cyan
Write-Host "  cargo +$requiredRustVersion fmt --all -- --check"
Write-Host "  cargo +$requiredRustVersion clippy --workspace --all-targets -- -D warnings"
Write-Host "  cargo +$requiredRustVersion test --workspace"
exit 0
