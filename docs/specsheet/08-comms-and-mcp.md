# 08-comms-and-mcp.md - surfaces, MCP topology, and the command layer

## Header

- Scope: what the relay speaks today, what it will speak as it grows into command and control, how MCP servers are shaped and shared, and the naming rules that follow.
- Status: decisions from the 2026-11-13 design discussion. The protocol table is verified against the live relay; the rest is forward-looking.
- Related: `00-overview.md` (standalone boundary), `06-rust-crate-split.md` (current crates), `README.md` (protocol table for readers).

## 1. Surfaces today

| Surface | Where | Direction |
|---|---|---|
| REST | ntfy server: `POST /<topic>` to publish, `PUT` for files, `GET /v1/health` | relay to ntfy |
| REST | OpenCode server: `/api/session/<id>/prompt`, `/api/session/<id>/message`, `/api/permission/...` | relay to OpenCode |
| SSE | `GET /api/event` on ntfy, and the OpenCode event stream | long-lived read inside REST |
| MCP | `ntfy-relay mcp` on stdio, one tool `send_file` | OpenCode calls the relay |

REST plus SSE is the data and control plane. MCP is a single-tool facade over the same publish path. MCP is inbound only in the sense that the agent calls the relay; the relay never calls an MCP server itself. The framework never talks to ntfy directly; design rule 1 in `00-overview.md` is binding.

## 2. Decision: no OpenAI-compatible API

Status: decided on 2026-11-13.

The relay will not expose `/v1/chat/completions`, `/v1/models`, `/v1/audio/*`, or any other OpenAI-shaped endpoint. The option was evaluated and rejected.

Reasons:

- The OpenAI API is a model API. It models prompts, tokens, streaming deltas, and tool calls. It has no concept of a job, a device, an ack, an idempotency key, a cursor, a permission, or replayable history.
- Modeling command and control as "tool calls inside a chat loop" turns every operation into a prompt: latency, token cost, nondeterminism, and no retry semantics.
- Compatibility is unbounded past the happy path: partial tool-call JSON, parallel calls, structured outputs, reasoning fields, and `/responses` versus `/chat/completions`. A compatibility claim would be a permanent maintenance contract.
- The system needs deterministic verbs and state, not a chat API. Clients that only speak OpenAI shapes are not the target.

What would reopen the decision: a concrete client that cannot be served otherwise and whose value exceeds the adapter's cost, or voice tooling that only ships OpenAI-shaped STT/TTS. Until then, none of this is built, and no code may assume it.

Consequence for publication: the project does not advertise chat-UI compatibility. Agents integrate through MCP, applications through REST and SSE.

## 3. MCP topology

Status: agreed direction, no code yet.

Today: one binary, three modes (`ntfy-relay run|mcp|status`). MCP mode is about 12 KB of protocol plus one tool, and it calls `outbound::files::send_file`, so the tool and the daemon share one policy path. OpenCode spawns one process per directory with the project folder as cwd and therefore as topic.

Rules for future domains:

- Domain code does not go inside `ntfy-relay.exe`. Mail, kanban, voice, and device control are separate projects with separate binaries, because the relay is a standalone public artifact and must not accumulate unrelated policy.
- One MCP server per domain, as its own binary. Independent release cadence, independent crash domain, and only that domain's tools in the model's context.
- A router binary that links several domain crates and serves all their tools in one process is allowed later if process count actually hurts. It would use the same crates with a different `main`. It is not the default.
- A single large binary with every domain compiled in is rejected: one release cadence, one dependency upgrade, one crash domain, and a permanently large tool list.

Process cost: an idle stdio MCP server uses a few MB of resident memory and no CPU. The scarce resource is model context, so split by domain and keep each server around 5-15 tools. Enable servers per project through the client config, not globally.

Naming: the MCP server name is the tool prefix (`<server>_<tool>`), so server names are short and stable, and tool names are public contract once an agent uses them.

## 4. Shared code without drift

Status: agreed.

The MCP protocol skeleton and the command layer become versioned crates, consumed by dependency, never copied freehand.

- `mcp-core`: newline-delimited JSON-RPC framing, `initialize`/`ping`/`tools/list`/`tools/call`, revision negotiation, line cap, stdout discipline. No policy, no config, no domain.
- Command layer: verbs, events, permission checks, idempotency, ack and result shapes. Shared by the daemon, the MCP servers, and any future transport adapter.
- Domain crates: tool definitions and their policy, one per domain.

Copies are forbidden because the relay already had drift once: the Node and Rust paths diverged until a port and a gate were needed to reconcile them. Every protocol fix and permission change would otherwise be manual work in N repositories, diverging silently.

Distribution, in order of preference: workspace path dependency while the projects share a checkout; git dependency on a pinned tag once they separate; vendored source with a checksum only as a last resort.

## 5. Command layer

Status: direction, to be specified before the relay grows beyond ntfy.

The long-term direction is a deterministic command layer over ntfy transport:

- Verbs and events as data. A command has a name, arguments, a target, and a result.
- Idempotency keys on mutations; every async action reports completion or failure; nothing is silently dropped.
- Permission checks per verb and per target, enforced in the core, not in a transport adapter.
- One event stream for state changes, with cursors so a client can resume.
- Adapters at the edge translate REST or MCP calls into the same core calls. MCP is not a second implementation of policy.

What belongs where: state and verbs in the core; transport shapes in adapters; delivery and retries in whichever entity owns the action.

## 6. Naming

Status: decided 2026-11-18, public name fixed.

Three names overlap and are treated separately:

- Repository and package: `ntfy-opencode-relay`. The name states the transport and the agent it serves.
- Binary: `ntfy-relay.exe`. Referenced by scripts, the lock file, log names, the scheduled task, the MCP registration, and the process checks. Renaming it is expensive and has no benefit, so it stays `ntfy-relay`.
- Product name: no separate brand; the repository name is the public name.

The project targets OpenCode. Internal interfaces keep the command layer and the transport apart, but the public scope is one harness, one MCP facade, and one ntfy transport. A second agent is not advertised and has no adapter.

## 7. Open points

1. Trigger for a router binary: number of servers, context pressure, or process count.
2. Where the shared crates live and who owns their versioning.
3. Tool naming and permission granularity across domains, and the per-project enablement rules.
4. Whether the command layer is one crate or split into protocol, policy, and transport crates.
5. Which domains get an MCP server at all, versus a REST endpoint plus an agent skill.

## 8. Non-goals

- No OpenAI-compatible endpoint and no compatibility claim.
- No MCP server for a domain that only needs a REST call from a script.
- No domain state inside an MCP process.
- No bridge logic in the relay and no relay logic in the bridge.
- No giant binary as an integration shortcut.
