# Rust relay docs audit

Status: step 2 (docs cross-check) complete. Round 1 is `audit/audit-rust-relay-code.md`. This report maps its 45 rows and the H-01 cross-check row to the project documentation, verifies the doc claims that name a limit or failure path against the code, and records the doc-only defects. No fix is designed here; round 3 triages.

## Scope

Workspace: the 23 project markdown files: `README.md`, `CHANGELOG.md`, `user-manual-ntfy-android.md`, the 11 files under `docs/`, the 8 files under `specsheet/`, and `ntfy-opencode-incoming-relay/README.md`.

Out of scope: `tools/`, the PowerShell wrappers, `ntfy-server-container/`, git history, and the `.scratch/handoff-20261112/current.md` copy. The R1 report is the finding list, not a project doc.

## Method

- Read all 23 files end to end after R1.
- Map every R1 row to the doc text that states the same behavior, limit, or guarantee.
- Mark a row `documented` when the docs state it and the code matches, `partial` when the docs state part of it, `contradicted` when the docs state a guarantee the code does not keep, and `silent` when no project doc states it.
- Re-read the code at the R1 citations for the partial and contradicted rows that turn on a doc claim. The code line numbers are R1's; only the doc comparison is new.
- Run the checks the docs cite as evidence.
- Use the R1 handoff cross-check only as a seed. Where the full doc text changes the verdict, this report supersedes the cross-check.

## Checks

| Check | Command | Result |
| --- | --- | --- |
| Markdown inventory | recursive `.md` walk | 23 project files; 24 with the R1 audit; 25 with this report; 26 with the `.scratch` handoff copy |
| Links and ASCII | `npm run links` | PASS 24 files before this report, 25 with it; every relative link resolves, all ASCII |
| TLD parity | `npm run tlds` | PASS `tlds=1285`, relay and Rust tables identical |
| Node tests | `npm test` | 94 passed, 0 failed |
| Rust tests | R1 evidence | 295 on Linux, 296 on Windows |
| Toolchain | R1 evidence | cargo 1.99.1, rustc 1.98.0 |
| Commit | `git rev-parse --short HEAD` | `1bb412e` |
| Tree | `git status --porcelain` | `?? audit/` only |

## Verdict

Using the R1 table severities: 2 High, 18 Medium, 25 Low.

| Doc status | Rows | What it means |
| --- | --- | --- |
| documented | 5 | The docs state the behavior and the code matches, or the docs record an accepted design |
| partial | 13 | The docs state a narrower behavior and omit the consequence or the failure path |
| contradicted | 15 | The docs state a guarantee that the code does not keep |
| silent | 12 | No project doc states the behavior |

Only 5 of 45 rows are fully documented: O-03, OB-01, OB-05, A-07, and A-09. Both High rows are outside full coverage: X-01 is partial and I-01 is silent. The same is true of the operator-facing failure rows OB-07, A-03, N-02, OB-04, and H-01.

The docs are strongest on topology, auth provisioning, the happy path, and dated gates. They are weakest on resource limits, failure paths, and the trust boundary. The spec set still carries Node-era text in `specsheet/01-phone-continuation.md`, including one direct contradiction about `/new`.

Doc-only defects: one Medium (D-01) and seven Low (D-02 to D-08).

## Disposition of the R1 rows

