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
/** The least height, in pixels, a count of failed requests above zero is drawn at. */
export const FAILED_MIN_HEIGHT = 3;

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
  /** Failed requests of all calls, in red, at the top of their stack. */
  failed: boolean;
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
        { key: "succeeded", name: "Succeeded", colour: groupColour(0), failed: false },
        { key: "failed", name: "Failed", colour: FAILED_COLOUR, failed: true },
      ],
      rows: first.points.map((point) => ({
        start: point.start,
        label: label(point.start),
        values: [Math.max(0, point.metrics.requests - point.metrics.errors), point.metrics.errors],
      })),
    };
  }
  const failed = ungrouped && metric === "errors";
  return {
    series: data.series.map((series, index) => ({
      key: series.key ?? "all",
      name: seriesName(series.label, data.group_by),
      colour: failed ? FAILED_COLOUR : groupColour(index),
      failed,
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
 * The least height, in pixels, series `series` is drawn at in row `row`: a
 * few pixels for failed requests above zero, so a handful among many still
 * shows; nothing for the rest, which are drawn to scale. Failures top their
 * stack, so this lifts a bar's top by a pixel or two at most and changes no
 * other segment.
 */
export function minHeight(chart: ChartData, series: number, row: number): number {
  const failed = chart.series[series]?.failed === true;
  return failed && (chart.rows[row]?.values[series] ?? 0) > 0 ? FAILED_MIN_HEIGHT : 0;
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
              // A line of the surface between segments keeps them apart. The
              // failures go without one, which would cover a thin red top.
              stroke={series.failed ? "none" : "var(--of-surface)"}
              strokeWidth={series.failed ? 0 : 1}
              minPointSize={(_value, row) => minHeight(chart, index, row)}
              isAnimationActive={false}
            />
          ))}
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
}
