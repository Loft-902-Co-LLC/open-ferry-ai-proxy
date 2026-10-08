// What open-ferry's own updates are doing, for screens. Apart from
// update.ts so the e2e mock server, which imports the fixtures, never
// reaches React.

import { useApiQuery } from "./hooks";
import { UPDATE, type UpdateStatus } from "./update";

/** How often the status is read again while a check runs. */
export const CHECKING_POLL_MS = 2_000;

/** What updates are doing, read again every few seconds while a check runs. */
export function useUpdateStatus() {
  return useApiQuery<UpdateStatus>(UPDATE, undefined, {
    refetchInterval: (query) => (query.state.data?.checking === true ? CHECKING_POLL_MS : false),
  });
}
