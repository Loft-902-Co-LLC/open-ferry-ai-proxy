// open-ferry's own updates, as docs/dashboard-api.md (Updates) has them:
// what updates are doing, and a check now. The mode is a config setting,
// `self-update.mode`, which the Settings tab changes. The status hook is in
// updateStatus.ts.

import { isApiError } from "./client";
import { DASHBOARD_API } from "./dashboard";

/** `GET`: what updates are doing. */
export const UPDATE = `${DASHBOARD_API}/update`;
/** `POST`: starts a check now, in the background. */
export const UPDATE_CHECK = `${UPDATE}/check`;

/** `self-update.mode`: from the most an update does to the least. */
export const UPDATE_MODES = ["auto", "notify", "off"] as const;
export type UpdateMode = (typeof UPDATE_MODES)[number];

export const UPDATE_MODE_LABELS: Record<UpdateMode, string> = {
  auto: "On",
  notify: "Notify only",
  off: "Off",
};

/**
 * The mode the server uses for `written`, as `SelfUpdateMode::parse` reads
 * it: `notify` or `off` in any case, with spaces around, else `auto`. The
 * server refuses a config with any other word, so none reaches here.
 */
export function updateModeOf(written: unknown): UpdateMode {
  const name = typeof written === "string" ? written.trim().toLowerCase() : "";
  return name === "notify" || name === "off" ? name : "auto";
}

/** What set the mode: `OPEN_FERRY_SELF_UPDATE` can only lower the config's. */
export type UpdateModeSource = "default" | "config" | "environment";

export type UpdateResult =
  | "up-to-date"
  | "update-available"
  | "cannot-update"
  | "staged"
  | "skipped"
  | "error";

/** An answer of `GET /update`. */
export interface UpdateStatus {
  mode: UpdateMode;
  mode_source: UpdateModeSource;
  /** The mode as people say it. */
  updates: "on" | "notify-only" | "off";
  check_every_seconds: number;
  running_version: string;
  /** The version a restart runs. */
  installed_version: string;
  restart_needed: boolean;
  target: string;
  latest_version: string | null;
  update_available: boolean;
  /** Downloaded and checked, ready for `open-ferry update`. */
  staged_version: string | null;
  /** Kept for `open-ferry update -rollback`. */
  previous_version: string | null;
  failed_versions: string[];
  rolled_back_version: string | null;
  last_check: string | null;
  last_result: UpdateResult | null;
  last_error: string | null;
  /** Null while updates are off. */
  next_check: string | null;
  checking: boolean;
  can_update_itself: boolean;
  why_not: string | null;
  why_not_code: string | null;
  /** False for a build with no release key, which never checks. */
  trusts_release_key: boolean;
  notes: string[];
}

/** An answer of `POST /update/check`: `running` when one was running already. */
export interface CheckStarted {
  check: "started" | "running";
}

/** The server runs no update checks, as the one `open-ferry -tui` starts doesn't. */
export function isUpdatesUnavailable(error: unknown): boolean {
  return isApiError(error) && error.status === 503 && error.code === "updates_unavailable";
}

/** A check now was refused because updates are off. */
export function isUpdatesOff(error: unknown): boolean {
  return isApiError(error) && error.status === 409 && error.code === "updates_off";
}
