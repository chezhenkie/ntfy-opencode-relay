# 09-permission-reply-routing.md - phone permission replies bound to the ask

## Header

- Scope: how `/allow`, `/always`, and `/deny` from the phone select the OpenCode session and permission request they answer.
- Status: implemented and live-verified as 0.9.0 on 2026-11-13. The local audit holds the matrix; cases 1 to 6 pass, the UI-driven ask case stays open.
- Related: `01-phone-continuation.md` (permission contract), `08-comms-and-mcp.md` (surfaces), `../operations.md` (notification behavior), `../phone-app-user-manual.md` (phone commands), `../open-work.md` item 10.2.

## 1. Problem

A phone reply is routed by ambient state, not by the notification the user saw. The push names the session, and the relay holds the request id; the reply path ignores both and re-resolves a session from the pin, then asks only that session for pending permissions. Any ask in another session is unreachable from the phone, and the error names the pinned session as if no ask existed.

Live evidence, 2026-11-13:

- Two API sessions raised `permission.asked` for `net user`; the relay published both pushes 15 s later (`permission requested`, tags `lock,oc-out`).
- The pin pointed at an older session, `Handoff status check`.
- A phone `/allow` was accepted by the server and answered with `no pending permission in Handoff status check`.
- Both asks stayed pending. Replying would have required typing `ses_e2b1370e...` after `/to`, which is unacceptable on a phone.

The same failure occurs with one session when the pin is stale, and with any ask raised by an API session, a subagent, or a background run while the pinned session is idle.

## 2. Root cause

`reply_to_permission` in `rust/crates/inbound/src/permission.rs`:

1. calls `resolve_session(adapter, entry, pin, prefer_running)`,
2. lists pending permissions only for that resolved session,
3. selects its first request,
4. posts the decision there.

The function never uses the ask that the outbound scheduler published. The registry of what the phone was told does not exist. The pin is a prompt-routing concept that leaks into permission replies, so the reply target depends on state the phone user cannot see and does not control.

`permission.replied` removes the pending timer, but there is no record of a published ask, so a desktop answer cannot invalidate a phone target.

## 3. Decision

A permission request becomes a first-class object from the moment its push is published. The session and request id are bound at publish time and stored with a short code. A phone reply resolves that record. No part of the permission reply path reads the routing hint, the routing preference, the active map, or session recency.

The routing hint keeps its meaning for text prompts only. Since 0.11.0 it is a one-shot pending target plus a 10-minute continuity hint, not a pin; the reply path still ignores it. Reaching the right session is achieved by not guessing: the ask carries the session it belongs to.

## 4. Requirements

- R1 Bind at publish: an ask enters the registry immediately before its push is published, with session id, request id, title, and push time.
- R2 Reply by record: `/allow`, `/always`, `/deny` resolve a registry record, never a routed session.
- R3 Default target: with no argument, the most recently pushed record. With an argument, the record whose short code matches.
- R4 Short codes: 4 hex characters, extended to 5 or 6 only on collision; displayed in the push; accepted with or without `#`.
- R5 Ack from the record: the ack names the record's session title and counts remaining records across the registry.
- R6 Desktop answer: `permission.replied` removes the record, so a later phone reply cannot target a dead ask.
- R7 Restart: the registry survives a relay restart; entries are optimistic and cleaned when the ask is gone.
- R8 Gone ask: a reply that the server reports as already answered or missing removes the record and acks `already answered in <title>`.
- R9 Cap: 512 records, oldest pushed evicted with one log line. An evicted ask remains answerable on the desktop only.
- R10 Corruption: an invalid or unknown-version file is quarantined and the relay starts with an empty registry; startup is never blocked.
- R11 Durability: the record is persisted before the push; a failed persist suppresses the push and retries on the next tick. A failed publish removes the record.
- R12 No new config key and no change to the 15 s grace, the 512 pending timer cap, or the `/always` approval semantics.
- R13 Observability: one log line per register, resolve, remove, and evict; `/status` reports `asks <n>`.
- R14 The phone never needs a session id to answer a permission ask.

## 5. Registry

Fields per record: `code`, `request` (the permission id, `per_...`), `session` (`ses_...`), `title` (session title at push time), `pushed` (epoch ms).

Ordering: `pushed` ascending, ties broken by `request`. The default target is the last record.

Code allocation: sha256 of `request`, first 4 lowercase hex characters; if the code already exists in the registry, extend to 5, then 6, then 7; a failure to find a unique code is an error that skips the push and logs. The same request keeps one code while registered.

File: `bin\ntfy-relay\ntfy-relay-asks.json`, compact JSON:

```json
{"version":1,"asks":[{"code":"3f2a","request":"per_1d4e...","session":"ses_e2b1...","title":"phone round","pushed":1794573661000}]}
```

Write path: serialize, write a temporary file in the same directory, `sync_all`, rename, fsync the parent on Unix, mode 0600. Read path: missing file is an empty registry; invalid JSON or a version other than 1 moves the file to `<name>.corrupt-<unix>` and starts empty.

The registry stores no tokens, command text, or file paths. Titles are the same text already published to the phone.

## 6. Reply resolution

