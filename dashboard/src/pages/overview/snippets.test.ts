import { describe, expect, it } from "vitest";

import type { ProxyRoute } from "../../api/dashboard";
import { clientSetup } from "../../test/fixtures";
import {
  addressOptions,
  buildSnippets,
  isLoopback,
  powerShellString,
  shellWord,
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
      { root: "https://proxy.example.com", label: "https://proxy.example.com (remote-management.base-url)" },
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

  it("names the model asked for, or the route's first when it hasn't that one", () => {
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
