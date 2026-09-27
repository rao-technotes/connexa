import { Emitter } from "./emitter";
import type { IceServerConfig, ParticipantId, ParticipantInfo } from "./protocol";
import type { SignalingClient } from "./signaling";

/** "camera" carries mic + camera tracks, "screen" carries the screen share. */
export type StreamKind = "camera" | "screen";
export type RouteType = "direct" | "relay" | "unknown";

export interface MediaState {
  mic: boolean;
  cam: boolean;
  screen: boolean;
}

export interface ChatMessage {
  id: string;
  from: ParticipantId;
  text: string;
  ts: number;
}

/** Messages exchanged peer-to-peer over the data channel (never via the server). */
type ChannelMessage =
  | { t: "chat"; id: string; text: string; ts: number }
  | { t: "stream"; streamId: string; kind: StreamKind }
  | { t: "state"; mic: boolean; cam: boolean; screen: boolean };

type Events = {
  "peer-added": ParticipantId;
  "peer-removed": ParticipantId;
  "peer-state": { id: ParticipantId; state: RTCPeerConnectionState; route: RouteType };
  "local-stream": { kind: StreamKind; stream: MediaStream | null };
  "local-media": MediaState;
  "remote-stream": { peerId: ParticipantId; stream: MediaStream; kind: StreamKind };
  "remote-stream-removed": { peerId: ParticipantId; streamId: string };
  "remote-media": { peerId: ParticipantId; state: MediaState };
  chat: ChatMessage;
};

const MAX_CHAT_CHARS = 4000;

class Peer {
  readonly pc: RTCPeerConnection;
  readonly channel: RTCDataChannel;
  readonly senders = new Map<string, RTCRtpSender>();
  readonly remoteStreams = new Map<string, MediaStream>();
  readonly remoteKinds = new Map<string, StreamKind>();
  route: RouteType = "unknown";
  makingOffer = false;
  ignoreOffer = false;
  settingRemoteAnswer = false;
  /** Serializes incoming signaling so candidates never race their description. */
  ops: Promise<void> = Promise.resolve();

  constructor(
    readonly id: ParticipantId,
    readonly polite: boolean,
    iceServers: IceServerConfig[],
  ) {
    this.pc = new RTCPeerConnection({ iceServers });
    // Negotiated on both sides with a fixed id: symmetric setup, no ondatachannel race.
    this.channel = this.pc.createDataChannel("connexa", { negotiated: true, id: 0, ordered: true });
  }
}

/**
 * Full-mesh WebRTC for small rooms: one RTCPeerConnection per remote participant.
 */
export class PeerMesh extends Emitter<Events> {
  private peers = new Map<ParticipantId, Peer>();
  private local = new Map<StreamKind, MediaStream>();
  private media: MediaState = { mic: false, cam: false, screen: false };
  private statsTimer: number;
  private unsubscribe: Array<() => void> = [];

  constructor(
    private readonly signaling: SignalingClient,
    private readonly selfId: ParticipantId,
    private readonly iceServers: IceServerConfig[],
  ) {
    super();
    this.unsubscribe.push(
      signaling.on("participant-joined", (p) => this.addPeer(p.participant_id)),
      signaling.on("participant-left", (id) => this.removePeer(id)),
      signaling.on("offer", ({ from, sdp }) => this.onDescription(from, { type: "offer", sdp })),
      signaling.on("answer", ({ from, sdp }) => this.onDescription(from, { type: "answer", sdp })),
      signaling.on("candidate", ({ from, candidate }) => this.onCandidate(from, candidate)),
      signaling.on("resumed", (participants) => this.reconcile(participants)),
    );
    this.statsTimer = window.setInterval(() => void this.refreshRoutes(), 3000);
  }

  get mediaState(): MediaState {
    return { ...this.media };
  }

  connectTo(participants: ParticipantInfo[]): void {
    participants.forEach((p) => this.addPeer(p.participant_id));
  }

  // ----- local media ---------------------------------------------------

