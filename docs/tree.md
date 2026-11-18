# 9. Directory tree

```
ntfy-proj\
  README.md                       entry point: parts, quick start, checks
  CHANGELOG.md                    change list, versioned
  LICENSE                         MIT
  package.json                    type module, Node >= 20, tlds/links/secrets scripts
  audit\                          code, docs, triage, and start/stop reports
    audit-rust-relay-code.md      round 1 code audit
    r2-docs.md                    round 2 docs audit
    r3-plan.md                    round 3 triage, release waves
    startstop-memory.md           start/stop and memory audit, bridge-bound rows
    permission-reply-0.9.0.md     0.9.0 live permission matrix and gate
  rust\                           Rust workspace root
    Cargo.toml                    Rust workspace, resolver 2
    Cargo.lock                    pinned Rust dependency graph
    rust-toolchain.toml           stable toolchain with rustfmt and clippy
    assets\
      ntfy-icon.svg               icon source, square with slightly rounded corners
      ntfy.ico                    multi-size icon embedded in both Windows binaries
    build\
      win-resource.rs             shared build script: icon and version strings
    crates\
      core\
        Cargo.toml
        src\lib.rs, topic.rs, clock.rs, adapter.rs     topic rule, clock, SessionApi and PermissionApi
        src\config.rs                                  defaults, validation, path resolution
        src\state.rs, asks.rs, text.rs                 state file, ask registry, byte budgets
        src\link_safe.rs, tlds.rs                      link rewrite, generated IANA table
        tests\state.rs, asks.rs                        atomic write, quarantine, cursor window, ask codes
      inbound\
        Cargo.toml
        src\control\                                  table, parse, load, engine
        src\filter.rs, queue.rs, routing.rs           acceptance, bounded queue, target choice
        src\prompting.rs, permission.rs, prune.rs     prompt delivery, native replies, inbox retention
        src\session.rs                                title and short-id display
        tests\                                        control table, engine, delivery, permission reply flows
      ntfy\
        Cargo.toml
        src\                                          header, http, publish, sse, stream, attach
        tests\                                        wire parity and attachment fixtures
      opencode\
        Cargo.toml
        src\                                          api client, event types, SessionApi and PermissionApi impl
        tests\fixtures\                               captured project, session, event, question form, and message data
      outbound\
        Cargo.toml
        src\runner.rs                                 event subscription, bounded channel, backfill, push worker
        src\turn.rs, files.rs, topic.rs               turn buffering and recovery, auto-attach and send_file, session topic
        src\format.rs, permission.rs, publish.rs      push text, grace scheduling, wire selection
        src\scope.rs, log.rs                          project containment, logger seam
        src\seen.rs                                   durable event-id and message-id window across restarts
      app\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs, runner.rs, relay.rs              CLI, run loop, message handler
        src\supervisor.rs                             restart policy for the SSE and outbound tasks
        src\mcp.rs                                    stdio MCP server and send_file tool
        tests\mcp.rs                                  spawned stdio handshake, tool contract, fake publish
        src\api_client.rs                             rediscovering OpenCode client
        src\sse.rs, discovery.rs                      subscription loop, project map
        src\store.rs, lock.rs, logger.rs, status.rs   state writer, lock, rotating log, checks
        src\util.rs                                   clock, clip, jitter, name helpers
      deploy\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs                                   ntfy-deploy apply|status|mode CLI
        src\config.rs, platform.rs                    deploy config rules, publish and portproxy plans
        src\deploy.rs, status.rs, runner.rs           apply, health wait, status, command seam
        src\mode.rs                                   interface detection, path menu, config write
      auth\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs                                   ntfy-auth apply|status CLI
        src\config.rs, auth.rs, runner.rs             config rules, bootstrap sequence, command seam
        src\test_support.rs                           fake container CLI and temp root
  .github\
    workflows\ci.yml              Node checks and Rust matrix on ubuntu and windows
  docs\
    index.md                manual contents, scope, environment
    index.html              front page for the three flow pages
    assets\front-page.png   front page screenshot used by the README
    phone-app-user-manual.md phone setup, commands, tags
    switching-networks.md   switching Tailscale, Nord Meshnet, and LAN
    inbound-outbound-message-flow.html  inbound and outbound flow diagrams
    commands-and-tags-flow.html  phone commands, routing and reply tags
    ask-and-permission-flow.html  one permission ask, grace, phone reply
    zero-google.md          the three leak paths, and how each is disabled
    binaries.md             ntfy binaries and checksums
    windows-podman.md       Windows Podman route: machine, image, run
    ubuntu.md               native systemd and rootless Podman on Ubuntu
    network.md              private network, loopback binding, TLS terminator, base-url matrix
    https.md                HTTPS switch, portproxy removal, firewall, rollback
    android.md              F-Droid client, WebSockets, battery, tests
    operations.md           lifecycle, tokens, startup check, notifications, relay, Rust smoke
    troubleshooting.md      symptom, cause, fix
    open-work.md            remaining work before publication
    tree.md                 this file
    specsheet\
      00-overview.md              system overview and part index
      01-phone-continuation.md    phone continuation design
      02-message-routes.md        both message routes, explained
      03-rust-relay-merge.md      Rust merge plan, phase status, cutover
      04-rust-relay-decisions.md  decision log for the merge
      05-js-to-rust-pitfalls.md   behavioral pitfalls for the port
      06-rust-crate-split.md      agreed process and crate split
      07-reconnect-backfill.md    Rust-only event gap recovery
      08-comms-and-mcp.md         comms surfaces, MCP topology, shared crates
      09-permission-reply-routing.md  phone permission replies bound to the ask
      10-deploy-mode.md               file ownership, relay root, VPN mode switch
      11-auth-bootstrap.md            one principal, wildcard row, token file
  container\
    Dockerfile                    alpine 3.24.4, non-root, baked server.yml, fetch stage for ntfy
    server.yml                    ntfy config; base-url and behind-proxy come from ntfy-deploy
    .dockerignore                 excludes hand-downloaded ntfy copies
  relay-windows\
    start-relay.ps1               single-instance wrapper, WMI spawn, PASS/FAIL log
    relay-config.ps1              binary, config, state, lock, log, server resolution
    relay-watchdog.ps1, .vbs      five-minute liveness check, silent while alive
    set-relay-watchdog.ps1        creates or repoints the scheduled task
    ntfy-relay-control.example.json  control table example
    ntfy-deploy.example.json      deployment config example
    ntfy-auth.example.json        auth bootstrap config example
  tools\
    build-image.ps1               builds the image from the tracked context
    startup-check.ps1             machine, container, TLS terminator, portproxy, HTTPS, relay
    rotate-token.ps1              two-phase token rotation, probe publish, temporary expiry
    private-net-portproxy.ps1     rollback for the retired VPN-facing portproxy
    check-doc-links.mjs           relative links and ASCII over every tracked markdown file
    link-safety\build-tlds.mjs    writes the Rust TLD table
    link-safety\tlds.txt          IANA TLD source list
  e2e-smoke-tests\.gitkeep        reserved for future test tooling
  bin\                            relay root: binary, config, tokens, state, asks, journal, log; gitignored
  resources\                      local artifacts, gitignored
  logs\                           local logs, gitignored
  temp\                           local scratch, gitignored
  rust\target\                    Rust build output, gitignored
```

