import { deviceKey, devicePlatform } from "./device";
import { Emitter } from "./emitter";
import {
  PROTOCOL_VERSION,
  type ActivityEvent,
  type ClientMessage,
  type DeviceSummary,
  type ErrorCode,
  type IceServerConfig,
  type ParticipantId,
  type ParticipantInfo,
  type RoomEndReason,
  type RoomSecurity,
  type ServerMessage,
  type ServerMessageOf,
  type SfuPc,
  type Topology,
} from "./protocol";

export interface SessionInfo {
  roomId: string;
  selfId: ParticipantId;
  hostId: ParticipantId;
  resumeToken: string;
  maxParticipants: number;
  iceServers: IceServerConfig[];
  topology: Topology;
  security: RoomSecurity;
}

export interface CreateOptions {
  pin?: string;
  lobby?: boolean;
  large?: boolean;
}

export interface JoinRequest {
  request_id: string;
  display_name: string;
  device_id?: string;
}

export type SignalingStatus = "connecting" | "open" | "reconnecting" | "closed";

type Events = {
  status: SignalingStatus;
  resumed: ParticipantInfo[];
  "participant-joined": ParticipantInfo;
  "participant-left": ParticipantId;
  offer: { from: ParticipantId; sdp: string };
  answer: { from: ParticipantId; sdp: string };
  candidate: { from: ParticipantId; candidate: RTCIceCandidateInit };
  ended: RoomEndReason | "connection_lost";
  error: { code: ErrorCode; message: string };
  /** Joiner: waiting for the host to admit us. */
  waiting: string;
  /** Host: someone is waiting in the lobby. */
  "join-request": JoinRequest;
  "join-request-cancelled": string;
  "sfu-offer": string;
  "sfu-answer": string;
  "sfu-candidate": { pc: SfuPc; candidate: RTCIceCandidateInit };
};

export class SignalingError extends Error {
  constructor(public code: ErrorCode | "connection_failed" | "cancelled", message: string) {
    super(message);
  }
}

const PING_INTERVAL_MS = 20_000;
/** Must stay below the server's reconnect grace period (30 s by default). */
const RECONNECT_WINDOW_MS = 25_000;

/**
 * WebSocket client for the Connexa signaling protocol.
 *
 * If the socket drops mid-session it reconnects and resumes the same
 * participant slot; WebRTC connections keep running in the meantime and
 * outgoing signaling is queued until the session is resumed.
 */
export class SignalingClient extends Emitter<Events> {
  private ws: WebSocket | null = null;
  private session: SessionInfo | null = null;
  private outbox: ClientMessage[] = [];
  private attached = false;
  private leaving = false;
  private pingTimer: number | undefined;
  private waiters: Array<{ handle: (msg: ServerMessage) => boolean; fail: (err: Error) => void }> = [];
  private verifiedDevice: string | null = null;

  constructor(
    private readonly url: string,
    private readonly deviceName = "",
  ) {
    super();
  }

  get info(): SessionInfo | null {
    return this.session;
  }

  /** This device's verified ID (null if the device key is unavailable). */
  get deviceId(): string | null {
    return this.verifiedDevice;
  }

  async createRoom(
    displayName: string,
    options: CreateOptions = {},
  ): Promise<{ session: SessionInfo; participants: ParticipantInfo[] }> {
    await this.open();
    const msg = await this.request(
      {
        type: "create_room",
        display_name: displayName,
        pin: options.pin || undefined,
        lobby: options.lobby ?? false,
        large: options.large ?? false,
      },
      "room_created",
    );
    this.session = {
      roomId: msg.room_id,
      selfId: msg.participant_id,
      hostId: msg.participant_id,
      resumeToken: msg.resume_token,
      maxParticipants: msg.max_participants,
      iceServers: msg.ice_servers,
      topology: msg.topology ?? "mesh",
      security: msg.security ?? { pin: false, lobby: false },
    };
    this.attached = true;
    return { session: this.session, participants: [] };
  }

