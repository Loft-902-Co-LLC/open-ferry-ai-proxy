import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Play } from "lucide-react";

import { callProblem } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import { USAGE_LEDGER, type LedgerState } from "../../api/dashboard";
import { useApiCall } from "../../api/hooks";
import { Alert } from "../../components/Alert";
import { Button } from "../../components/Button";
import { Code } from "../../components/Code";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Spinner } from "../../components/Spinner";
import { formatInteger } from "../../lib/format";

export const USAGE_STATISTICS_ENABLED = "/v0/management/usage-statistics-enabled";

/** Turns usage recording on, through the management API. */
function StartRecording() {
  const call = useApiCall();
  const client = useQueryClient();
  const start = useMutation({
    mutationFn: () =>
      call(USAGE_STATISTICS_ENABLED, { method: "PUT", json: { value: true } }),
    onSuccess: () => client.invalidateQueries({ queryKey: [USAGE_LEDGER] }),
  });
  return (
    <div className="space-y-2">
      <Button
        size="sm"
        variant="primary"
        disabled={start.isPending}
        onClick={() => {
          start.mutate();
        }}
      >
        {start.isPending ? <Spinner /> : <Play aria-hidden="true" className="size-4" />}
        Start recording
      </Button>
      {start.isError &&
        (isUnsupportedRoute(start.error) ? (
          <Alert tone="info" live title="This server can't change settings yet">
            <p>
              Set <Code>usage-statistics-enabled: true</Code> in config.yaml instead.
            </p>
          </Alert>
        ) : (
          <ProblemNotice problem={callProblem(start.error)} live />
        ))}
    </div>
  );
}

/** What is wrong with the ledger, if anything, and what to do about it. */
export function LedgerNotices({ ledger }: { ledger: LedgerState }) {
  if (!ledger.available) {
    return (
      <Alert tone="danger" title="The usage ledger couldn't be opened">
        <p>
          The server keeps usage in <Code>open-ferry-usage.sqlite3</Code>, in its log directory
          (<Code>WRITABLE_PATH</Code>, else beside config.yaml), and couldn&apos;t open or create
          it:
        </p>
        <p className="font-mono break-words">{ledger.unavailable_reason ?? "no reason given"}</p>
        <p>Check that the directory exists and the server may write to it, then restart it.</p>
      </Alert>
    );
  }
  return (
    <>
      {!ledger.usage_statistics_enabled && (
        <Alert tone="warn" title="Usage isn't being recorded">
          <p>
            <Code>usage-statistics-enabled</Code> is off, so new calls aren&apos;t counted. What
            was recorded before stays.
          </p>
          <StartRecording />
        </Alert>
      )}
      {(ledger.dropped_records ?? 0) > 0 && (
        <Alert tone="warn" title="Some calls weren't recorded">
          <p>
            The ledger fell behind and lost {formatInteger(ledger.dropped_records ?? 0)} records
            since the server started. A busy or slow disk can cause it; the totals here are low by
            that much.
          </p>
        </Alert>
      )}
    </>
  );
}
