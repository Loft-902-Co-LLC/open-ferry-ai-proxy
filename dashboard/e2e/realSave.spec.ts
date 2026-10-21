// A real save: the dashboard a debug build of open-ferry embeds, changing
// config.yaml through the binary's own management API rather than a mock.
// CI builds the embedded binary in dashboard-real. For a local pass:
//
//   npm run build                                  (in dashboard/)
//   cargo build -p open-ferry --bin open-ferry     (embeds dashboard/dist)
//   OPEN_FERRY_BIN=../target/debug/open-ferry.exe npm run e2e:real
//
// The spec starts the binary itself and stops it afterwards. It listens on
// 127.0.0.1 only, on OPEN_FERRY_E2E_PORT (18517 unless set, never 8317),
// over a config in a fresh temp directory with dummy keys, and sends every
// outbound request to a dead proxy at 127.0.0.1:9, except one local fake
// provider with its own direct override. The page may reach
// nothing but the binary. The management key is random and never printed,
// and neither is config.yaml, which holds it: checks on the file report a
// description, not its text.

import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { expect, test, type Locator, type Page } from "@playwright/test";

const BIN = process.env.OPEN_FERRY_BIN ?? "";
const PORT = Number(process.env.OPEN_FERRY_E2E_PORT ?? "18517");
const ORIGIN = `http://127.0.0.1:${String(PORT)}`;
const APP = `${ORIGIN}/dashboard/`;
const DEAD = "http://127.0.0.1:9";
const NL = "\n";

const CLAUDE_KEY = "sk-ant-dummy-0000000000000000";
const GEMINI_KEY = "AIza-dummy-gemini-000000000000";
const EDITED_KEY = "AIza-dummy-edited-000000000000";
const COMPAT_KEY = "sk-compat-dummy-000000000000";
const MODEL = "dashboard-local-model";
const ANSWER = "The local fake provider answered.";

test.skip(BIN === "", "set OPEN_FERRY_BIN to a debug build of open-ferry to run the real-save pass");
test.describe.configure({ mode: "serial" });

