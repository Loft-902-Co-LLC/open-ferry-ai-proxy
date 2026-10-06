import { describe, expect, it } from "vitest";

import { mockApi, mockUnreachable, route } from "../test/mockApi";
import {
  ApiError,
  ShortDownloadError,
  apiDownload,
  apiRequest,
  apiUrl,
  isShortDownload,
  isUnsupportedRoute,
} from "./client";

describe("apiUrl", () => {
  it("builds API paths with their query", () => {
    expect(apiUrl("/v0/management/logs", { limit: 10, after: undefined, all: true })).toBe(
      "/v0/management/logs?limit=10&all=true",
    );
    expect(apiUrl("/open-ferry/api/v1/usage/summary")).toBe("/open-ferry/api/v1/usage/summary");
  });

  it("refuses anything outside the two APIs", () => {
    expect(() => apiUrl("/v1/chat/completions")).toThrow(/not an API path/);
    expect(() => apiUrl("https://example.test/v0/management/x")).toThrow(/not an API path/);
    expect(() => apiUrl("/v0/management/../v1/models")).toThrow(/not an API path/);
    expect(() => apiUrl("/open-ferry/api/v2/x")).toThrow(/not an API path/);
  });
});

describe("apiRequest", () => {
  it("sends the key as a bearer token, without cookies or caching", async () => {
    const api = mockApi(route("GET", "/v0/management/debug", { json: { debug: true } }));
    const response = await apiRequest<{ debug: boolean }>("k-1", "/v0/management/debug");
    expect(response.data).toEqual({ debug: true });
    const [call] = api.calls;
    expect(call?.headers.get("authorization")).toBe("Bearer k-1");
    const init = (globalThis.fetch as unknown as { mock: { calls: [string, RequestInit][] } }).mock
      .calls[0]?.[1];
    expect(init).toMatchObject({
      credentials: "omit",
      cache: "no-store",
      redirect: "error",
      referrerPolicy: "no-referrer",
    });
  });

  it("sends JSON bodies as JSON", async () => {
    const api = mockApi(route("PUT", "/v0/management/debug", { json: { status: "ok" } }));
    await apiRequest("k", "/v0/management/debug", { method: "PUT", json: { value: true } });
    expect(api.calls[0]?.headers.get("content-type")).toBe("application/json");
    expect(api.calls[0]?.json()).toEqual({ value: true });
  });

  it("reads both error shapes", async () => {
    mockApi(
      route("GET", "/v0/management/x", { status: 401, json: { error: "invalid management key" } }),
      route("GET", "/open-ferry/api/v1/usage/summary", {
        status: 400,
        json: { error: "invalid_request", message: "from must be before to" },
      }),
    );
    const management = await apiRequest("k", "/v0/management/x").catch((e: unknown) => e);
    expect(management).toBeInstanceOf(ApiError);
    expect(management).toMatchObject({ status: 401, code: "invalid management key", detail: null });
    const dashboard = await apiRequest("k", "/open-ferry/api/v1/usage/summary").catch(
      (e: unknown) => e,
    );
    expect(dashboard).toMatchObject({
      status: 400,
      code: "invalid_request",
      detail: "from must be before to",
    });
  });

  it("tells an empty 404 from a named one", async () => {
    mockApi(
      route("GET", "/open-ferry/api/v1/request-logs/x.log", {
        status: 404,
        json: { error: "not_found", message: "no such log" },
      }),
    );
    const empty = await apiRequest("k", "/v0/management/unknown").catch((e: unknown) => e);
    expect(isUnsupportedRoute(empty)).toBe(true);
    const named = await apiRequest("k", "/open-ferry/api/v1/request-logs/x.log").catch(
      (e: unknown) => e,
    );
    expect(isUnsupportedRoute(named)).toBe(false);
  });

  it("turns no answer into status 0", async () => {
    mockUnreachable();
    await expect(apiRequest("k", "/v0/management/debug")).rejects.toMatchObject({ status: 0 });
  });
});

describe("apiDownload", () => {
  const PATH = "/open-ferry/api/v1/request-logs/x.log/download";
  // Bytes that aren't UTF-8, which a text answer would replace.
  const BYTES = new Uint8Array([0x3d, 0x3d, 0x0a, 0xff, 0x00, 0x89, 0x50, 0x4e, 0x47, 0xc3]);

  function file(bytes: BodyInit, length: number | null, extra: Record<string, string> = {}) {
    return {
      body: bytes,
      headers: {
        "content-type": "application/octet-stream",
        "content-disposition": 'attachment; filename="x.log"',
        ...(length === null ? {} : { "content-length": String(length) }),
        ...extra,
      },
    };
  }

  /** A body that sends `first`, then breaks off. */
  function breaking(first: Uint8Array): ReadableStream<Uint8Array> {
    let sent = false;
    return new ReadableStream({
      pull(controller) {
        if (sent) {
          controller.error(new TypeError("network error"));
          return;
        }
        sent = true;
        controller.enqueue(first);
      },
    });
  }

  it("hands back the bytes exactly, with the key in a header only", async () => {
    const api = mockApi(route("GET", PATH, file(BYTES, BYTES.length)));
    const progress: unknown[] = [];
    const blob = await apiDownload("k-1", PATH, { onProgress: (p) => progress.push(p) });
    expect(new Uint8Array(await blob.arrayBuffer())).toEqual(BYTES);
    const [call] = api.calls;
    expect(call?.headers.get("authorization")).toBe("Bearer k-1");
    expect(call?.headers.get("accept")).toBe("application/octet-stream, application/json");
    expect(call?.url.search).toBe("");
    expect(progress.at(0)).toEqual({ received: 0, total: BYTES.length });
    expect(progress.at(-1)).toEqual({ received: BYTES.length, total: BYTES.length });
  });

  it("refuses a body shorter than its Content-Length", async () => {
    mockApi(route("GET", PATH, file(BYTES.slice(0, 4), BYTES.length)));
    const error = await apiDownload("k", PATH).catch((e: unknown) => e);
    expect(isShortDownload(error)).toBe(true);
    expect(error).toMatchObject({ received: 4, expected: BYTES.length });
  });

  it("refuses a body that breaks off", async () => {
    mockApi(route("GET", PATH, file(breaking(BYTES.slice(0, 3)), null)));
    const error = await apiDownload("k", PATH).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ShortDownloadError);
    expect(error).toMatchObject({ received: 3, expected: null });
  });

  it("doesn't hold a compressed body to its Content-Length", async () => {
    mockApi(route("GET", PATH, file(BYTES, 4, { "content-encoding": "gzip" })));
    const progress: unknown[] = [];
    const blob = await apiDownload("k", PATH, { onProgress: (p) => progress.push(p) });
    expect(blob.size).toBe(BYTES.length);
    expect(progress.at(-1)).toEqual({ received: BYTES.length, total: null });
  });

  it("reads the API's errors", async () => {
    mockApi(
      route("GET", PATH, { status: 400, json: { error: "invalid_log_file", message: "a link" } }),
    );
    await expect(apiDownload("k", PATH)).rejects.toMatchObject({
      status: 400,
      code: "invalid_log_file",
      detail: "a link",
    });
  });
});
