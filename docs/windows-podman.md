# 2. Windows path: Podman

## 2.1 Install Podman

The winget package failed because its download hit the same GitHub 504:

```powershell
winget install --id RedHat.Podman --exact --silent --accept-package-agreements --accept-source-agreements --disable-interactivity
```

```
Download request status is not success.
0x801901f8 : Gateway timeout (504).
```

Download the MSI through the API instead. Release `podman-container-tools/podman` v5.8.3, asset `podman-installer-windows-amd64.msi` ID `445914014`:

```powershell
New-Item -ItemType Directory -Force resources | Out-Null
curl.exe -sS -L -H "Accept: application/octet-stream" -H "User-Agent: opencode" `
  -o resources\shasums.txt `
  "https://api.github.com/repos/podman-container-tools/podman/releases/assets/445914062"
curl.exe -sS -L -H "Accept: application/octet-stream" -H "User-Agent: opencode" `
  -o resources\podman-installer-windows-amd64.msi `
  "https://api.github.com/repos/podman-container-tools/podman/releases/assets/445914014"
(Get-FileHash resources\podman-installer-windows-amd64.msi -Algorithm SHA256).Hash.ToLower()
```

```
2c5cffb2f023ae122d52d7f4b30bb677adb933fb7aa7ad708d3d544f8d6fe13a
```

Silent elevated install, UAC prompt follows:

```powershell
$p = Start-Process msiexec.exe -ArgumentList '/i','C:\work\ntfy-proj\resources\podman-installer-windows-amd64.msi','/qn','/norestart','/L*v','C:\work\ntfy-proj\resources\install.log' -Verb RunAs -Wait -PassThru
$p.ExitCode
```

```
0
```

Verify:

```powershell
where.exe podman
podman --version
```

```
C:\Users\you\AppData\Local\Programs\Podman\podman.exe
podman version 5.8.3
```

The installer appends the Podman directory to the user PATH. Shells started before the install do not see it:

```powershell
$env:Path += ";$env:LOCALAPPDATA\Programs\Podman"
```

The installer is kept in `resources\` for reinstall or repair.

## 2.2 Initialize the Podman machine

Podman on Windows runs containers in a WSL2 VM. This pulls the VM image, so run it detached with logs:

```powershell
Start-Process -FilePath "$env:LOCALAPPDATA\Programs\Podman\podman.exe" `
  -ArgumentList 'machine','init','--now' `
  -RedirectStandardOutput "C:\work\ntfy-proj\resources\machine-init.out.log" `
  -RedirectStandardError "C:\work\ntfy-proj\resources\machine-init.err.log" `
  -WindowStyle Hidden
```

Check state:

```powershell
podman machine list
podman info --format '{{.Host.Arch}} {{.Host.OS}} {{.Store.GraphDriverName}}'
```

```
NAME                     VM TYPE     CREATED         LAST UP             CPUS   MEMORY   DISK SIZE
podman-machine-default*  wsl         10 minutes ago  Currently running   4      2GiB     100GiB

amd64 linux overlay
```

The machine is rootless. Ports below 1024 would need `podman machine set --rootful`; this setup uses 8080 inside and 8810 on the host.

## 2.3 Build context

```
container\
  Dockerfile
  server.yml
  .dockerignore
```

The build fetches the pinned ntfy release itself, so the context holds no binary. A hand-downloaded copy is kept out of the context by `.dockerignore`:

```
ntfy
ntfy_*.tar.gz
```

Dockerfile:

```dockerfile
# Base review: Alpine 3.24.4, linux/amd64 manifest pinned by digest.
# 3.24 is the current stable branch; 3.20 is past end of support.
# No apk packages in either stage: BusyBox supplies wget, sha256sum, tar, and
# grep, and the static ntfy binary needs neither ca-certificates (no outbound
# TLS, empty upstream-base-url) nor tzdata (UTC logs).
FROM alpine:3.24.4@sha256:0f49f331d9212aff11b932863be4eb01cdf962969aa86dab7d1ee21ac6f5f52b AS fetch

