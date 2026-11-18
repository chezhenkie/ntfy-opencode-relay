# ntfy relay

Self-hosted [ntfy](https://ntfy.sh) transport for a phone without Google services, with two-way OpenCode traffic: a push when a run ends or needs an answer, and a phone reply that reaches the running session.

- Three runtime pieces: the phone app, the ntfy server, and the relay. The MCP server is part of the relay, so the framework needs no plugin.
- The relay is the only publisher: it owns the server token and every push, and the framework never talks to the ntfy server.
- Deny-all access with one owner token and a wildcard topic grant. Only `/v1/health` is anonymous, and signup is off.
- No telemetry, no Google, no account. The phone runs the F-Droid build; the server has no Firebase key and no upstream relay.
- The server is reachable only from the private network: no public URL, no Funnel, no router forward.
- `ntfy-deploy mode` switches between Tailscale, Nord Meshnet, and the LAN in one command. The relay keeps running with the same pid and token.
- The phone sends text and files into the running session and answers permission asks.
- Reconnect and restart recover missed assistant text, message-id dedupe prevents duplicate pushes, and inbound delivery is at-most-once.
- Config, tokens, state, and logs live in one relay folder.

## Diagrams

<a href="https://chezhenkie.github.io/ntfy-opencode-relay/"><img src="docs/assets/front-page.png" alt="Flow pages front page: inbound and outbound, commands and tags, ask and permission" width="760"></a>

The [flow pages](https://chezhenkie.github.io/ntfy-opencode-relay/) draw the inbound and outbound routes, the phone commands and tags, and the permission reply.

## What it is

The repository has three runtime parts, with one topic per project folder.

| part | what it does |
|---|---|
| `container\` | ntfy server image with baked config, deny-all auth, and a 72 h cache; base URL and proxy flag supplied at creation |
| `rust\` | Rust workspace: the relay daemon, its MCP tool, and the deploy binary |
| `relay-windows\` | Windows launcher: single-instance lock, hidden spawn, five-minute watchdog |

The link between them is the ntfy topic, which is the project folder name, for example `ntfy-proj`. Outbound pushes carry the `oc-out` tag so the relay ignores them; every other message on the topic is treated as a phone message.

The relay is standalone. The bridge, a future per-environment process manager, is a separate project with no shared code, lifecycle, or process owner. It may start, stop, observe, and address the relay only through `/v1/health`, `service.json`, the token file, the log, and `ntfy-relay status`. Client comms beyond OpenCode are settled: no OpenAI-compatible endpoint, and any additional shape arrives as an additive module over the same command layer. See [specsheet 00](docs/specsheet/00-overview.md) and [specsheet 08](docs/specsheet/08-comms-and-mcp.md).

The source is a Rust workspace under `rust\`, built into three binaries: `ntfy-relay` (`run`, `mcp`, `status`), `ntfy-deploy` (container deployment and the network mode switch), and `ntfy-auth` (the one user, wildcard row, and token). Crate boundaries are in [specsheet 06](docs/specsheet/06-rust-crate-split.md); the build, smoke test, and cutover are in [operations 7.9](docs/operations.md#79-rust-daemon-build-smoke-cutover).

## How a message travels

Host to phone: the Rust outbound task POSTs to the server, the server stores the message and pushes it down the phone's own outbound WebSocket. `ntfy-relay mcp` sends a file on explicit request through the same path.

Phone to host: the phone POSTs to the same server. The relay holds one long-lived SSE connection per topic set, reads the message, and posts it to the OpenCode session as a prompt, with attachments written to `<project>\.ntfy-inbox\`.

Both routes, with diagrams: [docs/specsheet/02-message-routes.md](docs/specsheet/02-message-routes.md).

## Protocols

| Surface | Where | Direction |
|---|---|---|
| REST | ntfy server: `POST /<topic>` to publish, `PUT` for files, `GET /v1/health` | relay to ntfy |
| REST | OpenCode server: `/api/session/<id>/prompt`, `/api/session/<id>/message`, `/api/permission/...` | relay to OpenCode |
| SSE | `GET /api/event` on ntfy, and the OpenCode event stream | long-lived read inside REST |
| MCP | `ntfy-relay mcp` on stdio, one tool `send_file` | OpenCode calls the relay |

REST plus SSE is the data and control plane; MCP is a single-tool interface over the same publish path. There is no OpenAI-shaped endpoint anywhere, and MCP is inbound only in the sense that OpenCode calls the relay; the relay never calls an MCP server itself.

## Requirements

- A host for the ntfy server: the container under Podman, or the native binary under systemd on Linux
- A private network from phone to host, and TLS on 443. The tested path uses a TLS terminator with a certificate for the host name; any equivalent on that network fits. The Rust client trusts the rustls public root store only; a private CA or the native store is not supported.
- The F-Droid-flavor ntfy client on Android, from F-Droid or the GitHub release APK, with no Firebase and no Play services
- Node 20 or newer for the TLD table and doc-link checks
- Rust stable with rustfmt and clippy to build or test the workspace
- OpenCode 2.2.7 or newer with the `ntfy` MCP server

## Quick start

```powershell
# health, anonymous
curl.exe -sS https://ntfy.example.ts.net/v1/health

# publish with the owner token
$token = (Get-Content "$PWD\bin\ntfy-relay\ntfy-token.txt" -Raw).Trim()
curl.exe -sS -H "Authorization: Bearer $token" -H "Title: Test" -d "hello" https://ntfy.example.ts.net/ntfy-proj

# start the relay and check the whole path
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\relay-windows\start-relay.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\startup-check.ps1
```

Setup from an empty host starts at [docs/index.md](docs/index.md). The manual runs from binaries and checksums through the Podman route, the Ubuntu routes, the network path, the HTTPS switch, the Android client, and operations. The phone side is in [docs/phone-app-user-manual.md](docs/phone-app-user-manual.md). Deployment modes and the container lifecycle are in [docs/switching-networks.md](docs/switching-networks.md).

## Checks

```powershell
npm run tlds      # link-safety TLD table matches the IANA source
npm run links     # every relative doc link resolves
npm run secrets   # no tokens, private host names, or real absolute paths

# Rust workspace
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs the Node checks and the Rust tests on Ubuntu and Windows. The Windows build runs inside WSL2 Ubuntu with a WSL-native `CARGO_TARGET_DIR`.

## Repository layout

```text
README.md, CHANGELOG.md, LICENSE   entry point, change list, MIT
package.json                      Node checks: tlds, links, secrets
docs\                             setup and operations manual, phone app manual, specsheet\
audit\                            audit rounds and gate evidence, outside the manual
rust\                             Rust workspace: crates\ core, inbound, ntfy, opencode,
                                  outbound, app, deploy, auth
container\                        ntfy image: Dockerfile, server.yml, .dockerignore
relay-windows\                    Windows launcher, watchdog, control and deploy examples
tools\                            image build, startup check, token rotation, link and secret scans
.github\workflows\ci.yml          Node checks and Rust matrix
e2e-smoke-tests\                  reserved for test tooling
bin\, resources\, logs\, temp\    local only, gitignored
```

## License

MIT, see `LICENSE`. ntfy itself is Apache-2.0 and is not vendored: the image downloads the published binary and verifies a checksum pinned in the Dockerfile.
