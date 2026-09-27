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

export function mountApp(root: HTMLElement, options: AppOptions): void {
  new ConnexaApp(root, options).start();
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

  constructor(
    private readonly root: HTMLElement,
    private readonly options: AppOptions,
  ) {}

  start(): void {
    window.addEventListener("pagehide", () => this.signaling?.leave());
    const initial = this.options.initial;
    const joinParam = normalizeRoomCode(location.search + location.hash);
    this.renderHome(initial?.action === "join" ? initial.code : (joinParam ?? ""));
    if (initial?.action === "create") void this.begin(null);
    else if (initial?.action === "join") void this.begin(initial.code);
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
        </div>
      </main>`;
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
    const idle: MediaState = { mic: false, cam: false, screen: false };

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
      this.root.querySelector(".banner")?.toggleAttribute("hidden", status !== "reconnecting");
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
          </div>
        </header>
        <div class="banner" hidden>Reconnecting to server… calls already connected keep running.</div>
        <div class="workspace">
          <section class="stage" id="stage" aria-label="Video"></section>
          <aside class="side" id="side">
            <section class="panel">
              <h2>Participants <span id="member-count" class="muted"></span></h2>
              <ul class="members" id="members"></ul>
            </section>
            <section class="panel chat">
              <h2>Chat <span class="muted small">peer-to-peer</span></h2>
              <ol class="messages" id="messages" aria-live="polite"></ol>
              <form class="chat-form" id="chat-form">
                <input id="chat-input" maxlength="4000" autocomplete="off" placeholder="Message everyone">
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
    this.mesh?.close();
    this.mesh = null;
    this.signaling = null;
    this.session = null;
    this.members.clear();
    this.tiles.clear();
    this.renderHome("", message);
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
      tile.innerHTML = `<video autoplay playsinline></video><span class="avatar"></span><span class="tile-label"></span>`;
      stage.appendChild(tile);
      this.tiles.set(key, tile);
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
