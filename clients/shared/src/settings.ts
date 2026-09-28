import { escapeHtml } from "./format";

const SERVER_KEY = "connexa.serverUrl";

export function isValidServerUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "wss:" || url.protocol === "ws:";
  } catch {
    return false;
  }
}

export function loadServerUrl(fallback: string): string {
  try {
    const v = localStorage.getItem(SERVER_KEY);
    return v && isValidServerUrl(v) ? v : fallback;
  } catch {
    return fallback;
  }
}

export function saveServerUrl(url: string): void {
  try {
    localStorage.setItem(SERVER_KEY, url);
  } catch {
    /* storage unavailable */
  }
}

/** Full-page form for the signaling server URL (desktop and Android shells). */
export function renderServerSettings(root: HTMLElement, current: string, done: (url: string | null) => void): void {
  root.innerHTML = `
    <main class="home">
      <form class="home-card" id="settings-form">
        <h1 class="brand">Server</h1>
        <p class="tagline">The signaling server that hosts Internet sessions.</p>
        <label class="field">
          <span>WebSocket URL</span>
          <input id="server-url" required value="${escapeHtml(current)}" placeholder="wss://connexa.example.com/ws" autocomplete="url">
        </label>
        <p class="note">Use <code>wss://</code> for servers on the Internet. Plain <code>ws://</code> is only for local testing.</p>
        <p id="settings-error" class="error" role="alert"></p>
        <button class="btn primary wide">Save</button>
        <div class="home-links"><button type="button" class="link" id="settings-cancel">Cancel</button></div>
      </form>
    </main>`;
  const input = root.querySelector<HTMLInputElement>("#server-url")!;
  input.focus();
  root.querySelector("#settings-cancel")!.addEventListener("click", () => done(null));
  root.querySelector("#settings-form")!.addEventListener("submit", (ev) => {
    ev.preventDefault();
    const value = input.value.trim();
    if (!isValidServerUrl(value)) {
      root.querySelector("#settings-error")!.textContent = "Enter a ws:// or wss:// URL.";
      return;
    }
    saveServerUrl(value);
    done(value);
  });
}
