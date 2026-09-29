import type { NativeScreenSource } from "./agent";
import { Emitter } from "./emitter";
import { randomId } from "./format";
import type { IceServerConfig, ParticipantId, ParticipantInfo, Topology } from "./protocol";

/** "camera" carries mic + camera tracks, "screen" carries the screen share. */
export type StreamKind = "camera" | "screen";
/** How media reaches a peer: direct P2P, via a TURN relay, or via the server's SFU. */
export type RouteType = "direct" | "relay" | "sfu" | "unknown";

export interface MediaState {
  mic: boolean;
  cam: boolean;
  screen: boolean;
  /** This participant's shared screen can be remote-controlled (native agent + whole monitor shared). */
  control: boolean;
}

export interface ChatMessage {
  id: string;
  from: ParticipantId;
  text: string;
  ts: number;
}

/**
 * Messages exchanged between participants over data channels (P2P in mesh
 * rooms, relayed by the SFU in large rooms). The session handles
 * chat/stream/state itself; everything else is surfaced as a "message" event
 * for feature modules (file transfer, remote control, clipboard).
 */
export type PeerMessage =
  | { t: "chat"; id: string; text: string; ts: number }
  | { t: "stream"; streamId: string; kind: StreamKind }
  | ({ t: "state" } & MediaState)
  | { t: string; [key: string]: unknown };

export type SessionEvents = {
  "peer-added": ParticipantId;
  "peer-removed": ParticipantId;
  "peer-state": { id: ParticipantId; state: RTCPeerConnectionState; route: RouteType };
  "local-stream": { kind: StreamKind; stream: MediaStream | null };
  "local-media": MediaState;
  "remote-stream": { peerId: ParticipantId; stream: MediaStream; kind: StreamKind };
  "remote-stream-removed": { peerId: ParticipantId; streamId: string };
  "remote-media": { peerId: ParticipantId; state: MediaState };
  chat: ChatMessage;
  /** A peer message not handled by the session itself. Untrusted: validate before use. */
  message: { peerId: ParticipantId; msg: { t: string; [key: string]: unknown } };
  /** An extra data channel opened by a peer (e.g. a file transfer), with its logical label. */
  channel: { peerId: ParticipantId; channel: RTCDataChannel; label: string };
};

export const MAX_CHAT_CHARS = 4000;
/** Upper bound for any peer message (clipboard text is the largest). */
export const MAX_MESSAGE_CHARS = 256 * 1024;

interface RemotePeer {
  streams: Map<string, MediaStream>;
  kinds: Map<string, StreamKind>;
}

/**
 * Local media, chat and peer-message handling shared by the mesh (small rooms)
 * and the SFU session (large rooms). Subclasses decide how tracks and messages
 * reach other participants.
 */
export abstract class MediaSession extends Emitter<SessionEvents> {
  abstract readonly topology: Topology;
  protected local = new Map<StreamKind, MediaStream>();
  protected media: MediaState = { mic: false, cam: false, screen: false, control: false };
  private remote = new Map<ParticipantId, RemotePeer>();
  /** Viewer side: receive-only links for peers sharing a native (Android) screen. */
  private sideLinks = new Map<string, RTCPeerConnection>();
  /** Sender side: our native screen source, while sharing. */
  private nativeScreen: NativeScreenSource | null = null;

  constructor(
    protected readonly selfId: ParticipantId,
    protected readonly iceServers: IceServerConfig[],
  ) {
    super();
    this.on("peer-added", (id) => this.nativeScreen?.offer(id));
    this.on("peer-removed", (id) => {
      this.nativeScreen?.close(id);
      for (const [key, pc] of this.sideLinks) {
        if (key.startsWith(`${id}:`)) {
          pc.close();
          this.sideLinks.delete(key);
        }
      }
    });
  }

  abstract peerIds(): ParticipantId[];
  /** Send a message to one participant; false if it can't be delivered right now. */
  abstract send(peerId: ParticipantId, msg: PeerMessage): boolean;
  abstract broadcast(msg: PeerMessage): void;
  /** Open an extra reliable data channel to one participant ("channel" event there). */
  abstract createChannel(peerId: ParticipantId, label: string): RTCDataChannel | null;
  abstract connectTo(participants: ParticipantInfo[]): void;
  protected abstract onLocalTrackAdded(track: MediaStreamTrack, stream: MediaStream, kind: StreamKind): void;
  protected abstract onLocalTrackRemoved(track: MediaStreamTrack): void;
  protected abstract dispose(): void;

  get mediaState(): MediaState {
    return { ...this.media };
  }

  /** The shared screen track's settings (surface type, size), if sharing. */
  get screenSettings(): MediaTrackSettings | null {
    return this.local.get("screen")?.getVideoTracks()[0]?.getSettings() ?? null;
  }

