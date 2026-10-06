// The management key is kept in sessionStorage, so it lasts as long as the
// browser tab and is never shared with another tab, written to disk by the
// app, sent in a URL or set as a cookie. Signing out removes it.

const STORAGE_KEY = "open-ferry.management-key";

/** The key this tab signed in with, if any. */
export function readStoredKey(): string | null {
  try {
    const key = window.sessionStorage.getItem(STORAGE_KEY);
    return key === null || key === "" ? null : key;
  } catch {
    // Storage can be off, as in some private modes: the key then lives in
    // memory only, and a reload asks for it again.
    return null;
  }
}

/** Keeps `key` for this tab. */
export function storeKey(key: string): void {
  try {
    window.sessionStorage.setItem(STORAGE_KEY, key);
  } catch {
    // See readStoredKey.
  }
}

/** Forgets the key. */
export function forgetStoredKey(): void {
  try {
    window.sessionStorage.removeItem(STORAGE_KEY);
  } catch {
    // See readStoredKey.
  }
}
