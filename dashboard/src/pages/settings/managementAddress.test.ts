import { describe, expect, it } from "vitest";

import {
  dashboardUrlAt,
  ipv6Groups,
  managementAddressProblem,
  parseManagementAddress,
  reachOf,
  remoteWarning,
  restartNotice,
} from "./managementAddress";

/** What the parser makes of `text`: the address, or the start of the problem. */
function parsed(text: string) {
  const { address, problem } = parseManagementAddress(text);
  return address ?? problem;
}

describe("parseManagementAddress", () => {
  it("takes host:port, an IPv6 address in brackets, and :port for every interface", () => {
    expect(parsed("127.0.0.1:8318")).toEqual({ host: "127.0.0.1", port: 8318 });
    expect(parsed("  localhost:8318 ")).toEqual({ host: "localhost", port: 8318 });
    expect(parsed("[::1]:8318")).toEqual({ host: "::1", port: 8318 });
    expect(parsed("[::ffff:192.0.2.1]:1")).toEqual({ host: "::ffff:192.0.2.1", port: 1 });
    expect(parsed(":8318")).toEqual({ host: "", port: 8318 });
    expect(parsed("mgmt.example:65535")).toEqual({ host: "mgmt.example", port: 65535 });
  });

  // Each is the server's refusal, in its order, said as what to do.
  it.each([
    ["http://127.0.0.1:8318", "Leave out the scheme and any path: write host:port, such as 127.0.0.1:8318."],
    ["8318", "Add the host before the port, such as 127.0.0.1:8318, or write :8318 for every interface."],
    ["[::1:8318", "Close the IPv6 address with ], such as [::1]:8318."],
    ["[::1]", "Add the port after the ], such as [::1]:8318."],
    ["[::1]8318", "Add the port after the ], such as [::1]:8318."],
    ["[localhost]:8318", "[localhost] isn't an IPv6 address."],
    ["[127.0.0.1]:8318", "[127.0.0.1] isn't an IPv6 address."],
    ["[fe80::1%eth0]:8318", "[fe80::1%eth0] isn't an IPv6 address."],
    ["localhost", "Add the port, such as localhost:8318."],
    ["::1:8318", "Put an IPv6 address in brackets, such as [::1]:8318."],
    ["user@host:8318", "The host can't have spaces or any of / [ ] @ ? #."],
    ["my host:8318", "The host can't have spaces"],
    ["localhost:0", "End it with a port from 1 to 65535"],
    ["localhost:65536", "End it with a port from 1 to 65535"],
    ["localhost:08318", "End it with a port from 1 to 65535"],
    ["localhost:+8318", "End it with a port from 1 to 65535"],
    ["localhost:", "End it with a port from 1 to 65535"],
    ["127.0.0.1:8318/dashboard", "End it with a port from 1 to 65535, and nothing after the port"],
  ])("refuses %j", (text, problem) => {
    expect(parsed(text)).toEqual(expect.stringContaining(problem));
  });
});

describe("managementAddressProblem", () => {
  it("takes empty, which turns the address off", () => {
    expect(managementAddressProblem("", 8317)).toBeNull();
    expect(managementAddressProblem("  ", 8317)).toBeNull();
  });

  it("refuses the proxy's port on any host, as the server does", () => {
    const problem =
      "Port 8317 is the proxy's own (server.port). Pick another: the management address needs a port of its own.";
    expect(managementAddressProblem("127.0.0.1:8317", 8317)).toBe(problem);
    expect(managementAddressProblem("[::1]:8317", 8317)).toBe(problem);
    expect(managementAddressProblem("127.0.0.1:8318", 8317)).toBeNull();
    // A config without server.port has 0, which no address has.
    expect(managementAddressProblem("127.0.0.1:8317", 0)).toBeNull();
    expect(managementAddressProblem("8317", 8317)).toMatch(/^Add the host/);
  });
});

describe("ipv6Groups", () => {
  it("reads an address as Rust does", () => {
    expect(ipv6Groups("::")).toEqual([0, 0, 0, 0, 0, 0, 0, 0]);
    expect(ipv6Groups("::1")).toEqual([0, 0, 0, 0, 0, 0, 0, 1]);
    expect(ipv6Groups("2001:DB8::8:800:200C:417A")).toEqual([
      0x2001, 0xdb8, 0, 0, 0x8, 0x800, 0x200c, 0x417a,
    ]);
    expect(ipv6Groups("1:2:3:4:5:6:7::")).toEqual([1, 2, 3, 4, 5, 6, 7, 0]);
    expect(ipv6Groups("1:2:3:4:5:6:1.2.3.4")).toEqual([1, 2, 3, 4, 5, 6, 0x102, 0x304]);
    expect(ipv6Groups("0000:0:0:0:0:0:0:1")).toEqual([0, 0, 0, 0, 0, 0, 0, 1]);
  });

  it.each([
    "",
    ":",
    ":::",
    "1::2::3",
    "1:::2",
    "1:2:3:4:5:6:7",
    "1:2:3:4:5:6:7:8:9",
    "1:2:3:4:5:6:7:8::",
    "::1:2:3:4:5:6:7:8",
    "12345::",
    "1.2.3.4",
    "1.2.3.4::",
    "::1.2.3.04",
    "::256.0.0.1",
    "g::",
  ])("refuses %j", (text) => {
    expect(ipv6Groups(text)).toBeNull();
  });
});

