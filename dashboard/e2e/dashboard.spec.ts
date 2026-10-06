// The built app, served by `vite preview` with the headers open-ferry sends,
// in a real browser: every screen must render under the shipped
// Content-Security-Policy without one violation. The API is mocked in the
// page; nothing leaves 127.0.0.1.

import { mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { expect, test, type Page } from "@playwright/test";

import { CONTENT_SECURITY_POLICY, SECURITY_HEADERS } from "../build/securityHeaders";
import { E2E_KEY, mockServer, type MockOptions, type MockServer } from "./mockServer";
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
async function watch(page: Page, options: MockOptions = {}): Promise<Watched> {
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
  const server = await mockServer(page, APP_ORIGIN, options);
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
  for (const path of ["./", "usage", "credentials", "logs/main.log", "settings", "no-such-page"]) {
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
  await expect(page.getByRole("region", { name: "Providers" })).toContainText(
    "3 sign-ins and credential files, 3 provider API keys.",
  );

  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Credentials" }).click();
  await expect(page.getByRole("heading", { name: "Credentials", level: 1 })).toBeVisible();
  await expect(page.getByRole("article", { name: "codex-grace@example.com-pro.json" })).toContainText(
    "Resting",
  );
  await expect(page.getByRole("article", { name: "claude-lin@example.com.json" })).toContainText("Failing");
  await shot(page, "13-credentials");

  await page
    .getByRole("article", { name: "claude-ada@example.com.json" })
    .getByRole("button", { name: "Check quota" })
    .click();
  const quota = page.getByRole("dialog", { name: "Quota of claude-ada@example.com.json" });
  await expect(quota).toContainText("5 hours: 62% left");
  await shot(page, "14-credentials-quota");
  await quota.getByRole("button", { name: "Close" }).click();
  await expect(quota).toBeHidden();

  // The sign-in starts, and shows the provider's page as a link, which
  // isn't followed here.
  await page.getByRole("button", { name: "Sign in with Claude", exact: true }).click();
  const signInDialog = page.getByRole("dialog", { name: "Sign in with Claude" });
  await signInDialog.getByRole("button", { name: "Start", exact: true }).click();
  await expect(signInDialog.getByRole("link", { name: /^Open Claude's sign-in page/ })).toBeFocused();
  await expect(signInDialog.getByText("Waiting for you to sign in")).toBeVisible();
  await shot(page, "15-credentials-sign-in");
  await signInDialog.getByRole("button", { name: "Give up" }).click();
  await expect(signInDialog).toBeHidden();

  await page.getByRole("button", { name: "Add an API key" }).click();
  const addKey = page.getByRole("dialog", { name: "Add a provider API key" });
  await expect(addKey.getByLabel("API key", { exact: true })).toBeFocused();
  await shot(page, "16-credentials-add-key");
  await addKey.getByRole("button", { name: "Cancel" }).click();
  await expect(addKey).toBeHidden();

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
  await page.goto("credentials");
  await expect(page.getByRole("article", { name: "claude-lin@example.com.json" })).toContainText("Failing");
  await shot(page, "17-credentials-dark");
  await page.goto("settings");
  await expect(page.getByRole("textbox", { name: "Retries" })).toHaveValue("3");
  await shot(page, "24-settings-dark");
  await page.goto("settings?tab=file");
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  await expect(page.getByRole("textbox", { name: "config.yaml" })).toContainText("remote-management:");
  await shot(page, "25-settings-config-yaml-dark");
  expectClean(watched);
});

test("changes settings, client keys and config.yaml under the policy", async ({ page }) => {
  const watched = await watch(page);
  await signIn(page);
  await page.getByRole("navigation", { name: "Main" }).getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
  const retries = page.getByRole("textbox", { name: "Retries" });
  await expect(retries).toHaveValue("3");
  await shot(page, "19-settings");

  // Two settings, reviewed against the server, then saved one at a time.
  await retries.fill("5");
  await page.getByRole("checkbox", { name: "Debug logging" }).check();
  await expect(page.getByText("2 unsaved changes.")).toBeVisible();
  await page.getByRole("button", { name: "Review and save" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes" });
  await expect(review.getByRole("row")).toHaveCount(3);
  await expect(review.getByRole("button", { name: "Save 2 settings" })).toBeFocused();
  await shot(page, "20-settings-review");
  await review.getByRole("button", { name: "Save 2 settings" }).click();
  await expect(page.getByText("Saved 2 settings. The server uses them from now on.")).toBeVisible();
  await expect(page.getByText("No unsaved changes.")).toBeVisible();

  // A client key, made here and added on its own.
  await page.getByRole("button", { name: "Add a client key" }).click();
  const addKey = page.getByRole("dialog", { name: "Add a client key" });
  await expect(addKey.getByLabel("Client key", { exact: true })).toHaveAttribute("type", "password");
  await shot(page, "21-settings-add-client-key");
  await addKey.getByRole("button", { name: "Add the key" }).click();
  await expect(page.getByText("Added the key: the proxy takes it from now on.")).toBeVisible();
  await expect(page.getByRole("region", { name: "Client API keys" }).getByRole("listitem")).toHaveCount(3);

  // config.yaml in the editor, which lives in a shadow root.
  await page.getByRole("tab", { name: "config.yaml" }).click();
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  const editor = page.getByRole("textbox", { name: "config.yaml" });
  await expect(editor).toContainText("remote-management:");
  expect(
    await page.locator(".cm-editor").evaluate((element) => element.getRootNode() instanceof ShadowRoot),
    "the editor is in a shadow root",
  ).toBe(true);
  await editor.click();
  await page.keyboard.press("Control+End");
  await page.keyboard.type("# Edited in the end-to-end test");
  await expect(page.getByText("Unsaved changes.", { exact: true })).toBeVisible();
  await shot(page, "22-settings-config-yaml");
  await page.getByRole("button", { name: "Review changes" }).click();
  const yamlReview = page.getByRole("dialog", { name: "Review the changes to config.yaml" });
  await expect(yamlReview.getByRole("insertion")).toHaveText("Added: # Edited in the end-to-end test");
  await shot(page, "23-settings-config-yaml-review");
  await yamlReview.getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();

  expect(watched.server.writes).toEqual([
    'PATCH /v0/management/request-retry {"value":5}',
    'PATCH /v0/management/debug {"value":true}',
    "PATCH /v0/management/api-keys",
    "PUT /v0/management/config.yaml",
  ]);
  expectClean(watched);
});

test("says when the server can't change settings yet", async ({ page }) => {
  const watched = await watch(page, { writable: false });
  await signIn(page);
  await page.goto("settings");
  await page.getByRole("checkbox", { name: "Debug logging" }).check();
  await page.getByRole("button", { name: "Review and save" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes" });
  await review.getByRole("button", { name: "Save 1 setting" }).click();
  await expect(review.getByText("This server can't change settings yet")).toBeVisible();
  await shot(page, "26-settings-read-only");
  await review.getByRole("button", { name: "Close" }).click();
  await expect(page.getByText("1 unsaved change.")).toBeVisible();
  expect(watched.server.writes).toEqual(["PATCH /v0/management/debug"]);
  // The browser reports the refused write itself; nothing else may fail.
  watched.consoleErrors = watched.consoleErrors.filter(
    (text) => !text.includes("the server responded with a status of 503"),
  );
  expectClean(watched);
});

test("shows a first run the ways to connect a provider", async ({ page }) => {
  const watched = await watch(page, { credentials: false });
  await signIn(page);
  const connect = page.getByRole("region", { name: "Connect a provider" });
  await expect(connect).toBeVisible();
  await shot(page, "18-overview-first-run");
  // Each opens its dialog on Credentials, which starts nothing by itself.
  await connect.getByRole("link", { name: "Sign in with Codex" }).click();
  const signInDialog = page.getByRole("dialog", { name: "Sign in with Codex" });
  await expect(signInDialog.getByRole("button", { name: "Start", exact: true })).toBeFocused();
  await signInDialog.getByRole("button", { name: "Cancel" }).click();
  await expect(signInDialog).toBeHidden();
  await expect(page.getByRole("region", { name: "Sign-ins and credential files" })).toContainText(
    "None yet.",
  );
  expectClean(watched);
});
