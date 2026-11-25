# Auth bootstrap

Status: implemented in 0.14.0. `ntfy-auth apply|status` creates the one principal, its wildcard row, and the token file, and changes nothing on a second run. The specsheet is binding; operations 7.2 is the operator page.

## Why it exists

Per-topic ACL rows made every new subscription a host step: a topic that was never granted returned `403`. The wildcard row removes that step, and one principal with one token replaces the three users and two token files. Auth state lives in `auth.db` inside the container and changes only through the server CLI, so the bootstrap needs a small tool that runs those commands in order and writes the token once.

## Files

- `ntfy-auth.json`: config, in the relay install root, never in the OpenCode directories.
- `ntfy-token.txt`: the only token, written `0600` on Unix. The relay and each MCP process read it; the app keeps its copy.
- The relay start does not require the file. A missing or short file logs one WARN, the daemon keeps its lock and its control endpoint, and it attaches the token within a second of the file appearing, without a restart.
- No password file. `phone` receives a random password at creation that is never stored, printed, or used. The web UI has no login.

## Config

```json
{
  "version": 1,
  "container": "ntfy",
  "user": "phone",
  "topic": "*",
  "permission": "rw",
  "tokenLabel": "owner",
  "tokenFile": "ntfy-token.txt"
}
```

- Strict JSON: unknown fields, another version, and a missing field fail with exit 2.
- `container`: the running container name, 1 to 64 characters of letters, digits, dot, dash, or underscore.
- `user`: 1 to 64 characters of letters, digits, dash, or underscore. A wildcard user is rejected.
- `topic`: 1 to 64 characters of letters, digits, dash, underscore, or `*`. `*` grants every topic, including one that does not exist yet.
- `permission`: `rw`, `read`, or `write`. `deny` is rejected: bootstrap with deny is a mistake, not a policy.
- `tokenLabel`: the label shown by `ntfy token list`, 1 to 64 characters.
- `tokenFile`: a bare file name in the install root. Path separators, `..`, and a drive prefix are rejected, so the file cannot leave the root.

Default path: `NTFY_RELAY_HOME`, else the directory holding `ntfy-auth.exe`, plus `ntfy-auth.json`. No home-directory fallback.

## CLI

```powershell
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" apply
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" apply --dry-run
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" status
& "$env:NTFY_RELAY_HOME\ntfy-auth.exe" apply --config C:\path\to\ntfy-auth.json
```

- `apply` reads the state, then runs only the commands whose result is absent.
- `--dry-run` prints `plan:` lines for the commands it would run and changes nothing.
- `status` reads the same state and creates nothing.
- Exit codes: 0 ok, 2 invalid config, 3 incomplete, 4 platform command failed.

Output never contains a token, only the file name.

## Commands

The tool runs exactly these forms, directly, never through a shell:

```text
podman exec <container> ntfy user list
podman exec -e NTFY_PASSWORD <container> ntfy user add --role=user <user>
podman exec <container> ntfy access <user>
podman exec <container> ntfy access <user> <topic> <permission>
podman exec <container> ntfy token list <user>
podman exec <container> ntfy token add --label=<label> <user>
```

The password is generated in the tool and passed to the podman process through its environment, so it never appears in a command line. The string is 32 hex characters from `RandomState`, which is seeded from OS entropy, and it is dropped after the call.

## Apply sequence

1. `user list` and match the configured user exactly. `*` in that table is the anonymous principal, not a user.
2. If the user is absent, `user add`; nothing else can run yet.
3. `access <user>` and look for the exact permission and topic pair. If absent, `access <user> <topic> <permission>`.
4. `token list <user>` and read the token file.
5. Add a token only when the file is missing, unreadable, empty, malformed, or holds a value the server does not list. Write the returned token to the file, 32 characters plus a newline, `0600` on Unix.
6. A valid file token that the server lists is kept and no command writes.

A wiped token file is restored by one run. A stale file is replaced by a new token; old tokens stay until `tools\rotate-token.ps1 -User phone -RevokeOld` removes them. The tool never revokes.

## Status

`status` prints the container, the user, the access pair, and the token state, then `result: ok` or `result: incomplete`. The token state is `listed`, `file token not listed`, or `file missing`. Incomplete exits 3 and writes nothing.

## Boundaries

- `auth.db` is never edited on the host, and no second container is started on the cache volume.
- The tool reads no relay config, deploy config, or OpenCode file, and never restarts the relay. The relay does not run this tool either: it waits for the token file and attaches it on its own.
- It does not create a user with a password that anything uses, and it does not print a secret.
- Rotation, expiry, and revocation stay in `tools\rotate-token.ps1`, which is phone-only since 0.14.0.
- The relay stays the only publisher; this tool writes auth state and the token file, nothing else.

## Tests

The crate uses a fake `CommandRunner`; CI runs no podman. The suite covers config parsing and every rejected shape, a first apply that creates all three, a second apply with no mutating command, a kept token file, a restored missing file, a replaced stale file, dry run, status in each token state, a command failure, missing token output, an unwritable file, `0600` on Unix, the CLI table parsers, and the password shape.
