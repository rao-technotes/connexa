# Security model

## What the room code is (and isn't)

The 9-digit code is a short-lived **room identifier**, not a key. It's generated
from the OS CSPRNG (`getrandom`) with rejection sampling: 900 million values, no
leading zero. A room's code is invalidated when the room ends. Media is
encrypted by WebRTC (DTLS-SRTP) no matter what the code is.

Anyone who knows an active code can join, up to the room's capacity. To contain
that risk:

- **Rate limiting.** Each IP gets 20 join/resume attempts and 10 room creations
  per minute (configurable). With 1,000 live rooms, one IP guessing nonstop at
  that limit hits *some* room about once a month. An attacker with many IPs
  does proportionally better. A code alone is fine for casual sessions, but it
  isn't strong access control, hence the PIN/lobby plans below.
- **Small rooms.** The MVP caps rooms at 3 participants.
- **Short lifetimes.** Rooms close when the host leaves or ends them, after 30
  minutes without signaling activity, or after 12 hours regardless.
- **Redacted logs.** Codes appear only as `847***653`. SDP, ICE candidates,
  names and chat are never logged.

Planned for high-security sessions: an optional host-set PIN, host approval of
each joiner (a "lobby"), and verifying DTLS fingerprints with a short
authentication string shown to both users.

## Transport

- Signaling uses `wss://` in production. TLS terminates at the reverse proxy
  (Caddy in `infrastructure/deployment`).
- `CONNEXA_ALLOWED_ORIGINS` restricts which web origins and extension IDs may
  open the WebSocket.
- Frames are limited to 64 KiB. A connection is closed after 60 s without any
  frame. A client that falls 256 messages behind stops receiving messages.
- `resume_token` is a 128-bit random secret, compared in constant time.
- Relay messages (`sdp_*`, `ice_candidate`) reach only a participant in the
  sender's own room. `from` is set by the server and can't be spoofed.

## TURN

The server gives each participant time-limited TURN credentials
(`username = <expiry>:<participant_id>`, `credential = HMAC-SHA1(secret, username)`,
the coturn REST scheme) inside `room_created` / `room_joined`. Only people in a
room get credentials, and they expire (6 h by default). The provided coturn
config blocks relaying to private address ranges and sets per-user quotas.

## Clients

- Mic, camera and screen are off by default. Each one needs the browser's own
  permission prompt, and screen sharing always shows the OS/browser picker.
- Data-channel messages from peers are validated and rendered as text, never as
  HTML.
- The Chrome extension asks only for `storage`. It can't control the OS
  keyboard or mouse; only the desktop app can.
- Files arrive only after the receiver clicks **Accept**. File names are
  sanitized, and files are capped at 1 GB (200 MB on Android).
- A shared clipboard is shown with a **Copy** button. It's written to the OS
  clipboard automatically only when the host granted `clipboard` to that
  participant.

## Remote control

Remote control is layered so that no single mistake grants control:

1. **Capability.** Only the Windows desktop app can inject input, and only
   while its user shares an **entire screen**. It advertises this with
   `state.control`. Browsers, the extension and Android can view and request,
   but can't be controlled.
2. **In-app request.** A viewer's `control-request` opens a dialog on the host
   with granular checkboxes: mouse, keyboard, clipboard (clipboard is off by
   default).
3. **Native confirmation.** Clicking *Allow* calls the Rust `control_grant`
   command, which shows a Windows `MessageBox`. It defaults to **No** and names
   the person, the permissions and the screen. Page script can't click it, so
   even a compromised web layer can't grant control on its own.
4. **Native enforcement.** Every `input` event is checked again in Rust by
   `ControlGate` against that participant's grant. Events are clamped to the
   shared monitor, and unknown keys are rejected.
5. **Always revocable.** While anyone has control, a red banner shows who and
   what, with a **Stop control** button. Control also ends when the host stops
   sharing, the viewer releases it, either side leaves, or the window closes.
   Revoking releases any keys or mouse buttons the remote side held, so
   nothing stays stuck.

Windows itself blocks injected input into elevated (administrator) windows and
the secure desktop (UAC prompts, Ctrl+Alt+Del, the lock screen). This is
intentional and not worked around.