  async setMic(on: boolean): Promise<void> {
    const stream = this.cameraStream();
    let track = stream.getAudioTracks()[0];
    if (on && !track) {
      const captured = await navigator.mediaDevices.getUserMedia({
        audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
      });
      track = captured.getAudioTracks()[0];
      stream.addTrack(track);
      this.addTrackToPeers(track, stream);
      this.emit("local-stream", { kind: "camera", stream });
    }
    // Muting keeps the sender (no renegotiation) and just sends silence.
    if (track) track.enabled = on;
    this.setMedia({ mic: on });
  }

  async setCamera(on: boolean): Promise<void> {
    const stream = this.cameraStream();
    const existing = stream.getVideoTracks()[0];
    if (on && !existing) {
      const captured = await navigator.mediaDevices.getUserMedia({
        video: { width: { ideal: 1280 }, height: { ideal: 720 }, frameRate: { ideal: 30 } },
      });
      const track = captured.getVideoTracks()[0];
      stream.addTrack(track);
      this.addTrackToPeers(track, stream);
    } else if (!on && existing) {
      // Stop the track so the camera light turns off.
      existing.stop();
      stream.removeTrack(existing);
      this.removeTrackFromPeers(existing);
    }
    this.emit("local-stream", { kind: "camera", stream });
    this.setMedia({ cam: on });
  }

