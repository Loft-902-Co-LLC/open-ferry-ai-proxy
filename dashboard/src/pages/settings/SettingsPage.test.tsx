import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { API_KEYS, CONFIG, CONFIG_YAML, MANAGEMENT } from "../../api/management";
import { mockApi, route, type MockApi } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";
import type { YamlEditorProps } from "./YamlEditor";

// CodeMirror needs layout jsdom doesn't have; the end-to-end tests drive the
// real editor in a browser. Here a text area stands in for it.
vi.mock("./YamlEditor", () => ({
  YamlEditor: ({ initial, label, onChange }: YamlEditorProps) => (
    <textarea
      aria-label={label}
      defaultValue={initial}
      onChange={(event) => {
        onChange(event.target.value);
      }}
    />
  ),
}));

const NL = String.fromCharCode(10);
const KEY_A = "sk-test-client-key-aaaa-0001";
const KEY_B = "sk-test-client-key-bbbb-0002";
const YAML = ["port: 8317", "debug: false", "request-retry: 1", ""].join(NL);

interface Server {
  api: MockApi;
  config: Record<string, unknown>;
  keys: string[];
  yaml: string;
}

/** The setting each route changes, by its path under the management API. */
const SETTING_ROUTES = [
  "proxy-url",
  "routing/strategy",
  "request-retry",
  "max-retry-credentials",
  "max-retry-interval",
  "force-model-prefix",
  "debug",
  "logging-to-file",
  "logs-max-total-size-mb",
  "request-log",
  "error-logs-max-files",
  "usage-statistics-enabled",
];

/** A server whose config changes as the management routes change it. */
function server(config: Record<string, unknown> = {}, keys: string[] = [KEY_A, KEY_B]): Server {
  const state: Server = {
    api: mockApi(),
    config: {
      debug: false,
      "proxy-url": "",
      "request-retry": 1,
      "max-retry-credentials": 0,
      "max-retry-interval": 30,
      "force-model-prefix": false,
      "logging-to-file": true,
      "logs-max-total-size-mb": 0,
      "request-log": false,
      "error-logs-max-files": 10,
      "usage-statistics-enabled": true,
      routing: {},
      ...config,
    },
    keys: [...keys],
    yaml: YAML,
  };
  state.api.use(
    route("GET", CONFIG, () => ({ json: { ...state.config, "api-keys": state.keys } })),
    ...SETTING_ROUTES.map((setting) =>
      route("PATCH", `${MANAGEMENT}/${setting}`, (request) => {
        const { value } = request.json() as { value: unknown };
        if (setting === "routing/strategy") {
          state.config.routing = { strategy: value };
        } else {
          state.config[setting] = value;
        }
        return { json: { status: "ok" } };
      }),
    ),
    route("GET", API_KEYS, () => ({ json: { "api-keys": state.keys } })),
    route("PATCH", API_KEYS, (request) => {
      const body = request.json() as { old?: string; new?: string };
      if (body.old !== undefined && body.new !== undefined) {
        const at = state.keys.indexOf(body.old);
        if (at < 0) {
          state.keys.push(body.new);
        } else {
          state.keys[at] = body.new;
        }
      }
      return { json: { status: "ok" } };
    }),
    route("DELETE", API_KEYS, (request) => {
      state.keys.splice(Number(request.url.searchParams.get("index")), 1);
      return { json: { status: "ok" } };
    }),
    route("GET", CONFIG_YAML, () => ({
      text: state.yaml,
      headers: { "content-type": "application/yaml; charset=utf-8" },
    })),
    route("PUT", CONFIG_YAML, (request) => {
      state.yaml = request.body ?? "";
      return { json: { ok: true, changed: ["config"] } };
    }),
  );
  return state;
}

const WRITER_UNAVAILABLE = { status: 503, json: { error: "config writer unavailable" } };

function patches(api: MockApi) {
  return api.calls
    .filter((call) => call.method === "PATCH" && call.url.pathname !== API_KEYS)
    .map((call) => [call.url.pathname.slice(MANAGEMENT.length + 1), call.json()]);
}

