import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { RouterProvider, createMemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";

import {
  AUTH_FILES,
  KEY_LISTS,
  KEY_PROVIDERS,
  type Credential,
  type KeyProvider,
  type ProviderKey,
} from "../../api/credentials";
import { CLAUDE_CLI_ENTRIES, type ClaudeCliEntry } from "../../api/dashboard";
import { storeKey } from "../../api/keyStorage";
import { QueryProvider } from "../../app/QueryProvider";
import { SessionProvider } from "../../session/session";
import {
  claudeCliCredential,
  claudeCliEntry,
  cooldown,
  credential,
  credentialList,
} from "../../test/fixtures";
import { mockApi, route, type MockApi } from "../../test/mockApi";
import { TEST_KEY } from "../../test/renderApp";
import { claudeCliAnchor, credentialAnchor } from "../credentials/anchors";
import { ProvidersCard } from "./ProvidersCard";

const HEALTH = "Account health";
const CONNECT = "Connect a provider";
const NOT_LIVE = "[role=status], [role=alert], [aria-live]";

interface Server {
  api: MockApi;
  /** The server's credential files, as the mock holds them now. */
  files: Credential[];
}

/**
 * A server with `files`, provider `keys` and claude-cli `entries`; null
 * entries for one that doesn't serve them.
 */
function server(
  files: Credential[] = [],
  keys: Partial<Record<KeyProvider, ProviderKey[]>> = {},
  entries: ClaudeCliEntry[] | null = [],
): Server {
  const state: Server = { api: mockApi(), files: [...files] };
  state.api.use(
    route("GET", AUTH_FILES, () => ({ json: credentialList(state.files) })),
    ...KEY_PROVIDERS.map((provider) =>
      route("GET", KEY_LISTS[provider].path, {
        json: { [KEY_LISTS[provider].list]: keys[provider] ?? [] },
      }),
    ),
  );
  if (entries !== null) {
    state.api.use(route("GET", CLAUDE_CLI_ENTRIES, { json: { entries } }));
  }
  return state;
}

/** Renders the card on its own, as the Overview does, with somewhere for its links to go. */
function renderCard() {
  storeKey(TEST_KEY);
  const router = createMemoryRouter(
    [
      { path: "/", element: <ProvidersCard /> },
      { path: "/credentials", element: <p>The Credentials page</p> },
    ],
    { initialEntries: ["/"] },
  );
  const user = userEvent.setup();
  render(
    <SessionProvider>
      <QueryProvider retryDelay={0}>
        <RouterProvider router={router} />
      </QueryProvider>
    </SessionProvider>,
  );
  return { router, user };
}

describe("the providers card", () => {
  it("offers the ways to connect one on a first run", async () => {
    const { api } = server();
    renderCard();
    const card = await screen.findByRole("region", { name: CONNECT });
    expect(card).toHaveTextContent("The server has no account or key to send requests with yet.");
    expect(card).toHaveTextContent("Sign in with a Claude account or a ChatGPT account (for Codex)");
    expect(within(card).getByRole("link", { name: "Sign in with Claude" })).toHaveAttribute(
      "href",
      "/credentials?start=claude",
    );
    expect(within(card).getByRole("link", { name: "Sign in with ChatGPT" })).toHaveAttribute(
      "href",
      "/credentials?start=codex",
    );
    expect(within(card).getByRole("link", { name: "Add a provider API key" })).toHaveAttribute(
      "href",
      "/credentials?start=key",
    );
    // Uploading happens here, not a page away.
    const upload = within(card).getByLabelText("Upload credential files");
    expect(upload).toHaveAttribute("type", "file");
    expect(upload).toHaveAttribute("multiple");
    expect(within(card).queryByRole("link", { name: /upload/i })).toBeNull();
    expect(screen.queryByRole("region", { name: HEALTH })).toBeNull();
    expect(api.unhandled).toEqual([]);
  });

  it("uploads credential files, then shows how they're doing", async () => {
    const state = server();
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
    const { user } = renderCard();
    const connect = await screen.findByRole("region", { name: CONNECT });
    const file = new File(['{"type":"claude"}'], "claude-new.json", { type: "application/json" });
    await user.upload(within(connect).getByLabelText("Upload credential files"), file);

    const card = await screen.findByRole("region", { name: HEALTH });
    expect(within(card).getByRole("status")).toHaveTextContent(
      "claude-new.json. The server uses it from now on.",
    );
    expect(await within(card).findByText("The one account is ready.")).toBeVisible();
    expect(state.api.callsTo("POST", AUTH_FILES)).toHaveLength(1);
    expect(screen.queryByRole("region", { name: CONNECT })).toBeNull();
  });

  it("counts provider keys, which don't say how they're doing", async () => {
    const { api } = server([], { gemini: [{ "api-key": "AIzaSy-test-gemini-key-0001" }] });
    renderCard();
    const card = await screen.findByRole("region", { name: HEALTH });
    expect(
      await within(card).findByText(
        "The server has 1 provider API key. Keys don't report how they're doing, only accounts.",
      ),
    ).toBeVisible();
    expect(card).not.toHaveTextContent("AIzaSy");
    expect(within(card).queryByRole("list")).toBeNull();
    expect(api.unhandled).toEqual([]);
  });

  it("lists the failing accounts, then the resting ones soonest back first, each linking to it", async () => {
    const { api } = server(
      [
        credential(),
        credential({
          name: "claude-eve.json",
          id: "claude-eve.json",
          account: "eve@example.com",
          cooldowns: [cooldown("quota", 1800)],
        }),
        credential({
          name: "codex-bob.json",
          id: "codex-bob.json",
          provider: "codex",
          account: "bob@example.com",
          cooldowns: [cooldown("quota", 300)],
        }),
        credential({
          name: "claude-dee.json",
          id: "claude-dee.json",
          account: "dee@example.com",
          disabled: true,
          status: "disabled",
        }),
        credential({
          name: "claude-cy.json",
          id: "claude-cy.json",
          account: "cy@example.com",
          status: "error",
          status_message: "invalid_grant",
        }),
      ],
      { claude: [{ "api-key": "sk-ant-test-provider-key-0001" }] },
      [
        claudeCliEntry(),
        claudeCliEntry({
          name: "claude-max-2",
          credential: claudeCliCredential({
            label: "claude-max-2",
            unavailable: true,
            cooldowns: [cooldown("unauthorized", 600, { http_status: 401 })],
          }),
        }),
      ],
    );
    const { router, user } = renderCard();
    const card = await screen.findByRole("region", { name: HEALTH });
    const board = await within(card).findByRole("list", { name: "Accounts that need attention" });
    const rows = within(board).getAllByRole("listitem");
    expect(rows.map((row) => within(row).getByRole("link").textContent)).toEqual([
      "cy@example.com",
      "bob@example.com",
      "claude-max-2",
      "eve@example.com",
    ]);
    const [cy, bob, max, eve] = rows as [HTMLElement, HTMLElement, HTMLElement, HTMLElement];

    expect(within(cy).getByText("Failing")).toBeVisible();
    expect(cy).toHaveTextContent("Claude · The sign-in has expired or was revoked");
    expect(cy).not.toHaveTextContent("Back at");
    expect(within(cy).getByRole("link")).toHaveAttribute(
      "href",
      `/credentials#${credentialAnchor("claude-cy.json")}`,
    );

    expect(within(bob).getByText("Resting")).toBeVisible();
    expect(bob).toHaveTextContent("Codex · Rate-limited");
    expect(bob).toHaveTextContent(/Back at \d\d:\d\d \(in about 5 min\)/);
    expect(max).toHaveTextContent("Claude Code · The provider refused the credential");
    expect(max).toHaveTextContent(/Back at \d\d:\d\d \(in about 10 min\)/);
    expect(within(max).getByRole("link")).toHaveAttribute(
      "href",
      `/credentials#${claudeCliAnchor("claude-max-2")}`,
    );
    expect(eve).toHaveTextContent(/Back at \d\d:\d\d \(in about 30 min\)/);

    // The ready and off ones are only counted, and the keys beside them.
    expect(card).toHaveTextContent(
      "The rest: 2 ready, 1 off. The server also has 1 provider API key.",
    );
    expect(within(card).queryByRole("link", { name: "ada@example.com" })).toBeNull();
    expect(within(card).getByRole("link", { name: "Open Credentials" })).toHaveAttribute(
      "href",
      "/credentials",
    );

    await user.click(within(bob).getByRole("link"));
    expect(await screen.findByText("The Credentials page")).toBeVisible();
    expect(router.state.location.hash).toBe(`#${credentialAnchor("codex-bob.json")}`);
    expect(api.unhandled).toEqual([]);
  });

  it("says so in a line when every account is ready", async () => {
    server([
      credential(),
      credential({ name: "claude-bo.json", id: "bo", account: "bo@example.com" }),
    ]);
    renderCard();
    const card = await screen.findByRole("region", { name: HEALTH });
    expect(await within(card).findByText("All 2 accounts are ready.")).toBeVisible();
    expect(within(card).queryByRole("list")).toBeNull();
    expect(card).toHaveTextContent(/Checked at \d\d:\d\d:\d\d\./);
  });

  it("says nothing needs attention when the rest are ready or off", async () => {
    server([credential()], {}, [claudeCliEntry({ name: "spare", disabled: true, credential: null })]);
    renderCard();
    const card = await screen.findByRole("region", { name: HEALTH });
    expect(await within(card).findByText("Nothing needs attention: 1 ready, 1 off.")).toBeVisible();
    expect(within(card).queryByRole("list")).toBeNull();
  });

  it("works out once a minute when a resting account is back, without reading it out", async () => {
    // Only the clock and the polling: fetches and waits run as usual.
    vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
    try {
      vi.setSystemTime(new Date("2026-10-07T14:20:00.000Z"));
      // A server without claude-cli entries, which can't be reached after the first read.
      const state = server([credential({ cooldowns: [cooldown("quota", 300)] })], {}, null);
      state.api.use(
        route("GET", AUTH_FILES, () =>
          state.api.callsTo("GET", AUTH_FILES).length > 1
            ? { status: 502, json: { error: "bad gateway" } }
            : { json: credentialList(state.files) },
        ),
      );
      renderCard();
      const card = await screen.findByRole("region", { name: HEALTH });
      const row = await within(card).findByRole("listitem");
      expect(row).toHaveTextContent("Back at 14:25 (in about 5 min)");
      expect(row.closest(NOT_LIVE)).toBeNull();
      const checked = within(card).getByText("Checked at 14:20:00.");
      expect(checked.closest(NOT_LIVE)).toBeNull();

      await act(() => vi.advanceTimersByTimeAsync(60_000));
      // It asked again, and kept what it had when that failed.
      expect(state.api.callsTo("GET", AUTH_FILES).length).toBeGreaterThan(1);
      expect(row).toHaveTextContent("Back at 14:25 (in about 4 min)");
      // So it says when it last read them.
      expect(checked).toHaveTextContent("Checked at 14:20:00.");

      await act(() => vi.advanceTimersByTimeAsync(3 * 60_000));
      expect(row).toHaveTextContent("Back at 14:25 (in about 1 min)");
      expect(row).toBeVisible();
    } finally {
      vi.useRealTimers();
    }
  });

  it("says why it couldn't read the credentials, and tries again when asked", async () => {
    const state = server([credential()]);
    let down = true;
    state.api.use(
      route("GET", AUTH_FILES, () =>
        down ? { status: 500, json: { error: "boom" } } : { json: credentialList(state.files) },
      ),
    );
    const { user } = renderCard();
    const card = await screen.findByRole("region", { name: HEALTH });
    expect(await within(card).findByText("The server failed (HTTP 500)")).toBeVisible();
    expect(within(card).getByRole("link", { name: "Open Credentials" })).toHaveAttribute(
      "href",
      "/credentials",
    );
    down = false;
    await user.click(within(card).getByRole("button", { name: "Try again" }));
    expect(await within(card).findByText("The one account is ready.")).toBeVisible();
  });

  it("shows nothing on a server that serves none of it", async () => {
    const api = mockApi();
    renderCard();
    expect(screen.getByRole("region", { name: HEALTH })).toHaveTextContent("Loading");
    await waitFor(() => {
      expect(screen.queryByRole("region")).toBeNull();
    });
    expect(new Set(api.unhandled.map((call) => call.url.pathname))).toEqual(
      new Set([AUTH_FILES, CLAUDE_CLI_ENTRIES, ...KEY_PROVIDERS.map((p) => KEY_LISTS[p].path)]),
    );
  });
});
