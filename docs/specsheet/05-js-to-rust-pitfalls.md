# JS to Rust pitfalls: ntfy relay

Status: research input for the merge in `03-rust-relay-merge.md`. Not a decision record; open items still land in `04-rust-relay-decisions.md`. Most "decision needed" markers were resolved during the port; check `04` and the crate before treating one as open.
Scope: behavioral semantics of the inbound relay, the patched outbound plugin, and the helpers both use. Syntax, crate selection, and workspace layout are out of scope.
Method: read `ntfy-inbox-relay.mjs`, `relay-control.mjs`, `opencode-adapter.mjs`, `ntfy-topic.mjs`, `link-safe.mjs` and `dist/link-safe.js`, `phone-files.js`, `phone-outbound.js`, `ntfy-client.js`, `tests/*.mjs`, `tools/verify-ntfy-patch.mjs`, `03-rust-relay-merge.md`, and `04-rust-relay-decisions.md` on 2026-11-10. Counts rechecked 2026-11-11 with `npm test`: 94. At capture time the two `link-safe` JS copies were byte-identical; the plugin copy was deleted in 0.7.0. The Rust table is generated from the remaining list and checked by `npm run tlds`.

## How to use

Read the section for a module before porting that module. Each row is JS behavior, Rust trap, mitigation. Decisions live in `04`; this file records the behavior the port must preserve.

## Runtime model

`Relay.enqueue` stamps `msg.receivedAt`, pushes onto a 1000-slot array, drops the oldest with `shift`, and calls `void this.drain()`. `Relay.drain` is single-flight through `drainPromise` and handles one message at a time, so a slow prompt blocks every topic. Both properties are pinned by "drain: messages are handled serially in arrival order" and "queue: dropping the oldest message does not move the replay cursor".

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| `Relay.stop` aborts only `currentAbort` (the SSE response) and then polls `draining` for 10 s in `finishStop`; the in-flight `handleMessage` keeps running. | `JoinHandle::abort` on shutdown cancels a prompt mid-POST and loses the ack; waiting on the task can hang for a 30 s CLI call. | Cancel the reader with a `CancellationToken`, let the consumer finish under a hard deadline, then unlock. |
| Timeouts are `AbortController` plus `setTimeout` in `publishNtfy` and `downloadAttachment`, and a kill timer in `runProcess`. | `tokio::time::timeout` around a future that owns a `Child` drops the future but not the process. | Wrap the request future; set `kill_on_drop(true)` on `Command`, and on `reqwest` rely on the timeout covering the body. |
| `enqueue` is the only fire-and-forget call in the relay; `drain` catches per message, and the discovery interval attaches `.catch`. | A bare `tokio::spawn` without a stored handle hides panics and exits, so the supervisor in 03 section 8 never sees a reason. | Every task returns a typed reason through a `JoinHandle` the supervisor awaits; do not use `panic = "abort"` in the release profile. |
| A throw in a `setInterval` callback in `Relay.run` is an unhandled rejection, which terminates Node by default. | A panic in a spawned Rust task is contained by default, so the process keeps serving with one direction dead. | Treat `JoinError` as a restart reason; log one line per restart as 03 section 8 requires. |
| `Logger.log` uses `fs.appendFileSync`; state reads and writes use `fsp`. | Any `std::fs` call inside an async task blocks a worker thread. | Use `spawn_blocking` only if the sync append ever becomes measurable; one line per event does not need it, so keep it sync and simple. |

## SSE over HTTP

