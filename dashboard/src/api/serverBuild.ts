import { useQuery } from "@tanstack/react-query";

import { useManagementKey } from "../session/session";
import { checkManagementKey, type ServerBuild } from "./signIn";

/** The server's version, commit and build date, from its headers. */
export function useServerBuild() {
  const key = useManagementKey();
  return useQuery<ServerBuild>({
    queryKey: ["server-build"],
    queryFn: () => checkManagementKey(key),
    staleTime: Infinity,
  });
}
