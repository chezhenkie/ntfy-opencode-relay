# Bridge and relay: relation

The bridge is the process and administration layer above the relay. The relay stays a standalone project that owns ntfy semantics. The division of work is fixed; the routing interface and several ownership details are not.

Source: `fotonAPIbridge-v2-specsheet.md` in the bridge project, relay specsheet 00 and 08, open work 10.6.

## Division of responsibility

| Concern | Bridge | Relay |
| --- | --- | --- |
| Process start, stop, restart | owner of configured services | none |
| ntfy token and publish | never | owner |
| ntfy subscription, filter, dedupe | never | owner |
| Inbound command verbs | never | owner |
| Outbound event mapping | never | owner |
| Cross-environment routing | open, see Routing | emits events only |
| Administrative state | owner | none |
| MCP stdio server | not a child | spawned per directory by OpenCode |

## Contracts

- The OS starts the bridge; it is never a child of anything it supervises.
- Start order: container healthy, OpenCode service registration valid, relay last.
- Stop order: relay first, then OpenCode, then the container. The bridge design fixes only the relay step; who stops the container is open there.
- The relay serves no HTTP. The bridge reads `ntfy-relay status`, the lock file, the state file, the log, and the config. The status command prints the config path, the lock pid and its liveness, the state counts, and the last log line; it does not prove the subscription.
- `/v1/health` on port 8810 is the ntfy container's endpoint. A readiness probe against it is a container check, not an ntfy API call, and the source of readiness is still open in the bridge design.
- OpenCode's `service.json` is read for the start order and belongs to OpenCode, not to the relay.
- The bridge never calls the ntfy API, parses relay control messages, or holds the ntfy token. Nothing in start or stop needs the token file.
- The relay's internal supervisor (SSE and outbound tasks) and the bridge's process supervisor are separate layers. Neither reaches into the other.
- Separate projects: no shared code, repository, or lifecycle. No bridge logic in the relay and no relay logic in the bridge.

## Routing

Routing ownership is unresolved. Two models exist: the bridge tells the relay where to send output, or the relay decides an output has several consumers and emits accordingly. The invariant either way is one router per concern. The current leaning is relay emits, bridge delivers, but it is not accepted. The choice gates cross-environment work, which is step 3 of the bridge build order; local supervision and starting the relay in dependency order do not depend on it.

The data plane stays local: cached routes, and administrative traffic separate from message traffic, so an alpha election cannot interrupt a push or a prompt.

## Administrative state

- The bridge owns the child registry, desired state, routes, capability map, health summaries, and versions.
- Each bridge writes its own local records; one active alpha writes shared administrative records. The store is likely JSON over REST, with increasing record versions. Peer records reconcile when reachable, and verified local process state beats a stale replica.
- The alpha is fenced by a lease. Shared writes pause while the lease cannot be confirmed. Election and lease rules are open.
- Dormancy is distinct from death. Administrative state survives; buffers, sessions, and in-flight work do not. On wake, services start in dependency order.
- Secrets are not stored in plaintext. Token distribution is open; the relay and MCP read tokens from local files today.

## Current state

The bridge is deferred (open work 10.6); no release item depends on it. The relay is started today by the logon task and the watchdog, with no owning handle, pid-only reconcile, and the reboot race. Those are the three audit findings the bridge is meant to remove. MCP is not a bridge child: OpenCode spawns it per directory.

One tension remains: the relay docs call the division fixed and the interface able to grow, while the bridge design keeps routing ownership, alpha election and lease, dormancy semantics, the database shape, and the container runtime owner open.
