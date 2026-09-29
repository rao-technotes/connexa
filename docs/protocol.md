# Signaling protocol (v1)

JSON text frames over a WebSocket at `/ws` (`wss://` in production). Source of
truth: [`crates/connexa-protocol`](../crates/connexa-protocol/src/lib.rs); the
TypeScript mirror is [`clients/shared/src/protocol.ts`](../clients/shared/src/protocol.ts).

Every frame carries `version` and `type`:

```json
{ "version": 1, "type": "join_room", "room_id": "847291653" }
```

A missing `version` is treated as the current one. Any other version gets an
`unsupported_version` error. Incompatible changes bump `PROTOCOL_VERSION` in
`connexa-core`. Additive changes (new optional fields, new message types) do not.

The server only handles rooms, presence and SDP/ICE relay. Media, chat and
files travel peer-to-peer and never reach it.

## Client → server

| type            | fields                                         | notes |
|-----------------|------------------------------------------------|-------|
| `create_room`   | `display_name?`, `pin?`, `lobby?`, `large?`    | creator becomes host; `large` asks for an SFU room |
| `join_room`     | `room_id`, `display_name?`, `pin?`             | `room_id` may contain spaces/dashes |
| `resume`        | `room_id`, `participant_id`, `resume_token`    | re-attach after a dropped socket |
| `leave_room`    | –                                              | host leaving ends the room |
| `end_room`      | –                                              | host only |
| `admit` / `deny` | `request_id`                                  | host only: answer a lobby request |
| `sdp_offer`     | `target`, `sdp`                                | relayed to `target` only |
| `sdp_answer`    | `target`, `sdp`                                | |
| `ice_candidate` | `target`, `candidate` (`RTCIceCandidateInit`)  | opaque to the server |
| `sfu_offer`     | `sdp`                                          | SFU rooms: publisher connection offer |
| `sfu_answer`    | `sdp`                                          | SFU rooms: subscriber connection answer |
| `sfu_candidate` | `pc` (`pub`\|`sub`), `candidate`               | SFU rooms: trickle ICE |
| `device_hello`  | `public_key` (SPKI, base64), `name?`, `platform?` | start a device-key proof |
| `device_proof`  | `signature` (P1363, base64)                    | ECDSA P-256/SHA-256 over `"connexa-device-auth:" + nonce` |
| `trust_device`  | `device_id`, `trusted`                         | verified devices only |
| `list_trusted_devices` / `get_activity` | –                      | verified devices only |
| `report_event`  | `kind`, `subject?`                             | audit a P2P event (`control_granted`, `file_received`, …) |
| `ping`          | –                                              | keeps the socket and room alive |

## Server → client

| type                 | fields |
|----------------------|--------|
| `room_created`       | `room_id`, `participant_id`, `resume_token`, `max_participants`, `ice_servers`, `topology`, `security` |
| `room_joined`        | `room_id`, `participant_id`, `resume_token`, `host_id`, `participants[]`, `max_participants`, `ice_servers`, `topology`, `security` |
| `lobby_waiting`      | `room_id`: the joiner waits for the host |
| `join_request`       | `request_id`, `display_name`, `device_id?`: to the host |
| `join_request_cancelled` | `request_id` |
| `session_resumed`    | `room_id`, `participant_id`, `host_id`, `participants[]` |
| `participant_joined` | `participant` |
| `participant_left`   | `participant_id` |
| `sdp_offer` / `sdp_answer` | `from`, `sdp` |
| `ice_candidate`      | `from`, `candidate` |
| `sfu_offer` / `sfu_answer` / `sfu_candidate` | as above, from the SFU |
| `device_challenge`   | `nonce` |
| `device_verified`    | `device_id` (`xxxx-xxxx-xxxx-xxxx`) |
| `trusted_devices`    | `devices[]` (`device_id`, `name`, `platform`, `last_seen`) |
| `activity`           | `events[]` (`at`, `kind`, `actor?`, `subject?`) |
| `room_ended`         | `reason`: `host_ended` \| `host_left` \| `expired` \| `server_lost` |
| `error`              | `code`, `message` |
| `pong`               | – |

`topology` is `mesh` or `sfu`; `security` is `{ pin, lobby }`. New error codes:
`pin_required`, `wrong_pin`, `invalid_pin`, `join_denied`, `not_verified`,
`unavailable` (e.g. large rooms without an SFU).

`participants[]` entries are `{ participant_id, display_name, is_host, device_id? }` and
exclude the receiver. `ice_servers` follows the browser `RTCIceServer` shape and
may contain short-lived TURN credentials minted for that participant.

