# Round 3 triage plan

Status: triage complete. 45 R1 rows, H-01, and 8 R2 doc defects: 54 entries, all decided. No code or doc is changed by this report. Round 4 executes the waves below. The ceiling is 0.7.8: the former 0.8.0 rows are folded into 0.7.7 as fixed-value patches with no new config keys.

## Inputs and method

- `audit/audit-rust-relay-code.md` supplies the rows and severities. `audit/r2-docs.md` supplies the doc status and the D set.
- R1 severities stand. The code-or-doc question uses the R2 verdict and the R1 handoff cross-check.
- Every executable row was re-read at the R1 citation. Evidence corrections are listed at the end.
- One row gets one primary decision. A code row that also needs a doc sentence is counted once in code; the 0.7.8 doc pass collects those sentences.

Decision words:

| Word | Meaning |
| --- | --- |
| code 0.7.6 | restore a documented guarantee in config, wire, or parse code; patch |
| code 0.7.7 | restore a documented guarantee in the runtime or turn path, or close a fixed-value gap; patch |
| docs | the code is the intended behavior; the doc is wrong |
| accept | known cost, stays; one doc sentence states it |
| close | not a defect |

## Counts

| Primary decision | Rows | Release |
| --- | --- | --- |
| code 0.7.6 | 21 | patch, done |
| code 0.7.7 | 13 | patch, including the five former 0.8.0 rows |
| docs | 13 | 0.7.8 |
| accept | 3 | 0.7.8 doc sentence |
| close | 4 | none |
| total | 54 | |

## Release plan

| Release | Scope | Gate |
| --- | --- | --- |
| 0.7.6 | config validation, HTTP rules, SSE and publish caps, permission selection, metadata caps, ack clamp, file modes, tests | done: fmt, clippy -D warnings, 319 Linux, 320 Windows, npm test, tlds, links, CI 43317677579 |
| 0.7.7 | turn loss paths, state cap, target resolution, drain fairness, blocking fs, error-body timeout, topic ownership, attachment host, prompt rate, idle deadline, single-root guard | fmt, clippy -D warnings, workspace tests on Ubuntu and Windows, npm test, tlds, links, then a live turn and restart-dedupe smoke after the binary swap |
| 0.7.8 | the doc pass, D-01 first; audit reports committed and added to tree.md | links and ASCII, TLD parity, D-01 checked against the built-in table |

## 0.7.6 decisions

| ID | Sev | Decision | Action | Acceptance |
| --- | --- | --- | --- | --- |
| X-03 | L | code 0.7.6 | tests use the workspace temp tree; `block_on` gets a timeout | no OS temp path in the suite; a suspended future fails |
| C-01 | M | code 0.7.6 | reject `http://` unless the host is loopback; one transport rule for publish and attach | remote http config fails at load |
| C-02 | L | code 0.7.6 | `USERPROFILE` only under `cfg(windows)` | Unix with `USERPROFILE` set resolves `HOME` |
| C-03 | L | code 0.7.6 | strict boolean for every boolean, `enabled` included | `"false"` is a config error |
| C-04 | L | code 0.7.6 | integer-only bounded numbers; no fractional casts | `0.5`, `0`, `1e300` rejected |
| C-05 | L | code 0.7.6 | bound `heartbeatMinutes` so `from_secs_f64` cannot panic | `1e300` is a config error, not a panic |
| C-06 | L | code 0.7.6 | cap load at 500 ids and 512 topic rows; quarantine an unknown version | a 10k-id file loads bounded and logs |
| C-07 | L | code 0.7.6 | `inboxDir` must be plain relative; reject prefix, root, and `..` | `C:foo` is a config error |
| C-08 | L | code 0.7.6 | `sync_all` before rename; fsync the parent on Unix | code review; durability is not unit-testable |
| N-02 | M | code 0.7.6 | SSE line cap 1 MiB, data cap 8 MiB; drop the frame, log once | an oversize frame yields no event and bounded memory |
| N-03 | L | code 0.7.6 | `max_response_bytes` on publish, 8 KiB | a large error body is clipped |
| N-04 | L | code 0.7.6 | `unique_target` loops until a free name | both candidates taken yields the next free name |
| O-01 | M | code 0.7.6 | pick the first element, the server order; flip the test | `per_2, per_1` selects `per_2`; doc states id is not age |
| O-05 | L | code 0.7.6 | `HashSet` dedupe per row | merged order unchanged |
| O-06 | L | code 0.7.6 | clip server bodies at 200 in adapter and relay | no 300 or 500 path remains |
| I-02 | M | code 0.7.6 | caps: 32 tags x 64 B, name 255, url 2048; reject over | an oversize metadata frame is rejected |
| I-03 | M | code 0.7.6 | reject `time > now + 60 s` | a future message is rejected and the cursor stays sane |
| I-04 | L | code 0.7.6 | `core::text::is_js_space` in control parse and header trim | U+0085 is not whitespace |
| A-05 | M | code 0.7.6 | `link_safe` first, then a char-boundary clamp to 3400 | 3400 B of `a.md` yields at most 4096 and no live dot |
| A-06 | L | code 0.7.6 | MCP line cap 1 MiB, then error and exit | a 2 MiB line cannot grow the process |
| A-08 | L | code 0.7.6 | 0600 on Unix for state, journal, log, and attachment files | mode check per file |

