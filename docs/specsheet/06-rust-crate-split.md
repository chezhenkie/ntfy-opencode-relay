# Rust crate split

Status: agreed process and crate split, implemented and live. `core`, `ntfy`, `opencode`, `inbound`, `outbound`, and `app` build `ntfy-relay run|mcp|status`; `deploy` builds `ntfy-deploy`; `auth` builds `ntfy-auth`. The daemon owns both directions and the MCP stdio server shares the build. The design context is in `03-rust-relay-merge.md`; open questions stay in `04-rust-relay-decisions.md`; the porting pitfalls are in `05-js-to-rust-pitfalls.md`; deployment modes are in `10`; the auth bootstrap is in `11`.

The implementation has two layers: shipped entry points and internal crates, with the seams between them.

## Shipped entry points

The relay is one binary with three subcommands. Two admin tools sit beside it: `ntfy-deploy` for the container and network mode (`10`), and `ntfy-auth` for the auth bootstrap (`11`). Neither shares runtime state with the daemon.

- `ntfy-relay run` is the daemon. It is the only long-running process and owns the lock, config, token, state, supervisor, and logging.
- `ntfy-relay mcp` is the stdio MCP server. OpenCode spawns it per directory with `mcp` as the argument; the process working directory is the project root. Short-lived, one tool, no daemon dependency.
- `ntfy-relay status` is one-shot. Reads the lock, state, and log tail for manual checks. `tools\startup-check.ps1` checks that process and its heartbeat.

One binary, because version skew between the daemon and the MCP tool must not occur. The tool contract and the outbound policy must be the same build. If a separate `ntfy-mcp.exe` is ever wanted, it is a second `[[bin]]` target; the crates do not change.

No separate inbound and outbound executables. Two locks, two configs, two logs, and split state would be needed for isolation the supervisor already gives. This settles row 7 in favor of one process.

## Internal crates

```text
rust/
  Cargo.toml, Cargo.lock, rust-toolchain.toml
  crates/
    core/       lib: config, topic, link-safe, state store, clock, adapter trait, text budgets
    ntfy/       lib: HTTP client, publish text and file, SSE parser, attachment fetch, header encoding
    opencode/   lib: API client, event stream, event types, discovery, adapter impl
    inbound/    lib: filter, dedupe, queue, routing, control, prompting, inbox prune
    outbound/   lib: turn tracker, file scan, permission and question mapping, manual send, seen journal, backfill, event dedupe
    app/        bin: CLI, supervisor, lock, logging, signal handling, MCP stdio loop
    deploy/     bin: ntfy-deploy, deploy config, container apply, network mode
    auth/       bin: ntfy-auth, auth config, user, wildcard row, and token bootstrap
```

Dependency direction:

```text
app      -> inbound, outbound, opencode, ntfy, core
inbound  -> core
outbound -> core, ntfy, opencode
opencode -> core
ntfy     -> none
core     -> none
deploy   -> none
auth     -> none
```

`inbound` does not depend on `opencode`. It calls the `core::adapter::SessionApi` trait; `app` injects the OpenCode implementation. That is the pi/omp seam the spec already requires. `outbound` stays OpenCode-specific because turn text, permission asks, and the question tool are OpenCode event shapes; a generic event trait would leak immediately.

The `ntfy-forwarder` plugin and its loopback ingest were removed in 0.7.4. Outbound now has one feed, `/api/event`, and recovers completed text from `GET /api/session/{id}/message` after a gap or a start; one seen journal covers event ids and assistant message ids.

## What each boundary owns

- `core` is the only place link-safe and the TLD table live. The daemon, the MCP tool, and every ack call the same function. The generated Rust table is checked in; `npm run tlds` compares the Node copy and the Rust table to the text source.
- `ntfy` owns the wire, not the policy. No topic derivation, no link safety, no caller byte budgets. It takes bytes and headers, and keeps the plugin title and message caps because they are part of the serialized request. The two publish styles are explicit here.
- `opencode` owns the API and the event types, not the mapping. `outbound` maps events to pushes; contract tests parse recorded fixtures here.
- `inbound` owns the control table, confirm holds, and cooldowns. Modules, not crates.
- `app` is the composition root only: CLI, lock, supervisor, logging, signals, MCP protocol. No business rules.
- `deploy` and `auth` are standalone binaries with no workspace dependency beyond serde. They own their config in the relay root and never read the relay config or an OpenCode file. `deploy` owns the container and network path; `auth` owns the principal, its row, and the token file.

## The MCP call path

`ntfy-relay mcp` publishes directly through `core` and `ntfy`, not through the daemon. Three reasons:

- A user-requested send must not be dropped by the outbound queue's overflow policy.
- No local IPC server, no discovery file, no second auth surface.
- The tool works when the daemon is down.

Cost: a second token read and a second publisher. Both use the same policy code, so there is no drift. Project confinement comes from the single `projectRoots` entry, not from MCP scoping, so row 2 loses its scoping risk; only rendering and tool naming remain to test.

`outbound::send_file` is the library function the MCP loop calls. It enforces canonical path, 15 MB cap, `oc-out,oc-manual`, and link-safe message. The protocol layer stays thin.

Live since 2026-11-11: `mcp.servers.ntfy` runs the binary with `mcp`, `cwd: "."`, and `codemode: false`; a permission allow matches `ntfy_send_file`. An agent call returned the plugin's `Sent ...` text and one `oc-out,oc-manual` push. One process per directory means a directory outside `projectRoots` gets refusals, not publishes.

## Modules

Control, state, link-safe, MCP protocol, prune, and the TLD generator stay modules. A `test-support` crate appears only when a second crate duplicates the fake clock or fake ntfy server, not before.

The split is settled. The direct MCP publish would change only if a user-requested send ever needs the daemon queue.
