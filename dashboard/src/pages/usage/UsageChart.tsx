import { Bar, BarChart, CartesianGrid, Legend, ResponsiveContainer, Tooltip, XAxis, YAxis } from "recharts";

import type { Bucket, Metrics, UsageSeries } from "../../api/dashboard";
import { formatCompact, formatCost, formatInteger } from "../../lib/format";

export type ChartMetric = "requests" | "errors" | "tokens" | "cost";

export const CHART_METRICS: readonly { value: ChartMetric; label: string }[] = [
  { value: "requests", label: "Requests" },
  { value: "errors", label: "Failed requests" },
  { value: "tokens", label: "Tokens" },
  { value: "cost", label: "Cost" },
];

/**
 * Series colours: each keeps 3:1 contrast with the light and the dark
 * surface, as WCAG asks of graphics. The legend and table name them too.
 */
const COLOURS = ["#2f6fd6", "#c26a05", "#08875f", "#d1343c", "#7c5ce0", "#64748b"];

export function metricValue(metrics: Metrics, metric: ChartMetric): number {
  switch (metric) {
    case "requests":
      return metrics.requests;
    case "errors":
      return metrics.errors;
    case "tokens":
      return metrics.total_tokens;
    case "cost":
      return metrics.cost ?? 0;
  }
}

function bucketLabel(iso: string, bucket: Bucket): string {
  const date = new Date(iso);
  const options: Intl.DateTimeFormatOptions =
    bucket === "day"
      ? { month: "short", day: "numeric" }
      : bucket === "hour"
        ? { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hourCycle: "h23" }
        : { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };
  return new Intl.DateTimeFormat("en", options).format(date);
}

/** A series' name: its group's label, or what it stands for. */
export function seriesName(label: string | null, groupBy: UsageSeries["group_by"]): string {
  if (label === null) {
    return "All calls";
  }
  if (label === "") {
    return groupBy === "client_key" ? "No client key" : "None";
  }
  return label;
}

export interface ChartRow {
  start: string;
  label: string;
  values: number[];
}

/** The series as rows, one per bucket, with one value per series. */
export function chartRows(data: UsageSeries, metric: ChartMetric): ChartRow[] {
  const first = data.series[0];
  if (first === undefined) {
    return [];
  }
  return first.points.map((point, index) => ({
    start: point.start,
    label: bucketLabel(point.start, data.bucket),
    values: data.series.map((series) => {
      const at = series.points[index];
      return at === undefined ? 0 : metricValue(at.metrics, metric);
    }),
  }));
}

export interface UsageChartProps {
  data: UsageSeries;
  metric: ChartMetric;
}

/** Bars over time, stacked by group. Decorative for screen readers: the
 * table beside it has the numbers. */
export function UsageChart({ data, metric }: UsageChartProps) {
  const rows = chartRows(data, metric).map((row) => {
    const entry: Record<string, string | number> = { label: row.label };
    row.values.forEach((value, index) => {
      entry[`s${String(index)}`] = value;
    });
    return entry;
  });
  const format = (value: number) =>
    metric === "cost" ? formatCost(value, data.currency) : formatInteger(value);
  return (
    <div className="h-64 w-full text-muted" aria-hidden="true">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={rows} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
          <CartesianGrid vertical={false} stroke="currentColor" strokeOpacity={0.2} />
          <XAxis
            dataKey="label"
            tick={{ fill: "currentColor", fontSize: 12 }}
            tickLine={false}
            axisLine={{ stroke: "currentColor", strokeOpacity: 0.4 }}
            minTickGap={16}
          />
          <YAxis
            tick={{ fill: "currentColor", fontSize: 12 }}
            tickLine={false}
            axisLine={false}
            width={56}
            tickFormatter={(value: number) => formatCompact(value)}
          />
          <Tooltip
            cursor={{ fill: "currentColor", fillOpacity: 0.08 }}
            formatter={(value) => (typeof value === "number" ? format(value) : String(value))}
            contentStyle={{
              background: "var(--of-surface)",
              border: "1px solid var(--of-line)",
              borderRadius: 6,
              color: "var(--of-fg)",
            }}
            labelStyle={{ color: "var(--of-fg)" }}
          />
          {data.series.length > 1 && <Legend wrapperStyle={{ color: "var(--of-fg)" }} />}
          {data.series.map((series, index) => (
            <Bar
              key={series.key ?? "all"}
              dataKey={`s${String(index)}`}
              name={seriesName(series.label, data.group_by)}
              stackId="usage"
              fill={COLOURS[index % COLOURS.length]}
              isAnimationActive={false}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
