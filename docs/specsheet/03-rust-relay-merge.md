# Rust relay merge plan

Status: the Rust daemon is the live path in both directions. Steps 2 to 6 are implemented, plus the wire (`ntfy`) and OpenCode API (`opencode`) crates and the `app` composition root: 741 tests at 0.6.13. Service discovery reads `~/.local/state/opencode/service.json` and re-reads it after a transport error or HTTP 401; `opencode.baseUrl` remains an override. Outbound is live (`outbound.enabled: true`); the patched publisher plugin is removed and its vendored copy was deleted in 0.7.0. A reconnect or start backfills completed assistant text from the message API, so an `/api/event` gap does not lose turn text and there is no event forwarder. The Node relay was deleted in 0.8.0, so it is not an inbound rollback. Path note: the relay reads its config, tokens, state, asks, and journal from the install root, `%LOCALAPPDATA%\ntfy-relay\` on Windows; the config examples below record the earlier layout. See `10-deploy-mode.md`.

## Phase status

| Step | State | Evidence |
| --- | --- | --- |
| 1. Freeze and fixtures | done | seven fixtures under `rust\crates\opencode\tests\fixtures`; permission responses are not captured |
| 2. Core | done | `rust\crates\core`, 75 tests |
| Wire and API clients | done | `rust\crates\ntfy` 54 tests, `rust\crates\opencode` 50 tests |
| 3. Inbound | done | `rust\crates\inbound`, 86 tests |
| App composition | done | `rust\crates\app`, 99 tests; `run`, `mcp`, `status`, and the task supervisor |
| 4. Outbound | done | `rust\crates\outbound`, 89 tests; question asks map from `form.created`; the MCP loop ships in `rust\crates\app` |
| 5. Cutover | done | inbound live since 0.4.2; outbound, plugin removal, and MCP live since 0.5.0; restart dedupe and lock fix in 0.6.1; reconnect backfill in 0.7.4; gates in `docs\operations.md` 7.4 |
| 6. Cleanup | done | plugin and verifier deleted in 0.7.0; forwarder and loopback ingest deleted in 0.7.4; the Node relay was deleted in 0.8.0 |

## 1. Goal

One Rust application owns both directions of the phone channel:

- It subscribes to ntfy and delivers phone messages into OpenCode sessions.
- It subscribes to OpenCode's event stream and publishes assistant text, files, permission asks, and errors to ntfy.

OpenCode, its config, and its plugins never hold the ntfy token again. The patched third-party plugin and the patch script left the tree as the migration completed; the Node relay was deleted in 0.8.0. Inbound and outbound stay separate crates inside one Cargo workspace, so they can evolve and fail independently while sharing one config, one topic rule, and one link filter.

## 2. Current state

- Outbound: `rust\crates\outbound` subscribes to `/api/event` and publishes turn text, files, permission asks, question forms, and idle and error notices. On a reconnect or a start it reads completed assistant messages from `GET /api/session/{id}/message` and publishes text the live stream missed; one seen journal dedupes event ids and assistant message ids. The patched publisher plugin `opencode-ntfy-with-questions@0.2.1` is removed from `opencode.jsonc`; its vendored copy was deleted in 0.7.0. `ntfy_send_file` is the MCP tool from `ntfy-relay mcp`.
- Inbound: the Rust daemon owns the ntfy SSE subscription, filter, routing, control table, attachments, and state. `rust\crates\inbound` and `rust\crates\app` are live since 0.4.2. The Node relay `ntfy-opencode-incoming-relay` was the rollback until 0.8.0, when it was deleted.
- The split existed because a plugin runs per location and dies with it: an inbound listener there would duplicate prompts with two locations loaded and stop when no location is open. Outbound in the plugin got exact in-process events with no polling. The Rust outbound keeps the exact-event model through the API event stream and recovers completed text from the message API after a gap.
- Shared behavior that survived the merge: folder-name topic, deny-all auth, `oc-out` tag, link safety, auto-attach rules, permission grace, turn text assembly.

## 3. Feasibility evidence

The running server exposes `GET /api/event`, operation `event.subscribe`: a `text/event-stream` of native and plugin RPC events across all server locations, with a `types` query filter and an initial `server.connected` event.

Relevant event types:

- `session.text.started`, `session.text.delta`, `session.text.ended`
- `session.execution.started`, `session.execution.succeeded`, `session.execution.failed`, `session.execution.interrupted`
- `session.status`, `session.idle`
- `permission.asked`, `permission.replied`
- `filesystem.changed`
- `session.created`, `session.updated`-class events for session metadata
- `rpc.*` for plugin RPC

The OpenAPI surface is marked experimental and the stream is volatile by contract: a slow consumer overflows and fails the stream, and events during a disconnect are missed. Both points drive the failure model in section 8.

## 4. Target shape

The target shape is one Cargo workspace, one binary, and two tasks.

```
rust/
  Cargo.toml, Cargo.lock, rust-toolchain.toml
  crates/
    core/       config, topic derivation, link filter, state, text budgets, adapter traits
    ntfy/       wire client: publish, SSE parser, attachment fetch
    opencode/   API client, event types, adapter
    inbound/    ntfy SSE, filter, routing, prompting, permission replies, file download
    outbound/   OpenCode event subscription, turn buffer, file scan, send_file library
    app/        binary: config load, supervision, logging, lock file
