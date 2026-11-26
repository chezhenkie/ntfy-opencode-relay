# 9. Directory tree

```
ntfy-proj\
  README.md                       entry point: parts, quick start, checks
  CHANGELOG.md                    change list, versioned
  LICENSE                         MIT
  package.json                    type module, Node >= 20, tlds/links/secrets scripts
  rust\                           Rust workspace root
    Cargo.toml                    Rust workspace, resolver 2
    Cargo.lock                    pinned Rust dependency graph
    rust-toolchain.toml           stable toolchain with rustfmt and clippy
    assets\
      ntfy-icon.svg               icon source, square with slightly rounded corners
      ntfy.ico                    multi-size icon embedded in every Windows binary
    build\
      win-resource.rs             shared build script: icon and version strings
    crates\
      core\
        Cargo.toml
        src\lib.rs, topic.rs, clock.rs, adapter.rs     topic rule, clock, SessionApi and PermissionApi
        src\fsutil.rs                                  temp-sibling write and rename helpers
        src\config.rs                                  defaults, validation, path resolution
        src\state.rs, asks.rs, forms.rs, text.rs       state file, ask and form registries, byte budgets
        src\link_safe.rs, tlds.rs                      link rewrite, generated IANA table
        tests\state.rs, asks.rs                        atomic write, quarantine, cursor window, ask codes
      inbound\
        Cargo.toml
        src\control\                                  table, parse, load, engine
        src\rate.rs                                   prompt rate cap per topic
        src\filter.rs, queue.rs, routing.rs           acceptance, bounded queue, target choice
        src\prompting.rs, permission.rs, prune.rs     prompt delivery, native replies, inbox retention
        src\form_reply.rs                             /answer cancel pass-through
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
      control\
        Cargo.toml
        src\types.rs                                  envelope, error codes, mode names, reply bodies
        src\policy.rs                                 root add, remove, containment, platform case rule
        src\runtime.rs                                endpoint file with pid, port, per-process token
        src\process.rs                                pid liveness for the lock and the endpoint file
        src\server.rs                                 loopback listener, framing, caps, bearer check
        src\client.rs                                 blocking loopback client for ntfy-admin
        src\test_support.rs                           temp root for tests
      app\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs, runner.rs, relay.rs              CLI, run loop, message handler
        src\supervisor.rs                             restart policy for the SSE and outbound tasks
        src\mcp.rs                                    stdio MCP server and send_file tool
        tests\mcp.rs                                  spawned stdio handshake, tool contract, fake publish
        src\api_client.rs                             rediscovering OpenCode client
        src\sse.rs, discovery.rs                      subscription loop, project map
        src\store.rs, lock.rs, logger.rs, status.rs, status_checks.rs   state writer, lock, rotating log, status rows
        src\ensure.rs                                 watchdog step: running check, detached start
        src\control_endpoint.rs                       routing and call bridge for the control listener
        src\config_write.rs                           atomic projectRoots rewrite
        src\util.rs                                   clock, clip, jitter, name helpers
      deploy\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs                                   ntfy-deploy apply|status|mode|install|remove CLI
        src\config.rs, platform.rs                    deploy config rules, publish and portproxy plans
        src\deploy.rs, status.rs, runner.rs           apply, health wait, status, command seam
        src\mode.rs                                   interface detection, path menu, config write
        src\install.rs                                release placement, config defaults, tasks, units
      auth\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs                                   ntfy-auth apply|status CLI
        src\config.rs, auth.rs, runner.rs             config rules, bootstrap sequence, command seam
        src\test_support.rs                           fake container CLI and temp root
      admin\
        Cargo.toml
        build.rs                  embeds the Windows icon and version strings
        src\main.rs, cli.rs                           ntfy-admin topics|add|remove|status|stop|mode
        src\endpoint.rs, run.rs                       live client seam, dispatch, one line per item
  .github\
    workflows\ci.yml              Node checks on windows; Rust matrix on ubuntu and windows
    workflows\release.yml         patch bump, three targets, image, release, cleanup
  release\
    README-RELEASE.md.in          release notes template with case labels
  docs\
    index.md                manual contents, scope, environment
    index.html              site front page: diagram cards and a document table
    github-page-html-files\
      the generated pages: one per chapter, specsheet\, and the three flow diagrams
    assets\front-page.png   front page screenshot used by the README
    phone-app-user-manual.md phone setup, commands, tags
    switching-networks-user-manual.md  switching Tailscale, Nord Meshnet, and LAN
    zero-google.md          the three leak paths, and how each is disabled
    binaries.md             ntfy binaries and checksums
    windows-podman.md       Windows Podman route: machine, image, run
    ubuntu.md               native systemd and rootless Podman on Ubuntu
    network.md              private network, loopback binding, TLS terminator, base-url matrix
    https.md                HTTPS switch, portproxy removal, firewall, rollback
    android.md              F-Droid client, WebSockets, battery, tests
    operations.md           lifecycle, tokens, startup check, notifications, relay, Rust smoke
    startup.md              check, start, stop and restart the host route, container and relay
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
      12-control-path.md              loopback endpoint, topic edits, stop, ntfy-admin
  container\
    Dockerfile                    alpine 3.24.4, non-root, baked server.yml, fetch stage for ntfy
    server.yml                    ntfy config; base-url and behind-proxy come from ntfy-deploy
    compose.yml                   container definition for the released image
    env.example                   values for compose.yml
    .dockerignore                 excludes hand-downloaded ntfy copies
  tools\
    build-image.ps1               builds the image from the tracked context
    rotate-token.ps1              two-phase token rotation, probe publish, temporary expiry
    check-doc-links.mjs           relative links and ASCII over every tracked markdown file
    build-docs-site.mjs           regenerates the published pages and checks their links
    secret-scan.mjs               scans tracked files for tokens, hosts, and absolute paths
    secret-scan.allow             allowlisted fixture matches
    link-safety\build-tlds.mjs    checks or writes the Rust TLD table
    link-safety\tlds.txt          IANA TLD source list
  e2e-smoke-tests\
    smoke.mjs                     live ntfy smoke: health, auth, publish, SSE, attachment
    run-windows.ps1, run-linux.sh  platform wrappers
    README.md                     usage, checks, options
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
