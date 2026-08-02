# Continuum sign-release.ps1
# Placeholder for Windows Authenticode code-signing of release binaries.
#
# As of 0.1.0-alpha.1, this script is a placeholder — Continuum binaries ship
# unsigned and Windows SmartScreen warns on first launch. See
# docs/release.md for the plan to turn this on once we have a certificate.
#
# Usage (once wired up):
#   .\scripts\sign-release.ps1 -ArtifactDir target\release -Thumbprint <hex>

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ArtifactDir,

    [string]$Thumbprint = $env:CONTINUUM_SIGN_THUMBPRINT,
    [string]$TimestampUrl = "http://timestamp.digicert.com",
    [string]$DisplayName = "Continuum",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# Legacy fallback: pick up the old KAIRO_SIGN_THUMBPRINT env var during the
# Kairo→Continuum migration if the new name is unset.
if (-not $Thumbprint -and $env:KAIRO_SIGN_THUMBPRINT) {
    $Thumbprint = $env:KAIRO_SIGN_THUMBPRINT
}

if (-not $Thumbprint) {
    Write-Warning "No certificate thumbprint provided. Set CONTINUUM_SIGN_THUMBPRINT or pass -Thumbprint."
    Write-Warning "This script is a placeholder. Skipping signing."
    exit 0
}

# Locate signtool.exe
$signtool = Get-Command signtool.exe -ErrorAction SilentlyContinue
if (-not $signtool) {
    # Try the Windows 10/11 SDK default path
    $kits = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending
    foreach ($kit in $kits) {
        $candidate = Join-Path $kit.FullName "x64\signtool.exe"
        if (Test-Path $candidate) {
            $signtool = $candidate
            break
        }
    }
}
if (-not $signtool) {
    Write-Error "signtool.exe not found. Install the Windows SDK."
    exit 1
}

if (-not (Test-Path $ArtifactDir)) {
    Write-Error "Artifact directory not found: $ArtifactDir"
    exit 1
}

$binaries = @(
    (Join-Path $ArtifactDir "continuum.exe"),
    (Join-Path $ArtifactDir "continuum-mcp.exe"),
    (Join-Path $ArtifactDir "continuum-desktop.exe")
) | Where-Object { Test-Path $_ }

if ($binaries.Count -eq 0) {
    Write-Warning "No Continuum binaries found in $ArtifactDir"
    exit 0
}

foreach ($bin in $binaries) {
    Write-Host "Signing $bin..." -ForegroundColor Cyan
    $args = @(
        "sign",
        "/sha1", $Thumbprint,
        "/tr", $TimestampUrl,
        "/td", "sha256",
        "/fd", "sha256",
        "/d", $DisplayName,
        "/du", "https://github.com/vixco/Continuum",
        "`"$bin`""
    )
    if ($DryRun) {
        Write-Host "  [dry-run] signtool $($args -join ' ')"
    } else {
        & $signtool @args
        if ($LASTEXITCODE -ne 0) {
            Write-Error "Signing failed for $bin (exit code $LASTEXITCODE)"
            exit 1
        }
    }
}

Write-Host "All binaries signed." -ForegroundColor Green
