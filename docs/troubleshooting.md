# 8. Troubleshooting

GitHub 504 on release downloads: use the API asset endpoint with `Accept: application/octet-stream`; resolve the asset ID from the release JSON first.

`podman` not recognized on Windows: the shell predates the installer PATH update. Prepend `$env:LOCALAPPDATA\Programs\Podman` or open a new shell.

Healthcheck missing from the image: the build used the OCI format. Rebuild with `--format docker`.

Container cannot bind port 80: the Windows machine is rootless. Keep the container port at 8080 and publish a high host port, or switch with `podman machine set --rootful`.

The phone cannot reach the Windows container: check the actual listener. If it is `::1` only, a TLS terminator is the clean fix. If a portproxy is used, it must be `v4tov6` with `connectaddress=::1`; a v4tov4 rule to `127.0.0.1` gives connection reset.

Do not add an inbound firewall rule for the private address. The VPN client's inbound rule already allows it. Adding another rule only widens the surface if the listener ever binds to other interfaces.

The TLS terminator refuses to start: certificates must be enabled for the private DNS name, and the terminator must be enabled for the node. A failed certificate request confirms it is off.

Android notifications are delayed: confirm the F-Droid build is installed, the VPN client is connected, the "Subscription Service" notification is present, and ntfy is exempt from battery optimization.

Android still shows Firebase: the Play Store build is installed. Uninstall it and install the F-Droid build.

Permission denied on `cache.db` or attachments: the image runs as uid 1000. Chown the cache volume to `1000:1000` while the container is stopped; do not run the container as root. A volume from the earlier root image needs that one-time chown.

Port already in use on Windows: check `Get-NetTCPConnection -LocalPort 8810 -State Listen`; stop the old container before republishing.

Container does not return after `podman machine start`: check the restart policy and the user unit.

```powershell
podman inspect ntfy --format '{{.HostConfig.RestartPolicy.Name}}'
podman machine ssh 'systemctl --user is-enabled podman-restart'
```

Expected: `unless-stopped` and `enabled`. Linger must be on for the machine user; the image sets `Linger=yes`.

`systemctl --user enable podman-restart` fails with `Access denied`: `~/.config/systemd` is owned by root in the machine image.

```powershell
podman machine ssh 'sudo chown -R user:user /home/user/.config/systemd'
podman machine ssh 'systemctl --user enable --now podman-restart'
```

Containers disappear after `podman machine stop`: expected. The VM is the container host. Use the restart policy plus `podman-restart`, and start the machine again.

`401` or `403` on publish or poll: auth is on and the request has no token. Use `-H "Authorization: Bearer <token>"`, or basic auth with an empty username and the token as the password. `/v1/health` stays open.

Phone says `not authorized` after a token change: the app's user form sends basic auth as `username:password`, and a token in the password field returns `401`. Delete the app user and set the token as a custom `Authorization: Bearer tk_...` header for that server. The app cannot have both a user entry and an `Authorization` header for the same server, and `phone` has no password to fall back on.

Web UI login: none is available. The page loads, but `phone` has no password and the browser interface is out of scope; the app is the client. If a login is ever wanted, set a password inside the container with `podman exec -e "NTFY_PASSWORD=<new>" ntfy ntfy user change-pass phone`. `reset-pass` needs SMTP and is not usable here.

Startup check missing after logon: confirm the shortcut exists in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup` and that its target is the PowerShell script path. Task Scheduler registration fails with `Access is denied` without elevation; the Startup shortcut is the supported path.

Relay not receiving: run `& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" ensure` and read `logs\relay-ensure.log`. A failed start points at `logs\relay-stderr.log` (usually a bad config). A missing token file is not a failure: the relay logs `token: ... is missing or too short; waiting for ntfy-auth apply` and attaches the file when it appears, so check `ntfy-auth status` rather than the start. The relay log is `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.log`; a healthy process prints `subscribed` and a `heartbeat` line every 5 minutes. If the lock pid is stale, ensure replaces it once.

Rust daemon starts with `opencode: no service registration at ...` or `service registration ... points at pid ..., which is not running`: this is one WARN, not an exit. The relay starts degraded on `127.0.0.1:1` and reloads the registration on the first retry after OpenCode appears, so a logon start ahead of OpenCode recovers without the watchdog. The shared registration lives at `~/.local/state/opencode/service.json` and carries the URL, pid, and password. `service.json` under `~/.config/opencode` is only the explicit-server password. The live config has no `baseUrl`; the Rust daemon is selected since 0.4.2.

Rust daemon logs `service registration ... points at pid ..., which is not running` after a reboot: the relay started before OpenCode rewrote `service.json`, so the file still named a pid from the previous boot. The WARN is expected; the relay starts degraded and picks the registration up when OpenCode appears. If routing stays down after OpenCode is up, check the registration and the log for `rediscovered`.

Startup and repair run the relay through `ntfy-relay ensure` and verify the process against the lock pid. Keep `ntfy-relay.exe` in the install root (`%LOCALAPPDATA%\ntfy-relay\`); the watchdog and the logon task match that image name.

Phone messages arrive in an old session: routing follows the desktop's most recent activity, so a session that only the phone has touched can stay the newest by `time.updated`. Type a message in the intended chat on the host; merely opening it does not update `time.viewed`. A one-shot target from `/new` or `/to` expires after 10 minutes or one message; `/auto` drops it early.

Empty console window from the relay: that was the cmd host of the old WMI spawn, and closing it killed the relay; stderr showed `^C`. The launcher was removed in 0.6.0 and the current start has no visible window. Do not close one if it appears; restart through `ntfy-relay ensure`.

Relay dies between logons: the `ntfy-relay-watchdog` scheduled task checks every 5 minutes and runs `ensure` when the lock pid is dead. Read `logs\relay-ensure.log` (one line per start or failure). The task is hidden and runs only while the user is logged on.

Shell window flashing every 5 minutes: the task action is no longer `ntfy-relay.exe ensure`. Re-run `ntfy-deploy install` from the release folder to rewrite both tasks. The installer writes the action and the hidden setting together.

File name opens a scam site on the phone: the client linkifies any domain-shaped token, and a name such as `readme.md` points at the `.md` country domain. Pushes now show `readme[.]md`; the Rust path defangs everything outside the allowlist. If a bare file name becomes clickable again, the running path skipped the filter: check `core::link_safe` in the daemon and restart the relay.

Host in modern standby: `powercfg /a` reports S0 Low Power Idle, Network Disconnected on this machine (Kernel-Power event 172 confirms disconnection by policy). Resume is instant; a long sleep can require a login. The relay's TCP stream can survive, so no `subscribed` line appears after wake; a 27-minute sleep resumed without one. The Podman VM, the ntfy server, and the relay are frozen while asleep, therefore a phone publish during standby fails and is not stored; resend after wake. Wake-on-LAN is not a remote path: magic packets are link-local and the private network is L3. Keep the host awake on AC when remote notifications must be continuous.
