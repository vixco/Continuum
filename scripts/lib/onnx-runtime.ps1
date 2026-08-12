# Shared ONNX Runtime discovery for Continuum's Windows development scripts.

$script:ContinuumMinimumOnnxRuntimeVersion = [version]"1.23"

function Get-ContinuumOnnxRuntimeVersion {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }

    $versionInfo = (Get-Item -LiteralPath $Path).VersionInfo
    if ($versionInfo.FileMajorPart -lt 0 -or $versionInfo.FileMinorPart -lt 0) {
        return $null
    }

    return [version]::new($versionInfo.FileMajorPart, $versionInfo.FileMinorPart)
}

function Resolve-ContinuumOnnxRuntime {
    param([Parameter(Mandatory = $true)][string]$RepoRoot)

    $candidates = [System.Collections.Generic.List[object]]::new()
    $configuredPath = $env:ORT_DYLIB_PATH
    if ($configuredPath) {
        if (Test-Path -LiteralPath $configuredPath -PathType Container) {
            $candidates.Add([pscustomobject]@{ Path = Join-Path $configuredPath "onnxruntime.dll"; Source = "ORT_DYLIB_PATH" })
            $candidates.Add([pscustomobject]@{ Path = Join-Path $configuredPath "lib\onnxruntime.dll"; Source = "ORT_DYLIB_PATH" })
        } else {
            $candidates.Add([pscustomobject]@{ Path = $configuredPath; Source = "ORT_DYLIB_PATH" })
        }
    }

    @(
        (Join-Path $RepoRoot ".deps\onnxruntime\onnxruntime.dll"),
        (Join-Path $RepoRoot ".deps\onnxruntime\lib\onnxruntime.dll"),
        (Join-Path $env:LOCALAPPDATA "Continuum\onnxruntime\onnxruntime.dll"),
        "C:\onnxruntime\onnxruntime.dll",
        "C:\onnxruntime\lib\onnxruntime.dll"
    ) | ForEach-Object {
        $candidates.Add([pscustomobject]@{ Path = $_; Source = "known location" })
    }

    $rejected = [System.Collections.Generic.List[string]]::new()
    $seen = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($candidate in $candidates) {
        $expandedPath = [Environment]::ExpandEnvironmentVariables($candidate.Path)
        if (-not $seen.Add($expandedPath) -or -not (Test-Path -LiteralPath $expandedPath -PathType Leaf)) {
            continue
        }

        $resolvedPath = (Resolve-Path -LiteralPath $expandedPath).Path
        $version = Get-ContinuumOnnxRuntimeVersion -Path $resolvedPath
        if ($version -and $version -ge $script:ContinuumMinimumOnnxRuntimeVersion) {
            return [pscustomobject]@{
                Path = $resolvedPath
                Version = $version
                Source = $candidate.Source
            }
        }

        $versionLabel = if ($version) { $version.ToString() } else { "unknown version" }
        $rejected.Add("$resolvedPath ($versionLabel)")
    }

    $systemDll = Join-Path $env:SystemRoot "System32\onnxruntime.dll"
    if (Test-Path -LiteralPath $systemDll -PathType Leaf) {
        $systemVersion = Get-ContinuumOnnxRuntimeVersion -Path $systemDll
        $systemVersionLabel = if ($systemVersion) { $systemVersion.ToString() } else { "unknown version" }
        $rejected.Add("$systemDll ($systemVersionLabel; Windows system copy)")
    }

    $detail = if ($rejected.Count -gt 0) { " Found incompatible: $($rejected -join ';')." } else { "" }
    throw "Compatible ONNX Runtime not found. Continuum requires onnxruntime.dll >= $script:ContinuumMinimumOnnxRuntimeVersion.$detail Set ORT_DYLIB_PATH to the full compatible DLL path or place it at C:\onnxruntime\onnxruntime.dll."
}

# Default pinned version. Pinned to a known-good release so a fresh checkout
# always gets the same binary the rest of the project was tested against.
# Bump in lockstep with the `ort` crate's expected ABI.
$script:ContinuumOnnxRuntimePinnedVersion = "1.23.0"

# Download and install a compatible onnxruntime.dll into a known location.
# Idempotent: if the destination already has a >=1.23 DLL, returns it as-is.
# Uses the official Microsoft release ZIP from GitHub (no auth, no telemetry).
function Install-ContinuumOnnxRuntime {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [string]$DestinationPath = "C:\onnxruntime\onnxruntime.dll"
    )

    $existing = Get-ContinuumOnnxRuntimeVersion -Path $DestinationPath
    if ($existing -and $existing -ge $script:ContinuumMinimumOnnxRuntimeVersion) {
        return [pscustomobject]@{
            Path = $DestinationPath
            Version = $existing
            Source = "already installed"
        }
    }

    $version = $script:ContinuumOnnxRuntimePinnedVersion
    $url = "https://github.com/microsoft/onnxruntime/releases/download/v$version/onnxruntime-win-x64-$version.zip"
    $zip = Join-Path $env:TEMP "onnxruntime-$version.zip"
    $extract = Join-Path $env:TEMP "onnxruntime-$version-extract"

    if (Test-Path $zip) { Remove-Item -LiteralPath $zip -Force }
    if (Test-Path $extract) { Remove-Item -LiteralPath $extract -Recurse -Force }

    Write-Host "  Downloading ONNX Runtime $version from Microsoft GitHub release..." -ForegroundColor Yellow
    # curl.exe ships with Windows 10+. We avoid Invoke-WebRequest because
    # GitHub releases redirect to S3 and PowerShell's redirect handling has
    # historically been flaky on older versions.
    & curl.exe -L --fail --silent --show-error -o $zip $url
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to download ONNX Runtime $version (curl exit $LASTEXITCODE). URL: $url"
    }

    Expand-Archive -Path $zip -DestinationPath $extract -Force
    $dll = Get-ChildItem -Path $extract -Filter "onnxruntime.dll" -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $dll) {
        throw "onnxruntime.dll not found inside the downloaded archive"
    }

    $destDir = Split-Path -LiteralPath $DestinationPath -Parent
    if (-not (Test-Path -LiteralPath $destDir)) {
        New-Item -ItemType Directory -Force -Path $destDir | Out-Null
    }
    Copy-Item -LiteralPath $dll.FullName -Destination $DestinationPath -Force

    # Clean up the working set so a re-run doesn't keep multiple copies of the
    # archive on disk.
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $extract -Recurse -Force -ErrorAction SilentlyContinue

    $installed = Get-ContinuumOnnxRuntimeVersion -Path $DestinationPath
    return [pscustomobject]@{
        Path = $DestinationPath
        Version = $installed
        Source = "installed from $url"
    }
}
