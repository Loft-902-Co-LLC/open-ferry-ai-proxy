// Secrets on screen: keys are shown masked unless asked.

/** A key as the server masks one: its first three and last four characters. */
export function maskKey(key: string): string {
  return key.length <= 10 ? "•".repeat(Math.max(key.length, 4)) : `${key.slice(0, 3)}...${key.slice(-4)}`;
}
