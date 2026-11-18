# Recreates the retired VPN-facing portproxy. This is the rollback path
# documented in docs\https.md, not part of the current route: the TLS
# terminator reaches the container through the loopback bridge on
# 127.0.0.1:8810.

param(
    [Parameter(Mandatory = $true)][string]$ListenAddress,
    [int]$Port = 8810
)

$ErrorActionPreference = 'Stop'

if (-not $ListenAddress) {
    throw 'Pass -ListenAddress with the private address the phone uses.'
}

$log = Join-Path (Split-Path -Parent $PSScriptRoot) 'logs\private-net-portproxy.log'
Start-Transcript -Path $log -Force

netsh interface portproxy delete v4tov4 listenaddress=$ListenAddress listenport=$Port 2>$null
netsh interface portproxy delete v4tov6 listenaddress=$ListenAddress listenport=$Port 2>$null
netsh interface portproxy add v4tov6 listenaddress=$ListenAddress listenport=$Port connectaddress=::1 connectport=$Port

$ruleName = "ntfy $Port private"
Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule

netsh interface portproxy show all

Stop-Transcript
