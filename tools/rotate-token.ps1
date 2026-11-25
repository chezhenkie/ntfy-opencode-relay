# Rotate one ntfy access token in two explicit phases.
# Issue: create a token, write the consumer file, optionally restart the relay
#        and verify a publish with the new token.
# Revoke: delete an old token by prefix, never the token in the consumer file.
# The old token stays valid until the revoke phase, so a failed paste or a
# failed restart never locks the phone or the relay out.
# Temporary: -Expires issues a token that expires and writes no consumer file;
#            it is refused for a relay restart or the live consumer path.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('phone')][string]$User,
    [string]$Label = '',
    [string]$TokenFile = '',
    [string]$Expires = '',
    [string]$RevokeOld = '',
    [switch]$RestartRelay,
    [switch]$DryRun
)

$ErrorActionPreference = 'Continue'

$Container = 'ntfy'
$ProbeTopic = 'ntfy-probe'

function Fail([string]$Text) {
    Write-Host "FAIL $Text"
    exit 1
}

function Get-RelayRoot {
    if ($env:NTFY_RELAY_HOME) { return [System.IO.Path]::GetFullPath($env:NTFY_RELAY_HOME) }
    return (Join-Path $env:LOCALAPPDATA 'ntfy-relay')
}

function Get-RelayConfigPath {
    return (Join-Path (Get-RelayRoot) 'ntfy-relay.json')
}

function Read-RelayConfig {
    try {
        return (Get-Content -Path (Get-RelayConfigPath) -Raw -ErrorAction Stop | ConvertFrom-Json)
    } catch {
        return $null
    }
}

