# Permission case 7 - UI-driven ask

Date: 2026-11-18. Source `09cab3c`. Live relay 0.14.0, pid 18080. Closes row 7 of `permission-reply-0.9.0.md`.

## Method

A temporary `/api/event` client subscribed with the relay's type list and recorded the permission frames. The ask came from the TUI session `ses_e12078e36ffemU7lWwlijBrgCI` in `ntfy-proj`: a shell call for `net user`, prompt left unanswered. The reply came from the phone.

## Frames

`permission.asked`, `created` 1795018005990 (16:06:45.990Z). The location value is the project directory, shown sanitized:

```json
{"id":"evt_1ef691de6002d27OfF53dQ25xC","created":1795018005990,"type":"permission.asked","location":{"directory":"<project root>"},"data":{"id":"per_1ef691de60017ATniViYT8rv5e","sessionID":"ses_e12078e36ffemU7lWwlijBrgCI","action":"shell","resources":["net user"],"save":["net *"],"source":{"type":"tool","messageID":"msg_1ef68e037001Pxk9aQNxMZ3dmJ","id":"call_9a9da208515c421cb1c4e144"}}}
```

`permission.replied`, `created` 1795018050012 (16:07:30.012Z):

```json
{"id":"evt_1ef69c9dc001MWZdwA7KFsK5Sy","created":1795018050012,"type":"permission.replied","location":{"directory":"<project root>"},"data":{"sessionID":"ses_e12078e36ffemU7lWwlijBrgCI","requestID":"per_1ef691de60017ATniViYT8rv5e","reply":"once"}}
```

## Relay and phone

- Ask registered 16:07:01.037Z, 15.047 s after the frame: `permission ask code=5db4 session=ses_e12078e36ffemU7lWwlijBrgCI request=per_1ef691de60017ATniViYT8rv5e`.
- Phone message accepted 16:07:30.010Z; reply 16:07:30.015Z: `permission reply code=5db4 decision=once`.
- Ack body: `approved once in ses_e120 (5db4), 1 more pending`. The title is the short session id because the event location resolved the project without a session lookup. The remaining record is `0520`, stale since 2026-11-13.
- The shell call returned the account list. `GET /api/session/<id>/permission` after the reply returned `{"data":[]}`.
- Registry after: only `0520`. Code check: sha256 of the request id starts `5db46dcc`, first four hex `5db4`.

## Result

Row 7 passes: a UI-raised ask reaches the phone through the relay, the reply resolves the registry record, and the session continues. The 10.2 permission matrix is complete.
