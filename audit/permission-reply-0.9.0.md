# Permission reply routing 0.9.0 - live matrix

Date: 2026-11-13. Source `fa6878f`. Live binary 11,440,740 bytes, sha256 `1774687b3fb7f3b74658b04a5696ca9ece281e4b826277e98174e1d059712cf6`, built from that source. Rollback copy `bin\ntfy-relay\ntfy-relay.080-live.exe` (11,217,442 bytes, `2da18a80...`).

## Gate

- fmt and clippy `-D warnings` clean.
- 387 tests on Linux (352 before): core 56, core asks 11, inbound 69, permission reply 10, outbound 86, app 59.
- TLD 1285, links 29, secrets 0 over 142 tracked files.
- Swap: daemon pid 11216 stopped, binary replaced, `start-relay.ps1` reported pid 8168; `opencode reload`; API MCP check `connected`; `startup-check.ps1` all PASS (relay, heartbeat, container, Serve, portproxy, health 200).

## Matrix

Pin for the whole round: `ses_e30c608b1ffeVPgxMD0586arrN` (`Handoff status check`), no ask in it.

| # | case | observed | result |
|---|---|---|---|
| 1 | ask in another session, phone `/allow` | ask `32f1` registered 13:43:37Z for `ses_e2ada02ecffe...`; reply 13:50:28Z `code=32f1 decision=once`; registry empty; server permission list empty | pass |
| 2 | two asks, newest default, named older | `3a30` older, `0d22` newer; `/allow` replied `0d22` at 13:54:53Z; `/allow #3A30` replied `3a30` at 13:55:42Z; both server lists empty | pass |
| 3 | desktop answer, late phone reply | ask `9b76` registered 14:04:48Z; desktop reject 14:04:56Z, record removed `reason=answered`; phone `/allow 9b76` acked `already answered in ses_e2ac (9b76)` without a server call | pass |
| 4 | restart with a pending ask | ask `04c5` registered 14:02:58Z; daemon restarted 14:03:13Z (pid 19124); registry kept the record; phone `/allow 04c5` replied at 14:04:05Z `code=04c5 decision=once`; server list empty | pass |
| 5 | no asks | first `/allow` at 14:00:22Z hit the live tombstone of `b8f6` and acked `already answered in ses_e2ac (b8f6)`; after a restart cleared tombstones, `/allow` acked `no pending permission ask` with no session name | pass |
| 6 | `#code` and uppercase | case 2 used `/allow #3A30`; code parsed, resolved and acked `approved once in ses_e2ad (3a30)` | pass |
| 7 | UI-driven ask | not run; no event-stream capture, so no claim | open |

Ack text uses the short session id as title (`ses_e2ad`): the `permission.asked` event carries the project location, so the ask binds without a session lookup, and the title falls back to `short_session_id` per plan.

## Deviations

- The `/always` reply for `e805` saved `shell net *` for the project. Later `net user` runs were approved silently, so case 2 used two harmless ask-listed commands instead: `cd C:\Windows` and `Invoke-Expression 'echo gate'`. The saved row `psv_1d52dbc6a002iCEdYoU2d8wan4` was removed from the OpenCode permission table after the round.
- Case 5 first ran inside the 15-minute tombstone window left by the case 3 desktop answer. The tombstone ack is spec 09 section 8 behavior, not the empty-registry path; a restart cleared it.
- Cold restarts backfilled recent turn text (1 push at 14:01, 2 at 14:03) inside the 15-minute window. That is the documented backfill window, unrelated to permission routing.
- One earlier pair of asks used `net user`; the first was answered `/always`, the second ran with no push. Both server lists were empty afterwards.

## Server state after

- Registry `~/.local/state/opencode/ntfy-relay-asks.json`: `{"version":1,"asks":[]}`.
- No pending permission in any session used for the round.
- Daemon pid 19124, `subscribed`; tombstones empty after the last restart.
- `shell net *` approval removed; no `net user` rows remain in the permission table.
