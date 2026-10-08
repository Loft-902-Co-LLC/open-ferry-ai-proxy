import { zodResolver } from "@hookform/resolvers/zod";
import { useInfiniteQuery } from "@tanstack/react-query";
import { Search } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useForm } from "react-hook-form";
import { Link, useLocation, useNavigate, useSearchParams } from "react-router";

import { callProblem } from "../../api/access";
import { REQUEST_LOGS, type LogEntry, type LogSearchPage } from "../../api/dashboard";
import { useApiCall } from "../../api/hooks";
import { REQUEST_LOG_SETTING } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { BreakableText, NAME_IN_TABLE } from "../../components/BreakableText";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { SelectField } from "../../components/SelectField";
import { Spinner } from "../../components/Spinner";
import { Table, Td, Th } from "../../components/Table";
import { TextField } from "../../components/TextField";
import { TurnOnSetting } from "../../components/TurnOnSetting";
import { formatBytes, formatDateTime, formatInteger } from "../../lib/format";
import { RANGE_PRESETS } from "../../lib/timeRange";
import {
  ANY_TIME,
  GIVEN_TIMES,
  KIND_LABELS,
  logSearchForm,
  readLogQuery,
  searchParams,
  writeLogQuery,
  type LogQuery,
  type LogSearchForm,
} from "./logQuery";

const PAGE_SIZE = 50;

function rangeOptions(query: LogQuery) {
  const options = [
    { value: ANY_TIME, label: "Any time" },
    ...RANGE_PRESETS.map((preset) => ({ value: preset.id, label: preset.label })),
  ];
  if (query.range === GIVEN_TIMES) {
    const from = query.from === "" ? "the start" : formatDateTime(query.from);
    const to = query.to === "" ? "now" : formatDateTime(query.to);
    options.push({ value: GIVEN_TIMES, label: `From ${from} to ${to}` });
  }
  return options;
}

function SearchForm({ query, onSearch }: { query: LogQuery; onSearch: (query: LogQuery) => void }) {
  const form = useForm<LogSearchForm>({
    resolver: zodResolver(logSearchForm),
    defaultValues: query,
  });
  useEffect(() => {
    form.reset(query);
  }, [form, query]);
  const onSubmit = form.handleSubmit((values) => {
    onSearch({ ...query, ...values });
  });
  const errors = form.formState.errors;
  return (
    <form
      noValidate
      role="search"
      aria-label="Search the request logs"
      onSubmit={(event) => void onSubmit(event)}
      className="space-y-4"
    >
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <SelectField label="When" options={rangeOptions(query)} {...form.register("range")} />
        <SelectField
          label="Kind"
          options={Object.entries(KIND_LABELS).map(([value, label]) => ({ value, label }))}
          {...form.register("kind")}
        />
        <TextField
          label="Status"
          placeholder="502 or 5xx"
          spellCheck={false}
          error={errors.status?.message}
          {...form.register("status")}
        />
        <TextField
          label="Path contains"
          placeholder="/v1/chat/completions"
          spellCheck={false}
          autoCapitalize="none"
          {...form.register("path")}
        />
        <TextField
          label="Model contains"
          spellCheck={false}
          autoCapitalize="none"
          {...form.register("model")}
        />
        <TextField
          label="Text"
          hint="Anywhere in the log, ignoring case."
          spellCheck={false}
          autoCapitalize="none"
          {...form.register("q")}
        />
      </div>
      <Button type="submit" variant="primary">
        <Search aria-hidden="true" className="size-4" />
        Search
      </Button>
    </form>
  );
}

/** What the search keeps in its history entry. */
interface SearchState {
  /** When the search ran: where a preset range ends. */
  ranAt?: number;
}

/** What a log's page is told by the search that opened it. */
export interface OpenedFrom {
  fromSearch: true;
}

function LogRow({ log }: { log: LogEntry }) {
  return (
    <tr>
      <Td className="whitespace-nowrap">{formatDateTime(log.time)}</Td>
      <Td>
        <Badge tone={log.kind === "error" ? "danger" : "neutral"}>
          {log.kind === "error" ? "Error" : "Request"}
        </Badge>
      </Td>
      <Td className="font-mono">
        {log.method ?? ""}{" "}
        {log.url === null ? (
          <span className="text-muted">unknown</span>
        ) : (
          <BreakableText text={log.url} />
        )}
      </Td>
      <Td>{log.status ?? "–"}</Td>
      <Td className="font-mono">
        {log.model === null ? (
          "–"
        ) : (
          <span className={NAME_IN_TABLE}>
            <BreakableText text={log.model} kind="name" />
          </span>
        )}
      </Td>
      <Td className="text-right whitespace-nowrap">{formatBytes(log.size)}</Td>
      <Td>
        <Link
          to={`/logs/${encodeURIComponent(log.name)}`}
          state={{ fromSearch: true } satisfies OpenedFrom}
          aria-label={`Open ${log.name}`}
        >
          Open
        </Link>
      </Td>
    </tr>
  );
}

