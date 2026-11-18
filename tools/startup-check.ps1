# ntfy startup check. Starts the Podman machine when stopped, then verifies
# the container, Tailscale Serve, the loopback portproxy, and HTTPS health.
# Writes PASS/FAIL lines to logs\startup-check.log. Never elevates and never
# repairs the portproxy; it only reports. Exit code 0 means all checks passed.

param(
    [int]$MachineTimeoutSec = 240,
    [int]$HealthTimeoutSec = 120
)

$ErrorActionPreference = 'Continue'

$Root = Split-Path -Parent $PSScriptRoot
$LogDir = Join-Path $Root 'logs'
$LogFile = Join-Path $LogDir 'startup-check.log'
if ($env:NTFY_PODMAN_BIN) {
    $Podman = $env:NTFY_PODMAN_BIN
} else {
    $Podman = Join-Path $env:LOCALAPPDATA 'Programs\Podman\podman.exe'
}
if ($env:NTFY_TAILSCALE_BIN) {
    $Tailscale = $env:NTFY_TAILSCALE_BIN
} else {
    $Tailscale = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
}
. (Join-Path $Root 'relay-windows\relay-config.ps1')
$StartRelay = Join-Path $Root 'relay-windows\start-relay.ps1'
$RelayLog = Get-RelayLogFile
$RelayServer = Get-RelayServer
if ($env:NTFY_HEALTH_URL) {
    $HealthUrl = $env:NTFY_HEALTH_URL
} elseif ($RelayServer) {
    $HealthUrl = "$RelayServer/v1/health"
} else {
    $HealthUrl = 'http://127.0.0.1:8810/v1/health'
}

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

function Write-Log([string]$Text) {
    Write-RelayLog $LogFile $Text
}

function Add-Result([string]$Name, [bool]$Ok, [string]$Detail = '') {
    $state = 'FAIL'
    if ($Ok) { $state = 'PASS' }
    $line = "$state $Name"
    if ($Detail) { $line = "$line $Detail" }
    Write-Log $line
}

function Test-Portproxy {
    $text = (netsh interface portproxy show v4tov6) -join "`n"
    return [bool]($text -match '(?m)^\s*127\.0\.0\.1\s+8810\s+::1\s+8810\s*$')
}

# The relay owns its files: no ntfy file may remain under the OpenCode config
# or state directory (spec 10, ownership rule).
function Test-NoLegacyRelayFiles {
    $dirs = @(
        (Join-Path $env:USERPROFILE '.config\opencode'),
        (Join-Path $env:USERPROFILE '.local\state\opencode')
    )
    $hits = @()
    foreach ($dir in $dirs) {
        if (-not (Test-Path -LiteralPath $dir)) { continue }
        foreach ($pattern in @('ntfy*', 'notification-ntfy*')) {
            $hits += @(Get-ChildItem -LiteralPath $dir -Filter $pattern -ErrorAction SilentlyContinue |
                ForEach-Object { $_.FullName })
        }
    }
    return ($hits -join ',')
}

function Test-Health {
    $body = & curl.exe -sS -m 8 $HealthUrl 2>$null
    if ($LASTEXITCODE -ne 0) { return $false }
    return [bool](($body -join '') -match '"healthy":true')
}

function Test-Relay {
    $lockPid = Get-RelayLockPid
    $procOk = Test-RelayProcess $lockPid
    Add-Result 'relay-process' $procOk "pid=$lockPid"
    if (-not $procOk) { return $false }

    $heartbeat = $null
    $lifecycle = $null
    $eventAge = 'n/a'
    $queueDepth = 'n/a'
    $lines = Get-Content -Path $RelayLog -Tail 400 -ErrorAction SilentlyContinue
    foreach ($line in $lines) {
        if ($line -match '^(\S+) INFO heartbeat') {
            $heartbeat = [datetime]::Parse($matches[1]).ToUniversalTime()
            if ($line -match 'lastEventAge=(\S+)') { $eventAge = $matches[1] }
            if ($line -match ' queue=(\d+)') { $queueDepth = $matches[1] }
        } elseif ($line -match '^(\S+) INFO (subscribed|relay starting|topics now)') {
            $lifecycle = [datetime]::Parse($matches[1]).ToUniversalTime()
        }
    }
    $now = (Get-Date).ToUniversalTime()
    $hbAge = -1
    if ($heartbeat) { $hbAge = [int]($now - $heartbeat).TotalSeconds }
    $lcAge = -1
    if ($lifecycle) { $lcAge = [int]($now - $lifecycle).TotalSeconds }
    $fresh = ($hbAge -ge 0 -and $hbAge -le 720) -or ($lcAge -ge 0 -and $lcAge -le 600)
    Add-Result 'relay-heartbeat' $fresh "heartbeatAge=$hbAge lifecycleAge=$lcAge lastEventAge=$eventAge queue=$queueDepth"
    return $fresh
}