| Row | Doc evidence | Verdict | Evaluation |
| --- | --- | --- | --- |
| X-01 H | `README.md:17`, `specsheet/01-phone-continuation.md:10`, `docs/operations.md:133` | partial | The topic rule, `oc-out` filter, duplicate-basename merge, and ACLs are documented. The docs never state that write access to the topic equals the ability to prompt or steer the agent, answer permission asks, and read output; `README.md:17` treats every non-`oc-out` message as a phone message. The High risk is not visible from the docs. |
| X-02 L | `docs/https.md:38`, `README.md:36` | silent | Tailscale Serve and its certificate are the only TLS path described. No doc states that the Rust client uses rustls with public roots and has no custom-CA or native-store option, so a private-CA deployment fails. Out of scope for the current host, but the restriction is unstated. |
| X-03 L | `specsheet/05-js-to-rust-pitfalls.md:164` | contradicted | The doc states the repository rule bans system temp paths and requires workspace test dirs. `rust-src/crates/inbound/tests/control.rs:15-24` uses `std::env::temp_dir()`. The no-op-waker `block_on` loops also have no timeout; a future that suspends would spin. |
| X-04 L | `specsheet/01-phone-continuation.md:409-414` | partial | XDG on every OS and a macOS launchd wrapper are documented, but containment and dedupe case-fold only under `cfg!(windows)`. A macOS case-insensitive volume can fail containment. Current targets are Windows and Ubuntu; the macOS text is ahead of the code. |
| C-01 M | `docs/https.md:23-27`, `docs/operations.md:282` | silent | The deployment is HTTPS-only and the docs say HTTP remains only on loopback. No doc states that `ntfy.server` accepts `http://` and sends the bearer token and prompts in plaintext if configured that way. The transport split in the code is invisible. |
| C-02 L | `specsheet/01-phone-continuation.md:409` | contradicted | `home_dir` tries `USERPROFILE` before `HOME` on every target, while the code comment and the XDG sentence imply platform behavior. On Unix with `USERPROFILE` set, the token, state, log, and control paths diverge from `os.homedir()`. |
| C-03 L | `specsheet/05-js-to-rust-pitfalls.md:93`, `specsheet/04-rust-relay-decisions.md:7-20` | partial | The pitfall doc records the JS leniency (`"enabled": "false"` keeps running) and asks for a decision; `04` has no row. The Rust code reproduces the leniency, so the live behavior is an undecided carry-over. |
| C-04 L | `specsheet/05-js-to-rust-pitfalls.md:95`, `specsheet/01-phone-continuation.md:236` | silent | The pitfall doc notes that JS accepts floats. No doc states that the Rust casts truncate: `maxMessageBytes: 0.5` becomes 0, `maxFileBytes: 0.9` becomes 0, and a fractional `replayWindowMinutes` becomes 0 and rejects every older message. The config reference gives integers only. |
| C-05 L | `specsheet/01-phone-continuation.md:221`, `docs/troubleshooting.md:51-55` | contradicted | The spec says config load failures exit non-zero with a clear log line. `heartbeatMinutes: 1e300` passes validation and panics in `Duration::from_secs_f64`, so the failure is a panic, not a clear config error. The startup check sees only a failed start. |
| C-06 L | `specsheet/01-phone-continuation.md:307-312` | partial | The state section documents 500 IDs per topic and no topic-row pruning. `read_state` does not enforce `MAX_PROCESSED_IDS`, does not check the version, and accepts a hand-edited file whole; only a freshly written state is bounded. The doc sentence reads as an invariant. |
| C-07 L | `specsheet/01-phone-continuation.md:241` | silent | The config reference gives `files.inboxDir` as a relative path. No doc states the Windows drive-relative case: `C:foo` passes the absolute and `..` check, but `PathBuf::push` replaces the base, so the inbox can escape the project. Linux has no equivalent. |
| C-08 L | `specsheet/05-js-to-rust-pitfalls.md:77`, `specsheet/01-phone-continuation.md:307` | partial | The pitfall doc records that the atomic write never fsyncs. The spec and operations still say "atomic writes" without the durability caveat. A power loss can lose the latest pin or cursor, or leave a file the next start quarantines. |
| N-01 M | `specsheet/01-phone-continuation.md:353`, `docs/operations.md:249` | contradicted | The only SSRF sentence says "No host allowlist in Node; the residual SSRF risk is documented." It is not documented anywhere else, and the live Rust path has the same gap: HTTPS scheme and three redirects, no host, port, IP, or DNS check. An attachment URL can reach loopback or RFC1918 from the relay. |
| N-02 M | `specsheet/01-phone-continuation.md:423`, `specsheet/03-rust-relay-merge.md:143` | contradicted | Both specs claim every in-memory structure is bounded. `SseParser` keeps `buffer` and `data` without a cap; a peer can stream one long line or many data lines and grow memory. Both the ntfy and OpenCode subscriptions use the parser. |
| N-03 L | `specsheet/05-js-to-rust-pitfalls.md:48` | silent | The pitfall doc records the 10 s publish timeout and no retry. It does not state that publish responses are buffered without a byte cap, so a fast large error body is bounded only by the timeout. The OpenCode client has an 8 MiB cap; the ntfy publish path does not. |
| N-04 L | `specsheet/05-js-to-rust-pitfalls.md:79`, `specsheet/01-phone-continuation.md:356` | partial | The pitfall doc records the `exists` then write TOCTOU and the millisecond fallback. The spec says a collision is prefixed with milliseconds, which hides the second collision: if both `<seconds>-name` and `<milliseconds>-name` exist, the write truncates the second. Serial drains make this external-writer only. |
| O-01 M | `specsheet/01-phone-continuation.md:367` | contradicted | The spec says reply to the oldest pending request. `select_permission` takes the lexicographically smallest id, and nothing documents that ids sort by age. The Node suite pins the same "oldest by id" behavior. A reply can target a different request than the oldest. |
| O-02 M | `specsheet/01-phone-continuation.md:339-342`, `specsheet/05-js-to-rust-pitfalls.md:45-47` | contradicted | The spec says retry network errors and 5xx twice, then says delivery is at-most-once and a duplicate prompt could execute twice, so replay is preferred over blind retry. The adapter retries a lost-response POST, which is the blind retry the rationale rejects. One phone message can queue twice. |
| O-03 L | `specsheet/03-rust-relay-merge.md:134`, `specsheet/04-rust-relay-decisions.md:13`, `docs/operations.md:320` | documented | The shared registration and the explicit `baseUrl` password source are both documented, including the swap-on-401 rule. The two-file split is intended, not drift. |
| O-04 L | `specsheet/04-rust-relay-decisions.md:13` | silent | The decision row says the registration holds `url`, `pid`, and `password`, and the pid is verified. No doc states that owner and mode are not checked. A local principal who can write the registration can supply the URL and password the relay trusts. |
| O-05 L | none | silent | Discovery limits and merge order are documented; the per-row dedupe scan is not. Cost is rows times merged sessions. The default limits keep it small; a project with many rows and limit 100 is quadratic. No doc claim is broken. |
| O-06 L | `specsheet/01-phone-continuation.md:421` | contradicted | The spec caps a short failure reason at 200 characters. The adapter clips non-2xx bodies at 300 and the relay error push clips at 500; server bodies can carry paths, prompt text, or implementation detail to the phone. The count is wrong and the disclosure rule is missing. |
| I-01 H | `specsheet/01-phone-continuation.md:424-425`, `specsheet/01-phone-continuation.md:423` | silent | The docs bound individual structures and set a seven-day inbox retention, but there is no per-topic or per-day quota, no rate limit, and no disk budget. A publisher can drive unlimited agent runs and fill the project disk with attachments. The High risk has no doc sentence. |
| I-02 M | `specsheet/01-phone-continuation.md:289` | silent | The filter section caps `message` at `maxMessageBytes`. It does not state that `tags`, `attachment.name`, and `attachment.url` are unbounded, and that 1000 queue entries retain them. The body cap is documented; the metadata surface is not. |
| I-03 M | `specsheet/01-phone-continuation.md:290` | silent | The filter has a lower bound for staleness and no upper bound. A future message time is accepted into `last_processed` and `state.last_time`, and the shared SSE cursor then asks for events after that future second. The docs describe only the lower bound. |
| I-04 L | `specsheet/05-js-to-rust-pitfalls.md:153` | contradicted | The pitfall doc says `core::text::is_js_space` matches ECMAScript and that the control parser uses it. `rust-src/crates/inbound/src/control/parse.rs:219-221` defines a local `is_js_space` with `char::is_whitespace`, which includes U+0085. The doc's mitigation is not in the code, and the U+0085 divergence is live. |
| OB-01 M | `specsheet/03-rust-relay-merge.md:89`, `docs/operations.md:213` | documented | The terminal birthtime scan, the 20000-entry cap, and the reason for it are documented. The per-turn full walk is a documented cost, not drift. The cross-check's downgrade to Low is reasonable; the R1 table severity is kept here. |
| OB-02 M | `specsheet/01-phone-continuation.md:149`, `specsheet/03-rust-relay-merge.md:138-139` | partial | The docs say one PUT per file and a bounded reader channel that drops on overflow. They do not connect the two: eight sequential PUTs with 10 s timeouts can hold the single worker for about 90 s and drop live events. The consequence is not stated. |
| OB-03 M | `specsheet/01-phone-continuation.md:111`, `specsheet/07-reconnect-backfill.md:53` | partial | The docs say `execution.started` resets the text buffer and that backfill recovers completed messages. They do not state that a new start with buffered text and no terminal discards that text without a push, and that recovery waits for a reconnect. |
| OB-04 M | `specsheet/03-rust-relay-merge.md:143`, `specsheet/01-phone-continuation.md:423` | contradicted | Both specs claim every in-memory structure is bounded. `TurnTracker.states` has no count cap; each state can hold up to 2 MB for ten minutes, and expiry runs only on `tick`. A run that touches many sessions can retain hundreds of megabytes. |
| OB-05 L | `specsheet/03-rust-relay-merge.md:82`, `specsheet/01-phone-continuation.md:115` | documented | The docs state that `execution.interrupted` publishes the partial text with an `(interrupted)` marker. The idle flag not covering interrupted is a config-semantics gap, not a doc contradiction. |
| OB-06 M | `specsheet/07-reconnect-backfill.md:59` | partial | The dedupe section says an id is marked before publishing and committed after the effect. It does not state that a live event is marked seen even when target resolution fails or denies, so a transient lookup failure drops that text part until a later reconnect. |
| OB-07 M | `specsheet/07-reconnect-backfill.md:65` | contradicted | The recovery section says a failed publish produces one error push and one log line. `outbound/src/runner.rs:804-816` logs a warning and returns, with no error push. The phone gets no signal, and the message ids are committed, so the loss is silent. |
| OB-08 L | `docs/operations.md:207`, `user-manual-ntfy-android.md:75` | contradicted | The docs say a permission push is cancelled when the request is answered in time. At a configured delay of 0, `permission.rs:61-63` returns the push before the duplicate and replied checks. Zero is allowed by `core/src/config.rs:636-641`. The cross-check's raise to Medium is reasonable. |
| OB-09 L | `specsheet/07-reconnect-backfill.md:39`, `specsheet/03-rust-relay-merge.md:139` | partial | The backfill section says live events wait in the channel and dedupe normally. The channel is 256 slots; a slow server during backfill can overflow it and drop events. The docs state the queue exists but not its depth or the overflow during backfill. |
| OB-10 L | `specsheet/01-phone-continuation.md:175`, `specsheet/03-rust-relay-merge.md:91` | contradicted | The docs state project confinement as a contract with no override. `inspect` canonicalizes and checks the path, then the caller reads the path; a local writer can replace it with a symlink between the two calls. The contract is stronger than the code. The cross-check's raise to Medium is reasonable. |
| A-01 M | `specsheet/05-js-to-rust-pitfalls.md:13`, `specsheet/01-phone-continuation.md:342-343` | partial | The pitfall doc records the serial drain and that a slow prompt blocks every topic. It does not state that the Rust select loop also stops polling discovery, prune, heartbeat, and supervisor completion while `drain` runs, and that a 1000-message backlog can hold the loop for minutes. |
| A-02 M | `specsheet/05-js-to-rust-pitfalls.md:21` | partial | The pitfall doc accepts the synchronous logger. It does not state that state recording, log rotation, and prune also use blocking `std::fs` on the runtime thread, so a slow disk stalls the select loop and worker threads. |
| A-03 M | `specsheet/01-phone-continuation.md:223` | contradicted | The spec says network errors never exit and reconnect with backoff. On a non-2xx SSE response, `read_detail` reads the body with no timeout; the task never returns, so the supervisor never logs an end and never restarts. Inbound or outbound stops while the process looks healthy. |
| A-04 M | `specsheet/05-js-to-rust-pitfalls.md:33` | partial | The pitfall doc records that the stream body has no read timeout and suggests monitoring `lastEventAge` as the heartbeat does. It does not state that nothing acts on a stalled age: a half-open connection with no chunks is indistinguishable from idle, and no reconnect or restart happens. |
| A-05 M | `specsheet/01-phone-continuation.md:340`, `docs/operations.md:249`, `specsheet/05-js-to-rust-pitfalls.md:136-137` | contradicted | The spec and operations say ack bodies are capped at 3400 bytes. The cap is applied before `link_safe`; each `a.md` rewrite adds two bytes. `ntfy::header::MAX_MESSAGE_BYTES` is 4096, and the header publish does not re-check it, so the sent ack can exceed the server's 4096-byte message limit and be rejected. The pitfall doc records the growth but not the rejection. |
| A-06 L | `specsheet/06-rust-crate-split.md:12` | silent | The crate split says the MCP server is short-lived and stdio-based. No doc states that the line reader has no length cap, so one local client line can grow memory. No doc claim is broken. |
| A-07 L | `specsheet/06-rust-crate-split.md:62` | documented | The crate split says the tool works when the daemon is down. Ignoring `enabled` is the intended independent-tool behavior, not drift. |
| A-08 L | none | silent | The docs do not state the file modes for attachments, state, the seen journal, or logs. They are created with the default umask; only the journal temp file is 0600 on Unix. The data can be readable by other local users depending on directory modes. |
| A-09 L | `specsheet/01-phone-continuation.md:270`, `docs/operations.md:237` | documented | The 150-topic discovery cap and the newest-activity selection are documented in the spec and operations. The cap is implemented and tested. H-01 covers the separate ACL interaction. |

