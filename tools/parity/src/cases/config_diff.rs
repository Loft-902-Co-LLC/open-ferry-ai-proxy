//! Hand-written cases for config change details: the scenarios of
//! upstream's diff tests, written as the YAML a reload reads, with every
//! setting open-ferry types, the clean-ups parsing applies before the
//! diff, and the URL forms the lines redact.

use serde_json::json;

use super::Case;

fn case(name: &str, old: &str, new: &str) -> Case {
    Case::new(name, "", "").with_options(json!({ "old": old, "new": new }))
}

/// A config with every typed section set, for the no-change case and as a
/// base for others.
const FULL: &str = r#"port: 8317
auth-dir: "~/.cli-proxy-api"
debug: true
logging-to-file: true
usage-statistics-enabled: true
redis-usage-queue-retention-seconds: 120
disable-cooling: true
save-cooldown-status: true
transient-error-cooldown-seconds: 30
disable-image-generation: "chat"
request-log: true
logs-max-total-size-mb: 256
error-logs-max-files: 4
request-retry: 2
max-retry-credentials: 3
max-retry-interval: 20
proxy-url: "http://user:secret@proxy.example:3128/path"
ws-auth: false
force-model-prefix: true
nonstream-keepalive-interval: 10
quota-exceeded:
  switch-project: true
  switch-preview-model: true
  antigravity-credits: true
codex:
  stream-bootstrap-buffering: true
  stream-bootstrap-timeout: "15s"
  orphan-delegation-compatibility: true
xai:
  inject-x-search: true
client:
  codex:
    optimize-multi-agent-v2: true
    enable-apply-patch: true
routing:
  strategy: "fill-first"
api-keys:
  - "sk-client-1"
  - "sk-client-2"
payload:
  default:
    - models:
        - name: "gpt-*"
          protocol: "codex"
      params:
        "reasoning.effort": "high"
        "max_output_tokens": 4096
  default-raw:
    - models:
        - name: "gemini-*"
      params:
        "generationConfig.thinkingConfig": "{\"thinkingBudget\": 1024}"
  override:
    - models:
        - name: "*"
      params:
        "store": false
  override-raw:
    - models:
        - name: "claude-*"
      params:
        "metadata": "{\"user_id\": \"parity\"}"
  filter:
    - models:
        - name: "gpt-*"
      params:
        - "service_tier"
gemini-api-key:
  - api-key: "gemini-key-1"
    prefix: "team-g"
    base-url: "https://generativelanguage.example/v1beta"
    proxy-url: "socks5://gp.example:1080"
    headers:
      X-Team: "a"
    models:
      - name: "gemini-2.5-pro"
        alias: "g-pro"
        thinking:
          min: 128
          max: 32768
          dynamic-allowed: true
    excluded-models:
      - "gemini-1.5-*"
    disable-cooling: false
    request-retry: 1
interactions-api-key:
  - api-key: "interactions-key-1"
    prefix: "team-i"
    base-url: "https://interactions.example/v1beta"
    headers:
      X-Team: "i"
    models:
      - name: "gemini-2.5-pro"
        alias: "i-pro"
    disable-cooling: true
claude-api-key:
  - api-key: "claude-key-1"
    base-url: "https://api.anthropic.example"
    proxy-url: ""
    models:
      - name: "claude-sonnet-4-6"
        alias: "sonnet"
    excluded-models:
      - "claude-2*"
    rebuild-mid-system-message: true
codex-api-key:
  - api-key: "codex-key-1"
    base-url: "https://codex.example/backend-api/codex"
    proxy-url: ""
    websockets: true
    alpha-search: true
    models:
      - name: "gpt-6-sol"
        alias: "sol"
        force-mapping: true
xai-api-key:
  - api-key: "xai-key-1"
    priority: 2
    prefix: "team-x"
    base-url: "https://api.x.example/v1"
    proxy-url: "http://user:secret@xp.example:3128"
    websockets: true
    models:
      - name: "grok-4.5"
        alias: "grok"
        force-mapping: true
    excluded-models:
      - "grok-2*"
    request-retry: 1
meta-api-key:
  - api-key: "meta-key-1"
    priority: 1
    prefix: " team-m "
    headers:
      X-Team: "m"
    models:
      - name: "muse"
        alias: "m"
    disable-cooling: false
