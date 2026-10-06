// Numbers, sizes, durations and times as the screens show them. English
// formatting, in the browser's time zone.

const integer = new Intl.NumberFormat("en");
const compact = new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 1 });
const percent = new Intl.NumberFormat("en", { style: "percent", maximumFractionDigits: 1 });
const dateTime = new Intl.DateTimeFormat("en", {
  year: "numeric",
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hourCycle: "h23",
});
const shortDateTime = new Intl.DateTimeFormat("en", {
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  hourCycle: "h23",
});

/** 1,520 */
export function formatInteger(value: number): string {
  return integer.format(value);
}

/** 4.8M, 1.5K, 920 */
export function formatCompact(value: number): string {
  return compact.format(value);
}

/** 0.8% (of a ratio, 0 to 1) */
export function formatPercent(ratio: number): string {
  return percent.format(ratio);
}

/** A cost in the ledger's currency: 4.18 USD, 0.0213 USD. */
export function formatCost(value: number | null, currency: string): string {
  if (value === null) {
    return "no price";
  }
  const digits = value !== 0 && Math.abs(value) < 1 ? 4 : 2;
  const amount = new Intl.NumberFormat("en", {
    minimumFractionDigits: 2,
    maximumFractionDigits: digits,
  }).format(value);
  return currency === "" ? amount : `${amount} ${currency}`;
}

/** 610 ms, 2.14 s, 1 min 5 s */
export function formatMillis(ms: number): string {
  if (ms < 1000) {
    return `${String(Math.round(ms))} ms`;
  }
  if (ms < 60_000) {
    return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
  }
  return formatSeconds(Math.round(ms / 1000));
}

/** 45 s, 3 min 20 s, 2 h 5 min, 3 d 4 h */
export function formatSeconds(total: number): string {
  const seconds = Math.max(0, Math.round(total));
  if (seconds < 60) {
    return `${String(seconds)} s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    const rest = seconds % 60;
    return rest === 0 ? `${String(minutes)} min` : `${String(minutes)} min ${String(rest)} s`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    const rest = minutes % 60;
    return rest === 0 ? `${String(hours)} h` : `${String(hours)} h ${String(rest)} min`;
  }
  const days = Math.floor(hours / 24);
  const rest = hours % 24;
  return rest === 0 ? `${String(days)} d` : `${String(days)} d ${String(rest)} h`;
}

/** 512 B, 47 KiB, 17.5 MiB */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${String(bytes)} B`;
  }
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit] ?? "TiB"}`;
}

/** Oct 5, 2026, 11:58:02, in the browser's time zone. */
export function formatDateTime(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : dateTime.format(date);
}

/** Oct 5, 11:58 */
export function formatShortDateTime(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : shortDateTime.format(date);
}

/** The browser's offset from UTC, in minutes east, as usage/series takes it. */
export function browserUtcOffset(at: Date = new Date()): number {
  // Not `-offset`, which gives -0 in UTC.
  return 0 - at.getTimezoneOffset();
}
