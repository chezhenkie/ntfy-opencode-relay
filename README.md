# ntfy-opencode-relay

A self-hosted ntfy relay for OpenCode. The phone is both an output and an input: pushes leave the host, and text, files, commands and permission replies come back into the running session.

[![The published index page](docs/assets/front-page.png)](https://chezhenkie.github.io/ntfy-opencode-relay/)

- Push out: every assistant turn, and any file the agent sends, arrives as a notification.
- Push back: phone text and files become a prompt in the target session.
- Commands: `/new`, `/to`, `/sessions`, `/status` and `/auto` run on the host from the phone.
- Permission asks: a `lock` push carries a short code, four characters unless a collision extends it; `/allow`, `/always` or `/deny` decides it.
- Questions: a `question` push shows the agent's question; `/answer` followed by any text closes the newest form, and the assistant reads that text.
- Tags: the relay stamps each push (`oc-out`, `oc-attach`, `oc-manual`, `lock`, `question`, `warning`, `hourglass_done`). A phone message carries no `oc-out`, so it becomes a prompt; the relay ignores the pushes it wrote itself. The tag decides the direction, which is how command and control stays inside the relay.
- Boundary: the relay holds the only ntfy token and is the only publisher; OpenCode has no token and opens no connection to the server. The relay also drives OpenCode through the local API: create a session, target one, prompt it and reply to a permission.
- Footprint: the daemon and the MCP server are one Rust binary, so nothing needs Python, Node, JavaScript or TypeScript. There is no Google telemetry: the phone app is the F-Droid client, with no Firebase and no Play services, and the whole stack stays on the private network.

## What it is

### Always running

| element | how many | what it does |
|---|---|---|
| ntfy container | one | stores messages for 72 h and pushes them to the phone |
| relay | one daemon, plus one `mcp` process per open project | the only publisher and the only token holder; reads the phone topic, prompts OpenCode, sends each turn |
| phone | one Android app | the F-Droid ntfy client; reads pushes, sends text, files and commands |
| OpenCode | one host | runs the sessions; holds no ntfy token |

One topic per project folder carries the traffic. The MCP server is part of `ntfy-relay`, so the install has one service binary.

### One-shot tools

Four binaries ship for each platform. Only `ntfy-relay` stays running; the other three run on demand:

| binary | what it does |
|---|---|
| `ntfy-deploy` | installs the stack into its root, creates or updates the container, and switches the network mode |
| `ntfy-auth` | creates the `phone` user, the wildcard row and the token file |
| `ntfy-admin` | lists and changes project roots, reports the running relay, stops it, and plans a mode |

## The boundary

The relay sits between two systems that never meet and is the only process that holds the ntfy token. OpenCode has no token and no connection to the server. The relay reads the OpenCode event stream over the local API, builds each turn, and is the only process that publishes. It also drives OpenCode in the other direction: it creates a session, targets one, sends the prompt and answers a permission. OpenCode starts `ntfy-relay.exe mcp` for a project, and the file tool publishes through the same code path. There is no plugin and no second publisher.

## How a message travels

Host to phone: the relay publishes each assistant turn to the topic. The server stores it and pushes it over the phone's own WebSocket.

Phone to host: the phone posts to the same topic. The relay holds one SSE connection, reads the message and posts it to the session as a prompt. Attachments land in `<project>\.ntfy-inbox\`.

Reconnect and restart recover missed assistant text. Message-id dedupe prevents duplicate pushes, and inbound delivery is at-most-once.

## Diagrams

The [manual index](https://chezhenkie.github.io/ntfy-opencode-relay/) opens the setup chapters and three diagrams: [inbound and outbound](https://chezhenkie.github.io/ntfy-opencode-relay/github-page-html-files/inbound-outbound-message-flow.html), [commands and tags](https://chezhenkie.github.io/ntfy-opencode-relay/github-page-html-files/commands-and-tags-flow.html), and [ask and permission](https://chezhenkie.github.io/ntfy-opencode-relay/github-page-html-files/ask-and-permission-flow.html). The same pages are in `docs\github-page-html-files\`.

## Requirements

- A host for the ntfy server: Podman or Docker on Windows or Linux, or the native binary on Linux under systemd. The `ntfy-deploy` helper drives Podman; a Docker host runs the container commands itself
- A private network from phone to host, and TLS on 443. The certificate must chain to a public root, because the relay uses the rustls public store; a private CA is not supported. The LAN mode is the exception: it serves plain HTTP on 8810 inside the local network
- The F-Droid ntfy client on Android, with no Firebase and no Play services
- OpenCode 2.2.7 or newer

## Install

Start at [docs/index.html](docs/index.html), the published page at https://chezhenkie.github.io/ntfy-opencode-relay/, or read [docs/index.md](docs/index.md) in the repository: binaries and checksums, the Windows Podman path, the Ubuntu routes, the private network and the HTTPS switch. The phone side is [docs/phone-app-user-manual.md](docs/phone-app-user-manual.md); network modes are in [docs/switching-networks-user-manual.md](docs/switching-networks-user-manual.md).

After the install, check the path:

```powershell
curl.exe -sS https://ntfy.example.ts.net/v1/health
```

The live install sits outside the checkout in `%LOCALAPPDATA%\ntfy-relay\`, named by `NTFY_RELAY_HOME`. A release folder is installed with one command, which places the four binaries, writes the missing configs, and registers the watchdog and logon entries: scheduled tasks on Windows, systemd user units on Linux, or a reported skip when there is no systemd. `install` sets `NTFY_RELAY_HOME` with `setx`, which does not update the current shell; open a new shell, or use the full path, before the next command uses the variable.

```powershell
& .\ntfy-deploy.exe install
& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" status
& "$env:NTFY_RELAY_HOME\ntfy-admin.exe" topics
```

## Phone commands

Plain text goes to the chat last typed in on the desktop. `/new <title>` creates a session and targets it. `/to` lists five numbered sessions; `/to 2` or a title fragment targets one message. `/auto` drops a pending target. `/sessions` prints numbers and titles. `/status` prints the target and the open asks.

Permission replies are `/allow [code]`, `/always [code]` and `/deny [code]`. A permission ask carries a short code, four characters unless a collision extends it, and stays answerable until it is answered or evicted. A question form arrives on the phone as a `question` push; answer it with `/answer` followed by any text. The relay closes the desktop form with that text and the assistant asks again when the answer is not enough.

## Everyday operations

`ntfy-deploy.exe mode` switches between Tailscale, Nord Meshnet and the LAN; the relay keeps running with the same pid and token. `tools\rotate-token.ps1 -User phone` issues a token, writes `ntfy-token.txt`, and optionally restarts the relay; revocation is a second run with `-RevokeOld <prefix>`.

The one-page check, start and stop guide for the host route, container and relay is [docs/startup.md](docs/startup.md).

Config, token, state, asks, forms, journal and log live in one relay folder. Only `/v1/health` is anonymous, signup is off, and the server is reachable only from the private network.

## Development

The source is a Rust workspace under `rust\`, with 624 tests. Crates: `core`, `inbound`, `ntfy`, `opencode`, `outbound`, `control`, `app`, `deploy`, `auth` and `admin`. Crate boundaries are in [specsheet 06](docs/specsheet/06-rust-crate-split.md); the build, smoke test and cutover are in [operations 7.9](docs/operations.md#79-rust-daemon-build-smoke-cutover).

Build and test in WSL from `rust\`:

```bash
env PATH=/home/example/.cargo/bin:/usr/local/bin:/usr/bin:/bin \
  CARGO_TARGET_DIR=/home/example/target/ntfy-proj cargo test --workspace
env PATH=/home/example/.cargo/bin:/usr/local/bin:/usr/bin:/bin \
  CARGO_TARGET_DIR=/home/example/target/ntfy-proj cargo build -p ntfy-relay-app -p ntfy-relay-deploy -p ntfy-relay-auth -p ntfy-relay-admin --release --target x86_64-pc-windows-gnu
```

The checks:

```powershell
npm run tlds      # link-safety TLD table matches the IANA source
npm run links     # every relative doc link resolves
npm run secrets   # no tokens, private host names, or real absolute paths
npm run site      # regenerates the published pages and checks their links
```

CI runs the Node checks on Windows and the Rust tests on Ubuntu and Windows.

Protocols:

| surface | where | direction |
|---|---|---|
| REST | ntfy server: `POST /<topic>` to publish, `PUT` for files, `GET /v1/health` | relay to ntfy |
| REST | OpenCode server: `/api/session/<id>/prompt`, `/api/session/<id>/message`, `/api/session/<id>/permission` | relay to OpenCode |
| SSE | ntfy `GET /<topics>/sse?since=`, and the OpenCode `GET /api/event` stream | long-lived read inside REST |
| MCP | `ntfy-relay mcp` on stdio, one tool `send_file` | OpenCode calls the relay |

REST plus SSE is the data and control plane; MCP is a single-tool interface over the same publish path. See [specsheet 00](docs/specsheet/00-overview.md), [specsheet 02](docs/specsheet/02-message-routes.md) and [specsheet 08](docs/specsheet/08-comms-and-mcp.md).

Repository layout:

```text
rust\                             Rust workspace and the four binaries
container\                        ntfy image: Dockerfile, server.yml, .dockerignore
tools\                            image build, token rotation, doc and secret scans, docs site build
docs\                             manual chapters, specsheet\, generated pages in github-page-html-files\
.github\workflows\ci.yml          Node checks and Rust matrix
resources\, logs\, temp\          local only, gitignored
```

## License

MIT, see `LICENSE`. ntfy itself is Apache-2.0 and is not vendored: the image downloads the published binary and verifies a checksum pinned in the Dockerfile.
