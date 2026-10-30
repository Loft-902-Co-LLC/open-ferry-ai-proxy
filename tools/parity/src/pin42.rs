//! Regression inputs for the v8.0.42 ports. Each saved input runs through
//! the ordinary suite reader and strict comparison, including registry pairs.

use serde::Deserialize;
use std::sync::OnceLock;

use crate::cases::Case;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture<'a> {
    suites: Vec<String>,
    name: String,
    model: String,
    request: String,
    #[serde(default)]
    translated_request: String,
    #[serde(default)]
    events: Vec<String>,
    #[serde(default)]
    options: serde_json::Value,
    #[serde(default, borrow)]
    known_difference: Option<&'a str>,
}

const INPUTS: &[&str] = &[
    include_str!("cases/testdata/pin42/thinking.json"),
    include_str!("cases/testdata/pin42/schema.json"),
    include_str!("cases/testdata/pin42/arguments.json"),
    include_str!("cases/testdata/pin42/signatures.json"),
    include_str!("cases/testdata/pin42/tools.json"),
    include_str!("cases/testdata/pin42/terminal.json"),
    include_str!("cases/testdata/pin42/call-ids.json"),
    include_str!("cases/testdata/pin42/feedback.json"),
    include_str!("cases/testdata/pin42/usage.json"),
    include_str!("cases/testdata/pin42/speed.json"),
    include_str!("cases/testdata/pin42/outcomes.json"),
    include_str!("cases/testdata/pin42/search.json"),
    include_str!("cases/testdata/pin42/commentary.json"),
];

/// The saved inputs for a suite, in fixture order.
pub fn for_suite(key: &str) -> Vec<Case> {
    static FIXTURES: OnceLock<Vec<Fixture<'static>>> = OnceLock::new();
    FIXTURES
        .get_or_init(|| {
            INPUTS
                .iter()
                .flat_map(|input| {
                    serde_json::from_str::<Vec<Fixture<'static>>>(input).expect("pin42 fixtures")
                })
                .collect()
        })
        .iter()
        .filter(|fixture| fixture.suites.iter().any(|suite| suite == key))
        .cloned()
        .map(|fixture| Case {
            name: fixture.name,
            model: fixture.model,
            request: fixture.request,
            translated_request: fixture.translated_request,
            events: fixture.events,
            options: fixture.options,
            known_difference: fixture.known_difference,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn native_search_option_reaches_both_responses_request_entries() {
        use crate::translator::Translator;
        for translator in [
            Translator::ClaudeResponsesRequest,
            Translator::ClaudeResponsesRequestCompat,
        ] {
            let cases = for_suite(translator.key());
            let case = cases
                .iter()
                .find(|case| case.name == "pin42-search-tool_search_output-none-True")
                .unwrap();
            let enabled = translator.run_rust(case).unwrap();
            assert!(
                enabled["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "tool_search")
            );
            let mut disabled = case.clone();
            disabled.options["native_tool_search"] = false.into();
            let disabled = translator.run_rust(&disabled).unwrap();
            assert!(
                !disabled["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == "tool_search")
            );
        }
    }

    #[test]
    fn argument_traces_keep_completion_boundaries_and_one_json_value() {
        let cases = for_suite("gemini/claude/response");
        for (name, immediate, expected) in [
            (
                "implicit-empty-real-finish",
                "",
                serde_json::json!({"path": "x"}),
            ),
            ("implicit-final-empty-finish", "", serde_json::json!({})),
            ("closed-empty-real-finish", "{}", serde_json::json!({})),
        ] {
            let name = format!("pin42-arguments-{name}");
            let case = cases.iter().find(|case| case.name == name).unwrap();
            let output = crate::translator::Translator::GeminiClaudeStream
                .run_rust(case)
                .unwrap();
            let steps = output.as_array().unwrap();
            assert_eq!(steps.len(), case.events.len());
            let deltas = |step: &serde_json::Value| {
                step["frames"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|frame| frame["data"]["delta"]["type"] == "input_json_delta")
                    .filter_map(|frame| frame["data"]["delta"]["partial_json"].as_str())
                    .collect::<String>()
            };
            assert_eq!(deltas(&steps[0]), immediate, "{name}");
            let argument: String = steps.iter().map(deltas).collect();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&argument).unwrap(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn fixtures_have_unique_names_and_real_suite_keys() {
        let keys: BTreeSet<_> = crate::suite_registry(
            &crate::Args::parse(
                ["--upstream", "unused", "--random", "0"]
                    .map(str::to_owned)
                    .into_iter(),
            )
            .unwrap(),
            true,
        )
        .unwrap()
        .into_iter()
        .map(|(translator, _, _)| translator.key())
        .collect();
        let mut names = BTreeSet::new();
        for input in INPUTS {
            for fixture in serde_json::from_str::<Vec<Fixture>>(input).unwrap() {
                assert!(!fixture.suites.is_empty(), "{}", fixture.name);
                for key in fixture.suites {
                    assert!(keys.contains(key.as_str()), "{key}");
                    assert!(
                        names.insert((key, fixture.name.clone())),
                        "{}",
                        fixture.name
                    );
                }
            }
        }
    }
}
