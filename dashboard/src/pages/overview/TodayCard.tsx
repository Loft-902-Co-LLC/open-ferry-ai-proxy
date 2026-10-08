import { keepPreviousData } from "@tanstack/react-query";
import { RotateCw } from "lucide-react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import { isUnsupportedRoute } from "../../api/client";
import {
  USAGE_LEDGER,
  USAGE_SUMMARY,
  type LedgerState,
  type Metrics,
  type UsageSummary,
} from "../../api/dashboard";
import { useApiQuery } from "../../api/hooks";
import { USAGE_STATISTICS_ENABLED } from "../../api/management";
import { Alert } from "../../components/Alert";
import { Button, buttonClasses } from "../../components/Button";
import { Card } from "../../components/Card";
import { ProblemNotice } from "../../components/ProblemNotice";
import { Loading } from "../../components/QueryState";
import { TurnOnSetting } from "../../components/TurnOnSetting";
import { formatCost, formatInteger, formatPercent, formatShortDateTime } from "../../lib/format";
import { useMinuteClock } from "../../lib/timeRange";

/** How often today's numbers are read again, while the tab is in front. */
export const TODAY_REFRESH_MS = 60_000;

/** The Usage page with only the failed calls listed. */
export const FAILED_CALLS_LINK = "/usage?failed=true";

/** The midnight that starts the day `at` (ms) is in, in the browser's time zone. */
export function startOfDay(at: number): string {
  const day = new Date(at);
  day.setHours(0, 0, 0, 0);
  return day.toISOString();
}

function TryAgain({ onClick }: { onClick: () => void }) {
  return (
    <Button size="sm" onClick={onClick}>
      <RotateCw aria-hidden="true" className="size-4" />
      Try again
    </Button>
  );
}

function RecordingOff() {
  return (
    <Alert tone="warn" title="Usage isn't being recorded">
      <p>
        Usage statistics is off, so new calls aren&apos;t counted. Turn it on here, or under Logs
        and usage in <Link to="/settings">Settings</Link>.
      </p>
      <TurnOnSetting
        path={USAGE_STATISTICS_ENABLED}
        label="Start recording"
        configKey="usage-statistics-enabled"
        invalidate={[[USAGE_LEDGER], [USAGE_SUMMARY]]}
      />
    </Alert>
  );
}

function Numbers({
  totals,
  currency,
  recording,
  newest,
}: {
  totals: Metrics;
  currency: string;
  recording: boolean;
  newest: string | null;
}) {
  if (totals.requests === 0) {
    return (
      <p>
        {recording ? "No calls yet today." : "No calls were recorded today."}
        {newest !== null && ` The last was ${formatShortDateTime(newest)}.`}
      </p>
    );
  }
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-6 gap-y-1">
      <dt className="text-muted">Requests</dt>
      <dd className="tabular-nums">{formatInteger(totals.requests)}</dd>
      <dt className="text-muted">Failed</dt>
      <dd className="tabular-nums">
        {formatInteger(totals.errors)} ({formatPercent(totals.errors / totals.requests)} of
        requests)
        {totals.errors > 0 && (
          <>
            {" "}
            <Link to={FAILED_CALLS_LINK}>See the failed calls</Link>
          </>
        )}
      </dd>
      {totals.cost !== null && (
        <>
          <dt className="text-muted">Cost</dt>
          <dd>
            <span className="tabular-nums">{formatCost(totals.cost, currency)}</span>, an estimate
            from the prices you set
            {totals.unpriced_requests > 0 &&
              `; ${formatInteger(totals.unpriced_requests)} ${totals.unpriced_requests === 1 ? "call has" : "calls have"} no price`}
          </dd>
        </>
      )}
    </dl>
  );
}

/**
 * Today's calls to providers, from midnight in the browser's time zone:
 * how many, how many failed, and what they cost when prices are set.
 */
export function TodayCard() {
  const ledger = useApiQuery<LedgerState>(USAGE_LEDGER);
  // Reads after midnight start the new day.
  const from = startOfDay(useMinuteClock() - 1);
  const opened = ledger.data?.available === true;
  const today = useApiQuery<UsageSummary>(
    USAGE_SUMMARY,
    { from },
    { enabled: opened, placeholderData: keepPreviousData, refetchInterval: TODAY_REFRESH_MS },
  );

  let body;
  if (ledger.isPending) {
    body = <Loading>Loading today&apos;s usage…</Loading>;
  } else if (ledger.isError && isUnsupportedRoute(ledger.error)) {
    body = (
      <p className="text-muted">This server doesn&apos;t keep usage, so there&apos;s none to show.</p>
    );
  } else if (ledger.isError) {
    body = (
      <ProblemNotice
        problem={callProblem(ledger.error)}
        action={
          <TryAgain
            onClick={() => {
              void ledger.refetch();
            }}
          />
        }
      />
    );
  } else if (!ledger.data.available) {
    body = <ProblemNotice problem={{ kind: "ledger-unavailable" }} />;
  } else {
    const state = ledger.data;
    body = (
      <>
        {!state.usage_statistics_enabled && <RecordingOff />}
        {today.isPending ? (
          <Loading>Loading today&apos;s usage…</Loading>
        ) : today.isError ? (
          <ProblemNotice
            problem={callProblem(today.error)}
            action={
              <TryAgain
                onClick={() => {
                  void today.refetch();
                }}
              />
            }
          />
        ) : (
          <Numbers
            totals={today.data.totals}
            currency={today.data.currency}
            recording={state.usage_statistics_enabled}
            newest={state.newest}
          />
        )}
      </>
    );
  }

  return (
    <Card
      title="Today"
      description="Calls the proxy made to providers since midnight."
      actions={
        <Link to="/usage" className={buttonClasses("secondary", "sm")}>
          Open Usage
        </Link>
      }
    >
      {body}
    </Card>
  );
}
