import { describe, expect, it } from "vitest";

import { generateClientKey, isExampleKey, maskKey, usableKeys } from "./clientKeys";

describe("client keys", () => {
  it("knows CLIProxyAPI's example keys, as the server trims them", () => {
    expect(isExampleKey("your-api-key-1")).toBe(true);
    expect(isExampleKey(" your-api-key-3 ")).toBe(true);
    expect(isExampleKey("your-api-key-4")).toBe(false);
  });

  it("keeps the keys a client could use, each once", () => {
    expect(
      usableKeys(["your-api-key-1", "sk-one", "", "  ", "sk-two", "sk-one", "your-api-key-2 "]),
    ).toEqual(["sk-one", "sk-two"]);
  });

  it("masks a key as the server does, and a short one wholly", () => {
    expect(maskKey("sk-abcdefghijk9f3k")).toBe("sk-...9f3k");
    expect(maskKey("short")).toBe("•••••");
    expect(maskKey("ab")).toBe("••••");
  });

  it("makes long random keys", () => {
    const first = generateClientKey();
    const second = generateClientKey();
    expect(first).toMatch(/^sk-[A-Za-z0-9_-]{43}$/);
    expect(second).not.toBe(first);
  });
});
