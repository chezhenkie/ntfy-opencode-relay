# 4. Private network

## 4.1 Requirements

The phone and the host join the same private network: a VPN or the local network. The host has a stable address for the phone to reach, and a DNS name when TLS is used.

Check the host address and the route on the host:

```powershell
ipconfig
ping <host-private-ip>
```

```bash
ip addr
ping <host-private-ip>
```

No public address, port forward, or third-party relay is needed. Nothing in this chapter exposes the server outside the private network.

## 4.2 Windows: Podman binds loopback only

`podman ps` shows `127.0.0.1:8810->8080/tcp` for a deploy-created container, and the compose file uses the same loopback default. The real Windows listener is IPv6 loopback only:

```powershell
Get-NetTCPConnection -LocalPort 8810 -State Listen | Select-Object LocalAddress,LocalPort,OwningProcess
```

```
LocalAddress LocalPort OwningProcess
::1                  8810          5076     (wslrelay.exe)
```

This is expected Podman machine behavior on Windows, not a misconfiguration. The published port is forwarded to `[::1]` by `wslrelay`.

The Windows design is therefore:

```
phone -> https://<private-dns-name> (443)
      -> TLS terminator
      -> v4tov6 loopback bridge 127.0.0.1:8810
      -> [::1]:8810 (wslrelay) -> container :8080
```

IPv6 appears only on loopback. It is internal plumbing and not an exposure; do not remove it. Do not change `wslrelay`, do not publish on `0.0.0.0`, and do not add a broad firewall rule. The compose file binds `${NTFY_BIND:-127.0.0.1}`; the `NTFY_BIND` override is for a LAN deployment and sits outside the proxy rule.

The private network client allows inbound on its own interface. No extra firewall rule is required, and none should be added unless a scoped rule is proven necessary. Check the interface profile; it must stay Private:

```powershell
Get-NetConnectionProfile | Select-Object Name,InterfaceAlias,NetworkCategory
```

The loopback bridge is the only portproxy in the TLS route: `127.0.0.1:8810 -> [::1]:8810`. A VPN-facing `100.64.0.10:8810` entry belongs to HTTP mode only and is removed when TLS is enabled.

The connect target must be `::1`, not `127.0.0.1`. A v4tov4 rule to `127.0.0.1:8810` resets connections because `wslrelay` does not listen on IPv4 loopback.

Local access on the host itself: `http://localhost:8810/v1/health` works through `[::1]:8810` and needs no portproxy. It is HTTP, loopback only, and not reachable from the private network. There is no `https://localhost` unless the terminator listens there. Browsers treat `http://localhost` as a secure context, so web notifications are allowed there, but Chrome still routes browser push through FCM.

## 4.3 TLS front end

A TLS terminator on the host proxies to the loopback bridge from 4.2, with a valid certificate for the private DNS name.

- The certificate covers the DNS name, not the numeric address. A certificate authority normally will not issue one for a private address.
- The terminator must accept the DNS name on 443 and forward HTTP to `127.0.0.1:8810`.
- If the terminator rejects an IPv6 literal target, keep the loopback bridge; it exists for that case.

With the terminator active, the deploy config uses the HTTPS name and proxy mode:

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

`ntfy-deploy mode tailscale` rewrites these keys in the existing config and recreates the container with `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY`; `ntfy-deploy status` shows the result. Then remove the VPN-facing portproxy as shown in [section 5.5](https.md#55-remove-the-vpn-facing-portproxy).

The terminator is reachable only from the private network. Do not enable a public exposure feature.

## 4.4 Linux: direct private reachability

Native ntfy listens on `0.0.0.0:8080`; Podman publishes `0.0.0.0:8810`. Both are reachable at the private address without extra forwarding.

If ufw is active, allow the private interface only:

```bash
sudo ufw allow in on <vpn-interface> to any port 8080 proto tcp
sudo ufw allow in on <vpn-interface> to any port 8810 proto tcp
sudo ufw status verbose
```

Test from another node on the private network:

```bash
curl http://xubuntu-host.example.ts.net:8080/v1/health
curl http://xubuntu-host.example.ts.net:8810/v1/health
```

## 4.5 base-url matrix

`base-url` controls web app links, attachment URLs, and iOS push topics. Set it to the address clients actually use. With zero Google there is no iOS push, but the other links still depend on it. `ntfy-deploy` passes the value as `NTFY_BASE_URL` at container creation; the relay accepts that host for attachments and still fetches them through its loopback server.

| Deployment | mode | base URL |
|---|---|---|
| Windows, localhost only | lan | http://127.0.0.1:8810 |
| Windows, HTTP over the VPN | vpn | http://100.64.0.10:8810 |
| Windows, TLS terminator | proxy | https://ntfy.example.ts.net |
| Ubuntu native, private address | none | http://192.168.1.20:8080 |
| Ubuntu Podman, private address | vpn | http://192.168.1.20:8810 |
| Ubuntu, TLS terminator | proxy | https://xubuntu.example.test |

Plain HTTP requires a loopback or private IP literal in `baseUrl`; a DNS name must use HTTPS. The HTTP port must equal the deploy `port`.

Change it with `ntfy-deploy mode` in the relay root, or edit `ntfy-deploy.json` and run `ntfy-deploy apply`. No image rebuild and no relay restart.