# The release binary is fetched and checked in the build, so the build context
# holds no pre-downloaded file. Version, architecture, and checksum are pinned;
# the checksum is the linux/amd64 value from the release checksums.txt. The base
# digest above is the amd64 manifest, so amd64 is the claimed architecture;
# another one needs its own base digest and release checksum.
ARG NTFY_VERSION=2.37.2
ARG NTFY_ARCH=amd64
ARG NTFY_SHA256=e67f53d7fbed529b83faf3993b584110921ef3567b0ac0b033c6910879a4ffd1

RUN set -eux; \
    wget -q -O /tmp/ntfy.tgz "https://github.com/binwiederhier/ntfy/releases/download/v${NTFY_VERSION}/ntfy_${NTFY_VERSION}_linux_${NTFY_ARCH}.tar.gz"; \
    echo "${NTFY_SHA256}  /tmp/ntfy.tgz" | sha256sum -c -; \
    tar -xzf /tmp/ntfy.tgz -C /tmp --strip-components=1 "ntfy_${NTFY_VERSION}_linux_${NTFY_ARCH}/ntfy"

FROM alpine:3.24.4@sha256:0f49f331d9212aff11b932863be4eb01cdf962969aa86dab7d1ee21ac6f5f52b

# COPY --chmod avoids the second 76.6 MB layer that a separate chmod RUN created
# in the 166 MB alpine:3.20 image.
COPY --from=fetch --chmod=0755 /tmp/ntfy /usr/bin/ntfy
COPY server.yml /etc/ntfy/server.yml

RUN mkdir -p /var/cache/ntfy/attachments \
 && chown -R 1000:1000 /var/cache/ntfy

# Rootless Podman already maps container root to an unprivileged subuid; a named
# user is defense in depth. An existing cache volume must be chowned once.
USER 1000:1000
VOLUME /var/cache/ntfy
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
  CMD wget -q -O - http://127.0.0.1:8080/v1/health | grep -q '"healthy":true' || exit 1

ENTRYPOINT ["ntfy"]
CMD ["serve"]
```

The fetch stage keeps nothing in the final image. A wrong or missing `NTFY_SHA256` fails the build before the binary reaches a layer, and the build needs outbound HTTPS to GitHub for the pinned tarball.

`COPY --chmod=0755` writes the binary once. A separate `RUN chmod` copies it into a second layer, which is what made the `alpine:3.20` image 166 MB; the current image is 85 MB.

The base needs no packages. BusyBox provides `wget` and `grep` for the healthcheck, the static binary opens no outbound TLS connection (`upstream-base-url` is empty), and logs stay in UTC without tzdata.

`USER 1000:1000` needs a cache volume owned by that uid. A fresh volume inherits the directory from the image. An existing volume from the root image needs a one-time chown while the container is stopped:

```powershell
podman stop ntfy
podman rm ntfy
podman run --user 0:0 -v ntfy-cache:/data --name ntfy-volume-chown alpine:3.24.4 chown -R 1000:1000 /data
podman rm ntfy-volume-chown
```

server.yml:

```yaml
# ntfy server config - baked keys. Deployment values stay out of the image:
# ntfy-deploy supplies NTFY_BASE_URL and NTFY_BEHIND_PROXY at container
# creation.
listen-http: ":8080"

# Zero Google: no iOS/APNS relay, no Firebase key. Empty disables forwarding.
upstream-base-url: ""

# Auth: users and tokens live on the cache volume; anonymous access is denied.
auth-file: /var/cache/ntfy/auth.db
auth-default-access: deny-all
# No self-service accounts. Access is provisioned locally with the container CLI.
enable-signup: false

