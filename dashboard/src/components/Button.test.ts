import { describe, expect, it } from "vitest";

import { buttonClasses, type ButtonSize } from "./Button";

describe("a button's size", () => {
  it("grows to 44 px tall for a finger, at either size", () => {
    for (const size of ["sm", "md"] satisfies ButtonSize[]) {
      expect(buttonClasses("secondary", size).split(" ")).toContain("pointer-coarse:min-h-11");
    }
  });
});
