import "@testing-library/jest-dom/vitest";
import "../lib/zod";

import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(() => {
  // Tests of the build scripts run in Node, without a DOM.
  if (typeof window !== "undefined") {
    cleanup();
    window.sessionStorage.clear();
  }
});
