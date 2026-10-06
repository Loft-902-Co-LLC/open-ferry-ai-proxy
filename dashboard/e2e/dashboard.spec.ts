// The built app, served by `vite preview` with the headers open-ferry sends,
// in a real browser: every screen must render under the shipped
// Content-Security-Policy without one violation. The API is mocked in the
// page; nothing leaves 127.0.0.1.

import { mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { expect, test, type Page } from "@playwright/test";

import { CONTENT_SECURITY_POLICY, SECURITY_HEADERS } from "../build/securityHeaders";
import { E2E_KEY, mockServer, type MockServer } from "./mockServer";
import { APP_ORIGIN } from "./origin";

/** Where to save a screenshot of each screen, when set. */
const SHOTS_DIR = process.env.SHOTS_DIR ?? "";

interface Watched {
  /** Content-Security-Policy violations the page reported. */
  violations: string[];
  /** Errors logged to the console, which is where Chromium also reports a refusal. */
  consoleErrors: string[];
  /** Uncaught exceptions. */
  pageErrors: string[];
  server: MockServer;
}

/** Mocks the API on `page` and records everything that goes wrong on it. */
async function watch(page: Page): Promise<Watched> {
  const watched: Omit<Watched, "server"> = { violations: [], consoleErrors: [], pageErrors: [] };
  // Bindings and init scripts come from the browser's debugging protocol,
  // so the page's policy doesn't stop them.
  await page.exposeFunction("reportCspViolation", (text: string) => {
    watched.violations.push(text);
  });
  await page.addInitScript(() => {
    const report = (window as unknown as { reportCspViolation: (text: string) => Promise<void> })
      .reportCspViolation;
    document.addEventListener("securitypolicyviolation", (event) => {
      const blocked = event.blockedURI === "" ? "inline" : event.blockedURI;
      void report(`${event.effectiveDirective} refused ${blocked}: ${event.sample}`);
    });
  });
  page.on("console", (message) => {
    if (message.type() === "error") {
      watched.consoleErrors.push(message.text());
    }
  });
  page.on("pageerror", (error) => {
    watched.pageErrors.push(error.message);
  });
  const server = await mockServer(page, APP_ORIGIN);
  return { ...watched, server };
}

function expectClean(watched: Watched) {
  expect(watched.violations, "Content-Security-Policy violations").toEqual([]);
  expect(watched.consoleErrors, "console errors").toEqual([]);
  expect(watched.pageErrors, "uncaught exceptions").toEqual([]);
  expect(watched.server.unhandled, "API calls the mock doesn't answer").toEqual([]);
  expect(watched.server.offOrigin, "requests that left the app's origin").toEqual([]);
}

async function shot(page: Page, name: string) {
  if (SHOTS_DIR === "") {
    return;
  }
  mkdirSync(SHOTS_DIR, { recursive: true });
  // Let transitions and chart animations settle first.
  await page.waitForTimeout(600);
  await page.screenshot({ path: join(SHOTS_DIR, `${name}.png`), fullPage: true });
}

async function signIn(page: Page) {
  await page.goto("signin");
  await page.getByLabel("Management key").fill(E2E_KEY);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("heading", { name: "Overview", level: 1 })).toBeVisible();
}

test("serves every page with the policy open-ferry sends", async ({ page }) => {
  await mockServer(page, APP_ORIGIN);
  for (const path of ["./", "usage", "logs/main.log", "no-such-page"]) {
    const response = await page.goto(path);
    expect(response?.status(), path).toBe(200);
    const headers = response?.headers() ?? {};
    expect(headers["content-security-policy"], path).toBe(CONTENT_SECURITY_POLICY);
    for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
      expect(headers[name.toLowerCase()], `${path}: ${name}`).toBe(value);
    }
  }
});

