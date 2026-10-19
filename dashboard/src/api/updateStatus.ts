// What open-ferry's own updates are doing, for screens. Apart from
// update.ts so the e2e mock server, which imports the fixtures, never
// reaches React.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { shouldRetry } from "../app/QueryProvider";
import { apiQueryKey, useApiCall } from "./hooks";
import { UPDATE, type UpdateStatus } from "./update";

/** How often the status is read again while a check runs. */
export const CHECKING_POLL_MS = 2_000;
export const STATUS_POLL_TIMEOUT_MS = 5_000;

export function switchInProgress(status: UpdateStatus | undefined): boolean {
  const supervisor = status?.supervisor;
  if (status?.switch_request?.state === "preparing") return true;
  if (supervisor?.stale === true && (supervisor.failed_reads >= MAX_FAILED_POLLS ||
    (supervisor.snapshot_age_seconds ?? 0) * 1_000 >= MAX_POLL_ERROR_MS)) return false;
  return (status?.switch_request?.state === "accepted" && status.switch_request.pending) ||
    (supervisor != null && ["waiting-for-signins", "loading", "handing-over", "taking-over", "watching", "cancelled", "switching", "stopping"].includes(supervisor.phase));
}

export const MAX_FAILED_POLLS = 5;
export const MAX_POLL_ERROR_MS = 30_000;

export function pollFailuresExhausted(count: number, since: number | null, now = Date.now()): boolean {
  return count >= MAX_FAILED_POLLS || (since !== null && now - since >= MAX_POLL_ERROR_MS);
}

/** Read again while a check, preparation or supervised switch runs. */
export function useUpdateStatus() {
  const call = useApiCall();
  const client = useQueryClient();
  const [lastRead, setLastRead] = useState<UpdateStatus>();
  const [failures, setFailures] = useState({ count: 0, since: null as number | null });
  const [, refreshDeadline] = useState(0);
  useEffect(() => {
    if (failures.since === null) return;
    const timer = setTimeout(() => { refreshDeadline((revision) => revision + 1); }, Math.max(0, MAX_POLL_ERROR_MS - (Date.now() - failures.since)));
    return () => { clearTimeout(timer); };
  }, [failures.since]);
  const exhausted = pollFailuresExhausted(failures.count, failures.since);
  const query = useQuery<UpdateStatus>({
    queryKey: apiQueryKey(UPDATE),
    queryFn: async ({ signal }) => {
      try {
        const status = await call<UpdateStatus>(UPDATE, { signal: AbortSignal.any([signal, AbortSignal.timeout(STATUS_POLL_TIMEOUT_MS)]) });
        if (!signal.aborted) {
          setLastRead(status);
          setFailures({ count: 0, since: null });
        }
        return status;
      } catch (error) {
        if (!signal.aborted) setFailures((old) => ({ count: old.count + 1, since: old.since ?? Date.now() }));
        throw error;
      }
    },
    // During a switch, the next interval is the retry. Other screens retain
    // the shared policy, including initial reads before any status is known.
    retry: (count, error) => !switchInProgress(client.getQueryData<UpdateStatus>(apiQueryKey(UPDATE))) && shouldRetry(count, error),
    refetchInterval: (query) => !exhausted &&
      (query.state.data?.checking === true || switchInProgress(query.state.data))
        ? CHECKING_POLL_MS : false,
  });
  return { ...query, lastRead, hidePollError: switchInProgress(query.data) && !exhausted };
}
