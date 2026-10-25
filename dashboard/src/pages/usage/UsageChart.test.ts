import { describe, expect, it, vi } from "vitest";

import type { Bucket } from "../../api/dashboard";
import { metrics, series } from "../../test/fixtures";
import { chartData, FAILED_MIN_HEIGHT, minHeight } from "./UsageChart";

const DateTimeFormat = Intl.DateTimeFormat;

/** The old per-point rule, with the test's default time zone made explicit. */
function oldBucketLabel(start: string, bucket: Bucket, timeZone: string): string {
  const options: Intl.DateTimeFormatOptions =
    bucket === "day"
      ? { month: "short", day: "numeric" }
      : bucket === "hour"
        ? { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hourCycle: "h23" }
        : { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };
  return new DateTimeFormat("en", { ...options, timeZone }).format(new Date(start));
}

function mockTimeZone(timeZone: string) {
  return vi.spyOn(Intl, "DateTimeFormat").mockImplementation(function (locales, options) {
    return new DateTimeFormat(locales, { timeZone, ...options });
  });
}

function seriesWithPoints(bucket: Bucket, grouped: boolean, starts: string[]) {
  return series({
    bucket,
    bucket_seconds: bucket === "day" ? 86_400 : bucket === "hour" ? 3600 : 60,
    group_by: grouped ? "model" : null,
    series: (grouped ? ["first", "second"] : [null]).map((key, group) => ({
      key,
      label: key,
      points: starts.map((start, index) => ({
        start,
        metrics: metrics({ requests: 20 + group * 10 + index, errors: index % 3 }),
      })),
    })),
  });
}

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

  it.each(["minute", "hour", "day"] as const)(
    "keeps the old %s labels and order across daylight-saving changes",
    (bucket) => {
      const timeZone = "America/New_York";
      mockTimeZone(timeZone);
      const starts = [
        "2026-03-08T04:59:00.000Z",
        "2026-03-08T05:00:00.000Z",
        "2026-03-08T06:59:00.000Z",
        "2026-03-08T07:00:00.000Z",
        "2026-11-01T05:59:00.000Z",
        "2026-11-01T06:00:00.000Z",
      ];
      const expected = starts.map((start) => ({
        start,
        label: oldBucketLabel(start, bucket, timeZone),
      }));
      for (const grouped of [false, true]) {
        const data = seriesWithPoints(bucket, grouped, starts);
        for (const metric of ["requests", "errors", "tokens", "cost"] as const) {
          expect(chartData(data, metric).rows.map(({ start, label }) => ({ start, label }))).toEqual(
            expected,
          );
        }
      }
    },
  );

  it.each([false, true])(
    "constructs one formatter per long chart-data call (grouped: %s)",
    (grouped) => {
      const timeZone = "America/New_York";
      const starts = Array.from({ length: 2048 }, (_, index) =>
        new Date(Date.parse("2026-03-07T00:00:00.000Z") + index * 3_600_000).toISOString(),
      );
      const data = seriesWithPoints("hour", grouped, starts);
      const expected = starts.map((start, index) => ({
        start,
        label: oldBucketLabel(start, "hour", timeZone),
        values: grouped ? [20 + index, 30 + index] : [20 + index - (index % 3), index % 3],
      }));
      const formatter = mockTimeZone(timeZone);
      expect(chartData(data, "requests").rows).toEqual(expected);
      expect(formatter).toHaveBeenCalledExactlyOnceWith("en", {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        hourCycle: "h23",
      });
      expect(chartData(data, "requests").rows).toEqual(expected);
      expect(formatter).toHaveBeenCalledTimes(2);
    },
  );

  it("does not construct a formatter without points", () => {
    const formatter = vi.spyOn(Intl, "DateTimeFormat");
    expect(chartData(series({ series: [] }), "requests")).toEqual({ series: [], rows: [] });
    for (const grouped of [false, true]) {
      expect(chartData(seriesWithPoints("hour", grouped, []), "requests").rows).toEqual([]);
    }
    expect(formatter).not.toHaveBeenCalled();
  });

  it("uses the current default time zone on each call", () => {
    const start = "2026-03-08T07:00:00.000Z";
    const data = seriesWithPoints("hour", false, [start]);
    const formatter = mockTimeZone("UTC");
    const utc = chartData(data, "requests").rows[0]?.label;
    expect(utc).toBe(oldBucketLabel(start, "hour", "UTC"));
    mockTimeZone("America/New_York");
    const newYork = chartData(data, "requests").rows[0]?.label;
    expect(newYork).toBe(oldBucketLabel(start, "hour", "America/New_York"));
    expect(newYork).not.toBe(utc);
    expect(formatter).toHaveBeenCalledTimes(2);
  });
});
