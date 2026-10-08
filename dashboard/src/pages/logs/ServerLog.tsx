import { useQuery, useQueryClient } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

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
import { Checkbox } from "../../components/CheckboxField";
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
/** How long typing in the filter must pause before its outcome is announced. */
export const FILTER_SETTLE_MS = 600;

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

/** The lines a filter keeps: those containing it, in any case. */
function matching(lines: string[], filter: string): string[] {
  const needle = filter.trim().toLowerCase();
  return needle === "" ? lines : lines.filter((line) => line.toLowerCase().includes(needle));
}

/** How many lines there are, and how many the filter keeps. */
function countText(view: ServerLogView, filter: string): string {
  const total = formatInteger(view.lines.length);
  if (view.lines.length === 0) {
    return "The log is empty.";
  }
  if (filter.trim() === "") {
    const kept = view.trimmed ? `, the newest ${formatInteger(KEEP_LINES)} kept` : "";
    return `${total} lines${kept}.`;
  }
  const shown = formatInteger(matching(view.lines, filter).length);
  return `${shown} of ${total} lines contain “${filter.trim()}”.`;
}

/** How far from the bottom still counts as at it, in pixels. */
const AT_BOTTOM = 24;

/** The server's own log, main.log, followed as it grows. */
export function ServerLog() {
  const call = useApiCall();
  const client = useQueryClient();
  const [follow, setFollow] = useState(true);
  const [filter, setFilter] = useState("");
  const [filterUsed, setFilterUsed] = useState(false);
  // What the filter found, said once typing settles. The count on screen
  // changes with every read, so it isn't announced itself.
  const [announcement, setAnnouncement] = useState("");
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

  useEffect(() => {
    if (!filterUsed) {
      return;
    }
    const timer = window.setTimeout(() => {
      const latest = client.getQueryData<ServerLogView>(SERVER_LOG_KEY);
      setAnnouncement(latest === undefined ? "" : countText(latest, filter));
    }, FILTER_SETTLE_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, [client, filter, filterUsed]);

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
  const shown = matching(view.lines, filter);

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
          <label className="flex h-8 items-center gap-2 pointer-coarse:min-h-11">
            <Checkbox
              checked={follow}
              onChange={(event) => {
                setFollow(event.target.checked);
              }}
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
          setFilterUsed(true);
        }}
        className="max-w-md"
      />
      {log.isError && (
        <ProblemNotice problem={callProblem(log.error)} live className="text-sm" />
      )}
      {/* Says what needs saying: the rotation notice as it appears, and the
          filter's outcome. Out of the flow while it shows nothing. */}
      <div role="status" className={cn(!view.restarted && "absolute")}>
        {view.restarted && (
          <p className="text-muted">
            The log was rotated or cleared while open; it starts again below.
          </p>
        )}
        <span className="sr-only">{announcement}</span>
      </div>
      <p className="text-muted">{countText(view, filter)}</p>
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
          className="max-h-[60vh] overflow-auto rounded-md border border-line bg-surface p-3 font-mono text-xs leading-5"
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
