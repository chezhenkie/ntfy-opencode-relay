# 7. Operations

## 7.1 Machine lifecycle and restarts

Podman is daemonless. Killing the `podman` CLI does nothing to running containers. Killing `podman system service` inside the machine also leaves them running; conmon and crun own the processes. Killing `wslrelay` or `gvproxy` only breaks port forwarding.

Stopping the machine is different: `podman machine stop` shuts down the WSL2 VM and every container in it. Containers do not survive that.

Auto-start after a machine start needs two things:

1. The container must have a restart policy:

```powershell
podman inspect ntfy --format '{{.HostConfig.RestartPolicy.Name}}'
```

Expected: `unless-stopped`.

2. The user-level `podman-restart.service` must be enabled inside the machine:

```powershell
podman machine ssh 'systemctl --user is-enabled podman-restart'
```

Expected: `enabled`. If it is not:

```powershell
podman machine ssh 'sudo chown -R user:user /home/user/.config/systemd'
podman machine ssh 'systemctl --user enable --now podman-restart'
```

The `chown` is needed because the machine image ships `~/.config/systemd` owned by root, which makes `systemctl --user enable` fail with `Access denied`.

With both in place, a stop/start cycle restores the container:

```powershell
podman machine stop
podman machine start
podman ps --format '{{.Names}} {{.Status}} {{.Ports}}'
```

```
ntfy Up 11 seconds (starting) 0.0.0.0:8810->8080/tcp
```

The machine itself is not started automatically by Podman. Start it manually with `podman machine start`, or let Podman Desktop start it at login.

Docker Desktop substitutes one-for-one in the container commands below (`docker stop`, `docker logs`, `docker exec`) and has no machine step; the tarball is a docker-archive. `ntfy-deploy` currently drives Podman only, so a Docker host manages the container itself.

The private network client starts automatically and reconnects without a tray. A tray UI is optional and does not carry connectivity.

Windows:

```powershell
podman stop ntfy
podman start ntfy
podman logs -f ntfy
podman exec -it ntfy sh
```

Ubuntu native:

```bash
sudo systemctl restart ntfy
journalctl -u ntfy -f
```

Ubuntu Podman:

```bash
podman restart ntfy
podman logs -f ntfy
podman exec -it ntfy sh
```

Rebuild after changing `Dockerfile` or a baked key in `server.yml`:

```powershell
.\tools\build-image.ps1
ntfy-deploy apply
ntfy-deploy status
```

The build fetches the pinned ntfy release and checks it against `NTFY_SHA256`, so it needs outbound HTTPS to GitHub. A version change updates `NTFY_VERSION` and `NTFY_SHA256` in the Dockerfile together; the build tags `localhost/ntfy:server` regardless of version.

A mode or base URL change needs only `ntfy-deploy apply` with the new `ntfy-deploy.json`; it recreates the container and never rebuilds the image. A rebuild that keeps the tag is rolled out by the next `apply`, which compares the container's image ID with the local one and replaces the container when they differ. A relay root with no deploy config gets one from `ntfy-deploy init --mode tailscale --base-url https://<name>` (or `nord`/`lan`): it writes the file once, refuses when one exists, and prints `next: ntfy-deploy apply`. `ntfy-deploy mode` writes that config for you: with no argument it lists the paths it detects (Tailscale, Nord Meshnet, LAN) as a numbered menu, applies the choice, and prints the phone URL; `mode tailscale`, `mode nord`, and `mode lan` do the same without the menu, and `mode ... --dry-run` shows the document and plan first. See [switching networks manual](switching-networks-user-manual.md) for the operator steps. Modes are `proxy`, `vpn`, and `lan`; the config sits in the relay root. On Windows the switch needs an elevated shell for the portproxy step. `ntfy-deploy apply --dry-run` prints the plan, `ntfy-deploy status` reports mode, container, health, and the active portproxy. Exit codes: 0 ok, 2 invalid config or unavailable path, 3 unhealthy, 4 platform command failed. The relay keeps `ntfy.server` on loopback in every mode, so a switch never restarts it.

