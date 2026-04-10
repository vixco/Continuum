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

# --- Qwen 3 4B Q4_K_M (Triage LLM) ---
$TriageDir = Join-Path $ModelsBase "triage"
$TriagePath = Join-Path $TriageDir "qwen3-4b-q4_k_m.gguf"
$TriageSha256 = "" # TODO: fill after first download verification

if (Test-Path $TriagePath) {
    $fileSize = (Get-Item $TriagePath).Length
    if ($fileSize -gt 2000000000) {
        Write-Host "[OK] Qwen 3 4B Q4_K_M already exists ($([math]::Round($fileSize / 1MB)) MB)" -ForegroundColor Green
    } else {
        Write-Host "[WARN] Qwen 3 4B file exists but seems too small ($([math]::Round($fileSize / 1MB)) MB), re-downloading..." -ForegroundColor Yellow
        Remove-Item $TriagePath
    }
}

if (-not (Test-Path $TriagePath)) {
    Write-Host "[DL] Downloading Qwen 3 4B Q4_K_M GGUF (~2.5 GB)..." -ForegroundColor Yellow
    Write-Host "     This is the triage model. It will take a few minutes." -ForegroundColor Gray
    New-Item -ItemType Directory -Force -Path $TriageDir | Out-Null

    # Bartowski quantization from HuggingFace
    $TriageUrl = "https://huggingface.co/bartowski/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf"

    try {
        Invoke-WebRequest -Uri $TriageUrl -OutFile $TriagePath -UseBasicParsing
        $dlSize = (Get-Item $TriagePath).Length
        Write-Host "[OK] Qwen 3 4B downloaded to $TriagePath ($([math]::Round($dlSize / 1MB)) MB)" -ForegroundColor Green
    } catch {
        Write-Host "[WARN] Failed to download Qwen 3 4B: $_" -ForegroundColor Red
        Write-Host "       You can download it manually from:" -ForegroundColor Gray
        Write-Host "       $TriageUrl" -ForegroundColor Gray
        Write-Host "       Save to: $TriagePath" -ForegroundColor Gray
    }
}

Write-Host ""
Write-Host "Model directory: $ModelsBase" -ForegroundColor Gray
Write-Host ""
Write-Host "Done." -ForegroundColor Cyan