`createSSEParser` buffers text, splits with `/\r\n|\n|(\r)(?!$)/`, joins `data` lines with `\n`, ignores comment lines, and drops frames whose `JSON.parse` fails. `Relay.subscribe` owns the reconnect state machine.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| The `(?!$)` alternative holds a bare trailing `\r` until the next chunk, so a CRLF split at any byte is one event. | `BufRead::lines`, `str::lines`, or a per-chunk split treats the trailing CR as a line end and emits an empty line or a second event. | Keep a byte buffer with the same lookahead; test "sse parser: CRLF split at every byte" as the acceptance case. |
| `TextDecoder` with `{stream:true}` carries a multi-byte code point across `push`, and `decode()` at `end` flushes it. | `String::from_utf8` per chunk errors on a split code point; `from_utf8_lossy` emits U+FFFD and changes the payload. | Buffer bytes, decode only complete lines, and validate UTF-8 at dispatch. |
| Only `data` fields are used; `event`, `id`, and `retry` are ignored, and a bad JSON frame is dropped silently. | `serde_json` returns an error that aborts the read loop if the `?` is not handled. | Match on the parse result, log at debug, and continue; one malformed frame must not kill the subscription. |
| `subscribe` computes `since` as `lastProcessed - 60` when set, else `now/1000 - replayWindowMinutes*60`; `lastProcessed` is in-memory and advances only for accepted messages at handle time. | Persisting `lastProcessed` or deriving it from `state.topics[topic].lastTime` changes the replay window after restart. | Keep the global cursor for `since` and the per-topic `lastTime` for filtering; test "queue: dropping the oldest message does not move the replay cursor". |
| A 30 s `SSE_HEADER_TIMEOUT_MS` timer is cleared when `fetch` resolves; the body has no read timeout. | `reqwest`'s total timeout would also cut a healthy long-lived body if applied to the stream. | Time out only until headers; monitor `lastEventAge` for stalls, as the heartbeat does. |
| Backoff starts at 1 s, doubles to 60 s, is jittered 0.8 to 1.2, and resets to 1 s only when the previous connection lasted `STABLE_CONNECTION_MS` (60 s), evaluated on the next failure. | Resetting on connect, or on a 200 response, makes a flapping server retry at 1 s forever. | Keep the reset on the failure path, guarded by `connectedAt`. |
| Any status below 500 except 429 goes to a fixed 60 s pause with no backoff change; 429 and transport errors take the jittered path. | A generic "retry 5xx" helper retries 401 and 404 responses that will never succeed. | Match the JS classification, or record a deliberate change in `04`. |
| `discover` sets `resubscribe` and aborts when the sorted topic key list changes; the catch continues with no sleep. A project-ID-only change does not abort. | Comparing full entry signatures resubscribes on every project ID change and drops the stream. | Compare the sorted topic keys only; test "discovery: a project ID change updates the map without a resubscribe". |

## HTTP and retries

At capture time two publishers existed. `publishNtfy` in `ntfy-inbox-relay.mjs` POSTs to `${server}/${encodeURIComponent(topic)}` with `Title`, `Priority`, and `Tags` headers and a raw body. `createNtfyClient().publish` in `dist/ntfy-client.js` POSTs JSON to the base URL with `topic` in the body, and `publishFile` PUTs with query parameters. The plugin publisher was deleted in 0.7.0; the Rust `ntfy` crate now carries all three styles.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| Header-style POST and JSON-style POST are not wire-compatible. | One `core` client cannot be byte-identical to both; 03 step 2 said "compare wire bytes with the current client" without naming which one. | Decided (04 row 11): header POST for relay acks, JSON POST for outbound text, PUT for files; `rust\crates\ntfy` exposes all three. |
| `withRetry` retries `status === undefined` (spawn failure, timeout, unparsed error) and `status >= 500`, twice, with 1 s and 2 s delays; 4xx and 3xx are never retried. | A typed error enum with `Retryable` defaults retries a 401 or gives up on a transport error. | Classify explicitly: no status is retryable, 5xx retryable, everything else terminal. A no-status retry can duplicate a prompt whose response was lost; there is no idempotency key, so that duplicate risk is accepted. |
| `parseHttpStatus` extracts the status from `opencode api` output with `/HTTP\s+(\d{3})/i`. | Direct HTTP makes the parser unnecessary, but it also changes which failures were retried before. | The Rust client retries no-status and 5xx twice; the adapter tests pin it. |
| `opencode.timeoutMs` (30 s) is per CLI attempt; 3 attempts plus 3 s of sleeps bounds one prompt at about 93 s. | One timeout wrapped around the retry loop, or one around the whole prompt, cuts the budget differently. | Per attempt: `opencode.timeoutMs` wraps each request; retries add 1 s and 2 s, so one prompt is bounded at about 93 s. |
| `publishNtfy` has a 10 s `PUBLISH_TIMEOUT_MS` and no retry; a timeout is a lost ack, logged and swallowed by `ack`. | A retried publish can duplicate an ack on the phone. | Keep the publish path single-attempt unless 04 decides otherwise. |
| Node `fetch` uses the bundled CA set and ignores `HTTP_PROXY`/`HTTPS_PROXY` unless `NODE_USE_ENV_PROXY` is set. | `reqwest::Client::builder()` honors proxy environment variables and may use the platform verifier. | Call `.no_proxy()` or configure the proxy explicitly, and pin the TLS verifier. |
| `downloadAttachment` uses `redirect: "manual"`, at most 3 followed redirects (4 requests), re-validates HTTPS on each hop, and cancels the redirect body. | `reqwest`'s default policy follows 10 and does not re-check the scheme. | Custom redirect policy with the same cap and scheme check; test "attachment: a redirect downgrade to HTTP is rejected". |
| `sendPrompt` rebuilds the JSON string per attempt inside `apiRequest`. | `reqwest::Body` from a stream is consumed by the first attempt. | Serialize once to `Bytes` and clone per attempt. |
| The SSE response holds one socket for the process lifetime. | A small shared connection pool can starve publishes behind the stream. | Use one client with a pool large enough for SSE plus publishes, or a separate client for the stream. |

