// Ported from CLIProxyAPI internal/config/config_load.go (LoadConfig),
// parse.go (ParseConfigBytes), config_v8.go (Config.UnmarshalYAML) and
// weight.go (validateCredentialWeightYAML) (v8.0.15, MIT), without the
// `models` check Config.UnmarshalYAML makes.
// https://github.com/router-for-me/CLIProxyAPI

//! Loading a config file or parsing a config payload.
//!
//! Both read the first YAML document, flatten a v8 layout into the legacy
//! one, check credential weights, decode, and apply upstream's checks and
//! clean-ups. Errors are worded as upstream words them:
//! - The file can't be read: `failed to read config file: ...`.
//! - The text isn't YAML or a value has the wrong type: prefixed with
//!   `failed to parse config file: ` ([`Config::load`]) or
//!   `parse config payload: ` ([`Config::parse`]).
//! - Duplicate keys, a root that isn't a mapping, a bad v8 layout, weights
//!   and trusted proxies: upstream's message alone.
//!
//! An empty or comment-only file loads as the defaults; an empty payload is
//! an error, as upstream has it.
//!
//! Deviations from upstream:
//! - Nothing is written back: upstream replaces a plaintext management key
//!   with its bcrypt hash in the file and removes legacy fields that a v8
//!   field overrides.
//! - The text must be UTF-8 (a UTF-8 byte order mark is skipped); yaml.v3
//!   also reads UTF-16.
//! - Upstream's optional mode, which turns a missing or broken file into an
//!   empty config for cloud deployments, isn't ported.

use std::fs;
use std::path::Path;

use super::decode::decode;
use super::normalize::post_process;
use super::types::Config;
use super::v8::{flatten_v8, validate_weights};
use super::yaml::{YamlError, parse_document};
use super::{ConfigError, ConfigErrorKind};

impl Config {
    /// Loads the config file at `path`, as upstream's `LoadConfig` does
    /// without writing anything back.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let data = fs::read(path).map_err(|error| {
            ConfigError::new(
                ConfigErrorKind::Read,
                format!("failed to read config file: {}: {error}", path.display()),
            )
        })?;
        load_bytes(&data)
    }

    /// Parses a config payload, as upstream's `ParseConfigBytes` does.
    pub fn parse(data: impl AsRef<[u8]>) -> Result<Config, ConfigError> {
        let data = data.as_ref();
        if data.is_empty() {
            return Err(ConfigError::new(
                ConfigErrorKind::Empty,
                "config payload is empty",
            ));
        }
        from_bytes(data, "parse config payload: ")
    }
}

/// Loads a config file's contents, with [`Config::load`]'s error wording.
pub(crate) fn load_bytes(data: &[u8]) -> Result<Config, ConfigError> {
    from_bytes(data, "failed to parse config file: ")
}

