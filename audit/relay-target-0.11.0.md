# Relay target routing, 0.11.0

Date: 2026-11-14. Scope: the phone-to-host target resolver, the state hints, and the control commands that set them. Source: 0.11.0.

## Defect

The relay kept one pin per topic as routing authority. `resolve_session` accepted any pin whose session existed in the project (`inbound\src\routing.rs` before 0.11.0), so age, idleness, and desktop activity were not inputs. The only escapes were `/auto` or a `/to` carrying a session id prefix.

Live evidence, 2026-11-14:

```
state:  pins {"ntfy-proj":"ses_e30c608b1ffeVPgxMD0586arrN"}   session created 2026-11-12
13:03:28Z prompted session=ses_e30c608b1ffeVPgxMD0586arrN
13:05:04Z prompted session=ses_e30c608b1ffeVPgxMD0586arrN
13:05:55Z prompted session=ses_e30c608b1ffeVPgxMD0586arrN
```

The desktop was in `ses_e25e02a09ffeV2LgNMemjV7vJF`, created that day. Three texts went to the old session and nothing appeared in the active chat.

## Design

Routing is derived on every text; no decision is stored as authority.

Snapshot per message: the newest 20 non-archived sessions of the project with `time.updated`, `time.viewed`, `parentID`, plus the active map for the running flag. Child sessions are dropped.

Order:

1. A one-shot pending target from `/new` or `/to`, if under 10 minutes and present in the snapshot.
2. The session that accepted the previous prompt, if under 10 minutes and present.
3. The highest activity, where activity is `max(time.viewed, time.updated)`. Ties break on running, then the previous target, then id.

`time.viewed` is written by the desktop client; API prompts do not touch it. That keeps a stale session from becoming the newest through the relay's own writes.

State: `targets` with `last`, `lastAt`, `pending`, `pendingAt`. Both hints expire. An old `pins` field is ignored and dropped on the first write; no migration step.

Commands: `/to` lists five numbered titles, then takes a number or a title fragment. `/new` targets the created session. `/auto` drops a pending target. No command takes a session id. `/sessions` prints numbers. `/status` prints `target` instead of `pin`. Permission replies still resolve the ask record and never read a hint.

## Source

- `rust\crates\inbound\src\routing.rs`: pure resolver, 14 tests.
- `rust\crates\inbound\src\prompting.rs`: `DeliveryRequest`; resolves through the snapshot then prompts.
- `rust\crates\inbound\src\control\engine.rs`: numbered `/to` menu, one-shot pending write on the wall clock, `target` status.
- `rust\crates\core\src\state.rs`: `TargetState`, capped rows, old `pins` ignored.
- `rust\crates\core\src\adapter.rs`: `SessionSignal`, `TargetSink`; `PinRequest`, `pin_session`, and `get_session` removed.
- `rust\crates\opencode\src\adapter.rs`: `session_signals` carries `viewed` and `parentID`.
- `rust\crates\app\src\relay.rs`, `store.rs`, `runner.rs`, `status.rs`: hints, backfill scope, status output.
- `rust\crates\inbound\src\control\table.rs`: action field `pin` renamed `target`; `/to` ack `next message -> {title}`, `/auto` ack `target cleared`.

## Local gate

- `cargo fmt --all` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace`: 394 passed, 0 failed, Linux.
- `npm run links`: PASS, 32 markdown files, all links resolve, all ASCII.
- `npm run secrets`: PASS, 146 files, 0 hits.

## Live gate

- Binary 11,499,238 bytes, sha `3e41408e2d899f36e0765a9d96c186f56d87b5dc53f73dfc4f888e008f101323`, rollback `bin\ntfy-relay\ntfy-relay.010-live.exe`.
- Pid 19448, lock matches, loopback `/v1/health` 200, `subscribed`.
- MCP `connected` after `opencode reload`.
- State: `pendingAt` and `lastAt` are epoch milliseconds; the old pin is gone.

## Findings from the live run

Three client behaviors were measured, and two defects in the first cut were fixed.

1. Continuity was too strong. A 10-minute window held the previous phone target even after the desktop moved, so a test text at 14:02 went to the old chat. The resolver now uses the previous target only while no other session was active after the previous prompt.
2. The pending timestamp came from the control engine's monotonic clock, which is correct for confirm and cooldown arithmetic but not for hints. `pendingAt` was `4113794`, so the one-shot target never matched wall time. The engine now takes a separate wall clock for hints; a test asserts the value passed to the sink.
3. `time.viewed` is written on input, not on focus. Opening an existing chat does not make it a target; typing one message does. An empty TUI `/new` draft is not a server session at all until its first prompt, so the relay cannot route to it. The phone `/new` path creates the session through the API immediately, so it is not affected.

## Phone matrix

| # | Case | Result |
|---|---|---|
| 1 | plain phone text lands in the chat last used on the desktop | pass, 13:59:33Z, `source=activity` |
| 2 | ack names the target | pass, `queued to Finishing the Handof migration` |
| 3 | desktop uses another chat, next text follows it | pass with one typed message in the target chat; opening alone does not update `viewed` |
| 4 | `/new` from the phone creates the session | pass, `control new ok`, session visible in the API list |
| 5 | the text after `/new` is the first prompt there | pass, 15:26:16Z, `source=pending` |
| 6 | old pin cannot route after the upgrade | pass, `targets=0`, first delivery resolved by activity |

Case 5 initially resolved with `source=activity` because of the monotonic timestamp defect; after the fix the log records `source=pending`.