## Timers and clocks

Every window in `Relay` and `Control` uses `Date.now()` through the injected `now`: `confirmMs` 60 s, `CONFIRM_EXPIRED_GRACE_MS` 5 min, `cooldownMs` up to 1 h, the 10 s shutdown deadline, the 60 s stable window, and the replay window. A backward clock step extends a hold and a cooldown; a forward step expires them.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| All windows are wall clock. | `Instant` is monotonic and does not move with NTP or a manual clock change, so behavior diverges after a resume or a step. | Decided (04 row 10): wall time for replay and logs; monotonic for holds, cooldowns, and shutdown. |
| `setInterval` for discovery and heartbeat never catches up after a stall. | `tokio::time::interval` defaults to `MissedTickBehavior::Burst` and fires back to back. | Set `Delay` or `Skip`; keep the heartbeat a log line, not a push. |
| The discovery interval callback does not await the previous `discover()`, so two runs can overlap and both abort the SSE stream. | A spawned tick has the same race, and two aborts can interleave with a resubscribe. | Single-flight the discovery task, or run it in the same select loop as the stream. |
| `Relay.pause` sleeps in 250 ms slices and re-reads `this.now()`, so `stop()` interrupts it. | Plain `tokio::time::sleep` waits to the end. | `select!` on a shutdown token in every backoff and pause. |
| `phone-outbound` calls `.unref?.()` on the debounce and TTL timers so the plugin process can exit. | Rust timers always keep the runtime alive; there is no unref. | Cancel turn timers on shutdown and cap the drain wait. |
| `jitter` reads `Math.random`. | A seeded global RNG makes retry tests order-dependent. | Inject a jitter source, as the relay injects `now` and `sleep`. |
| Log lines use `toISOString()` (UTC, milliseconds); the lock uses the same; `corruptStatePath` and attachment names use epoch seconds, with a millisecond fallback on name collision. | Mixing second and millisecond precision shifts names and replay arithmetic. | Keep one helper per unit and test the exact string shape. |

## Filesystem

`acquireLock` creates with `wx`, `writeStateAtomic` writes a pid-suffixed temp then renames, and `phone-files` does a birthtime scan. These carry the most host-specific semantics in the port.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| `fsp.open(file, "wx")` creates exclusively, then `writeFile` fills the body; a reader can see an empty file, so `acquireLock` retries the read 3 times with 20 ms sleeps. | `create_new(true)` plus `write_all` has the same window; assuming the body is visible atomically makes a concurrent start fail as "unreadable". | Keep the read retry; write the body through the already-open handle, not a second open. |
| An unreadable lock fails closed and is not deleted; only a readable lock with a dead pid is removed, then one retry. | `remove_file` on any parse error converts a corrupted lock into a takeover. | Return `unreadable` and leave the file; test "lock: unreadable lock fails closed and is not deleted". |
| `pidAlive` uses `process.kill(pid, 0)` and treats `EPERM` as alive; the lock body's `started` field is never compared. | `kill(pid, 0)` with EPERM maps to `alive`, but a recycled pid is still accepted. | Match EPERM as alive; do not start trusting `started` unless 04 decides to add a start-time check. |
| `writeStateAtomic` writes `${file}.${pid}.tmp`, renames over the target, and never fsyncs or sweeps. | `fs::rename` matches the replace semantics on Windows, but a crash leaves a temp file that the Node code also leaves. | Keep the same temp name and cleanup behavior, or add a startup sweep and record the change. |
| `writeStateAtomic` serializes with `JSON.stringify`, so key order is insertion order, except integer-like topic names, which JS reorders numerically. | `serde_json` on a `HashMap` reorders everything; a byte-golden on the state file breaks. | Use a struct with fixed field order; do not golden the state bytes. |
| `downloadAttachment` does `existsSync` then `open(target, "w")`, so a collision is a TOCTOU that truncates the winner; the fallback name switches from epoch seconds to epoch milliseconds. | `create_new` turns the race into an error and changes the name format. | Preserved: `exists` then `tokio::fs::write`, milliseconds fallback; the TOCTOU remains, as in Node. |
| `scanNewFiles` uses a LIFO stack and a `SCAN_MAX_ENTRIES` 20000 cap, so which files are found at the cap depends on traversal order. | A BFS or a sorted walk changes the attachment set for huge trees. | Keep the LIFO order; test "PASS scan entry cap". |
| `stats.birthtimeMs > 0` gates inclusion in the scan, but the later cutoff check is skipped when birthtime is 0, so watcher paths with unknown birthtime pass and scanned paths do not. | `metadata.created()` returns `Err` on many Linux filesystems; substituting `now()` or `ctime` silently attaches old files. | Mirror the `> 0` sentinel and keep the watcher path separate; test on a filesystem without birth time. |
| `isPathInside` lowercases both sides on win32; `canonicalKey` lowercases for dedupe; `normalizeKey` in `phone-files` lowercases only when `isWindowsPath`. | `Path::starts_with` and `strip_prefix` are case-sensitive and component-wise; `pathdiff` is case-sensitive too. | Implement a component-wise compare with the same `toLowerCase` folding on win32; keep `platform` an explicit parameter, not `cfg!(windows)`. |
| `isInside` in `phone-files` rejects `rel === ""` and uses `relative()`, which is case-insensitive on win32. | Accepting the root itself, or a case mismatch, lets a path outside the project pass. | Port the same three rejections: empty, `..`, absolute. |
| `realpathSync` resolves symlinks and returns a normal Windows path. | `fs::canonicalize` returns a `\\?\` verbatim path; it leaks into logs, `isInside`, and `pathToFileURL` output. | Use `dunce::canonicalize` or strip the verbatim prefix before any string use. |
| `pathToFileURL(file.uri).href` builds the `files[].uri` sent to OpenCode; `01-phone-continuation.md` records that an absolute path is rejected with HTTP 400. | `Url::from_file_path` fails on a relative path, encodes `#`, `%`, and non-ASCII differently in edge cases, and cannot reproduce a Windows path when built on Linux. | Test exact strings for space, `#`, `%`, non-ASCII, and UNC; inject the platform base. |

