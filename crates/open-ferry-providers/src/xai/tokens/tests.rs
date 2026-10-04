//! Token counting, ported from upstream's `xai_executor_test.go`.

use serde_json::Value;

use super::*;

fn count(body: &str) -> i64 {
    count_input_tokens(&serde_json::from_str::<Value>(body).expect("valid JSON"))
}

fn tokens(text: &str) -> i64 {
    i64::try_from(
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len(),
    )
    .unwrap()
}

// TestCountXAIInputTokensExcludesRequestStructure.
#[test]
fn count_excludes_request_structure() {
    let semantic = r#"{
        "instructions":"Follow the repository instructions.",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Review this implementation."}]},
            {"type":"function_call","name":"read_file","arguments":"{\"path\":\"main.go\"}"},
            {"type":"function_call_output","output":"package main"},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"I will inspect the file."}]}
        ],
        "tools":[{"type":"function","name":"read_file","description":"Reads a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],
        "text":{"format":{"name":"result","schema":{"type":"object"}}}
    }"#;
    let structural = r#"{
        "model":"grok-4.5", "stream":false, "reasoning":{"effort":"high"},
        "metadata":{"large_wrapper":"this metadata must not affect estimated input tokens"},
        "prompt_cache_key":"session-123", "max_output_tokens":4096,
        "instructions":"Follow the repository instructions.",
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"Review this implementation."}]},
            {"type":"function_call","name":"read_file","arguments":"{\"path\":\"main.go\"}"},
            {"type":"function_call_output","output":"package main"},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"I will inspect the file."}]}
        ],
        "tools":[{"type":"function","name":"read_file","description":"Reads a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],
        "text":{"format":{"name":"result","schema":{"type":"object"}}}
    }"#;
    assert!(count(semantic) > 0);
    assert_eq!(count(structural), count(semantic));

    for (name, body, expected) in [
        (
            "instructions",
            r#"{"instructions":"unique instruction text"}"#,
            "unique instruction text",
        ),
        (
            "string input",
            r#"{"input":"unique input text"}"#,
            "unique input text",
        ),
        (
            "message content",
            r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"unique message text"}]}]}"#,
            "unique message text",
        ),
        (
            "refusal",
            r#"{"input":[{"type":"message","content":[{"type":"refusal","refusal":"unique refusal text"}]}]}"#,
            "unique refusal text",
        ),
        (
            "input image",
            r#"{"input":[{"type":"message","content":[{"type":"input_image","image_url":"https://example.com/unique.png"}]}]}"#,
            "https://example.com/unique.png",
        ),
        (
            "input file",
            r#"{"input":[{"type":"message","content":[{"type":"input_file","file_data":"unique file data","filename":"unique.txt"}]}]}"#,
            "unique file data\nunique.txt",
        ),
        (
            "input audio",
            r#"{"input":[{"type":"message","content":[{"type":"input_audio","data":"unique audio data"}]}]}"#,
            "unique audio data",
        ),
        (
            "function call",
            r#"{"input":[{"type":"function_call","call_id":"call-1","name":"unique_function","arguments":"{\"value\":\"unique argument\"}"}]}"#,
            "unique_function\n{\"value\":\"unique argument\"}",
        ),
        (
            "function call output",
            r#"{"input":[{"type":"function_call_output","call_id":"call-1","output":"unique tool output"}]}"#,
            "unique tool output",
        ),
        (
            "reasoning summary",
            r#"{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"unique summary text"}]}]}"#,
            "unique summary text",
        ),
        (
            "function tool",
            r#"{"tools":[{"type":"function","name":"unique_tool","description":"unique tool description","parameters":{"type":"object","properties":{"value":{"type":"string"}}}}]}"#,
            "unique_tool\nunique tool description\n{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"string\"}}}",
        ),
        (
            "structured text format",
            r#"{"text":{"format":{"name":"unique_format","schema":{"type":"object","properties":{"value":{"type":"string"}}}}}}"#,
            "unique_format\n{\"type\":\"object\",\"properties\":{\"value\":{\"type\":\"string\"}}}",
        ),
    ] {
        assert_eq!(count(body), tokens(expected), "{name}");
    }
}

// Not upstream's: a body with nothing the model reads counts nothing;
// other tool types and unknown items don't count.
#[test]
fn nothing_to_read_counts_nothing() {
    assert_eq!(count("{}"), 0);
    assert_eq!(count(r#"{"input":"   ","instructions":""}"#), 0);
    assert_eq!(
        count(
            r#"{"tools":[{"type":"web_search"},{"type":"x_search","name":"x"}],"input":[{"type":"compaction","encrypted_content":"e"}]}"#
        ),
        0
    );
}
