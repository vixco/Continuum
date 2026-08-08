[CmdletBinding()]
param(
    [string]$DataDir = $(
        if ($env:CONTINUUM_DATA_DIR) { $env:CONTINUUM_DATA_DIR }
        elseif ($env:KAIRO_DATA_DIR) { $env:KAIRO_DATA_DIR }
        else { Join-Path $HOME '.continuum-dev' }
    ),
    [string]$ComposioUserId = $env:USERNAME,
    [string[]]$EnabledToolkits = @(),
    [Security.SecureString]$ComposioApiKey,
    [switch]$SkipBuild,
    [switch]$SkipComposio
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$onWindows = $env:OS -eq 'Windows_NT'
if (-not $onWindows) {
    throw 'Continuum computer use currently targets Windows. Run this installer on the Windows Continuum machine.'
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$DataDir = [System.IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($DataDir))
$agentRoot = Join-Path $DataDir 'agent-os'
$binDir = Join-Path $DataDir 'bin'
$registryDir = Join-Path $DataDir 'mcp-servers'
$destination = Join-Path $binDir 'continuum-agent-os.exe'

New-Item -ItemType Directory -Force -Path $agentRoot, $binDir, $registryDir | Out-Null

if (-not $SkipBuild) {
    Push-Location $repoRoot
    try {
        & cargo build --release -p continuum-mcp --bin continuum-agent-os
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    }
    finally {
        Pop-Location
    }
}

$builtBinary = Join-Path $repoRoot 'target\release\continuum-agent-os.exe'
if (-not (Test-Path -LiteralPath $builtBinary -PathType Leaf)) {
    throw "Agent OS binary was not found at $builtBinary. Build it first or remove -SkipBuild."
}
Copy-Item -LiteralPath $builtBinary -Destination $destination -Force

# Continuum's existing MCP registry consumes one JSON file per local stdio
# server. The registration passes no secret material to the process.
$registration = [ordered]@{
    name = 'agent-os'
    command = $destination
    args = @('--data-dir', $DataDir)
}
$registrationPath = Join-Path $registryDir 'agent-os.json'
$registrationTemp = Join-Path $registryDir ('.agent-os-{0}.tmp' -f [Guid]::NewGuid().ToString('N'))
$registration | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $registrationTemp -Encoding UTF8
Move-Item -LiteralPath $registrationTemp -Destination $registrationPath -Force

if (-not $SkipComposio) {
    if ([string]::IsNullOrWhiteSpace($ComposioUserId)) {
        $ComposioUserId = Read-Host 'Composio user id (a stable local id or email)'
    }
    if ([string]::IsNullOrWhiteSpace($ComposioUserId)) {
        throw 'ComposioUserId cannot be empty. Use -SkipComposio to install computer use only.'
    }
    if ($null -eq $ComposioApiKey) {
        $ComposioApiKey = Read-Host 'Composio project API key (stored with Windows DPAPI)' -AsSecureString
    }
    if ($null -eq $ComposioApiKey) {
        throw 'A Composio API key is required unless -SkipComposio is set.'
    }

    # ConvertFrom-SecureString without an explicit key uses Windows DPAPI and
    # binds the ciphertext to the current Windows user on this machine.
    $encrypted = ConvertFrom-SecureString -SecureString $ComposioApiKey
    if ([string]::IsNullOrWhiteSpace($encrypted)) {
        throw 'The supplied Composio API key was empty.'
    }
    $keyPath = Join-Path $agentRoot 'composio-api-key.dpapi'
    Set-Content -LiteralPath $keyPath -Value $encrypted -Encoding ASCII -NoNewline
    try {
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        & icacls.exe $keyPath /inheritance:r /grant:r "${identity}:(R,W)" | Out-Null
    }
    catch {
        Write-Warning "Could not tighten the key-file ACL; the value is still DPAPI-encrypted. $($_.Exception.Message)"
    }

    $toolkits = @(
        $EnabledToolkits |
            ForEach-Object { $_.Trim().ToLowerInvariant() } |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
            Sort-Object -Unique
    )
    $composioConfig = [ordered]@{
        version = 1
        base_url = 'https://backend.composio.dev'
        user_id = $ComposioUserId.Trim()
        enabled_toolkits = $toolkits
        session_id = $null
        session_mcp_url = $null
        updated_at = [DateTime]::UtcNow.ToString('o')
    }
    $composioPath = Join-Path $agentRoot 'composio.json'
    $composioTemp = Join-Path $agentRoot ('.composio-{0}.tmp' -f [Guid]::NewGuid().ToString('N'))
    $composioConfig | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $composioTemp -Encoding UTF8
    Move-Item -LiteralPath $composioTemp -Destination $composioPath -Force
}


# Plans and evidence can contain user-approved action arguments. Restrict the
# full Agent OS state tree to the current Windows user and SYSTEM.
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    & icacls.exe $agentRoot /inheritance:r /grant:r "${identity}:(OI)(CI)(F)" "SYSTEM:(OI)(CI)(F)" /T | Out-Null
}
catch {
    Write-Warning "Could not tighten the Agent OS state-directory ACL. $($_.Exception.Message)"
}

$version = & $destination --version
if ($LASTEXITCODE -ne 0) { throw 'The installed Agent OS binary failed its version smoke test.' }

Write-Host ''
Write-Host "Installed $version" -ForegroundColor Green
Write-Host "MCP registration: $registrationPath"
Write-Host "Policy and evidence: $agentRoot"
if ($SkipComposio) {
    Write-Host 'Composio was skipped; computer-use tools are still installed.' -ForegroundColor Yellow
} else {
    Write-Host 'Composio credentials are DPAPI-protected for the current Windows user.'
}
Write-Host 'Restart Continuum so the next agent run loads the agent-os MCP server.' -ForegroundColor Cyan
