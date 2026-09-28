import type { ControlPermission, Monitor, NativeAgent, RemoteInput } from "../../shared/src/agent";
import { mountApp, type AppHandle, type AppOptions } from "../../shared/src/app";
import { escapeHtml, httpOriginFromWs, normalizeRoomCode } from "../../shared/src/format";
import { loadServerUrl, renderServerSettings } from "../../shared/src/settings";

// Windows desktop shell. Native features come from Rust via Tauri commands
// (see apps/desktop/src/lib.rs); `withGlobalTauri` exposes `invoke`.

type Invoke = <T>(cmd: string, args?: unknown, options?: { headers: Record<string, string> }) => Promise<T>;

declare global {
  interface Window {
    __TAURI__?: { core: { invoke: Invoke } };
  }
}

interface LanInfo {
  port: number;
  addresses: string[];
  host: string;
}

interface LanPeer {
  name: string;
  host: string;
  port: number;
}

const DEFAULT_SERVER = "ws://localhost:8080/ws";
const root = document.getElementById("app")!;
const invoke: Invoke = (cmd, args, options) => {
  const tauri = window.__TAURI__;
  if (!tauri) return Promise.reject(new Error("Connexa desktop runtime is not available"));
  return tauri.core.invoke(cmd, args, options);
};

let current: AppHandle | null = null;

async function createAgent(): Promise<NativeAgent> {
  const info = await invoke<{ input: boolean }>("agent_info").catch(() => ({ input: false }));
  return {
    platform: "windows",
    canControl: info.input,
    monitors: () => invoke<Monitor[]>("monitors"),
    grantControl: (peerId: string, peerName: string, permissions: ControlPermission[], monitor: Monitor) =>
      invoke<boolean>("control_grant", { peerId, peerName, permissions, monitor }),
    revokeControl: (peerId: string) => invoke<void>("control_revoke", { peerId }),
    inject: (peerId: string, input: RemoteInput) => invoke<void>("control_input", { peerId, input }),
    writeClipboard: (peerId: string, text: string) => invoke<void>("clipboard_write", { peerId, text }),
    readClipboard: () => invoke<string>("clipboard_read"),
    saveFile: async (blob: Blob, name: string) => {
      const bytes = new Uint8Array(await blob.arrayBuffer());
      await invoke<string>("save_file", bytes, { headers: { "x-file-name": encodeURIComponent(name) } });
    },
  };
}

const agentPromise = createAgent();

async function mount(options: Omit<AppOptions, "agent">): Promise<void> {
  current?.destroy();
  current = null;
  const agent = await agentPromise;
  current = mountApp(root, { ...options, agent });
}

function showInternet(): void {
  const server = loadServerUrl(DEFAULT_SERVER);
  void mount({
    signalingUrl: server,
    inviteBase: `${httpOriginFromWs(server)}/`,
    note: `Internet · ${new URL(server).host}`,
    homeLinks: [
      { label: "Server settings", onClick: showSettings },
      { label: "LAN mode (no server)", onClick: () => void showLan() },
    ],
  });
}

function showSettings(): void {
  current?.destroy();
  current = null;
  renderServerSettings(root, loadServerUrl(DEFAULT_SERVER), () => showInternet());
}

async function showLan(message = ""): Promise<void> {
  current?.destroy();
  current = null;
  root.innerHTML = `
    <main class="home">
      <div class="home-card">
        <h1 class="brand">LAN mode</h1>
        <p class="tagline">This computer hosts a temporary session server on your local network. No Internet server is involved.</p>
        <label class="field">
          <span>Your name</span>
          <input id="lan-name" maxlength="32" placeholder="Optional">
        </label>
        <button id="lan-host" class="btn primary wide">Host a session on this network</button>
        <div class="or"><span>or join a nearby session</span></div>
        <ul class="nearby" id="nearby"><li class="muted">Searching…</li></ul>
        <button id="lan-refresh" class="btn wide">Search again</button>
        <p id="lan-error" class="error" role="alert">${escapeHtml(message)}</p>
        <div class="home-links"><button type="button" class="link" id="lan-back">Back to Internet mode</button></div>
      </div>
    </main>`;
  const name = root.querySelector<HTMLInputElement>("#lan-name")!;
  try {
    name.value = localStorage.getItem("connexa.displayName") ?? "";
  } catch {
    /* storage unavailable */
  }
  root.querySelector("#lan-back")!.addEventListener("click", showInternet);
  root.querySelector("#lan-refresh")!.addEventListener("click", () => void search());
  root.querySelector("#lan-host")!.addEventListener("click", () => void hostLan(name.value.trim()));
  await search();
}

async function search(): Promise<void> {
  const list = root.querySelector("#nearby");
  if (!list) return;
  list.innerHTML = '<li class="muted">Searching…</li>';
  let peers: LanPeer[] = [];
  try {
    peers = await invoke<LanPeer[]>("lan_discover", { timeoutMs: 2500 });
  } catch (err) {
    list.innerHTML = `<li class="muted">Discovery failed: ${escapeHtml(String(err))}</li>`;
    return;
  }
  if (!root.contains(list)) return;
  if (!peers.length) {
    list.innerHTML = '<li class="muted">No sessions found. Ask the host for their address, or check that both devices are on the same network.</li>';
    return;
  }
  list.innerHTML = peers
    .map(
      (p, i) =>
        `<li><button class="nearby-item" data-i="${i}"><b>${escapeHtml(p.name)}</b><span class="muted small">${escapeHtml(p.host)}:${p.port}</span></button></li>`,
    )
    .join("");
  list.querySelectorAll<HTMLButtonElement>(".nearby-item").forEach((b) =>
    b.addEventListener("click", () => joinLan(peers[Number(b.dataset.i)])),
  );
}

function joinLan(peer: LanPeer): void {
  const code = normalizeRoomCode(prompt(`Enter the 9-digit code for ${peer.name}'s session`) ?? "");
  if (!code) return;
  void mount({
    signalingUrl: `ws://${peer.host}:${peer.port}/ws`,
    inviteBase: `http://${peer.host}:${peer.port}/`,
    note: `LAN · ${peer.name}`,
    initial: { action: "join", code },
    homeLinks: [{ label: "Back to LAN mode", onClick: () => void showLan() }],
  });
}

async function hostLan(name: string): Promise<void> {
  let info: LanInfo;
  try {
    info = await invoke<LanInfo>("lan_start", { name });
  } catch (err) {
    const el = root.querySelector("#lan-error");
    if (el) el.textContent = `Could not start the LAN server: ${String(err)}`;
    return;
  }
  const address = info.addresses[0] ?? "127.0.0.1";
  try {
    localStorage.setItem("connexa.displayName", name);
  } catch {
    /* storage unavailable */
  }
  void mount({
    signalingUrl: `ws://127.0.0.1:${info.port}/ws`,
    inviteBase: `http://${address}:${info.port}/`,
    note: `LAN · hosting on ${address}:${info.port}`,
    displayName: name,
    initial: { action: "create" },
    inviteQr: (url) => invoke<string>("qr_svg", { text: url }),
    onSessionEnd: () => {
      // The temporary server lives only as long as the meeting.
      void invoke("lan_stop").finally(() => void showLan("The LAN session ended and the local server was stopped."));
    },
  });
}

showInternet();
