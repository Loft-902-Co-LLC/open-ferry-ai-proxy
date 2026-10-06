// Client API keys: the `api-keys` list in config.yaml, which proxy calls
// authenticate with. The management API lists them in plain text; the app
// shows them masked unless asked.

import { EXAMPLE_API_KEYS } from "../../app/safeMode";

export { maskKey } from "../../lib/mask";

/** `GET /v0/management/api-keys`. */
export interface ApiKeysAnswer {
  "api-keys": string[] | null;
}

/** Whether `key` is one of CLIProxyAPI's examples, as the server checks it. */
export function isExampleKey(key: string): boolean {
  return EXAMPLE_API_KEYS.includes(key.trim());
}

/** The keys a client could use: not empty, not an example, each once. */
export function usableKeys(keys: readonly string[]): string[] {
  const usable: string[] = [];
  for (const key of keys) {
    if (key.trim() !== "" && !isExampleKey(key) && !usable.includes(key)) {
      usable.push(key);
    }
  }
  return usable;
}


/** A new client key: "sk-" and 32 random bytes, base64url. */
export function generateClientKey(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  const encoded = btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
  return `sk-${encoded}`;
}
