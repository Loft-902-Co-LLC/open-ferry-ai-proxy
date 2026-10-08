import { useInfiniteQuery, useMutation } from "@tanstack/react-query";
import { ArrowLeft, Download, RotateCw } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { Link, useLocation, useNavigate, useParams } from "react-router";

import { callProblem } from "../../api/access";
import { isApiError, isShortDownload, type DownloadProgress } from "../../api/client";
import {
  requestLogDownloadPath,
  requestLogPath,
  type LogEntry,
  type LogPiece,
} from "../../api/dashboard";
import { useApiCall, useApiDownload } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Badge } from "../../components/Badge";
import { BreakableText } from "../../components/BreakableText";
import { Button, buttonClasses } from "../../components/Button";
import { Card } from "../../components/Card";
import { Checkbox } from "../../components/CheckboxField";
import { PageHeader } from "../../components/PageHeader";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { Spinner } from "../../components/Spinner";
import { cn } from "../../lib/cn";
import { formatBytes, formatDateTime } from "../../lib/format";
import type { OpenedFrom } from "./RequestLogSearch";

/** Bytes shown at a time: 1 MiB, as the button says. */
const VIEW_PIECE = 1_048_576;
/**
 * How long a saved file's object URL is kept after the click. The browser
 * holds the file from the click on, but some revoke a URL out from under a
 * download that has only just started.
 */
export const REVOKE_AFTER_MS = 10_000;

/** Whether `name` could be a log's name, so worth asking for. */
export function isLogName(name: string): boolean {
  return name.endsWith(".log") && !/[/\\]/.test(name) && !name.includes("..");
}

/** Hands `blob` to the browser as a file named `name`, then lets the URL go. */
export function saveBlob(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  window.setTimeout(() => {
    URL.revokeObjectURL(url);
  }, REVOKE_AFTER_MS);
}

/** A download in progress: how much of how much. */
function DownloadStatus({ progress, size }: { progress: DownloadProgress | null; size: number }) {
  const total = progress?.total ?? size;
  const received = progress?.received ?? 0;
  return (
    <div className="flex flex-wrap items-center gap-3">
      <p role="status">Downloading the log, {formatBytes(total)}.</p>
      <progress
        aria-label="Downloaded so far"
        max={Math.max(total, 1)}
        value={Math.min(received, total)}
        className="h-2 w-48 accent-accent"
      />
      <span className="text-muted tabular-nums">
        {formatBytes(received)} of {formatBytes(total)}
      </span>
    </div>
  );
}

/** Why a download failed. Nothing was saved either way. */
function DownloadProblem({ error }: { error: Error }) {
  if (!isShortDownload(error)) {
    return <ProblemNotice problem={callProblem(error)} live />;
  }
  return (
    <Alert tone="danger" live title="The download stopped short">
      <p>
        {error.expected === null
          ? `The connection broke off after ${formatBytes(error.received)}.`
          : `The server sent ${formatBytes(error.received)} of ${formatBytes(error.expected)}; the log may have got shorter while it was sent.`}{" "}
        Nothing was saved. Try again.
      </p>
    </Alert>
  );
}

function LogFacts({ log }: { log: LogEntry }) {
  const facts: [string, ReactNode][] = [
    ["Time", formatDateTime(log.time)],
    [
      "Request",
      log.url === null ? (
        <span className="text-muted">not found in the log</span>
      ) : (
        <span className="font-mono">
          {log.method ?? ""} <BreakableText text={log.url} />
        </span>
      ),
    ],
    ["Status", log.status ?? <span className="text-muted">not found in the log</span>],
    [
      "Model",
      log.model === null ? (
        <span className="text-muted">not found in the log</span>
      ) : (
        <span className="font-mono">
          <BreakableText text={log.model} kind="name" />
        </span>
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
  const fetchFile = useApiDownload();
  const [progress, setProgress] = useState<DownloadProgress | null>(null);
  // A download stops when the page goes: leaving it shouldn't save a file
  // later.
  const lifetime = useRef<AbortController | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    lifetime.current = controller;
    return () => {
      controller.abort();
    };
  }, []);
  // The whole file, byte for byte, by its exact name, saved under the name
  // it was listed by.
  const download = useMutation({
    mutationFn: async () => {
      setProgress(null);
      const blob = await fetchFile(requestLogDownloadPath(name), {
        signal: lifetime.current?.signal,
        onProgress: setProgress,
      });
      saveBlob(blob, name);
    },
  });

  if (pieces.isError && pieces.data === undefined) {
    if (isApiError(pieces.error) && pieces.error.status === 404 && pieces.error.code !== null) {
      return <NoSuchLog />;
    }
    return (
      <>
        <PageHeader
          title="Log"
          description={
            <span className="font-mono">
              <BreakableText text={name} kind="name" />
            </span>
          }
          actions={<BackLink />}
        />
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
        description={
          <span className="font-mono">
            <BreakableText text={name} kind="name" />
          </span>
        }
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
        {download.isPending && log !== undefined && (
          <DownloadStatus progress={progress} size={log.size} />
        )}
        {download.isError && <DownloadProblem error={download.error} />}
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
              <label className="flex h-8 items-center gap-2 pointer-coarse:min-h-11">
                <Checkbox
                  checked={wrap}
                  onChange={(event) => {
                    setWrap(event.target.checked);
                  }}
                />
                Wrap long lines
              </label>
            }
          >
            <pre
              role="region"
              tabIndex={0}
              aria-label="The log's content"
              className={cn(
                "max-h-[70vh] overflow-auto rounded-md border border-line bg-surface p-3 font-mono text-xs leading-5",
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
