import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { AUTH_FILES, KEY_LISTS, type Credential } from "../api/credentials";
import { CLIENT_SETUP, type ClientSetup } from "../api/dashboard";
import { API_KEYS } from "../api/management";
import { EXAMPLE_API_KEYS } from "../app/safeMode";
import { clientSetup, cooldown, credential, credentialList } from "../test/fixtures";
import { mockApi, route, type MockApi } from "../test/mockApi";
import { renderApp } from "../test/renderApp";

const KEY = "sk-test-client-key-0001";
const OTHER_KEY = "sk-test-client-key-0002";
const PYTHON = "The OpenAI SDK (Python) setup";
const NEW_KEY = /^sk-[A-Za-z0-9_-]{43}$/;

/** The key list routes, as GET answers them when they are empty. */
const NO_PROVIDER_KEYS = Object.values(KEY_LISTS).map(({ list, path }) =>
  route("GET", path, { json: { [list]: [] } }),
);

interface Server {
  api: MockApi;
  /** The server's client keys, as the mock holds them now. */
  keys: string[];
  setup: ClientSetup;
}

/**
 * A server with `keys` and `setup`, whose key list changes as PATCH, DELETE
 * and PUT change it, and which leaves safe mode once no example key is left.
 */
function server(
  keys: string[],
  setup: Partial<ClientSetup> = {},
  credentials: Credential[] = [credential()],
): Server {
  const state: Server = { api: mockApi(), keys: [...keys], setup: clientSetup(setup) };
  state.api.use(
    route("GET", AUTH_FILES, { json: credentialList(credentials) }),
    ...NO_PROVIDER_KEYS,
    route("GET", CLIENT_SETUP, () => ({
      json: {
        ...state.setup,
        safe_mode: state.setup.safe_mode && state.keys.some((key) => EXAMPLE_API_KEYS.includes(key)),
      },
    })),
    route("GET", API_KEYS, () => ({ json: { "api-keys": state.keys } })),
    route("PATCH", API_KEYS, (request) => {
      const body = request.json() as { old: string; new: string };
      const index = state.keys.indexOf(body.old);
      if (index < 0) {
        state.keys.push(body.new);
      } else {
        state.keys[index] = body.new;
      }
      return { json: { status: "ok" } };
    }),
    route("DELETE", API_KEYS, (request) => {
      state.keys.splice(Number(request.url.searchParams.get("index")), 1);
      return { json: { status: "ok" } };
    }),
    route("PUT", API_KEYS, (request) => {
      state.keys = request.json() as string[];
      return { json: { status: "ok" } };
    }),
  );
  return state;
}

async function setupCode(name = PYTHON): Promise<HTMLElement> {
  return screen.findByLabelText(name);
}

