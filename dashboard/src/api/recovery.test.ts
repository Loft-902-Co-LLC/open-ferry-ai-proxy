import { describe, expect, it } from "vitest";
import { mockApi, route } from "../test/mockApi";
import { apiRequest } from "./client";
import { ConfigEditor } from "./configEditor";
import { CONFIG_YAML, V8_CONFIG } from "./management";

describe("recovery read and write transitions", () => {
  function state() {
    let broken = true;
    const api = mockApi(
      route("GET", V8_CONFIG, () => broken
        ? { status: 500, json: { error: "invalid_config", message: "file is empty" } }
        : { json: {}, headers: { ETag: '"repaired"' } }),
      route("GET", CONFIG_YAML, { text: "# raw recovery text" }),
      route("GET", "/v8/management/config.yaml", { text: "# versioned text", headers: { ETag: '"repaired"' } }),
      route("PUT", CONFIG_YAML, { json: {} }),
      route("PUT", "/v8/management/config.yaml", { json: {}, headers: { ETag: '"saved"' } }),
    );
    const editor = new ConfigEditor(<T,>(path: string, request = {}) => apiRequest<T>("unit-key", path, request));
    return { api, editor, repair: () => { broken = false; } };
  }
  it("leaves recovery on the next successful versioned GET and guards the next PUT", async () => {
    const { api, editor, repair } = state();
    await editor.call(CONFIG_YAML);
    expect(editor.recovery).not.toBeNull();
    repair();
    expect(await editor.call(CONFIG_YAML)).toBe("# versioned text");
    expect(editor.recovery).toBeNull();
    editor.setDirty(true);
    await editor.call(CONFIG_YAML, { method: "PUT", body: "# next draft" });
    expect(api.callsTo("PUT", CONFIG_YAML)).toEqual([]);
    expect(api.callsTo("PUT", "/v8/management/config.yaml")[0]?.headers.get("if-match")).toBe('"repaired"');
  });
  it("re-probes a raw draft and refuses raw PUT once another client repairs the file", async () => {
    const { api, editor, repair } = state();
    await editor.call(CONFIG_YAML);
    editor.setDirty(true);
    repair();
    await expect(editor.call(CONFIG_YAML, { method: "PUT", body: "# stale raw draft" }))
      .rejects.toThrow("The file can be read again. Reload to edit it with version checks.");
    expect(api.calls.filter((call) => call.method !== "GET")).toEqual([]);
  });
});
