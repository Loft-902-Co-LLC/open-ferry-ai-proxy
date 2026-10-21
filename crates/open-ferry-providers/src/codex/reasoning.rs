// Ported from CLIProxyAPI internal/runtime/executor/openai_responses_signature.go
// (v8.0.36, MIT), with its compat replay of an unknown-format
// encrypted_content.
// https://github.com/router-for-me/CLIProxyAPI

//! Cleans Responses reasoning before replay. Compatible target-family envelopes
//! survive; unknown blobs survive only in compatibility mode. Meta is fail-closed.
//! Router suffixes are private to OpenRouter, and native Meta rejects them.
//! Cleartext content becomes a summary outside compatibility mode, and orphan IDs
//! are removed when storage is disabled.
//!
//! Deviations from upstream:
//! - Edits a parsed body in place and reports whether anything changed,
//!   instead of splicing raw JSON; the edited items are written by
//!   `serde_json`.

use open_ferry_translate::signature::{
    Provider, detect_signature_provider, inspect_gpt_reasoning_signature,
    inspect_meta_reasoning_signature,
};
use serde_json::{Map, Value, json};

use crate::json::{bool_of, get, str_at, str_of};

/// Sanitizes the reasoning items of `body`'s `input`
/// (`sanitizeOpenAIResponsesReasoningEncryptedContentWithCompat`). With
/// `is_compat`, a third-party Responses model's cleartext reasoning and IDs
/// are kept. Returns whether anything changed.
#[cfg(test)]
pub(crate) fn sanitize_reasoning(body: &mut Value, is_compat: bool) -> bool {
    let target = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    sanitize_reasoning_for_target(body, is_compat, &target)
}

pub(crate) fn resolve_target_model<'a>(base: &'a str, requested: &'a str) -> &'a str {
    if Provider::from_model_name(base) != Provider::Unknown {
        base
    } else {
        let requested = super::request::base_model(requested);
        if matches!(
            Provider::from_model_name(requested),
            Provider::Meta | Provider::Grok | Provider::Gpt
        ) {
            requested
        } else {
            base
        }
    }
}

#[cfg(test)]
pub(crate) fn sanitize_reasoning_for_target(
    body: &mut Value,
    is_compat: bool,
    target: &str,
) -> bool {
    sanitize_reasoning_for_destination(body, is_compat, target, "")
}

#[cfg(test)]
pub(crate) fn sanitize_reasoning_for_meta(body: &mut Value) -> bool {
    sanitize_reasoning_for_meta_base_url(body, "")
}

pub(crate) fn sanitize_reasoning_for_meta_base_url(body: &mut Value, base: &str) -> bool {
    sanitize(
        body,
        ReplayPolicy {
            is_compat: false,
            keep_foreign: true,
            target: Provider::Meta,
            meta_native: native_meta_url(base),
            allow_suffix: is_openrouter_url(base),
        },
    )
}

pub(crate) fn sanitize_reasoning_for_destination(
    body: &mut Value,
    is_compat: bool,
    target: &str,
    base: &str,
) -> bool {
    sanitize(
        body,
        ReplayPolicy {
            is_compat,
            keep_foreign: is_compat,
            target: Provider::from_model_name(target),
            meta_native: !base.trim().is_empty() && native_meta_url(base),
            allow_suffix: is_openrouter_url(base),
        },
    )
}