describe("connecting a client", () => {
  it("fills the setups in with this page's address, the key masked and a model", async () => {
    const { api } = server([KEY]);
    renderApp("/");
    const python = await setupCode();
    expect(python).toHaveTextContent(`base_url="http://localhost:3000/v1"`);
    expect(python).toHaveTextContent(`api_key="sk-...0001"`);
    expect(python).toHaveTextContent(`model="claude-sonnet-4-5"`);
    expect(python).not.toHaveTextContent(KEY);
    expect(screen.getByRole("link", { name: "https://github.com/openai/openai-python#usage" })).toBeVisible();
    expect(api.unhandled).toEqual([]);
  });

  it("copies the whole key, and shows it when asked", async () => {
    server([KEY]);
    const { user } = renderApp("/");
    await setupCode();
    await user.click(screen.getByRole("button", { name: `Copy the OpenAI SDK (Python) setup` }));
    expect(await navigator.clipboard.readText()).toContain(`api_key="${KEY}"`);

    await user.click(screen.getByRole("checkbox", { name: /Show the key in the setups/ }));
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`api_key="${KEY}"`);
  });

  it("offers the server's own addresses too, and warns of one only this computer reaches", async () => {
    server([KEY]);
    const { user } = renderApp("/");
    await setupCode();
    const address = screen.getByLabelText("Address");
    expect(
      within(address)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual([
      "http://localhost:3000 (this page's address)",
      "http://127.0.0.1:8317 (the server's listen address)",
      "http://localhost:8317 (the server's listen address)",
      "https://proxy.example.com (remote-management.base-url)",
    ]);
    expect(screen.getByText("This address works only on the computer the proxy runs on.")).toBeVisible();

    await user.selectOptions(address, "https://proxy.example.com");
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`base_url="https://proxy.example.com/v1"`);
    expect(
      screen.queryByText("This address works only on the computer the proxy runs on."),
    ).not.toBeInTheDocument();
  });

  it("writes the terminal setups for the shell picked", async () => {
    server([KEY]);
    const { user } = renderApp("/");
    await setupCode();
    await user.click(screen.getByRole("tab", { name: "Claude Code" }));

    await user.selectOptions(screen.getByLabelText("Shell"), "powershell");
    const claude = screen.getByLabelText("The Claude Code setup");
    expect(claude).toHaveTextContent("$env:ANTHROPIC_BASE_URL = 'http://localhost:3000'");
    expect(claude).toHaveTextContent("$env:ANTHROPIC_MODEL = 'claude-sonnet-4-5'");

    await user.selectOptions(screen.getByLabelText("Shell"), "posix");
    expect(screen.getByLabelText("The Claude Code setup")).toHaveTextContent(
      "export ANTHROPIC_BASE_URL='http://localhost:3000'",
    );
  });

  it("says when the model picked isn't on a setup's route", async () => {
    const base = clientSetup();
    server([KEY], {
      routes: base.routes.map((proxyRoute) =>
        proxyRoute.protocol === "claude" ? { ...proxyRoute, models: ["claude-sonnet-4-5"] } : proxyRoute,
      ),
    });
    const { user } = renderApp("/");
    await setupCode();
    await user.selectOptions(screen.getByLabelText("Model"), "gpt-5.1-codex");
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`model="gpt-5.1-codex"`);

    await user.click(screen.getByRole("tab", { name: "Claude Code" }));
    expect(
      screen.getByText((_, element) =>
        element?.tagName === "P" &&
        element.textContent ===
          "The model you picked isn't on /v1/messages right now, so this setup names claude-sonnet-4-5.",
      ),
    ).toBeVisible();
    expect(screen.getByLabelText("The Claude Code setup")).toHaveTextContent(
      "ANTHROPIC_MODEL='claude-sonnet-4-5'",
    );
  });

  it("says when the proxy has no models yet", async () => {
    server([KEY], {
      models: [],
      routes: clientSetup().routes.map((proxyRoute) => ({ ...proxyRoute, models: [] })),
    });
    renderApp("/");
    expect(await screen.findByText("No models yet", { selector: "p" })).toBeVisible();
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`model="<model>"`);
    expect(screen.getByLabelText("Model")).toBeDisabled();
  });
});

