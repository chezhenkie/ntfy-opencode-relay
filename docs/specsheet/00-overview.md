# 00-overview.md - ntfy relay

## Header

- Title: ntfy relay - self-hosted ntfy transport for OpenCode, with phone continuation.
- Purpose: the reference for the system: the ntfy server, the Android client, the Rust relay in both directions, and the MCP file tool. The file and the code together describe the system.
- Stakeholders: the operator of the host, the phone user, and the upstream projects the system depends on: ntfy, OpenCode, and Podman.
- Status: running on the Rust path. The server is ntfy 2.37.2 on Podman behind a TLS terminator, deny-all auth, F-Droid-flavor Android client over WebSockets. The Rust merge is live in both directions: `core`, `ntfy`, `opencode`, `inbound`, `outbound`, and `app` build `ntfy-relay`; `deploy` builds `ntfy-deploy`; `auth` builds `ntfy-auth`; the daemon owns inbound and outbound; a reconnect or start backfills completed assistant text from the message API, so there is no event forwarder and no second feed; `ntfy_send_file` is the MCP tool; the patched publisher plugin is removed and its vendored copy was deleted in 0.7.0. The process and crate split is agreed in `06`; deployment modes are in `10`; the auth bootstrap is in `11`.
- Related documents: `README.md` (entry point), `CHANGELOG.md` (change list), `docs/index.md` (setup and operations manual), `LICENSE` (MIT).
- Evidence pin: live verification records live in `docs/operations.md`; the dated decision trail lives in git history. Cites here name files and functions, not commits.

## Short description

One host publishes OpenCode events to a private ntfy server over a private network and receives phone replies into the same session. The phone runs an F-Droid-flavor ntfy build, so no Google service is involved on either side. Both directions are the Rust relay: `rust\crates\outbound` subscribes to the native event stream, and `rust\crates\inbound` owns the ntfy subscription and routing. The MCP tool `ntfy_send_file` sends files on request. The Node relay was deleted in 0.8.0 and its folder is now `relay-windows\`, holding only the Windows launcher and watchdog. The container is baked from `container\`, the host route is a TLS terminator to a loopback portproxy, and the whole path is reachable only inside the private network.

## Table of contents

This file describes the system as a whole. The parts below are numbered in reading order and cover one subject each, so citations such as "section 6.4" keep resolving across the manual, the specs, and the code.

| part | covers |
|---|---|
| [01-phone-continuation.md](01-phone-continuation.md) | the phone continuation design: routing, commands, attachments, security rules, acceptance |
| [02-message-routes.md](02-message-routes.md) | both message routes explained without prior knowledge, with the path diagrams |
| [03-rust-relay-merge.md](03-rust-relay-merge.md) | the Rust merge plan, phase status, and cutover |
| [04-rust-relay-decisions.md](04-rust-relay-decisions.md) | the decision log for that merge, one row per open question |
| [05-js-to-rust-pitfalls.md](05-js-to-rust-pitfalls.md) | behavioral pitfalls the port must preserve, tied to the module that carries each one |
| [06-rust-crate-split.md](06-rust-crate-split.md) | the agreed process and crate split: entry points, internal crates, boundaries, MCP call path |
| [07-reconnect-backfill.md](07-reconnect-backfill.md) | Rust-only event gap recovery: message API backfill, dedupe, and gate |
| [08-comms-and-mcp.md](08-comms-and-mcp.md) | surfaces, the OpenAI rejection, MCP topology, shared crates, command layer, naming |
| [09-permission-reply-routing.md](09-permission-reply-routing.md) | permission replies bound to the pushed ask; registry, codes, restart, and tests |
| [10-deploy-mode.md](10-deploy-mode.md) | file ownership, the relay root, and VPN mode switching without an image rebuild |
| [11-auth-bootstrap.md](11-auth-bootstrap.md) | the one principal, the wildcard row, and the token file written by `ntfy-auth` |

## Design rules

1. The agentic framework never talks to the ntfy server. It exchanges text and files with the phone only through the relay. Outbound, the relay subscribes to the framework event stream and publishes to ntfy. Inbound, the relay subscribes to ntfy and delivers prompts, permission replies, and files into the framework session API. The MCP server is part of the relay, so the framework-facing surface is stdio MCP plus its own HTTP API.
2. Three runtime entities: the ntfy phone app, the ntfy server, and the relay. The relay contains the MCP server; the framework installs no plugin.
3. The relay is the only holder of the ntfy owner token and the only host-side publisher. A direct-publish plugin or a second publisher is rejected; it would double every push.
4. The framework is reached only through its HTTP API and service registration. The relay owns its config, tokens, state, journal, and log in one install root; no relay file lives in the framework config directories.
5. A network mode switch never edits the relay config, restarts the relay, or changes the token. The relay always talks to ntfy over loopback.

## File ownership

Each product owns its files. The relay root holds the relay config, tokens, deploy data, state, asks registry, journal, and logs. OpenCode is a runtime dependency reached only through its service registration and HTTP API; no relay-owned file lives under `~/.config/opencode` or `~/.local/state/opencode`.

- One path only. The move to the relay root has no fallback read, no dual config, and no compatibility shim.
- The old paths are deleted in the same change that introduces the root.
- No new cross-product file dependency is allowed for convenience. A design that needs one is rejected before code.
- The deploy helper in `10` reads no OpenCode file and never edits the relay config.

## Process supervision

The relay is one process among several. Today the OS logon shortcut and the `ntfy-relay-watchdog` task start it, and the daemon supervises its own SSE and outbound tasks; nothing above it owns the process. A future per-environment process manager, the bridge, will start and stop children in dependency order and route events between environments. The relay stays a black box to it: a health endpoint, a log, a service registration, and token files.

- Start order once a manager exists: container healthy, OpenCode service registration valid, relay last.
- Stop order: relay first, then OpenCode, then the container.
- Boundary: the manager owns process lifecycle and cross-environment routing; the relay owns ntfy semantics, meaning the token, the subscription, the control verbs, and the outbound mapping.
- The division is fixed; the interface may grow. The interface is limited to the relay's surface: `/v1/health`, `service.json`, the token files, and `ntfy-relay status`.

## Standalone entity and comms seam

The relay is a standalone project. The bridge is a separate entity and a separate project: no shared code, no shared repository, no shared lifecycle, and no process owner inside the relay. The relay never imports, spawns, or assumes the bridge. The bridge may start, stop, observe, and address the relay only through the documented surface: `/v1/health`, `service.json`, the token files, the log, and `ntfy-relay status`.

- Client comms beyond OpenCode is decided: no OpenAI-compatible endpoint. REST plus SSE is the control plane and MCP the action facade; `08` records the evaluation and what would reopen it.
- Any additional comms shape is additive: its own module behind a boundary trait, with the inbound and outbound paths unchanged. `08` holds the MCP topology, the shared-crate rule, and the naming split.
- Every shape obeys the standing rules: deny-all auth, one topic per project folder, link-safe text, one publisher, and no Google path.
- Nothing bridge-specific ships in this repository; the bridge design and its build plan stay in their own project.
