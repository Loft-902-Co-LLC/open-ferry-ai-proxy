// Calls with the session's key, for screens.

import { useQuery, type UseQueryOptions } from "@tanstack/react-query";
import { useCallback } from "react";

import { useManagementKey } from "../session/session";
import { apiDownload, apiRequest, type ApiRequest, type DownloadRequest } from "./client";

export type Query = ApiRequest["query"];

/** The query key of `path` with `query`: invalidate by path prefix. */
export function apiQueryKey(path: string, query?: Query): readonly unknown[] {
  return query === undefined ? [path] : [path, query];
}

/** Reads `path` with the session's key. */
export function useApiQuery<T>(
  path: string,
  query?: Query,
  options: Omit<UseQueryOptions<T>, "queryKey" | "queryFn"> = {},
) {
  const key = useManagementKey();
  return useQuery<T>({
    queryKey: apiQueryKey(path, query),
    queryFn: async ({ signal }) => (await apiRequest<T>(key, path, { query, signal })).data,
    ...options,
  });
}

/** A caller with the session's key, for mutations and paged reads. */
export function useApiCall() {
  const key = useManagementKey();
  return useCallback(
    async <T>(path: string, request: ApiRequest = {}): Promise<T> =>
      (await apiRequest<T>(key, path, request)).data,
    [key],
  );
}

/** A downloader with the session's key: see apiDownload. */
export function useApiDownload() {
  const key = useManagementKey();
  return useCallback(
    (path: string, request?: DownloadRequest) => apiDownload(key, path, request),
    [key],
  );
}
