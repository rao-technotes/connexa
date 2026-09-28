import type { ControlPermission, NativeAgent, RemoteInput } from "./agent";
import { RemoteControl } from "./control";
import { FileTransfers, formatBytes, type Transfer } from "./files";
import { escapeHtml, formatRoomCode, normalizeRoomCode } from "./format";
import { PeerMesh, type ChatMessage, type MediaState, type RouteType, type StreamKind } from "./mesh";
import type { ParticipantId, ParticipantInfo, RoomEndReason } from "./protocol";
import { SignalingClient, SignalingError, type SessionInfo } from "./signaling";

export interface AppOptions {
  /** WebSocket URL of the signaling server, e.g. wss://connexa.example/ws */
  signalingUrl: string;
  /** Base URL for shareable join links; `?join=<code>` is appended. */
  inviteBase: string;
  /** Start immediately, e.g. when opened from the extension popup. */
  initial?: { action: "create" } | { action: "join"; code: string };
  displayName?: string;
  /** Native capabilities (desktop / Android shells). */
  agent?: NativeAgent;
  /** Extra links under the home card, e.g. "Server settings". */
  homeLinks?: Array<{ label: string; onClick: () => void }>;
  /** Short line shown on the home card, e.g. the current network mode. */
  note?: string;
  /** Render a QR code (SVG markup) for the invite link. */
  inviteQr?: (url: string) => Promise<string>;
  /** Called after leaving or losing a session. */
  onSessionEnd?: () => void;
}

export interface AppHandle {
  destroy(): void;
}

interface Member {
  info: ParticipantInfo;
  state: RTCPeerConnectionState | "self";
  route: RouteType;
  media: MediaState;
}

const NAME_KEY = "connexa.displayName";

const ERROR_TEXT: Record<string, string> = {
  room_not_found: "No active session with that code. Check the digits or ask for a new code.",
  room_full: "That session is full.",
  invalid_room_code: "Session codes are 9 digits.",
  rate_limited: "Too many attempts. Wait a minute and try again.",
  server_busy: "The server is busy. Try again shortly.",
  connection_failed: "Could not reach the Connexa server.",
};

const END_TEXT: Record<RoomEndReason | "connection_lost", string> = {
  host_ended: "The host ended the session.",
  host_left: "The host left, so the session ended.",
  expired: "The session expired.",
  connection_lost: "Lost connection to the server.",
};

const ICONS = {
  mic: '<path d="M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3Z"/><path d="M19 11a7 7 0 0 1-14 0M12 18v3"/>',
  cam: '<rect x="3" y="6" width="13" height="12" rx="2"/><path d="m16 10 5-3v10l-5-3"/>',
  screen: '<rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/>',
  chat: '<path d="M4 5h16v11H8l-4 4Z"/>',
  leave: '<path d="M15 4h4v16h-4M10 8l-4 4 4 4M6 12h10"/>',
  copy: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>',
  file: '<path d="M21 11.5 12.5 20a5 5 0 0 1-7-7l8.5-8.5a3.5 3.5 0 0 1 5 5L10.5 18a2 2 0 0 1-3-3l7.5-7.5"/>',
  clip: '<rect x="6" y="4" width="12" height="17" rx="2"/><path d="M9 4V3h6v1M9 10h6M9 14h6"/>',
  qr: '<rect x="4" y="4" width="6" height="6"/><rect x="14" y="4" width="6" height="6"/><rect x="4" y="14" width="6" height="6"/><path d="M14 14h2v2h-2zM18 18h2v2h-2zM14 18h2M18 14h2"/>',
  expand: '<path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/>',
  pointer: '<path d="m5 3 14 7-6 2-3 6Z"/>',
};

const PERMISSION_TEXT: Record<ControlPermission, string> = {
  mouse: "Control the mouse",
  keyboard: "Control the keyboard",
  clipboard: "Write to the clipboard",
};

function icon(name: keyof typeof ICONS): string {
  return `<svg viewBox="0 0 24 24" aria-hidden="true">${ICONS[name]}</svg>`;
}

function readName(): string {
  try {
    return localStorage.getItem(NAME_KEY) ?? "";
  } catch {
    return "";
  }
}

function saveName(name: string): void {
  try {
    localStorage.setItem(NAME_KEY, name);
  } catch {
    /* storage unavailable */
  }
}

export function mountApp(root: HTMLElement, options: AppOptions): AppHandle {
  const app = new ConnexaApp(root, options);
  app.start();
  return { destroy: () => app.destroy() };
}

class ConnexaApp {
  private signaling: SignalingClient | null = null;
  private mesh: PeerMesh | null = null;
  private session: SessionInfo | null = null;
  private members = new Map<ParticipantId, Member>();
  private tiles = new Map<string, HTMLElement>();
  private unreadChat = 0;
  private busy = false;
  private syncingTiles = false;
  private files: FileTransfers | null = null;
  private control: RemoteControl | null = null;
  private transferEls = new Map<string, HTMLElement>();
  private controlRequests: Array<{ peerId: ParticipantId; permissions: ControlPermission[] }> = [];
  private readonly onPageHide = () => this.signaling?.leave();

