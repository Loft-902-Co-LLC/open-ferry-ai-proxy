// A stand-in for the server: answers the app's fetch calls from a table of
// routes, and records every call. A call no route takes gets the empty 404
// open-ferry answers for a route it doesn't serve, and is listed in
// `unhandled` so a test can insist there were none.

import { vi } from "vitest";

import { V8_CONFIG } from "../api/management";

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

/** The mapping at `keys` in `tree`, made as needed when `make` is set. */
function mappingAt(
  tree: Record<string, unknown>,
  keys: readonly string[],
  make: boolean,
): Record<string, unknown> | undefined {
  let node = tree;
  for (const key of keys) {
    const next = node[key];
    if (next !== null && typeof next === "object" && !Array.isArray(next)) {
      node = next as Record<string, unknown>;
    } else if (make) {
      const made: Record<string, unknown> = {};
      node[key] = made;
      node = made;
    } else {
      return undefined;
    }
  }
  return node;
}

/**
 * `GET <V8_CONFIG>`, and `GET` and `PUT <V8_CONFIG>/<path>` for each of
 * `paths`, over `tree`, config.yaml in the v8 layout, as the server answers
 * them: the whole GET gives the tree with its `config-version`; a path's
 * GET gives the value at the path, or 404 `not_found` where there is none;
 * a PUT sets it to the body, the bare JSON value, in place, so a mapping
 * `tree` shares with another answer changes there too.
 */
export function v8ConfigRoutes(
  tree: Record<string, unknown>,
  paths: readonly string[],
): MockRoute[] {
  const whole = route("GET", V8_CONFIG, () => ({ json: { "config-version": 8, ...tree } }));
  return [whole, ...paths.flatMap((path) => {
    const keys = path.split("/");
    const parents = keys.slice(0, -1);
    const last = keys.at(-1) ?? "";
    return [
      route("GET", `${V8_CONFIG}/${path}`, () => {
        const value = mappingAt(tree, parents, false)?.[last];
        return value === undefined
          ? { status: 404, json: { error: "not_found" } }
          : { json: value };
      }),
      route("PUT", `${V8_CONFIG}/${path}`, (request) => {
        const parent = mappingAt(tree, parents, true);
        if (parent !== undefined) {
          parent[last] = request.json();
        }
        return { json: { "config-version": 8, status: "ok" } };
      }),
    ];
  })];
}

/** A fetch that fails as an unreachable server's does. */
export function mockUnreachable(): void {
  vi.stubGlobal(
    "fetch",
    vi.fn(() => Promise.reject(new TypeError("Failed to fetch"))),
  );
}
