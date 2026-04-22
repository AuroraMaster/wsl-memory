param(
    [string]$ReleaseBase = $env:WSL_MEMORY_RELEASE_BASE,
    [string]$HostUrl = $env:WSL_MEMORY_HOST_URL,
    [string]$InstallDir = "$env:ProgramFiles\WSLMemoryAgent",
    [string]$Listen = "0.0.0.0:15555",
    [string]$TokenPath = "C:\Users\Public\wsl_agent_token",
    [switch]$NoStart
)

$ErrorActionPreference = "Stop"

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this command from an elevated PowerShell session."
    }
}

function New-TokenIfMissing([string]$Path) {
    if ((Test-Path -LiteralPath $Path) -and ((Get-Content -Raw -LiteralPath $Path).Trim().Length -gt 0)) {
        return
    }
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $bytes = [byte[]]::new(32)
    [Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
    [Convert]::ToBase64String($bytes).TrimEnd("=") | Set-Content -NoNewline -Encoding ascii -LiteralPath $Path
}

Assert-Administrator

if (-not $HostUrl) {
    if (-not $ReleaseBase) {
        $ReleaseBase = "https://github.com/wsl-memory-agent/wsl-memory-agent/releases/latest/download"
    }
    $HostUrl = "$ReleaseBase/host.exe"
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$HostExe = Join-Path $InstallDir "host.exe"

$svc = Get-Service -Name WSLMemoryHost -ErrorAction SilentlyContinue
if ($svc) {
    if ($svc.Status -ne "Stopped") {
        Stop-Service -Name WSLMemoryHost -Force -ErrorAction SilentlyContinue
        $svc.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(20))
    }
    if (Test-Path -LiteralPath $HostExe) {
        & $HostExe --uninstall 2>$null | Out-Null
    } else {
        sc.exe delete WSLMemoryHost | Out-Null
    }
    Start-Sleep -Seconds 2
}

Invoke-WebRequest -Uri $HostUrl -OutFile $HostExe
New-TokenIfMissing $TokenPath

$configDir = Join-Path $env:APPDATA "WSLMemoryAgent"
New-Item -ItemType Directory -Force -Path $configDir | Out-Null
$YamlTokenPath = $TokenPath.Replace("'", "''")
@"
listen_addr: "$Listen"
token_path: '$YamlTokenPath'
"@ | Set-Content -Encoding utf8 -LiteralPath (Join-Path $configDir "config.yaml")

if ($NoStart) {
    Write-Host "Installed host binary at $HostExe"
    Write-Host "Token path: $TokenPath"
    exit 0
}

& $HostExe --install

Write-Host ""
Write-Host "WSL Memory Host is installed and running."
Write-Host "Now run the WSL command from inside each WSL distro you want to manage."
Write-Host "Token path shared with WSL: $TokenPath"
