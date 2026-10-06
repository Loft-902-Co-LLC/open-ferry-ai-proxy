// A stand-in for open-ferry in the browser: every call the app makes is
// answered here, through Playwright's request interception, so nothing
// leaves the page but requests for the built app's own files. A call to
// any other origin is refused and recorded, as is an API call this table
// doesn't answer, so a test can insist there were none.

import type { Page, Route } from "@playwright/test";

import type { Credential, ProviderKey } from "../src/api/credentials";
import type { Bucket, LogEntry, Metrics, UsageRequest, UsageSeries } from "../src/api/dashboard";
import {
  clientSetup,
  cooldown,
  credential,
  credentialList,
  ledger,
  logEntry,
  logPiece,
  logSearch,
  metrics,
  prices,
  requestsPage,
  serverLogPage,
  summary,
  usageRequest,
} from "../src/test/fixtures";

/** The management key the tests sign in with; not a real one. */
export const E2E_KEY = "e2e-management-key-0001";

/** What open-ferry says about its build in every management answer. */
const BUILD_HEADERS = {
  "x-cpa-version": "0.1.0-e2e",
  "x-cpa-commit": "e2e0000",
  "x-cpa-build-date": "2026-10-05T00:00:00Z",
};

const API_PREFIXES = ["/v0/management/", "/v8/management/", "/open-ferry/api/v1/"];

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

/** A smooth, repeatable load: busier in the working day. */
function loadAt(time: number, index: number): number {
  const hour = new Date(time).getUTCHours();
  const daytime = Math.max(0, Math.sin(((hour - 6) / 24) * 2 * Math.PI));
  return Math.round(40 + 260 * daytime + 30 * Math.sin(index * 1.7));
}

function pointMetrics(requests: number): Metrics {
  const input = requests * 3100;
  const output = requests * 145;
  return metrics({
    requests,
    errors: Math.round(requests * 0.008),
    input_tokens: input,
    cache_read_tokens: Math.round(input * 0.62),
    cache_write_tokens: Math.round(input * 0.01),
    output_tokens: output,
    reasoning_tokens: Math.round(output * 0.4),
    total_tokens: input + output,
    cost: Math.round(requests * 0.0027 * 10_000) / 10_000,
  });
}

/** A series over the range asked for, in hourly or daily buckets. */
function usageSeries(url: URL): UsageSeries {
  const to = Date.parse(url.searchParams.get("to") ?? "") || Date.now();
  const from = Date.parse(url.searchParams.get("from") ?? "") || to - DAY;
  const bucket: Bucket = to - from > 2 * DAY ? "day" : "hour";
  const step = bucket === "day" ? DAY : HOUR;
  const points = [];
  for (let start = Math.floor(from / step) * step, index = 0; start < to; start += step, index++) {
    const requests = loadAt(start, index) * (bucket === "day" ? 14 : 1);
    points.push({ start: new Date(start).toISOString(), metrics: pointMetrics(requests) });
  }
  return {
    from: new Date(from).toISOString(),
    to: new Date(to).toISOString(),
    bucket,
    bucket_seconds: step / 1000,
    currency: "USD",
    group_by: null,
    series: [{ key: null, label: null, points }],
    more_groups: false,
  };
}

const MODELS = [
  { provider: "codex", model: "gpt-5.1-codex", endpoint: "POST /v1/responses", url: "/v1/responses" },
  { provider: "claude", model: "claude-sonnet-4-5", endpoint: "POST /v1/messages", url: "/v1/messages" },
  { provider: "codex", model: "gpt-5.1-codex", endpoint: "POST /v1/chat/completions", url: "/v1/chat/completions" },
] as const;

/** The last calls, newest first, one in eight failed. */
function recentCalls(now: number): UsageRequest[] {
  return Array.from({ length: 12 }, (_, index) => {
    const pick = MODELS[index % MODELS.length] ?? MODELS[0];
    const failed = index % 8 === 5;
    const id = (0x1234abcd + index * 0x1111).toString(16).padStart(8, "0").slice(-8);
    const input = 8000 + ((index * 3779) % 20_000);
    const output = 300 + ((index * 911) % 1500);
    return usageRequest({
      id: 48_213 - index,
      time: new Date(now - index * 97_000).toISOString(),
      request_id: `0b7c3f4e-5d2a-4c1b-9e8f-${(0x5678 + index).toString(16)}${id}`,
      endpoint: pick.endpoint,
      provider: pick.provider,
      model: pick.model,
      alias: pick.model,
      failed,
      status: failed ? 502 : 200,
      latency_ms: 1800 + ((index * 1373) % 7000),
      ttft_ms: failed ? null : 420 + ((index * 211) % 900),
      tokens: {
        input,
        cache_read: Math.round(input * 0.6),
        cache_write: 0,
        output: failed ? 0 : output,
        reasoning: failed ? 0 : Math.round(output * 0.4),
        total: input + (failed ? 0 : output),
      },
      cost: failed ? 0 : Math.round((input * 1.25 + output * 10) / 10_000) / 100,
    });
  });
}

