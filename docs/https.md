# 5. TLS front end and firewall surface

The walkthrough applies a TLS terminator on the host that proxies to `127.0.0.1:8810`. Any terminator that presents a certificate the phone trusts applies; the ntfy settings below stay the same.

## 5.1 What the switch changes (applied)

Before:

```
client -> http://ntfy.example.ts.net:8810
       -> portproxy 100.64.0.10:8810 -> [::1]:8810
       -> container :8080
```

After:

```
client -> https://ntfy.example.ts.net (443)
       -> TLS terminator
       -> http://127.0.0.1:8810 -> container :8080
```

Consequences:

- The URL loses the port: `https://ntfy.example.ts.net`.
- WebSockets become `wss://` automatically.
- `base-url` becomes the HTTPS name, `behind-proxy` becomes true.
- The VPN-facing portproxy is deleted; the loopback bridge remains.
- HTTP remains only on loopback between the terminator, the host, and the container.

## 5.2 Prerequisites

- A DNS name that the phone resolves on the private network.
- A certificate for that name, trusted by the phone.
- A TLS terminator that listens on 443 and proxies to `http://127.0.0.1:8810`.
- No public exposure feature. The server stays inside the private network.

A certificate for a private address is normally unavailable, so HTTPS requires the DNS name. The hostname is published in the Certificate Transparency ledger and cannot be removed; the message traffic stays private.

Check the certificate before changing anything:

```powershell
curl.exe -sS -o NUL -w "%{http_code}`n" https://ntfy.example.ts.net/
```

`200` means the terminator and the certificate are working. A TLS error means the name, certificate, or trust store does not match.

## 5.3 Enable and verify

Windows:

```
terminator :443 -> http://127.0.0.1:8810
curl.exe -sS https://ntfy.example.ts.net/v1/health
```

If the terminator rejects an IPv6 literal target, keep the loopback bridge. The minimal Windows portproxy is a loopback-only rule, `127.0.0.1:8810 -> [::1]:8810`. It adds no firewall rule and listens only on loopback. Elevated:

```powershell
netsh interface portproxy add v4tov6 listenaddress=127.0.0.1 listenport=8810 connectaddress=::1 connectport=8810
netsh interface portproxy show all
```

Then point the terminator at `http://127.0.0.1:8810` and verify:

```powershell
curl.exe -sS https://ntfy.example.ts.net/v1/health
```

The alternative is to remove `wslrelay` from the path: run ntfy natively on Windows, or use a terminator inside the Podman machine. Both avoid the loopback bridge entirely.

Ubuntu native and Podman:

```
terminator :443 -> http://127.0.0.1:8080   (native, or 8810 for Podman)
```

## 5.4 Config changes

The deploy config switches to proxy mode:

```json
{
  "version": 1,
  "mode": "proxy",
  "baseUrl": "https://ntfy.example.ts.net",
  "behindProxy": true,
  "port": 8810,
  "container": {
    "name": "ntfy",
    "image": "localhost/ntfy:server",
    "cacheVolume": "ntfy-cache"
  }
}
```

```powershell
ntfy-deploy mode tailscale
ntfy-deploy status
```

`mode tailscale` rewrites `mode`, `baseUrl`, and `behindProxy` in the existing `ntfy-deploy.json` from the detected MagicDNS name and applies it; the other keys keep their values. The file must already exist, because `mode` does not create it.

The auth keys from [section 2.3](windows-podman.md#23-build-context) stay unchanged. The terminator ends TLS, so `baseUrl` must be the HTTPS name; `behindProxy: true` makes ntfy read the `X-Forwarded-For` header the terminator sets. Without it, rate limiting and logs see `127.0.0.1` for every client.

`ntfy.server` may carry a reverse-proxy path prefix, for example `https://host/notify/`. The relay appends the topic to that base and sends the topic in the path, not in a JSON body, so the proxy must route the prefix to ntfy without stripping it.

The container is recreated with `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY`. The image and the cache volume are unchanged, and the relay config and token are not touched. On Ubuntu the same command applies; it publishes on all interfaces outside proxy mode and on loopback in proxy mode.

## 5.5 Remove the VPN-facing portproxy

Done on this machine after the HTTPS checks passed. Procedure, elevated:

```powershell
netsh interface portproxy delete v4tov6 listenaddress=100.64.0.10 listenport=8810
netsh interface portproxy show all
```

Expected: no entry for `100.64.0.10:8810`, only the loopback bridge `127.0.0.1:8810 -> [::1]:8810`. The portproxy listens only on loopback and is not reachable from the private network. The VPN entry is never restored; it would expose the port outside loopback.

## 5.6 Firewall surface after the switch

No inbound rule is added, and none is needed:

- The terminator accepts connections on the private interface and already has the client's inbound allow rule.
- Nothing listens on the LAN or WAN address unless the private interface is the LAN.
- The container port stays on loopback through `wslrelay`.
- The deleted portproxy was the only Windows-side listener on `100.64.0.10:8810`.
- The optional loopback bridge listens only on `127.0.0.1`; it is not reachable from the private network or the LAN.
- Access control is the private network itself, with the phone token as the only application credential. The terminator exposes only port 443.

Verify:

```powershell
netsh interface portproxy show all
Get-NetTCPConnection -LocalPort 443 -State Listen
Get-NetConnectionProfile | Select-Object Name,InterfaceAlias,NetworkCategory
```

Expected: no portproxy entry for `100.64.0.10:8810`, a 443 listener on the private address only, and the private interface in the Private profile. If the interface flips to Public, the client's allow rule can stop matching; set it back to Private.

## 5.7 Verification checklist

```powershell
curl.exe -sS https://ntfy.example.ts.net/v1/health
curl.exe -sS -o NUL -w "web ui http=%{http_code}`n" https://ntfy.example.ts.net/
curl.exe -sS -o NUL -w "https publish http=%{http_code}`n" -H "Authorization: Bearer <token>" -d "https publish test" https://ntfy.example.ts.net/ntfy-proj
```

Expected:

```text
{"healthy":true,"role":"active"}
web ui http=200
https publish http=200
```

Phone: change the server URL to `https://ntfy.example.ts.net`, no port. The app reconnects with `wss://`. Publish again with the app backgrounded.

## 5.8 Rollback

Stop the terminator or remove its 443 listener. Then switch back with `ntfy-deploy mode lan`, or set the deploy config by hand, for example `{"mode":"lan","baseUrl":"http://192.168.1.20:8810","behindProxy":false}`, and run `ntfy-deploy apply`. The container is recreated with the HTTP base URL and the VPN entry is removed. The old HTTP URL works again.

## 5.9 Current status

- Terminator active: `https://ntfy.example.ts.net` -> `http://127.0.0.1:8810`.
- Reboot behavior: the private network client starts automatically. The Podman machine does not start; `ntfy` returns after `podman machine start`.
- Loopback portproxy active: `127.0.0.1:8810 -> [::1]:8810`; no VPN-facing portproxy.
- `ntfy-deploy.json`: mode `proxy`, base URL `https://ntfy.example.ts.net`, `behindProxy: true`; container recreated and healthy.
- HTTPS health, publish, and a `wss://` subscribe/publish cycle verified.
- Android app URL: `https://ntfy.example.ts.net`, WebSockets.
- Browser notifications are available over HTTPS. Do not enable them in Chrome on Android: Chrome push uses FCM.
- The certificate covers the DNS name only, not the `100.x` address, so HTTPS requires the name.
- Ubuntu is unaffected: the terminator targets the published port directly, with the same deploy `baseUrl` and `behindProxy` values.
