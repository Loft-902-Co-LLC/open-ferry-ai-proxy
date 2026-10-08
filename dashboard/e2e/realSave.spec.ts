// A real save: the dashboard a debug build of open-ferry embeds, changing
// config.yaml through the binary's own management API rather than a mock.
// CI's dashboard job builds no binary, so this runs only when OPEN_FERRY_BIN
// names one, as a manual pass:
//
//   npm run build                                  (in dashboard/)
//   cargo build -p open-ferry --bin open-ferry     (embeds dashboard/dist)
//   OPEN_FERRY_BIN=../target/debug/open-ferry.exe npm run e2e:real
//
// The spec starts the binary itself and stops it afterwards. It listens on
// 127.0.0.1 only, on OPEN_FERRY_E2E_PORT (18517 unless set, never 8317),
// over a config in a fresh temp directory with dummy keys, and sends every
// outbound request to a dead proxy at 127.0.0.1:9. The page may reach
// nothing but the binary. The management key is random and never printed,
// and neither is config.yaml, which holds it: checks on the file report a
// description, not its text.

import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { expect, test, type Page } from "@playwright/test";

const BIN = process.env.OPEN_FERRY_BIN ?? "";
const PORT = Number(process.env.OPEN_FERRY_E2E_PORT ?? "18517");
const ORIGIN = `http://127.0.0.1:${String(PORT)}`;
const APP = `${ORIGIN}/dashboard/`;
const DEAD = "http://127.0.0.1:9";
const NL = "\n";

const CLAUDE_KEY = "sk-ant-dummy-0000000000000000";
const GEMINI_KEY = "AIza-dummy-gemini-000000000000";

test.skip(BIN === "", "set OPEN_FERRY_BIN to a debug build of open-ferry to run the real-save pass");
test.describe.configure({ mode: "serial" });

let dir = "";
let configPath = "";
let managementKey = "";
let server: ChildProcess | null = null;
let exited: number | null = null;
let page: Page | null = null;
const violations: string[] = [];
const pageErrors: string[] = [];
const offOrigin: string[] = [];

function configText(): string {
  return [
    "# The dashboard's real-save pass: dummy keys only.",
    "host: 127.0.0.1",
    `port: ${String(PORT)} # never 8317`,
    `auth-dir: ${join(dir, "auths").replaceAll("\\", "/")}`,
    "",
    "# Every outbound request goes to a dead proxy.",
    `proxy-url: ${DEAD}`,
    "debug: false",
    "logging-to-file: false",
    "usage-statistics-enabled: false",
    "request-log: false",
    "request-retry: 1 # retries comment",
    "some-future-setting: keep-me # a key open-ferry doesn't type",
    "",
    "# The client keys: CLIProxyAPI's examples, so the proxy starts in safe mode.",
    "api-keys:",
    "  - your-api-key-1",
    "  - your-api-key-2",
    "  - your-api-key-3",
    "",
    "remote-management:",
    `  secret-key: ${managementKey}`,
    "  allow-remote: false",
    "",
    "# A provider key, removed from the Credentials page.",
    "claude-api-key:",
    `  - api-key: ${CLAUDE_KEY}`,
    `    base-url: ${DEAD}`,
    "",
  ].join(NL);
}

function the(): Page {
  if (page === null) {
    throw new Error("the page isn't open");
  }
  return page;
}

function file(): string {
  return readFileSync(configPath, "utf8");
}

/** Checks config.yaml for `pattern`, naming the check rather than printing the file. */
function expectFile(pattern: string | RegExp, description: string, present = true) {
  const text = file();
  const found = typeof pattern === "string" ? text.includes(pattern) : pattern.test(text);
  expect(found, `config.yaml ${present ? "has" : "lacks"} ${description}`).toBe(present);
}

function expectCommentsKept() {
  expectFile("# The dashboard's real-save pass: dummy keys only.", "its first comment");
  expectFile("# Every outbound request goes to a dead proxy.", "the proxy comment");
  expectFile("# The client keys:", "the client keys comment");
  expectFile(/^some-future-setting: keep-me/m, "the setting open-ferry doesn't type");
}

