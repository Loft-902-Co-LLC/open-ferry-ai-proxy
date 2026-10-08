// `management.separate-address`, read as the server reads it: open-ferry-
// core's config::management_address (`ManagementAddress::parse`, `reach`
// and `base_url`, and the load's check against `server.port`). The checks
// run in the server's order, so the first problem said is the one the
// server would refuse the value for; the words are the dashboard's own.

/** An address the server takes: the host as written, without brackets, and the port. */
export interface ManagementAddress {
  /** An address or a host name; empty for every interface. */
  host: string;
  port: number;
}

/** Who can connect to the address, as far as its host tells. */
export type ManagementReach = "loopback" | "every-interface" | "address" | "name";

/** What a fixed value looks like. */
const EXAMPLE = "such as 127.0.0.1:8318";

/** An IPv4 address's four octets, as Rust reads one: no leading zeros. */
function ipv4Octets(text: string): number[] | null {
  const parts = text.split(".");
  if (parts.length !== 4 || !parts.every((part) => /^(0|[1-9][0-9]{0,2})$/.test(part))) {
    return null;
  }
  const octets = parts.map(Number);
  return octets.every((octet) => octet <= 255) ? octets : null;
}

/**
 * The 16-bit groups of `part`, groups of 1 to 4 hex digits split by colons,
 * the last of which may be an IPv4 address (two groups) when `ipv4` allows.
 */
function groupsIn(part: string, ipv4: boolean): number[] | null {
  if (part === "") {
    return [];
  }
  const pieces = part.split(":");
  const groups: number[] = [];
  for (const [index, piece] of pieces.entries()) {
    const octets = ipv4 && index === pieces.length - 1 ? ipv4Octets(piece) : null;
    if (octets !== null) {
      const [a = 0, b = 0, c = 0, d = 0] = octets;
      groups.push(a * 256 + b, c * 256 + d);
    } else if (/^[0-9a-f]{1,4}$/i.test(piece)) {
      groups.push(Number.parseInt(piece, 16));
    } else {
      return null;
    }
  }
  return groups;
}

/**
 * `text` as an IPv6 address, as Rust's `Ipv6Addr` reads one: its eight
 * groups, or null. One `::` stands for at least one group of zeros, and an
 * IPv4 address may end it, but not come before the `::`. No zone.
 */
export function ipv6Groups(text: string): number[] | null {
  const halves = text.split("::");
  if (halves.length === 1) {
    const groups = groupsIn(text, true);
    return groups?.length === 8 ? groups : null;
  }
  if (halves.length !== 2) {
    return null;
  }
  const head = groupsIn(halves[0] ?? "", false);
  const tail = groupsIn(halves[1] ?? "", true);
  if (head === null || tail === null || head.length + tail.length > 7) {
    return null;
  }
  return [...head, ...new Array<number>(8 - head.length - tail.length).fill(0), ...tail];
}