## 0.7.7 decisions

| ID | Sev | Decision | Action | Acceptance |
| --- | --- | --- | --- | --- |
| OB-03 | M | code 0.7.7 | flush buffered text on a new start even without a terminal | start, text, start publishes one partial |
| OB-04 | M | code 0.7.7 | cap at 256 turn states; flush expired, drop the oldest with one log | 300 sessions stay bounded; no buffered text is dropped without a push or a log line |
| OB-06 | M | code 0.7.7 | resolve the target before marking seen; a failed lookup stays unmarked | a failed lookup is recovered by backfill |
| OB-08 | L | code 0.7.7 | replied and duplicate checks run before the delay-0 return | at delay 0 a reply before the ask suppresses the push |
| OB-10 | L | code 0.7.7 | open once, stat the handle, read the cap from the same handle | a symlink swap cannot escape the project |
| A-01 | M | code 0.7.7 | drain at most 8 messages, then re-notify | 100 queued messages still let discovery and prune tick |
| A-02 | M | code 0.7.7 | state write, compaction, prune, and rotation move to `spawn_blocking` | the select loop is not blocked by whole-file rewrites, compaction, prune, or rotation |
| A-03 | M | code 0.7.7 | 10 s timeout on `read_detail` in both readers | a stalled error body restarts the task |
| X-01 | H | code 0.7.7 | one topic-ownership map shared by discovery, inbound routing, and outbound; a losing canonical is denied | two roots ending in `proj` produce one owner; the loser gets no route and no publish |
| N-01 | M | code 0.7.7 | attachment host must equal the ntfy host; no `attachments.allowedHosts` key in this range | with a public ntfy host an unrelated loopback or private URL is refused; a loopback ntfy host may fetch a loopback attachment |
| I-01 | H | code 0.7.7 | fixed per-topic prompt rate 30/min with a burst of 10, and a 200 MB per-project inbox byte budget; over budget is one ack error and no prompt | a burst and a disk budget are enforced with those fixed values |
| A-04 | M | code 0.7.7 | 90 s idle deadline on both event streams, then reconnect | a silent stream reconnects within 90 s; 90 s is twice the ntfy keepalive default of 45 s |
| H-01 | M | code 0.7.7 | no topic allowlist key in this range; more than one `projectRoots` entry fails at start | two roots is a startup error; one root starts |

The five rows above were the 0.8.0 wave. They are patch fixes with fixed values and no new config contract; 0.7.8 records the values and the single-root guard.

## Doc-only decisions

