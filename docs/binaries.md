# 1. ntfy binaries and checksums

## 1.1 GitHub 504 workaround

Direct release downloads returned 504 through the network edge:

```
https://github.com/binwiederhier/ntfy/releases/download/v2.37.2/...
HTTP/1.1 504 Gateway Time-out
x-github-edge-region: fra
```

The GitHub API asset endpoint worked. Resolve asset IDs first:

```powershell
$j = Invoke-RestMethod -Headers @{ "User-Agent" = "opencode" } `
  -Uri "https://api.github.com/repos/binwiederhier/ntfy/releases/latest"
$j.tag_name
$j.assets | Select-Object id,name,size
```

Download an asset by ID:

```powershell
New-Item -ItemType Directory -Force resources | Out-Null
curl.exe -sS -L -H "Accept: application/octet-stream" -H "User-Agent: opencode" `
  -o resources\ntfy_2.37.2_windows_amd64.zip `
  "https://api.github.com/repos/binwiederhier/ntfy/releases/assets/707826293"
```

On Linux the same works with `curl -L -H "Accept: application/octet-stream"`.

## 1.2 Checksums

```
72a1ad4f8976ee9986a77670d68e3d58a5b9a6b9fbbcaf43fc6dd24255b2d44c  ntfy_2.37.2_windows_amd64.zip
e67f53d7fbed529b83faf3993b584110921ef3567b0ac0b033c6910879a4ffd1  ntfy_2.37.2_linux_amd64.tar.gz
```

The container build pins the Linux value as `NTFY_SHA256` in `container\Dockerfile` and checks the tarball before it unpacks it. An arm64 checksum is in the release `checksums.txt`; the image targets amd64 only.

Windows verification:

```powershell
(Get-FileHash resources\ntfy_2.37.2_windows_amd64.zip -Algorithm SHA256).Hash.ToLower()
```

Linux verification:

```bash
echo "e67f53d7fbed529b83faf3993b584110921ef3567b0ac0b033c6910879a4ffd1  ntfy_2.37.2_linux_amd64.tar.gz" | sha256sum -c -
```

## 1.3 Windows binary

```powershell
Expand-Archive -Path resources\ntfy_2.37.2_windows_amd64.zip -DestinationPath resources -Force
.\resources\ntfy_2.37.2_windows_amd64\ntfy.exe --version
```

```
ntfy version 2.37.2
```

## 1.4 Linux binary for images

The build fetches this file itself. `container\Dockerfile` downloads `ntfy_2.37.2_linux_amd64.tar.gz` in a build stage, checks it against `NTFY_SHA256`, and copies the unpacked binary into the image. A version change updates `NTFY_VERSION` and `NTFY_SHA256` together.

To check the pin by hand (1.1 is the workaround when the download returns 504):

```powershell
New-Item -ItemType Directory -Force temp | Out-Null
curl.exe -sS -L -o temp\ntfy_2.37.2_linux_amd64.tar.gz `
  "https://github.com/binwiederhier/ntfy/releases/download/v2.37.2/ntfy_2.37.2_linux_amd64.tar.gz"
(Get-FileHash temp\ntfy_2.37.2_linux_amd64.tar.gz -Algorithm SHA256).Hash.ToLower()
```

The binary is statically linked, so the image needs no runtime libraries, no ca-certificates, and no tzdata. The base is Alpine 3.24.4, pinned by digest in `container\Dockerfile`.
