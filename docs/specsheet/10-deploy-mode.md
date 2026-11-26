# 10-deploy-mode.md - deployment ownership and network switching

## Header

- Status: spec final; migration executed 2026-11-14, the crate is implemented.
- Scope: one cross-platform helper, `ntfy-deploy`, that creates the first deploy config with `init`, switches the ntfy server between proxy, VPN, and LAN modes by recreating the container with environment variables, and maintains the Windows portproxy. `install` and `remove` place the binaries, register the startup entries, and stop or start the relay; `apply`, `mode`, and `init` never touch the daemon.
- Supersedes: the baked `base-url` rebuild path in `container\server.yml` and `tools\build-image.ps1`.
- Related: `00-overview.md` (file ownership), `06-rust-crate-split.md` (crate boundaries), `docs\https.md` (current proxy route), `docs\switching-networks-user-manual.md` (operator workflow).

## Ownership rule

Each product owns its files. The relay root holds the relay config, token files, deploy JSON, state, asks registry, journal, and logs. No relay-owned file lives under `~/.config/opencode` or `~/.local/state/opencode`. OpenCode is reached only through its service registration and HTTP API at runtime.

- One path only. No fallback reads, no dual config, no compatibility shim, no legacy path check.
- The old OpenCode paths are deleted in the same change that introduces the relay root.
- No cross-product file dependency is allowed for convenience. A design that needs one is rejected before code.
- The deploy tool reads no OpenCode file and calls no OpenCode endpoint.

## Install root

- Windows: the directory holding `ntfy-relay.exe`, by default `%LOCALAPPDATA%\ntfy-relay\` or the path in `NTFY_RELAY_HOME`.
- Linux: the install directory holding the binary, for example `/opt/ntfy-relay`.
- Override for staging and tests: `NTFY_RELAY_HOME`. The installer sets it to the install root; resolution is strict, and a missing root fails without a fallback.
- No file in this root is tracked in git. Tokens are `0600` on Unix. On Windows there is no owner-only mode: the token, configs, and runtime file rely on the root's inherited per-user ACLs, so the root must sit under `%LOCALAPPDATA%`; `install` warns when it does not, and the `status` `acl` row names the root.

Contents:

| file | purpose |
|---|---|
| `ntfy-relay.json` | daemon config |
| `ntfy-deploy.json` | deployment config for this spec |
| `ntfy-auth.json` | auth bootstrap config for spec 11 |
| `ntfy-token.txt` | the only token, read by the daemon and the MCP; the app keeps its copy |
| `ntfy-relay-state.json` | routing hints, state |
| `ntfy-relay-asks.json` | ask registry |
| `ntfy-relay-forms.json` | pushed question forms and tombstones |
| `ntfy-relay-endpoint.json` | control endpoint port and per-process token |
| `ntfy-relay-seen.jsonl` | journal |
| `ntfy-relay.log` | daemon log |

## Deploy config

```json
{
  "version": 1,
  "mode": "vpn",
  "baseUrl": "http://100.64.0.10:8810",
  "behindProxy": false,
  "port": 8810,
  "container": {
    "name": "ntfy",
    "image": "localhost/ntfy:server",
    "cacheVolume": "ntfy-cache"
  },
  "vpn": {
    "ip": "100.64.0.10"
  }
}
```

- `mode`: `proxy`, `vpn`, or `lan`. Unknown values fail validation.
- `baseUrl`: no path, no trailing slash. `https` for `proxy`; `http` allowed only for `vpn` and `lan`.
- `behindProxy`: true only for `proxy`, where a TLS terminator sits in front of ntfy.
- `vpn.ip`: required in `vpn` mode; the Windows portproxy listens on it.
- Credentials are never copied into this JSON.

## Relay contract

The relay always talks to ntfy through loopback, in every mode:

- `ntfy.server`: `http://127.0.0.1:<port>`, which the config already permits on loopback.
- A mode switch never edits the relay config, never restarts the relay, and never changes the token.
- The proxy, VPN, and LAN paths only change how the phone and other clients reach the server. The daemon path stays local.
- This keeps the MCP processes and the outbound publisher alive through a switch.
- The inbound attachment host rule accepts the deployment `baseUrl` host, because the server emits attachment URLs with that host. The relay reads only `baseUrl` from `ntfy-deploy.json` in its own root at each attachment fetch, adds that host to the `ntfy.server` host, and then fetches the hop through the configured server base. It never opens a connection to the deployment host. A missing or invalid deploy file leaves the server host only, and a mode switch needs no relay restart.

