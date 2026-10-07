import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import {
  AUTH_FILES,
  AUTH_FILE_STATUS,
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
import {
  claudeCliCredential,
  claudeCliEntry,
  cooldown,
  credential,
  credentialList,
} from "../../test/fixtures";
import { loadFirst } from "../../test/loadFirst";
import { mockApi, route, type MockApi } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";

const NAME = "claude-ada@example.com.json";
const STATE = "test-state-0001";
const SIGN_IN_URL = `https://sign-in.example/oauth/authorize?state=${STATE}`;
const CALLBACK = `http://localhost:54545/callback?code=test-code&state=${STATE}`;
const CLAUDE_KEY = "sk-ant-test-provider-key-0001";

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
        state.files.push(credential({ name: "claude-new@example.com.json", id: "new" }));
      }
      return { json: status };
    }),
  );
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
        id_token: { plan_type: "plus" },
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
    renderApp("/credentials");
    expect(await screen.findByRole("heading", { name: "Credentials", level: 1 })).toBeVisible();
    expect(
      within(screen.getByRole("navigation", { name: "Main" })).getByRole("link", {
        name: "Credentials",
      }),
    ).toHaveAttribute("aria-current", "page");

    const ada = await screen.findByRole("article", { name: NAME });
    expect(within(ada).getByText("Ready")).toBeVisible();
    expect(ada).toHaveTextContent("Claude sign-in");
    expect(ada).toHaveTextContent("ada@example.com");
    expect(ada).toHaveTextContent("1,520 succeeded, 12 failed");
    expect(ada).toHaveTextContent("In the last 20 min42 succeeded, 1 failed");
    expect(within(ada).getByRole("button", { name: "Delete" })).toBeVisible();
    expect(within(ada).queryByRole("button", { name: "Reset cooldown" })).toBeNull();
    expect(within(ada).queryByRole("button", { name: "Check quota" })).toBeNull();

    const bob = screen.getByRole("article", { name: "codex-bob.json" });
    expect(within(bob).getByText("Resting")).toBeVisible();
    expect(bob).toHaveTextContent("The account's quota is used up");
    expect(bob).toHaveTextContent("Back in about 5 min");
    expect(bob).toHaveTextContent("What to do: Wait until then");
    expect(within(bob).getByRole("heading", { name: "Models resting" })).toBeVisible();
    expect(bob).toHaveTextContent("gpt-5.1-codex: Back in less than a minute");
    expect(bob).toHaveTextContent("Rate-limited");
    expect(within(bob).getByText("plus")).toBeVisible();
    expect(within(bob).getByRole("button", { name: "Reset cooldown" })).toBeVisible();

    const key = screen.getByRole("article", { name: "openai-compat-1" });
    expect(key).toHaveTextContent("openai-compatibility API key, kept in memory only");
    expect(key).toHaveTextContent("sk-...0001");
    expect(key).not.toHaveTextContent("sk-test-provider-key-0001");
    expect(within(key).queryByRole("button", { name: "Delete" })).toBeNull();
    expect(api.unhandled).toEqual([]);
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
      credential({ name: "codex-new.json", id: "codex-new.json", provider: "codex", quota: { signals: {} } }),
    ]);
    renderApp("/credentials");
    const ada = await screen.findByRole("article", { name: NAME });
    expect(within(ada).getByRole("heading", { name: "Quota" })).toBeVisible();
    expect(ada).toHaveTextContent("5-hour25% used.");
    expect(ada).toHaveTextContent("WeeklyNo reading.");
    expect(ada).not.toHaveTextContent("0.5");
    expect(ada.querySelector("b")).toBeNull();

    const bob = screen.getByRole("article", { name: "codex-bob.json" });
    expect(bob).toHaveTextContent("The weekly limit is used up");
    expect(bob).toHaveTextContent("5-hour40% used.");
    expect(bob).toHaveTextContent("WeeklyUsed up. This is the limit that stops it.");

    const fresh = screen.getByRole("article", { name: "codex-new.json" });
    expect(within(fresh).queryByRole("heading", { name: "Quota" })).toBeNull();
  });

  it("explains a failing credential in plain words", async () => {
    server([credential({ status: "error", status_message: "invalid_grant" })]);
    renderApp("/credentials");
    const item = await screen.findByRole("article", { name: NAME });
    expect(within(item).getByText("Failing")).toBeVisible();
    expect(item).toHaveTextContent("The sign-in has expired or was revoked");
    expect(item).toHaveTextContent("What to do: Sign in again with the same account");
  });

  it("says how to add one when there are none", async () => {
    server([]);
    renderApp("/credentials");
    expect(
      await screen.findByText(/^None yet\. Sign in with Claude or Codex, upload a credential file/),
    ).toBeVisible();
  });

  it("turns a credential off and on again", async () => {
    const state = server();
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: NAME });
    await user.click(within(item).getByRole("button", { name: "Turn off" }));
    expect(await within(item).findByText("Turned off.")).toBeVisible();
    expect(await within(item).findByText("Off")).toBeVisible();
    expect(state.api.callsTo("PATCH", AUTH_FILE_STATUS)[0]?.json()).toEqual({
      name: NAME,
      auth_index: "a1b2c3d4e5f60718",
      disabled: true,
    });

    await user.click(await within(item).findByRole("button", { name: "Turn on" }));
    expect(await within(item).findByText("Turned on: the server uses it again.")).toBeVisible();
    expect(await within(item).findByText("Ready")).toBeVisible();
    expect(state.api.callsTo("PATCH", AUTH_FILE_STATUS)[1]?.json()).toMatchObject({
      disabled: false,
    });
  });

  it("resets a credential's cooldowns", async () => {
    const state = server([
      credential({
        cooldowns: [
          cooldown("quota", 120),
          cooldown("quota", 60, { scope: "model", model_key: "claude-opus-4-1" }),
        ],
      }),
    ]);
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: NAME });
    await user.click(within(item).getByRole("button", { name: "Reset cooldown" }));
    expect(
      await within(item).findByText(
        "Cooldown reset for it and 1 model: the server tries it again with the next request.",
      ),
    ).toBeVisible();
    expect(state.api.callsTo("POST", RESET_COOLDOWN)[0]?.json()).toEqual({
      auth_index: "a1b2c3d4e5f60718",
    });
    expect(await within(item).findByText("Ready")).toBeVisible();
    expect(within(item).queryByRole("button", { name: "Reset cooldown" })).toBeNull();
  });

  it("deletes a credential file once confirmed", async () => {
    const state = server([credential(), credential({ name: "codex-bob.json", id: "bob" })]);
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: NAME });
    await user.click(within(item).getByRole("button", { name: "Delete" }));
    const dialog = screen.getByRole("dialog", { name: `Delete ${NAME}?` });
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));

    expect(await screen.findByText(NAME, { selector: "span" })).toBeVisible();
    expect(screen.getByRole("status", { name: "" })).toHaveTextContent(`Deleted ${NAME}.`);
    await waitFor(() => {
      expect(screen.queryByRole("article", { name: NAME })).toBeNull();
    });
    expect(screen.getByRole("article", { name: "codex-bob.json" })).toBeVisible();
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
    const item = await screen.findByRole("article", { name: NAME });
    await user.click(within(item).getByRole("button", { name: "Check quota" }));
    const dialog = screen.getByRole("dialog", { name: `Quota of ${NAME}` });
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
    const item = await screen.findByRole("article", { name: NAME });
    await user.click(within(item).getByRole("button", { name: "Check quota" }));
    const dialog = screen.getByRole("dialog", { name: `Quota of ${NAME}` });
    expect(await within(dialog).findByText("The server failed (HTTP 502)")).toBeVisible();
    expect(dialog).toHaveTextContent("quota probe failed: provider answered 401");
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
    await user.upload(screen.getByLabelText("Upload files"), file);

    expect(await screen.findByText(/^claude-new\.json\. The server uses it from now on\.$/)).toBeVisible();
    expect(await screen.findByRole("article", { name: "claude-new.json" })).toBeVisible();
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
    await user.upload(screen.getByLabelText("Upload files"), [
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
      screen.getByLabelText("Upload files"),
      new File(["{}"], "a.json", { type: "application/json" }),
    );
    expect(await screen.findByText("The server refused the request")).toBeVisible();
    expect(screen.getByText("invalid auth file")).toBeVisible();
  });
});

