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

/** How many group colours index.css has, as --of-chart-1 and on. */
const GROUP_COLOUR_COUNT = 5;
const FAILED_COLOUR = "var(--of-danger)";

/**
 * A group's colour, from the tokens: each keeps 3:1 with the surface in both
 * themes, and each differs in lightness from the next, so neighbours in a
 * stack stay apart. Red isn't one of them: it means failed.
 */
function groupColour(index: number): string {
  return `var(--of-chart-${String((index % GROUP_COLOUR_COUNT) + 1)})`;
}

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

/** One stack in the chart, and one column in its numbers. */
export interface ChartSeries {
  key: string;
  name: string;
  colour: string;
}

/** What the chart draws and its table lists: the series, and a row per bucket. */
export interface ChartData {
  series: ChartSeries[];
  rows: ChartRow[];
}

/**
 * The chart's series and rows. Requests of all calls split into those that
 * succeeded and those that failed, stacked, so a bar is still every request
 * and its red top is the failures. Grouped, each group is one stack whose
 * height matches its row in the groups table; "Failed requests" shows the
 * failures by group.
 */
export function chartData(data: UsageSeries, metric: ChartMetric): ChartData {
  const first = data.series[0];
  if (first === undefined) {
    return { series: [], rows: [] };
  }
  const ungrouped = data.group_by === null && data.series.length === 1;
  const label = (start: string) => bucketLabel(start, data.bucket);
  if (ungrouped && metric === "requests") {
    return {
      series: [
        { key: "succeeded", name: "Succeeded", colour: groupColour(0) },
        { key: "failed", name: "Failed", colour: FAILED_COLOUR },
      ],
      rows: first.points.map((point) => ({
        start: point.start,
        label: label(point.start),
        values: [Math.max(0, point.metrics.requests - point.metrics.errors), point.metrics.errors],
      })),
    };
  }
  return {
    series: data.series.map((series, index) => ({
      key: series.key ?? "all",
      name: seriesName(series.label, data.group_by),
      colour: ungrouped && metric === "errors" ? FAILED_COLOUR : groupColour(index),
    })),
    rows: first.points.map((point, index) => ({
      start: point.start,
      label: label(point.start),
      values: data.series.map((series) => {
        const at = series.points[index];
        return at === undefined ? 0 : metricValue(at.metrics, metric);
      }),
    })),
  };
}

/**
 * Keeps the legend and the tooltip in the series' order, which is the table's,
 * instead of recharts' default, by name. Each bar's data key is "s" and its
 * index.
 */
function seriesOrder(item: { dataKey?: unknown }): number {
  return typeof item.dataKey === "string" ? Number(item.dataKey.slice(1)) : 0;
}

export interface UsageChartProps {
  data: UsageSeries;
  metric: ChartMetric;
}

/** Bars over time, stacked by group. Decorative for screen readers, and out
 * of the tab order: the table beside it has the numbers. */
export function UsageChart({ data, metric }: UsageChartProps) {
  const chart = chartData(data, metric);
  const rows = chart.rows.map((row) => {
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
        <BarChart
          data={rows}
          margin={{ top: 8, right: 8, bottom: 0, left: 0 }}
          accessibilityLayer={false}
        >
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
            itemStyle={{ color: "var(--of-fg)" }}
            itemSorter={seriesOrder}
          />
          {chart.series.length > 1 && (
            <Legend
              wrapperStyle={{ color: "var(--of-fg)" }}
              itemSorter={seriesOrder}
              // Legend text takes its series' colour unless told otherwise,
              // and some of those are too light to read as text.
              formatter={(value: string) => <span className="text-fg">{value}</span>}
            />
          )}
          {chart.series.map((series, index) => (
            <Bar
              key={series.key}
              dataKey={`s${String(index)}`}
              name={series.name}
              stackId="usage"
              fill={series.colour}
              // A line of the surface between segments keeps them apart.
              stroke="var(--of-surface)"
              strokeWidth={1}
              isAnimationActive={false}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
