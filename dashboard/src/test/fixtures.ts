// Answers of the dashboard API, as Contract 1 has them, for tests. Each
// takes overrides so a test states only what it is about.

import type { Cooldown, Credential, CredentialList, QuotaCheck } from "../api/credentials";
import type {
  ClaudeCliEntry,
  ClientSetup,
  LedgerState,
  LogEntry,
  LogPiece,
  LogSearchPage,
  Metrics,
  Prices,
  UsageRequest,
  UsageRequestsPage,
  UsageSeries,
  UsageSummary,
} from "../api/dashboard";
import type { ServerLogPage } from "../api/management";

export function metrics(overrides: Partial<Metrics> = {}): Metrics {
  return {
    requests: 1520,
    errors: 12,
    input_tokens: 4_810_000,
    cache_read_tokens: 3_200_000,
    cache_write_tokens: 41_000,
    output_tokens: 220_400,
    reasoning_tokens: 90_100,
    total_tokens: 5_030_400,
    latency_ms: { p50: 2140, p95: 9800, p99: 15_320 },
    ttft_ms: { p50: 610, p95: 2300, p99: 4100 },
    cost: 4.1825,
    unpriced_requests: 0,
    ...overrides,
  };
}

export function ledger(overrides: Partial<LedgerState> = {}): LedgerState {
  return {
    available: true,
    unavailable_reason: null,
    recording: true,
    usage_statistics_enabled: true,
    file: "/srv/open-ferry/logs/open-ferry-usage.sqlite3",
    size_bytes: 18_350_080,
    rows: 48_213,
    oldest: "2026-07-08T09:12:44.001Z",
    newest: "2026-10-05T11:58:02.114Z",
    retention_days: 90,
    max_rows: 1_000_000,
    currency: "USD",
    dropped_records: 0,
    ...overrides,
  };
}

/** The state of a ledger that couldn't be opened. */
export function unavailableLedger(reason: string): LedgerState {
  return {
    available: false,
    unavailable_reason: reason,
    recording: null,
    usage_statistics_enabled: true,
    file: null,
    size_bytes: null,
    rows: null,
    oldest: null,
    newest: null,
    retention_days: null,
    max_rows: null,
    currency: null,
    dropped_records: null,
  };
}

export function summary(overrides: Partial<UsageSummary> = {}): UsageSummary {
  return {
    from: "2026-10-04T12:00:00.000Z",
    to: "2026-10-05T12:00:00.000Z",
    currency: "USD",
    totals: metrics(),
    group_by: null,
    groups: [],
    more_groups: false,
    ...overrides,
  };
}

export function series(overrides: Partial<UsageSeries> = {}): UsageSeries {
  return {
    from: "2026-10-05T10:00:00.000Z",
    to: "2026-10-05T12:00:00.000Z",
    bucket: "hour",
    bucket_seconds: 3600,
    currency: "USD",
    group_by: null,
    series: [
      {
        key: null,
        label: null,
        points: [
          { start: "2026-10-05T10:00:00.000Z", metrics: metrics({ requests: 700 }) },
          { start: "2026-10-05T11:00:00.000Z", metrics: metrics({ requests: 820 }) },
        ],
      },
    ],
    more_groups: false,
    ...overrides,
  };
}

export function usageRequest(overrides: Partial<UsageRequest> = {}): UsageRequest {
  return {
    id: 48_213,
    time: "2026-10-05T11:58:02.114Z",
    request_id: "0b7c3f4e-5d2a-4c1b-9e8f-1234abcd",
    endpoint: "POST /v1/chat/completions",
    provider: "codex",
    model: "gpt-5.1-codex",
    alias: "gpt-5.1-codex",
    stream: true,
    failed: false,
    status: 200,
    latency_ms: 4210,
    ttft_ms: 640,
    credential: {
      id: "codex-user@example.com.json",
      auth_index: "3",
      label: "user@example.com",
      auth_type: "oauth",
    },
    client_key: { id: "ck_5f0a3c19e2b7d468", masked: "sk-...9f3k" },
    tokens: { input: 12_400, cache_read: 9000, cache_write: 0, output: 830, reasoning: 512, total: 13_230 },
    cost: 0.0213,
    ...overrides,
  };
}

