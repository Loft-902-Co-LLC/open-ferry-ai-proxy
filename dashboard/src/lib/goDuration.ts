// Go's duration text, such as `1h30m`, read as the server reads it: its
// config module's `parse_go_duration` (open-ferry-core's config/duration.rs),
// a port of Go's time.ParseDuration. The arithmetic is in BigInt, as the
// server's is in 64-bit integers, so the limits match to the nanosecond.

const LIMIT = 1n << 63n;
const WRAP = 1n << 64n;

const MICROSECOND = 1_000n;
const SECOND = 1_000_000_000n;

/** The units, by name: Go's, with both spellings of µs. */
const UNITS = new Map<string, bigint>([
  ["ns", 1n],
  ["us", MICROSECOND],
  // The micro sign and the Greek small mu.
  [`${String.fromCodePoint(0xb5)}s`, MICROSECOND],
  [`${String.fromCodePoint(0x3bc)}s`, MICROSECOND],
  ["ms", 1_000_000n],
  ["s", SECOND],
  ["m", 60n * SECOND],
  ["h", 3_600n * SECOND],
]);

/** The ASCII digits `text` starts with. */
function leadingDigits(text: string): string {
  return /^[0-9]*/.exec(text)?.[0] ?? "";
}

/** `digits` as a whole number, or null past 2^63, as Go's leadingInt refuses it. */
function wholePart(digits: string): bigint | null {
  let value = 0n;
  for (const digit of digits) {
    if (value > LIMIT / 10n) {
      return null;
    }
    value = value * 10n + BigInt(digit);
    if (value > LIMIT) {
      return null;
    }
  }
  return value;
}

/**
 * `digits` after a point, as Go's leadingFraction reads them: the value and
 * the power of ten it is over, with digits past what fits dropped.
 */
function fractionPart(digits: string): { value: bigint; scale: number } {
  let value = 0n;
  let scale = 1;
  for (const digit of digits) {
    if (value > (LIMIT - 1n) / 10n) {
      break;
    }
    const next = value * 10n + BigInt(digit);
    if (next > LIMIT) {
      break;
    }
    value = next;
    scale *= 10;
  }
  return { value, scale };
}

/**
 * `text` as a Go duration, in nanoseconds, or null when the server can't
 * read it: an optional sign, then `0`, or one or more numbers, each with a
 * unit (`ns`, `us`, `µs`, `ms`, `s`, `m` or `h`; there is none for days),
 * such as `1h30m` or `1.5h`. Nothing is trimmed.
 */
export function parseGoDuration(text: string): bigint | null {
  let rest = text;
  let negative = false;
  if (rest.startsWith("-") || rest.startsWith("+")) {
    negative = rest.startsWith("-");
    rest = rest.slice(1);
  }
  if (rest === "0") {
    return 0n;
  }
  if (rest === "") {
    return null;
  }
  let total = 0n;
  while (rest !== "") {
    if (!/^[0-9.]/.test(rest)) {
      return null;
    }
    const whole = leadingDigits(rest);
    let value = wholePart(whole);
    if (value === null) {
      return null;
    }
    rest = rest.slice(whole.length);
    let fraction = { value: 0n, scale: 1 };
    let afterPoint = "";
    if (rest.startsWith(".")) {
      afterPoint = leadingDigits(rest.slice(1));
      fraction = fractionPart(afterPoint);
      rest = rest.slice(1 + afterPoint.length);
    }
    if (whole === "" && afterPoint === "") {
      return null;
    }
    const name = /^[^0-9.]*/.exec(rest)?.[0] ?? "";
    const unit = UNITS.get(name);
    if (unit === undefined) {
      return null;
    }
    rest = rest.slice(name.length);
    if (value > LIMIT / unit) {
      return null;
    }
    value *= unit;
    if (fraction.value > 0n) {
      value += BigInt(Math.trunc(Number(fraction.value) * (Number(unit) / fraction.scale)));
      if (value > LIMIT) {
        return null;
      }
    }
    // Go adds in unsigned 64 bits, which wrap.
    total = (total + value) % WRAP;
    if (total > LIMIT) {
      return null;
    }
  }
  if (negative) {
    return -total;
  }
  return total < LIMIT ? total : null;
}
