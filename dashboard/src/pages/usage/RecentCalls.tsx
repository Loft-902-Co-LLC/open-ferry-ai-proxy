import { useInfiniteQuery } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { useMemo, useState } from "react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import {
  REQUEST_LOGS,
  USAGE_REQUESTS,
  shortRequestId,
  type LogEntry,
  type LogSearchPage,
  type UsageFilters,
  type UsageRequest,
  type UsageRequestsPage,
} from "../../api/dashboard";
import { useApiCall, useApiQuery } from "../../api/hooks";
import { REQUEST_LOG_SETTING } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { BreakableText, NAME_IN_TABLE } from "../../components/BreakableText";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Checkbox } from "../../components/CheckboxField";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { Table, Td, Th } from "../../components/Table";
import { TurnOnSetting } from "../../components/TurnOnSetting";
import {
  formatCompact,
  formatCost,
  formatDateTime,
  formatInteger,
  formatMillis,
  formatShortDateTime,
} from "../../lib/format";
import { rangeEndingAt, type RangePreset } from "../../lib/timeRange";

const PAGE_SIZE = 50;
/** How far a log's time may be from its call's, either way. */
const LOG_WINDOW_MS = 5 * 60_000;
/** The most logs one search lists, as the route allows. */
const LOG_SEARCH_LIMIT = 200;

/** The span of time a call's log may be named in: from its start, less a
 * margin, to its end plus one. A log is named by local time when it is
 * written, which may be after the call. */
function logSpan(rows: readonly Pick<UsageRequest, "time" | "latency_ms">[]) {
  let first = Number.POSITIVE_INFINITY;
  let last = Number.NEGATIVE_INFINITY;
  for (const row of rows) {
    const start = Date.parse(row.time);
    if (!Number.isNaN(start)) {
      first = Math.min(first, start);
      last = Math.max(last, start + row.latency_ms);
    }
  }
  if (first > last) {
    return null;
  }
  return {
    from: new Date(first - LOG_WINDOW_MS).toISOString(),
    to: new Date(last + LOG_WINDOW_MS).toISOString(),
  };
}

/** Where to look for one call's log, when the search here didn't find it. */
export function logSearchLink(row: UsageRequest): string {
  const span = logSpan([row]);
  const params = new URLSearchParams(span ?? {});
  return `/logs?${params.toString()}`;
}

/**
 * The request logs of the calls on screen, by short request ID. One search
 * by time covers them all: a search by `from` and `to` reads only names.
 */
function useLogIndex(rows: readonly UsageRequest[]) {
  const span = useMemo(() => logSpan(rows), [rows]);
  const search = useApiQuery<LogSearchPage>(
    REQUEST_LOGS,
    { ...span, limit: LOG_SEARCH_LIMIT },
    { enabled: span !== null },
  );
  const index = useMemo(() => {
    const byId = new Map<string, LogEntry>();
    for (const log of search.data?.logs ?? []) {
      // The full request log says more than the error log of the same call.
      const known = byId.get(log.request_id);
      if (known === undefined || (known.kind === "error" && log.kind === "request")) {
        byId.set(log.request_id, log);
      }
    }
    return byId;
  }, [search.data]);
  return {
    index,
    requestLog: search.data?.request_log ?? null,
    /** Whether every log of the span was listed, so a missing one has none. */
    complete: search.data?.next_cursor === null,
  };
}

function LogCell({
  row,
  log,
  requestLog,
  complete,
}: {
  row: UsageRequest;
  log: LogEntry | undefined;
  requestLog: boolean | null;
  complete: boolean;
}) {
  if (log !== undefined) {
    return (
      <Link to={`/logs/${encodeURIComponent(log.name)}`} aria-label={`Log of request ${log.request_id}`}>
        {log.kind === "error" ? "Error log" : "Log"}
      </Link>
    );
  }
  if (!complete) {
    return (
      <Link to={logSearchLink(row)} aria-label={`Find the log of request ${shortRequestId(row.request_id)}`}>
        Find
      </Link>
    );
  }
  const why =
    requestLog === false && !row.failed
      ? "No log: only failed requests are logged while request-log is off"
      : "No log found";
  return (
    <span title={why}>
      <span aria-hidden="true" className="text-muted">
        –
      </span>
      <span className="sr-only">{why}</span>
    </span>
  );
}

