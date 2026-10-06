import { describe, expect, it, vi } from "vitest";

import { forgetStoredKey, readStoredKey, storeKey } from "./keyStorage";

describe("keyStorage", () => {
  it("keeps the key in this tab's session storage only", () => {
    expect(readStoredKey()).toBeNull();
    storeKey("k-1");
    expect(readStoredKey()).toBe("k-1");
    expect(window.sessionStorage.getItem("open-ferry.management-key")).toBe("k-1");
    forgetStoredKey();
    expect(readStoredKey()).toBeNull();
  });

  it("works on when storage is off", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("denied", "SecurityError");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("denied", "SecurityError");
    });
    expect(() => {
      storeKey("k");
    }).not.toThrow();
    expect(readStoredKey()).toBeNull();
  });
});
