// What the provider's last response said of a Claude or Codex account's
// quota windows (`quota` in `GET auth-files`): how much of each is used,
// when it starts over, and which one stops the account. The readings are
// headers as the provider sent them, so each is checked, and only the
// numbers and times read from them are shown, never the text itself.

import type { Credential } from "../../api/credentials";

/** One of an account's quota windows. */
export interface QuotaWindow {
  /** "5-hour", "Weekly", … */
  name: string;
  /** How much of it is used, from 0 to 1, when the provider said. */
  used: number | null;
  /** When it starts over, when the provider said. */
  resetsAt: Date | null;
  /** Whether the provider says it is used up. */
  usedUp: boolean;
}

/** A credential's quota windows, as one response gave them. */
export interface QuotaReadings {
  /** When the response came. */
  observedAt: Date;
  windows: QuotaWindow[];
  /**
   * The used-up window that stops the account: the one Claude names, or
   * else the one that starts over last. Null when none is used up.
   */
  limiting: QuotaWindow | null;
}

/** Claude's windows: their header names, and how Claude names them as the one that limits. */
const CLAUDE_WINDOWS = [
  { id: "5h", name: "5-hour", claim: "five_hour" },
  { id: "7d", name: "Weekly", claim: "seven_day" },
] as const;

/**
 * The quota windows the credential's last Claude or Codex response gave,
 * or null when there is no such response or it gave none.
 */
export function quotaReadings(credential: Credential): QuotaReadings | null {
  const observedAt = new Date(credential.quota?.observed_at ?? "");
  if (Number.isNaN(observedAt.getTime())) {
    return null;
  }
  const signals = new Map<string, string>();
  for (const [name, value] of Object.entries(credential.quota?.signals ?? {})) {
    if (typeof value === "string") {
      signals.set(name.toLowerCase(), value.trim());
    }
  }
  const windows: QuotaWindow[] = [];
  let named: QuotaWindow | null = null;
  switch (credential.provider.trim().toLowerCase()) {
    case "claude": {
      const claim = signals.get("anthropic-ratelimit-unified-representative-claim")?.toLowerCase();
      for (const { id, name, claim: windowClaim } of CLAUDE_WINDOWS) {
        const prefix = `anthropic-ratelimit-unified-${id}-`;
        const status = signals.get(`${prefix}status`)?.toLowerCase();
        const used = number(signals.get(`${prefix}utilization`));
        const resetsAt = time(signals.get(`${prefix}reset`));
        if (status === undefined && used === null && resetsAt === null) {
          continue;
        }
        const window = { name, used, resetsAt, usedUp: status === "rejected" || (used ?? 0) >= 1 };
        windows.push(window);
        if (claim === windowClaim) {
          named = window;
        }
      }
      break;
    }
    case "codex":
      for (const which of ["Primary", "Secondary"]) {
        const prefix = `x-codex-${which.toLowerCase()}-`;
        const percent = number(signals.get(`${prefix}used-percent`));
        const minutes = number(signals.get(`${prefix}window-minutes`));
        const after = number(signals.get(`${prefix}reset-after-seconds`));
        const resetsAt =
          time(signals.get(`${prefix}reset-at`)) ??
          (after === null ? null : validDate(observedAt.getTime() + after * 1000));
        if (percent === null && minutes === null && resetsAt === null) {
          continue;
        }
        const used = percent === null ? null : percent / 100;
        windows.push({
          name: minutes !== null && Number.isInteger(minutes) && minutes > 0 ? windowName(minutes) : which,
          used,
          resetsAt,
          usedUp: (used ?? 0) >= 1,
        });
      }
      break;
    default:
      return null;
  }
  if (windows.length === 0) {
    return null;
  }
  const usedUp = windows.filter((window) => window.usedUp);
  const limiting =
    named?.usedUp === true
      ? named
      : (usedUp.sort((a, b) => (b.resetsAt?.getTime() ?? 0) - (a.resetsAt?.getTime() ?? 0))[0] ?? null);
  return { observedAt, windows, limiting };
}

/** "The weekly limit is used up", for the window that stops an account. */
export function limitUsedUp(window: QuotaWindow): string {
  return `The ${window.name.toLowerCase()} limit is used up`;
}

/** A window's length in words: "5-hour", "Weekly", "30-minute". */
function windowName(minutes: number): string {
  if (minutes === 7 * 24 * 60) {
    return "Weekly";
  }
  if (minutes === 24 * 60) {
    return "Daily";
  }
  if (minutes % (24 * 60) === 0) {
    return `${String(minutes / (24 * 60))}-day`;
  }
  if (minutes % 60 === 0) {
    return `${String(minutes / 60)}-hour`;
  }
  return `${String(minutes)}-minute`;
}

/** A reading's number, or null for anything but digits with a point or none. */
function number(text: string | undefined): number | null {
  if (text === undefined || !/^\d+(\.\d+)?$/.test(text)) {
    return null;
  }
  const value = Number(text);
  return Number.isFinite(value) ? value : null;
}

/** A reset time: Unix seconds, or an ISO 8601 date and time. */
function time(text: string | undefined): Date | null {
  if (text === undefined) {
    return null;
  }
  const seconds = number(text);
  if (seconds !== null) {
    return validDate(seconds * 1000);
  }
  return /^\d{4}-\d{2}-\d{2}T/.test(text) ? validDate(Date.parse(text)) : null;
}

function validDate(ms: number): Date | null {
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? null : date;
}
