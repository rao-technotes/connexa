//! Wire format of the Connexa signaling protocol.
//!
//! Every frame is a JSON object carrying a `version` and a `type`, e.g.
//! `{ "version": 1, "type": "join_room", "room_id": "847291653" }`.
//!
//! The signaling server coordinates rooms, relays SDP / ICE for mesh rooms and
//! negotiates with the SFU for large rooms. Chat and file data never pass
//! through it (in SFU rooms they pass through the SFU's data channels).
//!
//! Changes are additive within a version: new message types and new optional
//! fields. Incompatible changes bump [`PROTOCOL_VERSION`].

use serde::{Deserialize, Serialize};

pub use connexa_core::PROTOCOL_VERSION;

pub type ParticipantId = String;

/// A frame on the wire: protocol version plus the message body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(flatten)]
    pub body: T,
}

fn default_version() -> u32 {
    PROTOCOL_VERSION
}

impl<T> Envelope<T> {
    pub fn new(body: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            body,
        }
    }
}

/// Messages sent by clients to the signaling server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    CreateRoom {
        #[serde(default)]
        display_name: Option<String>,
        /// Optional 4–8 digit PIN joiners must also enter.
        #[serde(default)]
        pin: Option<String>,
        /// Joiners wait until the host admits them.
        #[serde(default)]
        lobby: bool,
        /// Ask for a large (SFU) room instead of a peer-to-peer mesh.
        #[serde(default)]
        large: bool,
    },
    JoinRoom {
        room_id: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        pin: Option<String>,
    },
    /// Re-attach to a room after the WebSocket dropped, within the grace period.
    Resume {
        room_id: String,
        participant_id: ParticipantId,
        resume_token: String,
    },
    /// Leave the room (or stop waiting in its lobby).
    LeaveRoom,
    /// Host only: close the room for everyone.
    EndRoom,
    /// Host only: let a waiting joiner in.
    Admit {
        request_id: String,
    },
    /// Host only: turn a waiting joiner away.
    Deny {
        request_id: String,
    },
    SdpOffer {
        target: ParticipantId,
        sdp: String,
    },
    SdpAnswer {
        target: ParticipantId,
        sdp: String,
    },
    /// `candidate` is opaque to the server (an `RTCIceCandidateInit` from browsers).
    IceCandidate {
        target: ParticipantId,
        candidate: serde_json::Value,
    },
    /// SFU rooms: offer for the publisher connection (client → SFU media).
    SfuOffer {
        sdp: String,
    },
    /// SFU rooms: answer for the subscriber connection (SFU → client media).
    SfuAnswer {
        sdp: String,
    },
    SfuCandidate {
        pc: SfuPc,
        candidate: serde_json::Value,
    },
    /// Start proving possession of a device key (ECDSA P-256, SPKI, base64).
    DeviceHello {
        public_key: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        platform: Option<String>,
    },
    /// Signature (IEEE P1363, base64) over `"connexa-device-auth:" + nonce`.
    DeviceProof {
        signature: String,
    },
    /// Mark another device as trusted (skips the lobby) or untrusted.
    TrustDevice {
        device_id: String,
        trusted: bool,
    },
    ListTrustedDevices,
    /// This device's recent activity (audit log).
    GetActivity,
    /// Record a peer-to-peer event (e.g. remote control granted) in the audit log.
    ReportEvent {
        kind: String,
        #[serde(default)]
        subject: Option<ParticipantId>,
    },
    Ping,
}

/// Messages sent by the signaling server to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    RoomCreated {
        room_id: String,
        participant_id: ParticipantId,
        resume_token: String,
        max_participants: usize,
        ice_servers: Vec<IceServer>,
        #[serde(default)]
        topology: Topology,
        #[serde(default)]
        security: RoomSecurity,
    },
    RoomJoined {
        room_id: String,
        participant_id: ParticipantId,
        resume_token: String,
        host_id: ParticipantId,
        /// Everyone already in the room, excluding the receiver.
        participants: Vec<ParticipantInfo>,
        max_participants: usize,
        ice_servers: Vec<IceServer>,
        #[serde(default)]
        topology: Topology,
        #[serde(default)]
        security: RoomSecurity,
    },
    /// The joiner is waiting for the host to admit them.
    LobbyWaiting {
        room_id: String,
    },
    /// Host: someone is waiting in the lobby.
    JoinRequest {
        request_id: String,
        display_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
    },
    /// Host: a waiting joiner gave up or disconnected.
    JoinRequestCancelled {
        request_id: String,
    },
    SessionResumed {
        room_id: String,
        participant_id: ParticipantId,
        host_id: ParticipantId,
        participants: Vec<ParticipantInfo>,
    },
    ParticipantJoined {
        participant: ParticipantInfo,
    },
    ParticipantLeft {
        participant_id: ParticipantId,
    },
    SdpOffer {
        from: ParticipantId,
        sdp: String,
    },
    SdpAnswer {
        from: ParticipantId,
        sdp: String,
    },
    IceCandidate {
        from: ParticipantId,
        candidate: serde_json::Value,
    },
    /// SFU rooms: offer for the subscriber connection.
    SfuOffer {
        sdp: String,
    },
    /// SFU rooms: answer for the publisher connection.
    SfuAnswer {
        sdp: String,
    },
    SfuCandidate {
        pc: SfuPc,
        candidate: serde_json::Value,
    },
    DeviceChallenge {
        nonce: String,
    },
    DeviceVerified {
        device_id: String,
    },
    TrustedDevices {
        devices: Vec<DeviceSummary>,
    },
    Activity {
        events: Vec<ActivityEvent>,
    },
    RoomEnded {
        reason: RoomEndReason,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    Pong,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParticipantInfo {
    pub participant_id: ParticipantId,
    pub display_name: String,
    pub is_host: bool,
    /// Present when the participant proved possession of a device key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// Mirrors the browser `RTCIceServer` dictionary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// How media flows in a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// Every participant connects to every other one (small rooms).
    #[default]
    Mesh,
    /// Every participant connects to the server's SFU, which forwards media.
    Sfu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoomSecurity {
    pub pin: bool,
    pub lobby: bool,
}

/// Which of the two SFU connections an ICE candidate belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SfuPc {
    #[serde(rename = "pub")]
    Publisher,
    #[serde(rename = "sub")]
    Subscriber,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    /// Unix seconds.
    pub last_seen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// Unix seconds.
    pub at: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomEndReason {
    HostEnded,
    HostLeft,
    Expired,
    /// The server hosting the room became unreachable (clustered deployments).
    ServerLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidMessage,
    UnsupportedVersion,
    InvalidRoomCode,
    RoomNotFound,
    RoomFull,
    AlreadyInRoom,
    NotInRoom,
    NotHost,
    TargetNotFound,
    ResumeFailed,
    RateLimited,
    ServerBusy,
    PinRequired,
    WrongPin,
    JoinDenied,
    InvalidPin,
    /// Device proof missing or invalid.
    NotVerified,
    /// The feature is not enabled on this server (e.g. SFU, database).
    Unavailable,
}

impl ServerMessage {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code,
            message: message.into(),
        }
    }
}

