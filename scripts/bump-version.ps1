# Kairo bump-version.ps1
# Updates the Kairo version number across every file that hard-codes it.
#
# Usage:
#   .\scripts\bump-version.ps1 -NewVersion 0.1.0-alpha.2
#   .\scripts\bump-version.ps1 -NewVersion 0.2.0 -DryRun

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$NewVersion,

    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# Validate the version loosely - fail fast on typos without becoming a SemVer parser.
if ($NewVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$') {
    Write-Error "Version '$NewVersion' doesn't look like SemVer (e.g. 0.1.0-alpha.1 or 1.2.3)."
    exit 1
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Write-Host "Kairo bump-version" -ForegroundColor Cyan
Write-Host "  repo: $repoRoot"
Write-Host "  new:  $NewVersion"
Write-Host ""

$targets = @(
    @{
        Path    = Join-Path $repoRoot "Cargo.toml"
        Find    = '(?ms)^(\[workspace\.package\][^\[]*?\nversion\s*=\s*")[^"]+(")'
        Replace = '${1}' + $NewVersion + '${2}'
        Label   = "Cargo workspace version"
    },
    @{
        Path    = Join-Path $repoRoot "apps\desktop\package.json"
        Find    = '("version"\s*:\s*")[^"]+(")'
        Replace = '${1}' + $NewVersion + '${2}'
        Label   = "desktop package.json"
    },
    @{
        Path    = Join-Path $repoRoot "apps\desktop\src-tauri\tauri.conf.json"
        Find    = '("version"\s*:\s*")[^"]+(")'
        Replace = '${1}' + $NewVersion + '${2}'
        Label   = "tauri.conf.json"
    },
    @{
        Path    = Join-Path $repoRoot "apps\desktop\src\lib\tauri.ts"
        Find    = '(version:\s*")[^"]+(")'
        Replace = '${1}' + $NewVersion + '${2}'
        Label   = "DEFAULT_STATE.system.version in tauri.ts"
    },
    @{
        Path    = Join-Path $repoRoot "Cargo.lock"
        Find    = '(?ms)(\[\[package\]\]\s*\r?\nname = "kairo-(?:core|desktop|llm|mcp|vision)"\s*\r?\nversion = ")[^"]+(")'
        Replace = '${1}' + $NewVersion + '${2}'
        Label   = "workspace packages in Cargo.lock"
    }
)

$changed = 0
foreach ($t in $targets) {
    if (-not (Test-Path $t.Path)) {
        Write-Warning "Skipping (missing): $($t.Path)"
        continue
    }
    $content = Get-Content $t.Path -Raw
    $updated = [System.Text.RegularExpressions.Regex]::Replace($content, $t.Find, $t.Replace)
    if ($updated -eq $content) {
        Write-Warning "$($t.Label): no match in $($t.Path) - pattern may be out of date."
        continue
    }
    if ($DryRun) {
        Write-Host "[dry-run] $($t.Label): $($t.Path)" -ForegroundColor Yellow
    } else {
        Set-Content -Path $t.Path -Value $updated -NoNewline
        Write-Host "[ok] $($t.Label)" -ForegroundColor Green
        $changed++
    }
}

Write-Host ""
if ($DryRun) {
    Write-Host "Dry run complete - no files modified." -ForegroundColor Yellow
} else {
    Write-Host "$changed file(s) updated." -ForegroundColor Green
    Write-Host "Next steps:" -ForegroundColor Cyan
    Write-Host "  cargo check --workspace"
    Write-Host "  git add -A"
    Write-Host "  git commit -m 'chore: bump version to $NewVersion'"
}
