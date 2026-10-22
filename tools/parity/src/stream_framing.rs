//! Prove the documented Gemini-to-Interactions SSE framing deviation by
//! rerunning upstream on bare payloads, never by matching case names.

use open_ferry_translate::go::trim_space;
use serde_json::{Value, json};

use crate::cases::Case;
use crate::compare;
use crate::translator::Translator;
use crate::upstream::GoResult;

pub const REASON: &str = "UPSTREAM.md (Gemini-to-Interactions stream deviation): accepts SSE data/DONE and ignores blank/event lines; unframed upstream comparison is identical or equivalent under the existing parity rules";

pub struct Unframed {
    pub case: Case,
    retained: Vec<usize>,
    original_len: usize,
    registry: bool,
}

/// Only the direct stream and that same registry pair can use this rule.
pub fn unframe(translator: Translator, case: &Case) -> Option<Unframed> {
    let registry = translator == Translator::RegistryStream
        && case.options["from"] == "gemini"
        && case.options["to"] == "interactions";
    if translator.key() != "gemini/interactions/response" && !registry {
        return None;
    }
    let mut bare = case.clone();
    bare.events.clear();
    let mut retained = Vec::new();
    let mut framed = false;
    for (index, line) in case.events.iter().enumerate() {
        let payload = if let Some(payload) = line.strip_prefix("data:") {
            framed = true;
            payload
        } else {
            line.as_str()
        };
        let payload = std::str::from_utf8(trim_space(payload.as_bytes())).ok()?;
        if payload.is_empty() || payload.starts_with("event:") {
            framed = true;
            continue;
        }
        // Preserve every payload, including [DONE], in the same order.
        bare.events.push(payload.to_owned());
        retained.push(index);
    }
    framed.then_some(Unframed {
        case: bare,
        retained,
        original_len: case.events.len(),
        registry,
    })
}

