import { describe, expect, it } from "vitest";

import { ApiError } from "../../api/client";
import { claudeCliCredential, claudeCliEntry, cooldown } from "../../test/fixtures";
import { claudeCommand, entriesUnserved, entryHealth } from "./claudeCli";
import { credentialHealth } from "./credentialStates";

const CHECK_SIGN_IN =
  "Check its sign-in. If Claude Code isn't signed in, sign it in again on the server's computer, then stop it resting.";
const CHECK_SIGN_IN_FAILING =
  "Check its sign-in. If Claude Code isn't signed in, sign it in again on the server's computer, then try it again now.";

describe("a claude-cli entry's health", () => {
  it("is off when config.yaml turns it off, and not loaded without a credential", () => {
    expect(entryHealth(claudeCliEntry({ disabled: true, credential: null }))).toEqual({
      tone: "neutral",
      label: "Off",
      summary: "Turned off in config.yaml: the server sends it no requests.",
      action: "To use it again, take disabled: true off its entry in config.yaml.",
      triage: "off",
    });
    expect(entryHealth(claudeCliEntry({ credential: null }))).toMatchObject({
      label: "Not loaded",
      summary: "The server hasn't loaded it yet.",
      triage: "other",
    });
  });

  it("is its credential's, with what to do about a failed sign-in", () => {
    const ready = claudeCliEntry();
    expect(entryHealth(ready)).toEqual(credentialHealth(claudeCliCredential()));
    expect(entryHealth(ready).label).toBe("Ready");

    const refused = entryHealth(
      claudeCliEntry({
        credential: claudeCliCredential({
          status: "error",
          status_message: "unauthorized",
          unavailable: true,
          cooldowns: [cooldown("unauthorized", 600, { http_status: 401 })],
        }),
      }),
    );
    expect(refused).toEqual({
      tone: "warn",
      label: "Resting",
      summary: "The provider refused the credential",
      action: CHECK_SIGN_IN,
      triage: "resting",
    });
    const failing = entryHealth(
      claudeCliEntry({
        credential: claudeCliCredential({ status: "error", status_message: "unauthorized" }),
      }),
    );
    expect(failing).toMatchObject({ label: "Failing", action: CHECK_SIGN_IN_FAILING });

    const resting = entryHealth(
      claudeCliEntry({
        credential: claudeCliCredential({ cooldowns: [cooldown("quota", 300)] }),
      }),
    );
    expect(resting.label).toBe("Resting");
    expect(resting.action).not.toBe(CHECK_SIGN_IN);
  });
});

describe("the claude commands", () => {
  it("name the entry's config directory as the docs do", () => {
    expect(claudeCommand("", "auth login")).toBe("claude auth login");
    expect(claudeCommand("  ", "auth login")).toBe("claude auth login");
    expect(claudeCommand("~/.claude-second", "auth login")).toBe(
      "CLAUDE_CONFIG_DIR=~/.claude-second claude auth login",
    );
    expect(claudeCommand(" /srv/claude/max-2 ", "auth status")).toBe(
      "CLAUDE_CONFIG_DIR=/srv/claude/max-2 claude auth status",
    );
  });

  it("quote a directory the shell would split or expand, keeping ~/ outside", () => {
    expect(claudeCommand("~/Claude accounts/max 3", "auth login")).toBe(
      "CLAUDE_CONFIG_DIR=~/'Claude accounts/max 3' claude auth login",
    );
    expect(claudeCommand("/srv/it's $HOME", "auth login")).toBe(
      `CLAUDE_CONFIG_DIR='/srv/it'"'"'s $HOME' claude auth login`,
    );
  });
});

describe("the entries route", () => {
  it("is unserved by CLIProxyAPI and by an open-ferry older than it", () => {
    expect(entriesUnserved(new ApiError(404, null, null, null))).toBe(true);
    expect(entriesUnserved(new ApiError(404, "not_found", "no such route", null))).toBe(true);
    expect(entriesUnserved(new ApiError(404, "management_disabled", null, null))).toBe(false);
    expect(entriesUnserved(new ApiError(500, null, null, null))).toBe(false);
    expect(entriesUnserved(new Error("boom"))).toBe(false);
  });
});