function CallRow({
  row,
  currency,
  log,
  requestLog,
  complete,
}: {
  row: UsageRequest;
  currency: string;
  log: LogEntry | undefined;
  requestLog: boolean | null;
  complete: boolean;
}) {
  return (
    <tr>
      <Td className="whitespace-nowrap">{formatDateTime(row.time)}</Td>
      <Td>
        <span className={`${NAME_IN_TABLE} font-mono`}>
          <BreakableText text={row.model} kind="name" />
        </span>
        {row.alias !== row.model && (
          <span className="block text-xs text-muted">
            asked for <BreakableText text={row.alias} kind="name" />
          </span>
        )}
      </Td>
      <Td>
        {row.provider}
        {row.credential !== null && (
          <span className="block text-xs text-muted">{row.credential.label || row.credential.id}</span>
        )}
      </Td>
      <Td className="font-mono whitespace-nowrap">{row.client_key?.masked ?? "none"}</Td>
      <Td>
        <Badge tone={row.failed ? "danger" : "ok"}>
          {row.failed ? `Failed ${String(row.status)}` : String(row.status)}
        </Badge>
      </Td>
      <Td className="text-right whitespace-nowrap">
        {formatMillis(row.latency_ms)}
        {row.ttft_ms !== null && (
          <span className="block text-xs text-muted">first token {formatMillis(row.ttft_ms)}</span>
        )}
      </Td>
      <Td className="text-right whitespace-nowrap">
        {formatCompact(row.tokens.input)} in / {formatCompact(row.tokens.output)} out
        {row.tokens.cache_read > 0 && (
          <span className="block text-xs text-muted">
            {formatCompact(row.tokens.cache_read)} from cache
          </span>
        )}
      </Td>
      <Td className="text-right whitespace-nowrap">{formatCost(row.cost, currency)}</Td>
      <Td>
        <LogCell row={row} log={log} requestLog={requestLog} complete={complete} />
      </Td>
    </tr>
  );
}

export interface RecentCallsProps {
  filters: Omit<UsageFilters, "from" | "to">;
  failedOnly: boolean;
  onFailedOnlyChange: (value: boolean) => void;
  currency: string;
  range: RangePreset;
}

/**
 * The calls themselves, newest first, each linked to its request log. The
 * list holds still while it is read: its range ends when it was loaded, and
 * "Refresh" brings it up to now.
 */
export function RecentCalls({
  filters,
  failedOnly,
  onFailedOnlyChange,
  currency,
  range,
}: RecentCallsProps) {
  const call = useApiCall();
  const [asOf, setAsOf] = useState(() => Date.now());
  const query = {
    ...filters,
    ...rangeEndingAt(range, asOf),
    failed: failedOnly ? true : undefined,
    limit: PAGE_SIZE,
  };
  const calls = useInfiniteQuery({
    queryKey: [USAGE_REQUESTS, query],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam, signal }) =>
      call<UsageRequestsPage>(USAGE_REQUESTS, { query: { ...query, cursor: pageParam }, signal }),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const rows = useMemo(() => calls.data?.pages.flatMap((page) => page.requests) ?? [], [calls.data]);
  const logs = useLogIndex(rows);

  return (
    <Card
      title="Calls"
      description={`Each call to a provider, newest first, up to ${formatShortDateTime(new Date(asOf).toISOString())}. A client request that was retried has a call per attempt.`}
      actions={
        <>
          <label className="flex h-8 items-center gap-2 pointer-coarse:min-h-11">
            <Checkbox
              checked={failedOnly}
              onChange={(event) => {
                onFailedOnlyChange(event.target.checked);
              }}
            />
            Failed only
          </label>
          <Button
            size="sm"
            onClick={() => {
              setAsOf(Date.now());
            }}
          >
            <RotateCw aria-hidden="true" className="size-4" />
            Refresh
          </Button>
        </>
      }
    >
      {logs.requestLog === false && (
        <Alert tone="info">
          <p>
            <span className="font-medium">request-log is off</span>, so only failed requests have
            logs.
          </p>
          <TurnOnSetting
            path={REQUEST_LOG_SETTING}
            label="Log every request"
            configKey="request-log"
            invalidate={[[REQUEST_LOGS]]}
          />
        </Alert>
      )}
      {calls.isPending && <Loading>Loading calls…</Loading>}
      {calls.isError && <ProblemNotice problem={callProblem(calls.error)} />}
      {calls.isSuccess && rows.length === 0 && (
        <p className="text-muted">
          {failedOnly ? "No failed calls in this range." : "No calls in this range."}
        </p>
      )}
      {rows.length > 0 && (
        <Table caption="Calls to providers, newest first">
          <thead>
            <tr>
              <Th>Time</Th>
              <Th>Model</Th>
              <Th>Provider</Th>
              <Th>Client key</Th>
              <Th>Status</Th>
              <Th className="text-right">Latency</Th>
              <Th className="text-right">Tokens</Th>
              <Th className="text-right">Cost</Th>
              <Th>Log</Th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <CallRow
                key={row.id}
                row={row}
                currency={currency}
                log={logs.index.get(shortRequestId(row.request_id))}
                requestLog={logs.requestLog}
                complete={logs.complete}
              />
            ))}
          </tbody>
        </Table>
      )}
      {calls.hasNextPage && (
        <Button
          disabled={calls.isFetchingNextPage}
          onClick={() => {
            void calls.fetchNextPage();
          }}
        >
          {calls.isFetchingNextPage && <Spinner />}
          Load {formatInteger(PAGE_SIZE)} more
        </Button>
      )}
    </Card>
  );
}
