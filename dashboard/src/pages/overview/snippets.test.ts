import { describe, expect, it } from "vitest";

import type { ModelInfo, ProxyRoute } from "../../api/dashboard";
import { clientSetup } from "../../test/fixtures";
import {
  MODEL_PLACEHOLDER,
  addressOptions,
  buildSnippets,
  isLoopback,
  powerShellString,
  shellWord,
  suggestedModel,
  tomlString,
  type SetupInput,
} from "./snippets";

const BACKSLASH = String.fromCharCode(92);
const DEL = String.fromCharCode(0x7f);
const RIGHT_QUOTE = String.fromCharCode(0x2019);
const LOW_QUOTE = String.fromCharCode(0x201a);

function input(overrides: Partial<SetupInput> = {}): SetupInput {
  return {
    root: "http://127.0.0.1:8317",
    key: "sk-client",
    model: "gpt-5.1-codex",
    models: clientSetup().models,
    routes: clientSetup().routes,
    shell: "posix",
    ...overrides,
  };
}

function code(snippetId: string, setup: SetupInput): string {
  const snippet = buildSnippets(setup).find((candidate) => candidate.id === snippetId);
  if (snippet === undefined) {
    throw new Error(`no ${snippetId} setup`);
  }
  return snippet.parts.map((part) => part.code).join("\n");
}

describe("quoting", () => {
  it("writes TOML basic strings with DEL escaped", () => {
    expect(tomlString(`a"b${BACKSLASH}c`)).toBe(`"a${BACKSLASH}"b${BACKSLASH}${BACKSLASH}c"`);
    expect(tomlString(`x${DEL}`)).toBe(`"x${BACKSLASH}u007f"`);
  });

  it("writes POSIX shell words nothing in can break out of", () => {
    expect(shellWord("plain")).toBe("'plain'");
    expect(shellWord("it's $HOME")).toBe(`'it'${BACKSLASH}''s $HOME'`);
  });

  it("doubles every quote PowerShell ends a string at", () => {
    expect(powerShellString("it's $env:HOME")).toBe("'it''s $env:HOME'");
    expect(powerShellString(`a${RIGHT_QUOTE}b${LOW_QUOTE}`)).toBe(
      `'a${RIGHT_QUOTE}${RIGHT_QUOTE}b${LOW_QUOTE}${LOW_QUOTE}'`,
    );
  });
});

describe("addressOptions", () => {
  const bases = clientSetup().base_urls;

  it("puts the page's own address first, then the server's, each once", () => {
    expect(addressOptions("https://ferry.example.org", bases)).toEqual([
      { root: "https://ferry.example.org", label: "https://ferry.example.org (this page's address)" },
      { root: "http://127.0.0.1:8317", label: "http://127.0.0.1:8317 (the server's listen address)" },
      { root: "http://localhost:8317", label: "http://localhost:8317 (the server's listen address)" },
      { root: "https://proxy.example.com", label: "https://proxy.example.com (management.base-url)" },
    ]);
  });

  it("leaves out a server address the page is already at", () => {
    const options = addressOptions("http://127.0.0.1:8317", [
      { url: "http://127.0.0.1:8317/", source: "listen" },
      { url: "https://proxy.example.com/ferry/", source: "config" },
      { url: "https://proxy.example.com/ferry", source: "config" },
    ]);
    expect(options.map((option) => option.root)).toEqual([
      "http://127.0.0.1:8317",
      "https://proxy.example.com/ferry",
    ]);
  });

  it("offers only the server's addresses when the page is at the management address", () => {
    const listen = bases.filter((base) => base.source === "listen");
    expect(addressOptions("http://127.0.0.1:8318", listen, true)).toEqual([
      { root: "http://127.0.0.1:8317", label: "http://127.0.0.1:8317 (the server's listen address)" },
      { root: "http://localhost:8317", label: "http://localhost:8317 (the server's listen address)" },
    ]);
  });

  it("offers nothing at the management address when the server lists no address", () => {
    expect(addressOptions("http://127.0.0.1:8318", [], true)).toEqual([]);
  });
});

/** A model as the server describes it. */
function described(id: string, ownedBy: string, created: number | null, chat = true): ModelInfo {
  return {
    id,
    display_name: id,
    owned_by: ownedBy,
    providers: [ownedBy],
    created,
    chat,
    context_length: null,
    max_output_tokens: null,
  };
}

/** A route with `models` on it, in ID order as the server lists them. */
function routeWith(models: readonly ModelInfo[], protocol = "openai"): ProxyRoute {
  return {
    id: `${protocol}-route`,
    protocol,
    method: "POST",
    path: "/v1/chat/completions",
    base_path: "/v1",
    models: models.map((info) => info.id).sort(),
  };
}

const ANTHROPIC = { maker: "anthropic", family: "claude" };
const OPENAI = { maker: "openai", family: "gpt" };

