// Ready-made client setups: each a few lines to paste, with the address,
// client key and model filled in, quoted for where they go. Each names the
// documentation it follows in a comment.

import type { BaseUrl, ProxyRoute } from "../../api/dashboard";

export type Shell = "posix" | "powershell";

export const SHELL_LABELS: Record<Shell, string> = {
  posix: "bash or zsh",
  powershell: "PowerShell",
};

/** What every setup is built from. */
export interface SetupInput {
  /** The server's root, such as http://127.0.0.1:8317, without a trailing slash. */
  root: string;
  /** The client key as it should appear: the key, masked or not, or a placeholder. */
  key: string;
  /** The model asked for; a route that hasn't it uses its first. */
  model: string;
  routes: readonly ProxyRoute[];
  shell: Shell;
}

export interface SnippetPart {
  /** What to do with it, such as "Add to ~/.codex/config.toml". */
  caption: string;
  code: string;
}

export interface Snippet {
  id: string;
  label: string;
  /** The route it calls. */
  route: ProxyRoute;
  /** The model it names. */
  model: string;
  /** The model asked for isn't on this route right now. */
  modelMissing: boolean;
  parts: SnippetPart[];
  /** The documentation it follows. */
  source: string;
}

// ---------------------------------------------------------------- quoting

/** A double-quoted string for JavaScript, Python or JSON. */
function quoted(value: string): string {
  return JSON.stringify(value);
}

/** A TOML basic string: JSON's escapes, and DEL escaped too. */
export function tomlString(value: string): string {
  return JSON.stringify(value).replaceAll("\x7f", "\\u007f");
}

/** A single-quoted POSIX shell word: nothing in it is special. */
export function shellWord(value: string): string {
  return `'${value.replaceAll("'", `'\\''`)}'`;
}

/** A single-quoted PowerShell string. PowerShell takes the curly single
 * quotes as quotes too, so they are doubled as well. */
export function powerShellString(value: string): string {
  return `'${value.replace(/['\u2018\u2019\u201a\u201b]/g, (quote) => quote + quote)}'`;
}

function shellString(value: string, shell: Shell): string {
  return shell === "posix" ? shellWord(value) : powerShellString(value);
}

function setEnv(name: string, value: string, shell: Shell): string {
  return shell === "posix"
    ? `export ${name}=${shellWord(value)}`
    : `$env:${name} = ${powerShellString(value)}`;
}

// --------------------------------------------------------------- addresses

export interface AddressOption {
  root: string;
  label: string;
}

/** A root without its trailing slash, for comparing and joining. */
function trimRoot(url: string): string {
  return url.replace(/\/+$/, "");
}

function sameRoot(a: string, b: string): boolean {
  try {
    const left = new URL(a);
    const right = new URL(b);
    return left.origin === right.origin && trimRoot(left.pathname) === trimRoot(right.pathname);
  } catch {
    return trimRoot(a) === trimRoot(b);
  }
}

/**
 * The addresses a client might reach the server at: this page's own origin
 * first (it may be a proxy in front of the server), then the server's, each
 * once.
 */
export function addressOptions(origin: string, baseUrls: readonly BaseUrl[]): AddressOption[] {
  const options: AddressOption[] = [{ root: trimRoot(origin), label: `${origin} (this page's address)` }];
  for (const base of baseUrls) {
    if (options.some((option) => sameRoot(option.root, base.url))) {
      continue;
    }
    const where = base.source === "config" ? "remote-management.base-url" : "the server's listen address";
    options.push({ root: trimRoot(base.url), label: `${trimRoot(base.url)} (${where})` });
  }
  return options;
}

/** Whether `root` reaches only the computer it is on. */
export function isLoopback(root: string): boolean {
  try {
    const host = new URL(root).hostname;
    return host === "localhost" || host === "[::1]" || host.startsWith("127.");
  } catch {
    return false;
  }
}

// ------------------------------------------------------------------ setups

const PROMPT = "Say hello.";

function routeFor(routes: readonly ProxyRoute[], protocol: string): ProxyRoute | undefined {
  return routes.find((route) => route.protocol === protocol);
}

function modelFor(route: ProxyRoute, wanted: string): { model: string; missing: boolean } {
  if (route.models.includes(wanted)) {
    return { model: wanted, missing: false };
  }
  return { model: route.models[0] ?? wanted, missing: true };
}

function chatBody(model: string): string {
  return JSON.stringify({ model, messages: [{ role: "user", content: PROMPT }] });
}

type Builder = (input: SetupInput, base: string, model: string) => Pick<Snippet, "parts" | "source">;

const OPENAI_PYTHON: Builder = ({ key }, base, model) => {
  const source = "https://github.com/openai/openai-python#usage";
  return {
    source,
    parts: [
      {
        caption: "Python, with the openai package installed",
        code: [
          `# The OpenAI Python SDK, pointed at open-ferry: ${source}`,
          "from openai import OpenAI",
          "",
          "client = OpenAI(",
          `    base_url=${quoted(base)},`,
          `    api_key=${quoted(key)},`,
          ")",
          "completion = client.chat.completions.create(",
          `    model=${quoted(model)},`,
          `    messages=[{"role": "user", "content": ${quoted(PROMPT)}}],`,
          ")",
          "print(completion.choices[0].message.content)",
        ].join("\n"),
      },
    ],
  };
};