## JSON and config

`loadConfigFromText` does a `deepMerge(DEFAULTS, raw)` and then a hand-written `validateConfig`; `validateControlTable` is strict per action but lax at the document top level; `normalizeState` accepts almost anything.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| Unknown config keys survive the merge and are ignored; `enabled` is never type-checked, so `"enabled": "false"` keeps the relay running. | `#[serde(deny_unknown_fields)]` rejects files that work today; a `bool` field rejects the string that silently worked. | Match leniency or make strictness a `04` decision; do not let serde defaults decide silently. |
| Arrays replace wholesale in `deepMerge`; `projectRoots` does not concatenate. | `serde(default)` plus a merge helper that appends changes the root set. | Replace arrays; test the "home expansion and relative roots" case. |
| `requirePositive` accepts floats; `cooldownMs` requires `Number.isInteger`; no limit is an integer by type. | `u64` rejects 0.5 and 16384.0; `f64` accepts NaN and infinity that `Number.isFinite` rejects. | A `PositiveF64` newtype with `is_finite` and `> 0`, converted to the unit the code uses. |
| A leading U+FEFF is stripped from config and control text; a BOM anywhere else is a parse error. | `serde_json` rejects a BOM; `str::trim` does not remove U+FEFF, while JS `String.trim` does. | Strip exactly one leading BOM from bytes; audit every `.trim()` for FEFF parity. |
| Duplicate JSON keys are last-wins in `JSON.parse`. | A serde derive errors with "duplicate field". | Config pre-parses into `serde_json::Value`, so the last key wins; typed state rejects a duplicate field and quarantines the file. |
| `Number(msg.time)` accepts a numeric string; `?? 0` only covers null and undefined. | `i64` deserialization rejects `"1700000000"` that the filter currently accepts. | Parse leniently at the wire boundary, strictly inside; test a string time. |
| The control file is size-capped at `MAX_CONTROL_FILE_BYTES` 64 KiB using `stat` before `readFile`; the config file has no cap. | `serde_json::from_reader` reads an unbounded config; a `stat` then read has the same TOCTOU. | Read into a bounded buffer for control; decide whether the config gets a cap; test "control: the control file is size-capped". |
| `validateControlTable` rejects unknown fields inside an action but drops unknown top-level fields. | A derived struct with `deny_unknown_fields` at both levels changes the contract. | Keep the asymmetry, or change both and record it. |
| `helpText` iterates `Object.entries(actions)` in file order. | `serde_json::Map` without `preserve_order` is a `BTreeMap` and sorts verbs alphabetically, so `/help` reorders. | Use `indexmap`/`preserve_order` or a `Vec` of actions. |
| `normalizeState` accepts a string `lastIds`, which turns into a character set in `new Set(topicStateEntry.lastIds)` and disables duplicate detection. | A typed state rejects the file; a lenient one may reproduce the character-set bug. | Reject a non-array `lastIds` and quarantine the file; record the recovery change. |
| `readState` quarantines only invalid JSON, not a wrong shape. | A typed `serde` error on shape sends every malformed state to the same path. | Split the error: parse failure quarantines, shape failure follows the current in-memory behavior or is quarantined by decision. |