  constructor(
    private readonly root: HTMLElement,
    private readonly options: AppOptions,
  ) {}

  start(): void {
    window.addEventListener("pagehide", this.onPageHide);
    const initial = this.options.initial;
    const joinParam = normalizeRoomCode(location.search + location.hash);
    this.renderHome(initial?.action === "join" ? initial.code : (joinParam ?? ""));
    if (initial?.action === "create") void this.begin(null);
    else if (initial?.action === "join") void this.begin(initial.code);
  }

  destroy(): void {
    window.removeEventListener("pagehide", this.onPageHide);
    if (this.session) {
      const isHost = this.session.hostId === this.session.selfId;
      this.signaling?.leave(isHost);
      this.teardown();
    }
    this.root.innerHTML = "";
  }

  // ----- home ----------------------------------------------------------

  private renderHome(prefill = "", error = ""): void {
    this.root.innerHTML = `
      <main class="home">
        <div class="home-card">
          <h1 class="brand">Connexa</h1>
          <p class="tagline">Share a 9-digit code. Connect peer-to-peer.</p>
          <label class="field">
            <span>Your name</span>
            <input id="name" maxlength="32" autocomplete="nickname" placeholder="Optional">
          </label>
          <button id="create" class="btn primary wide">Start session</button>
          <div class="or"><span>or join one</span></div>
          <form id="join-form" class="join-row">
            <input id="code" inputmode="numeric" autocomplete="off" placeholder="847 291 653" aria-label="Session code">
            <button class="btn">Join</button>
          </form>
          <p id="home-error" class="error" role="alert">${escapeHtml(error)}</p>
          ${this.options.note ? `<p class="note">${escapeHtml(this.options.note)}</p>` : ""}
          ${
            this.options.homeLinks?.length
              ? `<div class="home-links">${this.options.homeLinks
                  .map((l, i) => `<button type="button" class="link" data-link="${i}">${escapeHtml(l.label)}</button>`)
                  .join("")}</div>`
              : ""
          }
        </div>
      </main>`;
    this.root.querySelectorAll<HTMLButtonElement>("[data-link]").forEach((b) =>
      b.addEventListener("click", () => this.options.homeLinks?.[Number(b.dataset.link)]?.onClick()),
    );
    const name = this.$<HTMLInputElement>("#name");
    const code = this.$<HTMLInputElement>("#code");
    name.value = this.options.displayName ?? readName();
    code.value = formatRoomCode(prefill);
    code.addEventListener("input", () => {
      const digits = code.value.replace(/\D/g, "").slice(0, 9);
      code.value = formatRoomCode(digits);
    });
    this.$("#create").addEventListener("click", () => void this.begin(null));
    this.$("#join-form").addEventListener("submit", (ev) => {
      ev.preventDefault();
      const normalized = normalizeRoomCode(code.value);
      if (!normalized) return this.showHomeError(ERROR_TEXT.invalid_room_code);
      void this.begin(normalized);
    });
    (prefill ? name : code).focus();
  }

  private showHomeError(text: string): void {
    const el = this.root.querySelector("#home-error");
    if (el) el.textContent = text;
  }

  /** Create a room (`code === null`) or join one. */
  private async begin(code: string | null): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    const nameInput = this.root.querySelector<HTMLInputElement>("#name");
    const name = (nameInput?.value ?? this.options.displayName ?? readName()).trim();
    if (nameInput) saveName(name);
    this.root.querySelectorAll<HTMLButtonElement>(".home button").forEach((b) => (b.disabled = true));
    this.showHomeError(code ? "Joining…" : "Creating session…");