/** Characters a host can't have. */
const NOT_HOST = /[\s/[\]@?#]/;

/**
 * `text`, trimmed, as `host:port`, or what is wrong with it, saying how to
 * put it right. Empty isn't an address: the setting is off then.
 */
export function parseManagementAddress(
  text: string,
): { address: ManagementAddress; problem: null } | { address: null; problem: string } {
  const fail = (problem: string) => ({ address: null, problem });
  const value = text.trim();
  if (value.includes("://")) {
    return fail(`Leave out the scheme and any path: write host:port, ${EXAMPLE}.`);
  }
  if (/^[0-9]+$/.test(value)) {
    return fail(
      `Add the host before the port, such as 127.0.0.1:${value}, or write :${value} for every interface.`,
    );
  }
  let host: string;
  let port: string;
  if (value.startsWith("[")) {
    const close = value.indexOf("]");
    if (close < 0) {
      return fail("Close the IPv6 address with ], such as [::1]:8318.");
    }
    host = value.slice(1, close);
    const after = value.slice(close + 1);
    if (!after.startsWith(":")) {
      return fail("Add the port after the ], such as [::1]:8318.");
    }
    if (ipv6Groups(host) === null) {
      return fail(
        `[${host}] isn't an IPv6 address. Write one such as [::1]:8318, or a host name or IPv4 address without brackets.`,
      );
    }
    port = after.slice(1);
  } else {
    const colon = value.lastIndexOf(":");
    if (colon < 0) {
      return fail(`Add the port, such as ${value === "" ? "127.0.0.1" : value}:8318.`);
    }
    host = value.slice(0, colon);
    port = value.slice(colon + 1);
    if (host.includes(":")) {
      return fail("Put an IPv6 address in brackets, such as [::1]:8318.");
    }
  }
  if (NOT_HOST.test(host)) {
    return fail(
      `The host can't have spaces or any of / [ ] @ ? #. Write host:port, ${EXAMPLE}, with no path.`,
    );
  }
  const number = Number(port);
  if (!/^[1-9][0-9]{0,4}$/.test(port) || number > 65_535) {
    return fail(`End it with a port from 1 to 65535, and nothing after the port, ${EXAMPLE}.`);
  }
  return { address: { host, port: number }, problem: null };
}

/**
 * What is wrong with `text` as the management address, or null: as
 * parseManagementAddress, and not the proxy's port, `proxyPort`, which the
 * server refuses on any host.
 */
export function managementAddressProblem(text: string, proxyPort: number): string | null {
  if (text.trim() === "") {
    return null;
  }
  const { address, problem } = parseManagementAddress(text);
  if (address === null) {
    return problem;
  }
  if (address.port === proxyPort) {
    return `Port ${String(proxyPort)} is the proxy's own (server.port). Pick another: the management address needs a port of its own.`;
  }
  return null;
}

/** Who can connect to `address`, as the server tells from its host. */
export function reachOf(address: ManagementAddress): ManagementReach {
  if (address.host === "") {
    return "every-interface";
  }
  const v4 = ipv4Octets(address.host);
  if (v4 !== null) {
    if (v4[0] === 127) {
      return "loopback";
    }
    return v4.every((octet) => octet === 0) ? "every-interface" : "address";
  }
  const v6 = ipv6Groups(address.host);
  if (v6 !== null) {
    if (v6.slice(0, 7).every((group) => group === 0)) {
      if (v6[7] === 1) {
        return "loopback";
      }
      if (v6[7] === 0) {
        return "every-interface";
      }
    }
    return "address";
  }
  return address.host.toLowerCase() === "localhost" ? "loopback" : "name";
}

/**
 * The dashboard's URL at `address`, as a client on the server's own
 * computer reaches it: 127.0.0.1 for every interface, an IPv6 address in
 * brackets, over HTTPS when `tls` is on.
 */
export function dashboardUrlAt(address: ManagementAddress, tls: boolean): string {
  const host =
    reachOf(address) === "every-interface"
      ? "127.0.0.1"
      : address.host.includes(":")
        ? `[${address.host}]`
        : address.host;
  return `${tls ? "https" : "http"}://${host}:${String(address.port)}/dashboard/`;
}

/**
 * The warning for `text` as the management address while
 * `management.allow-remote` is off, or null: an address other computers can
 * reach, whose management calls the server then refuses. A host name may or
 * may not be one. The server can't tell the dashboard whether
 * MANAGEMENT_PASSWORD is set, which allows them too.
 */
export function remoteWarning(text: string, allowRemote: boolean): string | null {
  const { address } = parseManagementAddress(text);
  if (allowRemote || address === null) {
    return null;
  }
  const reach = reachOf(address);
  if (reach === "loopback") {
    return null;
  }
  const lead =
    reach === "name"
      ? `If other computers can reach ${address.host}, the server will refuse them there`
      : "The server will refuse clients on other computers at this address";
  return `${lead}, as management.allow-remote is off. To manage it from them, turn that on in config.yaml, or start the server with MANAGEMENT_PASSWORD set.`;
}

/** The port of `origin`, an origin such as `http://127.0.0.1:8318`. */
function portOf(origin: string): number {
  const url = new URL(origin);
  return url.port === "" ? (url.protocol === "https:" ? 443 : 80) : Number(url.port);
}

/**
 * What the save review says of `after`, the management address a save sets:
 * that it takes a restart, and where the dashboard is then. `tls` is the
 * server's `tls.enable`, `proxyPort` its `server.port`, and `origin` this
 * page's.
 */
export function restartNotice(
  after: string,
  { tls, proxyPort, origin }: { tls: boolean; proxyPort: number; origin: string },
): string {
  const lead =
    "The server reads the management address only when it starts, so this page works as it does now until the server restarts.";
  const { address } = parseManagementAddress(after);
  if (address === null) {
    const port = proxyPort > 0 ? ` (${String(proxyPort)})` : "";
    const here = portOf(origin) === proxyPort ? "" : ", not at this page's address";
    return `${lead} Then the dashboard and the management API are back on the proxy's port${port}${here}.`;
  }
  const url = dashboardUrlAt(address, tls);
  const where =
    reachOf(address) === "every-interface"
      ? `${url}, or port ${String(address.port)} at any of the computer's addresses`
      : url;
  const here = new URL(url).origin === origin ? "" : ", not at this page's address";
  return `${lead} Then the dashboard and the management API are at ${where}${here}, and the proxy's port no longer serves them.`;
}