function Get-RelayPath([string]$Value, [string]$Default) {
    if (-not $Value) { $Value = $Default }
    if ($Value -eq '~') { return $env:USERPROFILE }
    if ($Value.StartsWith('~/') -or $Value.StartsWith('~\')) {
        return (Join-Path $env:USERPROFILE $Value.Substring(2))
    }
    if ([System.IO.Path]::IsPathRooted($Value)) { return $Value }
    return (Join-Path (Get-RelayRoot) $Value)
}

function Get-RelayStateFile {
    $cfg = Read-RelayConfig
    $value = ''
    if ($cfg -and $cfg.stateFile) { $value = [string]$cfg.stateFile }
    return (Get-RelayPath $value 'ntfy-relay-state.json')
}

function Get-RelayLockFile {
    return [System.IO.Path]::ChangeExtension((Get-RelayStateFile), '.lock')
}

function Get-RelayLockPid {
    try {
        return [int]((Get-Content -Path (Get-RelayLockFile) -Raw -ErrorAction Stop | ConvertFrom-Json).pid)
    } catch {
        return 0
    }
}

function Test-RelayProcess([int]$ProcessId) {
    if ($ProcessId -le 0) { return $false }
    $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -ErrorAction SilentlyContinue
    if (-not $proc) { return $false }
    return [bool]($proc.CommandLine -match 'ntfy-relay(\.exe)?["\s]+run')
}

function Get-RelayServer {
    $cfg = Read-RelayConfig
    if ($cfg -and $cfg.ntfy -and $cfg.ntfy.server) {
        return ([string]$cfg.ntfy.server).TrimEnd('/')
    }
    return $null
}

function Get-RelayLogFile {
    $cfg = Read-RelayConfig
    $value = ''
    if ($cfg -and $cfg.logging -and $cfg.logging.file) { $value = [string]$cfg.logging.file }
    return (Get-RelayPath $value 'ntfy-relay.log')
}

function Get-DefaultTokenFile {
    return (Join-Path (Get-RelayRoot) 'ntfy-token.txt')
}

function Get-TokenFile {
    if ($TokenFile) { return [System.IO.Path]::GetFullPath($TokenFile) }
    return (Get-DefaultTokenFile)
}

function ConvertTo-ExpirySpan([string]$Text) {
    if ($Text -notmatch '^([1-9][0-9]{0,3})([mhdw])$') { return $null }
    $count = [int]$Matches[1]
    switch ($Matches[2]) {
        'm' { return [TimeSpan]::FromMinutes($count) }
        'h' { return [TimeSpan]::FromHours($count) }
        'd' { return [TimeSpan]::FromDays($count) }
        'w' { return [TimeSpan]::FromDays(7 * $count) }
    }
}

function Get-UserTokens([string]$Name) {
    $out = & podman exec $Container ntfy token list $Name 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { Fail "token-list user=$Name exit=$LASTEXITCODE" }
    $tokens = @()
    foreach ($line in ($out -split "`n")) {
        $trim = $line.Trim()
        if ($trim -match '^-\s+(tk_[A-Za-z0-9]+)') { $tokens += $Matches[1] }
    }
    return $tokens
}

$Expires = $Expires.Trim().ToLowerInvariant()
$ExpirySpan = $null
if ($Expires) {
    $ExpirySpan = ConvertTo-ExpirySpan $Expires
    if (-not $ExpirySpan) { Fail 'expires-format, use Nd, Nh, Nm, or Nw with N 1-9999' }
    if ($RestartRelay) { Fail 'expires-live-token, a relay token must not expire' }
    if ($RevokeOld) { Fail 'expires-with-revoke, issue and revoke stay separate phases' }
    if ($TokenFile) {
        $resolved = [System.IO.Path]::GetFullPath($TokenFile)
        $consumers = @(Get-DefaultTokenFile)
        if ($consumers -contains $resolved) { Fail "expires-consumer-file path=$resolved, a temporary token gets no consumer file" }
    }
}

if (-not $Label) { $Label = 'phone-' + (Get-Date -Format 'yyyyMMdd-HHmm') }
$File = Get-TokenFile $User
$Tokens = Get-UserTokens $User
if ($RevokeOld -and $Tokens.Count -eq 0) { Fail "no-token user=$User" }

if ($RevokeOld) {
    if ($RevokeOld.Length -lt 4) { Fail 'revoke-prefix too short, use at least 4 characters' }
    $hits = @($Tokens | Where-Object { $_.StartsWith($RevokeOld) })
    if ($hits.Count -ne 1) { Fail "revoke-match user=$User prefix=$RevokeOld hits=$($hits.Count)" }
    $Old = $hits[0]
    $Keep = ''
    if (Test-Path $File) { $Keep = (Get-Content $File -Raw).Trim() }
    if ($Keep -and $Old -eq $Keep) { Fail 'revoke-refused token is the consumer file token' }
    if ($DryRun) {
        Write-Host "DRY-RUN podman exec $Container ntfy token remove $User $($Old.Substring(0, 8))..."
        exit 0
    }
    & podman exec $Container ntfy token remove $User $Old 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "token-remove user=$User exit=$LASTEXITCODE" }
    $After = Get-UserTokens $User
    if ($After -contains $Old) { Fail 'revoke-still-present' }
    if ($Keep -and -not ($After -contains $Keep)) { Fail 'revoke-current-token-missing' }
    Write-Host "PASS token-revoked user=$User prefix=$($Old.Substring(0, 8))..."
    exit 0
}

if ($DryRun) {
    $plan = "DRY-RUN podman exec $Container ntfy token add --label=$Label"
    if ($Expires) { $plan += " --expires=$Expires" }
    Write-Host "$plan $User"
    if ($Expires) {
        $at = (Get-Date).ToUniversalTime().Add($ExpirySpan).ToString('yyyy-MM-dd HH:mm')
        Write-Host "DRY-RUN temporary token, no consumer file, expires $Expires at ${at}z"
    }
    else { Write-Host "DRY-RUN write $File" }
    if ($RestartRelay) {
        Write-Host "DRY-RUN stop relay pid=$(Get-RelayLockPid); ntfy-relay ensure; publish $ProbeTopic"
    }
    Write-Host "DRY-RUN old tokens: $(($Tokens | ForEach-Object { $_.Substring(0, 8) + '...' }) -join ', ')"
    exit 0
}

