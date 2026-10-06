// Where `npm run dev` and `npm run preview` send the app's API calls.
//
// They proxy to the open-ferry named by OPEN_FERRY_URL, which has no
// default: a developer runs their own server, on a port of their choosing,
// with a throwaway config. Port 8317 is refused on every host. It is
// CLIProxyAPI's and open-ferry's default port, and on the maintainers'
// machines a live proxy that serves their own sessions listens there; a
// development server must never reach it, by name, by address or by
// accident.

/** The port the development servers never proxy to. */
export const REFUSED_PORT = 8317;

/** The paths the development servers forward to open-ferry. */
export const PROXIED_PREFIXES = [
  "/v0/management",
  "/v8/management",
  "/open-ferry/api",
] as const;

/**
 * The origin to proxy API calls to, from OPEN_FERRY_URL.
 *
 * Throws, with a message saying what to do, when the variable is unset or
 * blank, isn't an http(s) origin, carries credentials, a path, a query or a
 * fragment, or names port 8317.
 */
export function devProxyTarget(raw: string | undefined): string {
  const value = raw?.trim() ?? "";
  if (value === "") {
    throw new Error(
      "OPEN_FERRY_URL isn't set. Start an open-ferry of your own on a loopback port " +
        "with a throwaway config, then run, for example:\n" +
        "  OPEN_FERRY_URL=http://127.0.0.1:18317 npm run dev",
    );
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error(`OPEN_FERRY_URL isn't a URL: ${value}`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error(`OPEN_FERRY_URL must be an http:// or https:// URL, not ${url.protocol}`);
  }
  if (url.username !== "" || url.password !== "") {
    throw new Error("OPEN_FERRY_URL must not hold a user name or password");
  }
  if (url.pathname !== "/" || url.search !== "" || url.hash !== "") {
    throw new Error(
      `OPEN_FERRY_URL must be an origin only, such as http://127.0.0.1:18317, not ${value}`,
    );
  }
  const port = url.port === "" ? (url.protocol === "https:" ? 443 : 80) : Number(url.port);
  if (port === REFUSED_PORT) {
    throw new Error(
      `OPEN_FERRY_URL names port ${String(REFUSED_PORT)}, which the development servers refuse: ` +
        "it is the default port of a live proxy. Run your test server on another port.",
    );
  }
  return url.origin;
}
