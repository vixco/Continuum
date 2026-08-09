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
