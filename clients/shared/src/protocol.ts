// Mirror of crates/connexa-protocol. Keep in sync with the Rust definitions.

export const PROTOCOL_VERSION = 1;

export type ParticipantId = string;

export interface ParticipantInfo {
  participant_id: ParticipantId;
  display_name: string;
  is_host: boolean;
}

export interface IceServerConfig {
  urls: string[];
  username?: string;
  credential?: string;
}

export type RoomEndReason = "host_ended" | "host_left" | "expired";

export type ErrorCode =
  | "invalid_message"
  | "unsupported_version"
  | "invalid_room_code"
  | "room_not_found"
  | "room_full"
  | "already_in_room"
  | "not_in_room"
  | "not_host"
  | "target_not_found"
  | "resume_failed"
  | "rate_limited"
  | "server_busy";

export type ClientMessage =
  | { type: "create_room"; display_name?: string }
  | { type: "join_room"; room_id: string; display_name?: string }
  | { type: "resume"; room_id: string; participant_id: ParticipantId; resume_token: string }
  | { type: "leave_room" }
  | { type: "end_room" }
  | { type: "sdp_offer"; target: ParticipantId; sdp: string }
  | { type: "sdp_answer"; target: ParticipantId; sdp: string }
  | { type: "ice_candidate"; target: ParticipantId; candidate: RTCIceCandidateInit }
  | { type: "ping" };

export type ServerMessage =
  | {
      type: "room_created";
      room_id: string;
      participant_id: ParticipantId;
      resume_token: string;
      max_participants: number;
      ice_servers: IceServerConfig[];
    }
  | {
      type: "room_joined";
      room_id: string;
      participant_id: ParticipantId;
      resume_token: string;
      host_id: ParticipantId;
      participants: ParticipantInfo[];
      max_participants: number;
      ice_servers: IceServerConfig[];
    }
  | {
      type: "session_resumed";
      room_id: string;
      participant_id: ParticipantId;
      host_id: ParticipantId;
      participants: ParticipantInfo[];
    }
  | { type: "participant_joined"; participant: ParticipantInfo }
  | { type: "participant_left"; participant_id: ParticipantId }
  | { type: "sdp_offer"; from: ParticipantId; sdp: string }
  | { type: "sdp_answer"; from: ParticipantId; sdp: string }
  | { type: "ice_candidate"; from: ParticipantId; candidate: RTCIceCandidateInit }
  | { type: "room_ended"; reason: RoomEndReason }
  | { type: "error"; code: ErrorCode; message: string }
  | { type: "pong" };

export type ServerMessageOf<T extends ServerMessage["type"]> = Extract<ServerMessage, { type: T }>;
