# Kairo download-models.ps1
# Downloads default model files for Kairo's local inference.
# Idempotent — skips files that already exist.

$ErrorActionPreference = "Stop"

Write-Host "Kairo Model Downloader" -ForegroundColor Cyan
Write-Host "======================" -ForegroundColor Cyan
Write-Host ""
Write-Host "TODO: Phase 1/2 will implement actual model downloads." -ForegroundColor Yellow
Write-Host ""
Write-Host "Models will be downloaded to:" -ForegroundColor Gray
Write-Host "  ~/.kairo/models/vision/   — Moondream 2 (ONNX, ~300 MB)"
Write-Host "  ~/.kairo/models/triage/   — Qwen 2.5 3B Q4_K_M (GGUF, ~2 GB)"
Write-Host "  ~/.kairo/models/stt/      — Whisper small (~244 MB)"
Write-Host "  ~/.kairo/models/tts/      — Piper en_US-lessac-medium (~75 MB)"
Write-Host ""

exit 0
