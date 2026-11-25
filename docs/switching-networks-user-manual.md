# Switching networks - user manual

How to move the ntfy server between Tailscale, NordVPN Meshnet, and the local network. The tool is `ntfy-deploy`, which sits beside the relay and never touches it. Install and first-run steps are in [operations.md](operations.md); the phone side is in [phone-app-user-manual.md](phone-app-user-manual.md).

## 1. What a switch does

`ntfy-deploy mode` detects the network paths that exist on this host, writes `ntfy-deploy.json` for the one you pick, recreates the container with that base URL, and prints the phone URL.

| | proxy (Tailscale) | vpn (Nord Meshnet) | lan |
|---|---|---|---|
| server URL | `https://<name>` | `http://<private ip>:8810` | `http://<lan ip>:8810` |
| TLS terminator | required | not used | not used |
| Windows portproxy | loopback | loopback plus the mesh address | loopback |
| phone network | tailnet | Meshnet | same LAN |

A switch does not rebuild the image, restart the relay, change the token, or edit the relay config. The container is replaced, which takes a few seconds; the cache volume and all messages survive. The relay keeps talking to `http://127.0.0.1:8810` through the whole switch.

## 2. Before you start

- The relay is running and `curl.exe -sS http://127.0.0.1:8810/v1/health` returns 200.
- Exactly one private network is active on the host: Tailscale or Nord Meshnet. Both use `100.64/10`, and two routes over that range make the phone path unpredictable.
- On Windows, open an elevated PowerShell. The portproxy step uses `netsh` and fails with exit 4 without administrator rights.
- The phone is on the same private network and you can reach its ntfy settings.

The commands below run from the repository root, or from the install directory with `ntfy-deploy` on `PATH`.

## 3. The three paths

**Tailscale.** Tailscale is connected and the TLS terminator still serves 443 to loopback. The tool reads the node's MagicDNS name and IPv4 from `tailscale status --json`. The phone uses the HTTPS name, so the certificate must cover it.

**Nord Meshnet.** NordVPN is connected with Meshnet enabled, and the host has a `100.64.x.x` mesh address on the Nord interface. The `10.5.x` address is the tunnel endpoint and is not reachable from the phone; the tool prefers the mesh address and prints a note when it finds only the tunnel one. The phone must be on Nord Meshnet too, from the same account.

**Local network.** The host and the phone share a LAN and the host address is private. There is no TLS on this path; the phone uses plain HTTP inside the network.

## 4. Switch with the menu

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" mode
```

The tool lists what it found and waits for a number:

```
available network paths:
1) Tailscale proxy  https://ntfy.example.ts.net
2) NordVPN   vpn    http://100.64.0.10:8810
3) Local network lan    http://192.168.1.20:8810
     note: the phone must be on the same network

select 1-3 or press Enter to cancel:
```

An empty line cancels and writes nothing. A path is listed only when its address exists, so a stopped VPN is simply absent.

After the pick it prints the written config path, the apply report, and the phone URL:

```
config written: C:\Users\me\AppData\Local\ntfy-relay\ntfy-deploy.json
mode: proxy
base url: https://ntfy.example.ts.net
container: ntfy (localhost/ntfy:2.37.2)
publish: 8810:8080
container state: recreated
health: 200
portproxy: 127.0.0.1:8810 -> ::1:8810
result: applied
phone server URL: https://ntfy.example.ts.net
set this URL in the app if it changed; the token stays the same.
```

`container state: reused` means the running container already matched, so nothing was replaced.

## 5. Switch with a name

Named targets skip the menu and are safe in a script:

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" mode tailscale
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" mode nord
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" mode lan
```

An unavailable name exits 2 and names what was detected:

```
FAIL nord is not available; detected: tailscale, lan
```

Preview a switch without changing anything:

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" mode nord --dry-run
```

This prints the exact JSON document it would write and the apply plan, and writes nothing.

## 6. Set the phone

After the switch, open ntfy on the phone, open the subscription, and set the server URL the command printed. The token in the custom `Authorization` header stays the same, and so does the topic.

- Tailscale: `https://<name>`, no port.
- Nord Meshnet: `http://100.64.x.x:8810`.
- LAN: `http://192.168.x.x:8810`.

On the Tailscale path the app reconnects over `wss://`; on the other two it uses `ws://` inside the private network. Send one message from the phone and answer it from the desktop to prove both directions.

## 7. Check and roll back

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" status
```

`status` prints mode, base URL, container, publish, health, and the active portproxy or listener. Exit 0 means the container runs, loopback health is 200, and the rules the mode requires exist; exit 3 means one of those is missing.

Rolling back is the same command with the other path, for example `mode tailscale` after a Nord session. Going back to proxy removes the mesh portproxy entry. The relay pid and the token do not change.

## 8. When it fails

| symptom | cause | fix |
|---|---|---|
| `no network path detected` | no VPN connected, no LAN address | connect the VPN, or check `ipconfig`; every path needs an address |
| target exited 2 as unavailable | that VPN is down | connect it, then rerun with no target to see what is listed |
| `FAIL ... netsh ...` exit 4 | shell is not elevated | rerun from PowerShell started as administrator |
| the phone cannot connect, health is 200 | phone URL or firewall | check the URL, then allow inbound TCP 8810 on the private interface |
| both `tailscale` and `nord` are listed | both VPNs are up | disconnect one; the note on the Nord entry says the same |
| messages arrive, attachments do not open | `baseUrl` does not match the phone's network | switch to the path the phone is on; links follow `baseUrl` |
| `status` exits 3 in vpn mode | the mesh portproxy entry is absent | run `apply` in an elevated shell; nothing else is wrong |

If a switch writes a config and then fails at the health wait, the container is down but the config names the new path. Fix the cause, then run `apply` again; there is no separate undo step.

## 9. Exit codes

`0` ok. `2` invalid config, unknown argument, or unavailable path. `3` container unhealthy after a switch. `4` a platform command failed, most often `netsh` without elevation.
