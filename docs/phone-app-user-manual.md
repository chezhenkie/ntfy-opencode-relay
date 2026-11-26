# ntfy on Android - user manual

Phone-side manual for this relay. Host setup, tokens, and relay internals are in [operations.md](operations.md); this file covers only what happens on the phone.

## 1. Install

1. Install `ntfy` from one of the two zero-Google sources: F-Droid (install https://f-droid.org, then ntfy from its repository), or the GitHub release APK `ntfy-<version>-fdroid-release.apk` from https://github.com/binwiederhier/ntfy-android/releases/latest. Do not use the Play Store build or the `play-release.apk` asset: both use Firebase, and this setup has no Google path.
2. Install the VPN client and join the same private network as the host. Keep it connected; the ntfy server is not reachable without it.
3. Open ntfy and allow notifications. On Android 13 and later, also allow "Alarms & reminders"; the app needs exact alarms to reconnect after Doze.
4. Set battery usage to Unrestricted: Settings -> Apps -> ntfy -> Battery -> Unrestricted.
5. Keep the "Subscription Service" foreground notification. It is what keeps the socket alive when the app is closed. Long-press it and open settings if you want it hidden, but Android may then kill the connection.

## 2. Add the server and topic

1. In ntfy, tap `+`, choose the topic name for the project, for example `ntfy-proj`. The name is the project folder name and the folder must sit under a relay root; with a parent root, no host change is needed. See [startup](startup.md#project-topics).
2. Server URL: `https://ntfy.example.ts.net` (your own host name).
3. Do not add a user. Authentication is a custom header:
   - Settings -> Advanced -> Custom headers -> add
   - name: `Authorization`
   - value: `Bearer <token>`
4. Save, then open the subscription. Publish a short message to test. It should appear in the topic immediately. A message published from the phone appears even when the relay is not serving the topic, so this test proves the server and token only.

The app sends that header on every request to the server, including the WebSocket handshake. A user entry plus an `Authorization` header for the same server is not allowed, and a token typed into the password field returns `401`. The token is the only phone credential.

### Display name

A subscription created from a pasted URL or an `ntfy://host/topic` link can store the whole URL as its display name, so the drawer shows the URL instead of the topic. Delivery is unaffected; the topic is still the last path segment. To set the name, open the topic, open its subscription settings, and use `Display name`; an empty field shows the topic. The same screen sets a display icon. The link form `ntfy://<host>/<topic>?display=<name>` sets a custom label directly.

## 3. Commands

Type commands in the message bar the same way as a normal message. A message without a leading slash is delivered to the routed session as a prompt.

| command | what it does |
| --- | --- |
| `/help` | list the commands |
| `/status` | relay status: topics, queue, target, pending asks, uptime |
| `/new [title]` | create a session in the project and send the next message to it; the title is optional and capped at 200 characters |
| `/sessions [count]` | list recent sessions with numbers and titles |
| `/to` | list up to five sessions with numbers, for the next message |
| `/to 2` | send the next message to entry 2 of that list |
| `/to <title fragment>` | send the next message to the single session whose title matches |
| `/auto` | drop a target set by `/to` or `/new`; routing goes back to automatic |
| `/steer <text>` | inject text into the running turn instead of queueing a new prompt |
| `/allow [code]` | allow the only pending permission ask once, or the ask with that code |
| `/always [code]` | allow it and remember the approval for matching asks |
| `/deny [code]` | reject the only pending permission ask, or the ask with that code |
| `/yes` | confirm a held action |
| `/no` | cancel a held action |

Notes:

- Normal use needs no command. A message goes to the session you last used on the host. Typing in a chat is what counts; merely opening it changes nothing.
- Free text never creates a session. Only `/new` does; the optional argument is the title.
- There is no session switch on the phone. After `/new`, the relay targets the new session, and the next message you send in this topic is its first prompt. Do not open another topic and do not select anything in the app; the target is already set. `/status` shows it.
- The session exists on the server at once and gets no tab on the desktop by itself: open it there with `/sessions` (`Ctrl+X` then `L`) or `Ctrl+O`.
- The topic list is chronological, oldest at the top. New messages arrive at the bottom, so an ack or a reply can sit below the command that caused it; read down for the newest.
- `/to` and `/new` set one target for one message, or ten minutes. No command takes a session id.
- A confirmation window is 60 seconds. After that the held action is dropped; send the command again.
- `/always` can satisfy a matching ask silently later. Use `/allow` when you want a single approval.
- Permission replies always go to the ask that was pushed, never to the target.
- If a message landed in the wrong session, send a message in the intended chat on the host, then send again from the phone.

Example, two messages from the phone:

```
/new link fix
run the link check and fix every broken link
```

The first creates a session titled `link fix` and targets it. The second is its first prompt; the replies come back on the phone. The switch is that second message: there is no list to open and no session to pick in the ntfy app, and the ack for `/new` names the session so you can recognise it on the desktop.

## 4. Tags

Tags are optional message labels. Add them in the publish dialog, comma-separated. Most are only markers; one changes relay behavior.

| tag | user | relay | opencode | effect |
| --- | --- | --- | --- | --- |
| `oc-out` | not allowed | allowed | not allowed | the relay ignores any message carrying it |
| `oc-attach` | allowed | allowed | not allowed | marks a file the relay attached automatically at the end of a turn |
| `oc-manual` | allowed | allowed | not allowed | marks a file sent by an explicit `ntfy_send_file` request |
| `lock` | allowed | allowed | not allowed | permission ask; answer with `/allow`, `/always`, or `/deny` |
| `question` | allowed | allowed | not allowed | question form; answer with `/answer` and free text |
| `warning` | allowed | allowed | not allowed | error notice |
| `hourglass_done` | allowed | allowed | not allowed | idle notice |

## 5. Permission asks and confirmations

A permission ask arrives with the `lock` tag and the command the agent wants to run. The push names the session and carries a short ask code in this line:

`Reply /allow <code>, /always <code>, or /deny <code>; with one ask, /allow is enough.`

Answer from the phone:

- `/allow` allows the only pending ask once. With several asks pending, a reply without a code lists their codes and titles and approves nothing. `/allow 3f2a` or `/allow #3f2a` answers that exact ask; the code is case-insensitive.
- `/always` or `/always <code>` allows it and saves a matching approval, which can satisfy later asks silently.
- `/deny` or `/deny <code>` rejects the only pending ask or the named one.

The push is sent 15 seconds after the ask; replying inside that window cancels the push. An answer other than these three values is rejected and the ask stays pending. The ack names the session title and the code, for example `approved once in Fix links (3f2a), 2 more pending`. A reply with no pending ask answers `no pending permission ask`; the routing target is never named.

A question form arrives with the `question` tag and lists the question and its options. The last line reads:

`Reply with /answer and your answer in your own words.`

Answer however you like:

- `/answer 2`, `/answer keep both, any order`, and `/answer 1 keep 2 any` all close the newest question and pass the text to the assistant. There is no code and no field syntax.
- The desktop question closes at once. The assistant reads your text as the answer and asks again when it needs more.
- `/answer` with no text dismisses the question and tells the assistant no answer was given.

The ack reads `answer sent for <title>`. A settled form answers `that question in <title> was already answered`. A form with no visible field is pushed as text and must be answered on the desktop.

When a command holds for confirmation, reply `/yes` or `/no` within 60 seconds.

## 6. Troubleshooting

- `not authorized`: the token is in the user password field, or the header value is missing `Bearer `. Delete the user entry, keep only the custom header.
- Nothing arrives: check the VPN client, then the subscription, then battery Unrestricted and the foreground service. Publish a message from the app; if the topic shows it, the server and token are fine.
- A command does nothing: it may carry `oc-out`, or a `/to` target is pending. `/auto` clears it.
- The relay receives but the session does not show it: check `/status` on the phone and the relay log on the host.

## 7. Token handling

The token lives on the host in `%LOCALAPPDATA%\ntfy-relay\ntfy-token.txt` and is rotated with `tools\rotate-token.ps1`. After a rotation, replace the header value in the app; the old token keeps working until the revoke phase, so there is no outage window. Never send the token as a message or commit it.
