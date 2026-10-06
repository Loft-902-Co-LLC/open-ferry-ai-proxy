// Why the server refused a call, in either API's words.
//
// The management API keeps CLIProxyAPI's shape, `{"error": "<text>"}`, and
// answers an empty 404 while no management key is set. The dashboard API
// answers `{"error": "<code>", "message": "<text>"}` (docs/dashboard-api.md).
// Both check access with the same code, so each refusal has a twin here.

import { isApiError, type ApiError } from "./client";

/** A refusal that is about access, not about the call itself. */
export type AccessProblem =
  | { kind: "unreachable" }
  | { kind: "missing-key" }
  | { kind: "wrong-key" }
  | { kind: "remote-disabled" }
  | { kind: "banned"; retryIn: string | null }
  | { kind: "management-off" };

/** Every way a call can fail, sorted for the screens to explain. */
export type CallProblem =
  | AccessProblem
  | { kind: "ledger-unavailable" }
  | { kind: "settings-read-only" }
  | { kind: "unsupported" }
  | { kind: "not-found" }
  | { kind: "invalid"; message: string | null }
  | { kind: "server-error"; status: number; message: string | null }
  | { kind: "unexpected"; status: number; message: string | null };

/**
 * What the management API answers (503) to a change of config.yaml while
 * the server has nothing to save the file with.
 */
const SETTINGS_READ_ONLY = "config writer unavailable";

const BAN_WAIT = /try again in (\S+?)\.?$/i;

function banWait(...texts: (string | null)[]): string | null {
  for (const text of texts) {
    const match = text === null ? null : BAN_WAIT.exec(text.trim());
    if (match?.[1] !== undefined) {
      return match[1];
    }
  }
  return null;
}

/** The access problem behind `error`, or null when it isn't one. */
export function accessProblem(error: unknown): AccessProblem | null {
  if (!isApiError(error)) {
    return null;
  }
  const code = error.code ?? "";
  switch (error.status) {
    case 0:
      return { kind: "unreachable" };
    case 401:
      return code === "missing management key" || code === "missing_management_key"
        ? { kind: "missing-key" }
        : { kind: "wrong-key" };
    case 403:
      if (code === "remote management disabled" || code === "remote_management_disabled") {
        return { kind: "remote-disabled" };
      }
      if (code === "ip_banned" || code.startsWith("IP banned")) {
        return { kind: "banned", retryIn: banWait(error.detail, code) };
      }
      if (code === "remote management key not set") {
        return { kind: "management-off" };
      }
      return null;
    case 404:
      // The dashboard API names it; the management API answers every path
      // with an empty 404 instead, which only a route both have tells apart
      // from a route this server lacks. See callProblem.
      return code === "management_disabled" ? { kind: "management-off" } : null;
    default:
      return null;
  }
}

function problemOf(error: ApiError): CallProblem {
  const access = accessProblem(error);
  if (access !== null) {
    return access;
  }
  const message = error.detail ?? error.code;
  if (error.status === 503 && error.code === "ledger_unavailable") {
    return { kind: "ledger-unavailable" };
  }
  if (error.status === 503 && error.code === SETTINGS_READ_ONLY) {
    return { kind: "settings-read-only" };
  }
  if (error.status === 404) {
    return error.code === null ? { kind: "unsupported" } : { kind: "not-found" };
  }
  if (error.status === 400 || error.status === 413 || error.status === 422) {
    return { kind: "invalid", message };
  }
  if (error.status >= 500) {
    return { kind: "server-error", status: error.status, message };
  }
  return { kind: "unexpected", status: error.status, message };
}

/**
 * Whether `error` says the server can't change its settings: it doesn't
 * serve the route, or it can't save config.yaml.
 */
export function isSettingsReadOnly(error: unknown): boolean {
  if (!isApiError(error)) {
    return false;
  }
  const kind = problemOf(error).kind;
  return kind === "unsupported" || kind === "settings-read-only";
}

/** Sorts any failed call into a CallProblem. */
export function callProblem(error: unknown): CallProblem {
  if (!isApiError(error)) {
    return { kind: "unexpected", status: 0, message: error instanceof Error ? error.message : null };
  }
  return problemOf(error);
}
