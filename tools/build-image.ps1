# Builds the ntfy image from the tracked build context. The ntfy release binary
# is fetched and checksum-verified inside the build, so no file is downloaded
# next to the Dockerfile first.
# Deployment values are not rendered into the image: ntfy-deploy supplies
# NTFY_BASE_URL and NTFY_BEHIND_PROXY when it creates the container.

param(
    [string]$Tag
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
$Context = Join-Path $Root 'container'
$Dockerfile = Join-Path $Context 'Dockerfile'

# The local tag is stable: `server`, with `latest` as an alias. The ntfy
# version stays pinned in the Dockerfile, so a server bump rebuilds the same
# tag and `ntfy-deploy apply` replaces the container by image ID. No relay
# release is involved.
$text = Get-Content -LiteralPath $Dockerfile -Raw
$pin = [regex]::Match($text, '(?m)^ARG NTFY_VERSION=(\S+)\s*$')
if (-not $pin.Success) {
    throw "No ARG NTFY_VERSION pin in $Dockerfile."
}
if (-not $Tag) {
    $Tag = 'server'
}

if ($env:NTFY_PODMAN_BIN) {
    $Podman = $env:NTFY_PODMAN_BIN
} else {
    $Podman = Join-Path $env:LOCALAPPDATA 'Programs\Podman\podman.exe'
}

Write-Output "building localhost/ntfy:$Tag from $Context (ntfy $($pin.Groups[1].Value))"
& $Podman build --format docker -t "localhost/ntfy:$Tag" -t 'localhost/ntfy:latest' $Context
exit $LASTEXITCODE
