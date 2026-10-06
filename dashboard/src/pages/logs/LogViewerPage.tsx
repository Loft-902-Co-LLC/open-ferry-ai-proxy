import { useInfiniteQuery, useMutation } from "@tanstack/react-query";
import { ArrowLeft, Download, RotateCw } from "lucide-react";
import { useState, type ReactNode } from "react";
import { Link, useLocation, useNavigate, useParams } from "react-router";

import { callProblem } from "../../api/access";
import { isApiError } from "../../api/client";
import { requestLogPath, type LogEntry, type LogPiece } from "../../api/dashboard";
import { useApiCall } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { Button, buttonClasses } from "../../components/Button";
import { Card } from "../../components/Card";
import { PageHeader } from "../../components/PageHeader";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { cn } from "../../lib/cn";
import { formatBytes, formatDateTime } from "../../lib/format";
import type { OpenedFrom } from "./RequestLogSearch";

/** Bytes shown at a time: 1 MiB, as the button says. */
const VIEW_PIECE = 1_048_576;
/** Bytes read at a time to download: the most the route reads at once. */
export const DOWNLOAD_PIECE = 4_194_304;

/** Whether `name` could be a log's name, so worth asking for. */
export function isLogName(name: string): boolean {
  return name.endsWith(".log") && !/[/\\]/.test(name) && !name.includes("..");
}

type Call = ReturnType<typeof useApiCall>;

/**
 * Reads a whole log in the route's largest pieces, as the dashboard API
 * serves it by its exact name. (The management API's download by request
 * ID picks the newest log with that ID, which may be another.)
 */
export async function readWholeLog(call: Call, name: string): Promise<string[]> {
  const parts: string[] = [];
  let offset: number | null = 0;
  while (offset !== null) {
    const piece: LogPiece = await call<LogPiece>(requestLogPath(name), {
      query: { offset, length: DOWNLOAD_PIECE },
    });
    parts.push(piece.content);
    // A piece that doesn't move on would repeat forever.
    offset = piece.next_offset !== null && piece.next_offset > offset ? piece.next_offset : null;
  }
  return parts;
}

