// Time ranges as the screens offer them: the last hour, day, week...

import { useEffect, useState } from "react";

export interface RangePreset {
  id: string;
  label: string;
  seconds: number;
}

const LAST_DAY: RangePreset = { id: "24h", label: "Last 24 hours", seconds: 86_400 };

export const RANGE_PRESETS: readonly RangePreset[] = [
  { id: "1h", label: "Last hour", seconds: 3600 },
  LAST_DAY,
  { id: "7d", label: "Last 7 days", seconds: 7 * 86_400 },
  { id: "30d", label: "Last 30 days", seconds: 30 * 86_400 },
  { id: "90d", label: "Last 90 days", seconds: 90 * 86_400 },
];

export const DEFAULT_RANGE = LAST_DAY.id;
const DEFAULT_PRESET = LAST_DAY;

export function presetById(id: string): RangePreset {
  return RANGE_PRESETS.find((preset) => preset.id === id) ?? DEFAULT_PRESET;
}

/** The range `preset` ending at `end` (ms), as RFC 3339 `from` and `to`. */
export function rangeEndingAt(preset: RangePreset, end: number): { from: string; to: string } {
  return {
    from: new Date(end - preset.seconds * 1000).toISOString(),
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
