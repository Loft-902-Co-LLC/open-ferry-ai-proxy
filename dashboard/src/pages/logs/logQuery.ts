// A request-log search lives in the page's address, so it can be shared and
// restored with Back. A range is a preset ("24h") or explicit times, as the
// Usage page's "Find" links send.

import type { LogKind, LogSearch } from "../../api/dashboard";
import { RANGE_PRESETS, presetById, rangeEndingAt } from "../../lib/timeRange";
import { z } from "../../lib/zod";

export const ANY_TIME = "all";
/** The range of explicit `from` and `to` in the address. */
export const GIVEN_TIMES = "given";

export const KIND_LABELS: Record<LogKind | "all", string> = {
  all: "All logs",
  request: "Request logs",
  error: "Error logs",
};

export interface LogQuery {
  /** A preset's id, ANY_TIME, or GIVEN_TIMES. */
  range: string;
  from: string;
  to: string;
  kind: LogKind | "all";
  path: string;
  status: string;
  model: string;
  q: string;
}

function isKind(value: string | null): value is LogKind | "all" {
  return value === "all" || value === "request" || value === "error";
}

export function readLogQuery(params: URLSearchParams): LogQuery {
  const from = params.get("from") ?? "";
  const to = params.get("to") ?? "";
  const range = params.get("range");
  const kind = params.get("kind");
  return {
    range:
      range !== null && (range === ANY_TIME || RANGE_PRESETS.some((preset) => preset.id === range))
        ? range
        : from !== "" || to !== ""
          ? GIVEN_TIMES
          : ANY_TIME,
    from,
    to,
    kind: isKind(kind) ? kind : "all",
    path: params.get("path") ?? "",
    status: params.get("status") ?? "",
    model: params.get("model") ?? "",
    q: params.get("q") ?? "",
  };
}

/** The address of a search: only what is set. */
export function writeLogQuery(query: LogQuery): URLSearchParams {
  const params = new URLSearchParams();
  if (query.range === GIVEN_TIMES) {
    if (query.from !== "") {
      params.set("from", query.from);
    }
    if (query.to !== "") {
      params.set("to", query.to);
    }
  } else if (query.range !== ANY_TIME) {
    params.set("range", query.range);
  }
  for (const name of ["kind", "path", "status", "model", "q"] as const) {
    const value = query[name];
    if (value !== "" && !(name === "kind" && value === "all")) {
      params.set(name, value);
    }
  }
  return params;
}

/** The search as the route takes it, a preset's range ending at `now`. */
export function searchParams(query: LogQuery, now: number): Omit<LogSearch, "limit" | "cursor"> {
  let range: { from?: string; to?: string } = {};
  if (query.range === GIVEN_TIMES) {
    range = {
      from: query.from === "" ? undefined : query.from,
      to: query.to === "" ? undefined : query.to,
    };
  } else if (query.range !== ANY_TIME) {
    range = rangeEndingAt(presetById(query.range), now);
  }
  const text = (value: string) => (value.trim() === "" ? undefined : value.trim());
  return {
    ...range,
    kind: query.kind === "all" ? undefined : query.kind,
    path: text(query.path),
    status: text(query.status)?.toLowerCase(),
    model: text(query.model),
    q: text(query.q),
  };
}

/** The search form's rules, as the route's. */
export const logSearchForm = z.object({
  range: z.string(),
  kind: z.enum(["all", "request", "error"]),
  path: z.string(),
  status: z
    .string()
    .trim()
    .refine(
      (value) => value === "" || /^(\d{3}|[1-5]xx)$/i.test(value),
      "A status is three digits, such as 502, or a class, such as 5xx.",
    ),
  model: z.string(),
  q: z.string(),
});
export type LogSearchForm = z.input<typeof logSearchForm>;