## Process model and lifecycle

`main` acquires the lock before reading the token, quarantines corrupt state, registers `SIGINT` and `SIGTERM`, and releases the lock in the `finally` around `relay.run()`.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| `SIGINT` and `SIGTERM` call `shutdown`, which is idempotent through `stopPromise`. | The Windows launcher starts the relay detached and hidden, so no console Ctrl+C arrives and the handlers never run; `TerminateProcess` skips them. | Treat a leftover lock as the normal stop path; the next start must replace it via the dead-pid rule. `ctrlc` covers console events, not service stop. |
| Exit code 1 for config, unreadable lock, token, and quarantine failures; 0 when already running, when disabled, and after a signal stop. | A Rust `main` returning `Result` exits 1 through the runtime message, and a panic exits 101. | Return an explicit `ExitCode`; match the four cases; test "already running" and "disabled". |
| The lock is released in `main`'s `finally` after `relay.run()` returns, but a 30 s CLI child can outlive that point. | Unlocking before the consumer drains allows a second instance to write the same state while the first still holds it in memory. | Join the consumer under the 10 s deadline before unlocking, or keep the lock until the process exits. |
| `runProcess` spawns with `windowsHide: true` and `stdio: ["ignore", "pipe", "pipe"]`, and kills at a 4 MiB buffer cap. | `Command::output()` inherits no window but buffers without a cap; `kill_on_drop` is not set by default. | Cap both pipes, set `kill_on_drop(true)`, and hide the window explicitly on Windows. |
| `isMainModule` compares `import.meta.url` with `pathToFileURL(realpathSync(argv[1]))`. | A binary has no equivalent and does not need one; porting the check as a startup condition adds a false failure mode. | Drop it. |

## Logging

`Logger` writes `${ISO} ${LEVEL} ${message}\n` with `appendFileSync`, rotates before a write when `size >= maxBytes` to a single `.1`, and swallows rotation and write errors.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| `size` starts from `stat` at init and increments by the exact bytes appended; rotation can leave the file one line over `maxBytes`. | A `BufWriter` reports a different size and loses the tail on a crash. | Count bytes at the write call, flush per line or accept the same loss window. |
| Rotation removes `.1`, renames the current file, and resets size even if the rename failed. | A failed rename in Rust returns an error that aborts the log path if unwrapped. | Swallow and reset exactly as `rotate` does; test "logger: rotates at the size limit and keeps one old file". |
| `echo` sends warn and error to stderr, everything else to stdout; the default config sets `echo: false`. | `tracing` to stderr by default changes what the launcher captures. | Match the stream split or make it config-driven. |
| Heartbeat is a log line: `heartbeat topics=N lastEventAge=age queue=N`; `age` is -1 before the first event. `ntfy-relay status` prints the last log line. | A structured logger changes the field syntax and breaks the status report. | Keep the exact field names and the -1 sentinel. |
| No prompt text is logged by convention: `deliverPrompt` logs `textBytes`, but failure paths interpolate `error.message`, which can contain CLI stderr. | A retry logger that dumps the request body leaks prompts to disk. | Keep payload logging out of the client; log byte counts and statuses only. |
| `clip` slices by UTF-16 code units and can split a surrogate pair. | `str` slicing by chars cannot reproduce the replacement character. | Log a lossy, valid UTF-8 string; do not claim byte parity for clipped error text. |

## Text and encoding

There are three byte budgets and three truncation behaviors. `clampBytes` reserves 3 bytes for `...` under `ACK_MAX_BYTES` 3400; `truncateUtf8` cuts at a UTF-8 boundary with no marker under `MAX_MESSAGE_BYTES` 4096; `boundQuestionMessage` appends `\n(truncated)` under the same 4096.

