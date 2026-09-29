# Networking

## Topology

| room | topology | media path | size |
|---|---|---|---|
| normal | **mesh** | every participant ↔ every other participant, P2P | up to 3 (`CONNEXA_MAX_PARTICIPANTS`) |
| *Large meeting* | **SFU** | every participant ↔ the server's SFU | up to 25 (`CONNEXA_SFU_MAX_PARTICIPANTS`) |

In a mesh each sender uploads one copy per peer, which is fine for three
people. In an SFU room each participant uploads once, and the SFU
(`crates/connexa-sfu`, written in Rust) forwards packets to everyone else
without transcoding. Keyframe requests are passed back to the sender.
Browsers cap camera video to 600 kbps and screen shares to 2.5 Mbps in SFU
rooms, so a 25-person room stays within reach of a single server. All SFU
media uses one UDP port (`CONNEXA_SFU_UDP_PORT`, default 3479). Behind 1:1 NAT
(cloud VMs, Kubernetes), set `CONNEXA_SFU_PUBLIC_IP`.

The SFU advertises IPv4 only and always acts as the DTLS client. Current
browsers offer post-quantum key-exchange groups that webrtc-rs's DTLS server
rejects, and its client handshake interoperates.

## Multiple signaling nodes

```text
            load balancer (no sticky sessions)
            /              |                      node 1          node 2          node 3      ← CONNEXA_NODE_SLOT 1..9
           \______________ Redis pub/sub _____________/
```

A room lives on one node, and the first digit of its code is that node's
slot. A client can connect to any node: when it joins a room owned elsewhere,
its node forwards its messages over Redis (`connexa:node:<slot>`) and relays
replies back. Nodes heartbeat every 3 s. If a room's node disappears, its
clients get `room_ended { server_lost }`. If a client's node disappears, the
owner treats the client as disconnected, and the reconnect grace applies.
Rate limits are per node. On Kubernetes, the slot comes from the StatefulSet
ordinal automatically.

Media never crosses nodes. In SFU rooms, clients connect their media to the
room owner's SFU, so every node running the SFU needs its own reachable UDP
port.

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

## LAN mode

The desktop app can host a session with no Internet server at all:

```text
Host EXE (192.168.1.10)
  ├── temporary signaling server  :47800  (same Hub + web client as the cloud server)
  ├── mDNS advert  _connexa._tcp.local.
  └── WebRTC peer
        ▲            ▲             ▲
     Desktop      Browser       Android
   (finds it     (opens the     (server URL
   via mDNS)     link / QR)     ws://192.168.1.10:47800/ws)
```

- **LAN mode → Host a session on this network** starts the embedded server on
  port 47800 (or a free port), creates the room, and shows an invite link and a
  QR code.
- Other desktop apps see the host under **Nearby sessions**, found by mDNS, and
  still need the 9-digit code to join. The advert contains only a display name
  and port, never the code.
- The server shuts down when the host's meeting ends or the app closes.
- No STUN or TURN is configured: peers on one LAN connect through host
  candidates.
- Windows asks once to allow Connexa through the firewall on private networks.

Browsers that open the LAN link (`http://192.168.x.x:47800`) can watch screens,
chat, exchange files and request control. They can't send their own camera,
mic or screen, because browsers allow that only on HTTPS. Use the desktop or
Android app for two-way media on a LAN.

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
| `CONNEXA_DATABASE_URL` | – | Postgres for devices, trust and audit log (in memory if unset) |
| `CONNEXA_AUDIT_RETENTION_DAYS` | `90` | audit log retention |
| `CONNEXA_REDIS_URL` | – | enables multi-node clustering |
| `CONNEXA_NODE_SLOT` | from `HOSTNAME` | cluster slot 1–9 (StatefulSet `-N` → N+1) |
| `CONNEXA_METRICS_TOKEN` | – | bearer token for `/metrics` (open if unset) |
| `CONNEXA_SFU` | `false` | enable large (SFU) rooms |
| `CONNEXA_SFU_UDP_PORT` | `3479` | SFU media port (UDP) |
| `CONNEXA_SFU_PUBLIC_IP` | – | IP advertised by the SFU behind NAT |
| `CONNEXA_SFU_MAX_PARTICIPANTS` | `25` | large room capacity |
| `RUST_LOG` | `info` | log filter |
