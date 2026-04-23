param(
    [string]$ReleaseBase = $env:WSL_MEMORY_RELEASE_BASE,
    [string]$HostUrl = $env:WSL_MEMORY_HOST_URL,
    [string]$InstallDir = "$env:ProgramFiles\WSLMemoryAgent",
    [string]$Listen = "",
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

function Test-PortAvailable([int]$Port) {
    $tcp = $null
    $udp = $null
    try {
        $tcp = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, $Port)
        $tcp.Start()
        $udp = [System.Net.Sockets.UdpClient]::new($Port)
        return $true
    } catch {
        return $false
    } finally {
        if ($tcp) { $tcp.Stop() }
        if ($udp) { $udp.Dispose() }
    }
}

function Select-ListenPort {
    foreach ($port in @(15555, 15556, 25555, 35555, 45555, 5555)) {
        if (Test-PortAvailable $port) { return $port }
    }
    return 15555
}

function Install-StartupTask([string]$ExePath, [string]$InstallDir) {
    $taskName = "WSLMemoryHost"
    $runner = Join-Path $InstallDir "run-host.ps1"
    @"
`$ErrorActionPreference = 'Stop'
& '$ExePath'
"@ | Set-Content -Encoding utf8 -LiteralPath $runner

    schtasks.exe /Delete /TN $taskName /F 2>$null | Out-Null
    $taskRun = "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$runner`""
    schtasks.exe /Create /TN $taskName /SC ONLOGON /RL HIGHEST /TR $taskRun /F | Out-Null
    schtasks.exe /Run /TN $taskName | Out-Null
}

Assert-Administrator

if (-not $HostUrl) {
    if (-not $ReleaseBase) {
        $ReleaseBase = "https://github.com/AuroraMaster/wsl-memory/releases/latest/download"
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
schtasks.exe /Delete /TN WSLMemoryHost /F 2>$null | Out-Null

Invoke-WebRequest -Uri $HostUrl -OutFile $HostExe
New-TokenIfMissing $TokenPath

$configDir = Join-Path $env:APPDATA "WSLMemoryAgent"
New-Item -ItemType Directory -Force -Path $configDir | Out-Null
$YamlTokenPath = $TokenPath.Replace("'", "''")
if (-not $Listen) {
    $ListenPort = Select-ListenPort
    $ListenIp = "0.0.0.0"
} else {
    $ListenIp, $ListenPortText = $Listen -split ':', 2
    if (-not $ListenIp) { $ListenIp = "0.0.0.0" }
    $ListenPort = [int]$ListenPortText
}
@"
listen_ip: "$ListenIp"
listen_port: $ListenPort
token_path: '$YamlTokenPath'
"@ | Set-Content -Encoding utf8 -LiteralPath (Join-Path $configDir "config.yaml")

if ($NoStart) {
    Write-Host "Installed host binary at $HostExe"
    Write-Host "Token path: $TokenPath"
    exit 0
}

try {
    & $HostExe --install
    $service = Get-Service -Name WSLMemoryHost -ErrorAction SilentlyContinue
    if (-not $service -or $service.Status -ne "Running") {
        throw "Windows service did not reach Running state."
    }
} catch {
    Write-Warning "Windows service install/start failed; using scheduled-task startup fallback. $_"
    sc.exe delete WSLMemoryHost 2>$null | Out-Null
    Install-StartupTask -ExePath $HostExe -InstallDir $InstallDir
}

Write-Host ""
Write-Host "WSL Memory Host is installed and running."
Write-Host "Now run the WSL command from inside each WSL distro you want to manage."
Write-Host "Token path shared with WSL: $TokenPath"