Grammar: the existing native verbs plus one optional token, case-insensitive: `/allow`, `/allow 3f2a`, `/allow #3f2a`; same for `always` and `deny`.

1. If a token is present, find the exact code; an unknown code is an error listing up to three current codes and titles.
2. Without a token, take the newest record.
3. Post the decision to the record's session and request id.
4. Remove the record on success.
5. Ack: `approved once in <title> (<code>), N more pending`, `approved always ...`, or `rejected ...`; `N` counts all remaining records.
6. Empty registry: `no pending permission ask`. The message must not name a session.

Server outcomes on reply:

| outcome | action |
|---|---|
| 2xx | remove record, ack decision |
| 404 or 409, or a 400 whose body says already answered/replied | remove record, ack `already answered in <title> (<code>)` |
| other 4xx | keep record, error push with the server text clipped to 200 bytes |
| 5xx or transport error | keep record, error push; the user can retry |

A reply after a desktop answer therefore removes the record and acks `already answered in <title> (<code>)`, rather than naming another session as having no pending request.

## 7. Publish path

The outbound scheduler keeps its 15 s grace. On a due timer the worker:

1. allocates the code,
2. writes the registry entry and persists it,
3. publishes the push,
4. on a failed publish, removes the entry and logs `permission push failed`.

The push body adds one line: `Reply /allow <code>, /always <code>, or /deny <code>; with one ask, /allow is enough.` The code appears in the body only; the title stays `<project> - permission requested`, so notification actions and previews stay unchanged.

A persist failure leaves the scheduler entry pending; the next tick retries the same code. After three consecutive failures the entry is dropped with one error log line; the ask remains answerable on the desktop. This bounds a broken state directory without silently spinning.

## 8. Desktop and event interaction

- `permission.replied` removes the record by request id. This covers an answer given on the desktop after the push, and a rejection that races the push.
- The existing rule that a reply before the ask suppresses the push stays: the scheduler never publishes, so no record exists.
- Duplicate `permission.asked` events stay suppressed by the scheduler's known list; no duplicate record.
- Two asks in one session and two asks in different sessions are both first-class; the newest default and the codes disambiguate.
- `/to` and `/auto` do not touch the registry.
- A removed record is remembered in memory for 15 minutes, at most 32 entries. A later phone reply for that request or code acks `already answered in <title> (<code>)`. Tombstones are not persisted; after a restart the server outcome still maps a dead ask to already answered.

## 9. Restart and reconciliation

- On start, load the file. Do not scan sessions; startup latency and API load stay unchanged.
- Records are verified lazily on reply.
- A relay restart while an ask is pending keeps the notification answerable, because the request id and session id are stable in the OpenCode server.
- An OpenCode restart invalidates the ids; the first reply removes the record and acks already answered.
- The registry is per relay process. Since 0.14.0 the relay serves every root in `projectRoots`, so one registry covers several topics; a reply resolves by request id, never by topic.

## 10. Status and logs

- `/status` adds `asks <n>` after the `target` line. Additive; no existing field changes.
- Log lines, one each, no full command text:
  - `permission ask code=<code> session=<id> request=<id>`
  - `permission reply code=<code> decision=<once|always|reject>`
  - `permission ask removed code=<code> reason=<answered|gone|evicted|publish-failed>`

## 11. Tests

Unit, fake adapter and fake permissions API:

- Registry: code allocation, collision extension, newest default, exact code, cap eviction, persist/load round trip, corrupt quarantine.
- Inbound: pinned session has no ask while another session does, reply resolves the other (the regression for this defect); two sessions two asks, default resolves the newest, code resolves the older; unknown code lists current codes; empty registry error has no session name; already-answered mapping; transient 5xx keeps the record.
- Outbound: entry persisted before publish; persist failure suppresses the publish and retries; publish failure removes the entry; `permission.replied` removes the entry.
- App: `/status` includes `asks`; no permission path calls `resolve_session`.

Live acceptance, after the 0.9.0 swap:

1. Pin the topic to a session with no pending ask.
2. Raise `net user` in a different session.
3. Phone: one push with a code; `/allow` resolves it; ack names that session, not the pin.
4. Two asks in two sessions; `/allow` resolves the newest; `/allow <code>` resolves the older; the ack remainder counts both.
5. Answer one ask on the desktop after its push; a phone reply acks already answered and removes it.
6. Restart the relay with a pending ask; the phone reply still resolves it.
7. Send `/allow` with no asks; the error is global, not tied to a session.

## 12. Migration

- Version 0.9.0: daemon behavior and phone contract change.
- No config change. The registry file is optional; a previous binary ignores it.
- Docs to update in the same change: this file, `../phone-app-user-manual.md` (reply line and code), `../operations.md` (permission routing, the pin is prompt-only), `../tree.md` (new spec file), `../open-work.md` 10.2 (unblocked only after live acceptance), `CHANGELOG.md`.
- The 10.2 audit records the defect, the fix version, and the completed live matrix.

## 13. Out of scope

- Answering question forms from the phone; they stay desktop-only.
- ntfy notification action buttons; the command message stays the transport.
- Sharing approvals across topics; the registry is per relay.
- A generic job or command router; `/allow` stays a native verb, not a table action.
