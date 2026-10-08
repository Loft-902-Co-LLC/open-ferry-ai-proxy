import type { UseQueryResult } from "@tanstack/react-query";
import type { ReactNode } from "react";

import { callProblem } from "../../api/access";
import { Alert } from "../../components/Alert";
import { explainProblem } from "../../components/ProblemNotice";
import { QueryState } from "../../components/QueryState";
import { clockTime } from "./clock";

/** "Checked at 14:32:05": when the list on show was read. Not announced. */
export function CheckedAt({ at }: { at: number }) {
  if (at <= 0) {
    return null;
  }
  return <span className="block">Checked at {clockTime(at)}.</span>;
}

/**
 * Like QueryState, for a list the page reads again and again: when a later
 * read fails, it keeps showing what it has, and says so once.
 */
export function PolledState<T>({
  query,
  loading,
  children,
}: {
  query: UseQueryResult<T>;
  loading?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  const data = query.data;
  if (data === undefined) {
    return (
      <QueryState query={query} loading={loading}>
        {children}
      </QueryState>
    );
  }
  return (
    <>
      {query.isError && (
        <Alert tone="warn" live title="Couldn't check again">
          <p>
            {explainProblem(callProblem(query.error)).title}. This shows what the server said at{" "}
            {clockTime(query.dataUpdatedAt)}, and the page keeps trying.
          </p>
        </Alert>
      )}
      {children(data)}
    </>
  );
}
