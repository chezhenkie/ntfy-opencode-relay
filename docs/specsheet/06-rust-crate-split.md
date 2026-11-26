# Rust crate split

Status: agreed process and crate split, implemented and live. `core`, `ntfy`, `opencode`, `inbound`, `outbound`, and `app` build `ntfy-relay run|mcp|ensure|status`; `deploy` builds `ntfy-deploy`; `auth` builds `ntfy-auth`; `admin` builds `ntfy-admin`. `control` is the shared library for the relay and the CLI (`12`): types, root policy, runtime file, pid liveness, listener, and client. The daemon owns both directions and the MCP stdio server shares the build. The design context is in `03-rust-relay-merge.md`; open questions stay in `04-rust-relay-decisions.md`; the porting pitfalls are in `05-js-to-rust-pitfalls.md`; deployment modes are in `10`; the auth bootstrap is in `11`.

The implementation has two layers: shipped entry points and internal crates, with the seams between them.

## Shipped entry points

The relay is one binary with four subcommands. Three one-shot tools sit beside it: `ntfy-deploy` for the container and network mode (`10`), `ntfy-auth` for the auth bootstrap (`11`), and `ntfy-admin` for the control endpoint (`12`). Only `ntfy-admin` talks to the running daemon; none shares its runtime state.

- `ntfy-relay run` is the daemon. It is the only long-running process and owns the lock, config, token, state, supervisor, and logging.
- `ntfy-relay mcp` is the stdio MCP server. OpenCode spawns it per directory with `mcp` as the argument; the process working directory is the project root. Short-lived, one tool, no daemon dependency.
- `ntfy-relay ensure` is the one-shot watchdog step. It exits 0 with `running pid=<n>` when the lock names a live relay, otherwise starts the daemon detached from itself and waits up to 60 s for the lock. A child that exits first is reported at once. It writes one line to `<root>\logs\relay-ensure.log` per start or failure and stays silent while the relay is alive. The scheduled task names it directly.
- `ntfy-relay status` is one-shot and read-only. It prints the config, lock, state, and log tail, then the platform rows: podman binary and machine, container presence and health, Tailscale Serve, the loopback portproxy, legacy relay files, the health endpoint, the elevated flag, and the install root's ACL. Each row reads PASS, FAIL, or SKIP, and exit 0 is a report rather than a verdict. The installer's logon task is the step that starts things; `status` starts nothing.

One binary, because version skew between the daemon and the MCP tool must not occur. The tool contract and the outbound policy must be the same build. If a separate `ntfy-mcp.exe` is ever wanted, it is a second `[[bin]]` target; the crates do not change.

No separate inbound and outbound executables. Two locks, two configs, two logs, and split state would be needed for isolation the supervisor already gives. This settles row 7 in favor of one process.

On Windows the binary ships as a windowless (GUI-subsystem) build. No console is allocated, so the scheduled watchdog names `ntfy-relay.exe ensure` directly and the `wscript` layer is gone from it. The logon task names `wscript.exe` and the generated `ntfy-relay-startup.vbs`, because its Podman step is a console program and a direct action flashes a console window; the script starts every child hidden. `mcp` stdio and redirected log output work unchanged. The cost is that a hand-started daemon receives no Ctrl+C, so it is stopped by pid: `Stop-Process -Id <pid>` on Windows, `kill <pid>` on Linux. The user manual states the stop path for each OS, including that the watchdog restarts a stopped relay within five minutes unless its task is disabled first.

The liveness and startup scripts fold into the same binary: a short-lived `ensure` subcommand does the watchdog check and hidden restart (live since 0.5.0), `status` absorbs the startup check's reporting (live since 0.5.1), and since 0.6.0 `ntfy-deploy install` writes and repairs the Windows tasks or the systemd user units while its logon entry runs the boot steps (Podman machine start, `ensure`). The start action belongs to the boot entry, never to `status`.

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
    control/    lib: endpoint types, root policy, runtime file, pid liveness, blocking client
    app/        bin: CLI, supervisor, lock, logging, signal handling, MCP stdio loop
    deploy/     bin: ntfy-deploy, deploy config, container apply, network mode
    auth/       bin: ntfy-auth, auth config, user, wildcard row, and token bootstrap
    admin/      bin: ntfy-admin, control CLI: topics, add, remove, status, stop, mode
