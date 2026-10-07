//! What of open-ferry's own environment reaches Claude Code. This changes
//! the process's variables, so it has a test binary of its own, with one
//! test, which sets them before anything else runs.

mod common;

use std::collections::BTreeSet;

use open_ferry_core::config::ClaudeCli;
use open_ferry_core::exec::Format;
use serde_json::{Value, json};

use common::{FAKE, Fixture, claude_body, execute, success};

/// A made-up token: never a real one.
const TOKEN: &str = "test-token-not-real";

/// The variables the test gives open-ferry that Claude Code mustn't get:
/// ones that would point it elsewhere, change how it runs, or say it runs
/// inside another Claude Code.
const SCRUBBED: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_USE_BEDROCK",
    "MAX_THINKING_TOKENS",
];

fn names(record: &Value) -> BTreeSet<String> {
    record["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_ascii_uppercase())
        .collect()
}

#[test]
fn scrubs_the_environment_and_keeps_the_token_only_without_a_config_dir() {
    let inherited = Fixture::new(&json!({}));
    let mut scenario = success(&["Hi"]);
    scenario["record_values"] = json!(["OPEN_FERRY_TEST_KEEP"]);
    inherited.set_scenario(&scenario);
    // SAFETY: no other thread runs yet: the runtime is built below.
    unsafe {
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", TOKEN);
        std::env::set_var("ANTHROPIC_API_KEY", "test-key-not-real");
        std::env::set_var("ANTHROPIC_BASE_URL", "http://127.0.0.1:9");
        std::env::set_var("CLAUDECODE", "1");
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
        std::env::set_var("MAX_THINKING_TOKENS", "31999");
        std::env::set_var("OPEN_FERRY_TEST_KEEP", "kept");
        std::env::set_var("CLAUDE_CONFIG_DIR", inherited.config_dir());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // An entry with its own config directory: the token is someone
    // else's, so it goes too.
    let own = Fixture::new(&scenario);
    runtime
        .block_on(execute(
            &own,
            &own.entry(),
            Format::CLAUDE,
            claude_body("Hello"),
        ))
        .unwrap();
    let record = own.record();
    let got = names(&record);
    assert!(!got.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{got:?}");
    for name in SCRUBBED {
        assert!(!got.contains(name), "{name}: {got:?}");
    }
    assert_eq!(
        record["env"],
        json!({
            "CLAUDE_CONFIG_DIR": own.config_dir().display().to_string(),
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS": "1024",
            "OPEN_FERRY_TEST_KEEP": "kept",
        })
    );

    // An entry without one runs as the user's own Claude Code would, in
    // the directory it was given, with their token.
    let entry = ClaudeCli {
        name: "inherits".into(),
        command: FAKE.into(),
        ..ClaudeCli::default()
    };
    runtime
        .block_on(execute(
            &inherited,
            &entry,
            Format::CLAUDE,
            claude_body("Hello"),
        ))
        .unwrap();
    let record = inherited.record();
    let got = names(&record);
    assert!(got.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{got:?}");
    for name in SCRUBBED {
        assert!(!got.contains(name), "{name}: {got:?}");
    }
    assert_eq!(
        record["env"]["CLAUDE_CONFIG_DIR"],
        inherited.config_dir().display().to_string()
    );

    // The token is never an argument nor part of the input, and isn't in
    // the record at all.
    for record in [own.record(), inherited.record()] {
        assert!(!record.to_string().contains(TOKEN), "{record}");
    }
}
