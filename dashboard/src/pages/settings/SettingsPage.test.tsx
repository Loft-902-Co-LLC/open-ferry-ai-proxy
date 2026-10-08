import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { readStoredKey } from "../../api/keyStorage";
import { API_KEYS, CONFIG, CONFIG_YAML, MANAGEMENT, V8_CONFIG } from "../../api/management";
import { quotaRouting, v8Config } from "../../test/fixtures";
import { loadFirst } from "../../test/loadFirst";
import { mockApi, route, v8ConfigRoutes, type MockApi } from "../../test/mockApi";
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
  /** config.yaml in the v8 layout, as the v8 config route reads it; its `routing` is the config's. */
  v8: Record<string, unknown>;
  keys: string[];
  yaml: string;
}

/** The v8 config paths the Settings tab reads or writes. */
const V8_PATHS = [
  "management/separate-address",
  "management/allow-remote",
  "server/port",
  "routing/quota/prefer",
  "routing/quota/reserve-percent",
  "routing/quota/check-after",
];

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

/**
 * A server whose config changes as the management routes change it, with
 * `file` the parts of config.yaml only the v8 config route reads.
 */
function server(
  config: Record<string, unknown> = {},
  keys: string[] = [KEY_A, KEY_B],
  file: Parameters<typeof v8Config>[0] = {},
): Server {
  const state: Server = {
    api: mockApi(),
    v8: v8Config(file),
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
  // One mapping, so a quota setting written through the v8 route shows in
  // GET /config, and the strategy written through its route in the file.
  const routing = state.config.routing as Record<string, unknown>;
  state.v8.routing = routing;
  state.api.use(
    route("GET", CONFIG, () => ({ json: { ...state.config, "api-keys": state.keys } })),
    ...v8ConfigRoutes(state.v8, V8_PATHS),
    ...SETTING_ROUTES.map((setting) =>
      route("PATCH", `${MANAGEMENT}/${setting}`, (request) => {
        const { value } = request.json() as { value: unknown };
        if (setting === "routing/strategy") {
          routing.strategy = value;
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

/** Each write through the v8 config route: its path, and the value sent. */
function v8Puts(api: MockApi) {
  return api.calls
    .filter((call) => call.method === "PUT" && call.url.pathname.startsWith(`${V8_CONFIG}/`))
    .map((call) => [call.url.pathname.slice(V8_CONFIG.length + 1), call.json()]);
}

/** How many times the tab read the management address. */
function addressReads(api: MockApi) {
  return api.callsTo("GET", V8_CONFIG).length;
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

/**
 * Checks that nothing on the Settings tab is unsaved: the save bar is down
 * to its status line, for screen readers, with no buttons.
 */
function expectNothingUnsaved() {
  expect(screen.getByText("No unsaved changes.")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Review and save" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Discard" })).not.toBeInTheDocument();
}

loadFirst(() => import("./SettingsPage"));

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
    expectNothingUnsaved();
    expect(api.unhandled).toEqual([]);
  });

  it("brings the save bar up with the first change, clear of the field changed", async () => {
    server();
    const { user } = await openSettings();
    expectNothingUnsaved();
    const debug = screen.getByRole("checkbox", { name: "Debug logging" });
    // Laid out as at the foot of the window, where the bar comes up over it.
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (
      this: HTMLElement,
    ) {
      const top = this === debug ? 680 : this.classList.contains("sticky") ? 650 : 0;
      return { top, bottom: top + 20, left: 0, right: 0, width: 0, height: 20 } as DOMRect;
    });
    const scrollBy = vi.spyOn(window, "scrollBy").mockImplementation(() => undefined);

    await user.click(debug);
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    expect(screen.getByRole("button", { name: "Review and save" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Discard" })).toBeEnabled();
    expect(scrollBy).toHaveBeenCalledExactlyOnceWith({ top: 66 });

    await user.click(debug);
    expectNothingUnsaved();
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
    expectNothingUnsaved();
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
    expect(await screen.findByText("Retries is a whole number, 0 or more.")).toBeVisible();
    expect(retries).toHaveAccessibleDescription(/Retries is a whole number, 0 or more\./);
    await fill(user, screen.getByLabelText("Proxy for outbound requests"), "socks5://proxy.example:1080");
    expect(await screen.findByText(/can't use a SOCKS5 proxy yet/)).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(state.api.callsTo("GET", CONFIG)).toHaveLength(1);

    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(retries).toHaveValue("1");
    expectNothingUnsaved();
    expect(screen.queryByText(/whole number/)).not.toBeInTheDocument();
  });

  it("with request-retry: 5000 in the loaded config, editing Debug saves only debug", async () => {
    const state = server({ "request-retry": 5000 });
    const { user } = await openSettings();
    const retries = screen.getByRole("textbox", { name: "Retries" });
    expect(retries).toHaveValue("5000");
    expect(retries).not.toHaveAttribute("aria-invalid");

    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    expect(within(dialog).getAllByRole("row")).toHaveLength(2);
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting\./)).toBeVisible();
    expect(patches(state.api)).toEqual([["debug", { value: true }]]);
    expect(state.config["request-retry"]).toBe(5000);
  });

  it("a loaded socks5:// proxy shows the warning and still lets another setting save", async () => {
    const state = server({ "proxy-url": "socks5://user:secret@proxy.example:1080" });
    const { user } = await openSettings();
    const proxy = screen.getByLabelText("Proxy for outbound requests");
    const warning = "The server can't use a SOCKS5 proxy yet. Use an HTTP or HTTPS proxy. Saving the other settings leaves it as it is.";
    expect(screen.getByText(warning)).toBeVisible();
    expect(proxy).toHaveAccessibleDescription(expect.stringContaining(warning));
    expect(proxy).not.toHaveAttribute("aria-invalid");
    // The warning names the problem, not the address.
    expect(document.body).not.toHaveTextContent("user:secret");

    await fill(user, screen.getByRole("textbox", { name: "Retries" }), "4");
    expect(screen.getByText(warning)).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting\./)).toBeVisible();
    expect(patches(state.api)).toEqual([["request-retry", { value: 4 }]]);
    expect(state.config["proxy-url"]).toBe("socks5://user:secret@proxy.example:1080");

    // Edited, the proxy is checked as any edit is, and the warning gives way
    // to the error; put back, it is a warning again.
    await fill(user, proxy, "socks5://other.example:1080");
    expect(await screen.findByText(/can't use a SOCKS5 proxy yet\. Use an HTTP or HTTPS proxy\.$/)).toBeVisible();
    expect(proxy).toHaveAttribute("aria-invalid", "true");
    expect(screen.queryByText(warning)).not.toBeInTheDocument();
    await fill(user, proxy, "socks5://user:secret@proxy.example:1080");
    expect(await screen.findByText(warning)).toBeVisible();
    expect(proxy).not.toHaveAttribute("aria-invalid");
  });

  it("a loaded number past the largest the form holds exactly doesn't stop a save", async () => {
    const state = server({ "logs-max-total-size-mb": 2 ** 60 });
    const { user } = await openSettings();
    expect(
      screen.getByText(
        "The log directory's limit can be at most 9,007,199,254,740,991. Saving the other settings leaves it as it is.",
      ),
    ).toBeVisible();
    await user.click(screen.getByRole("checkbox", { name: "Request logs" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting\./)).toBeVisible();
    expect(patches(state.api)).toEqual([["request-log", { value: true }]]);
  });

  it("an edited number past MAX_SAFE_INTEGER is refused", async () => {
    const state = server();
    const { user } = await openSettings();
    const retries = screen.getByRole("textbox", { name: "Retries" });
    await fill(user, retries, "9007199254740992");
    expect(await screen.findByText("Retries can be at most 9,007,199,254,740,991.")).toBeVisible();
    expect(retries).toHaveAttribute("aria-invalid", "true");

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(state.api.callsTo("GET", CONFIG)).toHaveLength(1);
    expect(patches(state.api)).toEqual([]);

    // The largest is taken.
    await fill(user, retries, "9007199254740991");
    await waitFor(() => {
      expect(screen.queryByText(/can be at most/)).not.toBeInTheDocument();
    });
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting\./)).toBeVisible();
    expect(patches(state.api)).toEqual([["request-retry", { value: Number.MAX_SAFE_INTEGER }]]);
  });

  it("says there is nothing to save when the server already has the change", async () => {
    const state = server();
    const { user } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    state.config.debug = true;
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(await screen.findByText("Nothing to save: the server already has these values.")).toBeVisible();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expectNothingUnsaved();
    expect(patches(state.api)).toEqual([]);
  });

  it("gives the server's reason when it fails to save config.yaml", async () => {
    const state = server();
    state.api.use(
      route("PATCH", `${MANAGEMENT}/debug`, {
        status: 500,
        json: { error: "failed to save config: disk full" },
      }),
    );
    const { user } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await within(dialog).findByText("The server failed (HTTP 500)")).toBeVisible();
    expect(dialog).toHaveTextContent("failed to save config: disk full");
    expect(within(dialog).queryByText("This server can't save config.yaml")).not.toBeInTheDocument();
  });
  it("says when this server can't save config.yaml", async () => {
    const state = server();
    state.api.use(route("PATCH", `${MANAGEMENT}/debug`, WRITER_UNAVAILABLE));
    const { user } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await within(dialog).findByText("This server can't save config.yaml")).toBeVisible();
    expect(within(dialog).queryByText("Saved some of the changes")).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Close" }));
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    expect(screen.getByRole("checkbox", { name: "Debug logging" })).toBeChecked();
    // The client keys are in config.yaml too.
    expect(screen.getByText("Client keys can't be changed here")).toBeVisible();
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

describe("routing by quota", () => {
  const strategy = () => screen.getByRole("combobox", { name: "How credentials are picked" });

  it("shows its settings only while it is picked, and puts them back when it isn't", async () => {
    server({ routing: quotaRouting() });
    const { user } = await openSettings();
    expect(strategy()).toHaveValue("quota");
    expect(screen.getByRole("option", { name: "By quota" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Prefer" })).toHaveValue("most-left");
    expect(screen.getByRole("textbox", { name: "Kept back (%)" })).toHaveValue("10");
    expect(screen.getByRole("textbox", { name: "Check quota rests after" })).toHaveValue("1h");

    await user.selectOptions(screen.getByRole("combobox", { name: "Prefer" }), "soonest-reset");
    await fill(user, screen.getByRole("textbox", { name: "Kept back (%)" }), "20");
    expect(screen.getByText("2 unsaved changes.")).toBeVisible();

    await user.selectOptions(strategy(), "round-robin");
    expect(screen.queryByRole("combobox", { name: "Prefer" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Kept back (%)" })).not.toBeInTheDocument();
    // Its check after a quota rest works with any strategy.
    expect(screen.getByRole("textbox", { name: "Check quota rests after" })).toBeVisible();
    expect(screen.getByText("1 unsaved change.")).toBeVisible();

    await user.selectOptions(strategy(), "quota");
    expect(screen.getByRole("combobox", { name: "Prefer" })).toHaveValue("most-left");
    expect(screen.getByRole("textbox", { name: "Kept back (%)" })).toHaveValue("10");
    expectNothingUnsaved();
  });

  it("saves the strategy through its route and the quota settings through the v8 config route", async () => {
    const state = server();
    const { user } = await openSettings();
    expect(screen.queryByRole("combobox", { name: "Prefer" })).not.toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "Check quota rests after" })).toHaveValue("");

    await user.selectOptions(strategy(), "quota");
    const prefer = screen.getByRole("combobox", { name: "Prefer" });
    expect(prefer).toHaveValue("soonest-reset");
    expect(screen.getByRole("textbox", { name: "Kept back (%)" })).toHaveValue("0");
    await user.selectOptions(prefer, "most-left");
    await fill(user, screen.getByRole("textbox", { name: "Kept back (%)" }), " 15 ");
    await fill(user, screen.getByRole("textbox", { name: "Check quota rests after" }), "90m");
    expect(screen.getByText("4 unsaved changes.")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    const rows = within(dialog).getAllByRole("row");
    expect(rows.slice(1).map((row) => row.textContent)).toEqual([
      "How credentials are picked routing.strategyRound robinBy quota",
      "Quota preference routing.quota.preferThe limit that resets soonestThe most quota left",
      "Quota kept back routing.quota.reserve-percentNone15%",
      "Check quota rests after routing.quota.check-afterOff90m",
    ]);
    expect(dialog).toHaveTextContent(
      "The quota settings and the management address go through the server's v8 config route, which saves the whole file in the v8 layout",
    );
    expect(dialog).not.toHaveTextContent("Nothing else in the file changes.");
    expect(within(dialog).queryByText(/takes a restart/)).not.toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Save 4 settings" }));
    expect(
      await screen.findByText("Saved 4 settings. The server uses them from now on."),
    ).toBeVisible();
    expect(patches(state.api)).toEqual([["routing/strategy", { value: "quota" }]]);
    expect(v8Puts(state.api)).toEqual([
      ["routing/quota/prefer", "most-left"],
      ["routing/quota/reserve-percent", 15],
      ["routing/quota/check-after", "90m"],
    ]);
    expect(state.config.routing).toEqual({
      strategy: "quota",
      quota: { prefer: "most-left", "reserve-percent": 15, "check-after": "90m" },
    });
    expectNothingUnsaved();
    expect(state.api.unhandled).toEqual([]);
  });

  it("checks a share kept back and a time as they are typed in", async () => {
    const state = server({ routing: quotaRouting() });
    const { user } = await openSettings();
    const reserve = screen.getByRole("textbox", { name: "Kept back (%)" });
    await fill(user, reserve, "101");
    expect(await screen.findByText("The share kept back is a whole number from 0 to 100.")).toBeVisible();
    expect(reserve).toHaveAttribute("aria-invalid", "true");
    await fill(user, reserve, "10");

    const checkAfter = screen.getByRole("textbox", { name: "Check quota rests after" });
    await fill(user, checkAfter, "1 day");
    const unread = await screen.findByText(/^The server can't read this as a time, and takes it as off\./);
    expect(unread).toHaveTextContent("such as 1h, 90m or 1h30m (h, m, s, ms, us or ns; a day is 24h).");
    expect(checkAfter).toHaveAttribute("aria-invalid", "true");
    expect(checkAfter).toHaveAccessibleDescription(/can't read this as a time/);
    await fill(user, checkAfter, "-1h");
    expect(
      await screen.findByText("A negative time is off. Leave it empty for off, or write a time such as 1h."),
    ).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(state.api.callsTo("GET", CONFIG)).toHaveLength(1);

    // Empty is off, and fine.
    await fill(user, checkAfter, "");
    await waitFor(() => {
      expect(checkAfter).not.toHaveAttribute("aria-invalid");
    });
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    expect(within(dialog).getAllByRole("row")[1]).toHaveTextContent(/check-after.*1h.*Off$/);
  });

  it("warns of a time in config.yaml the server reads as off, and saves the rest", async () => {
    const state = server({ routing: quotaRouting({ "check-after": "2 days" }) });
    const { user } = await openSettings();
    const checkAfter = screen.getByRole("textbox", { name: "Check quota rests after" });
    expect(checkAfter).toHaveValue("2 days");
    expect(checkAfter).not.toHaveAttribute("aria-invalid");
    expect(checkAfter).toHaveAccessibleDescription(/takes it as off\..*Saving the other settings leaves it as it is\./);

    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    expect(dialog).toHaveTextContent("Nothing else in the file changes.");
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting\./)).toBeVisible();
    expect(v8Puts(state.api)).toEqual([]);
  });
});

describe("the management address", () => {
  const address = () => screen.getByRole("textbox", { name: /^Management address/ });

  it("shows the address from config.yaml, which takes a restart", async () => {
    const state = server({}, undefined, { separateAddress: "127.0.0.1:8318" });
    await openSettings();
    const card = screen.getByRole("region", { name: "Management" });
    expect(address()).toHaveValue("127.0.0.1:8318");
    expect(address()).toHaveAccessibleName("Management address Takes a restart");
    expect(address()).toHaveAttribute("placeholder", "127.0.0.1:8318");
    expect(card).not.toHaveTextContent(/refuse/);
    expectNothingUnsaved();
    expect(state.api.unhandled).toEqual([]);
  });

  it("checks it as it is typed in, against the proxy's port", async () => {
    server({}, undefined, { port: 9000 });
    const { user } = await openSettings();
    expect(address()).toHaveValue("");
    await fill(user, address(), "8318");
    expect(
      await screen.findByText(
        "Add the host before the port, such as 127.0.0.1:8318, or write :8318 for every interface.",
      ),
    ).toBeVisible();
    expect(address()).toHaveAttribute("aria-invalid", "true");
    await fill(user, address(), "[::1]:9000");
    expect(
      await screen.findByText(
        "Port 9000 is the proxy's own (server.port). Pick another: the management address needs a port of its own.",
      ),
    ).toBeVisible();
    await fill(user, address(), "[::1]:9001");
    await waitFor(() => {
      expect(address()).not.toHaveAttribute("aria-invalid");
    });
    expect(screen.queryByText(/refuse/)).not.toBeInTheDocument();
  });

  it("warns when other computers could reach it while allow-remote is off", async () => {
    server();
    const { user } = await openSettings();
    await fill(user, address(), ":8318");
    const warning = await screen.findByText(
      "The server will refuse clients on other computers at this address, as management.allow-remote is off. To manage it from them, turn that on in config.yaml, or start the server with MANAGEMENT_PASSWORD set.",
    );
    expect(warning).toBeVisible();
    expect(address()).not.toHaveAttribute("aria-invalid");
    await fill(user, address(), "mgmt.example:8318");
    expect(
      await screen.findByText(/^If other computers can reach mgmt\.example, the server will refuse them there/),
    ).toBeVisible();
    await fill(user, address(), "localhost:8318");
    await waitFor(() => {
      expect(screen.queryByText(/refuse/)).not.toBeInTheDocument();
    });
  });

  it("doesn't warn with allow-remote on", async () => {
    server({}, undefined, { separateAddress: "0.0.0.0:8318", allowRemote: true });
    await openSettings();
    expect(address()).toHaveValue("0.0.0.0:8318");
    expect(screen.queryByText(/refuse/)).not.toBeInTheDocument();
  });

  it("saves it through the v8 config route, and says where the dashboard is after a restart", async () => {
    const state = server();
    const { user } = await openSettings();
    expect(addressReads(state.api)).toBe(1);
    await fill(user, address(), " 127.0.0.1:8318 ");
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    // The review reads the address and the proxy's port again.
    expect(addressReads(state.api)).toBe(2);
    expect(dialog).toHaveTextContent(
      "the server uses it from then on, except the management address, which takes a restart.",
    );
    const rows = within(dialog).getAllByRole("row");
    expect(rows[2]).toHaveTextContent(
      "Management address management.separate-addressNone: on the proxy's port127.0.0.1:8318",
    );
    expect(within(dialog).getByText("The management address takes a restart")).toBeVisible();
    expect(dialog).toHaveTextContent(
      "The server reads the management address only when it starts, so this page works as it does now until the server restarts. Then the dashboard and the management API are at http://127.0.0.1:8318/dashboard/, not at this page's address, and the proxy's port no longer serves them.",
    );

    await user.click(within(dialog).getByRole("button", { name: "Save 2 settings" }));
    expect(
      await screen.findByText(
        "Saved 2 settings. The server uses them from now on, and the management address after a restart.",
      ),
    ).toBeVisible();
    expect(patches(state.api)).toEqual([["debug", { value: true }]]);
    expect(v8Puts(state.api)).toEqual([["management/separate-address", "127.0.0.1:8318"]]);
    expect(state.v8.management).toEqual({ "allow-remote": false, "separate-address": "127.0.0.1:8318" });
    expectNothingUnsaved();
    expect(address()).toHaveValue("127.0.0.1:8318");
    expect(state.api.unhandled).toEqual([]);
  });

  it("says the dashboard goes back to the proxy's port when it is emptied", async () => {
    const state = server({}, undefined, { separateAddress: ":8318" });
    const { user } = await openSettings();
    await user.clear(address());
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    expect(within(dialog).getAllByRole("row")[1]).toHaveTextContent(/:8318None: on the proxy's port$/);
    expect(dialog).toHaveTextContent(
      "Then the dashboard and the management API are back on the proxy's port (8317), not at this page's address.",
    );
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(
      await screen.findByText("Saved 1 setting. The server uses it after a restart."),
    ).toBeVisible();
    expect(v8Puts(state.api)).toEqual([["management/separate-address", ""]]);
  });

  it("checks the port again in the review, against the server's port then", async () => {
    const state = server();
    const { user } = await openSettings();
    await fill(user, address(), "127.0.0.1:8318");
    // Meanwhile, the proxy moves to that port.
    (state.v8.server as Record<string, unknown>).port = 8318;
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    expect(await screen.findByText(/^Port 8318 is the proxy's own/)).toBeVisible();
    expect(address()).toHaveAttribute("aria-invalid", "true");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(v8Puts(state.api)).toEqual([]);
  });

  it("says why it can't be read, and still lets the other settings save", async () => {
    const state = server();
    state.api.use(
      route("GET", V8_CONFIG, {
        status: 500,
        json: { error: "read_failed", message: "failed to read config" },
      }),
    );
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Management" });
    expect(await within(card).findByRole("button", { name: "Try again" })).toBeVisible();
    expect(within(card).queryByRole("textbox")).not.toBeInTheDocument();

    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(dialog).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting\./)).toBeVisible();
    expect(patches(state.api)).toEqual([["debug", { value: true }]]);
  });

  it("isn't offered by a server without the v8 config route", async () => {
    const state = server();
    state.api.use(route("GET", V8_CONFIG, { status: 404 }));
    await openSettings();
    expect(screen.queryByRole("region", { name: "Management" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: /^Management address/ })).not.toBeInTheDocument();
    expectNothingUnsaved();
  });
});

/** Adds a new key through the dialog, and gives back the key it made. */
async function addKey(user: ReturnType<typeof renderApp>["user"]) {
  await user.click(screen.getByRole("button", { name: "Add a client key" }));
  const dialog = await screen.findByRole("dialog", { name: "Add a client key" });
  const made = within(dialog).getByLabelText<HTMLInputElement>("Client key").value;
  await user.click(within(dialog).getByRole("button", { name: "Add to the list" }));
  await waitFor(() => {
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
  return made;
}

/** The key as the screens show it, masked. */
function masked(key: string) {
  return `sk-...${key.slice(-4)}`;
}

/** What each write to the client keys did, in order. */
function keyWrites(api: MockApi) {
  return api.calls
    .filter((call) => call.url.pathname === API_KEYS && call.method !== "GET")
    .map((call) => (call.method === "DELETE" ? `DELETE${call.url.search}` : call.method));
}

describe("the client keys", () => {
  it("lists them masked; a new key waits, copyable, for the review", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    expect(card).not.toHaveTextContent(KEY_A);
    expect(card).not.toHaveTextContent(/take effect at once/);
    expect(within(card).getAllByRole("listitem")).toHaveLength(2);

    await user.click(within(card).getByRole("button", { name: "Add a client key" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a client key" });
    const field = within(dialog).getByLabelText("Client key");
    expect(field).toHaveAttribute("type", "password");
    expect(dialog).toHaveTextContent(/It works only once you save the changes/);
    const made = (field as HTMLInputElement).value;
    expect(made).toMatch(/^sk-[\w-]{43}$/);
    await user.click(within(dialog).getByRole("button", { name: "Add to the list" }));

    const rows = within(card).getAllByRole("listitem");
    expect(rows).toHaveLength(3);
    expect(rows[2]).toHaveTextContent("Not saved yet");
    expect(rows[2]).not.toHaveTextContent(made);
    expect(
      within(card).getByRole("button", { name: `Copy the client key ${masked(made)}` }),
    ).toBeVisible();
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    expect(state.api.callsTo("PATCH", API_KEYS)).toEqual([]);

    state.keys.push("sk-added-elsewhere-0003");
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    const list = within(review).getByRole("list", { name: "The client key changes" });
    expect(within(list).getAllByRole("listitem").map((item) => item.textContent)).toEqual([
      `Add client key ${masked(made)}`,
    ]);
    expect(review).not.toHaveTextContent(made);
    expect(within(review).queryByRole("table")).not.toBeInTheDocument();
    await user.click(within(review).getByRole("button", { name: "Save 1 change" }));

    expect(await screen.findByText(/Saved 1 change\. The server uses it/)).toBeVisible();
    expect(state.api.callsTo("PATCH", API_KEYS).map((call) => call.json())).toEqual([
      { old: made, new: made },
    ]);
    expect(state.keys).toEqual([KEY_A, KEY_B, "sk-added-elsewhere-0003", made]);
    // The key never goes in an address.
    expect(state.api.calls.some((call) => call.url.href.includes(made))).toBe(false);
    await waitFor(() => {
      expect(within(card).getAllByRole("listitem")).toHaveLength(4);
    });
    expect(card).not.toHaveTextContent("Not saved yet");
    expectNothingUnsaved();
  });

  it("refuses an example key and one already listed", async () => {
    const state = server();
    const { user } = await openSettings();
    const made = await addKey(user);
    await user.click(screen.getByRole("button", { name: "Add a client key" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a client key" });
    const field = within(dialog).getByLabelText("Client key");
    await fill(user, field, "your-api-key-1");
    await user.click(within(dialog).getByRole("button", { name: "Add to the list" }));
    expect(await within(dialog).findByText(/one of CLIProxyAPI's examples/)).toBeVisible();
    for (const key of [KEY_B, made]) {
      await fill(user, field, key);
      await user.click(within(dialog).getByRole("button", { name: "Add to the list" }));
      expect(await within(dialog).findByText("That key is already in the list.")).toBeVisible();
    }
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    expect(state.api.callsTo("PATCH", API_KEYS)).toEqual([]);
  });

  it("deletes a key on saving, by its place in the list as it is then", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    await user.click(
      within(card).getByRole("button", { name: `Delete the client key ${masked(KEY_B)}` }),
    );
    expect(within(card).getAllByRole("listitem")[1]).toHaveTextContent("Will be deleted");
    expect(
      within(card).getByRole("button", { name: `Undo deleting the client key ${masked(KEY_B)}` }),
    ).toHaveFocus();
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    expect(state.api.callsTo("DELETE", API_KEYS)).toEqual([]);

    // Another key arrives ahead of it before the save.
    state.keys.unshift("sk-added-elsewhere-0003");
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    const list = within(review).getByRole("list", { name: "The client key changes" });
    expect(list).toHaveTextContent(`Delete client key ${masked(KEY_B)}`);
    expect(within(review).queryByText("This deletes the last client key")).not.toBeInTheDocument();
    await user.click(within(review).getByRole("button", { name: "Save 1 change" }));
    await waitFor(() => {
      expect(state.keys).toEqual(["sk-added-elsewhere-0003", KEY_A]);
    });
    expect(state.api.callsTo("DELETE", API_KEYS)[0]?.url.search).toBe("?index=2");
  });

  it("undoes an added or deleted key before it is saved", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    const made = await addKey(user);
    await user.click(
      within(card).getByRole("button", { name: `Delete the client key ${masked(KEY_A)}` }),
    );
    expect(screen.getByText("2 unsaved changes.")).toBeVisible();

    const undoDelete = within(card).getByRole("button", {
      name: `Undo deleting the client key ${masked(KEY_A)}`,
    });
    await user.click(undoDelete);
    expect(card).not.toHaveTextContent("Will be deleted");
    expect(undoDelete).toHaveAccessibleName(`Delete the client key ${masked(KEY_A)}`);
    expect(undoDelete).toHaveFocus();

    await user.click(
      within(card).getByRole("button", { name: `Undo adding the client key ${masked(made)}` }),
    );
    expect(within(card).getAllByRole("listitem")).toHaveLength(2);
    expect(within(card).getByRole("button", { name: "Add a client key" })).toHaveFocus();
    expectNothingUnsaved();
    expect(keyWrites(state.api)).toEqual([]);
  });

  it("discards key changes with the settings", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    await addKey(user);
    await user.click(
      within(card).getByRole("button", { name: `Delete the client key ${masked(KEY_A)}` }),
    );
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    expect(screen.getByText("3 unsaved changes.")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expectNothingUnsaved();
    expect(within(card).getAllByRole("listitem")).toHaveLength(2);
    expect(card).not.toHaveTextContent(/Not saved yet|Will be deleted/);
    expect(screen.getByRole("checkbox", { name: "Debug logging" })).not.toBeChecked();
    expect(keyWrites(state.api)).toEqual([]);
  });

  it("warns in the review when saving leaves no client key, as the proxy then lets anyone in", async () => {
    const state = server({}, [KEY_A]);
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    await user.click(within(card).getByRole("button", { name: /^Delete the client key/ }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    expect(within(review).getByText("This deletes the last client key")).toBeVisible();
    await user.click(within(review).getByRole("button", { name: "Save 1 change" }));
    expect(await within(card).findByText("No client keys")).toBeVisible();
    expect(state.keys).toEqual([]);
  });

  it("adds new keys before deleting old ones, so the list is never empty on the way", async () => {
    const state = server({}, [KEY_A]);
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    await user.click(within(card).getByRole("button", { name: /^Delete the client key/ }));
    const made = await addKey(user);
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    const list = within(review).getByRole("list", { name: "The client key changes" });
    expect(within(list).getAllByRole("listitem").map((item) => item.textContent)).toEqual([
      `Add client key ${masked(made)}`,
      `Delete client key ${masked(KEY_A)}`,
    ]);
    expect(within(review).queryByText("This deletes the last client key")).not.toBeInTheDocument();
    await user.click(within(review).getByRole("button", { name: "Save 2 changes" }));
    expect(await screen.findByText(/Saved 2 changes\. The server uses them/)).toBeVisible();
    expect(keyWrites(state.api)).toEqual(["PATCH", "DELETE?index=0"]);
    expect(state.keys).toEqual([made]);
  });

  it("says which changes were saved when the server refuses one", async () => {
    const state = server();
    state.api.use(
      route("PATCH", `${MANAGEMENT}/debug`, { status: 400, json: { error: "invalid body" } }),
    );
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    const made = await addKey(user);
    await user.click(
      within(card).getByRole("button", { name: `Delete the client key ${masked(KEY_B)}` }),
    );
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("checkbox", { name: "Request logs" }));
    expect(screen.getByText("4 unsaved changes.")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    expect(within(review).getByRole("heading", { name: "Client keys" })).toBeVisible();
    expect(within(review).getByRole("heading", { name: "Settings" })).toBeVisible();
    expect(within(review).getAllByRole("row")).toHaveLength(3);
    await user.click(within(review).getByRole("button", { name: "Save 4 changes" }));
    const partial = await within(review).findByText(/Saved 2 of 4/);
    expect(partial).toHaveTextContent(
      "Saved 2 of 4. “Debug logging” wasn't saved, nor the 1 change after it.",
    );
    expect(within(review).getByText("invalid body")).toBeVisible();
    expect(state.keys).toEqual([KEY_A, made]);
    expect(patches(state.api)).toEqual([["debug", { value: true }]]);

    await user.click(within(review).getByRole("button", { name: "Close" }));
    // The keys saved are no longer unsaved; the two settings still are.
    expect(screen.getByText("2 unsaved changes.")).toBeVisible();
    expect(card).not.toHaveTextContent(/Not saved yet|Will be deleted/);
    expect(screen.getByRole("checkbox", { name: "Debug logging" })).toBeChecked();
  });

  it("says when a key was added or deleted elsewhere after the review", async () => {
    const state = server();
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    const made = await addKey(user);
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    let review = await screen.findByRole("dialog", { name: "Review the changes" });
    state.keys.push(made);
    await user.click(within(review).getByRole("button", { name: "Save 1 change" }));
    expect(await within(review).findByText("That key is already in the list")).toBeVisible();
    await user.click(within(review).getByRole("button", { name: "Close" }));
    await waitFor(() => {
      expectNothingUnsaved();
    });

    await user.click(
      within(card).getByRole("button", { name: `Delete the client key ${masked(KEY_B)}` }),
    );
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    review = await screen.findByRole("dialog", { name: "Review the changes" });
    state.keys.splice(state.keys.indexOf(KEY_B), 1);
    await user.click(within(review).getByRole("button", { name: "Save 1 change" }));
    expect(await within(review).findByText("That key was already deleted")).toBeVisible();
    expect(keyWrites(state.api)).toEqual([]);
  });

  it("says when this server can't save the client keys, and stops offering changes", async () => {
    const state = server();
    state.api.use(route("PATCH", API_KEYS, WRITER_UNAVAILABLE));
    const { user } = await openSettings();
    const card = screen.getByRole("region", { name: "Client API keys" });
    const made = await addKey(user);
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(review).getByRole("button", { name: "Save 1 change" }));
    expect(await within(review).findByText("This server can't save config.yaml")).toBeVisible();
    await user.click(within(review).getByRole("button", { name: "Close" }));

    expect(within(card).getByText("Client keys can't be changed here")).toBeVisible();
    expect(within(card).getByText(/no way to save config\.yaml from here/)).toBeVisible();
    expect(within(card).queryByRole("button", { name: /^Delete/ })).not.toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Add a client key" })).not.toBeInTheDocument();
    // The key not saved stays, so it can be undone.
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
    await user.click(
      within(card).getByRole("button", { name: `Undo adding the client key ${masked(made)}` }),
    );
    expectNothingUnsaved();
    expect(state.keys).toEqual([KEY_A, KEY_B]);
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

  it("says when this server can't save the file", async () => {
    const state = server();
    state.api.use(route("PUT", CONFIG_YAML, WRITER_UNAVAILABLE));
    const { user } = await openSettings();
    const editor = await openFile(user);
    fireEvent.change(editor, { target: { value: `${YAML}# mine${NL}` } });
    await user.click(screen.getByRole("button", { name: "Review changes" }));
    const dialog = await screen.findByRole("dialog", { name: "Review the changes to config.yaml" });
    await user.click(within(dialog).getByRole("button", { name: "Save config.yaml" }));
    expect(await within(dialog).findByText("This server can't save config.yaml")).toBeVisible();
  });
});

describe("a server without the settings routes", () => {
  it("says the settings and client keys can't be changed here", async () => {
    mockApi();
    renderApp("/settings");
    expect(await screen.findByText("Settings can't be changed here")).toBeVisible();
    expect(screen.getByText("Client keys can't be changed here")).toBeVisible();
    expect(screen.queryByText(/by hand/)).not.toBeInTheDocument();
  });
});

describe("leaving with unsaved changes", () => {
  /** Whether the browser would ask before unloading the page now. */
  function unloadAsks() {
    const event = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(event);
    return event.defaultPrevented;
  }

  it("asks first, and stays when asked to", async () => {
    server();
    const { user, router } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("link", { name: "Overview" }));
    const dialog = await screen.findByRole("dialog", { name: "Leave without saving?" });
    expect(within(dialog).getByRole("button", { name: "Stay" })).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Stay" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/settings");
    expect(screen.getByRole("checkbox", { name: "Debug logging" })).toBeChecked();
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
  });

  it("leaves without saving when confirmed", async () => {
    const state = server();
    const { user, router } = await openSettings();
    await addKey(user);
    await user.click(screen.getByRole("link", { name: "Overview" }));
    const dialog = await screen.findByRole("dialog", { name: "Leave without saving?" });
    await user.click(within(dialog).getByRole("button", { name: "Leave without saving" }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/");
    });
    expect(keyWrites(state.api)).toEqual([]);
  });

  it("keeps config.yaml edits across the tabs, and asks before leaving with them", async () => {
    server();
    const { user, router } = await openSettings();
    await user.click(screen.getByRole("tab", { name: "config.yaml" }));
    await user.click(screen.getByRole("button", { name: "Show config.yaml" }));
    const editor = await screen.findByRole("textbox", { name: "config.yaml" });
    fireEvent.change(editor, { target: { value: `${YAML}# mine${NL}` } });
    await user.click(screen.getByRole("tab", { name: "Settings" }));
    expect(router.state.location.search).toBe("");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await user.click(screen.getByRole("tab", { name: "config.yaml" }));
    expect(screen.getByRole("textbox", { name: "config.yaml" })).toHaveValue(`${YAML}# mine${NL}`);
    expect(unloadAsks()).toBe(true);
    await user.click(screen.getByRole("link", { name: "Overview" }));
    expect(await screen.findByRole("dialog", { name: "Leave without saving?" })).toBeVisible();
  });

  it("doesn't ask with nothing unsaved", async () => {
    server();
    const { user, router } = await openSettings();
    expect(unloadAsks()).toBe(false);
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    expect(unloadAsks()).toBe(true);
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(unloadAsks()).toBe(false);
    await user.click(screen.getByRole("link", { name: "Overview" }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/");
    });
    expect(screen.queryByRole("dialog", { name: "Leave without saving?" })).not.toBeInTheDocument();
  });

  it("doesn't ask once the changes are saved", async () => {
    server();
    const { user, router } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Review and save" }));
    const review = await screen.findByRole("dialog", { name: "Review the changes" });
    await user.click(within(review).getByRole("button", { name: "Save 1 setting" }));
    expect(await screen.findByText(/Saved 1 setting/)).toBeVisible();
    expect(unloadAsks()).toBe(false);
    await user.click(screen.getByRole("link", { name: "Overview" }));
    await waitFor(() => {
      expect(router.state.location.pathname).toBe("/");
    });
  });

  // Signing out swaps the whole frame for the sign-in page, with no move the
  // page's own guard sees, so the frame asks instead.
  it("asks before signing out, and stays when asked to", async () => {
    server();
    const { user, router } = await openSettings();
    await user.click(screen.getByRole("checkbox", { name: "Debug logging" }));
    await user.click(screen.getByRole("button", { name: "Sign out" }));
    const dialog = await screen.findByRole("dialog", { name: "Sign out without saving?" });
    expect(within(dialog).getByRole("button", { name: "Stay" })).toHaveFocus();
    await user.click(within(dialog).getByRole("button", { name: "Stay" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/settings");
    expect(readStoredKey()).not.toBeNull();
    expect(screen.getByText("1 unsaved change.")).toBeVisible();
  });

  it("signs out without saving when confirmed", async () => {
    const state = server();
    const { user } = await openSettings();
    await addKey(user);
    await user.click(screen.getByRole("button", { name: "Sign out" }));
    const dialog = await screen.findByRole("dialog", { name: "Sign out without saving?" });
    await user.click(within(dialog).getByRole("button", { name: "Sign out without saving" }));
    expect(await screen.findByText("You signed out.")).toBeVisible();
    expect(readStoredKey()).toBeNull();
    expect(keyWrites(state.api)).toEqual([]);
  });

  it("signs out at once with nothing unsaved", async () => {
    server();
    const { user } = await openSettings();
    await user.click(screen.getByRole("button", { name: "Sign out" }));
    expect(await screen.findByText("You signed out.")).toBeVisible();
    expect(
      screen.queryByRole("dialog", { name: "Sign out without saving?" }),
    ).not.toBeInTheDocument();
  });
});
