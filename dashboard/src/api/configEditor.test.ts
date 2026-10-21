import { describe, expect, it, vi } from "vitest";
import { mockApi, route } from "../test/mockApi";
import { apiRequest } from "./client";
import { ConfigEditor, ConfigStore } from "./configEditor";
import { API_KEYS, CONFIG, CONFIG_YAML, MANAGEMENT, V8_CONFIG } from "./management";

const RETRY = `${V8_CONFIG}/routing/retry/request-retry`;
function editor() {
  return new ConfigEditor(<T,>(path: string, request = {}) => apiRequest<T>("unit-key", path, request));
}

describe("a form's config version", () => {
  function shared() {
    let version = 1;
    const tree = { routing: { retry: { "request-retry": 1 } },
      observability: { logs: { debug: false } }, access: { "api-keys": ["unit-client"] } };
    let race = false;
    const api = mockApi(
      route("GET", V8_CONFIG, () => {
        const answer = { json: structuredClone(tree), headers: { ETag: `"${String(version)}"` } };
        if (race) { race = false; version++; tree.observability.logs.debug = true; }
        return answer;
      }),
      ...[[RETRY, (value: unknown) => { tree.routing.retry["request-retry"] = Number(value); }],
        [`${V8_CONFIG}/observability/logs/debug`, (value: unknown) => { tree.observability.logs.debug = value === true; }],
      ].map(([path, write]) => route("PUT", path as string, (request) => {
        if (request.headers.get("if-match") !== `"${String(version)}"`) {
          return { status: 412, json: { error: "precondition_failed" }, headers: { ETag: '"refusal-only"' } };
        }
        (write as (value: unknown) => void)(request.json());
        version++;
        return { json: {}, headers: { ETag: `"${String(version)}"` } };
      })),
    );
    const changed = vi.fn();
    const store = new ConfigStore(<T,>(path: string, request = {}) => apiRequest<T>("unit-key", path, request), changed);
    return { api, tree, changed, first: new ConfigEditor(store), second: new ConfigEditor(store), race: () => { race = true; } };
  }
  const retryWrite = { method: "PATCH", json: { value: 2 } } as const;
  const debugPath = `${MANAGEMENT}/debug`;

  it("lets a clean sibling adopt another editor's save on refetch", async () => {
    const state = shared();
    await Promise.all([state.first.call(CONFIG), state.second.call(CONFIG)]);
    await state.first.call(`${MANAGEMENT}/request-retry`, retryWrite);
    expect(await state.second.call(CONFIG)).toMatchObject({ "request-retry": 2 });
    await state.second.call(debugPath, { method: "PUT", json: { value: true } });
    expect(state.api.callsTo("PUT", `${V8_CONFIG}/observability/logs/debug`)[0]?.headers.get("if-match")).toBe('"2"');
    expect(state.changed).toHaveBeenCalledTimes(2);
  });

  it("keeps a draft's base through refetch and rebases an unrelated-field save", async () => {
    const state = shared();
    await Promise.all([state.first.call(CONFIG), state.second.call(CONFIG)]);
    state.first.setDirty(true);
    await state.second.call(debugPath, { method: "PUT", json: { value: true } });
    await expect(state.first.call(CONFIG)).resolves.toMatchObject({ debug: true });
    await state.first.checkChanges([`${MANAGEMENT}/request-retry`]);
    await state.first.call(`${MANAGEMENT}/request-retry`, retryWrite);
    expect(state.api.callsTo("PUT", RETRY)[0]?.headers.get("if-match")).toBe('"2"');
    expect(state.tree.routing.retry["request-retry"]).toBe(2);
    expect(state.tree.observability.logs.debug).toBe(true);
  });

  it("names a drafted field changed by another editor and sends no stale write", async () => {
    const state = shared();
    await Promise.all([state.first.call(CONFIG), state.second.call(CONFIG)]);
    state.first.setDirty(true);
    await state.second.call(`${MANAGEMENT}/request-retry`, { method: "PUT", json: { value: 3 } });
    await state.first.call(CONFIG);
    await expect(state.first.call(`${MANAGEMENT}/request-retry`, retryWrite))
      .rejects.toMatchObject({ status: 412, fields: ["Retries"] });
    expect(state.api.callsTo("PUT", RETRY)).toHaveLength(1);
  });

  it("uses the re-read's If-Match and never retries a racing server refusal", async () => {
    const state = shared();
    await state.first.call(CONFIG);
    state.first.setDirty(true);
    state.race();
    await expect(state.first.call(`${MANAGEMENT}/request-retry`, retryWrite)).rejects.toMatchObject({ status: 412 });
    expect(state.api.callsTo("PUT", RETRY)).toHaveLength(1);
    expect(state.api.callsTo("PUT", RETRY)[0]?.headers.get("if-match")).toBe('"1"');
    await state.first.call(`${MANAGEMENT}/request-retry`, retryWrite);
    expect(state.api.callsTo("PUT", RETRY)[1]?.headers.get("if-match")).toBe('"2"');
  });

  it.each([undefined, '"base"'])("guards YAML, client keys and provider lists with version %s", async (etag) => {
    const headers: Record<string, string> = etag === undefined ? {} : { ETag: etag };
    const api = mockApi(
      route("GET", V8_CONFIG, { json: {}, headers }),
      route("GET", CONFIG_YAML, { text: "debug: false" }),
      route("GET", "/v8/management/config.yaml", { text: "debug: false", headers }),
      route("GET", API_KEYS, { json: { "api-keys": ["unit-client"] } }),
      route("PUT", API_KEYS, { json: {} }),
      ...[CONFIG_YAML, "/v8/management/config.yaml", `${V8_CONFIG}/access/api-keys`, `${MANAGEMENT}/gemini-api-key`, `${V8_CONFIG}/api-keys/gemini`].map((path) => route("PUT", path, { json: {}, headers })),
    );
    const form = editor();
    await form.call(CONFIG_YAML);
    await form.call(CONFIG_YAML, { method: "PUT", body: "debug: true" });
    await form.call(API_KEYS, { method: "PUT", json: ["unit-client"] });
    await form.call(`${MANAGEMENT}/gemini-api-key`, { method: "PUT", json: [{ "api-key": "unit-provider" }] });
    const writes = api.calls.filter((call) => call.method === "PUT");
    expect(writes.map((call) => call.url.pathname)).toEqual(etag === undefined
      ? [CONFIG_YAML, API_KEYS, `${MANAGEMENT}/gemini-api-key`]
      : ["/v8/management/config.yaml", `${V8_CONFIG}/access/api-keys`, `${V8_CONFIG}/api-keys/gemini`]);
    for (const call of writes) {
      expect(call.headers.get("if-match")).toBe(etag ?? null);
    }
  });

  it("uses the whole-file ETag after its own semantic YAML save", async () => {
    let version = 1;
    const tree = { routing: { retry: { "request-retry": 1 } } };
    let text = "routing: {retry: {request-retry: 1}}\n";
    const api = mockApi(
      route("GET", V8_CONFIG, () => ({ json: tree, headers: { ETag: `"${String(version)}"` } })),
      route("GET", "/v8/management/config.yaml", () => ({ text, headers: { ETag: `"${String(version)}"` } })),
      route("PUT", "/v8/management/config.yaml", (request) => {
        tree.routing.retry["request-retry"] = 2;
        text = request.body ?? "";
        version++;
        return { json: {}, headers: { ETag: `"${String(version)}"` } };
      }),
    );
    const form = editor();
    await form.call(CONFIG_YAML);
    form.setDirty(true);
    await form.call(CONFIG_YAML, { method: "PUT", body: "routing: {retry: {request-retry: 2}}\n" });
    form.setDirty(false);
    await form.call(CONFIG_YAML);
    form.setDirty(true);
    await form.call(CONFIG_YAML, { method: "PUT", body: `${text}# another edit\n` });
    expect(api.callsTo("PUT", "/v8/management/config.yaml").map((call) => call.headers.get("if-match")))
      .toEqual(['"1"', '"2"']);
  });

  it("refuses a whole client-key payload raced after a clean one-click form's read", async () => {
    let keys = ["unit-original"];
    let etag = '"base"';
    const api = mockApi(route("GET", V8_CONFIG, () => ({
      json: { access: { "api-keys": keys } }, headers: { ETag: etag },
    })));
    const form = editor();
    await form.call(API_KEYS);
    keys = [...keys, "unit-other-writer"];
    etag = '"other"';
    for (let attempt = 0; attempt < 2; attempt++) {
      await expect(form.call(API_KEYS, { method: "PUT", json: ["unit-replacement"] }))
        .rejects.toMatchObject({ fields: ["Client API keys"] });
    }
    expect(api.calls.filter((call) => call.method !== "GET")).toEqual([]);
  });

  it("keeps the established guard if a YAML read omits its ETag", async () => {
    const api = mockApi(
      route("GET", V8_CONFIG, { json: {}, headers: { ETag: '"base"' } }),
      route("GET", "/v8/management/config.yaml", { text: "debug: false" }),
      route("PUT", "/v8/management/config.yaml", { json: {}, headers: { ETag: '"saved"' } }),
    );
    const form = editor();
    await expect(form.call(CONFIG_YAML)).rejects.toThrow("The server omitted the config version");
    await form.call(CONFIG_YAML, { method: "PUT", body: "debug: true" });
    expect(api.callsTo("PUT", "/v8/management/config.yaml")[0]?.headers.get("if-match")).toBe('"base"');
    expect(api.callsTo("PUT", CONFIG_YAML)).toEqual([]);
  });

  it.each([undefined, '"base"'])("refuses an unmapped writer with version %s", async (etag) => {
    const api = mockApi(route("GET", V8_CONFIG, { json: {}, headers: etag === undefined ? {} : { ETag: etag } }));
    await expect(editor().call(`${MANAGEMENT}/unknown-setting`, { method: "PUT", json: true }))
      .rejects.toThrow("This config write has no versioned route.");
    expect(api.calls.filter((call) => call.method !== "GET")).toEqual([]);
  });

  it("uses disk values when the file watcher has not updated the legacy reads", async () => {
    const api = mockApi(
      route("GET", V8_CONFIG, { json: {
        access: { "api-keys": ["unit-disk-client"] },
        routing: { retry: { "request-retry": 8 } },
        "api-keys": { gemini: [{ "base-url": "http://127.0.0.1:9", prefix: "shared", keys: [{ "api-key": "unit-provider", auth_index: "7" }] }] },
      }, headers: { ETag: '"disk"' } }),
      route("GET", CONFIG, { json: { "request-retry": 1 } }),
      route("GET", API_KEYS, { json: { "api-keys": ["unit-stale-client"] } }),
      route("PUT", `${V8_CONFIG}/access/api-keys`, { json: {}, headers: { ETag: '"saved"' } }),
    );
    const form = editor();
    expect((await form.call<Record<string, unknown>>(CONFIG))["request-retry"]).toBe(8);
    expect(await form.call(`${MANAGEMENT}/gemini-api-key`)).toEqual({ "gemini-api-key": [{
      "base-url": "http://127.0.0.1:9", prefix: "shared", "api-key": "unit-provider", "auth-index": "7",
    }] });
    await form.call(API_KEYS, { method: "PATCH", json: { old: "unit-added", new: "unit-added" } });
    expect(api.callsTo("PUT", `${V8_CONFIG}/access/api-keys`)[0]?.json()).toEqual(["unit-disk-client", "unit-added"]);
    expect(api.callsTo("GET", API_KEYS)).toEqual([]);
    expect(api.callsTo("GET", CONFIG)).toEqual([]);
  });

  it("guards config-backed credential toggles and preserves inherited exclusions", async () => {
    const api = mockApi(
      route("GET", V8_CONFIG, { json: { "api-keys": { gemini: [{
        "base-url": "http://127.0.0.1:9", "excluded-models": ["old-model"],
        keys: [{ "api-key": "unit-provider", auth_index: "7" }, { "api-key": "unit-spare", auth_index: "8" }],
      }] } }, headers: { ETag: '"base"' } }),
      route("GET", `${MANAGEMENT}/gemini-api-key`, { json: { "gemini-api-key": [
        { "api-key": "unit-provider", "base-url": "http://127.0.0.1:9", "auth-index": "7" },
      ] } }),
      route("PATCH", V8_CONFIG, { json: {}, headers: { ETag: '"saved"' } }),
    );
    const form = editor();
    expect(await form.toggleCredential({ name: "unit-credential", auth_index: "7" }, true))
      .toEqual({ status: "ok", disabled: true });
    const write = api.callsTo("PATCH", V8_CONFIG)[0];
    expect(write?.headers.get("if-match")).toBe('"base"');
    expect(write?.json()).toMatchObject({ "api-keys": { gemini: [{
      "excluded-models": ["old-model"],
      keys: [{ "excluded-models": ["old-model", "*"] }, { "api-key": "unit-spare" }],
    }] } });
    expect(api.callsTo("PATCH", "/v0/management/auth-files/status")).toEqual([]);
    await expect(form.toggleCredential({ name: "unit-removed", auth_index: "missing" }, false))
      .rejects.toMatchObject({ status: 412 });
    expect(api.callsTo("PATCH", V8_CONFIG)).toHaveLength(1);
  });

  it("keeps each in-flight toggle's original version when another save finishes", async () => {
    const snapshot = { json: { "api-keys": { gemini: [{ keys: [
      { "api-key": "unit-first", auth_index: "7" }, { "api-key": "unit-second", auth_index: "8" },
    ] }] } }, headers: { ETag: '"base"' } };
    let gets = 0;
    let release: () => void = () => { throw new Error("the deferred read was not initialized"); };
    const held = new Promise<void>((done) => { release = done; });
    let etag = '"base"';
    const api = mockApi(
      route("GET", V8_CONFIG, async () => { if (++gets === 3) await held; return snapshot; }),
      route("GET", `${MANAGEMENT}/gemini-api-key`, { json: { "gemini-api-key": [
        { "api-key": "unit-first", "auth-index": "7" }, { "api-key": "unit-second", "auth-index": "8" },
      ] } }),
      route("PATCH", V8_CONFIG, (request) => {
        if (request.headers.get("if-match") !== etag) {
          return { status: 412, json: { error: "precondition_failed" } };
        }
        etag = '"saved"';
        return { json: {}, headers: { ETag: etag } };
      }),
    );
    const form = editor();
    await form.ready();
    const first = form.toggleCredential({ name: "unit-first", auth_index: "7" }, true);
    const second = form.toggleCredential({ name: "unit-second", auth_index: "8" }, true);
    await first;
    release();
    await expect(second).rejects.toMatchObject({ status: 412 });
    expect(api.callsTo("PATCH", V8_CONFIG).map((call) => call.headers.get("if-match")))
      .toEqual(['"base"', '"base"']);
  });

  it("uses the legacy credential status route without a header on a server without ETags", async () => {
    const api = mockApi(
      route("GET", V8_CONFIG, { json: {} }),
      route("PATCH", "/v0/management/auth-files/status", { json: { status: "ok", disabled: true } }),
    );
    await editor().toggleCredential({ name: "unit-credential", auth_index: "7" }, true);
    expect(api.callsTo("PATCH", "/v0/management/auth-files/status")[0]?.headers.has("if-match")).toBe(false);
  });

  it("refuses a config credential whose live index still describes an older disk key", async () => {
    const api = mockApi(
      route("GET", V8_CONFIG, { json: { "api-keys": { gemini: [{ keys: [{ "api-key": "unit-new", auth_index: "7" }] }] } }, headers: { ETag: '"disk"' } }),
      route("GET", `${MANAGEMENT}/gemini-api-key`, { json: { "gemini-api-key": [{ "api-key": "unit-old", "auth-index": "7" }] } }),
    );
    await expect(editor().toggleCredential({ name: "unit-old", auth_index: "7" }, true))
      .rejects.toMatchObject({ status: 412 });
    expect(api.callsTo("PATCH", V8_CONFIG)).toEqual([]);
  });

  it.each(["add", "delete"])("preserves named provider groups when keys %s", async (operation) => {
    const groups = [{ name: "named", prefix: "team", keys: [
      { "api-key": "unit-first", auth_index: "7" }, { "api-key": "unit-second", auth_index: "8" },
    ] }];
    const path = `${MANAGEMENT}/gemini-api-key`;
    const target = `${V8_CONFIG}/api-keys/gemini`;
    const api = mockApi(
      route("GET", V8_CONFIG, { json: { "api-keys": { gemini: groups } }, headers: { ETag: '"base"' } }),
      route("PUT", target, { json: {}, headers: { ETag: '"saved"' } }),
    );
    const form = editor();
    const read = await form.call<{ "gemini-api-key": Record<string, unknown>[] }>(path);
    if (operation === "add") {
      const entries = read["gemini-api-key"].map((value) => {
        const entry = { ...value }; delete entry["auth-index"]; return entry;
      });
      await form.call(path, { method: "PUT", json: [...entries, { "api-key": "unit-new" }] });
      expect(api.callsTo("PUT", target)[0]?.json()).toEqual([...groups, { keys: [{ "api-key": "unit-new" }] }]);
    } else {
      await form.call(path, { method: "DELETE", query: { index: 1 }, configTarget: { key: "unit-second" } });
      expect(api.callsTo("PUT", target)[0]?.json()).toEqual([{ ...groups[0], keys: [{ "api-key": "unit-first", auth_index: "7" }] }]);
    }
  });

  it.each([
    { shared: {}, joins: true },
    { shared: { prefix: "team" }, joins: false },
    { shared: { headers: { "X-Team": "unit-team" } }, joins: false },
  ])("joins an added provider key only without unrequested shared fields ($shared)", async ({ shared, joins }) => {
    const base = "http://127.0.0.1:9";
    const group = { "base-url": base, ...shared, keys: [{ "api-key": "unit-first" }] };
    const path = `${MANAGEMENT}/gemini-api-key`;
    const target = `${V8_CONFIG}/api-keys/gemini`;
    const api = mockApi(
      route("GET", V8_CONFIG, { json: { "api-keys": { gemini: [group] } }, headers: { ETag: '"base"' } }),
      route("PUT", target, { json: {}, headers: { ETag: '"saved"' } }),
    );
    const form = editor();
    const entries = (await form.call<{ "gemini-api-key": unknown[] }>(path))["gemini-api-key"];
    await form.call(path, { method: "PUT", json: [...entries, { "base-url": base, "api-key": "unit-new" }] });
    expect(api.callsTo("PUT", target)[0]?.json()).toEqual(joins
      ? [{ ...group, keys: [...group.keys, { "api-key": "unit-new" }] }]
      : [group, { "base-url": base, keys: [{ "api-key": "unit-new" }] }]);
  });

  it("removes a named provider group when its last key is deleted", async () => {
    const keep = { "base-url": "http://127.0.0.1:9", keys: [{ "api-key": "unit-keep" }] };
    const target = `${V8_CONFIG}/api-keys/gemini`;
    const api = mockApi(
      route("GET", V8_CONFIG, { json: { "api-keys": { gemini: [
        { name: "removed", prefix: "team", headers: { "X-Team": "unit" }, keys: [{ "api-key": "unit-delete" }] }, keep,
      ] } }, headers: { ETag: '"base"' } }),
      route("PUT", target, { json: {} }),
    );
    await editor().call(`${MANAGEMENT}/gemini-api-key`, {
      method: "DELETE", query: { index: 0 }, configTarget: { key: "unit-delete" },
    });
    expect(api.callsTo("PUT", target)[0]?.json()).toEqual([keep]);
  });

  it.each([undefined, '"base"'])("rejects unmapped client-key methods even without ETags (%s)", async (etag) => {
    const api = mockApi(route("GET", V8_CONFIG, { json: {}, headers: etag === undefined ? {} : { ETag: etag } }));
    await expect(editor().call(API_KEYS, { method: "POST", json: ["unit-unmapped"] }))
      .rejects.toThrow("This config write has no versioned route.");
    expect(api.calls.filter((call) => call.method !== "GET")).toEqual([]);
  });

  it("refuses an index delete which now names another client key", async () => {
    const api = mockApi(route("GET", V8_CONFIG, { json: { access: { "api-keys": ["unit-other"] } }, headers: { ETag: '"base"' } }));
    await expect(editor().call(API_KEYS, { method: "DELETE", query: { index: 0 }, configTarget: { key: "unit-intended" } }))
      .rejects.toMatchObject({ status: 412 });
    expect(api.callsTo("PUT", `${V8_CONFIG}/access/api-keys`)).toEqual([]);
  });

  it("keeps the stale version on 412 and never retries without it", async () => {
    const api = mockApi(
      route("GET", V8_CONFIG, { json: {}, headers: { ETag: '"base"' } }),
      route("PUT", RETRY, { status: 412, json: { error: "precondition_failed" }, headers: { ETag: '"new"' } }),
    );
    const form = editor();
    for (let i = 0; i < 2; i++) {
      await expect(form.call(`${MANAGEMENT}/request-retry`, { method: "PATCH", json: { value: 2 } }))
        .rejects.toMatchObject({ status: 412, code: "precondition_failed" });
    }
    expect(api.callsTo("PUT", RETRY)).toHaveLength(2);
    expect(api.callsTo("PUT", RETRY).every((call) => call.headers.get("if-match") === '"base"')).toBe(true);
  });
});
