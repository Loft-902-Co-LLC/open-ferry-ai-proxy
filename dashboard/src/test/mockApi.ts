// A stand-in for the server: answers the app's fetch calls from a table of
// routes, and records every call. A call no route takes gets the empty 404
// open-ferry answers for a route it doesn't serve, and is listed in
// `unhandled` so a test can insist there were none.

import { vi } from "vitest";

export interface MockRequest {
  method: string;
  url: URL;
  headers: Headers;
  body: string | null;
  /** A multipart body, as the app built it. */
  form: FormData | null;
  /** The body parsed as JSON. */
  json: () => unknown;
}

export interface MockReply {
  status?: number;
  json?: unknown;
  text?: string;
  /** A body as it is, such as bytes or a stream, for downloads. */
  body?: BodyInit;
  headers?: Record<string, string>;
}

export type MockHandler = (request: MockRequest) => MockReply | Promise<MockReply>;

export interface MockRoute {
  method: string;
  path: string;
  reply: MockReply | MockHandler;
}

/** A route answering `method` `path` (without the query) with `reply`. */
export function route(method: string, path: string, reply: MockReply | MockHandler): MockRoute {
  return { method, path, reply };
}

/** The app's origin in tests. */
export const TEST_ORIGIN = "http://127.0.0.1:4173";

export interface MockApi {
  calls: MockRequest[];
  unhandled: MockRequest[];
  /** Adds routes ahead of the existing ones, so they win. */
  use: (...routes: MockRoute[]) => void;
  /** The calls to `path`, by method. */
  callsTo: (method: string, path: string) => MockRequest[];
}

/** Replaces fetch with `routes` for the rest of the test. */
export function mockApi(...initial: MockRoute[]): MockApi {
  const routes = [...initial];
  const calls: MockRequest[] = [];
  const unhandled: MockRequest[] = [];

  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const raw = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    const body = typeof init?.body === "string" ? init.body : null;
    const request: MockRequest = {
      method: (init?.method ?? "GET").toUpperCase(),
      url: new URL(raw, TEST_ORIGIN),
      headers: new Headers(init?.headers),
      body,
      form: init?.body instanceof FormData ? init.body : null,
      json: () => (body === null ? undefined : (JSON.parse(body) as unknown)),
    };
    calls.push(request);
    const match = routes.find(
      (candidate) => candidate.method === request.method && candidate.path === request.url.pathname,
    );
    if (match === undefined) {
      unhandled.push(request);
      return new Response(null, { status: 404 });
    }
    const reply = typeof match.reply === "function" ? await match.reply(request) : match.reply;
    const headers = new Headers(reply.headers);
    let text: string | null = reply.text ?? null;
    if (reply.json !== undefined) {
      text = JSON.stringify(reply.json);
      if (!headers.has("content-type")) {
        headers.set("content-type", "application/json; charset=utf-8");
      }
    }
    const status = reply.status ?? 200;
    return new Response(status === 204 ? null : (reply.body ?? text), { status, headers });
  });
  vi.stubGlobal("fetch", fetch);

  return {
    calls,
    unhandled,
    use: (...more) => {
      routes.unshift(...more);
    },
    callsTo: (method, path) =>
      calls.filter((call) => call.method === method && call.url.pathname === path),
  };
}

/** A fetch that fails as an unreachable server's does. */
export function mockUnreachable(): void {
  vi.stubGlobal(
    "fetch",
    vi.fn(() => Promise.reject(new TypeError("Failed to fetch"))),
  );
}