impl Unframed {
    /// Identical or equivalent under the suite's existing, unchanged rules,
    /// after its volatile ID/time reader. A known-case marker is no proof.
    pub fn matches(&self, translator: Translator, rust: &Value, go: &GoResult) -> bool {
        let GoResult::Output(bytes) = go else {
            return false;
        };
        let Some(mut expected) = translator.read(&self.case, bytes) else {
            return false;
        };
        if self.registry {
            // Removing noise removes input slots from the registry report.
            // Reinsert only empty slots, so any output on a noise line still
            // fails, and all payload boundaries/finish/error fields must match.
            let Some(chunks) = expected
                .as_array_mut()
                .and_then(|items| items.last_mut())
                .and_then(|item| item.pointer_mut("/registry/chunks"))
            else {
                return false;
            };
            let Some(bare_chunks) = chunks.as_array() else {
                return false;
            };
            if bare_chunks.len() != self.retained.len() {
                return false;
            }
            let mut expanded = vec![json!([]); self.original_len];
            for (&index, chunk) in self.retained.iter().zip(bare_chunks) {
                let Some(slot) = expanded.get_mut(index) else {
                    return false;
                };
                *slot = chunk.clone();
            }
            *chunks = expanded.into();
        }
        compare::compare_numbers(
            &expected,
            rust,
            translator.embedded_json(&self.case),
            translator.numbers(),
        )
        .differences
        .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct() -> Translator {
        crate::interactions::suites(38, 0)
            .into_iter()
            .map(|(translator, _, _)| translator)
            .find(|translator| translator.key() == "gemini/interactions/response")
            .expect("direct Gemini stream suite")
    }

    #[test]
    fn rule_requires_framing_the_right_pair_and_matching_upstream_output() {
        let translator = direct();
        let case = Case::response(
            "arbitrary-name-not-a-fixture",
            "{}",
            [
                "event: message",
                "\r\n",
                "data: {\"candidates\":[]}",
                "data: [DONE]",
            ]
            .map(str::to_owned)
            .into(),
        );
        let bare = unframe(translator, &case).expect("framed input");
        assert_eq!(bare.case.events, ["{\"candidates\":[]}", "[DONE]"]);
        assert!(unframe(translator, &bare.case).is_none());
        assert!(unframe(Translator::Stream, &case).is_none());
        let go = GoResult::Output(b"event: done\ndata: [DONE]\n\n".to_vec());
        let expected = translator
            .read(
                &bare.case,
                match &go {
                    GoResult::Output(bytes) => bytes,
                    GoResult::Panic(_) => unreachable!(),
                },
            )
            .expect("SSE output");
        assert!(bare.matches(translator, &expected, &go));
        assert!(!bare.matches(translator, &json!([]), &go));
        assert!(!bare.matches(translator, &expected, &GoResult::Panic("failed".into())));
        assert!(!bare.matches(
            translator,
            &expected,
            &GoResult::Output(b"not SSE".to_vec())
        ));
    }

    #[test]
    fn existing_embedded_json_equivalence_does_not_hide_changed_arguments() {
        let translator = direct();
        let case = Case::response("any-name", "{}", vec!["data: {}".into()]);
        let bare = unframe(translator, &case).expect("framed input");
        // Both saved review cases use the existing EmbeddedJson / Whole
        // equivalence at this exact argument path: whitespace and escapes.
        for (upstream, ours) in [
            ("{ \"n\": 1 }", "{\"n\":1}"),
            ("{\"emoji\":\"\\ud83d\\ude80\"}", "{\"emoji\":\"🚀\"}"),
        ] {
            let data = json!({"delta":{"arguments":upstream}});
            let go = GoResult::Output(format!("event: step.delta\ndata: {data}\n\n").into_bytes());
            let mut rust = json!([{"event":"step.delta","data":{"delta":{"arguments":ours}}}]);
            assert!(bare.matches(translator, &rust, &go));
            let GoResult::Output(bytes) = &go else {
                unreachable!()
            };
            let expected = translator.read(&bare.case, bytes).expect("SSE output");
            let comparison = compare::compare_numbers(
                &expected,
                &rust,
                translator.embedded_json(&bare.case),
                translator.numbers(),
            );
            assert_eq!(
                comparison.deviations,
                std::collections::BTreeSet::from([compare::Deviation::EmbeddedJson])
            );
            rust[0]["data"]["delta"]["arguments"] = "{\"n\":2}".into();
            assert!(!bare.matches(translator, &rust, &go));
        }
    }

    #[test]
    fn registry_noise_slots_must_be_empty_and_payload_boundaries_must_match() {
        let translator = Translator::RegistryStream;
        let case = Case::response(
            "any-seed",
            "{}",
            vec!["event: message".into(), "data: [DONE]".into()],
        )
        .with_options(json!({"from":"gemini", "to":"interactions"}));
        let bare = unframe(translator, &case).expect("registry pair");
        let go = GoResult::Output(
            serde_json::to_vec(&json!({
                "events":[["event: done\ndata: [DONE]\n\n"]],
                "finish":[], "failed":false
            }))
            .expect("report"),
        );
        let expected = json!([
            {"unparsed":"event: done\ndata: [DONE]"},
            {"registry":{"chunks":[[],[["event: done"]]], "finish":[], "tool_input_failed":false}}
        ]);
        assert!(bare.matches(translator, &expected, &go));
        let mut wrong = expected.clone();
        wrong[1]["registry"]["chunks"][0] = json!([["interaction.created"]]);
        assert!(!bare.matches(translator, &wrong, &go));
        wrong = expected.clone();
        wrong[1]["registry"]["finish"] = json!([["done"]]);
        assert!(!bare.matches(translator, &wrong, &go));
        wrong = expected.clone();
        wrong[1]["registry"]["tool_input_failed"] = true.into();
        assert!(!bare.matches(translator, &wrong, &go));
        let wrong_pair = case.with_options(json!({"from":"interactions", "to":"gemini"}));
        assert!(unframe(translator, &wrong_pair).is_none());
        assert!(unframe(Translator::RegistryNonStream, &wrong_pair).is_none());
    }
}
