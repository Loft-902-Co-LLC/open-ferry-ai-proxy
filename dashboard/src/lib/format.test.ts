import { describe, expect, it } from "vitest";

import {
  browserUtcOffset,
  formatBytes,
  formatCompact,
  formatCost,
  formatDateTime,
  formatMillis,
  formatSeconds,
} from "./format";
import { decimalField, optionalDecimalField, wholeNumberField } from "./fields";

describe("formatting", () => {
  it("shows costs with the ledger's currency, and none as no price", () => {
    expect(formatCost(4.1825, "USD")).toBe("4.18 USD");
    expect(formatCost(0.0213, "EUR")).toBe("0.0213 EUR");
    expect(formatCost(0, "USD")).toBe("0.00 USD");
    expect(formatCost(null, "USD")).toBe("no price");
    expect(formatCost(12, "")).toBe("12.00");
  });

  it("shows durations in the unit that reads best", () => {
    expect(formatMillis(610)).toBe("610 ms");
    expect(formatMillis(2140)).toBe("2.14 s");
    expect(formatMillis(15_320)).toBe("15.3 s");
    expect(formatMillis(65_000)).toBe("1 min 5 s");
    expect(formatSeconds(1800)).toBe("30 min");
    expect(formatSeconds(7500)).toBe("2 h 5 min");
    expect(formatSeconds(3 * 86_400 + 4 * 3600)).toBe("3 d 4 h");
  });

  it("shows sizes and counts compactly", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(48_120)).toBe("47 KiB");
    expect(formatBytes(1_572_864)).toBe("1.5 MiB");
    expect(formatCompact(4_810_000)).toBe("4.8M");
  });

  it("shows times in the browser's zone, and keeps text it can't read", () => {
    // Tests run in UTC.
    expect(formatDateTime("2026-10-05T11:58:02.114Z")).toBe("Oct 5, 2026, 11:58:02");
    expect(formatDateTime("not a time")).toBe("not a time");
    expect(browserUtcOffset(new Date("2026-10-05T00:00:00Z"))).toBe(0);
  });
});

describe("number fields", () => {
  it("reads whole numbers within their range", () => {
    const days = wholeNumberField("Days", 1, 3650);
    expect(days.parse(" 30 ")).toBe(30);
    expect(days.safeParse("0").success).toBe(false);
    expect(days.safeParse("1.5").success).toBe(false);
    expect(days.safeParse("").error?.issues[0]?.message).toBe(
      "Days is a whole number from 1 to 3,650.",
    );
  });

  it("reads prices, and an empty optional one as none", () => {
    expect(decimalField("Input", 0, 1e6).parse("1.25")).toBe(1.25);
    expect(decimalField("Input", 0, 1e6).parse(".5")).toBe(0.5);
    expect(decimalField("Input", 0, 1e6).safeParse("1e3").success).toBe(false);
    expect(decimalField("Input", 0, 1e6).safeParse("1000001").success).toBe(false);
    expect(optionalDecimalField("Cache", 0, 1e6).parse("")).toBeNull();
    expect(optionalDecimalField("Cache", 0, 1e6).parse("0.125")).toBe(0.125);
    expect(optionalDecimalField("Cache", 0, 1e6).safeParse("-1").success).toBe(false);
  });
});
