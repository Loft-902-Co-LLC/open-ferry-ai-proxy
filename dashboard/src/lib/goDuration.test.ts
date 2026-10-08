import { describe, expect, it } from "vitest";

import { parseGoDuration } from "./goDuration";

const SECOND = 1_000_000_000n;
const MINUTE = 60n * SECOND;
const HOUR = 60n * MINUTE;
const MAX = (1n << 63n) - 1n;

describe("parseGoDuration", () => {
  // The server's own cases (open-ferry-core's config/duration.rs).
  it.each<[string, bigint | null]>([
    ["0", 0n],
    ["-0", 0n],
    ["5s", 5n * SECOND],
    ["+5s", 5n * SECOND],
    ["-1.5h", -90n * MINUTE],
    ["2h45m", 2n * HOUR + 45n * MINUTE],
    ["300ms", 300_000_000n],
    ["1.s", SECOND],
    [".5s", 500_000_000n],
    ["1us", 1_000n],
    [`1${String.fromCodePoint(0xb5)}s`, 1_000n],
    [`1${String.fromCodePoint(0x3bc)}s`, 1_000n],
    ["9223372036854775807ns", MAX],
    ["-9223372036854775808ns", -MAX - 1n],
    ["9223372036854775808ns", null],
    // Go's sum wraps at 2^64.
    ["9223372036854775808ns9223372036854775808ns1ns", 1n],
    ["9223372036854775808ns9223372036854775808ns", 0n],
    ["-9223372036854775808ns9223372036854775808ns5ns", -5n],
    ["9223372036854775808ns1ns", null],
    ["", null],
    ["-", null],
    ["5", null],
    ["s", null],
    [".s", null],
    ["5x", null],
    ["1d", null],
    ["3000000h", null],
  ])("reads %j as the server does", (text, nanos) => {
    expect(parseGoDuration(text)).toBe(nanos);
  });

  it("reads several numbers, a fraction and no spaces", () => {
    expect(parseGoDuration("1h30m")).toBe(90n * MINUTE);
    expect(parseGoDuration("1.5h")).toBe(90n * MINUTE);
    expect(parseGoDuration("0s")).toBe(0n);
    expect(parseGoDuration("00")).toBeNull();
    expect(parseGoDuration(" 1h")).toBeNull();
    expect(parseGoDuration("1h ")).toBeNull();
    expect(parseGoDuration("1H")).toBeNull();
    expect(parseGoDuration("1h-5m")).toBeNull();
  });
});
