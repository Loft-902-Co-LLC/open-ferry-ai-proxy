import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { REQUEST_LOGS, requestLogPath } from "../../api/dashboard";
import { LOGGING_TO_FILE, REQUEST_LOG_SETTING, SERVER_LOGS } from "../../api/management";
import { logEntry, logPiece, logSearch, serverLogPage } from "../../test/fixtures";
import { loadFirst } from "../../test/loadFirst";
import { mockApi, route, type MockApi } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";
import { KEEP_LINES, mergeServerLog } from "./ServerLog";

const errorLog = logEntry({
  name: "error-v1-messages-2026-10-05T120110-9f8e7d6c.log",
  kind: "error",
  request_id: "9f8e7d6c",
  time: "2026-10-05T12:01:10.000Z",
  url: "/v1/messages",
  status: 502,
  model: "claude-sonnet-4-5",
});

function searches(api: MockApi) {
  return api.callsTo("GET", REQUEST_LOGS).map((call) => call.url.searchParams);
}

function lastSearch(api: MockApi) {
  const search = searches(api).at(-1);
  if (search === undefined) {
    throw new Error("no search");
  }
  return search;
}

// One test opens a request log from the list.
loadFirst(() => import("./LogsPage"), () => import("./LogViewerPage"));

describe("the request log search", () => {
  it("lists the logs, newest first, each with a link to it", async () => {
    const api = mockApi(route("GET", REQUEST_LOGS, { json: logSearch([errorLog, logEntry()]) }));
    renderApp("/logs");
    expect(await screen.findByRole("heading", { name: "Logs", level: 1 })).toBeVisible();
    const table = await screen.findByRole("table", { name: "Request logs, newest first" });
    const rows = within(table).getAllByRole("row");
    expect(rows[1]).toHaveTextContent("/v1/messages");
    expect(rows[1]).toHaveTextContent("Error");
    expect(rows[1]).toHaveTextContent("502");
    expect(rows[2]).toHaveTextContent("gpt-5.1-codex");
    expect(
      within(table).getByRole("link", { name: "Open v1-chat-completions-2026-10-05T115802-1234abcd.log" }),
    ).toHaveAttribute("href", "/logs/v1-chat-completions-2026-10-05T115802-1234abcd.log");

    const sent = lastSearch(api);
    expect(sent.get("limit")).toBe("50");
    expect(sent.has("from")).toBe(false);
    expect(sent.has("kind")).toBe(false);
    expect(api.unhandled).toEqual([]);
  });

  it("restores a search from the address", async () => {
    const api = mockApi(route("GET", REQUEST_LOGS, { json: logSearch([errorLog]) }));
    renderApp("/logs?range=24h&kind=error&status=5XX&model=claude");
    expect(await screen.findByRole("combobox", { name: "When" })).toHaveValue("24h");
    expect(screen.getByRole("combobox", { name: "Kind" })).toHaveValue("error");
    expect(screen.getByRole("textbox", { name: "Status" })).toHaveValue("5XX");
    await screen.findByRole("table", { name: "Request logs, newest first" });
    const sent = lastSearch(api);
    expect(sent.get("kind")).toBe("error");
    expect(sent.get("status")).toBe("5xx");
    expect(sent.get("model")).toBe("claude");
    const span = Date.parse(sent.get("to") ?? "") - Date.parse(sent.get("from") ?? "");
    expect(span).toBe(86_400_000);
  });

  it("searches the times a Usage page link gives", async () => {
    const api = mockApi(route("GET", REQUEST_LOGS, { json: logSearch([logEntry()]) }));
    renderApp("/logs?from=2026-10-05T11:53:02.114Z&to=2026-10-05T12:03:06.324Z");
    const when = await screen.findByRole("combobox", { name: "When" });
    expect(when).toHaveValue("given");
    expect(within(when).getByRole("option", { selected: true })).toHaveTextContent(/^From .+ to .+$/);
    await screen.findByRole("table", { name: "Request logs, newest first" });
    expect(lastSearch(api).get("from")).toBe("2026-10-05T11:53:02.114Z");
    expect(lastSearch(api).get("to")).toBe("2026-10-05T12:03:06.324Z");
  });

  it("searches with the form, and keeps the search in the address", async () => {
    const api = mockApi(route("GET", REQUEST_LOGS, { json: logSearch([logEntry()]) }));
    const { user, router } = renderApp("/logs");
    await screen.findByRole("table", { name: "Request logs, newest first" });
    await user.type(screen.getByRole("textbox", { name: "Path contains" }), "/v1/responses");
    await user.type(screen.getByRole("textbox", { name: "Text" }), "rate limit");
    await user.selectOptions(screen.getByRole("combobox", { name: "When" }), "7d");
    await user.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => {
      expect(lastSearch(api).get("path")).toBe("/v1/responses");
    });
    expect(lastSearch(api).get("q")).toBe("rate limit");
    expect(router.state.location.search).toContain("range=7d");
    expect(router.state.location.search).toContain("q=rate+limit");
  });

  it("refuses a status that isn't one, without searching", async () => {
    const api = mockApi(route("GET", REQUEST_LOGS, { json: logSearch([]) }));
    const { user } = renderApp("/logs");
    expect(await screen.findByText("No logs match.")).toBeVisible();
    const before = searches(api).length;
    await user.type(screen.getByRole("textbox", { name: "Status" }), "50x");
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(
      await screen.findByText("A status is three digits, such as 502, or a class, such as 5xx."),
    ).toBeVisible();
    expect(screen.getByRole("textbox", { name: "Status" })).toHaveAttribute("aria-invalid", "true");
    expect(searches(api)).toHaveLength(before);
  });

  it("offers to keep searching where a search stopped at its limit, not 'no results'", async () => {
    const api = mockApi(
      route("GET", REQUEST_LOGS, (request) =>
        request.url.searchParams.get("cursor") === "after-2000"
          ? { json: logSearch([errorLog]) }
          : {
              json: logSearch([], {
                next_cursor: "after-2000",
                scanned: { files: 2000, bytes: 41_943_040, limit_reached: true },
              }),
            },
      ),
    );
    const { user } = renderApp("/logs?q=overloaded");
    expect(await screen.findByText("Nothing found yet")).toBeVisible();
    expect(screen.getByText(/has read 2,000 files \(40 MiB\)/)).toBeVisible();
    expect(screen.queryByText("No logs match.")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Keep searching" }));
    expect(await screen.findByRole("table", { name: "Request logs, newest first" })).toHaveTextContent(
      "/v1/messages",
    );
    expect(lastSearch(api).get("cursor")).toBe("after-2000");
    expect(lastSearch(api).get("q")).toBe("overloaded");
    expect(screen.queryByRole("button", { name: "Keep searching" })).not.toBeInTheDocument();
  });

  it("loads more with the cursor", async () => {
    const api = mockApi(
      route("GET", REQUEST_LOGS, (request) =>
        request.url.searchParams.get("cursor") === "page-2"
          ? { json: logSearch([errorLog]) }
          : { json: logSearch([logEntry()], { next_cursor: "page-2" }) },
      ),
    );
    const { user } = renderApp("/logs");
    await user.click(await screen.findByRole("button", { name: "Load more" }));
    const table = screen.getByRole("table", { name: "Request logs, newest first" });
    expect(await within(table).findByText("/v1/messages", { exact: false })).toBeVisible();
    expect(lastSearch(api).get("cursor")).toBe("page-2");
  });

  it("says only failed requests are logged while request-log is off, and turns it on", async () => {
    let requestLog = false;
    const api = mockApi(
      route("GET", REQUEST_LOGS, () => ({ json: logSearch([errorLog], { request_log: requestLog }) })),
      route("PUT", REQUEST_LOG_SETTING, (request) => {
        requestLog = (request.json() as { value: boolean }).value;
        return { json: { status: "ok" } };
      }),
    );
    const { user } = renderApp("/logs");
    expect(await screen.findByText("Only failed requests are logged")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Log every request" }));
    await waitFor(() => {
      expect(screen.queryByText("Only failed requests are logged")).not.toBeInTheDocument();
    });
    expect(api.callsTo("PUT", REQUEST_LOG_SETTING)[0]?.json()).toEqual({ value: true });
  });

  it("comes back from a log to the same results, without searching again", async () => {
    const name = "v1-chat-completions-2026-10-05T115802-1234abcd.log";
    const api = mockApi(
      route("GET", REQUEST_LOGS, { json: logSearch([logEntry()]) }),
      route("GET", requestLogPath(name), { json: logPiece("=== REQUEST INFO ===\n") }),
    );
    const { user, router } = renderApp("/logs?range=1h");
    await user.click(await screen.findByRole("link", { name: `Open ${name}` }));
    expect(await screen.findByRole("heading", { name: "Request log", level: 1 })).toBeVisible();
    const searched = searches(api).length;

    await user.click(screen.getByRole("button", { name: "Back to the search" }));
    expect(await screen.findByRole("table", { name: "Request logs, newest first" })).toBeVisible();
    expect(router.state.location.search).toBe("?range=1h");
    expect(searches(api)).toHaveLength(searched);
  });

  it("is in the main navigation", async () => {
    mockApi(route("GET", REQUEST_LOGS, { json: logSearch([]) }));
    const { user, router } = renderApp("/about");
    await user.click(
      within(await screen.findByRole("navigation", { name: "Main" })).getByRole("link", {
        name: "Logs",
      }),
    );
    expect(await screen.findByRole("heading", { name: "Logs", level: 1 })).toBeVisible();
    expect(router.state.location.pathname).toBe("/logs");
  });

  it("links a server failure to the server log", async () => {
    mockApi(
      route("GET", REQUEST_LOGS, { status: 500, json: { error: "disk full" } }),
      route("GET", SERVER_LOGS, { json: serverLogPage(["[info ] started"]) }),
    );
    const { user, router } = renderApp("/logs");
    expect(await screen.findByText("The server failed (HTTP 500)")).toBeVisible();
    await user.click(screen.getByRole("link", { name: "server log" }));
    expect(await screen.findByRole("log", { name: "Server log lines" })).toBeVisible();
    expect(router.state.location.search).toBe("?tab=server");
  });
});

describe("the server log", () => {
  const lines = [
    "[2026-10-05 11:58:02] [--------] [info ] [server.go:88] API server started",
    "[2026-10-05 11:58:03] [1234abcd] [warn ] [auth.go:41] credential cooling down",
  ];

  it("shows the log's newest lines, and reads on from the cursor", async () => {
    const api = mockApi(
      route("GET", SERVER_LOGS, (request) =>
        request.url.searchParams.get("cursor") === "cursor-1"
          ? {
              json: serverLogPage(
                ["[2026-10-05 11:58:09] [9f8e7d6c] [error] [proxy.go:12] upstream 502"],
                { "next-cursor": "cursor-2" },
              ),
            }
          : { json: serverLogPage(lines) },
      ),
    );
    const { user } = renderApp("/logs?tab=server");
    const log = await screen.findByRole("log", { name: "Server log lines" });
    expect(log).toHaveTextContent("API server started");
    expect(log).toHaveTextContent("credential cooling down");
    expect(api.callsTo("GET", SERVER_LOGS)[0]?.url.searchParams.get("limit")).toBe("500");

    await user.click(screen.getByRole("checkbox", { name: "Follow" }));
    await user.click(screen.getByRole("button", { name: "Read new lines" }));
    await waitFor(() => {
      expect(log).toHaveTextContent("upstream 502");
    });
    expect(log).toHaveTextContent("API server started");
    expect(api.callsTo("GET", SERVER_LOGS).at(-1)?.url.searchParams.get("cursor")).toBe("cursor-1");
    expect(screen.getByText("3 lines.")).toBeVisible();
  });

  it("shows only the lines containing a filter", async () => {
    mockApi(route("GET", SERVER_LOGS, { json: serverLogPage(lines) }));
    const { user } = renderApp("/logs?tab=server");
    const log = await screen.findByRole("log", { name: "Server log lines" });
    await user.type(screen.getByRole("textbox", { name: "Show lines containing" }), "COOLING");
    expect(log).not.toHaveTextContent("API server started");
    expect(log).toHaveTextContent("credential cooling down");
    expect(screen.getByText("1 of 2 lines contain “COOLING”.")).toBeVisible();
  });

  it("says when the server doesn't log to a file, and turns that on", async () => {
    let toFile = false;
    const api = mockApi(
      route("GET", SERVER_LOGS, () =>
        toFile
          ? { json: serverLogPage(lines) }
          : { status: 400, json: { error: "logging to file disabled" } },
      ),
      route("PUT", LOGGING_TO_FILE, (request) => {
        toFile = (request.json() as { value: boolean }).value;
        return { json: { status: "ok" } };
      }),
    );
    const { user } = renderApp("/logs?tab=server");
    expect(await screen.findByText("The server doesn't write its log to a file")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Log to a file" }));
    expect(await screen.findByRole("log", { name: "Server log lines" })).toHaveTextContent(
      "API server started",
    );
    expect(api.callsTo("PUT", LOGGING_TO_FILE)[0]?.json()).toEqual({ value: true });
  });

  it("switches between the tabs", async () => {
    mockApi(
      route("GET", REQUEST_LOGS, { json: logSearch([logEntry()]) }),
      route("GET", SERVER_LOGS, { json: serverLogPage(lines) }),
    );
    const { user, router } = renderApp("/logs");
    await user.click(await screen.findByRole("tab", { name: "Server log" }));
    expect(await screen.findByRole("log", { name: "Server log lines" })).toBeVisible();
    expect(router.state.location.search).toBe("?tab=server");
    await user.click(screen.getByRole("tab", { name: "Request logs" }));
    expect(await screen.findByRole("table", { name: "Request logs, newest first" })).toBeVisible();
  });
});

describe("mergeServerLog", () => {
  const view = { lines: ["a", "b"], cursor: "c1", restarted: false, trimmed: false };

  it("adds the lines read from a cursor", () => {
    const next = mergeServerLog(view, serverLogPage(["c"], { "next-cursor": "c2" }), true);
    expect(next).toEqual({ lines: ["a", "b", "c"], cursor: "c2", restarted: false, trimmed: false });
  });

  it("starts again when the cursor no longer holds", () => {
    const next = mergeServerLog(view, serverLogPage(["x"], { "cursor-reset": true }), true);
    expect(next.lines).toEqual(["x"]);
    expect(next.restarted).toBe(true);
  });

  it("keeps the newest lines only", () => {
    const many = Array.from({ length: KEEP_LINES }, (_, index) => `line ${String(index)}`);
    const next = mergeServerLog({ ...view, lines: many }, serverLogPage(["newest"]), true);
    expect(next.lines).toHaveLength(KEEP_LINES);
    expect(next.lines.at(0)).toBe("line 1");
    expect(next.lines.at(-1)).toBe("newest");
    expect(next.trimmed).toBe(true);
  });

  it("reads the end afresh without a cursor", () => {
    const next = mergeServerLog(view, serverLogPage(["z"], { "next-cursor": "" }), false);
    expect(next).toEqual({ lines: ["z"], cursor: "", restarted: false, trimmed: false });
  });
});
