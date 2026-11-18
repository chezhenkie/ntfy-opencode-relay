# One-shot relay watchdog for the scheduled task. Silent when the relay is
# alive; restarts it through the wrapper when the lock pid is gone.
# The wrapper enforces the single-instance lock, so a run that races with a
# manual start is harmless.

$ErrorActionPreference = 'Continue'

. (Join-Path $PSScriptRoot 'relay-config.ps1')

$Root = Split-Path -Parent $PSScriptRoot
$LogDir = Join-Path $Root 'logs'
$LogFile = Join-Path $LogDir 'relay-watchdog.log'
$StartRelay = Join-Path $PSScriptRoot 'start-relay.ps1'

function Write-Log([string]$Text) {
    Write-RelayLog $LogFile $Text
}

$lockPid = Get-RelayLockPid
$alive = Test-RelayProcess $lockPid
if ($alive) { exit 0 }

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
Write-Log "relay down lockPid=$lockPid, starting"
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $StartRelay
exit $LASTEXITCODE
