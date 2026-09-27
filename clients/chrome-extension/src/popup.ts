import { formatRoomCode, normalizeRoomCode } from "../../shared/src/format";
import { loadSettings, saveSettings } from "./settings";

// The popup closes as soon as it loses focus, so the session itself runs in a
// full extension tab (which can also use getUserMedia / getDisplayMedia).

const name = document.getElementById("name") as HTMLInputElement;
const code = document.getElementById("code") as HTMLInputElement;
const error = document.getElementById("error")!;

void loadSettings().then((s) => {
  name.value = s.displayName;
  code.focus();
});

code.addEventListener("input", () => {
  code.value = formatRoomCode(code.value.replace(/\D/g, "").slice(0, 9));
});

async function open(params: Record<string, string>): Promise<void> {
  await saveSettings({ displayName: name.value.trim() });
  const url = new URL(chrome.runtime.getURL("session.html"));
  for (const [k, v] of Object.entries(params)) url.searchParams.set(k, v);
  await chrome.tabs.create({ url: url.toString() });
  window.close();
}

document.getElementById("create")!.addEventListener("click", () => void open({ action: "create" }));

document.getElementById("join-form")!.addEventListener("submit", (ev) => {
  ev.preventDefault();
  const normalized = normalizeRoomCode(code.value);
  if (!normalized) {
    error.textContent = "Session codes are 9 digits.";
    return;
  }
  void open({ action: "join", code: normalized });
});

document.getElementById("settings")!.addEventListener("click", (ev) => {
  ev.preventDefault();
  void chrome.runtime.openOptionsPage();
});
