# End-to-end smoke tests

One Node script, `smoke.mjs`, exercises the live ntfy server over its HTTP API: the anonymous health endpoint, the deny-all gate, header and JSON publish, poll read-back, SSE delivery, and an attachment round trip.

It needs no npm packages and speaks only to the server. The container, host route, and relay process are covered by `ntfy-relay status` and `ntfy-deploy status`.

## Run

Windows, from the repository root:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\e2e-smoke-tests\run-windows.ps1
```

Linux:

```bash
bash e2e-smoke-tests/run-linux.sh
```

Both wrappers accept `--base-url`, `--token-file`, `--topic`, and `--timeout`; on Windows the parameters are `-BaseUrl`, `-TokenFile`, `-Topic`, and `-TimeoutMs`. `node e2e-smoke-tests/smoke.mjs` runs the same suite directly.

The base URL defaults to `http://127.0.0.1:8810`. The token file defaults to `$NTFY_TOKEN_FILE`, else `$NTFY_RELAY_HOME/ntfy-token.txt`, else `%LOCALAPPDATA%\ntfy-relay\ntfy-token.txt` on Windows, else `bin/ntfy-relay/ntfy-token.txt` under the repository. On Linux the install root is usually set with `NTFY_RELAY_HOME`.

To test the phone route instead of loopback, pass the name the phone uses:

```bash
bash e2e-smoke-tests/run-linux.sh --base-url https://ntfy.example.ts.net
```

## Checks

| check | proves |
|---|---|
| `health-anonymous` | `/v1/health` answers 200 with `healthy:true` and needs no token |
| `publish-denied-anonymous` | an untokened publish returns 403 and stores nothing |
| `read-denied-anonymous` | an untokened poll returns 401 or 403 |
| `publish-header` | token publish to `/<topic>` with `Title`, `Tags`, and a text body returns one 12-character message id |
| `read-back` | `/<topic>/json?poll=1&since=` returns that id with the same body, title, and `oc-out` tag |
| `sse-live` | `/<topic>/sse?since=` opens, a later publish arrives as a message frame with the same body |
| `publish-json` | JSON publish to the base URL returns a message that reads back with the same title; the priority is reported but not compared |
| `attachment-roundtrip` | a `PUT` upload stores the file, and the attachment URL returns the same bytes |

## Options

| option | environment | default |
|---|---|---|
| `--base-url` | `NTFY_BASE_URL` | `http://127.0.0.1:8810` |
| `--token-file` | `NTFY_TOKEN_FILE` | relay root `ntfy-token.txt` |
| `--topic` | | `e2e-smoke` |
| `--timeout` | | `10000` ms per request |

## Notes

The suite publishes to `e2e-smoke` with the `oc-out` tag. The relay ignores that tag and subscribes only to project folder names, so the traffic never reaches a session. Messages and attachments expire with the server cache, 72 hours.

The token is read once and never printed. Exit code 0 means every check passed, 1 means at least one failed, 2 means the setup or an argument was wrong.
