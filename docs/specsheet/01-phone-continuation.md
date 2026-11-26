# Phone Continuation Spec

Status: phases 1 to 4 implemented and verified. Phase 4 close-out includes the detached startup wrapper, the reboot and wake tests, and the startup check. Control and inbox retention were added after phase 4. The live path is the Rust daemon; the Node relay was deleted in 0.8.0. The plugin sections below are historical: the patched publisher plugin was deleted in 0.7.0. Path note: since 0.10.0 the relay reads its config, tokens, state, asks, and journal from the install root, `%LOCALAPPDATA%\ntfy-relay\` on Windows; the paths in the bodies below record the earlier layout. Routing note: since 0.11.0 there is no pin. Section 6.7 and the command table carry the current rule: a one-shot target, a 10-minute continuity hint, then the newest activity from `time.viewed` and `time.updated`. Auth note: since 0.14.0 there is one principal, one wildcard row, and one token file, created by `ntfy-auth`. See `10-deploy-mode.md` and `11-auth-bootstrap.md`.

Goal: use the phone as a second client for an existing OpenCode session. The phone reads assistant text and files and sends text and files back into the same session, over one ntfy topic per project.

## 1. Locked decisions

- One topic per project. The topic is the project folder name exactly. No prefix, no slug, no hash, no random suffix. Folder names must match `[-_A-Za-z0-9]{1,64}`; other names are skipped with a warning.
- Duplicate folder names are unsupported. Two project paths with the same folder name share a topic; the relay logs a warning. Do not build machinery for it.
- Outbound is the Rust daemon since 0.5.0; the OpenCode notification plugin is historical. Inbound is a standalone process, the Rust daemon live since 0.4.2; the Node relay was deleted in 0.8.0. The plugin was never used for inbound.
- The relay routes free text into an existing session only. A declared control action such as `new` may create one.
- Default delivery is queue. `/steer` interrupts with steer delivery.
- Turn text up to 3400 bytes is sent inline. Longer text becomes a `.md` attachment with a short inline summary.
- Auto-attach newly created media and document files only. Never source code. Never modified existing files.
- Permission requests can be answered from the phone with `/allow`, `/always`, `/deny`.
- Outgoing text is link-safe. The phone client linkifies any domain-shaped token, so a file name such as `readme.md` opened the `.md` country domain and a scam site. Only allowlisted top-level domains stay linkable; every other domain-shaped token is defanged to `readme[.]md`. This is a security rule.
- Portable code only. No hardcoded user paths, drive letters, OS names, or machine names. Local scope comes from configuration.

## 2. Components

```
OpenCode server  <--HTTP-->  ntfy-relay (Rust app: inbound and outbound tasks)
                                   |
                                   | publish and SSE
                                   v
                             ntfy topic <folder>  <--push and publish-->  phone app
