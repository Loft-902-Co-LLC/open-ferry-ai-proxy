// The end-to-end tests: the built app under `vite preview`, which sends the
// headers open-ferry does, driven in Chromium. `npm run e2e` builds first;
// CI runs `playwright test` straight after its own build step.

import { defineConfig, devices } from "@playwright/test";

import { APP_URL } from "./e2e/origin";

export default defineConfig({
  testDir: "e2e",
  // The real-save pass runs on its own: playwright.real.config.ts.
  testIgnore: "realSave.spec.ts",
  forbidOnly: process.env.CI !== undefined,
  reporter: process.env.CI === undefined ? "list" : [["github"], ["list"]],
  use: {
    baseURL: APP_URL,
    // The point of the tests: the page runs under its real policy.
    bypassCSP: false,
    serviceWorkers: "block",
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "npm run preview",
    url: APP_URL,
    reuseExistingServer: false,
    // The preview proxies the API to OPEN_FERRY_URL and won't start without
    // one. The tests answer every API call in the page, so nothing is ever
    // sent there; port 9 (discard) has nothing listening.
    env: { OPEN_FERRY_URL: "http://127.0.0.1:9" },
    stdout: "ignore",
  },
});
