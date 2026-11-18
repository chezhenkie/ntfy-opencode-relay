# Start, stop, and memory audit

Scope: every file the relay, its wrappers, and its tooling write, plus the start/stop ownership path. Method: measured sizes and mtimes under `~/.local/state/opencode`, `logs\`, and `temp\`; read the writers (`logger.rs`, `start-relay.ps1`, `start-rust-relay.ps1`, `relay-watchdog.ps1`, `startup-check.ps1`, `state.rs`, `runner.rs`); traced restart and lock behavior from the scripts, not from memory.

## Disk evidence

| Artifact | Size at audit | Write rate | Bound now |
| --- | --- | --- | --- |
| `ntfy-relay.log` | 415390 B | about 200 KB/day | 5 MB, one `.1` copy |
| `logs\relay-start.log` | 6414 B | one line per start attempt | none |
| `logs\relay-watchdog.log` | 1777 B | one line per restart | none |
| `logs\startup-check.log` | 27810 B | about 18 KB/day | none |
| `logs\relay-rust-stdout.log` | 0 B | truncated per start | one run |
| `logs\relay-rust-stderr.log` | 0 B | truncated per start | one run |
| `ntfy-relay-state.json` | 817 B | 500 ids per topic, 512 rows | bounded at load |
| `ntfy-relay-state.json.corrupt-*` | 7 B, one file | one per corrupt load | none |
| `ntfy-relay-seen.jsonl` | 2891 B | 15 min or 8192 ids | 1 MiB, compacted |
| `.ntfy-inbox` | not present | per attachment | 200 MB per project, 7 days |
| `temp\` staged binaries | 11 MB per copy | one per swap | none, manual |

## Findings

| ID | Finding | Severity | Decision |
| --- | --- | --- | --- |
| S-01 | `relay-start.log` appends forever. At one line per start attempt it is slow, but a stuck watchdog or a start loop grows it without limit. | low | fix 0.7.9 |
| S-02 | `relay-watchdog.log` appends forever, one line per restart. A crash loop leaves the only restart signal unbounded. | low | fix 0.7.9 |
| S-03 | `startup-check.log` appends forever at about 18 KB/day, and it is written on every logon. | low | fix 0.7.9 |
| S-04 | The launchers truncate `relay-rust-std{out,err}.log` per start with `cmd >`, so they are bounded by one run. A long run with repeated stderr output is the only growth path; the daemon log already carries the same failures. | none | accept |
| S-05 | A corrupt state load quarantines the file to `.corrupt-<unix>` and nothing ever deletes it. Each event is small, but a repeating corrupt start leaves one file per start. | low | fix 0.7.9 |
| S-06 | The daemon log rotates at 5 MB into one `.1` copy. Measured rate gives about 50 days of retention and a 10 MB ceiling. | none | accept |
| S-07 | State, seen journal, and inbox are bounded at write or load: 500 ids per topic, 512 rows, 1 MiB journal, 200 MB per project, 7-day inbox prune. | none | accept |
| S-08 | `temp\` holds one staged binary per swap, 11 MB each, gitignored. The current copy is the rollback for the live version, so it is kept until the next release. | none | accept, prune at release |
| S-09 | The relay has no owning handle. `start-rust-relay.ps1` spawns through WMI `Win32_Process.Create`, by design, so the process outlives the wrapper and nothing kills a child tree on stop. The watchdog is the only restart path. | medium | bridge |
| S-10 | Reconcile is the lock pid plus `Test-RelayProcess`. There is no port, health, or ownership check at start, and no adopted-child list. | medium | bridge |
| S-11 | After a reboot the relay can start before the OpenCode registration is valid, exit, and wait for the watchdog tick. `FAIL` then `PASS` in `relay-start.log` is this race. | low | bridge |

## 0.7.9 fix set

- S-01, S-02, S-03: one helper in `relay-config.ps1` appends a stamped line and rotates at 1 MiB into one `.1` copy; the four local `Write-Log` functions delegate to it. Rotation happens before the append and never fails a start; a failed rotate still writes the line.
- S-05: `prune_corrupt_backups` in `core::state` removes `<state>.corrupt-*` files older than 7 days, keyed by the numeric suffix, at startup after the state load. A quarantine in the same start creates a fresh backup, so the new file is never pruned. The count is logged once when it is non-zero.
- Docs: the state and log row in `operations.md` records the quarantine TTL and the wrapper log cap.
- Version: 0.7.9, patch. No config key, no daemon contract change.

Verification: PowerShell parse check on the four scripts; a rotation test that writes past 1 MiB into a scratch path and asserts one `.1` copy; a core test that prunes an 8-day backup, keeps a fresh one, and ignores an unrelated name; the full Linux and Windows Rust suites; Node tests; TLD and link checks.

## Bridge-bound rows

S-09 to S-11 are the bridge contract, not a patch to the current wrappers.

- Ownership: one bridge per environment, the only process without a TTL. Windows owns children through a job object with kill-on-close; Linux uses cgroup v2 with `KillMode=control-group`. The relay, the MCP servers, and the container are children, so a bridge stop leaves no orphan.
- Reconcile on start: read the lock pid, verify the image and command line, adopt a live match, kill a mismatch, and re-check that the container is healthy before the relay starts. Alpha is elected by what is alive, not by a file.
- Reboot order: container healthy, registration valid, relay last; stop in reverse. The current startup check starts the relay first and is the known race.

## Accepted costs while the bridge does not exist

- A hard stop leaves the lock behind; the next start replaces the dead pid.
- The watchdog is a logon task, so nothing restarts the relay while no user session exists.
- The relay can lose one boot attempt to the registration race and recover on the next watchdog tick.
- Launcher logs keep at most 2 MiB each after 0.7.9; older lines are dropped by rotation.
