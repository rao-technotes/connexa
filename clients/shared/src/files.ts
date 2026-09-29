import { randomId } from "./format";
import { Emitter } from "./emitter";
import type { MediaSession } from "./session";
import type { ParticipantId } from "./protocol";

/**
 * Peer-to-peer file transfer over WebRTC data channels.
 *
 * Flow: sender offers (name, size) on the control channel → receiver must
 * explicitly accept → sender opens a dedicated channel `file:<id>` and streams
 * chunks with backpressure. Either side can cancel at any time.
 */

export const MAX_FILE_BYTES = 1024 * 1024 * 1024; // held in memory by the receiver
const CHUNK_BYTES = 16 * 1024; // safe for every browser's SCTP message size
const HIGH_WATER = 4 * 1024 * 1024;
const LOW_WATER = 1024 * 1024;
/** How long the sender waits for the receiver to confirm after sending the last byte. */
const ACK_TIMEOUT_MS = 120_000;

export type TransferState = "offered" | "waiting" | "active" | "done" | "rejected" | "cancelled" | "failed";

export interface Transfer {
  id: string;
  peerId: ParticipantId;
  direction: "in" | "out";
  name: string;
  size: number;
  mime: string;
  state: TransferState;
  bytes: number;
}

type Events = {
  update: Transfer;
  received: { transfer: Transfer; blob: Blob };
};

interface Internal extends Transfer {
  file?: File;
  chunks: ArrayBuffer[];
  channel?: RTCDataChannel;
}

export class FileTransfers extends Emitter<Events> {
  private transfers = new Map<string, Internal>();
  private unsubscribe: Array<() => void>;

  constructor(private readonly mesh: MediaSession) {
    super();
    this.unsubscribe = [
      mesh.on("message", ({ peerId, msg }) => this.onMessage(peerId, msg)),
      mesh.on("channel", ({ peerId, channel, label }) => this.onChannel(peerId, channel, label)),
      mesh.on("peer-removed", (peerId) => {
        for (const t of this.transfers.values()) if (t.peerId === peerId) this.finish(t, "failed");
      }),
    ];
  }

  /** Offer a file to every connected participant; returns false if too large. */
  offer(file: File): boolean {
    if (file.size > MAX_FILE_BYTES) return false;
    for (const peerId of this.mesh.peerIds()) {
      const t: Internal = {
        id: randomId(),
        peerId,
        direction: "out",
        name: file.name,
        size: file.size,
        mime: file.type || "application/octet-stream",
        state: "offered",
        bytes: 0,
        file,
        chunks: [],
      };
      this.transfers.set(t.id, t);
      this.mesh.send(peerId, { t: "file-offer", id: t.id, name: t.name, size: t.size, mime: t.mime });
      this.update(t);
    }
    return true;
  }

  accept(id: string): void {
    const t = this.transfers.get(id);
    if (!t || t.direction !== "in" || t.state !== "offered") return;
    t.state = "waiting";
    this.mesh.send(t.peerId, { t: "file-accept", id });
    this.update(t);
  }

  reject(id: string): void {
    const t = this.transfers.get(id);
    if (!t || t.state !== "offered") return;
    this.mesh.send(t.peerId, { t: "file-reject", id });
    this.finish(t, "rejected");
  }

  cancel(id: string): void {
    const t = this.transfers.get(id);
    if (!t || isFinal(t.state)) return;
    this.mesh.send(t.peerId, { t: "file-cancel", id });
    this.finish(t, "cancelled");
  }

  close(): void {
    for (const t of this.transfers.values()) if (!isFinal(t.state)) this.finish(t, "cancelled");
    this.unsubscribe.forEach((u) => u());
  }

  private onMessage(peerId: ParticipantId, msg: { t: string; [k: string]: unknown }): void {
    const id = typeof msg.id === "string" ? msg.id : "";
    const t = this.transfers.get(id);
    switch (msg.t) {
      case "file-offer": {
        const size = Number(msg.size);
        if (!id || this.transfers.has(id) || !Number.isSafeInteger(size) || size < 0) return;
        const incoming: Internal = {
          id,
          peerId,
          direction: "in",
          name: sanitizeFileName(String(msg.name ?? "file")),
          size,
          mime: typeof msg.mime === "string" ? msg.mime.slice(0, 100) : "application/octet-stream",
          state: "offered",
          bytes: 0,
          chunks: [],
        };
        this.transfers.set(id, incoming);
        if (size > MAX_FILE_BYTES) {
          this.mesh.send(peerId, { t: "file-reject", id });
          return this.finish(incoming, "rejected");
        }
        return this.update(incoming);
      }
      case "file-accept":
        if (t && t.peerId === peerId && t.direction === "out" && t.state === "offered") void this.send(t);
        return;
      case "file-reject":
        if (t && t.peerId === peerId) this.finish(t, "rejected");
        return;
      case "file-cancel":
        if (t && t.peerId === peerId) this.finish(t, "cancelled");
        return;
      case "file-done":
        if (t && t.peerId === peerId && t.direction === "out" && t.bytes === t.size) this.finish(t, "done");
        return;
    }
  }