describe("making a client key", () => {
  it("adds a new key without touching the others, and uses it", async () => {
    const state = server([KEY]);
    const { user } = renderApp("/");
    await setupCode();
    await user.click(screen.getByRole("button", { name: "Make a new key" }));

    expect(await screen.findByText(/^Added a client key\./)).toBeVisible();
    const patch = state.api.callsTo("PATCH", API_KEYS);
    expect(patch).toHaveLength(1);
    const body = patch[0]?.json() as { old: string; new: string };
    expect(body.new).toMatch(NEW_KEY);
    expect(body.old).toBe(body.new);
    expect(patch[0]?.url.search).toBe("");
    expect(state.keys).toEqual([KEY, body.new]);
    expect(state.api.callsTo("PUT", API_KEYS)).toEqual([]);

    await waitFor(() => {
      expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`api_key="sk-...${body.new.slice(-4)}"`);
    });
    const select = screen.getByLabelText("Client key");
    expect(within(select).getAllByRole("option")).toHaveLength(2);
    await user.click(screen.getByRole("button", { name: `Copy the OpenAI SDK (Python) setup` }));
    expect(await navigator.clipboard.readText()).toContain(`api_key="${body.new}"`);
  });

  it("gives the key to add by hand when the server can't change settings", async () => {
    const state = server([KEY]);
    state.api.use(route("PATCH", API_KEYS, { status: 404 }));
    const { user } = renderApp("/");
    await setupCode();
    await user.click(screen.getByRole("button", { name: "Make a new key" }));

    expect(await screen.findByText("This server can't change settings yet")).toBeVisible();
    const select = screen.getByLabelText("Client key");
    const made = within(select).getAllByRole("option").at(-1);
    expect(made).toHaveTextContent(/\(made here, not saved\)$/);
    expect(select).toHaveValue("1");

    await user.click(screen.getByRole("button", { name: "Copy the new key" }));
    const key = await navigator.clipboard.readText();
    expect(key).toMatch(NEW_KEY);
    expect(state.keys).toEqual([KEY]);
  });

  it("works without the key list on a server that can't give it", async () => {
    const api = mockApi(route("GET", CLIENT_SETUP, { json: clientSetup() }));
    renderApp("/");
    expect(await screen.findByText("This server can't list its client keys yet")).toBeVisible();
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`api_key="<your client key>"`);
    expect(screen.getByText("No key")).toBeVisible();
    expect(screen.getByLabelText("Client key")).toBeDisabled();
    expect(new Set(api.unhandled.map((call) => call.url.pathname))).toEqual(
      new Set([API_KEYS, AUTH_FILES, ...Object.values(KEY_LISTS).map(({ path }) => path)]),
    );
    expect(screen.queryByRole("heading", { name: "Providers" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Connect a provider" })).toBeNull();
  });
});

describe("the providers card", () => {
  it("offers the ways to connect one on a first run", async () => {
    const { api } = server([KEY], {}, []);
    renderApp("/");
    const card = await screen.findByRole("region", { name: "Connect a provider" });
    expect(within(card).getByRole("link", { name: "Sign in with Claude" })).toHaveAttribute(
      "href",
      "/credentials?start=claude",
    );
    expect(within(card).getByRole("link", { name: "Sign in with Codex" })).toHaveAttribute(
      "href",
      "/credentials?start=codex",
    );
    expect(within(card).getByRole("link", { name: "Add a provider API key" })).toHaveAttribute(
      "href",
      "/credentials?start=key",
    );
    expect(within(card).getByRole("link", { name: "Upload a credential file" })).toHaveAttribute(
      "href",
      "/credentials",
    );
    expect(api.unhandled).toEqual([]);
  });

  it("counts provider keys as connected", async () => {
    const { api } = server([KEY], {}, []);
    api.use(
      route("GET", KEY_LISTS.gemini.path, {
        json: { "gemini-api-key": [{ "api-key": "AIzaSy-test-gemini-key-0001" }] },
      }),
    );
    renderApp("/");
    expect(
      await screen.findByText("0 sign-ins and credential files, 1 provider API key."),
    ).toBeVisible();
    const card = screen.getByRole("region", { name: "Providers" });
    expect(card).not.toHaveTextContent("AIzaSy");
  });

  it("tallies the credentials by health, and says when some need attention", async () => {
    server([KEY], {}, [
      credential(),
      credential({ name: "codex-bob.json", provider: "codex", cooldowns: [cooldown("quota", 300)] }),
      credential({ name: "claude-cy.json", status: "error", status_message: "unauthorized" }),
      credential({ name: "claude-dee.json", disabled: true, status: "disabled" }),
    ]);
    renderApp("/");
    expect(
      await screen.findByText("4 sign-ins and credential files, 0 provider API keys."),
    ).toBeVisible();
    const card = screen.getByRole("region", { name: "Providers" });
    const tally = within(card).getByRole("list", { name: "Credential health" });
    expect(
      within(tally)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual(["1 ready", "1 resting", "1 failing", "1 off"]);
    expect(card).toHaveTextContent("2 need attention");
    expect(within(card).getByRole("link", { name: "Open Credentials" })).toHaveAttribute(
      "href",
      "/credentials",
    );
  });
});

describe("safe mode", () => {
  it("replaces the example keys with a new one", async () => {
    const state = server([...EXAMPLE_API_KEYS], { safe_mode: true });
    const { user } = renderApp("/");
    const notice = (await screen.findByText("The proxy is in safe mode")).parentElement;
    expect(notice).toHaveTextContent(
      "examples (your-api-key-1, your-api-key-2, your-api-key-3), which anyone could guess",
    );

    await user.click(screen.getByRole("button", { name: "Replace the example keys with a new key" }));
    await waitFor(() => {
      expect(screen.queryByText("The proxy is in safe mode")).not.toBeInTheDocument();
    });
    // The new key takes the first example's place before any example goes,
    // so the list is never empty; then the others go one at a time.
    const patch = state.api.callsTo("PATCH", API_KEYS);
    expect(patch).toHaveLength(1);
    const body = patch[0]?.json() as { old: string; new: string };
    expect(body.old).toBe("your-api-key-1");
    expect(body.new).toMatch(NEW_KEY);
    expect(state.api.callsTo("DELETE", API_KEYS).map((call) => call.url.search)).toEqual([
      "?index=2",
      "?index=1",
    ]);
    expect(state.keys).toEqual([body.new]);
    expect(state.api.callsTo("PUT", API_KEYS)).toEqual([]);
    await waitFor(() => {
      expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`api_key="sk-...${body.new.slice(-4)}"`);
    });
  });

  it("removes the examples, keeping the keys of the user's own", async () => {
    const state = server(["your-api-key-1", KEY, "your-api-key-2"], { safe_mode: true });
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    // A key added elsewhere after the page loaded stays.
    state.keys.push(OTHER_KEY);
    await user.click(screen.getByRole("button", { name: "Remove the example keys" }));
    await waitFor(() => {
      expect(state.keys).toEqual([KEY, OTHER_KEY]);
    });
    expect(state.api.callsTo("DELETE", API_KEYS).map((call) => call.url.search)).toEqual([
      "?index=2",
      "?index=0",
    ]);
    expect(state.api.callsTo("PATCH", API_KEYS)).toEqual([]);
    expect(state.api.callsTo("PUT", API_KEYS)).toEqual([]);
  });

  it("says it's saved while the proxy has yet to reload its config", async () => {
    const state = server([...EXAMPLE_API_KEYS], { safe_mode: true });
    // A server that hasn't reloaded config.yaml yet: still in safe mode.
    state.api.use(route("GET", CLIENT_SETUP, { json: clientSetup({ safe_mode: true }) }));
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    await user.click(screen.getByRole("button", { name: "Replace the example keys with a new key" }));
    expect(
      await screen.findByText(/Saved\. The proxy leaves safe mode when it reloads config\.yaml/),
    ).toBeVisible();
  });

  it("gives the key to put in by hand when the server can't change settings", async () => {
    const state = server([...EXAMPLE_API_KEYS], { safe_mode: true });
    state.api.use(
      route("PATCH", API_KEYS, { status: 503, json: { error: "config writer unavailable" } }),
    );
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    await user.click(screen.getByRole("button", { name: "Replace the example keys with a new key" }));
    const notice = (await screen.findByText("This server can't change settings yet")).parentElement;
    expect(notice).toHaveTextContent("In config.yaml, replace the example keys under api-keys");
    expect(state.api.callsTo("DELETE", API_KEYS)).toEqual([]);
    expect(state.keys).toEqual([...EXAMPLE_API_KEYS]);
  });

  it("says so when the server can't remove the examples", async () => {
    const state = server(["your-api-key-1", KEY], { safe_mode: true });
    state.api.use(route("DELETE", API_KEYS, { status: 404 }));
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    await user.click(screen.getByRole("button", { name: "Remove the example keys" }));
    const notice = (await screen.findByText("This server can't change settings yet")).parentElement;
    expect(notice).toHaveTextContent("Edit config.yaml by hand");
    expect(state.keys).toEqual(["your-api-key-1", KEY]);
  });

  it("takes the user to the key setup when sent from the safe-mode page", async () => {
    server([...EXAMPLE_API_KEYS], { safe_mode: true });
    renderApp("/?safe-mode=configure");
    const button = await screen.findByRole("button", {
      name: "Replace the example keys with a new key",
    });
    await waitFor(() => {
      expect(button).toHaveFocus();
    });
  });

  it("says so when sent from the safe-mode page after it has lifted", async () => {
    server([KEY]);
    renderApp("/?safe-mode=configure");
    expect(await screen.findByText("The proxy isn't in safe mode")).toBeVisible();
    await waitFor(() => {
      expect(screen.getByLabelText("Client key")).toHaveFocus();
    });
  });
});
