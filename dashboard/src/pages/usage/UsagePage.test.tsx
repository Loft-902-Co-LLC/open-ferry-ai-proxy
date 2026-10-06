import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import {
  REQUEST_LOGS,
  USAGE_LEDGER,
  USAGE_REQUESTS,
  USAGE_SERIES,
  USAGE_SUMMARY,
} from "../../api/dashboard";
import { USAGE_STATISTICS_ENABLED } from "../../api/management";
import {
  ledger,
  logEntry,
  logSearch,
  metrics,
  requestsPage,
  series,
  summary,
  unavailableLedger,
  usageRequest,
} from "../../test/fixtures";
import { mockApi, route, type MockRoute } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";

/** A server with usage recorded, one call, and its log. */
function usageServer(...overrides: MockRoute[]) {
  const api = mockApi(
    route("GET", USAGE_LEDGER, { json: ledger() }),
    route("GET", USAGE_SUMMARY, { json: summary() }),
    route("GET", USAGE_SERIES, { json: series() }),
    route("GET", USAGE_REQUESTS, { json: requestsPage([usageRequest()]) }),
    route("GET", REQUEST_LOGS, { json: logSearch([logEntry()]) }),
  );
  api.use(...overrides);
  return api;
}

function params(api: ReturnType<typeof mockApi>, path: string, index = -1) {
  const calls = api.callsTo("GET", path);
  const call = calls.at(index);
  if (call === undefined) {
    throw new Error(`no call to ${path}`);
  }
  return call.url.searchParams;
}