  /** Resolves once in the room; emits "waiting" while the host decides (lobby rooms). */
  async joinRoom(
    roomId: string,
    displayName: string,
    pin?: string,
  ): Promise<{ session: SessionInfo; participants: ParticipantInfo[] }> {
    await this.open();
    const msg = await this.request(
      { type: "join_room", room_id: roomId, display_name: displayName, pin: pin || undefined },
      "room_joined",
    );
    this.session = {
      roomId: msg.room_id,
      selfId: msg.participant_id,
      hostId: msg.host_id,
      resumeToken: msg.resume_token,
      maxParticipants: msg.max_participants,
      iceServers: msg.ice_servers,
      topology: msg.topology ?? "mesh",
      security: msg.security ?? { pin: false, lobby: false },
    };
    this.attached = true;
    return { session: this.session, participants: msg.participants };
  }

  admit(requestId: string): void {
    this.send({ type: "admit", request_id: requestId });
  }

  deny(requestId: string): void {
    this.send({ type: "deny", request_id: requestId });
  }

  async trustDevice(deviceId: string, trusted: boolean): Promise<DeviceSummary[]> {
    const reply = await this.request({ type: "trust_device", device_id: deviceId, trusted }, "trusted_devices");
    return reply.devices;
  }

  async trustedDevices(): Promise<DeviceSummary[]> {
    return (await this.request({ type: "list_trusted_devices" }, "trusted_devices")).devices;
  }

  async activity(): Promise<ActivityEvent[]> {
    return (await this.request({ type: "get_activity" }, "activity")).events;
  }

  /** Record a peer-to-peer event (e.g. control granted) in this device's audit log. */
  reportEvent(kind: string, subject?: ParticipantId): void {
    if (this.verifiedDevice) this.send({ type: "report_event", kind, subject });
  }

  /** Connect without joining a room (e.g. to manage trusted devices). */
  async connect(): Promise<void> {
    await this.open();
  }

  /** Queue-aware send for in-session messages. */
  send(msg: ClientMessage): void {
    if (this.attached && this.ws?.readyState === WebSocket.OPEN) {
      this.raw(msg);
    } else if (this.session) {
      this.outbox.push(msg);
    }
  }

