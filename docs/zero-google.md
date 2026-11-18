# Zero Google baseline (read first)

Three separate paths can leak to Google or to ntfy.sh. All three are disabled here.

## 1. No FCM on the server

ntfy only creates a Firebase client when `firebase-key-file` is set. It is not set, so the server has no FCM sender and makes no Firebase calls.

## 2. No upstream relay

`upstream-base-url` forwards poll requests to another server, normally `https://ntfy.sh`, for iOS push. It defaults to empty. It is set explicitly to empty in `server.yml` so a later edit cannot silently enable it:

```yaml
upstream-base-url: ""
```

Do not set `upstream-access-token`, `firebase-key-file`, or SMTP settings.

## 3. No Google libraries in the Android app

Use an F-Droid-flavor build of the ntfy Android app: either from the F-Droid repository or the `-fdroid-release.apk` asset on the ntfy-android GitHub releases. Per the ntfy docs, the F-Droid flavor does not use Firebase at all; instant delivery runs over the app's WebSocket or the JSON stream, not FCM. The Google Play flavor contains Firebase and Play services libraries, even though it does not use FCM for self-hosted servers.

Do not use the PWA for background notifications on Android: browser push is delivered through the browser's push service, which for Chrome is FCM. The PWA is fine in the foreground, not as a zero-Google background path.

## Android app settings

1. Install F-Droid from https://f-droid.org, then install ntfy from the F-Droid repository, or download `ntfy-<version>-fdroid-release.apk` from https://github.com/binwiederhier/ntfy-android/releases/latest. Do not install the Play Store version or the `play-release.apk` asset.
2. Add the self-hosted server URL `https://ntfy.example.ts.net`. No port.
3. Subscribe to the topics you need.
4. Keep the VPN connected. Instant delivery needs a route to the server at all times.
5. Allow the foreground service: ntfy shows a permanent "Subscription Service" notification while instant delivery is active. Do not swipe it away.
6. Exempt ntfy from battery optimization: Android Settings -> Apps -> ntfy -> Battery -> Unrestricted. Without this, Doze can delay or drop the WebSocket.

The F-Droid build has no setting to disable instant delivery; that is the intended behavior.

## Verify the server is clean

Container:

```powershell
podman exec ntfy sh -c 'grep -E "upstream|firebase" /etc/ntfy/server.yml'
podman exec ntfy sh -c 'env | grep -iE "firebase|upstream" || echo no-firebase-env'
```

Native Ubuntu:

```bash
grep -E 'upstream|firebase' /etc/ntfy/server.yml
```

Expected: `upstream-base-url: ""` and no Firebase entries. No `firebase-key-file` means no FCM client is created; empty `upstream-base-url` means no poll requests are forwarded.

## UnifiedPush for other apps

If other Android apps should also avoid Google push, use UnifiedPush and let ntfy act as the distributor where supported. That is outside the ntfy notification path but follows the same principle.

## Out of scope

Android system components and Play services can still contact Google on their own. This baseline guarantees that ntfy, its server, and its app do not.
