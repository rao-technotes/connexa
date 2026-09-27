//! Wire format of the Connexa signaling protocol.
//!
//! Every frame is a JSON object carrying a `version` and a `type`, e.g.
//! `{ "version": 1, "type": "join_room", "room_id": "847291653" }`.
//!
//! The signaling server only coordinates rooms and relays SDP / ICE.
//! Media, chat and file data never pass through it.

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
    },
    JoinRoom {
        room_id: String,
        #[serde(default)]
        display_name: Option<String>,
    },
    /// Re-attach to a room after the WebSocket dropped, within the grace period.
    Resume {
        room_id: String,
        participant_id: ParticipantId,
        resume_token: String,
    },
    LeaveRoom,
    /// Host only: close the room for everyone.
    EndRoom,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomEndReason {
    HostEnded,
    HostLeft,
    Expired,
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
                display_name: None
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
}
