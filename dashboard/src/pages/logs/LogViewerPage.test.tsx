import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { requestLogDownloadPath, requestLogPath } from "../../api/dashboard";
import { formatBytes } from "../../lib/format";
import { logEntry, logPiece } from "../../test/fixtures";
import { mockApi, route, type MockReply } from "../../test/mockApi";
import { TEST_KEY, renderApp } from "../../test/renderApp";
import { REVOKE_AFTER_MS, isLogName, saveBlob } from "./LogViewerPage";

const NAME = "v1-chat-completions-2026-10-05T115802-1234abcd.log";
const PATH = requestLogPath(NAME);
const DOWNLOAD = requestLogDownloadPath(NAME);

/** The download route's answer: `bytes`, announced as `length` long. */
function file(bytes: Uint8Array<ArrayBuffer>, length: number): MockReply {
  return {
    body: bytes,
    headers: {
      "content-type": "application/octet-stream",
      // A name the app must not use: it saves under the listed one.
      "content-disposition": 'attachment; filename="other.log"',
      "content-length": String(length),
    },
  };
}

/** Object URLs, which jsdom lacks: records each blob and each revoke. */
function stubObjectUrls() {
  const saved: Blob[] = [];
  const revoked: string[] = [];
  vi.stubGlobal(
    "URL",
    Object.assign(
      class extends URL {},
      {
        createObjectURL: (blob: Blob) => {
          saved.push(blob);
          return `blob:http://127.0.0.1:4173/${String(saved.length)}`;
        },
        revokeObjectURL: (url: string) => {
          revoked.push(url);
        },
      },
    ),
  );
  return { saved, revoked };
}

const head = "=== REQUEST INFO ===\nURL: /v1/chat/completions\n";
const tail = "=== RESPONSE ===\nStatus: 200\n";
const SIZE = head.length + tail.length;

/** A log of `head` then `tail`, served in pieces split after `head`. */
function twoPieces() {
  return route("GET", PATH, (request) => {
    const offset = Number(request.url.searchParams.get("offset") ?? "0");
    const log = logEntry({ size: SIZE });
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

    const content = screen.getByRole("region", { name: "The log's content" });
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

  it("downloads the log byte for byte, by its exact name", async () => {
    // Bytes that aren't UTF-8, as an image upload's body is.
    const bytes = new Uint8Array([0x3d, 0x0a, 0xff, 0x00, 0x89, 0x50, 0x4e, 0x47]);
    const api = mockApi(twoPieces(), route("GET", DOWNLOAD, file(bytes, bytes.length)));
    const { saved } = stubObjectUrls();
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);

    const { user } = renderApp(`/logs/${NAME}`);
    await screen.findByRole("heading", { name: "Request log", level: 1 });
    await user.click(screen.getByRole("button", { name: "Download" }));
    await waitFor(() => {
      expect(click).toHaveBeenCalledTimes(1);
    });
    // Saved under the name it was listed by, not the header's.
    expect((click.mock.contexts[0] as HTMLAnchorElement).download).toBe(NAME);
    expect(saved).toHaveLength(1);
    expect(new Uint8Array(await (saved.at(0) ?? new Blob()).arrayBuffer())).toEqual(bytes);
    const [call] = api.callsTo("GET", DOWNLOAD);
    expect(call?.headers.get("authorization")).toBe(`Bearer ${TEST_KEY}`);
    expect(call?.url.search).toBe("");
    expect(api.callsTo("GET", PATH)).toHaveLength(1);
  });

  it("shows the size while it downloads", async () => {
    let finish: (reply: MockReply) => void = () => undefined;
    const body = new Uint8Array(SIZE);
    mockApi(
      twoPieces(),
      route("GET", DOWNLOAD, () => new Promise<MockReply>((resolve) => (finish = resolve))),
    );
    stubObjectUrls();
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);

    const { user } = renderApp(`/logs/${NAME}`);
    await screen.findByRole("heading", { name: "Request log", level: 1 });
    await user.click(screen.getByRole("button", { name: "Download" }));
    expect(await screen.findByText(`Downloading the log, ${formatBytes(SIZE)}.`)).toBeVisible();
    expect(screen.getByRole("progressbar", { name: "Downloaded so far" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Downloading…" })).toBeDisabled();

    finish(file(body, body.length));
    await waitFor(() => {
      expect(click).toHaveBeenCalledTimes(1);
    });
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
  });

  it("saves nothing when the download stops short", async () => {
    const bytes = new Uint8Array([1, 2, 3]);
    mockApi(twoPieces(), route("GET", DOWNLOAD, file(bytes, SIZE)));
    const { saved } = stubObjectUrls();
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);

    const { user } = renderApp(`/logs/${NAME}`);
    await screen.findByRole("heading", { name: "Request log", level: 1 });
    await user.click(screen.getByRole("button", { name: "Download" }));
    expect(await screen.findByText("The download stopped short")).toBeVisible();
    expect(screen.getByText(/^The server sent 3 B of /)).toHaveTextContent(
      `The server sent 3 B of ${formatBytes(SIZE)}; the log may have got shorter while it was sent. Nothing was saved. Try again.`,
    );
    expect(click).not.toHaveBeenCalled();
    expect(saved).toEqual([]);
  });

  it("explains a download the server refuses", async () => {
    mockApi(
      twoPieces(),
      route("GET", DOWNLOAD, {
        status: 400,
        json: { error: "invalid_log_file", message: "the log has another hard link" },
      }),
    );
    const { user } = renderApp(`/logs/${NAME}`);
    await screen.findByRole("heading", { name: "Request log", level: 1 });
    await user.click(screen.getByRole("button", { name: "Download" }));
    expect(await screen.findByText("the log has another hard link")).toBeVisible();
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

describe("saveBlob", () => {
  it("saves under the name given, then lets the object URL go", () => {
    vi.useFakeTimers();
    try {
      const { saved, revoked } = stubObjectUrls();
      const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);
      const blob = new Blob(["x"]);
      saveBlob(blob, NAME);
      expect(saved).toEqual([blob]);
      const anchor = click.mock.contexts[0] as HTMLAnchorElement;
      expect(anchor.download).toBe(NAME);
      expect(anchor.isConnected).toBe(false);
      expect(revoked).toEqual([]);
      vi.advanceTimersByTime(REVOKE_AFTER_MS);
      expect(revoked).toEqual(["blob:http://127.0.0.1:4173/1"]);
    } finally {
      vi.useRealTimers();
    }
  });
});