## 9.1 Crate boundaries

The workspace separates the harness-neutral relay from the OpenCode integration. Dependencies point inward: `inbound` and `opencode` depend on `core`, `ntfy` is standalone, and `app` depends on all of them. Only `app` and `outbound` name a concrete harness; `outbound` is the documented exception noted below.

- `core`: `SessionApi` and `PermissionApi`, the topic rule, config parsing, the state file, text budgets, and link safety. It knows no product name, HTTP route, or session id format.
- `ntfy`: the ntfy wire: header and JSON publish, file upload, SSE parsing, attachment streaming. It carries bytes and headers, not routing policy.
- `inbound`: filter, dedupe, queue, routing, control engine, prompts, permission replies, inbox retention. It depends on the `core` traits, never on `opencode`.
- `opencode`: the only harness crate today: API client, service registration and auth, event types, and the trait implementations. It is OpenCode-specific by design.
- `app`: composition root: config gate, lock, log, discovery, SSE task, queue drain, CLI, `status`. It builds the OpenCode adapter and injects it into `inbound`.

The seam is one direction: `inbound` calls the traits, `app` selects the implementation. A second harness adds its own crate implementing the same traits and a registry entry in `app`; `core`, `ntfy`, and `inbound` do not change. Its discovery and auth stay in that crate, because those are the least portable parts. `outbound` is the documented exception: turn tracking, file scan, and publish policy are neutral, while event mapping follows the harness event shapes and moves into the harness crate when a second one exists.

Today the boundary is narrow on purpose: `adapter.name` must be `opencode`, and discovery reads the OpenCode service registration. OpenCode is the only supported harness this year; the pi/omp seam is kept open, not implemented. The Rust outbound is the live publisher; text missed during an event gap is recovered from the message API. The Node relay was deleted in 0.8.0, so the Rust path is the only implementation.
