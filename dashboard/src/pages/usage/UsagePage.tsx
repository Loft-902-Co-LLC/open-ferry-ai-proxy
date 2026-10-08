import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import { Filter, Settings2, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router";

import { callProblem } from "../../api/access";
import {
  USAGE_LEDGER,
  USAGE_SERIES,
  USAGE_SUMMARY,
  type GroupBy,
  type LedgerState,
  type Metrics,
  type UsageGroup,
  type UsageSeries,
  type UsageSummary,
} from "../../api/dashboard";
import { useApiQuery } from "../../api/hooks";
import { BreakableText, NAME_IN_TABLE } from "../../components/BreakableText";
import { buttonClasses, Button } from "../../components/Button";
import { Card } from "../../components/Card";
import { PageHeader } from "../../components/PageHeader";
import { QueryState } from "../../components/QueryState";
import { SelectField } from "../../components/SelectField";
import { Table, Td, Th } from "../../components/Table";
import {
  browserUtcOffset,
  formatCompact,
  formatCost,
  formatInteger,
  formatMillis,
  formatPercent,
  formatShortDateTime,
} from "../../lib/format";
import { RANGE_PRESETS, rangeEndingAt, useMinuteClock } from "../../lib/timeRange";
import { LedgerNotices } from "./LedgerNotices";
import { RecentCalls } from "./RecentCalls";
import {
  CHART_METRICS,
  UsageChart,
  chartData,
  seriesName,
  type ChartMetric,
} from "./UsageChart";
import {
  GROUP_BY_LABELS,
  GROUP_BYS,
  filterQuery,
  useUsageView,
  type ActiveFilter,
} from "./usageParams";

/** Groups the summary lists, and series the chart draws. */
const SUMMARY_GROUPS = 50;
const CHART_GROUPS = 5;

/**
 * One of the totals. Two to a row until the wide layout puts them all in
 * one; an odd one out at the end takes its row's width, not half of it.
 */
function Stat({ label, value, detail }: { label: string; value: ReactNode; detail?: ReactNode }) {
  return (
    <div className="rounded-lg border border-line bg-surface px-4 py-3 max-lg:odd:last:col-span-2">
      <dt className="text-muted">{label}</dt>
      <dd className="text-xl font-semibold tabular-nums">{value}</dd>
      {detail !== undefined && <dd className="text-xs text-muted">{detail}</dd>}
    </div>
  );
}

function Totals({ totals, currency }: { totals: Metrics; currency: string }) {
  const failedShare = totals.requests === 0 ? 0 : totals.errors / totals.requests;
  return (
    <dl className="grid grid-cols-2 gap-3 lg:grid-cols-5">
      <Stat label="Requests" value={formatInteger(totals.requests)} />
      <Stat
        label="Failed"
        value={formatInteger(totals.errors)}
        detail={totals.requests === 0 ? undefined : `${formatPercent(failedShare)} of requests`}
      />
      <Stat
        label="Tokens"
        value={formatCompact(totals.total_tokens)}
        detail={`${formatCompact(totals.input_tokens)} in (${formatCompact(totals.cache_read_tokens)} from cache), ${formatCompact(totals.output_tokens)} out`}
      />
      <Stat
        label="Cost"
        value={totals.cost === null ? "No prices" : formatCost(totals.cost, currency)}
        detail={
          totals.unpriced_requests > 0 ? (
            <>
              {formatInteger(totals.unpriced_requests)} calls without a price.{" "}
              <Link to="/usage/ledger#prices">Set prices</Link>
            </>
          ) : totals.cost === null ? undefined : (
            "An estimate from the prices you set"
          )
        }
      />
      <Stat
        label="Latency, median"
        value={totals.latency_ms === null ? "–" : formatMillis(totals.latency_ms.p50)}
        detail={
          totals.latency_ms === null
            ? undefined
            : `p95 ${formatMillis(totals.latency_ms.p95)}${
                totals.ttft_ms === null ? "" : `, first token ${formatMillis(totals.ttft_ms.p50)}`
              }`
        }
      />
    </dl>
  );
}

function groupName(group: UsageGroup, groupBy: GroupBy): string {
  return seriesName(group.label, groupBy);
}

function Groups({
  summary,
  groupBy,
  onShowOnly,
}: {
  summary: UsageSummary;
  groupBy: GroupBy;
  onShowOnly: (filter: ActiveFilter) => void;
}) {
  if (summary.groups.length === 0) {
    return <p className="text-muted">No calls in this range.</p>;
  }
  return (
    <>
      <Table caption={`Usage by ${GROUP_BY_LABELS[groupBy].toLowerCase()}`}>
        <thead>
          <tr>
            <Th>{GROUP_BY_LABELS[groupBy]}</Th>
            <Th className="text-right">Requests</Th>
            <Th className="text-right">Failed</Th>
            <Th className="text-right">Tokens</Th>
            <Th className="text-right">Cost</Th>
            <Th className="text-right">Latency p95</Th>
            <Th>
              <span className="sr-only">Actions</span>
            </Th>
          </tr>
        </thead>
        <tbody>
          {summary.groups.map((group) => {
            const name = groupName(group, groupBy);
            return (
              <tr key={group.key}>
                <Td>
                  <span
                    className={
                      groupBy === "client_key" ? `${NAME_IN_TABLE} font-mono` : NAME_IN_TABLE
                    }
                  >
                    <BreakableText text={name} kind="name" />
                  </span>
                  {group.credential !== undefined && group.credential.label !== group.credential.id && (
                    <span className="block text-xs text-muted">
                      {group.credential.id} · {group.credential.auth_type}
                    </span>
                  )}
                </Td>
                <Td className="text-right">{formatInteger(group.metrics.requests)}</Td>
                <Td className="text-right">{formatInteger(group.metrics.errors)}</Td>
                <Td className="text-right">{formatCompact(group.metrics.total_tokens)}</Td>
                <Td className="text-right whitespace-nowrap">
                  {formatCost(group.metrics.cost, summary.currency)}
                </Td>
                <Td className="text-right whitespace-nowrap">
                  {group.metrics.latency_ms === null ? "–" : formatMillis(group.metrics.latency_ms.p95)}
                </Td>
                <Td className="text-right">
                  {group.key !== "" && (
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={`Show only ${name}`}
                      onClick={() => {
                        onShowOnly({ name: groupBy, value: group.key, label: name });
                      }}
                    >
                      <Filter aria-hidden="true" className="size-4" />
                      Show only
                    </Button>
                  )}
                </Td>
              </tr>
            );
          })}
        </tbody>
      </Table>
      {summary.more_groups && (
        <p className="text-muted">
          Only the {formatInteger(SUMMARY_GROUPS)} busiest are listed. Filter to see the rest.
        </p>
      )}
    </>
  );
}

/** The chart's numbers, for those who can't see the chart. */
function ChartNumbers({ data, metric }: { data: UsageSeries; metric: ChartMetric }) {
  const { series, rows } = chartData(data, metric);
  const format = (value: number) =>
    metric === "cost" ? formatCost(value, data.currency) : formatInteger(value);
  return (
    <details>
      <summary className="cursor-pointer text-accent">Show the numbers</summary>
      <Table caption="The chart's numbers, by time" className="mt-2">
        <thead>
          <tr>
            <Th>Starting</Th>
            {series.map((one) => (
              <Th key={one.key} className="text-right">
                {one.name}
              </Th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.start}>
              <Td className="whitespace-nowrap">{formatShortDateTime(row.start)}</Td>
              {row.values.map((value, index) => (
                <Td key={series[index]?.key ?? index} className="text-right">
                  {format(value)}
                </Td>
              ))}
            </tr>
          ))}
        </tbody>
      </Table>
    </details>
  );
}

