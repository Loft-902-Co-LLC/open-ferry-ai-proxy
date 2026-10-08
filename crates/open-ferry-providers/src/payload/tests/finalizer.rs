// Ported from CLIProxyAPI internal/runtime/executor/helps/payload_finalizer_test.go
// (TestPayloadFinalizerDefaultsUseOriginalAndNormalizeBeforeRules,
// TestPayloadRulesMatchFinalBodyAndTrackPaths) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The rules as upstream's final barrier applies them, to a body every
//! built-in change has been made to: defaults check the client's request,
//! and conditions and tracked paths read the body.

use super::{Args, headers, json, rules};

/// `TestPayloadFinalizerDefaultsUseOriginalAndNormalizeBeforeRules`: a
/// default is skipped for a field the client sent, and written over a
/// field only a built-in change set; an override and a filter apply to the
/// body as it is.
#[test]
fn defaults_use_original_and_normalize_before_rules() {
    let config = r#"
payload:
  default:
    - models:
        - name: alias
          protocol: openai
          from-protocol: claude
          headers:
            X-Test: "yes"
      params:
        missing: user default
        present: not applied
  override:
    - models:
        - name: alias
      params:
        tools.0.function.parameters.properties.count.type: number
  filter:
    - models:
        - name: alias
      params:
        - late
"#;
    let args = Args {
        executor: "openai",
        model: "upstream",
        protocol: "openai",
        from: "claude",
        original: Some(r#"{"present":"caller"}"#),
        requested: "alias",
        headers: headers(&[("X-Test", "yes"), ("User-Agent", "codex-cli/0.1")]),
        ..Args::default()
    };
    let out = args.run(
        config,
        r#"{"missing":"built-in","present":"caller","late":"injected","tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"count":{"type":"integer"}}}}}]}"#,
    );
    assert_eq!(
        out,
        json(
            r#"{"missing":"user default","present":"caller","tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"count":{"type":"number"}}}}}]}"#
        )
    );
}

/// `TestPayloadRulesMatchFinalBodyAndTrackPaths`: conditions read the body,
/// not the client's request, and the paths a rule touches are reported.
#[test]
fn rules_match_final_body_and_track_paths() {
    let config = r#"
payload:
  override:
    - models:
        - name: "*"
          match:
            - max_tokens: 100
      params:
        unexpected: true
    - models:
        - name: "*"
          match:
            - max_tokens: 300
      params:
        diagnostics.user: true
  filter:
    - models:
        - name: "*"
          match:
            - max_tokens: 300
      params:
        - context_management
        - messages.0
"#;
    let args = Args {
        model: "model",
        protocol: "claude",
        from: "claude",
        original: Some(r#"{"max_tokens":100,"messages":[{},{}]}"#),
        requested: "model",
        tracked: &["diagnostics", "context_management"],
        ..Args::default()
    };
    let (out, touched) = args.apply(
        Some(&rules(config)),
        r#"{"max_tokens":300,"context_management":{"builtin":true},"messages":[{},{},{}]}"#,
    );
    assert_eq!(
        out,
        json(r#"{"max_tokens":300,"messages":[{},{}],"diagnostics":{"user":true}}"#)
    );
    assert!(touched.contains("diagnostics"), "{touched:?}");
    assert!(touched.contains("context_management"), "{touched:?}");
}
