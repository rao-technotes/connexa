export const DEFAULT_SERVER_URL = "ws://localhost:8080/ws";

export interface Settings {
  serverUrl: string;
  displayName: string;
}

export async function loadSettings(): Promise<Settings> {
  const stored = await chrome.storage.sync.get(["serverUrl", "displayName"]);
  return {
    serverUrl: typeof stored.serverUrl === "string" && stored.serverUrl ? stored.serverUrl : DEFAULT_SERVER_URL,
    displayName: typeof stored.displayName === "string" ? stored.displayName : "",
  };
}

export async function saveSettings(patch: Partial<Settings>): Promise<void> {
  await chrome.storage.sync.set(patch);
}

export function isValidServerUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "wss:" || url.protocol === "ws:";
  } catch {
    return false;
  }
}
