# 10-deploy-mode.md - deployment ownership and network switching

## Header

- Status: spec final; migration executed 2026-11-14, the crate is implemented.
- Scope: one cross-platform helper, `ntfy-deploy`, that switches the ntfy server between proxy, VPN, and LAN modes by recreating the container with environment variables and maintaining the Windows portproxy. It never manages the relay daemon.
- Supersedes: the baked `base-url` rebuild path in `container\server.yml` and `tools\build-image.ps1`.
- Related: `00-overview.md` (file ownership), `06-rust-crate-split.md` (crate boundaries), `docs\https.md` (current proxy route), `docs\switching-networks.md` (operator workflow).

## Ownership rule

Each product owns its files. The relay root holds the relay config, token files, deploy JSON, state, asks registry, journal, and logs. No relay-owned file lives under `~/.config/opencode` or `~/.local/state/opencode`. OpenCode is reached only through its service registration and HTTP API at runtime.

- One path only. No fallback reads, no dual config, no compatibility shim, no legacy path check.
- The old OpenCode paths are deleted in the same change that introduces the relay root.
- No cross-product file dependency is allowed for convenience. A design that needs one is rejected before code.
- The deploy tool reads no OpenCode file and calls no OpenCode endpoint.

## Install root

- Windows: the directory holding `ntfy-relay.exe`, currently `bin\ntfy-relay\`.
- Linux: the install directory holding the binary, for example `/opt/ntfy-relay`.
- Override for tests only: `NTFY_RELAY_HOME`. Resolution is strict; a missing root fails, it does not fall back.
- No file in this root is tracked in git. Tokens are `0600` on Unix.

Contents:

| file | purpose |
|---|---|
| `ntfy-relay.json` | daemon config |
| `ntfy-deploy.json` | deployment config for this spec |
| `ntfy-auth.json` | auth bootstrap config for spec 11 |
| `ntfy-token.txt` | the only token, read by the daemon and the MCP; the app keeps its copy |
| `ntfy-relay-state.json` | routing hints, state |
| `ntfy-relay-asks.json` | ask registry |
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
    "image": "localhost/ntfy:2.37.2",
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

## Image contract

- `container\server.yml` drops `base-url` and `behind-proxy`. Every deployment supplies `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY` at container creation.
- The rest of the file stays baked: auth file, cache, attachments, deny-all default.
- A mode switch recreates the container. It never rebuilds the image.
- A rebuild is needed only for a new ntfy version or a change to the baked keys.
- Build source: the tracked context holds the Dockerfile, `server.yml`, and `.dockerignore`. A build stage fetches the pinned ntfy release tarball and checks it against `NTFY_SHA256` before the binary enters a layer, so the tree holds no binary and no download step precedes the build. A version change updates `NTFY_VERSION` and `NTFY_SHA256` together.
- Reference: `localhost/ntfy:<ntfy version>`, plus `:latest`. `tools\build-image.ps1` reads the version from the Dockerfile, so tag and pin cannot drift. When a rebuild keeps the tag, the image-ID rule in `apply` replaces the container on the next run.
- Distribution: no registry. A release ships one `podman save --format docker-archive` archive per claimed architecture, named `ntfy-image-<ntfy version>-<arch>.tar` and carrying the version tag and `latest`; `podman load` restores both without a build, account, or network.
- Architecture: amd64 is claimed, and the pinned base digest and release checksum name it. Another architecture needs its own base digest and release checksum, and ships only after a live run on that architecture.

## Modes

| | proxy | vpn | lan |
|---|---|---|---|
| `baseUrl` | `https://<name>` | `http://<vpn-ip>:8810` | `http://<lan-ip>:8810` |
| `behindProxy` | true | false | false |
| container publish | loopback | loopback | LAN interface or all |
| Windows portproxy | loopback only | loopback plus VPN IP | none |
| ingress | TLS terminator on 443 | VPN client | LAN |
| phone URL | https name | http VPN IP | http host |

Operator steps, not performed by the tool:

- Start or stop the TLS terminator or the VPN client.
- Update the phone server URL after a mode switch.
- Keep the token and topic unchanged.

## Commands

`ntfy-deploy apply [--config <path>] [--dry-run]`

1. Read and validate the config. Refuse unknown fields and unknown modes.
2. Compute the container environment and the portproxy set for the mode.
3. Stop and remove the container.
4. Run it with the image, cache volume, publish rule, and env pair.
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

Idempotency: a second apply with the same config produces the same state and does not recreate a healthy container with matching arguments. Argument matching includes the image content: the container's image ID from `podman inspect` must equal the local image ID for the configured reference, so a rebuild that keeps the tag is rolled out by the next `apply`. When either ID is unavailable the reference comparison alone decides. `apply` and `status` never write the config; only `mode` does.

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
- Portproxy mapping: proxy removes the VPN entry, VPN adds it, LAN leaves none.
- Container argument construction: image, name, volume, publish, env pair per mode.
- Dry-run emits commands and changes nothing.
- Status output for each mode.
- Idempotent apply.
- Image drift: a matching reference with a different image ID is recreated, a matching ID is reused, and a missing ID falls back to the reference.
- Detection parsing: `netsh` interface blocks, `ip -4 -o addr` rows, the default route device, and the Tailscale status JSON.
- Choice building: a stopped Tailscale is not offered, a Nord tunnel address carries a Meshnet note, and LAN uses the default interface.
- Mode writing: every choice round-trips through parse and validate, and the document keeps the example shape.
- Relay attachment host: the server host plus the deploy file host, deduplicated, and a missing or invalid file leaves the server host only.
- Grep test: no `\.config\opencode` or `.local/state/opencode` path in the crate or deploy config.

## Non-goals

- No relay lifecycle, restart, or supervision. That is the bridge boundary.
- No writes to OpenCode config or state.
- No edit of the relay config at runtime.
- No auth, cache, or attachment changes.
- No writes to `ntfy-deploy.json` outside the `mode` command.
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
