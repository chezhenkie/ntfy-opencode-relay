# ntfy over a private network - setup manual

Self-hosted ntfy 2.38.2 without Google, reachable only over a private network, on Windows through Podman or on Ubuntu natively. A Rust relay carries OpenCode turns and files to the phone and phone replies back to the session.

## Start

- [Zero Google baseline](zero-google.md): the three paths that can leak to Google or ntfy.sh, and how each is disabled. Read it before installing anything.
- [Windows path: Podman](windows-podman.md): Podman machine, image build, container, local verification.
- [Ubuntu/Xubuntu path](ubuntu.md): native binary with systemd, or rootless Podman with the same image.
- Container engine: the image builds from one standard Dockerfile and the release tarball is a docker-archive; `podman load` and `docker load` restore the same image. The deploy CLI currently drives Podman.
- [Startup, stop and restart](startup.md): one page for the check, the start, the stop, the restart, and the logs.

## Network and TLS

- [Private network](network.md): the private network, the loopback binding, the TLS terminator, the base URL matrix.
- [TLS front end and firewall surface](https.md): the HTTPS switch, portproxy removal, firewall surface, rollback.

## Client

- [Android transport and battery](android.md): F-Droid build, WebSockets, battery, test procedure.
- [ntfy on Android - user manual](phone-app-user-manual.md): the app from the phone side, subscriptions, and replies.
- [Switching networks - user manual](switching-networks-user-manual.md): what changes, and what does not, when the host moves.

## Run and repair

- [Operations](operations.md): lifecycle, tokens, notifications, the relay, the Rust smoke check, sleep.
- [Troubleshooting](troubleshooting.md): symptom to cause to fix.
- [Binaries and checksums](binaries.md): ntfy 2.38.2 downloads for Windows and Linux, with sums.
- [Directory tree](tree.md): repository layout and crate boundaries.
- [Open work](open-work.md): what remains before the first public release, and what is deferred.

## Reference

- [Specsheet](specsheet/00-overview.md): the design record, 00 to 12, covering the phone continuation, message routes, the relay merge and its decisions, JS to Rust pitfalls, the crate split, backfill, MCP and the command layer, permission replies, deployment and network switching, the auth bootstrap, and the control path.
- [Bridge and relay: relation](bridge-relay-relation.md): which project owns which process, and where the reboot order lives.

## Flow diagrams

- [Inbound and outbound](github-page-html-files/inbound-outbound-message-flow.html)
- [Commands and tags](github-page-html-files/commands-and-tags-flow.html)
- [Ask and permission](github-page-html-files/ask-and-permission-flow.html)

## Environment used by the examples

- Windows 11, PowerShell 5.1, Podman 5.8.3 with machine `podman-machine-default`, rootless, 4 CPU, 2 GiB.
- A private network client, node `ntfy-host`, private address `100.64.0.10`, private DNS `ntfy.example.ts.net`.
- ntfy 2.38.2, linux/amd64 in the image and windows/amd64 native, tag `localhost/ntfy:server` with `localhost/ntfy:latest` as an alias.
- Host port 8810 maps to container port 8080 on loopback; the container runs as uid 1000 with a read-only root, tmpfs `/tmp`, no added capabilities, and a 10 MiB log cap.
- The checkout is `C:\work\ntfy-proj` and the placeholder network values replace your own.
