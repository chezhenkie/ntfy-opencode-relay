# Message routes

How a phone message reaches an agent, and how an agent turn reaches the phone.

## The model

ntfy is a broker: a shared channel with named topics. A publisher posts to a topic, and every client subscribed to that topic receives the message. The two sides never connect to each other.

One topic carries the whole conversation, named after the project folder (`ntfy-proj`). The phone app and the relay both connect to the ntfy server as clients.

The general process:

1. The phone posts a message or file to the topic.
2. The relay receives it and adds the prompt to an existing session through the local OpenCode API.
3. The agent runs the turn.
4. The outbound task watches the turn and posts the result back to the same topic.
5. The phone shows the notification.

Nothing connects the phone to the agent directly. The agent runs inside the OpenCode server on the same machine; outside clients reach it through the broker or the local API. Messages published by the agent side carry the `oc-out` tag, so the relay ignores them and does not answer itself.

## Entities

| Name | What it is |
| --- | --- |
| Android app | ntfy client on the phone. Publishes to the topic, and holds a live subscription to it. |
| TLS terminator | HTTPS front door on the laptop. Terminates TLS with the private certificate and forwards to the container. |
| ntfy server | The broker. Container on port 8080, reachable as 8810 on the host. Keeps 72 h of history and fans out to every subscriber. |
| Relay | Our code, inbound and outbound. The Rust `app` crate is live; the Node relay is the rollback. |
| Plugin | Historical outbound. Patched npm package that ran inside the OpenCode server; removed from the config at 0.5.0 and deleted in 0.7.0. Not a rollback path. |
| OpenCode server | Runs sessions. Exposes the local API the relay calls and the event stream the outbound task listens to. |
| Session | One agent conversation. It runs turns; it does not know the phone exists. |
| MCP server | `ntfy-relay mcp`. One `send_file` tool, spawned per directory by OpenCode, no daemon dependency. |

## Inbound: phone to agent

```
INBOUND

[Android app]
     |  publish text or file, phone token
     v
[TLS terminator]        HTTPS front door, terminates TLS, forwards to the container
     |
     v
[ntfy server]            broker, keeps history, fans out
     ^
     |  subscribe to topic ntfy-proj, owner token
     |
[Relay]                 filter, duplicate check, save file to .ntfy-inbox
     |
     |  POST prompt into an existing session, queue or steer
     v
[OpenCode server] ---> [Session]        agent turn starts
```

Walk through it:

1. The app publishes over HTTPS with the `phone` token. The request enters through the TLS terminator, which terminates TLS and forwards it to the container.
2. The ntfy server stores the message in the topic and fans it out to every subscriber of that topic.
3. The relay is one of those subscribers. It holds a single subscription for all its project topics, skips messages tagged `oc-out`, skips messages outside the replay window, and skips ids it has already handled.
4. An attachment is written to `<project>\.ntfy-inbox\<unix>-<name>` and passed to the session as a `file:///` URI.
5. The relay picks a target session from a live snapshot: a one-shot pending target from `/new` or `/to`, else the previous target inside a 10-minute continuity window, else the session with the newest activity. Activity is the later of `time.viewed` (desktop use) and `time.updated` (any message). A running session breaks a tie, then the previous target, then the id; child sessions are excluded. No pin is stored, so no decision outlives the evidence. Free text never creates a session; an explicit control action may.
6. It POSTs the prompt to the local API. The turn starts and the agent works on the phone's text.
7. Every accepted message gets a short ack published back to the topic. Every failure gets one error push. Both are link-safe.

## Command and control

The relay also executes a declared command set from the phone and controls the local agent harness through it. The set is data, not code: JSON in `bin\ntfy-relay\ntfy-relay-control.json`, read at startup.

Rules:

- The parser is a module, not a handler: `rust\crates\inbound\src\control` in the live path. It owns prefix stripping, verb matching and argument checking only; effects are injected handlers. The Rust relay loads no Node runtime.
- Handlers call a harness adapter, never a harness API directly. The adapter interface is `createSession`, `listSessions`, `sessionSignals`, `prompt`, plus a capability list. OpenCode is the only adapter implemented now; the interface stays capability-based so pi and omp can be added later without touching the parser or the schema. The control module names no harness; the config still requires `adapter.name: opencode` today.
- A message is a command when its first token after a configured prefix is a configured verb, and the action is enabled. Default prefixes are `/` and `hey relay`, case-insensitive.
- Any other text stays a prompt, including unknown slash text. This keeps the verified `/usr/bin/env is missing` case working. `@` is never a prefix; it stays prompt text and the agent resolves it.
- A message that starts with `hey relay` but matches no verb gets one `unknown action` ack and is not prompted. The phrase is an explicit call to the relay.
- Actions are keyed by the verb. Each has `handler`, optional `aliases`, optional `arg` (the argument label the handler validates), optional `requires`, optional `confirm`, `ack`, and optional `target`, `cooldownMs`, `enabled`. The remainder after the verb is one argument string. No pattern language, no slots.
- Each action may declare `requires`, an array of adapter capabilities (`session.create`, `session.list`, `session.pin`, `session.auto`; the last two gate target selection and clearing). At load the relay checks every enabled action. An action with an unmet requirement is disabled, logged once, and answers with one error ack naming the missing capability; the rest of the table keeps working. This is how the same schema runs on a harness that cannot create sessions.
- Each action may declare `confirm: true`. The relay then holds the parsed action for 60 s without executing. It acks the verb and argument and accepts `/yes` or `/no`. Only one confirmation is pending per topic; a second confirmable command is rejected with one ack. After 60 s the pending action is dropped and a late `/yes` gets `confirmation expired`. `/yes` and `/no` are built-in control verbs, not config actions, and only resolve while a confirmation is pending. The gate protects against a typo; it is not authentication. Admin-level handlers will need a separate credential from the Android-to-ntfy review.
- The handler registry is closed: `session.create`, `session.list`, `session.pin`, `session.auto`, `relay.help`, `relay.status`. Permission replies (`allow`, `always`, `deny`) stay native because they correlate with a pending request id. No shell, no arbitrary paths, no dynamic code.
- `session.create` calls the adapter with the topic's project canonical and sets the one-shot target for the new session when `target` is true. The adapter applies its own session defaults. It acks the new id and title.
- The adapter block in `ntfy-relay.json` selects the harness and holds its options. The OpenCode adapter needs `agent` and `model` for created sessions; a session without them loops on empty tool calls. Other adapters declare their own fields.
- Missing `.json`: the relay uses the built-in action table and logs the expected path once. It never writes the file. `relay-windows\ntfy-relay-control.example.json` is the committed example. Present but invalid: the relay fails to start, like an invalid `ntfy-relay.json`. There is no merge; the file is the whole table.
- Commands never carry attachments. A command with a file attachment is rejected with one error ack.
- Control actions use the same publish gate and link filter as every other ack. A failed handler produces one error push naming the action; prompt text is never logged.

