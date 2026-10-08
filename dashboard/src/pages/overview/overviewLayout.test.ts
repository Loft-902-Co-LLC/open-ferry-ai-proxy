import { describe, expect, it } from "vitest";

import { overviewLayout, type SetupFacts } from "./overviewLayout";
import { startOfDay } from "./TodayCard";

/** A proxy with a credential and calls recorded: set up. */
const SET_UP: SetupFacts = {
  connected: true,
  recordedCalls: 12,
  recording: true,
  clientKey: true,
  safeMode: false,
};

describe("the Overview's layout", () => {
  it("leads with health once a call is recorded", () => {
    expect(overviewLayout(SET_UP)).toBe("health");
    // Recorded before recording was turned off.
    expect(overviewLayout({ ...SET_UP, recording: false, clientKey: false })).toBe("health");
  });

  it("leads with the setup while there's nothing to send requests with", () => {
    expect(overviewLayout({ ...SET_UP, connected: false })).toBe("setup");
  });

  it("leads with the setup in safe mode, whatever else holds", () => {
    expect(overviewLayout({ ...SET_UP, safeMode: true })).toBe("setup");
  });

  it("leads with the setup while a recording ledger holds no call", () => {
    expect(overviewLayout({ ...SET_UP, recordedCalls: 0 })).toBe("setup");
  });

  it("goes by the client keys when the ledger can't tell", () => {
    // Recording is off, or there's no ledger to read.
    for (const facts of [
      { recordedCalls: 0, recording: false },
      { recordedCalls: null, recording: false },
    ]) {
      expect(overviewLayout({ ...SET_UP, ...facts })).toBe("health");
      expect(overviewLayout({ ...SET_UP, ...facts, clientKey: false })).toBe("setup");
    }
  });
});

describe("the start of today", () => {
  it("is the midnight before, in the browser's time zone", () => {
    const afternoon = new Date(2026, 9, 7, 15, 30, 12).getTime();
    expect(startOfDay(afternoon)).toBe(new Date(2026, 9, 7).toISOString());
    expect(startOfDay(new Date(2026, 9, 7).getTime())).toBe(new Date(2026, 9, 7).toISOString());
    expect(startOfDay(new Date(2026, 9, 7).getTime() - 1)).toBe(new Date(2026, 9, 6).toISOString());
  });
});