```

Dependency direction:

```text
app      -> inbound, outbound, opencode, ntfy, control, core
control  -> core
inbound  -> core
outbound -> core, ntfy, opencode
opencode -> core
admin    -> control
ntfy     -> none
core     -> none
deploy   -> none
auth     -> none
```

`inbound` does not depend on `opencode`. It calls the `core::adapter::SessionApi` trait; `app` injects the OpenCode implementation. That is the pi/omp seam the spec already requires. `outbound` stays OpenCode-specific because turn text, permission asks, and the question tool are OpenCode event shapes; a generic event trait would leak immediately.

The `ntfy-forwarder` plugin and its loopback ingest were removed in 0.7.4. Outbound now has one feed, `/api/event`, and recovers completed text from `GET /api/session/{id}/message` after a gap or a start; one seen journal covers event ids and assistant message ids.

## What each boundary owns

- `core` is the only place link-safe and the TLD table live. The daemon, the MCP tool, and every ack call the same function. The generated Rust table is checked in; `npm run tlds` compares it to the text source.
- `ntfy` owns the wire, not the policy. No topic derivation, no link safety, no caller byte budgets. It takes bytes and headers, and keeps the plugin title and message caps because they are part of the serialized request. The three publish styles are explicit here.
- `opencode` owns the API and the event types, not the mapping. `outbound` maps events to pushes; contract tests parse recorded fixtures here.
- `inbound` owns the control table, confirm holds, and cooldowns. Modules, not crates.
- `app` is the composition root only: CLI, lock, supervisor, logging, signals, MCP protocol. No business rules.
- `control` owns the endpoint contract, the root policy, and the runtime file. The config write and the discovery trigger stay in `app`.
- `deploy` and `auth` are standalone binaries with no workspace dependency beyond serde. They own their config in the relay root and never read the relay config or an OpenCode file. `deploy` owns the container and network path; `auth` owns the principal, its row, and the token file; `admin` owns the command line over the control endpoint and links `control` only.

## The control crate and its CLI

The `control` crate carries the envelope and error codes, the project-root policy, the runtime file, pid liveness, the listener, and a blocking client. The daemon serves the listener; the config write and the discovery trigger are injected by `app`. The relay owns the config write and the discovery reload for the changes it can apply itself; `ntfy-admin` is a thin front end that talks to the relay only. Dependencies: `app -> control`, `admin -> control`, `control -> core`. `control` adds no binary, and `deploy` stays the separate privileged helper (`10`). The transport, file, envelope, and topic policy are fixed in `12`.

## The MCP call path

`ntfy-relay mcp` publishes directly through `core` and `ntfy`, not through the daemon. Three reasons:

- A user-requested send must not be dropped by the outbound queue's overflow policy.
- No local IPC server, no discovery file, no second auth surface.
- The tool works when the daemon is down.

Cost: a second token read and a second publisher. Both use the same policy code, so there is no drift. Project confinement comes from the single `projectRoots` entry, not from MCP scoping, so row 2 loses its scoping risk; only rendering and tool naming remain to test.

`outbound::files::send_file` is the library function the MCP loop calls. It enforces canonical path, 15 MB cap, `oc-out,oc-manual`, and link-safe message. The protocol layer stays thin.

Live since 2026-11-11: `mcp.servers.ntfy` runs the binary with `mcp`, `cwd: "."`, and `codemode: false`; a permission allow matches `ntfy_send_file`. An agent call returned the plugin's `Sent ...` text and one `oc-out,oc-manual` push. One process per directory means a directory outside `projectRoots` gets refusals, not publishes.

## Modules

Control, state, link-safe, MCP protocol, prune, and the TLD generator stay modules. A `test-support` crate appears only when a second crate duplicates the fake clock or fake ntfy server, not before.

The split is settled. The direct MCP publish would change only if a user-requested send ever needs the daemon queue.
