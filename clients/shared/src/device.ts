/**
 * Device identity: an ECDSA P-256 key pair generated on first use and kept in
 * IndexedDB. The private key is non-extractable, so page script can use it to
 * sign but can never read or export it. The server derives the device ID from
 * the public key and verifies a signed challenge on every connection.
 */

const DB_NAME = "connexa-device";
const STORE = "keys";
const KEY_ID = "device";
const CONTEXT = "connexa-device-auth:";

export interface DeviceKey {
  publicKey: string; // SPKI, base64
  sign(nonce: string): Promise<string>; // IEEE P1363 signature, base64
}

let cached: Promise<DeviceKey | null> | null = null;

/** The device key, created on first call. Null when WebCrypto/IndexedDB are unavailable. */
export function deviceKey(): Promise<DeviceKey | null> {
  cached ??= load().catch((err) => {
    console.warn("device identity unavailable", err);
    return null;
  });
  return cached;
}

async function load(): Promise<DeviceKey | null> {
  if (!globalThis.crypto?.subtle || !globalThis.indexedDB) return null;
  const db = await openDb();
  let pair = await get<CryptoKeyPair>(db, KEY_ID);
  if (!pair) {
    pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);
    await put(db, KEY_ID, pair);
  }
  const spki = await crypto.subtle.exportKey("spki", pair.publicKey);
  const privateKey = pair.privateKey;
  return {
    publicKey: toBase64(new Uint8Array(spki)),
    async sign(nonce: string) {
      const sig = await crypto.subtle.sign(
        { name: "ECDSA", hash: "SHA-256" },
        privateKey,
        new TextEncoder().encode(CONTEXT + nonce),
      );
      return toBase64(new Uint8Array(sig));
    },
  };
}

export function devicePlatform(): string {
  const ua = navigator.userAgent;
  if (/Android/i.test(ua)) return "android";
  if (/Windows/i.test(ua)) return "windows";
  if (/Mac OS X/i.test(ua)) return "macos";
  if (/Linux/i.test(ua)) return "linux";
  return "web";
}

function toBase64(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

function openDb(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function get<T>(db: IDBDatabase, key: string): Promise<T | undefined> {
  return new Promise((resolve, reject) => {
    const req = db.transaction(STORE).objectStore(STORE).get(key);
    req.onsuccess = () => resolve(req.result as T | undefined);
    req.onerror = () => reject(req.error);
  });
}

function put(db: IDBDatabase, key: string, value: unknown): Promise<void> {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).put(value, key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}
