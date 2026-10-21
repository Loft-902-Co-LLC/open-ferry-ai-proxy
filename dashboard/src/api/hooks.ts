// Calls with the session's key, for screens.

import { useQuery, useQueryClient, type QueryClient, type UseQueryOptions } from "@tanstack/react-query";
import { useCallback, useId, useMemo } from "react";

import { useManagementKey } from "../session/session";
import { apiDownload, apiRequest, type ApiRequest, type DownloadRequest } from "./client";
import { ConfigEditor, ConfigStore } from "./configEditor";

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

const configStores = new WeakMap<QueryClient, { key: string; store: ConfigStore }>();
function configStore(client: QueryClient, key: string): ConfigStore {
  const existing = configStores.get(client);
  if (existing?.key === key) return existing.store;
  const store = new ConfigStore(
    <T,>(path: string, request?: ApiRequest) => apiRequest<T>(key, path, request),
    () => { void client.invalidateQueries(); },
  );
  configStores.set(client, { key, store });
  return store;
}

/** Shared freshness, with a baseline retained only for this form's draft. */
export function useConfigEditor() {
  const key = useManagementKey();
  const client = useQueryClient();
  const scope = useId();
  const editor = useMemo(() => new ConfigEditor(configStore(client, key)), [client, key]);
  const reload = useCallback(async () => {
    editor.reset();
    await client.refetchQueries({ predicate: (query) => query.queryKey.includes(scope) });
  }, [client, editor, scope]);
  return useMemo(() => ({ editor, call: editor.call, ready: editor.ready,
    toggleCredential: editor.toggleCredential, setDirty: editor.setDirty,
    checkChanges: editor.checkChanges, reload, scope }), [editor, reload, scope]);
}

export function useConfigQuery<T>(
  editor: ReturnType<typeof useConfigEditor>,
  path: string,
  options: Omit<UseQueryOptions<T>, "queryKey" | "queryFn"> = {},
) {
  return useQuery<T>({
    queryKey: [path, "config-editor", editor.scope],
    queryFn: ({ signal }) => editor.call<T>(path, { signal }),
    ...options,
  });
}
