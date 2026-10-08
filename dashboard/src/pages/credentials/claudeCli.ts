// The config's `claude-cli` entries, each a Claude Code account the server
// runs Claude Code with (docs/claude-subscription.md): what state an entry
// is in, in words, and the commands that check and sign in its Claude
// Code, as the docs give them.

import { isApiError } from "../../api/client";
import type { Credential } from "../../api/credentials";
import type { ClaudeCliEntry } from "../../api/dashboard";
import {
  credentialCooldowns,
  credentialHealth,
  reasonOfMessage,
  resetRetries,
  type Health,
} from "./credentialStates";

/**
 * Whether `error` says the server doesn't list the entries: CLIProxyAPI
 * answers an empty 404, and an open-ferry older than the route a 404
 * `not_found`.
 */
export function entriesUnserved(error: unknown): boolean {
  return (
    isApiError(error) && error.status === 404 && (error.code === null || error.code === "not_found")
  );
}

/** The reasons that mean Claude Code's sign-in failed. */
const SIGN_IN_REASONS: readonly string[] = ["unauthorized", "invalid_grant"];

/** Whether what stops the credential is its sign-in. */
function signInFailed(credential: Credential): boolean {
  const resting = credentialCooldowns(credential)[0];
  if (resting !== undefined) {
    return SIGN_IN_REASONS.includes(resting.reason);
  }
  const reason = reasonOfMessage(credential.status_message ?? "");
  return credential.status === "error" && reason !== null && SIGN_IN_REASONS.includes(reason);
}

/** An entry's state, in words, with what to do about it. */
export function entryHealth(entry: ClaudeCliEntry): Health {
  if (entry.disabled) {
    return {
      tone: "neutral",
      label: "Off",
      summary: "Turned off in config.yaml: the server sends it no requests.",
      action: "To use it again, take disabled: true off its entry in config.yaml.",
      triage: "off",
    };
  }
  if (entry.credential === null) {
    return {
      tone: "neutral",
      label: "Not loaded",
      summary: "The server hasn't loaded it yet.",
      action: "It loads with config.yaml. If it stays like this, the server's log says why.",
      triage: "other",
    };
  }
  const health = credentialHealth(entry.credential);
  if (!signInFailed(entry.credential)) {
    return health;
  }
  const then = resetRetries(health) ? "try it again now" : "stop it resting";
  return {
    ...health,
    action: `Check its sign-in. If Claude Code isn't signed in, sign it in again on the server's computer, then ${then}.`,
  };
}

/**
 * `claude <args>` for an entry with `configDir`: with the directory as
 * `CLAUDE_CONFIG_DIR` when it has one, as the docs write it.
 */
export function claudeCommand(configDir: string, args: string): string {
  const dir = configDir.trim();
  return dir === "" ? `claude ${args}` : `CLAUDE_CONFIG_DIR=${shellWord(dir)} claude ${args}`;
}

/**
 * `text` as one word of a POSIX shell: as it is when nothing in it is
 * special to the shell, else quoted, with a leading `~/` left outside the
 * quotes so the shell still expands it, as the server does.
 */
function shellWord(text: string): string {
  if (/^[\w@%+=:,./~-]+$/.test(text)) {
    return text;
  }
  const home = text.startsWith("~/") ? "~/" : "";
  return `${home}'${text.slice(home.length).replaceAll("'", `'"'"'`)}'`;
}
