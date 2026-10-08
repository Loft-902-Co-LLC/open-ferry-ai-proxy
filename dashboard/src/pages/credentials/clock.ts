// Times of day, in the browser's time zone, as the credential lists give
// them: when a list was read, and when a rest ends.

const seconds = new Intl.DateTimeFormat("en", {
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hourCycle: "h23",
});
const minutes = new Intl.DateTimeFormat("en", {
  hour: "2-digit",
  minute: "2-digit",
  hourCycle: "h23",
});

/** "14:32:05" */
export function clockTime(at: number): string {
  return seconds.format(at);
}

/** "14:32" */
export function clockMinutes(at: number): string {
  return minutes.format(at);
}