describe("reachOf", () => {
  const reach = (text: string) => {
    const { address } = parseManagementAddress(text);
    return address === null ? null : reachOf(address);
  };

  it("tells loopback from every interface, an address and a name", () => {
    expect(reach("127.0.0.1:8318")).toBe("loopback");
    expect(reach("127.8.9.10:8318")).toBe("loopback");
    expect(reach("[::1]:8318")).toBe("loopback");
    expect(reach("[0:0:0:0:0:0:0:1]:8318")).toBe("loopback");
    expect(reach("LocalHost:8318")).toBe("loopback");
    expect(reach(":8318")).toBe("every-interface");
    expect(reach("0.0.0.0:8318")).toBe("every-interface");
    expect(reach("[::]:8318")).toBe("every-interface");
    expect(reach("192.168.1.5:8318")).toBe("address");
    expect(reach("[::ffff:127.0.0.1]:8318")).toBe("address");
    expect(reach("mgmt.example:8318")).toBe("name");
    // Rust reads no IPv4 address with a leading zero, so this is a name.
    expect(reach("127.0.0.01:8318")).toBe("name");
  });
});

describe("dashboardUrlAt", () => {
  it("gives the URL a client on the server's computer uses", () => {
    expect(dashboardUrlAt({ host: "127.0.0.1", port: 8318 }, false)).toBe(
      "http://127.0.0.1:8318/dashboard/",
    );
    expect(dashboardUrlAt({ host: "", port: 8318 }, true)).toBe("https://127.0.0.1:8318/dashboard/");
    expect(dashboardUrlAt({ host: "::", port: 8318 }, false)).toBe("http://127.0.0.1:8318/dashboard/");
    expect(dashboardUrlAt({ host: "::1", port: 8318 }, false)).toBe("http://[::1]:8318/dashboard/");
    expect(dashboardUrlAt({ host: "mgmt.example", port: 9000 }, false)).toBe(
      "http://mgmt.example:9000/dashboard/",
    );
  });
});

describe("remoteWarning", () => {
  it("warns of an address other computers reach while allow-remote is off", () => {
    expect(remoteWarning(":8318", false)).toBe(
      "The server will refuse clients on other computers at this address, as management.allow-remote is off. To manage it from them, turn that on in config.yaml, or start the server with MANAGEMENT_PASSWORD set.",
    );
    expect(remoteWarning("192.168.1.5:8318", false)).toMatch(/^The server will refuse clients/);
    expect(remoteWarning("mgmt.example:8318", false)).toMatch(
      /^If other computers can reach mgmt\.example, the server will refuse them there, as/,
    );
  });

  it("says nothing for loopback, with allow-remote on, or for a value it can't read", () => {
    expect(remoteWarning("127.0.0.1:8318", false)).toBeNull();
    expect(remoteWarning("localhost:8318", false)).toBeNull();
    expect(remoteWarning("[::1]:8318", false)).toBeNull();
    expect(remoteWarning(":8318", true)).toBeNull();
    expect(remoteWarning("", false)).toBeNull();
    expect(remoteWarning("8318", false)).toBeNull();
  });
});

describe("restartNotice", () => {
  const LEAD =
    "The server reads the management address only when it starts, so this page works as it does now until the server restarts.";
  const here = { tls: false, proxyPort: 8317, origin: "http://127.0.0.1:8317" };

  it("says where the dashboard is after the restart", () => {
    expect(restartNotice("127.0.0.1:8318", here)).toBe(
      `${LEAD} Then the dashboard and the management API are at http://127.0.0.1:8318/dashboard/, not at this page's address, and the proxy's port no longer serves them.`,
    );
    expect(restartNotice(":8318", { ...here, tls: true })).toContain(
      "are at https://127.0.0.1:8318/dashboard/, or port 8318 at any of the computer's addresses, not at",
    );
    // Already there, as when the page is at the address the server was given.
    expect(restartNotice(":8318", { ...here, origin: "http://127.0.0.1:8318" })).toContain(
      "any of the computer's addresses, and the proxy's port",
    );
  });

  it("says the dashboard goes back to the proxy's port when it is turned off", () => {
    expect(restartNotice("", { ...here, origin: "http://127.0.0.1:8318" })).toBe(
      `${LEAD} Then the dashboard and the management API are back on the proxy's port (8317), not at this page's address.`,
    );
    expect(restartNotice("", here)).toMatch(/back on the proxy's port \(8317\)\.$/);
    expect(restartNotice("", { ...here, proxyPort: 0 })).toMatch(
      /back on the proxy's port, not at this page's address\.$/,
    );
  });
});
