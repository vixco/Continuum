# Continuum vision model preflight, repair, and update helper.
#
# This script deliberately manages only the model and metadata files consumed by
# `continuum-vision`. `Check` is read-only and never accesses the network.
# `Repair` downloads only missing or clearly incomplete files. `Update`
# compares the remote Hugging Face ETag with the local manifest and downloads
# only files whose source revision changed.

[CmdletBinding()]
param(
    [ValidateSet("Check", "Repair", "Update")]
    [string]$Mode = "Check",
    [ValidateSet("smolvlm-256m", "smolvlm-500m", "smolvlm2-2.2b-q4")]
    [string]$Variant = "smolvlm-500m",
    [string]$ModelsDir = $env:CONTINUUM_MODELS_DIR
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($ModelsDir)) {
    $ModelsDir = Join-Path $env:USERPROFILE ".continuum-dev\models"
}

$VisionDir = Join-Path $ModelsDir "vision\$Variant"
$ManifestPath = Join-Path $VisionDir "continuum-vision-manifest.json"
$Repository = switch ($Variant) {
    "smolvlm-500m" { "HuggingFaceTB/SmolVLM-500M-Instruct" }
    "smolvlm-256m" { "HuggingFaceTB/SmolVLM-256M-Instruct" }
    "smolvlm2-2.2b-q4" { "ggml-org/SmolVLM2-2.2B-Instruct-GGUF" }
}
$SourceBase = "https://huggingface.co/$Repository/resolve/main"
$Models = if ($Variant -eq "smolvlm2-2.2b-q4") {
    @(
        [PSCustomObject]@{
            Name = "Q4_K_M language model"
            File = "model-q4_k_m.gguf"
            RemotePath = "SmolVLM2-2.2B-Instruct-Q4_K_M.gguf"
            MinimumBytes = 1000MB
        },
        [PSCustomObject]@{
            Name = "FP16 multimodal projector"
            File = "mmproj-f16.gguf"
            RemotePath = "mmproj-SmolVLM2-2.2B-Instruct-f16.gguf"
            MinimumBytes = 800MB
        }
    )
} else {
    $minimumModelBytes = if ($Variant -eq "smolvlm-500m") {
        @{ Encoder = 350MB; Embed = 170MB; Decoder = 1300MB }
    } else {
        @{ Encoder = 330MB; Embed = 100MB; Decoder = 480MB }
    }
    @(
        [PSCustomObject]@{ Name = "vision encoder"; File = "vision_encoder.onnx"; RemotePath = "onnx/vision_encoder.onnx"; MinimumBytes = $minimumModelBytes.Encoder },
        [PSCustomObject]@{ Name = "token embedder"; File = "embed_tokens.onnx"; RemotePath = "onnx/embed_tokens.onnx"; MinimumBytes = $minimumModelBytes.Embed },
        [PSCustomObject]@{ Name = "text decoder"; File = "decoder.onnx"; RemotePath = "onnx/decoder_model_merged.onnx"; MinimumBytes = $minimumModelBytes.Decoder },
        [PSCustomObject]@{ Name = "tokenizer"; File = "tokenizer.json"; RemotePath = "tokenizer.json"; MinimumBytes = 10KB },
        [PSCustomObject]@{ Name = "preprocessor config"; File = "preprocessor_config.json"; RemotePath = "preprocessor_config.json"; MinimumBytes = 100 },
        [PSCustomObject]@{ Name = "processor config"; File = "processor_config.json"; RemotePath = "processor_config.json"; MinimumBytes = 50 },
        [PSCustomObject]@{ Name = "generation config"; File = "generation_config.json"; RemotePath = "generation_config.json"; MinimumBytes = 100 },
        [PSCustomObject]@{ Name = "chat template"; File = "chat_template.json"; RemotePath = "chat_template.json"; MinimumBytes = 100 },
        [PSCustomObject]@{ Name = "model config"; File = "config.json"; RemotePath = "config.json"; MinimumBytes = 1KB }
    )
}

function Get-LocalStatus {
    param([Parameter(Mandatory = $true)]$Model)

    $path = Join-Path $VisionDir $Model.File
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return [PSCustomObject]@{ Ready = $false; Reason = "missing"; Path = $path; Bytes = 0 }
    }

    $bytes = (Get-Item -LiteralPath $path).Length
    if ($bytes -lt $Model.MinimumBytes) {
        return [PSCustomObject]@{ Ready = $false; Reason = "too small ($bytes bytes; expected at least $($Model.MinimumBytes))"; Path = $path; Bytes = $bytes }
    }

    return [PSCustomObject]@{ Ready = $true; Reason = "ready"; Path = $path; Bytes = $bytes }
}

