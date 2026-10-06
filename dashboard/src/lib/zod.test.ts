import { describe, expect, it, vi } from "vitest";

import { z } from "./zod";

describe("zod", () => {
  it("runs without eval, as the Content-Security-Policy requires", () => {
    expect(z.config().jitless).toBe(true);
    const spy = vi.spyOn(globalThis, "Function");
    const schema = z.object({ a: z.string(), b: z.number().int() });
    expect(schema.parse({ a: "x", b: 1 })).toEqual({ a: "x", b: 1 });
    expect(schema.safeParse({ a: 1 }).success).toBe(false);
    expect(spy).not.toHaveBeenCalled();
  });
});
