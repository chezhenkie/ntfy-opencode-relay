# 10. Open work

What remains before the project is public. No order is implied. Paths are deliberately omitted: the layout changes before this list is worked off, so items name the artifact, not the folder.

## 10.1 Windows executable icon

The Windows build carries no icon and no version strings, so Explorer shows the generic executable and the file properties are empty. Needs a multi-size `.ico`, a resource with the icon and the product, description, and version strings, and an embedding step that runs for every release build. Done when a fresh Explorer view of the release asset shows the icon and the correct version in its properties.

Status 2026-11-16: done. `rust\assets\ntfy.ico` holds 16 to 256 px entries cut from `rust\assets\ntfy-icon.svg`; `rust\build\win-resource.rs` writes the icon and version block from `package.json` and links it through `windres` on windows-gnu. The 0.13.2 windows-gnu build reports 0.13.2 and the icon in the properties of both binaries.

## 10.2 Question and permission round on the phone

Both interactive paths have unit tests and fixtures, but no live phone round exists. Exercise a question with one field, a question with several fields and options, hidden fields omitted, and the answer reaching the session. For permissions: an ask answered on the desktop inside the 15 s grace (no push), an unanswered ask (one push), `/allow`, `/always`, `/deny`, two pending asks (ack names the remainder), and a reply with no pending ask (error push). Record the observed push and the session outcome in the audit. A saved `always` can satisfy a matching ask silently, so use an unapproved command such as `net user`.

Status 2026-11-13: permission replies are fixed in 0.9.0 and verified live: an unrelated pin no longer captures a reply, the newest ask is the default, `/allow #CODE` and uppercase resolve a named ask, a desktop answer leaves a truthful tombstone, the registry survives a restart, and an empty registry answers `no pending permission ask`. Matrix in `audit\permission-reply-0.9.0.md`. Question pushes pass; answering a form stays desktop-only per spec 09 section 13.

Status 2026-11-18: the matrix's last open row, a UI-driven ask, is closed. A TUI shell call for `net user` produced a `permission.asked` frame with the project location; the relay registered the ask as `5db4` after the 15 s grace, the phone reply `/allow 5db4` was accepted, and the shell call completed. The session permission list was empty afterwards. Record in `audit\permission-case7-0.14.0.md`.

## 10.3 First GitHub release

The repository is private with no tags and no releases. Decided 2026-11-13: one public repository, published from the sanitized tree as a fresh history; the current private repository is deleted at the cut, never kept alongside, so the public name is free. The binary name stays. The Node inbound rollback is decided: pruned in 0.8.0, so the release ships one implementation. Build the Windows and Linux binaries from the tagged source, publish SHA256 sums, and take the notes from `CHANGELOG.md`. Add a tag-triggered release job so CI builds the artifacts. The Android client stays upstream: the F-Droid APK comes from the ntfy-android releases.

Release goal: Linux ships two targets, `linux-gnu` for glibc hosts and `linux-musl` for Alpine. The workspace resolves rustls with the ring provider and no OpenSSL, and musl targets already default to `crt-static`, so the builder needs a C toolchain rather than a second TLS stack. CI tests the gnu target only today, so the musl artifact needs its own job and one `cargo test --workspace` plus a live start on Alpine before the release claims it. The relay does not belong inside the ntfy container: it needs the OpenCode registration, the token files, and the project roots on the host.

The release carries the image tarball from `podman save` (one per claimed architecture; amd64 for now), a compose file, an env example, the binary zips, and SHA256 sums, so a host installs with `podman load` and three commands, without a registry or a clone.

## 10.4 README pass

Rewriting one section at a time with the user, agreeing on the wording before the next. The target is an entry point for someone who has never seen the project. Move version and crate-by-crate status out; keep the protocol table, requirements, quick start, and license. Stay ASCII and keep relative links resolving. The layout section waits for 10.8.