H-01 is the 46th row from the R1 handoff cross-check; it is not counted in the 45. It is silent: the docs require an explicit grant for each project (`docs/operations.md:137`) and cap the subscription at 150 topics (`specsheet/01-phone-continuation.md:270`), but do not state that a discovered ungranted topic is still added to the single SSE URL, and that ntfy can reject the whole subscription, stopping inbound for the granted topic too.

## Doc-only defects

| ID | Severity | Doc evidence | Finding |
| --- | --- | --- | --- |
| D-01 | Medium | `specsheet/01-phone-continuation.md:329`, `:375-386`, `:520`; `docs/operations.md:239`; `ntfy-opencode-incoming-relay/README.md:34-44`; `user-manual-ntfy-android.md:33`; `specsheet/02-message-routes.md:81` | The primary phone spec says starting a new session from the phone is out of scope, and its command table omits `/new` and `/status`. The live built-in table has `new`, `sessions`, `to`, `auto`, `help`, and `status`; operations, the relay README, the user manual, and spec 02 all document `/new`. This is the clearest internal contradiction in the set. |
| D-02 | Low | `specsheet/01-phone-continuation.md:3`, `:22-30`, `:448`, `:492`, `:540-551` | The rollout and deliverables still name `ntfy-opencode-modified-plugin/`, `patch-ntfy-plugin.ps1`, `tools/verify-ntfy-patch.mjs`, and `phone-files.js`. All are deleted and untracked; the 0.7.0 changelog records the deletion. The top note calls the plugin sections historical, but the paths sit in the current deliverables list. |
| D-03 | Low | `specsheet/07-reconnect-backfill.md:113` | The 0.7.4 acceptance line says `npm run links` checked 29 files. The current check reports 24 before this report and 25 with it; the six `docs/audit/` files were deleted in 0.7.5. The line is a dated record, not a current count. |
| D-04 | Low | `docs/operations.md:211`; `rust-src/crates/core/src/link_safe.rs:15-21`; `specsheet/01-phone-continuation.md:186` | The operations allowlist omits 14 entries present in the code: `ae`, `bg`, `br`, `ee`, `gr`, `hr`, `il`, `lt`, `lu`, `lv`, `mx`, `ro`, `ua`, `za`. Spec 01 lists all but `lu`. A reader cannot tell from operations which suffixes stay linkable. |
| D-05 | Low | `docs/troubleshooting.md:51`; `ntfy-opencode-incoming-relay/start-rust-relay.ps1:14`, `:62`; `start-relay.ps1:14`, `:80` | The failure hint points at `logs\relay-stderr.log`. The live Rust wrapper writes and names `logs\relay-rust-stderr.log`; the Node rollback uses `relay-stderr.log`. The hint is only correct for the rollback. |
| D-06 | Low | `docs/zero-google.md:21`; `docs/android.md:7-16` | Zero Google says the F-Droid flavor always uses instant delivery over a WebSocket. Android documents the JSON stream over HTTP as a supported connection protocol. Both are zero Google; the absolute claim is wrong. |
| D-07 | Low | `README.md:9-15` | The lead says "Two parts"; the table lists three. The third is the inbound rollback, not a part of the live path. |
| D-08 | Low | `docs/tree.md:3-109`; working tree | The tree has no `audit/` entry. If the round reports are committed, the tree is incomplete. The working tree also has an empty `crates/core`, `crates/inbound`, and `crates/ntfy` skeleton left from the 0.7.3 move; it is untracked and should be removed. Not a doc error until the audit is committed. |

