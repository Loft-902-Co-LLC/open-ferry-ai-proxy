// Checking a management key, and what a failed check means.

import { callProblem, type CallProblem } from "./access";
import { apiRequest, isApiError } from "./client";

/** The cheapest management read: a single boolean. */
export const CHECK_PATH = "/v0/management/usage-statistics-enabled";

/** What the server says about itself in every management answer. */
export interface ServerBuild {
  version: string | null;
  commit: string | null;
  buildDate: string | null;
}

export function serverBuild(headers: Headers): ServerBuild {
  return {
    version: headers.get("x-cpa-version"),
    commit: headers.get("x-cpa-commit"),
    buildDate: headers.get("x-cpa-build-date"),
  };
}

/** Checks `key` with a management read; throws an ApiError if refused. */
export async function checkManagementKey(key: string): Promise<ServerBuild> {
  const response = await apiRequest(key, CHECK_PATH);
  return serverBuild(response.headers);
}

/** Why a key check failed, for the sign-in screen to explain. */
export function signInProblem(error: unknown): CallProblem {
  // Every server serves CHECK_PATH, so its empty 404 can only mean the
  // management API is off: no management key is set.
  if (isApiError(error) && error.status === 404) {
    return { kind: "management-off" };
  }
  return callProblem(error);
}