function FilterChips({
  filters,
  onRemove,
}: {
  filters: readonly ActiveFilter[];
  onRemove: (filter: ActiveFilter) => void;
}) {
  if (filters.length === 0) {
    return null;
  }
  return (
    <ul aria-label="Filters" className="flex flex-wrap gap-2">
      {filters.map((filter) => (
        <li
          key={filter.name}
          className="inline-flex items-center gap-1 rounded-full border border-line bg-raised py-0.5 pr-1 pl-3"
        >
          <span>
            <span className="text-muted">{GROUP_BY_LABELS[filter.name]}:</span> {filter.label}
          </span>
          <button
            type="button"
            aria-label={`Remove the filter ${GROUP_BY_LABELS[filter.name]}: ${filter.label}`}
            className="rounded-full p-1 text-muted hover:bg-surface hover:text-fg"
            onClick={() => {
              onRemove(filter);
            }}
          >
            <X aria-hidden="true" className="size-3.5" />
          </button>
        </li>
      ))}
    </ul>
  );
}

function NothingRecorded() {
  return (
    <Card title="Nothing recorded yet">
      <p>
        Usage is counted as clients call the proxy. Once a client has made a call, its requests,
        tokens and latency show here.
      </p>
      <p>
        <Link to="/">Set up a client</Link>
      </p>
    </Card>
  );
}