fn native_meta_url(base: &str) -> bool {
    url::Url::parse(base.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_none_or(|host| host.is_empty() || host == "api.meta.ai" || host.ends_with(".meta.ai"))
}

pub(crate) fn is_openrouter_url(base: &str) -> bool {
    url::Url::parse(base.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

pub(crate) fn canonical_signature(raw: &str) -> &str {
    use open_ferry_translate::signature::{
        inspect_grok_encrypted_content, signature_payload_without_provider_prefix,
    };
    let raw = signature_payload_without_provider_prefix(raw.trim());
    let has_suffix = inspect_gpt_reasoning_signature(raw)
        .is_ok_and(|i| !i.router_suffix.is_empty())
        || inspect_meta_reasoning_signature(raw).is_ok_and(|i| !i.router_suffix.is_empty())
        || inspect_grok_encrypted_content(raw).is_ok_and(|i| !i.router_suffix.is_empty());
    if has_suffix {
        raw.split_once('.').map_or(raw, |(head, _)| head)
    } else {
        raw
    }
}

#[derive(Clone, Copy)]
struct ReplayPolicy {
    is_compat: bool,
    keep_foreign: bool,
    target: Provider,
    meta_native: bool,
    allow_suffix: bool,
}

impl ReplayPolicy {
    fn normalized(self, raw: &str) -> Option<String> {
        use open_ferry_translate::signature::{
            compatible_signature_for_provider, inspect_grok_encrypted_content,
            split_signature_provider_prefix,
        };
        if raw != raw.trim() || raw.is_empty() {
            return None;
        }
        let accepted = match self.target {
            Provider::Meta => {
                let info = inspect_meta_reasoning_signature(raw).ok()?;
                if self.meta_native && !info.router_suffix.is_empty() {
                    return None;
                }
                Some(raw.to_owned())
            }
            Provider::Unknown | Provider::Gpt => {
                let candidate = match split_signature_provider_prefix(raw) {
                    Some((Provider::Gpt, payload)) => payload,
                    _ => raw,
                };
                inspect_gpt_reasoning_signature(candidate)
                    .ok()
                    .map(|_| candidate.to_owned())
            }
            Provider::Grok => inspect_grok_encrypted_content(raw)
                .ok()
                .map(|_| raw.to_owned()),
            target => compatible_signature_for_provider(target, raw),
        };
        if let Some(accepted) = accepted {
            return Some(if self.allow_suffix {
                accepted
            } else {
                canonical_signature(&accepted).to_owned()
            });
        }
        (self.keep_foreign && detect_signature_provider(raw) == Provider::Unknown)
            .then(|| raw.to_owned())
    }
}

fn sanitize(body: &mut Value, policy: ReplayPolicy) -> bool {
    let strip_ids = !bool_of(get(body, "store"));
    let Some(Value::Array(items)) = body.get_mut("input") else {
        return false;
    };
    let mut changed = false;
    for item in items {
        if str_at(item, "type").trim() != "reasoning" {
            continue;
        }
        let Value::Object(item) = item else {
            continue;
        };
        let has_content =
            matches!(item.get("content"), Some(Value::Array(parts)) if !parts.is_empty());
        if !policy.is_compat && has_content {
            if summary_is_empty(item.get("summary")) {
                promote_reasoning_text(item);
            }
            item.insert("content".to_owned(), json!([]));
            changed = true;
        }
        let normalized = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .and_then(|raw| policy.normalized(raw));
        if let Some(normalized) = normalized {
            if item.get("encrypted_content").and_then(Value::as_str) != Some(&normalized) {
                item.insert("encrypted_content".into(), normalized.into());
                changed = true;
            }
        } else {
            changed |= item.shift_remove("encrypted_content").is_some();
            if !policy.is_compat && strip_ids {
                changed |= item.shift_remove("id").is_some();
            }
        }
    }
    changed
}

/// Whether a reasoning item's summary is missing, null or an empty array
/// (`openaiResponsesReasoningSummaryIsEmpty`).
fn summary_is_empty(summary: Option<&Value>) -> bool {
    match summary {
        None | Some(Value::Null) => true,
        Some(Value::Array(parts)) => parts.is_empty(),
        Some(_) => false,
    }
}

/// Sets the item's `summary` to its non-empty `reasoning_text` parts, as
/// `summary_text`, when there are any
/// (`promoteOpenAIResponsesReasoningTextToSummary`).
fn promote_reasoning_text(item: &mut Map<String, Value>) {
    let Some(Value::Array(parts)) = item.get("content") else {
        return;
    };
    let summary: Vec<Value> = parts
        .iter()
        .filter(|part| str_at(part, "type").trim() == "reasoning_text")
        .map(|part| str_of(get(part, "text")))
        .filter(|text| !text.is_empty())
        .map(|text| json!({"type": "summary_text", "text": text}))
        .collect();
    if !summary.is_empty() {
        item.insert("summary".to_owned(), Value::Array(summary));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::Engine as _;

    /// A well-formed GPT reasoning signature
    /// (`validOpenAIResponsesReasoningEncryptedContentForTest`).
    pub(crate) fn valid_signature() -> String {
        let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
        payload[0] = 0x80;
        for (i, byte) in payload.iter_mut().enumerate().skip(9) {
            *byte = i as u8;
        }
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    }

    #[test]
    fn meta_replay_is_native_and_target_gated() {
        let native = format!("Q-PaDgE{}", "A".repeat(1419));
        for sig in [&native, &valid_signature(), "opaque"] {
            for target in ["muse-spark", "gpt-5"] {
                let mut body = json!({"model":target,"store":false,"input":[{"type":"reasoning","id":"r","encrypted_content":sig}]});
                sanitize_reasoning(&mut body, true);
                assert_eq!(
                    body["input"][0].get("encrypted_content").is_some(),
                    if target == "muse-spark" {
                        sig == native
                    } else {
                        sig != native
                    }
                );
            }
            let mut body = json!({"store":false,"input":[{"type":"reasoning","id":"r","encrypted_content":sig}]});
            sanitize_reasoning_for_meta(&mut body);
            assert_eq!(
                body["input"][0].get("encrypted_content").is_some(),
                sig == native
            );
        }
    }

    fn sanitize(raw: &str) -> Value {
        let mut body: Value = serde_json::from_str(raw).unwrap();
        sanitize_reasoning(&mut body, false);
        body
    }

    fn assert_empty_content(body: &Value, path: &str) {
        assert_eq!(get(body, path), Some(&json!([])), "{path} in {body}");
    }

    #[test]
    fn strips_orphan_ids_when_store_disabled() {
        let valid = valid_signature();
        let got = sanitize(&format!(
            r#"{{"store":false,"input":[{{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]}},{{"id":"rs_orphan","type":"reasoning","summary":[]}},{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"id":"msg_1","type":"message","role":"user","content":"hi"}}]}}"#
        ));
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert!(get(&got, "input.0.id").is_none(), "{got}");
        assert!(get(&got, "input.1.id").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.2.id"), "rs_good");
        assert_eq!(str_at(&got, "input.2.encrypted_content"), valid);
        assert_eq!(str_at(&got, "input.3.id"), "msg_1");
    }

    #[test]
    fn keeps_ids_when_store_enabled() {
        let got = sanitize(
            r#"{"store":true,"input":[{"id":"rs_bad","type":"reasoning","encrypted_content":"bad","summary":[]},{"id":"rs_orphan","type":"reasoning","summary":[]}]}"#,
        );
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.0.id"), "rs_bad");
        assert_eq!(str_at(&got, "input.1.id"), "rs_orphan");
    }

    #[test]
    fn moves_cleartext_content_to_summary() {
        let got = sanitize(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"The model thinking process from a previous turn with a third-party provider..."}],"encrypted_content":null},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        );
        assert_empty_content(&got, "input.0.content");
        assert_eq!(str_at(&got, "input.0.summary.0.type"), "summary_text");
        assert_eq!(
            str_at(&got, "input.0.summary.0.text"),
            "The model thinking process from a previous turn with a third-party provider..."
        );
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.1.content.0.type"), "input_text");
    }

    #[test]
    fn does_not_duplicate_existing_summary() {
        let got = sanitize(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"already summarized"}],"content":[{"type":"reasoning_text","text":"duplicate thinking"}]}]}"#,
        );
        assert_empty_content(&got, "input.0.content");
        assert_eq!(
            get(&got, "input.0.summary")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(str_at(&got, "input.0.summary.0.text"), "already summarized");
    }

    #[test]
    fn promotes_multiple_reasoning_text_parts() {
        let got = sanitize(
            r#"{"store":false,"input":[{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"step one"},{"type":"reasoning_text","text":"step two"}]}]}"#,
        );
        assert_empty_content(&got, "input.0.content");
        assert_eq!(
            get(&got, "input.0.summary")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        assert_eq!(str_at(&got, "input.0.summary.0.text"), "step one");
        assert_eq!(str_at(&got, "input.0.summary.1.text"), "step two");
    }

    #[test]
    fn keeps_valid_encrypted_content_when_stripping_content() {
        let valid = valid_signature();
        let got = sanitize(&format!(
            r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[],"content":[{{"type":"reasoning_text","text":"cleartext thinking"}}]}}]}}"#
        ));
        assert_empty_content(&got, "input.0.content");
        assert_eq!(str_at(&got, "input.0.id"), "rs_good");
        assert_eq!(str_at(&got, "input.0.encrypted_content"), valid);
        assert_eq!(str_at(&got, "input.0.summary.0.text"), "cleartext thinking");
    }

    #[test]
    fn noop_leaves_the_body_unchanged() {
        let valid = valid_signature();
        let raw = format!(
            r#"{{"store":false,"input":[{{"id":"rs_good","type":"reasoning","encrypted_content":"{valid}","summary":[]}},{{"role":"user","content":"hi"}}]}}"#
        );
        let mut body: Value = serde_json::from_str(&raw).unwrap();
        assert!(!sanitize_reasoning(&mut body, false));
        assert_eq!(body.to_string(), raw);
    }

    #[test]
    fn compat_preserves_reasoning_content_and_id() {
        let mut body: Value = serde_json::from_str(
            r#"{"store":false,"input":[{"id":"rs_compat","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"keep cleartext thinking"}],"encrypted_content":null},{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        )
        .unwrap();
        assert!(sanitize_reasoning(&mut body, true));
        assert_eq!(str_at(&body, "input.0.id"), "rs_compat");
        assert_eq!(
            get(&body, "input.0.content")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(str_at(&body, "input.0.content.0.type"), "reasoning_text");
        assert_eq!(
            str_at(&body, "input.0.content.0.text"),
            "keep cleartext thinking"
        );
        assert_eq!(get(&body, "input.0.summary"), Some(&json!([])));
        assert!(get(&body, "input.0.encrypted_content").is_none(), "{body}");
    }

    // v8.0.32's
    // TestSanitizeOpenAIResponsesReasoningEncryptedContentWithCompat_PreservesUnknownFormatEncryptedContent.
    #[test]
    fn compat_preserves_unknown_format_encrypted_content() {
        const UNKNOWN: &str = "opaque-encrypted-reasoning-token-xyz";
        let claude = crate::gemini::testing::CLAUDE_SIGNATURE;
        let raw = format!(
            r#"{{"store":false,"input":[{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{UNKNOWN}"}},{{"type":"reasoning","summary":[],"content":null,"encrypted_content":""}},{{"type":"reasoning","summary":[],"content":null,"encrypted_content":"{claude}"}}]}}"#
        );
        let mut body: Value = serde_json::from_str(&raw).unwrap();
        sanitize_reasoning(&mut body, true);
        assert_eq!(
            str_at(&body, "input.0.encrypted_content"),
            UNKNOWN,
            "{body}"
        );
        assert!(get(&body, "input.1.encrypted_content").is_none(), "{body}");
        assert!(get(&body, "input.2.encrypted_content").is_none(), "{body}");

        let got = sanitize(&raw);
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
    }

    #[test]
    fn rejects_whitespace_and_non_strings() {
        let valid = valid_signature();
        let got = sanitize(&format!(
            r#"{{"store":true,"input":[{{"id":"a","type":"reasoning","encrypted_content":" {valid}"}},{{"id":"b","type":" reasoning ","encrypted_content":123}},"reasoning",{{"type":"reasoning"}}]}}"#
        ));
        assert!(get(&got, "input.0.encrypted_content").is_none(), "{got}");
        assert!(get(&got, "input.1.encrypted_content").is_none(), "{got}");
        assert_eq!(str_at(&got, "input.0.id"), "a");
        assert_eq!(get(&got, "input.2"), Some(&json!("reasoning")));
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    #[test]
    fn router_suffixes_follow_real_destination_and_family() {
        use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
        let meta = format!("Q-PaDgE{}", "A".repeat(1419));
        let gpt = tests::valid_signature();
        let grok = STANDARD_NO_PAD.encode((0..255).collect::<Vec<u8>>());
        for (model, native) in [("muse-spark", &meta), ("gpt-5", &gpt), ("grok-4", &grok)] {
            for base in [
                "https://openrouter.ai/api/v1",
                "https://sub.openrouter.ai/v1",
                "https://api.openai.com/v1",
                "https://chatgpt.com/backend-api/codex",
                "https://api.x.ai/v1",
                "https://other.example/v1",
                "https://openrouter.ai.evil.example/v1",
            ] {
                let routed = format!("{native}.Router123");
                let mut body = json!({"input":[{"type":"reasoning","encrypted_content":routed}]});
                sanitize_reasoning_for_destination(&mut body, false, model, base);
                assert_eq!(
                    body["input"][0]["encrypted_content"].as_str(),
                    Some(if is_openrouter_url(base) {
                        routed.as_str()
                    } else {
                        native.as_str()
                    })
                );
                assert_eq!(canonical_signature(&routed), native);
            }
        }
        for base in ["", "https://api.meta.ai/v1", "https://a.meta.ai/v1"] {
            let mut body = json!({"input":[{"type":"reasoning","id":"orphan","encrypted_content":format!("{meta}.Router123")}]});
            sanitize_reasoning_for_meta_base_url(&mut body, base);
            assert!(body["input"][0].get("encrypted_content").is_none());
            assert!(body["input"][0].get("id").is_none());
        }
        for (model, foreign) in [
            ("grok-4", &meta),
            ("muse-spark", &gpt),
            ("claude-sonnet", &gpt),
            ("gemini-3", &gpt),
        ] {
            let mut body = json!({"input":[{"type":"reasoning","encrypted_content":foreign}]});
            sanitize_reasoning_for_destination(&mut body, true, model, "https://openrouter.ai");
            assert!(body["input"][0].get("encrypted_content").is_none());
        }
    }
    #[test]
    fn deployment_alias_fallback_is_limited_to_responses_families() {
        for model in ["meta/muse", "grok-4", "gpt-5"] {
            assert_eq!(resolve_target_model("deployment", model), model);
        }
        assert_eq!(
            resolve_target_model("deployment", "claude-sonnet"),
            "deployment"
        );
        assert_eq!(resolve_target_model("gpt-5", "meta/muse"), "gpt-5");
    }
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    use crate::codex::request::{Context, Kind, prepare_body};
    use open_ferry_core::exec::{Format, Options, Request};
    #[test]
    fn alias_family_is_selected_before_http_stream_and_compact_translation() {
        let native = format!("Q-PaDgE{}", "A".repeat(1419));
        let payload = json!({"model":"muse-spark","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"summary","signature":format!("{native}.Router123")},{"type":"text","text":"answer"}]},{"role":"user","content":"continue"}]});
        let request = Request {
            model: "deployment".into(),
            payload: bytes::Bytes::from(payload.to_string()),
        };
        let mut options = Options::new(Format::CLAUDE);
        options.metadata.requested_model = "muse-spark".into();
        for kind in [Kind::Execute, Kind::Stream, Kind::Compact] {
            // Compact is a native Responses endpoint, as upstream registers no
            // Claude-to-Responses request translator for it.
            let compact_request = Request {
                model: "deployment".into(),
                payload: bytes::Bytes::from(json!({"input":[{"type":"reasoning","encrypted_content":format!("{native}.Router123"),"summary":[]}]}).to_string()),
            };
            let mut compact_options = Options::new(Format::OPENAI_RESPONSE);
            compact_options.metadata.requested_model = "muse-spark".into();
            let (request, options) = if kind == Kind::Compact {
                (&compact_request, &compact_options)
            } else {
                (&request, &options)
            };
            let prepared = prepare_body(kind, Context::default(), request, options).unwrap();
            assert_eq!(prepared.body["model"], "deployment");
            let reasoning = prepared.body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["type"] == "reasoning")
                .expect("alias translation lost reasoning");
            assert_eq!(reasoning["encrypted_content"], native);
        }
    }
    #[test]
    fn search_history_is_reconciled_in_every_http_preparation_path() {
        for kind in [Kind::Execute, Kind::Stream, Kind::Compact] {
            let request = Request { model:"gpt-5".into(), payload: bytes::Bytes::from(json!({"input":[{"type":"web_search_call","id":"search"},{"role":"user","content":"next"}]}).to_string()) };
            let prepared = prepare_body(
                kind,
                Context::default(),
                &request,
                &Options::new(Format::OPENAI_RESPONSE),
            )
            .unwrap();
            assert!(
                prepared.body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|t| t["type"] == "web_search")
            );
        }
    }
}
