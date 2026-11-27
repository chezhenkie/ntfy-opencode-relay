# Requirements

The conditions a host, network and client must meet for the relay to run. Install steps are in [docs/index.md](docs/index.md); the design is in [docs/specsheet/00-overview.md](docs/specsheet/00-overview.md). A statement here is a requirement on the environment, not a description of behaviour.

## Server host

- Podman 5.x, rootless, on Windows through a WSL2 machine or on Linux. Docker runs the same image, but `ntfy-deploy` drives Podman, so a Docker host runs the container and compose commands itself.
- A linux/amd64 host or emulation; the released image is amd64.
- A loopback publish of 8810 to container 8080. The container runs read-only as uid 1000 with tmpfs `/tmp`, no added capabilities and no-new-privileges.
- A cache volume for the message cache and attachments. Messages and attachments expire after 72 h; attachments are capped at 1 GiB total and 40 MiB per file.
- 10 MiB of log space per container. `ntfy-deploy` selects the k8s-file driver because the Podman default journald ignores the cap.
- A synchronized clock, because message and attachment expiry use wall time.

## Network and TLS

- A private path from the phone to the host: Tailscale, Nord Meshnet or the local network. No public exposure, no router forward and no inbound firewall rule; the private network ACL stays deny by default.
- TLS on 443 in front of `127.0.0.1:8810`. The certificate must chain to a public root, because the relay validates with the rustls public store; a private CA is unsupported.
- A base URL the phone can reach over the private network and a certificate that matches its host. ntfy builds message links and the WebSocket URL from that base.
- LAN mode is the exception: plain HTTP on 8810, a private IPv4 literal in the base URL, and no certificate.

## Phone

- Android with the F-Droid build of the ntfy client and WebSockets. No Firebase key, no Play services and no Google push path; the PWA is not a background path.
- The `phone` principal and its token. The app, the relay and MCP share one token, kept under the relay root and `0600` on Unix.

## Relay host

- Windows 10 or 11, or Linux. On Windows `install` registers a logon task and a five-minute watchdog task through `schtasks`. On Linux it writes three systemd user units when a user session exists, and otherwise reports that no autostart and no watchdog were installed.
- Four executables from the release in one folder, with `NTFY_RELAY_HOME` naming the install root or `%LOCALAPPDATA%\ntfy-relay` as the default.
- An install root under `%LOCALAPPDATA%` or the user's home. Config, token, state, asks, forms, journal and logs live there; `install` warns about any other location.
- Outbound HTTPS to the ntfy origin and HTTP to the OpenCode API on the same host.
- On Windows, a running Podman machine before the relay starts; the startup script runs `podman machine start` and then `ensure`.

## OpenCode

- OpenCode 2.2.7 or newer, with its service registration present. The relay registers `mcp.servers.ntfy` as `<root>/ntfy-relay mcp`.
- An HTTPS origin in `opencode.baseUrl`; plain HTTP is accepted only for `127.0.0.1`, `::1` or `localhost`.
- The API surface the relay uses: `/api/event`, session create, prompt, message, permission and form routes.

## Build and tests

- Rust stable with rustfmt and clippy, as pinned in `rust\rust-toolchain.toml`; four packages build from `rust\`.
- An `x86_64-pc-windows-gnu` build needs `x86_64-w64-mingw32-windres`; an MSVC build needs `rc.exe` from the Windows SDK. The version comes from `package.json` and the workspace `Cargo.toml`.
- Node 20 or newer for the `pins`, `tlds`, `links`, `secrets` and `site` checks.
- The 746 workspace tests run offline. The smoke suite needs a live stack, the configured token and `e2e-smoke-tests\`; cleanup needs `--cache-db` with a local cache file.
- A container engine, network access and the pinned ntfy release to build the image.

## Repository rules

- Tracked markdown is ASCII, and every relative link resolves.
- No token, tailnet name or real absolute path in a tracked file; `npm run secrets` enforces both.
- Version 0.6.13, patch releases only, and every action pinned to a commit SHA.
