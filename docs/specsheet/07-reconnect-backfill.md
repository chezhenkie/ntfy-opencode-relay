# 07 - reconnect backfill

Status: implemented in 0.7.4. The project-local JS forwarder and the loopback ingest are gone; completed assistant text is recovered from the message API. The decision row is 12 in `04-rust-relay-decisions.md`. Gate evidence is recorded under Live gate and in `docs/operations.md` 7.4.

## Goal

The Rust daemon owns outbound turn text with no JavaScript and no second event feed:

- `/api/event` remains the primary live source.
- After a stream reconnect or a daemon start, the daemon reads completed assistant messages from the session message API and publishes any text it missed.
- The forwarder plugin, the loopback ingest, the registration file, and the ingest bearer token are deleted once the gate passes.

## Evidence

The live OpenCode 2.2.7 server exposes, in `/openapi.json`:

```
GET /api/session/{sessionID}/message
  ?type=assistant&order=desc&limit=<n>&cursor=<opaque>
```

The response is `{ data: Session.Message.Info[], cursor: { previous, next } }`. Assistant messages carry:

- `id`: the assistant message id.
- `time.created`, `time.streamed`, `time.completed` in epoch milliseconds.
- `content[]` with `type` = `text`, `reasoning`, or `tool`; text items carry `text`.

Verified on the pinned session: `limit=3&order=desc&type=assistant` returned complete assistant messages with ordered text content. `session.text.ended` already carries `assistantMessageID`, so live parts and backfilled messages share one dedupe key.

Residual: `permission.asked` and `form.created` are transient and have no history endpoint. A gap can still hide an ask; text is the recoverable payload.

## Design

### Gap detection

- The outbound runner records `last_event_ms` from every accepted envelope.
- On stream end, it records `gap_start_ms` as the current wall time. On reconnect or start, it backfills only messages with `time.completed >= backfill_from`.
- `backfill_from` is `gap_start_ms - 60 s` for a reconnect. On a cold start it is `now - 15 min`, matching the seen journal window.
- The backfill runs in the worker after the subscription is established, so live frames arriving during the queries wait in the 256-slot channel, dedupe normally, and can be dropped under a long backfill.

### Session scope

- Reconnect: backfill sessions with an open turn and the session that published the last text.
- Cold start: backfill the newest 20 non-archived sessions per project root, using the existing discovery list. The sessions the state file remembers are always included.
- Cap: 200 messages per reconnect; stop paginating when a page is older than `backfill_from` or the cap is hit.
- Endpoint calls reuse the 5 s API timeout and the rediscovering client.

### Text reconstruction

- Keep only `content[].type == "text"`, in array order.
- Join parts with two newlines, apply `core::link_safe`, and reuse the turn publisher: inline up to 3400 UTF-8 bytes, otherwise the 500-character summary plus a `turn-<session8>-<epoch>.md` attachment.
- One push per completed assistant message with non-empty text. This can produce more pushes than a live multi-message turn; acceptable for a recovery path.
- Require `time.completed`; skip streamed or partial messages.

### Dedupe

- Extend the seen journal with assistant message ids. Event ids and message ids already use distinct prefixes (`evt_`, `msg_`), so one 8192-id window covers both.
- On `session.text.ended`, mark the `assistantMessageID` seen as well as the event id.
- Backfill skips any message id already seen, marks an id before publishing, and commits after the effect, matching the current at-most-once rule.
- A message published by backfill must not be republished by a late live event, and the reverse.

### Failure handling

- A failed backfill query logs one line and leaves the live stream running; the next reconnect retries.
- A failed publish leaves one log line and commits the message ids to the seen journal anyway; there is no error push and no outbox, so that text is not retried.
- A gap with no recoverable text logs one line with the gap duration and the session count.

## Implementation steps

1. `rust/crates/opencode`: add `list_messages(session_id, type, order, limit, cursor)` and the response types (message, content part, cursor). Test against a recorded fixture.
2. `rust/crates/outbound/seen.rs`: accept and persist message ids in the same journal; add the message-id accessor used by both feeds.
3. `rust/crates/outbound/turn.rs`: expose text-part reconstruction and the existing publish path for a recovered message.
4. `rust/crates/outbound/runner.rs`: add gap tracking, the backfill task, session scope, cap, and the seen handoff. Keep it inside the reader/worker ownership model; no new process.
5. `rust/crates/outbound/ingest.rs`: delete after the gate; first leave it compiled but unused, then remove the registration and config references in the same commit as the plugin deletion.
6. Delete `.opencode\plugins\ntfy-forwarder`, its test, the loopback registration writer, and every ingest token doc. Remove `ntfy-relay-forward.json` from the state directory at upgrade.
7. Docs: update operations 7.2 and spec 03 section 8; delete the forwarder from the tree, README, handoff, and crate split.
8. Changelog 0.7.4: Rust-only event recovery; forwarder and ingest removed.

## Tests

Unit and integration:

- gap detection uses the last event time, not the reconnect time.
- message pagination follows `cursor.next` and stops at `backfill_from`, the cap, or the last page.
- text reconstruction filters reasoning and tool parts and keeps order.
- inline and attachment thresholds reuse the turn path.
- a live part and the same message from backfill publish once.
- backfill then live, and live then backfill, both dedupe.
- marking before publish and a failed publish produce one error, not a retry loop.
- a backfill API failure logs once and does not stop the stream.
- cold start limits to the journal window and the top 20 sessions.
- an assistant message without `time.completed` is skipped.

Live gate:

- Move `.opencode\plugins\ntfy-forwarder` out, run `opencode reload`, confirm no registration file.
- Kill the relay mid-turn for 30 s; on restart, all parts arrive once.
- Repeat with a 17 s gap; the old failure case now delivers parts 1 to 3.
- Restart the daemon after a completed turn; no duplicate push.
- Confirm `~/.local/state/opencode/ntfy-relay-forward.json` is absent and no process listens on the ingest path.
- Run `cargo test --workspace`, `npm run tlds`, `npm run links`.

Result 2026-11-12: the plugin was moved out and `opencode reload` ran; the relay was stopped for 41 s while a gate turn completed. The start logged `backfill cold=true sessions=20 messages=111 published=1`; the phone received `GATE-A`, `GATE-B`, `GATE-C` once. A second start logged `published=0`. A 17 s reconnect inside one process is covered by the runner tests (`a_reconnect_backfills_missed_messages_in_order_once`, `a_published_turn_blocks_a_later_backfill`, `buffered_parts_are_dropped_after_backfill_delivers_the_message`); it was not forced live because it needs a dropped `/api/event` stream. The registration file and `.opencode\` are gone.

## Acceptance

- Turn text survives a stream gap and a daemon restart with no JS plugin loaded.
- `.opencode\` is deleted from the repository.
- No loopback ingest, registration file, or ingest token remains.
- Live and backfilled text never double-publish.
- Permission and question gaps are logged as uncovered, not silently ignored.

Acceptance, 2026-11-12: all items hold. `cargo test --workspace` 295 on Linux and 296 on Windows, `npm test` 94, `npm run tlds` 1285, `npm run links` over every tracked markdown file.
