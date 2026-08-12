# Continuum download-models.ps1
# Downloads default model files for Continuum's local inference.
# Idempotent -- skips files that already exist with valid sizes.
# For the vision model only, use `scripts/ensure-vision-model.ps1`: its
# default Check mode is offline/read-only, while Repair and Update download
# only files that need attention.
#
# Uses curl.exe (ships with Windows 10+) instead of Invoke-WebRequest
# because HuggingFace requires following redirects (302 -> CDN) and
# Invoke-WebRequest handles this unreliably on older PowerShell versions.

$ErrorActionPreference = "Stop"

Write-Host "Continuum Model Downloader" -ForegroundColor Cyan
Write-Host "======================" -ForegroundColor Cyan
Write-Host ""

# Models base directory. The onboarding wizard passes a custom path via
# CONTINUUM_MODELS_DIR; fall back to ~/.continuum-dev/models when unset.
if ($env:CONTINUUM_MODELS_DIR -and $env:CONTINUUM_MODELS_DIR.Trim() -ne "") {
    $ModelsBase = $env:CONTINUUM_MODELS_DIR
} else {
    $ModelsBase = Join-Path $env:USERPROFILE ".continuum-dev\models"
}

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

# Small-file variant for sidecar configs (.onnx.json, tokenizer.json). These
# are KB-sized so the 1 MB validity floor doesn't apply -- we only check that
# the response is non-empty and does not look like an HTML error page.
function Download-Sidecar {
    param(
        [string]$Name,
        [string]$Url,
        [string]$OutPath
    )

    $dir = Split-Path $OutPath -Parent
    if (-not (Test-Path $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }

    if (Test-Path $OutPath) {
        $size = (Get-Item $OutPath).Length
        if ($size -gt 100) {
            Write-Host "[OK] $Name already exists ($size bytes)" -ForegroundColor Green
            return
        } else {
            Remove-Item $OutPath
        }
    }

    Write-Host "[DL] Downloading $Name..." -ForegroundColor Yellow
    & curl.exe -L --fail --silent -o $OutPath $Url

    if ($LASTEXITCODE -ne 0) {
        Write-Host "[FAIL] Download failed for $Name" -ForegroundColor Red
        if (Test-Path $OutPath) { Remove-Item $OutPath }
        return
    }

    if (Test-Path $OutPath) {
        $dlSize = (Get-Item $OutPath).Length
        $head = Get-Content $OutPath -TotalCount 1 -ErrorAction SilentlyContinue
        if ($dlSize -lt 100 -or ($head -like "<!DOCTYPE*") -or ($head -like "<html*")) {
            Write-Host "[FAIL] $Name response is too small or looks like an HTML error page" -ForegroundColor Red
            Remove-Item $OutPath
            return
        }
        Write-Host "[OK] $Name downloaded ($dlSize bytes)" -ForegroundColor Green
    }
}

# ============================================================================
# SmolVLM-500M (Vision -- Layer 1)
# ============================================================================
# Source: HuggingFaceTB official repo (onnx-community is now auth-gated).
# The continuum-vision crate expects: vision_encoder.onnx, embed_tokens.onnx,
# decoder.onnx, and tokenizer.json in the same directory.

$VisionEnsure = Join-Path $PSScriptRoot "ensure-vision-model.ps1"
Write-Host "`n--- SmolVLM2-2.2B Q4_K_M (Preferred Vision) ---" -ForegroundColor Cyan
& $VisionEnsure -Mode Repair -Variant smolvlm2-2.2b-q4 -ModelsDir $ModelsBase
if ($LASTEXITCODE -ne 0) { throw "Preferred SmolVLM2 vision model repair failed." }

Write-Host "`n--- SmolVLM-500M (Vision Fallback) ---" -ForegroundColor Cyan

$VisionDir = Join-Path $ModelsBase "vision\smolvlm-500m"
$HfVisionBase = "https://huggingface.co/HuggingFaceTB/SmolVLM-500M-Instruct/resolve/main"

Download-Model `
    -Name "SmolVLM vision encoder" `
    -Url "$HfVisionBase/onnx/vision_encoder.onnx" `
    -OutPath (Join-Path $VisionDir "vision_encoder.onnx") `
    -ExpectedSizeMB "393"

Download-Model `
    -Name "SmolVLM embed_tokens" `
    -Url "$HfVisionBase/onnx/embed_tokens.onnx" `
    -OutPath (Join-Path $VisionDir "embed_tokens.onnx") `
    -ExpectedSizeMB "189"

Download-Model `
    -Name "SmolVLM decoder" `
    -Url "$HfVisionBase/onnx/decoder_model_merged.onnx" `
    -OutPath (Join-Path $VisionDir "decoder.onnx") `
    -ExpectedSizeMB "1450"

Download-Model `
    -Name "SmolVLM tokenizer" `
    -Url "$HfVisionBase/tokenizer.json" `
    -OutPath (Join-Path $VisionDir "tokenizer.json") `
    -ExpectedSizeMB "3"

foreach ($sidecar in @(
    @("SmolVLM preprocessor config", "preprocessor_config.json"),
    @("SmolVLM processor config", "processor_config.json"),
    @("SmolVLM generation config", "generation_config.json"),
    @("SmolVLM chat template", "chat_template.json"),
    @("SmolVLM model config", "config.json")
)) {
    Download-Sidecar `
        -Name $sidecar[0] `
        -Url "$HfVisionBase/$($sidecar[1])" `
        -OutPath (Join-Path $VisionDir $sidecar[1])
}

# Clean up stale encoder.onnx if it exists (previous script saved 401 error as file)
$StaleEncoder = Join-Path $VisionDir "encoder.onnx"
if ((Test-Path $StaleEncoder) -and ((Get-Item $StaleEncoder).Length -lt $MinValidSize)) {
    Write-Host "[CLEAN] Removing stale encoder.onnx (29-byte error page)" -ForegroundColor Yellow
    Remove-Item $StaleEncoder
}

# ============================================================================
# Qwen 3 8B Q4_K_M (Triage LLM -- Layer 2, default)
# ============================================================================
# Source: Official Qwen org on HuggingFace (no auth required).
# 8B recommended for accuracy on Dutch + decision boundary classification.
# The wizard may override the URL via CONTINUUM_QWEN_URL so the user can pick a
# custom HuggingFace GGUF (e.g. a different quant or a community fine-tune).

Write-Host "`n--- Qwen 3 8B Q4_K_M (Triage, default) ---" -ForegroundColor Cyan

$Qwen8BOutName = "qwen3-8b-q4_k_m.gguf"
if ($env:CONTINUUM_QWEN_URL -and $env:CONTINUUM_QWEN_URL.Trim() -ne "") {
    $Qwen8BUrl = $env:CONTINUUM_QWEN_URL
    # Derive a filename from the URL's last path segment; fall back to a generic
    # name if the URL has no usable segment.
    $seg = Split-Path -Leaf $Qwen8BUrl
    if (-not $seg -or $seg -eq "") { $seg = "qwen3-8b-custom.gguf" }
    $Qwen8BOutName = $seg
    Write-Host "[INFO] Using custom Qwen URL: $Qwen8BUrl" -ForegroundColor Gray
} else {
    $Qwen8BUrl = "https://huggingface.co/Qwen/Qwen3-8B-GGUF/resolve/main/Qwen3-8B-Q4_K_M.gguf"
}

Download-Model `
    -Name "Qwen 3 8B Q4_K_M" `
    -Url $Qwen8BUrl `
    -OutPath (Join-Path $ModelsBase "triage\$Qwen8BOutName") `
    -ExpectedSizeMB "4800"

# ============================================================================
# Qwen 3 4B Q4_K_M (Triage LLM -- low-VRAM fallback)
# ============================================================================
# For GPUs with <6GB VRAM or CPU-only users. Lower accuracy on Dutch and
# decision boundaries compared to 8B.

Write-Host "`n--- Qwen 3 4B Q4_K_M (Triage, fallback) ---" -ForegroundColor Cyan

Download-Model `
    -Name "Qwen 3 4B Q4_K_M" `
    -Url "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf" `
    -OutPath (Join-Path $ModelsBase "triage\qwen3-4b-q4_k_m.gguf") `
    -ExpectedSizeMB "2500"

# ============================================================================
# Whisper medium (STT -- Layer 1 audio, default)
# ============================================================================
# medium (1.5 GB) is the default because whisper-small struggles with the
# wake word "Continuum" -- it isn't in the vocab and small hallucinates real
# English words around it ("You're one guy at all", "Hey, can I have one?").
# medium recognises uncommon proper nouns much more reliably, and the RTX
# 3060 runs it in ~200 ms per 3-second clip.
# Small is downloaded too so users can switch back via config if needed.

Write-Host "`n--- Whisper medium (STT, default) ---" -ForegroundColor Cyan

Download-Model `
    -Name "Whisper medium" `
    -Url "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin" `
    -OutPath (Join-Path $ModelsBase "stt\whisper-medium.bin") `
    -ExpectedSizeMB "1533"

Write-Host "`n--- Whisper small (STT, fallback) ---" -ForegroundColor Cyan

Download-Model `
    -Name "Whisper small" `
    -Url "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin" `
    -OutPath (Join-Path $ModelsBase "stt\whisper-small.bin") `
    -ExpectedSizeMB "465"

# ============================================================================
# Piper TTS voices (Phase 5 -- voice)
# ============================================================================
# Piper voices are shipped as paired .onnx + .onnx.json files. Config sidecar
# holds sample_rate, speaker table, inference params -- Continuum reads sample_rate
# from it at engine init, so both files are required together.
#
# Voice repository: MIT. Dataset-specific terms still apply; English Norman
# was trained from scratch from public-domain LibriVox recordings.

Write-Host "`n--- Piper TTS (English -- en_US-norman-medium) ---" -ForegroundColor Cyan

$TtsDir = Join-Path $ModelsBase "tts"
$PiperBase = "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0"

Download-Model `
    -Name "Piper EN voice (norman-medium)" `
    -Url "$PiperBase/en/en_US/norman/medium/en_US-norman-medium.onnx" `
    -OutPath (Join-Path $TtsDir "en_US-norman-medium.onnx") `
    -ExpectedSizeMB "63"

Download-Sidecar `
    -Name "Piper EN config" `
    -Url "$PiperBase/en/en_US/norman/medium/en_US-norman-medium.onnx.json" `
    -OutPath (Join-Path $TtsDir "en_US-norman-medium.onnx.json")

Write-Host "`n--- Piper TTS (Dutch -- nl_NL-mls-medium) ---" -ForegroundColor Cyan

Download-Model `
    -Name "Piper NL voice (mls-medium)" `
    -Url "$PiperBase/nl/nl_NL/mls/medium/nl_NL-mls-medium.onnx" `
    -OutPath (Join-Path $TtsDir "nl_NL-mls-medium.onnx") `
    -ExpectedSizeMB "76"

Download-Sidecar `
    -Name "Piper NL config" `
    -Url "$PiperBase/nl/nl_NL/mls/medium/nl_NL-mls-medium.onnx.json" `
    -OutPath (Join-Path $TtsDir "nl_NL-mls-medium.onnx.json")

# ============================================================================
# Piper binary + espeak-ng-data (Windows)
# ============================================================================
# The official rhasspy/piper Windows release zip (piper_windows_amd64.zip)
# bundles piper.exe alongside the espeak-ng-data directory. We extract the
# whole tree to ~/.continuum-dev/bin/piper/ and copy espeak-ng-data/ to the
# location Continuum's config expects.
#
# Both the binary and espeak-ng-data come from the same archive -- this
# guarantees version compatibility and avoids depending on the 404'd
# rhasspy/espeak-ng-data repo. The PiperEngine calls piper.exe as a
# subprocess and reads PIPER_ESPEAKNG_DATA_DIRECTORY from the environment
# (set by voice::tts::set_espeak_data_dir).

Write-Host "`n--- Piper binary + espeak-ng-data (Windows) ---" -ForegroundColor Cyan

$EspeakDir = Join-Path $TtsDir "espeak-ng-data"
$PiperZipUrl = "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_windows_amd64.zip"
$FallbackEspeakUrl = "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/espeak-ng-data.tar.bz2"
$PiperBinRoot = Join-Path $env:USERPROFILE ".continuum-dev\bin\piper"
$PiperExe = Join-Path $PiperBinRoot "piper.exe"

$NeedsPiper  = -not (Test-Path $PiperExe)
$NeedsEspeak = -not ((Test-Path $EspeakDir) -and ((Get-ChildItem $EspeakDir -Recurse -ErrorAction SilentlyContinue | Measure-Object).Count -gt 50))

if (-not $NeedsPiper -and -not $NeedsEspeak) {
    Write-Host "[OK] Piper binary and espeak-ng-data already installed" -ForegroundColor Green
    Write-Host "     piper.exe: $PiperExe" -ForegroundColor Gray
    Write-Host "     espeak-ng-data: $EspeakDir" -ForegroundColor Gray
} else {
    $zipPath = Join-Path $env:TEMP "continuum-piper-win-amd64.zip"
    $extractRoot = Join-Path $env:TEMP "continuum-piper-extract"

    Write-Host "[DL] Downloading Piper Windows release (~22 MB)..." -ForegroundColor Yellow
    Write-Host "     URL: $PiperZipUrl" -ForegroundColor Gray
    & curl.exe -L --fail --progress-bar -o $zipPath $PiperZipUrl

    if ($LASTEXITCODE -ne 0) {
        Write-Host "[FAIL] Could not download Piper Windows release." -ForegroundColor Red
        Write-Host "       Manual fallback:" -ForegroundColor Gray
        Write-Host "         1. Download $PiperZipUrl" -ForegroundColor Gray
        Write-Host "         2. Extract the 'piper' folder to $PiperBinRoot" -ForegroundColor Gray
        Write-Host "         3. Copy the 'espeak-ng-data' folder to $EspeakDir" -ForegroundColor Gray
    } else {
        if (Test-Path $extractRoot) { Remove-Item -Recurse -Force $extractRoot }
        New-Item -ItemType Directory -Force -Path $extractRoot | Out-Null

        Write-Host "[EX] Extracting..." -ForegroundColor Yellow
        Expand-Archive -Path $zipPath -DestinationPath $extractRoot -Force

        # The archive structure is piper/piper.exe + piper/espeak-ng-data/ +
        # piper/*.dll. Find piper.exe recursively so we don't depend on the
        # exact nesting changing between releases.
        $foundExe = Get-ChildItem -Path $extractRoot -Filter "piper.exe" -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -eq $foundExe) {
            Write-Host "[FAIL] piper.exe not found inside archive" -ForegroundColor Red
        } else {
            $piperSrcDir = $foundExe.Directory.FullName
            $espeakSrc   = Join-Path $piperSrcDir "espeak-ng-data"

            # Install the Piper binary tree
            if (Test-Path $PiperBinRoot) { Remove-Item -Recurse -Force $PiperBinRoot }
            New-Item -ItemType Directory -Force -Path $PiperBinRoot | Out-Null
            Copy-Item -Path (Join-Path $piperSrcDir "*") -Destination $PiperBinRoot -Recurse -Force
            Write-Host "[OK] Piper binary installed at $PiperBinRoot" -ForegroundColor Green
            Write-Host "     Set CONTINUUM_PIPER_BIN=$PiperExe or add $PiperBinRoot to PATH" -ForegroundColor Gray

            # Copy espeak-ng-data to the location the Continuum config expects
            if (Test-Path $espeakSrc) {
                if (Test-Path $EspeakDir) { Remove-Item -Recurse -Force $EspeakDir }
                New-Item -ItemType Directory -Force -Path $EspeakDir | Out-Null
                Copy-Item -Path (Join-Path $espeakSrc "*") -Destination $EspeakDir -Recurse -Force
                Write-Host "[OK] espeak-ng-data installed at $EspeakDir" -ForegroundColor Green
            } else {
                Write-Host "[WARN] espeak-ng-data not found alongside piper.exe" -ForegroundColor Yellow
                Write-Host "       Fallback: $FallbackEspeakUrl" -ForegroundColor Gray
            }
        }

        Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
        Remove-Item $extractRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# ============================================================================
# Kokoros TTS (optional, higher-quality local alternative to Piper)
# ============================================================================
# Kokoros (Kokoro-82M) is an optional TTS engine selected with
# `tts.engine = "kokoros"`. It is NOT required -- Piper remains the default.
# We download the ONNX model + voices catalog so the engine is ready if the
# user switches to it. The `koko` CLI binary has no official Windows prebuilt,
# so we only check for it and print build instructions when missing (see
# voice::tts::resolve_koko_binary for the resolution order).

Write-Host "`n--- Kokoros TTS (optional -- Kokoro-82M ONNX) ---" -ForegroundColor Cyan

$KokoroDir = Join-Path $TtsDir "kokoro"
$KokoroOnnxBase = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0"

Download-Model `
    -Name "Kokoro ONNX model" `
    -Url "$KokoroOnnxBase/kokoro-v1.0.onnx" `
    -OutPath (Join-Path $KokoroDir "kokoro-v1.0.onnx") `
    -ExpectedSizeMB 100

Download-Model `
    -Name "Kokoro voices catalog" `
    -Url "$KokoroOnnxBase/voices-v1.0.bin" `
    -OutPath (Join-Path $KokoroDir "voices-v1.0.bin") `
    -ExpectedSizeMB 60

# `koko` binary -- manual step. No official Windows prebuilt exists as of
# 2026-08. Build it from the Kokoros repo (requires libopus for the opus
# encoder used by the `openai` subcommand; the `stream`/`text` modes we use
# output WAV and do not need opus at runtime, but the crate may still require
# the headers to compile).
$KokorosBinRoot = Join-Path $env:USERPROFILE ".continuum-dev\bin\kokoros"
$KokoExe = Join-Path $KokorosBinRoot "koko.exe"
if (Test-Path $KokoExe) {
    Write-Host "[OK] koko binary already installed at $KokoExe" -ForegroundColor Green
} else {
    Write-Host "[INFO] koko binary not found (optional -- Piper is the default TTS)." -ForegroundColor Yellow
    Write-Host "       To enable Kokoros TTS, build the 'koko' binary from" -ForegroundColor Gray
    Write-Host "       https://github.com/lucasjinreal/Kokoros and copy koko.exe to:" -ForegroundColor Gray
    Write-Host "         $KokorosBinRoot" -ForegroundColor Gray
    Write-Host "       or set CONTINUUM_KOKO_BIN to its location." -ForegroundColor Gray
    Write-Host "       Build: git clone the repo, then 'cargo build --release --bin koko'" -ForegroundColor Gray
    Write-Host "       (Windows may need libopus headers for the opus encoder crate.)" -ForegroundColor Gray
}

# ============================================================================
# Moshi S2S front-end (optional, full-duplex voice)
# ============================================================================
# Moshi (Kyutai) is an optional full-duplex speech-to-speech front-end,
# selected with `voice.frontend.mode = "moshi"` + the `moshi` cargo feature.
# It is NOT required -- the pipeline (whisper -> triage -> orchestrator -> TTS)
# is the default. Moshi needs a CUDA-built `moshi-backend.exe` and (for audio)
# libopus + the `moshi-opus` cargo feature. There is no prebuilt Windows
# binary; this section only checks for it and prints build instructions.

Write-Host "`n--- Moshi S2S front-end (optional -- needs CUDA) ---" -ForegroundColor Cyan

$MoshiBinRoot = Join-Path $env:USERPROFILE ".continuum-dev\bin\moshi"
$MoshiBackendExe = Join-Path $MoshiBinRoot "moshi-backend.exe"
$MoshiModelDir = Join-Path $env:USERPROFILE ".continuum-dev\models\moshi\moshiko-candle-q8"

if (Test-Path $MoshiBackendExe) {
    Write-Host "[OK] moshi-backend already installed at $MoshiBackendExe" -ForegroundColor Green
} else {
    Write-Host "[INFO] moshi-backend.exe not found (optional -- pipeline voice is the default)." -ForegroundColor Yellow
    Write-Host "       To enable Moshi S2S voice:" -ForegroundColor Gray
    Write-Host "         1. Build the Kyutai moshi-backend with CUDA:" -ForegroundColor Gray
    Write-Host "            git clone https://github.com/kyutai-labs/moshi" -ForegroundColor Gray
    Write-Host "            cd moshi/rust && cargo build --release --bin moshi-backend --features cuda" -ForegroundColor Gray
    Write-Host "         2. Copy moshi-backend.exe to:" -ForegroundColor Gray
    Write-Host "            $MoshiBinRoot" -ForegroundColor Gray
    Write-Host "            (or set CONTINUUM_MOSHI_BIN to its full path)" -ForegroundColor Gray
    Write-Host "         3. On first run the backend auto-downloads the model files" -ForegroundColor Gray
    Write-Host "            from kyutai/moshiko-candle-q8 into $MoshiModelDir" -ForegroundColor Gray
    Write-Host "         4. Build Continuum with: cargo build --release --features moshi" -ForegroundColor Gray
    Write-Host "            For realtime audio (talk + listen) add the moshi-opus feature" -ForegroundColor Gray
    Write-Host "            AND install libopus, then build with --features moshi,moshi-opus:" -ForegroundColor Gray
    Write-Host "              vcpkg install opus:x64-windows" -ForegroundColor Gray
    Write-Host "              (or set OPUS_LIB_DIR to a prebuilt opus.lib + headers dir)" -ForegroundColor Gray
    Write-Host "            Without moshi-opus, Moshi connects and the assistant text" -ForegroundColor Gray
    Write-Host "            transcript flows, but mic/assistant audio I/O is disabled." -ForegroundColor Gray
    Write-Host "         5. Set voice.frontend.mode = 'moshi' in config.toml" -ForegroundColor Gray
    Write-Host "       Windows support is community-grade; expect upstream fixes." -ForegroundColor Gray
}

# ============================================================================
# Summary
# ============================================================================

Write-Host "`n============================================" -ForegroundColor Cyan
Write-Host "Model directory: $ModelsBase" -ForegroundColor Gray

# Verify all critical files are present. Each entry is
# (label, path, min-size-bytes). Model files use the default 1 MB floor;
# piper.exe is ~500 KB so we relax its threshold to 100 KB.
$critical = @(
    @("Vision encoder",         (Join-Path $VisionDir "vision_encoder.onnx"),      $MinValidSize),
    @("Vision embed_tokens",    (Join-Path $VisionDir "embed_tokens.onnx"),        $MinValidSize),
    @("Vision decoder",         (Join-Path $VisionDir "decoder.onnx"),             $MinValidSize),
    @("Vision tokenizer",       (Join-Path $VisionDir "tokenizer.json"),           10000),
    @("Vision preprocessor",    (Join-Path $VisionDir "preprocessor_config.json"), 100),
    @("Vision processor",       (Join-Path $VisionDir "processor_config.json"),     50),
    @("Vision generation",      (Join-Path $VisionDir "generation_config.json"),   100),
    @("Vision chat template",   (Join-Path $VisionDir "chat_template.json"),       100),
    @("Vision model config",    (Join-Path $VisionDir "config.json"),             1000),
    @("Triage model (8B)",      (Join-Path $ModelsBase "triage\$Qwen8BOutName"),       $MinValidSize),
    @("Triage model (4B fallback)", (Join-Path $ModelsBase "triage\qwen3-4b-q4_k_m.gguf"), $MinValidSize),
    @("Whisper medium",         (Join-Path $ModelsBase "stt\whisper-medium.bin"),  $MinValidSize),
    @("Whisper small (fallback)", (Join-Path $ModelsBase "stt\whisper-small.bin"), $MinValidSize),
    @("Piper EN voice",         (Join-Path $TtsDir "en_US-norman-medium.onnx"),    $MinValidSize),
    @("Piper NL voice",         (Join-Path $TtsDir "nl_NL-mls-medium.onnx"),       $MinValidSize),
    @("Piper binary",           $PiperExe,                                         100000),
    @("espeak-ng-data",         (Join-Path $EspeakDir "phontab"),                  1)
)

$allOk = $true
foreach ($item in $critical) {
    $label   = $item[0]
    $path    = $item[1]
    $minSize = $item[2]
    if ((Test-Path $path) -and ((Get-Item $path).Length -ge $minSize)) {
        $bytes = (Get-Item $path).Length
        $display = if ($bytes -ge 1MB) {
            "$([math]::Round($bytes / 1MB)) MB"
        } else {
            "$([math]::Round($bytes / 1KB)) KB"
        }
        Write-Host "  [OK] $label ($display)" -ForegroundColor Green
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
