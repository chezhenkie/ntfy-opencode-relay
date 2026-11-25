# 6. Android transport and battery

The F-Droid app shows a banner: "Switching to WebSockets is the recommended way to connect to your server, and could improve battery life, but may require additional config in your proxy." This chapter explains the two transports, what to enable, and the battery trade-offs.

## 6.1 WebSockets vs JSON stream

| | WebSockets | JSON stream over HTTP |
|---|---|---|
| Transport | Persistent bidirectional TCP after an HTTP upgrade | Long-lived HTTP response, newline-delimited JSON |
| App setting | Settings -> Advanced -> Connection protocol -> WebSockets | Settings -> Advanced -> Connection protocol -> JSON stream |
| Recommendation | Recommended by upstream | Alternative transport |
| Battery | Lower per-message overhead, better keepalive | More overhead, may use more battery |
| Proxy | Needs the proxy to pass Upgrade and Connection headers | Works through almost anything |
| Google | No | No |

Both are instant delivery to your server. Neither uses FCM. The banner is only about transport, not about push providers.

The server sits behind a TLS terminator, which passes WebSocket upgrades without extra config. If the terminator changes later, it must forward the Upgrade and Connection headers.

## 6.2 Why WebSockets usually save battery

- One TCP connection with small framed messages, no repeated HTTP parsing.
- Ping/pong keepalive instead of stream reconnects.
- The app uses exact alarms to reconnect after Doze instead of polling.

Community measurements put a self-hosted WebSocket at roughly 1% per hour on Wi-Fi. Mobile data, especially 5G, is higher. Treat that as a rough number, not a guarantee.

## 6.3 What to enable on the phone

1. Tap "Enable now" on the banner, or open Settings -> Advanced -> Connection protocol -> WebSockets.
2. Grant "Alarms & reminders" when asked. Android 13+ needs exact alarms to reconnect the socket after Doze.
3. Set battery usage to Unrestricted: Settings -> Apps -> ntfy -> Battery -> Unrestricted.
4. Keep the "Subscription Service" foreground notification. It is what keeps the socket alive when the app is closed. Long-press the notification, open settings, and toggle "Subscription Service" off to hide it, but Android may then kill the connection.
5. Keep the VPN connected. The socket goes to your server over the private network.
6. If the server uses auth ([section 7.2](operations.md#72-auth-and-tokens)), carry the token as a custom header and do not add a user: Settings -> Advanced -> Custom headers, server URL `https://<host>`, name `Authorization`, value `Bearer tk_...`. The app sends it on every request to that server, including the WebSocket handshake. `phone` has no password, so the header is the only app credential. For curl and scripts the same token works as a Bearer header, or as basic auth with an empty username and the token as the password (`-u ":tk_..."`). A non-empty username with the token as the password returns `401`.

## 6.4 Battery expectations

Two persistent things run on the phone: the VPN client and the ntfy socket. Both cost battery; neither is free.

- Wi-Fi: roughly 1% per hour for ntfy, plus the VPN's own use.
- Mobile data: higher, especially on 5G, because the radio stays active for keepalives.
- Doze: Android still tries to suspend. Exact alarms and the foreground service are what keep delivery working.
- Multiple topics share one connection. Adding topics does not add sockets.

The low-battery alternative is FCM, and this setup rejects it. There is no zero-Google transport that is as cheap as FCM.

## 6.5 Test procedure

1. Switch to WebSockets.
2. Background the app or lock the phone.
3. Publish from the server:

```bash
curl -H "Authorization: Bearer <token>" -H "Title: WS test" -d "websocket background delivery" https://ntfy.example.ts.net/ntfy-proj
```

4. The notification should arrive within seconds.

## 6.6 Troubleshooting

"WebSocket not supported": wrong URL, TLS mismatch, or a proxy stripping the Upgrade headers. Test the same URL in the phone browser. With a TLS terminator in the path, check its status and the container log for the upgrade request.

Delayed after the screen turns off: check the exact alarms permission, battery optimization, and the foreground notification. Also confirm the VPN is still connected; Android may pause it.

Battery drain on mobile data: expected with a persistent socket plus VPN. Prefer Wi-Fi, or accept FCM if battery outweighs the Google requirement.
