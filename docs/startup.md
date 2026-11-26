# Startup, stop and restart

The stack is three layers: the host services and route, the ntfy container, and the relay. Check them in that order, because a failure in one hides the next. Commands run at the repository root in PowerShell 5.1 on Windows; Ubuntu commands are at the end.

## Check everything

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" status
```

The report is read-only: the config, lock, state, and last log line, then `podman-binary`, `podman-machine`, `container-present`, `container-health`, `tailscale-serve`, `loopback-portproxy`, `relay-root-clean`, and `https-health`, each PASS, FAIL, or SKIP. It starts and repairs nothing and exits 0 as a report, so a FAIL line names the fault. The start or repair step is the logon task, or `ntfy-deploy install` from a release folder.

The server side alone is one command:

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" status
```

Expected output ends with `health: 200`, the portproxy line, and `status: ok`. Exit code 3 means the container is not healthy; 4 means a platform command failed.

The health check calls `ntfy.server` from the relay config, which is loopback in the shipped config; `NTFY_HEALTH_URL` overrides the URL for the relay status report. It proves the server answers, not that the phone route works. Check the route by name with `curl.exe -sS https://<private-dns-name>/v1/health`.

## Check one layer

| layer | check | expected |
|---|---|---|
| private network | `Get-Service Tailscale` | `Running` |
| TLS route | `& "$env:ProgramFiles\Tailscale\tailscale.exe" serve status` | `proxy http://127.0.0.1:8810` |
| loopback bridge | `netsh interface portproxy show v4tov6` | `127.0.0.1 8810 ::1 8810` |
| Podman machine | `podman machine list --format '{{.Name}} running={{.Running}}'` | `running=true` |
| container | `podman ps -a --filter 'name=^ntfy$' --format '{{.Names}} {{.Status}}'` | `Up ... (healthy)` |
| relay | `& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" status` | `lock ... alive`, a log line within 12 minutes, and the platform rows |
| relay endpoint | `& "$env:NTFY_RELAY_HOME\ntfy-admin.exe" status` | `pid:`, `started:`, the roots, and the live topics |
| watchdog | `Get-ScheduledTask -TaskName ntfy-relay-watchdog` | `Ready`, or `Running` during its tick |
| logon start | `Get-ScheduledTask -TaskName ntfy-relay-startup` | `Ready` |

`ntfy-relay status` proves the process and the lock, not the subscription; `ntfy-admin status` proves the control endpoint and shows the live topic set; the last relay log line covers the subscription. The status rows are a read-only report: they start and repair nothing and exit 0, so a FAIL line names the fault. The lock file is `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-state.lock`, and its `pid` must match a running `ntfy-relay.exe run`.

The route examples use Tailscale. For Nord Meshnet or LAN mode, check the equivalent client and use the base URL that `ntfy-deploy status` prints; see [switching networks](switching-networks-user-manual.md).

## Start

At logon Windows runs the `ntfy-relay-startup` task, so the usual case needs no manual step. It starts the Podman machine and runs `ensure`; no console window opens. To start by hand, in order:

```powershell
podman machine start
podman machine list --format '{{.Running}}'   # wait for true
podman start ntfy                             # only after an explicit container stop
& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" ensure
```

The machine start restores the container through the `unless-stopped` policy and `podman-restart.service`; `podman start ntfy` is for a container that was stopped on purpose. If the container is gone, `& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" apply` recreates it from `%LOCALAPPDATA%\ntfy-relay\ntfy-deploy.json`. Do not use a bare `podman run`: the image has no base URL and the container exits.

`ensure` is idempotent. It prints `running pid=<n>` and exits 0 when the lock names a live, subscribed relay; `starting pid=<n>` when the relay is alive but still waiting for its token; `disabled` when the config sets `enabled: false`, after it stops a live relay. Otherwise it starts the binary detached, prints `started pid=<n>` when the lock appears, and exits 1 with a FAIL line in `logs\relay-ensure.log` when the child dies first or the lock does not appear within 60 s. The watchdog task runs this subcommand every five minutes. Do not start a second relay by hand: two processes against one state file handle every phone message twice.

The Tailscale service is Automatic and needs no manual start. `Start-Service Tailscale` is the recovery step, and it needs an elevated shell.

## Project topics

The topic is the project folder name. The relay serves every OpenCode project under each folder in `projectRoots`, so one parent folder covers every project below it. The list is local: it sits in `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.json` and the relay writes it, so no path is compiled into the binary. A parent entry looks like this:

```json
"projectRoots": ["C:\\work"]
```

