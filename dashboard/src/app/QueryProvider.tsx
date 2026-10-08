// Server state, through TanStack Query. A 401 from any call means the key
// this tab holds no longer works: the tab signs out and says why.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";

import { isApiError } from "../api/client";
import { useSession } from "../session/session";

/**
 * Retries only failures a retry may fix: no answer, or a server error. A
 * ledger that couldn't be opened stays so until the server restarts, and a
 * server that runs no update checks never will.
 */
function shouldRetry(failureCount: number, error: unknown): boolean {
  if (isApiError(error) && error.status >= 400 && error.status < 500) {
    return false;
  }
  if (
    isApiError(error) &&
    (error.code === "ledger_unavailable" || error.code === "updates_unavailable")
  ) {
    return false;
  }
  return failureCount < 2;
}

export function createQueryClient(retryDelay?: number): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        retry: shouldRetry,
        staleTime: 5_000,
        ...(retryDelay === undefined ? {} : { retryDelay }),
      },
      mutations: { retry: false },
    },
  });
}

/** Calls `onUnauthorized` whenever a query or mutation fails with 401. */
function watchForUnauthorized(client: QueryClient, onUnauthorized: () => void): () => void {
  const check = (error: unknown) => {
    if (isApiError(error) && error.status === 401) {
      onUnauthorized();
    }
  };
  const stopQueries = client.getQueryCache().subscribe((event) => {
    if (event.type === "updated" && event.action.type === "error") {
      check(event.action.error);
    }
  });
  const stopMutations = client.getMutationCache().subscribe((event) => {
    if (event.type === "updated" && event.action.type === "error") {
      check(event.action.error);
    }
  });
  return () => {
    stopQueries();
    stopMutations();
  };
}

export interface QueryProviderProps {
  children: ReactNode;
  /** Milliseconds between retries; tests set 0. */
  retryDelay?: number;
}

export function QueryProvider({ children, retryDelay }: QueryProviderProps) {
  const { key, signOut } = useSession();
  const [client] = useState(() => createQueryClient(retryDelay));

  useEffect(() => {
    // Signed out, a 401 is the sign-in check refusing a candidate key.
    if (key === null) {
      // Nothing read with one key outlives it.
      client.clear();
      return undefined;
    }
    return watchForUnauthorized(client, () => {
      signOut("key-rejected");
    });
  }, [client, key, signOut]);

  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}
