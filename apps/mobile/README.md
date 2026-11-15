# Waku Mobile

Expo client for connecting to one or more remote Waku daemons from iOS and
Android.

## Install / Build Android APK

### Option 1: Download from GitHub Actions (Recommended)

The `Android` workflow (`.github/workflows/android.yml`) builds and verifies a
release `waku-android.apk` on `ubuntu-24.04` whenever mobile or client code
changes on `main` (or via manual `workflow_dispatch`).

Download the `waku-android-apk` artifact from the latest successful **Android**
run and install `waku-android.apk` on your phone.

### Option 2: Build Locally

From the repository root:

```sh
bun install
bun --filter @waku/mobile ios
bun --filter @waku/mobile android
```

To produce a standalone release APK locally (requires Android SDK 36, NDK
`27.1.12297006`, and JDK 17):

```sh
cd apps/mobile
bunx expo prebuild --platform android --no-install
cd android
./gradlew assembleRelease
```

## Connect (Local / Tailnet / Always-On V2Ray)

In Waku Desktop, enable **Settings → Daemon** and copy its WebSocket address
and token. Add those values in the mobile app. Use `wss://` outside a trusted
LAN or private tailnet; the token grants full control of the daemon host.

Saved profile metadata stays in app storage. On iOS and Android, daemon tokens
are stored separately in the device keychain through Expo SecureStore.

> **Important URL rule**: Waku Mobile normalizes addresses to the origin root
> (`normalizeDaemonAddress`), so always expose your daemon at a root host or
> subdomain (e.g. `wss://waku.example.com`), never a subpath (`wss://example.com/waku`).

### Keeping V2Ray Always On on Android

Android allows only one active system VPN slot at a time, so running V2Ray
prevents enabling the official Tailscale app simultaneously. Use any of these
setups to reach your `waku-daemon` without turning off V2Ray:

Run the built-in helper from the repo root to inspect your detected daemon
port/token and generate ready-to-copy configs:

```sh
bun run mobile:tunnel
```

1. **Cloudflare Tunnel (`wss://`) — fastest zero-VPS option**

   Launch an ephemeral Quick Tunnel for your local daemon:

   ```sh
   bun run mobile:tunnel cloudflared
   ```

   Or configure a persistent Cloudflare Named Tunnel pointing a subdomain
   (`waku.example.com`) to `http://127.0.0.1:<port>`, then connect the Android
   app to `wss://waku.example.com`.

2. **Self-hosted VPS (`frp` + Caddy `wss://`) — lowest latency / full control**

   Forward your daemon port to your VPS with `frpc` and terminate TLS with
   Caddy on a root subdomain (`waku.example.com`), then connect Waku Mobile to
   `wss://waku.example.com`. (You can add `waku.example.com` to V2Ray's
   `DIRECT` bypass rules.)

3. **Share live sessions with Waku Desktop over SSH (`--state-file`)**

   If your code lives on a remote Linux devbox that Waku Desktop reaches over
   SSH (`waku-daemon bridge`), run `waku-daemon` on that host with `--state-file`
   so both Desktop (SSH) and Android (`wss://`) share the exact same daemon and
   live agent sessions:

   ```sh
   WAKU_DAEMON_TOKEN="<secret>" ~/.cache/waku/server/waku-daemon \
     --bind 127.0.0.1:7420 \
     --state-file ~/.cache/waku/server/daemon.json
   ```

4. **Single-VPN alternative (`sing-box` / `Mihomo`)**

   If your Android proxy client uses `sing-box` (1.8+) or `Mihomo`, you can
   configure Tailscale as an outbound inside the same proxy profile and route
   `100.64.0.0/10` to it while V2Ray handles the rest of your traffic.

Waku is licensed under the repository's GNU GPL v3.0-only license.
