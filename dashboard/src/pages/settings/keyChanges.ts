// Client keys added or deleted on the Settings tab: changes that wait, as a
// setting's edit does, until "Review and save" writes them to `api-keys` in
// config.yaml. Until then the server's list stays as it is.

import type { ApiRequest } from "../../api/client";
import { API_KEYS } from "../../api/management";
import { isExampleKey, maskKey, type ApiKeysAnswer } from "../overview/clientKeys";

/** A client key to add to the list, or to delete from it. */
export interface KeyChange {
  kind: "add" | "delete";
  key: string;
}

/** The keys in `answer`, an answer of `GET /api-keys`. */
export function keysIn(answer: unknown): string[] {
  const list = (answer as Partial<ApiKeysAnswer> | null)?.["api-keys"];
  return Array.isArray(list) ? list.filter((key): key is string => typeof key === "string") : [];
}

/** A key as the screens show it: an example key in full, as anyone may know it, else masked. */
export function shownKey(key: string): string {
  return isExampleKey(key) ? key : maskKey(key);
}

/** A key's name for buttons and notices: "the client key sk-...abcd". */
export function keyName(key: string): string {
  return isExampleKey(key) ? `the example key ${key}` : `the client key ${maskKey(key)}`;
}

/** A change in words, as the review lists it: "Add client key sk-...abcd". */
export function keyChangeLabel(change: KeyChange): string {
  return `${change.kind === "add" ? "Add" : "Delete"} client key ${shownKey(change.key)}`;
}

/**
 * The changes in `changes` that still change `saved`, the server's list:
 * each new key it doesn't have yet, then each deletion of a key it has
 * (once for each time it is listed). New keys come first, so replacing the
 * last key never leaves the list empty on the way, which would let anyone
 * in for a moment.
 */
export function effectiveKeyChanges(
  saved: readonly string[],
  changes: readonly KeyChange[],
): KeyChange[] {
  const adds: KeyChange[] = [];
  const deletes: KeyChange[] = [];
  const listed = new Map<string, number>();
  for (const key of saved) {
    listed.set(key, (listed.get(key) ?? 0) + 1);
  }
  for (const change of changes) {
    if (change.kind === "add") {
      if (!listed.has(change.key) && !adds.some((add) => add.key === change.key)) {
        adds.push(change);
      }
      continue;
    }
    const left = listed.get(change.key) ?? 0;
    if (left > 0) {
      listed.set(change.key, left - 1);
      deletes.push(change);
    }
  }
  return [...adds, ...deletes];
}

/** One key as the card lists it. */
export interface KeyRow {
  key: string;
  /** On the server; added here and not saved yet; or to be deleted on saving. */
  state: "saved" | "added" | "deleted";
}

/** The server's keys, then the new ones, each marked with what saving does to it. */
export function keyRows(saved: readonly string[], changes: readonly KeyChange[]): KeyRow[] {
  const effective = effectiveKeyChanges(saved, changes);
  const deleting = new Map<string, number>();
  for (const change of effective) {
    if (change.kind === "delete") {
      deleting.set(change.key, (deleting.get(change.key) ?? 0) + 1);
    }
  }
  const rows = saved.map((key): KeyRow => {
    const left = deleting.get(key) ?? 0;
    if (left === 0) {
      return { key, state: "saved" };
    }
    deleting.set(key, left - 1);
    return { key, state: "deleted" };
  });
  for (const change of effective) {
    if (change.kind === "add") {
      rows.push({ key: change.key, state: "added" });
    }
  }
  return rows;
}

/** `changes` without the last change of `kind` to `key`: that change undone. */
export function withoutChange(
  changes: readonly KeyChange[],
  kind: KeyChange["kind"],
  key: string,
): KeyChange[] {
  const at = changes.findLastIndex((change) => change.kind === kind && change.key === key);
  return at < 0 ? [...changes] : changes.filter((_change, index) => index !== at);
}

/** `saved` as it is once `changes` are saved. */
export function applyKeyChanges(saved: readonly string[], changes: readonly KeyChange[]): string[] {
  const keys = [...saved];
  for (const change of changes) {
    if (change.kind === "add") {
      keys.push(change.key);
    } else {
      const at = keys.indexOf(change.key);
      if (at >= 0) {
        keys.splice(at, 1);
      }
    }
  }
  return keys;
}

/** A key already in the list when it came to adding it. */
export class DuplicateKeyError extends Error {
  constructor() {
    super("duplicate key");
    this.name = "DuplicateKeyError";
  }
}

/** A key no longer in the list when it came to deleting it. */
export class KeyGoneError extends Error {
  constructor() {
    super("key gone");
    this.name = "KeyGoneError";
  }
}

type Call = <T>(path: string, request?: ApiRequest) => Promise<T>;

/**
 * Writes one key change, checking the list as it is just then, so a key
 * added or deleted elsewhere meanwhile isn't added twice or mistaken.
 */
export async function saveKeyChange(call: Call, change: KeyChange): Promise<void> {
  const keys = keysIn(await call<unknown>(API_KEYS));
  if (change.kind === "add") {
    if (keys.includes(change.key)) {
      throw new DuplicateKeyError();
    }
    // Upstream's patchStringList replaces the first entry equal to `old`
    // and appends `new` when there is none, so this adds the key and
    // leaves the others alone. The key goes in the body, never the URL.
    await call<unknown>(API_KEYS, { method: "PATCH", json: { old: change.key, new: change.key } });
    return;
  }
  // Deleted by its place in the list as it is now, so the key itself never
  // goes in an address (upstream's `?value=` would put it in the URL, and
  // so in access logs). The cost is a race: a change to the list between
  // this read and the DELETE can move the keys, and then the key now at
  // that place goes instead. The list is read again afterwards, so the card
  // shows what happened.
  const index = keys.indexOf(change.key);
  if (index < 0) {
    throw new KeyGoneError();
  }
  await call<unknown>(API_KEYS, { method: "DELETE", query: { index } });
}
