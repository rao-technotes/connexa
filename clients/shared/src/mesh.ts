import type { IceServerConfig, ParticipantId, ParticipantInfo } from "./protocol";
import { MediaSession, type PeerMessage, type RouteType, type StreamKind } from "./session";
import type { SignalingClient } from "./signaling";

export type { ChatMessage, MediaState, PeerMessage, RouteType, StreamKind } from "./session";

/** Messages queued per peer while its data channel is still opening. */
const MAX_QUEUED = 256;

class Peer {
  readonly pc: RTCPeerConnection;
  readonly channel: RTCDataChannel;
  readonly senders = new Map<string, RTCRtpSender>();
  /** Messages sent before the data channel opened. */
  outbox: string[] = [];
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
export class PeerMesh extends MediaSession {
  readonly topology = "mesh" as const;
  private peers = new Map<ParticipantId, Peer>();
  private statsTimer: number;
  private unsubscribe: Array<() => void> = [];

  constructor(
    private readonly signaling: SignalingClient,
    selfId: ParticipantId,
    iceServers: IceServerConfig[],
  ) {
    super(selfId, iceServers);
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

  peerIds(): ParticipantId[] {
    return [...this.peers.keys()];
  }

  send(peerId: ParticipantId, msg: PeerMessage): boolean {
    const peer = this.peers.get(peerId);
    if (!peer) return false;
    return this.sendTo(peer, msg);
  }

  broadcast(msg: PeerMessage): void {
    for (const peer of this.peers.values()) this.sendTo(peer, msg);
  }

  createChannel(peerId: ParticipantId, label: string): RTCDataChannel | null {
    const peer = this.peers.get(peerId);
    if (!peer || peer.pc.connectionState === "closed") return null;
    return peer.pc.createDataChannel(label, { ordered: true });
  }

  connectTo(participants: ParticipantInfo[]): void {
    participants.forEach((p) => this.addPeer(p.participant_id));
  }

  protected onLocalTrackAdded(track: MediaStreamTrack, stream: MediaStream, kind: StreamKind): void {
    for (const peer of this.peers.values()) this.addTrack(peer, track, stream, kind);
  }

  protected onLocalTrackRemoved(track: MediaStreamTrack): void {
    for (const peer of this.peers.values()) {
      const sender = peer.senders.get(track.id);
      if (!sender) continue;
      peer.senders.delete(track.id);
      if (peer.pc.signalingState !== "closed") peer.pc.removeTrack(sender);
    }
  }

  protected dispose(): void {
    window.clearInterval(this.statsTimer);
    this.unsubscribe.forEach((u) => u());
    for (const id of [...this.peers.keys()]) this.removePeer(id);
  }

  private addTrack(peer: Peer, track: MediaStreamTrack, stream: MediaStream, kind: StreamKind): void {
    const sender = peer.pc.addTrack(track, stream);
    peer.senders.set(track.id, sender);
    if (kind === "screen" && track.kind === "video") {
      // Keep text legible: drop frame rate before resolution when bandwidth is short.
      const params = sender.getParameters();
      params.degradationPreference = "maintain-resolution";
      sender.setParameters(params).catch(() => {});
    }
  }

  /** Send now, or queue until the channel opens. */
  private sendTo(peer: Peer, msg: PeerMessage): boolean {
    const state = peer.channel.readyState;
    if (state === "open") {
      peer.channel.send(JSON.stringify(msg));
      return true;
    }
    if (state === "connecting" && peer.outbox.length < MAX_QUEUED) {
      peer.outbox.push(JSON.stringify(msg));
      return true;
    }
    return false;
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
    pc.ontrack = ({ track, streams }) => {
      const stream = streams[0] ?? new MediaStream([track]);
      this.remoteTrack(id, stream, stream.id);
    };

    channel.onopen = () => {
      // Tell the new peer what our streams are and our current mic/cam state,
      // then deliver anything queued before the channel opened.
      for (const msg of this.introduction()) this.sendTo(peer, msg);
      for (const queued of peer.outbox.splice(0)) channel.send(queued);
    };
    channel.onmessage = (ev) => this.peerMessage(id, ev.data);
    pc.ondatachannel = ({ channel: extra }) => this.emit("channel", { peerId: id, channel: extra, label: extra.label });

    for (const [kind, stream] of this.local) {
      stream.getTracks().forEach((t) => this.addTrack(peer, t, stream, kind));
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
    this.forgetPeer(id);
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