The image runs as uid 1000. A volume created by the earlier root image needs a one-time chown after the stop, see [windows-podman.md](windows-podman.md#23-build-context).

Publish from the host with `curl.exe`:

```powershell
$token = (Get-Content "$env:NTFY_RELAY_HOME\ntfy-token.txt" -Raw).Trim()
curl.exe -sS -H "Authorization: Bearer $token" -d "message" https://ntfy.example.ts.net/ntfy-proj
```

A rebuild-free config change goes through `ntfy-deploy apply` with the new `ntfy-deploy.json`; `ntfy-deploy mode` writes that file for the network step. A manual `podman run` is not a supported shortcut: the baked `server.yml` sets `attachment-cache-dir` and no `base-url`, so the container exits at startup unless `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY` are passed.

## 7.2 Auth and tokens

`server.yml` sets:

```yaml
auth-file: /var/cache/ntfy/auth.db
auth-default-access: deny-all
```

`auth.db` lives on the `ntfy-cache` volume, so users and tokens survive container replacement. Anonymous read and write are denied. `/v1/health` stays anonymous for the container healthcheck and for uptime checks.

An ACL row is one entry in `auth.db`: a user, a topic pattern, and a permission. `rw` allows publish and subscribe, `read` subscribes only, `write` publishes only, `deny` blocks both; `*` or `everyone` is the anonymous principal, left with nothing under `deny-all`.

The bootstrap is `ntfy-auth`, which runs the container CLI through `podman exec`. Place `ntfy-auth.json` in the relay root, then:

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" apply
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" apply --dry-run
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" status
```

`apply` creates the user, the `*` row, and the token file only where they are absent; a second run issues no write. A wiped token file is restored by one run. `status` reports the user, the access pair, and the token state without changing anything. Exit codes: 0 ok, 2 invalid config, 3 incomplete, 4 platform command failed. Spec 11 carries the config shape and the rules. The equivalent manual commands are `ntfy user add --role=user phone`, `ntfy access phone "*" rw`, and `ntfy token add --label=owner phone` inside the container; a non-interactive `user add` takes `NTFY_PASSWORD` from the environment. Tokens never expire unless `--expires` is set and carry the full rights of their user.

A token can also authenticate with HTTP basic auth as an empty username plus the token as the password (`-u ":tk_..."`). A non-empty username with the token as the password returns `401`.

Show state and change a password:

```powershell
podman exec ntfy ntfy user list
podman exec ntfy ntfy access phone
podman exec -e "NTFY_PASSWORD=<new>" ntfy ntfy user change-pass phone
```

The `phone` user is the only principal, with one row, `*` read-write, since 2026-11-18. Any topic works with no host step; a new project that the relay should serve needs its folder to sit under a root in `projectRoots`, not an ACL row. The token is the only credential: the app carries it as a custom `Authorization: Bearer` header, and the relay and the MCP read `%LOCALAPPDATA%\ntfy-relay\ntfy-token.txt`. No password is used and the web UI has no login. The `system` and `admin` users and the per-topic rows are removed; the token is not duplicated on disk.

A user entry with the token in the password field returns `401`; the header is the only app credential.

Verified 2026-11-18: `phone` publishes and polls a topic that was never granted with `200`, and anonymous publish returns `403`.

Test enforcement over HTTPS:

```powershell
curl.exe -sS -o NUL -w "anon publish=%{http_code}`n" -d test https://ntfy.example.ts.net/ntfy-proj
curl.exe -sS -o NUL -w "token publish=%{http_code}`n" -H "Authorization: Bearer <token>" -d test https://ntfy.example.ts.net/ntfy-proj
```

Expected: `403` and `200`.

### Token rotation

`tools\rotate-token.ps1` rotates one token in two phases. The old token stays valid until the revoke phase, so a failed paste or restart cannot lock anything out.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\rotate-token.ps1 -User phone -RestartRelay
# copy the token from ntfy-token.txt into the app before revoking the old value
Get-Content "$env:NTFY_RELAY_HOME\ntfy-token.txt"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\rotate-token.ps1 -User phone -RevokeOld tk_old5678
```

The script reads the container and the default user from `ntfy-auth.json` in the relay root; `-User` only overrides the user. The issue phase creates the token, writes `ntfy-token.txt` atomically, and prints `TOKEN prefix=<8 chars>... file=<path>`: the full token appears only in the file, never in the terminal, so a transcript or a screen share holds no usable credential. With `-RestartRelay` it stops the relay, starts it with `ntfy-relay ensure`, and requires a `200` publish to `ntfy-probe` with the new token before it reports PASS; the probe reads the bearer from a temporary curl config that is deleted in a `finally`. MCP processes hold the token they read at startup, so they pick up the new value only after an OpenCode reload. The revoke phase refuses a prefix shorter than 4 characters, a prefix matching no token or more than one, and the token in the consumer file, and it passes the old token to `ntfy token remove` on standard input rather than the command line. `-DryRun` prints the planned calls and the prefixes currently held.

A temporary token carries an expiry and is the only kind that may. It serves a test device, a demo, or a one-off client, never the relay or the phone; the issue phase writes no consumer file and refuses `-RestartRelay`, `-RevokeOld`, or the live consumer path.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\rotate-token.ps1 -User phone -Expires 7d -Label demo
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\rotate-token.ps1 -User phone -RevokeOld tk_temp12
```

`-Expires` takes one unit, `Nm`, `Nh`, `Nd`, or `Nw`, with N from 1 to 9999. The issue phase reads `ntfy token list` back, fails when the server did not apply the expiry, and prints the token plus the computed UTC end. A plain issue asserts `never expires` the same way, so a live token cannot silently acquire an expiry. An expired token stays listed until it is removed and returns `401` like any other invalid token.

A process that read the token at start keeps the old one until it restarts, and each MCP process caches the token it read. A reload of the shared OpenCode service picks up the new value; it is shared with other sessions and runs only with approval. Rotation is `phone` only.

Rehearsed on `system` before that user was removed in 0.14.0 (2026-11-18): the new token was written, the relay restarted with it, the probe publish returned `200`, the revoked token returned `401`, and `send_file` delivered after `opencode reload`. Rehearsed on `phone`: the app carries the token as an `Authorization: Bearer` custom header with no user entry, `ntfy token list` showed a fresh access from the phone, and the revoked token returned `401`.

## 7.3 Startup and repair

`ntfy-deploy install` is the startup and repair step. It places the four binaries, sets `NTFY_RELAY_HOME`, writes `ntfy-relay.json` and `ntfy-auth.json` when they are missing, and registers two scheduled tasks from generated XML: `ntfy-relay-watchdog` every five minutes runs `ntfy-relay.exe ensure`, and `ntfy-relay-startup` at logon runs the generated `ntfy-relay-startup.vbs` through `wscript.exe`, which runs `podman machine start` and `ntfy-relay ensure` in order with hidden consoles. It never writes `ntfy-deploy.json`; `init` creates that file once and `mode` rewrites it. The container returns through `unless-stopped` and `podman-restart.service`; `apply` stays a deploy step. It deletes the old logon shortcut and the installed script folder. A re-run is the upgrade and repair step; it keeps the token, state, log, and any existing config. The install root must sit under `%LOCALAPPDATA%` on Windows or the user's home on Unix: the token, configs, and runtime file rely on inherited per-user ACLs, because Windows has no owner-only mode, and `install` prints a warning when the root is elsewhere. Exit codes: 0 ok, 2 refused or a file failure, 4 a platform command failed.

The read-only check is `ntfy-relay status`: config, lock pid, liveness and phase, state counts, the last log line, then the ten startup rows, each PASS, FAIL, or SKIP. The last two rows report an elevated process and the install root's ACL, naming the root and the inherited per-user assumption. It starts and repairs nothing and exits 0, so a FAIL line names the fault. To act on it, re-run `ntfy-deploy install`, start the missing layer by hand, or read `logs\relay-ensure.log`.

Both tasks use the user's interactive token, least privilege, `IgnoreNew`, `StartWhenAvailable`, the battery defaults, and a ten minute limit. The watchdog names the windowless relay binary directly. The logon task names `wscript.exe` and the generated script, which runs `podman machine start` and `ntfy-relay ensure` with style 0; a direct action that names a console program flashes a window. The relay start is `ntfy-relay ensure`: it checks the lock owner and starts the daemon detached, with output under `logs\`. The watchdog covers a relay death between logons; the logon task covers a reboot.

The PowerShell launcher and the startup check were removed in 0.6.0; since 0.6.1 the logon chain runs through the generated VBS. `docs\startup.md` carries the manual start order and the per-layer checks.

## 7.4 OpenCode notifications

Outbound is the Rust daemon since 2026-11-11 (0.5.0). `rust\crates\outbound` subscribes to `/api/event` and publishes session idle, execution error, permission prompts, `question` tool prompts, and assistant turn text to the project folder name as the ntfy topic, for example `ntfy-proj`. It runs in the relay process, not in the OpenCode server. Every message carries the `oc-out` tag, which the inbound relay ignores. The MCP tool `ntfy_send_file` sends one existing file on request.

The `/api/event` stream has no replay cursor. It closes and reconnects after 90 seconds with no bytes, as does the ntfy subscription. Since 0.7.4 the daemon backfills completed assistant text: after a stream reconnect or a start it reads `GET /api/session/{id}/message` (`type=assistant`, newest first, cursor pages) and publishes text it has not delivered. Reconnect scope is the open turns, the session that published last, and the sessions the state file remembers; a cold start reads the newest 20 sessions per project root inside the 15-minute window, capped at 200 messages. One seen journal at `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-seen.jsonl` holds event ids and assistant message ids for 15 minutes (8192 ids at most). A message id is marked before its push and committed after it, so a live part and a backfilled message publish once and a restart inside the window republishes nothing. The journal is an optimization: if it cannot be read or written, the daemon logs once and keeps the in-memory window. Permission asks and question forms have no history endpoint and are not recovered. There is no event forwarder and no second feed; `/api/event` is the only source.

Gate 2026-11-12 (0.7.4): the forwarder was moved out and `opencode reload` ran, then the relay was stopped for 41 s while a gate turn completed. The start logged `backfill cold=true sessions=20 messages=111 published=1` and the phone received `GATE-A`, `GATE-B`, `GATE-C` once. A second start of the same binary logged `published=0`. The forwarder, the loopback ingest, `ntfy-relay-forward.json`, and its bearer token were deleted; `.opencode\` is gone from the tree.

Configuration lives outside the repository, in the relay root `%LOCALAPPDATA%\ntfy-relay\`:

- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.json`: `outbound.enabled: true` plus the event switches, subagent suppression, permission delay, turn text, and auto-attach values.
- `~/.config/opencode/opencode.jsonc`: `mcp.servers.ntfy` (`type: local`, `command` = the relay binary plus `mcp`, `cwd: "."`, `codemode: false`) and the permission allow for `ntfy_send_file`.
- `%LOCALAPPDATA%\ntfy-relay\ntfy-token.txt`: the only token, 32 characters, read at daemon start and by each MCP process; the app keeps its copy. When the file is absent the daemon waits for it with one WARN and attaches it within a second, so a fresh root, `ntfy-deploy install`, and a logon start do not depend on the auth step having run first.

Verified 2026-11-11: the daemon logged `event stream subscribed`; one turn produced one `oc-out` text push; a long turn produced a 11361-byte `turn-*.md` attachment; a three-file turn produced three `oc-attach` pushes; a permission ask produced one `lock,oc-out` push 15 s after the ask; a question form produced one `question,oc-out` push; a child session produced none. An agent call to `ntfy_send_file` returned the plugin text and one `oc-out,oc-manual` push. Gap gate: see the 0.7.4 result above.

The publisher plugin `opencode-ntfy-with-questions@0.2.1` is removed; the patched copy was deleted in 0.7.0 and survives only in git history. The behavior text below describes the contract the Rust port implements.

Payload: project folder name, session id, error type and message, permission action and patterns, and the full question text plus options. Subagent idle and error events are suppressed. Permission notifications wait 15 seconds and are cancelled when the request is answered in time. A published permission ask is written to the ask registry with its session id, request id, title, and push time, plus a short code derived from the request id. The push ends with `Reply /allow <code>, /always <code>, or /deny <code>; with one ask, /allow is enough.` `/allow`, `/always`, and `/deny` resolve that record; the routing target is never consulted for a permission reply.

A published question form is written to the form registry with its session id, form id, topic, title, and visible fields before the push. The push carries the question and its options and ends with `Reply with /answer and your answer in your own words.` The relay does not interpret the answer: `/answer` followed by any text cancels the newest pushed form with that text as the reason through `DELETE /api/session/{session}/form/{form}?message=<reason>` (`session.form.cancel`), so the desktop form closes at once and the assistant reads the free text. A 404 or 409 removes the record and answers `that question in <title> was already answered`; any other 4xx keeps the record and pushes the server reason. The registry is `ntfy-relay-forms.json` in the relay root, holds 64 forms, and prunes quarantined copies after 7 days.

Turn text: `session.text.ended` parts are buffered per session, sorted by event time then ordinal, and published once at the terminal event. The terminal is debounced for 3 seconds so `session.execution.succeeded` plus `session.status idle` cannot produce two pushes. Text up to 3400 UTF-8 bytes is inline; larger text becomes a `turn-<session>-<unix>.md` attachment with a 500-character summary. A buffer that reaches 10 minutes without a terminal event publishes its text as a partial with the `partial` tag; the tracker holds at most 256 turn states, and a new state at the cap publishes the oldest buffered text as a partial first. `session.execution.failed` publishes the error and any partial text; `session.execution.interrupted` publishes partial text with an `(interrupted)` marker. The turn text push title is the folder name only; idle, error, permission, question, attachment, and relay pushes keep their event suffix.

Link safety: the Android client linkifies any token that looks like a domain, so `readme.md` became a link to the `.md` country domain and opened a scam site. Extensions must never be read as links. Outgoing turn text, file titles, file messages, and relay acks therefore pass through the link filter: `core::link_safe`. The TLD table is generated from `tools\link-safety\tlds.txt`, and `npm run tlds` fails when the Rust table drifts. A token whose last label is one of the 68 entries in `core::link_safe::ALLOWED_TLDS` stays as written: ae ai app at au be bg biz blog br ca ch cloud cn co com cz de dev dk edu ee es eu fi fr gov gr hk hr hu ie il in info int io is it jp kr lt lu lv mil mx net nl no nz online org pl pt ro se sg shop si site sk store tech tw ua uk us za. Every other token that ends in a real IANA top-level domain, or in any two-letter suffix, has its last dot replaced with `[.]`, so `readme.md` is published as `readme[.]md`. Scheme URLs and dotted names that are not domains, such as `package.json` or `main.mjs`, pass through unchanged. The long-text attachment keeps the raw turn text up to the 2 MB per-turn cap; later parts are dropped and logged once.

Files: new media and document files created during a run are auto-attached at the terminal. The allowlist is png jpg jpeg webp gif bmp tiff svg avif heic mp3 wav ogg flac m4a aac opus mp4 webm mov mkv pdf txt md csv docx xlsx pptx epub zip 7z tar gz. `.git`, `node_modules`, `.cache`, `.ntfy-inbox`, `.opencode`, `logs`, `dist`, `build`, `bin`, `target`, `venv`, `.venv`, `__pycache__`, `temp`, `resources`, and `.scratch` are excluded. Caps default to 8 files, 10 MB each, and 30 MB per turn; the accepted range is 1 to 16 files, 1 to 20 MB per file, and the per-file cap to 40 MB per turn. Cap skips produce one summary line at min priority. Each file is one push tagged `oc-out,oc-attach` with title `<project> - <filename>` and message `<bytes> bytes`. New source files and modified existing files are not attached. OpenCode's native `filesystem.changed` event only fires for files watched through the API, and the server's recursive watcher does not report nested directories reliably, so the daemon scans the project at the terminal for files whose creation time falls inside the run; a root-level watcher is a real-time supplement. The scan costs one project walk per turn and is capped at 20000 entries. The MCP tool `ntfy_send_file` sends one existing file on explicit request: `path` plus optional `message`, project-confined, 15 MB limit, tags `oc-out,oc-manual`, never throws; a refusal returns `isError: true`.

Verified 2026-11-08: the idle probe arrived after a restart; an induced `session.execution.failed` push arrived on the phone. That probe exposed duplicate idle and error pushes from two loaded locations. The first gate read the wrong field and failed open; it now reads `location.directory`, covers idle and error, and passes offline assertions including the native session shape. Live re-check after the second restart: one `session error` push at 19:38:52 and one `session idle` push at 19:39:18, with both locations loaded.

Phase 2 verified 2026-11-08 with the real handler and ntfy client against the live server: one inline turn push and one long turn push arrived on `ntfy-proj` with `oc-out`; the `.md` attachment content matched the source exactly. Offline assertions cover topic derivation, assembly order, debounce, subagent suppression, the 3400-byte threshold, the attachment name, the client topic override, and the phone config sections.

Folder topic re-verified 2026-11-08 after the prefix removal: one turn push, title `ntfy-proj - Folder topic verify`, body `folder-topic-ok`, tag `oc-out`, no duplicate.

Long text re-verified 2026-11-08 on the folder topic: one `oc-out` push carried a `turn-ses_e42c-1794176892.md` attachment of 5340 bytes. The attachment size field matched the download, the inline summary was the first 500 characters, the begin marker was present, and the end marker was the last line. No duplicate push appeared.

Two locations loaded re-verified 2026-11-08: a second location with its own active plugin instance received the same server-global terminal frames. One turn in `ntfy-proj` produced exactly one `oc-out` push there and zero messages on the second topic (`ntfy-probe`). The gate reads `SessionInfo.location.directory` and fails open only when the session lookup fails.

`ntfy_send_file` verified live 2026-11-09: a 70-byte PNG produced one `oc-out,oc-manual` push on `ntfy-proj`, title `ntfy-proj - phase3-probe.png`, message `phase3 manual`, attachment type `image/png` and size 70.

Auto-attach verified live 2026-11-09: a nested PNG and MP3 created during a turn produced exactly two `oc-attach` pushes (70 and 64 bytes, priority low) with the expected titles; a new `.ts` file and a previously existing modified file produced none. The native event feed and the server-side recursive watcher do not cover nested directories, so the terminal scan is the source of truth; the offline suite covers the scan, allowlist, excluded directories, caps, and the tool contract.

Rust outbound verified live 2026-11-11 on `ntfy-proj`: turn text, a long turn (`turn-ses_e343-1794421292.md`, 11361 bytes), a three-file auto-attach set, a `net user` permission ask pushed 15 s later, a question form, and a child session with no push. The MCP tool call returned `Sent gate-set-1.md (3 bytes) to the phone.` with `oc-out,oc-manual`.

The ntfy version is pinned at 2.38.2; re-audit the server and the Android client before upgrading.

## 7.5 Phone continuation relay

Status: the Rust daemon is the live path in both directions since 0.5.0. Inbound text and attachments, outbound turn text, auto-attach, `ntfy_send_file`, the phone permission commands, and question forms are verified live. Round 2 hardening is implemented: atomic lock, compact state with quarantine, a separate replay cursor, publish timeouts, a transport rule that uses HTTPS except on a loopback host, case-insensitive commands, and a 150-topic subscription cap. Command and control is table-driven with a capability gate and a 60 s confirm hold. The Rust port in `rust\crates\inbound`, `rust\crates\outbound`, and `rust\crates\app` is the only implementation; the Node relay was deleted in 0.8.0. Section 7.9 covers the build, smoke, and cutover.

The topic is the project canonical path's folder name, unchanged: no prefix and no slug. Names outside `[-_A-Za-z0-9]{1,64}` are skipped with a warning. The daemon holds one SSE subscription for all project topics, ignores messages tagged `oc-out`, and routes accepted text into an existing session; free text never creates one. A JSON command table at `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-control.json` supplies the control verbs (`new`, `sessions`, `to`, `auto`, plus `help` and `status`), with built-in defaults when the file is missing; `/steer`, `/allow`, `/always`, and `/deny` stay native. Commands are matched case-insensitively; any other text starting with `/`, such as `/usr/bin/env is missing`, is delivered as a prompt. The subscription covers at most 150 topics; above that it keeps the 150 with the newest activity and logs the rest. Inbound attachments are saved to `.ntfy-inbox` under the project; the relay prunes that directory at startup and every 6 hours, deleting only `<epoch>-<name>` regular files older than 7 days. The Rust inbound path adds a 200 MB per-project byte budget: the oldest matching files are pruned first, and a message that still does not fit gets one error push and no prompt. It limits prompts to 30 per minute per topic with a burst of 10; `/steer` counts, and a burst above 10 gets one `rate limited` error push and no prompt. It also requires attachment URLs to use the `ntfy.server` host or the deployment `baseUrl` host on every hop. Acks and error pushes pass through the same link filter, so a session title or a `/to` argument that contains a file name cannot become a link.

Routing needs no command. Every text resolves against a live snapshot of the project's sessions. A one-shot target set by `/new` or `/to` wins, then the session that accepted the previous prompt inside 10 minutes, then the session with the newest activity where activity is the later of `time.viewed` (desktop use) and `time.updated` (any message). A running session breaks an activity tie, then the previous target, then the id. Child sessions are never targets. A stored hint outside its window is ignored, so no decision outlives the evidence and a stale target cannot capture traffic. `/sessions` and `/to` list numbers and titles; no command takes a session id. Permissions resolve the ask that was pushed, never the target. Free text never creates a session: start one on the host, or send `/new` to create one and send the next message to it.

Local configuration, not committed, in the relay root `%LOCALAPPDATA%\ntfy-relay\`:

- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.json`: server, token file, `projectRoots`, `adapter` block (`agent` and `model` for created sessions), state and log paths. `opencode.command` is accepted and ignored. `projectRoots` holds any number of roots since 0.3.0; an empty list serves no topics. Each discovered project folder name is a topic, and discovery keeps the newest 150 topics. Two roots whose folders share a name collide on one topic: the more recently active project wins and the relay logs a duplicate-topic warning.
- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-control.json` (optional): command table. Built-in `new`, `sessions`, `to`, `auto`, `help`, and `status` when absent; invalid content stops the relay at startup.
- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-state.json`: routing hints (`last`, `pending`) and processed message ids, compact JSON. A file that is unreadable, oversized, not UTF-8, invalid JSON, or another version is renamed to `.corrupt-<ms>`, backups older than 7 days are removed at startup, and the relay starts empty.
- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-asks.json`: published permission asks, compact JSON, mode 0600. Each record holds the code from the push, the request id, the session id, the session title, and the push time. Written before the push, removed on a reply or a desktop answer, capped at 512 records and evicted oldest first. A file that is unreadable, larger than 1 MiB, not UTF-8, invalid JSON, or another version is quarantined to `.corrupt-<ms>` and the relay starts with an empty registry; startup never blocks on it.
- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.log`: log, rotates at 5 MB, default level `info`, prompt text never written. The heartbeat line carries topic count, last event age, and queue depth.
- `%LOCALAPPDATA%\ntfy-relay\ntfy-relay-state.lock`: single-instance lock, created atomically with `open('wx')`, carrying the pid, start time, start phase, and image path.

Round 2 behavior: an unreadable lock fails closed instead of being deleted; the replay cursor advances only after a message is handled, so a queue drop never skips messages; publish calls abort after 10 seconds; an attachment URL must name the `ntfy.server` host or the deployment host, HTTPS on every hop except a loopback host, with at most 3 followed redirects; acks are capped at 3400 bytes; at shutdown the relay aborts the SSE stream, waits up to 5 s for each task (a 6 s bound), logs the dropped count, then releases the lock.

Run it detached through `ensure`. The command is idempotent: with a live lock it prints `running pid=<n>` once the relay is subscribed and `starting pid=<n>` while it waits for its token, and it exits without spawning a second process either way. A config with `enabled: false` stops a live relay and prints `disabled`. Otherwise it starts the binary hidden, waits up to 60 seconds for the lock, and reports the new pid. The root is `NTFY_RELAY_HOME`, else the binary's directory; the state, log, asks, and journal paths come from the relay config.

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" ensure
Get-Content "$env:NTFY_RELAY_HOME\logs\relay-ensure.log" -Tail 3
```

Run it in the foreground for debugging:

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-relay.exe" run
```

The Rust suite is the acceptance gate: `cargo test --workspace` under `rust\`.

Verified 2026-11-08 on the Node relay, deleted in 0.8.0: 40 tests passed on Windows and WSL. A text message published to `ntfy-proj` with the phone token produced exactly one prompt in the existing running session, no new session, and one `oc-out` ack. A relay restart filtered the replayed message as a duplicate. A PNG attachment was saved to `.ntfy-inbox` and prompted successfully; an absolute Windows path in `files[].uri` was rejected with HTTP 400, while `file:///C:/...` was accepted, so the relay always sends `file://` URIs. The suite reached 94 cases on 2026-11-10, in the Node era, before the Rust suites replaced it.

Verified 2026-11-09, before 0.11.0, against real pending permission requests in a scratch session: `/to` pinned the session and wrote the state pin; `/allow` approved once and the `del` command ran; `/always` approved the session-wide pattern and the command ran; `/deny` rejected the request and the tool returned a declined error. Each command produced exactly one ack on `ntfy-proj`. `/always` writes a saved approval row to the local permission table for the project, so the live test rows were removed afterwards.

Verified 2026-11-09 for phase 4: with the relay already running, the wrapper reported `PASS relay-running` and the node process count stayed at one. After a forced stop, the wrapper started a new process, the lock pid matched, the log reached `subscribed`, and the startup check returned all PASS including `relay-process` and `relay-heartbeat`. The first real reboot exposed a hang: the wrapper used `Start-Process`, node inherited the check's stdout pipe, and the check stopped after `PASS podman-binary` with the Podman machine still down. The wrapper then spawned through WMI and the check invoked it with `Start-Process -Wait` (commit `e3f2b44`). Cold start re-verified with the relay and the Podman machine stopped: all PASS, container healthy, HTTPS 200, relay `subscribed`, one node process. The phone then sent `reboot-ok`: the relay accepted it, published a `queued` ack, prompted the newest session, and the turn push `reboot-ok` arrived with tag `oc-out`. The wrapper and the check were removed in 0.6.0.

Modern standby verified 2026-11-09: the host entered S0 Low Power Idle at 12:36:18 and resumed at 12:37:27, 69 seconds later, exit reason keyboard, no login. Connectivity in standby is disconnected by policy, yet the frozen SSE stream resumed without a reconnect line; the 45-second keepalive ticks show a 113-second gap. The phone message `wake-ok` was accepted 27 seconds after the resume and routed to the same session. While the host is in standby the network is off, so a phone publish fails and nothing is stored; resend after wake. A message accepted just before the standby transition is recovered by the relay replay window.

## 7.6 Message flow and inbound surface

Both directions are outbound from the host. Nothing on the host accepts notification traffic, and there is no inbound firewall rule or port forward for it.

Host to phone: the Rust outbound task POSTs to `https://ntfy.example.ts.net/<topic>` with the owner token. The server stores the message and pushes it to the phone over the phone's own outbound WebSocket connection.

Phone to host: the phone POSTs to the same server over its own outbound HTTPS. The relay holds one long-lived HTTPS GET built from the topic set (`sse.rs` in Rust; `buildSseUrl` was the deleted Node relay's builder): `https://ntfy.example.ts.net/<topics>/sse?since=<last seen minus 60>`, with comma-separated topics and `since=now minus replay window` on the first connection. It reads the streaming response, and the server writes each new message down that already-open stream. Keepalive frames hold the connection open; a dropped stream reconnects with 1 to 60 second backoff, resets the backoff only after a connection stayed up 60 seconds, times out response headers after 30 seconds, closes after 90 seconds with no bytes, and replays from `since`.

Consequences:

- The server never connects to the host. No inbound rule, no port forward, no LAN exposure, and the host can sit behind NAT.
- The only host listeners are the TLS terminator on 443 of the private interface and the loopback portproxy to `[::1]:8810`. Neither accepts ntfy traffic on the home LAN.
- `192.168.2.9` answers nothing on the home LAN: the Wi-Fi profile is Public, inbound ICMP echo is disabled, and no LAN-facing TCP port is open. A LAN link or status probe therefore reports the host offline even while ntfy works, so ignore that indicator in third-party apps such as a WoL sender.
- Wake-on-LAN is the one path that is not client-initiated. The magic packet is L2 and must reach the NIC from the home LAN; the private network is L3, so the phone cannot deliver it over the VPN.

## 7.7 Sleep and wake

Long sleep verified 2026-11-09: the host entered S0 Low Power Idle at 12:55:15 and resumed at 13:22:06, 26 min 51 s later, connectivity disconnected by policy, login required. The relay process and the OpenCode session survived; the frozen SSE resumed without a reconnect line, the overdue heartbeat fired at resume, and the phone message `Waketest` was accepted 34 seconds after resume and routed to the same session. The 5-minute heartbeat and the 10-minute turn buffer expired during sleep; the 60-minute replay window and the 72-hour cache did not. A 69-second cycle behaved the same. If a longer sleep drops the socket instead, the relay retries with backoff up to 60 seconds and logs `subscribed`, and a message accepted just before sleep is recovered by the replay window. While the host sleeps the network is off, so a phone publish fails and is not stored; resend after wake.

Remote wake stays a future goal: local input and wake timers exist, and the Intel Wi-Fi 7 NIC is wake-armed for a LAN magic packet, but WoL is L2 and the private network is L3, so the phone cannot wake the host. No inbound rule is allowed.

## 7.8 Remote wake: parked

Wake-on-LAN over Wi-Fi does not wake this laptop. Tested 2026-11-09 with BIOS 330 and Intel Wi-Fi 7 BE201 driver 24.50.0.4: the sender's 102-byte magic packet was captured at the NIC, the adapter stayed associated through modern standby with no WLAN-AutoConfig disconnect, and Windows reports the NIC as wake_armed with magic packet and pattern match enabled. Every standby session still exited on local input, and a TCP SYN pattern test changed nothing. Hibernate was not testable because the virtualization driver behind the Podman/WSL VM blocks the S4 transition, and this model's BIOS exposes no wake control. Enabling networking in standby was required for the test; an OEM power policy blocks it by default.

Conclusion: remote wake is not available on this hardware under modern standby. Revisit on the Linux hosts with desktop motherboards, where Ethernet WoL from S4/S5 is the reliable path; test Wi-Fi wake only where the card and access point both support it. The phone side needs no change: the verified sender already emits a standard magic packet, and its status probe cannot work here because the host exposes nothing on the LAN ([section 7.6](#76-message-flow-and-inbound-surface)).

## 7.9 Rust daemon: build, smoke, cutover

The live path is the Rust daemon since 2026-11-11 (0.5.0; supervision in 0.6.0; reconnect backfill in 0.7.4). `rust\crates\app` builds `ntfy-relay run|mcp|ensure|status` with the same filter, routing, control, permission, attachment, and retention behavior, plus the outbound event subscription, the task supervisor, and the MCP stdio server. The Windows release binary is cross-built at `%LOCALAPPDATA%\ntfy-relay\ntfy-relay.exe` and started hidden through `ntfy-relay ensure`. Service discovery reads `~/.local/state/opencode/service.json` and uses its URL, pid, and password; `opencode.baseUrl` remains an optional override. A missing or dead registration starts the relay degraded on a placeholder client, and the reload picks the registration up when OpenCode appears. After a transport error or HTTP 401 the client re-reads the registration and swaps only when the URL or password changed.

Build and test in WSL2 Ubuntu with a WSL-native target directory:

```bash
cd /path/to/ntfy-proj/rust
export CARGO_TARGET_DIR=$HOME/target/ntfy-proj
~/.cargo/bin/cargo test --workspace
~/.cargo/bin/cargo build -p ntfy-relay-app
```

That binary is a Linux ELF. A Windows cutover needs a Windows build: `cargo build -p ntfy-relay-app` produces `rust\target\debug\ntfy-relay.exe`. A release build for all four tools is `cargo build -p ntfy-relay-app -p ntfy-relay-deploy -p ntfy-relay-auth -p ntfy-relay-admin --release --target x86_64-pc-windows-gnu`. The gnu target needs `x86_64-w64-mingw32-windres`; an MSVC build needs `rc.exe`, and its absence fails the build instead of producing a binary without the icon or version block. CI compiles and tests the workspace on `windows-latest`.

The daemon resolves its config in this order: `NTFY_RELAY_CONFIG`, else `<install root>/ntfy-relay.json`, where the root is `NTFY_RELAY_HOME` or the directory that holds the running binary. It discovers the shared service from `~/.local/state/opencode/service.json`; `opencode.baseUrl` overrides the discovered URL and then uses the password from `~/.config/opencode/service.json`. It also needs the token file, and `adapter.agent` plus `adapter.model` for `/new`. `opencode.command` is accepted and ignored: the Rust client talks HTTP.

Smoke with a temporary state file and log, so live state is untouched. Copy the live config into `temp\rust-smoke\`, set absolute `stateFile` and `logging.file` under that folder, set `logging.echo` to true, and set `NTFY_RELAY_CONFIG` to the copy. No `opencode.baseUrl` is needed while the shared service registration is current. Set `ntfy.tokenFile` absolute as well when the home directory differs from the live one: `~` is expanded before the daemon reads the token.

Run the build for the target OS. The WSL binary is a Linux process: its `~` is the Linux home, and `127.0.0.1` points into the WSL network namespace, not the Windows host unless WSL networking is mirrored. Use a native Windows build for a smoke against the Windows server, or a `baseUrl` that the Linux side can reach. The shared registration is read from `<home>/.local/state/opencode/service.json`, so a WSL run needs `HOME` pointed at the mounted Windows profile or a copy of that file in the Linux home. The explicit override password comes from `<home>/.config/opencode/service.json`.

Stop the relay before the smoke: both processes read the same lock path when they share a state file, and the second process exits on a live lock. There is no stop script; a hard stop leaves a stale lock, and the next start replaces a readable dead pid once. Never delete a lock held by a live pid.

Smoke checks:

- the log lists `topics now` with the project and then `subscribed`;
- `/help` returns one ack;
- one text prompt reaches a session;
- one attachment lands in `.ntfy-inbox`;
- on Linux, Ctrl+C finishes the current message, drops queued messages with one log line, joins the outbound and SSE tasks with a 5 s grace and a 6 s bound per task, and removes the lock; the Windows build is windowless and is stopped by pid, which leaves the lock for the next start to replace;
- `ntfy-relay status` prints the config, lock pid and liveness, state counts, and the last log line, then the read-only platform rows of the startup check (podman, container, Tailscale Serve, portproxy, legacy files, health, elevated, ACL). It starts nothing, a FAIL line does not change the exit code, and it does not prove the subscription.

Cutover (done 2026-11-11): `ntfy-relay.exe` moved to the install root, then `bin\ntfy-relay\`; it now lives at `%LOCALAPPDATA%\ntfy-relay\`, and `ensure` starts it. The outbound cutover followed on the same day: set `outbound.enabled: true`, remove plugin 0.2.1 from `opencode.jsonc`, register `mcp.servers.ntfy` with `codemode: false`, and restart the relay through `ensure`. Rollback: run a previous `ntfy-relay.exe` build from the same folder. The Node relay was deleted in 0.8.0, so there is no implementation switch; `relay.implementation` in an old config is ignored. Do not restore the deleted publisher plugin. Do not run two relays against the same state file: they would double-handle the same phone message. Do not run both publishers: every push would double.

Check the routing hint after a cutover or a long gap: `status` prints `targets` and `pending` counts, and `/status` on the phone prints the target. There is no pin to clear. A one-shot target from `/new` or `/to` expires after 10 minutes or one message; `/auto` drops it early. Routing then follows the newest desktop activity; type one message in the intended chat, because merely opening a chat does not update `time.viewed`.
