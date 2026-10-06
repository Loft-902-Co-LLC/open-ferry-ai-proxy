// The signed-in state: the management key this tab holds, if any.

import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from "react";

import { forgetStoredKey, readStoredKey, storeKey } from "../api/keyStorage";

/** Why the tab no longer holds a key, for the sign-in screen to say. */
export type SignOutReason = "signed-out" | "key-rejected";

export interface Session {
  /** The management key, or null when signed out. */
  key: string | null;
  /** Why the last sign-out happened, until the next sign-in. */
  signOutReason: SignOutReason | null;
  signIn: (key: string) => void;
  signOut: (reason?: SignOutReason) => void;
}

const SessionContext = createContext<Session | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [key, setKey] = useState<string | null>(readStoredKey);
  const [signOutReason, setSignOutReason] = useState<SignOutReason | null>(null);

  const signIn = useCallback((next: string) => {
    storeKey(next);
    setSignOutReason(null);
    setKey(next);
  }, []);

  const signOut = useCallback((reason: SignOutReason = "signed-out") => {
    forgetStoredKey();
    setKey(null);
    setSignOutReason(reason);
  }, []);

  const value = useMemo(
    () => ({ key, signOutReason, signIn, signOut }),
    [key, signOutReason, signIn, signOut],
  );
  return <SessionContext value={value}>{children}</SessionContext>;
}

export function useSession(): Session {
  const session = useContext(SessionContext);
  if (session === null) {
    throw new Error("useSession outside SessionProvider");
  }
  return session;
}

/** The key of a signed-in screen; those render only while one is held. */
export function useManagementKey(): string {
  const { key } = useSession();
  if (key === null) {
    throw new Error("useManagementKey while signed out");
  }
  return key;
}
