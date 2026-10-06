// @vitest-environment node
import { describe, expect, it } from "vitest";

import { REFUSED_PORT, devProxyTarget } from "./devTarget";

describe("devProxyTarget", () => {
  it("takes a loopback origin on another port", () => {
    expect(devProxyTarget("http://127.0.0.1:18317")).toBe("http://127.0.0.1:18317");
    expect(devProxyTarget(" http://localhost:9000/ ")).toBe("http://localhost:9000");
    expect(devProxyTarget("https://[::1]:8443")).toBe("https://[::1]:8443");
  });

  it("refuses to run without OPEN_FERRY_URL", () => {
    expect(() => devProxyTarget(undefined)).toThrow(/OPEN_FERRY_URL isn't set/);
    expect(() => devProxyTarget("   ")).toThrow(/OPEN_FERRY_URL isn't set/);
  });

  it("refuses port 8317 by any name", () => {
    expect(REFUSED_PORT).toBe(8317);
    for (const url of [
      "http://127.0.0.1:8317",
      "http://localhost:8317/",
      "http://[::1]:8317",
      "https://example.test:8317",
      "http://0.0.0.0:8317",
      "http://127.1:8317",
      "http://2130706433:8317",
    ]) {
      expect(() => devProxyTarget(url), url).toThrow(/8317/);
    }
  });

  it("refuses what isn't a plain http(s) origin", () => {
    expect(() => devProxyTarget("127.0.0.1:18317")).toThrow();
    expect(() => devProxyTarget("not a url")).toThrow(/isn't a URL/);
    expect(() => devProxyTarget("ftp://127.0.0.1:21")).toThrow(/http/);
    expect(() => devProxyTarget("http://user:pw@127.0.0.1:18317")).toThrow(/user name/);
    expect(() => devProxyTarget("http://127.0.0.1:18317/v0")).toThrow(/origin only/);
    expect(() => devProxyTarget("http://127.0.0.1:18317/?a=1")).toThrow(/origin only/);
    expect(() => devProxyTarget("http://127.0.0.1:18317/#x")).toThrow(/origin only/);
  });
});
