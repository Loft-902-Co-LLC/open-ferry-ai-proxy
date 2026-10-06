// The dashboard API's shapes, as docs/dashboard-api.md (Contract 1) has
// them, under /open-ferry/api/v1/.

export const DASHBOARD_API = "/open-ferry/api/v1";

export const USAGE_SUMMARY = `${DASHBOARD_API}/usage/summary`;
export const USAGE_SERIES = `${DASHBOARD_API}/usage/series`;
export const USAGE_REQUESTS = `${DASHBOARD_API}/usage/requests`;
export const USAGE_LEDGER = `${DASHBOARD_API}/usage/ledger`;
export const USAGE_RECORDS = `${DASHBOARD_API}/usage/records`;
export const USAGE_PRICES = `${DASHBOARD_API}/usage/prices`;
export const REQUEST_LOGS = `${DASHBOARD_API}/request-logs`;
export const CLIENT_SETUP = `${DASHBOARD_API}/client-setup`;

/** The path of one request log, by its name as listed. */
export function requestLogPath(name: string): string {
  return `${REQUEST_LOGS}/${encodeURIComponent(name)}`;
}

// ---------------------------------------------------------------- usage

export type GroupBy = "model" | "provider" | "credential" | "client_key";

/** The filters usage summary, series and requests share. */
export interface UsageFilters {
  from?: string;
  to?: string;
  model?: string;
  provider?: string;
  credential?: string;
  client_key?: string;
}

export interface Percentiles {
  p50: number;
  p95: number;
  p99: number;
}

/** The sums over a set of calls. */
export interface Metrics {
  requests: number;
  errors: number;
  input_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  total_tokens: number;
  latency_ms: Percentiles | null;
  ttft_ms: Percentiles | null;
  cost: number | null;
  unpriced_requests: number;
}

export interface CredentialRef {
  id: string;
  auth_index: string;
  label: string;
  auth_type: string;
}

export interface ClientKeyRef {
  id: string;
  masked: string;
}

export interface UsageGroup {
  key: string;
  label: string;
  metrics: Metrics;
  credential?: CredentialRef;
  client_key?: ClientKeyRef;
}

export interface UsageSummary {
  from: string;
  to: string;
  currency: string;
  totals: Metrics;
  group_by: GroupBy | null;
  groups: UsageGroup[];
  more_groups: boolean;
}

export type Bucket = "minute" | "hour" | "day";

export interface SeriesPoint {
  start: string;
  metrics: Metrics;
}

export interface Series {
  key: string | null;
  label: string | null;
  points: SeriesPoint[];
}

export interface UsageSeries {
  from: string;
  to: string;
  bucket: Bucket;
  bucket_seconds: number;
  currency: string;
  group_by: GroupBy | null;
  series: Series[];
  more_groups: boolean;
}

export interface UsageRequest {
  id: number;
  time: string;
  request_id: string;
  endpoint: string;
  provider: string;
  model: string;
  alias: string;
  stream: boolean;
  failed: boolean;
  status: number;
  latency_ms: number;
  ttft_ms: number | null;
  credential: CredentialRef | null;
  client_key: ClientKeyRef | null;
  tokens: {
    input: number;
    cache_read: number;
    cache_write: number;
    output: number;
    reasoning: number;
    total: number;
  };
  cost: number | null;
}

export interface UsageRequestsPage {
  requests: UsageRequest[];
  next_cursor: string | null;
}

export interface LedgerState {
  available: boolean;
  unavailable_reason: string | null;
  recording: boolean | null;
  usage_statistics_enabled: boolean;
  file: string | null;
  size_bytes: number | null;
  rows: number | null;
  oldest: string | null;
  newest: string | null;
  retention_days: number | null;
  max_rows: number | null;
  currency: string | null;
  dropped_records: number | null;
}

export interface LedgerSettings {
  retention_days?: number;
  max_rows?: number;
  currency?: string;
}

export interface Price {
  model: string;
  input: number;
  cache_read: number | null;
  cache_write: number | null;
  output: number;
  updated: string;
}

export interface Prices {
  currency: string;
  prices: Price[];
  unpriced_models: string[];
}

export type PriceInput = Omit<Price, "updated">;

// --------------------------------------------------------- request logs

export type LogKind = "request" | "error";

export interface LogEntry {
  name: string;
  kind: LogKind;
  request_id: string;
  time: string;
  size: number;
  modified: string;
  method: string | null;
  url: string | null;
  status: number | null;
  model: string | null;
}

export interface LogSearch {
  from?: string;
  to?: string;
  kind?: LogKind | "all";
  path?: string;
  status?: string;
  model?: string;
  q?: string;
  limit?: number;
  cursor?: string;
}

export interface LogSearchPage {
  logs: LogEntry[];
  next_cursor: string | null;
  scanned: { files: number; bytes: number; limit_reached: boolean };
  request_log: boolean;
}

export interface LogPiece {
  log: LogEntry;
  offset: number;
  next_offset: number | null;
  content: string;
}

/** A log's short request ID: the last eight characters of the full one. */
export function shortRequestId(requestId: string): string {
  return requestId.slice(-8);
}

// ---------------------------------------------------------- client setup

export interface BaseUrl {
  url: string;
  source: "listen" | "config";
}

export type RouteProtocol = "openai" | "openai-responses" | "claude" | "gemini" | "codex";

export interface ProxyRoute {
  id: string;
  protocol: RouteProtocol | (string & {});
  method: string;
  path: string;
  base_path: string;
  models: string[];
}

export interface ModelInfo {
  id: string;
  display_name: string | null;
  owned_by: string | null;
  providers: string[];
  context_length: number | null;
  max_output_tokens: number | null;
}

export interface ClientSetup {
  base_urls: BaseUrl[];
  tls: boolean;
  safe_mode: boolean;
  routes: ProxyRoute[];
  models: ModelInfo[];
}
