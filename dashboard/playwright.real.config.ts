// The real-save pass: e2e/realSave.spec.ts drives the dashboard a debug
// build of open-ferry embeds, and starts that binary itself, so there is no
// preview server here. `npm run e2e:real` runs it; the spec says how.

import { defineConfig, devices } from "@playwright/test";

// Playwright failure prompts include a DOM snapshot, which can hold keys.
process.env.PLAYWRIGHT_NO_COPY_PROMPT = "1";

export default defineConfig({
  testDir: "e2e",
  testMatch: "realSave.spec.ts",
  forbidOnly: process.env.CI !== undefined,
  workers: 1,
  reporter: "list",
  timeout: 60_000,
  // Twelve serialized config writes can take longer on a busy debug build.
  expect: { timeout: 30_000 },
  use: {
    // The point of the pass: the page runs under the binary's own policy.
    bypassCSP: false,
    serviceWorkers: "block",
    // A trace or a screenshot could hold the management key.
    trace: "off",
    screenshot: "off",
    video: "off",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
