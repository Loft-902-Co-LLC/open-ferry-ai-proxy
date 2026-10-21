//! Synthetic signature replay cases for the v8.0.36 translator changes.
use super::Case;
use serde_json::{Value, json};

pub fn for_suite(suite: &str) -> Vec<Case> {
    let signature = format!("Q-PaDgE{}", "A".repeat(1419));
    let reasoning = json!({"type":"reasoning","id":"r","encrypted_content":signature,"summary":[{"type":"summary_text","text":"hidden summary"}]});
    let request = match suite {
        "codex/gemini/request" => Some(
            json!({"model":"muse-spark","contents":[{"role":"model","parts":[{"thought":true,"text":"hidden summary"},{"thought":true,"text":"","thoughtSignature":signature},{"text":"visible"}]}]}),
        ),
        "codex/interactions/request" => Some(
            json!({"model":"muse-spark","input":[{"type":"thought","signature":signature,"text":"summary"},{"type":"thought","thoughtSignature":signature,"text":"duplicate"}],"include":["other"]}),
        ),
        "codex/claude/request" | "codex/claude/request-compat" => Some(
            json!({"model":"muse-spark","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"summary","signature":signature},{"type":"text","text":"visible"}]},{"role":"user","content":"next"}]}),
        ),
        "openai/gemini/request" => Some(
            json!({"contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{}},"thoughtSignature":"gemini-signature"}]}]}),
        ),
        _ => None,
    };
    if let Some(request) = request {
        return ["muse-spark", "gpt-5", "deployment"]
            .into_iter()
            .map(|model| {
                Case::new(
                    format!("signature-target-{model}"),
                    model,
                    request.to_string(),
                )
            })
            .collect();
    }
    if matches!(
        suite,
        "codex/gemini/response" | "codex/interactions/response" | "codex/claude/response"
    ) {
        let events = vec![
            json!({"type":"response.created","response":{"id":"resp_sig","model":"muse-spark","created_at":1}}),
            json!({"type":"response.output_item.done","item":reasoning}),
            json!({"type":"response.output_item.done","item":reasoning}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","name":"first","call_id":"c1","arguments":"{}"}}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","name":"second","call_id":"c2","arguments":"{}"}}),
            json!({"type":"response.output_text.delta","delta":"answer"}),
            json!({"type":"response.completed","response":{"id":"resp_sig","usage":{"input_tokens":1,"output_tokens":2}}}),
        ].into_iter().map(|event| format!("data: {event}")).collect();
        return vec![Case::response(
            "signed-done-only-and-consecutive-calls",
            "{}",
            events,
        )];
    }
    if matches!(
        suite,
        "codex/gemini/response-non-stream"
            | "codex/interactions/response-non-stream"
            | "codex/claude/response-non-stream"
    ) {
        return vec![Case::response("signed-reasoning-with-summary", "{}", vec![json!({"type":"response.completed","response":{"id":"resp_sig","model":"muse-spark","created_at":1,"output":[reasoning],"usage":{"input_tokens":1,"output_tokens":2}}}).to_string()])];
    }
    if suite == "openai/gemini/response" {
        let events = [
            json!({"choices":[{"delta":{"content":"visible","tool_calls":[{"index":0,"function":{"name":"lookup","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"extra_content":{"google":{"thought_signature":"late-signature"}}}]},"finish_reason":"tool_calls"}]}),
        ];
        return vec![Case::response(
            "mixed-content-and-signature-only-finish",
            "{}",
            events.into_iter().map(|e| format!("data: {e}")).collect(),
        )];
    }
    if suite == "openai/gemini/response-non-stream" {
        let event: Value = json!({"choices":[{"message":{"tool_calls":[{"type":"function","function":{"name":"lookup","arguments":"{}"},"extra_content":{"google":{"thought_signature":"signature"}}}]},"finish_reason":"tool_calls"}]});
        return vec![Case::response(
            "tool-thought-signature",
            "{}",
            vec![event.to_string()],
        )];
    }
    Vec::new()
}