/** A server with a Claude sign-in and a Codex Plus one, as the catalog dates their models. */
const SIGNED_IN: ModelInfo[] = [
  described("claude-3-5-haiku-20241022", "anthropic", 1_729_555_200),
  described("claude-opus-5-5", "anthropic", 1_790_035_200),
  described("claude-sonnet-4-5-20250929", "anthropic", 1_759_104_000),
  described("claude-sonnet-5-5", "anthropic", 1_790_553_600),
  described("gpt-5.5", "openai", 1_776_902_400),
  described("gpt-6-luna", "openai", 1_790_035_200),
  described("gpt-6.1-sol", "openai", 1_790_640_000),
];

describe("suggestedModel", () => {
  it("suggests the newest model, not the first by name", () => {
    const route = routeWith(SIGNED_IN);
    expect(route.models[0]).toBe("claude-3-5-haiku-20241022");
    expect(suggestedModel(route, SIGNED_IN)).toBe("gpt-6.1-sol");
  });

  it("suggests the maker's newest model to a client made for it", () => {
    const route = routeWith(SIGNED_IN);
    expect(suggestedModel(route, SIGNED_IN, ANTHROPIC)).toBe("claude-sonnet-5-5");
    expect(suggestedModel(route, SIGNED_IN, OPENAI)).toBe("gpt-6.1-sol");
  });

  it("knows the maker's models by their names too, after any prefix", () => {
    const models = [
      described("claude-opus-4-6-thinking", "antigravity", null),
      described("team/claude-sonnet-4-6", "antigravity", null),
      described("gemini-3.6-flash", "google", 1_782_864_000),
    ];
    const route = routeWith(models, "claude");
    expect(suggestedModel(route, models, ANTHROPIC)).toBe("claude-opus-4-6-thinking");
    expect(suggestedModel(routeWith(models.slice(1), "claude"), models, ANTHROPIC)).toBe(
      "team/claude-sonnet-4-6",
    );
    // Without one of the maker's models, the newest of any maker.
    expect(suggestedModel(route, models, OPENAI)).toBe("gemini-3.6-flash");
  });

  it("suggests chat models only", () => {
    const models = [
      described("gemini-2.5-pro", "google", 1_750_000_000),
      described("gemini-3-pro-image-preview", "google", 1_771_459_200, false),
      described("grok-imagine-video", "xai", 1_790_000_000, false),
      described("imagen-4.0-generate-001", "google", 1_790_000_000, false),
    ];
    expect(suggestedModel(routeWith(models), models)).toBe("gemini-2.5-pro");
  });

  it("puts a model of unknown date last, and of the same date the first on the route", () => {
    const models = [
      described("a-undated", "acme", null),
      described("b-dated", "acme", 1_700_000_000),
      described("c-dated", "acme", 1_700_000_000),
    ];
    const route = routeWith(models);
    expect(suggestedModel(route, models)).toBe("b-dated");
    // The order the models are described in doesn't matter.
    expect(suggestedModel(route, [...models].reverse())).toBe("b-dated");
    const undated = [described("x-model", "acme", null), described("y-model", "acme", null)];
    expect(suggestedModel(routeWith(undated), undated)).toBe("x-model");
  });

  it("falls back to the route's first model, then to none", () => {
    const images = [
      described("imagen-3.0-generate-002", "google", 1_740_000_000, false),
      described("imagen-4.0-generate-001", "google", 1_750_000_000, false),
    ];
    expect(suggestedModel(routeWith(images), images)).toBe("imagen-3.0-generate-002");
    // A model the server doesn't describe isn't known to be a chat model.
    expect(suggestedModel(routeWith(images), [])).toBe("imagen-3.0-generate-002");
    expect(suggestedModel(routeWith([]), SIGNED_IN)).toBeUndefined();
  });
});

describe("isLoopback", () => {
  it("knows the addresses that reach only this computer", () => {
    expect(isLoopback("http://127.0.0.1:8317")).toBe(true);
    expect(isLoopback("http://127.1.2.3")).toBe(true);
    expect(isLoopback("http://localhost:3000")).toBe(true);
    expect(isLoopback("http://[::1]:8317")).toBe(true);
    expect(isLoopback("http://192.168.1.20:8317")).toBe(false);
    expect(isLoopback("https://proxy.example.com")).toBe(false);
    expect(isLoopback("not a url")).toBe(false);
  });
});