async function call(path: string, method = "GET", body?: unknown): Promise<Response> {
  return fetch(`${ORIGIN}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${managementKey}`,
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/** A management call from the test itself, for state the screens don't set. */
async function management<T>(method: string, route: string, body?: unknown): Promise<T> {
  const response = await call(`/v0/management/${route}`, method, body);
  expect(response.status, `${method} ${route}`).toBe(200);
  return (await response.json()) as T;
}

async function clientKeys(): Promise<string[]> {
  return (await management<{ "api-keys"?: string[] }>("GET", "api-keys"))["api-keys"] ?? [];
}

async function waitForServer() {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    if (exited !== null) {
      throw new Error(`open-ferry exited with ${String(exited)}; its log is in ${dir}`);
    }
    try {
      const response = await fetch(APP);
      if (response.status === 200) {
        return;
      }
    } catch {
      // Not listening yet.
    }
    await new Promise((done) => setTimeout(done, 250));
  }
  throw new Error(`open-ferry didn't serve ${APP} within a minute; its log is in ${dir}`);
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(120_000);
  expect(PORT, "the port").not.toBe(8317);
  dir = mkdtempSync(join(tmpdir(), "ofp-real-save-"));
  mkdirSync(join(dir, "auths"));
  configPath = join(dir, "config.yaml");
  managementKey = `mgmt-dummy-${randomBytes(18).toString("hex")}`;
  writeFileSync(configPath, configText());

  const log = openSync(join(dir, "server.log"), "w");
  const child = spawn(resolve(BIN), ["-config", configPath, "-no-browser"], {
    cwd: dir,
    env: {
      ...process.env,
      HTTP_PROXY: DEAD,
      HTTPS_PROXY: DEAD,
      ALL_PROXY: DEAD,
      http_proxy: DEAD,
      https_proxy: DEAD,
      all_proxy: DEAD,
      NO_PROXY: "",
      no_proxy: "",
    },
    stdio: ["ignore", log, log],
    windowsHide: true,
  });
  closeSync(log);
  child.on("exit", (code) => {
    exited = code ?? -1;
  });
  server = child;
  await waitForServer();

  const opened = await browser.newPage();
  page = opened;
  await opened.exposeFunction("reportCspViolation", (text: string) => {
    violations.push(text);
  });
  await opened.addInitScript(() => {
    const report = (window as unknown as { reportCspViolation: (text: string) => Promise<void> })
      .reportCspViolation;
    document.addEventListener("securitypolicyviolation", (event) => {
      void report(`${event.effectiveDirective} refused ${event.blockedURI || "inline"}`);
    });
  });
  opened.on("pageerror", (error) => {
    pageErrors.push(error.message);
  });
  await opened.route("**/*", (route) => {
    const url = route.request().url();
    if (new URL(url).origin === ORIGIN) {
      return route.continue();
    }
    offOrigin.push(url);
    return route.abort();
  });

  await opened.goto(`${APP}signin`);
  await opened.getByLabel("Management key").fill(managementKey);
  await opened.getByRole("button", { name: "Sign in" }).click();
  await expect(opened.getByRole("heading", { name: "Overview", level: 1 })).toBeVisible();
});