/// Parse a client frame, rejecting unknown protocol versions.
#[allow(clippy::result_large_err)] // the error is sent straight back to the client
pub fn decode_client(text: &str) -> Result<ClientMessage, ServerMessage> {
    let envelope: Envelope<ClientMessage> = serde_json::from_str(text).map_err(|e| {
        ServerMessage::error(ErrorCode::InvalidMessage, format!("invalid message: {e}"))
    })?;
    if envelope.version != PROTOCOL_VERSION {
        return Err(ServerMessage::error(
            ErrorCode::UnsupportedVersion,
            format!(
                "protocol version {} is not supported (server speaks {PROTOCOL_VERSION})",
                envelope.version
            ),
        ));
    }
    Ok(envelope.body)
}

pub fn encode_server(msg: &ServerMessage) -> String {
    serde_json::to_string(&Envelope::new(msg)).expect("server messages always serialize")
}

pub fn encode_client(msg: &ClientMessage) -> String {
    serde_json::to_string(&Envelope::new(msg)).expect("client messages always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_join_room() {
        let msg = decode_client(r#"{"version":1,"type":"join_room","room_id":"847291653"}"#);
        assert_eq!(
            msg,
            Ok(ClientMessage::JoinRoom {
                room_id: "847291653".into(),
                display_name: None,
                pin: None,
            })
        );
    }

    #[test]
    fn old_create_room_frames_still_parse() {
        assert_eq!(
            decode_client(r#"{"type":"create_room","display_name":"A"}"#),
            Ok(ClientMessage::CreateRoom {
                display_name: Some("A".into()),
                pin: None,
                lobby: false,
                large: false,
            })
        );
    }

    #[test]
    fn missing_version_defaults_to_current() {
        assert_eq!(decode_client(r#"{"type":"ping"}"#), Ok(ClientMessage::Ping));
    }

    #[test]
    fn rejects_future_version() {
        match decode_client(r#"{"version":99,"type":"ping"}"#) {
            Err(ServerMessage::Error { code, .. }) => {
                assert_eq!(code, ErrorCode::UnsupportedVersion)
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_type() {
        match decode_client(r#"{"version":1,"type":"format_disk"}"#) {
            Err(ServerMessage::Error { code, .. }) => assert_eq!(code, ErrorCode::InvalidMessage),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn encodes_with_version_and_type() {
        let text = encode_server(&ServerMessage::ParticipantLeft {
            participant_id: "p2".into(),
        });
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            json!({"version": 1, "type": "participant_left", "participant_id": "p2"})
        );
    }

    #[test]
    fn ice_candidate_is_opaque() {
        let msg = decode_client(
            r#"{"type":"ice_candidate","target":"p2","candidate":{"candidate":"candidate:1 1 udp 1 1.2.3.4 5 typ host","sdpMid":"0"}}"#,
        )
        .unwrap();
        assert!(matches!(msg, ClientMessage::IceCandidate { .. }));
    }

    #[test]
    fn sfu_candidate_names_its_connection() {
        let msg = decode_client(r#"{"type":"sfu_candidate","pc":"sub","candidate":{}}"#).unwrap();
        assert!(matches!(
            msg,
            ClientMessage::SfuCandidate {
                pc: SfuPc::Subscriber,
                ..
            }
        ));
    }

    #[test]
    fn room_created_reports_topology_and_security() {
        let text = encode_server(&ServerMessage::RoomCreated {
            room_id: "123456789".into(),
            participant_id: "p1".into(),
            resume_token: "t".into(),
            max_participants: 25,
            ice_servers: vec![],
            topology: Topology::Sfu,
            security: RoomSecurity {
                pin: true,
                lobby: false,
            },
        });
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["topology"], "sfu");
        assert_eq!(v["security"], json!({"pin": true, "lobby": false}));
    }
}