describe("the Usage page", () => {
  it("shows the totals, the chart's numbers and the calls", async () => {
    const api = usageServer();
    renderApp("/usage");

    expect(await screen.findByRole("heading", { name: "Usage", level: 1 })).toBeVisible();
    const totals = await screen.findByText("Requests", { selector: "dt" });
    expect(totals.nextElementSibling).toHaveTextContent("1,520");
    expect(screen.getByText("Failed", { selector: "dt" }).nextElementSibling).toHaveTextContent("12");
    expect(screen.getByText("Cost", { selector: "dt" }).nextElementSibling).toHaveTextContent(
      "4.18 USD",
    );

    const numbers = await screen.findByRole("table", { name: "The chart's numbers, by time" });
    expect(within(numbers).getByRole("columnheader", { name: "All calls" })).toBeInTheDocument();
    expect(within(numbers).getByRole("cell", { name: "820" })).toBeInTheDocument();

    const calls = await screen.findByRole("table", { name: "Calls to providers, newest first" });
    expect(within(calls).getByText("gpt-5.1-codex")).toBeVisible();
    expect(within(calls).getByText("sk-...9f3k")).toBeVisible();

    const summaryQuery = params(api, USAGE_SUMMARY);
    expect(summaryQuery.get("from")).not.toBeNull();
    expect(summaryQuery.get("to")).not.toBeNull();
    expect(summaryQuery.has("group_by")).toBe(false);
    expect(api.unhandled).toEqual([]);
  });

  it("sends the browser's UTC offset with the series, so days are the user's", async () => {
    const api = usageServer();
    renderApp("/usage");
    await screen.findByRole("table", { name: "The chart's numbers, by time" });
    // Tests run in UTC.
    expect(params(api, USAGE_SERIES).get("utc_offset")).toBe("0");
  });

  it("links a call to its request log by the last eight characters of its ID", async () => {
    const api = usageServer();
    renderApp("/usage");
    const link = await screen.findByRole("link", { name: "Log of request 1234abcd" });
    expect(link).toHaveAttribute(
      "href",
      "/logs/v1-chat-completions-2026-10-05T115802-1234abcd.log",
    );
    // One search by time covers the calls on screen.
    const search = params(api, REQUEST_LOGS);
    expect(search.get("from")).toBe("2026-10-05T11:53:02.114Z");
    expect(search.get("to")).toBe("2026-10-05T12:03:06.324Z");
  });

  it("says why a call has no log while request-log is off", async () => {
    usageServer(
      route("GET", REQUEST_LOGS, { json: logSearch([], { request_log: false }) }),
    );
    renderApp("/usage");
    expect(await screen.findByText("request-log is off")).toBeVisible();
    expect(
      screen.getByText("No log: only failed requests are logged while request-log is off"),
    ).toBeInTheDocument();
  });

  it("offers to find a log the search here didn't reach", async () => {
    usageServer(
      route("GET", REQUEST_LOGS, { json: logSearch([], { next_cursor: "more" }) }),
    );
    renderApp("/usage");
    const find = await screen.findByRole("link", { name: "Find the log of request 1234abcd" });
    expect(find.getAttribute("href")).toMatch(/^\/logs\?from=/);
  });

  it("shows failed calls only, when asked", async () => {
    const api = usageServer();
    const { user, router } = renderApp("/usage");
    await user.click(await screen.findByRole("checkbox", { name: "Failed only" }));
    await waitFor(() => {
      expect(params(api, USAGE_REQUESTS).get("failed")).toBe("true");
    });
    expect(router.state.location.search).toContain("failed=true");
  });

  it("loads more calls with the cursor", async () => {
    const api = usageServer(
      route("GET", USAGE_REQUESTS, (request) =>
        request.url.searchParams.get("cursor") === "page-2"
          ? { json: requestsPage([usageRequest({ id: 1, model: "older-model" })]) }
          : { json: requestsPage([usageRequest()], "page-2") },
      ),
    );
    const { user } = renderApp("/usage");
    await user.click(await screen.findByRole("button", { name: "Load 50 more" }));
    expect(await screen.findByText("older-model")).toBeVisible();
    expect(params(api, USAGE_REQUESTS).get("cursor")).toBe("page-2");
  });

  it("groups by model, and filters to one", async () => {
    const grouped = summary({
      group_by: "model",
      groups: [
        { key: "gpt-5.1-codex", label: "gpt-5.1-codex", metrics: metrics({ requests: 1000 }) },
        { key: "claude-sonnet-4-5", label: "claude-sonnet-4-5", metrics: metrics({ requests: 520 }) },
      ],
    });
    const api = usageServer(route("GET", USAGE_SUMMARY, { json: grouped }));
    const { user, router } = renderApp("/usage");

    await user.selectOptions(await screen.findByRole("combobox", { name: "Group by" }), "model");
    const table = await screen.findByRole("table", { name: "Usage by model" });
    expect(within(table).getByText("claude-sonnet-4-5")).toBeVisible();
    expect(params(api, USAGE_SUMMARY).get("group_by")).toBe("model");
    expect(params(api, USAGE_SERIES).get("groups")).toBe("5");

    await user.click(within(table).getByRole("button", { name: "Show only claude-sonnet-4-5" }));
    const chips = await screen.findByRole("list", { name: "Filters" });
    expect(chips).toHaveTextContent("Model: claude-sonnet-4-5");
    await waitFor(() => {
      expect(params(api, USAGE_REQUESTS).get("model")).toBe("claude-sonnet-4-5");
    });
    expect(params(api, USAGE_SUMMARY).get("model")).toBe("claude-sonnet-4-5");
    expect(router.state.location.search).toContain("model=claude-sonnet-4-5");

    await user.click(
      within(chips).getByRole("button", { name: "Remove the filter Model: claude-sonnet-4-5" }),
    );
    expect(screen.queryByRole("list", { name: "Filters" })).not.toBeInTheDocument();
    expect(router.state.location.search).not.toContain("model=");
  });

  it("restores its view from the address", async () => {
    const api = usageServer();
    renderApp("/usage?range=7d&group=provider&credential=codex-a.json&credential_label=a%40example.com");
    expect(await screen.findByRole("combobox", { name: "Range" })).toHaveValue("7d");
    expect(screen.getByRole("combobox", { name: "Group by" })).toHaveValue("provider");
    expect(await screen.findByRole("list", { name: "Filters" })).toHaveTextContent(
      "Credential: a@example.com",
    );
    await waitFor(() => {
      expect(params(api, USAGE_SUMMARY).get("credential")).toBe("codex-a.json");
    });
    const query = params(api, USAGE_SUMMARY);
    const from = Date.parse(query.get("from") ?? "");
    const to = Date.parse(query.get("to") ?? "");
    expect(to - from).toBe(7 * 86_400_000);
  });

  it("explains a ledger that couldn't be opened, and asks nothing else", async () => {
    const api = usageServer(
      route("GET", USAGE_LEDGER, { json: unavailableLedger("disk I/O error") }),
    );
    renderApp("/usage");
    expect(await screen.findByText("The usage ledger couldn't be opened")).toBeVisible();
    expect(screen.getByText("disk I/O error")).toBeVisible();
    expect(api.callsTo("GET", USAGE_SUMMARY)).toEqual([]);
    expect(api.callsTo("GET", USAGE_REQUESTS)).toEqual([]);
  });

  it("asks the ledger why, when a usage query finds it unavailable", async () => {
    let opened = true;
    const api = usageServer(
      route("GET", USAGE_LEDGER, () => ({
        json: opened ? ledger() : unavailableLedger("database disk image is malformed"),
      })),
      route("GET", USAGE_SUMMARY, () => {
        opened = false;
        return {
          status: 503,
          json: { error: "ledger_unavailable", message: "the usage ledger is unavailable" },
        };
      }),
    );
    renderApp("/usage");
    expect(await screen.findByText("database disk image is malformed")).toBeVisible();
    expect(screen.getByText("The usage ledger couldn't be opened")).toBeVisible();
    // Not retried: it stays so until the server restarts.
    expect(api.callsTo("GET", USAGE_SUMMARY)).toHaveLength(1);
  });

  it("starts recording when it is off", async () => {
    let enabled = false;
    const api = usageServer(
      route("GET", USAGE_LEDGER, () => ({
        json: ledger({ usage_statistics_enabled: enabled, recording: enabled }),
      })),
      route("PUT", USAGE_STATISTICS_ENABLED, (request) => {
        enabled = (request.json() as { value: boolean }).value;
        return { json: { status: "ok" } };
      }),
    );
    const { user } = renderApp("/usage");
    expect(await screen.findByText("Usage isn't being recorded")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Start recording" }));
    await waitFor(() => {
      expect(screen.queryByText("Usage isn't being recorded")).not.toBeInTheDocument();
    });
    expect(api.callsTo("PUT", USAGE_STATISTICS_ENABLED)[0]?.json()).toEqual({ value: true });
  });

  it("says when this server can't turn recording on", async () => {
    usageServer(
      route("GET", USAGE_LEDGER, {
        json: ledger({ usage_statistics_enabled: false, recording: false }),
      }),
    );
    const { user } = renderApp("/usage");
    await user.click(await screen.findByRole("button", { name: "Start recording" }));
    expect(await screen.findByText("This server can't change settings yet")).toBeVisible();
  });

  it("says so too when this server can't save config.yaml", async () => {
    usageServer(
      route("GET", USAGE_LEDGER, {
        json: ledger({ usage_statistics_enabled: false, recording: false }),
      }),
      route("PUT", USAGE_STATISTICS_ENABLED, {
        status: 503,
        json: { error: "config writer unavailable" },
      }),
    );
    const { user } = renderApp("/usage");
    await user.click(await screen.findByRole("button", { name: "Start recording" }));
    const notice = (await screen.findByText("This server can't change settings yet")).parentElement;
    expect(notice).toHaveTextContent("Set usage-statistics-enabled: true in config.yaml instead.");
  });

  it("says when nothing has been recorded yet", async () => {
    const api = usageServer(
      route("GET", USAGE_LEDGER, { json: ledger({ rows: 0, oldest: null, newest: null }) }),
    );
    renderApp("/usage");
    expect(await screen.findByRole("heading", { name: "Nothing recorded yet" })).toBeVisible();
    expect(api.callsTo("GET", USAGE_REQUESTS)).toEqual([]);
  });

  it("is in the main navigation", async () => {
    usageServer();
    const { user, router } = renderApp("/");
    await user.click(
      within(await screen.findByRole("navigation", { name: "Main" })).getByRole("link", {
        name: "Usage",
      }),
    );
    expect(await screen.findByRole("heading", { name: "Usage", level: 1 })).toBeVisible();
    expect(router.state.location.pathname).toBe("/usage");
  });
});
