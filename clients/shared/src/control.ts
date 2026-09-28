import { parseRemoteInput, type ControlPermission, type Monitor, type NativeAgent, type RemoteInput } from "./agent";
import { Emitter } from "./emitter";
import type { PeerMesh } from "./mesh";
import type { ParticipantId } from "./protocol";

/**
 * Remote control over the peer data channel.
 *
 * Host = the participant sharing their whole screen from the desktop app.
 * Viewer = someone watching that screen who asks for control.
 *
 *   viewer ── control-request {perms} ──► host   (host sees a prompt)
 *   viewer ◄── control-grant {perms} ──── host   (after a native confirmation)
 *   viewer ── input {event} ────────────► host   (checked again natively)
 *   either ── control-revoke / control-release ─► ends control immediately
 */

export type ViewerStatus = "none" | "pending" | "granted";

const PERMISSIONS: ControlPermission[] = ["mouse", "keyboard", "clipboard"];

type Events = {
  /** Host: a peer asks for control. */
  request: { peerId: ParticipantId; permissions: ControlPermission[] };
  /** Host: the set of peers currently in control changed. */
  grants: Map<ParticipantId, ControlPermission[]>;
  /** Viewer: our control status over `peerId`'s screen changed. */
  viewer: { peerId: ParticipantId; status: ViewerStatus; permissions: ControlPermission[] };
  /** Clipboard text shared by a peer; `applied` if written to our clipboard under a grant. */
  clipboard: { peerId: ParticipantId; text: string; applied: boolean };
  notice: string;
};

export class RemoteControl extends Emitter<Events> {
  private grants = new Map<ParticipantId, ControlPermission[]>();
  private viewing = new Map<ParticipantId, { status: ViewerStatus; permissions: ControlPermission[] }>();
  private unsubscribe: Array<() => void>;
  private lastInjectError = 0;

  constructor(
    private readonly mesh: PeerMesh,
    private readonly agent: NativeAgent | undefined,
    private readonly nameOf: (id: ParticipantId) => string,
  ) {
    super();
    this.unsubscribe = [
      mesh.on("message", ({ peerId, msg }) => void this.onMessage(peerId, msg)),
      mesh.on("local-media", () => this.refreshAvailability()),
      mesh.on("peer-removed", (peerId) => {
        void this.revoke(peerId, false);
        this.setViewer(peerId, "none", []);
      }),
    ];
  }

  /** Our shared screen can be controlled: native agent present and a whole monitor is shared. */
  get available(): boolean {
    const surface = this.mesh.screenSettings?.displaySurface;
    return !!this.agent?.canControl && this.mesh.mediaState.screen && surface === "monitor";
  }

  viewerStatus(peerId: ParticipantId): { status: ViewerStatus; permissions: ControlPermission[] } {
    return this.viewing.get(peerId) ?? { status: "none", permissions: [] };
  }

  // ----- host side -------------------------------------------------------

  async grant(peerId: ParticipantId, permissions: ControlPermission[]): Promise<void> {
    if (!this.available || !this.agent?.grantControl) return this.deny(peerId, "unsupported");
    const monitor = await this.pickMonitor();
    if (!monitor) return this.deny(peerId, "unsupported");
    const ok = await this.agent.grantControl(peerId, this.nameOf(peerId), permissions, monitor);
    if (!ok) return this.deny(peerId, "declined");
    this.grants.set(peerId, permissions);
    this.mesh.send(peerId, { t: "control-grant", permissions });
    this.emit("grants", new Map(this.grants));
  }

  deny(peerId: ParticipantId, reason: "declined" | "unsupported"): void {
    this.mesh.send(peerId, { t: "control-deny", reason });
  }

  async revoke(peerId: ParticipantId, notify = true): Promise<void> {
    if (!this.grants.delete(peerId)) return;
    await this.agent?.revokeControl?.(peerId);
    if (notify) this.mesh.send(peerId, { t: "control-revoke" });
    this.emit("grants", new Map(this.grants));
  }

  async revokeAll(): Promise<void> {
    await Promise.all([...this.grants.keys()].map((id) => this.revoke(id)));
  }

  // ----- viewer side -----------------------------------------------------

  request(peerId: ParticipantId, permissions: ControlPermission[]): void {
    if (this.mesh.send(peerId, { t: "control-request", permissions })) {
      this.setViewer(peerId, "pending", permissions);
    }
  }

