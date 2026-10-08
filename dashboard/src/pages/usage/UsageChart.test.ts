import { describe, expect, it } from "vitest";

import { metrics, series } from "../../test/fixtures";
import { chartData, FAILED_MIN_HEIGHT, minHeight } from "./UsageChart";

describe("the chart's data", () => {
  it("splits requests into succeeded and failed when there are no groups", () => {
    const chart = chartData(series(), "requests");
    expect(chart.series).toEqual([
      { key: "succeeded", name: "Succeeded", colour: "var(--of-chart-1)", failed: false },
      { key: "failed", name: "Failed", colour: "var(--of-danger)", failed: true },
    ]);
    expect(chart.rows.map((row) => row.values)).toEqual([
      [688, 12],
      [808, 12],
    ]);
  });

  it("draws failed requests in the danger colour", () => {
    const chart = chartData(series(), "errors");
    expect(chart.series).toEqual([
      { key: "all", name: "All calls", colour: "var(--of-danger)", failed: true },
    ]);
    expect(chart.rows.map((row) => row.values)).toEqual([[12], [12]]);
  });

  it("keeps one series per group, in the group colours", () => {
    const point = (requests: number) => ({
      start: "2026-10-05T10:00:00.000Z",
      metrics: metrics({ requests }),
    });
    const labels = ["a", "b", "c", "d", "e", "f"];
    const chart = chartData(
      series({
        group_by: "model",
        series: labels.map((label, index) => ({
          key: label,
          label,
          points: [point(index + 1)],
        })),
      }),
      "requests",
    );
    expect(chart.series.map((one) => one.name)).toEqual(labels);
    expect(chart.series.some((one) => one.failed)).toBe(false);
    expect(chart.series.map((one) => one.colour)).toEqual([
      "var(--of-chart-1)",
      "var(--of-chart-2)",
      "var(--of-chart-3)",
      "var(--of-chart-4)",
      "var(--of-chart-5)",
      "var(--of-chart-1)",
    ]);
    expect(chart.rows[0]?.values).toEqual([1, 2, 3, 4, 5, 6]);
  });

  it("draws any failures at all a few pixels high, and the rest to scale", () => {
    const point = (start: string, requests: number, errors: number) => ({
      start,
      metrics: metrics({ requests, errors }),
    });
    const data = series({
      series: [
        {
          key: null,
          label: null,
          points: [
            point("2026-10-05T10:00:00.000Z", 5000, 1),
            point("2026-10-05T11:00:00.000Z", 4000, 0),
          ],
        },
      ],
    });
    const requests = chartData(data, "requests");
    // Succeeded, then failed, in each row.
    expect(minHeight(requests, 0, 0)).toBe(0);
    expect(minHeight(requests, 1, 0)).toBe(FAILED_MIN_HEIGHT);
    expect(minHeight(requests, 1, 1)).toBe(0);
    const errors = chartData(data, "errors");
    expect(minHeight(errors, 0, 0)).toBe(FAILED_MIN_HEIGHT);
    expect(minHeight(errors, 0, 1)).toBe(0);
    // The numbers stay as they are.
    expect(requests.rows.map((row) => row.values)).toEqual([
      [4999, 1],
      [4000, 0],
    ]);
  });

  it("has nothing to draw without a series", () => {
    expect(chartData(series({ series: [] }), "requests")).toEqual({ series: [], rows: [] });
  });
});