/// The shared pipeline; `prefix` starts syntax and decode errors.
fn from_bytes(data: &[u8], prefix: &str) -> Result<Config, ConfigError> {
    let Ok(text) = std::str::from_utf8(data) else {
        return Err(ConfigError::new(
            ConfigErrorKind::Syntax,
            format!("{prefix}yaml: input is not valid UTF-8"),
        ));
    };
    let root = match parse_document(text) {
        Ok(Some(root)) => root,
        // yaml.v3 leaves the defaults alone when there's no document.
        Ok(None) => {
            let mut config = Config::default();
            post_process(&mut config)?;
            return Ok(config);
        }
        Err(error) => {
            let message = format!("{prefix}{}", error.message());
            return Err(ConfigError::new(ConfigErrorKind::Syntax, message));
        }
    };
    let flattened = flatten_v8(&root)?;
    validate_weights(&flattened.root)?;
    let mut config: Config = decode(&flattened.root).map_err(|error| {
        let kind = match error {
            YamlError::Syntax(_) => ConfigErrorKind::Syntax,
            YamlError::Fatal(_) | YamlError::Type(_) => ConfigErrorKind::Decode,
        };
        ConfigError::new(kind, format!("{prefix}{}", error.message()))
    })?;
    config.oauth_only_fields = flattened.oauth_only_fields;
    post_process(&mut config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::RoutingStrategy;
    use crate::config::testing::TempDir;
    use crate::config::types::{CodexModel, GeminiModel, RoutingConfig};
    use ConfigErrorKind::{Decode, Invalid, Syntax};

    const LOAD: &str = "failed to parse config file: ";
    const PARSE: &str = "parse config payload: ";
    const MAPPING: &str = "config must be a mapping";

    /// Upstream's `yaml.Unmarshal` into a `Config`: flattened and decoded,
    /// without the checks and clean-ups.
    fn unmarshal(text: &str) -> Config {
        let root = parse_document(text).expect("syntax").expect("a document");
        let flattened = flatten_v8(&root).expect("layout");
        let mut config: Config = decode(&flattened.root).expect("decode");
        config.oauth_only_fields = flattened.oauth_only_fields;
        config
    }

    fn parse(text: &str) -> Config {
        Config::parse(text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
    }

    fn defaults() -> Config {
        let mut config = Config::default();
        post_process(&mut config).expect("defaults are valid");
        config
    }

    fn is_default(config: &Config) -> bool {
        *config == defaults()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// A one-line YAML mapping that sets the dotted `path` to `value`.
    fn nest(path: &str, value: &str) -> String {
        let mut parts: Vec<&str> = path.split('.').collect();
        let last = parts.pop().expect("a key");
        let mut text = format!("{last}: {value}");
        while let Some(part) = parts.pop() {
            text = format!("{part}: {{{text}}}");
        }
        text
    }

    // The cases below were run through upstream's LoadConfig and
    // ParseConfigBytes to get the expected results.

    /// Inputs both entry points reject, with upstream's message, and whether
    /// the entry point's prefix starts it. Type errors leave out yaml.v3's
    /// excerpt of the value.
    const REJECTED: &[(&str, ConfigErrorKind, bool, &str)] = &[
        ("~", Invalid, false, MAPPING),
        ("foo", Invalid, false, MAPPING),
        ("- a\n- b\n", Invalid, false, MAPPING),
        ("!!str foo\n", Invalid, false, MAPPING),
        ("''\n", Invalid, false, MAPPING),
        ("[]\n", Invalid, false, MAPPING),
        ("null\n", Invalid, false, MAPPING),
        ("---\n", Invalid, false, MAPPING),
        ("--- ~\n", Invalid, false, MAPPING),
        (
            "port: 1\nport: 2\n",
            Decode,
            false,
            "yaml: unmarshal errors:\n  line 2: mapping key \"port\" already defined at line 1",
        ),
        (
            "port: 1\ntls:\n  enable: true\n  enable: false\nhost: a\nhost: b\n",
            Decode,
            false,
            "yaml: unmarshal errors:\n  line 6: mapping key \"host\" already defined at line 5",
        ),
        (
            "port: 1\nport: 2\nhost: x\nnested:\n  a: 1\n  a: 2\n",
            Decode,
            false,
            "yaml: unmarshal errors:\n  line 2: mapping key \"port\" already defined at line 1",
        ),
        (
            "port: abc\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into int",
        ),
        (
            "port: 1\nhost: [a]\nrequest-retry: x\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 2: cannot unmarshal !!seq into string\n  \
             line 3: cannot unmarshal !!str into int",
        ),
        (
            "debug: 'true'\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into bool",
        ),
        (
            "codex-api-key:\n  - api-key: k\n    weight: '5'\n",
            Invalid,
            false,
            "codex-api-key[0].weight: weight must be an integer",
        ),
        (
            "codex-api-key:\n  - api-key: k\n    weight: 2000000\n",
            Invalid,
            false,
            "codex-api-key[0].weight: weight must not exceed 1000000",
        ),
        (
            "config-version: 7\n",
            Invalid,
            false,
            "unsupported config-version (expected 8)",
        ),
        (
            "config-version: '8'\n",
            Invalid,
            false,
            "unsupported config-version (expected 8)",
        ),
        (
            "host: !!binary '%%%'\n",
            Decode,
            false,
            "yaml: !!binary value contains invalid base64 data",
        ),
        (
            "port: 9999999999999999999\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!int into int",
        ),
        (
            "port: true\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!bool into int",
        ),
        (
            "port: ''\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into int",
        ),
        (
            "port: 2024-01-02\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!timestamp into int",
        ),
        (
            "port: !!int abc\n",
            Decode,
            false,
            "yaml: cannot decode !!str as a !!int",
        ),
        (
            "port: !custom 5\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !custom into int",
        ),
        (
            "api-keys: a\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into []string",
        ),
        (
            "a: &x [*x]\n",
            Decode,
            false,
            "yaml: anchor 'x' value contains itself",
        ),
        (
            "a: *nope\n",
            Syntax,
            true,
            "yaml: unknown anchor 'nope' referenced",
        ),
        ("server: 5\n", Invalid, false, "server must be a mapping"),
        ("routing: 5\n", Invalid, false, "routing must be a mapping"),
        (
            "routing: [a]\n",
            Invalid,
            false,
            "routing must be a mapping",
        ),
        (
            "oauth: {providers: {claude: 5}}\n",
            Invalid,
            false,
            "oauth.providers.claude must be a mapping",
        ),
        (
            "? [a]\n: b\n",
            Decode,
            false,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!seq into string",
        ),
        (
            "tls: foo\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into config.TLSConfig",
        ),
        (
            "tls: [a]\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!seq into config.TLSConfig",
        ),
        (
            "host: {a: 1}\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!map into string",
        ),
        (
            "api-keys:\n  codex:\n    - base-url: http://x\n      keys:\n        - api-key: k1\n          \
             weight: '3'\n",
            Invalid,
            false,
            "api-keys.codex.keys[0].weight: weight must be an integer",
        ),
        (
            "api-keys:\n  codex: 5\n",
            Invalid,
            false,
            "api-keys.codex must be a list",
        ),
        (
            "api-keys:\n  codex:\n    - base-url: http://x\n      bogus: 1\n      keys: []\n",
            Invalid,
            false,
            "api-keys.codex: unsupported group field bogus",
        ),
        (
            "debug: 1\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!int into bool",
        ),
        (
            "port: \"5\"\n",
            Decode,
            true,
            "yaml: unmarshal errors:\n  line 1: cannot unmarshal !!str into int",
        ),
        (
            "trusted-proxies: [' 1.2.3.4']\n",
            Invalid,
            false,
            "invalid trusted-proxies entry \" 1.2.3.4\": expected an IP address or CIDR",
        ),
        (
            "trusted-proxies: ['1.2.3.0/33']\n",
            Invalid,
            false,
            "invalid trusted-proxies entry \"1.2.3.0/33\": invalid CIDR address: 1.2.3.0/33",
        ),
        (
            "trusted-proxies: ['nope']\n",
            Invalid,
            false,
            "invalid trusted-proxies entry \"nope\": invalid CIDR address: nope",
        ),
        (
            "trusted-proxies: ['']\n",
            Invalid,
            false,
            "invalid trusted-proxies entry \"\": expected an IP address or CIDR",
        ),
    ];

    type Check = fn(&Config) -> bool;

    /// Inputs both entry points accept, with a check of the result.
    const ACCEPTED: &[(&str, Check)] = &[
        ("# only a comment\n", is_default),
        ("{}\n", is_default),
        ("debug: yes\n", |c| c.debug),
        ("debug: on\n", |c| c.debug),
        ("debug: \"on\"\n", |c| c.debug),
        ("debug: ~\nport: ~\n", is_default),
        ("routing: ~\n", is_default),
        ("port: 1\n---\nport: 2\n", |c| c.port == 1),
        ("base: &b {port: 5}\n<<: *b\n", |c| c.port == 5),
        ("x: !!binary aGVsbG8=\nhost: !!binary aGVsbG8=\n", |c| {
            c.host == "hello"
        }),
        ("port: 1.5\n", |c| c.port == 1),
        ("port: 0x1F\n", |c| c.port == 31),
        ("port: 1_000\n", |c| c.port == 1000),
        ("port: 0777\n", |c| c.port == 511),
        ("port: 0o17\n", |c| c.port == 15),
        ("port: 0b101\n", |c| c.port == 5),
        ("port: .5\n", |c| c.port == 0),
        ("port: 1e3\n", |c| c.port == 1000),
        ("port: !!int '42'\n", |c| c.port == 42),
        ("host: !custom 5\n", |c| c.host == "5"),
        ("api-keys: [a, ~, b]\n", |c| c.api_keys == ["a", "b"]),
        ("codex-api-key: [~]\n", is_default),
        ("~: 5\nport: 3\n", |c| c.port == 3),
        ("x: &a\n  - 1\nb: [*a, *a]\n", is_default),
        ("port: 65536\nhost: 5\n", |c| {
            c.port == 65536 && c.host == "5"
        }),
        ("port: -1\nmax-retry-credentials: -3\n", |c| {
            c.port == -1 && c.max_retry_credentials == 0
        }),
        ("port: 1\n...\n", |c| c.port == 1),
        ("%YAML 1.1\n---\nport: 3\n", |c| c.port == 3),
        ("port: 3 # c\n", |c| c.port == 3),
        ("!!map {port: 3}\n", |c| c.port == 3),
        ("request-retry: 1.9\n", |c| c.request_retry == 1),
        ("request-retry: -1.5\n", |c| c.request_retry == -1),
        ("codex: {stream-bootstrap-timeout: 5}\n", |c| {
            c.codex.stream_bootstrap_timeout == "5"
        }),
        ("access:\n  api-keys: [a]\napi-keys: [b]\n", |c| {
            c.api_keys == ["a"]
        }),
        ("remote-management:\n  secret-key: abc\n", |c| {
            c.remote_management.secret_key == "abc"
        }),
        (
            "streaming:\n  keepalive-seconds: 5\nnonstream-keepalive-interval: 3\n",
            |c| c.streaming.keepalive_seconds == 5 && c.nonstream_keepalive_interval == 3,
        ),
        (
            "oauth-excluded-models:\n  codex: ~\n  claude: [A, ' b ', a]\n",
            |c| {
                c.oauth_excluded_models
                    == BTreeMap::from([("claude".to_owned(), vec!["a".to_owned(), "b".to_owned()])])
            },
        ),
        (
            "codex-api-key:\n  - api-key: k\n    base-url: http://x\n    weight: -5\n",
            |c| c.codex_api_key.len() == 1 && c.codex_api_key.iter().all(|k| k.weight == Some(-5)),
        ),
        (
            "codex-api-key:\n  - api-key: k\n    base-url: http://x\n    headers:\n      X-A: ~\n      \
             ' X-B ': ' v '\n      '': x\n",
            |c| {
                c.codex_api_key
                    .iter()
                    .map(|k| k.headers.clone())
                    .collect::<Vec<_>>()
                    == [BTreeMap::from([("X-B".to_owned(), "v".to_owned())])]
            },
        ),
        (
            "api-keys:\n  codex:\n    - base-url: http://x\n      keys:\n        - api-key: k1\n          \
             weight: 3\n",
            |c| {
                c.codex_api_key.len() == 1
                    && c.codex_api_key.iter().all(|k| {
                        k.api_key == "k1" && k.base_url == "http://x" && k.weight == Some(3)
                    })
            },
        ),
    ];

    #[test]
    fn rejected_inputs_match_upstream() {
        for (text, kind, prefixed, message) in REJECTED {
            for (prefix, result) in [
                (LOAD, load_bytes(text.as_bytes())),
                (PARSE, Config::parse(text)),
            ] {
                let error = result.expect_err(text);
                let want = if *prefixed {
                    format!("{prefix}{message}")
                } else {
                    (*message).to_owned()
                };
                assert_eq!(error.to_string(), want, "{text:?}");
                assert_eq!(error.kind(), *kind, "{text:?}");
            }
        }
    }

    #[test]
    fn accepted_inputs_match_upstream() {
        for (text, check) in ACCEPTED {
            let loaded =
                load_bytes(text.as_bytes()).unwrap_or_else(|error| panic!("{text:?}: {error}"));
            assert!(check(&loaded), "{text:?}: {loaded:?}");
            assert_eq!(parse(text), loaded, "{text:?}");
        }
    }

    #[test]
    fn syntax_errors_carry_the_prefix_and_line() {
        let error = load_bytes(b"a: [\n").expect_err("unclosed flow sequence");
        assert_eq!(error.kind(), Syntax);
        let message = error.to_string();
        assert!(
            message.starts_with("failed to parse config file: yaml: line "),
            "{message}"
        );
        let error = Config::parse("a: [\n").expect_err("unclosed flow sequence");
        assert!(
            error
                .to_string()
                .starts_with("parse config payload: yaml: line "),
            "{error}"
        );
        let error = Config::parse(b"port: \xff\n").expect_err("not UTF-8");
        assert_eq!(
            error.to_string(),
            "parse config payload: yaml: input is not valid UTF-8"
        );
        assert_eq!(error.kind(), Syntax);
    }

    #[test]
    fn empty_input_loads_defaults_but_an_empty_payload_is_an_error() {
        assert!(is_default(&load_bytes(b"").expect("empty file")));
        assert!(is_default(
            &load_bytes("\u{feff}".as_bytes()).expect("byte order mark")
        ));
        let error = Config::parse("").expect_err("empty payload");
        assert_eq!(error.to_string(), "config payload is empty");
        assert_eq!(error.kind(), ConfigErrorKind::Empty);
        assert!(is_default(&parse("# only a comment\n")));
    }

    #[test]
    fn load_reads_the_file_and_never_writes_it() {
        let dir = TempDir::new();
        let text = "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\n\
                    routing:\n  retry: {request-retry: 0}\n";
        let path = dir.write("config.yaml", text);
        let config = Config::load(&path).expect("load");
        assert_eq!(config.request_retry, 0);
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), text);

        let error = Config::load(dir.join("missing.yaml")).expect_err("missing file");
        assert_eq!(error.kind(), ConfigErrorKind::Read);
        let message = error.to_string();
        assert!(
            message.starts_with("failed to read config file: "),
            "{message}"
        );
        assert!(message.contains("missing.yaml"), "{message}");
        let error = Config::load(dir.path()).expect_err("a directory");
        assert_eq!(error.kind(), ConfigErrorKind::Read);
    }

    #[test]
    fn tab_before_a_key_is_a_documented_deviation() {
        // yaml.v3 rejects this ("found character that cannot start any
        // token"); saphyr reads it.
        assert_eq!(parse("\tport: 1\n").port, 1);
    }

    // remote_management_test.go

    #[test]
    fn remote_management_base_url() {
        let config = parse(
            "remote-management:\n  allow-remote: true\n  base-url: \"https://proxy.example.com\"\n",
        );
        assert_eq!(
            config.remote_management.base_url,
            "https://proxy.example.com"
        );
        assert!(config.remote_management.allow_remote);
    }

    // trusted_proxies_test.go

    #[test]
    fn trusted_proxies() {
        let config = parse("trusted-proxies:\n  - 192.0.2.0/24\n  - 2001:db8::1\n");
        assert_eq!(config.trusted_proxies, ["192.0.2.0/24", "2001:db8::1"]);
        assert!(Config::parse("trusted-proxies: [not-an-ip]").is_err());
    }

    // cooling_override_test.go

    #[test]
    fn cooling_override_presence_is_kept() {
        let config = parse(
            "disable-cooling: true\n\
             gemini-api-key:\n  - api-key: gemini-key\n    disable-cooling: false\n\
             interactions-api-key:\n  - api-key: interactions-key\n    disable-cooling: false\n\
             claude-api-key:\n  - api-key: claude-key\n    disable-cooling: false\n  - api-key: unset\n\
             codex-api-key:\n  - api-key: codex-key\n    base-url: https://codex.example.com\n    \
             disable-cooling: false\n\
             xai-api-key:\n  - api-key: xai-key\n    base-url: https://api.x.ai/v1\n    \
             disable-cooling: false\n\
             openai-compatibility:\n  - name: compat\n    base-url: https://compat.example.com/v1\n    \
             disable-cooling: false\n\
             vertex-api-key:\n  - api-key: vertex-key\n    base-url: https://vertex.example.com\n    \
             disable-cooling: false\n",
        );
        assert!(config.disable_cooling);
        assert_eq!(config.openai_compatibility[0].disable_cooling, Some(false));
        assert_eq!(config.gemini_api_key[0].disable_cooling, Some(false));
        assert_eq!(config.interactions_api_key[0].disable_cooling, Some(false));
        assert_eq!(config.xai_api_key[0].disable_cooling, Some(false));
        assert_eq!(config.vertex_api_key[0].disable_cooling, Some(false));
        let claude: Vec<Option<bool>> = config
            .claude_api_key
            .iter()
            .map(|k| k.disable_cooling)
            .collect();
        assert_eq!(claude, [Some(false), None]);
        let codex: Vec<Option<bool>> = config
            .codex_api_key
            .iter()
            .map(|k| k.disable_cooling)
            .collect();
        assert_eq!(codex, [Some(false)]);
    }

    // weight_test.go

    #[test]
    fn api_key_weight_validation() {
        for (weight, valid) in [
            ("-1", true),
            ("1000000", true),
            ("1.5", false),
            ("1000001", false),
            ("9223372036854775808", false),
        ] {
            for family in [
                "gemini-api-key",
                "interactions-api-key",
                "codex-api-key",
                "xai-api-key",
                "meta-api-key",
                "claude-api-key",
                "vertex-api-key",
            ] {
                let text = format!("{family}:\n  - api-key: key\n    weight: {weight}\n");
                assert_eq!(Config::parse(&text).is_ok(), valid, "{text:?}");
            }
        }
    }

    #[test]
    fn api_key_weight_zero_is_explicit() {
        let config = parse(
            "xai-api-key:\n  - api-key: key\n    base-url: https://api.x.ai/v1\n    \
             weight: 0\n  - api-key: other\n    base-url: https://api.x.ai/v1\n",
        );
        let weights: Vec<Option<i64>> = config.xai_api_key.iter().map(|k| k.weight).collect();
        assert_eq!(weights, [Some(0), None]);
    }

    // xai_api_key_test.go: TestParseConfigBytesXAIConfig

    #[test]
    fn xai_config() {
        assert!(!parse("{}").xai.inject_x_search);
        assert!(parse("xai:\n  inject-x-search: true\n").xai.inject_x_search);
    }

    // xai_api_key_test.go: TestParseConfigBytesXAIAPIKeyMatchesCodexShape

    #[test]
    fn xai_api_key_matches_codex_shape() {
        let config = parse(
            "xai-api-key:\n  - api-key: \" xai-key \"\n    priority: 3\n    weight: 5\n    \
             prefix: \" team-xai \"\n    base-url: \" https://api.x.ai/v1 \"\n    websockets: true\n    \
             proxy-url: \" http://proxy.local \"\n    headers:\n      X-Custom: value\n    \
             models:\n      - name: grok-4.5\n        alias: grok-latest\n        \
             display-name: Grok Latest\n        force-mapping: true\n    \
             excluded-models:\n      - \" grok-3-* \"\n    disable-cooling: true\n    \
             request-retry: 0\n  - api-key: dropped\n    base-url: \" \"\n",
        );
        assert_eq!(config.xai_api_key.len(), 1);
        let entry = &config.xai_api_key[0];
        // The key and proxy stay as written, as Codex keys' do.
        assert_eq!(entry.api_key, " xai-key ");
        assert_eq!((entry.priority, entry.weight), (3, Some(5)));
        assert_eq!(entry.prefix, "team-xai");
        assert_eq!(entry.base_url, "https://api.x.ai/v1");
        assert!(entry.websockets);
        assert_eq!(entry.proxy_url, " http://proxy.local ");
        assert_eq!(entry.disable_cooling, Some(true));
        assert_eq!(entry.request_retry, Some(0));
        assert_eq!(
            entry.headers,
            BTreeMap::from([("X-Custom".to_owned(), "value".to_owned())])
        );
        assert_eq!(
            entry.models,
            [CodexModel {
                name: "grok-4.5".to_owned(),
                alias: "grok-latest".to_owned(),
                display_name: "Grok Latest".to_owned(),
                force_mapping: true,
                ..CodexModel::default()
            }]
        );
        assert_eq!(entry.excluded_models, ["grok-3-*"]);
    }

    // config_meta_test.go: TestMetaConfigDropsUnusableKeys

    #[test]
    fn meta_config_drops_unusable_keys() {
        let config = parse(
            "meta-api-key:\n  - {}\n  - api-key: \"   \"\n  - base-url: \"https://api.meta.ai/v1\"\n  \
             - headers: {X-Trace: placeholder}\n  - api-key: \" LLM|valid \"\n  \
             - api-key: \"dca:requires-oauth-storage\"\n",
        );
        let keys: Vec<(&str, &str)> = config
            .meta_api_key
            .iter()
            .map(|key| (key.api_key.as_str(), key.base_url.as_str()))
            .collect();
        assert_eq!(keys, [("LLM|valid", "https://api.meta.ai/v1")]);
    }

    // request_retry_test.go, with one more Codex key.

    #[test]
    fn request_retry_overrides() {
        let config = parse(
            "gemini-api-key:\n  - api-key: gemini-zero\n    request-retry: 0\n  \
             - api-key: gemini-unset\n\
             interactions-api-key:\n  - api-key: interactions-two\n    request-retry: 2\n\
             xai-api-key:\n  - api-key: xai-zero\n    base-url: https://api.x.ai/v1\n    \
             request-retry: 0\n\
             vertex-api-key:\n  - api-key: vertex-four\n    request-retry: 4\n\
             codex-api-key:\n  - api-key: codex-neg\n    base-url: https://codex.example.com\n    \
             request-retry: -1\n  - api-key: codex-unset\n    base-url: https://codex.example.com\n\
             claude-api-key:\n  - api-key: claude-three\n    request-retry: 3\n\
             openai-compatibility:\n  - name: compat\n    base-url: https://compat.example.com/v1\n    \
             request-retry: 0\n    api-key-entries:\n      - api-key: compat-key\n",
        );
        assert_eq!(config.openai_compatibility[0].request_retry, Some(0));
        let codex: Vec<Option<i64>> = config
            .codex_api_key
            .iter()
            .map(|k| k.request_retry)
            .collect();
        assert_eq!(codex, [Some(-1), None]);
        let claude: Vec<Option<i64>> = config
            .claude_api_key
            .iter()
            .map(|k| k.request_retry)
            .collect();
        assert_eq!(claude, [Some(3)]);
        let gemini: Vec<Option<i64>> = config
            .gemini_api_key
            .iter()
            .map(|k| k.request_retry)
            .collect();
        assert_eq!(gemini, [Some(0), None]);
        let vertex: Vec<Option<i64>> = config
            .vertex_api_key
            .iter()
            .map(|k| k.request_retry)
            .collect();
        assert_eq!(vertex, [Some(4)]);
        let interactions: Vec<Option<i64>> = config
            .interactions_api_key
            .iter()
            .map(|k| k.request_retry)
            .collect();
        assert_eq!(interactions, [Some(2)]);
        let xai: Vec<Option<i64>> = config.xai_api_key.iter().map(|k| k.request_retry).collect();
        assert_eq!(xai, [Some(0)]);
    }

    // api_key_is_compat_test.go, is_compat_test.go, max_context_length_test.go
    // and model_display_name_test.go, which decode with yaml.Unmarshal.

    #[test]
    fn api_key_model_fields_decode() {
        let config = unmarshal(
            "claude-api-key:\n  - models:\n      - name: claude-upstream\n        \
             alias: claude-alias\n        is-compat: true\n        display-name: Claude Name\n        \
             max-context-length: 1048576\n      - name: claude-native\n        alias: claude-native\n\
             codex-api-key:\n  - models:\n      - name: codex-upstream\n        alias: codex-alias\n        \
             is-compat: true\n        display-name: Codex Name\n        max-context-length: 1048576\n        \
             support-configuration-update: true\n      - name: codex-native\n        \
             alias: codex-native\n\
             openai-compatibility:\n  - name: compat\n    models:\n      - name: compat-upstream\n        \
             alias: compat-alias\n        is-compat: true\n        display-name: Compatibility Name\n        \
             max-context-length: 1048576\n      - name: compat-native\n        alias: compat-native\n\
             gemini-api-key:\n  - models:\n      - name: gemini-upstream\n        \
             alias: gemini-alias\n        is-compat: true\n        display-name: Gemini Name\n        \
             max-context-length: 1048576\n      - name: gemini-native\n        alias: gemini-native\n\
             vertex-api-key:\n  - models:\n      - name: vertex-upstream\n        \
             alias: vertex-alias\n        display-name: Vertex Name\n\
             interactions-api-key:\n  - models:\n      - name: interactions-upstream\n        \
             alias: interactions-alias\n        is-compat: true\n        \
             max-context-length: 1048576\n\
             xai-api-key:\n  - models:\n      - name: xai-upstream\n        alias: xai-alias\n        \
             is-compat: true\n        display-name: xAI Name\n        max-context-length: 1048576\n",
        );
        assert_eq!(
            config.gemini_api_key.first().map(|key| key.models.clone()),
            Some(vec![
                GeminiModel {
                    name: "gemini-upstream".to_owned(),
                    alias: "gemini-alias".to_owned(),
                    display_name: "Gemini Name".to_owned(),
                    max_context_length: 1_048_576,
                    is_compat: true,
                    ..GeminiModel::default()
                },
                GeminiModel {
                    name: "gemini-native".to_owned(),
                    alias: "gemini-native".to_owned(),
                    ..GeminiModel::default()
                },
            ])
        );
        let vertex = config.vertex_api_key.first().map(|key| &key.models[0]);
        assert_eq!(
            vertex.map(|model| model.display_name.as_str()),
            Some("Vertex Name")
        );
        let compat = &config
            .openai_compatibility
            .first()
            .expect("a compat provider")
            .models;
        assert_eq!(compat.len(), 2);
        let compat_first = compat.first().expect("a model");
        assert!(compat_first.is_compat);
        assert!(!compat.get(1).expect("a model").is_compat);
        assert_eq!(compat_first.display_name, "Compatibility Name");
        assert_eq!(compat_first.max_context_length, 1_048_576);
        let claude = &config.claude_api_key.first().expect("a claude key").models;
        let codex = &config.codex_api_key.first().expect("a codex key").models;
        assert_eq!((claude.len(), codex.len()), (2, 2));
        let first = claude.first().expect("a model");
        assert!(first.is_compat);
        assert!(!claude.get(1).expect("a model").is_compat);
        assert_eq!(first.display_name, "Claude Name");
        assert_eq!(first.max_context_length, 1_048_576);
        assert_eq!(
            codex.first(),
            Some(&CodexModel {
                name: "codex-upstream".to_owned(),
                alias: "codex-alias".to_owned(),
                display_name: "Codex Name".to_owned(),
                max_context_length: 1_048_576,
                is_compat: true,
                support_configuration_update: true,
                ..CodexModel::default()
            })
        );
        assert_eq!(
            codex.get(1),
            Some(&CodexModel {
                name: "codex-native".to_owned(),
                alias: "codex-native".to_owned(),
                ..CodexModel::default()
            })
        );
        assert_eq!(
            config
                .interactions_api_key
                .first()
                .map(|key| key.models.clone()),
            Some(vec![GeminiModel {
                name: "interactions-upstream".to_owned(),
                alias: "interactions-alias".to_owned(),
                max_context_length: 1_048_576,
                is_compat: true,
                ..GeminiModel::default()
            }])
        );
        assert_eq!(
            config.xai_api_key.first().map(|key| key.models.clone()),
            Some(vec![CodexModel {
                name: "xai-upstream".to_owned(),
                alias: "xai-alias".to_owned(),
                display_name: "xAI Name".to_owned(),
                max_context_length: 1_048_576,
                is_compat: true,
                ..CodexModel::default()
            }])
        );
    }

    // use_max_completion_tokens_test.go:
    // TestOpenAICompatibilityUseMaxCompletionTokensYAMLDecoding. The JSON
    // variant is dropped: configs are only read from YAML here.

    #[test]
    fn openai_compatibility_use_max_completion_tokens() {
        let config = unmarshal(
            "\nopenai-compatibility:\n  - name: test-provider\n    models:\n      \
             - name: new-reasoning-model\n        alias: new-alias\n        \
             use-max-completion-tokens: true\n      - name: legacy-model\n        \
             alias: legacy-alias\n",
        );
        assert_eq!(config.openai_compatibility.len(), 1);
        let models = &config.openai_compatibility[0].models;
        assert_eq!(models.len(), 2);
        assert!(models[0].use_max_completion_tokens);
        assert!(!models[1].use_max_completion_tokens);
    }

    #[test]
    fn openai_compatibility_decodes_every_field() {
        let config = parse(
            "openai-compatibility:\n  - name: \" kimi \"\n    priority: 3\n    disabled: true\n    \
             prefix: /teamA/\n    base-url: \" https://compat.example.com/v1 \"\n    \
             api-key-entries:\n      - api-key: k1\n        weight: 2\n        \
             proxy-url: http://proxy.local\n      - api-key: k2\n    \
             models:\n      - name: kimi-k2\n        alias: k2\n        image: true\n        \
             input-modalities: [text]\n        output-modalities: [text]\n        \
             force-mapping: true\n        thinking: {levels: [low, high]}\n    \
             headers: {X-Team: \" a \"}\n    support-prompt-cache-key: true\n  \
             - name: no-base-url\n",
        );
        assert_eq!(config.openai_compatibility.len(), 1);
        let compat = &config.openai_compatibility[0];
        assert_eq!(compat.name, "kimi");
        assert_eq!(compat.priority, 3);
        assert!(compat.disabled && compat.support_prompt_cache_key);
        assert_eq!(compat.prefix, "teamA");
        assert_eq!(compat.base_url, "https://compat.example.com/v1");
        assert_eq!(
            compat.headers,
            BTreeMap::from([("X-Team".to_owned(), "a".to_owned())])
        );
        let keys: Vec<(&str, Option<i64>, &str)> = compat
            .api_key_entries
            .iter()
            .map(|key| (key.api_key.as_str(), key.weight, key.proxy_url.as_str()))
            .collect();
        assert_eq!(
            keys,
            [("k1", Some(2), "http://proxy.local"), ("k2", None, "")]
        );
        let model = &compat.models[0];
        assert_eq!(
            (model.name.as_str(), model.alias.as_str()),
            ("kimi-k2", "k2")
        );
        assert!(model.image && model.force_mapping);
        assert_eq!(model.input_modalities, ["text"]);
        assert_eq!(model.output_modalities, ["text"]);
        assert_eq!(
            model
                .thinking
                .as_ref()
                .map(|thinking| thinking.levels.clone()),
            Some(strings(&["low", "high"]))
        );
        assert!(
            Config::parse(
                "openai-compatibility:\n  - name: p\n    base-url: u\n    \
                 api-key-entries: [{api-key: k, weight: 1000001}]\n"
            )
            .is_err()
        );
    }

    // codex_websocket_header_defaults_test.go and claude_code_test.go

    #[test]
    fn codex_header_defaults_are_trimmed() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "\ncodex-header-defaults:\n  user-agent: \"  my-codex-client/1.0  \"\n  \
             beta-features: \"  feature-a,feature-b  \"\n",
        );
        let config = Config::load(path).expect("load");
        // The user agent is read and ignored: this port sends its own.
        assert_eq!(
            config.codex_header_defaults.beta_features,
            "feature-a,feature-b"
        );
    }

    #[test]
    fn codex_options() {
        let dir = TempDir::new();
        let path = dir.write(
            "config.yaml",
            "\ncodex:\n  disable-codex-cloaking: true\n  optimize-multi-agent-v2: true\n",
        );
        let config = Config::load(path).expect("load");
        assert!(config.client.codex.optimize_multi_agent_v2);
    }

    #[test]
    fn model_level_cooling() {
        let dir = TempDir::new();
        let path = dir.write("config.yaml", "\ncodex:\n  model-level-cooling: true\n");
        assert!(Config::load(path).expect("load").codex.model_level_cooling);
        assert!(!parse("{}").codex.model_level_cooling);
        assert!(!parse("port: 8317\n").claude.model_level_cooling);
        assert!(
            parse("claude:\n  model-level-cooling: true\n")
                .claude
                .model_level_cooling
        );
    }

    // client_test.go and client_optimize_test.go

    #[test]
    fn client_codex_enable_apply_patch() {
        for (text, want) in [
            ("config-version: 8\n", false),
            ("client: {codex: {enable-apply-patch: false}}\n", false),
            ("client: {codex: {enable-apply-patch: true}}\n", true),
        ] {
            let config = parse(text);
            assert_eq!(config.client.codex.enable_apply_patch, want, "{text:?}");
            assert!(
                !config
                    .oauth_only_fields()
                    .contains("client.codex.enable-apply-patch")
            );
            assert_eq!(unmarshal(text).client, config.client);
        }
    }

    #[test]
    fn client_codex_optimize_multi_agent_v2() {
        for (text, want) in [
            ("config-version: 8\n", false),
            ("client: {codex: {optimize-multi-agent-v2: false}}\n", false),
            (
                "client: {codex: {optimize-multi-agent-v2: true, enable-apply-patch: true}}\n",
                true,
            ),
            ("client: {codex: {optimize-multi-agent-v2: null}}\n", false),
        ] {
            assert_eq!(
                parse(text).client.codex.optimize_multi_agent_v2,
                want,
                "{text:?}"
            );
        }
    }

    #[test]
    fn client_codex_optimize_multi_agent_v2_historical_paths() {
        const HISTORICAL: [&str; 3] = [
            "oauth.providers.codex.optimize-multi-agent-v2",
            "providers.codex.optimize-multi-agent-v2",
            "codex.optimize-multi-agent-v2",
        ];
        for (text, want) in [
            ("codex: {optimize-multi-agent-v2: true}\n", true),
            (
                "providers: {codex: {optimize-multi-agent-v2: true}}\n",
                true,
            ),
            (
                "oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n",
                true,
            ),
            (
                "providers: {codex: {optimize-multi-agent-v2: false}}\n",
                false,
            ),
            (
                "client: {codex: {optimize-multi-agent-v2: false}}\n\
                 providers: {codex: {optimize-multi-agent-v2: true}}\n\
                 oauth: {providers: {codex: {optimize-multi-agent-v2: true}}}\n\
                 codex: {optimize-multi-agent-v2: true}\n",
                false,
            ),
            (
                "client: {codex: {optimize-multi-agent-v2: true}}\n\
                 oauth: {providers: {codex: {optimize-multi-agent-v2: false}}}\n",
                true,
            ),
            (
                "client: {codex: {optimize-multi-agent-v2: null}}\n\
                 providers: {codex: {optimize-multi-agent-v2: true}}\n",
                false,
            ),
            (
                "oauth: {providers: {codex: {optimize-multi-agent-v2: false}}}\n\
                 providers: {codex: {optimize-multi-agent-v2: true}}\n\
                 codex: {optimize-multi-agent-v2: true}\n",
                false,
            ),
            (
                "providers: {codex: {optimize-multi-agent-v2: false}}\n\
                 codex: {optimize-multi-agent-v2: true}\n",
                false,
            ),
            (
                "client: &client {codex: {optimize-multi-agent-v2: false}}\n\
                 providers: {<<: *client}\n",
                false,
            ),
        ] {
            let mut config = parse(text);
            assert_eq!(
                config.client.codex.optimize_multi_agent_v2, want,
                "{text:?}"
            );
            for path in HISTORICAL {
                assert!(
                    !config.oauth_only_fields().contains(path),
                    "{text:?}: {path}"
                );
            }
            // A scoped copy keeps the client settings and the shared values.
            config.codex.response_steering = true;
            config.oauth_only_fields = ["codex.response-steering".to_owned()].into();
            let api = config.for_api_key();
            assert!(matches!(api, Cow::Owned(_)));
            assert_eq!(api.client, config.client);
            assert!(api.codex.response_steering);
        }
    }

    // oauth_model_alias_test.go, oauth_request_scoped_errors_test.go and
    // request_scoped_errors_test.go

    #[test]
    fn oauth_meta_channel() {
        let config = parse(
            "\noauth-model-alias:\n  meta:\n    - name: \"muse-spark-1.3\"\n      \
             alias: \"muse-latest\"\n      fork: true\n      force-mapping: true\n\
             oauth-excluded-models:\n  meta:\n    - \"muse-spark-1.1\"\n\
             oauth-request-scoped-errors:\n  meta:\n    - status: 400\n      match:\n        \
             - \"context_length_exceeded\"\n      action: \"stop\"\n",
        );
        let aliases = config.oauth_model_alias.get("meta").expect("meta aliases");
        assert_eq!(aliases.len(), 1);
        let alias = aliases.first().expect("an alias");
        assert_eq!(
            (alias.name.as_str(), alias.alias.as_str()),
            ("muse-spark-1.3", "muse-latest")
        );
        assert!(alias.fork && alias.force_mapping);
        assert_eq!(
            config.oauth_excluded_models.get("meta"),
            Some(&strings(&["muse-spark-1.1"]))
        );
        let rules = config
            .oauth_request_scoped_errors
            .get("meta")
            .expect("meta rules");
        assert_eq!(rules.len(), 1);
        let rule = rules.first().expect("a rule");
        assert_eq!((rule.status, rule.action.as_str()), (400, "stop"));
        assert_eq!(rule.matches, ["context_length_exceeded"]);
    }

    #[test]
    fn oauth_request_scoped_errors() {
        let mut text = String::from(
            "\noauth-request-scoped-errors:\n  vertex:\n    - status: 400\n      match:\n        \
             - \"maximum_context_length\"\n        - \"context_length_exceeded\"\n      \
             match-regexr:\n        - \"maximum_context_length$\"\n        \
             - \"^context_length_exceeded\"\n      action: \"stop\"\n",
        );
        for (channel, status, matched, action) in [
            ("aistudio", 400, "invalid_argument", "continue"),
            (
                "antigravity",
                500,
                "internal_server_error",
                "stop-and-cooldown",
            ),
            ("claude", 429, "rate_limit", "continue-and-cooldown"),
            ("codex", 400, "context_window_exceeded", "stop"),
            ("kimi", 400, "length_limit", "stop"),
            ("xai", 400, "max_tokens_exceeded", "stop"),
            ("meta", 400, "context_length_exceeded", "stop"),
        ] {
            text.push_str(&format!(
                "  {channel}:\n    - status: {status}\n      match:\n        - \"{matched}\"\n      \
                 action: \"{action}\"\n"
            ));
        }
        let config = parse(&text);
        assert_eq!(config.oauth_request_scoped_errors.len(), 8);
        let rules = config
            .oauth_request_scoped_errors
            .get("vertex")
            .expect("vertex rules");
        assert_eq!(rules.len(), 1);
        let rule = rules.first().expect("a rule");
        assert_eq!((rule.status, rule.action.as_str()), (400, "stop"));
        assert_eq!((rule.matches.len(), rule.match_regexr.len()), (2, 2));
    }

    #[test]
    fn request_scoped_errors() {
        let config = parse(
            "\ngemini-api-key:\n  - api-key: gemini-key-1\n    request-scoped-errors:\n      \
             - status: 400\n        match:\n          - \"maximum_context_length\"\n          \
             - \"context_length_exceeded\"\n        match-regexr:\n          \
             - \"maximum_context_length$\"\n          - \"^context_length_exceeded\"\n        \
             action: stop\n\
             interactions-api-key:\n  - api-key: interactions-key-1\n    request-scoped-errors:\n      \
             - status: 400\n        match:\n          - \"invalid_argument\"\n        \
             action: continue\n\
             codex-api-key:\n  - api-key: codex-key-1\n    base-url: https://codex.example.com/v1\n    \
             request-scoped-errors:\n      - status: 400\n        match:\n          \
             - \"context_window_exceeded\"\n        action: stop-and-cooldown\n\
             xai-api-key:\n  - api-key: xai-key-1\n    base-url: https://api.x.ai/v1\n    \
             request-scoped-errors:\n      - status: 500\n        match:\n          \
             - \"rate_limit_exceeded\"\n        action: continue-and-cooldown\n\
             claude-api-key:\n  - api-key: claude-key-1\n    request-scoped-errors:\n      \
             - status: 400\n        match:\n          - \"prompt is too long\"\n        \
             match-regexr:\n          - \"too long$\"\n        action: stop\n\
             openai-compatibility:\n  - name: compat\n    base-url: https://compat.example.com/v1\n    \
             request-scoped-errors:\n      - status: 400\n        match:\n          - \"too many tokens\"\n          \
             - \"context length\"\n        match-regexr:\n          - \"tokens? exceeded\"\n          \
             - \"^context\"\n        action: stop\n",
        );
        let compat = &config
            .openai_compatibility
            .first()
            .expect("a compat provider")
            .request_scoped_errors;
        assert_eq!(compat.len(), 1);
        let rule = compat.first().expect("a rule");
        assert_eq!((rule.status, rule.action.as_str()), (400, "stop"));
        assert_eq!((rule.matches.len(), rule.match_regexr.len()), (2, 2));
        let codex = &config
            .codex_api_key
            .first()
            .expect("a codex key")
            .request_scoped_errors;
        assert_eq!(codex.len(), 1);
        let rule = codex.first().expect("a rule");
        assert_eq!(
            (rule.status, rule.action.as_str()),
            (400, "stop-and-cooldown")
        );
        let claude = &config
            .claude_api_key
            .first()
            .expect("a claude key")
            .request_scoped_errors;
        assert_eq!(claude.len(), 1);
        let rule = claude.first().expect("a rule");
        assert_eq!((rule.status, rule.action.as_str()), (400, "stop"));
        assert_eq!((rule.matches.len(), rule.match_regexr.len()), (1, 1));
        let gemini = &config
            .gemini_api_key
            .first()
            .expect("a gemini key")
            .request_scoped_errors;
        assert_eq!(gemini.len(), 1);
        let rule = gemini.first().expect("a rule");
        assert_eq!((rule.status, rule.action.as_str()), (400, "stop"));
        assert_eq!((rule.matches.len(), rule.match_regexr.len()), (2, 2));
        let interactions = &config
            .interactions_api_key
            .first()
            .expect("an interactions key")
            .request_scoped_errors;
        assert_eq!(interactions.len(), 1);
        let rule = interactions.first().expect("a rule");
        assert_eq!((rule.status, rule.action.as_str()), (400, "continue"));
        assert_eq!(rule.matches.len(), 1);
        let xai = &config
            .xai_api_key
            .first()
            .expect("an xai key")
            .request_scoped_errors;
        assert_eq!(xai.len(), 1);
        let rule = xai.first().expect("a rule");
        assert_eq!(
            (rule.status, rule.action.as_str()),
            (500, "continue-and-cooldown")
        );
    }

    // oauth_settings_test.go

    fn setting_lengths(config: &Config, channel: &str) -> Vec<(String, i64)> {
        config
            .oauth_settings
            .get(channel)
            .into_iter()
            .flatten()
            .map(|setting| (setting.name.clone(), setting.max_context_length))
            .collect()
    }

    #[test]
    fn oauth_settings_v8_and_legacy() {
        let v8 = parse(
            "\nconfig-version: 8\noauth:\n  settings:\n    codex:\n      - name: \"gpt-6-sol\"\n        \
             max-context-length: 524288\n      - name: \"deepseek-v4-flash\"\n        \
             max-context-length: 1048576\n",
        );
        assert_eq!(
            setting_lengths(&v8, "codex"),
            [
                ("gpt-6-sol".to_owned(), 524_288),
                ("deepseek-v4-flash".to_owned(), 1_048_576)
            ]
        );
        let legacy = parse(
            "\noauth-settings:\n  codex:\n    - name: \"gpt-6-sol\"\n      max-context-length: 524288\n",
        );
        assert_eq!(
            setting_lengths(&legacy, "codex"),
            [("gpt-6-sol".to_owned(), 524_288)]
        );
    }

    #[test]
    fn duplicate_oauth_settings_later_wins() {
        let config = parse(
            "\nconfig-version: 8\noauth:\n  settings:\n    codex:\n      - name: \"gpt-6-sol\"\n        \
             max-context-length: 524288\n      - name: \"gpt-6-sol\"\n        \
             max-context-length: 1048576\n",
        );
        assert_eq!(
            setting_lengths(&config, "codex"),
            [("gpt-6-sol".to_owned(), 1_048_576)]
        );
    }

    // oauth_scope_test.go

    #[test]
    fn oauth_scope_survives_clones() {
        let config = parse(
            "codex: {stream-bootstrap-buffering: true}\noauth:\n  providers:\n    \
             codex: {disable-codex-cloaking: true, model-level-cooling: true}\n    \
             aistudio: {ws-auth: true}\n    claude:\n      disable-claude-cloak-mode: true\n      \
             header-defaults: {user-agent: oauth-agent}\n    xai: {inject-x-search: true}\n\
             api-keys:\n  codex:\n    - name: api\n      base-url: https://example.invalid\n      \
             keys: [{api-key: test-key, disable-codex-cloaking: true}]\n",
        );
        let cloned = config.clone();
        for value in [&config, &cloned] {
            let api = value.for_api_key();
            assert!(
                !api.ws_auth,
                "the API-key view inherited an OAuth-only setting"
            );
            assert!(api.codex.model_level_cooling && api.codex.stream_bootstrap_buffering);
            assert!(
                api.xai.inject_x_search,
                "the API-key view lost a shared setting"
            );
            assert_eq!(api.codex_api_key.len(), 1);
            assert!(value.ws_auth, "the API-key view changed the shared config");
            assert!(value.xai.inject_x_search);
        }
    }

    #[test]
    fn scope_keeps_legacy_globals() {
        let config = parse(
            "codex: {disable-codex-cloaking: true}\ndisable-claude-cloak-mode: true\nws-auth: false\n",
        );
        let api = config.for_api_key();
        assert!(matches!(api, Cow::Borrowed(borrowed) if std::ptr::eq(borrowed, &config)));
    }

    // shared_upstream_test.go

    const SHARED_LEGACY: &str = "auth-dir: client-auth\nauth-auto-refresh-workers: 3\n\
        codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, \
        stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, \
        model-level-cooling: true, response-steering: true}\n\
        claude: {model-level-cooling: true}\nclaude-code: {disable-cloaking-model-list: true}\n\
        disable-claude-cloak-mode: true\n\
        claude-header-defaults: {user-agent: client-agent, package-version: 1.2.3, \
        runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, \
        stabilize-device-profile: true}\n\
        xai: {inject-x-search: true}\n\
        oauth: {auth-dir: client-auth, auth-auto-refresh-workers: 3, providers: {codex: \
        {header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}}}\n\
        api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, \
        disable-codex-cloaking: false}]}]}\n";

    const SHARED_CANONICAL: &str = "upstream:\n  \
        codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, \
        stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, \
        model-level-cooling: true, response-steering: true}\n  \
        claude:\n    model-level-cooling: true\n    disable-cloaking-model-list: true\n    \
        disable-claude-cloak-mode: true\n    \
        header-defaults: {user-agent: client-agent, package-version: 1.2.3, \
        runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, \
        stabilize-device-profile: true}\n  \
        xai: {inject-x-search: true}\n\
        oauth: {auth-dir: client-auth, auth-auto-refresh-workers: 3, providers: {codex: \
        {header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}}}\n\
        api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, \
        disable-codex-cloaking: false}]}]}\n";

    const SHARED_HISTORICAL: &str = "oauth:\n  auth-dir: client-auth\n  \
        auth-auto-refresh-workers: 3\n  providers:\n    \
        codex: {disable-codex-cloaking: true, stream-bootstrap-buffering: true, \
        stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, \
        model-level-cooling: true, response-steering: true, \
        header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}\n    \
        claude:\n      model-level-cooling: true\n      \
        claude-code: {disable-cloaking-model-list: true}\n      \
        disable-claude-cloak-mode: true\n      \
        header-defaults: {user-agent: client-agent, package-version: 1.2.3, \
        runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, \
        stabilize-device-profile: true}\n    \
        xai: {inject-x-search: true}\n\
        api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, \
        disable-codex-cloaking: false}]}]}\n";

    #[test]
    fn shared_upstream_layouts_agree() {
        // The canonical layout is also what upstream's migration writes for
        // the other two.
        for (name, text) in [
            ("legacy", SHARED_LEGACY),
            ("upstream", SHARED_CANONICAL),
            ("historical OAuth", SHARED_HISTORICAL),
        ] {
            let config = parse(text);
            let cloned = config.clone();
            for view in [&config, &cloned] {
                let api = view.for_api_key();
                for scoped in [view, api.as_ref()] {
                    let actual = (
                        scoped.auth_dir.as_str(),
                        scoped.auth_auto_refresh_workers,
                        scoped.codex.stream_bootstrap_buffering,
                        scoped.codex.stream_bootstrap_timeout.as_str(),
                        scoped.codex.orphan_delegation_compatibility,
                        scoped.codex.model_level_cooling,
                        scoped.codex.response_steering,
                        scoped.claude.model_level_cooling,
                    );
                    let want = ("client-auth", 3, true, "10s", true, true, true, true);
                    assert_eq!(actual, want, "{name}");
                    assert!(scoped.xai.inject_x_search, "{name}");
                }
                assert_eq!(
                    view.codex_header_defaults.beta_features, "oauth-beta",
                    "{name}"
                );
                assert_eq!(api.codex_header_defaults.beta_features, "", "{name}");
                assert_eq!(view.codex_api_key.len(), 1, "{name}");
            }
        }
    }

    #[test]
    fn shared_upstream_presence_wins_over_historical_aliases() {
        for value in ["false", "null"] {
            let text = format!(
                "codex: {{response-steering: true}}\n\
                 oauth: {{providers: {{codex: {{response-steering: true}}, claude: {{header-defaults: \
                 {{user-agent: historical-agent, stabilize-device-profile: true}}}}}}}}\n\
                 upstream: {{codex: {{response-steering: {value}}}, claude: {{header-defaults: \
                 {{user-agent: '', stabilize-device-profile: {value}}}}}}}\n"
            );
            let config = parse(&text);
            assert!(!config.codex.response_steering, "{value}");
            assert_eq!(config.auth_auto_refresh_workers, 0, "{value}");
        }
    }

    // config_v8_test.go

    #[test]
    fn v8_example_loads() {
        let example = include_str!("testdata/config.example.yaml");
        let active = parse(example);
        assert_eq!(active.port, 8317);
        assert_eq!(active.api_keys.len(), 3);
        assert_eq!(active.request_retry, 3);
        assert!(active.quota_exceeded.antigravity_credits);
        assert!(active.codex_api_key.is_empty() && active.claude_api_key.is_empty());
        assert!(active.gemini_api_key.is_empty() && active.vertex_api_key.is_empty());
        assert!(active.xai_api_key.is_empty() && active.meta_api_key.is_empty());
        assert!(active.interactions_api_key.is_empty());
        assert!(active.has_example_api_keys());

        // The provider examples, uncommented as an operator would.
        let text = example.replace("\r\n", "\n");
        let (_, block) = text
            .split_once("# BEGIN API KEY EXAMPLES\n")
            .expect("examples start");
        let (block, _) = block
            .split_once("# END API KEY EXAMPLES")
            .expect("examples end");
        let mut uncommented = String::new();
        for line in block
            .trim_end_matches('\n')
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let line = line.strip_prefix('#').expect("examples stay commented");
            uncommented.push_str(line.strip_prefix(' ').unwrap_or(line));
            uncommented.push('\n');
        }
        let config = parse(&format!("{text}\n{uncommented}"));
        assert_eq!(config.port, 8317);
        assert_eq!(config.api_keys.len(), 3);
        assert_eq!(config.gemini_api_key.len(), 3);
        assert_eq!(config.codex_api_key.len(), 1);
        assert_eq!(config.claude_api_key.len(), 2);
        assert_eq!(config.vertex_api_key.len(), 1);
        assert_eq!(config.xai_api_key.len(), 1);
        assert_eq!(config.meta_api_key.len(), 1);
        assert_eq!(config.interactions_api_key.len(), 1);
        assert!(config.quota_exceeded.antigravity_credits);
        assert!(!config.quota_exceeded.switch_project);
        assert!(!config.quota_exceeded.switch_preview_model);
    }

    #[test]
    fn v8_presence_precedence() {
        for (text, retry, cooling, keys) in [
            (
                "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\n",
                4,
                true,
                1,
            ),
            (
                "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\nrouting:\n  \
                 retry: {request-retry: 0}\n  cooldown: {disable-cooling: false}\n\
                 access: {api-keys: []}\n",
                0,
                false,
                0,
            ),
            (
                "config-version: 8\nrequest-retry: 4\ndisable-cooling: true\napi-keys: [old]\n",
                4,
                true,
                1,
            ),
            (
                "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\n\
                 routing: {retry: {max-retry-credentials: 2}}\n",
                4,
                true,
                1,
            ),
        ] {
            let dir = TempDir::new();
            let path = dir.write("config.yaml", text);
            let config = Config::load(&path).expect("load");
            assert_eq!(
                (
                    config.request_retry,
                    config.disable_cooling,
                    config.api_keys.len()
                ),
                (retry, cooling, keys),
                "{text:?}"
            );
            assert_eq!(std::fs::read_to_string(&path).expect("read back"), text);
        }
    }

    /// A key's inheritable fields: api key, priority, weight, prefix, proxy,
    /// header, model and excluded-model counts, cooling and retry.
    type KeySummary = (
        String,
        i64,
        Option<i64>,
        String,
        String,
        usize,
        usize,
        usize,
        Option<bool>,
        Option<i64>,
    );

    fn key_summaries(config: &Config, provider: &str) -> Vec<KeySummary> {
        if provider == "gemini" || provider == "interactions" {
            let summary = |k: &crate::config::GeminiKey| {
                (
                    k.api_key.clone(),
                    k.priority,
                    k.weight,
                    k.prefix.clone(),
                    k.proxy_url.clone(),
                    k.headers.len(),
                    k.models.len(),
                    k.excluded_models.len(),
                    k.disable_cooling,
                    k.request_retry,
                )
            };
            let keys = if provider == "gemini" {
                &config.gemini_api_key
            } else {
                &config.interactions_api_key
            };
            keys.iter().map(summary).collect()
        } else if provider == "vertex" {
            let summary = |k: &crate::config::VertexCompatKey| {
                (
                    k.api_key.clone(),
                    k.priority,
                    k.weight,
                    k.prefix.clone(),
                    k.proxy_url.clone(),
                    k.headers.len(),
                    k.models.len(),
                    k.excluded_models.len(),
                    k.disable_cooling,
                    k.request_retry,
                )
            };
            config.vertex_api_key.iter().map(summary).collect()
        } else if ["codex", "xai", "meta"].contains(&provider) {
            let summary = |k: &crate::config::CodexKey| {
                (
                    k.api_key.clone(),
                    k.priority,
                    k.weight,
                    k.prefix.clone(),
                    k.proxy_url.clone(),
                    k.headers.len(),
                    k.models.len(),
                    k.excluded_models.len(),
                    k.disable_cooling,
                    k.request_retry,
                )
            };
            let keys = match provider {
                "xai" => &config.xai_api_key,
                "meta" => &config.meta_api_key,
                _ => &config.codex_api_key,
            };
            keys.iter().map(summary).collect()
        } else {
            let summary = |k: &crate::config::ClaudeKey| {
                (
                    k.api_key.clone(),
                    k.priority,
                    k.weight,
                    k.prefix.clone(),
                    k.proxy_url.clone(),
                    k.headers.len(),
                    k.models.len(),
                    k.excluded_models.len(),
                    k.disable_cooling,
                    k.request_retry,
                )
            };
            config.claude_api_key.iter().map(summary).collect()
        }
    }

    #[test]
    fn v8_key_inheritance() {
        for provider in [
            "gemini",
            "interactions",
            "vertex",
            "codex",
            "claude",
            "xai",
            "meta",
        ] {
            let text = format!(
                "request-retry: 9\napi-keys:\n  {provider}:\n    - name: shared\n      \
                 base-url: https://example.invalid\n      priority: 7\n      prefix: group\n      \
                 proxy-url: direct\n      headers: {{X-Group: yes}}\n      \
                 models: [{{name: model, alias: alias}}]\n      excluded-models: [blocked]\n      \
                 disable-cooling: true\n      request-retry: 3\n      keys:\n        \
                 - api-key: inherited\n          priority: null\n          headers: null\n          \
                 disable-cooling: null\n          request-retry: null\n        \
                 - api-key: overridden\n          weight: 0\n          priority: 0\n          \
                 prefix: ''\n          proxy-url: ''\n          headers: {{}}\n          models: []\n          \
                 excluded-models: []\n          disable-cooling: false\n          request-retry: 0\n        \
                 - api-key: global-retry\n          request-retry: -1\n"
            );
            let config = parse(&text);
            assert_eq!(config.request_retry, 9);
            // `n` is the header, model and excluded-model count; the proxy
            // comes with the group.
            let summary = |key: &str,
                           priority: i64,
                           weight: Option<i64>,
                           prefix: &str,
                           n: usize,
                           cooling: Option<bool>,
                           retry: Option<i64>|
             -> KeySummary {
                let proxy = if n == 0 { "" } else { "direct" };
                let (key, prefix, proxy) = (key.to_owned(), prefix.to_owned(), proxy.to_owned());
                (
                    key, priority, weight, prefix, proxy, n, n, n, cooling, retry,
                )
            };
            assert_eq!(
                key_summaries(&config, provider),
                [
                    summary("inherited", 7, None, "group", 1, Some(true), Some(3)),
                    summary("overridden", 0, Some(0), "", 0, Some(false), Some(0)),
                    summary("global-retry", 7, None, "group", 1, Some(true), Some(-1)),
                ],
                "{provider}"
            );
        }
        let config = parse(
            "api-keys:\n  codex:\n    - base-url: https://example.invalid\n      \
             headers: {X-Group: yes}\n      keys: [{api-key: k}]\n",
        );
        let headers = config.codex_api_key.first().map(|k| k.headers.clone());
        assert_eq!(
            headers,
            Some(BTreeMap::from([("X-Group".to_owned(), "yes".to_owned())]))
        );
    }

    /// Upstream's `TestV8MigrationPreservesLegacySemantics` input.
    const SEMANTICS_LEGACY: &str = "host: 127.0.0.1\nport: 8317\napi-keys: [client]\n\
        request-retry: 0\nws-auth: false\n\
        quota-exceeded: {switch-project: true, switch-preview-model: true, antigravity-credits: true}\n\
        codex-api-key:\n  - api-key: a\n    base-url: https://example.invalid\n    \
        headers: {X-Test: first}\n    request-retry: 0\n    disable-cooling: false\n  \
        - api-key: b\n    base-url: https://example.invalid\n    headers: {X-Test: second}\n    \
        request-retry: -1\n\
        openai-compatibility:\n  - name: compatible\n    base-url: https://example.invalid\n    \
        api-key-entries: [{api-key: a, weight: 0}, {api-key: b, proxy-url: direct}]\n";

    /// What upstream's `NormalizeConfigLayout(SEMANTICS_LEGACY, true)` writes.
    const SEMANTICS_V8: &str = "quota-exceeded: {switch-project: true, switch-preview-model: true}\n\
        access:\n    api-keys: [client]\nserver:\n    host: 127.0.0.1\n    port: 8317\n\
        routing:\n    retry:\n        request-retry: 0\n\
        oauth:\n    providers:\n        antigravity:\n            antigravity-credits: true\n        \
        aistudio:\n            ws-auth: false\n\
        api-keys:\n    codex:\n        - name: codex-1\n          base-url: https://example.invalid\n          \
        headers: {X-Test: first}\n          request-retry: 0\n          disable-cooling: false\n          \
        keys:\n            - api-key: a\n        - name: codex-2\n          \
        base-url: https://example.invalid\n          headers: {X-Test: second}\n          \
        request-retry: -1\n          keys:\n            - api-key: b\n    \
        openai-compatibility:\n        - name: compatible\n          base-url: https://example.invalid\n          \
        keys: [{api-key: a, weight: 0}, {api-key: b, proxy-url: direct}]\n\
        config-version: 8\n";

    #[test]
    fn v8_migration_preserves_legacy_semantics() {
        let before = parse(SEMANTICS_LEGACY);
        let mut after = parse(SEMANTICS_V8);
        assert!(after.oauth_only_fields().contains("ws-auth"));
        assert!(
            after
                .oauth_only_fields()
                .contains("quota-exceeded.antigravity-credits")
        );
        // Scope metadata is added when fields move under oauth.providers.
        after.oauth_only_fields.clear();
        assert_eq!(before, after);
        assert_eq!(after.codex_api_key.len(), 2);
        assert!(after.quota_exceeded.switch_project && after.quota_exceeded.switch_preview_model);
    }

    /// Every typed field, in the legacy layout.
    const TYPED_LEGACY: &str = "host: 0.0.0.0\nport: 9000\ntrusted-proxies: [10.0.0.0/8]\n\
        tls: {enable: true, cert: c.pem, key: k.pem}\n\
        remote-management: {allow-remote: true, secret-key: s, disable-control-panel: true, \
        panel-github-repository: ' https://example.invalid/panel '}\n\
        auth-dir: ~/auth\napi-keys: [one, two]\ndebug: true\nlogging-to-file: true\n\
        request-log: true\nproxy-url: socks5://127.0.0.1:1080\npassthrough-headers: true\n\
        streaming: {keepalive-seconds: 15, bootstrap-retries: 2}\nnonstream-keepalive-interval: 7\n\
        disable-cooling: true\ntransient-error-cooldown-seconds: 30\nauth-auto-refresh-workers: 4\n\
        request-retry: 2\nmax-retry-credentials: 3\nmax-retry-interval: 60\n\
        quota-exceeded: {switch-project: true, switch-preview-model: true, antigravity-credits: true}\n\
        routing: {strategy: fill-first}\nws-auth: true\nforce-model-prefix: true\n\
        codex: {stream-bootstrap-buffering: true, stream-bootstrap-timeout: 20s, \
        orphan-delegation-compatibility: true, model-level-cooling: true, response-steering: true, \
        optimize-multi-agent-v2: true}\n\
        codex-header-defaults: {beta-features: ' b1,b2 '}\nclaude: {model-level-cooling: true}\n\
        codex-api-key:\n  - api-key: ck\n    priority: 2\n    weight: 5\n    prefix: /team/\n    \
        base-url: https://codex.invalid\n    websockets: true\n    proxy-url: direct\n    \
        models: [{name: m, alias: a, display-name: D, max-context-length: 1000, force-mapping: true, \
        is-compat: true, support-configuration-update: true}]\n    headers: {X-A: ' 1 '}\n    \
        excluded-models: [M1, m1]\n    disable-cooling: false\n    request-retry: 1\n    \
        request-scoped-errors: [{status: 400, match: [x], action: STOP}]\n\
        claude-api-key:\n  - api-key: ak\n    base-url: https://claude.invalid\n    \
        models: [{name: c, alias: ca, thinking: {min: 1, max: 2, zero-allowed: true, levels: [low]}}]\n    \
        rebuild-mid-system-message: true\n\
        oauth-excluded-models: {Codex: [A, a]}\n\
        oauth-model-alias: {codex: [{name: n, alias: al, fork: true}]}\n\
        oauth-request-scoped-errors: {claude: [{status: 429, match: [rate], action: continue}]}\n\
        oauth-settings: {codex: [{name: n, max-context-length: 5}]}\n\
        client: {codex: {enable-apply-patch: true}}\n";

    /// What upstream's `NormalizeConfigLayout(TYPED_LEGACY, true)` writes.
    const TYPED_V8: &str = "quota-exceeded: {switch-project: true, switch-preview-model: true}\n\
        routing: {strategy: fill-first, force-model-prefix: true, cooldown: {disable-cooling: true, \
        transient-error-cooldown-seconds: 30}, retry: {request-retry: 2, max-retry-credentials: 3, \
        max-retry-interval: 60}}\n\
        client: {codex: {enable-apply-patch: true, optimize-multi-agent-v2: true}}\n\
        requests:\n    proxy-url: socks5://127.0.0.1:1080\n    passthrough-headers: true\n    \
        streaming:\n        keepalive-seconds: 15\n        bootstrap-retries: 2\n    \
        nonstream-keepalive-interval: 7\n\
        observability:\n    logs:\n        request-log: true\n        debug: true\n        \
        logging-to-file: true\n\
        access:\n    api-keys: [one, two]\n\
        server:\n    host: 0.0.0.0\n    port: 9000\n    trusted-proxies: [10.0.0.0/8]\n    tls:\n        \
        enable: true\n        cert: c.pem\n        key: k.pem\n\
        management:\n    allow-remote: true\n    secret-key: s\n    disable-control-panel: true\n    \
        panel-github-repository: ' https://example.invalid/panel '\n\
        oauth:\n    auth-dir: ~/auth\n    auth-auto-refresh-workers: 4\n    providers:\n        \
        antigravity:\n            antigravity-credits: true\n        aistudio:\n            \
        ws-auth: true\n        codex:\n            header-defaults:\n                \
        beta-features: ' b1,b2 '\n    excluded-models: {Codex: [A, a]}\n    \
        model-alias: {codex: [{name: n, alias: al, fork: true}]}\n    \
        request-scoped-errors: {claude: [{status: 429, match: [rate], action: continue}]}\n    \
        settings: {codex: [{name: n, max-context-length: 5}]}\n\
        upstream:\n    codex:\n        stream-bootstrap-buffering: true\n        \
        stream-bootstrap-timeout: 20s\n        orphan-delegation-compatibility: true\n        \
        model-level-cooling: true\n        response-steering: true\n    claude:\n        \
        model-level-cooling: true\n\
        api-keys:\n    codex:\n        - name: codex-1\n          priority: 2\n          \
        prefix: /team/\n          base-url: https://codex.invalid\n          proxy-url: direct\n          \
        models: [{name: m, alias: a, display-name: D, max-context-length: 1000, force-mapping: true, \
        is-compat: true, support-configuration-update: true}]\n          headers: {X-A: ' 1 '}\n          \
        excluded-models: [M1, m1]\n          disable-cooling: false\n          request-retry: 1\n          \
        request-scoped-errors: [{status: 400, match: [x], action: STOP}]\n          keys:\n            \
        - api-key: ck\n              weight: 5\n              websockets: true\n    claude:\n        \
        - name: claude-1\n          base-url: https://claude.invalid\n          \
        models: [{name: c, alias: ca, thinking: {min: 1, max: 2, zero-allowed: true, levels: [low]}}]\n          \
        keys:\n            - api-key: ak\n              rebuild-mid-system-message: true\n\
        config-version: 8\n";

    #[test]
    fn legacy_and_v8_layouts_type_the_same_values() {
        let legacy = parse(TYPED_LEGACY);
        let mut v8 = parse(TYPED_V8);
        assert!(legacy.oauth_only_fields().is_empty());
        assert_eq!(
            v8.oauth_only_fields()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "codex-header-defaults.beta-features",
                "quota-exceeded.antigravity-credits",
                "ws-auth"
            ]
        );
        let api = v8.for_api_key().into_owned();
        assert!(!api.ws_auth && !api.quota_exceeded.antigravity_credits);
        assert_eq!(api.codex_header_defaults.beta_features, "");
        v8.oauth_only_fields.clear();
        assert_eq!(legacy, v8);

        assert_eq!((legacy.host.as_str(), legacy.port), ("0.0.0.0", 9000));
        assert_eq!(legacy.tls.cert, "c.pem");
        assert_eq!(
            legacy.remote_management.panel_github_repository,
            "https://example.invalid/panel"
        );
        assert_eq!(legacy.auth_dir, "~/auth");
        assert!(legacy.debug && legacy.logging_to_file && legacy.request_log);
        assert!(legacy.passthrough_headers);
        assert_eq!(legacy.streaming.bootstrap_retries, 2);
        assert_eq!(legacy.transient_error_cooldown_seconds, 30);
        assert_eq!(legacy.auth_auto_refresh_workers, 4);
        assert_eq!(legacy.max_retry_interval, 60);
        assert_eq!(legacy.routing_strategy(), RoutingStrategy::FillFirst);
        assert!(legacy.force_model_prefix && legacy.ws_auth);
        assert_eq!(legacy.codex.stream_bootstrap_timeout, "20s");
        assert!(legacy.client.codex.optimize_multi_agent_v2);
        assert!(legacy.client.codex.enable_apply_patch);
        assert_eq!(legacy.codex_header_defaults.beta_features, "b1,b2");
        let codex = legacy.codex_api_key.first().expect("a codex key");
        assert_eq!(
            (codex.priority, codex.weight, codex.prefix.as_str()),
            (2, Some(5), "team")
        );
        assert!(codex.websockets);
        assert_eq!(
            codex.headers,
            BTreeMap::from([("X-A".to_owned(), "1".to_owned())])
        );
        assert_eq!(codex.excluded_models, ["m1"]);
        let claude = legacy.claude_api_key.first().expect("a claude key");
        assert!(claude.rebuild_mid_system_message);
        let thinking = claude
            .models
            .first()
            .and_then(|m| m.thinking.as_ref())
            .expect("thinking");
        assert_eq!(
            (thinking.min, thinking.max, thinking.zero_allowed),
            (1, 2, true)
        );
        assert_eq!(
            legacy.oauth_excluded_models,
            BTreeMap::from([("codex".to_owned(), strings(&["a"]))])
        );
        let rules = legacy
            .oauth_request_scoped_errors
            .get("claude")
            .expect("claude rules");
        assert_eq!(
            rules.first().map(|rule| rule.action.as_str()),
            Some("continue")
        );
    }

    #[test]
    fn v8_empty_legacy_containers_keep_new_values() {
        let config = parse(
            "port: 8317\ntls: null\ncodex: {disable-codex-cloaking: true, live-media-relay: {}}\n\
             server: {tls: {enable: true, cert: server.crt, key: server.key}}\n\
             oauth: {providers: {codex: {live-media-relay: {max-sessions: 12}}}}\n",
        );
        assert!(config.tls.enable);
        assert_eq!(
            (config.tls.cert.as_str(), config.tls.key.as_str()),
            ("server.crt", "server.key")
        );
    }

    #[test]
    fn v8_empty_legacy_containers_match_their_new_paths() {
        let base = "port: 8317\nplugins: {configs: {sample: {enabled: false, options: {}}}}\n";
        for (old, current) in [
            ("tls", "server.tls"),
            ("remote-management", "management"),
            ("pprof", "observability.pprof"),
            ("discovery", "server.discovery"),
            ("discovery.interfaces", "server.discovery.interfaces"),
            ("credential-concurrency", "credentials.concurrency"),
            ("credential-in-flight", "credentials.in-flight"),
            ("streaming", "requests.streaming"),
            ("payload", "requests.payload"),
            ("codex", "oauth.providers.codex"),
            (
                "codex.live-media-relay",
                "oauth.providers.codex.live-media-relay",
            ),
            (
                "codex-header-defaults",
                "oauth.providers.codex.header-defaults",
            ),
            ("claude", "upstream.claude"),
            ("claude-code", "upstream.claude"),
            ("claude-header-defaults", "upstream.claude.header-defaults"),
            ("antigravity", "oauth.providers.antigravity"),
            (
                "antigravity.connection-pool",
                "oauth.providers.antigravity.connection-pool",
            ),
            ("xai", "upstream.xai"),
            ("devin", "oauth.providers.devin"),
        ] {
            let mut after = parse(&format!("{base}{}\n", nest(current, "{}")));
            after.oauth_only_fields.clear();
            for empty in ["{}", "null"] {
                let before = parse(&format!("{base}{}\n", nest(old, empty)));
                assert_eq!(before, after, "{old}: {empty}");
                assert_eq!(before.port, 8317);
            }
        }
    }

    #[test]
    fn v8_rejects_invalid_groups() {
        for text in [
            "api-keys: {codex: [{name: a, keys: [{api-key: a, weight: 1.5}]}]}",
            "api-keys: {codex: [{name: a, keys: [{api-key: a, base-url: https://invalid}]}]}",
            "api-keys: {codex: [{name: a, keys: {api-key: a}}]}",
            "server: true",
            "config-version: 9",
            "server: {port: bad}",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn legacy_write_layouts_still_load() {
        for text in [
            "debug: true",
            "server: {port: 8317}\nport: 8318",
            "api-keys: [client]",
            "codex-api-key: []",
            "codex: {}",
            "quota-exceeded: {antigravity-credits: true}",
            "home: {enabled: true}",
            "enable-gemini-cli-endpoint: true",
            "unknown-root: true",
            "<<: {debug: true}",
        ] {
            assert!(Config::parse(text).is_ok(), "{text:?}");
        }
        assert_eq!(parse("server: {port: 8317}\nport: 8318").port, 8317);
        assert!(parse("<<: {debug: true}").debug);
    }

    /// Upstream's `TestV8AliasesAndMergeKeys` input.
    const ALIASES_LEGACY: &str = "routing: &routing\n  strategy: round-robin\n  \
        retry: {request-retry: 0}\n\
        codex-api-key:\n  - &key\n    api-key: first\n    base-url: https://example.invalid\n    \
        request-retry: 0\n  - <<: *key\n    api-key: second\n\
        api-keys:\n  gemini:\n    - &upstream\n      name: first\n      \
        base-url: https://example.invalid\n      request-retry: 2\n      keys: [{api-key: one}]\n    \
        - <<: *upstream\n      name: second\n      keys: [{api-key: two, request-retry: 0}]\n";

    /// What upstream's `NormalizeConfigLayout(ALIASES_LEGACY, true)` writes.
    const ALIASES_V8: &str = "routing:\n    strategy: round-robin\n    retry: {request-retry: 0}\n\
        api-keys:\n    gemini:\n        - name: first\n          base-url: https://example.invalid\n          \
        request-retry: 2\n          keys: [{api-key: one}]\n        - name: second\n          \
        keys: [{api-key: two, request-retry: 0}]\n          base-url: https://example.invalid\n          \
        request-retry: 2\n    \
        codex:\n        - name: codex-1\n          base-url: https://example.invalid\n          \
        request-retry: 0\n          keys:\n            - api-key: first\n        - name: codex-2\n          \
        base-url: https://example.invalid\n          request-retry: 0\n          keys:\n            \
        - api-key: second\n\
        config-version: 8\n";

    #[test]
    fn v8_aliases_and_merge_keys() {
        let before = parse(ALIASES_LEGACY);
        let after = parse(ALIASES_V8);
        assert_eq!(before, after);
        let keys: Vec<(&str, Option<i64>)> = after
            .codex_api_key
            .iter()
            .map(|k| (k.api_key.as_str(), k.request_retry))
            .collect();
        assert_eq!(keys, [("first", Some(0)), ("second", Some(0))]);
        let keys: Vec<(&str, Option<i64>)> = after
            .gemini_api_key
            .iter()
            .map(|k| (k.api_key.as_str(), k.request_retry))
            .collect();
        assert_eq!(keys, [("one", Some(2)), ("two", Some(0))]);
        // The same groups under codex:
        let config = parse(
            "api-keys:\n  codex:\n    - &upstream\n      name: first\n      \
             base-url: https://example.invalid\n      request-retry: 2\n      \
             keys: [{api-key: one}]\n    - <<: *upstream\n      name: second\n      \
             keys: [{api-key: two, request-retry: 0}]\n",
        );
        let keys: Vec<(&str, Option<i64>)> = config
            .codex_api_key
            .iter()
            .map(|k| (k.api_key.as_str(), k.request_retry))
            .collect();
        assert_eq!(keys, [("one", Some(2)), ("two", Some(0))]);
    }

    /// Not upstream's: settings whose aliases expand to more than 64 MiB of
    /// text don't load, though the tree holds them in a little more than
    /// one copy; yaml.v3 loads them.
    #[test]
    fn excessive_aliasing_fails() {
        let long = "x".repeat(1 << 20);
        let copies = vec!["*big"; 65].join(", ");
        let text = format!("unused: &big {long}\napi-keys: [{copies}]\n");
        let error = Config::parse(&text).expect_err("too much aliased text");
        assert_eq!(
            (error.kind(), error.to_string()),
            (
                Decode,
                format!("{PARSE}yaml: document contains excessive aliasing")
            )
        );
    }

    #[test]
    fn v8_legacy_null_routing() {
        for text in [
            "port: 8317\nrouting: null\n",
            "port: 8317\nrouting: ~\n",
            "port: 8317\nrouting:\n",
            "port: 8317\nrouting: null\nrequest-retry: 3\n",
            "server: {port: 8317}\nrouting: null\n",
        ] {
            let dir = TempDir::new();
            let path = dir.write("config.yaml", text);
            let config = Config::load(&path).expect("load");
            assert_eq!(config.port, 8317, "{text:?}");
            assert_eq!(config.routing, RoutingConfig::default(), "{text:?}");
            assert_eq!(parse(text), config);
            assert_eq!(std::fs::read_to_string(&path).expect("read back"), text);
        }
        assert_eq!(
            parse("port: 8317\nrouting: null\nrequest-retry: 3\n").request_retry,
            3
        );
        for text in [
            "routing: false",
            "routing: []",
            "routing: {retry: false}",
            "server: null",
        ] {
            assert!(Config::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn v8_management_references_resolve() {
        // Upstream also writes the key's bcrypt hash back to the file; this
        // port keeps it as written.
        const SECRET: &str = "test-management-reference-secret";
        for (text, allow_remote) in [
            (
                format!(
                    "defaults: &management\n  secret-key: {SECRET}\n  allow-remote: true\n\
                     management: *management\nother: *management\n"
                ),
                true,
            ),
            (
                format!(
                    "defaults: &management\n  secret-key: {SECRET}\n  allow-remote: true\n\
                     management: {{<<: *management, allow-remote: false}}\nother: *management\n"
                ),
                false,
            ),
            (
                format!(
                    "password: &password {SECRET}\n\
                     management: {{secret-key: *password, allow-remote: true}}\n"
                ),
                true,
            ),
            (
                format!(
                    "defaults: &root\n  management: {{secret-key: {SECRET}, allow-remote: true}}\n\
                     <<: *root\n"
                ),
                true,
            ),
            (
                format!(
                    "remote-management: {{secret-key: stale-secret, allow-remote: false}}\n\
                     defaults: &management {{secret-key: {SECRET}, allow-remote: true}}\n\
                     management: *management\n"
                ),
                true,
            ),
            (
                format!(
                    "defaults: &management\n  secret-key: {SECRET}\n  allow-remote: true\n\
                     remote-management: *management\nother: *management\n"
                ),
                true,
            ),
        ] {
            let config = parse(&format!("{text}# Keep this comment\nport: 8317\n"));
            assert_eq!(config.remote_management.secret_key, SECRET, "{text:?}");
            assert_eq!(
                config.remote_management.allow_remote, allow_remote,
                "{text:?}"
            );
            assert_eq!(config.port, 8317);
        }
    }

    // Not upstream's: LoadConfig's defaults and clamps for the logging,
    // usage and cooldown settings (config_load.go:67-73 and 143-155), with
    // their v8 paths.
    #[test]
    fn observability_settings_load_with_upstream_defaults() {
        let limits = |config: &Config| {
            (
                config.logs_max_total_size_mb,
                config.error_logs_max_files,
                config.redis_usage_queue_retention_seconds,
            )
        };
        assert_eq!(limits(&defaults()), (0, 10, 60));
        assert_eq!(limits(&parse("port: 1\n")), (0, 10, 60));
        let config = parse(
            "logs-max-total-size-mb: -5\nerror-logs-max-files: -1\n\
             redis-usage-queue-retention-seconds: 0\n",
        );
        assert_eq!(limits(&config), (0, 10, 60));
        let config = parse(
            "logs-max-total-size-mb: 7\nerror-logs-max-files: 0\n\
             redis-usage-queue-retention-seconds: 9000\n",
        );
        assert_eq!(limits(&config), (7, 0, 3600));
        let config = parse(
            "server: {commercial-mode: true}\n\
             observability:\n  logs: {logs-max-total-size-mb: 2, error-logs-max-files: 3}\n  \
             usage: {usage-statistics-enabled: true, redis-usage-queue-retention-seconds: 120}\n\
             routing: {cooldown: {save-cooldown-status: true}}\n",
        );
        assert_eq!(limits(&config), (2, 3, 120));
        assert!(config.commercial_mode);
        assert!(config.usage_statistics_enabled);
        assert!(config.save_cooldown_status);
    }

    // Not upstream's: the payload section decodes as yaml.v3 decodes it,
    // keeping params in file order, and LoadConfig's SanitizePayloadRules
    // drops raw rules that aren't JSON.
    #[test]
    fn payload_rules_load() {
        use crate::config::{AnyValue, PayloadModelRule};

        let config = parse(
            "payload:\n  default:\n    - models: [{name: gpt-*, protocol: codex, \
             headers: {X-Team: a*}, match: [{a: 1}], exist: [b]}]\n      \
             params: {z: 1, a: [x, ~]}\n  default-raw:\n    - params: {x: '{'}\n    \
             - params: {y: '{\"k\":1}'}\n  filter:\n    - models: [{name: m}]\n      \
             params: [a.b]\n",
        );
        let payload = &config.payload;
        let [rule] = payload.default.as_slice() else {
            panic!("one default rule: {payload:?}");
        };
        assert_eq!(
            rule.params,
            [
                ("z".to_owned(), AnyValue::Int(1)),
                (
                    "a".to_owned(),
                    AnyValue::Seq(vec![AnyValue::Str("x".into()), AnyValue::Null])
                ),
            ]
        );
        assert_eq!(
            rule.models,
            [PayloadModelRule {
                name: "gpt-*".into(),
                protocol: "codex".into(),
                headers: BTreeMap::from([("X-Team".into(), "a*".into())]),
                r#match: vec![BTreeMap::from([("a".into(), AnyValue::Int(1))])],
                exist: strings(&["b"]),
                ..PayloadModelRule::default()
            }]
        );
        assert_eq!(payload.default_raw.len(), 1);
        assert_eq!(payload.filter.len(), 1);
        assert_eq!(
            payload.filter.first().map(|rule| rule.params.clone()),
            Some(strings(&["a.b"]))
        );
        let v8 = parse("requests:\n  payload:\n    override:\n      - params: {c: true}\n");
        assert_eq!(v8.payload.r#override.len(), 1);
        assert_eq!(
            Config::parse("payload: {default: 5, filter: [{params: {a: 1}}]}\n")
                .map_err(|error| error.to_string()),
            Err(format!(
                "{PARSE}yaml: unmarshal errors:\n  \
                 line 1: cannot unmarshal !!int into []config.PayloadRule\n  \
                 line 1: cannot unmarshal !!map into []string"
            ))
        );
    }

    // Not upstream's: model_catalogs_test.go's TestModelCatalogConfigValidation
    // (v8.0.15) has upstream refuse the first six sources. The catalog
    // sources aren't ported, so `models` is read and ignored, and they load.
    #[test]
    fn model_catalog_sources_are_ignored() {
        let base = parse("port: 8317\n");
        for field in ["catalog", "codex-catalog", "devin-catalog"] {
            for source in [
                "relative.json",
                "./models.json",
                "~/models.json",
                "ftp://example.com/models",
                "file:///tmp/models.json",
                "https:///models",
                "https://example.com/models.json",
                "",
            ] {
                let config = parse(&format!("port: 8317\nmodels:\n  {field}: '{source}'\n"));
                assert_eq!(config, base, "{field}: {source}");
            }
        }
        assert_eq!(parse("port: 8317\nmodels: {}\n"), base);
    }
}
