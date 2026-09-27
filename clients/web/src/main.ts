import { mountApp } from "../../shared/src/app";

// The web client is served by the signaling server itself, so signaling is same-origin.
const wsProtocol = location.protocol === "https:" ? "wss:" : "ws:";

mountApp(document.getElementById("app")!, {
  signalingUrl: `${wsProtocol}//${location.host}/ws`,
  inviteBase: `${location.origin}/`,
});
