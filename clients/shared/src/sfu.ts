import type { IceServerConfig, ParticipantId, ParticipantInfo, SfuPc } from "./protocol";
import { MediaSession, type PeerMessage, type StreamKind } from "./session";
import type { SignalingClient } from "./signaling";

/** Camera video sent to the SFU is capped so large rooms stay within server bandwidth. */
const SFU_CAMERA_BITRATE = 600_000;
const SFU_SCREEN_BITRATE = 2_500_000;

/**
 * Large rooms: two connections with the server's SFU instead of one per peer.
 *
 * - publisher (we offer): our mic / camera / screen and the `connexa` data
 *   channel for peer messages, sent as `{to?, msg}`.
 * - subscriber (the SFU offers): everyone else's tracks, and a `connexa`
 *   channel delivering `{from, msg}`.
 *
 * Remote stream ids arrive as `<owner>~<original stream id>`.
 */
export class SfuSession extends MediaSession {
  readonly topology = "sfu" as const;
  private readonly pub: RTCPeerConnection;
  private readonly sub: RTCPeerConnection;
  private readonly uplink: RTCDataChannel;
  private peers = new Set<ParticipantId>();
  private senders = new Map<string, RTCRtpSender>();
  private negotiating = false;
  private negotiateAgain = false;
  private pending: Record<SfuPc, RTCIceCandidateInit[]> = { pub: [], sub: [] };
  private subOps: Promise<void> = Promise.resolve();
  /** Uplink messages queued until the data channel opens. */
  private outbox: string[] = [];
  private unsubscribe: Array<() => void> = [];

  constructor(
    private readonly signaling: SignalingClient,
    selfId: ParticipantId,
    iceServers: IceServerConfig[],
  ) {
    super(selfId, iceServers);
    this.pub = new RTCPeerConnection({ iceServers });
    this.sub = new RTCPeerConnection({ iceServers });
    this.uplink = this.pub.createDataChannel("connexa", { ordered: true });
    this.uplink.onopen = () => {
      for (const msg of this.introduction()) this.broadcast(msg);
      for (const queued of this.outbox.splice(0)) this.uplink.send(queued);
    };

    this.pub.onnegotiationneeded = () => void this.negotiate();
    this.pub.onicecandidate = ({ candidate }) => {
      if (candidate) this.signaling.send({ type: "sfu_candidate", pc: "pub", candidate: candidate.toJSON() });
    };
    this.sub.onicecandidate = ({ candidate }) => {
      if (candidate) this.signaling.send({ type: "sfu_candidate", pc: "sub", candidate: candidate.toJSON() });
    };
    for (const pc of [this.pub, this.sub]) {
      pc.onconnectionstatechange = () => {
        if (pc.connectionState === "failed") pc.restartIce();
        this.emitStates();
      };
    }

    this.sub.ontrack = ({ track, streams }) => {
      const stream = streams[0] ?? new MediaStream([track]);
      const [owner, original] = splitStreamId(stream.id);
      if (owner) this.remoteTrack(owner, stream, original);
    };
    this.sub.ondatachannel = ({ channel }) => {
      if (channel.label === "connexa") {
        channel.onmessage = (ev) => this.onRelayed(ev.data);
        return;
      }
      // from:<sender>:<label>, e.g. a file transfer
      const m = /^from:([^:]+):(.+)$/.exec(channel.label);
      if (m) this.emit("channel", { peerId: m[1], channel, label: m[2] });
      else channel.close();
    };

    this.unsubscribe.push(
      signaling.on("sfu-answer", (sdp) => void this.onAnswer(sdp)),
      signaling.on("sfu-offer", (sdp) => this.onOffer(sdp)),
      signaling.on("sfu-candidate", ({ pc, candidate }) => void this.onCandidate(pc, candidate)),
      signaling.on("participant-joined", (p) => this.addPeer(p.participant_id)),
      signaling.on("participant-left", (id) => this.removePeer(id)),
      signaling.on("resumed", (list) => {
        const present = new Set(list.map((p) => p.participant_id));
        for (const id of [...this.peers]) if (!present.has(id)) this.removePeer(id);
        for (const id of present) this.addPeer(id);
      }),
    );
  }

  peerIds(): ParticipantId[] {
    return [...this.peers];
  }

  send(peerId: ParticipantId, msg: PeerMessage): boolean {
    if (!this.peers.has(peerId)) return false;
    return this.up(JSON.stringify({ to: peerId, msg }));
  }

  broadcast(msg: PeerMessage): void {
    this.up(JSON.stringify({ msg }));
  }

  /** Send on the uplink, or queue until it opens. */
  private up(payload: string): boolean {
    if (this.uplink.readyState === "open") {
      this.uplink.send(payload);
      return true;
    }
    if (this.uplink.readyState === "connecting" && this.outbox.length < 256) {
      this.outbox.push(payload);
      return true;
    }
    return false;
  }

