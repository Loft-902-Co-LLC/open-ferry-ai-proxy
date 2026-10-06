import { useQuery, useQueryClient } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";

import { callProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import { useApiCall } from "../../api/hooks";
import {
  LOGGING_TO_FILE,
  LOGGING_TO_FILE_DISABLED,
  SERVER_LOGS,
  type ServerLogPage,
} from "../../api/management";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { Code } from "../../components/Code";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { TextField } from "../../components/TextField";
import { TurnOnSetting } from "../../components/TurnOnSetting";
import { cn } from "../../lib/cn";
import { formatInteger } from "../../lib/format";

/** Lines read when the log is first opened. */
export const TAIL_LINES = 500;
/** The most new lines one poll reads. */
const POLL_LINES = 1000;
/** The most lines kept on screen. */
export const KEEP_LINES = 2000;
const POLL_MS = 3000;

const SERVER_LOG_KEY = [SERVER_LOGS, "follow"] as const;

/** The log as shown: its newest lines, and where to read on from. */
export interface ServerLogView {
  lines: string[];
  cursor: string;
  /** The log was rotated or cleared since the last read. */
  restarted: boolean;
  /** Lines were dropped from the top to keep KEEP_LINES. */
  trimmed: boolean;
}

/**
 * The view after a read. A read with a cursor continues the lines, unless
 * the server says the cursor no longer holds; a read without one is the
 * log's end afresh.
 */
export function mergeServerLog(
  before: ServerLogView | undefined,
  page: ServerLogPage,
  sentCursor: boolean,
): ServerLogView {
  const restarted = sentCursor && page["cursor-reset"] === true;
  const continued = sentCursor && !restarted && before !== undefined;
  const lines = continued ? [...before.lines, ...page.lines] : page.lines;
  const trimmed = lines.length > KEEP_LINES || (continued && before.trimmed);
  return {
    lines: lines.length > KEEP_LINES ? lines.slice(-KEEP_LINES) : lines,
    // An empty cursor means there was no log yet: read its end next time.
    cursor: page["next-cursor"],
    restarted: restarted || (continued && before.restarted),
    trimmed,
  };
}

function isLoggingOff(error: unknown): boolean {
  return isApiError(error) && error.status === 400 && error.code === LOGGING_TO_FILE_DISABLED;
}

// upstream's format: [2026-10-05 11:58:02] [1234abcd] [warn ] [file.go:12] message
const LEVEL = /^\[[^\]]*\] \[[^\]]*\] \[(\w+)\s*\]/;

function lineTone(line: string): string | undefined {
  const level = LEVEL.exec(line)?.[1];
  if (level === "error" || level === "fatal" || level === "panic") {
    return "text-danger";
  }
  if (level === "warn") {
    return "text-warn";
  }
  if (level === "debug" || level === "trace") {
    return "text-muted";
  }
  return undefined;
}

/** How far from the bottom still counts as at it, in pixels. */
const AT_BOTTOM = 24;

/** The server's own log, main.log, followed as it grows. */
export function ServerLog() {
  const call = useApiCall();
  const client = useQueryClient();
  const [follow, setFollow] = useState(true);
  const [filter, setFilter] = useState("");
  const log = useQuery({
    queryKey: SERVER_LOG_KEY,
    queryFn: async ({ signal }) => {
      const before = client.getQueryData<ServerLogView>(SERVER_LOG_KEY);
      const cursor = before?.cursor ?? "";
      const page = await call<ServerLogPage>(SERVER_LOGS, {
        query: cursor === "" ? { limit: TAIL_LINES } : { cursor, limit: POLL_LINES },
        signal,
      });
      return mergeServerLog(before, page, cursor !== "");
    },
    refetchInterval: (query) => (follow && query.state.status !== "error" ? POLL_MS : false),
    refetchIntervalInBackground: false,
  });

  const box = useRef<HTMLPreElement>(null);
  const atBottom = useRef(true);
  const lines = log.data?.lines;
  useLayoutEffect(() => {
    const element = box.current;
    if (element !== null && atBottom.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [lines]);

  if (log.isPending) {
    return <Loading>Reading the server&apos;s log…</Loading>;
  }
  if (log.isError && log.data === undefined) {
    if (isLoggingOff(log.error)) {
      return (
        <Alert tone="info" title="The server doesn't write its log to a file">
          <p>
            <Code>logging-to-file</Code> is off, so the server logs only to its console, which
            the dashboard can&apos;t read. Turned on, it writes <Code>main.log</Code> in its log
            directory.
          </p>
          <TurnOnSetting
            path={LOGGING_TO_FILE}
            label="Log to a file"
            configKey="logging-to-file"
            invalidate={[SERVER_LOG_KEY]}
          />
        </Alert>
      );
    }
    return (
      <ProblemNotice
        problem={callProblem(log.error)}
        action={
          <Button
            size="sm"
            onClick={() => {
              void log.refetch();
            }}
          >
            <RotateCw aria-hidden="true" className="size-4" />
            Try again
          </Button>
        }
      />
    );
  }

  const view = log.data;
  const needle = filter.trim().toLowerCase();
  const shown =
    needle === "" ? view.lines : view.lines.filter((line) => line.toLowerCase().includes(needle));

  return (
    <Card
      title="Server log"
      description={
        <>
          The newest lines of <Code>main.log</Code>
          {follow ? ", read every few seconds." : "."}
        </>
      }
      actions={
        <>
          <label className="flex h-8 items-center gap-2">
            <input
              type="checkbox"
              checked={follow}
              onChange={(event) => {
                setFollow(event.target.checked);
              }}
              className="size-4 accent-accent"
            />
            Follow
          </label>
          {!follow && (
            <Button
              size="sm"
              disabled={log.isFetching}
              onClick={() => {
                void log.refetch();
              }}
            >
              {log.isFetching ? <Spinner /> : <RotateCw aria-hidden="true" className="size-4" />}
              Read new lines
            </Button>
          )}
        </>
      }
    >
      <TextField
        label="Show lines containing"
        spellCheck={false}
        autoCapitalize="none"
        value={filter}
        onChange={(event) => {
          setFilter(event.target.value);
        }}
        className="max-w-md"
      />
      {log.isError && (
        <ProblemNotice problem={callProblem(log.error)} live className="text-sm" />
      )}
      {view.restarted && (
        <p className="text-muted">The log was rotated or cleared while open; it starts again below.</p>
      )}
      <p className="text-muted" role="status">
        {view.lines.length === 0
          ? "The log is empty."
          : needle === ""
            ? `${formatInteger(view.lines.length)} lines${view.trimmed ? `, the newest ${formatInteger(KEEP_LINES)} kept` : ""}.`
            : `${formatInteger(shown.length)} of ${formatInteger(view.lines.length)} lines contain “${filter.trim()}”.`}
      </p>
      {shown.length > 0 && (
        <pre
          ref={box}
          role="log"
          aria-label="Server log lines"
          aria-live="off"
          tabIndex={0}
          onScroll={(event) => {
            const element = event.currentTarget;
            atBottom.current =
              element.scrollHeight - element.scrollTop - element.clientHeight <= AT_BOTTOM;
          }}
          className="max-h-[60vh] overflow-auto rounded-md border border-line bg-raised p-3 font-mono text-xs leading-5"
        >
          {shown.map((line, index) => (
            // Lines repeat and have no IDs; their place is their identity.
            <span key={index} className={cn("block whitespace-pre-wrap break-all", lineTone(line))}>
              {line}
            </span>
          ))}
        </pre>
      )}
    </Card>
  );
}