/** The logs of those calls: error logs for the failed ones. */
function recentLogs(calls: UsageRequest[]): LogEntry[] {
  return calls.map((call) => {
    const id = call.request_id.slice(-8);
    const pick = MODELS.find((candidate) => candidate.endpoint === call.endpoint) ?? MODELS[0];
    // As open-ferry names logs: 2026-10-05T115802.
    const stamp = `${call.time.slice(0, 10)}T${call.time.slice(11, 19).replaceAll(":", "")}`;
    const slug = pick.url.slice(1).replaceAll("/", "-");
    return logEntry({
      name: `${call.failed ? "error-" : ""}${slug}-${stamp}-${id}.log`,
      kind: call.failed ? "error" : "request",
      request_id: id,
      time: call.time,
      modified: call.time,
      url: pick.url,
      status: call.status,
      model: call.model,
    });
  });
}

/** A log's file, byte for byte. */
function logFile(entry: LogEntry): Buffer {
  return Buffer.concat([Buffer.from(logContent(entry), "utf8"), UPLOADED_IMAGE]);
}

/**
 * The end of every log: an image upload's first bytes, which aren't UTF-8,
 * so only a byte-exact download keeps them.
 */
const UPLOADED_IMAGE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0x00, 0xc3]);

function logContent(entry: LogEntry): string {
  return [
    "=== REQUEST INFO ===",
    `Version: 0.1.0-e2e`,
    `URL: ${entry.url ?? ""}`,
    `Method: ${entry.method ?? "POST"}`,
    `Timestamp: ${entry.time}`,
    "",
    "=== HEADERS ===",
    "Content-Type: application/json",
    "Authorization: Bearer sk-...9f3k",
    "",
    "=== REQUEST BODY ===",
    JSON.stringify({ model: entry.model, stream: true, input: "Say hello." }, null, 2),
    "",
    "=== RESPONSE ===",
    `Status: ${String(entry.status ?? 200)}`,
    "Content-Type: text/event-stream",
    "",
    'data: {"type":"response.output_text.delta","delta":"Hello"}',
    'data: {"type":"response.output_text.delta","delta":"!"}',
    'data: {"type":"response.completed"}',
    "",
  ].join("\n");
}

function serverLines(now: number): string[] {
  const at = (offset: number) =>
    new Date(now - offset * 1000).toISOString().replace("T", " ").replace(/\.\d+Z$/, "");
  return [
    `[${at(300)}] [--------] [info ] [main.go:212] open-ferry 0.1.0-e2e listening on 127.0.0.1:18317`,
    `[${at(240)}] [--------] [info ] [watcher.go:88] config.yaml loaded: 3 credentials, 2 client keys`,
    `[${at(180)}] [1234abcd] [info ] [handler.go:141] POST /v1/responses gpt-5.1-codex 200 4.2s`,
    `[${at(120)}] [2345bcde] [warn ] [conductor.go:301] codex: rate limited, retrying with the next credential`,
    `[${at(90)}] [3456cdef] [error] [handler.go:157] POST /v1/messages claude-sonnet-4-5 502 upstream closed the stream`,
    `[${at(60)}] [4567def0] [debug] [usage.go:77] ledger: 1 call recorded`,
    `[${at(5)}] [5678ef01] [info ] [handler.go:141] POST /v1/chat/completions gpt-5.1-codex 200 2.1s`,
  ];
}

/** Requests in the last ten-minute windows, oldest first. */
function recentRequests(now: number, load: number): Credential["recent_requests"] {
  const window = 600_000;
  const time = (start: number) => new Date(start).toISOString().slice(11, 16);
  return Array.from({ length: 6 }, (_, index) => {
    const start = Math.floor(now / window) * window - (5 - index) * window;
    return {
      time: `${time(start)}-${time(start + window)}`,
      success: Math.round(load * (0.6 + 0.4 * Math.sin(index))),
      failed: index === 3 ? 1 : 0,
    };
  });
}