| ID | Sev | Decision | Action | Location |
| --- | --- | --- | --- | --- |
| X-02 | L | docs | state rustls public roots, no custom CA, no native store | spec 01 TLS, README 36 |
| X-04 | L | docs | remove the macOS containment claim or mark it untested | spec 01 409-414 |
| O-02 | M | docs | state the retry and the duplicate risk; drop at-most-once for prompts | spec 01 339-342, pitfalls 45-47 |
| O-04 | L | docs | the registration is trusted local state; owner and mode are not checked | decisions 13 |
| OB-07 | M | docs | a failed publish leaves one log line, commits the ids, and sends no error push | backfill 65 |
| D-01 | M | docs | spec 01: `/new` and `/status` in scope; fix the command table and the out-of-scope line | spec 01 329, 375-386, 520 |
| D-02 | L | docs | drop the deleted plugin paths from the deliverables list | spec 01 3, 22-30, 448, 492, 540-551 |
| D-03 | L | docs | correct the 29-file link count to the 24 or 25 of that date | backfill 113 |
| D-04 | L | docs | add the 14 missing allowlist entries, and `lu` to spec 01 | operations 211, spec 01 186 |
| D-05 | L | docs | name `relay-rust-stderr.log` for the Rust path | troubleshooting 51 |
| D-06 | L | docs | qualify instant delivery: WebSocket or the JSON stream | zero-google 21 |
| D-07 | L | docs | "Two parts" becomes three rows, or the third is marked rollback | README 9-15 |
| D-08 | L | docs | add `audit/` to tree.md in the commit that adds the reports | tree.md |

## Accepted

| ID | Sev | Decision | Text to add | Location |
| --- | --- | --- | --- | --- |
| OB-01 | M | accept | the terminal birthtime scan is the source and costs one walk per turn, capped at 20000 entries | operations 213, merge 89 |
| OB-02 | M | accept | eight sequential PUTs can hold the worker about 90 s while the 256-slot channel drops and counts | spec 01 149, merge 138-139 |
| OB-09 | L | accept | backfill runs in the worker, so live frames wait and can overflow the 256-slot channel | backfill 39, merge 139 |

## Closed

| ID | Sev | Reason |
| --- | --- | --- |
| O-03 | L | documented and intended: shared registration, or the private file with an explicit base URL |
| OB-05 | L | documented behavior; the idle flag is not the interrupted switch |
| A-07 | L | the MCP tool is independent of the daemon by contract |
| A-09 | L | the 150-topic cap is documented and tested; H-01 covers the ACL interaction |

## Evidence corrections

- A-03: `read_detail` caps the body at 200 bytes. The defect is time only; do not add a size cap again.
- C-06: `read_state` forces version 1 and accepts the file. The live gaps are the unbounded `last_ids` and stale topic rows, not a version check.
- OB-03: `started` already flushes when `debounce_at` is set. The loss needs buffered text and no terminal.
- O-01: `PermissionInfo` carries only `id`; there is no age field. First element is the only contract-based choice. `inbound/src/permission.rs:129` pins the lexicographic pick today.
- D-08: the empty `crates/core`, `crates/inbound`, and `crates/ntfy` skeletons do not exist; all three crates are populated. Only the tree entry is missing.
- H-01: latent with one project root; the ACL covers `ntfy-proj` and `ntfy-probe`.
- X-01: the basename collapse is pinned as intended at `core/src/topic.rs:191`. The defect is that the losing canonical still routes and publishes.

## Round 4 order

1. 0.7.6 wave, done at `2154a62`. One commit per crate group; tests with each fix. Gate passed: fmt, clippy `-D warnings`, 319 Linux, 320 Windows, npm test, tlds, links, CI 43317677579.
2. 0.7.7 wave. The eight runtime and turn fixes plus the five former 0.8.0 rows, tests with each. Same gate, then stop, replace, and start the relay for a turn-text, ack, and restart-dedupe smoke.
3. 0.7.8 wave. D-01 first. Commit `audit/` with the tree.md entry. Gate: links and ASCII, TLD parity, no stale count, D-01 checked against the built-in table. This is the last release; no 0.8.0.
4. Record the live binary and rollback copy after 0.7.7; the current rollback set stays.

## Fixed values in 0.7.7

- prompt rate: 30 per minute with a burst of 10, per topic.
- inbox budget: 200 MB per project, oldest files pruned first.
- idle deadline: 90 s on both event streams; the ntfy 2.37.2 keepalive default is 45 s and `server.yml` does not override it.
- attachment host: must equal the host in `ntfy.server`; no `allowedHosts` override in this range.
- topic guard: more than one `projectRoots` entry fails at start; no allowlist key in this range.
- turn state cap: 256 states, matching the seen-window order of magnitude.
