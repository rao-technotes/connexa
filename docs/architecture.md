# Connexa architecture

The core rule: **signaling, media and remote control are separate layers.**

| layer | responsibility | where |
|---|---|---|
| Signaling | rooms, presence, SDP/ICE relay, reconnect | Rust `Hub`: cloud server or embedded in the desktop app (LAN) |
| WebRTC | audio, video, screen, chat, files, clipboard, control messages | peer-to-peer between clients |
| Native agent | mouse and keyboard injection, clipboard, file saving | `connexa-agent` in the Windows app; file saving on Android |
| Infrastructure | STUN, TURN, TLS, deployment | `infrastructure/` |

Every client speaks the same versioned protocol ([protocol.md](protocol.md))
and runs the same TypeScript client, so browser, extension, Windows and Android
users can share one room.

```text
                    clients/shared  (TypeScript: signaling, mesh, files, control, UI)
        ┌──────────────┬──────────────┬──────────────┬──────────────┐
        Web page      Chrome ext.    Windows app    Android app
        (browser)     (MV3 tab)      (Tauri +       (WebView +
                                     WebView2)      Kotlin shell)
                                        │               │
                                   connexa-agent    save to Downloads,
                                   input, clipboard camera/mic permissions,
                                   LAN server       native screen capture
                                   + mDNS           (MediaProjection)
```

## Repository layout

```text
crates/
  connexa-core        constants (protocol version, room code length, limits)
  connexa-protocol    wire types (serde), versioned envelope, encode/decode
  connexa-security    room codes, tokens, rate limiter, TURN credentials, redaction
  connexa-signaling   transport-agnostic room Hub: rooms, lobby, PIN, device proofs,
                      audit/SFU events, metrics (cloud server and LAN mode)
  connexa-agent       permission-gated input injection (SendInput), clipboard
  connexa-sfu         selective forwarding unit for large rooms (webrtc-rs)
apps/
  signaling-server    Axum WebSocket transport, web client, /healthz, /metrics,
                      store (memory/Postgres), cluster (Redis), SFU adapter
  desktop/            Windows app (Tauri 2): native commands, LAN server, mDNS
clients/
  shared/             TypeScript: protocol, SignalingClient, PeerMesh, FileTransfers,
                      RemoteControl, UI
  web/                browser client (served by the signaling server)
  desktop/            desktop entry: Internet / LAN modes, Tauri bridge
  chrome-extension/   Manifest V3 popup + session tab + settings
  android/            Android app (Gradle/Kotlin) + its web entry (android/web)
infrastructure/       Docker image, compose (Caddy + signaling + coturn), coturn config
.github/workflows/    CI: tests, lint, extension, APK, Windows installer, releases
docs/                 architecture, protocol, security, networking, screenshots
```

## Server internals

`Hub` keeps rooms in a `DashMap<code, Room>`. Each connection gets a bounded
outbound channel. The hub only uses non-blocking `try_send`, so one slow client
can't stall a room. A sweeper runs every 5 s. It removes participants whose
reconnect grace has run out and closes idle or expired rooms. The server needs
no database, Redis or broker: restarting it ends all rooms, and clients return
to the home screen.

The desktop app embeds the same server library (`serve_until`) for LAN mode.

To scale horizontally later, route by room code (consistent hashing at the
proxy) or move room state into Redis pub/sub. Neither changes the protocol.

## Client internals

- `SignalingClient` handles the WebSocket, create/join requests, keep-alive
  pings, automatic reconnect with `resume`, and an outbox for messages sent
  while the socket is down.
- `PeerMesh` keeps one `RTCPeerConnection` per remote participant, uses perfect
  negotiation, and sets up a negotiated control data channel. Feature modules
  receive peer messages as `message` events and can open extra channels.
- `FileTransfers` handles offer → accept → a dedicated `file:<id>` channel with
  backpressure, plus cancel.
- `RemoteControl` handles the viewer and host roles of remote control (see
  [security.md](security.md#remote-control)).
- `app.ts` is the UI. Each shell mounts it with a `NativeAgent` that describes
  what the platform can do (control, clipboard, saving files).

### Windows app

`apps/desktop` is a Tauri 2 app. The window loads `clients/desktop` (the shared
UI plus Internet/LAN mode switching). Rust commands (`agent_info`, `monitors`,
`control_grant`, `control_input`, `control_revoke`, `clipboard_*`,
`save_file`, `qr_svg`, `lan_start`, `lan_stop`, `lan_discover`) provide the
native side. Screen capture uses WebView2's `getDisplayMedia`, which is backed
by Windows Graphics Capture, and Chromium's WebRTC video encoders.

### Android app

`clients/android` is a Kotlin shell around a WebView. The client is bundled in
`assets/www` and served from `https://appassets.androidplatform.net` through
`WebViewAssetLoader`, which counts as a secure context, so the camera and
microphone work. The shell maps WebRTC permission requests to Android runtime
permissions, handles the file picker, and saves received files to Downloads
through a small JS bridge.

Android WebView has no `getDisplayMedia`, so screen sharing is native:
`NativeScreenShare.kt` captures with MediaProjection (in a `mediaProjection`
foreground service, as Android 14 requires) and sends it with Google's WebRTC
library over a separate send-only **side link** to each viewer. The page
relays that link's offer, answer and ICE as peer messages (`nscreen-*`), so it
works in mesh and SFU rooms alike. Viewers show it as a normal screen tile.

## Roadmap status

| phase | scope | status |
|---|---|---|
| 1 | Rust signaling + extension: rooms, presence, SDP/ICE, disconnects | ✅ |
| 2 | WebRTC P2P, data channel, connection state, reconnection | ✅ |
| 3 | Screen / window / tab sharing | ✅ |
| 4 | Microphone, camera, mute, camera toggle | ✅ |
| 5 | Three-participant mesh + participant UI | ✅ |
| 6 | Chat over data channels | ✅ |
| 7 | Windows EXE: screen capture, clipboard, file transfer | ✅ Tauri app + NSIS installer |
| 8 | Remote control: mouse, keyboard, permissions, request/approve/revoke | ✅ Windows hosts |
| 9 | Android client on the same protocol | ✅ including native screen sharing |
| 10 | LAN mode: EXE hosts a temporary server, mDNS discovery, QR | ✅ |
| 11 | STUN/TURN + Docker deployment | ✅ |
| 12 | Scaling: SFU, Redis clustering, load balancing, Kubernetes, monitoring, database | ✅ |
| + | Session security: lobby, PIN, device identity, trusted devices, audit log | ✅ |

Phase 12 in detail:

- **SFU:** `connexa-sfu`, used for rooms created as *Large meeting*
  ([networking.md](networking.md#topology)).
- **Scale-out:** Redis-backed multi-node signaling with slot-routed room codes
  and no sticky sessions ([networking.md](networking.md#multiple-signaling-nodes)).
- **Database:** Postgres for device identities, trust and the audit log.
- **Operations:** Prometheus metrics, a Grafana dashboard, and Kubernetes and
  Compose deployments ([deployment.md](deployment.md)).

Next candidates: macOS and Linux desktop builds (the Tauri shell is portable,
but input injection needs a platform backend), simulcast in the SFU so
viewers get quality matched to their bandwidth, end-to-end encryption through
the SFU (SFrame), and resumable file transfers.
