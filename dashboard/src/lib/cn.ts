/** Joins class names, skipping the falsy ones. */
export function cn(...names: (string | false | null | undefined)[]): string {
  return names.filter(Boolean).join(" ");
}
