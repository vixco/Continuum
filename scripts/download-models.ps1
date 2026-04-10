# Kairo download-models.ps1
# Downloads default model files for Kairo's local inference.
# Idempotent — skips files that already exist.

$ErrorActionPreference = "Stop"

Write-Host "Kairo Model Downloader" -ForegroundColor Cyan
Write-Host "======================" -ForegroundColor Cyan
Write-Host ""

$ModelsBase = Join-Path $env:USERPROFILE ".kairo-dev\models"

# --- SmolVLM-256M (Vision) ---
$VisionDir = Join-Path $ModelsBase "vision\smolvlm-256m"
$EncoderPath = Join-Path $VisionDir "encoder.onnx"

if (Test-Path $EncoderPath) {
    Write-Host "[OK] SmolVLM-256M encoder already exists" -ForegroundColor Green
} else {
    Write-Host "[DL] Downloading SmolVLM-256M ONNX encoder (~200 MB)..." -ForegroundColor Yellow
    New-Item -ItemType Directory -Force -Path $VisionDir | Out-Null

    # HuggingFace ONNX model repository for SmolVLM-256M
    $VisionUrl = "https://huggingface.co/onnx-community/SmolVLM-256M-Instruct/resolve/main/onnx/vision_encoder.onnx"

    try {
        Invoke-WebRequest -Uri $VisionUrl -OutFile $EncoderPath -UseBasicParsing
        Write-Host "[OK] SmolVLM-256M encoder downloaded to $EncoderPath" -ForegroundColor Green
    } catch {
        Write-Host "[WARN] Failed to download SmolVLM-256M encoder: $_" -ForegroundColor Red
        Write-Host "       You can download it manually from:" -ForegroundColor Gray
        Write-Host "       $VisionUrl" -ForegroundColor Gray
        Write-Host "       Save to: $EncoderPath" -ForegroundColor Gray
    }
}

# --- Whisper small (STT) ---
$SttDir = Join-Path $ModelsBase "stt"
$WhisperPath = Join-Path $SttDir "whisper-small.bin"

if (Test-Path $WhisperPath) {
    Write-Host "[OK] Whisper small model already exists" -ForegroundColor Green
} else {
    Write-Host "[DL] Downloading Whisper small model (~244 MB)..." -ForegroundColor Yellow
    New-Item -ItemType Directory -Force -Path $SttDir | Out-Null

    # ggml-format whisper model from HuggingFace
    $WhisperUrl = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"

    try {
        Invoke-WebRequest -Uri $WhisperUrl -OutFile $WhisperPath -UseBasicParsing
        Write-Host "[OK] Whisper small model downloaded to $WhisperPath" -ForegroundColor Green
    } catch {
        Write-Host "[WARN] Failed to download Whisper model: $_" -ForegroundColor Red
        Write-Host "       You can download it manually from:" -ForegroundColor Gray
        Write-Host "       $WhisperUrl" -ForegroundColor Gray
        Write-Host "       Save to: $WhisperPath" -ForegroundColor Gray
    }
}

Write-Host ""
Write-Host "Model directory: $ModelsBase" -ForegroundColor Gray
Write-Host ""
Write-Host "Done." -ForegroundColor Cyan