  /** Whether remote control of our shared screen is currently offered to peers. */
  setControlAvailable(available: boolean): void {
    if (this.media.control !== available) this.setMedia({ control: available });
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
      this.onLocalTrackAdded(track, stream, "camera");
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
      this.onLocalTrackAdded(track, stream, "camera");
    } else if (!on && existing) {
      // Stop the track so the camera light turns off.
      existing.stop();
      stream.removeTrack(existing);
      this.onLocalTrackRemoved(existing);
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
    this.attachScreen(stream);
  }

  /** Share an already-captured screen stream (also used by native capture). */
  attachScreen(stream: MediaStream): void {
    const video = stream.getVideoTracks()[0];
    // Optimize for desktop text rather than motion.
    if (video && "contentHint" in video) video.contentHint = "detail";
    video?.addEventListener("ended", () => this.stopScreenShare());

    this.local.set("screen", stream);
    this.broadcast({ t: "stream", streamId: stream.id, kind: "screen" });
    stream.getTracks().forEach((t) => this.onLocalTrackAdded(t, stream, "screen"));
    this.emit("local-stream", { kind: "screen", stream });
    this.setMedia({ screen: true });
  }

  /** Share the screen through a native capturer (Android). */
  async startNativeScreen(source: NativeScreenSource): Promise<void> {
    if (this.media.screen) return;
    if (!(await source.start(this.iceServers))) return;
    this.nativeScreen = source;
    source.onEvent = (e) => {
      if (e.type === "offer") this.send(e.peerId, { t: "nscreen-offer", sdp: e.sdp, streamId: source.streamId });
      else if (e.type === "ice") this.send(e.peerId, { t: "nscreen-ice", candidate: e.candidate });
      else this.stopScreenShare();
    };
    this.broadcast({ t: "stream", streamId: source.streamId, kind: "screen" });
    for (const id of this.peerIds()) source.offer(id);
    this.setMedia({ screen: true });
  }

  stopScreenShare(): void {
    const native = this.nativeScreen;
    if (native) {
      this.nativeScreen = null;
      native.onEvent = null;
      native.stop();
      this.broadcast({ t: "nscreen-stop" });
      this.setMedia({ screen: false, control: false });
      return;
    }
    const stream = this.local.get("screen");
    if (!stream) return;
    this.media.control = false;
    this.local.delete("screen");
    stream.getTracks().forEach((t) => {
      t.stop();
      this.onLocalTrackRemoved(t);
    });
    this.emit("local-stream", { kind: "screen", stream: null });
    this.setMedia({ screen: false });
  }

  // ----- chat ----------------------------------------------------------

  sendChat(text: string): void {
    const trimmed = text.trim().slice(0, MAX_CHAT_CHARS);
    if (!trimmed) return;
    const msg: ChatMessage = { id: randomId(), from: this.selfId, text: trimmed, ts: Date.now() };
    this.broadcast({ t: "chat", id: msg.id, text: msg.text, ts: msg.ts });
    this.emit("chat", msg);
  }

  close(): void {
    if (this.nativeScreen) this.stopScreenShare();
    for (const pc of this.sideLinks.values()) pc.close();
    this.sideLinks.clear();
    this.dispose();
    for (const stream of this.local.values()) stream.getTracks().forEach((t) => t.stop());
    this.local.clear();
    this.remote.clear();
  }

  // ----- helpers for subclasses -------------------------------------------

  /** Messages that tell a (new) peer what we are sending. */
  protected introduction(): PeerMessage[] {
    const msgs: PeerMessage[] = [...this.local].map(([kind, stream]) => ({ t: "stream", streamId: stream.id, kind }));
    if (this.nativeScreen) msgs.push({ t: "stream", streamId: this.nativeScreen.streamId, kind: "screen" });
    msgs.push({ t: "state", ...this.media });
    return msgs;
  }

  protected setMedia(patch: Partial<MediaState>): void {
    this.media = { ...this.media, ...patch };
    this.broadcast({ t: "state", ...this.media });
    this.emit("local-media", this.mediaState);
  }

  private cameraStream(): MediaStream {
    let stream = this.local.get("camera");
    if (!stream) {
      stream = new MediaStream();
      this.local.set("camera", stream);
    }
    return stream;
  }

  protected remotePeer(peerId: ParticipantId): RemotePeer {
    let r = this.remote.get(peerId);
    if (!r) this.remote.set(peerId, (r = { streams: new Map(), kinds: new Map() }));
    return r;
  }