## Mode switch from the control path

A mode switch is never started by the relay. The relay validates the request and returns a plan: the resolved mode, the deploy command line, and the phone URL it will produce. The elevated CLI runs `ntfy-deploy mode` itself, and deploy alone writes `ntfy-deploy.json` (`init` creates it, `mode` rewrites it). A daemon that spawned an elevated child would raise a UAC prompt from a background process, where no one can answer it, and on Linux would need a sudo terminal; the plan keeps the relay unprivileged and puts the failure in the shell the user is watching. The relay never runs elevated, and a switch still leaves its config and token untouched.

## Image contract

- `container\server.yml` drops `base-url` and `behind-proxy`. Every deployment supplies `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY` at container creation.
- The rest of the file stays baked: auth file, cache, attachments, deny-all default.
- A mode switch recreates the container. It never rebuilds the image.
- A rebuild is needed for a new ntfy version or a change to the baked keys, and it never waits for a relay release.
- Build source: the tracked context holds the Dockerfile, `server.yml`, and `.dockerignore`. The Dockerfile is a standard one; Podman builds it directly, so there is no second builder file, and `--format docker` selects the image format that keeps the healthcheck. A build stage fetches the pinned ntfy release tarball and checks it against `NTFY_SHA256` before the binary enters a layer, so the tree holds no binary and no download step precedes the build. A version change updates `NTFY_VERSION` and `NTFY_SHA256` together.
- Engine: the artifact is engine-neutral. The Dockerfile is standard, the saved archive is docker-archive, and either `podman load` or `docker load` restores the same `localhost/ntfy:server` tag. `ntfy-deploy` currently runs `podman`; only the `status` rows honor `NTFY_PODMAN_BIN`. Docker is a manual route until engine detection and installation are designed (open-work 10.10).
- Reference: `localhost/ntfy:server`, plus `:latest` as an alias. The tag is stable across ntfy versions; `tools\build-image.ps1` reads the version pin only to report it. When a rebuild keeps the tag, the image-ID rule in `apply` replaces the container on the next run, so a server bump is a rebuild and an apply, never a relay release.
- Distribution: no registry. A release ships one `docker save` archive per claimed architecture, named `ntfy-image-<ntfy version>-<arch>.tar` and carrying the `server` tag and `latest`; `podman load` restores both without a build, account, or network, and the docker-archive format also loads in Docker.
- Architecture: amd64 is claimed, and the pinned base digest and release checksum name it. Another architecture needs its own base digest and release checksum, and ships only after a live run on that architecture.

## Modes

| | proxy | vpn | lan |
|---|---|---|---|
| `baseUrl` | `https://<name>` | `http://<vpn-ip>:8810` | `http://<lan-ip>:8810` |
| `behindProxy` | true | false | false |
| container publish | Windows VM `8810:8080`, Linux loopback | all interfaces | all interfaces |
| Windows portproxy | loopback only | loopback plus VPN IP | loopback only |
| ingress | TLS terminator on 443 | VPN client | LAN |
| phone URL | https name | http VPN IP | http host |

Operator steps, not performed by the tool:

- Start or stop the TLS terminator or the VPN client.
- Update the phone server URL after a mode switch.
- Keep the token and topic unchanged.

## Commands

`ntfy-deploy init --mode <tailscale|nord|lan> --base-url <url> [--config <path>]`

- Writes `ntfy-deploy.json` once, with version 1, port 8810, container `ntfy`, image `localhost/ntfy:server`, cache volume `ntfy-cache`, `behindProxy` true only for `tailscale`, and `vpn.ip` from the base URL host in `nord` mode.
- Refuses with exit 2 when the file exists; creation uses an exclusive open, so a racing second call loses.
- The base URL is validated before the write: a proxy mode needs HTTPS, a plain HTTP URL needs a loopback or private IPv4 host and the configured port, and `nord` needs an IPv4 host.
- Prints `config written: <path>` and `next: ntfy-deploy apply`. It never touches the relay or the container.

`ntfy-deploy apply [--config <path>] [--dry-run]`