$addArgs = @('exec', $Container, 'ntfy', 'token', 'add', "--label=$Label")
if ($Expires) { $addArgs += "--expires=$Expires" }
$addArgs += $User
$out = & podman @addArgs 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { Fail "token-add user=$User exit=$LASTEXITCODE" }
if ($out -notmatch '(tk_[A-Za-z0-9]+)') { Fail 'token-add output has no token' }
$New = $Matches[1]

$listOut = & podman exec $Container ntfy token list $User 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { Fail "token-list user=$User exit=$LASTEXITCODE" }
$line = (@($listOut -split "`n") | Where-Object { $_ -match [regex]::Escape($New) } | Select-Object -First 1)
if (-not $line) { Fail 'token-list missing the new token' }
if ($Expires -and $line -match 'never expires') { Fail 'expires-not-applied' }
if (-not $Expires -and $line -notmatch 'never expires') { Fail 'expiry-unexpected on a live token' }

if ($Expires) {
    $at = (Get-Date).ToUniversalTime().Add($ExpirySpan).ToString('yyyy-MM-dd HH:mm')
    Write-Host "PASS token-issued user=$User prefix=$($New.Substring(0, 8))... expires=$Expires at=${at}z file=none"
}
else {
    $dir = Split-Path -Parent $File
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    $tmp = "$File.tmp"
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($tmp, $New + "`n", $utf8)
    Move-Item -Force -Path $tmp -Destination $File
    Write-Host "PASS token-issued user=$User prefix=$($New.Substring(0, 8))... file=$File"
}

if ($RestartRelay) {
    $oldPid = Get-RelayLockPid
    if (Test-RelayProcess $oldPid) {
        & (Join-Path (Get-RelayRoot) 'ntfy-admin.exe') stop *> $null
        $deadline = (Get-Date).AddSeconds(15)
        while ((Test-RelayProcess $oldPid) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
        if (Test-RelayProcess $oldPid) {
            Stop-Process -Id $oldPid -Force
            Start-Sleep -Milliseconds 500
        }
        if (Test-RelayProcess $oldPid) { Fail "relay-stop pid=$oldPid" }
        Write-Host "PASS relay-stopped pid=$oldPid"
    }
    & (Join-Path (Get-RelayRoot) 'ntfy-relay.exe') ensure *> $null
    if ($LASTEXITCODE -ne 0) { Fail "relay-ensure exit=$LASTEXITCODE see logs\relay-ensure.log" }
    $newPid = Get-RelayLockPid
    if (-not (Test-RelayProcess $newPid)) { Fail 'relay-start no live process' }
    Write-Host "PASS relay-started pid=$newPid"

    $server = Get-RelayServer
    if (-not $server) { Fail 'relay-server missing in config' }
    $codes = @()
    for ($i = 0; $i -lt 10; $i++) {
        $code = & curl.exe -sS -o NUL -m 10 -w '%{http_code}' -H "Authorization: Bearer $New" -d 'rotate-check' "$server/$ProbeTopic" 2>&1
        $codes += [string]$code
        if ([string]$code -eq '200') { break }
        Start-Sleep -Seconds 2
    }
    if ($codes[-1] -ne '200') { Fail "publish-probe codes=$($codes -join ',')" }
    Write-Host "PASS publish-probe topic=$ProbeTopic code=200"

    $logFile = Get-RelayLogFile
    $tail = Get-Content -Path $logFile -Tail 30 -ErrorAction SilentlyContinue
    if ($tail -match ' 401 ' -or $tail -match 'unauthorized') { Fail 'relay-log unauthorized after restart' }
    Write-Host 'PASS relay-log clean'
}

if ($Expires) { Write-Host "NEXT remove the temporary token by prefix when it is no longer needed: .\tools\rotate-token.ps1 -User $User -RevokeOld <prefix>" }
else { Write-Host "NEXT revoke the old token: .\tools\rotate-token.ps1 -User $User -RevokeOld <prefix>" }
Write-Host "TOKEN $New"
Write-Host 'MCP processes pick up the new token only after an OpenCode reload.'
