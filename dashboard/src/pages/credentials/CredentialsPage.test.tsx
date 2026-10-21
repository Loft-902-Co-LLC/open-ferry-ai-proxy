import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  AUTH_FILES,
  AUTH_FILE_STATUS,
  BIND_CALLBACK,
  BIND_SESSION,
  BIND_START,
  BIND_STATUS,
  KEY_LISTS,
  KEY_PROVIDERS,
  QUOTA_FETCH,
  RESET_COOLDOWN,
  SIGN_IN_CALLBACK,
  SIGN_IN_SESSION,
  SIGN_IN_START,
  SIGN_IN_STATUS,
  type Credential,
  type KeyProvider,
  type ProviderKey,
  type SignInStatus,
} from "../../api/credentials";
import { CLAUDE_CLI_AUTH_STATUS, CLAUDE_CLI_ENTRIES, type ClaudeCliEntry } from "../../api/dashboard";
import { V8_CONFIG } from "../../api/management";
import {
  claudeCliCredential,
  claudeCliEntry,
  cooldown,
  credential,
  credentialList,
  quotaCheck,
} from "../../test/fixtures";
import { loadFirst } from "../../test/loadFirst";
import { mockApi, route, type MockApi } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";
import { claudeCliAnchor, credentialAnchor, providerKeyAnchor } from "./anchors";
import { CREDENTIALS_REFRESH_MS } from "./CredentialList";
import { SIGN_IN_GRACE_MS, SIGN_IN_LIMIT_MS, SIGN_IN_POLL_MS } from "./SignInDialog";

const NAME = "claude-ada@example.com.json";
const STATE = "test-state-0001";
const SIGN_IN_URL = `https://sign-in.example/oauth/authorize?state=${STATE}`;
const CALLBACK = `http://localhost:54545/callback?code=test-code&state=${STATE}`;
const CLAUDE_KEY = "sk-ant-test-provider-key-0001";
const CREDENTIALS = "Sign-ins and credential files";
const CLAUDE_CLI = "Claude Code accounts";

interface Server {
  api: MockApi;
  files: Credential[];
  keys: Record<KeyProvider, ProviderKey[]>;
  entries: ClaudeCliEntry[];
}

/**
 * A server holding `files`, provider `keys` and `claude-cli` `entries`,
 * which change as the management routes change them. `keys: null` serves
 * no key lists, and `entries: null` no entries, as a server without those
 * routes.
 */
function server(
  files: Credential[] = [credential()],
  keys: Partial<Record<KeyProvider, ProviderKey[]>> | null = {},
  entries: ClaudeCliEntry[] | null = [],
): Server {
  const state: Server = {
    api: mockApi(),
    files: [...files],
    keys: { claude: [], codex: [], gemini: [], ...keys },
    entries: [...(entries ?? [])],
  };
  state.api.use(
    route("GET", AUTH_FILES, () => ({ json: credentialList(state.files) })),
    route("PATCH", AUTH_FILE_STATUS, (request) => {
      const body = request.json() as { name: string; disabled: boolean };
      const file = state.files.find((candidate) => candidate.name === body.name);
      if (file === undefined) {
        return { status: 404, json: { error: "auth file not found" } };
      }
      file.disabled = body.disabled;
      file.status = body.disabled ? "disabled" : "active";
      return { json: { status: "ok", disabled: body.disabled } };
    }),
    route("DELETE", AUTH_FILES, (request) => {
      const name = request.url.searchParams.get("name");
      state.files = state.files.filter((file) => file.name !== name);
      return { json: { status: "ok" } };
    }),
    route("POST", RESET_COOLDOWN, (request) => {
      const body = request.json() as { auth_index: string };
      const file = [
        ...state.files,
        ...state.entries.flatMap((entry) => (entry.credential === null ? [] : [entry.credential])),
      ].find((candidate) => candidate.auth_index === body.auth_index);
      const models = (file?.cooldowns ?? []).flatMap((item) =>
        item.model_key === undefined ? [] : [item.model_key],
      );
      if (file !== undefined) {
        file.cooldowns = [];
        file.unavailable = false;
        delete file.next_retry_after;
        // As the server does, a failed one is back in use.
        if (file.status === "error") {
          file.status = "active";
          file.status_message = "";
        }
      }
      return { json: { status: "ok", auth_index: body.auth_index, models } };
    }),
  );
  if (entries !== null) {
    state.api.use(route("GET", CLAUDE_CLI_ENTRIES, () => ({ json: { entries: state.entries } })));
  }
  if (keys !== null) {
    for (const provider of KEY_PROVIDERS) {
      const { list, path } = KEY_LISTS[provider];
      state.api.use(
        route("GET", path, () => ({ json: { [list]: state.keys[provider] } })),
        route("PUT", path, (request) => {
          state.keys[provider] = request.json() as ProviderKey[];
          return { json: { status: "ok" } };
        }),
        route("DELETE", path, (request) => {
          const index = Number(request.url.searchParams.get("index"));
          state.keys[provider].splice(index, 1);
          return { json: { status: "ok" } };
        }),
      );
    }
  }
  return state;
}

/** A Claude sign-in that answers each status call with the next of `statuses`. */
function claudeSignIn(state: Server, statuses: SignInStatus[]) {
  let polls = 0;
  state.api.use(
    route("GET", SIGN_IN_START.claude, {
      json: { status: "ok", url: SIGN_IN_URL, state: STATE },
    }),
    route("GET", SIGN_IN_STATUS, () => {
      const status = statuses[Math.min(polls, statuses.length - 1)] ?? { status: "wait" };
      polls += 1;
      if (status.status === "ok") {
        state.files.push(
          credential({
            name: "claude-new@example.com.json",
            id: "new",
            account: "new@example.com",
            label: "new@example.com",
          }),
        );
      }
      return { json: status };
    }),
  );
}

/** The text an element shows, without what only screen readers hear. */
function shownText(element: HTMLElement): string {
  const copy = element.cloneNode(true) as HTMLElement;
  for (const hidden of copy.querySelectorAll(".sr-only")) {
    hidden.remove();
  }
  return copy.textContent;
}

/**
 * Opens the folded group of `region`, named by its tally, then the details
 * of the entry named `name` in it; returns the entry.
 */
async function openFolded(
  user: UserEvent,
  region: HTMLElement,
  tally: string,
  name: string,
): Promise<HTMLElement> {
  await user.click(await within(region).findByRole("button", { name: tally }));
  const entry = within(region).getByRole("article", { name });
  await user.click(within(entry).getByRole("button", { name: `Details ${name}` }));
  return entry;
}

loadFirst(() => import("./CredentialsPage"));