Write-Log '---- startup check begin ----'

if (Test-Path $LogFile) {
    $lines = Get-Content -Path $LogFile -ErrorAction SilentlyContinue
    if ($lines.Count -gt 2000) {
        $lines | Select-Object -Last 1000 | Set-Content -Path $LogFile
    }
}

$ok = $true

if (-not (Test-Path $Podman)) {
    Add-Result 'podman-binary' $false $Podman
    Write-Log '---- startup check end (aborted) ----'
    exit 1
}
Add-Result 'podman-binary' $true

# Do not pipe the wrapper: a detached child could inherit the pipe and keep
# this call open. Start-Process -Wait never ties the call to the pipe.
$startArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $StartRelay)
Start-Process -FilePath 'powershell.exe' -ArgumentList $startArgs -Wait -WindowStyle Hidden
if (-not (Test-Relay)) { $ok = $false }

$running = ((& $Podman machine list --format '{{.Running}}' 2>$null) -join '').Trim()
if ($running -ne 'true') {
    Write-Log 'INFO podman machine stopped, starting'
    & $Podman machine start 2>&1 | ForEach-Object { Write-Log "INFO $_" }
}

$deadline = (Get-Date).AddSeconds($MachineTimeoutSec)
while ((Get-Date) -lt $deadline) {
    $running = ((& $Podman machine list --format '{{.Running}}' 2>$null) -join '').Trim()
    if ($running -eq 'true') { break }
    Start-Sleep -Seconds 5
}
$machineOk = ($running -eq 'true')
Add-Result 'podman-machine' $machineOk
if (-not $machineOk) { $ok = $false }

$name = ((& $Podman ps -a --filter 'name=^ntfy$' --format '{{.Names}}' 2>$null) -join '').Trim()
$containerOk = ($name -eq 'ntfy')
Add-Result 'container-present' $containerOk
if (-not $containerOk) { $ok = $false }

if ($containerOk -and $machineOk) {
    $deadline = (Get-Date).AddSeconds($HealthTimeoutSec)
    $health = ''
    while ((Get-Date) -lt $deadline) {
        $health = ((& $Podman inspect ntfy --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' 2>$null) -join '').Trim()
        if ($health -eq 'healthy') { break }
        Start-Sleep -Seconds 5
    }
    $containerHealthOk = ($health -eq 'healthy')
    Add-Result 'container-health' $containerHealthOk $health
    if (-not $containerHealthOk) { $ok = $false }
}

if (Test-Path $Tailscale) {
    $deadline = (Get-Date).AddSeconds(90)
    $serveOk = $false
    while ((Get-Date) -lt $deadline) {
        $serve = (& $Tailscale serve status 2>$null) -join "`n"
        if ($serve -match 'proxy http://127\.0\.0\.1:8810') { $serveOk = $true; break }
        Start-Sleep -Seconds 5
    }
    Add-Result 'tailscale-serve' $serveOk
    if (-not $serveOk) { $ok = $false }
} else {
    Add-Result 'tailscale-bin' $false $Tailscale
    $ok = $false
}

$proxyOk = Test-Portproxy
Add-Result 'loopback-portproxy' $proxyOk
if (-not $proxyOk) { $ok = $false }

$legacy = Test-NoLegacyRelayFiles
Add-Result 'relay-root-clean' (-not $legacy) $legacy
if ($legacy) { $ok = $false }

$deadline = (Get-Date).AddSeconds($HealthTimeoutSec)
$httpsOk = $false
while ((Get-Date) -lt $deadline) {
    if (Test-Health) { $httpsOk = $true; break }
    Start-Sleep -Seconds 5
}
Add-Result 'https-health' $httpsOk $HealthUrl
if (-not $httpsOk) { $ok = $false }

if ($ok) {
    Write-Log 'RESULT all checks passed'
    Write-Log '---- startup check end ----'
    exit 0
}
Write-Log 'RESULT failures present, see FAIL lines'
Write-Log '---- startup check end ----'
exit 1
