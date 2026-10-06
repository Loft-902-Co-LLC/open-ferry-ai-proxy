// Form fields hold text; these read it as the numbers the APIs take, with
// messages that say what is wrong.

import { z } from "./zod";

/** A whole number from `min` to `max`, from a text field. */
export function wholeNumberField(label: string, min: number, max: number) {
  const range = `${label} is a whole number from ${min.toLocaleString("en")} to ${max.toLocaleString("en")}.`;
  return z
    .string()
    .trim()
    .regex(/^\d+$/, range)
    .transform(Number)
    .pipe(z.number().int(range).min(min, range).max(max, range));
}

/**
 * A whole number, 0 or more, from a text field: a count the server holds as
 * a 64-bit integer. The cap is the largest whole number a number in the page
 * holds exactly, so what is sent is what was typed.
 */
export function countField(label: string) {
  const format = `${label} is a whole number, 0 or more.`;
  const tooLarge = `${label} can be at most ${Number.MAX_SAFE_INTEGER.toLocaleString("en")}.`;
  return z
    .string()
    .trim()
    .regex(/^\d+$/, format)
    .refine((text) => !/^\d+$/.test(text) || Number(text) <= Number.MAX_SAFE_INTEGER, tooLarge)
    .transform(Number);
}

const DECIMAL = /^(\d+(\.\d*)?|\.\d+)$/;

/** A number from `min` to `max`, decimals allowed, from a text field. */
export function decimalField(label: string, min: number, max: number) {
  const range = `${label} is a number from ${min.toLocaleString("en")} to ${max.toLocaleString("en")}.`;
  return z
    .string()
    .trim()
    .regex(DECIMAL, range)
    .transform(Number)
    .pipe(z.number().min(min, range).max(max, range));
}

/** As `decimalField`, or empty for none (null). */
export function optionalDecimalField(label: string, min: number, max: number) {
  const range = `${label} is empty or a number from ${min.toLocaleString("en")} to ${max.toLocaleString("en")}.`;
  return z
    .string()
    .trim()
    .refine((text) => text === "" || DECIMAL.test(text), range)
    .transform((text) => (text === "" ? null : Number(text)))
    .pipe(z.number().min(min, range).max(max, range).nullable());
}