`ntfy-admin` changes the list while the relay runs; discovery reruns at once, and outbound scope and a running `mcp` process follow it, so no restart is needed:

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-admin.exe" topics
& "$env:NTFY_RELAY_HOME\ntfy-admin.exe" add C:\work
& "$env:NTFY_RELAY_HOME\ntfy-admin.exe" remove C:\work
```

A new project under an existing root needs no host edit. Open it once so OpenCode registers the folder; the relay discovers it within 60 seconds, resubscribes, and the phone can subscribe to the folder name. A hand edit of `projectRoots` still works for recovery, but it needs a relay restart.

Every project under the root gets a topic and its turns are published there, whether or not the phone follows it; the phone shows only its subscriptions. A project outside every root is silent in both directions, and the log shows `outbound skipped session ...: project is outside projectRoots`. The live topic set is `ntfy-admin topics` or the `topics now` line in the relay log; the `topics=` field in `status` counts routing state, not the subscription.

## Stop

Stop in reverse order: relay, container, machine. The watchdog restarts a stopped relay within five minutes, so disable it first for a planned stop.

```powershell
Disable-ScheduledTask -TaskName ntfy-relay-watchdog
& "$env:NTFY_RELAY_HOME\ntfy-admin.exe" stop

podman stop ntfy
podman machine stop
```

`ntfy-admin stop` posts to the control endpoint. The daemon finishes the current message, drops the queue with one log line, joins the outbound and SSE tasks with a 5 s grace and a 6 s bound per task, and removes the lock and the endpoint file; the CLI waits up to 15 s for the pid and reports `still running` otherwise. A force stop is immediate and leaves the lock file behind; the next start replaces a readable dead pid once. A message in flight can be lost or repeated: restart within the 60-minute replay window. The Windows binary is windowless, so a foreground run receives no Ctrl+C. If the endpoint does not answer, the fallback on Windows is `Stop-Process -Id <pid> -Force`, which sends no signal; on Linux `kill <pid>` sends SIGTERM and follows the same clean stop path.

`podman stop ntfy` stops only the server; the relay keeps running and retries. `podman machine stop` shuts down WSL2, the container, and `wslrelay`, so stop the relay first. Stopping the private network client takes the phone route down as well; `Stop-Service Tailscale` needs an elevated shell.

Re-enable the watchdog after the next start, or leave it disabled if the stack is meant to stay down:

```powershell
Enable-ScheduledTask -TaskName ntfy-relay-watchdog
```

## Restart

Restart a hung server with `podman restart ntfy`; a config or image change needs `& "$env:NTFY_RELAY_HOME\ntfy-deploy.exe" apply`, which recreates the container. A new relay binary or config needs `ntfy-admin stop` followed by `ntfy-relay.exe ensure`, or a re-run of `ntfy-deploy install` from the release folder. A token rotation restarts the relay through `tools\rotate-token.ps1 -RestartRelay`. After a reboot the logon task starts the stack; nothing needs to be started by hand.

## Ubuntu

Native ntfy with systemd:

```bash
systemctl status ntfy --no-pager
sudo systemctl start ntfy
sudo systemctl stop ntfy
journalctl -u ntfy -f
```

Rootless Podman:

```bash
podman ps -a --filter name=^ntfy$
podman start ntfy
podman stop ntfy
podman logs --tail 50 ntfy
```

`ntfy-deploy install` writes the systemd user units on Linux: it places the four binaries, writes `ntfy-relay-watchdog.service` and its timer, and `ntfy-relay-startup.service`, then enables both. The startup unit runs `ntfy-relay ensure`; the container returns through `unless-stopped` and `podman-restart.service`, and `apply` stays a deploy step. The Windows tasks do not apply. `ntfy-deploy status` and `ntfy-admin status|topics|stop` work on both platforms.

Alpine and any other host without systemd take the same install with the unit step skipped. The report carries `unit: skipped, no systemd (systemctl not found)` and `warning: no autostart and no watchdog, run ntfy-relay ensure after each boot`, the relay still starts, and the last line reads `result: installed, without autostart`. A host whose `systemctl` answers but whose user manager is not running takes the same path and reports `unit: skipped, no systemd user session (systemctl --user show-environment failed)`. The relay on such a host does not come back by itself after a reboot. The token comes from the host that runs the container, because `ntfy-auth` drives `podman exec` there.

## Logs

| file | holds |
|---|---|
| `logs\relay-ensure.log` | one line per start or failed start through `ensure` |
| `logs\relay-stdout.log`, `logs\relay-stderr.log` | the daemon's own output |
| `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.log` | relay heartbeat, subscription, delivery, errors |
| `podman logs --tail 50 ntfy` | server log |

Tail the relay log with `Get-Content "$env:NTFY_RELAY_HOME\ntfy-relay.log" -Tail 20 -Wait`. The relay log rotates at 5 MB; the ensure, stdout and stderr logs rotate at 1 MiB with one `.1` copy. The stdout and stderr logs are 0600 on Unix.
