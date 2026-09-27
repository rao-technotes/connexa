/** Extract a 9-digit room code from user input ("847 291 653", "847-291-653", a join link). */
export function normalizeRoomCode(input: string): string | null {
  const fromLink = /[?&#]join=(\d{9})/.exec(input);
  if (fromLink) return fromLink[1];
  const digits = input.replace(/[\s-]/g, "");
  return /^\d{9}$/.test(digits) ? digits : null;
}

/** "847291653" -> "847 291 653" */
export function formatRoomCode(code: string): string {
  return code.replace(/\D/g, "").slice(0, 9).replace(/(\d{3})(?=\d)/g, "$1 ");
}

export function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

/** Derive the HTTP origin serving the web client from a signaling WebSocket URL. */
export function httpOriginFromWs(wsUrl: string): string {
  const url = new URL(wsUrl);
  url.protocol = url.protocol === "wss:" ? "https:" : "http:";
  return url.origin;
}
