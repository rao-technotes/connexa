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
| `create_room`   | `display_name?`                                | creator becomes host |
| `join_room`     | `room_id`, `display_name?`                     | `room_id` may contain spaces/dashes |
| `resume`        | `room_id`, `participant_id`, `resume_token`    | re-attach after a dropped socket |
| `leave_room`    | –                                              | host leaving ends the room |
| `end_room`      | –                                              | host only |
| `sdp_offer`     | `target`, `sdp`                                | relayed to `target` only |
| `sdp_answer`    | `target`, `sdp`                                | |
| `ice_candidate` | `target`, `candidate` (`RTCIceCandidateInit`)  | opaque to the server |
| `ping`          | –                                              | keeps the socket and room alive |

## Server → client

| type                 | fields |
|----------------------|--------|
| `room_created`       | `room_id`, `participant_id`, `resume_token`, `max_participants`, `ice_servers` |
| `room_joined`        | `room_id`, `participant_id`, `resume_token`, `host_id`, `participants[]`, `max_participants`, `ice_servers` |
| `session_resumed`    | `room_id`, `participant_id`, `host_id`, `participants[]` |
| `participant_joined` | `participant` |
| `participant_left`   | `participant_id` |
| `sdp_offer` / `sdp_answer` | `from`, `sdp` |
| `ice_candidate`      | `from`, `candidate` |
| `room_ended`         | `reason`: `host_ended` \| `host_left` \| `expired` |
| `error`              | `code`, `message` |
| `pong`               | – |

`participants[]` entries are `{ participant_id, display_name, is_host }` and
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
| `state`  | `mic`, `cam`, `screen`             | sender's media toggles |

Peers treat each other's data as untrusted and validate every field.