```

| Component | Runs in | Role |
| --- | --- | --- |
| Rust outbound task | `ntfy-relay` daemon | Outbound: turn text, auto-attached files, permission and error pushes |
| Rust inbound task | `ntfy-relay` daemon | Inbound: subscribe, filter, route, prompt, download files, answer permissions |
| Node inbox relay | Standalone Node.js process | Removed in 0.8.0; no rollback exists |
| ntfy server | Podman container | Transport and cache |
| Phone app | Android | Subscribe to project topics, read pushes, publish text and files |

The split follows from how plugins run. A plugin runs per loaded location, so an inbound listener there would duplicate prompts with two locations open and die when a location unloads. The relay is exactly one process, independent of which locations are open; the Rust daemon runs both directions as two supervised tasks.

## 3. Topic contract

Input is the project canonical path: the `canonical` field from `GET /api/project`.

Algorithm:

1. Trim trailing path separators, except for a filesystem root.
2. Take the last path segment using the host path rules.
3. Reject the project when the name is empty or does not match `[-_A-Za-z0-9]{1,64}`.
4. Use the name unchanged as the topic.

No prefix, no lowercasing, no character replacement, no truncation.

Examples:

| Canonical | Topic |
| --- | --- |
| `C:\Users\me\ocdev\ntfy-proj` | `ntfy-proj` |
| `/home/me/ocdev/ntfy-proj` | `ntfy-proj` |
| `C:\Users\me\ocdev\MyApp` | `MyApp` |
| `C:\Users\me\ocdev\My App` | rejected, not a valid ntfy topic |

Topic names are case-sensitive in ntfy. A folder rename changes the topic. Messages on the old topic stay until cache expiry.

## 4. Auth and ACL

Users:

- `phone`: the only principal since 0.14.0, with one row, `*` read-write. Any topic works from the phone; anonymous stays denied. The `admin` and `system` users and the per-topic rows were removed on 2026-11-18.

Setup, no container rebuild, is `ntfy-auth apply` (`11-auth-bootstrap.md`). The equivalent commands are:

```powershell
podman exec -e "NTFY_PASSWORD=<random>" ntfy ntfy user add --role=user phone
podman exec ntfy ntfy access phone "*" rw
podman exec ntfy ntfy token add --label=owner phone
```

The token is written to `%LOCALAPPDATA%\ntfy-relay\ntfy-token.txt` on Windows, or the install directory on Linux, 32 characters, never in git. No password is stored or used; the web UI is out of scope. Tokens inherit user access, and the wildcard row covers every topic.

Tokens for `phone` never expire; the issue phase fails unless `ntfy token list` reports `never expires`. Expiry is only for a temporary token: a test device, a demo, or a one-off client. `-Expires` issues it, writes no consumer file, and refuses a relay restart or the consumer path. Remove a temporary token by prefix when it is no longer needed; an expired token stays listed and returns `401`.

A destructive handler needs its own user, token, topic, and local activation. Nothing on a project topic can destroy state with the phone token alone, and the control registry stays closed.

Verification:

- `phone` can publish and subscribe on an arbitrary topic.
- Anonymous publish and subscribe return 403.
- No other principal exists.

## 5. Outbound contract

The contract below ran in `opencode-ntfy-with-questions@0.2.1` with patches 1 to 3 plus the four patches recorded here. Since 0.7.0 the plugin is gone and `rust\crates\outbound` implements it; the text stays as the behavior record.

### 5.1 Per-project topic

- Derive the topic from `ctx.location.project.canonical`.
- Use the project folder name unchanged as the topic.
- Publish nothing when the canonical is missing or the folder name is not a valid ntfy topic.
- No location gate is needed: one subscriber sees every location; the topic comes from the session's project canonical.
- Every published message gets tag `oc-out`.
- Keep subagent suppression.

### 5.2 Turn text

Events:

- `session.execution.started`: reset the text buffer for the session, mark the turn window.
- `session.text.ended`: append `{assistantMessageID, ordinal, text}`. Sort by event time, then ordinal.
- `session.execution.succeeded`, `session.idle`: terminal. If text exists and no publish happened for this turn, publish.
- `session.execution.failed`: publish the existing error notification, plus buffered partial text when present.
- `session.execution.interrupted`: publish buffered partial text with an `(interrupted)` marker.

Rules:

- Debounce terminal events per session for 3 seconds so `session.idle` plus `session.execution.succeeded` cannot publish twice.
- Clear the buffer after publish or after 10 minutes without a terminal event.
- Suppress subagent sessions.
- Cap the buffered text per turn at 2 MB of UTF-8. Once the cap is reached, drop later parts and log one line per turn.

Payload:

- Inline: title `<project>`, body the assembled text, priority default, tags `oc-out`.
- Long: if UTF-8 length exceeds 3400 bytes, publish one PUT:
  - File body: full text as UTF-8.
  - `Filename`: `turn-<session-short>-<unix>.md`.
  - `Message`: first 500 characters plus `Full response attached.`
  - `Title`: `<project>`.
  - Tags: `oc-out`.
- The attachment carries the buffered text up to the 2 MB turn cap; nothing else is cut.

### 5.3 Files

Auto-attach:

- Subscribe to `filesystem.changed` events.
- Collect events with `event: add` inside the current turn window for the session location.
- At terminal, filter:
  - File exists and is a regular file.
  - Resolved path is inside the project canonical.
  - Extension is in the allowlist.
  - No excluded directory segment.
  - Size at most the configured per-file cap: default 10 MB, accepted range 1 to 20 MB.
  - At most the configured files and total bytes per turn: defaults 8 files and 30 MB, accepted range 1 to 16 files and 1 to 40 MB, with the total at least the per-file cap.
- Deduplicate by resolved path. Sort by modification time, then path.
- Publish one message per file with a PUT, tags `oc-out,oc-attach`, priority low, title `<project> - <filename>`, message `<bytes> bytes`.
- If files were skipped by caps, publish one summary line at min priority. Do not list every excluded source file.
- In the Rust path, eight sequential PUTs can hold the outbound worker about 90 seconds; the bounded event channel drops and counts frames during that time.
- Never attach `.ntfy-inbox` content.

Allowlist, configurable:

```
png jpg jpeg webp gif bmp tiff svg avif heic
mp3 wav ogg flac m4a aac opus
mp4 webm mov mkv
pdf txt md csv docx xlsx pptx epub
zip 7z tar gz
```

Excluded directory segments:

```
.git node_modules .cache .ntfy-inbox .opencode logs dist build bin target venv .venv __pycache__ temp resources .scratch
```

Explicit tool:

- Serve `ntfy_send_file` from `ntfy-relay mcp`; the live registration is `mcp.servers.ntfy` with `codemode: false`.
- Input: `path` string, optional `message` string.
- Description: send a local file to the user's phone only when the user explicitly asks for it.
- Resolve relative paths against `ctx.location.directory`; reject paths outside the project canonical; reject non-regular files and files over 15 MB.
- Project-confined is the contract: an absolute path outside the project canonical is rejected, including a Desktop or user-home path, and there is no configuration override.
- Publish with `oc-out,oc-manual`, title `<project> - <filename>`, message the optional text.
- Return a short success or failure string to the model. Never throw.

Existing permission, question, and error notifications retarget to the project topic and keep their current behavior.

### 5.4 Link safety

The phone client linkifies any token that looks like a domain. `.md` is Moldova's top-level domain, so `readme.md` in a turn push opened a scam site in the browser. Extensions must never be read as links.

- Every outgoing turn text body, file title, and file message passes through `core::link_safe`. The same filter covers acks and error pushes.
- A token stays as written when its last label is on the allowlist: com net org edu gov mil int info biz app dev io ai co cloud tech blog site online shop store, plus the common country domains nl de be uk ie fr es pt it ch at dk se no fi is pl cz sk hu ro bg gr hr si ee lv lt lu ua ca us mx br au nz jp kr cn hk tw sg in za il ae eu.
- Any other token that ends in a real IANA top-level domain, or in any two-letter suffix, gets its last dot replaced with `[.]`. `readme.md` is published as `readme[.]md`.
- Scheme URLs and dotted names that are not top-level domains, such as `package.json` or `main.mjs`, pass through unchanged.
- The long-text attachment keeps the raw turn text, so attachment content is never rewritten.
- The Rust table is generated from `tools\link-safety\tlds.txt`; `npm run tlds` fails when it drifts. The Node copy was deleted with the relay in 0.8.0.

### 5.5 Plugin configuration (historical)

This section records the plugin that was deleted in 0.7.0. Nothing reads these files and no install step needs them; outbound is the Rust task.

Add to `~/.config/opencode/notification-ntfy-with-questions.json`:

```json
{
  "turnText": { "inlineMaxBytes": 3400, "attachmentPrefix": "turn" },
  "autoAttach": {
    "enabled": true,
    "extensions": ["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "svg", "avif", "heic", "mp3", "wav", "ogg", "flac", "m4a", "aac", "opus", "mp4", "webm", "mov", "mkv", "pdf", "txt", "md", "csv", "docx", "xlsx", "pptx", "epub", "zip", "7z", "tar", "gz"],
    "maxFiles": 8,
    "maxFileBytes": 10485760,
    "maxTotalBytes": 31457280
  },
  "tools": { "sendFile": true }
}
```

Change `ntfy.token` to `{file:~/.config/opencode/ntfy-token.txt}`.

## 6. Inbound relay

### 6.1 Process

- Process `ntfy-relay run`, built from `rust\crates\app`. The Node relay this section originally specified was deleted in 0.8.0.
- Single instance. Lock file in the state directory with PID and start time. The lock is created atomically with `open('wx')`. On `EEXIST` the relay reads the owner: a live PID exits with code 0, an unreadable lock fails closed, and only a readable lock with a dead PID is removed before one retry.
- Signals abort the SSE stream, wait up to 5 s for each task to stop (a 6 s bound), log the number of dropped queued messages, then release the lock.
- Writes its own log with timestamp, level, message. Default level `info`, console echo off. Prompt text is never logged; decision lines carry the message ID and byte count instead. Rotate at 5 MB, keep one old file. `logging.echo` may be enabled for a manual console run.
- Heartbeat every 5 minutes with `topics=`, `lastEventAge=`, and `queue=`. A large age is a warning, not a restart condition.
- Config load failures and fatal errors exit non-zero with a clear log line.
- A file that is unreadable, oversized, not UTF-8, invalid JSON, or another version is renamed to `.corrupt-<ms>` and the relay starts with empty state instead of crash-looping.
- Network errors never exit. Reconnect with exponential backoff, 1 second to 60 seconds, jitter. Reset the backoff only after a connection stayed up for 60 seconds, and time out the response headers after 30 seconds.

### 6.2 Configuration

`%LOCALAPPDATA%\ntfy-relay\ntfy-relay.json`, no secrets committed. Relative paths resolve inside the install root (spec `10`); the live config names the token, log, and state there.

```json
{
  "enabled": true,
  "ntfy": {
    "server": "https://ntfy.example.com",
    "tokenFile": "ntfy-token.txt",
    "replayWindowMinutes": 60,
    "maxMessageBytes": 16384
  },
  "projectRoots": ["~/projects"],
  "opencode": { "command": ["opencode"], "timeoutMs": 30000 },
  "routing": { "preferRunning": true, "delivery": "queue" },
  "files": { "inboxDir": ".ntfy-inbox", "maxFileBytes": 15728640 },
  "outbound": { "enabled": true },
  "logging": {
    "file": "ntfy-relay.log",
    "heartbeatMinutes": 5,
    "level": "info",
    "echo": false
  },
  "stateFile": "ntfy-relay-state.json"
}
```

- Example values only. The local config supplies the real server, token file, and roots.
- `~` expands to the home directory. Relative roots resolve against the config file.
- `projectRoots` matches with a path boundary. Case-insensitive only on Windows. The Rust path accepts several roots; each folder name is a topic, discovery keeps the newest 150, and a duplicate folder name keeps the more recently active project with a warning.
- The Rust path uses rustls and its public root store. There is no custom CA and no native certificate store, so the ntfy endpoint must present a publicly trusted certificate.
- `opencode.command` is accepted for compatibility and ignored: the relay calls the OpenCode HTTP API directly, so it needs no binary and no PATH.
- There is no OpenCode binary override; the API base URL comes from the service registration or `opencode.baseUrl`.
- `logging.level` is `debug`, `info`, `warn`, or `error`. `logging.echo` defaults to false because stdout is redirected to a non-rotating file; enable it only for a manual console run.

### 6.3 Project discovery

- Every 60 seconds and at startup, call `GET /api/project` over HTTP.
- Keep projects whose canonical is inside a configured root.
- Skip a project without a string id, with a warning.
- Deduplicate by canonical. A canonical may have more than one project ID; keep all IDs for routing.
- Build a map topic -> `{canonical, projectIDs, lastActivity}`.
- Skip a project whose folder name is not a valid ntfy topic, with a warning.
- On duplicate folder name, log `WARN duplicate topic` with both canonicals and keep the project with the newest `time.active`. Re-evaluate the winner on every refresh.
- Compare the full map on every refresh, including project IDs and `lastActivity`, and update it in place.
- Close and reopen the subscription only when the subscribed topic set changes.
- Keep at most 150 topics: above that, subscribe to the 150 with the newest `lastActivity` and log each skipped topic.

### 6.4 Subscription

- One multi-topic SSE connection: `GET <server>/<topic1>,<topic2>/sse?since=<unix>`.
- Header `Authorization: Bearer <token>`.
- Up to 150 topics. Above that, subscribe to the 150 topics with the newest `lastActivity` and log every skipped topic. Do not refuse all.
- First start: `since` is now minus `replayWindowMinutes`.
- Reconnect: `since` is the last processed message time minus 60 seconds.
- The Rust path drops and resubscribes after 90 seconds with no bytes; the ntfy keepalive is 45 seconds.
- Parse SSE `data:` lines into JSON. Ignore `open`, `keepalive`, and non-message events.

### 6.5 Message filter

Accept only when all are true:

- `event` is `message`.
- Topic is in the project map.
- Tags do not contain `oc-out`.
- Body is non-empty, or an attachment exists.
- Body is at most `maxMessageBytes`.
- Message time is at least now minus `replayWindowMinutes`, measured against the enqueue time (`receivedAt`), not the drain time, so a backlog does not make accepted messages stale.
- Message ID has not been processed.

Reject silently with a debug log line. Keep a bounded processed set of the last 500 IDs per topic. The replay cursor (`lastProcessed`) advances only after a message is handled; a queue drop never moves it, so a dropped message can be replayed.

### 6.6 State

`~/.local/state/opencode/ntfy-relay-state.json`, now `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-state.json`:

```json
{
  "version": 1,
  "targets": { "ntfy-proj": { "last": "ses_...", "lastAt": 1794662225371, "pending": "ses_...", "pendingAt": 1794662300000 } },
  "topics": { "ntfy-proj": { "lastTime": 1794166767, "lastIds": ["abc123"] } }
}
```

- Atomic writes: write a temp file in the same directory, then rename.
- The file is compact JSON, no pretty printing.
- On restart, do not execute a message that is already in `lastIds` or older than `lastTime` minus 60 seconds.
- Hints are advisory: `last` and `pending` are validated against the live snapshot on every message, and both expire. There is no pin. A 0.10.0 `pins` field is ignored and dropped on the first write.
- Both paths keep 500 IDs per topic. Target rows and topic rows are capped at 512, and target rows are trimmed on write, so the map cannot grow past the cap between reads. A temporarily missing project must not replay its messages.
- A file that is unreadable, oversized, not UTF-8, invalid JSON, or another version is quarantined as `<name>.corrupt-<ms>` (a second fault in the same millisecond adds `-<n>`) and the relay starts empty. Backups older than 7 days are removed at startup.

### 6.7 Session routing

For each message:

1. Resolve project IDs for the topic.
2. Read the hints: `last`/`lastAt` and `pending`/`pendingAt`.
3. Fetch the snapshot: `/api/session?project=<id>&limit=20&order=desc&archived=false` for each project ID, merged, carrying `time.updated`, `time.viewed`, and `parentID`; plus `/api/session/active` for the running flag.
4. Drop child sessions; a subagent is never a target, including when a pending or continuity hint names one.
5. If `pending` is at most 10 minutes old and its id is in the snapshot, use it.
6. Else if `last` is at most 10 minutes old and its id is in the snapshot, use it.
7. Else use the highest activity, where activity is `max(time.viewed, time.updated)`. Ties break on running, then `last`, then id.
8. If there is no session, publish an error push and stop. Never create one for free text.

Resolution failures are delivery failures: a failed lookup publishes an error push and a log line with the message ID, so the phone sees the problem.

Default use needs no command, and no stored target can outlive the desktop's newer activity. `/to` lists five numbered titles; `/sessions` lists twenty. A number or title fragment sets the one-shot target. Hints are per topic.

A new session is created only by `/new [title]`, which targets it; free text never creates one. `/new` needs `adapter.agent` and `adapter.model` in the config.

The selected session title is used in acks and titles. Fall back to the first 8 characters of the ID.

### 6.8 Prompt delivery

- Default: `POST /api/session/<id>/prompt` with `{"text": "...", "delivery": "queue"}`.
- `/steer <text>`: same call with `delivery: "steer"`.
- With files: add `files: [{"uri": "file:///C:/...", "name": "<original name>"}]` and append `Attached file: <absolute path>` to the text. Always send a `file://` URI, built from the saved absolute path.
- A file-only message carries `Attached file: <absolute path>` as its text.
- Timeout 30 seconds per attempt. Retry network errors and 5xx twice, 1 second then 2 seconds. Never retry 4xx.
- Ack on success with min priority, title `<project> - queued`, body `queued to <session title>` or `steering <session title>`, tags `oc-out`. Ack bodies are capped at 3400 bytes.
- On failure, publish an error push with the HTTP status and a short message; log stderr in full.
- A retry can repeat a prompt whose response was lost: the request may have reached OpenCode even though the relay saw a network error. There is no idempotency key, so prompt delivery is at-least-once in that case, not at-most-once.
- Inbound handling stays at-most-once: the message is marked processed before delivery, every failure produces one error push and one log line with the message ID, and there is no outbox.
- The Rust path limits prompts to 30 per minute per topic with a burst of 10; `/steer` counts. Above that it publishes one `rate limited` error push and sends no prompt.
- Every relay publish call (ack, error) has a 10 second application timeout via `AbortController`. A stalled POST must not block the serial drain.

Verified 2026-11-08 on Windows: an absolute path in `files[].uri` is rejected with HTTP 400; `file:///C:/...` is accepted and the prompt is queued. The relay always sends `file://` URIs. Recorded in `docs/operations.md` and the handoff.

Observed 2026-11-08: the prompt renderer shows the attachment path itself, so the appended `Attached file:` line appears twice in the session. Cosmetic only; keep the appended line as specified.

### 6.9 Inbound attachments

- Use `attachment.url`, `attachment.name`, `attachment.size`.
- Download without a token, over HTTPS except on a loopback host. Cap at 15 MB and verify the size when present.
- Reject a non-loopback HTTP URL. Follow redirects manually, at most 3, and apply the same transport rule and host allowlist on every hop. The deleted Node relay had no host allowlist; the Rust path requires the URL host to equal the `ntfy.server` host or the deployment `baseUrl` host on every hop.
- Sanitize the name: basename only, strip control characters, keep `[A-Za-z0-9._-]`, limit to 120 characters, fallback `file-<epoch ms>`.
- Save to `<project canonical>/<inboxDir>/<unix>-<name>`. Create the directory.
- On collision, prefix with unix milliseconds.
- If download fails, publish an error push and do not send the prompt.
- The inbox directory is local working data. Add it to a global git ignore; do not edit project files automatically.
- Retention: prune `<inboxDir>` at startup and every 6 hours, deleting regular files whose name matches `^[0-9]{10}([0-9]{3})?-` and whose modification time is older than 7 days. Seven days clears the 72-hour server cache and attachment expiry with room for a paused multi-day session. Skip symlinks, directories, and anything that does not match the name pattern. Log one line per run with roots, removed count, and bytes; a per-root failure is logged and does not stop the relay.
- The Rust inbound path also holds a 200 MB per-project byte budget. At prune time and when a file arrives, the oldest matching `<epoch>-<name>` files are removed first; if the new file still does not fit, it publishes one error push and sends no prompt.

### 6.10 Permission replies

- `/allow`: reply `{"decision": "once"}`.
- `/always`: reply `{"decision": "always"}`.
- `/deny`: reply `{"decision": "reject"}`.
- Reply to the published ask record. The registry binds session, request id, title, and push time before the push, and a code selects the record; the routing target and the session's pending list are never read. Spec `09` defines the registry and the ack.
- With no argument, one live ask on the topic is answered; with several, the reply lists their codes and titles and approves nothing. With a code, the matching record is used wherever it lives. The ack names the record's session title.
- A desktop answer removes the record through `permission.replied`; a later phone reply reports that the ask was already answered.
- No pending request: publish an error push.

### 6.11 Commands

A command is a known name, matched case-insensitively after trim. Any other leading-slash text is delivered as a prompt, not swallowed.

| Command | Action |
| --- | --- |
| `/help` | List commands |
| `/status` | Relay status: topics, queue, target, uptime |
| `/new [title]` | Create a session in the project and target it; requires `adapter.agent` and `adapter.model`; title capped at 200 characters |
| `/sessions [count]` | List up to 20 sessions with number, running flag, title |
| `/to` | List five numbered sessions for the next message |
| `/to <number>` | Target entry `<number>` of the last list, within 10 minutes |
| `/to <title fragment>` | Target the single session whose title contains the fragment |
| `/auto` | Drop the pending target |
| `/steer <text>` | Prompt with steer delivery |
| `/allow` | Approve the only pending permission once; with several, list the codes |
| `/always` | Approve the only pending permission for the session; with several, list the codes |
| `/deny` | Reject the only pending permission; with several, list the codes |
| `/yes` | Confirm a held action |
| `/no` | Cancel a held action |
| `/answer <text>` | Cancel the newest live question form with the text as its reason |

`/usr/bin/env is missing` is a prompt. `/nope` is a prompt. A command without its required argument gets a usage ack.

## 7. Message formats

| Source | Tag | Priority | Title | Body |
| --- | --- | --- | --- | --- |
| Turn text | `oc-out` | default | `<project>` | assistant text |
| Long text | `oc-out` | default | `<project>` | summary plus `.md` |
| Auto file | `oc-out,oc-attach` | low | `<project> - <filename>` | `<bytes> bytes` |
| Manual file | `oc-out,oc-manual` | default | `<project> - <filename>` | optional message |
| Permission ask | `lock,oc-out` | default | `<project> - permission requested` | action, patterns, and reply line |
| Session error | `warning,oc-out` | default | `<project> - session error` | error type and message |
| Relay ack | `oc-out` | min | `<project> - queued` | target session |
| Relay error | `oc-out` | high | `<project> - relay error` | short failure |

The relay ignores every message with `oc-out`. That is the only direction marker. Phone messages never carry it.

## 8. Portability rules

- No absolute paths in source. Paths come from the config, the install root, or the environment; no user name, drive letter, or machine name is compiled in.
- Use `std::path` for every join, resolve, and file name; containment checks resolve `..` and symlinks where the code requires it.
- Case-insensitive path comparison only on Windows; the code branches on the platform.
- The relay is Rust with no Node runtime and no npm dependencies. Node is used only by the `tools\` scripts and the docs site.
- Config, state, logs, asks, forms, and journal live in the install root: `%LOCALAPPDATA%\ntfy-relay\` on Windows, the install directory on Linux. No relay file lives under the OpenCode config or state directories.
- Startup is a deployment artifact per OS: two scheduled tasks with a generated VBS on Windows, three systemd user units on Linux, or the no-autostart path when `systemctl --version` or `systemctl --user show-environment` fails.
- macOS is untested and unsupported. Case folding is Windows-only, so a case-insensitive macOS filesystem can fail containment.
- Tests run in CI on Linux and Windows; fixtures cover both path styles.

## 9. Errors and limits

- Relay log line per decision: accepted, filtered, routed, prompted, failed.
- Never log the token, full headers, or prompt text. Decision lines carry the message ID and byte count. A short failure reason is capped at 200 characters.
- API failures produce one error push, not a retry storm. Subprocess stderr is logged in full to the rotating file.
- Bound every in-memory structure: 500 processed IDs per topic, 20 sessions per query, 8 attachments per turn.
- Attachment expiry is 72 hours, capped by cache duration.
- The phone must have the VPN client connected. The host must be awake. The relay cannot wake a sleeping host.

## 10. Testing

Unit and integration tests run with `cargo test`. The workspace holds 742 tests at 0.6.13: 108 in `deploy`, 104 in `outbound`, 97 in `inbound`, 129 in `app`, 101 in `core`, 61 in `ntfy`, 56 in `opencode`, 34 in `control`, 30 in `auth`, and 22 in `admin`. One test makes a connect attempt to an unroutable address to prove the 5 s connect deadline; test directories are unique paths under the system temp and are removed on drop.

- Topic derivation: Windows and POSIX paths, trailing separators, invalid characters, length limit, empty basename, duplicate folder names.
- Filter: `oc-out`, stale, oversized, empty, attachment-only, duplicate ID.
- State: atomic write, restart dedupe, stale hint.
- Routing: activity order, pending target, continuity window, child exclusion, missing session, multiple project IDs.
- Text assembly: order, multiple assistant messages, debounce, interrupted.
- Auto-attach: add versus change, allowlist, excluded dirs, caps, missing file, symlink, path outside project.
- Filename sanitization and path traversal.
- Config validation and `~` expansion.
- Lock: two concurrent acquires yield one winner; an unreadable lock fails closed; a stale readable lock is replaced.
- Queue: a dropped message does not advance the replay cursor; a delayed message is accepted against its enqueue time.
- State: corrupt JSON is quarantined; writes are compact.
- Publish: application timeout aborts a stalled request; ack bodies are clamped to 3400 bytes.
- Attachments: HTTP, HTTPS-to-HTTP redirect, and more than 3 redirects are rejected.
- Commands: case-insensitive known names; unknown leading-slash text becomes a prompt.
- Discovery: changed project IDs refresh without a resubscribe; more than 150 topics keeps the newest 150.
- SSE: CRLF split across chunk boundaries still produces one event.

The plugin verifier and its assertions were deleted with the plugin in 0.7.0. The Rust outbound suite covers turn assembly, the byte threshold, attachment naming, link safety, and the file filter.

Live acceptance:

1. Phone text to an idle session starts a turn in that session, no new session.
2. Phone text to a running session is queued, then runs.
3. `/steer` interrupts the running turn.
4. `/sessions`, `/to`, `/auto` select a one-shot target.
5. Long answer arrives as one `.md` attachment, content complete.
6. New PNG and MP3 auto-attach; new `.ts` and modified files do not.
7. Phone PNG arrives in `.ntfy-inbox` and the session can read it.
8. `ntfy_send_file` delivers an existing file.
9. Permission ask answered with `/allow`, `/always`, `/deny`; the turn continues or stops.
10. Two locations loaded produce exactly one push per turn and one prompt per phone message.
11. Relay restart does not re-execute an already processed message.
12. Reboot with no TUI: first phone message still reaches the session.
13. Tests pass in CI on Linux and Windows; `cfg(unix)` tests do not run on Windows.

Acceptance criteria:

- One topic per project, both directions, no loop.
- No source code auto-attached.
- No new session created by the relay.
- No duplicate pushes or prompts.
- Deny-all auth preserved; private network only; no Google components.
- Portable code and config-driven roots.

## 11. Rollout (completed)

Phase 1, auth and relay text:

1. Create `phone`, the token, and the `*` row.
2. Start the installed `ntfy-relay run` from the install root. The Node files this step originally named were deleted in 0.8.0.
3. Run the relay in the foreground against `ntfy-proj`.
4. Send text with the phone token; confirm the prompt lands in the existing session and the relay logs one accepted message.

Phase 2, plugin outbound (historical, superseded in 0.5.0 and deleted in 0.7.0):

1. The plugin, its patch script, and its offline verifier are gone from the tree; do not restore them.
2. The Rust outbound task publishes one push per turn; see `03-rust-relay-merge.md` and operations 7.9.

Phase 3, files and permissions:

1. File auto-attach and `ntfy_send_file`.
2. Inbound attachment download and prompt `files`.
3. Permission commands.
4. Run the live acceptance matrix.

Phase 4, lifecycle and docs:

1. Startup tasks or units and the single-instance lock.
2. Startup-check integration: relay alive and heartbeating.
3. Reboot and wake tests.
4. Update the manual under `docs\` and the changelog.
5. Update the handoff.

Rollback:

- Stop the relay. Outbound notifications continue.
- Restore a previous `ntfy-relay.exe` build for the Rust path, or check out the last known good commit and rebuild. The deleted plugin and patch script are not restored.
- Auth changes are additive. A wiped token file is restored by `ntfy-auth apply`, and the phone token never changes, so the app stays as it is. Re-adding `system` and `admin` is not a rollback: it restores the dual-token model that was removed.

## 12. Out of scope

- Answering the question tool's multiple-choice prompts.
- Token-by-token streaming.
- Multi-host arbitration when two machines run the relay for the same topics.
- Waking a sleeping host.
- Public exposure, a public tunnel, Google push, or PWA background delivery.

## 13. Risks

- The MCP registration, the event stream, and the session API can change with an OpenCode upgrade. Re-run the smoke suite and the live gates after one.
- A systemd host without a live user manager takes the no-autostart path; the relay then needs `ntfy-relay ensure` after each boot.
- After a reboot with no TUI, the first phone message reached the session in the verified run; re-check after an OpenCode upgrade.
- Duplicate folder names merge topics by design.
- A second relay on another machine would double-handle messages. Run one relay per ntfy account; document it.
- Attachment and cache expiry is 72 hours.
- A permission ask stalls the turn until answered. The phone commands are the mitigation.
- The laptop must be awake and online for any of this to run.

## 14. Deliverables

```
docs/specsheet/01-phone-continuation.md   this file
rust/                             Rust workspace: core, ntfy, opencode, inbound, outbound, app
tools/                            image build, token rotation, doc checks
docs/                             operation and troubleshooting manual
```

Local-only configuration, not committed:

```
%LOCALAPPDATA%\ntfy-relay\ntfy-relay.json
%LOCALAPPDATA%\ntfy-relay\ntfy-token.txt
```

On Linux the same two files sit in the install directory.

The Rust cutover in `03` is done; the Node relay was deleted in 0.8.0, so there is no implementation switch and no inbound rollback to it. Do not redesign the locked decisions.