test("reports a violation, so a clean run means something", async ({ page }) => {
  const watched = await watch(page);
  await page.goto("signin");
  await expect(page.getByLabel("Management key")).toBeVisible();
  // What a library that injects styles does.
  await page.evaluate(() => {
    const style = document.createElement("style");
    style.textContent = "body { outline: 1px solid red; }";
    document.head.append(style);
    document.body.setAttribute("style", "outline: 1px solid red");
  });
  await expect.poll(() => watched.violations.length).toBe(2);
  expect(watched.violations.join("\n")).toContain("style-src-elem refused inline");
  expect(watched.violations.join("\n")).toContain("style-src-attr refused inline");
});

test("renders every screen under the policy", async ({ page }) => {
  const watched = await watch(page);

  await page.goto("./");
  await expect(page.getByLabel("Management key")).toBeVisible();
  await shot(page, "01-sign-in");
  await signIn(page);

  await expect(page.getByLabel("The OpenAI SDK (Python) setup", { exact: true })).toContainText(
    `base_url="${APP_ORIGIN}/v1"`,
  );
  await shot(page, "02-overview-client-setup");
  await page.getByRole("tab", { name: "Codex CLI" }).click();
  await expect(page.getByLabel("The Codex CLI setup, step 1", { exact: true })).toContainText(`wire_api = "responses"`);
  await shot(page, "03-overview-codex");

  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Usage" }).click();
  await expect(page.getByRole("heading", { name: "Usage", level: 1 })).toBeVisible();
  await expect(page.locator("svg.recharts-surface").first()).toBeVisible();
  await shot(page, "04-usage");

  await page.goto("usage/ledger");
  await expect(page.getByRole("heading", { name: "Ledger and prices", level: 1 })).toBeVisible();
  await shot(page, "05-usage-ledger");
  await page.getByRole("button", { name: "Delete all recorded calls" }).click();
  const confirm = page.getByRole("dialog", { name: "Delete every recorded call?" });
  await expect(confirm).toBeVisible();
  await shot(page, "06-usage-ledger-delete");
  await confirm.getByRole("button", { name: "Cancel" }).click();
  await expect(confirm).toBeHidden();

  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Logs" }).click();
  const table = page.getByRole("table", { name: "Request logs, newest first" });
  await expect(table).toBeVisible();
  await shot(page, "07-request-logs");

  await page.getByRole("tab", { name: "Server log" }).click();
  await expect(page.getByRole("log", { name: "Server log lines" })).toContainText("listening on");
  await shot(page, "08-server-log");

  await page.getByRole("tab", { name: "Request logs" }).click();
  await table.getByRole("link").first().click();
  await expect(page.getByLabel("The log's content", { exact: true })).toContainText("=== REQUEST INFO ===");
  await shot(page, "09-log-viewer");

  // The download is the log's exact bytes, saved under its listed name
  // through an object URL, which the policy must allow.
  const logName = decodeURIComponent(new URL(page.url()).pathname.split("/").at(-1) ?? "");
  const downloading = page.waitForEvent("download");
  await page.getByRole("button", { name: "Download" }).click();
  const download = await downloading;
  expect(download.suggestedFilename()).toBe(logName);
  const saved = readFileSync(await download.path());
  const expected = watched.server.logBytes(logName);
  expect(expected).toBeDefined();
  expect(saved.equals(expected ?? Buffer.alloc(0)), "the saved file is the log's bytes").toBe(true);

  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "About" }).click();
  await expect(page.getByRole("heading", { name: "About", level: 1 })).toBeVisible();
  await expect(page.getByText("0.1.0-e2e")).toBeVisible();
  await shot(page, "10-about");

  await page.goto("no-such-page");
  await expect(page.getByRole("heading", { name: "Page not found", level: 1 })).toBeVisible();

  expectClean(watched);
});

test("renders the screens in dark mode under the policy too", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "dark" });
  const watched = await watch(page);
  await signIn(page);
  await expect(page.getByLabel("The OpenAI SDK (Python) setup", { exact: true })).toBeVisible();
  await shot(page, "11-overview-dark");
  await page.goto("usage");
  await expect(page.locator("svg.recharts-surface").first()).toBeVisible();
  await shot(page, "12-usage-dark");
  expectClean(watched);
});