  release(peerId: ParticipantId): void {
    if (this.viewerStatus(peerId).status === "none") return;
    this.mesh.send(peerId, { t: "control-release" });
    this.setViewer(peerId, "none", []);
  }

  sendInput(peerId: ParticipantId, input: RemoteInput): void {
    if (this.viewerStatus(peerId).status === "granted") this.mesh.send(peerId, { t: "input", ...input });
  }

  shareClipboard(text: string): void {
    this.mesh.broadcast({ t: "clipboard", text: text.slice(0, 100_000) });
  }

  close(): void {
    void this.revokeAll();
    this.unsubscribe.forEach((u) => u());
  }

  // ----- internals -------------------------------------------------------

  private refreshAvailability(): void {
    const available = this.available;
    this.mesh.setControlAvailable(available);
    if (!available && this.grants.size) {
      void this.revokeAll();
      this.emit("notice", "Remote control ended because screen sharing stopped.");
    }
  }

  private async pickMonitor(): Promise<Monitor | null> {
    const monitors = (await this.agent?.monitors?.()) ?? [];
    const settings = this.mesh.screenSettings;
    const exact = monitors.filter((m) => m.width === settings?.width && m.height === settings?.height);
    return (exact.length === 1 ? exact[0] : undefined) ?? monitors.find((m) => m.primary) ?? monitors[0] ?? null;
  }

  private setViewer(peerId: ParticipantId, status: ViewerStatus, permissions: ControlPermission[]): void {
    const prev = this.viewerStatus(peerId);
    if (prev.status === status && prev.permissions.join() === permissions.join()) return;
    if (status === "none") this.viewing.delete(peerId);
    else this.viewing.set(peerId, { status, permissions });
    this.emit("viewer", { peerId, status, permissions });
  }

  private async onMessage(peerId: ParticipantId, msg: { t: string; [k: string]: unknown }): Promise<void> {
    switch (msg.t) {
      case "control-request": {
        const permissions = parsePermissions(msg.permissions);
        if (!this.available) return this.deny(peerId, "unsupported");
        if (this.grants.has(peerId)) {
          this.mesh.send(peerId, { t: "control-grant", permissions: this.grants.get(peerId) });
          return;
        }
        if (permissions.length) this.emit("request", { peerId, permissions });
        return;
      }
      case "control-release":
        return this.revoke(peerId, false);
      case "input": {
        if (!this.grants.has(peerId) || !this.agent?.inject) return;
        const input = parseRemoteInput(msg);
        if (!input) return;
        try {
          await this.agent.inject(peerId, input);
        } catch (err) {
          if (Date.now() - this.lastInjectError > 5000) {
            this.lastInjectError = Date.now();
            this.emit("notice", `Remote input was blocked: ${String(err)}`);
          }
        }
        return;
      }
      case "clipboard": {
        if (typeof msg.text !== "string") return;
        const text = msg.text.slice(0, 100_000);
        let applied = false;
        if (this.grants.get(peerId)?.includes("clipboard") && this.agent?.writeClipboard) {
          try {
            await this.agent.writeClipboard(peerId, text);
            applied = true;
          } catch {
            /* fall back to showing it */
          }
        }
        this.emit("clipboard", { peerId, text, applied });
        return;
      }
      case "control-grant":
        if (this.viewerStatus(peerId).status !== "none") {
          this.setViewer(peerId, "granted", parsePermissions(msg.permissions));
        }
        return;
      case "control-deny":
        if (this.viewerStatus(peerId).status === "pending") {
          this.emit(
            "notice",
            msg.reason === "unsupported"
              ? `${this.nameOf(peerId)} can't be controlled. They need the Connexa desktop app and must share their entire screen.`
              : `${this.nameOf(peerId)} declined your control request.`,
          );
        }
        this.setViewer(peerId, "none", []);
        return;
      case "control-revoke":
        if (this.viewerStatus(peerId).status !== "none") this.emit("notice", `${this.nameOf(peerId)} ended remote control.`);
        this.setViewer(peerId, "none", []);
        return;
    }
  }
}

function parsePermissions(raw: unknown): ControlPermission[] {
  if (!Array.isArray(raw)) return [];
  return PERMISSIONS.filter((p) => raw.includes(p));
}