/** Replaces what `field` holds with `text`, as pasted. */
async function fill(user: ReturnType<typeof renderApp>["user"], field: HTMLElement, text: string) {
  await user.clear(field);
  await user.paste(text);
}

async function openSettings() {
  const view = renderApp("/settings");
  expect(await screen.findByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
  await screen.findByRole("textbox", { name: "Retries" });
  return view;
}

describe("the settings form", () => {
  it("shows the settings as the server uses them", async () => {
    const { api } = server({
      "proxy-url": "http://user:secret@proxy.example:3128",
      routing: { strategy: "ff" },
      "error-logs-max-files": -1,
    });
    await openSettings();
    expect(screen.getByLabelText("Proxy for outbound requests")).toHaveValue(
      "http://user:secret@proxy.example:3128",
    );
    expect(screen.getByLabelText("Proxy for outbound requests")).toHaveAttribute("type", "password");
    expect(screen.getByRole("combobox", { name: "How credentials are picked" })).toHaveValue(
      "fill-first",
    );
    expect(screen.getByRole("textbox", { name: "Retries" })).toHaveValue("1");
    expect(screen.getByRole("textbox", { name: "Failed-request logs kept" })).toHaveValue("10");
    expect(screen.getByRole("checkbox", { name: "Log to files" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "Debug logging" })).not.toBeChecked();
    expect(screen.getByText("No unsaved changes.")).toBeVisible();
    expect(screen.getByRole("button", { name: "Review and save" })).toBeDisabled();
    expect(api.unhandled).toEqual([]);
  });

  it("saves only what changed, each through its own route, after a review against the server", async () => {
    const state = server();
    const { user } = await openSettings();

    await fill(user, screen.getByRole("textbox", { name: "Retries" }), "3");
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    expect(screen.getByText("2 unsaved changes.")).toBeVisible();

    // Meanwhile, something else changes the server's config.
    state.config["request-retry"] = 2;
    state.config["request-log"] = true;

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    const rows = within(dialog).getAllByRole("row");
    expect(rows).toHaveLength(3);
    expect(rows[1]).toHaveTextContent(/Retries.*request-retry.*Changed on the server.*2 retries.*3 retries/);
    expect(rows[2]).toHaveTextContent(/Debug logging.*debug.*Off.*On/);
    expect(within(dialog).getByText("Some of these changed on the server since the page loaded")).toBeVisible();
    expect(patches(state.api)).toEqual([]);

    await user.click(within(dialog).getByRole("button", { name: "Save 2 settings" }));
    expect(await screen.findByText(/Saved 2 settings/)).toBeVisible();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(patches(state.api)).toEqual([
      ["request-retry", { value: 3 }],
      ["debug", { value: true }],
    ]);
    expect(state.api.callsTo("PUT", CONFIG_YAML)).toEqual([]);
    expect(screen.getByText("No unsaved changes.")).toBeVisible();
    // A setting left alone shows the server's new value.
    await waitFor(() => {
      expect(screen.getByRole("checkbox", { name: "Request logs" })).toBeChecked();
    });
    expect(screen.getByRole("textbox", { name: "Retries" })).toHaveValue("3");
  });

  it("sends a routing strategy and a proxy as the routes take them", async () => {
    const state = server();
    const { user } = await openSettings();
    await user.selectOptions(
      screen.getByRole("combobox", { name: "How credentials are picked" }),
      "weighted-round-robin",
    );
    await fill(user, screen.getByLabelText("Proxy for outbound requests"), "  http://a:b@proxy.example:8080 ");
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    // The proxy's password isn't shown, even in the review.
    expect(dialog).toHaveTextContent("http://•••@proxy.example:8080");
    expect(dialog).not.toHaveTextContent("a:b@");
    await user.click(within(dialog).getByRole("button", { name: "Save 2 settings" }));
    expect(await screen.findByText(/Saved 2 settings/)).toBeVisible();
    expect(patches(state.api)).toEqual([
      ["proxy-url", { value: "http://a:b@proxy.example:8080" }],
      ["routing/strategy", { value: "weighted-round-robin" }],
    ]);
  });

  it("checks each field as it is typed in", async () => {
    const state = server();
    const { user } = await openSettings();
    const retries = screen.getByRole("textbox", { name: "Retries" });
    await fill(user, retries, "two");
    expect(await screen.findByText("Retries is a whole number from 0 to 1,000.")).toBeVisible();
    expect(retries).toHaveAccessibleDescription(/Retries is a whole number from 0 to 1,000\./);
    await fill(user, screen.getByLabelText("Proxy for outbound requests"), "socks5://proxy.example:1080");
    expect(await screen.findByText(/can't use a SOCKS5 proxy yet/)).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(state.api.callsTo("GET", CONFIG)).toHaveLength(1);

    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(retries).toHaveValue("1");
    expect(screen.getByText("No unsaved changes.")).toBeVisible();
    expect(screen.queryByText(/whole number/)).not.toBeInTheDocument();
  });

  it("says there is nothing to save when the server already has the change", async () => {
    const state = server();
    const { user } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    state.config.debug = true;
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(await screen.findByText("Nothing to save: the server already has these values.")).toBeVisible();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByText("No unsaved changes.")).toBeVisible();
    expect(patches(state.api)).toEqual([]);
  });

  it("says this server can't change settings yet when it can't save config.yaml", async () => {
    const state = server();
    state.api.use(route("PATCH", `${MANAGEMENT}/debug`, WRITER_UNAVAILABLE));
    const { user } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await within(dialog).findByText("This server can't change settings yet")).toBeVisible();
    expect(within(dialog).queryByText("Saved some of the changes")).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Debug logging" })).toBeChecked();
  });

  it("says which settings were saved when the server refuses one", async () => {
    const state = server();
    state.api.use(
      route("PATCH", `${MANAGEMENT}/debug`, {
        status: 400,
        json: { error: "invalid body" },
      }),
    );
    const { user } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("checkbox", { name: "Request logs" }));
    await fill(user, screen.getByRole("textbox", { name: "Longest wait for a retry (seconds)" }), "60");
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 3 settings" }));
    const partial = await within(dialog).findByText(/Saved 1 of 3/);
    expect(partial).toHaveTextContent(
      "Saved 1 of 3. “Debug logging” wasn't saved, nor the 1 setting after it.",
    );
    expect(within(dialog).getByText("invalid body")).toBeVisible();
    expect(patches(state.api)).toEqual([
      ["max-retry-interval", { value: 60 }],
      ["debug", { value: true }],
    ]);
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    // The one saved is no longer unsaved; the other two still are.
    expect(screen.getByText("2 unsaved changes.")).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Longest wait for a retry (seconds)" })).toHaveValue("60");
  });
});

