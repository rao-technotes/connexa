// Mirror of crates/connexa-protocol. Keep in sync with the Rust definitions.

export const PROTOCOL_VERSION = 1;

export type ParticipantId = string;

export interface ParticipantInfo {
  participant_id: ParticipantId;
  display_name: string;
  is_host: boolean;
  /** Present when the participant proved possession of a device key. */
  device_id?: string;
}

export interface IceServerConfig {
  urls: string[];
  username?: string;
  credential?: string;
}

export type Topology = "mesh" | "sfu";
export type SfuPc = "pub" | "sub";

export interface RoomSecurity {
  pin: boolean;
  lobby: boolean;
}

export interface DeviceSummary {
  device_id: string;
  name: string;
  platform: string;
  last_seen: number;
}

export interface ActivityEvent {
  at: number;
  kind: string;
  actor?: string;
  subject?: string;
}

export type RoomEndReason = "host_ended" | "host_left" | "expired" | "server_lost";

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
  | "server_busy"
  | "pin_required"
  | "wrong_pin"
  | "join_denied"
  | "invalid_pin"
  | "not_verified"
  | "unavailable";

export type ClientMessage =
  | { type: "create_room"; display_name?: string; pin?: string; lobby?: boolean; large?: boolean }
  | { type: "join_room"; room_id: string; display_name?: string; pin?: string }
  | { type: "resume"; room_id: string; participant_id: ParticipantId; resume_token: string }
  | { type: "leave_room" }
  | { type: "end_room" }
  | { type: "admit"; request_id: string }
  | { type: "deny"; request_id: string }
  | { type: "sdp_offer"; target: ParticipantId; sdp: string }
  | { type: "sdp_answer"; target: ParticipantId; sdp: string }
  | { type: "ice_candidate"; target: ParticipantId; candidate: RTCIceCandidateInit }
  | { type: "sfu_offer"; sdp: string }
  | { type: "sfu_answer"; sdp: string }
  | { type: "sfu_candidate"; pc: SfuPc; candidate: RTCIceCandidateInit }
  | { type: "device_hello"; public_key: string; name?: string; platform?: string }
  | { type: "device_proof"; signature: string }
  | { type: "trust_device"; device_id: string; trusted: boolean }
  | { type: "list_trusted_devices" }
  | { type: "get_activity" }
  | { type: "report_event"; kind: string; subject?: ParticipantId }
  | { type: "ping" };

export type ServerMessage =
  | {
      type: "room_created";
      room_id: string;
      participant_id: ParticipantId;
      resume_token: string;
      max_participants: number;
      ice_servers: IceServerConfig[];
      topology?: Topology;
      security?: RoomSecurity;
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
      topology?: Topology;
      security?: RoomSecurity;
    }
  | { type: "lobby_waiting"; room_id: string }
  | { type: "join_request"; request_id: string; display_name: string; device_id?: string }
  | { type: "join_request_cancelled"; request_id: string }
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
  | { type: "sfu_offer"; sdp: string }
  | { type: "sfu_answer"; sdp: string }
  | { type: "sfu_candidate"; pc: SfuPc; candidate: RTCIceCandidateInit }
  | { type: "device_challenge"; nonce: string }
  | { type: "device_verified"; device_id: string }
  | { type: "trusted_devices"; devices: DeviceSummary[] }
  | { type: "activity"; events: ActivityEvent[] }
  | { type: "room_ended"; reason: RoomEndReason }
  | { type: "error"; code: ErrorCode; message: string }
  | { type: "pong" };

export type ServerMessageOf<T extends ServerMessage["type"]> = Extract<ServerMessage, { type: T }>;