/**
 * The credentials: a Claude sign-in in use with one model resting, a Codex
 * sign-in resting on its quota, and a Claude sign-in that has expired. (The
 * provider API keys in config.yaml aren't listed here: upstream lists files
 * and runtime-only credentials only.)
 */
function credentials(now: number): Credential[] {
  const at = (seconds: number) => new Date(now + seconds * 1000).toISOString();
  return [
    credential({
      supports_quota: true,
      recent_requests: recentRequests(now, 40),
      last_refresh: at(-3480),
      cooldowns: [
        cooldown("model_not_supported", 1500, {
          scope: "model",
          model_key: "claude-opus-4-1",
          retry_at: at(1500),
        }),
      ],
    }),
    credential({
      id: "codex-grace@example.com-pro.json",
      name: "codex-grace@example.com-pro.json",
      auth_index: "b7e1c94a0d2f3658",
      type: "codex",
      provider: "codex",
      label: "grace@example.com",
      email: "grace@example.com",
      account: "grace@example.com",
      success: 8211,
      failed: 64,
      recent_requests: recentRequests(now, 25),
      supports_quota: true,
      unavailable: true,
      next_retry_after: at(2700),
      last_refresh: at(-900),
      id_token: { plan_type: "pro" },
      path: "/srv/open-ferry/auths/codex-grace@example.com-pro.json",
      cooldowns: [cooldown("credential_quota", 2700, { retry_at: at(2700), http_status: 429 })],
    }),
    credential({
      id: "claude-lin@example.com.json",
      name: "claude-lin@example.com.json",
      auth_index: "4f1c0e6b9a2d7c35",
      label: "lin@example.com",
      email: "lin@example.com",
      account: "lin@example.com",
      status: "error",
      status_message: "invalid_grant",
      unavailable: true,
      success: 310,
      failed: 7,
      recent_requests: [],
      last_refresh: at(-9 * 86_400),
      path: "/srv/open-ferry/auths/claude-lin@example.com.json",
    }),
  ];
}

// Placeholders shaped like provider keys; none is real.
const CLAUDE_KEY = "sk-ant-api03-e2e-not-a-real-key-0000-Qw9x";
const CODEX_KEY = "sk-proj-e2e-not-a-real-key-Zt4m";
const GEMINI_KEY = "AIzaSy-e2e-not-a-real-key-7Qx";

/** As the server lists them: with every field, empty or not. */
const PROVIDER_KEYS: Record<string, ProviderKey[]> = {
  "claude-api-key": [
    { "api-key": CLAUDE_KEY, "base-url": "", "proxy-url": "", models: null, "auth-index": "9a8b7c6d5e4f3021" },
  ],
  "codex-api-key": [
    {
      "api-key": CODEX_KEY,
      "base-url": "https://llm.example.com/v1",
      "proxy-url": "",
      prefix: "team",
      models: null,
      "auth-index": "1f2e3d4c5b6a7980",
    },
  ],
  "gemini-api-key": [{ "api-key": GEMINI_KEY, "base-url": "", "auth-index": "5a6b7c8d9e0f1a2b" }],
};

/** What a Claude sign-in's start gives: the provider's page, never opened here. */
const SIGN_IN = {
  status: "ok",
  url: "https://sign-in.example/oauth/authorize?client_id=e2e&state=e2e-state-0001",
  state: "e2e-state-0001",
};

export interface MockOptions {
  /** Whether the server has credentials and keys; else it's a first run. Default true. */
  credentials?: boolean;
}

export interface MockServer {
  /** API calls the table doesn't answer, as "METHOD /path". */
  unhandled: string[];
  /** A log's bytes, exactly as its download sends them. */
  logBytes: (name: string) => Buffer | undefined;
  /** Requests to any origin but the app's, which were refused. */
  offOrigin: string[];
}

/**
 * Answers the app's API calls on `page` and refuses everything that isn't
 * the app's own origin.
 */
