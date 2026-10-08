// Time ranges as the screens offer them: the last hour, today, the last day,
// week...

import { useEffect, useState } from "react";

export interface RangePreset {
  id: string;
  label: string;
  /** Where the range starts (ms), given where it ends. */
  start: (end: number) => number;
}

/** The midnight that starts the day `at` (ms) is in, in the browser's time zone. */
export function startOfDay(at: number): string {
  const day = new Date(at);
  day.setHours(0, 0, 0, 0);
  return day.toISOString();
}

/** The range of the last `seconds`. */
function last(id: string, label: string, seconds: number): RangePreset {
  return { id, label, start: (end) => end - seconds * 1000 };
}

const LAST_DAY = last("24h", "Last 24 hours", 86_400);

/**
 * From midnight in the browser's time zone, as the Overview's Today card
 * counts. A range ends after its last moment, so it is the day before that.
 */
export const TODAY: RangePreset = {
  id: "today",
  label: "Today",
  start: (end) => Date.parse(startOfDay(end - 1)),
};

export const RANGE_PRESETS: readonly RangePreset[] = [
  last("1h", "Last hour", 3600),
  TODAY,
  LAST_DAY,
  last("7d", "Last 7 days", 7 * 86_400),
  last("30d", "Last 30 days", 30 * 86_400),
  last("90d", "Last 90 days", 90 * 86_400),
];

export const DEFAULT_RANGE = LAST_DAY.id;
const DEFAULT_PRESET = LAST_DAY;

export function presetById(id: string): RangePreset {
  return RANGE_PRESETS.find((preset) => preset.id === id) ?? DEFAULT_PRESET;
}

/** The range `preset` ending at `end` (ms), as RFC 3339 `from` and `to`. */
export function rangeEndingAt(preset: RangePreset, end: number): { from: string; to: string } {
  return {
    from: new Date(preset.start(end)).toISOString(),
    to: new Date(end).toISOString(),
  };
}

/**
 * The end of the current minute, moving on each minute. Ranges end there,
 * so their queries stay the same for a minute, then move on and refresh.
 */
export function useMinuteClock(): number {
  const [end, setEnd] = useState(nextMinute);
  useEffect(() => {
    const timer = window.setInterval(() => {
      setEnd(nextMinute());
    }, 15_000);
    return () => {
      window.clearInterval(timer);
    };
  }, []);
  return end;
}

function nextMinute(): number {
  return Math.ceil((Date.now() + 1) / 60_000) * 60_000;
}
