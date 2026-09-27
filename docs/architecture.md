# Connexa architecture

The core rule: **signaling, media and remote control are separate layers.**

| layer | responsibility | where |
|---|---|---|
| Signaling | rooms, presence, SDP/ICE relay, reconnect | Rust server (`/ws`) |
| WebRTC | audio, video, screen, chat, (files) | peer-to-peer between clients |
| Native agent | mouse, keyboard, clipboard, OS capture | Windows / Android apps (planned) |
| Infrastructure | STUN, TURN, TLS, deployment | `infrastructure/` |

Because every client speaks the same versioned protocol
([protocol.md](protocol.md)), browser, extension, Windows and Android clients
can share one room.

## Repository layout

```text
crates/
  connexa-core        constants (protocol version, room code length, limits)
  connexa-protocol    wire types (serde), versioned envelope, encode/decode
  connexa-security    room codes, tokens, rate limiter, TURN credentials, redaction
  connexa-signaling   transport-agnostic room Hub (embeddable for LAN mode)
apps/
  signaling-server    Axum WebSocket transport + static web client + /healthz
clients/
  shared/             TypeScript: protocol mirror, SignalingClient, PeerMesh, UI
  web/                browser client (served by the signaling server)
  chrome-extension/   Manifest V3 popup + session tab + settings
infrastructure/
  docker/             container image (server + web client)
  deployment/         docker compose: Caddy (TLS) + signaling + coturn
  turn/               coturn config
docs/                 architecture, protocol, security, networking
```

## Server internals

`Hub` keeps rooms in a `DashMap<code, Room>`. Each connection gets a bounded
outbound channel. The hub only uses non-blocking `try_send`, so one slow client
can't stall a room. A sweeper runs every 5 s. It removes participants whose
reconnect grace has run out and closes idle or expired rooms. The server needs
no database, Redis or broker: restarting it ends all rooms, and clients return
to the home screen.

To scale horizontally later, route by room code (consistent hashing at the
proxy) or move room state into Redis pub/sub. Neither changes the protocol.

## Client internals

- `SignalingClient` handles the WebSocket, create/join requests, keep-alive
  pings, automatic reconnect with `resume`, and an outbox for messages sent
  while the socket is down.
- `PeerMesh` keeps one `RTCPeerConnection` per remote participant, uses perfect
  negotiation, and sets up a negotiated data channel for chat and metadata.
  Mic, camera and screen tracks are added and removed dynamically. Screen
  tracks use `contentHint = "detail"` and `maintain-resolution` so text stays
  sharp.
- `app.ts` is the UI. The web page and the extension's session tab mount the
  same UI. Only the signaling URL differs.

The extension popup only collects create/join input. The session runs in a
full extension tab, because popups close when they lose focus and can't host
a call.

## Roadmap status

| phase | scope | status |
|---|---|---|
| 1 | Rust signaling + extension: rooms, presence, SDP/ICE, disconnects | ✅ |
| 2 | WebRTC P2P, data channel, connection state, reconnection | ✅ |
| 3 | Screen / window / tab sharing | ✅ browser |
| 4 | Microphone, camera, mute, camera toggle | ✅ browser |
| 5 | Three-participant mesh + participant UI | ✅ |
| 6 | Chat over data channels | ✅ |
| 7 | Windows EXE (native capture, clipboard, files) | planned |
| 8 | Remote control with a permission system | planned |
| 9 | Android client | planned |
| 10 | LAN mode (EXE embeds `connexa-signaling`) | planned; hub is embeddable |
| 11 | STUN/TURN + Docker deployment | ✅ config provided |
| 12 | Scaling (Redis, SFU, LB, monitoring) | deliberately deferred |
