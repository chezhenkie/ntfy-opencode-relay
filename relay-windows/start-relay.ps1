# Start the Rust relay detached when it is not already running.
# The relay itself takes the lock; a second launch exits quietly.
# Writes PASS/FAIL to logs\relay-start.log and redirects process output to
# logs\relay-stdout.log and logs\relay-stderr.log.

$ErrorActionPreference = 'Continue'

. (Join-Path $PSScriptRoot 'relay-config.ps1')

$Root = Split-Path -Parent $PSScriptRoot
$LogDir = Join-Path $Root 'logs'
$LogFile = Join-Path $LogDir 'relay-start.log'
$Stdout = Join-Path $LogDir 'relay-stdout.log'
$Stderr = Join-Path $LogDir 'relay-stderr.log'
$Binary = Get-RelayBinary

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null

function Write-Log([string]$Text) {
    Write-RelayLog $LogFile $Text
}

if (-not (Test-Path $Binary)) {
    Write-Log "FAIL relay-binary $Binary"
    exit 1
}

$lockPid = Get-RelayLockPid
if (Test-RelayProcess $lockPid) {
    Write-Log "PASS relay-running pid=$lockPid"
    exit 0
}

# Spawn through WMI so the relay inherits no handles from the caller.
# ShowWindow 0 keeps the console hidden.
$cmdLine = 'cmd.exe /d /s /c ""{0}" run >"{1}" 2>"{2}""' -f $Binary, $Stdout, $Stderr
$startup = ([wmiclass]'Win32_ProcessStartup').CreateInstance()
$startup.Properties['ShowWindow'].Value = [uint16]0
$spawn = ([wmiclass]'Win32_Process').Create($cmdLine, $Root, $startup)
if ($spawn.ReturnValue -ne 0) {
    Write-Log "FAIL relay-spawn ReturnValue=$($spawn.ReturnValue)"
    exit 1
}

$deadline = (Get-Date).AddSeconds(30)
$startedPid = 0
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 2
    $lockPid = Get-RelayLockPid
    if (Test-RelayProcess $lockPid) { $startedPid = $lockPid; break }
}

if ($startedPid -gt 0) {
    Write-Log "PASS relay-started pid=$startedPid"
    exit 0
}
Write-Log 'FAIL relay-start no live process within 30 s, see relay-stderr.log'
exit 1