  /**
   * A remote track arrived. `kindKey` is the stream id the sender announced
   * (differs from `stream.id` when the SFU rewrites stream ids).
   */
  protected remoteTrack(peerId: ParticipantId, stream: MediaStream, kindKey: string): void {
    const r = this.remotePeer(peerId);
    if (!r.streams.has(stream.id)) {
      r.streams.set(stream.id, stream);
      stream.addEventListener("removetrack", () => {
        if (stream.getTracks().length === 0 && r.streams.delete(stream.id)) {
          this.emit("remote-stream-removed", { peerId, streamId: stream.id });
        }
      });
    }
    (stream as MediaStream & { kindKey?: string }).kindKey = kindKey;
    this.emit("remote-stream", { peerId, stream, kind: r.kinds.get(kindKey) ?? "camera" });
  }

  protected remoteStreamEnded(peerId: ParticipantId, streamId: string): void {
    if (this.remote.get(peerId)?.streams.delete(streamId)) {
      this.emit("remote-stream-removed", { peerId, streamId });
    }
  }

  protected forgetPeer(peerId: ParticipantId): void {
    const r = this.remote.get(peerId);
    this.remote.delete(peerId);
    for (const streamId of r?.streams.keys() ?? []) this.emit("remote-stream-removed", { peerId, streamId });
  }

  /** Validate and dispatch a message from a peer. */
  protected peerMessage(peerId: ParticipantId, data: unknown): void {
    let msg: PeerMessage;
    if (typeof data === "string") {
      if (data.length > MAX_MESSAGE_CHARS) return;
      try {
        msg = JSON.parse(data);
      } catch {
        return;
      }
    } else if (data && typeof data === "object") {
      msg = data as PeerMessage;
    } else {
      return;
    }
    switch (msg?.t) {
      case "chat":
        if (typeof msg.text === "string" && typeof msg.id === "string") {
          this.emit("chat", {
            id: msg.id,
            from: peerId,
            text: msg.text.slice(0, MAX_CHAT_CHARS),
            ts: Number(msg.ts) || Date.now(),
          });
        }
        break;
      case "stream":
        if (typeof msg.streamId === "string" && (msg.kind === "camera" || msg.kind === "screen")) {
          const r = this.remotePeer(peerId);
          r.kinds.set(msg.streamId, msg.kind);
          for (const stream of r.streams.values()) {
            if ((stream as MediaStream & { kindKey?: string }).kindKey === msg.streamId) {
              this.emit("remote-stream", { peerId, stream, kind: msg.kind });
            }
          }
        }
        break;
      case "state":
        this.emit("remote-media", {
          peerId,
          state: {
            mic: msg.mic === true,
            cam: msg.cam === true,
            screen: msg.screen === true,
            control: msg.control === true,
          },
        });
        break;
      case "nscreen-offer":
        if (typeof msg.sdp === "string" && typeof msg.streamId === "string") {
          void this.acceptSideLink(peerId, msg.sdp, msg.streamId);
        }
        break;
      case "nscreen-answer":
        if (typeof msg.sdp === "string") this.nativeScreen?.answer(peerId, msg.sdp);
        break;
      case "nscreen-ice":
        if (msg.candidate && typeof msg.candidate === "object") {
          const c = msg.candidate as RTCIceCandidateInit;
          const link = this.sideLinks.get(`${peerId}:screen`);
          if (link) void link.addIceCandidate(c).catch(() => {});
          else this.nativeScreen?.candidate(peerId, c);
        }
        break;
      case "nscreen-stop": {
        const link = this.sideLinks.get(`${peerId}:screen`);
        if (link) {
          link.close();
          this.sideLinks.delete(`${peerId}:screen`);
          const streamId = (link as RTCPeerConnection & { streamId?: string }).streamId;
          if (streamId) this.remoteStreamEnded(peerId, streamId);
        }
        break;
      }
      default:
        if (typeof msg?.t === "string") {
          this.emit("message", { peerId, msg: msg as { t: string; [key: string]: unknown } });
        }
    }
  }

  /** Viewer side: answer a peer's native screen offer with a receive-only link. */
  private async acceptSideLink(peerId: ParticipantId, sdp: string, streamId: string): Promise<void> {
    const key = `${peerId}:screen`;
    this.sideLinks.get(key)?.close();
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
    this.sideLinks.set(key, pc);
    pc.onicecandidate = ({ candidate }) => {
      if (candidate) this.send(peerId, { t: "nscreen-ice", candidate: candidate.toJSON() });
    };
    pc.ontrack = ({ track, streams }) => {
      const stream = streams[0] ?? new MediaStream([track]);
      (pc as RTCPeerConnection & { streamId?: string }).streamId = stream.id;
      this.remotePeer(peerId).kinds.set(streamId, "screen");
      this.remoteTrack(peerId, stream, streamId);
    };
    try {
      await pc.setRemoteDescription({ type: "offer", sdp });
      await pc.setLocalDescription(await pc.createAnswer());
      this.send(peerId, { t: "nscreen-answer", sdp: pc.localDescription!.sdp });
    } catch (err) {
      console.warn("native screen link failed", err);
    }
  }
}
