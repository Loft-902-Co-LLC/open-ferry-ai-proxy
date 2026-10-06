// CLIProxyAPI's safe-mode message sends users to
// `/management.html?safe-mode=configure`, which open-ferry redirects to
// `/dashboard/?safe-mode=configure`: the app then opens its API-key setup.

/** Whether `search` (a URL's query, with or without `?`) asks for it. */
export function asksForSafeModeSetup(search: string): boolean {
  return new URLSearchParams(search).get("safe-mode") === "configure";
}

/** CLIProxyAPI's example client keys, which put the proxy in safe mode. */
export const EXAMPLE_API_KEYS: readonly string[] = [
  "your-api-key-1",
  "your-api-key-2",
  "your-api-key-3",
];