export async function mockServer(
  page: Page,
  appOrigin: string,
  options: MockOptions = {},
): Promise<MockServer> {
  const now = Date.now();
  const connected = options.credentials ?? true;
  const calls = recentCalls(now);
  const logs = recentLogs(calls).map((entry) => ({ ...entry, size: logFile(entry).length }));
  const server: MockServer = {
    unhandled: [],
    offOrigin: [],
    logBytes: (name) => {
      const entry = logs.find((log) => log.name === name);
      return entry === undefined ? undefined : logFile(entry);
    },
  };

  const json = (route: Route, body: unknown, status = 200) =>
    route.fulfill({
      status,
      contentType: "application/json; charset=utf-8",
      headers: BUILD_HEADERS,
      body: JSON.stringify(body),
    });

  await page.route("**/*", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    if (url.origin !== appOrigin) {
      server.offOrigin.push(request.url());
      await route.abort("blockedbyclient");
      return;
    }
    if (!API_PREFIXES.some((prefix) => url.pathname.startsWith(prefix))) {
      await route.continue();
      return;
    }
    if (request.headers().authorization !== `Bearer ${E2E_KEY}`) {
      await json(route, { error: "invalid management key" }, 401);
      return;
    }
    const method = request.method();
    const path = url.pathname;
    const call = `${method} ${path}`;
    switch (call) {
      case "GET /v0/management/usage-statistics-enabled":
        return json(route, { "usage-statistics-enabled": true });
      case "GET /v0/management/api-keys":
        return json(route, { "api-keys": ["sk-laptop-4c0b2f7a9e1d9f3k", "sk-ci-runner-8d2e61b0a7c3"] });
      case "GET /v0/management/logs":
        return json(route, serverLogPage(serverLines(now)));
      case "GET /open-ferry/api/v1/client-setup":
        return json(
          route,
          clientSetup({
            base_urls: [
              { url: "http://127.0.0.1:18317", source: "listen" },
              { url: "https://ferry.example.com", source: "config" },
            ],
          }),
        );
      case "GET /open-ferry/api/v1/usage/ledger":
        return json(route, ledger({ newest: calls[0]?.time ?? null }));
      case "GET /open-ferry/api/v1/usage/summary": {
        const totals = pointMetrics(5210);
        return json(
          route,
          summary({
            from: url.searchParams.get("from") ?? new Date(now - DAY).toISOString(),
            to: url.searchParams.get("to") ?? new Date(now).toISOString(),
            totals: { ...totals, latency_ms: { p50: 2140, p95: 9800, p99: 15_320 } },
          }),
        );
      }
      case "GET /open-ferry/api/v1/usage/series":
        return json(route, usageSeries(url));
      case "GET /open-ferry/api/v1/usage/requests":
        return json(route, requestsPage(calls));
      case "GET /open-ferry/api/v1/usage/prices":
        return json(route, prices());
      case "GET /open-ferry/api/v1/request-logs":
        return json(route, logSearch(logs));
      case "GET /v0/management/auth-files":
        return json(route, credentialList(connected ? credentials(now) : []));
      case "POST /v0/management/quota/fetch":
        return json(route, {
          subscription: { plan: "Max", tierName: "20x" },
          groups: [
            {
              displayName: "Usage limits",
              buckets: [
                { window: "5 hours", remainingFraction: 0.62, resetTime: new Date(now + 2 * HOUR).toISOString() },
                { window: "7 days", remainingFraction: 0.81, resetTime: new Date(now + 4 * DAY).toISOString() },
              ],
            },
          ],
        });
      case "GET /v0/management/anthropic-auth-url":
        return json(route, SIGN_IN);
      case "GET /v0/management/get-auth-status":
        return json(route, { status: "wait" });
      case "DELETE /v0/management/oauth-session":
        return json(route, { status: "ok" });
      default:
        break;
    }
    const list = path.slice("/v0/management/".length);
    if (method === "GET" && Object.hasOwn(PROVIDER_KEYS, list)) {
      return json(route, { [list]: connected ? PROVIDER_KEYS[list] : [] });
    }
    const logPrefix = "/open-ferry/api/v1/request-logs/";
    if (method === "GET" && path.startsWith(logPrefix)) {
      const rest = path.slice(logPrefix.length);
      const download = rest.endsWith("/download");
      const name = decodeURIComponent(download ? rest.slice(0, -"/download".length) : rest);
      const entry = logs.find((log) => log.name === name);
      if (entry === undefined) {
        return json(route, { error: "not_found", message: "no such log" }, 404);
      }
      const bytes = server.logBytes(name) ?? Buffer.alloc(0);
      if (download) {
        return route.fulfill({
          status: 200,
          headers: {
            ...BUILD_HEADERS,
            "content-type": "application/octet-stream",
            "content-disposition": `attachment; filename="${name}"`,
            "content-length": String(bytes.length),
            "cache-control": "no-store",
          },
          body: bytes,
        });
      }
      // The piece route shows bytes that aren't UTF-8 as U+FFFD.
      const content = new TextDecoder().decode(bytes);
      return json(route, logPiece(content, { log: entry }));
    }
    server.unhandled.push(call);
    await route.fulfill({ status: 404, body: "" });
  });
  return server;
}