describe("buildSnippets", () => {
  it("offers each setup whose route the server has, at that route's base", () => {
    const snippets = buildSnippets(input());
    expect(snippets.map((snippet) => snippet.id)).toEqual([
      "openai-python",
      "openai-node",
      "codex",
      "claude-code",
      "curl",
    ]);
    expect(code("openai-python", input())).toContain(`base_url="http://127.0.0.1:8317/v1",`);
    expect(code("openai-node", input())).toContain(`baseURL: "http://127.0.0.1:8317/v1",`);
    expect(code("codex", input())).toContain(`base_url = "http://127.0.0.1:8317/v1"`);
    expect(code("claude-code", input())).toContain(
      "export ANTHROPIC_BASE_URL='http://127.0.0.1:8317'",
    );
  });

  it("leaves out a setup whose route the server doesn't have", () => {
    const routes = clientSetup().routes.filter((route) => route.protocol === "claude");
    expect(buildSnippets(input({ routes })).map((snippet) => snippet.id)).toEqual(["claude-code"]);
    expect(buildSnippets(input({ routes: [] }))).toEqual([]);
  });

  it("names the model picked, or the route's suggestion when it hasn't that one", () => {
    const routes: ProxyRoute[] = clientSetup().routes.map((route) =>
      route.protocol === "claude" ? { ...route, models: ["claude-sonnet-4-5"] } : route,
    );
    const snippets = buildSnippets(input({ routes, model: "gpt-5.1-codex" }));
    const claude = snippets.find((snippet) => snippet.id === "claude-code");
    const python = snippets.find((snippet) => snippet.id === "openai-python");
    expect(claude).toMatchObject({ model: "claude-sonnet-4-5", modelMissing: true });
    expect(python).toMatchObject({ model: "gpt-5.1-codex", modelMissing: false });
    expect(code("claude-code", input({ routes }))).toContain(
      "export ANTHROPIC_MODEL='claude-sonnet-4-5'",
    );
  });

  it("gives each setup its suggestion while no model is picked", () => {
    const routes = clientSetup().routes.map((route) => ({
      ...route,
      models: SIGNED_IN.map((info) => info.id),
    }));
    const snippets = buildSnippets(input({ routes, models: SIGNED_IN, model: null }));
    expect(snippets.map(({ id, model, modelMissing }) => ({ id, model, modelMissing }))).toEqual([
      { id: "openai-python", model: "gpt-6.1-sol", modelMissing: false },
      { id: "openai-node", model: "gpt-6.1-sol", modelMissing: false },
      { id: "codex", model: "gpt-6.1-sol", modelMissing: false },
      { id: "claude-code", model: "claude-sonnet-5-5", modelMissing: false },
      { id: "curl", model: "gpt-6.1-sol", modelMissing: false },
    ]);

    const empty = clientSetup().routes.map((route) => ({ ...route, models: [] }));
    for (const snippet of buildSnippets(input({ routes: empty, models: [], model: null }))) {
      expect(snippet).toMatchObject({ model: MODEL_PLACEHOLDER, modelMissing: false });
    }
  });

  it("quotes the key for where it goes", () => {
    const key = `sk-"it's"`;
    expect(code("openai-python", input({ key }))).toContain(`api_key="sk-${BACKSLASH}"it's${BACKSLASH}"",`);
    expect(code("claude-code", input({ key }))).toContain(
      `export ANTHROPIC_AUTH_TOKEN='sk-"it'${BACKSLASH}''s"'`,
    );
    expect(code("claude-code", input({ key, shell: "powershell" }))).toContain(
      `$env:ANTHROPIC_AUTH_TOKEN = 'sk-"it''s"'`,
    );
  });

  it("writes curl for POSIX shells and Invoke-RestMethod for PowerShell", () => {
    const posix = code("curl", input());
    expect(posix).toContain("curl 'http://127.0.0.1:8317/v1/chat/completions'");
    expect(posix).toContain("-H 'Authorization: Bearer sk-client'");
    expect(posix).toContain(`"model":"gpt-5.1-codex"`);

    const powershell = code("curl", input({ shell: "powershell" }));
    expect(powershell).toContain(
      "Invoke-RestMethod -Method Post -Uri 'http://127.0.0.1:8317/v1/chat/completions'",
    );
    expect(powershell).toContain("Authorization = 'Bearer sk-client'");
  });

  it("sets Codex up with the key in an environment variable, for either shell", () => {
    const posix = buildSnippets(input()).find((snippet) => snippet.id === "codex");
    expect(posix?.parts).toHaveLength(2);
    expect(posix?.parts[0]?.code).toContain(`env_key = "OPEN_FERRY_API_KEY"`);
    expect(posix?.parts[0]?.code).toContain(`wire_api = "responses"`);
    expect(posix?.parts[0]?.code).not.toContain("sk-client");
    expect(posix?.parts[1]?.code).toBe("export OPEN_FERRY_API_KEY='sk-client'\ncodex");
    expect(code("codex", input({ shell: "powershell" }))).toContain(
      "$env:OPEN_FERRY_API_KEY = 'sk-client'\ncodex",
    );
  });

  it("names the documentation each setup follows in it", () => {
    for (const snippet of buildSnippets(input())) {
      expect(snippet.source).toMatch(/^https:\/\//);
      expect(snippet.parts[0]?.code).toContain(snippet.source);
    }
  });
});
