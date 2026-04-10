# Kairo dev-setup.ps1
# Checks for prerequisites and prints their versions.
# Idempotent — safe to run multiple times.

$ErrorActionPreference = "Stop"

Write-Host "Kairo Development Setup" -ForegroundColor Cyan
Write-Host "========================" -ForegroundColor Cyan
Write-Host ""

$allGood = $true

# Check Rust
Write-Host "Checking Rust..." -NoNewline
try {
    $rustVersion = rustc --version 2>&1
    Write-Host " OK ($rustVersion)" -ForegroundColor Green
} catch {
    Write-Host " MISSING" -ForegroundColor Red
    Write-Host "  Install Rust: https://rustup.rs" -ForegroundColor Yellow
    $allGood = $false
}

# Check Cargo
Write-Host "Checking Cargo..." -NoNewline
try {
    $cargoVersion = cargo --version 2>&1
    Write-Host " OK ($cargoVersion)" -ForegroundColor Green
} catch {
    Write-Host " MISSING" -ForegroundColor Red
    $allGood = $false
}

# Check pnpm
Write-Host "Checking pnpm..." -NoNewline
try {
    $pnpmVersion = pnpm --version 2>&1
    Write-Host " OK (v$pnpmVersion)" -ForegroundColor Green
} catch {
    Write-Host " MISSING" -ForegroundColor Red
    Write-Host "  Install pnpm: npm install -g pnpm" -ForegroundColor Yellow
    $allGood = $false
}

# Check Node.js
Write-Host "Checking Node.js..." -NoNewline
try {
    $nodeVersion = node --version 2>&1
    Write-Host " OK ($nodeVersion)" -ForegroundColor Green
} catch {
    Write-Host " MISSING" -ForegroundColor Red
    Write-Host "  Install Node.js 20+: https://nodejs.org" -ForegroundColor Yellow
    $allGood = $false
}

# Check Claude Code CLI
Write-Host "Checking Claude Code CLI..." -NoNewline
try {
    $claudeVersion = claude --version 2>&1
    Write-Host " OK ($claudeVersion)" -ForegroundColor Green
} catch {
    Write-Host " MISSING" -ForegroundColor Red
    Write-Host "  Install: npm install -g @anthropic-ai/claude-code" -ForegroundColor Yellow
    Write-Host "  Then run: claude login" -ForegroundColor Yellow
    $allGood = $false
}

Write-Host ""

if ($allGood) {
    Write-Host "All prerequisites found. You're ready to develop Kairo." -ForegroundColor Green
    Write-Host ""
    Write-Host "Quick start:" -ForegroundColor Cyan
    Write-Host "  cargo build --workspace     # Build all Rust crates"
    Write-Host "  cd apps/desktop && pnpm install && pnpm build  # Build frontend"
    exit 0
} else {
    Write-Host "Some prerequisites are missing. Install them and re-run this script." -ForegroundColor Red
    exit 1
}
