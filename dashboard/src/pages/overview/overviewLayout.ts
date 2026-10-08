// Which Overview to show: setup first, for a proxy nothing uses yet, or
// health first, once a client has connected.

import { useState } from "react";

import { AUTH_FILES, KEY_LISTS, keysOf, type CredentialList } from "../../api/credentials";
import {
  CLAUDE_CLI_ENTRIES,
  CLIENT_SETUP,
  USAGE_LEDGER,
  type ClaudeCliEntries,
  type ClientSetup,
  type LedgerState,
} from "../../api/dashboard";
import { useApiQuery } from "../../api/hooks";
import { API_KEYS } from "../../api/management";
import { usableKeys, type ApiKeysAnswer } from "./clientKeys";

/**
 * "setup": how to connect providers and clients, first and open.
 * "health": today's calls and the accounts first, the client setup closed.
 */
export type OverviewLayout = "setup" | "health";

/** What the server says about itself, as the layout needs it. */
export interface SetupFacts {
  /** It has a credential, a provider API key or a claude-cli entry. */
  connected: boolean;
  /**
   * The calls the usage ledger holds, back to its retention; null when it
   * can't say: the server doesn't serve the ledger, or it couldn't be
   * opened or read.
   */
  recordedCalls: number | null;
  /** The ledger records new calls (usage-statistics-enabled is on). */
  recording: boolean;
  /** It has a client key that isn't one of CLIProxyAPI's examples. */
  clientKey: boolean;
  /** It is in safe mode, refusing every proxy request. */
  safeMode: boolean;
}

/**
 * The page counts as set up, and leads with health, once the server has
 * something to send requests with (a credential, a provider API key or a
 * claude-cli entry) and a client has used it: the usage ledger holds at
 * least one call. `GET /usage/ledger` counts every call it keeps in one
 * cheap read, and the Today section reads it anyway.
 *
 * When the ledger can't tell (the route isn't served, the ledger couldn't
 * be opened, or recording is off and it holds no call), a client key that
 * isn't an example stands in for the call: a client could have connected.
 *
 * Safe mode is never set up: the proxy refuses every request until its
 * example keys go, and the setup card is where that's fixed.
 */
export function overviewLayout(facts: SetupFacts): OverviewLayout {
  if (!facts.connected || facts.safeMode) {
    return "setup";
  }
  if (facts.recordedCalls !== null && facts.recordedCalls > 0) {
    return "health";
  }
  const ledgerCanTell = facts.recordedCalls !== null && facts.recording;
  return !ledgerCanTell && facts.clientKey ? "health" : "setup";
}

/**
 * The layout for this visit, or null while the server's answers are still
 * coming. It is decided once and then holds, so the page doesn't rearrange
 * itself under the user: making the first key, or a client's first call,
 * changes it on the next visit. The queries are the ones the cards use, so
 * they are read once.
 */
export function useOverviewLayout(): OverviewLayout | null {
  const files = useApiQuery<CredentialList>(AUTH_FILES);
  const claude = useApiQuery<unknown>(KEY_LISTS.claude.path);
  const codex = useApiQuery<unknown>(KEY_LISTS.codex.path);
  const gemini = useApiQuery<unknown>(KEY_LISTS.gemini.path);
  const entries = useApiQuery<ClaudeCliEntries>(CLAUDE_CLI_ENTRIES);
  const ledger = useApiQuery<LedgerState>(USAGE_LEDGER);
  const keys = useApiQuery<ApiKeysAnswer>(API_KEYS);
  const setup = useApiQuery<ClientSetup>(CLIENT_SETUP);
  const [decided, setDecided] = useState<OverviewLayout | null>(null);

  if (decided !== null) {
    return decided;
  }
  const queries = [files, claude, codex, gemini, entries, ledger, keys, setup];
  if (queries.some((query) => query.isPending)) {
    return null;
  }
  // A list the server failed to give counts as empty: the cards say why.
  const providerKeys = [
    { query: claude, list: KEY_LISTS.claude.list },
    { query: codex, list: KEY_LISTS.codex.list },
    { query: gemini, list: KEY_LISTS.gemini.list },
  ].some(({ query, list }) => keysOf(query.data, list).length > 0);
  const opened = ledger.data?.available === true;
  const layout = overviewLayout({
    connected:
      (files.data?.files?.length ?? 0) > 0 ||
      providerKeys ||
      (entries.data?.entries.length ?? 0) > 0,
    recordedCalls: opened ? (ledger.data?.rows ?? null) : null,
    recording: ledger.data?.recording === true,
    clientKey: usableKeys(keys.data?.["api-keys"] ?? []).length > 0,
    safeMode: setup.data?.safe_mode === true,
  });
  setDecided(layout);
  return layout;
}
