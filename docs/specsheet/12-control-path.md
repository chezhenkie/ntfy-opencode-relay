# 12-control-path.md - relay control path

Status: implemented. The crate carries the envelope and error codes, the root policy, the runtime file, the listener, and a blocking client; the daemon serves the endpoint and `ntfy-admin` is the console front end. The network mode request follows `10`; the relay returns the command line and the CLI runs it in the current console.

## Why a local endpoint

The relay owns its config, its discovery, and its reload. A CLI that edited `ntfy-relay.json` would be a second writer and could not apply a change without a restart. One loopback endpoint keeps the config single-owned and lets the user change topics while the daemon runs. The user never edits JSON; the manual host route stays for recovery.

## Transport

The daemon listens on `127.0.0.1` with port `0`, so the OS assigns it. Nothing else is reachable: no non-loopback bind, no TLS, no proxy header trust. The listener starts after the lock is taken and stops during shutdown.

After the bind the daemon writes `<root>\ntfy-relay-endpoint.json`:

```json
{"pid":1234,"port":51234,"token":"<32 lowercase hex>"}
```

The file is `0600` on Unix and replaced atomically on every start. A stale file is ignored: the CLI accepts it only when the pid is alive and the request is answered. The token is fresh per process, from OS randomness, and is never logged. A clean stop removes the file; a crash leaves a stale file that the next start replaces. The name is separate from `ntfy-relay-control.json`, which is the command table; 0.3.0 to 0.4.1 shared one name, and a hard kill then left a runtime file that the next start read as a table and refused.

One request per connection with `Connection: close`, an 8 KiB header cap, a 16 KiB body cap, and a 5 s deadline. `Authorization: Bearer <token>` is required on every request; a missing or wrong token is `401` with no detail. Method and path are parsed as exact strings; anything else is `404` or `405`.

## Envelope

Requests carry JSON bodies; GET requests carry none. Every response is JSON:

```json
{"ok":true,"data":{...}}
{"ok":false,"error":{"code":"covered","message":"path is inside a served root"}}
```

Codes: `invalid`, `not-found`, `conflict`, `covered`, `unsupported`, `internal`, `unauthorized`. HTTP status is `200` for `ok:true`, `400` for `invalid`, `401` for `unauthorized`, `404` for `not-found`, `409` for `covered` and `conflict`, `500` for `internal`, `501` for `unsupported`.

## Topic operations

A topic is a project folder name. Served folders are those under an entry in `projectRoots`. The control path edits that list, which is the only user-owned key in the relay config.

`GET /v1/topics` returns `{"roots":[...],"topics":[...]}` from the running discovery.

`POST /v1/topics/add` with `{"path":"<folder>"}`:

- `~` is expanded, the path must be absolute after expansion, must exist, and must be a directory; the stored value is canonicalized with the platform's case rule.
- A path inside an entry is `409 covered`; a path equal to an entry is `409 conflict` with `path is already served`. The config is not written in either case.
- A path that contains existing entries replaces them: the contained entries are dropped in the same write, and one log line names them.
- The new list replaces `projectRoots` wholesale. The file is written to a temporary sibling, flushed, and renamed, so a failure leaves the old config.
- After the write the daemon runs discovery immediately instead of waiting for the tick, then answers with `{"roots":[...],"topics":[...]}`.

`POST /v1/topics/remove` with `{"path":"<folder>"}` removes an exact entry and applies the same write, discovery, and answer. A folder that no longer exists is still accepted for removal, so a dead root does not need a hand edit. Removing an entry that is not present is `409 conflict`. Removing the last entry is allowed and leaves a relay with no topics; an empty `projectRoots` list is valid at load time.

A write failure is `internal` with the old config intact. The relay logs one line per applied change with the resulting root count and topic count. The live root list is shared with outbound scope, and `ntfy-relay mcp` re-reads the config on every tool call, so a change applies to both without a restart.

## Stop and status

`GET /v1/status` returns `{"pid":n,"started":"<iso>","roots":[...],"topics":[...]}`. It answers only while the daemon runs; the file-based `status` subcommand stays for a stopped relay.

`POST /v1/stop` answers `{"ok":true,"data":null}` and then shuts the daemon down through the same path as SIGTERM: finish the current message, drop the queue with one log line, join the outbound and SSE tasks with a 5 s grace and a 6 s bound per task, remove the lock and the endpoint file. The CLI waits up to 15 s for the pid to exit and reports `still running` at that point. This is the graceful stop on Windows, where the windowless process receives no console event.

## Mode request

`POST /v1/mode/plan` with `{"mode":"tailscale"|"nord"|"lan"}` answers with the validated mode, the absolute deploy path, and the command line `ntfy-deploy mode <name>`. The CLI runs the returned deploy path in the current console and shows the phone URL that deploy prints; on Windows the portproxy step needs an elevated shell. The relay never spawns deploy, never runs elevated, and never edits `ntfy-deploy.json`; `10` owns the decision and the reasons.

## CLI

`ntfy-admin`, a console exe beside the relay. It talks to the endpoint only, with no config parsing and no second backend:

```text
ntfy-admin topics
ntfy-admin add <folder>
ntfy-admin remove <folder>
ntfy-admin status
ntfy-admin stop
ntfy-admin mode <name>
```

It resolves the install root like the relay: `NTFY_RELAY_HOME`, else the directory that holds the binary. It reads the endpoint file and exits `3` with `no running relay` when the file is missing, the pid is dead, or the port refuses. A `401` means a stale token: the request is sent again once, which re-reads the file. Output is one line per item, no color, and `--json` prints the response envelope unchanged; `mode --json` exits `2`, because the mode switch runs the deploy path in the current console and has no plan-only rendering.

Exit codes: `0` ok, `1` refused or failed, `2` usage, `3` no running relay, `4` platform command failed. `stop` waits up to 15 s for the pid to exit and reports `still running` otherwise. Plain output uses `root:`, `topic:`, `pid:`, `started:`, `mode:`, `deploy:`, and `command:` lines. A rejected request prints `FAIL <code>: <message>` on stderr.

## Crate

`control` is a library shared by the daemon and the CLI: request and response types, the path policy, the runtime file, the pid liveness check, the token check, the listener, and a small blocking client. It adds no binary and depends on `core` for config types. `server` owns the socket, the framing, the caps, and the bearer check; `app` injects the config write and the discovery trigger through a call channel, so the endpoint holds no policy of its own. `admin` is the binary: argument parsing, one request per command, and one line per item.

## Tests

Unit: path policy for equal, inside, containing, `~`, relative, missing, and file paths with the Windows case rule; envelope and error mapping; token compare and header parsing; endpoint file write, stale file, and `0600` on Unix; pid liveness. Integration: a listener on port `0` with a fake handler, one request per connection, `401` on a wrong token, and the 16 KiB body cap. The app adds the routing table, the mode plan, the root edit, and the atomic config write; `admin` adds argument parsing, the response rendering, the 401 retry, the stop wait, and the mode run over a fake endpoint.

## Open

- The status payload may carry the last log line later; it does not now.
- A settings endpoint is not defined: `projectRoots` is the only user-owned key, and it is reached through topics.