| JS behavior | Rust trap | Mitigation |
| --- | --- | --- |
| `clampBytes` iterates code points with `for...of`, stops before `limit - 3`, and appends ASCII `...`; `ackBody` then runs `linkSafe`, which can grow the string past the clamp. | Clamping by bytes after `linkSafe`, or using `...` with a different width, changes the ack length. | Clamp first, then defang, as `ackBody` does; assert the multibyte case "ack body is clamped to 3400 bytes". |
| `ackBody` clamps to `ACK_MAX_BYTES` 3400 and then runs `linkSafe`; each rewritten token grows by two bytes because `[.]` replaces `.`, so the sent body can exceed 3400. | Re-clamping after the filter drops characters, and clamping after the filter changes which text is cut. | Keep clamp-then-defang; assert the post-filter length can exceed 3400 and never re-clamp. |
| `firstCharacters` uses `Array.from` for the 500-character summary, so it counts code points and can split a grapheme cluster. | `chars().take(500)` matches; `byte` slicing does not; `graphemes` changes the cut. | Use code points; keep the `.md` attachment body as `rawBody`, which is not defanged. |
| `cleanTitle` uses `.length` and `.slice(0, 200)`, so it can split a surrogate pair; `MAX_CREATED_TITLE_CHARS` is 200 UTF-16 units. | Rust `String` cannot hold a lone surrogate, so `chars().take(200)` allows more visible characters. | Accept the divergence in the safe direction and record it; do not truncate bytes. |
| `sanitizeFilename` keeps only `[A-Za-z0-9._-]`, cuts at 120 UTF-16 units, prefixes `_` to DOS device names, and drops non-ASCII letters; the test pins `Rsum.pdf` for an accented name. | `Path::file_name` plus a Unicode sanitizer keeps characters the relay removes. | Port the exact regex and the reserved-name test. |
| `headerText` collapses CR and LF to spaces, returns `-` for empty, and RFC 2047 base64-encodes any non-ASCII; there is no byte cap. | `HeaderValue::from_str` rejects non-ASCII and control characters; `from_bytes` does not produce the encoded-word. | Encode the same way and test "header text: ascii passthrough, newlines collapsed, unicode encoded". |
| `normalizeTags` silently drops tags that do not match `[A-Za-z0-9_-]+` and omits the header when the list is empty. | A typed `Vec<Tag>` rejects the whole publish on one bad tag. | Filter, do not fail. |
| `linkSafe` returns non-string input unchanged, including `undefined`; `sendFile` passes `undefined` as the message when the user gave none. | `&str` cannot represent the absent message; `Some("")` sends an empty body instead of no body. | Keep `Option<&str>` through the publish path; test the tool without a message. |

## Byte parity

The generated table in `rust\crates\core\src\tlds.rs` is the only link-safety TLD list. `tools/link-safety/build-tlds.mjs` (wrapped by `npm run tlds`) checks the generated `IANA_TLDS` literal and fails when it is out of date; the table is rewritten only with `--write` (`npm run tlds:write`). The Node copies and the plugin verifier were deleted with the publisher plugin in 0.7.0 and with the Node relay in 0.8.0.

| Concern | Trap | Mitigation |
| --- | --- | --- |
| The TLD table is a generated literal inside `link-safe.js`, built from `tools/link-safety/tlds.txt`, ASCII 2 to 24 letters, punycode dropped. | The Rust table is generated from the same source; a manual edit drifts silently. | `tools\link-safety\build-tlds.mjs`, wrapped by `npm run tlds`, fails when the Rust table drifts; `--write` rewrites it. The Node copy was deleted in 0.8.0. |
| `SCAN_PATTERN` uses the `g` and `i` flags and `String.replace` with a callback. | Rust `replace_all` is equivalent for this pattern, but `(?i)` in the regex crate uses Unicode folding; JS `/i` on `[A-Za-z]` also folds U+212A and U+017F. | Use `(?i-u)` or explicit ASCII classes, then prove it on a fixture with those code points. |
| `\s` and `\S` differ between JS and the regex crate at U+FEFF (JS whitespace, Unicode White_Space is not). | A prefix or verb separated by FEFF parses differently. | `core::text::is_js_space` matches the ECMAScript set; the control parser uses it and tests FEFF. |
| The JS parity check compared two Node files in one tree; the Rust table was a third copy. | A Rust port makes parity a multi-file problem, and a "looks the same" review does not scale. | Shared fixture corpus: tokens for every ALLOWED_TLD, unknown 2-letter, unknown long, scheme URL, plain dotted word, `readme.md`, `package.json`, and the unicode edge cases; assert identical output strings. Since 0.8.0 the Rust table is the only copy and `npm run tlds` checks it against the source. |

## Testing

The 94 `node --test` cases pin exact reason strings, ack text, header names, byte caps, lock behavior, the queue cap, and the SSE split-by-byte case. They do not pin real ntfy frames, publish wire bytes beyond a fake `fetchImpl`, the reconnect schedule, or the backoff sequence.

