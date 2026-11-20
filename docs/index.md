# ntfy over a private network - setup manual

This folder is the manual: file names carry no numbers, and the chapters are read in the order of the table below. Section numbers inside the chapters (1.1 to 9.x) are stable, so prose references such as section 5.5 keep resolving. The numbered part scheme exists only in `specsheet\`, the folder next to this file.

## Scope

Install and run ntfy 2.37.2 in three ways, all reachable over a private network:

- Windows: Podman machine (WSL2), image built locally.
- Ubuntu/Xubuntu: native binary with a systemd unit.
- Ubuntu/Xubuntu: rootless Podman with the same image.

The setup is deliberately zero Google: no Firebase Cloud Messaging, no Google Play services in the Android app, no ntfy.sh relay. Read [the Zero Google baseline](zero-google.md) before installing anything.

## Table of contents

| chapter | covers |
|---|---|
| [Zero Google baseline](zero-google.md) | the three paths that can leak to Google or ntfy.sh, and how each is disabled |
| [1. ntfy binaries and checksums](binaries.md) | ntfy 2.37.2 downloads, checksums, Windows and Linux binaries |
| [2. Windows path: Podman](windows-podman.md) | Podman machine, image build, run, local verification |
| [3. Ubuntu/Xubuntu path](ubuntu.md) | native binary with systemd, rootless Podman |
| [4. Private network](network.md) | private network, loopback binding, TLS terminator, base-url matrix |
| [5. HTTPS switch and firewall surface](https.md) | the HTTPS switch, portproxy removal, firewall surface, rollback |
| [6. Android transport and battery](android.md) | F-Droid client, WebSockets, battery, test procedure |
| [7. Operations](operations.md) | lifecycle, tokens, startup check, notifications, relay, Rust smoke, sleep |
| [8. Troubleshooting](troubleshooting.md) | symptom to cause to fix |
| [9. Directory tree](tree.md) | repository layout and crate boundaries |
| [10. Open work](open-work.md) | what remains before the first public release, and what is deferred |

Chapters 4 and 5 describe a generic private network and TLS terminator. Any VPN or LAN that reaches the host on 443 works; the ntfy settings do not change.

Related: [README](../README.md), [specsheet](specsheet/00-overview.md), [phone app manual](phone-app-user-manual.md), [switching networks](switching-networks.md), [bridge and relay relation](bridge-relay-relation.md).

Flow diagrams: [front page](index.html) with [inbound and outbound](inbound-outbound-message-flow.html), [commands and tags](commands-and-tags-flow.html), and [ask and permission](ask-and-permission-flow.html).

## Environment used by the examples

- Windows 11, PowerShell 5.1
- Podman 5.8.3, machine `podman-machine-default`, rootless, 4 CPU, 2 GiB
- A private network client, node `ntfy-host`, private address `100.64.0.10`, private DNS `ntfy.example.ts.net`
- ntfy 2.37.2, linux/amd64 in the image, windows/amd64 binary for native Windows use
- Image tags `localhost/ntfy:2.37.2` and `localhost/ntfy:latest`
- Host port 8810 maps to container port 8080
- Container runs as uid 1000 with a read-only root filesystem, tmpfs `/tmp`, and no added capabilities

Examples use `ntfy.example.ts.net` for the private DNS name, `100.64.0.10` for the private address, and `C:\work\ntfy-proj` for the checkout. Substitute your own.
