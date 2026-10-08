import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { AUTH_FILES, KEY_LISTS, type Credential } from "../api/credentials";
import {
  CLAUDE_CLI_ENTRIES,
  CLIENT_SETUP,
  USAGE_LEDGER,
  USAGE_SUMMARY,
  type ClientSetup,
} from "../api/dashboard";
import { API_KEYS, USAGE_STATISTICS_ENABLED } from "../api/management";
import { EXAMPLE_API_KEYS } from "../app/safeMode";
import { startOfDay } from "../lib/timeRange";
import {
  clientSetup,
  credential,
  credentialList,
  ledger,
  metrics,
  summary,
  unavailableLedger,
} from "../test/fixtures";
import { mockApi, route, type MockApi } from "../test/mockApi";
import { renderApp } from "../test/renderApp";

const KEY = "sk-test-client-key-0001";
const OTHER_KEY = "sk-test-client-key-0002";
const PYTHON = "The OpenAI SDK (Python) setup";
const NEW_KEY = /^sk-[A-Za-z0-9_-]{43}$/;
/** A ledger that records, and holds no call yet: the proxy isn't set up. */
const NO_CALLS = ledger({ rows: 0, oldest: null, newest: null });
/** A ledger with calls in it: the proxy is set up. */
const SOME_CALLS = ledger();

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
 * Its usage ledger holds no call, so the page shows the setup first.
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
    route("GET", CLAUDE_CLI_ENTRIES, { json: { entries: [] } }),
    route("GET", USAGE_LEDGER, { json: NO_CALLS }),
    route("GET", USAGE_SUMMARY, { json: summary() }),
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

/** The same server, set up: a client has called it. */
function setUpServer(keys: string[] = [KEY], setup: Partial<ClientSetup> = {}): Server {
  const state = server(keys, setup);
  state.api.use(route("GET", USAGE_LEDGER, { json: SOME_CALLS }));
  return state;
}

async function setupCode(name = PYTHON): Promise<HTMLElement> {
  return screen.findByLabelText(name);
}

/** Shows the address, model and shell, which start hidden. */
async function openChoices(user: ReturnType<typeof renderApp>["user"]): Promise<void> {
  await user.click(screen.getByRole("button", { name: "Address, model and shell", expanded: false }));
}

/** The names of the page's cards, in order. */
function cardTitles(): string[] {
  return screen.getAllByRole("heading", { level: 2 }).map((heading) => heading.textContent);
}