describe("the credential list", () => {
  it("shows each credential's health, why, and what to do about it", async () => {
    const { api } = server([
      credential(),
      credential({
        name: "codex-bob.json",
        id: "codex-bob.json",
        provider: "codex",
        account: "bob@example.com",
        label: "bob@example.com",
        id_token: { plan_type: "plus" },
        // Named by a server clock in another time zone: the place counts.
        recent_requests: [
          { time: "07:20-07:30", success: 0, failed: 2 },
          { time: "07:30-07:40", success: 0, failed: 0 },
          { time: "07:40-07:50", success: 0, failed: 0 },
          { time: "07:50-08:00", success: 0, failed: 0 },
        ],
        cooldowns: [
          cooldown("credential_quota", 300),
          cooldown("quota", 45, { scope: "model", model_key: "gpt-5.1-codex" }),
        ],
      }),
      credential({
        name: "openai-compat-1",
        id: "openai-compat-1",
        provider: "openai-compatibility",
        account_type: "api_key",
        account: "sk-test-provider-key-0001",
        source: "memory",
        runtime_only: true,
      }),
    ]);
    const { user } = renderApp("/credentials");
    expect(await screen.findByRole("heading", { name: "Credentials", level: 1 })).toBeVisible();
    expect(
      within(screen.getByRole("navigation", { name: "Main" })).getByRole("link", {
        name: "Credentials",
      }),
    ).toHaveAttribute("aria-current", "page");

    // The resting one comes first, in full.
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const bob = await within(card).findByRole("article", { name: "bob@example.com" });
    expect(within(bob).getByText("Resting")).toBeVisible();
    expect(bob).toHaveTextContent("Codex sign-in · Plus plan · Last used about 30 min ago");
    expect(bob).toHaveTextContent("The account's quota is used up");
    expect(bob).toHaveTextContent("Back in about 5 min");
    expect(bob).toHaveTextContent("What to do: Wait until then");
    expect(within(bob).getByRole("heading", { name: "Models resting" })).toBeVisible();
    expect(bob).toHaveTextContent("gpt-5.1-codex: Back in less than a minute");
    expect(bob).toHaveTextContent("Rate-limited");
    expect(within(bob).getByRole("button", { name: "Stop resting bob@example.com" })).toBeVisible();
    expect(card).toHaveTextContent(/Checked at \d\d:\d\d:\d\d\./);
    expect(
      within(card)
        .getByText(/^Checked at/)
        .closest("[role=status], [role=alert], [aria-live]"),
    ).toBeNull();

    // The ready ones fold into one row, named by how many.
    expect(within(card).queryByRole("article", { name: "ada@example.com" })).toBeNull();
    const group = within(card).getByRole("button", { name: "2 ready" });
    expect(group).toHaveAttribute("aria-expanded", "false");
    await user.click(group);
    expect(group).toHaveAttribute("aria-expanded", "true");
    expect(
      within(card)
        .getAllByRole("heading", { level: 3 })
        .map((heading) => heading.textContent),
    ).toEqual(["bob@example.com", "ada@example.com", "openai-compat-1"]);

    const ada = within(card).getByRole("article", { name: "ada@example.com" });
    expect(within(ada).getByText("Ready")).toBeVisible();
    expect(ada).toHaveTextContent("Claude sign-in · Last used in the last 10 min");
    expect(within(ada).queryByRole("button", { name: "Delete ada@example.com" })).toBeNull();
    const details = within(ada).getByRole("button", { name: "Details ada@example.com" });
    expect(details).toHaveAttribute("aria-expanded", "false");
    await user.click(details);
    expect(details).toHaveAttribute("aria-expanded", "true");
    expect(ada).toHaveTextContent("1,520 succeeded, 12 failed");
    expect(ada).toHaveTextContent("In the last 20 min42 succeeded, 1 failed");
    expect(within(ada).getByRole("button", { name: "Turn off ada@example.com" })).toBeVisible();
    expect(within(ada).getByRole("button", { name: "Delete ada@example.com" })).toBeVisible();
    expect(within(ada).queryByRole("button", { name: /^Stop resting/ })).toBeNull();
    expect(within(ada).queryByRole("button", { name: /^Check quota/ })).toBeNull();
    // Its heading names the account, and nothing shown repeats it.
    expect(shownText(ada).split("ada@example.com")).toHaveLength(2);

    const key = within(card).getByRole("article", { name: "openai-compat-1" });
    expect(key).toHaveTextContent("openai-compatibility API key, kept in memory only");
    await user.click(within(key).getByRole("button", { name: "Details openai-compat-1" }));
    expect(key).toHaveTextContent("sk-...0001");
    expect(key).not.toHaveTextContent("sk-test-provider-key-0001");
    expect(within(key).queryByRole("button", { name: /^Delete/ })).toBeNull();
    expect(api.unhandled).toEqual([]);
  });

  it("says when the server next checks a capped quota rest, and which model it covers", async () => {
    server([
      credential({
        name: "codex-bob.json",
        id: "codex-bob.json",
        provider: "codex",
        account: "bob@example.com",
        label: "bob@example.com",
        // While it waits for its check, the rest ends at the check.
        cooldowns: [
          cooldown("credential_quota", 3600, { retry_at: "2026-10-05T13:00:00.123456789Z" }),
          cooldown("quota", 330_000, {
            scope: "model",
            model_key: "gpt-5.1-codex",
            retry_at: "2026-10-09T08:00:00Z",
          }),
          cooldown("quota", 300, { scope: "model", model_key: "gpt-5.1-mini" }),
        ],
        quota_checks: [
          quotaCheck("resting"),
          quotaCheck("checking", { scope: "model", model_key: "gpt-5.1-codex" }),
          quotaCheck("due", { scope: "model", model_key: "gpt-5.1" }),
        ],
      }),
    ]);
    renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const bob = await within(card).findByRole("article", { name: "bob@example.com" });
    expect(bob).toHaveTextContent(
      "until the allowance comes back. Next check in about 1 h, at Oct 5, 13:00, rather than at the provider's reset, Oct 9, 08:00.",
    );
    expect(bob).not.toHaveTextContent("Back in about 1 h");
    expect(bob).toHaveTextContent(
      "gpt-5.1-codex: A check is under way: one request is finding out whether the quota is back.",
    );
    // A rest the cap doesn't hold still says when it ends.
    expect(bob).toHaveTextContent("gpt-5.1-mini: Back in about 5 min");
    // A due check has no rest left to show it beside.
    expect(bob).toHaveTextContent(
      "gpt-5.1: A check is due: the next request finds out whether the quota is back.",
    );
    // Nothing counts down out loud.
    expect(within(bob).getByText(/Next check in/).closest("[role], [aria-live]")).toBeNull();
    expect(within(bob).getByText(/A check is due/).closest("[role], [aria-live]")).toBeNull();
  });

  it("says so in one line when nothing needs attention", async () => {
    server([credential(), credential({ name: "claude-bo.json", account: "bo@example.com" })]);
    renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    expect(await within(card).findByText("Nothing needs attention.")).toBeVisible();
    expect(within(card).getByRole("button", { name: "2 ready" })).toBeVisible();
    expect(within(card).queryByRole("article")).toBeNull();
  });

  it("names two sign-ins to one account by their providers", async () => {
    server([credential(), credential({ name: "codex-ada.json", provider: "codex" })]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    await user.click(await within(card).findByRole("button", { name: "2 ready" }));
    expect(
      within(card)
        .getAllByRole("heading", { level: 3 })
        .map((heading) => heading.textContent),
    ).toEqual(["ada@example.com (Claude)", "ada@example.com (Codex)"]);
  });

  it("shows a Claude or Codex credential's quota windows, and which one stops it", async () => {
    server([
      credential({
        quota: {
          observed_at: "2026-10-05T11:58:00.000Z",
          signals: {
            "Anthropic-Ratelimit-Unified-5h-Utilization": "0.25",
            "Anthropic-Ratelimit-Unified-7d-Utilization": "<b>0.5</b>",
            "Anthropic-Ratelimit-Unified-7d-Status": "allowed",
          },
        },
      }),
      credential({
        name: "codex-bob.json",
        id: "codex-bob.json",
        provider: "codex",
        account: "bob@example.com",
        cooldowns: [cooldown("credential_quota", 300)],
        quota: {
          observed_at: "2026-10-05T11:58:00.000Z",
          signals: {
            "X-Codex-Primary-Used-Percent": "40",
            "X-Codex-Primary-Window-Minutes": "300",
            "X-Codex-Secondary-Used-Percent": "100",
            "X-Codex-Secondary-Window-Minutes": "10080",
          },
        },
      }),
      credential({
        name: "codex-new.json",
        id: "codex-new.json",
        provider: "codex",
        account: "cy@example.com",
        quota: { signals: {} },
      }),
    ]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const ada = await openFolded(user, card, "2 ready", "ada@example.com");
    expect(within(ada).getByRole("heading", { name: "Quota" })).toBeVisible();
    expect(ada).toHaveTextContent("5-hour25% used.");
    expect(ada).toHaveTextContent("WeeklyNo reading.");
    expect(ada).not.toHaveTextContent("0.5");
    expect(ada.querySelector("b")).toBeNull();

    const bob = within(card).getByRole("article", { name: "bob@example.com" });
    expect(bob).toHaveTextContent("The weekly limit is used up");
    expect(bob).toHaveTextContent("5-hour40% used.");
    expect(bob).toHaveTextContent("WeeklyUsed up. This is the limit that stops it.");

    const fresh = within(card).getByRole("article", { name: "cy@example.com" });
    await user.click(within(fresh).getByRole("button", { name: "Details cy@example.com" }));
    expect(within(fresh).queryByRole("heading", { name: "Quota" })).toBeNull();
  });

  it("explains a failing credential in plain words", async () => {
    server([credential({ status: "error", status_message: "invalid_grant" })]);
    renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "ada@example.com" });
    expect(within(item).getByText("Failing")).toBeVisible();
    expect(item).toHaveTextContent("The sign-in has expired or was revoked");
    expect(item).toHaveTextContent("What to do: Sign in again with the same account");
  });

  it("says how to add one when there are none", async () => {
    server([]);
    renderApp("/credentials");
    expect(
      await screen.findByText(/^None yet\. Sign in with Claude or ChatGPT, upload credential files/),
    ).toBeVisible();
  });

  it("turns a credential off and on again", async () => {
    const state = server();
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await openFolded(user, card, "1 ready", "ada@example.com");
    await user.click(within(item).getByRole("button", { name: "Turn off ada@example.com" }));
    expect(await within(card).findByRole("status")).toHaveTextContent(
      "Turned off ada@example.com.",
    );
    expect(await within(item).findByText("Off")).toBeVisible();
    expect(within(card).getByRole("button", { name: "1 off" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );
    expect(state.api.callsTo("PATCH", AUTH_FILE_STATUS)[0]?.json()).toEqual({
      name: NAME,
      auth_index: "a1b2c3d4e5f60718",
      disabled: true,
    });

    await user.click(await within(item).findByRole("button", { name: "Turn on ada@example.com" }));
    await waitFor(() => {
      expect(within(card).getByRole("status")).toHaveTextContent(
        "Turned on ada@example.com: the server uses it again.",
      );
    });
    expect(await within(item).findByText("Ready")).toBeVisible();
    expect(state.api.callsTo("PATCH", AUTH_FILE_STATUS)[1]?.json()).toMatchObject({
      disabled: false,
    });
  });

  it("toggles a runtime-only API key without writing config.yaml", async () => {
    const state = server([credential({
      account_type: "api_key", account: "unit-runtime-key", source: "memory", runtime_only: true,
    })]);
    state.api.use(route("GET", V8_CONFIG, { json: {}, headers: { ETag: '"modern"' } }));
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await openFolded(user, card, "1 ready", NAME);
    await user.click(within(item).getByRole("button", { name: `Turn off ${NAME}` }));
    expect(await within(card).findByRole("button", { name: `Turn on ${NAME}` })).toBeVisible();
    expect(state.api.callsTo("PATCH", AUTH_FILE_STATUS)).toHaveLength(1);
    expect(state.api.callsTo("PATCH", AUTH_FILE_STATUS)[0]?.headers.has("if-match")).toBe(false);
    expect(state.api.callsTo("PATCH", V8_CONFIG)).toEqual([]);
  });

  it("stops a credential resting, and keeps saying so as it folds away", async () => {
    const state = server([
      credential({
        cooldowns: [
          cooldown("quota", 120),
          cooldown("quota", 60, { scope: "model", model_key: "claude-opus-4-1" }),
        ],
      }),
    ]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await within(card).findByRole("article", { name: "ada@example.com" });
    await user.click(within(item).getByRole("button", { name: "Stop resting ada@example.com" }));
    const said =
      "ada@example.com and 1 model are no longer resting: the server tries them again with the next request.";
    expect(await within(card).findByRole("status")).toHaveTextContent(said);
    expect(state.api.callsTo("POST", RESET_COOLDOWN)[0]?.json()).toEqual({
      auth_index: "a1b2c3d4e5f60718",
    });
    // Ready now, it folds away with the rest, and the notice stays.
    expect(await within(card).findByRole("button", { name: "1 ready" })).toBeVisible();
    expect(within(card).queryByRole("article", { name: "ada@example.com" })).toBeNull();
    expect(within(card).getByRole("status")).toHaveTextContent(said);
  });

  it("tries a failing credential again", async () => {
    const state = server([
      credential({
        status: "error",
        status_message: "payment_required",
        unavailable: true,
        next_retry_after: "2026-10-05T12:30:00Z",
      }),
    ]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await within(card).findByRole("article", { name: "ada@example.com" });
    expect(within(item).getByText("Failing")).toBeVisible();
    expect(item).toHaveTextContent(
      "What to do: Check the account's billing and plan with the provider, then try it again now.",
    );
    expect(within(item).queryByRole("button", { name: /^Stop resting/ })).toBeNull();
    await user.click(within(item).getByRole("button", { name: "Try again now ada@example.com" }));
    expect(await within(card).findByRole("status")).toHaveTextContent(
      "ada@example.com is back in use: the server tries it again with the next request.",
    );
    expect(state.api.callsTo("POST", RESET_COOLDOWN)[0]?.json()).toEqual({
      auth_index: "a1b2c3d4e5f60718",
    });
    expect(await within(card).findByRole("button", { name: "1 ready" })).toBeVisible();
  });

  it("deletes a credential file once confirmed", async () => {
    const state = server([
      credential(),
      credential({ name: "codex-bob.json", id: "bob", account: "bob@example.com" }),
    ]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await openFolded(user, card, "2 ready", "ada@example.com");
    await user.click(within(item).getByRole("button", { name: "Delete ada@example.com" }));
    const dialog = screen.getByRole("dialog", { name: `Delete ${NAME}?` });
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Delete file" }));

    expect(await within(card).findByRole("status")).toHaveTextContent(`Deleted ${NAME}.`);
    await waitFor(() => {
      expect(within(card).queryByRole("article", { name: "ada@example.com" })).toBeNull();
    });
    expect(within(card).getByRole("article", { name: "bob@example.com" })).toBeVisible();
    expect(state.api.callsTo("DELETE", AUTH_FILES)[0]?.url.searchParams.get("name")).toBe(NAME);
  });

  it("shows a credential's quota as the provider gives it", async () => {
    const state = server([credential({ supports_quota: true })]);
    state.api.use(
      route("POST", QUOTA_FETCH, {
        json: {
          subscription: { plan: "Max", tierName: "20x" },
          groups: [
            {
              displayName: "Usage limits",
              buckets: [{ window: "5 hours", remainingFraction: 0.62 }],
            },
          ],
          summary: [
            { key: "credits", label: "Credits left", value: 12.5, format: "currency", currency: "USD" },
          ],
        },
      }),
    );
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await openFolded(user, card, "1 ready", "ada@example.com");
    await user.click(within(item).getByRole("button", { name: "Check quota of ada@example.com" }));
    const dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
    expect(await within(dialog).findByText("Plan:")).toBeVisible();
    expect(dialog).toHaveTextContent("Plan: Max, 20x");
    expect(dialog).toHaveTextContent("5 hours: 62% left");
    expect(dialog).toHaveTextContent("Credits left$12.50");
    expect(state.api.callsTo("POST", QUOTA_FETCH)[0]?.json()).toEqual({
      auth_index: "a1b2c3d4e5f60718",
    });
  });

  it("says why a quota check failed", async () => {
    const state = server([credential({ supports_quota: true })]);
    state.api.use(
      route("POST", QUOTA_FETCH, {
        status: 502,
        json: { error: "quota probe failed: provider answered 401" },
      }),
    );
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await openFolded(user, card, "1 ready", "ada@example.com");
    await user.click(within(item).getByRole("button", { name: "Check quota of ada@example.com" }));
    const dialog = screen.getByRole("dialog", { name: "Quota of ada@example.com" });
    expect(await within(dialog).findByText("The server failed (HTTP 502)")).toBeVisible();
    expect(dialog).toHaveTextContent("quota probe failed: provider answered 401");
  });

  it("keeps showing what it has when a later read fails, and says so once", async () => {
    const state = server([credential({ cooldowns: [cooldown("quota", 120)] })]);
    let down = false;
    state.api.use(
      route("GET", AUTH_FILES, () =>
        down ? { status: 500, json: { error: "boom" } } : { json: credentialList(state.files) },
      ),
    );
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await within(card).findByRole("article", { name: "ada@example.com" });
    down = true;
    await user.click(within(item).getByRole("button", { name: "Stop resting ada@example.com" }));
    expect(await within(card).findByText("Couldn't check again")).toBeVisible();
    expect(card).toHaveTextContent(
      /The server failed \(HTTP 500\)\. This shows what the server said at \d\d:\d\d:\d\d, and the page keeps trying\./,
    );
    expect(within(card).getAllByText("Couldn't check again")).toHaveLength(1);
    // What it read before still shows.
    expect(within(card).getByRole("article", { name: "ada@example.com" })).toBeVisible();
    expect(within(item).getByText("Resting")).toBeVisible();
  });
});

describe("an address that points at one", () => {
  it("opens a folded credential, scrolls to it and focuses its heading", async () => {
    const scrolled = vi.spyOn(window.Element.prototype, "scrollIntoView");
    try {
      server([
        credential(),
        credential({
          name: "codex-bob.json",
          provider: "codex",
          account: "bob@example.com",
          cooldowns: [cooldown("quota", 300)],
        }),
      ]);
      renderApp(`/credentials#${credentialAnchor(NAME)}`);
      const card = await screen.findByRole("region", { name: CREDENTIALS });
      const heading = await within(card).findByRole("heading", {
        name: "ada@example.com",
        level: 3,
      });
      await waitFor(() => {
        expect(heading).toHaveFocus();
      });
      expect(within(card).getByRole("button", { name: "1 ready" })).toHaveAttribute(
        "aria-expanded",
        "true",
      );
      const ada = within(card).getByRole("article", { name: "ada@example.com" });
      expect(ada).toHaveAttribute("id", credentialAnchor(NAME));
      expect(within(ada).getByRole("button", { name: "Details ada@example.com" })).toHaveAttribute(
        "aria-expanded",
        "true",
      );
      expect(within(ada).getByRole("button", { name: "Turn off ada@example.com" })).toBeVisible();
      expect(scrolled.mock.contexts).toContain(ada);
    } finally {
      scrolled.mockRestore();
    }
  });

  it("opens a claude-cli entry", async () => {
    server([], {}, [
      claudeCliEntry({
        credential: claudeCliCredential({ unavailable: true, cooldowns: [cooldown("quota", 120)] }),
      }),
      claudeCliEntry({ name: "claude-max-2" }),
    ]);
    renderApp(`/credentials#${claudeCliAnchor("claude-max-2")}`);
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    const heading = await within(card).findByRole("heading", { name: "claude-max-2", level: 3 });
    await waitFor(() => {
      expect(heading).toHaveFocus();
    });
    expect(
      within(card).getByRole("button", { name: "Check sign-in claude-max-2" }),
    ).toBeVisible();
  });

  it("focuses a provider API key", async () => {
    server([], {
      claude: [{ "api-key": CLAUDE_KEY, "auth-index": "0123abcd" }],
    });
    renderApp(`/credentials#${providerKeyAnchor("claude", "0123abcd")}`);
    const claude = await screen.findByRole("region", { name: "Claude" });
    await within(claude).findByText("sk-...0001");
    await waitFor(() => {
      expect(document.activeElement).toHaveAttribute("id", providerKeyAnchor("claude", "0123abcd"));
    });
    expect(document.activeElement).toHaveTextContent("sk-...0001");
  });
});

describe("uploading credential files", () => {
  it("uploads a file and lists it", async () => {
    const state = server([]);
    state.api.use(
      route("POST", AUTH_FILES, (request) => {
        for (const file of request.form?.getAll("file") ?? []) {
          if (file instanceof File) {
            state.files.push(credential({ name: file.name, id: file.name }));
          }
        }
        return { json: { status: "ok" } };
      }),
    );
    const { user } = renderApp("/credentials");
    await screen.findByText(/^None yet\./);
    const file = new File(['{"type":"claude"}'], "claude-new.json", { type: "application/json" });
    await user.upload(screen.getByLabelText("Upload credential files"), file);

    expect(await screen.findByText(/^claude-new\.json\. The server uses it from now on\.$/)).toBeVisible();
    const card = screen.getByRole("region", { name: CREDENTIALS });
    const added = await openFolded(user, card, "1 ready", "ada@example.com");
    expect(added).toHaveTextContent("File: claude-new.json");
    const sent = state.api.callsTo("POST", AUTH_FILES)[0];
    expect(sent?.form?.getAll("file").map((part) => (part as File).name)).toEqual(["claude-new.json"]);
    expect(sent?.headers.get("content-type")).toBeNull();
  });

  it("says which of several files failed", async () => {
    const state = server([]);
    state.api.use(
      route("POST", AUTH_FILES, {
        status: 207,
        json: {
          status: "partial",
          uploaded: 1,
          files: ["a.json"],
          failed: [{ name: "b.json", error: "invalid auth file" }],
        },
      }),
    );
    const { user } = renderApp("/credentials");
    await screen.findByText(/^None yet\./);
    await user.upload(screen.getByLabelText("Upload credential files"), [
      new File(["{}"], "a.json", { type: "application/json" }),
      new File(["{}"], "b.json", { type: "application/json" }),
    ]);
    expect(await screen.findByText("Uploaded 1 of 2 files")).toBeVisible();
    expect(screen.getByText("b.json").closest("li")).toHaveTextContent("b.json: invalid auth file");
  });

  it("says why the server refused a file", async () => {
    const state = server([]);
    state.api.use(route("POST", AUTH_FILES, { status: 400, json: { error: "invalid auth file" } }));
    const { user } = renderApp("/credentials");
    await screen.findByText(/^None yet\./);
    await user.upload(
      screen.getByLabelText("Upload credential files"),
      new File(["{}"], "a.json", { type: "application/json" }),
    );
    expect(await screen.findByText("The server refused the request")).toBeVisible();
    expect(screen.getByText("invalid auth file")).toBeVisible();
  });
});

describe("signing in with Claude or ChatGPT", () => {
  it("opens from a link without starting anything, and starts when asked", async () => {
    const state = server();
    state.api.use(
      route("GET", SIGN_IN_START.codex, { json: { status: "ok", url: SIGN_IN_URL, state: STATE } }),
      route("GET", SIGN_IN_STATUS, { json: { status: "wait" } }),
    );
    const { user } = renderApp("/credentials?start=codex");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with ChatGPT" });
    expect(screen.getByRole("button", { name: "Sign in with ChatGPT" })).toBeVisible();
    expect(screen.queryByText(/Sign in with Codex/)).toBeNull();
    expect(dialog).toHaveTextContent(
      "The server starts a sign-in with your ChatGPT account (for Codex). You sign in on ChatGPT's site",
    );
    expect(state.api.callsTo("GET", SIGN_IN_START.codex)).toEqual([]);
    expect(within(dialog).getByRole("button", { name: "Start" })).toHaveFocus();

    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    const link = await within(dialog).findByRole("link", { name: /^Open ChatGPT's sign-in page/ });
    expect(link).toHaveAttribute("href", SIGN_IN_URL);
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noopener noreferrer");
    expect(dialog).toHaveTextContent("Finish within five minutes of starting");
    expect(within(dialog).getByText("Time left: 5:00")).toBeVisible();
    await waitFor(() => {
      expect(link).toHaveFocus();
    });
    expect(state.api.callsTo("GET", SIGN_IN_START.codex)[0]?.url.searchParams.get("is_webui")).toBe(
      "true",
    );
    expect(state.api.callsTo("GET", SIGN_IN_STATUS)[0]?.url.searchParams.get("state")).toBe(STATE);
  });

  it("finishes on its own when the provider sends the browser back to the server", async () => {
    const state = server([]);
    claudeSignIn(state, [{ status: "ok" }]);
    const { user } = renderApp("/credentials");
    await screen.findByText(/^None yet\./);
    await user.click(screen.getByRole("button", { name: "Sign in with Claude" }));
    const dialog = screen.getByRole("dialog", { name: "Sign in with Claude" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    expect(await within(dialog).findByText("Signed in with Claude")).toBeVisible();
    expect(within(dialog).getByRole("button", { name: "Done" })).toHaveFocus();
    const card = screen.getByRole("region", { name: CREDENTIALS });
    expect(await within(card).findByRole("button", { name: "1 ready" })).toBeVisible();

    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(state.api.callsTo("DELETE", SIGN_IN_SESSION)).toEqual([]);
  });

  it("finishes with the address pasted, after checking it is this sign-in's", async () => {
    const state = server([]);
    let pasted = false;
    state.api.use(
      route("GET", SIGN_IN_START.claude, { json: { status: "ok", url: SIGN_IN_URL, state: STATE } }),
      route("GET", SIGN_IN_STATUS, () => ({ json: { status: pasted ? "ok" : "wait" } })),
      route("POST", SIGN_IN_CALLBACK, () => {
        pasted = true;
        return { json: { status: "ok" } };
      }),
    );
    const { user, router } = renderApp("/credentials?start=claude");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    expect(await within(dialog).findByText("Waiting for you to sign in…")).toBeVisible();

    // It shows, so you can check what you pasted, and hides when asked.
    const field = within(dialog).getByLabelText("Address of the page it sent you to");
    expect(field).toHaveAttribute("type", "text");
    expect(field).toHaveAttribute("autocomplete", "off");
    await user.click(within(dialog).getByRole("button", { name: "Hide the address" }));
    expect(field).toHaveAttribute("type", "password");
    await user.click(within(dialog).getByRole("button", { name: "Show the address" }));
    expect(field).toHaveAttribute("type", "text");
    await user.click(field);
    await user.paste("http://localhost:54545/callback?code=c&state=another");
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(
      await within(dialog).findByText(/^That address isn't from this sign-in\./),
    ).toBeVisible();
    expect(state.api.callsTo("POST", SIGN_IN_CALLBACK)).toEqual([]);

    await user.clear(field);
    await user.paste(CALLBACK);
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(await within(dialog).findByText("Signed in with Claude")).toBeVisible();
    expect(state.api.callsTo("POST", SIGN_IN_CALLBACK)[0]?.json()).toEqual({
      provider: "claude",
      redirect_url: CALLBACK,
    });
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    expect(router.state.location.search).toBe("");
  });

  it("explains a pasted address the server refused", async () => {
    const state = server([]);
    claudeSignIn(state, [{ status: "wait" }]);
    state.api.use(
      route("POST", SIGN_IN_CALLBACK, {
        status: 409,
        json: { status: "error", error: "oauth flow is already completed" },
      }),
    );
    const { user } = renderApp("/credentials?start=claude");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await user.click(await within(dialog).findByLabelText("Address of the page it sent you to"));
    await user.paste(CALLBACK);
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(await within(dialog).findByText("This sign-in has already finished")).toBeVisible();
  });

  it("explains a sign-in that failed, and starts again", async () => {
    const state = server([]);
    claudeSignIn(state, [{ status: "error", error: "Timeout waiting for OAuth callback" }]);
    const { user } = renderApp("/credentials?start=claude");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    expect(await within(dialog).findByText("The sign-in waited too long")).toBeVisible();
    expect(dialog).toHaveTextContent("Start again, and finish signing in within five minutes.");
    const again = within(dialog).getByRole("button", { name: "Start again" });
    expect(again).toHaveFocus();
    await user.click(again);
    await waitFor(() => {
      expect(state.api.callsTo("GET", SIGN_IN_START.claude)).toHaveLength(2);
    });
  });

  it("counts the five minutes down, says when one is left, and offers to start again", async () => {
    // Only the clock and the countdown's tick: fetches and waits run as usual.
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      const state = server([]);
      claudeSignIn(state, [{ status: "wait" }]);
      state.api.use(route("DELETE", SIGN_IN_SESSION, { json: { status: "ok", cancelled: true } }));
      const { user } = renderApp("/credentials?start=claude");
      const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
      await user.click(within(dialog).getByRole("button", { name: "Start" }));
      const said = () =>
        within(dialog)
          .getAllByRole("status")
          .map((region) => region.textContent)
          .join(" | ");

      const left = await within(dialog).findByText("Time left: 5:00");
      // Not read out as it changes, each second.
      expect(left.closest("[role=status], [role=alert], [aria-live]")).toBeNull();
      expect(said()).not.toContain("minute");
      act(() => {
        vi.advanceTimersByTime(SIGN_IN_LIMIT_MS - 61_000);
      });
      expect(left).toHaveTextContent("Time left: 1:01");
      expect(said()).not.toContain("minute");
      expect(within(dialog).queryByRole("button", { name: "Start again" })).toBeNull();

      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(left).toHaveTextContent("Time left: 1:00");
      expect(said()).toContain("One minute left to finish signing in.");
      expect(within(dialog).getByText("Time is nearly up")).toBeVisible();
      expect(within(dialog).getByRole("button", { name: "Start again" })).toBeVisible();
      const once = said();
      act(() => {
        vi.advanceTimersByTime(30_000);
      });
      expect(left).toHaveTextContent("Time left: 0:30");
      expect(said()).toBe(once);

      // Time's up here, but the server still says wait: it is asked, not blamed.
      act(() => {
        vi.advanceTimersByTime(30_000);
      });
      expect(within(dialog).getByText("Checking with the server…")).toBeVisible();
      expect(within(dialog).queryByRole("alert")).toBeNull();
      // Past the grace with the server still waiting, it has failed.
      act(() => {
        vi.advanceTimersByTime(SIGN_IN_GRACE_MS);
      });
      expect(within(dialog).getByRole("alert")).toHaveTextContent("The sign-in waited too long");
      expect(within(dialog).queryByText(/^Time left/)).toBeNull();
      const again = within(dialog).getByRole("button", { name: "Start again" });
      await waitFor(() => {
        expect(again).toHaveFocus();
      });
      await user.click(again);
      expect(await within(dialog).findByText("Time left: 5:00")).toBeVisible();
      expect(state.api.callsTo("GET", SIGN_IN_START.claude)).toHaveLength(2);
      // The server may still have been waiting for the first: it is given up.
      expect(state.api.callsTo("DELETE", SIGN_IN_SESSION)[0]?.url.searchParams.get("state")).toBe(
        STATE,
      );
    } finally {
      vi.useRealTimers();
    }
  });

  it("gives a waiting sign-in up when closed", async () => {
    const state = server([]);
    claudeSignIn(state, [{ status: "wait" }]);
    state.api.use(
      route("DELETE", SIGN_IN_SESSION, { json: { status: "ok", cancelled: true } }),
    );
    const { user, router } = renderApp("/credentials?start=claude");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByRole("button", { name: "Give up" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    await waitFor(() => {
      expect(state.api.callsTo("DELETE", SIGN_IN_SESSION)[0]?.url.searchParams.get("state")).toBe(
        STATE,
      );
    });
    expect(router.state.location.search).toBe("");
  });

  it("starts without the local callback when the server can't listen for it", async () => {
    const state = server([]);
    state.api.use(
      route("GET", SIGN_IN_START.claude, (request) =>
        request.url.searchParams.has("is_webui")
          ? { status: 500, json: { error: "failed to start callback server" } }
          : { json: { status: "ok", url: SIGN_IN_URL, state: STATE } },
      ),
      route("GET", SIGN_IN_STATUS, { json: { status: "wait" } }),
    );
    const { user } = renderApp("/credentials?start=claude");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    expect(
      await within(dialog).findByText("The server couldn't listen for the sign-in's answer"),
    ).toBeVisible();
    expect(dialog).toHaveTextContent("port 54545");
    await user.click(within(dialog).getByRole("button", { name: "Start without it" }));
    expect(
      await within(dialog).findByText(/^Claude then sends you to a page on localhost that won't load\./),
    ).toBeVisible();
    const calls = state.api.callsTo("GET", SIGN_IN_START.claude);
    expect(calls).toHaveLength(2);
    expect(calls[1]?.url.searchParams.has("is_webui")).toBe(false);
  });
});

describe("adding OpenAI API access to a Codex credential", () => {
  const FILE = "codex-grace+test@example.com.json";
  const OPEN = /^Add OpenAI API access/;
  const BIND_URL = `https://sign-in.example/oauth/authorize?state=${STATE}`;
  // As ChatGPT sends the browser back: with the client ID the server issued.
  const BIND_REDIRECT = `http://localhost:1455/auth/callback?code=bind-code&state=${STATE}&client_id=bind-client`;

  function codex(overrides: Partial<Credential> = {}): Credential {
    return credential({
      name: FILE,
      id: FILE,
      type: "codex",
      provider: "codex",
      auth_index: "c0dec0dec0dec0de",
      account: "grace@example.com",
      label: "grace@example.com",
      ...overrides,
    });
  }

  /** A field of a pasted `redirect`, as the server reads it; null for a bad address. */
  function fieldOf(redirect: string): ((name: string) => string) | null {
    let address: URL;
    try {
      address = new URL(redirect.trim());
    } catch {
      return null;
    }
    const fragment = new URLSearchParams(address.hash.replace(/^#/, ""));
    return (name) => {
      const value = address.searchParams.get(name)?.trim() ?? "";
      return value === "" ? (fragment.get(name)?.trim() ?? "") : value;
    };
  }

  /**
   * Why the server would refuse a bind's pasted `redirect`, in its words; else
   * null. It reads the fields trimmed, the query first and the fragment where
   * the query's is empty, and wants this bind's state and a code or an error.
   */
  function refusal(redirect: string): string | null {
    const field = fieldOf(redirect);
    if (field === null) {
      return "invalid redirect URL";
    }
    if (field("state") !== STATE) {
      return "invalid state";
    }
    if (field("code") === "" && field("error") === "" && field("error_description") === "") {
      return "code or error is required";
    }
    return null;
  }

  /**
   * A bind that ends only after the server accepts a callback with its state,
   * a code and the issued client ID (`bind.accepted`), unless `script` gives
   * the status calls' answers instead, the last one for good. While
   * `bind.hold` is set, an accepted bind still says "wait".
   */
  function bindServer(
    script: SignInStatus[] | null = null,
    cancel: unknown = { status: "ok", cancelled: true },
  ) {
    const state = server([codex()]);
    const bind = { accepted: false, hold: false, failed: "" };
    let polls = 0;
    state.api.use(
      route("GET", BIND_START, { json: { status: "ok", url: BIND_URL, state: STATE } }),
      route("GET", BIND_STATUS, () => {
        if (script === null) {
          if (bind.failed !== "") {
            return { json: { status: "error", error: bind.failed } };
          }
          return { json: { status: bind.accepted && !bind.hold ? "ok" : "wait" } };
        }
        const status = script[Math.min(polls, script.length - 1)] ?? { status: "wait" };
        polls += 1;
        return { json: status };
      }),
      route("POST", BIND_CALLBACK, (request) => {
        const { redirect_url: redirect } = request.json() as { redirect_url: string };
        const problem = refusal(redirect);
        if (problem !== null) {
          return { status: 400, json: { status: "error", error: problem } };
        }
        // A provider error ends the session before any exchange, and wins; so
        // does a code without the issued client ID. Only a code with one succeeds.
        const field = fieldOf(redirect);
        const error = field?.("error") ?? "";
        const described = field?.("error_description") ?? "";
        if (error !== "" || described !== "") {
          bind.failed = error === "" ? described : error;
        } else if (field?.("client_id") === "") {
          bind.failed = "callback did not contain an issued client ID";
        } else {
          bind.accepted = true;
        }
        return { json: { status: "ok" } };
      }),
      route("DELETE", BIND_SESSION, { json: cancel }),
    );
    return Object.assign(state, { bind });
  }

  async function openBind(user: UserEvent, tally = "1 ready"): Promise<HTMLElement> {
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const item = await openFolded(user, card, tally, "grace@example.com");
    await user.click(within(item).getByRole("button", { name: OPEN }));
    return screen.getByRole("dialog", { name: "Add OpenAI API access" });
  }

  it("is offered only on Codex sign-ins with a file, and names its item", async () => {
    server([
      codex(),
      credential(),
      codex({ name: "codex-key", id: "codex-key", account_type: "api_key", account: "sk-x", auth_index: "beef" }),
      codex({
        name: "codex-memory",
        id: "codex-memory",
        source: "memory",
        runtime_only: true,
        auth_index: "ffff",
        account: "memory@example.com",
      }),
    ]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CREDENTIALS });
    const grace = await openFolded(user, card, "4 ready", "grace@example.com");
    expect(
      within(grace).getByRole("button", { name: "Add OpenAI API access grace@example.com" }),
    ).toBeVisible();
    expect(shownText(within(grace).getByRole("button", { name: OPEN })).trim()).toBe(
      "Add OpenAI API access",
    );
    // No other item, hidden or not, has the button.
    expect(screen.getAllByRole("button", { name: OPEN, hidden: true })).toHaveLength(1);
    // The ordinary sign-in button is the page's one, unchanged.
    expect(screen.getByRole("button", { name: "Sign in with ChatGPT" })).toBeVisible();
  });

  it("explains the bind, starts it with the bind query, and finishes with the pasted address", async () => {
    const state = bindServer();
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    expect(dialog).toHaveTextContent("keeps its Codex sign-in and gains Sign in with ChatGPT tokens");
    expect(dialog).toHaveTextContent("go to the OpenAI API, under your ChatGPT plan");
    expect(state.api.callsTo("GET", BIND_START)).toEqual([]);
    expect(within(dialog).getByRole("button", { name: "Start" })).toHaveFocus();

    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    const link = await within(dialog).findByRole("link", { name: /^Open ChatGPT's sign-in page/ });
    expect(link).toHaveAttribute("href", BIND_URL);
    const start = state.api.callsTo("GET", BIND_START)[0];
    expect(start?.url.searchParams.get("provider")).toBe("codex");
    expect(start?.url.searchParams.get("mode")).toBe("siwc-bind");
    expect(start?.url.searchParams.get("name")).toBe(FILE);
    expect(start?.url.searchParams.get("is_webui")).toBe("1");
    expect(start?.url.search).toContain("name=codex-grace%2Btest%40example.com.json");
    expect(state.api.callsTo("GET", SIGN_IN_START.codex)).toEqual([]);

    // Nothing ends it but an accepted callback.
    await waitFor(() => {
      expect(state.api.callsTo("GET", BIND_STATUS).length).toBeGreaterThan(0);
    });
    expect(within(dialog).queryByText("OpenAI API access added")).toBeNull();
    const field = within(dialog).getByLabelText("Address of the page it sent you to");
    await user.click(field);
    await user.paste(BIND_REDIRECT);
    const listed = state.api.callsTo("GET", AUTH_FILES).length;
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(await within(dialog).findByText("OpenAI API access added")).toBeVisible();
    expect(state.api.callsTo("POST", BIND_CALLBACK)[0]?.json()).toEqual({
      provider: "codex",
      redirect_url: BIND_REDIRECT,
    });
    expect(state.api.callsTo("GET", BIND_STATUS)[0]?.url.searchParams.get("state")).toBe(STATE);
    expect(within(dialog).getByRole("button", { name: "Done" })).toHaveFocus();
    await waitFor(() => {
      expect(state.api.callsTo("GET", AUTH_FILES).length).toBeGreaterThan(listed);
    });
    expect(dialog).not.toHaveTextContent("bind-code");

    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(state.api.callsTo("DELETE", BIND_SESSION)).toEqual([]);
  });

  it("takes an address with its fields in the fragment, the query winning", async () => {
    const state = bindServer([{ status: "wait" }]);
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    const field = within(dialog).getByLabelText("Address of the page it sent you to");
    const send = within(dialog).getByRole("button", { name: "Finish signing in" });
    const posted = () => state.api.callsTo("POST", BIND_CALLBACK).map((call) => call.json());

    // The query's state wins over the fragment's: this one isn't this sign-in's.
    await user.click(field);
    await user.paste(
      `http://localhost:1455/cb?state=other&code=query-code&client_id=I#state=${STATE}&code=frag-code`,
    );
    await user.click(send);
    expect(await within(dialog).findByText(/^That address isn't from this sign-in\./)).toBeVisible();
    expect(dialog).not.toHaveTextContent("query-code");
    expect(dialog).not.toHaveTextContent("frag-code");
    expect(posted()).toEqual([]);

    // Only a fragment.
    const fragment = `http://localhost:1455/cb#code=frag-code&state=${STATE}&client_id=I`;
    await user.clear(field);
    await user.click(field);
    await user.paste(fragment);
    await user.click(send);
    await waitFor(() => {
      expect(posted()).toEqual([{ provider: "codex", redirect_url: fragment }]);
    });
    expect(dialog).not.toHaveTextContent("frag-code");

    // The query's state is this sign-in's, whatever the fragment says.
    const both = `http://localhost:1455/cb?state=${STATE}&code=query-code&client_id=I#state=other`;
    await user.clear(field);
    await user.click(field);
    await user.paste(both);
    await user.click(send);
    await waitFor(() => {
      expect(posted()).toHaveLength(2);
    });
    expect(posted()[1]).toEqual({ provider: "codex", redirect_url: both });
  });

  it("says what to do when the server refuses to start", async () => {
    const state = server([codex()]);
    state.api.use(
      route("GET", BIND_START, { status: 400, json: { error: "invalid SIWC bind target" } }),
    );
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    expect(
      await within(dialog).findByText("This credential can't take OpenAI API access"),
    ).toBeVisible();
    expect(dialog).toHaveTextContent("What to do: Check the credential in the list");
    expect(within(dialog).getByRole("button", { name: "Try again" })).toHaveFocus();
  });

  it("says what to do when the bind fails after the sign-in", async () => {
    bindServer([{ status: "error", error: "Failed to save authentication tokens" }]);
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    expect(await within(dialog).findByText("The server couldn't add the access")).toBeVisible();
    expect(within(dialog).getByRole("alert")).toHaveTextContent("What to do: Start again.");
    expect(within(dialog).getByRole("button", { name: "Start again" })).toHaveFocus();
  });

  it("gives a waiting bind up when asked", async () => {
    const state = bindServer([{ status: "wait" }]);
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByRole("button", { name: "Give up" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(state.api.callsTo("DELETE", BIND_SESSION)[0]?.url.searchParams.get("state")).toBe(STATE);
  });

  it("keeps following a bind the server was already saving when it was given up", async () => {
    const state = bindServer([{ status: "wait" }, { status: "ok" }], {
      status: "ok",
      cancelled: false,
    });
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByRole("button", { name: "Give up" }));

    expect(await within(dialog).findByText("The server is already saving it")).toBeVisible();
    expect(within(dialog).queryByText(/cancelled/i)).toBeNull();
    expect(screen.getByRole("dialog", { name: "Add OpenAI API access" })).toBeVisible();
    expect(within(dialog).getByRole("button", { name: "Close" })).toHaveFocus();
    // It goes on to the end, and says the access was added.
    expect(
      await within(dialog).findByText("OpenAI API access added", undefined, { timeout: 4000 }),
    ).toBeVisible();
    expect(state.api.callsTo("DELETE", BIND_SESSION)).toHaveLength(1);
  });

  it("asks for the client ID in the pasted address before it sends one", async () => {
    const state = bindServer();
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    const field = within(dialog).getByLabelText("Address of the page it sent you to");
    await user.click(field);
    await user.paste(`http://localhost:1455/auth/callback?code=bind-code&state=${STATE}`);
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(await within(dialog).findByText(/^That address has no client ID in it\./)).toBeVisible();
    expect(state.api.callsTo("POST", BIND_CALLBACK)).toEqual([]);
  });

  it("shows the failure, not success, when the pasted address carries a provider error", async () => {
    const state = bindServer();
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByLabelText("Address of the page it sent you to"));
    await user.paste(
      `http://localhost:1455/auth/callback?error=access_denied&state=${STATE}&client_id=bind-client`,
    );
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(
      await within(dialog).findByText("The sign-in failed", undefined, { timeout: 4000 }),
    ).toBeVisible();
    expect(dialog).toHaveTextContent("access_denied");
    expect(within(dialog).queryByText("OpenAI API access added")).toBeNull();
    expect(state.api.callsTo("POST", BIND_CALLBACK)).toHaveLength(1);
  });

  it.each([
    ["a 300 with a JSON body", { status: 300, json: { status: "ok", cancelled: true } }],
    ["a 200 that isn't the cancel answer", { status: 200, json: { status: "error" } }],
    ["a 200 with no JSON", { status: 200, text: "ok" }],
    ["a cancelled of text", { status: 200, json: { status: "ok", cancelled: "false" } }],
    ["a cancelled of null", { status: 200, json: { status: "ok", cancelled: null } }],
  ])("does not take %s for a stopped bind", async (_name, reply) => {
    const state = bindServer([{ status: "wait" }]);
    state.api.use(route("DELETE", BIND_SESSION, reply));
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByRole("button", { name: "Give up" }));

    expect(await within(dialog).findByText(/^Unexpected answer/)).toBeVisible();
    expect(dialog).toHaveTextContent("this sign-in may still finish");
    expect(screen.getByRole("dialog", { name: "Add OpenAI API access" })).toBeVisible();
    expect(within(dialog).getByText("Waiting for you to sign in…")).toBeVisible();
    expect(state.api.callsTo("GET", BIND_START)).toHaveLength(1);
    const polled = state.api.callsTo("GET", BIND_STATUS).length;
    await waitFor(
      () => {
        expect(state.api.callsTo("GET", BIND_STATUS).length).toBeGreaterThan(polled);
      },
      { timeout: 4000 },
    );
  });

  it("does not take a failed give-up for a cancel", async () => {
    const state = bindServer([{ status: "wait" }]);
    let failing = true;
    state.api.use(
      route("DELETE", BIND_SESSION, () =>
        failing
          ? { status: 500, json: { error: "session store unavailable" } }
          : { json: { status: "ok", cancelled: true } },
      ),
    );
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByRole("button", { name: "Give up" }));

    // It stays on this sign-in, says it may still finish, and goes on polling.
    expect(await within(dialog).findByText("The server failed (HTTP 500)")).toBeVisible();
    expect(dialog).toHaveTextContent("this sign-in may still finish");
    expect(screen.getByRole("dialog", { name: "Add OpenAI API access" })).toBeVisible();
    expect(within(dialog).getByText("Waiting for you to sign in…")).toBeVisible();
    expect(state.api.callsTo("GET", BIND_START)).toHaveLength(1);
    const polled = state.api.callsTo("GET", BIND_STATUS).length;
    await waitFor(
      () => {
        expect(state.api.callsTo("GET", BIND_STATUS).length).toBeGreaterThan(polled);
      },
      { timeout: 4000 },
    );

    // It can be tried again, and a stopped bind closes the dialog.
    failing = false;
    await user.click(within(dialog).getByRole("button", { name: "Give up" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(state.api.callsTo("DELETE", BIND_SESSION)).toHaveLength(2);
    expect(state.api.callsTo("GET", BIND_START)).toHaveLength(1);
  });

  it("takes Escape as a give-up, and stays open while the server decides", async () => {
    const state = bindServer([{ status: "wait" }], { status: "ok", cancelled: false });
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");

    // The browser's Escape is a cancelable event; the dialog keeps it from closing.
    expect(fireEvent(dialog, new Event("cancel", { cancelable: true }))).toBe(false);
    expect(await within(dialog).findByText("The server is already saving it")).toBeVisible();
    expect(dialog).toHaveAttribute("open");
    expect(state.api.callsTo("DELETE", BIND_SESSION)).toHaveLength(1);

    // Closed from there, the action works again.
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    const card = screen.getByRole("region", { name: CREDENTIALS });
    await user.click(
      within(card).getByRole("button", { name: "Add OpenAI API access grace@example.com" }),
    );
    expect(screen.getByRole("dialog", { name: "Add OpenAI API access" })).toBeVisible();
  });

  it("ignores a second Escape while the give-up is on its way", async () => {
    const state = bindServer([{ status: "wait" }]);
    let release: () => void = () => undefined;
    const answer = new Promise<void>((resolve) => {
      release = resolve;
    });
    state.api.use(
      route("DELETE", BIND_SESSION, async () => {
        await answer;
        return { json: { status: "ok", cancelled: true } };
      }),
    );
    const { user } = renderApp("/credentials");
    const dialog = await openBind(user);
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");

    fireEvent(dialog, new Event("cancel", { cancelable: true }));
    await waitFor(() => {
      expect(state.api.callsTo("DELETE", BIND_SESSION)).toHaveLength(1);
    });
    expect(within(dialog).getByRole("button", { name: "Give up" })).toBeDisabled();
    fireEvent(dialog, new Event("cancel", { cancelable: true }));
    expect(screen.getByRole("dialog", { name: "Add OpenAI API access" })).toBeVisible();
    release();
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(state.api.callsTo("DELETE", BIND_SESSION)).toHaveLength(1);
  });

  it("keeps the dialog and its polling when the credential moves in the list", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      const state = bindServer();
      const { user } = renderApp("/credentials");
      const dialog = await openBind(user);
      await user.click(within(dialog).getByRole("button", { name: "Start" }));
      await within(dialog).findByText("Waiting for you to sign in…");
      const card = screen.getByRole("region", { name: CREDENTIALS });
      // Ready, so folded away for now.
      expect(within(card).getByRole("button", { name: "1 ready" })).toBeVisible();

      // It starts to rest; the next read of the list puts it in the other group.
      const file = state.files[0];
      if (file === undefined) {
        throw new Error("no credential");
      }
      file.cooldowns = [cooldown("credential_quota", 300)];
      const polled = state.api.callsTo("GET", BIND_STATUS).length;
      await act(() => vi.advanceTimersByTimeAsync(CREDENTIALS_REFRESH_MS + 1000));
      const row = await within(card).findByRole("article", { name: "grace@example.com" });
      expect(within(row).getByText("Resting")).toBeVisible();
      expect(within(card).queryByRole("button", { name: /ready$/ })).toBeNull();

      // The same dialog is still there, still following the sign-in.
      expect(screen.getByRole("dialog", { name: "Add OpenAI API access" })).toBe(dialog);
      expect(within(dialog).getByText("Waiting for you to sign in…")).toBeVisible();
      expect(state.api.callsTo("GET", BIND_STATUS).length).toBeGreaterThan(polled);

      // And it finishes, whichever group the credential is in.
      await user.click(within(dialog).getByLabelText("Address of the page it sent you to"));
      await user.paste(BIND_REDIRECT);
      await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
      await act(() => vi.advanceTimersByTimeAsync(SIGN_IN_POLL_MS + 500));
      expect(await within(dialog).findByText("OpenAI API access added")).toBeVisible();
    } finally {
      vi.useRealTimers();
    }
  });

  it("moves focus into the dialog when checking removes the address field, bind", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      bindServer();
      const { user } = renderApp("/credentials");
      const dialog = await openBind(user);
      await user.click(within(dialog).getByRole("button", { name: "Start" }));
      await within(dialog).findByText("Waiting for you to sign in…");
      const field = within(dialog).getByLabelText("Address of the page it sent you to");
      await user.click(field);
      expect(field).toHaveFocus();

      await act(() => vi.advanceTimersByTimeAsync(SIGN_IN_LIMIT_MS + 2000));
      const checking = await within(dialog).findByText("Checking with the server…");
      expect(field).not.toBeInTheDocument();
      expect(document.activeElement).not.toBe(document.body);
      expect(dialog.contains(document.activeElement)).toBe(true);
      expect(checking).toHaveFocus();
    } finally {
      vi.useRealTimers();
    }
  });

  it("moves focus into the dialog when checking removes the address field, ordinary", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      const state = server([]);
      claudeSignIn(state, [{ status: "wait" }]);
      const { user } = renderApp("/credentials?start=claude");
      const dialog = await screen.findByRole("dialog", { name: "Sign in with Claude" });
      await user.click(within(dialog).getByRole("button", { name: "Start" }));
      await within(dialog).findByText("Waiting for you to sign in…");
      const field = within(dialog).getByLabelText("Address of the page it sent you to");
      await user.click(field);
      expect(field).toHaveFocus();

      await act(() => vi.advanceTimersByTimeAsync(SIGN_IN_LIMIT_MS + 2000));
      const checking = await within(dialog).findByText("Checking with the server…");
      expect(document.activeElement).not.toBe(document.body);
      expect(dialog.contains(document.activeElement)).toBe(true);
      expect(checking).toHaveFocus();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not call a bind failed at five minutes while the server still says wait", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      const state = bindServer();
      const { user } = renderApp("/credentials");
      const dialog = await openBind(user);
      await user.click(within(dialog).getByRole("button", { name: "Start" }));
      await within(dialog).findByText("Waiting for you to sign in…");

      // The callback comes at 4:59; the server is still exchanging it.
      await act(() => vi.advanceTimersByTimeAsync(SIGN_IN_LIMIT_MS - 1000));
      state.bind.hold = true;
      await user.click(within(dialog).getByLabelText("Address of the page it sent you to"));
      await user.paste(BIND_REDIRECT);
      await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
      await waitFor(() => {
        expect(state.api.callsTo("POST", BIND_CALLBACK)).toHaveLength(1);
      });

      // Past 5:00 it says it is checking, not that it failed.
      await act(() => vi.advanceTimersByTimeAsync(3000));
      const checking = await within(dialog).findByText("Checking with the server…");
      expect(checking).toBeVisible();
      expect(checking).toHaveAttribute("role", "status");
      // Nothing in it has a fixed width, so it fits a 390 px screen.
      expect(checking.className).not.toMatch(/(^|\s)(min-|max-)?w-/);
      expect(checking.parentElement?.className).not.toMatch(/(^|\s)(min-|max-)?w-/);
      expect(within(dialog).queryByRole("alert")).toBeNull();
      expect(within(dialog).queryByText("The sign-in waited too long")).toBeNull();
      await act(() => vi.advanceTimersByTimeAsync(60_000));
      expect(within(dialog).getByText("Checking with the server…")).toBeVisible();
      expect(within(dialog).queryByRole("alert")).toBeNull();

      // Then the server finishes saving.
      state.bind.hold = false;
      await act(() => vi.advanceTimersByTimeAsync(SIGN_IN_POLL_MS + 500));
      expect(await within(dialog).findByText("OpenAI API access added")).toBeVisible();
      expect(within(dialog).queryByText("The sign-in waited too long")).toBeNull();
      expect(state.api.callsTo("DELETE", BIND_SESSION)).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("signing in with ChatGPT, not binding", () => {
  it("starts without a mode, a name or a provider", async () => {
    const state = server([]);
    state.api.use(
      route("GET", SIGN_IN_START.codex, { json: { status: "ok", url: SIGN_IN_URL, state: STATE } }),
      route("GET", SIGN_IN_STATUS, { json: { status: "wait" } }),
    );
    const { user } = renderApp("/credentials?start=codex");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with ChatGPT" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByRole("link", { name: /^Open ChatGPT's sign-in page/ });
    const params = state.api.callsTo("GET", SIGN_IN_START.codex)[0]?.url.searchParams;
    expect(params?.has("mode")).toBe(false);
    expect(params?.has("name")).toBe(false);
    expect(params?.has("provider")).toBe(false);
    expect(state.api.callsTo("GET", BIND_START)).toEqual([]);
  });

  it("still refuses an address with its fields only in the fragment", async () => {
    const state = server([]);
    state.api.use(
      route("GET", SIGN_IN_START.codex, { json: { status: "ok", url: SIGN_IN_URL, state: STATE } }),
      route("GET", SIGN_IN_STATUS, { json: { status: "wait" } }),
      route("POST", SIGN_IN_CALLBACK, { json: { status: "ok" } }),
    );
    const { user } = renderApp("/credentials?start=codex");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with ChatGPT" });
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await within(dialog).findByText("Waiting for you to sign in…");
    await user.click(within(dialog).getByLabelText("Address of the page it sent you to"));
    await user.paste(`http://localhost:1455/cb#code=frag-code&state=${STATE}`);
    await user.click(within(dialog).getByRole("button", { name: "Finish signing in" }));
    expect(await within(dialog).findByText(/^That address isn't from this sign-in\./)).toBeVisible();
    expect(dialog).not.toHaveTextContent("frag-code");
    expect(state.api.callsTo("POST", SIGN_IN_CALLBACK)).toEqual([]);
  });
});

describe("the Claude Code accounts", () => {
  it("shows each claude-cli entry's state, cooldown, last error and quota, and checks nothing", async () => {
    const { api } = server(
      [],
      {},
      [
        claudeCliEntry({
          prefix: "max1",
          config_dir: "~/.claude-second",
          credential: claudeCliCredential({
            status: "error",
            status_message: "unauthorized",
            unavailable: true,
            next_retry_after: "2026-10-05T12:10:00.123456789Z",
            cooldowns: [cooldown("unauthorized", 600, { http_status: 401 })],
            quota: {
              observed_at: "2026-10-05T11:58:00.5Z",
              signals: {
                "anthropic-ratelimit-unified-5h-utilization": "0.4",
                "anthropic-ratelimit-unified-7d-utilization": "1",
                "anthropic-ratelimit-unified-7d-status": "rejected",
              },
            },
          }),
          last_error: { message: "Not signed in to Claude Code", http_status: 401 },
        }),
        claudeCliEntry({ name: "claude-max-2" }),
        claudeCliEntry({
          name: "spare",
          config_dir: "/srv/claude/spare",
          disabled: true,
          credential: null,
        }),
      ],
    );
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    expect(card).toHaveTextContent("The claude-cli entries in config.yaml");
    expect(card).toHaveTextContent(/Checked at \d\d:\d\d:\d\d\./);

    const first = await within(card).findByRole("article", { name: "claude-max-1" });
    expect(within(first).getByText("Resting")).toBeVisible();
    expect(first).toHaveTextContent("Claude Code, run by the server · Last used in the last 10 min");
    expect(first).toHaveTextContent("Prefixmax1: calls to max1/<model> go to it");
    expect(first).toHaveTextContent("Config directory~/.claude-second");
    expect(first).toHaveTextContent("The provider refused the credential");
    expect(first).toHaveTextContent("Back in about 10 min");
    expect(first).toHaveTextContent(
      "What to do: Check its sign-in. If Claude Code isn't signed in, sign it in again",
    );
    expect(within(first).getByRole("heading", { name: "Last error" })).toBeVisible();
    expect(first).toHaveTextContent("Not signed in to Claude Code (HTTP 401)");
    expect(within(first).getByRole("heading", { name: "Quota" })).toBeVisible();
    expect(first).toHaveTextContent("5-hour40% used.");
    expect(first).toHaveTextContent("WeeklyUsed up. This is the limit that stops it.");
    expect(first).toHaveTextContent("214 succeeded, 3 failed");
    expect(within(first).getByRole("button", { name: "Stop resting claude-max-1" })).toBeVisible();

    const second = await openFolded(user, card, "1 ready, 1 off", "claude-max-2");
    expect(within(second).getByText("Ready")).toBeVisible();
    expect(second).toHaveTextContent("PrefixNone");
    expect(second).toHaveTextContent(
      "Config directoryNone: the server's CLAUDE_CONFIG_DIR, else Claude Code's default",
    );
    expect(within(second).queryByRole("heading", { name: "Last error" })).toBeNull();
    expect(within(second).queryByRole("heading", { name: "Quota" })).toBeNull();
    expect(within(second).queryByRole("button", { name: /^Stop resting/ })).toBeNull();

    const spare = within(card).getByRole("article", { name: "spare" });
    await user.click(within(spare).getByRole("button", { name: "Details spare" }));
    expect(within(spare).getByText("Off")).toBeVisible();
    expect(spare).toHaveTextContent("Turned off in config.yaml: the server sends it no requests.");
    expect(spare).toHaveTextContent("Config directory/srv/claude/spare");
    expect(spare).not.toHaveTextContent("succeeded");
    expect(spare).not.toHaveTextContent("Last used");
    expect(within(spare).getByRole("button", { name: "Check sign-in spare" })).toBeVisible();
    // Off sorts before ready.
    expect(
      within(card)
        .getAllByRole("heading", { level: 3 })
        .map((heading) => heading.textContent),
    ).toEqual(["claude-max-1", "spare", "claude-max-2"]);

    // A check runs Claude Code on the server, so nothing checks by itself.
    expect(api.callsTo("GET", CLAUDE_CLI_AUTH_STATUS)).toEqual([]);
    expect(api.unhandled).toEqual([]);
  });

  it("says when a capped quota rest of an entry's is being checked", async () => {
    server(
      [],
      {},
      [
        claudeCliEntry({
          credential: claudeCliCredential({
            unavailable: true,
            cooldowns: [cooldown("credential_quota", 330_000, { retry_at: "2026-10-09T08:00:00Z" })],
            quota_checks: [quotaCheck("checking")],
          }),
        }),
      ],
    );
    renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    const entry = await within(card).findByRole("article", { name: "claude-max-1" });
    expect(entry).toHaveTextContent(
      "until the allowance comes back. A check is under way: one request is finding out whether the quota is back.",
    );
    expect(entry).not.toHaveTextContent("Back in");
  });

  it("checks an entry's sign-in when asked, and says how it is signed in", async () => {
    const state = server([], {}, [claudeCliEntry()]);
    state.api.use(
      route("GET", CLAUDE_CLI_AUTH_STATUS, { json: { loggedIn: true, authMethod: "claude.ai" } }),
    );
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    const item = await openFolded(user, card, "1 ready", "claude-max-1");
    await user.click(within(item).getByRole("button", { name: "Check sign-in claude-max-1" }));
    expect(await within(item).findByText("Signed in")).toBeVisible();
    expect(item).toHaveTextContent("Claude Code is signed in. Sign-in method: claude.ai.");
    const calls = state.api.callsTo("GET", CLAUDE_CLI_AUTH_STATUS);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.url.searchParams.get("name")).toBe("claude-max-1");
  });

  it("says what to run when an entry isn't signed in, with its config directory", async () => {
    const state = server([], {}, [
      claudeCliEntry({ config_dir: "~/.claude-second" }),
      claudeCliEntry({ name: "claude-max-2" }),
    ]);
    state.api.use(
      route("GET", CLAUDE_CLI_AUTH_STATUS, { json: { loggedIn: false, authMethod: "" } }),
    );
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    const first = await openFolded(user, card, "2 ready", "claude-max-1");
    await user.click(within(first).getByRole("button", { name: "Check sign-in claude-max-1" }));
    expect(await within(first).findByText("Not signed in")).toBeVisible();
    expect(first).toHaveTextContent("Claude Code isn't signed in");
    expect(first).not.toHaveTextContent("Sign-in method");
    expect(
      within(first).getByText("CLAUDE_CONFIG_DIR=~/.claude-second claude auth login"),
    ).toBeVisible();
    expect(
      within(first).getByRole("button", { name: "Copy the sign-in command of claude-max-1" }),
    ).toBeVisible();

    const second = within(card).getByRole("article", { name: "claude-max-2" });
    await user.click(within(second).getByRole("button", { name: "Details claude-max-2" }));
    await user.click(within(second).getByRole("button", { name: "Check sign-in claude-max-2" }));
    expect(await within(second).findByText("Not signed in")).toBeVisible();
    expect(within(second).getByText("claude auth login")).toBeVisible();
    expect(second).not.toHaveTextContent("CLAUDE_CONFIG_DIR=");
    expect(
      state.api
        .callsTo("GET", CLAUDE_CLI_AUTH_STATUS)
        .map((call) => call.url.searchParams.get("name")),
    ).toEqual(["claude-max-1", "claude-max-2"]);
  });

  it("explains a check that Claude Code failed, or took too long for", async () => {
    const state = server([], {}, [claudeCliEntry({ config_dir: "/srv/claude/max-1" })]);
    let answer = {
      status: 502,
      json: { error: "claude_cli_failed", message: "claude-cli claude-max-1: could not run claude" },
    };
    state.api.use(route("GET", CLAUDE_CLI_AUTH_STATUS, () => answer));
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    const item = await openFolded(user, card, "1 ready", "claude-max-1");
    const check = within(item).getByRole("button", { name: "Check sign-in claude-max-1" });
    await user.click(check);
    expect(await within(item).findByText("Claude Code couldn't be checked")).toBeVisible();
    expect(item).toHaveTextContent("claude-cli claude-max-1: could not run claude");

    answer = {
      status: 504,
      json: { error: "claude_cli_timeout", message: "claude-cli claude-max-1: timed out" },
    };
    await user.click(check);
    expect(await within(item).findByText("Claude Code took too long to answer")).toBeVisible();
    expect(
      within(item).getByText("CLAUDE_CONFIG_DIR=/srv/claude/max-1 claude auth status"),
    ).toBeVisible();
    expect(within(item).queryByText("Claude Code couldn't be checked")).toBeNull();
  });

  it("stops an entry resting", async () => {
    const state = server([], {}, [
      claudeCliEntry({
        credential: claudeCliCredential({ unavailable: true, cooldowns: [cooldown("quota", 120)] }),
      }),
    ]);
    const { user } = renderApp("/credentials");
    const card = await screen.findByRole("region", { name: CLAUDE_CLI });
    const item = await within(card).findByRole("article", { name: "claude-max-1" });
    expect(within(item).getByText("Resting")).toBeVisible();
    await user.click(within(item).getByRole("button", { name: "Stop resting claude-max-1" }));
    expect(await within(card).findByRole("status")).toHaveTextContent(
      "claude-max-1 is no longer resting: the server tries it again with the next request.",
    );
    expect(state.api.callsTo("POST", RESET_COOLDOWN)[0]?.json()).toEqual({
      auth_index: "3734a62b508f0029",
    });
    expect(await within(card).findByRole("button", { name: "1 ready" })).toBeVisible();
    expect(state.api.callsTo("GET", CLAUDE_CLI_ENTRIES).length).toBeGreaterThan(1);
  });

  /** Renders the page from `api`, and expects no Claude Code accounts card. */
  async function expectNoCard(api: MockApi) {
    renderApp("/credentials");
    expect(await screen.findByRole("region", { name: "Provider API keys" })).toBeVisible();
    await waitFor(() => {
      expect(api.callsTo("GET", CLAUDE_CLI_ENTRIES)).toHaveLength(1);
    });
    await screen.findByText(/^None yet\./);
    expect(screen.queryByRole("region", { name: CLAUDE_CLI })).toBeNull();
    expect(screen.queryByText("This server can't do that yet")).toBeNull();
  }

  it("shows no card for a server without entries", async () => {
    await expectNoCard(server([], {}, []).api);
  });

  it("shows no card for a server that doesn't list entries", async () => {
    await expectNoCard(server([], {}, null).api);
  });

  it("shows no card for an open-ferry older than the entries route", async () => {
    const { api } = server([], {}, null);
    api.use(
      route("GET", CLAUDE_CLI_ENTRIES, {
        status: 404,
        json: { error: "not_found", message: "no such route" },
      }),
    );
    await expectNoCard(api);
  });
});

describe("the provider API keys", () => {
  const existing: ProviderKey = {
    "api-key": CLAUDE_KEY,
    "base-url": "https://gateway.example/v1",
    "auth-index": "0123abcd",
    models: [{ name: "claude-sonnet-4-5", alias: "sonnet" }],
  };

  it("lists the keys masked, and adds one keeping the others as they were", async () => {
    const state = server([], { claude: [existing] });
    const { user, router } = renderApp("/credentials");
    const claude = await screen.findByRole("region", { name: "Claude" });
    expect(await within(claude).findByText("sk-...0001")).toBeVisible();
    expect(claude).not.toHaveTextContent(CLAUDE_KEY);
    expect(claude).toHaveTextContent("Sent to https://gateway.example/v1.");
    expect(within(screen.getByRole("region", { name: "Codex" })).getByText("None.")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Add an API key" }));
    expect(router.state.location.search).toBe("?start=key");
    const dialog = screen.getByRole("dialog", { name: "Add a provider API key" });
    const key = within(dialog).getByLabelText("API key");
    expect(key).toHaveFocus();
    await user.paste("sk-ant-test-provider-key-0002");
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));

    expect(await screen.findByText("Added the Claude key: the server uses it from now on.")).toBeVisible();
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    expect(router.state.location.search).toBe("");
    expect(state.api.callsTo("PUT", KEY_LISTS.claude.path)[0]?.json()).toEqual([
      {
        "api-key": CLAUDE_KEY,
        "base-url": "https://gateway.example/v1",
        models: [{ name: "claude-sonnet-4-5", alias: "sonnet" }],
      },
      { "api-key": "sk-ant-test-provider-key-0002" },
    ]);
    expect(await within(claude).findByText("sk-...0002")).toBeVisible();
  });

  it("asks a Codex key for its base URL, and refuses a key already there", async () => {
    const state = server([], { claude: [existing] });
    const { user } = renderApp("/credentials?start=key");
    const dialog = await screen.findByRole("dialog", { name: "Add a provider API key" });
    await user.selectOptions(within(dialog).getByLabelText("Provider"), "codex");
    await user.click(within(dialog).getByLabelText("API key"));
    await user.paste("sk-test-codex-key-0001");
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(
      await within(dialog).findByText("A Codex key needs a base URL: the server skips one without it."),
    ).toBeVisible();

    await user.selectOptions(within(dialog).getByLabelText("Provider"), "claude");
    await user.clear(within(dialog).getByLabelText("API key"));
    await user.paste(CLAUDE_KEY);
    await user.click(within(dialog).getByLabelText("Base URL (optional)"));
    await user.paste("https://gateway.example/v1");
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(await within(dialog).findByText("That key is already in the list")).toBeVisible();
    expect(state.api.callsTo("PUT", KEY_LISTS.claude.path)).toEqual([]);
    expect(state.api.callsTo("PUT", KEY_LISTS.codex.path)).toEqual([]);
  });

  it("deletes a key by its place in the list, never putting it in an address", async () => {
    const state = server([], {
      claude: [{ "api-key": "sk-ant-test-provider-key-0009" }, existing],
    });
    const { user } = renderApp("/credentials");
    const remove = await screen.findByRole("button", { name: "Delete the Claude key sk-...0001" });
    await user.click(remove);
    const dialog = screen.getByRole("dialog", { name: "Delete this Claude key?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete key" }));
    await waitFor(() => {
      expect(screen.queryByText("sk-...0001")).toBeNull();
    });
    expect(state.api.callsTo("DELETE", KEY_LISTS.claude.path)[0]?.url.searchParams.get("index")).toBe(
      "1",
    );
    expect(state.api.calls.some((call) => call.url.href.includes(CLAUDE_KEY))).toBe(false);
    expect(state.keys.claude).toEqual([{ "api-key": "sk-ant-test-provider-key-0009" }]);
  });

  it("says when the server doesn't serve them", async () => {
    server([], null);
    renderApp("/credentials");
    expect(await screen.findByText("Provider API keys can't be changed here")).toBeVisible();
    expect(screen.queryByRole("button", { name: "Add an API key" })).toBeNull();
  });

  it("stops offering changes once the server says it can't save config.yaml", async () => {
    const state = server([], { claude: [existing] });
    state.api.use(
      route("PUT", KEY_LISTS.gemini.path, {
        status: 503,
        json: { error: "config writer unavailable" },
      }),
    );
    const { user } = renderApp("/credentials?start=key");
    const dialog = await screen.findByRole("dialog", { name: "Add a provider API key" });
    await user.selectOptions(within(dialog).getByLabelText("Provider"), "gemini");
    await user.click(within(dialog).getByLabelText("API key"));
    await user.paste("AIza-test-gemini-key-0001");
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(await within(dialog).findByText("This server can't save config.yaml")).toBeVisible();
    expect(dialog).toHaveTextContent("nothing was changed");

    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).toBeNull();
    });
    const card = screen.getByRole("region", { name: "Provider API keys" });
    expect(card).toHaveTextContent("This server has no way to save config.yaml from here.");
    // The keys still show, but nothing offers to change them.
    expect(within(card).getByText("sk-...0001")).toBeVisible();
    expect(within(card).queryByRole("button", { name: /^Delete/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Add an API key" })).toBeNull();
  });
});
