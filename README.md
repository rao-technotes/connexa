# Connexa

Cross-platform peer-to-peer meetings and remote support built around a
temporary **9-digit session code**:

> Start a session → get `847 291 653` → share it → others enter it → connected.

Audio, video, screen sharing and chat run directly between participants over
WebRTC. The Rust server only coordinates rooms and relays connection setup.

## Screenshots

Captured from a real 3-person session (Alice, Bob, Carol) running against the
local server. Chrome's built-in fake camera stands in for real webcams, and the
shared "screen" is a generated slide.

| Start or join | Three people, P2P chat |
|---|---|
| ![Home screen](docs/screenshots/home.png) | ![Three-person session](docs/screenshots/session.png) |

| Screen sharing (viewer's side) | Phone layout | Chrome extension |
|---|---|---|
| ![Screen sharing](docs/screenshots/screen-share.png) | <img src="docs/screenshots/mobile.png" alt="Mobile view" width="260"> | <img src="docs/screenshots/extension-popup.png" alt="Extension popup" width="260"> |

## What works today

- Rust signaling server: create and join 9-digit rooms, presence, SDP/ICE relay,
  host end, room expiry, rate limiting, reconnect with session resume
- Browser client, served by the server itself, with nothing to install
- Chrome extension (Manifest V3) using the same client
- WebRTC mesh for up to 3 people: mic, camera, screen/window/tab sharing, P2P chat
- Each link labelled **P2P** (direct) or **Relay** (TURN)
- Docker deployment with automatic HTTPS (Caddy) and a TURN relay (coturn)

The Windows EXE, remote control, Android and LAN mode come next. See the
roadmap in [docs/architecture.md](docs/architecture.md).

## Run locally

Requirements: Rust (stable, edition 2024) and Node 18+.

```bash
cd clients && npm install && npm run build && cd ..
cargo run -p signaling-server
```

Open http://localhost:8080 in two browser windows. Click **Start session** in
one and enter the code in the other.

### Chrome extension

1. `cd clients && npm run build:extension`
2. Open `chrome://extensions`, turn on **Developer mode**, click **Load
   unpacked** and pick `clients/chrome-extension/dist`.
3. The extension uses `ws://localhost:8080/ws` by default. Change it under
   **Server settings** in the popup.

Invite links from the extension point at the web client on the same server,
so invitees don't need the extension.

### Tests

```bash
cargo test --workspace
```

```bash
npm --prefix clients run typecheck
```

## Deploy

See [infrastructure/deployment](infrastructure/deployment): copy
`.env.example` to `.env`, fill it in, then run `docker compose up -d --build`.
Browsers need HTTPS for the camera, mic and screen sharing anywhere except
`localhost`.

## Docs

- [Architecture](docs/architecture.md)
- [Signaling protocol](docs/protocol.md)
- [Security model](docs/security.md)
- [Networking, STUN/TURN and configuration](docs/networking.md)