1. Read and validate the config. Refuse unknown fields and unknown modes.
2. Compute the container environment and the portproxy set for the mode.
3. Stop and remove the container.
4. Run it with the image, cache volume, publish rule, hardening, a 10 MiB log cap, and the env pair.
5. Ensure the Windows loopback bridge, then wait for `/v1/health` on loopback. Do not continue before a 200. The loopback rule is a precondition because the relay and the terminator use it in every mode.
6. Apply the remaining portproxy changes last: add or remove the VPN entry.
7. Print the resulting state.

`--dry-run` prints every command and makes no change.

`ntfy-deploy status`

- Print mode, base URL, container state, loopback health, and the active portproxy or listener.
- Exit codes: `0` ok, `2` invalid config, `3` unhealthy, `4` platform command failed.

`ntfy-deploy mode [tailscale|nord|lan] [--config <path>] [--dry-run]`

- With no target it prints the detected paths as a numbered menu and reads one number; an empty line cancels and writes nothing.
- A target chooses that path directly. An unavailable target names what was detected and exits `2`.
- Detection reads interface addresses from `netsh interface ipv4 show addresses` on Windows, from `ip -4 -o addr show` plus the default route on Linux, and the node name and IPv4 from `tailscale status --json`. A stopped VPN or a missing tool removes its entry; it is not an error.
- The choices are `tailscale` (proxy, `https://<MagicDNS name>`), `nord` (vpn, `http://<address>:<port>`, preferring a `100.64/10` Meshnet address and noting a tunnel-only address), and `lan` (lan, `http://<default-interface address>:<port>`).
- A path that cannot be reached from both ends is still offered when its address exists, with a note: Tailscale and Meshnet share `100.64/10`, so only one should be active.
- The tool writes `ntfy-deploy.json` with the chosen mode, base URL, `behindProxy`, and `vpn.ip`, proves the exact text parses and validates, then runs the same apply path. `--dry-run` prints the document and the plan and writes nothing.
- It prints the resulting phone server URL. On Windows the portproxy step needs an elevated shell; a platform failure is exit `4` and the tool says so.
- The relay is not a party: no relay config is read, nothing is restarted, the token is unchanged.

Idempotency: a second apply with the same config produces the same state and does not recreate a healthy container with matching arguments. Argument matching includes the image content and the hardening set: the container's image ID from `podman inspect` must equal the local image ID for the configured reference, the container must run with `cap-drop=ALL`, `no-new-privileges` and the `/tmp` tmpfs, and its log driver must be `k8s-file` or `json-file`. A rebuild that keeps the tag is rolled out by the next `apply`. When either ID is unavailable the reference comparison alone decides. `apply` and `status` never write the config; `init` creates it once and `mode` rewrites it.

## Installer

`ntfy-deploy install [--dry-run]` and `ntfy-deploy remove [--dry-run]`. No fifth binary: the installer is the deploy binary, run from the release folder that holds the four executables. It never creates a container, never runs the auth bootstrap, and never writes the deploy config; `init` is the only creation path. A root with no token is not an install failure: the relay waits for the token file and answers `running pid=<n>` to the final `ensure`, and `ntfy-auth apply` attaches the token without a restart.

Root: `NTFY_RELAY_HOME` when set, else `%LOCALAPPDATA%\ntfy-relay` on Windows and `~/.local/share/ntfy-relay` on Linux. Windows sets the user variable to the root with `setx` and skips it when `reg query HKCU\Environment` already holds the value. Linux leaves the variable unset; the relay finds its root beside the binary.

Install:

