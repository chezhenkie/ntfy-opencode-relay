# 3. Ubuntu/Xubuntu path

## 3.1 Native binary with systemd

Install dependencies and download the release:

```bash
sudo apt update
sudo apt install -y curl tar
cd /tmp
curl -fL -o ntfy_2.37.2_linux_amd64.tar.gz \
  https://github.com/binwiederhier/ntfy/releases/download/v2.37.2/ntfy_2.37.2_linux_amd64.tar.gz
echo "e67f53d7fbed529b83faf3993b584110921ef3567b0ac0b033c6910879a4ffd1  ntfy_2.37.2_linux_amd64.tar.gz" | sha256sum -c -
tar xzf ntfy_2.37.2_linux_amd64.tar.gz
```

On arm64 use `ntfy_2.37.2_linux_arm64.tar.gz` and its checksum from `checksums.txt`.

Install the binary and directories:

```bash
sudo install -m 755 ntfy_2.37.2_linux_amd64/ntfy /usr/local/bin/ntfy
sudo useradd --system --home-dir /var/lib/ntfy --shell /usr/sbin/nologin ntfy
sudo mkdir -p /etc/ntfy /var/cache/ntfy/attachments
sudo chown -R ntfy:ntfy /var/cache/ntfy
```

Minimal `/etc/ntfy/server.yml`, zero Google:

```yaml
base-url: http://xubuntu-host.example.ts.net:8080
listen-http: ":8080"

# Zero Google: no iOS/APNS relay, no Firebase key.
upstream-base-url: ""

# Auth: same deny-all baseline as the container image.
auth-file: /var/cache/ntfy/auth.db
auth-default-access: deny-all
enable-signup: false

cache-file: /var/cache/ntfy/cache.db
cache-duration: 72h
attachment-cache-dir: /var/cache/ntfy/attachments
attachment-expiry-duration: 72h
```

Write it with an editor, then fix ownership:

```bash
sudo chown ntfy:ntfy /etc/ntfy/server.yml
sudo chmod 640 /etc/ntfy/server.yml
```

Create the `phone` user with the `ntfy user add`, `ntfy access phone "*" rw`, and `ntfy token add` commands in [operations 7.2](operations.md#72-auth-and-tokens), run directly on the host instead of through `podman exec`; `auth.db` lands under `/var/cache/ntfy`. The wildcard row covers every topic, and anonymous access stays denied.

systemd unit `/etc/systemd/system/ntfy.service`:

```ini
[Unit]
Description=ntfy server
After=network-online.target
Wants=network-online.target

[Service]
User=ntfy
Group=ntfy
ExecStart=/usr/local/bin/ntfy serve
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now ntfy
systemctl status ntfy --no-pager
curl -sS http://localhost:8080/v1/health
```

```
{"healthy":true,"role":"active"}
```

## 3.2 Podman on Ubuntu

```bash
sudo apt update
sudo apt install -y podman
podman --version
```

Copy the `container` directory to the Ubuntu host. The image build fetches the pinned ntfy release and checks its checksum, so the directory holds no binary and the host needs outbound HTTPS to GitHub. `server.yml` holds only baked keys; the deployment base URL and proxy flag come from `ntfy-deploy.json` in the relay install root. Then build:

```bash
cd ~/ntfy-proj
podman build --format docker -t localhost/ntfy:2.37.2 -t localhost/ntfy:latest container
```

Run rootless. `ntfy-deploy mode` writes the config from the detected path and applies it; `ntfy-deploy apply` runs an existing config. The manual equivalent:

```bash
podman run -d --name ntfy -p 8810:8080 -v ntfy-cache:/var/cache/ntfy --restart unless-stopped --read-only --tmpfs /tmp --cap-drop=ALL --security-opt no-new-privileges -e NTFY_BASE_URL=http://192.168.1.20:8810 -e NTFY_BEHIND_PROXY=false localhost/ntfy:2.37.2
podman ps --format '{{.Names}} {{.Status}} {{.Ports}}'
```

Keep the container running without an active login and restore it on boot:

```bash
sudo loginctl enable-linger $USER
systemctl --user enable --now podman-restart.service
```

Unlike the Windows machine, rootless Podman on Linux binds published ports on all interfaces, so no portproxy is needed.

Verify:

```bash
curl -sS http://localhost:8810/v1/health
podman logs --tail 5 ntfy
```
