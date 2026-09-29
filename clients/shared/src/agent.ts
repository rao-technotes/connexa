/**
 * Native capabilities provided by a platform shell (Windows desktop app,
 * Android app). The browser and extension run without one.
 */

export type ControlPermission = "mouse" | "keyboard" | "clipboard";

export interface Monitor {
  name: string;
  /** Physical pixels on the virtual desktop. */
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
}

/** Remote input, coordinates normalized to the shared screen (0..1). */
export type RemoteInput =
  | { e: "move"; x: number; y: number }
  | { e: "down" | "up"; x: number; y: number; button: "left" | "middle" | "right" }
  | { e: "wheel"; dx: number; dy: number }
  | { e: "key"; code: string; down: boolean };

export interface NativeAgent {
  platform: "windows" | "android";
  /** Can inject mouse/keyboard input (remote control host). */
  canControl: boolean;
  /** Native screen capture, where the web view has no getDisplayMedia (Android). */
  screen?: NativeScreenSource;
  monitors?(): Promise<Monitor[]>;
  /**
   * Grant control to a peer. Implementations must show a native confirmation
   * the web layer cannot bypass; resolves false if the user declines.
   */
  grantControl?(peerId: string, peerName: string, permissions: ControlPermission[], monitor: Monitor): Promise<boolean>;
  revokeControl?(peerId: string): Promise<void>;
  /** Injection is re-checked natively against the grant. */
  inject?(peerId: string, input: RemoteInput): Promise<void>;
  writeClipboard?(peerId: string, text: string): Promise<void>;
  readClipboard?(): Promise<string>;
  /** Save a received file (platforms where <a download> doesn't work). */
  saveFile?(blob: Blob, name: string): Promise<void>;
}

/**
 * A screen captured natively and sent over its own peer connection to each
 * viewer ("side link"). Offers, answers and ICE travel as peer messages.
 */
export interface NativeScreenSource {
  /** Ask the OS for capture permission and start capturing. Resolves false if declined. */
  start(iceServers: RTCIceServer[]): Promise<boolean>;
  /** Create a side link to a viewer; the offer arrives as an "offer" event. */
  offer(peerId: string): void;
  answer(peerId: string, sdp: string): void;
  candidate(peerId: string, candidate: RTCIceCandidateInit): void;
  close(peerId: string): void;
  stop(): void;
  /** Stream id the viewers will see, announced as a "screen" stream. */
  readonly streamId: string;
  onEvent: ((e: NativeScreenEvent) => void) | null;
}

export type NativeScreenEvent =
  | { type: "offer"; peerId: string; sdp: string }
  | { type: "ice"; peerId: string; candidate: RTCIceCandidateInit }
  | { type: "stopped" };

/** Validate an input event coming from an untrusted peer. */
export function parseRemoteInput(raw: unknown): RemoteInput | null {
  if (!raw || typeof raw !== "object") return null;
  const v = raw as Record<string, unknown>;
  const num = (n: unknown) => typeof n === "number" && Number.isFinite(n);
  switch (v.e) {
    case "move":
      return num(v.x) && num(v.y) ? { e: "move", x: v.x as number, y: v.y as number } : null;
    case "down":
    case "up":
      return num(v.x) && num(v.y) && (v.button === "left" || v.button === "middle" || v.button === "right")
        ? { e: v.e, x: v.x as number, y: v.y as number, button: v.button }
        : null;
    case "wheel":
      return num(v.dx) && num(v.dy) ? { e: "wheel", dx: v.dx as number, dy: v.dy as number } : null;
    case "key":
      return typeof v.code === "string" && v.code.length < 32 && typeof v.down === "boolean"
        ? { e: "key", code: v.code, down: v.down }
        : null;
    default:
      return null;
  }
}
