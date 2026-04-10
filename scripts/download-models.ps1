# Kairo download-models.ps1
# Downloads default model files for Kairo's local inference.
# Idempotent — skips files that already exist with valid sizes.
#
# Uses curl.exe (ships with Windows 10+) instead of Invoke-WebRequest
# because HuggingFace requires following redirects (302 → CDN) and
# Invoke-WebRequest handles this unreliably on older PowerShell versions.

$ErrorActionPreference = "Stop"

Write-Host "Kairo Model Downloader" -ForegroundColor Cyan
Write-Host "======================" -ForegroundColor Cyan
Write-Host ""

$ModelsBase = Join-Path $env:USERPROFILE ".kairo-dev\models"

# Minimum file size (bytes) to consider a download valid.
# Anything smaller is likely an error page saved as a file.
$MinValidSize = 1048576  # 1 MB

function Download-Model {
    param(
        [string]$Name,
        [string]$Url,
        [string]$OutPath,
        [string]$ExpectedSizeMB
    )

    $dir = Split-Path $OutPath -Parent
    if (-not (Test-Path $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }

    if (Test-Path $OutPath) {
        $size = (Get-Item $OutPath).Length
        if ($size -gt $MinValidSize) {
            Write-Host "[OK] $Name already exists ($([math]::Round($size / 1MB)) MB)" -ForegroundColor Green
            return
        } else {
            Write-Host "[WARN] $Name exists but is only $size bytes (corrupt/incomplete), re-downloading..." -ForegroundColor Yellow
            Remove-Item $OutPath
        }
    }

    Write-Host "[DL] Downloading $Name (~$ExpectedSizeMB MB)..." -ForegroundColor Yellow
    Write-Host "     URL: $Url" -ForegroundColor Gray
    Write-Host "     To:  $OutPath" -ForegroundColor Gray

    # Use curl.exe with -L to follow redirects. This handles HuggingFace's
    # 302 redirect to their CDN correctly, unlike Invoke-WebRequest which
    # sometimes fails with "Invalid username or password" on redirect.
    & curl.exe -L --fail --progress-bar -o $OutPath $Url

    if ($LASTEXITCODE -ne 0) {
        Write-Host "[FAIL] Download failed for $Name (curl exit code $LASTEXITCODE)" -ForegroundColor Red
        Write-Host "       Try downloading manually from:" -ForegroundColor Gray
        Write-Host "       $Url" -ForegroundColor Gray
        Write-Host "       Save to: $OutPath" -ForegroundColor Gray
        if (Test-Path $OutPath) { Remove-Item $OutPath }
        return
    }

    # Verify download succeeded by checking file size.
    if (Test-Path $OutPath) {
        $dlSize = (Get-Item $OutPath).Length
        if ($dlSize -lt $MinValidSize) {
            Write-Host "[FAIL] $Name downloaded but file is only $dlSize bytes (expected ~${ExpectedSizeMB} MB)" -ForegroundColor Red
            Write-Host "       The server may have returned an error page instead of the file." -ForegroundColor Gray
            Remove-Item $OutPath
            return
        }
        Write-Host "[OK] $Name downloaded ($([math]::Round($dlSize / 1MB)) MB)" -ForegroundColor Green
    } else {
        Write-Host "[FAIL] $Name file not found after download" -ForegroundColor Red
    }
}

# ============================================================================
# SmolVLM-256M (Vision — Layer 1)
# ============================================================================
# Source: HuggingFaceTB official repo (onnx-community is now auth-gated).
# The kairo-vision crate expects: vision_encoder.onnx, embed_tokens.onnx,
# decoder.onnx, and tokenizer.json in the same directory.

Write-Host "`n--- SmolVLM-256M (Vision) ---" -ForegroundColor Cyan

$VisionDir = Join-Path $ModelsBase "vision\smolvlm-256m"
$HfVisionBase = "https://huggingface.co/HuggingFaceTB/SmolVLM-256M-Instruct/resolve/main"

Download-Model `
    -Name "SmolVLM vision encoder" `
    -Url "$HfVisionBase/onnx/vision_encoder.onnx" `
    -OutPath (Join-Path $VisionDir "vision_encoder.onnx") `
    -ExpectedSizeMB "374"

Download-Model `
    -Name "SmolVLM embed_tokens" `
    -Url "$HfVisionBase/onnx/embed_tokens.onnx" `
    -OutPath (Join-Path $VisionDir "embed_tokens.onnx") `
    -ExpectedSizeMB "113"

Download-Model `
    -Name "SmolVLM decoder" `
    -Url "$HfVisionBase/onnx/decoder_model_merged.onnx" `
    -OutPath (Join-Path $VisionDir "decoder.onnx") `
    -ExpectedSizeMB "86"

Download-Model `
    -Name "SmolVLM tokenizer" `
    -Url "$HfVisionBase/tokenizer.json" `
    -OutPath (Join-Path $VisionDir "tokenizer.json") `
    -ExpectedSizeMB "3"

# Clean up stale encoder.onnx if it exists (previous script saved 401 error as file)
$StaleEncoder = Join-Path $VisionDir "encoder.onnx"
if ((Test-Path $StaleEncoder) -and ((Get-Item $StaleEncoder).Length -lt $MinValidSize)) {
    Write-Host "[CLEAN] Removing stale encoder.onnx (29-byte error page)" -ForegroundColor Yellow
    Remove-Item $StaleEncoder
}

# ============================================================================
# Qwen 3 4B Q4_K_M (Triage LLM — Layer 2)
# ============================================================================
# Source: Official Qwen org on HuggingFace (no auth required).
# Note: bartowski repo is auth-gated; the official Qwen org is not.

Write-Host "`n--- Qwen 3 4B Q4_K_M (Triage) ---" -ForegroundColor Cyan

Download-Model `
    -Name "Qwen 3 4B Q4_K_M" `
    -Url "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf" `
    -OutPath (Join-Path $ModelsBase "triage\qwen3-4b-q4_k_m.gguf") `
    -ExpectedSizeMB "2500"

# ============================================================================
# Whisper small (STT — Layer 1 audio)
# ============================================================================

Write-Host "`n--- Whisper small (STT) ---" -ForegroundColor Cyan

Download-Model `
    -Name "Whisper small" `
    -Url "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin" `
    -OutPath (Join-Path $ModelsBase "stt\whisper-small.bin") `
    -ExpectedSizeMB "465"

# ============================================================================
# Summary
# ============================================================================

Write-Host "`n============================================" -ForegroundColor Cyan
Write-Host "Model directory: $ModelsBase" -ForegroundColor Gray

# Verify all critical files are present.
$critical = @(
    @("Vision encoder", (Join-Path $VisionDir "vision_encoder.onnx")),
    @("Vision embed_tokens", (Join-Path $VisionDir "embed_tokens.onnx")),
    @("Vision decoder", (Join-Path $VisionDir "decoder.onnx")),
    @("Vision tokenizer", (Join-Path $VisionDir "tokenizer.json")),
    @("Triage model", (Join-Path $ModelsBase "triage\qwen3-4b-q4_k_m.gguf")),
    @("Whisper STT", (Join-Path $ModelsBase "stt\whisper-small.bin"))
)

$allOk = $true
foreach ($item in $critical) {
    $label = $item[0]
    $path = $item[1]
    if ((Test-Path $path) -and ((Get-Item $path).Length -gt $MinValidSize)) {
        $sz = [math]::Round((Get-Item $path).Length / 1MB)
        Write-Host "  [OK] $label ($sz MB)" -ForegroundColor Green
    } else {
        Write-Host "  [MISSING] $label" -ForegroundColor Red
        $allOk = $false
    }
}

Write-Host ""
if ($allOk) {
    Write-Host "All models ready." -ForegroundColor Green
} else {
    Write-Host "Some models are missing! Check errors above." -ForegroundColor Red
    exit 1
}