  async startScreenShare(): Promise<void> {
    if (this.local.has("screen")) return;
    const stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: 30, max: 30 } },
      audio: true,
    });
    const video = stream.getVideoTracks()[0];
    // Optimize for desktop text rather than motion.
    if (video && "contentHint" in video) video.contentHint = "detail";
    video?.addEventListener("ended", () => this.stopScreenShare());

    this.local.set("screen", stream);
    this.broadcast({ t: "stream", streamId: stream.id, kind: "screen" });
    stream.getTracks().forEach((t) => this.addTrackToPeers(t, stream));
    this.emit("local-stream", { kind: "screen", stream });
    this.setMedia({ screen: true });
  }

  stopScreenShare(): void {
    const stream = this.local.get("screen");
    if (!stream) return;
    this.local.delete("screen");
    stream.getTracks().forEach((t) => {
      t.stop();
      this.removeTrackFromPeers(t);
    });
    this.emit("local-stream", { kind: "screen", stream: null });
    this.setMedia({ screen: false });
  }

  // ----- chat ----------------------------------------------------------

  sendChat(text: string): void {
    const trimmed = text.trim().slice(0, MAX_CHAT_CHARS);
    if (!trimmed) return;
    const msg: ChatMessage = { id: crypto.randomUUID(), from: this.selfId, text: trimmed, ts: Date.now() };
    this.broadcast({ t: "chat", id: msg.id, text: msg.text, ts: msg.ts });
    this.emit("chat", msg);
  }

  // ----- lifecycle -----------------------------------------------------

  close(): void {
    window.clearInterval(this.statsTimer);
    this.unsubscribe.forEach((u) => u());
    for (const id of [...this.peers.keys()]) this.removePeer(id);
    for (const stream of this.local.values()) stream.getTracks().forEach((t) => t.stop());
    this.local.clear();
  }

  private cameraStream(): MediaStream {
    let stream = this.local.get("camera");
    if (!stream) {
      stream = new MediaStream();
      this.local.set("camera", stream);
    }
    return stream;
  }

  private setMedia(patch: Partial<MediaState>): void {
    this.media = { ...this.media, ...patch };
    this.broadcast({ t: "state", ...this.media });
    this.emit("local-media", this.mediaState);
  }

  private addTrackToPeers(track: MediaStreamTrack, stream: MediaStream): void {
    for (const peer of this.peers.values()) this.addTrack(peer, track, stream);
  }

  private addTrack(peer: Peer, track: MediaStreamTrack, stream: MediaStream): void {
    const sender = peer.pc.addTrack(track, stream);
    peer.senders.set(track.id, sender);
    if (stream === this.local.get("screen") && track.kind === "video") {
      // Keep text legible: drop frame rate before resolution when bandwidth is short.
      const params = sender.getParameters();
      params.degradationPreference = "maintain-resolution";
      sender.setParameters(params).catch(() => {});
    }
  }

  private removeTrackFromPeers(track: MediaStreamTrack): void {
    for (const peer of this.peers.values()) {
      const sender = peer.senders.get(track.id);
      if (!sender) continue;
      peer.senders.delete(track.id);
      if (peer.pc.signalingState !== "closed") peer.pc.removeTrack(sender);
    }
  }

  // ----- peers ---------------------------------------------------------

  private addPeer(id: ParticipantId): Peer {
    const existing = this.peers.get(id);
    if (existing) return existing;

    const peer = new Peer(id, this.selfId > id, this.iceServers);
    this.peers.set(id, peer);
    const { pc, channel } = peer;

    pc.onnegotiationneeded = async () => {
      try {
        peer.makingOffer = true;
        await pc.setLocalDescription();
        this.signaling.send({ type: "sdp_offer", target: id, sdp: pc.localDescription!.sdp });
      } catch (err) {
        console.warn("negotiation failed", err);
      } finally {
        peer.makingOffer = false;
      }
    };
    pc.onicecandidate = ({ candidate }) => {
      if (candidate) this.signaling.send({ type: "ice_candidate", target: id, candidate: candidate.toJSON() });
    };
    pc.onconnectionstatechange = () => {
      if (pc.connectionState === "failed") pc.restartIce();
      this.emitPeerState(peer);
      if (pc.connectionState === "connected") void this.refreshRoutes();
    };
    pc.ontrack = ({ track, streams }) => this.onRemoteTrack(peer, track, streams[0]);

    channel.onopen = () => {
      // Tell the new peer what our streams are and our current mic/cam state.
      for (const [kind, stream] of this.local) this.sendTo(peer, { t: "stream", streamId: stream.id, kind });
      this.sendTo(peer, { t: "state", ...this.media });
    };
    channel.onmessage = (ev) => this.onChannelMessage(peer, ev.data);

    for (const stream of this.local.values()) {
      stream.getTracks().forEach((t) => this.addTrack(peer, t, stream));
    }
    this.emit("peer-added", id);
    this.emitPeerState(peer);
    return peer;
  }

  private removePeer(id: ParticipantId): void {
    const peer = this.peers.get(id);
    if (!peer) return;
    this.peers.delete(id);
    peer.pc.close();
    for (const streamId of peer.remoteStreams.keys()) this.emit("remote-stream-removed", { peerId: id, streamId });
    this.emit("peer-removed", id);
  }

  /** After a signaling reconnect, align peers with the server's view of the room. */
  private reconcile(participants: ParticipantInfo[]): void {
    const present = new Set(participants.map((p) => p.participant_id));
    for (const id of [...this.peers.keys()]) if (!present.has(id)) this.removePeer(id);
    for (const id of present) this.addPeer(id);
  }

  private onDescription(from: ParticipantId, description: RTCSessionDescriptionInit): void {
    const peer = this.addPeer(from);
    const { pc } = peer;
    peer.ops = peer.ops.then(async () => {
      try {
        // "Perfect negotiation": the impolite peer ignores colliding offers,
        // the polite peer rolls back its own and accepts the remote one.
        const readyForOffer = !peer.makingOffer && (pc.signalingState === "stable" || peer.settingRemoteAnswer);
        const collision = description.type === "offer" && !readyForOffer;
        peer.ignoreOffer = !peer.polite && collision;
        if (peer.ignoreOffer) return;

        peer.settingRemoteAnswer = description.type === "answer";
        await pc.setRemoteDescription(description);
        peer.settingRemoteAnswer = false;
        if (description.type === "offer") {
          await pc.setLocalDescription();
          this.signaling.send({ type: "sdp_answer", target: from, sdp: pc.localDescription!.sdp });
        }
      } catch (err) {
        peer.settingRemoteAnswer = false;
        console.warn("failed to apply remote description", err);
      }
    });
  }

  private onCandidate(from: ParticipantId, candidate: RTCIceCandidateInit): void {
    const peer = this.peers.get(from);
    if (!peer) return;
    peer.ops = peer.ops.then(async () => {
      try {
        await peer.pc.addIceCandidate(candidate);
      } catch (err) {
        if (!peer.ignoreOffer) console.warn("failed to add ICE candidate", err);
      }
    });
  }

  private onRemoteTrack(peer: Peer, track: MediaStreamTrack, stream: MediaStream | undefined): void {
    const s = stream ?? new MediaStream([track]);
    if (!peer.remoteStreams.has(s.id)) {
      peer.remoteStreams.set(s.id, s);
      s.addEventListener("removetrack", () => {
        if (s.getTracks().length === 0 && peer.remoteStreams.delete(s.id)) {
          this.emit("remote-stream-removed", { peerId: peer.id, streamId: s.id });
        }
      });
    }
    this.emit("remote-stream", { peerId: peer.id, stream: s, kind: peer.remoteKinds.get(s.id) ?? "camera" });
  }

  private onChannelMessage(peer: Peer, data: unknown): void {
    if (typeof data !== "string" || data.length > MAX_CHAT_CHARS * 4) return;
    let msg: ChannelMessage;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    // Peers are untrusted: validate before use.
    switch (msg?.t) {
      case "chat":
        if (typeof msg.text === "string" && typeof msg.id === "string") {
          this.emit("chat", {
            id: msg.id,
            from: peer.id,
            text: msg.text.slice(0, MAX_CHAT_CHARS),
            ts: Number(msg.ts) || Date.now(),
          });
        }
        break;
      case "stream":
        if (typeof msg.streamId === "string" && (msg.kind === "camera" || msg.kind === "screen")) {
          peer.remoteKinds.set(msg.streamId, msg.kind);
          const stream = peer.remoteStreams.get(msg.streamId);
          if (stream) this.emit("remote-stream", { peerId: peer.id, stream, kind: msg.kind });
        }
        break;
      case "state":
        this.emit("remote-media", {
          peerId: peer.id,
          state: { mic: msg.mic === true, cam: msg.cam === true, screen: msg.screen === true },
        });
        break;
    }
  }

  private broadcast(msg: ChannelMessage): void {
    for (const peer of this.peers.values()) this.sendTo(peer, msg);
  }

  private sendTo(peer: Peer, msg: ChannelMessage): void {
    if (peer.channel.readyState === "open") peer.channel.send(JSON.stringify(msg));
  }

  private emitPeerState(peer: Peer): void {
    this.emit("peer-state", { id: peer.id, state: peer.pc.connectionState, route: peer.route });
  }

  /** Detect whether each connection is direct P2P or relayed through TURN. */
  private async refreshRoutes(): Promise<void> {
    for (const peer of this.peers.values()) {
      if (peer.pc.connectionState !== "connected") continue;
      const route = await selectedRoute(peer.pc);
      if (route !== peer.route) {
        peer.route = route;
        this.emitPeerState(peer);
      }
    }
  }
}

async function selectedRoute(pc: RTCPeerConnection): Promise<RouteType> {
  try {
    const stats = await pc.getStats();
    let pairId: string | undefined;
    stats.forEach((s) => {
      if (s.type === "transport" && s.selectedCandidatePairId) pairId = s.selectedCandidatePairId;
    });
    if (!pairId) {
      stats.forEach((s) => {
        if (s.type === "candidate-pair" && s.nominated && s.state === "succeeded") pairId ??= s.id;
      });
    }
    const pair = pairId ? stats.get(pairId) : undefined;
    const local = pair ? stats.get(pair.localCandidateId) : undefined;
    const remote = pair ? stats.get(pair.remoteCandidateId) : undefined;
    if (!local) return "unknown";
    return local.candidateType === "relay" || remote?.candidateType === "relay" ? "relay" : "direct";
  } catch {
    return "unknown";
  }
}