function Get-RemoteMetadata {
    param([Parameter(Mandatory = $true)]$Model)

    $url = "$SourceBase/$($Model.RemotePath)"
    $headers = & curl.exe -sSIL --fail --proto '=https' --tlsv1.2 $url
    if ($LASTEXITCODE -ne 0) {
        throw "Could not check the remote version for $($Model.Name)."
    }

    $etag = @($headers | Where-Object { $_ -match '^etag:\s*(.+)$' } |
        ForEach-Object { $Matches[1].Trim() } | Select-Object -Last 1)[0]
    if ([string]::IsNullOrWhiteSpace($etag)) {
        throw "The remote version check for $($Model.Name) did not return an ETag."
    }

    return [PSCustomObject]@{ Url = $url; Etag = $etag }
}

function Save-Manifest {
    param([Parameter(Mandatory = $true)][hashtable]$Entries)

    $manifest = [PSCustomObject]@{
        format_version = 1
        checked_at_utc = (Get-Date).ToUniversalTime().ToString("o")
        source = $Repository
        files = $Entries
    }
    $temporary = "$ManifestPath.partial"
    $manifest | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $temporary -Encoding utf8
    Move-Item -LiteralPath $temporary -Destination $ManifestPath -Force
}

function Download-Model {
    param(
        [Parameter(Mandatory = $true)]$Model,
        [Parameter(Mandatory = $true)]$Remote
    )

    New-Item -ItemType Directory -Force -Path $VisionDir | Out-Null
    $destination = Join-Path $VisionDir $Model.File
    $temporary = "$destination.partial"
    Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue

    Write-Host "[DOWNLOAD] $($Model.Name)" -ForegroundColor Yellow
    & curl.exe -L --fail --proto '=https' --tlsv1.2 --progress-bar -o $temporary $Remote.Url
    if ($LASTEXITCODE -ne 0) {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        throw "Download failed for $($Model.Name)."
    }

    $downloaded = Get-LocalStatus ([PSCustomObject]@{ Name = $Model.Name; File = "$($Model.File).partial"; MinimumBytes = $Model.MinimumBytes })
    if (-not $downloaded.Ready) {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        throw "Downloaded $($Model.Name) is invalid: $($downloaded.Reason)."
    }

    Move-Item -LiteralPath $temporary -Destination $destination -Force
}

$invalid = @()
foreach ($model in $Models) {
    $status = Get-LocalStatus $model
    if ($status.Ready) {
        Write-Host "[OK] $($model.Name) ($([math]::Round($status.Bytes / 1MB, 1)) MB)" -ForegroundColor Green
    } else {
        Write-Host "[NEEDS REPAIR] $($model.Name): $($status.Reason)" -ForegroundColor Yellow
        $invalid += $model
    }
}

if ($Mode -eq "Check") {
    if ($invalid.Count -gt 0) {
        Write-Host "Run: .\scripts\ensure-vision-model.ps1 -Mode Repair -Variant $Variant" -ForegroundColor Yellow
        exit 1
    }
    Write-Host "Vision model is present. To check for upstream updates, run with -Mode Update." -ForegroundColor Green
    exit 0
}

if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue)) {
    throw "curl.exe is required to repair or update the vision model. Install Windows curl or add it to PATH."
}

$manifest = $null
if (Test-Path -LiteralPath $ManifestPath) {
    try { $manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json }
    catch { Write-Warning "Ignoring unreadable vision manifest; it will be replaced." }
}

$entries = @{}
foreach ($model in $Models) {
    $remote = Get-RemoteMetadata $model
    $existing = if ($manifest -and $manifest.files) { $manifest.files.($model.File) } else { $null }
    $local = Get-LocalStatus $model
    $needsDownload = (-not $local.Ready) -or ($Mode -eq "Update" -and $existing.etag -ne $remote.Etag)

    if ($needsDownload) { Download-Model -Model $model -Remote $remote }
    else { Write-Host "[CURRENT] $($model.Name)" -ForegroundColor Green }

    $entries[$model.File] = [PSCustomObject]@{
        etag = $remote.Etag
        source_url = $remote.Url
        bytes = (Get-Item -LiteralPath (Join-Path $VisionDir $model.File)).Length
    }
}

Save-Manifest $entries
Write-Host "Vision model $($Mode.ToLowerInvariant()) completed: $VisionDir" -ForegroundColor Green