  createChannel(peerId: ParticipantId, label: string): RTCDataChannel | null {
    if (!this.peers.has(peerId) || this.pub.connectionState === "closed") return null;
    return this.pub.createDataChannel(`relay:${peerId}:${label}`, { ordered: true });
  }

  connectTo(participants: ParticipantInfo[]): void {
    participants.forEach((p) => this.addPeer(p.participant_id));
    // The uplink data channel already triggers the first publisher offer.
  }

  protected onLocalTrackAdded(track: MediaStreamTrack, stream: MediaStream, kind: StreamKind): void {
    const sender = this.pub.addTrack(track, stream);
    this.senders.set(track.id, sender);
    if (track.kind === "video") {
      const params = sender.getParameters();
      params.encodings = params.encodings?.length ? params.encodings : [{}];
      params.encodings[0].maxBitrate = kind === "screen" ? SFU_SCREEN_BITRATE : SFU_CAMERA_BITRATE;
      if (kind === "screen") params.degradationPreference = "maintain-resolution";
      sender.setParameters(params).catch(() => {});
    }
  }

  protected onLocalTrackRemoved(track: MediaStreamTrack): void {
    const sender = this.senders.get(track.id);
    if (!sender) return;
    this.senders.delete(track.id);
    if (this.pub.signalingState !== "closed") this.pub.removeTrack(sender);
  }

  protected dispose(): void {
    this.unsubscribe.forEach((u) => u());
    for (const id of [...this.peers]) this.removePeer(id);
    this.pub.close();
    this.sub.close();
  }

  private addPeer(id: ParticipantId): void {
    if (id === this.selfId || this.peers.has(id)) return;
    this.peers.add(id);
    this.emit("peer-added", id);
    // After the app has registered the participant (it listens after us).
    queueMicrotask(() => this.emitStates());
    // Tell the newcomer what we send (the SFU buffers it until their channel opens).
    for (const msg of this.introduction()) this.send(id, msg);
  }

  private removePeer(id: ParticipantId): void {
    if (!this.peers.delete(id)) return;
    this.forgetPeer(id);
    this.emit("peer-removed", id);
  }

  private emitStates(): void {
    const states = [this.pub.connectionState, this.sub.connectionState];
    const state: RTCPeerConnectionState = states.includes("failed")
      ? "failed"
      : states.every((s) => s === "connected")
        ? "connected"
        : states.includes("disconnected")
          ? "disconnected"
          : "connecting";
    for (const id of this.peers) this.emit("peer-state", { id, state, route: "sfu" });
  }

  private onRelayed(data: unknown): void {
    if (typeof data !== "string") return;
    let envelope: { from?: unknown; msg?: unknown };
    try {
      envelope = JSON.parse(data);
    } catch {
      return;
    }
    // `from` is set by the SFU, not by the sender.
    if (typeof envelope.from === "string" && this.peers.has(envelope.from)) {
      this.peerMessage(envelope.from, envelope.msg);
    }
  }

  private async negotiate(): Promise<void> {
    if (this.negotiating) {
      this.negotiateAgain = true;
      return;
    }
    this.negotiating = true;
    try {
      await this.pub.setLocalDescription(await this.pub.createOffer());
      this.signaling.send({ type: "sfu_offer", sdp: this.pub.localDescription!.sdp });
    } catch (err) {
      console.warn("SFU publisher offer failed", err);
      this.negotiating = false;
    }
  }

  private async onAnswer(sdp: string): Promise<void> {
    try {
      await this.pub.setRemoteDescription({ type: "answer", sdp });
      await this.flush("pub");
    } catch (err) {
      console.warn("SFU answer rejected", err);
    }
    this.negotiating = false;
    if (this.negotiateAgain) {
      this.negotiateAgain = false;
      void this.negotiate();
    }
  }

  private onOffer(sdp: string): void {
    this.subOps = this.subOps.then(async () => {
      try {
        await this.sub.setRemoteDescription({ type: "offer", sdp });
        await this.flush("sub");
        await this.sub.setLocalDescription(await this.sub.createAnswer());
        this.signaling.send({ type: "sfu_answer", sdp: this.sub.localDescription!.sdp });
      } catch (err) {
        console.warn("SFU subscriber negotiation failed", err);
      }
    });
  }

  private async onCandidate(which: SfuPc, candidate: RTCIceCandidateInit): Promise<void> {
    const pc = which === "pub" ? this.pub : this.sub;
    if (!pc.remoteDescription) {
      this.pending[which].push(candidate);
      return;
    }
    await pc.addIceCandidate(candidate).catch(() => {});
  }

  private async flush(which: SfuPc): Promise<void> {
    const pc = which === "pub" ? this.pub : this.sub;
    const queued = this.pending[which];
    this.pending[which] = [];
    for (const c of queued) await pc.addIceCandidate(c).catch(() => {});
  }
}

function splitStreamId(id: string): [string | null, string] {
  const i = id.indexOf("~");
  return i > 0 ? [id.slice(0, i), id.slice(i + 1)] : [null, id];
}