describe("connecting a client", () => {
  it("fills the setups in with this page's address, the key masked and a model", async () => {
    const { api } = server([KEY]);
    renderApp("/");
    const python = await setupCode();
    // A region, so screen readers read its name.
    expect(screen.getByRole("region", { name: PYTHON })).toBe(python);
    expect(python).toHaveTextContent(`base_url="http://localhost:3000/v1"`);
    expect(python).toHaveTextContent(`api_key="sk-...0001"`);
    // The newest model; Claude Code's is Anthropic's.
    expect(python).toHaveTextContent(`model="gpt-5.1-codex"`);
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
    await openChoices(user);
    const address = screen.getByLabelText("Address");
    expect(
      within(address)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual([
      "http://localhost:3000 (this page's address)",
      "http://127.0.0.1:8317 (the server's listen address)",
      "http://localhost:8317 (the server's listen address)",
      "https://proxy.example.com (management.base-url)",
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
    await openChoices(user);

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
    await openChoices(user);
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

  it("suggests a model for each setup until one is picked", async () => {
    server([KEY]);
    const { user } = renderApp("/");
    const python = await setupCode();
    await openChoices(user);
    const model = screen.getByLabelText("Model");
    expect(model).toHaveValue("");
    expect(model).toHaveAccessibleDescription(/newest chat model/);
    expect(
      within(model)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual([
      "Suggested for each setup",
      "Claude Sonnet 4.5 (claude-sonnet-4-5)",
      "GPT 5.1 Codex (gpt-5.1-codex)",
    ]);
    expect(python).toHaveTextContent(`model="gpt-5.1-codex"`);

    await user.selectOptions(model, "claude-sonnet-4-5");
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`model="claude-sonnet-4-5"`);
    expect(model).not.toHaveAccessibleDescription(/newest chat model/);
    await user.click(screen.getByRole("tab", { name: "Codex CLI" }));
    expect(screen.getByLabelText("The Codex CLI setup, step 1")).toHaveTextContent(
      `model = "claude-sonnet-4-5"`,
    );

    await user.selectOptions(model, "");
    expect(screen.getByLabelText("The Codex CLI setup, step 1")).toHaveTextContent(
      `model = "gpt-5.1-codex"`,
    );
    await user.click(screen.getByRole("tab", { name: "Claude Code" }));
    expect(screen.getByLabelText("The Claude Code setup")).toHaveTextContent(
      "ANTHROPIC_MODEL='claude-sonnet-4-5'",
    );
  });

  it("says when the proxy has no models yet", async () => {
    server([KEY], {
      models: [],
      routes: clientSetup().routes.map((proxyRoute) => ({ ...proxyRoute, models: [] })),
    });
    const { user } = renderApp("/");
    expect(await screen.findByText("No models yet", { selector: "p" })).toBeVisible();
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`model="<model>"`);
    await openChoices(user);
    expect(screen.getByLabelText("Model")).toBeDisabled();
  });
});

describe("making a client key", () => {
  it("adds a new key without touching the others, and uses it", async () => {
    const state = server([KEY]);
    const { user } = renderApp("/");
    await setupCode();
    const make = screen.getByRole("button", { name: "Make a new key" });
    expect(make).toHaveAccessibleDescription(
      "A new key is saved to config.yaml as soon as it's made.",
    );
    await user.click(make);

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

  it("says when the server can't save the new key", async () => {
    const state = server([KEY]);
    state.api.use(
      route("PATCH", API_KEYS, { status: 503, json: { error: "config writer unavailable" } }),
    );
    const { user } = renderApp("/");
    await setupCode();
    await user.click(screen.getByRole("button", { name: "Make a new key" }));

    const notice = (await screen.findByText("This server can't save config.yaml")).parentElement;
    expect(notice).toHaveTextContent("nothing was changed");
    expect(notice).not.toHaveTextContent(/by hand/);
    expect(screen.queryByText(/^Added a client key\./)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Copy the new key" })).toBeNull();
    const select = screen.getByLabelText("Client key");
    const made = within(select).getAllByRole("option").at(-1);
    expect(made).toHaveTextContent(/\(made here, not saved\)$/);
    expect(state.keys).toEqual([KEY]);
  });

  it("works without the key list on a server that can't give it", async () => {
    const api = mockApi(route("GET", CLIENT_SETUP, { json: clientSetup() }));
    renderApp("/");
    expect(await screen.findByText("This server doesn't list its client keys")).toBeVisible();
    expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`api_key="<your client key>"`);
    expect(screen.getByText("No key")).toBeVisible();
    expect(screen.getByLabelText("Client key")).toBeDisabled();
    expect(new Set(api.unhandled.map((call) => call.url.pathname))).toEqual(
      new Set([
        API_KEYS,
        AUTH_FILES,
        CLAUDE_CLI_ENTRIES,
        USAGE_LEDGER,
        ...Object.values(KEY_LISTS).map(({ path }) => path),
      ]),
    );
    expect(screen.queryByRole("heading", { name: "Account health" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Connect a provider" })).toBeNull();
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
    const lifted = (await screen.findByText("The proxy is out of safe mode")).parentElement;
    expect(lifted).toHaveTextContent("A new key took the example keys' place in config.yaml");
    await waitFor(() => {
      expect(screen.getByLabelText(PYTHON)).toHaveTextContent(`api_key="sk-...${body.new.slice(-4)}"`);
    });
  });

  it("deletes the examples, keeping the keys of the user's own", async () => {
    const state = server(["your-api-key-1", KEY, "your-api-key-2"], { safe_mode: true });
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    // A key added elsewhere after the page loaded stays.
    state.keys.push(OTHER_KEY);
    await user.click(screen.getByRole("button", { name: "Delete the example keys" }));
    await waitFor(() => {
      expect(state.keys).toEqual([KEY, OTHER_KEY]);
    });
    expect(state.api.callsTo("DELETE", API_KEYS).map((call) => call.url.search)).toEqual([
      "?index=2",
      "?index=0",
    ]);
    expect(state.api.callsTo("PATCH", API_KEYS)).toEqual([]);
    expect(state.api.callsTo("PUT", API_KEYS)).toEqual([]);
    const lifted = (await screen.findByText("The proxy is out of safe mode")).parentElement;
    expect(lifted).toHaveTextContent("The example keys are gone from config.yaml");
  });

  it("says it's saved while the proxy has yet to leave safe mode", async () => {
    const state = server([...EXAMPLE_API_KEYS], { safe_mode: true });
    // A server still in safe mode after the keys are saved.
    state.api.use(route("GET", CLIENT_SETUP, { json: clientSetup({ safe_mode: true }) }));
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    await user.click(screen.getByRole("button", { name: "Replace the example keys with a new key" }));
    expect(
      await screen.findByText(/Saved\. Waiting for the proxy to load the new keys and leave safe mode/),
    ).toBeVisible();
    expect(screen.queryByText("The proxy is out of safe mode")).not.toBeInTheDocument();
  });

  it("says when the server can't save the new key in safe mode", async () => {
    const state = server([...EXAMPLE_API_KEYS], { safe_mode: true });
    state.api.use(
      route("PATCH", API_KEYS, { status: 503, json: { error: "config writer unavailable" } }),
    );
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    await user.click(screen.getByRole("button", { name: "Replace the example keys with a new key" }));
    const notice = (await screen.findByText("This server can't save config.yaml")).parentElement;
    expect(notice).toHaveTextContent("nothing was changed");
    expect(notice).not.toHaveTextContent(/by hand/);
    // The user can try again.
    expect(
      screen.getByRole("button", { name: "Replace the example keys with a new key" }),
    ).toBeEnabled();
    expect(state.api.callsTo("DELETE", API_KEYS)).toEqual([]);
    expect(state.keys).toEqual([...EXAMPLE_API_KEYS]);
  });

  it("says so when the server can't delete the examples", async () => {
    const state = server(["your-api-key-1", KEY], { safe_mode: true });
    state.api.use(route("DELETE", API_KEYS, { status: 404 }));
    const { user } = renderApp("/");
    await screen.findByText("The proxy is in safe mode");
    await user.click(screen.getByRole("button", { name: "Delete the example keys" }));
    const notice = (await screen.findByText("This server can't save config.yaml")).parentElement;
    expect(notice).toHaveTextContent("nothing was changed");
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

describe("the page's layout", () => {
  it("leads with the setup, open, while no client has called", async () => {
    const { api } = server([KEY]);
    renderApp("/");
    await setupCode();
    expect(screen.getByText("Connect providers and clients to this proxy.")).toBeVisible();
    const titles = cardTitles();
    expect(titles).not.toContain("Today");
    expect(titles.at(-1)).toBe("Connect a client");
    // Open, with nothing to close it with.
    expect(screen.queryByRole("button", { name: "Connect a client" })).toBeNull();
    expect(api.callsTo("GET", USAGE_SUMMARY)).toEqual([]);
  });

  it("leads with the setup on a first run, whatever the ledger holds", async () => {
    const { api } = server([KEY], {}, []);
    api.use(route("GET", USAGE_LEDGER, { json: SOME_CALLS }));
    renderApp("/");
    expect(await screen.findByRole("heading", { name: "Connect a provider" })).toBeVisible();
    expect(cardTitles()).toEqual(["Connect a provider", "Connect a client"]);
  });

  it("keeps the client setup closed as the next step until a provider is connected", async () => {
    const { api } = server([KEY], {}, []);
    const files: Credential[] = [];
    api.use(
      route("GET", AUTH_FILES, () => ({ json: credentialList(files) })),
      route("POST", AUTH_FILES, () => {
        files.push(credential());
        return { json: { status: "ok" } };
      }),
    );
    const { user } = renderApp("/");
    const toggle = await screen.findByRole("button", { name: "Connect a client", expanded: false });
    const card = screen.getByRole("region", { name: "Connect a client" });
    expect(card).toHaveTextContent("The next step, once a provider is connected.");
    expect(screen.queryByLabelText(PYTHON)).toBeNull();

    // It can be opened before, all the same.
    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(await setupCode()).toBeVisible();
    await user.click(toggle);
    expect(screen.queryByLabelText(PYTHON)).toBeNull();

    // A provider connected here opens it, as the step that's next.
    const connect = screen.getByRole("region", { name: "Connect a provider" });
    const file = new File(['{"type":"claude"}'], "claude-new.json", { type: "application/json" });
    await user.upload(within(connect).getByLabelText("Upload credential files"), file);
    expect(await setupCode()).toBeVisible();
    expect(screen.queryByRole("button", { name: "Connect a client" })).toBeNull();
    expect(card).not.toHaveTextContent("The next step");
    expect(screen.getByRole("region", { name: "Account health" })).toBeVisible();
  });

  it("keeps the client setup open on a first run in safe mode", async () => {
    server([...EXAMPLE_API_KEYS], { safe_mode: true }, []);
    renderApp("/");
    expect(await screen.findByText("The proxy is in safe mode")).toBeVisible();
    expect(screen.getByRole("heading", { name: "Connect a provider" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Connect a client" })).toBeNull();
    expect(screen.getByRole("region", { name: "Connect a client" })).not.toHaveTextContent(
      "The next step",
    );
  });

  it("leads with today's calls once a client has called, the setup closed", async () => {
    const { api } = setUpServer();
    renderApp("/");
    expect(await screen.findByText("How the proxy is doing today.")).toBeVisible();
    const titles = cardTitles();
    expect(titles[0]).toBe("Today");
    expect(titles.at(-1)).toBe("Connect a client");
    const toggle = screen.getByRole("button", { name: "Connect a client", expanded: false });
    expect(toggle).not.toHaveAttribute("aria-controls");
    expect(screen.getByRole("region", { name: "Connect a client" })).toHaveTextContent(
      "Ready-made setups for common clients",
    );
    expect(screen.queryByLabelText(PYTHON)).toBeNull();
    await screen.findByText("1,520");
    expect(api.unhandled).toEqual([]);
  });

  it("opens the client setup from its title, and closes it again", async () => {
    setUpServer();
    const { user } = renderApp("/");
    const toggle = await screen.findByRole("button", { name: "Connect a client", expanded: false });
    toggle.focus();
    await user.keyboard("{Enter}");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    const python = await setupCode();
    expect(document.getElementById(toggle.getAttribute("aria-controls") ?? "")).toContainElement(
      python,
    );
    expect(screen.getByRole("button", { name: "Make a new key" })).toBeVisible();

    await user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByLabelText(PYTHON)).toBeNull();
  });

  it("leads with the setup in safe mode, whatever the ledger holds", async () => {
    setUpServer([...EXAMPLE_API_KEYS], { safe_mode: true });
    renderApp("/");
    expect(await screen.findByText("The proxy is in safe mode")).toBeVisible();
    expect(cardTitles()).not.toContain("Today");
  });

  it("opens the setup and takes the user to the key when sent from the safe-mode page", async () => {
    setUpServer();
    renderApp("/?safe-mode=configure");
    expect(await screen.findByText("The proxy isn't in safe mode")).toBeVisible();
    expect(cardTitles()[0]).toBe("Today");
    expect(screen.getByRole("button", { name: "Connect a client", expanded: true })).toBeVisible();
    await waitFor(() => {
      expect(screen.getByLabelText("Client key")).toHaveFocus();
    });
  });

  it("goes by the client keys when the server keeps no usage", async () => {
    const { api } = server([KEY]);
    api.use(route("GET", USAGE_LEDGER, { status: 404 }));
    renderApp("/");
    expect(await screen.findByText("How the proxy is doing today.")).toBeVisible();
    expect(
      screen.getByText("This server doesn't keep usage, so there's none to show."),
    ).toBeVisible();
    expect(api.callsTo("GET", USAGE_SUMMARY)).toEqual([]);
  });

  it("goes by the client keys while recording is off", async () => {
    const { api } = server([KEY]);
    api.use(
      route("GET", USAGE_LEDGER, {
        json: ledger({
          rows: 0,
          oldest: null,
          newest: null,
          recording: false,
          usage_statistics_enabled: false,
        }),
      }),
    );
    renderApp("/");
    expect(await screen.findByText("How the proxy is doing today.")).toBeVisible();
    expect(screen.getByText("Usage isn't being recorded")).toBeVisible();
  });

  it("stays as it is while the user makes the first client key", async () => {
    const { api } = server([]);
    api.use(route("GET", USAGE_LEDGER, { status: 404 }));
    const { user } = renderApp("/");
    await setupCode();
    expect(screen.getByText("Connect providers and clients to this proxy.")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Make a new key" }));
    expect(await screen.findByText(/^Added a client key\./)).toBeVisible();
    // With a key, the next visit leads with health; this one doesn't move.
    expect(cardTitles()).not.toContain("Today");
    expect(screen.getByLabelText(PYTHON)).toBeVisible();
  });
});

describe("today's calls", () => {
  it("counts the calls since midnight, the failed ones and their cost", async () => {
    const { api } = setUpServer();
    renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    await within(today).findByText("1,520");
    expect(
      within(today)
        .getAllByRole("term")
        .map((term) => term.textContent),
    ).toEqual(["Requests", "Failed", "Cost"]);
    expect(
      within(today)
        .getAllByRole("definition")
        .map((definition) => definition.textContent),
    ).toEqual([
      "1,520",
      "12 (0.8% of requests) See today's failed calls",
      "4.18 USD, an estimate from the prices you set",
    ]);
    expect(
      within(today).getByRole("link", { name: "See today's failed calls" }),
    ).toHaveAttribute("href", "/usage?range=today&failed=true");
    expect(within(today).getByRole("link", { name: "Open Usage" })).toHaveAttribute(
      "href",
      "/usage",
    );
    const from = api.callsTo("GET", USAGE_SUMMARY)[0]?.url.searchParams.get("from");
    expect(from).toBe(startOfDay(Date.now()));
  });

  it("says how many calls have no price", async () => {
    const { api } = setUpServer();
    api.use(
      route("GET", USAGE_SUMMARY, {
        json: summary({ totals: metrics({ errors: 0, unpriced_requests: 3 }) }),
      }),
    );
    renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    expect(await within(today).findByText(/an estimate from the prices you set/)).toHaveTextContent(
      "4.18 USD, an estimate from the prices you set; 3 calls have no price",
    );
    expect(
      within(today).queryByRole("link", { name: "See today's failed calls" }),
    ).toBeNull();
  });

  it("leaves the cost out while no price is set", async () => {
    const { api } = setUpServer();
    api.use(route("GET", USAGE_SUMMARY, { json: summary({ totals: metrics({ cost: null }) }) }));
    renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    await within(today).findByText("1,520");
    expect(
      within(today)
        .getAllByRole("term")
        .map((term) => term.textContent),
    ).toEqual(["Requests", "Failed"]);
  });

  it("says when there have been no calls yet today", async () => {
    const { api } = setUpServer();
    api.use(
      route("GET", USAGE_SUMMARY, {
        json: summary({ totals: metrics({ requests: 0, errors: 0, cost: 0 }) }),
      }),
    );
    renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    expect(await within(today).findByText(/^No calls yet today\. The last was /)).toBeVisible();
    expect(within(today).queryByRole("term")).toBeNull();
  });

  it("says it's loading today's numbers", async () => {
    const { api } = setUpServer();
    api.use(
      route(
        "GET",
        USAGE_SUMMARY,
        () =>
          new Promise<never>(() => {
            // The answer never comes.
          }),
      ),
    );
    renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    expect(within(today).getByRole("status")).toHaveTextContent("Loading today's usage…");
  });

  it("says when today's numbers can't be read, and tries again", async () => {
    const { api } = setUpServer();
    api.use(route("GET", USAGE_SUMMARY, { status: 500, json: { error: "disk full" } }));
    const { user } = renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    expect(await within(today).findByText("The server failed (HTTP 500)")).toBeVisible();

    api.use(route("GET", USAGE_SUMMARY, { json: summary() }));
    await user.click(within(today).getByRole("button", { name: "Try again" }));
    expect(await within(today).findByText("1,520")).toBeVisible();
  });

  it("says when the ledger can't be read, and tries again", async () => {
    const { api } = setUpServer();
    api.use(route("GET", USAGE_LEDGER, { status: 500, json: { error: "disk full" } }));
    const { user } = renderApp("/");
    // The client key stands in for a call: there's a key to call with.
    const today = await screen.findByRole("region", { name: "Today" });
    expect(await within(today).findByText("The server failed (HTTP 500)")).toBeVisible();
    expect(api.callsTo("GET", USAGE_SUMMARY)).toEqual([]);

    api.use(route("GET", USAGE_LEDGER, { json: SOME_CALLS }));
    await user.click(within(today).getByRole("button", { name: "Try again" }));
    expect(await within(today).findByText("1,520")).toBeVisible();
  });

  it("says when the ledger couldn't be opened", async () => {
    const { api } = setUpServer();
    api.use(route("GET", USAGE_LEDGER, { json: unavailableLedger("disk full") }));
    renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    expect(within(today).getByText("The usage ledger couldn't be opened")).toBeVisible();
    expect(api.callsTo("GET", USAGE_SUMMARY)).toEqual([]);
  });

  it("says when usage isn't recorded, and turns recording on", async () => {
    const { api } = setUpServer();
    let enabled = false;
    api.use(
      route("GET", USAGE_LEDGER, () => ({
        json: ledger({ recording: enabled, usage_statistics_enabled: enabled }),
      })),
      route("PUT", USAGE_STATISTICS_ENABLED, () => {
        enabled = true;
        return { json: { status: "ok" } };
      }),
    );
    const { user } = renderApp("/");
    const today = await screen.findByRole("region", { name: "Today" });
    const notice = (await within(today).findByText("Usage isn't being recorded")).parentElement;
    expect(notice).toHaveTextContent("Usage statistics is off, so new calls aren't counted.");
    expect(within(today).getByRole("link", { name: "Settings" })).toHaveAttribute(
      "href",
      "/settings",
    );
    // Calls recorded before it went off still count.
    expect(await within(today).findByText("1,520")).toBeVisible();

    await user.click(within(today).getByRole("button", { name: "Start recording" }));
    await waitFor(() => {
      expect(within(today).queryByText("Usage isn't being recorded")).not.toBeInTheDocument();
    });
    expect(api.callsTo("PUT", USAGE_STATISTICS_ENABLED)[0]?.json()).toEqual({ value: true });
  });
});
