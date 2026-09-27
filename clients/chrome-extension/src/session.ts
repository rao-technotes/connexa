import { mountApp, type AppOptions } from "../../shared/src/app";
import { httpOriginFromWs, normalizeRoomCode } from "../../shared/src/format";
import { loadSettings } from "./settings";

const settings = await loadSettings();
const params = new URLSearchParams(location.search);
const code = normalizeRoomCode(params.get("code") ?? "");

let initial: AppOptions["initial"];
if (params.get("action") === "create") initial = { action: "create" };
else if (params.get("action") === "join" && code) initial = { action: "join", code };

// Don't re-run create/join if this tab is reloaded.
history.replaceState(null, "", location.pathname);

mountApp(document.getElementById("app")!, {
  signalingUrl: settings.serverUrl,
  // Invite links point at the web client hosted by the same server, so invitees need no install.
  inviteBase: `${httpOriginFromWs(settings.serverUrl)}/`,
  displayName: settings.displayName,
  initial,
});