## Evaluation

What the docs get right:

- Topology and ports: Serve 443, the `127.0.0.1:8810` loopback bridge, `[::1]:8810`, container 8080, no Funnel, no tailnet-facing portproxy. `docs/https.md`, `docs/tailscale.md`, and operations 7.6 agree with the code and the live evidence.
- Auth: deny-all, anonymous `/v1/health`, the three users, per-topic ACLs, the phone custom `Authorization: Bearer` header, and the two-phase rotation. Operations 7.2 and the Android manual are precise and match the token tooling.
- Happy path: topic derivation, `oc-out`, turn text assembly, the 3400-byte inline threshold, the `.md` attachment, auto-attach allowlist and caps, permission grace, question form, and the `ntfy_send_file` contract. The docs and the code agree on the normal path.
- Recovery: the 0.7.4 backfill scope, the 15-minute seen window, the 200-message cap, cursor pages without `order`, and the 41 s gate. `specsheet/07-reconnect-backfill.md` and operations 7.4 are the best-documented failure path in the set.
- Verification history: the dated gates are specific and consistent with the handoff. The link and ASCII checks pass, and the TLD check proves the two tables are identical.

What the docs get wrong:

- Limits: "everything in memory is bounded" is false for the SSE buffers and the turn tracker. There is no inbound rate limit, no per-topic quota, and no disk budget.
- Failure paths: a failed publish has no error push, a non-2xx SSE body can hang the reader, a stalled stream has no idle deadline, and a failed target lookup drops a text part. The docs describe the reconnect, not these paths.
- Trust: the docs describe the ACLs but not the consequence. Write access to the topic is the ability to prompt, steer, and answer permissions; read access is the output and the files. The `oc-out` tag is a marker, not authentication.
- Config edges: `"enabled": "false"`, fractional byte caps, `1e300` heartbeat, `http://` server, drive-relative `inboxDir`, and delay 0 all pass the current loader and break a documented guarantee or a safe default.
- Spec 01: the `/new` contradiction and the deleted plugin deliverables are the two places where the spec describes a product that no longer exists. The top historical note does not cover the out-of-scope table or the deliverables.
- Counts and names: 29 files, 200 characters, and `relay-stderr.log` are all date- or path-bound and no longer match the tree.

## Handoff to round 3

- Triage the 15 contradicted rows first: each one is either a code fix or a doc correction, and the docs cannot be trusted on that point until one side changes.
- Treat X-01, I-01, I-02, and H-01 as the security and resource cluster: either add a limit or state the accepted exposure in operations 7.2 and spec 01.
- Fix D-01 before any doc release: the phone spec and the live control table disagree about a shipped feature.
- Keep the round 2 scope out of the code plan except where the doc is the contract: O-01, OB-07, A-03, and A-05 need a code decision; D-02, D-03, D-04, D-05, and D-06 are doc edits.
