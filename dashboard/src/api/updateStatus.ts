// What open-ferry's own updates are doing, for screens. Apart from
// update.ts so the e2e mock server, which imports the fixtures, never
// reaches React.

import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { apiQueryKey, useApiCall } from "./hooks";
import { UPDATE, type UpdateStatus } from "./update";

/** How often the status is read again while a check runs. */
export const CHECKING_POLL_MS = 2_000;

export function switchInProgress(status: UpdateStatus | undefined): boolean {
  const supervisor = status?.supervisor;
  return status?.switch_request?.pending === true ||
    (supervisor != null && !["idle", "degraded", "stopping"].includes(supervisor.phase));
}

export const MAX_FAILED_POLLS = 5;
export const MAX_POLL_ERROR_MS = 30_000;

export function pollFailuresExhausted(count: number, since: number | null, now = Date.now()): boolean {
  return count >= MAX_FAILED_POLLS || (since !== null && now - since >= MAX_POLL_ERROR_MS);
}

/** Read again while a check, preparation or supervised switch runs. */
export function useUpdateStatus() {
  const call = useApiCall();
  const [failures, setFailures] = useState({ count: 0, since: null as number | null });
  const [deadlineReached, setDeadlineReached] = useState(false);
  useEffect(() => {
    if (failures.since === null) return;
    const timer = setTimeout(() => { setDeadlineReached(true); }, Math.max(0, MAX_POLL_ERROR_MS - (Date.now() - failures.since)));
    return () => { clearTimeout(timer); };
  }, [failures.since]);
  const exhausted = deadlineReached || pollFailuresExhausted(failures.count, failures.since);
  const query = useQuery<UpdateStatus>({
    queryKey: apiQueryKey(UPDATE),
    queryFn: async ({ signal }) => {
      try {
        const status = await call<UpdateStatus>(UPDATE, { signal });
        setFailures({ count: 0, since: null });
        setDeadlineReached(false);
        return status;
      } catch (error) {
        if (!signal.aborted) setFailures((old) => ({ count: old.count + 1, since: old.since ?? Date.now() }));
        throw error;
      }
    },
    // Each interval is one failed poll; retries come from the polling interval.
    retry: false,
    refetchInterval: (query) => !exhausted &&
      (query.state.data?.checking === true || switchInProgress(query.state.data))
        ? CHECKING_POLL_MS : false,
  });
  return { ...query, hidePollError: switchInProgress(query.data) && !exhausted };
}