let dir = "";
let configPath = "";
let managementKey = "";
let server: ChildProcess | null = null;
let exited: number | null = null;
let page: Page | null = null;
let fake: Server | null = null;
let fakeOrigin = "";
let fakeRequests = 0;
let persistedClientKeys: string[] = [];
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
  expectFile(/^[ #]*some-future-setting: keep-me/m, "the setting open-ferry doesn't type, kept as a comment after migration");
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

async function startServer() {
  exited = null;
  const log = openSync(join(dir, "server.log"), "a");
  const child = spawn(resolve(BIN), ["-config", configPath, "-no-browser"], {
    cwd: dir,
    env: {
      ...process.env,
      OPEN_FERRY_SELF_UPDATE: "off",
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
}

async function stopServer() {
  const child = server;
  if (child !== null && exited === null) {
    const stopped = new Promise((done) => child.once("exit", done));
    child.kill();
    await stopped;
  }
  server = null;
}

// A failed Playwright fill/paste can quote its argument. Replace that
// diagnostic before it reaches a reporter or a failure context.
async function fillSecret(field: Locator, value: string) {
  try { await field.fill(value); }
  catch { throw new Error("Could not fill a secret field"); }
}
async function pasteSecret(text: string) {
  // Exercise CodeMirror's plain-text paste handler without the OS clipboard.
  // CDP insertText on a multiline selection makes Chromium synthesize inline
  // style attributes, which the dashboard's CSP correctly refuses.
  try {
    await the().getByRole("textbox", { name: "config.yaml" }).evaluate((element, text) => {
      const data = new DataTransfer();
      data.setData("text/plain", text);
      element.dispatchEvent(new ClipboardEvent("paste", {
        clipboardData: data, bubbles: true, cancelable: true, composed: true,
      }));
    }, text);
  }
  catch { throw new Error("Could not edit config.yaml"); }
}

async function openYaml() {
  const page = the();
  await page.goto(`${APP}settings?tab=file`);
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  await expectEditorShows("# The dashboard's real-save pass", "the first comment");
  return page.getByRole("textbox", { name: "config.yaml" });
}
async function editYaml(change: (text: string) => string) {
  const page = the();
  const editor = await openYaml();
  const draft = change(file());
  await editor.click();
  await page.keyboard.press("ControlOrMeta+A");
  await pasteSecret(draft);
  await page.getByRole("button", { name: "Review changes" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes to config.yaml" });
  await review.getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(120_000);
  expect(Number.isInteger(PORT) && PORT > 0 && PORT < 65536 && PORT !== 8317, "a safe loopback port").toBe(true);
  dir = mkdtempSync(join(tmpdir(), "ofp-real-save-"));
  mkdirSync(join(dir, "auths"));
  configPath = join(dir, "config.yaml");
  managementKey = `mgmt-dummy-${randomBytes(18).toString("hex")}`;
  writeFileSync(configPath, configText());

  await startServer();

  const opened = await browser.newPage({ bypassCSP: false, serviceWorkers: "block" });
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
  await fillSecret(opened.getByLabel("Management key"), managementKey);
  await opened.getByRole("button", { name: "Sign in" }).click();
  await expect(opened.getByRole("heading", { name: "Overview", level: 1 })).toBeVisible();
});

test.afterAll(async () => {
  await page?.close();
  // Only the open-ferry and fake provider this spec started.
  await stopServer();
  await new Promise<void>((done, reject) => {
    if (fake === null) { done(); return; }
    fake.close((error) => { if (error) reject(error); else done(); });
    fake.closeAllConnections();
  });
  if (dir !== "" && test.info().errors.length === 0) {
    rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
});

test("reviews a second YAML edit after legacy migration without leaving the editor", async () => {
  const page = the();
  const editor = await openYaml();
  expectFile(/^host: 127\.0\.0\.1$/m, "the original legacy layout before its first YAML save");
  // The v8 writer accepts v8 bodies, including JSON (valid YAML). Its
  // stored legacy file migrates, and this flow-style draft is re-rendered.
  const versioned = await (await call("/v8/management/config")).json() as Record<string, unknown>;
  const section = versioned.management;
  if (section === null || typeof section !== "object" || Array.isArray(section)) {
    throw new Error("The normalized fixture has no management section");
  }
  (section as Record<string, unknown>)["secret-key"] = managementKey;
  const comments = configText().split(NL).filter((line) => line.startsWith("#"));
  const draft = [...comments, "# some-future-setting: keep-me", JSON.stringify(versioned, null, 2), "# first YAML migration save", ""].join(NL);
  await editor.click();
  await page.keyboard.press("ControlOrMeta+A");
  await pasteSecret(draft);
  await page.getByRole("button", { name: "Review changes" }).click();
  const written = page.waitForResponse((response) => new URL(response.url()).pathname === "/v8/management/config.yaml" && response.request().method() === "PUT");
  await page.getByRole("dialog").getByRole("button", { name: "Save config.yaml" }).click();
  const answer = await written;
  const body: unknown = await answer.json();
  const code = body !== null && typeof body === "object" && "error" in body && typeof body.error === "string" && /^[a-z_]+$/.test(body.error) ? body.error : "none";
  expect(answer.status(), `whole-YAML HTTP status (code: ${code})`).toBe(200);
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  expectFile(/^server:$/m, "the stored v8 layout");
  await editor.click();
  await page.keyboard.press("ControlOrMeta+End");
  await pasteSecret("\n# second YAML edit after normalization\n");
  await page.getByRole("button", { name: "Review changes" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes to config.yaml" });
  await expect(review.getByText("config.yaml changed on the server since you opened it")).toHaveCount(0);
  await review.getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  expectFile("# second YAML edit after normalization", "the second YAML save");
});

test("lifts safe mode from the Overview, then makes a key", async () => {
  const page = the();
  await page.goto(APP);
  await expect(page.getByText("The proxy is in safe mode")).toBeVisible();
  await page.getByRole("button", { name: "Replace the example keys with a new key" }).click();
  await expect(page.getByText("The proxy is out of safe mode")).toBeVisible();
  await expect(page.getByText("The proxy is in safe mode")).toHaveCount(0);
  const replaced = await clientKeys();
  expect(replaced.length, "the replacement client key count").toBe(1);
  expectFile("your-api-key", "no example keys", false);
  expectFile(replaced[0] ?? "-", "the new key");
  expectCommentsKept();

  await page.getByRole("button", { name: "Make a new key" }).click();
  await expect(page.getByText(/^Added a client key\./)).toBeVisible();
  const keys = await clientKeys();
  expect(keys.length, "the client key count").toBe(2);
  expectFile(keys[1] ?? "-", "the key made on the Overview");
  await expect(page.getByRole("combobox", { name: "Client key" }).getByRole("option")).toHaveCount(2);
});

test("turns settings on where the Usage and Logs pages offer to", async () => {
  const page = the();
  await page.goto(`${APP}usage`);
  await expect(page.getByText("Usage isn't being recorded")).toBeVisible();
  await page.getByRole("button", { name: "Start recording" }).click();
  await expect(page.getByText("Usage isn't being recorded")).toHaveCount(0);
  expectFile(/^ *usage-statistics-enabled: true$/m, "usage-statistics-enabled: true");

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
  expectFile(/^ *request-log: true$/m, "request-log: true, from the Usage page");

  await management("PATCH", "request-log", { value: false });
  expectFile(/^ *request-log: false$/m, "request-log: false again");
  await page.goto(`${APP}logs`);
  await expect(page.getByText("Only failed requests are logged")).toBeVisible();
  await page.getByRole("button", { name: "Log every request" }).click();
  await expect(page.getByText("Only failed requests are logged")).toHaveCount(0);
  expectFile(/^ *request-log: true$/m, "request-log: true, from the Logs page");

  await page.goto(`${APP}logs?tab=server`);
  await expect(page.getByText("The server doesn't write its log to a file")).toBeVisible();
  await page.getByRole("button", { name: "Log to a file" }).click();
  await expect(page.getByText("The server doesn't write its log to a file")).toHaveCount(0);
  expectFile(/^ *logging-to-file: true$/m, "logging-to-file: true");
  expectCommentsKept();
});

test("records usage then makes a client key in the Overview health layout", async () => {
  const page = the();
  await management("PATCH", "usage-statistics-enabled", { value: false });
  await page.goto(APP);
  await expect(page.getByRole("button", { name: "Start recording" })).toBeVisible();
  await page.getByRole("button", { name: "Start recording" }).click();
  await expect(page.getByRole("button", { name: "Start recording" })).toHaveCount(0);
  await page.getByRole("button", { name: "Connect a client", exact: true }).click();
  await page.getByRole("button", { name: "Make a new key" }).click();
  await expect(page.getByText(/^Added a client key\./)).toBeVisible();
  const keys = await clientKeys();
  expect(keys.length, "the client key count after the sibling form saves").toBe(3);
  expectFile(keys[2] ?? "-", "the key made after Start recording");
  expectFile(/^ *usage-statistics-enabled: true$/m, "usage recording retained after key creation");
  await management("DELETE", "api-keys?index=2");
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
    [/^ *proxy-url: "?https:\/\/127\.0\.0\.1:9"?$/m, "the new proxy"],
    [/^routing:\n(?: +.*\n)*? +strategy: "?fill-first"?$/m, "routing.strategy: fill-first"],
    [/^ *request-retry: 4\b/m, "request-retry: 4"],
    [/^ *max-retry-credentials: 2$/m, "max-retry-credentials: 2"],
    [/^ *max-retry-interval: 7$/m, "max-retry-interval: 7"],
    [/^ *force-model-prefix: true$/m, "force-model-prefix: true"],
    [/^ *request-log: false$/m, "request-log: false"],
    [/^ *logging-to-file: false$/m, "logging-to-file: false"],
    [/^ *debug: true$/m, "debug: true"],
    [/^ *logs-max-total-size-mb: 64$/m, "logs-max-total-size-mb: 64"],
    [/^ *error-logs-max-files: 5$/m, "error-logs-max-files: 5"],
    [/^ *usage-statistics-enabled: false$/m, "usage-statistics-enabled: false"],
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

test("rebases a Settings draft after a different YAML field saves without leaving the page", async () => {
  const page = the();
  await page.goto(`${APP}settings`);
  const retries = page.getByRole("textbox", { name: "Retries" });
  await expect(retries).toHaveValue("4");
  await retries.fill("6");
  await page.getByRole("tab", { name: "config.yaml" }).click();
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  const editor = page.getByRole("textbox", { name: "config.yaml" });
  await editor.click();
  await page.keyboard.press("ControlOrMeta+A");
  await pasteSecret(file().replace(/(debug: )true\b/, "$1false"));
  await page.getByRole("button", { name: "Review changes" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  await page.getByRole("tab", { name: "Settings", exact: true }).click();
  await expect(retries).toHaveValue("6");
  await page.getByRole("button", { name: "Review and save" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Save 1 setting" }).click();
  await expect(page.getByText("Saved 1 setting. The server uses it from now on.")).toBeVisible();
  expectFile(/^ *request-retry: 6\b/m, "the Settings draft rebased onto the YAML save");
  expectFile(/^ *debug: false$/m, "the independent YAML edit retained");
  await management("PATCH", "request-retry", { value: 4 });
  await management("PATCH", "debug", { value: true });
});

test("names a same-field Settings conflict and reloads only that form", async () => {
  const page = the();
  await page.goto(`${APP}settings`);
  const retries = page.getByRole("textbox", { name: "Retries" });
  await expect(retries).toHaveValue("4");
  await retries.fill("6");
  await page.getByRole("tab", { name: "config.yaml" }).click();
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  const editor = page.getByRole("textbox", { name: "config.yaml" });
  await editor.click();
  await page.keyboard.press("ControlOrMeta+A");
  await pasteSecret(file().replace(/(request-retry: )4\b/, "$19"));
  await page.getByRole("button", { name: "Review changes" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  const afterYaml = file();
  await editor.click();
  await page.keyboard.press("ControlOrMeta+End");
  await pasteSecret("\n# YAML draft survives a sibling reload\n");
  await page.getByRole("tab", { name: "Settings", exact: true }).click();
  await expect(retries).toHaveValue("6");
  await page.getByRole("button", { name: "Review and save" }).click();
  await expect(page.getByText("The config changed since this page loaded")).toBeVisible();
  await expect(page.getByText(/Changed fields: Retries/)).toBeVisible();
  await page.getByRole("button", { name: "Reload latest config" }).click();
  await expect(retries).toHaveValue("9");
  expect(file() === afterYaml, "a Settings form reload leaves the YAML file byte for byte").toBe(true);
  await page.getByRole("tab", { name: "config.yaml" }).click();
  await editor.click();
  await page.keyboard.press("ControlOrMeta+End");
  await expectEditorShows("# YAML draft survives a sibling reload", "the other form's unsaved draft");
  await page.getByRole("button", { name: "Review changes" }).click();
  const yamlReview = page.getByRole("dialog", { name: "Review the changes to config.yaml" });
  await expect(yamlReview.getByText("config.yaml changed on the server since you opened it")).toHaveCount(0);
  const written = page.waitForResponse((response) => new URL(response.url()).pathname === "/v8/management/config.yaml" && response.request().method() === "PUT");
  await yamlReview.getByRole("button", { name: "Save config.yaml" }).click();
  expect((await written).status(), "the YAML save after sibling reload").toBe(200);
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  expectFile("# YAML draft survives a sibling reload", "the YAML draft saved after the Settings reload");
  expectFile(/^ *request-retry: 9\b/m, "the conflicting YAML field kept");
  await management("PATCH", "request-retry", { value: 4 });
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
  expect((await clientKeys()).length, "the client key count").toBe(2);
  await page.getByRole("button", { name: "Review and save" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes" });
  await review.getByRole("button", { name: "Save 1 change" }).click();
  await expect(page.getByText("Saved 1 change. The server uses it from now on.")).toBeVisible();
  await expect(card.getByText("Not saved yet")).toHaveCount(0);
  await expect(card.getByRole("listitem")).toHaveCount(3);
  const keys = await clientKeys();
  expect(keys.length, "the client key count").toBe(3);
  const added = keys[2] ?? "-";
  expectFile(added, "the key added on the Settings page");

  await card.getByRole("button", { name: /^Delete / }).last().click();
  await expect(card.getByText("Will be deleted")).toBeVisible();
  expect((await clientKeys()).length, "the client key count").toBe(3);
  await page.getByRole("button", { name: "Review and save" }).click();
  await review.getByRole("button", { name: "Save 1 change" }).click();
  await expect(card.getByRole("listitem")).toHaveCount(2);
  expect((await clientKeys()).length, "the client key count").toBe(2);
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
  await fillSecret(add.getByLabel("API key", { exact: true }), GEMINI_KEY);
  await add.getByLabel("Base URL (optional)").fill(DEAD);
  await add.getByRole("button", { name: "Add the key" }).click();
  await expect(page.getByText("Added the Gemini key: the server uses it from now on.")).toBeVisible();
  expectFile(/^ *gemini:$/m, "the v8 Gemini key group");
  expectFile(GEMINI_KEY, "the Gemini key");

  await page.getByRole("button", { name: /^Delete the Claude key / }).click();
  const confirm = page.getByRole("dialog", { name: "Delete this Claude key?" });
  await confirm.getByRole("button", { name: "Delete key" }).click();
  await expect(page.getByRole("button", { name: /^Delete the Claude key / })).toHaveCount(0);
  expectFile(CLAUDE_KEY, "the removed Claude key", false);
  expectFile(GEMINI_KEY, "the Gemini key still");
  expectCommentsKept();
});

test("edits and removes a provider API key through the dashboard", async () => {
  const page = the();
  await editYaml((text) => text.replace(GEMINI_KEY, EDITED_KEY));
  expectFile(GEMINI_KEY, "the old provider key", false);
  expectFile(EDITED_KEY, "the edited provider key");
  await page.goto(`${APP}credentials`);
  await page.getByRole("button", { name: /^Delete the Gemini key / }).click();
  await page.getByRole("dialog", { name: "Delete this Gemini key?" })
    .getByRole("button", { name: "Delete key" }).click();
  await expect(page.getByRole("button", { name: /^Delete the Gemini key / })).toHaveCount(0);
  expectFile(EDITED_KEY, "the removed provider key", false);
});

test("adds a local compatible provider and records a successful proxy request", async () => {
  fake = createServer((request, response) => {
    if (request.url !== "/v1/chat/completions" || request.method !== "POST" ||
        request.headers.authorization !== `Bearer ${COMPAT_KEY}`) {
      response.writeHead(400).end();
      return;
    }
    request.resume();
    fakeRequests++;
    response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
      id: "local-answer", object: "chat.completion", created: 1, model: MODEL,
      choices: [{ index: 0, message: { role: "assistant", content: ANSWER }, finish_reason: "stop" }],
      usage: { prompt_tokens: 3, completion_tokens: 5, total_tokens: 8 },
    }));
  });
  await new Promise<void>((done) => { fake?.listen(0, "127.0.0.1", done); });
  const address = fake.address();
  if (address === null || typeof address === "string" || address.port === 8317) {
    throw new Error("The fake provider needs a safe loopback port");
  }
  fakeOrigin = `http://127.0.0.1:${String(address.port)}`;
  await editYaml((text) => text.replace(/^api-keys:$/m,
    "api-keys:\n    openai-compatibility:\n" +
    `        - name: dashboard-local\n          base-url: ${fakeOrigin}/v1\n` +
    `          keys:\n            - api-key: ${COMPAT_KEY}\n              proxy-url: direct\n` +
    `          models:\n            - name: ${MODEL}\n              alias: ${MODEL}`));
  expectFile(COMPAT_KEY, "the local provider credential");
  const page = the();
  await page.goto(`${APP}usage`);
  await page.getByRole("button", { name: "Start recording" }).click();
  await expect(page.getByText("Usage isn't being recorded")).toHaveCount(0);
  const [key] = await clientKeys();
  const response = await fetch(`${ORIGIN}/v1/chat/completions`, {
    method: "POST", headers: { Authorization: `Bearer ${key ?? ""}`, "Content-Type": "application/json" },
    body: JSON.stringify({ model: MODEL, messages: [{ role: "user", content: "hello" }] }),
  });
  expect(response.status, "the local proxy response status").toBe(200);
  const body = await response.json() as { choices?: { message?: { content?: string } }[] };
  expect(body.choices?.[0]?.message?.content === ANSWER, "the fake provider's answer").toBe(true);
  expect(fakeRequests, "requests received by the fake provider").toBe(1);
  await expect.poll(async () => {
    const usage = await call("/open-ferry/api/v1/usage/requests");
    const data = await usage.json() as { requests?: { model: string; status: number }[] };
    return data.requests?.some((request) => request.model === MODEL && request.status === 200) ?? false;
  }, { message: "the usage ledger records the local provider call", timeout: 15_000 }).toBe(true);
  await page.goto(`${APP}usage`);
  const row = page.getByRole("table", { name: "Calls to providers, newest first" })
    .getByRole("row").filter({ hasText: MODEL });
  await expect(row, "the dashboard reports the local model request").toHaveCount(1);
  await expect(row.getByText("200", { exact: true })).toBeVisible();
  persistedClientKeys = await clientKeys();
});

test("refuses stale settings and YAML saves after an on-disk edit", async () => {
  const page = the();
  await page.goto(`${APP}settings`);
  const retries = page.getByRole("textbox", { name: "Retries" });
  await expect(retries).toHaveValue("4");
  await retries.fill("6");
  await page.getByRole("button", { name: "Review and save" }).click();
  const review = page.getByRole("dialog", { name: "Review the changes" });
  await expect(review).toBeVisible();
  // Hold the actual PUT after all dashboard re-reads, then change disk.
  // This must exercise the server's If-Match refusal, not a local preflight.
  const retryWrite = `${ORIGIN}/v8/management/config/routing/retry/request-retry`;
  await page.route(retryWrite, async (route) => {
    writeFileSync(configPath, file().replace(/(request-retry: )4\b/, (_, prefix: string) => `${prefix}8`));
    await expect.poll(async () => (await management<Record<string, unknown>>("GET", "config"))["request-retry"],
      { message: "the server loaded the independent on-disk retry change" }).toBe(8);
    await route.continue();
  });
  const retryResponse = page.waitForResponse((response) => response.url() === retryWrite && response.request().method() === "PUT");
  await review.getByRole("button", { name: "Save 1 setting" }).click();
  expect((await retryResponse).status(), "the racing settings save is refused by the server").toBe(412);
  await page.unroute(retryWrite);
  await expect(review.getByText("The config changed since this page loaded")).toBeVisible();
  await expect(review.getByText(/Unsaved edits will be lost on reload/)).toBeVisible();
  await expect(review.getByText(/The server refused this save/)).toBeVisible();
  expectFile(/^ *request-retry: 8\b/m, "the on-disk retry value after the refused settings save");
  await review.getByRole("button", { name: "Reload latest config" }).click();
  await expect(retries).toHaveValue("8");

  const editor = await openYaml();
  await editor.click();
  await page.keyboard.press("ControlOrMeta+End");
  await pasteSecret("\n# unsaved dashboard edit\n");
  await page.getByRole("button", { name: "Review changes" }).click();
  const yamlReview = page.getByRole("dialog", { name: "Review the changes to config.yaml" });
  await expect(yamlReview).toBeVisible();
  const yamlWrite = `${ORIGIN}/v8/management/config.yaml`;
  let onDisk = "";
  await page.route(yamlWrite, async (route) => {
    if (route.request().method() === "PUT") {
      writeFileSync(configPath, `${file()}\n# independent disk edit\n`);
      onDisk = file();
    }
    await route.continue();
  });
  const yamlResponse = page.waitForResponse((response) => response.url() === yamlWrite && response.request().method() === "PUT");
  await yamlReview.getByRole("button", { name: "Save config.yaml" }).click();
  expect((await yamlResponse).status(), "the racing YAML save is refused by the server").toBe(412);
  await page.unroute(yamlWrite);
  await expect(yamlReview.getByText("The config changed since this page loaded")).toBeVisible();
  expect(file() === onDisk, "the refused YAML save leaves every on-disk byte unchanged").toBe(true);
  await yamlReview.getByRole("button", { name: "Reload latest config" }).click();
  await expectEditorShows("# The dashboard's real-save pass", "the reloaded file");
});

test("repairs a broken on-disk config while the server keeps its last good config", async () => {
  const page = the();
  const good = file();
  writeFileSync(configPath, `${good}\nbroken: [\n`);
  await page.goto(`${APP}settings`);
  await expect(page.getByText("config.yaml doesn't parse").first()).toBeVisible();
  await expect(page.getByText("The server failed (HTTP 500)")).toHaveCount(0);
  await expect.poll(async () => (await management<Record<string, unknown>>("GET", "config"))["request-retry"],
    { message: "the last good configuration remains active" }).toBe(8);
  await page.getByRole("link", { name: "YAML editor" }).first().click();
  await page.getByRole("button", { name: "Show config.yaml" }).click();
  await expect(page.getByText("Recovery mode: config.yaml doesn't parse")).toBeVisible();
  const editor = page.getByRole("textbox", { name: "config.yaml" });
  await editor.click();
  await page.keyboard.press("ControlOrMeta+A");
  await pasteSecret(good.replace(/(request-retry: )8\b/, "$17"));
  await page.getByRole("button", { name: "Review changes" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Save config.yaml" }).click();
  await expect(page.getByText("Saved config.yaml. The server uses it from now on.")).toBeVisible();
  await expect.poll(async () => (await management<Record<string, unknown>>("GET", "config"))["request-retry"],
    { message: "the repaired configuration reloads" }).toBe(7);
  expectFile("broken: [", "the invalid syntax", false);
  await management("PATCH", "request-retry", { value: 8 });
});

test("restarts on the same config and auth directory with saved settings and credentials", async () => {
  const beforeRestart = file();
  await stopServer();
  await startServer();
  expect(file() === beforeRestart, "restart keeps the saved file byte for byte").toBe(true);
  const page = the();
  await page.goto(`${APP}settings`);
  await expect(page.getByRole("textbox", { name: "Retries" })).toHaveValue("8");
  await expect(page.getByRole("combobox", { name: "How credentials are picked" })).toHaveValue("fill-first");
  await expect(page.getByRole("textbox", { name: "Log directory limit (MB)" })).toHaveValue("64");
  const keys = await clientKeys();
  expect(JSON.stringify(keys) === JSON.stringify(persistedClientKeys), "client keys survived restart").toBe(true);
  await page.goto(`${APP}credentials`);
  await expect(page.getByRole("button", { name: /^Delete the Claude key / })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^Delete the Gemini key / })).toHaveCount(0);
  expectFile(COMPAT_KEY, "the saved compatible-provider credential after restart");
  await page.goto(APP);
  await page.getByRole("button", { name: "Connect a client", exact: true }).click();
  await page.getByText("Address, model and shell", { exact: true }).click();
  await page.getByRole("combobox", { name: "Model", exact: true }).selectOption(MODEL);
  await expect(page.getByRole("combobox", { name: "Model", exact: true })).toHaveValue(MODEL);
  const setup = await (await call("/open-ferry/api/v1/client-setup")).json() as { safe_mode: boolean; models: { id: string }[] };
  expect(setup.safe_mode, "safe mode stays lifted after restart").toBe(false);
  expect(setup.models.some((model) => model.id === MODEL), "the saved provider model is active after restart").toBe(true);
  expectCommentsKept();
});

test("shows plain update status and refuses install and rollback with CLI instructions", async () => {
  const page = the();
  await page.goto(`${APP}about`);
  await expect(page.getByRole("region", { name: "Updates" })).toBeVisible();
  await expect(page.getByText(/To update this server, run.*open-ferry update.*then restart it/)).toBeVisible();
  const response = await call("/open-ferry/api/v1/update");
  expect(response.status, "plain update status").toBe(200);
  const status = await response.json() as { supervisor: unknown };
  expect(status.supervisor, "a plain run has no supervisor").toBeNull();
  await expect(page.getByRole("button", { name: "Install now" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Roll back" })).toHaveCount(0);
  for (const action of ["install", "rollback"]) {
    const refused = await call(`/open-ferry/api/v1/update/${action}`, "POST",
      action === "rollback" ? { target: "0.0.1" } : undefined);
    expect(refused.status, `plain ${action} status`).toBe(409);
    const error = await refused.json() as { error: string; message: string };
    expect(error.error).toBe("not_supervised");
    expect(error.message).toBe("this install is not supervised; run `open-ferry update` (or `open-ferry update -rollback`) on the server's computer, then restart the server");
  }
});

test("ran under the binary's policy, on its origin only", () => {
  expect(violations.length, "Content-Security-Policy violation count").toBe(0);
  expect(pageErrors.length, "uncaught exception count").toBe(0);
  expect(offOrigin.length, "requests that left the binary's origin").toBe(0);
});
