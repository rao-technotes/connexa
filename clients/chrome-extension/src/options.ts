import { isValidServerUrl, loadSettings, saveSettings } from "./settings";

const form = document.getElementById("form") as HTMLFormElement;
const server = document.getElementById("server") as HTMLInputElement;
const status = document.getElementById("status")!;

void loadSettings().then((s) => (server.value = s.serverUrl));

form.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const value = server.value.trim();
  if (!isValidServerUrl(value)) {
    status.textContent = "Enter a ws:// or wss:// URL.";
    return;
  }
  await saveSettings({ serverUrl: value });
  status.textContent = "Saved.";
});
