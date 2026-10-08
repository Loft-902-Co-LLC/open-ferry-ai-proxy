import { describe, expect, it } from "vitest";

import { DEFAULT_RANGE, presetById, rangeEndingAt, startOfDay } from "./timeRange";

describe("the start of today", () => {
  it("is the midnight before, in the browser's time zone", () => {
    const afternoon = new Date(2026, 9, 7, 15, 30, 12).getTime();
    expect(startOfDay(afternoon)).toBe(new Date(2026, 9, 7).toISOString());
    expect(startOfDay(new Date(2026, 9, 7).getTime())).toBe(new Date(2026, 9, 7).toISOString());
    expect(startOfDay(new Date(2026, 9, 7).getTime() - 1)).toBe(new Date(2026, 9, 6).toISOString());
  });
});

describe("a range", () => {
  it("counts back from its end", () => {
    const end = new Date(2026, 9, 7, 15, 31).getTime();
    expect(rangeEndingAt(presetById("1h"), end)).toEqual({
      from: new Date(2026, 9, 7, 14, 31).toISOString(),
      to: new Date(end).toISOString(),
    });
    expect(rangeEndingAt(presetById("7d"), end).from).toBe(
      new Date(2026, 8, 30, 15, 31).toISOString(),
    );
  });

  it("starts today's at midnight, until the day is out", () => {
    const today = presetById("today");
    expect(today.label).toBe("Today");
    const end = new Date(2026, 9, 7, 15, 31).getTime();
    expect(rangeEndingAt(today, end).from).toBe(new Date(2026, 9, 7).toISOString());
    // A range ending at the next midnight still covers the day before it.
    const midnight = new Date(2026, 9, 8).getTime();
    expect(rangeEndingAt(today, midnight)).toEqual({
      from: new Date(2026, 9, 7).toISOString(),
      to: new Date(midnight).toISOString(),
    });
  });

  it("is the last 24 hours when the address names none it knows", () => {
    expect(presetById("yesterday").id).toBe(DEFAULT_RANGE);
    expect(DEFAULT_RANGE).toBe("24h");
  });
});