export function requestsPage(
  requests: UsageRequest[],
  next_cursor: string | null = null,
): UsageRequestsPage {
  return { requests, next_cursor };
}

export function logEntry(overrides: Partial<LogEntry> = {}): LogEntry {
  return {
    name: "v1-chat-completions-2026-10-05T115802-1234abcd.log",
    kind: "request",
    request_id: "1234abcd",
    time: "2026-10-05T11:58:02.000Z",
    size: 48_120,
    modified: "2026-10-05T11:58:06.271Z",
    method: "POST",
    url: "/v1/chat/completions",
    status: 200,
    model: "gpt-5.1-codex",
    ...overrides,
  };
}

export function logSearch(
  logs: LogEntry[],
  overrides: Partial<LogSearchPage> = {},
): LogSearchPage {
  return {
    logs,
    next_cursor: null,
    scanned: { files: logs.length, bytes: 48_120 * logs.length, limit_reached: false },
    request_log: true,
    ...overrides,
  };
}

/** A piece of `log` at `offset`: the rest of it unless `next_offset` says. */
export function logPiece(content: string, overrides: Partial<LogPiece> = {}): LogPiece {
  const offset = overrides.offset ?? 0;
  return {
    log: logEntry({ size: offset + content.length }),
    offset,
    next_offset: null,
    content,
    ...overrides,
  };
}

/** An answer of upstream's `GET /v0/management/logs`. */
export function serverLogPage(lines: string[], overrides: Partial<ServerLogPage> = {}): ServerLogPage {
  return {
    lines,
    "line-count": lines.length,
    "latest-timestamp": 1_791_201_482,
    "next-cursor": "cursor-1",
    ...overrides,
  };
}

export function prices(overrides: Partial<Prices> = {}): Prices {
  return {
    currency: "USD",
    prices: [
      {
        model: "gpt-5.1-codex",
        input: 1.25,
        cache_read: 0.125,
        cache_write: null,
        output: 10,
        updated: "2026-10-01T08:00:00.000Z",
      },
    ],
    unpriced_models: ["claude-sonnet-4-5"],
    ...overrides,
  };
}

export function clientSetup(overrides: Partial<ClientSetup> = {}): ClientSetup {
  const models = ["claude-sonnet-4-5", "gpt-5.1-codex"];
  return {
    base_urls: [
      { url: "http://127.0.0.1:8317", source: "listen" },
      { url: "http://localhost:8317", source: "listen" },
      { url: "https://proxy.example.com", source: "config" },
    ],
    tls: false,
    safe_mode: false,
    separate_management: false,
    routes: [
      { id: "claude-messages", protocol: "claude", method: "POST", path: "/v1/messages", base_path: "", models },
      { id: "codex-responses", protocol: "codex", method: "POST", path: "/backend-api/codex/responses", base_path: "/backend-api/codex", models },
      { id: "gemini-generate-content", protocol: "gemini", method: "POST", path: "/v1beta/models/{model}:generateContent", base_path: "", models },
      { id: "openai-chat-completions", protocol: "openai", method: "POST", path: "/v1/chat/completions", base_path: "/v1", models },
      { id: "openai-responses", protocol: "openai-responses", method: "POST", path: "/v1/responses", base_path: "/v1", models },
    ],
    models: [
      { id: "claude-sonnet-4-5", display_name: "Claude Sonnet 4.5", owned_by: "anthropic", providers: ["claude"], created: 1_759_104_000, chat: true, context_length: 200_000, max_output_tokens: 64_000 },
      { id: "gpt-5.1-codex", display_name: "GPT 5.1 Codex", owned_by: "openai", providers: ["codex"], created: 1_762_992_000, chat: true, context_length: 400_000, max_output_tokens: 128_000 },
    ],
    ...overrides,
  };
}

