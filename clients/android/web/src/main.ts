import type { NativeAgent, NativeScreenEvent, NativeScreenSource } from "../../../shared/src/agent";
import { mountApp, type AppHandle } from "../../../shared/src/app";
import { httpOriginFromWs } from "../../../shared/src/format";
import { loadServerUrl, renderServerSettings } from "../../../shared/src/settings";

// Android shell: the page runs inside the app's WebView (see MainActivity.kt).

interface AndroidBridge {
  saveFile(name: string, mime: string, base64: string): string;
  screenStart?(iceServersJson: string): void;
  screenOffer(peerId: string): void;
  screenAnswer(peerId: string, sdp: string): void;
  screenCandidate(peerId: string, candidateJson: string): void;
  screenClose(peerId: string): void;
  screenStop(): void;
}

declare global {
  interface Window {
    ConnexaAndroid?: AndroidBridge;
    /** Called by MainActivity with native events. */
    __connexaNative?: (event: NativeScreenEvent | { type: "started" | "denied" }) => void;
  }
}

/** Screen capture through MediaProjection + native WebRTC (see NativeScreenShare.kt). */
function nativeScreen(bridge: AndroidBridge): NativeScreenSource {
  let pendingStart: ((ok: boolean) => void) | null = null;
  const source: NativeScreenSource = {
    streamId: "android-screen",
    onEvent: null,
    start(iceServers) {
      return new Promise((resolve) => {
        pendingStart = resolve;
        bridge.screenStart!(JSON.stringify(iceServers));
      });
    },
    offer: (peerId) => bridge.screenOffer(peerId),
    answer: (peerId, sdp) => bridge.screenAnswer(peerId, sdp),
    candidate: (peerId, c) => bridge.screenCandidate(peerId, JSON.stringify(c)),
    close: (peerId) => bridge.screenClose(peerId),
    stop: () => bridge.screenStop(),
  };
  window.__connexaNative = (event) => {
    if (event.type === "started" || event.type === "denied") {
      pendingStart?.(event.type === "started");
      pendingStart = null;
    } else {
      source.onEvent?.(event as NativeScreenEvent);
    }
  };
  return source;
}

/** 10.0.2.2 is the host machine as seen from the Android emulator. */
const DEFAULT_SERVER = "ws://10.0.2.2:8080/ws";
/** Received files cross the JS bridge as base64 in memory, so keep them bounded. */
const MAX_SAVE_BYTES = 200 * 1024 * 1024;
const root = document.getElementById("app")!;
let current: AppHandle | null = null;

const agent: NativeAgent = {
  platform: "android",
  canControl: false,
  screen: window.ConnexaAndroid?.screenStart ? nativeScreen(window.ConnexaAndroid) : undefined,
  saveFile: async (blob, name) => {
    const bridge = window.ConnexaAndroid;
    if (!bridge) throw new Error("not running inside the Connexa app");
    if (blob.size > MAX_SAVE_BYTES) throw new Error("files over 200 MB can't be saved on Android yet");
    const dataUrl = await new Promise<string>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(String(reader.result));
      reader.onerror = () => reject(reader.error);
      reader.readAsDataURL(blob);
    });
    bridge.saveFile(name, blob.type, dataUrl.slice(dataUrl.indexOf(",") + 1));
  },
};

function showHome(): void {
  current?.destroy();
  const server = loadServerUrl(DEFAULT_SERVER);
  current = mountApp(root, {
    signalingUrl: server,
    inviteBase: `${httpOriginFromWs(server)}/`,
    agent: window.ConnexaAndroid ? agent : undefined,
    note: `Server · ${new URL(server).host}`,
    homeLinks: [{ label: "Server settings", onClick: showSettings }],
  });
}

function showSettings(): void {
  current?.destroy();
  current = null;
  renderServerSettings(root, loadServerUrl(DEFAULT_SERVER), () => showHome());
}

showHome();