describe("the client keys", () => {
  it("lists them masked, and adds one by itself, leaving the others alone", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    expect(card).not.toHaveTextContent(KEY_A);
    expect(within(card).getAllByRole("listitem")).toHaveLength(2);

    await user.click(within(card).getByRole("button", { name: "Add a client key" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a client key" });
    const field = within(dialog).getByLabelText("Client key");
    expect(field).toHaveAttribute("type", "password");
    const made = (field as HTMLInputElement).value;
    expect(made).toMatch(/^sk-[\w-]{43}$/);

    state.keys.push("sk-added-elsewhere-0003");
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(await within(card).findByText("Added the key: the proxy takes it from now on.")).toBeVisible();
    expect(state.api.callsTo("PATCH", API_KEYS).map((call) => call.json())).toEqual([
      { old: made, new: made },
    ]);
    expect(state.keys).toEqual([KEY_A, KEY_B, "sk-added-elsewhere-0003", made]);
    // The key never goes in an address.
    expect(state.api.calls.some((call) => call.url.href.includes(made))).toBe(false);
    await waitFor(() => {
      expect(within(card).getAllByRole("listitem")).toHaveLength(4);
    });
  });

  it("refuses an example key and one already listed", async () => {
    const state = server();
    const { user } = await openSettings();
    await user.click(screen.getByRole("button", { name: "Add a client key" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a client key" });
    const field = within(dialog).getByLabelText("Client key");
    await fill(user, field, "your-api-key-1");
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(await within(dialog).findByText(/one of CLIProxyAPI's examples/)).toBeVisible();
    await fill(user, field, KEY_B);
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(await within(dialog).findByText("That key is already in the list")).toBeVisible();
    expect(state.api.callsTo("PATCH", API_KEYS)).toEqual([]);
  });

  it("removes a key by its place in the list as it is now", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    const remove = within(card).getByRole("button", { name: /^Remove the client key .*0002$/ });
    // Another key arrives ahead of it before the removal.
    state.keys.unshift("sk-added-elsewhere-0003");
    await user.click(remove);
    const confirm = await screen.findByRole("dialog", { name: "Remove this client key?" });
    expect(within(confirm).queryByText("This is the last client key")).not.toBeInTheDocument();
    await user.click(within(confirm).getByRole("button", { name: "Remove" }));
    await waitFor(() => {
      expect(state.keys).toEqual(["sk-added-elsewhere-0003", KEY_A]);
    });
    expect(state.api.callsTo("DELETE", API_KEYS)[0]?.url.search).toBe("?index=2");
  });

  it("warns before removing the last key, as the proxy then lets anyone in", async () => {
    const state = server({}, [KEY_A]);
    const { user } = await openSettings();
    await user.click(screen.getByRole("button", { name: /^Remove the client key/ }));
    const confirm = await screen.findByRole("dialog", { name: "Remove this client key?" });
    expect(within(confirm).getByText("This is the last client key")).toBeVisible();
    await user.click(within(confirm).getByRole("button", { name: "Remove" }));
    const card = screen.getByRole("region", { name: "Client API keys" });
    expect(await within(card).findByText("No client keys")).toBeVisible();
    expect(state.keys).toEqual([]);
  });

  it("says this server can't change settings yet, and stops offering changes", async () => {
    const state = server();
    state.api.use(route("PATCH", API_KEYS, WRITER_UNAVAILABLE));
    const { user } = await openSettings();
    await user.click(screen.getByRole("button", { name: "Add a client key" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a client key" });
    await user.click(within(dialog).getByRole("button", { name: "Add the key" }));
    expect(await within(dialog).findByText("This server can't change settings yet")).toBeVisible();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    const card = screen.getByRole("region", { name: "Client API keys" });
    expect(within(card).getByText(/has no way to save it/)).toBeVisible();
    expect(within(card).queryByRole("button", { name: /^Remove/ })).not.toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Add a client key" })).not.toBeInTheDocument();
  });
});

describe("config.yaml", () => {
  async function openFile(user: ReturnType<typeof renderApp>["user"]) {
    await user.click(screen.getByRole("tab", { name: "config.yaml" }));
    await user.click(screen.getByRole("button", { name: "Show config.yaml" }));
    return screen.findByRole("textbox", { name: "config.yaml" });
  }

  it("shows the file only when asked, as it holds keys", async () => {
    const state = server();
    const { user, router } = await openSettings();
    await user.click(screen.getByRole("tab", { name: "config.yaml" }));
    expect(router.state.location.search).toBe("?tab=file");
    expect(screen.getByText(/holds the client keys and the provider API keys/)).toBeVisible();
    expect(state.api.callsTo("GET", CONFIG_YAML)).toEqual([]);
    await user.click(screen.getByRole("button", { name: "Show config.yaml" }));
    expect(await screen.findByRole("textbox", { name: "config.yaml" })).toHaveValue(YAML);
  });

  it("shows the diff against the file as it is now, then saves the whole file", async () => {
    const state = server();
    const { user } = await openSettings();
    const editor = await openFile(user);
    expect(screen.getByRole("button", { name: "Review changes" })).toBeDisabled();
    const draft = YAML.replace("debug: false", "debug: true");
    fireEvent.change(editor, { target: { value: draft } });
    expect(screen.getByText("Unsaved changes.")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Review changes" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes to config.yaml" });
    expect(within(dialog).queryByText(/changed on the server since you opened it/)).not.toBeInTheDocument();
    const diff = within(dialog).getByRole("region", { name: "The changes to config.yaml" });
    expect(within(diff).getByRole("deletion")).toHaveTextContent("Removed: debug: false");
    expect(within(diff).getByRole("insertion")).toHaveTextContent("Added: debug: true");
    expect(dialog).toHaveTextContent("1 line added and 1 line removed");
    expect(state.api.callsTo("GET", CONFIG_YAML)).toHaveLength(2);

    await user.click(within(dialog).getByRole("button", { name: "Save config.yaml" }));
    expect(await screen.findByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
    const put = state.api.callsTo("PUT", CONFIG_YAML);
    expect(put).toHaveLength(1);
    expect(put[0]?.body).toBe(draft);
    expect(put[0]?.headers.get("content-type")).toBe("application/yaml");
    expect(screen.getByText("No unsaved changes.", { selector: "[role=status]:not([hidden] *)" })).toBeVisible();
  });

  it("warns when the file changed on the server since it was opened", async () => {
    const state = server();
    const { user } = await openSettings();
    const editor = await openFile(user);
    fireEvent.change(editor, { target: { value: `${YAML}# mine${NL}` } });
    state.yaml = YAML.replace("request-retry: 1", "request-retry: 5");
    await user.click(screen.getByRole("button", { name: "Review changes" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes to config.yaml" });
    expect(within(dialog).getByText("config.yaml changed on the server since you opened it")).toBeVisible();
    const diff = within(dialog).getByRole("region", { name: "The changes to config.yaml" });
    expect(within(diff).getByRole("deletion")).toHaveTextContent("request-retry: 5");
    expect(within(diff).getAllByRole("insertion").map((line) => line.textContent)).toEqual([
      "Added: request-retry: 1",
      "Added: # mine",
    ]);
  });

  it("says why the server refused the file, and keeps the edit", async () => {
    const state = server();
    state.api.use(
      route("PUT", CONFIG_YAML, {
        status: 400,
        json: { error: "invalid_yaml", message: "yaml: line 2: mapping values are not allowed here" },
      }),
    );
    const { user } = await openSettings();
    const editor = await openFile(user);
    fireEvent.change(editor, { target: { value: `${YAML}oops: : :${NL}` } });
    await user.click(screen.getByRole("button", { name: "Review changes" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes to config.yaml" });
    await user.click(within(dialog).getByRole("button", { name: "Save config.yaml" }));
    expect(await within(dialog).findByText("That isn't valid YAML")).toBeVisible();
    expect(dialog).toHaveTextContent("mapping values are not allowed here");
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    expect(screen.getByText("Unsaved changes.")).toBeVisible();
  });

  it("says this server can't change settings yet when it can't save the file", async () => {
    const state = server();
    state.api.use(route("PUT", CONFIG_YAML, WRITER_UNAVAILABLE));
    const { user } = await openSettings();
    const editor = await openFile(user);
    fireEvent.change(editor, { target: { value: `${YAML}# mine${NL}` } });
    await user.click(screen.getByRole("button", { name: "Review changes" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes to config.yaml" });
    await user.click(within(dialog).getByRole("button", { name: "Save config.yaml" }));
    expect(await within(dialog).findByText("This server can't change settings yet")).toBeVisible();
  });
});

describe("a server without the settings routes", () => {
  it("says it can't change settings yet", async () => {
    mockApi();
    renderApp("/settings");
    await waitFor(() => {
      expect(screen.getAllByText("This server can't change settings yet")).toHaveLength(2);
    });
  });
});