/** A Claude sign-in in use, as `GET auth-files` lists one. */
export function credential(overrides: Partial<Credential> = {}): Credential {
  const name = overrides.name ?? "claude-ada@example.com.json";
  return {
    id: name,
    auth_index: "a1b2c3d4e5f60718",
    name,
    type: "claude",
    provider: "claude",
    label: "ada@example.com",
    status: "active",
    status_message: "",
    disabled: false,
    unavailable: false,
    runtime_only: false,
    source: "file",
    size: 1840,
    success: 1520,
    failed: 12,
    recent_requests: [
      { time: "11:40-11:50", success: 30, failed: 1 },
      { time: "11:50-12:00", success: 12, failed: 0 },
    ],
    supports_quota: false,
    email: "ada@example.com",
    account_type: "oauth",
    account: "ada@example.com",
    modtime: "2026-10-04T08:15:00.000Z",
    last_refresh: "2026-10-05T11:02:00.000Z",
    path: "/srv/open-ferry/auths/claude-ada@example.com.json",
    cooldowns: [],
    ...overrides,
  };
}

/** A cooldown ending `seconds` after 2026-10-05T12:00:00Z. */
export function cooldown(reason: string, seconds: number, overrides: Partial<Cooldown> = {}): Cooldown {
  return {
    scope: "credential",
    reason,
    retry_at: new Date(Date.parse("2026-10-05T12:00:00.000Z") + seconds * 1000).toISOString(),
    remaining_seconds: seconds,
    ...overrides,
  };
}

/**
 * A quota rest `routing.quota.check-after` caps, in `state`: of the whole
 * credential, checked at 13:00 on 2026-10-05, an hour after the lists are
 * read, rather than at the provider's reset four days on.
 */
export function quotaCheck(
  state: QuotaCheck["state"],
  overrides: Partial<QuotaCheck> = {},
): QuotaCheck {
  return {
    scope: "credential",
    state,
    next_check_at: "2026-10-05T13:00:00.123456789Z",
    provider_reset_at: "2026-10-09T08:00:00Z",
    wait_seconds: 3600,
    ...overrides,
  };
}

/** `GET auth-files` with `files`. */
export function credentialList(files: Credential[]): CredentialList {
  return { files, observed_at: "2026-10-05T12:00:00.000Z" };
}

/**
 * The credential of the `claude-cli` entry `claude-max-1`, in use, as
 * `GET claude-cli/entries` gives it: made from config.yaml, so kept in
 * memory, with no file and no account.
 */
export function claudeCliCredential(overrides: Partial<Credential> = {}): Credential {
  return {
    id: "claude-cli:479b4a4c3660",
    auth_index: "3734a62b508f0029",
    name: "claude-cli:479b4a4c3660",
    type: "claude-cli",
    provider: "claude-cli",
    label: "claude-max-1",
    status: "active",
    status_message: "",
    disabled: false,
    unavailable: false,
    runtime_only: false,
    source: "memory",
    size: 0,
    success: 214,
    failed: 3,
    recent_requests: [
      { time: "11:40-11:50", success: 9, failed: 0 },
      { time: "11:50-12:00", success: 4, failed: 1 },
    ],
    account_type: "api_key",
    created_at: "2026-10-05T08:00:00.123456789Z",
    modtime: "2026-10-05T11:59:30.5Z",
    updated_at: "2026-10-05T11:59:30.5Z",
    quota: { signals: {} },
    cooldowns: [],
    ...overrides,
  };
}

/** The `claude-cli` entry `claude-max-1`, in use, as `GET claude-cli/entries` lists it. */
export function claudeCliEntry(overrides: Partial<ClaudeCliEntry> = {}): ClaudeCliEntry {
  return {
    name: "claude-max-1",
    prefix: "",
    config_dir: "",
    disabled: false,
    credential: claudeCliCredential({ label: overrides.name ?? "claude-max-1" }),
    last_error: null,
    ...overrides,
  };
}