/** Hands `parts` to the browser as a file named `name`. */
function saveText(parts: string[], name: string) {
  const url = URL.createObjectURL(new Blob(parts, { type: "text/plain;charset=utf-8" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  // The download has its own reference by now; give it a moment regardless.
  window.setTimeout(() => {
    URL.revokeObjectURL(url);
  }, 10_000);
}

function LogFacts({ log }: { log: LogEntry }) {
  const facts: [string, ReactNode][] = [
    ["Time", formatDateTime(log.time)],
    [
      "Request",
      log.url === null ? (
        <span className="text-muted">not found in the log</span>
      ) : (
        <span className="font-mono break-all">
          {log.method ?? ""} {log.url}
        </span>
      ),
    ],
    ["Status", log.status ?? <span className="text-muted">not found in the log</span>],
    [
      "Model",
      log.model === null ? (
        <span className="text-muted">not found in the log</span>
      ) : (
        <span className="font-mono">{log.model}</span>
      ),
    ],
    ["Request ID", <span className="font-mono">{log.request_id}</span>],
    ["Size", formatBytes(log.size)],
    ["Last changed", formatDateTime(log.modified)],
  ];
  return (
    <dl className="grid gap-x-6 gap-y-2 sm:grid-cols-[max-content_1fr]">
      {facts.map(([label, value]) => (
        <div key={label} className="contents">
          <dt className="text-muted">{label}</dt>
          <dd className="min-w-0 tabular-nums">{value}</dd>
        </div>
      ))}
    </dl>
  );
}

function BackLink() {
  const location = useLocation();
  const navigate = useNavigate();
  const fromSearch = (location.state as OpenedFrom | null)?.fromSearch === true;
  if (fromSearch) {
    return (
      <Button
        size="sm"
        onClick={() => {
          void navigate(-1);
        }}
      >
        <ArrowLeft aria-hidden="true" className="size-4" />
        Back to the search
      </Button>
    );
  }
  return (
    <Link to="/logs" className={buttonClasses("secondary", "sm")}>
      <ArrowLeft aria-hidden="true" className="size-4" />
      All logs
    </Link>
  );
}

function NoSuchLog() {
  return (
    <>
      <PageHeader title="No such log" actions={<BackLink />} />
      <Alert tone="warn" title="The server has no log by this name">
        <p>
          It may have been deleted since it was listed. Search the logs to find what is there now.
        </p>
      </Alert>
    </>
  );
}

/** One request or error log, a piece at a time, with its download. */
export function LogViewerPage() {
  const { name = "" } = useParams();
  if (!isLogName(name)) {
    return <NoSuchLog />;
  }
  return <LogViewer key={name} name={name} />;
}

function LogViewer({ name }: { name: string }) {
  const call = useApiCall();
  const [wrap, setWrap] = useState(true);
  const path = requestLogPath(name);
  const pieces = useInfiniteQuery({
    queryKey: [path, "view"],
    initialPageParam: 0,
    queryFn: ({ pageParam, signal }) =>
      call<LogPiece>(path, { query: { offset: pageParam, length: VIEW_PIECE }, signal }),
    getNextPageParam: (last) =>
      last.next_offset !== null && last.next_offset > last.offset ? last.next_offset : undefined,
  });
  const download = useMutation({
    mutationFn: async () => {
      saveText(await readWholeLog(call, name), name);
    },
  });

  if (pieces.isError && pieces.data === undefined) {
    if (isApiError(pieces.error) && pieces.error.status === 404 && pieces.error.code !== null) {
      return <NoSuchLog />;
    }
    return (
      <>
        <PageHeader title="Log" description={<span className="font-mono">{name}</span>} actions={<BackLink />} />
        <ProblemNotice
          problem={callProblem(pieces.error)}
          action={
            <Button
              size="sm"
              onClick={() => {
                void pieces.refetch();
              }}
            >
              <RotateCw aria-hidden="true" className="size-4" />
              Try again
            </Button>
          }
        />
      </>
    );
  }

  const loaded = pieces.data?.pages ?? [];
  const latest = loaded.at(-1);
  const log = latest?.log;
  const text = loaded.map((piece) => piece.content).join("");
  const shownBytes = latest === undefined ? 0 : (latest.next_offset ?? latest.log.size);

  return (
    <>
      <PageHeader
        title={log === undefined ? "Log" : log.kind === "error" ? "Error log" : "Request log"}
        description={<span className="font-mono break-all">{name}</span>}
        actions={
          <>
            <BackLink />
            <Button
              size="sm"
              disabled={download.isPending || log === undefined}
              onClick={() => {
                download.mutate();
              }}
            >
              {download.isPending ? <Spinner /> : <Download aria-hidden="true" className="size-4" />}
              {download.isPending ? "Downloading…" : "Download"}
            </Button>
          </>
        }
      />
      <div className="space-y-4">
        {download.isError && (
          <ProblemNotice problem={callProblem(download.error)} live />
        )}
        {pieces.isPending && <Loading>Reading the log…</Loading>}
        {log !== undefined && (
          <Card
            title={
              <span className="flex items-center gap-2">
                Details
                {log.kind === "error" && <Badge tone="danger">Error</Badge>}
              </span>
            }
          >
            <LogFacts log={log} />
          </Card>
        )}
        {latest !== undefined && (
          <Card
            title="Content"
            description={
              latest.next_offset === null
                ? `All ${formatBytes(latest.log.size)}.`
                : `The first ${formatBytes(shownBytes)} of ${formatBytes(latest.log.size)}.`
            }
            actions={
              <label className="flex h-8 items-center gap-2">
                <input
                  type="checkbox"
                  checked={wrap}
                  onChange={(event) => {
                    setWrap(event.target.checked);
                  }}
                  className="size-4 accent-accent"
                />
                Wrap long lines
              </label>
            }
          >
            <pre
              tabIndex={0}
              aria-label="The log's content"
              className={cn(
                "max-h-[70vh] overflow-auto rounded-md border border-line bg-raised p-3 font-mono text-xs leading-5",
                wrap ? "whitespace-pre-wrap break-all" : "whitespace-pre",
              )}
            >
              {text}
            </pre>
            {pieces.isFetchNextPageError && (
              <ProblemNotice problem={callProblem(pieces.error)} live />
            )}
            {pieces.hasNextPage && (
              <Button
                disabled={pieces.isFetchingNextPage}
                onClick={() => {
                  void pieces.fetchNextPage();
                }}
              >
                {pieces.isFetchingNextPage && <Spinner />}
                Show the next 1 MiB
              </Button>
            )}
          </Card>
        )}
      </div>
    </>
  );
}