1. Refuse when one of the four executables is missing beside the installer, before any change.
2. Copy an executable only when its bytes differ; keep the outgoing image as `<stem>.previous.exe`, one generation. A failed copy restores the backup. The running relay keeps its image, so the file can be replaced while it runs. A running MCP process can hold the canonical previous name as its own image, and that file cannot be deleted: the outgoing image then moves to the next free `<stem>.previous.N.exe` from 2 to 9, and the report names the file it used.
3. Stop the relay through the installed `ntfy-admin stop`; exit 0 stopped, 1 still running after the CLI's 15 s budget, and 3 no running relay are accepted. On exit 1 only, the installer force-kills the pid named in the endpoint file when that pid still runs a relay image under the install root; a stale endpoint or a foreign pid records `stop: not killed, endpoint stale` and is left alone. Then it starts the relay with `ntfy-relay ensure`, with output discarded so a detached daemon cannot hold the installer's pipe open. A dry run does neither.
4. Write `ntfy-relay.json` and `ntfy-auth.json` only when absent, with the default server `http://127.0.0.1:8810`, outbound on, an empty root list, and the `phone`/`*`/rw auth shape. `ntfy-deploy.json` is never written; `init` creates it once and `mode` rewrites it.
5. Write the Windows startup script `<root>\ntfy-relay-startup.vbs` and register the Windows tasks below, or install the Linux units below.
6. Delete the installed script folder (`relay-windows-scripts` or `relay-linux-scripts`) and the legacy startup shortcut `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\ntfy startup check.lnk` when present.

Windows tasks, both with the current user's interactive token, least privilege, `IgnoreNew`, `StartWhenAvailable`, allowed on battery, hidden, a 10 minute execution limit, and the root as working directory:

- `ntfy-relay-watchdog`, a time trigger with a `PT5M` repetition and no end date, runs `<root>\ntfy-relay.exe ensure`.
- `ntfy-relay-startup`, a logon trigger bound to the current account, runs `<system>\wscript.exe //B //NoLogo "<root>\ntfy-relay-startup.vbs"`. The script runs `<podman path> machine start` and `<root>\ntfy-relay.exe ensure` in order with `WshShell.Run(..., 0, True)`, so every console is created hidden. Podman's exit code is ignored because an already running machine is reported as an error; an ensure failure becomes the task result. The container returns through `unless-stopped` and `podman-restart.service`; `apply` is a deploy step and does not run at logon.

The installer generates the XML and the script and registers the task with `schtasks /Create /TN <name> /XML <file> /F`, then removes the temp file. The file is UTF-16 LE with a BOM; `schtasks` rejects the declaration without it. A re-run is the repair step and rewrites the script.

Linux units in `~/.config/systemd/user/`:

- `ntfy-relay-watchdog.service` is a oneshot that runs `<root>/ntfy-relay ensure`; `ntfy-relay-watchdog.timer` starts it at `OnBootSec=1min` and repeats at `OnUnitActiveSec=5min`, wanted by `timers.target`.
- `ntfy-relay-startup.service` is a oneshot wanted by `default.target`; it runs `<root>/ntfy-relay ensure`. The container returns through `unless-stopped` and `podman-restart.service`; `apply` is a deploy step and does not run at login.

Install runs `systemctl --user daemon-reload`, then `enable --now ntfy-relay-watchdog.timer` and `enable ntfy-relay-startup.service`.

Two read-only probes decide it: `systemctl --version`, then `systemctl --user show-environment`. A host without systemd is not an install failure. When `systemctl --version` cannot run or exits non-zero, install writes no unit, runs no `systemctl`, reports `unit: skipped, no systemd (systemctl not found)` and `warning: no autostart and no watchdog, run ntfy-relay ensure after each boot`, places the binaries, starts the relay, and ends with `result: installed, without autostart`. When the binary answers but the user manager does not, install writes no unit, runs no enable or reload, reports `unit: skipped, no systemd user session (systemctl --user show-environment failed)` with the same warning, and ends the same way. Remove takes the same probes, skips the disable calls and the reload, and deletes a unit file that is present. The relay on such a host runs until the next reboot and must be started by hand after it. `ntfy-auth` is not usable there either: it drives `podman exec` on the host that runs the ntfy container, so the token file is copied to the relay host instead.

