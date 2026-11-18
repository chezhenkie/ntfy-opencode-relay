# Shared relay helpers for the Windows wrappers.
# Resolves the install root, the binary, and the config, state, and log paths.
# Mirrors the relay's own path rules: the root is NTFY_RELAY_HOME or the
# directory holding the binary; ~ expands to the profile; relative paths
# resolve against the config file directory.

function Expand-RelayHome([string]$Value) {
    if (-not $Value) { return $Value }
    if ($Value -eq '~') { return $env:USERPROFILE }
    if ($Value.StartsWith('~/') -or $Value.StartsWith('~\')) {
        return (Join-Path $env:USERPROFILE $Value.Substring(2))
    }
    return $Value
}

# Append one stamped line and rotate to a single .1 copy at 1 MiB.
# A failed move never blocks the line: the append still happens.
function Write-RelayLog([string]$Path, [string]$Text) {
    $limit = 1048576
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    try {
        if ((Test-Path -LiteralPath $Path) -and
            ((Get-Item -LiteralPath $Path).Length -ge $limit)) {
            Move-Item -LiteralPath $Path -Destination "$Path.1" -Force -ErrorAction Stop
        }
    } catch {
    }
    Add-Content -LiteralPath $Path -Value "$stamp $Text"
}

function Get-RelayRoot {
    if ($env:NTFY_RELAY_HOME) { return [System.IO.Path]::GetFullPath($env:NTFY_RELAY_HOME) }
    if ($env:NTFY_RELAY_BIN) {
        return [System.IO.Path]::GetFullPath((Split-Path -Parent $env:NTFY_RELAY_BIN))
    }
    return (Join-Path (Split-Path -Parent $PSScriptRoot) 'bin\ntfy-relay')
}

function Get-RelayConfigPath {
    if ($env:NTFY_RELAY_CONFIG) { return $env:NTFY_RELAY_CONFIG }
    return (Join-Path (Get-RelayRoot) 'ntfy-relay.json')
}

function Resolve-RelayPath([string]$Value, [string]$ConfigPath) {
    $expanded = Expand-RelayHome $Value
    if (-not [System.IO.Path]::IsPathRooted($expanded)) {
        $base = Split-Path -Parent $ConfigPath
        if (-not $base) { $base = (Get-Location).Path }
        $expanded = Join-Path $base $expanded
    }
    return [System.IO.Path]::GetFullPath($expanded)
}

function Get-RelayStateFile {
    $configPath = Get-RelayConfigPath
    $value = $null
    try {
        $cfg = Get-Content -Path $configPath -Raw -ErrorAction Stop | ConvertFrom-Json
        if ($cfg.stateFile) { $value = [string]$cfg.stateFile }
    } catch {
        $value = $null
    }
    if (-not $value) { $value = 'ntfy-relay-state.json' }
    return Resolve-RelayPath $value $configPath
}

function Get-RelayLockFile {
    return [System.IO.Path]::ChangeExtension((Get-RelayStateFile), '.lock')
}

function Get-RelayServer {
    $configPath = Get-RelayConfigPath
    try {
        $cfg = Get-Content -Path $configPath -Raw -ErrorAction Stop | ConvertFrom-Json
        if ($cfg.ntfy -and $cfg.ntfy.server) {
            return ([string]$cfg.ntfy.server).TrimEnd('/')
        }
    } catch {
        return $null
    }
    return $null
}

function Get-RelayLogFile {
    $configPath = Get-RelayConfigPath
    $value = $null
    try {
        $cfg = Get-Content -Path $configPath -Raw -ErrorAction Stop | ConvertFrom-Json
        if ($cfg.logging -and $cfg.logging.file) { $value = [string]$cfg.logging.file }
    } catch {
        $value = $null
    }
    if (-not $value) { $value = 'ntfy-relay.log' }
    return Resolve-RelayPath $value $configPath
}

function Get-RelayBinary {
    if ($env:NTFY_RELAY_BIN) { return $env:NTFY_RELAY_BIN }
    return (Join-Path (Get-RelayRoot) 'ntfy-relay.exe')
}

function Get-RelayLockPid {
    $lockFile = Get-RelayLockFile
    try {
        return [int]((Get-Content -Path $lockFile -Raw -ErrorAction Stop | ConvertFrom-Json).pid)
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
