//! Ported from upstream's
//! internal/runtime/executor/helps/payload_helpers_disable_image_generation_test.go.

use super::{Args, headers, json};

const ALL: &str = "disable-image-generation: true\n";

/// `TestApplyPayloadConfigWithRoot_DisableImageGeneration_RemovesToolsEntry`.
#[test]
fn removes_tools_entry() {
    let out = Args::with_root("gpt-5.4", "openai-response", "").run(
        ALL,
        r#"{"tools":[{"type":"image_generation","output_format":"png"},{"type":"function","name":"f1"}]}"#,
    );
    assert_eq!(out, json(r#"{"tools":[{"type":"function","name":"f1"}]}"#));
}

/// `TestApplyPayloadConfigWithRoot_DisableImageGeneration_RemovesToolsEntryWithRoot`.
#[test]
fn removes_tools_entry_with_root() {
    let out = Args::with_root("gpt-5.4", "antigravity", "request").run(
        ALL,
        r#"{"request":{"tools":[{"type":"image_generation"},{"type":"web_search"}]}}"#,
    );
    assert_eq!(
        out,
        json(r#"{"request":{"tools":[{"type":"web_search"}]}}"#)
    );
}

/// `TestApplyPayloadConfigWithRoot_DisableImageGeneration_RemovesToolChoiceByType`.
#[test]
fn removes_tool_choice_by_type() {
    let out = Args::with_root("gpt-5.4", "openai-response", "").run(
        ALL,
        r#"{"tools":[{"type":"image_generation"},{"type":"function","name":"f1"}],"tool_choice":{"type":"image_generation"}}"#,
    );
    assert!(out.get("tool_choice").is_none(), "{out}");
}

/// `TestApplyPayloadConfigWithRoot_DisableImageGeneration_RemovesToolChoiceByNameWithRoot`.
#[test]
fn removes_tool_choice_by_name_with_root() {
    let out = Args::with_root("gpt-5.4", "antigravity", "request").run(
        ALL,
        r#"{"request":{"tools":[{"type":"image_generation"},{"type":"web_search"}],"tool_choice":{"type":"tool","name":"image_generation"}}}"#,
    );
    assert!(out["request"].get("tool_choice").is_none(), "{out}");
}

/// `TestApplyPayloadConfigWithRoot_DisableImageGenerationChat_KeepsImageGenerationOnImagesEndpoints`.
#[test]
fn chat_keeps_image_generation_on_images_endpoints() {
    let input = r#"{"tools":[{"type":"image_generation"},{"type":"function","name":"f1"}],"tool_choice":{"type":"image_generation"}}"#;
    let args = Args {
        request_path: "/v1/images/generations",
        ..Args::with_root("gpt-5.4", "openai-response", "")
    };
    let out = args.run("disable-image-generation: chat\n", input);
    assert_eq!(out, json(input));
}

/// `TestApplyPayloadConfigWithRoot_DisableImageGenerationPassthrough_KeepsPayloadUnchanged`.
#[test]
fn passthrough_keeps_payload_unchanged() {
    let input = r#"{"tools":[{"type":"image_generation"},{"type":"function","name":"f1"}],"tool_choice":{"type":"image_generation"}}"#;
    for request_path in ["", "/v1/responses", "/v1/images/generations"] {
        let args = Args {
            request_path,
            ..Args::with_root("gpt-5.4", "openai-response", "")
        };
        let out = args.run("disable-image-generation: passthrough\n", input);
        assert_eq!(out, json(input), "path {request_path:?}");
    }
}

/// `TestApplyPayloadConfigWithRoot_DisableImageGeneration_PayloadOverrideCanRestoreImageGeneration`.
#[test]
fn payload_override_can_restore_image_generation() {
    let config = r#"
disable-image-generation: true
payload:
  override-raw:
    - models:
        - name: gpt-5.4
          protocol: openai-response
      params:
        tools: '[{"type":"image_generation"},{"type":"function","name":"f1"}]'
        tool_choice: '{"type":"image_generation"}'
"#;
    let input = r#"{"tools":[{"type":"image_generation"},{"type":"function","name":"f1"}],"tool_choice":{"type":"image_generation"}}"#;
    let out = Args::with_root("gpt-5.4", "openai-response", "").run(config, input);
    assert_eq!(out, json(input));
}

/// Not upstream's: `chat` strips everywhere but the images endpoints, a
/// default rule checks the body as it was before the strip, and the strip
/// leaves a body with nothing to take out as it is.
#[test]
fn chat_strips_elsewhere_and_defaults_see_the_client_body() {
    let config = r#"
disable-image-generation: chat
payload:
  default:
    - models:
        - name: gpt-5.4
      params:
        tool_choice: auto
        parallel_tool_calls: false
"#;
    let input = r#"{"tools":[{"type":"image_generation"}],"tool_choice":"image_generation"}"#;
    let args = Args {
        request_path: " /v1/responses ",
        ..Args::with_root("gpt-5.4", "openai-response", "")
    };
    let out = args.run(config, input);
    assert_eq!(out, json(r#"{"tools":[],"parallel_tool_calls":false}"#));

    let args = Args {
        request_path: "/v1/images/edits",
        ..Args::with_root("gpt-5.4", "openai-response", "")
    };
    let out = args.run(config, input);
    assert_eq!(
        out,
        json(
            r#"{"tools":[{"type":"image_generation"}],"tool_choice":"image_generation","parallel_tool_calls":false}"#
        )
    );

    let input = r#"{"tools":"none","tool_choice":{"type":"function","name":"f"}}"#;
    let out = Args::with_root("gpt-5.4", "openai-response", "").run(ALL, input);
    assert_eq!(out, json(input));
}

/// `TestApplyPayloadConfigWithRequest_HeaderGateRequiresWildcardMatch`.
#[test]
fn header_gate_requires_wildcard_match() {
    let config = r#"
payload:
  override:
    - models:
        - name: gpt-*
          protocol: openai
          headers:
            X-Client-Tier: tenant-*-region-*
      params:
        metadata.enabled: true
"#;
    let mut args = Args {
        model: "gpt-5.4",
        protocol: "openai",
        from: "responses",
        headers: headers(&[("X-Client-Tier", "tenant-alpha-region-us")]),
        ..Args::default()
    };
    let out = args.run(config, r#"{"model":"gpt-5.4"}"#);
    assert_eq!(out["metadata"]["enabled"], true, "{out}");

    args.headers = headers(&[("X-Client-Tier", "tenant-alpha")]);
    let out = args.run(config, r#"{"model":"gpt-5.4"}"#);
    assert!(out.get("metadata").is_none(), "{out}");
}

/// `TestApplyPayloadConfigWithRequest_FromProtocolGateUsesSourceProtocol`.
#[test]
fn from_protocol_gate_uses_source_protocol() {
    let config = r#"
payload:
  override:
    - models:
        - name: gpt-*
          protocol: openai
          from-protocol: responses
      params:
        metadata.source: responses
    - models:
        - name: gpt-*
          protocol: openai
          from-protocol: openai
      params:
        metadata.source: openai
"#;
    for (from, want) in [("openai-response", "responses"), ("openai", "openai")] {
        let args = Args {
            model: "gpt-5.4",
            protocol: "openai",
            from,
            ..Args::default()
        };
        let out = args.run(config, r#"{"model":"gpt-5.4"}"#);
        assert_eq!(out["metadata"]["source"], want, "{out}");
    }
}

/// `TestApplyPayloadConfigWithRequest_PayloadConditionsNarrowRule`.
#[test]
fn payload_conditions_narrow_rule() {
    let config = r#"
payload:
  override:
    - models:
        - name: gpt-*
          match:
            - metadata.client: codex
            - 'tools.#(type=="web_search").enabled': true
          not-match:
            - metadata.mode: dev
          exist:
            - 'tools.#(type=="web_search").type'
          not-exist:
            - metadata.missing
            - metadata.null_value
      params:
        metadata.applied: true
"#;
    let args = Args {
        model: "gpt-5.4",
        protocol: "openai",
        from: "responses",
        ..Args::default()
    };
    let out = args.run(
        config,
        r#"{"model":"gpt-5.4","metadata":{"client":"codex","mode":"prod","null_value":null},"tools":[{"type":"function"},{"type":"web_search","enabled":true}]}"#,
    );
    assert_eq!(out["metadata"]["applied"], true, "{out}");
}

/// `TestApplyPayloadConfigWithRequest_PayloadConditionsSkipRule`.
#[test]
fn payload_conditions_skip_rule() {
    let cases = [
        (
            "match mismatch",
            "match:\n            - metadata.client: codex",
        ),
        (
            "not-match matched",
            "not-match:\n            - metadata.mode: dev",
        ),
        ("exist missing", "exist:\n            - metadata.missing"),
        ("exist null", "exist:\n            - metadata.null_value"),
        (
            "not-exist present",
            "not-exist:\n            - metadata.client",
        ),
    ];
    for (name, condition) in cases {
        let config = format!(
            r#"
payload:
  override:
    - models:
        - name: gpt-*
          {condition}
      params:
        metadata.applied: true
"#
        );
        let args = Args {
            model: "gpt-5.4",
            protocol: "openai",
            from: "responses",
            ..Args::default()
        };
        let out = args.run(
            &config,
            r#"{"model":"gpt-5.4","metadata":{"client":"other","mode":"dev","null_value":null}}"#,
        );
        assert!(out["metadata"].get("applied").is_none(), "{name}: {out}");
    }
}