vertex-api-key:
  - api-key: "vertex-key-1"
    base-url: "https://vertex.example"
    models:
      - name: "gemini-2.5-flash"
        alias: "flash"
oauth-excluded-models:
  codex:
    - "gpt-5.5-mini"
oauth-model-alias:
  antigravity:
    - name: "claude-opus-4-6-thinking"
      alias: "opus"
      display-name: "Opus"
oauth-request-scoped-errors:
  vertex:
    - status: 400
      match:
        - "context_length"
      action: "stop"
oauth-settings:
  codex:
    - name: "gpt-6-sol"
      max-context-length: 272000
remote-management:
  allow-remote: true
  secret-key: "$2a$10$abcdefghijklmnopqrstuv"
  disable-control-panel: false
  panel-github-repository: "https://github.com/example/panel"
  base-url: "https://panel.example"
openai-compatibility:
  - name: "compat-a"
    base-url: "https://compat-a.example/v1"
    api-key-entries:
      - api-key: "compat-key-1"
    models:
      - name: "m1"
        alias: "a1"
    headers:
      X-Compat: "1"
"#;

/// The hand-written cases for `config-diff/details`.
pub fn details() -> Vec<Case> {
    let mut cases = vec![
        case("no-changes", FULL, FULL),
        case("everything-removed", FULL, "port: 8317\n"),
        case("everything-added", "port: 8317\n", FULL),
        // Ports TestBuildConfigChangeDetails_FlagsAndKeys' scalars.
        case(
            "scalars",
            r#"port: 8080
auth-dir: "~/.cli-proxy-api"
request-retry: 1
max-retry-credentials: 1
max-retry-interval: 1
ws-auth: false
proxy-url: "http://old-proxy"
api-keys:
  - "key-1"
"#,
            r#"port: 9090
auth-dir: " /new/auth "
debug: true
logging-to-file: true
usage-statistics-enabled: true
redis-usage-queue-retention-seconds: 120
disable-cooling: true
save-cooldown-status: true
transient-error-cooldown-seconds: -1
disable-image-generation: true
request-log: true
logs-max-total-size-mb: 512
error-logs-max-files: 3
request-retry: 2
max-retry-credentials: 3
max-retry-interval: 30
proxy-url: "socks5://user:pass@proxy.example:1080/path?x=1"
ws-auth: true
force-model-prefix: true
nonstream-keepalive-interval: 5
api-keys:
  - " key-1 "
  - "key-2"
"#,
        ),
        // Parsing clamps these before the diff sees them.
        case(
            "clamped-scalars",
            "port: 1\n",
            r#"port: 1
redis-usage-queue-retention-seconds: 5000
logs-max-total-size-mb: -3
error-logs-max-files: -1
max-retry-credentials: -2
"#,
        ),
        case(
            "quota-codex-client-routing",
            r#"codex:
  stream-bootstrap-timeout: " 5s "
routing:
  strategy: "round-robin"
"#,
            r#"quota-exceeded:
  switch-project: true
  switch-preview-model: true
  antigravity-credits: true
codex:
  stream-bootstrap-buffering: true
  stream-bootstrap-timeout: "5s"
  orphan-delegation-compatibility: true
client:
  codex:
    optimize-multi-agent-v2: true
    enable-apply-patch: true
routing:
  strategy: " Fill-First "
"#,
        ),
        case(
            "codex-stream-bootstrap-timeout",
            "codex:\n  stream-bootstrap-timeout: \"5s\"\n",
            "codex:\n  stream-bootstrap-timeout: \"10s\"\n",
        ),
        // Params in another order are the same map.
        case(
            "payload-params-reordered",
            r#"payload:
  default:
    - models:
        - name: "gpt-*"
      params:
        "a": 1
        "b": "two"
"#,
            r#"payload:
  default:
    - models:
        - name: "gpt-*"
      params:
        "b": "two"
        "a": 1
"#,
        ),
        case(
            "payload-sections",
            r#"payload:
  default:
    - models:
        - name: "gpt-*"
      params:
        "a": 1
  override-raw:
    - models:
        - name: "claude-*"
      params:
        "metadata": "{}"
  filter:
    - models:
        - name: "gpt-*"
      params:
        - "x"
"#,
            r#"payload:
  default:
    - models:
        - name: "gpt-*"
      params:
        "a": 1.0
  default-raw:
    - models:
        - name: "gemini-*"
      params:
        "raw": "[1, 2]"
  override:
    - models:
        - name: "*"
          protocol: "openai"
      params:
        "nested":
          "list":
            - 1
            - "two"
            - null
    - models:
        - name: "*"
      params:
        "flag": true
  filter:
    - models:
        - name: "gpt-*"
      params:
        - "y"
"#,
        ),
        // Raw rules with invalid JSON are dropped while parsing, so only the
        // valid rule is compared.
        case(
            "payload-raw-dropped",
            r#"payload:
  default-raw:
    - models:
        - name: "gemini-*"
      params:
        "ok": "{\"a\": 1}"
    - models:
        - name: "gemini-*"
      params:
        "bad": "{not json"
"#,
            r#"payload:
  default-raw:
    - models:
        - name: "gemini-*"
      params:
        "ok": "{\"a\": 1}"
"#,
        ),
        case(
            "payload-model-rule-fields",
            r#"payload:
  override:
    - models:
        - name: "gpt-*"
          headers:
            X-Tier: "pro*"
          from-protocol: "openai"
          match:
            - "model": "gpt-6"
          exist:
            - "tools"
      params:
        "store": false
"#,
            r#"payload:
  override:
    - models:
        - name: "gpt-*"
          headers:
            X-Tier: "pro*"
          from-protocol: "openai"
          match:
            - "model": "gpt-6"
          not-exist:
            - "tools"
      params:
        "store": false
"#,
        ),
        case(
            "api-keys-trimmed-equal",
            "api-keys:\n  - \" a \"\n  - \"b\"\n",
            "api-keys:\n  - \"a\"\n  - \"b \"\n",
        ),
        case(
            "api-keys-values-updated",
            "api-keys:\n  - \"a\"\n  - \"b\"\n",
            "api-keys:\n  - \"a\"\n  - \"c\"\n",
        ),
        // Ports TestBuildConfigChangeDetails_AllBranches' keys.
        case(
            "provider-key-fields",
            r#"gemini-api-key:
  - api-key: "g-old"
    base-url: "http://g-old"
    proxy-url: "http://gp-old"
    headers:
      A: "1"
claude-api-key:
  - api-key: "c-old"
    base-url: "http://c-old"
    proxy-url: "http://cp-old"
    headers:
      H: "1"
    excluded-models:
      - "x"
codex-api-key:
  - api-key: "x-old"
    base-url: "http://x-old"
    proxy-url: "http://xp-old"
    headers:
      H: "1"
    excluded-models:
      - "x"
vertex-api-key:
  - api-key: "v-old"
    base-url: "http://v-old"
    proxy-url: "http://vp-old"
    headers:
      H: "1"
    models:
      - name: "m1"
        alias: "a1"
"#,
            r#"gemini-api-key:
  - api-key: "g-new"
    prefix: "team"
    base-url: "http://g-new"
    proxy-url: "http://gp-new"
    headers:
      A: "2"
    excluded-models:
      - "x"
      - "y"
    disable-cooling: false
    request-retry: 2
claude-api-key:
  - api-key: "c-new"
    prefix: "/team/"
    base-url: "http://c-new"
    proxy-url: "http://cp-new"
    headers:
      H: "2"
    excluded-models:
      - "x"
      - "y"
    rebuild-mid-system-message: true
    disable-cooling: true
codex-api-key:
  - api-key: "x-new"
    base-url: "http://x-new"
    proxy-url: "http://xp-new"
    websockets: true
    alpha-search: true
    headers:
      H: "2"
    excluded-models:
      - "x"
      - "y"
    request-retry: 0
vertex-api-key:
  - api-key: "v-new"
    prefix: "vx"
    base-url: "http://v-new"
    proxy-url: "http://vp-new"
    headers:
      H: "2"
    models:
      - name: "m1"
        alias: "a1"
      - name: "m2"
        alias: "a2"
    excluded-models:
      - "z"
    disable-cooling: true
    request-retry: 3
"#,
        ),
        case(
            "provider-key-models",
            r#"gemini-api-key:
  - api-key: "g"
    models:
      - name: "Gemini-2.5-Pro"
        alias: "Pro"
      - name: "gemini-2.5-flash"
        alias: "flash"
claude-api-key:
  - api-key: "c"
    base-url: ""
    proxy-url: ""
    models:
      - name: "claude-sonnet-4-6"
        alias: "sonnet"
        thinking:
          levels:
            - "low"
            - "<high>"
codex-api-key:
  - api-key: "x"
    base-url: "https://codex.example"
    proxy-url: ""
    models:
      - name: "gpt-6-sol"
        alias: "sol"
vertex-api-key:
  - api-key: "v"
    base-url: "https://vertex.example"
    models:
      - name: "m"
        alias: "a"
        display-name: "A"
"#,
            r#"gemini-api-key:
  - api-key: "g"
    models:
      - name: "gemini-2.5-flash"
        alias: "FLASH"
      - name: "gemini-2.5-pro"
        alias: "pro"
      - name: "gemini-2.5-pro"
        alias: "pro"
claude-api-key:
  - api-key: "c"
    base-url: ""
    proxy-url: ""
    models:
      - name: "claude-sonnet-4-6"
        alias: "sonnet"
        thinking:
          levels:
            - "low"
            - "high"
codex-api-key:
  - api-key: "x"
    base-url: "https://codex.example"
    proxy-url: ""
    models:
      - name: "gpt-6-sol"
        alias: "sol"
        force-mapping: true
vertex-api-key:
  - api-key: "v"
    base-url: "https://vertex.example"
    models:
      - name: "m"
        alias: "a"
        display-name: "A"
      - name: "m"
        alias: "a"
        display-name: "A"
"#,
        ),
        // Ports TestBuildConfigChangeDetails_XAIKeys.
        case(
            "xai-keys",
            r#"xai-api-key:
  - api-key: "old-key"
    priority: 1
    prefix: "old"
    base-url: "https://old.example.com/v1"
    proxy-url: "http://old-proxy"
    websockets: false
    disable-cooling: false
    request-retry: 1
    headers:
      X-Test: "old"
    models:
      - name: "grok-old"
        alias: "grok"
    excluded-models:
      - "grok-hidden"
"#,
            r#"xai-api-key:
  - api-key: "new-key"
    priority: 2
    prefix: "new"
    base-url: "https://new.example.com/v1"
    proxy-url: "http://new-proxy"
    websockets: true
    disable-cooling: true
    request-retry: 0
    headers:
      X-Test: "new"
    models:
      - name: "grok-new"
        alias: "grok"
    excluded-models:
      - "grok-other"
"#,
        ),
        // Ports TestBuildConfigChangeDetails_XAIForceMappingOnly.
        case(
            "xai-force-mapping-only",
            r#"xai-api-key:
  - api-key: "xai-key"
    base-url: "https://api.x.ai/v1"
    models:
      - name: "grok-4.5"
        alias: "grok-latest"
"#,
            r#"xai-api-key:
  - api-key: "xai-key"
    base-url: "https://api.x.ai/v1"
    models:
      - name: "grok-4.5"
        alias: "grok-latest"
        force-mapping: true
"#,
        ),
        // Meta's prefix shows as written; it has no websockets line, and
        // parsing gives it a default base URL and clears xAI's alpha search.
        case(
            "interactions-meta-key-fields",
            r#"interactions-api-key:
  - api-key: "i-old"
    base-url: "https://user:pass@i-old.example/v1beta?key=secret"
    prefix: " team "
meta-api-key:
  - api-key: "m-old"
    prefix: " team "
xai-api-key:
  - api-key: "x"
    base-url: "https://api.x.example/v1"
"#,
            r#"interactions-api-key:
  - api-key: "i-new"
    base-url: "https://i-new.example/v1beta"
    prefix: "/team/"
    disable-cooling: false
    request-retry: 2
meta-api-key:
  - api-key: "m-old"
    base-url: "https://api.meta.ai/v1"
    prefix: "team"
    websockets: true
    alpha-search: true
    priority: 3
xai-api-key:
  - api-key: "x"
    base-url: "https://api.x.example/v1"
    alpha-search: true
"#,
        ),
        case(
            "meta-prefix-padded",
            r#"meta-api-key:
  - api-key: "m"
    prefix: "a"
"#,
            r#"meta-api-key:
  - api-key: "m"
    prefix: " b "
"#,
        ),
        case(
            "xai-inject-x-search",
            r#"xai:
  inject-x-search: true
"#,
            r#"port: 8317
"#,
        ),
        // Ports TestBuildConfigChangeDetails_CountBranches.
        case(
            "provider-key-counts",
            "port: 1\n",
            r#"gemini-api-key:
  - api-key: "g"
interactions-api-key:
  - api-key: "i"
claude-api-key:
  - api-key: "c"
    base-url: ""
    proxy-url: ""
codex-api-key:
  - api-key: "x"
    base-url: "https://codex.example"
    proxy-url: ""
xai-api-key:
  - api-key: "x"
    base-url: "https://api.x.example/v1"
meta-api-key:
  - api-key: "m"
vertex-api-key:
  - api-key: "v"
    base-url: "http://v"
"#,
        ),
        // Ports the OAuth map tests.
        case(
            "oauth-maps",
            r#"oauth-excluded-models:
  ProviderA:
    - "model-1"
    - "model-2"
  providerB:
    - "x"
oauth-model-alias:
  antigravity:
    - name: "claude-opus-4-6-thinking"
      alias: "claude-antigravity-opus-4-6-thinking"
      display-name: "Antigravity Opus 4.6"
oauth-request-scoped-errors:
  vertex:
    - status: 400
      match:
        - "context_length"
      action: "stop"
  claude:
    - status: 429
      match:
        - "rate_limit"
      action: "continue"
oauth-settings:
  codex:
    - name: "gpt-6-sol"
      max-context-length: 272000
  vertex:
    - name: "gemini-2.5-pro"
      max-context-length: 1048576
"#,
            r#"oauth-excluded-models:
  providerA:
    - "model-1"
    - "model-3"
  providerC:
    - "y"
oauth-model-alias:
  antigravity:
    - name: "claude-opus-4-6-thinking"
      alias: "claude-antigravity-opus-4-6-thinking"
      display-name: "Antigravity Opus 4.6 (Thinking)"
oauth-request-scoped-errors:
  vertex:
    - status: 400
      match:
        - "context_length_updated"
      action: "stop"
  codex:
    - status: 400
      match:
        - "window_exceeded"
      action: "stop"
oauth-settings:
  codex:
    - name: "gpt-6-sol"
      max-context-length: 524288
  claude:
    - name: "claude-sonnet-4-5-20250929"
      max-context-length: 200000
"#,
        ),
        case(
            "oauth-settings-reordered",
            r#"oauth-settings:
  codex:
    - name: "gpt-6-sol"
      max-context-length: 524288
    - name: "deepseek-v4-flash"
      max-context-length: 1048576
"#,
            r#"oauth-settings:
  codex:
    - name: "deepseek-v4-flash"
      max-context-length: 1048576
    - name: "gpt-6-sol"
      max-context-length: 524288
"#,
        ),
        case(
            "oauth-excluded-normalized-equal",
            "oauth-excluded-models:\n  codex:\n    - \"A\"\n    - \" a \"\n    - \"b\"\n",
            "oauth-excluded-models:\n  codex:\n    - \"b\"\n    - \"a\"\n",
        ),
        // Ports TestBuildConfigChangeDetails' remote management lines.
        case(
            "remote-management",
            r#"remote-management:
  panel-github-repository: "repo-old"
"#,
            r#"remote-management:
  allow-remote: true
  secret-key: "$2a$10$abcdefghijklmnopqrstuv"
  disable-control-panel: true
  disable-auto-update-panel: true
  panel-github-repository: "https://user:pass@panel.example/private?token=t"
  base-url: "https://new.example.com/base"
"#,
        ),
        case(
            "remote-management-secret-updated",
            "remote-management:\n  secret-key: \"$2a$10$old\"\n",
            "remote-management:\n  secret-key: \"$2b$10$new\"\n",
        ),
        case(
            "remote-management-secret-deleted",
            "remote-management:\n  secret-key: \"$2y$10$old\"\n",
            "remote-management:\n  allow-remote: false\n",
        ),
        // Ports the openai-compatibility tests that parsing lets through.
        case(
            "openai-compatibility",
            r#"openai-compatibility:
  - name: "provider-a"
    base-url: "https://a.example/v1"
    api-key-entries:
      - api-key: "key-a"
    models:
      - name: "m1"
        alias: ""
  - name: "provider-gone"
    base-url: "https://gone.example/v1"
    models:
      - name: "m1"
        alias: ""
  - name: "duplicate"
    base-url: "https://dup.example/v1"
    models: []
  - name: "duplicate"
    base-url: "https://dup.example/v1"
    models: []
"#,
            r#"openai-compatibility:
  - name: "provider-a"
    base-url: "https://a.example/v1"
    disabled: true
    support-prompt-cache-key: true
    disable-cooling: true
    request-retry: 2
    api-key-entries:
      - api-key: "key-a"
      - api-key: "key-b"
      - api-key: " "
    models:
      - name: "m1"
        alias: ""
      - name: ""
        alias: "m2"
    headers:
      X-Test: "1"
  - name: "provider-b"
    base-url: "https://b.example/v1"
    api-key-entries:
      - api-key: "key-b"
    models: []
  - name: "duplicate"
    base-url: "https://dup.example/v1"
    support-prompt-cache-key: true
    models: []
  - name: "duplicate"
    base-url: "https://dup.example/v1"
    models: []
  - name: "duplicate"
    base-url: "https://dup.example/v1"
    models: []
  - name: ""
    base-url: "https://user:pass@nameless.example/v1?k=secret"
    models: []
  - name: "no-base-url"
    models: []
"#,
        ),
        case(
            "proxy-url-untrimmed",
            "proxy-url: \"http://proxy.example:3128\"\n",
            "proxy-url: \" http://proxy.example:3128 \"\n",
        ),
        case("parse-error-new", "port: 1\n", "port: [\n"),
        case("parse-error-old-empty", "", "port: 1\n"),
    ];

    // Each mode, and the same mode written another way, in either layout.
    for (name, old, new) in [
        ("off-chat", "false", "chat"),
        ("chat-passthrough", "\"chat\"", "passthrough"),
        ("passthrough-on", "passthrough", "true"),
        ("on-off", "true", "false"),
        ("unset-on", "", "yes"),
        ("unset-off", "", "off"),
        ("chat-padded", "\" Chat \"", "CHAT"),
        ("on-spelled", "1", "\"on\""),
        ("off-empty", "no", "\"\""),
    ] {
        let setting = |value: &str| {
            if value.is_empty() {
                "port: 1\n".to_owned()
            } else {
                format!("port: 1\ndisable-image-generation: {value}\n")
            }
        };
        cases.push(case(
            &format!("disable-image-generation-{name}"),
            &setting(old),
            &setting(new),
        ));
    }
    cases.push(case(
        "disable-image-generation-v8-layout",
        "server:\n  port: 1\nmultimedia:\n  disable-image-generation: chat\n",
        "port: 1\ndisable-image-generation: passthrough\n",
    ));
    cases.push(case(
        "disable-image-generation-invalid",
        "port: 1\n",
        "port: 1\ndisable-image-generation: images\n",
    ));

    // Ports TestFormatProxyURL, through proxy-url.
    for (name, url) in [
        ("invalid", "http://[::1"),
        ("full", "http://user:pass@example.com:8080/path?x=1#frag"),
        ("socks5", "socks5://user:pass@192.168.1.1:1080/path?x=1"),
        ("socks5-host-port", "socks5://proxy.example.com:1080/"),
        ("host-port-no-scheme", "example.com:1234/path?x=1"),
        ("relative-path", "/just/path"),
        ("scheme-and-host", "https://example.com"),
        ("ipv6", "http://[fe80::1%25en0]:8080/x"),
        ("bare-host", "proxy.example"),
        ("percent-host", "http://ex%41mple.com/"),
        ("userinfo-only", "http://user@"),
        ("opaque", "mailto:someone@example.com"),
    ] {
        cases.push(case(
            &format!("format-url-{name}"),
            "proxy-url: \"http://before.example\"\n",
            &format!("proxy-url: {}\n", serde_json::Value::from(url)),
        ));
    }
    cases
}