describe("signing in with Claude or Codex", () => {
  it("opens from a link without starting anything, and starts when asked", async () => {
    const state = server();
    state.api.use(
      route("GET", SIGN_IN_START.codex, { json: { status: "ok", url: SIGN_IN_URL, state: STATE } }),
      route("GET", SIGN_IN_STATUS, { json: { status: "wait" } }),
    );
    const { user } = renderApp("/credentials?start=codex");
    const dialog = await screen.findByRole("dialog", { name: "Sign in with Codex" });
    expect(state.api.callsTo("GET", SIGN_IN_START.codex)).toEqual([]);
    expect(within(dialog).getByRole("button", { name: "Start" })).toHaveFocus();

    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    const link = await within(dialog).findByRole("link", { name: /^Open Codex's sign-in page/ });
    expect(link).toHaveAttribute("href", SIGN_IN_URL);
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noopener noreferrer");
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
    expect(await screen.findByRole("article", { name: "claude-new@example.com.json" })).toBeVisible();

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

    const field = within(dialog).getByLabelText("Address of the page it sent you to");
    expect(field).toHaveAttribute("type", "password");
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
    renderApp("/credentials");
    const card = await screen.findByRole("region", { name: "Claude Code accounts" });
    expect(card).toHaveTextContent("The claude-cli entries in config.yaml");

    const first = within(card).getByRole("article", { name: "claude-max-1" });
    expect(within(first).getByText("Resting")).toBeVisible();
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
    expect(within(first).getByRole("button", { name: "Reset cooldown" })).toBeVisible();

    const second = within(card).getByRole("article", { name: "claude-max-2" });
    expect(within(second).getByText("Ready")).toBeVisible();
    expect(second).toHaveTextContent("PrefixNone");
    expect(second).toHaveTextContent(
      "Config directoryNone: the server's CLAUDE_CONFIG_DIR, else Claude Code's default",
    );
    expect(within(second).queryByRole("heading", { name: "Last error" })).toBeNull();
    expect(within(second).queryByRole("heading", { name: "Quota" })).toBeNull();
    expect(within(second).queryByRole("button", { name: "Reset cooldown" })).toBeNull();

    const spare = within(card).getByRole("article", { name: "spare" });
    expect(within(spare).getByText("Off")).toBeVisible();
    expect(spare).toHaveTextContent("Turned off in config.yaml: the server sends it no requests.");
    expect(spare).toHaveTextContent("Config directory/srv/claude/spare");
    expect(spare).not.toHaveTextContent("succeeded");
    expect(within(spare).getByRole("button", { name: "Check sign-in" })).toBeVisible();

    // A check runs Claude Code on the server, so nothing checks by itself.
    expect(api.callsTo("GET", CLAUDE_CLI_AUTH_STATUS)).toEqual([]);
    expect(api.unhandled).toEqual([]);
  });

  it("checks an entry's sign-in when asked, and says how it is signed in", async () => {
    const state = server([], {}, [claudeCliEntry()]);
    state.api.use(
      route("GET", CLAUDE_CLI_AUTH_STATUS, { json: { loggedIn: true, authMethod: "claude.ai" } }),
    );
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "claude-max-1" });
    await user.click(within(item).getByRole("button", { name: "Check sign-in" }));
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
    const first = await screen.findByRole("article", { name: "claude-max-1" });
    await user.click(within(first).getByRole("button", { name: "Check sign-in" }));
    expect(await within(first).findByText("Not signed in")).toBeVisible();
    expect(first).toHaveTextContent("Claude Code isn't signed in");
    expect(first).not.toHaveTextContent("Sign-in method");
    expect(
      within(first).getByText("CLAUDE_CONFIG_DIR=~/.claude-second claude auth login"),
    ).toBeVisible();
    expect(
      within(first).getByRole("button", { name: "Copy the sign-in command of claude-max-1" }),
    ).toBeVisible();

    const second = screen.getByRole("article", { name: "claude-max-2" });
    await user.click(within(second).getByRole("button", { name: "Check sign-in" }));
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
    const item = await screen.findByRole("article", { name: "claude-max-1" });
    const check = within(item).getByRole("button", { name: "Check sign-in" });
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

  it("resets an entry's cooldown", async () => {
    const state = server([], {}, [
      claudeCliEntry({
        credential: claudeCliCredential({ unavailable: true, cooldowns: [cooldown("quota", 120)] }),
      }),
    ]);
    const { user } = renderApp("/credentials");
    const item = await screen.findByRole("article", { name: "claude-max-1" });
    expect(within(item).getByText("Resting")).toBeVisible();
    await user.click(within(item).getByRole("button", { name: "Reset cooldown" }));
    expect(
      await within(item).findByText(
        "Cooldown reset: the server tries it again with the next request.",
      ),
    ).toBeVisible();
    expect(state.api.callsTo("POST", RESET_COOLDOWN)[0]?.json()).toEqual({
      auth_index: "3734a62b508f0029",
    });
    expect(await within(item).findByText("Ready")).toBeVisible();
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
    expect(screen.queryByRole("region", { name: "Claude Code accounts" })).toBeNull();
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

  it("removes a key by its place in the list, never putting it in an address", async () => {
    const state = server([], {
      claude: [{ "api-key": "sk-ant-test-provider-key-0009" }, existing],
    });
    const { user } = renderApp("/credentials");
    const remove = await screen.findByRole("button", { name: "Remove the Claude key sk-...0001" });
    await user.click(remove);
    const dialog = screen.getByRole("dialog", { name: "Remove this Claude key?" });
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
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
    expect(within(card).queryByRole("button", { name: /^Remove/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Add an API key" })).toBeNull();
  });
});