test.afterAll(async () => {
  await page?.close();
  // Only the open-ferry this spec started.
  const child = server;
  if (child !== null && exited === null) {
    const stopped = new Promise((done) => child.once("exit", done));
    child.kill();
    await stopped;
  }
  if (dir !== "" && test.info().errors.length === 0) {
    rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
});

test("lifts safe mode from the Overview, then makes a key", async () => {
  const page = the();
  await page.goto(APP);
  await expect(page.getByText("The proxy is in safe mode")).toBeVisible();
  await page.getByRole("button", { name: "Replace the example keys with a new key" }).click();
  await expect(page.getByText("The proxy is out of safe mode")).toBeVisible();
  await expect(page.getByText("The proxy is in safe mode")).toHaveCount(0);
  const replaced = await clientKeys();
  expect(replaced).toHaveLength(1);
  expectFile("your-api-key", "no example keys", false);
  expectFile(replaced[0] ?? "-", "the new key");
  expectCommentsKept();

  await page.getByRole("button", { name: "Make a new key" }).click();
  await expect(page.getByText(/^Added a client key\./)).toBeVisible();
  const keys = await clientKeys();
  expect(keys).toHaveLength(2);
  expectFile(keys[1] ?? "-", "the key made on the Overview");
  await expect(page.getByRole("combobox", { name: "Client key" }).getByRole("option")).toHaveCount(2);
});

test("turns settings on where the Usage and Logs pages offer to", async () => {
  const page = the();
  await page.goto(`${APP}usage`);
  await expect(page.getByText("Usage isn't being recorded")).toBeVisible();
  await page.getByRole("button", { name: "Start recording" }).click();
  await expect(page.getByText("Usage isn't being recorded")).toHaveCount(0);
  expectFile(/^usage-statistics-enabled: true$/m, "usage-statistics-enabled: true");

  // The recent calls show once the ledger has a row: one call, which fails
  // at the dead proxy.
  const setup = (await (await call("/open-ferry/api/v1/client-setup")).json()) as {
    models?: { id: string }[];
  };
  const [clientKey] = await clientKeys();
  await fetch(`${ORIGIN}/v1/chat/completions`, {
    method: "POST",
    headers: { Authorization: `Bearer ${clientKey ?? ""}`, "Content-Type": "application/json" },
    body: JSON.stringify({
      model: setup.models?.[0]?.id ?? "claude-sonnet-4-5",
      messages: [{ role: "user", content: "hello" }],
    }),
  });
  await page.reload();
  const off = page.getByText("request-log is off");
  await expect(off).toBeVisible();
  await page.getByRole("button", { name: "Log every request" }).click();
  await expect(off).toHaveCount(0);
  expectFile(/^request-log: true$/m, "request-log: true, from the Usage page");

  await management("PATCH", "request-log", { value: false });
  expectFile(/^request-log: false$/m, "request-log: false again");
  await page.goto(`${APP}logs`);
  await expect(page.getByText("Only failed requests are logged")).toBeVisible();
  await page.getByRole("button", { name: "Log every request" }).click();
  await expect(page.getByText("Only failed requests are logged")).toHaveCount(0);
  expectFile(/^request-log: true$/m, "request-log: true, from the Logs page");

  await page.goto(`${APP}logs?tab=server`);
  await expect(page.getByText("The server doesn't write its log to a file")).toBeVisible();
  await page.getByRole("button", { name: "Log to a file" }).click();
  await expect(page.getByText("The server doesn't write its log to a file")).toHaveCount(0);
  expectFile(/^logging-to-file: true$/m, "logging-to-file: true");
  expectCommentsKept();
});

test("saves every setting through its own route", async () => {
  const page = the();
  await page.goto(`${APP}settings`);
  await expect(page.getByRole("heading", { name: "Settings", level: 1 })).toBeVisible();
  const form = {
    proxy: page.getByRole("textbox", { name: "Proxy for outbound requests" }),
    strategy: page.getByRole("combobox", { name: "How credentials are picked" }),
    retries: page.getByRole("textbox", { name: "Retries" }),
    perRound: page.getByRole("textbox", { name: "Credentials per round" }),
    wait: page.getByRole("textbox", { name: "Longest wait for a retry (seconds)" }),
    prefix: page.getByRole("checkbox", { name: "Prefixed credentials need the prefix" }),
    requestLog: page.getByRole("checkbox", { name: "Request logs" }),
    toFiles: page.getByRole("checkbox", { name: "Log to files" }),
    debug: page.getByRole("checkbox", { name: "Debug logging" }),
    logLimit: page.getByRole("textbox", { name: "Log directory limit (MB)" }),
    failedKept: page.getByRole("textbox", { name: "Failed-request logs kept" }),
    usage: page.getByRole("checkbox", { name: "Usage statistics" }),
  };
  await expect(form.retries).toHaveValue("1");
  await expect(form.requestLog).toBeChecked();

  await form.proxy.fill("https://127.0.0.1:9");
  await form.strategy.selectOption("fill-first");
  await form.retries.fill("4");
  await form.perRound.fill("2");
  await form.wait.fill("7");
  await form.prefix.check();
  await form.requestLog.uncheck();
  await form.toFiles.uncheck();
  await form.debug.check();
  await form.logLimit.fill("64");
  await form.failedKept.fill("5");
  await form.usage.uncheck();
  await expect(page.getByText("12 unsaved changes.")).toBeVisible();
  await page.getByRole("button", { name: "Review and save" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes" });
  await review.getByRole("button", { name: "Save 12 settings" }).click();
  await expect(page.getByText("Saved 12 settings. The server uses them from now on.")).toBeVisible();
  await expect(page.getByText("No unsaved changes.")).toBeVisible();

  const saved: [RegExp, string][] = [
    [/^proxy-url: "?https:\/\/127\.0\.0\.1:9"?$/m, "the new proxy"],
    [/^routing:\n(?: {2}.*\n)*? {2}strategy: "?fill-first"?$/m, "routing.strategy: fill-first"],
    [/^request-retry: 4\b/m, "request-retry: 4"],
    [/^max-retry-credentials: 2$/m, "max-retry-credentials: 2"],
    [/^max-retry-interval: 7$/m, "max-retry-interval: 7"],
    [/^force-model-prefix: true$/m, "force-model-prefix: true"],
    [/^request-log: false$/m, "request-log: false"],
    [/^logging-to-file: false$/m, "logging-to-file: false"],
    [/^debug: true$/m, "debug: true"],
    [/^logs-max-total-size-mb: 64$/m, "logs-max-total-size-mb: 64"],
    [/^error-logs-max-files: 5$/m, "error-logs-max-files: 5"],
    [/^usage-statistics-enabled: false$/m, "usage-statistics-enabled: false"],
  ];
  for (const [pattern, description] of saved) {
    expectFile(pattern, description);
  }
  expectCommentsKept();

  // The screen shows what the server now has.
  await page.reload();
  await expect(form.proxy).toHaveValue("https://127.0.0.1:9");
  await expect(form.strategy).toHaveValue("fill-first");
  await expect(form.retries).toHaveValue("4");
  await expect(form.perRound).toHaveValue("2");
  await expect(form.wait).toHaveValue("7");
  await expect(form.prefix).toBeChecked();
  await expect(form.requestLog).not.toBeChecked();
  await expect(form.toFiles).not.toBeChecked();
  await expect(form.debug).toBeChecked();
  await expect(form.logLimit).toHaveValue("64");
  await expect(form.failedKept).toHaveValue("5");
  await expect(form.usage).not.toBeChecked();
});

test("adds and deletes a client key on the Settings page", async () => {
  const page = the();
  await page.goto(`${APP}settings`);
  const card = page.getByRole("region", { name: "Client API keys" });
  await expect(card.getByRole("listitem")).toHaveCount(2);
  await page.getByRole("button", { name: "Add a client key" }).click();
  const add = page.getByRole("dialog", { name: "Add a client key" });
  await add.getByRole("button", { name: "Add to the list" }).click();
  // The new key waits for the review, like a setting's edit.
  await expect(card.getByText("Not saved yet")).toBeVisible();
  expect(await clientKeys()).toHaveLength(2);
  await page.getByRole("button", { name: "Review and save" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes" });
  await review.getByRole("button", { name: "Save 1 change" }).click();
  await expect(page.getByText("Saved 1 change. The server uses it from now on.")).toBeVisible();
  await expect(card.getByText("Not saved yet")).toHaveCount(0);
  await expect(card.getByRole("listitem")).toHaveCount(3);
  const keys = await clientKeys();
  expect(keys).toHaveLength(3);
  const added = keys[2] ?? "-";
  expectFile(added, "the key added on the Settings page");

  await card.getByRole("button", { name: /^Delete / }).last().click();
  await expect(card.getByText("Will be deleted")).toBeVisible();
  expect(await clientKeys()).toHaveLength(3);
  await page.getByRole("button", { name: "Review and save" }).click();
  await review.getByRole("button", { name: "Save 1 change" }).click();
  await expect(card.getByRole("listitem")).toHaveCount(2);
  expect(await clientKeys()).toHaveLength(2);
  expectFile(added, "the deleted key", false);
  expectCommentsKept();
});

/**
 * Waits for the editor to show `text`. The editor draws only the lines in
 * view, and its text holds the management key, so this reports whether the
 * text showed, never the editor's text.
 */
async function expectEditorShows(text: string, description: string) {
  const editor = the().getByRole("textbox", { name: "config.yaml" });
  await expect
    .poll(async () => (await editor.textContent())?.includes(text) ?? false, {
      message: `the editor shows ${description}`,
    })
    .toBe(true);
}

test("saves config.yaml from its editor after showing the diff", async () => {
  const page = the();
  await page.goto(`${APP}settings`);
  await page.getByRole("tab", { name: "config.yaml" }).click();
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  const editor = page.getByRole("textbox", { name: "config.yaml" });
  await expectEditorShows("# The dashboard's real-save pass", "the file's first comment");
  await editor.click();
  await page.keyboard.press("Control+End");
  await page.keyboard.type("# Saved from the dashboard's editor");
  await page.getByRole("button", { name: "Review changes" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes to config.yaml" });
  await expect(review.getByRole("insertion")).toHaveText("Added: # Saved from the dashboard's editor");
  await review.getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  expectFile("# Saved from the dashboard's editor", "the comment added in the editor");
  expectFile(`secret-key: ${managementKey}`, "the management key as it was");
  expectCommentsKept();

  // The screen shows the file as the server now has it, at its end.
  await page.reload();
  await page.getByRole("tab", { name: "config.yaml" }).click();
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  await expectEditorShows("# The dashboard's real-save pass", "the file's first comment");
  await editor.click();
  await page.keyboard.press("Control+End");
  await expectEditorShows("# Saved from the dashboard's editor", "the comment saved from it");
});

test("adds and removes provider keys on the Credentials page", async () => {
  const page = the();
  await page.goto(`${APP}credentials?start=key`);
  const add = page.getByRole("dialog", { name: "Add a provider API key" });
  await add.getByLabel("Provider").selectOption("gemini");
  await add.getByLabel("API key", { exact: true }).fill(GEMINI_KEY);
  await add.getByLabel("Base URL (optional)").fill(DEAD);
  await add.getByRole("button", { name: "Add the key" }).click();
  await expect(page.getByText("Added the Gemini key: the server uses it from now on.")).toBeVisible();
  expectFile(/^gemini-api-key:$/m, "a gemini-api-key list");
  expectFile(GEMINI_KEY, "the Gemini key");

  await page.getByRole("button", { name: /^Remove the Claude key / }).click();
  const confirm = page.getByRole("dialog", { name: "Remove this Claude key?" });
  await confirm.getByRole("button", { name: "Remove" }).click();
  await expect(page.getByRole("button", { name: /^Remove the Claude key / })).toHaveCount(0);
  expectFile(CLAUDE_KEY, "the removed Claude key", false);
  expectFile(GEMINI_KEY, "the Gemini key still");
  expectCommentsKept();
});

test("ran under the binary's policy, on its origin only", () => {
  expect(violations, "Content-Security-Policy violations").toEqual([]);
  expect(pageErrors, "uncaught exceptions").toEqual([]);
  expect(offOrigin, "requests that left the binary's origin").toEqual([]);
});