Remove stops the relay through the installed admin (the same 0, 1, and 3 rules), deletes both tasks or disables the timer and the startup unit, deletes the unit files and reloads, deletes the startup script, the legacy shortcut and the installed script folder, deletes the four executables and their `.previous` copies, and removes the Windows variable. It keeps every other file in the root: configs, token, state, asks, journal, log, and `logs\`. A previous copy that a running MCP process still holds is reported as `kept` and does not fail the removal.

Exit codes: 0 ok, 2 refused or file failure, 4 platform command failed. `--dry-run` prints the plan and changes nothing.

## Platform behavior

Windows:

- Keep the loopback bridge `127.0.0.1:8810 -> [::1]:8810` in every mode.
- `vpn`: add `<vpn.ip>:8810 -> [::1]:8810`.
- `proxy`: remove the VPN entry.
- `lan`: loopback only.
- `netsh` requires elevation. A failure is reported as exit `4`; the tool does not silently skip the rule.
- Firewall: rely on the private network inbound allow. Add a scoped rule only when the listener is confirmed and the phone still fails.

Linux:

- No portproxy and no WSL bridge.
- `proxy`: publish on loopback, with the TLS terminator in front.
- `vpn` and `lan`: publish `8810:8080` on all interfaces. The relay contract keeps `ntfy.server` on loopback in every mode, so the published port has to include loopback; deny-all auth and the private-interface firewall bound the exposure.
- Firewall: allow TCP 8810 on the private interface only when the distro firewall is enabled.

## Crate

- New internal crate `rust\crates\deploy`, package `ntfy-relay-deploy`, binary `ntfy-deploy`.
- No dependency on other workspace crates. Local modules for config, validation, platform, and command execution.
- External dependencies: `serde` and `serde_json` only.
- `CommandRunner` trait with a real runner and a fake used by tests. CI does not run Podman or netsh.
- Platform code behind `#[cfg(windows)]` and `#[cfg(unix)]`; common code stays platform-neutral.

## Tests

- Config parse, required fields, unknown mode, unknown field.
- URL rules: https any host, http only on a loopback, VPN, or LAN IP literal, no path, no trailing slash, and an HTTP port equal to `port`.
- Portproxy mapping: proxy removes the VPN entry, VPN adds it, LAN leaves the loopback entry only.
- Container argument construction: image, name, volume, publish, hardening, log cap, env pair per mode.
- `init`: one write per path, refusal when the file exists, validation before the write.
- Dry-run emits commands and changes nothing.
- Status output for each mode.
- Idempotent apply.
- Image drift: a matching reference with a different image ID is recreated, a matching ID is reused, and a missing ID falls back to the reference.
- Detection parsing: `netsh` interface blocks, `ip -4 -o addr` rows, the default route device, and the Tailscale status JSON.
- Choice building: a stopped Tailscale is not offered, a Nord tunnel address carries a Meshnet note, and LAN uses the default interface.
- Mode writing: every choice round-trips through parse and validate, and the document keeps the example shape.
- Installer: release-file refusal, byte-equal skip, backup and restore, missing-only config writes, task and unit text, shortcut and variable handling, remove list, dry-run, `whoami` SID parsing, `reg query` matching, and admin exit 3.
- Relay attachment host: the server host plus the deploy file host, deduplicated, and a missing or invalid file leaves the server host only.
- Grep test: no `\.config\opencode` or `.local/state/opencode` path in the crate or deploy config.

## Non-goals

- No relay lifecycle in `apply` or `mode`; `install` and `remove` stop and start the relay. Supervision belongs to the bridge.
- No writes to OpenCode config or state.
- No edit of the relay config at runtime.
- No auth, cache, or attachment changes.
- No writes to `ntfy-deploy.json` outside `init` and `mode`.
- No container image build.
- No TLS termination. HTTPS remains a proxy or terminator concern.
- No VPN client control.

## Migration

First next session, before the crate. One shot, no fallback. Executed 2026-11-14; the steps below record the sequence.

1. Stop the relay and every MCP process.
2. Create the relay root. Move `ntfy-relay.json`, `ntfy-token.txt`, state, asks, journal, and log into it. Remove the stale lock and the corrupt state backup.
3. Delete `~/.config/opencode\notification-ntfy-with-questions.json`. It belongs to the removed plugin.
4. Update the daemon default token path, config path, launcher, watchdog, MCP registration, `rotate-token`, `startup-check`, docs, and specs.
5. Confirm no path under `~/.config/opencode` or `~/.local/state/opencode` is referenced or present for the relay.
6. No fallback read, no legacy path warning, no dual write.

## Acceptance

- `apply` switches proxy to VPN and back, changing only the container env and the Windows VPN portproxy.
- `mode tailscale|nord|lan` writes the config, reaches health 200, and prints the phone URL in one command; an unavailable target exits 2 and writes nothing.
- The relay pid is unchanged through both switches.
- Health is 200 on the loopback and on the mode URL from the phone.
- `status` matches the applied config.
- No relay file remains under the OpenCode config or state directories.
- No code path reads an old location.
