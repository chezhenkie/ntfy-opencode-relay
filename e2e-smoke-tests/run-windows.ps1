# Windows wrapper for the ntfy end-to-end smoke.
# Resolves node and hands every option to smoke.mjs unchanged.
# Exit codes: 0 all checks passed, 1 a check failed, 2 node or setup missing.

param(
    [string]$BaseUrl,
    [string]$TokenFile,
    [string]$Topic,
    [int]$TimeoutMs = 0
)

$ErrorActionPreference = 'Stop'

if ($env:NTFY_NODE) {
    $Node = $env:NTFY_NODE
} elseif (Get-Command node.exe -ErrorAction SilentlyContinue) {
    $Node = (Get-Command node.exe).Source
} else {
    $Node = Join-Path $env:ProgramFiles 'nodejs\node.exe'
}
if (-not (Test-Path -LiteralPath $Node)) {
    Write-Output "FAIL node $Node"
    exit 2
}

$Smoke = Join-Path $PSScriptRoot 'smoke.mjs'
if (-not (Test-Path -LiteralPath $Smoke)) {
    Write-Output "FAIL script $Smoke"
    exit 2
}

$Arguments = @($Smoke)
if ($BaseUrl) { $Arguments += @('--base-url', $BaseUrl) }
if ($TokenFile) { $Arguments += @('--token-file', $TokenFile) }
if ($Topic) { $Arguments += @('--topic', $Topic) }
if ($TimeoutMs -gt 0) { $Arguments += @('--timeout', "$TimeoutMs") }

& $Node @Arguments
exit $LASTEXITCODE
