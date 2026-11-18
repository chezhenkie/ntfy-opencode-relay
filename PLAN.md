# Plan: one credential, remote subscribe

Status: binding. Scope: ntfy auth model, phone subscriptions, relay token path, docs. Implementation starts at Gate 0.

## Outcome

A new subscription is a phone-only action: `+`, type the name, done. No host command, no login prompt, no token paste. One credential exists, in one file, and the app already holds its copy.

## Why it fails today

- `docs/operations.md:142` states the rule: no wildcards, every project topic needs an explicit `ntfy access` grant for both users. A topic that was never granted returns 403, so the app can subscribe but receives nothing.
- The human has a token (app header) plus a password (web UI) that lives only as a hash in `auth.db`. The password is why the login prompt cannot be answered.
- The relay caches the token at startup (`runner.rs:138-154`), so it is bound to the `system` user and a second file. Two users, two tokens, three files.
- Audit H-01: discovery can add a topic the ACL does not cover, and ntfy can reject the whole SSE subscription. Per-topic grants are already a latent failure.

## Decisions (binding)

1. One principal: `phone`, role user, one ACL row `*` read-write. `system` and `admin` are removed. Anonymous stays deny-all, signup stays off.
2. One long-lived token. One host file, `bin\ntfy-relay\ntfy-token.txt`, read by the relay and the MCP server. The app keeps its copy. No second token file, no password file.
3. No passwords in the workflow. The user gets a random password at creation that is never stored or used. The web UI is out of scope and the docs say so; recovery is a documented `change-pass` if it is ever wanted.
4. Auth state is owned by the container. It is changed only by the server's own CLI executed inside the running container, never by editing `auth.db` and never by a second container on that volume. A new internal crate `ntfy-auth` (package `ntfy-relay-auth`) is the only bootstrap writer: `apply` and `status`, idempotent, `--dry-run`, tokens written 0600, no secret printed. `tools\rotate-token.ps1` keeps rotation only and drops `system`.
5. The relay stays a host process. It spawns the host OpenCode process and reads host project roots; containerizing it is a separate decision and not part of this change.
6. No `/subscribe` command and no relay-mediated ACL changes. A relay that grants access would need an admin credential and a privileged write path from chat, which is exactly the hack to avoid.

Wildcards are the native mechanism: ntfy 2.37.2 accepts topic patterns (`ntfy access --help`). One row removes the host step, the 403, and H-01 at once. The alternative, read `*` plus per-topic write rows, was rejected because it keeps per-project host grants and splits the model for no real gain: the token holder can already prompt the agent on its own project topic.

## Work

**Gate 0.** Prove the mechanism in a throwaway container with its own volume and port: user, `*` row, publish and subscribe on an arbitrary topic, anonymous 403. No live state touched.

**Phase A, live migration**, in this order, each step verified before the next:

1. `ntfy access phone "*" rw`. The existing rows stay for now.
2. Point `ntfy.tokenFile` at `ntfy-token.txt`; restart the relay; require publish 200 and a clean subscribe in `bin\ntfy-relay\ntfy-relay.log`.
3. Verify the MCP `send_file` path still publishes.
4. Delete `system` and `admin`; verify the phone token still returns 200 and anonymous 403.
5. Reset `phone` to the single `*` row; delete `ntfy-system-token.txt` and `bin\ntfy-relay\ntfy-credentials.txt` (the file added earlier in the session).

The phone token is unchanged, so the app needs no paste.

**Phase B, code.**

- `crates/core/src/config.rs`: default token file becomes `ntfy-token.txt`; update the tests that pin the name.
- New `crates/auth`: config `ntfy-auth.json` in the install root, `apply` and `status`, `CommandRunner` abstraction with fake tests, commands limited to `user list`, `user add`, `access`, `token list`, `token add`.
- `tools/rotate-token.ps1`: `phone` only, one consumer file, no restart path for a missing user.
- Example config beside the existing examples in `relay-windows\`.

**Phase C, docs.** One rule applied everywhere: no per-topic grant, no password, one token.

- `docs/operations.md` 7.2 and the token rotation section.
- `docs/operations.md` manual override at line 98: add `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY`, or replace the command with a pointer to `ntfy-deploy apply`. As written it exits at startup, because the baked `server.yml` sets `attachment-cache-dir` and no `base-url`; proven in Gate 0.
- `docs/phone-app-user-manual.md` section 2: one-time server plus header, then "add topic, pick the server, done"; no login.
- `docs/troubleshooting.md`: remove the web UI password path as primary; `not authorized` means the token was rotated.
- `docs/specsheet/01-phone-continuation.md` sections 4 and 11, including the stale `~/.config/opencode` token path at line 85.
- `docs/specsheet/10-deploy-mode.md` install-root table and the relay contract sentence about the token.
- `README.md` access sentence and quick start; `docs/open-work.md` 10.7; one `CHANGELOG.md` entry.
- `container/server.yml` comment at line 12.

## Addendum 2026-11-18: multi-root

The phone drove `fotonfrontend-proj`, a second project. The relay watched one root only, so the messages were cached but never consumed. `projectRoots` now accepts several roots; discovery and the SSE URL already handled a topic set, so the config gate was the only block. Done: 0.14.0, roots `ntfy-proj` and `fotonfrontend-proj`, 457 tests, live relay pid 18080, three queued prompts delivered in 80 ms and the replies reached the phone. A duplicate folder name keeps the more recently active project with a warning.

## Acceptance

- A topic never granted before, published and polled with the phone token: 200. Anonymous: 403.
- On the phone, away from the host: add a subscription to a fresh name, publish from the host, message arrives. Nothing was run on the host.
- A prompt round from the phone still reaches the session; no 401 or 403 in the relay log.
- `ntfy-auth apply` twice: the second run changes nothing and prints no secret. A wiped token file is restored by one run.
- `cargo test --workspace`, `npm run links`, `npm run secrets` pass.

## Rollback

Re-add `system` and `admin`, restore their rows, point `ntfy.tokenFile` back. The old token files are kept in `temp\` until acceptance passes. The phone token never changes, so the app is untouched through a rollback.

## Limits stated up front

The host must be running for the server to exist. A leaked token can read every topic and prompt the relay; that is already true for the project topic today. The app's no-prompt behaviour for a new subscription is from the implementation, not a device test, so it is verified on the phone before Phase C closes.
