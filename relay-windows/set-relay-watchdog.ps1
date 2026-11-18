param(
    [string]$TaskName = 'ntfy-relay-watchdog',
    [string]$Vbs
)

# Creates or repairs the ntfy relay watchdog task.
#
# The action is wscript.exe with relay-watchdog.vbs: a powershell.exe action hands
# off to Windows Terminal and flashes a console window on this host.
# An existing task is repointed in place, so every other setting survives.
# A missing task is created with the settings recorded in the handoff.
# schtasks /create and /delete do not carry these settings and /delete is blocked
# on this host, so the Schedule.Service COM path is the supported one.

$ErrorActionPreference = 'Stop'

if (-not $Vbs) { $Vbs = Join-Path $PSScriptRoot 'relay-watchdog.vbs' }
$Vbs = (Resolve-Path -LiteralPath $Vbs).Path
if (-not (Test-Path -LiteralPath $Vbs)) { throw "missing $Vbs" }

$wscript = Join-Path $env:WINDIR 'System32\wscript.exe'
$svc = New-Object -ComObject Schedule.Service
$svc.Connect()
$folder = $svc.GetFolder('\')

$existing = $null
try { $existing = $folder.GetTask($TaskName) } catch { $existing = $null }

if ($existing) {
    $def = $existing.Definition
    $user = $def.Principal.UserId
    $mode = 'repointed'
} else {
    $user = $env:USERNAME
    $def = $svc.NewTask(0)
    $def.RegistrationInfo.Description = 'Restarts the ntfy inbox relay when its lock owner is gone.'
    $def.Principal.LogonType = 3
    $def.Principal.UserId = $user
    $def.Principal.RunLevel = 0
    $def.Settings.Enabled = $true
    $def.Settings.Hidden = $true
    $def.Settings.DisallowStartIfOnBatteries = $false
    $def.Settings.StopIfGoingOnBatteries = $false
    $def.Settings.StartWhenAvailable = $true
    $def.Settings.MultipleInstances = 2
    $def.Settings.ExecutionTimeLimit = 'PT72H'
    $trigger = $def.Triggers.Create(1)
    $trigger.StartBoundary = (Get-Date).AddMinutes(1).ToString('yyyy-MM-ddTHH:mm:ss')
    $trigger.Repetition.Interval = 'PT5M'
    $trigger.Repetition.Duration = 'P3650D'
    $trigger.Repetition.StopAtDurationEnd = $false
    $mode = 'created'
}

$def.Actions.Clear()
$action = $def.Actions.Create(0)
$action.Path = $wscript
$action.Arguments = '"' + $Vbs + '"'

$null = $folder.RegisterTaskDefinition($TaskName, $def, 6, $user, $null, 3, $null)

$check = $folder.GetTask($TaskName)
$a = $check.Definition.Actions.Item(1)
Write-Output "$mode $TaskName"
Write-Output "action: $($a.Path) $($a.Arguments)"
Write-Output "principal: logon=$($check.Definition.Principal.LogonType) user=$($check.Definition.Principal.UserId) runlevel=$($check.Definition.Principal.RunLevel)"
Write-Output "settings: hidden=$($check.Definition.Settings.Hidden) battery-start=$($check.Definition.Settings.DisallowStartIfOnBatteries) battery-stop=$($check.Definition.Settings.StopIfGoingOnBatteries) startwhenavailable=$($check.Definition.Settings.StartWhenAvailable) multiple=$($check.Definition.Settings.MultipleInstances)"
Write-Output "repetition=$($check.Definition.Triggers.Item(1).Repetition.Interval) duration=$($check.Definition.Triggers.Item(1).Repetition.Duration)"
