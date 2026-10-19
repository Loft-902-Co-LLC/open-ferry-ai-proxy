// What open-ferry's own updates are doing, for screens. Apart from
// update.ts so the e2e mock server, which imports the fixtures, never
// reaches React.

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";

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

/** Combine cancellation and the poll deadline on browsers without AbortSignal.any. */
function pollSignal(query: AbortSignal, timeout: AbortSignal) {
  if (typeof AbortSignal.any === "function") {
    return { signal: AbortSignal.any([query, timeout]), dispose: () => undefined };
  }
  const controller = new AbortController();
  const cancel = () => { controller.abort(query.reason); };
  const expire = () => { controller.abort(timeout.reason); };
  query.addEventListener("abort", cancel, { once: true });
  timeout.addEventListener("abort", expire, { once: true });
  if (query.aborted) cancel();
  else if (timeout.aborted) expire();
  return { signal: controller.signal, dispose: () => {
    query.removeEventListener("abort", cancel);
    timeout.removeEventListener("abort", expire);
  } };
}

/** Read again while a check, preparation or supervised switch runs. */
export function useUpdateStatus() {
  const call = useApiCall();
  const client = useQueryClient();
  const [lastRead, setLastRead] = useState<UpdateStatus>();
  const previousRead = useRef<UpdateStatus>(undefined);
  const recovering = useRef(false);
  const [recoveringSupervisor, setRecoveringSupervisor] = useState(false);
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
      const poll = pollSignal(signal, AbortSignal.timeout(STATUS_POLL_TIMEOUT_MS));
      let status: UpdateStatus;
      try {
        status = await call<UpdateStatus>(UPDATE, { signal: poll.signal });
      } finally {
        poll.dispose();
      }
      if (!signal.aborted) {
        const restart = status.switch_request?.state !== "preparing" && status.supervisor?.phase === "degraded" && status.supervisor.switch === null &&
          (recovering.current || switchInProgress(previousRead.current));
        recovering.current = restart;
        previousRead.current = status;
        setRecoveringSupervisor(restart);
        setLastRead(status);
        if (restart) setFailures((old) => ({ count: old.count + 1, since: old.since ?? Date.now() }));
        else setFailures({ count: 0, since: null });
      }
      return status;
    },
    // Count a poll only after the shared retry policy has finished its attempts.
    // During a switch, the next interval is the retry.
    retry: (count, error) => {
      const retry = !recovering.current && !switchInProgress(client.getQueryData<UpdateStatus>(apiQueryKey(UPDATE))) && shouldRetry(count, error);
      if (!retry) setFailures((old) => ({ count: old.count + 1, since: old.since ?? Date.now() }));
      return retry;
    },
    refetchInterval: (query) => !exhausted &&
      (query.state.data?.checking === true || recoveringSupervisor || switchInProgress(query.state.data))
        ? CHECKING_POLL_MS : false,
  });
  return { ...query, lastRead, recoveringSupervisor, polling: !exhausted && (query.data?.checking === true || recoveringSupervisor || switchInProgress(query.data)), hidePollError: (switchInProgress(query.data) || recoveringSupervisor) && !exhausted };
}