```

- `core` has no async task of its own. It owns the topic rule, `linkSafe`, the config schema, the state file, and the text budgets. The wire client is `ntfy`; the OpenCode client is `opencode`.
- `inbound` ports the current relay semantics one to one. The Node inbound suite in `ntfy-opencode-incoming-relay\tests\` was the porting gate and was deleted in 0.8.0; the Rust suites in `rust\crates\inbound` and `rust\crates\app` are the acceptance gate.
- `outbound` replaces the plugin. It needs no location gate because one subscriber sees every location; the topic comes from the session's project canonical.
- `app` is the composition root. It starts the inbound task with one token, one config, one log, and the outbound task. It also serves the MCP stdio server. The supervisor restarts a failed task in place since 0.6.0.

## 5. Outbound design

Subscribe with `types` set to the minimum: `session.text.ended`, `session.execution.*`, `session.status`, `permission.*`, `session.created`, `filesystem.changed`. Idle comes from `session.status` with type idle or from `session.execution.succeeded`. Deltas are not needed if `session.text.ended` carries the complete part text, which the outbound worker relies on.

Mapping, carried over from patches 1 to 5:

| Event | Push |
| --- | --- |
| `session.execution.succeeded` | idle |
| `session.status` with idle | idle, debounced against the success |
| `session.execution.failed` | error, plus buffered partial text |
| `session.execution.interrupted` | partial text with an `(interrupted)` marker |
| `session.text.ended` parts | buffered per session, sorted by time then ordinal, one push per turn |
| `permission.asked` | permission push after 15 s, cancelled by `permission.replied` |
| question tool | one push with the question text and options |

Turn text rules stay: 3400 UTF-8 bytes inline, longer text as a `.md` attachment with a 500-character summary, 10-minute buffer expiry, at most 256 tracked turn states, single push at the terminal, subagent suppression.

Files stay: terminal scan of files whose birthtime falls inside the turn, allowlist and caps, `oc-out,oc-attach` tags, one PUT per file, never modified files, never source files. The terminal scan costs one project walk per turn, capped at 20000 entries. Eight auto-attach files publish as sequential PUTs and can hold the worker about 90 seconds; the 256-slot event channel drops and counts during that time.

`ntfy_send_file` moved to the Rust app: `ntfy-relay mcp` serves one `send_file` tool over stdio with the same contract (project-confined path from the process working directory, 15 MB cap, `oc-out,oc-manual`, link-safe message, never throws; a refusal sets `isError`). The live registration is `mcp.servers.ntfy` with `codemode: false`; OpenCode renders the tool as `ntfy_send_file`. A live agent call returned the plugin's `Sent ...` text and one file push. The plugin fallback was not needed.

## 6. Inbound design

Behavior is unchanged: one SSE subscription to ntfy, `oc-out` filter, replay window, duplicate-ID state, routing hints, queue and steer delivery, the JSON control table (`new`, `sessions`, `to`, `auto`, `help`, `status`) with its capability gate and confirm hold, `/answer` as a native verb, `/steer`, `/allow`, `/always`, `/deny`, attachment download to `.ntfy-inbox`, link-safe acks and error pushes. Since 0.11.0 the routing hint replaces the pin; spec 01 section 6.7 defines the order.

Two changes:

- The OpenCode calls go through HTTP directly instead of the `opencode api` CLI, so the process needs no child shell and no PATH assumptions.
- The state file and locks move into `core`, with the same atomic-write and single-instance rules.

## 7. Configuration

One file, one token, both directions. The implemented file keeps the Node shape so existing configs load unchanged:

```json
{
  "enabled": true,
  "ntfy": {
    "server": "https://ntfy.example.ts.net",
    "tokenFile": "~/.config/opencode/ntfy-token.txt",
    "replayWindowMinutes": 60,
    "maxMessageBytes": 16384
  },
  "projectRoots": ["C:\\work\\ntfy-proj"],
  "opencode": { "baseUrl": "http://127.0.0.1:4096", "timeoutMs": 30000 },
  "adapter": {
    "name": "opencode",
    "agent": "build",
    "model": { "providerID": "provider", "id": "model", "variant": "max" }
  },
  "routing": { "preferRunning": true, "delivery": "queue" },
  "files": { "inboxDir": ".ntfy-inbox", "maxFileBytes": 15728640 },
  "logging": {
    "file": "~/.local/state/opencode/ntfy-relay.log",
    "heartbeatMinutes": 5,
    "level": "info",
    "echo": false
  },
  "stateFile": "~/.local/state/opencode/ntfy-relay-state.json"
}
```

`projectRoots` serves discovery and bounds outbound: a session whose project is outside the roots is skipped. Any number of entries are allowed since 0.3.0, including none; each folder name is a topic, a duplicate folder name keeps the more recently active project with a warning, and the topic-ownership map guarantees one owner per topic. The daemon discovers the shared service from `~/.local/state/opencode/service.json` and re-reads it after a transport error or HTTP 401; `opencode.baseUrl` is an optional override, and the port is dynamic. `opencode.command` is accepted and ignored: the client talks HTTP with Basic auth. The `outbound` block carries the plugin defaults; the live config enables it after the plugin was removed. Server discovery is decided in `04` row 5.

## 8. Failure model

- Reader and publisher are separate tasks joined by a bounded channel. The reader never awaits ntfy.
- Channel overflow on the outbound side drops queued pushes and logs once, then publishes a single `outbound overflow` notice. It never blocks the event stream.
- On event-stream failure, reconnect with backoff. Both event streams close and reconnect after 90 seconds with no bytes, twice the ntfy 45-second keepalive. After the subscription is back, the daemon reads completed assistant messages for the open turns, the last text session, and the sessions the state file remembers, and publishes any unseen text. A cold start reads the newest 20 sessions per project root inside the 15-minute seen window, capped at 200 messages. A message id and a live text part share one dedupe key, so a part delivered live is not published again from backfill; permission asks and question forms remain uncovered and the log records the gap.
- On ntfy SSE failure, keep the current replay-window and duplicate-ID recovery.
- Task supervision: each task returns a reason, the supervisor restarts with capped backoff and writes one log line per restart.
- Everything in memory is bounded: turn buffers, processed IDs, queue depth, attachment count.

Current implementation: the app supervisor restarts a failed SSE or outbound task with 1 s to 60 s backoff and one log line per restart; a panicking outbound worker aborts its reader. The outbound worker tracks connection generations in `FeedState`; a new generation triggers one backfill inside the worker, so live frames wait in the 256-slot channel and can be dropped under a long backfill. `seen` holds event ids and assistant message ids in one 15-minute, 8192-id journal, and a message id is marked before its push and committed after it. Permission asks and question forms have no history endpoint and are not recovered. The forwarder and the loopback ingest were deleted in 0.7.4. See `07-reconnect-backfill.md`.

## 9. Migration plan

1. Freeze. No new plugin patches beyond link safety. Record real `/api/event` captures for fixtures.
2. `core` and `ntfy`. Config, topic, `linkSafe` (port the existing allowlist and tests), wire client with text, file, tags, priority, auth. Parity test: publish from Rust, compare wire bytes with the current client.
3. `inbound` and `opencode`. Port the relay with the inbound test suite; the OpenCode client talks HTTP directly. Run it beside the Node relay only in offline tests. Live gate: one scripted session on `ntfy-proj` during a quiet window, same commands and acks.
4. `outbound`. Replay the recorded fixtures through the new assembler and compare pushes field by field with the plugin output. Live gate: one turn, one long turn, one file set, one permission ask, one question.
5. Cutover. Remove the plugin from `opencode.jsonc`, stop the Node relay, start the Rust binary, verify turn text, files, permissions, and phone replies. Keep the Node relay in the last known good commit as rollback at the time; it was deleted in 0.8.0.
6. Cleanup. Delete `ntfy-opencode-modified-plugin` and the patch script (done in 0.7.0). The Node relay was deleted in 0.8.0; keep the docs history.

Steps 1 to 6 are executed: the plugin and verifier are deleted, and the Node relay was deleted in 0.8.0. The live gate ran on 2026-11-11: turn text, one long turn (11361-byte attachment), a three-file set, a permission ask with the 15 s grace, a question form, and child-session suppression. The MCP tool was called by an agent and returned the plugin text.

Rollback at any phase: run a previous `ntfy-relay.exe` build, or check out the last known good commit and rebuild. The Node relay was deleted in 0.8.0, so there is no implementation switch.

## 10. Testing

- Unit: topic derivation, filter, dedupe, routing, turn assembly, link safety, config parsing, path confinement.
- Contract: parse recorded real events; fail when a required field disappears after an OpenCode upgrade.
- Golden pushes: serialized expected pushes per scenario, compared field by field.
- Live scripted: one session per scenario on a scratch project, same as the phase 3 close-out harness.
- Link safety: one live push containing `readme.md`, checked on the phone.

## 11. Open questions

Tracked in `04-rust-relay-decisions.md`, one row per question with a lean, a status, and the decision. The sections above give the context for each: stream gaps in section 3 and 8, the tool replacement in section 5, event mapping in section 5, discovery and process model in section 7 and 9, state in section 4.

## 12. Non-goals

- No ntfy server changes, no APK fork, no ACL changes.
- No change to the topic contract, the `oc-out` tag, or the link-safety allowlist.
- No change to the phone command set or routing rules.
- No project root is discovered outside `projectRoots`; the list stays explicit.
