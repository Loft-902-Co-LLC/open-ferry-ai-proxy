import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { requestLogPath } from "../../api/dashboard";
import { logEntry, logPiece } from "../../test/fixtures";
import { mockApi, route } from "../../test/mockApi";
import { renderApp } from "../../test/renderApp";
import { DOWNLOAD_PIECE, isLogName } from "./LogViewerPage";

const NAME = "v1-chat-completions-2026-10-05T115802-1234abcd.log";
const PATH = requestLogPath(NAME);

const head = "=== REQUEST INFO ===\nURL: /v1/chat/completions\n";
const tail = "=== RESPONSE ===\nStatus: 200\n";

/** A log of `head` then `tail`, served in pieces split after `head`. */
function twoPieces() {
  const size = head.length + tail.length;
  return route("GET", PATH, (request) => {
    const offset = Number(request.url.searchParams.get("offset") ?? "0");
    const log = logEntry({ size });
    return offset === 0
      ? { json: logPiece(head, { log, next_offset: head.length }) }
      : { json: logPiece(tail, { log, offset, next_offset: null }) };
  });
}

describe("a log's page", () => {
  it("shows the log's details and its first piece, and the next on request", async () => {
    const api = mockApi(twoPieces());
    const { user } = renderApp(`/logs/${NAME}`);
    expect(await screen.findByRole("heading", { name: "Request log", level: 1 })).toBeVisible();
    expect(screen.getByText(NAME)).toBeVisible();
    expect(screen.getByText("Model").nextElementSibling).toHaveTextContent("gpt-5.1-codex");
    expect(screen.getByText("Request ID").nextElementSibling).toHaveTextContent("1234abcd");

    const content = screen.getByLabelText("The log's content");
    expect(content).toHaveTextContent("URL: /v1/chat/completions");
    expect(content).not.toHaveTextContent("Status: 200");
    expect(api.callsTo("GET", PATH)[0]?.url.searchParams.get("length")).toBe("1048576");

    await user.click(screen.getByRole("button", { name: "Show the next 1 MiB" }));
    await waitFor(() => {
      expect(content).toHaveTextContent("Status: 200");
    });
    expect(api.callsTo("GET", PATH).at(-1)?.url.searchParams.get("offset")).toBe(
      String(head.length),
    );
    expect(screen.queryByRole("button", { name: "Show the next 1 MiB" })).not.toBeInTheDocument();
  });

  it("downloads the whole log by its exact name, in the largest pieces", async () => {
    const api = mockApi(twoPieces());
    // jsdom has no object URLs. These stay for the rest of this file, whose
    // environment is its own; the page revokes its URL after a delay.
    const saved: Blob[] = [];
    Object.defineProperty(URL, "createObjectURL", {
      configurable: true,
      writable: true,
      value: (blob: Blob) => {
        saved.push(blob);
        return "blob:http://127.0.0.1:4173/log";
      },
    });
    Object.defineProperty(URL, "revokeObjectURL", {
      configurable: true,
      writable: true,
      value: () => undefined,
    });
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);

    const { user } = renderApp(`/logs/${NAME}`);
    await screen.findByRole("heading", { name: "Request log", level: 1 });
    await user.click(screen.getByRole("button", { name: "Download" }));
    await waitFor(() => {
      expect(click).toHaveBeenCalledTimes(1);
    });
    const anchor = click.mock.contexts[0] as HTMLAnchorElement;
    expect(anchor.download).toBe(NAME);
    expect(await saved[0]?.text()).toBe(head + tail);
    const reads = api.callsTo("GET", PATH).slice(1);
    expect(reads.map((call) => call.url.searchParams.get("length"))).toEqual([
      String(DOWNLOAD_PIECE),
      String(DOWNLOAD_PIECE),
    ]);
  });

  it("names an error log as one", async () => {
    const name = "error-v1-messages-2026-10-05T120110-9f8e7d6c.log";
    mockApi(
      route("GET", requestLogPath(name), {
        json: logPiece("x", { log: logEntry({ name, kind: "error", status: 502 }) }),
      }),
    );
    renderApp(`/logs/${name}`);
    expect(await screen.findByRole("heading", { name: "Error log", level: 1 })).toBeVisible();
  });

  it("says when the server has no such log", async () => {
    mockApi(
      route("GET", PATH, { status: 404, json: { error: "not_found", message: "no such log" } }),
    );
    renderApp(`/logs/${NAME}`);
    expect(await screen.findByText("The server has no log by this name")).toBeVisible();
    expect(screen.getByRole("link", { name: "All logs" })).toHaveAttribute("href", "/logs");
  });

  it("explains a log the server won't read", async () => {
    mockApi(
      route("GET", PATH, {
        status: 400,
        json: { error: "invalid_log_file", message: "the log is a symbolic link" },
      }),
    );
    renderApp(`/logs/${NAME}`);
    expect(await screen.findByText("the log is a symbolic link")).toBeVisible();
  });

  it("doesn't ask for a name that can't be a log's", async () => {
    const api = mockApi();
    renderApp(`/logs/${encodeURIComponent("../config.yaml")}`);
    expect(await screen.findByText("The server has no log by this name")).toBeVisible();
    expect(api.calls).toEqual([]);
  });
});

describe("isLogName", () => {
  it("takes a log's name and nothing like a path", () => {
    expect(isLogName(NAME)).toBe(true);
    expect(isLogName("main.log")).toBe(true);
    expect(isLogName("config.yaml")).toBe(false);
    expect(isLogName("..\\main.log")).toBe(false);
    expect(isLogName("a/b.log")).toBe(false);
    expect(isLogName("..log")).toBe(false);
  });
});
