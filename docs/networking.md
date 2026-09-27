# Networking

## Topology

Rooms of up to 3 use a full WebRTC mesh: every participant has one
`RTCPeerConnection` to each other participant (A↔B, A↔C, B↔C). No media server
is involved. Past about 4–5 participants, upload bandwidth grows linearly per
sender, and that's when to evaluate an SFU. The protocol already carries
`max_participants`, so clients adapt once the server allows larger rooms.

## Connectivity

```text
client ──wss──► signaling (room, presence, SDP/ICE relay)
client ◄══════ WebRTC ══════► client     direct P2P (host / srflx candidates via STUN)
client ◄══► TURN relay ◄══► client       fallback when NATs/firewalls block direct paths
```

- **STUN** lets each client find its public address. Set it with
  `CONNEXA_STUN_URLS` (default `stun:stun.l.google.com:19302`).
- **TURN** is used only when ICE finds no direct path. Set it with
  `CONNEXA_TURN_URLS` and `CONNEXA_TURN_SECRET` (or static
  `CONNEXA_TURN_USERNAME` / `CONNEXA_TURN_CREDENTIAL`).
- The participants panel shows each link as **P2P** (direct) or **Relay**
  (TURN), read from the selected ICE candidate pair in `getStats()`.
- If ICE fails, the client calls `restartIce()` automatically.

Expect roughly 10–20% of real-world connections to need TURN (symmetric NATs,
corporate firewalls). Offer TURN over TCP/443 as well if clients sit behind
strict firewalls.

## Secure-context requirement

Browsers expose `getUserMedia` and `getDisplayMedia` only in secure contexts:
`https://` or `http://localhost`. To test across devices on a LAN, put the
server behind HTTPS (see `infrastructure/deployment`). A plain `http://192.168.x.x`
page can signal and chat, but can't use the mic, camera or screen share.

## LAN mode (planned, Phase 10)

`connexa-signaling` has no transport dependencies, so a desktop client can embed
the same `Hub` and serve `/ws` on the LAN. On a LAN, ICE host candidates connect
directly and need neither STUN nor TURN.

## Server configuration

| variable | default | meaning |
|---|---|---|
| `CONNEXA_BIND` | `0.0.0.0:8080` | listen address |
| `CONNEXA_WEB_ROOT` | `clients/web/dist` | built web client |
| `CONNEXA_MAX_PARTICIPANTS` | `3` | room capacity (≥ 2) |
| `CONNEXA_MAX_ROOMS` | `10000` | global room cap |
| `CONNEXA_ROOM_IDLE_TIMEOUT_SECS` | `1800` | close rooms with no signaling activity |
| `CONNEXA_ROOM_MAX_LIFETIME_SECS` | `43200` | hard room lifetime |
| `CONNEXA_RECONNECT_GRACE_SECS` | `30` | slot kept after a socket drop |
| `CONNEXA_JOIN_ATTEMPTS_PER_MINUTE` | `20` | per-IP join/resume limit |
| `CONNEXA_CREATES_PER_MINUTE` | `10` | per-IP create limit |
| `CONNEXA_SOCKET_IDLE_TIMEOUT_SECS` | `60` | close silent sockets |
| `CONNEXA_ALLOWED_ORIGINS` | *(any)* | comma-separated WebSocket origins |
| `CONNEXA_TRUST_PROXY` | `false` | use `X-Forwarded-For` for rate limiting |
| `CONNEXA_STUN_URLS` | Google STUN | comma-separated |
| `CONNEXA_TURN_URLS` | – | comma-separated `turn:` / `turns:` URLs |
| `CONNEXA_TURN_SECRET` | – | coturn `static-auth-secret` |
| `CONNEXA_TURN_USERNAME` / `CONNEXA_TURN_CREDENTIAL` | – | static TURN credentials |
| `CONNEXA_TURN_TTL_SECS` | `21600` | TURN credential lifetime |
| `RUST_LOG` | `info` | log filter |