| JS test shape | Rust equivalent | Note |
| --- | --- | --- |
| Pure functions (`shouldAcceptMessage`, `parseControl`, `linkSafe`, `topicForProject`) called directly. | Unit tests on the same functions. | Keep the reason strings; the tests assert them. |
| Injected `fetchImpl`, `sleep`, `now`, `apiRequest`, `writeState` through the `Relay` deps object. | Traits or function pointers for the clock and HTTP; `wiremock` for wire assertions. | Do not bind a port for parser tests; `createSSEParser` is exported for that reason. |
| `node --test` on `tests/*.mjs`, temp dirs under `tests/.tmp-*`. | `cargo test`; `tempfile::Builder::tempdir_in(env!("CARGO_MANIFEST_DIR"))`. | The repo rule bans system temp paths; keep test dirs in the workspace. |
| Time is injected (`Control({now})`, `Relay({deps.now})`, `withRetry({sleep})`, `phone-outbound` `now` and `timers`). | `tokio::time::pause` covers tokio timers only, not `SystemTime::now()` inside logic. | Decided (04 row 10): inject `Clock::now_ms`; replay uses wall time, control windows use the monotonic source. |
| Concurrency: two `acquireLock` calls with different pids in one process. | Two tasks or two processes; the pid is a parameter in JS. | Keep `pid` injectable in the Rust signature. |
| JS-only cases: `undefined` versus null, `Number("...")`, stable `sort`, `localeCompare`, `toISOString`, `for await` over a body. | Rust must choose an explicit behavior. | Add a test per case; do not rely on the type system to guess the old behavior. |
| Outbound turn and file behavior lived in `tools/verify-ntfy-patch.mjs`, not in `tests/`. | The verifier was deleted with the publisher plugin in 0.7.0. | The contract is now held by the Rust suites: the 89 tests in `rust\crates\outbound`, the inbound suite, and the app MCP test. |

## Per-module risk

| Module | Hardest semantic to preserve | Test that proves it | Risk |
| --- | --- | --- | --- |
| `ntfy-inbox-relay.mjs` `Relay.subscribe` and `createSSEParser` | Incremental UTF-8 and CRLF parsing plus the reconnect and backoff state machine | "sse parser: CRLF split at every byte", "queue: dropping the oldest message does not move the replay cursor" | high |
| `ntfy-inbox-relay.mjs` `Relay.handleMessage`, `shouldAcceptMessage`, `recordProcessed` | Filter order, enqueue-time `now`, state written before delivery, per-topic dedupe | "message: state is written before delivery and a failure pushes an error", "filter: delayed message is judged at enqueue time" | high |
| `relay-control.mjs` `Control` and `validateControlTable` | Confirm hold per topic, expiry grace, cooldown consumed before the handler runs, exact validation messages | "control: /yes executes the held action once", "control: a held action expires and a late /yes reports expired", "control: cooldown rejects a repeated run" | medium |
| `opencode-adapter.mjs` `createOpenCodeAdapter` | Capability list driven by `agent` and `model`; pin by id prefix; title cleaning | "control: /help lists configured verbs", "control: created session titles are cleaned and capped" | medium |
| `ntfy-topic.mjs` `topicForProject` | Host-independent Windows and POSIX parsing, 64-character ASCII topic, trailing separators | "topic derivation: windows and posix", "topic derivation: folder name length limit is 64" | low |
| `link-safe.mjs` `linkSafe` | Byte-identical table and token scan across two copies | "PASS relay link-safe copy identical", "PASS link-safe allowlist and defang" | medium |
| `phone-outbound.js` `createTurnTracker` | Debounce plus TTL, key dedupe, cap drops the tail once, partial flush, unref timers | "PASS turn text assembled once", "PASS late duplicate terminal suppressed", "PASS turn text cap drops the tail once" | high |
| `phone-files.js` `createFileOutbound` | Birthtime sentinel, LIFO scan cap, allowlist versus tool, caps summary, read-time regrowth check | "PASS terminal scan for nested files", "PASS scan entry cap", "PASS file allowlist, caps, tool contract" | high |
| `ntfy-inbox-relay.mjs` `acquireLock` and `writeStateAtomic` | `wx` semantics, unreadable lock left in place, temp plus rename | "lock: concurrent acquire yields exactly one winner", "lock: unreadable lock fails closed and is not deleted" | medium |
| `ntfy-inbox-relay.mjs` `downloadAttachment` | HTTPS on every hop, 3 redirects, declared versus streamed size, filename sanitization | "attachment: a redirect downgrade to HTTP is rejected", "attachment: oversize download is rejected and removed" | medium |
| `ntfy-inbox-relay.mjs` `Logger` | Rotation at a byte size with one old file and UTC millisecond lines | "logger: rotates at the size limit and keeps one old file" | low |
| `ntfy-inbox-relay.mjs` `loadConfigFromText` | Merge leniency, BOM, floats, and error text | "config: invalid documents are rejected", "config: home expansion and relative roots" | medium |