Status 2026-11-16: done. The README opens with the three runtime parts and the topic, keeps the protocol table, requirements, quick start, checks, and license, and sends version history and crate boundaries to specsheet 03 and 06 and operations 7.9. The layout names `audit\` and `e2e-smoke-tests\`. The quick start read the system token file at the time; 0.14.0 replaced that with `ntfy-token.txt`.

## 10.5 Container image

The image builds locally. The deployment base URL is supplied at container creation, so the image carries no host-specific value. Decide registry and tag scheme, architecture coverage, and how the binary is fetched into the build instead of an untracked file next to the Dockerfile. Done when a clean host pulls the image, runs one documented command, and health passes without a rebuild.

Status 2026-11-16: done. The Dockerfile has a fetch stage that downloads the pinned release tarball, checks `NTFY_SHA256`, and unpacks it there; the final stage copies the binary from that stage. The tree holds no binary and the build needs no preparation. The tag is the ntfy version plus `latest`, read from the Dockerfile by `tools\build-image.ps1`; there is no registry, and a host loads a `podman save` archive. amd64 is the claimed architecture because the base digest and checksum pin it. Evidence: image saved to `temp\image-release\ntfy-image-2.37.2-amd64.tar` (85,334,528 bytes, sha `08ec36d4...`), the container and every tag removed, the archive loaded, `ntfy-deploy apply` recreated the container, health 200 on loopback and HTTPS, and a second apply reported `reused`.

## 10.6 Bridge

Deferred. Process ownership, reconcile, and the reboot start order stay in the bridge project. No release item depends on it. Keep the docs from implying the relay owns them.

## 10.7 Foton phone topic

The Foton phone integration uses a dedicated topic, never a project topic. The relay and the MCP tool need no change: both are bounded to `projectRoots`, so neither subscribes nor publishes on that topic. The container needs no rebuild and no config change; since 0.14.0 the `phone` wildcard row covers the topic with no server-side step. Reuse the phone token and custom header. The topic must not equal a project folder name, or the relay would treat those messages as phone prompts.

Status 2026-11-14: done. `phone` had read-write on `foton-alerts` (`ntfy access phone foton-alerts rw`). Verified with the phone token over HTTPS: publish `200`, poll `200` with the message, anonymous publish `403`; the removed `system` user had no row. The phone subscription and its mute choice stay manual. The ntfy hook note in `foton-frontend-shell-proj` and `fotonfrontend-proj` now uses `foton-alerts`; the two copies are byte-identical. The `foton-alerts` row was folded into the wildcard on 2026-11-18.

## 10.8 Directory reorganization

The docs tree is settled: the numbered spec set is `docs\specsheet\`, the phone app manual is `docs\phone-app-user-manual.md`, and `audit\` stays outside `docs\`.

Status 2026-11-14: done. `rust-src\` is now `rust\` and `ntfy-server-container\` is now `container\`. The tree chapter, README layout, every specsheet tree, CI, the TLD generator, the image script, `.gitignore`, and the Rust doc comments name the new paths; `CHANGELOG.md` and `audit\` keep the names that were true when written. Stale statements about a Node rollback were removed. No code changed.

## 10.9 Pre-publication secret and path scan

Run before 10.3, over the working tree and every reachable blob, because a deleted file stays in the object graph. Patterns: `tk_` tokens and Bearer values, passwords and private keys, the private DNS name and addresses, absolute Windows and POSIX paths, user and device names, and captured fixtures that carry a real project list, session titles, or command lines.

State on 2026-11-13: `tools\secret-scan.mjs` is committed with a short allowlist and runs in CI as `npm run secrets`. The tree passes with 138 tracked files and 0 hits. The history does not: `--history` reports 874 hits, including a deleted `readme.html` that carries the private address. Tokens and keys are absent from both scopes. The fixtures and docs now use synthetic paths, titles, questions, and the example host. 10.3 deletes this private repository and publishes the sanitized tree as a fresh history, so the old blobs disappear with it; the scan must report 0 on the new repository before publication.

Pass: a committed scanner reports zero hits over tracked files and all reachable blobs, with a short explicit allowlist for the documented example values. If history cannot be cleaned, publish a fresh repository rather than flipping this one public.

## 10.10 Installer CLI

After the first release, a small interactive installer for Linux and Windows can remove the remaining manual steps: load the bundled images, write the env file, create the systemd unit or the scheduled task, register the MCP server with OpenCode, start the stack, and run the health and phone checks. It drives the same compose file and env format as the manual path, never a second config source, and stays optional. One static binary per target, no PowerShell or Node for the end user: on Linux it writes a systemd user unit or drives compose, on Windows a per-user scheduled task. Windows is one target, not the default, and containers are a Linux option. Design reference, not shared code: the ytdlp-cli project, for release-asset digest verification, a staged swap that keeps config, an uninstaller, and a check-only update notice. The live install leaves the checkout only through this installer, never by a manual copy: one runtime tree under `%LOCALAPPDATA%` or `~/.local/share`, and the repo stays the source. Until then the runtime lives in a gitignored `bin\ntfy-relay\`, which replaced the old `resources\ntfy-relay\` in 0.8.0. Nothing under `resources\` is a project fixture or a runtime path; it is user scratch.

## 10.11 Post-publication audit

Not part of the publication gate. `audit\` stays as written through 10.3; when the project is complete and published, one full audit runs over the published tree, the live stack, and the release assets, and records fresh findings in a new round rather than reopening the old reports.

## 10.12 Phone question forms

A question form can only be answered on the desktop. A form with several fields, or one field with several choices, has no phone format; the reply must name the form, the field, and the choice without ambiguity. Decide the reply grammar and the code namespace, then implement it in the relay and the phone flow or record the scope cut. Status 2026-11-18: open; first item next session.

## 10.13 Diagram site

The three flow pages under `docs\` are linked from the manual index but sit as loose files. A GitHub page can present them in one place and give each diagram a stable address. Decide the index layout, the URL, and whether the pages publish from the repository as they are or are generated; keep one source per diagram. Status 2026-11-18: open; next session after 10.12.