  leave(endForEveryone = false): void {
    this.leaving = true;
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.raw(endForEveryone ? { type: "end_room" } : { type: "leave_room" });
    }
    this.shutdown();
  }

  private shutdown(): void {
    this.session = null;
    this.attached = false;
    this.outbox = [];
    const pending = this.waiters;
    this.waiters = [];
    pending.forEach((w) => w.fail(new SignalingError("cancelled", "The connection was closed.")));
    window.clearInterval(this.pingTimer);
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      ws.onclose = null;
      // Let the leave message flush before closing.
      setTimeout(() => ws.close(1000), 50);
    }
    this.emit("status", "closed");
  }

  private open(): Promise<void> {
    if (this.ws?.readyState === WebSocket.OPEN) return Promise.resolve();
    this.emit("status", this.session ? "reconnecting" : "connecting");
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      let settled = false;
      ws.onopen = () => {
        settled = true;
        this.ws = ws;
        window.clearInterval(this.pingTimer);
        this.pingTimer = window.setInterval(() => this.raw({ type: "ping" }), PING_INTERVAL_MS);
        this.emit("status", "open");
        // Prove the device key first so the server can attach our device ID.
        void this.proveDevice().finally(() => resolve());
      };
      ws.onerror = () => {
        if (!settled) {
          settled = true;
          reject(new SignalingError("connection_failed", "Could not reach the signaling server."));
        }
      };
      ws.onclose = () => {
        window.clearInterval(this.pingTimer);
        if (this.ws === ws) this.ws = null;
        const pending = this.waiters;
        this.waiters = [];
        pending.forEach((w) => w.fail(new SignalingError("connection_failed", "Connection to the server was lost.")));
        if (!settled) {
          settled = true;
          reject(new SignalingError("connection_failed", "Could not reach the signaling server."));
          return;
        }
        this.attached = false;
        if (this.session && !this.leaving) void this.reconnect();
        else if (!this.leaving) this.emit("status", "closed");
      };
      ws.onmessage = (ev) => this.dispatch(ev.data);
    });
  }

  private async proveDevice(): Promise<void> {
    try {
      const key = await deviceKey();
      if (!key) return;
      const challenge = await this.request(
        {
          type: "device_hello",
          public_key: key.publicKey,
          name: this.deviceName || undefined,
          platform: devicePlatform(),
        },
        "device_challenge",
      );
      const verified = await this.request(
        { type: "device_proof", signature: await key.sign(challenge.nonce) },
        "device_verified",
      );
      this.verifiedDevice = verified.device_id;
    } catch (err) {
      // Identity is optional: sessions still work unverified.
      if (!(err instanceof SignalingError && err.code === "connection_failed")) {
        console.warn("device verification failed", err);
      }
    }
  }

  private async reconnect(): Promise<void> {
    this.emit("status", "reconnecting");
    const deadline = Date.now() + RECONNECT_WINDOW_MS;
    for (let attempt = 0; this.session && !this.leaving && Date.now() < deadline; attempt++) {
      await sleep(Math.min(500 * 2 ** attempt, 4000));
      if (!this.session || this.leaving) return;
      try {
        await this.open();
        const s = this.session;
        const resumed = await this.request(
          { type: "resume", room_id: s.roomId, participant_id: s.selfId, resume_token: s.resumeToken },
          "session_resumed",
        );
        this.attached = true;
        const queued = this.outbox;
        this.outbox = [];
        queued.forEach((m) => this.raw(m));
        this.emit("resumed", resumed.participants);
        return;
      } catch (err) {
        if (err instanceof SignalingError && err.code !== "connection_failed") break;
      }
    }
    if (this.session && !this.leaving) {
      this.shutdown();
      this.emit("ended", "connection_lost");
    }
  }

  private request<T extends ServerMessage["type"]>(msg: ClientMessage, expect: T): Promise<ServerMessageOf<T>> {
    return new Promise((resolve, reject) => {
      this.waiters.push({
        handle: (reply) => {
          if (reply.type === expect) {
            resolve(reply as ServerMessageOf<T>);
            return true;
          }
          if (reply.type === "error") {
            reject(new SignalingError(reply.code, reply.message));
            return true;
          }
          return false;
        },
        fail: reject,
      });
      this.raw(msg);
    });
  }

  private raw(msg: ClientMessage): void {
    this.ws?.send(JSON.stringify({ version: PROTOCOL_VERSION, ...msg }));
  }

  private dispatch(data: unknown): void {
    let msg: ServerMessage;
    try {
      msg = JSON.parse(String(data));
    } catch {
      return;
    }
    const waiter = this.waiters[0];
    if (waiter && waiter.handle(msg)) {
      this.waiters.shift();
      return;
    }
    switch (msg.type) {
      case "participant_joined":
        return this.emit("participant-joined", msg.participant);
      case "participant_left":
        return this.emit("participant-left", msg.participant_id);
      case "sdp_offer":
        return this.emit("offer", { from: msg.from, sdp: msg.sdp });
      case "sdp_answer":
        return this.emit("answer", { from: msg.from, sdp: msg.sdp });
      case "ice_candidate":
        return this.emit("candidate", { from: msg.from, candidate: msg.candidate });
      case "lobby_waiting":
        return this.emit("waiting", msg.room_id);
      case "join_request":
        return this.emit("join-request", {
          request_id: msg.request_id,
          display_name: msg.display_name,
          device_id: msg.device_id,
        });
      case "join_request_cancelled":
        return this.emit("join-request-cancelled", msg.request_id);
      case "sfu_offer":
        return this.emit("sfu-offer", msg.sdp);
      case "sfu_answer":
        return this.emit("sfu-answer", msg.sdp);
      case "sfu_candidate":
        return this.emit("sfu-candidate", { pc: msg.pc, candidate: msg.candidate });
      case "room_ended":
        this.leaving = true;
        this.shutdown();
        return this.emit("ended", msg.reason);
      case "error":
        return this.emit("error", { code: msg.code, message: msg.message });
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
