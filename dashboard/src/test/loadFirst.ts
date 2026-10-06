import { beforeAll } from "vitest";

/**
 * The limit for loadFirst's hook, longer than Vitest's 10 s for hooks.
 * Loading is all the hook does, and while a debug cargo build ran, the Usage
 * page took up to 5.1 s to load, which leaves 10 s too little room. No test
 * waits any longer for it.
 */
const LOAD_TIMEOUT_MS = 30_000;

/**
 * Loads pages that the app loads lazily (src/app/routes.tsx), once, before
 * the test file's tests run. A test file that imports the page itself
 * already has it loaded.
 *
 * Each test file starts with nothing loaded. The first test to show a lazy
 * page paid for loading it inside its first wait for the page, a 3 s wait
 * (asyncUtilTimeout in setup.ts). The Usage page, which brings in the chart
 * library, takes about 0.7 s on a quiet machine. On a machine busy with a
 * build every page loads several times slower, and the waits for the Usage,
 * Ledger, Credentials and Settings headings ran out mid-load.
 *
 * Loaded here, the app's own lazy import gets the same module, already
 * loaded, so a test waits only for the page to render and fetch its data.
 *
 * @example loadFirst(() => import("./UsagePage"));
 */
export function loadFirst(...loads: (() => Promise<unknown>)[]) {
  beforeAll(async () => {
    await Promise.all(loads.map((load) => load()));
  }, LOAD_TIMEOUT_MS);
}