  private onChannel(peerId: ParticipantId, channel: RTCDataChannel, label: string): void {
    const id = label.startsWith("file:") ? label.slice(5) : "";
    const t = this.transfers.get(id);
    if (!t || t.peerId !== peerId || t.direction !== "in" || t.state !== "waiting") {
      channel.close();
      return;
    }
    t.channel = channel;
    t.state = "active";
    channel.binaryType = "arraybuffer";
    let lastUpdate = 0;
    channel.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer) || t.state !== "active") return;
      t.chunks.push(ev.data);
      t.bytes += ev.data.byteLength;
      if (t.bytes > t.size) return this.finish(t, "failed");
      if (t.bytes === t.size) {
        const blob = new Blob(t.chunks, { type: t.mime });
        t.chunks = [];
        this.mesh.send(t.peerId, { t: "file-done", id: t.id });
        this.finish(t, "done");
        this.emit("received", { transfer: snapshot(t), blob });
      } else if (performance.now() - lastUpdate > 150) {
        lastUpdate = performance.now();
        this.update(t);
      }
    };
    channel.onclose = () => {
      if (t.state === "active") this.finish(t, "failed");
    };
    this.update(t);
  }

  private async send(t: Internal): Promise<void> {
    const channel = this.mesh.createChannel(t.peerId, `file:${t.id}`);
    if (!channel || !t.file) return this.finish(t, "failed");
    t.channel = channel;
    t.state = "active";
    this.update(t);
    channel.bufferedAmountLowThreshold = LOW_WATER;
    try {
      await new Promise<void>((resolve, reject) => {
        channel.onopen = () => resolve();
        channel.onerror = () => reject(new Error("channel error"));
        channel.onclose = () => reject(new Error("channel closed"));
      });
      let lastUpdate = 0;
      for (let offset = 0; offset < t.size; offset += CHUNK_BYTES) {
        if (t.state !== "active") return;
        if (channel.bufferedAmount > HIGH_WATER) {
          await new Promise<void>((r) => channel.addEventListener("bufferedamountlow", () => r(), { once: true }));
        }
        const chunk = await t.file.slice(offset, offset + CHUNK_BYTES).arrayBuffer();
        if (t.state !== "active") return;
        channel.send(chunk);
        t.bytes = Math.min(t.size, offset + chunk.byteLength);
        if (performance.now() - lastUpdate > 150) {
          lastUpdate = performance.now();
          this.update(t);
        }
      }
      // Everything is queued. Keep the channel open until the receiver confirms
      // (file-done) or closes its end: closing here could discard data still in
      // flight, especially when an SFU relays the channel.
      channel.onclose = () => {
        if (t.state === "active") this.finish(t, "done");
      };
      setTimeout(() => {
        if (t.state === "active") this.finish(t, "failed");
      }, ACK_TIMEOUT_MS);
    } catch {
      if (t.state === "active") this.finish(t, "failed");
    }
  }

  private finish(t: Internal, state: TransferState): void {
    if (isFinal(t.state)) return;
    t.state = state;
    t.chunks = [];
    t.file = undefined;
    const channel = t.channel;
    t.channel = undefined;
    if (channel) {
      channel.onclose = null;
      channel.close();
    }
    this.update(t);
  }

  private update(t: Internal): void {
    this.emit("update", snapshot(t));
  }
}

function snapshot(t: Internal): Transfer {
  const { id, peerId, direction, name, size, mime, state, bytes } = t;
  return { id, peerId, direction, name, size, mime, state, bytes };
}

function isFinal(state: TransferState): boolean {
  return state === "done" || state === "rejected" || state === "cancelled" || state === "failed";
}

export function sanitizeFileName(name: string): string {
  const cleaned = name
    .replace(/[\u0000-\u001f\u007f<>:"/\\|?*]/g, "_")
    .replace(/^[.\s]+/, "")
    .trim()
    .slice(0, 120);
  return cleaned || "file";
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}