## Pre-port checklist

- Which exact strings does this module emit, and which test or doc asserts them?
- Which clock does this window use, and does a clock step change the outcome?
- Where does the JS code accept `undefined`, null, a numeric string, or a float that a Rust type would reject?
- Is the cap in bytes or UTF-16 units, and does it add a marker?
- Does the code mutate state across an `await` that a second task could observe?
- What happens on Windows casing, a `\\?\` path, and a non-ASCII filename?
- Does this call retry, on which statuses, and with which timeout boundary?
- What did the run of `verify-ntfy-patch.mjs` print for this module, and is that assertion moving to `cargo test`?

## Anti-patterns from past conversions

General, from the Python and autocache ports; no artefact of those ports exists in this tree. Each row names where the same class of bug would bite this code.

| Pattern | Where it would bite here | Rust countermeasure |
| --- | --- | --- |
| Truthiness and coercion | `validateConfig` never checks `enabled`; `Number(msg.time)` accepts strings; `?? 0` only covers null. | Typed newtypes and explicit `Option`; parse at the wire boundary. |
| Exceptions as control flow | `ControlError` for cooldowns, `error.status` on `apiRequest` errors, `EINVALIDSTATE` for state. | An enum per failure class, matched exhaustively. |
| Stringly-typed config | `adapter.name`, `handler`, `logging.level`, `routing.delivery`. | Parse into enums at load time, not at use time. |
| Implicit `this` and default arguments | `Relay` deps defaults, `now = Date.now` passed unbound (works today because it is called as a function). | Explicit struct fields and injected trait objects; no method values passed as callbacks. |
| Date arithmetic | Confirm, cooldown, replay, shutdown, and log timestamps all use `Date.now()`. | One clock abstraction per unit; never subtract a `SystemTime` from an `Instant`. |
| Integer width and overflow | `Number.isInteger` accepts values above 2^53; byte caps and queue lengths are floats in principle. | `u64`/`usize` with checked conversions at parse, saturating at the cap. |
| Mutation shared across async boundaries | `Relay.projects` is replaced during discovery while `handleMessage` runs; `state` is mutated and written from `setPin` inside a command. | One owner task for state, or a mutex with a write-through API; no lock held across an await. |
| Silent fallbacks that hide a misconfiguration | `createSSEParser` drops bad JSON, `Logger` swallows rotation errors, `normalizeState` accepts a bad shape, `safeLog` swallows everything. | Keep the fallback but emit one log line with a stable field, and count it in tests. |

## Evidence gaps

- Test counts drifted during the port: `00-overview.md` said 69 and this file said 89 at the time of the audit; `npm test` reported 94 at that time. The inbound acceptance gate is the 86 `rust\crates\inbound` tests plus the wire and adapter fixtures, not a count in prose.
- `01-phone-continuation.md` originally said every publish includes a "heartbeat push"; the heartbeat is a log line, which the status report shows. There is no heartbeat publish call in the tree.
- `03` step 2 said "compare wire bytes with the current client", but there are two clients: `publishNtfy` posts headers to `/{topic}`, `ntfy-client.js` posts JSON to the base URL and PUTs files with query parameters. `rust\crates\ntfy` implements all three and tests each style.
- `03` says "3400 UTF-8 bytes inline"; the plugin also has `MAX_MESSAGE_BYTES` 4096 and `boundQuestionMessage` with a `(truncated)` marker. `core::text` carries the 3400 ack budget; `ntfy::header` carries the plugin 1024/4096 caps.
- `04` covers the clock choice (row 10) and the wire format (row 11), but has no row for duplicate-key policy or unknown-field strictness. Config parses through `serde_json::Value`, so a duplicate key takes the last value; typed state rejects a duplicate field.
- `03` section 6 says the OpenCode calls go through HTTP directly. The Rust adapter retries no-status and 5xx twice, 1 s then 2 s, matching the Node classifier; the parser is gone.
- The 94 Node tests cover inbound, control, and the inbox prune. Outbound turn, file, gap, and backfill behavior is pinned by the 104 tests in `rust\crates\outbound` and the app MCP test; outbound and the MCP tool are live, and the publisher plugin is deleted.