    const signaling = new SignalingClient(this.options.signalingUrl);
    try {
      const { session, participants } = code
        ? await signaling.joinRoom(code, name)
        : await signaling.createRoom(name);
      this.enterSession(signaling, session, participants, name);
    } catch (err) {
      signaling.leave();
      const key = err instanceof SignalingError ? err.code : "connection_failed";
      this.renderHome(code ?? "", ERROR_TEXT[key] ?? (err as Error).message);
    } finally {
      this.busy = false;
    }
  }

  // ----- session -------------------------------------------------------

  private enterSession(
    signaling: SignalingClient,
    session: SessionInfo,
    participants: ParticipantInfo[],
    name: string,
  ): void {
    this.signaling = signaling;
    this.session = session;
    const mesh = new PeerMesh(signaling, session.selfId, session.iceServers);
    this.mesh = mesh;
    this.members.clear();
    this.tiles.clear();
    const isHost = session.hostId === session.selfId;
    const idle: MediaState = { mic: false, cam: false, screen: false, control: false };
    const files = new FileTransfers(mesh);
    const control = new RemoteControl(mesh, this.options.agent, (id) => this.nameOf(id));
    this.files = files;
    this.control = control;
    this.transferEls.clear();
    this.controlRequests = [];

    this.members.set(session.selfId, {
      info: { participant_id: session.selfId, display_name: name || (isHost ? "Host" : "You"), is_host: isHost },
      state: "self",
      route: "unknown",
      media: idle,
    });
    for (const p of participants) {
      this.members.set(p.participant_id, { info: p, state: "new", route: "unknown", media: idle });
    }
    history.replaceState(null, "", location.pathname);

    this.renderSession(isHost);

    signaling.on("participant-joined", (p) => {
      this.members.set(p.participant_id, { info: p, state: "new", route: "unknown", media: idle });
      this.toast(`${p.display_name} joined`);
      this.renderMembers();
    });
    signaling.on("participant-left", (id) => {
      const m = this.members.get(id);
      this.members.delete(id);
      if (m) this.toast(`${m.info.display_name} left`);
      this.renderMembers();
    });
    signaling.on("resumed", (list) => {
      for (const id of [...this.members.keys()]) {
        if (id !== session.selfId && !list.some((p) => p.participant_id === id)) this.members.delete(id);
      }
      for (const p of list) {
        if (!this.members.has(p.participant_id)) {
          this.members.set(p.participant_id, { info: p, state: "new", route: "unknown", media: idle });
        }
      }
      this.renderMembers();
    });
    signaling.on("status", (status) => {
      this.root.querySelector("#reconnect-banner")?.toggleAttribute("hidden", status !== "reconnecting");
    });
    signaling.on("ended", (reason) => this.exitSession(END_TEXT[reason]));
    signaling.on("error", ({ message }) => this.toast(message));

    mesh.on("peer-state", ({ id, state, route }) => {
      const m = this.members.get(id);
      if (!m) return;
      m.state = state;
      m.route = route;
      this.renderMembers();
    });
    mesh.on("remote-media", ({ peerId, state }) => {
      const m = this.members.get(peerId);
      if (m) m.media = state;
      this.renderMembers();
    });
    files.on("update", (t) => this.renderTransfer(t));
    files.on("received", ({ transfer, blob }) => void this.saveFile(blob, transfer.name));
    control.on("request", (req) => {
      this.controlRequests.push(req);
      this.showNextControlRequest();
    });
    control.on("grants", (grants) => this.renderControlBanner(grants));
    control.on("viewer", () => this.refreshScreenTiles());
    control.on("notice", (text) => this.toast(text));
    control.on("clipboard", ({ peerId, text, applied }) => this.appendClipboard(peerId, text, applied));
    mesh.on("local-media", (state) => {
      const me = this.members.get(session.selfId);
      if (me) me.media = state;
      this.renderControls();
      this.renderMembers();
    });
    mesh.on("local-stream", ({ kind, stream }) => {
      const key = `${session.selfId}:${kind}`;
      if (stream) this.upsertTile(key, session.selfId, stream, kind, true);
      else this.removeTile(key);
    });
    mesh.on("remote-stream", ({ peerId, stream, kind }) => this.upsertTile(`${peerId}:${stream.id}`, peerId, stream, kind, false));
    mesh.on("remote-stream-removed", ({ peerId, streamId }) => this.removeTile(`${peerId}:${streamId}`));
    mesh.on("peer-removed", (id) => {
      for (const key of [...this.tiles.keys()]) if (key.startsWith(`${id}:`)) this.removeTile(key);
    });
    mesh.on("chat", (msg) => this.appendChat(msg));

    // Always show a tile for yourself, even with camera off.
    this.upsertTile(`${session.selfId}:camera`, session.selfId, new MediaStream(), "camera", true);
    mesh.connectTo(participants);
  }

  private renderSession(isHost: boolean): void {
    const code = this.session!.roomId;
    this.root.innerHTML = `
      <div class="session">
        <header class="topbar">
          <div class="code-block">
            <span class="label">Session code</span>
            <span class="code" id="room-code">${formatRoomCode(code)}</span>
          </div>
          <div class="top-actions">
            <button class="btn ghost small" id="copy-code" title="Copy code">${icon("copy")}<span>Code</span></button>
            <button class="btn ghost small" id="copy-link" title="Copy invite link">${icon("link")}<span>Link</span></button>
            ${this.options.inviteQr ? `<button class="btn ghost small" id="show-qr" title="Show QR code">${icon("qr")}<span>QR</span></button>` : ""}
          </div>
        </header>
        <div class="banner" id="reconnect-banner" hidden>Reconnecting to server… calls already connected keep running.</div>
        <div class="banner control-banner" id="control-banner" hidden>
          <span id="control-text"></span>
          <button class="btn small" id="stop-control">Stop control</button>
        </div>
        <div class="workspace">
          <section class="stage" id="stage" aria-label="Video"></section>
          <aside class="side" id="side">
            <section class="panel">
              <h2>Participants <span id="member-count" class="muted"></span></h2>
              <ul class="members" id="members"></ul>
            </section>
            <section class="panel chat">
              <h2>Chat &amp; files <span class="muted small">peer-to-peer</span></h2>
              <ol class="messages" id="messages" aria-live="polite"></ol>
              <form class="chat-form" id="chat-form">
                <button type="button" class="icon-btn" id="send-file" title="Send files">${icon("file")}</button>
                <button type="button" class="icon-btn" id="send-clip" title="Share clipboard text">${icon("clip")}</button>
                <input type="file" id="file-input" multiple hidden>
                <input id="chat-input" maxlength="4000" autocomplete="off" placeholder="Message">
                <button class="btn small">Send</button>
              </form>
            </section>
          </aside>
        </div>
        <footer class="controls" id="controls">
          <button class="ctl" id="ctl-mic">${icon("mic")}<span>Mic</span></button>
          <button class="ctl" id="ctl-cam">${icon("cam")}<span>Camera</span></button>
          <button class="ctl" id="ctl-screen">${icon("screen")}<span>Share</span></button>
          <button class="ctl" id="ctl-chat">${icon("chat")}<span>Chat</span><b class="badge" id="chat-badge" hidden></b></button>
          <button class="ctl danger" id="ctl-leave">${icon("leave")}<span>${isHost ? "End" : "Leave"}</span></button>
        </footer>
        <div class="toasts" id="toasts" aria-live="polite"></div>
        <dialog class="dialog" id="control-dialog">
          <form method="dialog">
            <h2>Remote control request</h2>
            <p id="control-who"></p>
            <div id="control-perms" class="perm-list"></div>
            <p class="muted small">Only your shared screen is controlled. You can stop control at any time, and Windows will ask you to confirm.</p>
            <div class="dialog-actions">
              <button class="btn" value="deny">Deny</button>
              <button class="btn primary" value="allow">Allow…</button>
            </div>
          </form>
        </dialog>
        <dialog class="dialog" id="qr-dialog">
          <form method="dialog">
            <h2>Scan to join</h2>
            <div id="qr-image" class="qr"></div>
            <p class="muted small" id="qr-link"></p>
            <div class="dialog-actions"><button class="btn">Close</button></div>
          </form>
        </dialog>
      </div>`;

    this.$("#copy-code").addEventListener("click", () => this.copy(code, "Code copied"));
    this.$("#copy-link").addEventListener("click", () => this.copy(this.inviteLink(code), "Invite link copied"));
    this.$("#ctl-mic").addEventListener("click", () => this.toggle("mic"));
    this.$("#ctl-cam").addEventListener("click", () => this.toggle("cam"));
    this.$("#ctl-screen").addEventListener("click", () => this.toggle("screen"));
    this.$("#ctl-chat").addEventListener("click", () => {
      const side = this.$("#side");
      side.classList.toggle("open");
      if (side.classList.contains("open")) {
        this.unreadChat = 0;
        this.renderChatBadge();
        this.$<HTMLInputElement>("#chat-input").focus();
      }
    });
    this.$("#ctl-leave").addEventListener("click", () => {
      if (isHost && this.members.size > 1 && !confirm("End the session for everyone?")) return;
      this.leave();
    });
    this.$("#send-file").addEventListener("click", () => this.$<HTMLInputElement>("#file-input").click());
    this.$<HTMLInputElement>("#file-input").addEventListener("change", (ev) => {
      const input = ev.target as HTMLInputElement;
      this.sendFiles([...(input.files ?? [])]);
      input.value = "";
    });
    this.$("#send-clip").addEventListener("click", () => void this.shareClipboard());
    this.$("#stop-control").addEventListener("click", () => void this.control?.revokeAll());
    this.$<HTMLDialogElement>("#control-dialog").addEventListener("close", () => this.answerControlRequest());
    this.root.querySelector("#show-qr")?.addEventListener("click", () => void this.showQr(code));
    if (!navigator.mediaDevices?.getDisplayMedia) this.$("#ctl-screen").hidden = true;
    this.$("#chat-form").addEventListener("submit", (ev) => {
      ev.preventDefault();
      const input = this.$<HTMLInputElement>("#chat-input");
      this.mesh?.sendChat(input.value);
      input.value = "";
    });
    this.renderMembers();
    this.renderControls();
  }

  private inviteLink(code: string): string {
    const url = new URL(this.options.inviteBase);
    url.searchParams.set("join", code);
    return url.toString();
  }

  private async toggle(what: "mic" | "cam" | "screen"): Promise<void> {
    const mesh = this.mesh;
    if (!mesh) return;
    const state = mesh.mediaState;
    if (!navigator.mediaDevices) {
      // Browsers hide media devices on plain-http pages (e.g. a LAN invite link).
      return this.toast("Camera, microphone and screen sharing need HTTPS here. You can still watch, chat and send files, or use the Connexa app.");
    }
    try {
      if (what === "mic") await mesh.setMic(!state.mic);
      else if (what === "cam") await mesh.setCamera(!state.cam);
      else if (state.screen) mesh.stopScreenShare();
      else await mesh.startScreenShare();
    } catch (err) {
      const name = (err as DOMException)?.name;
      if (name === "NotAllowedError") {
        if (what !== "screen") this.toast(`Permission to use the ${what === "mic" ? "microphone" : "camera"} was denied.`);
      } else if (name === "NotFoundError") {
        this.toast(`No ${what === "mic" ? "microphone" : "camera"} found.`);
      } else {
        this.toast(`Could not start ${what === "screen" ? "screen sharing" : what}: ${(err as Error).message}`);
      }
    }
  }

  private leave(): void {
    const isHost = this.session?.hostId === this.session?.selfId;
    this.signaling?.leave(isHost);
    this.exitSession("");
  }

  private exitSession(message: string): void {
    this.teardown();
    this.renderHome("", message);
    this.options.onSessionEnd?.();
  }

  private teardown(): void {
    this.control?.close();
    this.files?.close();
    this.mesh?.close();
    this.control = null;
    this.files = null;
    this.mesh = null;
    this.signaling = null;
    this.session = null;
    this.members.clear();
    this.tiles.clear();
    this.transferEls.clear();
  }

  private nameOf(id: ParticipantId): string {
    return this.members.get(id)?.info.display_name ?? "Guest";
  }

  // ----- files & clipboard ---------------------------------------------

  private sendFiles(list: File[]): void {
    if (!this.files || !list.length) return;
    if (!this.mesh?.peerIds().length) return this.toast("No one else is in the session yet.");
    for (const file of list) {
      if (!this.files.offer(file)) this.toast(`${file.name} is larger than 1 GB.`);
    }
  }

  private renderTransfer(t: Transfer): void {
    const list = this.root.querySelector("#messages");
    if (!list) return;
    let li = this.transferEls.get(t.id);
    if (!li) {
      li = document.createElement("li");
      li.className = `transfer ${t.direction === "out" ? "mine" : ""}`;
      li.innerHTML = `<div class="meta"></div><div class="file-name"></div><progress></progress><div class="file-status"></div><div class="file-actions"></div>`;
      list.appendChild(li);
      list.scrollTop = list.scrollHeight;
      this.transferEls.set(t.id, li);
    }
    const peer = this.nameOf(t.peerId);
    li.querySelector(".meta")!.innerHTML =
      t.direction === "in" ? `<b>${escapeHtml(peer)}</b> sent a file` : `<b>You</b> → ${escapeHtml(peer)}`;
    li.querySelector(".file-name")!.textContent = `${t.name} · ${formatBytes(t.size)}`;
    const progress = li.querySelector("progress")!;
    progress.max = Math.max(t.size, 1);
    progress.value = t.bytes;
    progress.hidden = t.state !== "active";
    const status: Record<Transfer["state"], string> = {
      offered: t.direction === "in" ? "Do you want to receive it?" : "Waiting for them to accept…",
      waiting: "Starting…",
      active: `${formatBytes(t.bytes)} of ${formatBytes(t.size)}`,
      done: t.direction === "in" ? "Received" : "Sent",
      rejected: "Declined",
      cancelled: "Cancelled",
      failed: "Transfer failed",
    };
    li.querySelector(".file-status")!.textContent = status[t.state];
    const actions = li.querySelector<HTMLElement>(".file-actions")!;
    actions.innerHTML = "";
    const button = (label: string, cls: string, onClick: () => void) => {
      const b = document.createElement("button");
      b.className = `btn small ${cls}`;
      b.textContent = label;
      b.addEventListener("click", onClick);
      actions.appendChild(b);
    };
    if (t.direction === "in" && t.state === "offered") {
      button("Accept", "primary", () => this.files?.accept(t.id));
      button("Decline", "", () => this.files?.reject(t.id));
    } else if (t.state === "offered" || t.state === "waiting" || t.state === "active") {
      button("Cancel", "", () => this.files?.cancel(t.id));
    }
  }

  private async saveFile(blob: Blob, name: string): Promise<void> {
    try {
      if (this.options.agent?.saveFile) {
        await this.options.agent.saveFile(blob, name);
        this.toast(`Saved ${name}`);
        return;
      }
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = name;
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
    } catch (err) {
      this.toast(`Could not save ${name}: ${(err as Error).message}`);
    }
  }

  private async shareClipboard(): Promise<void> {
    if (!this.control) return;
    if (!this.mesh?.peerIds().length) return this.toast("No one else is in the session yet.");
    let text = "";
    try {
      text = this.options.agent?.readClipboard
        ? await this.options.agent.readClipboard()
        : await navigator.clipboard.readText();
    } catch {
      return this.toast("Clipboard access was denied.");
    }
    if (!text.trim()) return this.toast("Your clipboard has no text.");
    this.control.shareClipboard(text);
    this.appendClipboard(this.session!.selfId, text, false);
  }

  private appendClipboard(peerId: ParticipantId, text: string, applied: boolean): void {
    const list = this.root.querySelector("#messages");
    if (!list || !this.session) return;
    const mine = peerId === this.session.selfId;
    const li = document.createElement("li");
    li.className = `clip ${mine ? "mine" : ""}`;
    li.innerHTML = `<div class="meta"><b>${escapeHtml(mine ? "You" : this.nameOf(peerId))}</b> shared clipboard text</div><pre class="clip-text"></pre><div class="file-actions"></div>`;
    li.querySelector(".clip-text")!.textContent = text.length > 500 ? `${text.slice(0, 500)}…` : text;
    if (!mine) {
      const actions = li.querySelector(".file-actions")!;
      if (applied) {
        actions.textContent = "Copied to your clipboard";
      } else {
        const b = document.createElement("button");
        b.className = "btn small";
        b.textContent = "Copy";
        b.addEventListener("click", () => void this.copy(text, "Copied"));
        actions.appendChild(b);
      }
    }
    list.appendChild(li);
    list.scrollTop = list.scrollHeight;
  }

  private async showQr(code: string): Promise<void> {
    const link = this.inviteLink(code);
    try {
      const svg = await this.options.inviteQr!(link);
      this.$("#qr-image").innerHTML = svg;
      this.$("#qr-link").textContent = link;
      this.$<HTMLDialogElement>("#qr-dialog").showModal();
    } catch (err) {
      this.toast(`Could not create QR code: ${(err as Error).message}`);
    }
  }

  // ----- remote control --------------------------------------------------

  private showNextControlRequest(): void {
    const dialog = this.root.querySelector<HTMLDialogElement>("#control-dialog");
    const req = this.controlRequests[0];
    if (!dialog || !req || dialog.open) return;
    this.$("#control-who").innerHTML = `<b>${escapeHtml(this.nameOf(req.peerId))}</b> wants to control this computer.`;
    this.$("#control-perms").innerHTML = req.permissions
      .map(
        (p) =>
          `<label><input type="checkbox" value="${p}" ${p === "clipboard" ? "" : "checked"}> ${PERMISSION_TEXT[p]}</label>`,
      )
      .join("");
    dialog.returnValue = "";
    dialog.showModal();
  }

  private answerControlRequest(): void {
    const dialog = this.$<HTMLDialogElement>("#control-dialog");
    const req = this.controlRequests.shift();
    if (req && this.control) {
      const chosen = [...dialog.querySelectorAll<HTMLInputElement>("#control-perms input:checked")].map(
        (i) => i.value as ControlPermission,
      );
      if (dialog.returnValue === "allow" && chosen.length) void this.control.grant(req.peerId, chosen);
      else this.control.deny(req.peerId, "declined");
    }
    setTimeout(() => this.showNextControlRequest(), 0);
  }

  private renderControlBanner(grants: Map<ParticipantId, ControlPermission[]>): void {
    const banner = this.root.querySelector<HTMLElement>("#control-banner");
    if (!banner) return;
    banner.hidden = grants.size === 0;
    const parts = [...grants].map(([id, perms]) => `${this.nameOf(id)} (${perms.join(", ")})`);
    this.$("#control-text").textContent = `Remote control active: ${parts.join("; ")}`;
  }

  /** Buttons and interactivity on remote screen-share tiles. */
  private refreshScreenTiles(): void {
    const control = this.control;
    for (const tile of this.tiles.values()) {
      const actions = tile.querySelector<HTMLElement>(".tile-actions");
      if (!actions) continue;
      const remoteScreen = tile.dataset.kind === "screen" && !tile.classList.contains("local");
      const owner = tile.dataset.owner ?? "";
      const status = control?.viewerStatus(owner).status ?? "none";
      const controlling = remoteScreen && status === "granted";
      tile.classList.toggle("controlling", controlling);
      tile.tabIndex = controlling ? 0 : -1;
      const key = `${tile.dataset.kind}:${remoteScreen}:${status}:${this.members.get(owner)?.media.control}`;
      if (actions.dataset.key === key) continue;
      actions.dataset.key = key;
      actions.innerHTML = "";
      if (tile.dataset.kind !== "screen") continue;
      const add = (html: string, title: string, onClick: () => void) => {
        const b = document.createElement("button");
        b.className = "tile-btn";
        b.innerHTML = html;
        b.title = title;
        b.addEventListener("click", (ev) => {
          ev.stopPropagation();
          onClick();
        });
        actions.appendChild(b);
      };
      if (remoteScreen && control) {
        if (status === "granted") {
          add(`${icon("pointer")}<span>Release control</span>`, "Stop controlling", () => control.release(owner));
        } else if (status === "pending") {
          add(`${icon("pointer")}<span>Waiting for approval…</span>`, "Cancel request", () => control.release(owner));
        } else if (this.members.get(owner)?.media.control) {
          add(`${icon("pointer")}<span>Request control</span>`, "Ask to control this screen", () =>
            control.request(owner, ["mouse", "keyboard", "clipboard"]),
          );
        }
      }
      add(icon("expand"), "Full screen", () => void tile.requestFullscreen?.());
    }
  }

  /** Forward pointer and keyboard input on a controlled screen tile to its owner. */
  private attachControlInput(tile: HTMLElement): void {
    const video = tile.querySelector("video")!;
    const owner = () => tile.dataset.owner ?? "";
    const active = () => tile.classList.contains("controlling") && !!this.control;
    const send = (input: RemoteInput) => this.control?.sendInput(owner(), input);
    const point = (ev: MouseEvent): { x: number; y: number } | null => {
      const r = video.getBoundingClientRect();
      const vw = video.videoWidth;
      const vh = video.videoHeight;
      if (!vw || !vh) return null;
      // The video is letterboxed (object-fit: contain); map onto the picture itself.
      const scale = Math.min(r.width / vw, r.height / vh);
      const cw = vw * scale;
      const ch = vh * scale;
      const x = (ev.clientX - (r.left + (r.width - cw) / 2)) / cw;
      const y = (ev.clientY - (r.top + (r.height - ch) / 2)) / ch;
      return x >= 0 && x <= 1 && y >= 0 && y <= 1 ? { x, y } : null;
    };
    const buttons = ["left", "middle", "right"] as const;
    let queued: { x: number; y: number } | null = null;
    let frame = 0;

    video.addEventListener("pointermove", (ev) => {
      if (!active()) return;
      queued = point(ev);
      if (queued && !frame) {
        frame = requestAnimationFrame(() => {
          frame = 0;
          if (queued) send({ e: "move", ...queued });
          queued = null;
        });
      }
    });
    for (const type of ["pointerdown", "pointerup"] as const) {
      video.addEventListener(type, (ev) => {
        if (!active()) return;
        ev.preventDefault();
        if (type === "pointerdown") {
          tile.focus();
          video.setPointerCapture(ev.pointerId);
        }
        const p = point(ev);
        const button = buttons[ev.button];
        if (p && button) send({ e: type === "pointerdown" ? "down" : "up", ...p, button });
      });
    }
    video.addEventListener(
      "wheel",
      (ev) => {
        if (!active()) return;
        ev.preventDefault();
        const unit = ev.deltaMode === 1 ? 40 : ev.deltaMode === 2 ? 800 : 1;
        send({ e: "wheel", dx: ev.deltaX * unit, dy: ev.deltaY * unit });
      },
      { passive: false },
    );
    video.addEventListener("contextmenu", (ev) => {
      if (active()) ev.preventDefault();
    });
    const held = new Set<string>();
    tile.addEventListener("keydown", (ev) => {
      if (!active() || !ev.code) return;
      ev.preventDefault();
      held.add(ev.code);
      send({ e: "key", code: ev.code, down: true });
    });
    tile.addEventListener("keyup", (ev) => {
      if (!active() || !ev.code) return;
      ev.preventDefault();
      held.delete(ev.code);
      send({ e: "key", code: ev.code, down: false });
    });
    tile.addEventListener("blur", () => {
      for (const code of held) send({ e: "key", code, down: false });
      held.clear();
    });
  }

  // ----- rendering helpers ---------------------------------------------

  private renderControls(): void {
    const state = this.mesh?.mediaState;
    if (!state) return;
    const set = (id: string, on: boolean, onLabel: string, offLabel: string) => {
      const btn = this.root.querySelector<HTMLButtonElement>(id);
      if (!btn) return;
      btn.classList.toggle("on", on);
      btn.setAttribute("aria-pressed", String(on));
      btn.querySelector("span")!.textContent = on ? onLabel : offLabel;
    };
    set("#ctl-mic", state.mic, "Mute", "Unmute");
    set("#ctl-cam", state.cam, "Stop video", "Start video");
    set("#ctl-screen", state.screen, "Stop share", "Share");
  }

  private renderMembers(): void {
    const list = this.root.querySelector("#members");
    if (!list || !this.session) return;
    const selfId = this.session.selfId;
    const members = [...this.members.values()].sort((a, b) => Number(b.info.is_host) - Number(a.info.is_host));
    list.innerHTML = members
      .map((m) => {
        const you = m.info.participant_id === selfId;
        const status = you ? "" : connectionBadge(m.state, m.route);
        const flags = [
          m.info.is_host ? '<span class="tag">Host</span>' : "",
          you ? '<span class="tag">You</span>' : "",
        ].join("");
        return `<li>
          <span class="avatar small">${escapeHtml(initial(m.info.display_name))}</span>
          <span class="member-name">${escapeHtml(m.info.display_name)} ${flags}</span>
          <span class="member-media">
            <i class="${m.media.mic ? "live" : ""}" title="Microphone">${icon("mic")}</i>
            <i class="${m.media.cam ? "live" : ""}" title="Camera">${icon("cam")}</i>
            ${m.media.screen ? `<i class="live" title="Sharing screen">${icon("screen")}</i>` : ""}
          </span>
          ${status}
        </li>`;
      })
      .join("");
    const count = this.root.querySelector("#member-count");
    if (count) count.textContent = `${this.members.size}/${this.session.maxParticipants}`;
    this.tilesChanged();
  }

  private upsertTile(key: string, owner: ParticipantId, stream: MediaStream, kind: StreamKind, isLocal: boolean): void {
    const stage = this.root.querySelector("#stage");
    if (!stage) return;
    let tile = this.tiles.get(key);
    if (!tile) {
      tile = document.createElement("div");
      tile.className = "tile";
      tile.innerHTML = `<video autoplay playsinline></video><span class="avatar"></span><span class="tile-label"></span><div class="tile-actions"></div>`;
      stage.appendChild(tile);
      this.tiles.set(key, tile);
      this.attachControlInput(tile);
    }
    tile.dataset.owner = owner;
    tile.dataset.kind = kind;
    tile.classList.toggle("screen", kind === "screen");
    tile.classList.toggle("local", isLocal);
    const video = tile.querySelector("video")!;
    // Never play our own microphone back to ourselves.
    video.muted = isLocal;
    if (video.srcObject !== stream) video.srcObject = stream;
    stream.onaddtrack = stream.onremovetrack = () => this.refreshTileLabels();
    this.tilesChanged();
  }

  private removeTile(key: string): void {
    const tile = this.tiles.get(key);
    if (!tile) return;
    const video = tile.querySelector("video");
    if (video) video.srcObject = null;
    tile.remove();
    this.tiles.delete(key);
    this.tilesChanged();
  }

  private tilesChanged(): void {
    if (!this.syncingTiles) {
      this.syncingTiles = true;
      try {
        this.syncPlaceholders();
      } finally {
        this.syncingTiles = false;
      }
    }
    this.refreshTileLabels();
    this.layoutStage();
    this.refreshScreenTiles();
  }

  /** Remote participants without a camera/mic stream still get an avatar tile. */
  private syncPlaceholders(): void {
    const selfId = this.session?.selfId;
    for (const id of this.members.keys()) {
      if (id === selfId) continue;
      const key = `${id}:placeholder`;
      const hasStream = [...this.tiles].some(
        ([k, t]) => k !== key && t.dataset.owner === id && t.dataset.kind === "camera",
      );
      if (!hasStream && !this.tiles.has(key)) this.upsertTile(key, id, new MediaStream(), "camera", false);
      if (hasStream && this.tiles.has(key)) this.removeTile(key);
    }
    for (const [key, tile] of [...this.tiles]) {
      if (key.endsWith(":placeholder") && !this.members.has(tile.dataset.owner ?? "")) this.removeTile(key);
    }
  }

  private refreshTileLabels(): void {
    for (const tile of this.tiles.values()) {
      const member = this.members.get(tile.dataset.owner ?? "");
      const name = member?.info.display_name ?? "Guest";
      const video = tile.querySelector("video")!;
      const stream = video.srcObject as MediaStream | null;
      const hasVideo =
        !!stream?.getVideoTracks().some((t) => t.readyState === "live") &&
        (tile.dataset.kind === "screen" || member?.media.cam !== false);
      tile.classList.toggle("no-video", !hasVideo);
      tile.querySelector(".avatar")!.textContent = initial(name);
      const you = tile.classList.contains("local") ? " (you)" : "";
      const label = tile.dataset.kind === "screen" ? `${name}'s screen${you}` : `${name}${you}`;
      const muted = tile.dataset.kind === "camera" && member && !member.media.mic ? " · muted" : "";
      tile.querySelector(".tile-label")!.textContent = label + muted;
    }
  }

  /** Screen shares get the big spot; everything else shares the grid. */
  private layoutStage(): void {
    const stage = this.root.querySelector<HTMLElement>("#stage");
    if (!stage) return;
    const hasScreen = [...this.tiles.values()].some((t) => t.dataset.kind === "screen");
    stage.classList.toggle("presenting", hasScreen);
    stage.dataset.count = String(this.tiles.size);
  }

  private appendChat(msg: ChatMessage): void {
    const list = this.root.querySelector("#messages");
    if (!list || !this.session) return;
    const mine = msg.from === this.session.selfId;
    const name = mine ? "You" : (this.members.get(msg.from)?.info.display_name ?? "Guest");
    const time = new Date(msg.ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const li = document.createElement("li");
    li.className = mine ? "mine" : "";
    li.innerHTML = `<div class="meta"><b>${escapeHtml(name)}</b> <time>${time}</time></div><div class="text"></div>`;
    li.querySelector(".text")!.textContent = msg.text;
    list.appendChild(li);
    list.scrollTop = list.scrollHeight;
    if (!mine && !this.root.querySelector("#side")?.classList.contains("open") && window.innerWidth < 900) {
      this.unreadChat++;
      this.renderChatBadge();
    }
  }

  private renderChatBadge(): void {
    const badge = this.root.querySelector<HTMLElement>("#chat-badge");
    if (!badge) return;
    badge.hidden = this.unreadChat === 0;
    badge.textContent = String(this.unreadChat);
  }

  private async copy(text: string, done: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      this.toast(done);
    } catch {
      prompt("Copy this:", text);
    }
  }

  private toast(text: string): void {
    const box = this.root.querySelector("#toasts");
    if (!box) return;
    const el = document.createElement("div");
    el.className = "toast";
    el.textContent = text;
    box.appendChild(el);
    setTimeout(() => el.remove(), 3500);
  }

  private $<T extends HTMLElement = HTMLElement>(selector: string): T {
    return this.root.querySelector<T>(selector)!;
  }
}

function initial(name: string): string {
  return (name.trim()[0] ?? "?").toUpperCase();
}

function connectionBadge(state: RTCPeerConnectionState | "self", route: RouteType): string {
  if (state === "connected") {
    return route === "relay"
      ? '<span class="conn relay" title="Connected through a TURN relay">Relay</span>'
      : '<span class="conn ok" title="Direct peer-to-peer connection">P2P</span>';
  }
  if (state === "failed") return '<span class="conn bad">Failed</span>';
  if (state === "disconnected") return '<span class="conn warn">Reconnecting</span>';
  return '<span class="conn warn">Connecting</span>';
}