Example document:

```json
{
  "version": 1,
  "prefixes": ["/", "hey relay"],
  "caseSensitive": false,
  "actions": {
    "new": {
      "handler": "session.create",
      "requires": ["session.create"],
      "aliases": ["start"],
      "arg": "title",
      "ack": "new session {id} in {project}",
      "target": true,
      "cooldownMs": 5000
    },
    "sessions": {
      "handler": "session.list",
      "requires": ["session.list"],
      "arg": "count",
      "ack": "sessions listed"
    },
    "to": {
      "handler": "session.pin",
      "requires": ["session.pin"],
      "arg": "session",
      "ack": "next message -> {title}"
    },
    "auto": {
      "handler": "session.auto",
      "requires": ["session.auto"],
      "ack": "target cleared"
    }
  }
}
```

## Outbound: agent to phone

```
OUTBOUND

[Session]
     |  turn text, files, permission ask, error
     v
[Rust app]                event stream subscription, one process for all projects
     |
     |  publish to topic ntfy-proj, owner token, tag oc-out
     v
[ntfy server]
     |
     |  push to every subscriber
     v
[TLS terminator]        HTTPS front door
     |
     v
[Android app]            shows above the subscription list
```

Walk through it:

1. The outbound task listens to the OpenCode event stream. It sees turn text, execution results, permission asks, question tools, and file events without polling.
2. It buffers text per session and publishes once at the end of the turn, not per delta.
3. Text under 3400 bytes goes inline. Longer text goes as one `.md` attachment with a summary.
4. New media and document files created during the turn are attached. Source code and modified files are not.
5. The topic is the project folder name, unchanged: `ntfy-proj`. The `oc-out` tag marks every publish as agent-originated.
6. Domain-shaped tokens are defanged before publishing, so the phone client cannot turn a file name into a link.
7. The ntfy server fans the message out to every subscriber. The phone app holds one of those subscriptions and shows the notification.

## Split between the directions

The plugin saw exact in-process events, which made outbound exact and immediate, but it could not do inbound: an instance ran per loaded project, so two loaded projects would handle the same phone message twice, and it died when the project closed. The Rust outbound keeps the exact-event model through the OpenCode event stream and stays in one process.

The relay does inbound because it is one process for all topics, independent of any project window. It cannot see agent internals, so it injects prompts through the local API instead. Both directions now share that one process.

## The merge (executed at 0.5.0)

The plan replaces both components with one Rust app that owns the subscription to ntfy, the subscription to the OpenCode event stream, and the local API client. Same topic, same tags, same filters, same phone app. The command and control code becomes the `control` module in `inbound` with the same `.json` schema; the handler registry moves with it, and the harness adapter traits stay a separate layer so pi and omp plug in beside OpenCode.

```
AFTER THE MERGE, INBOUND

[Android app] -> [TLS terminator] -> [ntfy server]
                                          ^
                                          |  subscribe, filter, dedupe
                                          |
                                   [Rust app]
                                          |
                        +-----------------+------------------+
                        |                                    |
                        |  wake: POST prompt                 |  fetch: tool call
                        v                                    v
                 [OpenCode server] ---> [Session] <-- [MCP server]
```

```
AFTER THE MERGE, OUTBOUND

[Session] -> [OpenCode server]
                    |
                    |  event stream subscription
                    v
               [Rust app]
                    |
                    |  publish, tag oc-out, link-safe
                    v
               [ntfy server] -> [TLS terminator] -> [Android app]
```

What changed at 0.5.0:

- The plugin left the live path. Outbound is a subscriber to the event stream instead of a set of in-process plugin instances.
- The patch script and the location gate left the live path; the plugin and its cache copy are not a rollback path.
- The relay's `opencode api` calls became direct HTTP calls, so no child process and no PATH assumptions.
- One config, one token, one lock, one log for both directions.
- The `ntfy_send_file` tool moved to an MCP server in the same app, so OpenCode no longer holds the ntfy token.

What stays identical: the broker, the phone app, the topic rule (project folder name), the `oc-out` tag, the 72 h history, the replay window, the dedupe rule, the own-subscription filter, the link-safety allowlist, and the `.ntfy-inbox` layout.

Open point: MCP is pull-only. A tool call is made by the session, so it cannot start a turn. The wake path is a separate local prompt call; MCP is for tools inside a running turn.