cache-file: /var/cache/ntfy/cache.db
cache-duration: 72h
attachment-cache-dir: /var/cache/ntfy/attachments
attachment-expiry-duration: 72h
```

`.dockerignore` keeps a hand-downloaded copy out of the context; the build fetches its own:

```
ntfy
ntfy_*.tar.gz
```

`base-url` and `behind-proxy` are deployment values, not image keys. `ntfy-deploy` supplies them as `NTFY_BASE_URL` and `NTFY_BEHIND_PROXY` when it creates the container; see [section 4.5](network.md#45-base-url-matrix) for the alternatives. A mode switch recreates the container and never rebuilds the image.

`cache-duration` is 72h. Messages older than three days are pruned; clients that reconnect within that window still get them. `attachment-expiry-duration` is also 72h; ntfy caps it at `cache-duration`, so files never outlive their message.

## 2.4 Build the image

```powershell
.\tools\build-image.ps1
```

The script reads the version pin from the Dockerfile, then builds `container\` in place and tags `localhost/ntfy:2.37.2` and `localhost/ntfy:latest`. Nothing has to be downloaded first; the build fetches the pinned release and checks its checksum. A plain `podman build` against `container` also works:

```powershell
podman build --format docker -t localhost/ntfy:2.37.2 -t localhost/ntfy:latest container
```

`--format docker` is required. With the default OCI format Podman drops `HEALTHCHECK`:

```
level=warning msg="HEALTHCHECK is not supported for OCI image format and will be ignored. Must use `docker` format"
```

Confirm:

```powershell
podman image inspect localhost/ntfy:2.37.2 --format '{{json .Config.Healthcheck}}'
```

```
localhost/ntfy:2.37.2   85 MB
localhost/ntfy:latest   85 MB
```

Most of the size is the 76.6 MB static binary; the base contributes 8.7 MB.

## 2.5 Run

`ntfy-deploy mode` detects the paths, writes `ntfy-deploy.json` in the relay root, and runs the container with the mode values. `ntfy-deploy apply` runs an existing config without changing it. The manual equivalent:

```powershell
podman run -d --name ntfy --restart unless-stopped --read-only --tmpfs /tmp --cap-drop=ALL --security-opt no-new-privileges -p 8810:8080 -v ntfy-cache:/var/cache/ntfy -e NTFY_BASE_URL=https://ntfy.example.ts.net -e NTFY_BEHIND_PROXY=true localhost/ntfy:2.37.2
podman ps --format '{{.Names}} {{.Status}} {{.Ports}}'
```

```
ntfy Up Less than a second (healthy) 0.0.0.0:8810->8080/tcp
```

The named volume `ntfy-cache` survives container replacement. `--read-only` is safe because the process writes only under `/var/cache/ntfy`; `/tmp` is a tmpfs. The process listens on 8080, so `--cap-drop=ALL` removes no needed capability.

## 2.6 Verify locally

```powershell
$token = "tk_..."   # user token, see section 7.2
curl.exe -sS http://localhost:8810/v1/health
curl.exe -sS -H "Authorization: Bearer $token" -d "hello from podman" http://localhost:8810/testtopic
curl.exe -sS -H "Authorization: Bearer $token" "http://localhost:8810/testtopic/json?poll=1"
curl.exe -sS -o NUL -w "web ui http=%{http_code}`n" http://localhost:8810/
```

```json
{"healthy":true,"role":"active"}
{"id":"FXL2Q0jAlZ8k","time":1794133719,"expires":1794176919,"event":"message","topic":"testtopic","message":"hello from podman"}
web ui http=200
```

`/v1/health` stays anonymous. Publish, poll, and the API return `403` without a token. The web UI page itself returns `200`; it has no usable login, because the only principal has no password.

Cache persistence:

```powershell
podman restart ntfy
Start-Sleep -Seconds 3
curl.exe -sS "http://localhost:8810/testtopic/json?poll=1"
```

The message is still returned, so `cache.db` on the volume is working.