/** Searches the request logs, newest first, a page at a time. */
export function RequestLogSearch() {
  const [params, setParams] = useSearchParams();
  const query = useMemo(() => readLogQuery(params), [params]);
  const location = useLocation();
  const navigate = useNavigate();
  // A preset range ends when the search ran, which the history entry keeps:
  // coming Back from a log shows the same results, from the cache, and
  // searching again moves the range on.
  const keptAt = (location.state as SearchState | null)?.ranAt;
  const [openedAt] = useState(() => Date.now());
  const ranAt = keptAt ?? openedAt;
  useEffect(() => {
    if (keptAt === undefined) {
      void navigate(
        { pathname: location.pathname, search: location.search },
        { replace: true, state: { ranAt: openedAt } satisfies SearchState },
      );
    }
  }, [keptAt, navigate, location.pathname, location.search, openedAt]);
  const call = useApiCall();
  const search = { ...searchParams(query, ranAt), limit: PAGE_SIZE };
  const logs = useInfiniteQuery({
    queryKey: [REQUEST_LOGS, search, ranAt],
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam, signal }) =>
      call<LogSearchPage>(REQUEST_LOGS, { query: { ...search, cursor: pageParam }, signal }),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
  const pages = logs.data?.pages ?? [];
  const entries = pages.flatMap((page) => page.logs);
  const last = pages.at(-1);
  const stoppedAtLimit = last?.scanned.limit_reached === true && last.next_cursor !== null;
  const scannedFiles = pages.reduce((sum, page) => sum + page.scanned.files, 0);
  const scannedBytes = pages.reduce((sum, page) => sum + page.scanned.bytes, 0);

  return (
    <div className="space-y-4">
      <Card>
        <SearchForm
          query={query}
          onSearch={(next) => {
            setParams(
              (current) => {
                const written = writeLogQuery(next);
                const tab = current.get("tab");
                if (tab !== null) {
                  written.set("tab", tab);
                }
                return written;
              },
              { state: { ranAt: Date.now() } satisfies SearchState },
            );
          }}
        />
      </Card>
      {last?.request_log === false && (
        <Alert tone="info" title="Only failed requests are logged">
          <p>
            <Code>request-log</Code> is off, so the server writes a log only for a request that
            failed, as <Code>error-*.log</Code>.
          </p>
          <TurnOnSetting
            path={REQUEST_LOG_SETTING}
            label="Log every request"
            configKey="request-log"
            invalidate={[[REQUEST_LOGS]]}
          />
        </Alert>
      )}
      <Card title="Logs" description="Newest first.">
        {logs.isPending && <Loading>Searching…</Loading>}
        {logs.isError && <ProblemNotice problem={callProblem(logs.error)} />}
        {logs.isSuccess && entries.length === 0 && !stoppedAtLimit && (
          <p className="text-muted">No logs match.</p>
        )}
        {entries.length > 0 && (
          <Table caption="Request logs, newest first">
            <thead>
              <tr>
                <Th>Time</Th>
                <Th>Kind</Th>
                <Th>Request</Th>
                <Th>Status</Th>
                <Th>Model</Th>
                <Th className="text-right">Size</Th>
                <Th>
                  <span className="sr-only">Open</span>
                </Th>
              </tr>
            </thead>
            <tbody>
              {entries.map((log) => (
                <LogRow key={log.name} log={log} />
              ))}
            </tbody>
          </Table>
        )}
        {logs.isFetchNextPageError && <ProblemNotice problem={callProblem(logs.error)} live />}
        {stoppedAtLimit && (
          <Alert tone="info" title={entries.length === 0 ? "Nothing found yet" : "The search paused"}>
            <p>
              A search reads at most 2,000 files or 64 MiB at a time. This one has read{" "}
              {formatInteger(scannedFiles)} files ({formatBytes(scannedBytes)}) and hasn&apos;t
              reached the oldest logs.
            </p>
            <Button
              size="sm"
              disabled={logs.isFetchingNextPage}
              onClick={() => {
                void logs.fetchNextPage();
              }}
            >
              {logs.isFetchingNextPage ? <Spinner /> : <Search aria-hidden="true" className="size-4" />}
              Keep searching
            </Button>
          </Alert>
        )}
        {logs.hasNextPage && !stoppedAtLimit && (
          <Button
            disabled={logs.isFetchingNextPage}
            onClick={() => {
              void logs.fetchNextPage();
            }}
          >
            {logs.isFetchingNextPage && <Spinner />}
            Load more
          </Button>
        )}
      </Card>
    </div>
  );
}
