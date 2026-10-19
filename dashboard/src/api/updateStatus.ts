// What open-ferry's own updates are doing, for screens. Apart from
// update.ts so the e2e mock server, which imports the fixtures, never
// reaches React.

import { useApiQuery } from "./hooks";
import { UPDATE, type UpdateStatus } from "./update";

/** How often the status is read again while a check runs. */
export const CHECKING_POLL_MS = 2_000;

export function switchInProgress(status: UpdateStatus | undefined): boolean {
  const supervisor = status?.supervisor;
  return status?.switch_request?.pending === true ||
    (supervisor != null && !["idle", "degraded", "stopping"].includes(supervisor.phase));
}

/** Read again while a check, preparation or supervised switch runs. */
export function useUpdateStatus() {
  return useApiQuery<UpdateStatus>(UPDATE, undefined, {
    refetchInterval: (query) =>
      query.state.data?.checking === true || switchInProgress(query.state.data)
        ? CHECKING_POLL_MS
        : false,
  });
}