const OPENAI_NODE: Builder = ({ key }, base, model) => {
  const source = "https://github.com/openai/openai-node#usage";
  return {
    source,
    parts: [
      {
        caption: "JavaScript (an ES module), with the openai package installed",
        code: [
          `// The OpenAI Node SDK, pointed at open-ferry: ${source}`,
          `import OpenAI from "openai";`,
          "",
          "const client = new OpenAI({",
          `  baseURL: ${quoted(base)},`,
          `  apiKey: ${quoted(key)},`,
          "});",
          "const completion = await client.chat.completions.create({",
          `  model: ${quoted(model)},`,
          `  messages: [{ role: "user", content: ${quoted(PROMPT)} }],`,
          "});",
          "console.log(completion.choices[0].message.content);",
        ].join("\n"),
      },
    ],
  };
};

/** The environment variable the Codex setup reads the key from. */
export const CODEX_KEY_VARIABLE = "OPEN_FERRY_API_KEY";

const CODEX: Builder = ({ key, shell }, base, model) => {
  const source = "https://github.com/openai/codex/blob/main/docs/config.md#model_providers";
  return {
    source,
    parts: [
      {
        caption: "Add to ~/.codex/config.toml. The first two lines go above any [table] in it.",
        code: [
          `# Codex CLI through open-ferry: ${source}`,
          `model = ${tomlString(model)}`,
          `model_provider = "open-ferry"`,
          "",
          "[model_providers.open-ferry]",
          `name = "open-ferry"`,
          `base_url = ${tomlString(base)}`,
          `env_key = "${CODEX_KEY_VARIABLE}"`,
          `wire_api = "responses"`,
        ].join("\n"),
      },
      {
        caption: `Then start Codex with the key in ${CODEX_KEY_VARIABLE}`,
        code: [setEnv(CODEX_KEY_VARIABLE, key, shell), "codex"].join("\n"),
      },
    ],
  };
};

const CLAUDE_CODE: Builder = ({ key, shell }, base, model) => {
  const source = "https://docs.claude.com/en/docs/claude-code/llm-gateway";
  return {
    source,
    parts: [
      {
        caption: "Start Claude Code with these set",
        code: [
          `# Claude Code through open-ferry, as an LLM gateway: ${source}`,
          setEnv("ANTHROPIC_BASE_URL", base, shell),
          setEnv("ANTHROPIC_AUTH_TOKEN", key, shell),
          setEnv("ANTHROPIC_MODEL", model, shell),
          "claude",
        ].join("\n"),
      },
    ],
  };
};

const CURL: Builder = ({ key, shell }, base, model) => {
  const source = "https://platform.openai.com/docs/api-reference/chat/create";
  const url = `${base}/chat/completions`;
  const code =
    shell === "posix"
      ? [
          `# A chat completion, as OpenAI's API takes it: ${source}`,
          `curl ${shellWord(url)} \\`,
          `  -H ${shellWord(`Authorization: Bearer ${key}`)} \\`,
          `  -H 'Content-Type: application/json' \\`,
          `  -d ${shellWord(chatBody(model))}`,
        ]
      : [
          `# A chat completion, as OpenAI's API takes it: ${source}`,
          `Invoke-RestMethod -Method Post -Uri ${powerShellString(url)} \``,
          `  -Headers @{ Authorization = ${powerShellString(`Bearer ${key}`)} } \``,
          `  -ContentType 'application/json' \``,
          `  -Body ${shellString(chatBody(model), shell)}`,
        ];
  return { source, parts: [{ caption: "Run in a terminal", code: code.join("\n") }] };
};

const SETUPS: readonly { id: string; label: string; protocol: string; build: Builder }[] = [
  { id: "openai-python", label: "OpenAI SDK (Python)", protocol: "openai", build: OPENAI_PYTHON },
  { id: "openai-node", label: "OpenAI SDK (Node)", protocol: "openai", build: OPENAI_NODE },
  { id: "codex", label: "Codex CLI", protocol: "openai-responses", build: CODEX },
  { id: "claude-code", label: "Claude Code", protocol: "claude", build: CLAUDE_CODE },
  { id: "curl", label: "curl", protocol: "openai", build: CURL },
];

/** The setups the server's routes allow, built from `input`. */
export function buildSnippets(input: SetupInput): Snippet[] {
  const snippets: Snippet[] = [];
  for (const setup of SETUPS) {
    const route = routeFor(input.routes, setup.protocol);
    if (route === undefined) {
      continue;
    }
    const { model, missing } = modelFor(route, input.model);
    const base = `${input.root}${route.base_path}`;
    snippets.push({
      id: setup.id,
      label: setup.label,
      route,
      model,
      modelMissing: missing,
      ...setup.build(input, base, model),
    });
  }
  return snippets;
}