Error codes: `invalid_message`, `unsupported_version`, `invalid_room_code`,
`room_not_found`, `room_full`, `already_in_room`, `not_in_room`, `not_host`,
`target_not_found`, `resume_failed`, `rate_limited`, `server_busy`.

## Session flow

```text
A: create_room            → A: room_created {room_id: 847291653}
B: join_room 847291653    → B: room_joined {participants: [A]}
                          → A: participant_joined {B}
A and B each create an RTCPeerConnection for the other and negotiate:
A: sdp_offer target=B     → B: sdp_offer from=A
B: sdp_answer target=A    → A: sdp_answer from=B
both: ice_candidate …     → relayed
                          ═══ WebRTC P2P: audio / video / screen / data ═══
```

Clients use the [perfect negotiation](https://w3c.github.io/webrtc-pc/#perfect-negotiation-example)
pattern: either side may send an offer at any time (e.g. when starting a screen
share). The peer with the lexically greater `participant_id` is *polite*: on an
offer collision it rolls back its own offer and accepts the other.

## Lobby and PIN

With a PIN, `join_room` must carry it (`pin_required` / `wrong_pin`). After
5 wrong guesses the room pauses new joins for 5 minutes. With a lobby, the
joiner gets `lobby_waiting` and the host a `join_request`. The host answers
with `admit` or `deny`, and the joiner receives `room_joined` or a
`join_denied` error. Joiners whose verified device the host trusts are
admitted directly.

## SFU rooms

Each participant keeps two connections with the server's SFU. On the
**publisher** connection the client offers (`sfu_offer`), and it carries
upstream media and a `connexa` data channel sending `{to?, msg}`. On the
**subscriber** connection the SFU offers, and it carries everyone else's
tracks, whose stream ids are `<owner>~<original stream id>`, plus a `connexa`
channel delivering `{from, msg}`. The peer messages below are identical in
both topologies. Extra channels labelled `relay:<target>:<label>` reach the
target as `from:<sender>:<label>`.

## Reconnection

If the WebSocket drops, the participant keeps its slot for the reconnect grace
period (30 s by default). Established WebRTC connections keep running. Messages
addressed to the participant are queued, then replayed after a successful
`resume`. If the participant doesn't resume in time, the others get
`participant_left`, or `room_ended {host_left}` if it was the host.

## Peer-to-peer data channel

Each peer connection has a negotiated data channel (`id: 0`, label `connexa`)
carrying JSON:

| `t`      | fields                             | purpose |
|----------|------------------------------------|---------|
| `chat`   | `id`, `text`, `ts`                 | chat message |
| `stream` | `streamId`, `kind` (`camera`\|`screen`) | labels an incoming MediaStream |
| `state`  | `mic`, `cam`, `screen`, `control`  | sender's media toggles; `control` = their shared screen can be remote-controlled |
| `clipboard` | `text`                          | clipboard text shared by the sender |
| `file-offer` | `id`, `name`, `size`, `mime`   | offer a file (receiver must accept) |
| `file-accept` / `file-reject` / `file-cancel` | `id` | transfer decisions |
| `control-request` | `permissions[]`           | viewer asks to control the sender's screen |
| `control-grant` | `permissions[]`             | host approved (after a native confirmation) |
| `control-deny` | `reason` (`declined`\|`unsupported`) | host refused, or can't be controlled |
| `control-revoke` / `control-release` | –      | host / viewer ends control |
| `input`  | `e` + event fields (below)         | remote input, only acted on while granted |
| `file-done` | `id`                            | receiver has every byte (the sender then marks the file sent) |
| `nscreen-offer` | `sdp`, `streamId`           | a native (Android) screen share offers a receive-only side link |
| `nscreen-answer` / `nscreen-ice` | `sdp` / `candidate` | side-link negotiation |
| `nscreen-stop` | –                            | the native screen share ended |

`permissions` are drawn from `mouse`, `keyboard`, `clipboard`. Input events:

| `e`      | fields |
|----------|--------|
| `move`   | `x`, `y` (0–1, normalized to the shared screen) |
| `down` / `up` | `x`, `y`, `button` (`left`\|`middle`\|`right`) |
| `wheel`  | `dx`, `dy` (pixels, as in `WheelEvent`) |
| `key`    | `code` (`KeyboardEvent.code`), `down` |

Keys are sent as physical key codes, so typing follows the host's keyboard layout.

### File channels

After `file-accept`, the sender opens an extra data channel labelled
`file:<id>` and streams 16 KiB binary chunks with backpressure. The transfer
completes when `size` bytes have arrived. Either side closing the channel or
sending `file-cancel` aborts it.

Peers treat each other's data as untrusted and validate every field.
