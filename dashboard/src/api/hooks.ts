// Calls with the session's key, for screens.

import { useQuery, type UseQueryOptions } from "@tanstack/react-query";
import { useCallback, useId, useMemo } from "react";

import { useManagementKey } from "../session/session";
import { apiDownload, apiRequest, type ApiRequest, type DownloadRequest } from "./client";
import { ConfigEditor } from "./configEditor";

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

/** Like `useApiCall`, but gives the whole answer, its status too. */
export function useApiResponse() {
  const key = useManagementKey();
  return useCallback(
    async <T>(path: string, request: ApiRequest = {}) => apiRequest<T>(key, path, request),
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

/** One form's config snapshot. Its queries have their own cache entries so
 * cached values from another form cannot be paired with a newer version. */
export function useConfigEditor() {
  const response = useApiResponse();
  const scope = useId();
  const editor = useMemo(() => new ConfigEditor(response), [response]);
  return useMemo(() => ({ call: editor.call, ready: editor.ready, toggleCredential: editor.toggleCredential, scope }), [editor, scope]);
}

export function useConfigQuery<T>(
  editor: ReturnType<typeof useConfigEditor>,
  path: string,
) {
  return useQuery<T>({
    queryKey: [path, "config-editor", editor.scope],
    queryFn: ({ signal }) => editor.call<T>(path, { signal }),
  });
}
