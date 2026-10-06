import type { UseQueryResult } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import type { ReactNode } from "react";

import { callProblem } from "../api/access";
import { Button } from "./Button";
import { ProblemNotice } from "./ProblemNotice";
import { Spinner } from "./Spinner";

/** "Loading…" with a spinner, announced politely. */
export function Loading({ children = "Loading…" }: { children?: ReactNode }) {
  return (
    <p role="status" className="flex items-center gap-2 text-muted">
      <Spinner /> {children}
    </p>
  );
}

/**
 * Shows a query's loading state or its error, with a retry; renders
 * `children` with the data once it is there.
 */
export function QueryState<T>({
  query,
  loading,
  children,
}: {
  query: UseQueryResult<T>;
  loading?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  if (query.isPending) {
    return <Loading>{loading}</Loading>;
  }
  if (query.isError) {
    return (
      <ProblemNotice
        problem={callProblem(query.error)}
        action={
          <Button
            size="sm"
            onClick={() => {
              void query.refetch();
            }}
          >
            <RotateCw aria-hidden="true" className="size-4" />
            Try again
          </Button>
        }
      />
    );
  }
  return <>{children(query.data)}</>;
}