export function UsagePage() {
  const { view, setRange, setGroupBy, addFilter, removeFilter, setFailedOnly } = useUsageView();
  const [metric, setMetric] = useState<ChartMetric>("requests");
  const end = useMinuteClock();
  const range = rangeEndingAt(view.range, end);
  const filters = filterQuery(view.filters);
  const groupBy = view.groupBy ?? undefined;

  const ledger = useApiQuery<LedgerState>(USAGE_LEDGER);
  const ready = ledger.data?.available === true;
  const summary = useApiQuery<UsageSummary>(
    USAGE_SUMMARY,
    { ...range, ...filters, group_by: groupBy, limit: SUMMARY_GROUPS },
    { enabled: ready, placeholderData: keepPreviousData },
  );
  const series = useApiQuery<UsageSeries>(
    USAGE_SERIES,
    {
      ...range,
      ...filters,
      group_by: groupBy,
      groups: groupBy === undefined ? undefined : CHART_GROUPS,
      utc_offset: browserUtcOffset(new Date(end)),
    },
    { enabled: ready, placeholderData: keepPreviousData },
  );

  // A ledger can fail after it was opened; its state then says why.
  const client = useQueryClient();
  const ledgerLost = [summary.error, series.error].some(
    (error) => error !== null && callProblem(error).kind === "ledger-unavailable",
  );
  useEffect(() => {
    if (ledgerLost) {
      void client.invalidateQueries({ queryKey: [USAGE_LEDGER] });
    }
  }, [client, ledgerLost]);

  return (
    <>
      <PageHeader
        title="Usage"
        description="Calls the proxy made to providers: how many, how big, how fast, and what they cost."
        actions={
          <Link to="/usage/ledger" className={buttonClasses("secondary", "md")}>
            <Settings2 aria-hidden="true" className="size-4" />
            Ledger and prices
          </Link>
        }
      />
      <QueryState query={ledger} loading="Loading the usage ledger…">
        {(state) => (
          <div className="space-y-4">
            <LedgerNotices ledger={state} />
            {state.available && state.rows === 0 ? (
              <NothingRecorded />
            ) : (
              state.available && (
                <>
                  <div
                    role="group"
                    aria-label="What to show"
                    className="flex flex-wrap items-center gap-x-5 gap-y-2"
                  >
                    <SelectField
                      inline
                      label="Range"
                      value={view.range.id}
                      options={RANGE_PRESETS.map((preset) => ({
                        value: preset.id,
                        label: preset.label,
                      }))}
                      onChange={(event) => {
                        setRange(event.target.value);
                      }}
                    />
                    <SelectField
                      inline
                      label="Group by"
                      value={view.groupBy ?? ""}
                      options={[
                        { value: "", label: "Nothing" },
                        ...GROUP_BYS.map((value) => ({ value, label: GROUP_BY_LABELS[value] })),
                      ]}
                      onChange={(event) => {
                        const value = event.target.value;
                        setGroupBy(value === "" ? null : (value as GroupBy));
                      }}
                    />
                    <FilterChips
                      filters={view.filters}
                      onRemove={(filter) => {
                        removeFilter(filter.name);
                      }}
                    />
                  </div>
                  <QueryState query={summary} loading="Loading totals…">
                    {(data) => <Totals totals={data.totals} currency={data.currency} />}
                  </QueryState>
                  <Card
                    title="Over time"
                    actions={
                      <SelectField
                        inline
                        label="Show"
                        value={metric}
                        options={CHART_METRICS}
                        onChange={(event) => {
                          setMetric(event.target.value as ChartMetric);
                        }}
                      />
                    }
                  >
                    <QueryState query={series} loading="Loading the chart…">
                      {(data) => (
                        <>
                          <UsageChart data={data} metric={metric} />
                          {data.more_groups && (
                            <p className="text-muted">
                              The {formatInteger(CHART_GROUPS)} busiest groups are drawn; the rest
                              aren&apos;t.
                            </p>
                          )}
                          <ChartNumbers data={data} metric={metric} />
                        </>
                      )}
                    </QueryState>
                  </Card>
                  {view.groupBy !== null && (
                    <Card title={`By ${GROUP_BY_LABELS[view.groupBy].toLowerCase()}`}>
                      <QueryState query={summary} loading="Loading groups…">
                        {(data) =>
                          view.groupBy === null ? null : (
                            <Groups summary={data} groupBy={view.groupBy} onShowOnly={addFilter} />
                          )
                        }
                      </QueryState>
                    </Card>
                  )}
                  <RecentCalls
                    key={`${view.range.id} ${JSON.stringify(filters)}`}
                    range={view.range}
                    filters={filters}
                    failedOnly={view.failedOnly}
                    onFailedOnlyChange={setFailedOnly}
                    currency={state.currency ?? ""}
                  />
                </>
              )
            )}
          </div>
        )}
      </QueryState>
    </>
  );
}
