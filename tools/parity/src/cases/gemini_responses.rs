//! Hand-written cases for the translators between Responses and Gemini.

use open_ferry_translate::gemini::openai::responses::convert_openai_responses_request_to_gemini;
use serde_json::{Value, json};

use super::{Case, gpt_signature};
use crate::generate::gemini_responses::{CARRIER_PREFIX, carrier, gemini_signature};

/// Why a chunk or body that isn't JSON fails a response whose request
/// declares `apply_patch`: see the deviations in the port's `response.rs`.
const UNREADABLE_WITH_PATCH: &str = "with apply_patch declared, a chunk or body that isn't JSON fails the response; upstream reads it as nothing";

const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// A function name longer than Gemini allows, so the request translator
/// shortens it and the response translators map it back.
const LONG_NAME: &str =
    "mcp__documentation_server__search_the_documentation_index_for_matching_pages";

/// Two distinct valid Gemini thought signatures.
fn signatures() -> (String, String) {
    let first: Vec<u8> = (1..=40).collect();
    let second: Vec<u8> = (0..24).map(|i| 200 - i).collect();
    (gemini_signature(&first), gemini_signature(&second))
}

/// Tools as a Codex client declares them: functions, one with a long name,
/// a namespace, the `apply_patch` custom tool and web search.
fn client_tools() -> Value {
    json!([
        { "type": "function", "name": "get_weather", "description": "Weather.", "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] } },
        { "type": "function", "name": LONG_NAME, "parameters": { "type": "object", "properties": { "q": { "type": "string" } } } },
        { "type": "namespace", "name": "mcp__github", "description": "GitHub.", "tools": [
            { "type": "function", "name": "read_file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } },
            { "type": "custom", "name": "run" }
        ]},
        { "type": "custom", "name": "apply_patch", "description": "Edit files.", "format": { "type": "grammar", "syntax": "lark", "definition": "start: /.+/" } },
        { "type": "web_search", "filters": { "allowed_domains": ["docs.rs", " example.com "] } }
    ])
}

/// The same tools without `apply_patch`.
fn tools_without_patch() -> Value {
    let mut tools = client_tools();
    if let Value::Array(tools) = &mut tools {
        tools.retain(|tool| tool["name"] != "apply_patch");
    }
    tools
}

fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

fn assistant(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": text }] })
}

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({ "type": "function_call", "call_id": id, "name": name, "arguments": arguments })
}

fn output(id: &str, output: Value) -> Value {
    json!({ "type": "function_call_output", "call_id": id, "output": output })
}

fn reasoning(content: &str) -> Value {
    json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Thought." }], "encrypted_content": content })
}

/// A reasoning item holding only a carrier.
fn carried(signature: &str, direction: &str, target: &str) -> Value {
    json!({ "type": "reasoning", "summary": [], "encrypted_content": carrier(signature, direction, target) })
}

/// A request with these input items, the client's tools, and `model`.
fn with_tools(model: &str, items: Vec<Value>) -> String {
    json!({ "model": model, "input": items, "tools": tools_without_patch() }).to_string()
}

/// Responses requests the generator is unlikely to build in one piece.
pub fn requests() -> Vec<Case> {
    let (first, second) = signatures();
    let mut cases = vec![
        Case::new(
            "typical-sdk-request",
            "gemini-2.5-pro",
            json!({
                "model": "gemini-2.5-pro",
                "instructions": "You are terse.",
                "input": [
                    user("Weather in Paris?"),
                    reasoning(&first),
                    call("call_1", "get_weather", "{\"city\":\"Paris\"}"),
                    output("call_1", json!("18°C")),
                    assistant("It is 18°C."),
                    user("Thanks!")
                ],
                "tools": client_tools(),
                "tool_choice": "auto",
                "max_output_tokens": 1024,
                "temperature": 0.7,
                "top_p": 0.9,
                "reasoning": { "effort": "medium", "summary": "auto" },
                "parallel_tool_calls": true,
                "store": false,
                "stream": true,
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "cache-1"
            })
            .to_string(),
        ),
        Case::new("string-input", "gemini-2.5-flash", r#"{"input":"hi"}"#),
        Case::new(
            "carriers-in-every-direction",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Go."),
                    carried(&first, "next", "text"),
                    assistant("Checking."),
                    carried(&second, "previous", "text"),
                    carried(&first, "next", "function"),
                    call("call_a", "get_weather", "{\"city\":\"Oslo\"}"),
                    carried(&second, "previous", "function"),
                    output("call_a", json!("cold")),
                    carried(&first, "standalone", "any"),
                    carried(&second, "next", "any"),
                    assistant("Done."),
                    user("Next."),
                ],
            ),
        ),
        Case::new(
            "carriers-without-their-neighbour",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    carried(&first, "previous", "text"),
                    user("Go."),
                    carried(&first, "next", "function"),
                    assistant("No call follows."),
                    carried(&second, "previous", "function"),
                    carried(&second, "next", "text"),
                    call("call_b", "get_weather", "{}"),
                    output("call_b", json!("ok")),
                    carried(&first, "sideways", "text"),
                    json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Kept." }], "encrypted_content": format!("{}!!", carrier(&first, "next", "text")) }),
                    user("Next."),
                ],
            ),
        ),
        Case::new(
            "detached-reasoning-items",
            "gemini-3-pro-preview",
            with_tools(
                "gemini-3-pro-preview",
                vec![
                    user("Go."),
                    json!({ "type": "reasoning", "id": "rs_abc_detached_before_0", "summary": [], "encrypted_content": carrier(&first, "next", "text") }),
                    assistant("Answer."),
                    json!({ "type": "reasoning", "id": "rs_abc_detached_after_0", "summary": [], "encrypted_content": carrier(&second, "previous", "text") }),
                    json!({ "type": "reasoning", "id": "rs_abc_detached_after_1", "summary": [], "encrypted_content": first }),
                    user("More."),
                ],
            ),
        ),
        Case::new(
            "signatures-kept-on-calls",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Go."),
                    json!({ "type": "function_call", "call_id": "call_s1", "name": "get_weather", "arguments": "{\"city\":\"Rome\"}", "_cpa_reasoning_signature": first, "_cpa_reasoning_summary": "Weather first." }),
                    json!({ "type": "function_call", "call_id": "call_s2", "name": "get_weather", "arguments": "{\"city\":\"Nice\"}", "_cpa_reasoning_signature": "skip_thought_signature_validator" }),
                    json!({ "type": "function_call", "call_id": "call_s3", "name": "get_weather", "arguments": "{}", "_cpa_reasoning_signature": gpt_signature() }),
                    output("call_s1", json!("warm")),
                    output("call_s2", json!("mild")),
                    output("call_s3", json!("hot")),
                ],
            ),
        ),
        Case::new(
            "foreign-and-bypass-signatures",
            "gemini-2.5-flash",
            with_tools(
                "gemini-2.5-flash",
                vec![
                    user("Go."),
                    reasoning(&gpt_signature()),
                    assistant("One."),
                    reasoning("skip_thought_signature_validator"),
                    assistant("Two."),
                    reasoning(&format!(" {second} ")),
                    assistant("Three."),
                    reasoning(""),
                    user("Next."),
                ],
            ),
        ),
        Case::new(
            "media-parts",
            "gemini-2.5-pro",
            json!({
                "input": [{ "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "Look:" },
                    { "type": "input_image", "image_url": "data:image/png;base64,aGVsbG8=" },
                    { "type": "input_image", "image_url": "https://example.com/cat.JPG", "detail": "high" },
                    { "type": "input_file", "file_data": "data:application/pdf;base64,aGVsbG8=", "filename": "doc.pdf" },
                    { "type": "input_file", "file_url": "https://example.com/report.pdf" },
                    { "type": "input_file", "file_id": "file-abc123" },
                    { "type": "input_audio", "input_audio": { "data": "aGVsbG8=", "format": "wav" } },
                    { "type": "input_video", "video_url": "https://example.com/clip.mp4" },
                    { "type": "input_image", "image_url": { "url": "data:image/jpeg;base64,aGVsbG8=" } }
                ]}]
            })
            .to_string(),
        ),
        Case::new(
            "tool-output-with-media",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Screenshot?"),
                    call("call_m", "get_weather", "{\"city\":\"Paris\"}"),
                    output(
                        "call_m",
                        json!([
                            { "type": "input_text", "text": "Here:" },
                            { "type": "input_image", "image_url": "data:image/png;base64,aGVsbG8=" },
                            { "type": "input_file", "file_data": "aGVsbG8=", "filename": "a.pdf" }
                        ]),
                    ),
                    output("call_m", json!({ "ok": true })),
                ],
            ),
        ),
        Case::new(
            "web-search-with-allowed-domains",
            "gemini-2.5-pro",
            json!({
                "input": "Latest Rust release?",
                "tools": [
                    { "type": "web_search", "filters": { "allowed_domains": ["blog.rust-lang.org", "", " docs.rs "] } },
                    { "type": "web_search_preview" }
                ],
                "tool_choice": { "type": "web_search" }
            })
            .to_string(),
        ),
        Case::new(
            "web-search-beside-functions",
            "gemini-3-pro-preview",
            json!({
                "instructions": "Search when unsure.",
                "input": [user("Weather and news?")],
                "tools": [
                    { "type": "web_search" },
                    { "type": "function", "name": "get_weather", "parameters": { "type": "object", "properties": {} } }
                ],
                "tool_choice": "auto"
            })
            .to_string(),
        ),
        Case::new(
            "allowed-tools-choice",
            "gemini-2.5-pro",
            json!({
                "input": [user("Go.")],
                "tools": client_tools(),
                "tool_choice": { "type": "allowed_tools", "mode": "required", "tools": [
                    { "type": "function", "name": "get_weather" },
                    { "type": "function", "name": "read_file", "namespace": "mcp__github" },
                    { "type": "custom", "name": "apply_patch" }
                ]}
            })
            .to_string(),
        ),
        Case::new(
            "namespace-calls",
            "gemini-2.5-pro",
            json!({
                "input": [
                    user("Read it."),
                    { "type": "function_call", "call_id": "call_n", "name": "read_file", "namespace": "mcp__github", "arguments": "{\"path\":\"README.md\"}" },
                    output("call_n", json!("# Title")),
                    { "type": "custom_tool_call", "call_id": "call_r", "name": "run", "namespace": "mcp__github", "input": "ls" },
                    { "type": "custom_tool_call_output", "call_id": "call_r", "output": "a b" }
                ],
                "tools": client_tools(),
                "tool_choice": { "type": "function", "name": "read_file", "namespace": "mcp__github" }
            })
            .to_string(),
        ),
        Case::new(
            "apply-patch-round-trip",
            "gemini-2.5-pro",
            json!({
                "input": [
                    user("Add a file."),
                    { "type": "custom_tool_call", "call_id": "call_p", "name": "apply_patch", "input": PATCH },
                    { "type": "custom_tool_call_output", "call_id": "call_p", "output": "Done!" }
                ],
                "tools": client_tools(),
                "tool_choice": { "type": "custom", "name": "apply_patch" }
            })
            .to_string(),
        ),
        Case::new(
            "call-without-output",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Go."),
                    call("call_x", "get_weather", "{\"city\":\"Paris\"}"),
                    call("call_y", LONG_NAME, "{\"q\":\"x\"}"),
                    output("call_y", json!("found")),
                    user("Why stop?"),
                ],
            ),
        ),
        Case::new(
            "outputs-out-of-order",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Both."),
                    call("call_1", "get_weather", "{\"city\":\"A\"}"),
                    call("call_2", "get_weather", "{\"city\":\"B\"}"),
                    output("call_2", json!("b")),
                    output("call_1", json!("a")),
                    output("call_9", json!("orphan")),
                ],
            ),
        ),
        Case::new(
            "system-messages-mid-conversation",
            "gemini-2.5-pro",
            json!({
                "instructions": "Base rules.",
                "input": [
                    { "type": "message", "role": "system", "content": "Before." },
                    user("Hi."),
                    assistant("Hello."),
                    { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "Be brief." }] },
                    user("Again.")
                ]
            })
            .to_string(),
        ),
        Case::new(
            "trailing-assistant-prefill",
            "gemini-2.5-pro",
            json!({
                "input": [
                    user("Finish this: roses are"),
                    assistant("Roses are red,"),
                    { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": " violets" }] }
                ]
            })
            .to_string(),
        ),
        Case::new(
            "unknown-roles",
            "gemini-2.5-pro",
            json!({
                "input": [
                    { "type": "message", "role": "critic", "content": "Odd role." },
                    { "type": "message", "role": "tool", "content": [{ "type": "input_text", "text": "Tool role." }] },
                    { "type": "message", "content": "No role." },
                    user("Hi.")
                ]
            })
            .to_string(),
        ),
        Case::new(
            "json-schema-output",
            "gemini-2.5-flash",
            json!({
                "input": "Give JSON.",
                "text": { "format": { "type": "json_schema", "name": "out", "strict": true, "schema": {
                    "type": "object",
                    "properties": { "a": { "type": "string", "enum": ["x", "y"] }, "b": { "type": ["integer", "null"] } },
                    "required": ["a"],
                    "additionalProperties": false
                }}},
                "max_output_tokens": 256,
                "stop_sequences": ["END"]
            })
            .to_string(),
        ),
    ];
    for (name, model, effort) in [
        ("effort-low-gemini-3", "gemini-3-pro-preview", "low"),
        (
            "effort-high-gemini-3-flash",
            "gemini-3-flash-preview",
            "high",
        ),
        (
            "effort-minimal-gemini-2-5-flash",
            "gemini-2.5-flash",
            "minimal",
        ),
        ("effort-none-gemini-2-5-pro", "gemini-2.5-pro", "none"),
        ("effort-auto-gemini-2-5-pro", "gemini-2.5-pro", "auto"),
        (
            "effort-xhigh-gemini-2-5-flash-lite",
            "gemini-2.5-flash-lite",
            "xhigh",
        ),
        ("effort-with-suffix", "gemini-2.5-pro(low)", "high"),
    ] {
        cases.push(Case::new(
            name,
            model,
            json!({ "input": "Think.", "reasoning": { "effort": effort, "summary": "auto" } })
                .to_string(),
        ));
    }
    cases.extend(media_requests());
    cases.extend(carrier_requests());
    cases
}

/// Audio, video and files typed by every format the translator names, and
/// by data URL, format, file name or none of them.
fn media_requests() -> Vec<Case> {
    let message = |parts: Vec<Value>| {
        json!({ "model": "gemini-2.5-pro", "input": [{ "type": "message", "role": "user", "content": parts }] })
            .to_string()
    };
    let mut audio: Vec<Value> = [
        "wav", "mp3", "mpeg", "ogg", "flac", "aac", "webm", "pcm16", "pcm", "g711_ulaw",
        "g711_alaw", "opus", "m4a", "wma", "aiff", "mid", "weird", "audio/x-custom", "",
        "application/octet-stream", " MP3 ",
    ]
    .iter()
    .map(|format| json!({ "type": "input_audio", "input_audio": { "data": "aGVsbG8=", "format": format } }))
    .collect();
    audio.extend([
        // Data URLs of no particular type, typed by format, file name or
        // neither.
        json!({ "type": "input_audio", "audio_url": "data:application/octet-stream;base64,aGVsbG8=", "format": "ogg" }),
        json!({ "type": "input_audio", "audio_url": "data:;base64,aGVsbG8=", "filename": "voice.flac" }),
        json!({ "type": "audio", "audio_url": { "url": "data:application/octet-stream;base64,aGVsbG8=" } }),
        json!({ "type": "audio", "data": "data:;base64,aGVsbG8=", "filename": "a.wma" }),
        json!({ "type": "audio", "data": "data:application/octet-stream;base64,aGVsbG8=" }),
        // Neither remote nor a data URL.
        json!({ "type": "input_audio", "audio_url": "file-abc123", "filename": "song.mp3" }),
        json!({ "type": "input_audio", "url": "relative/voice", "format": "aiff" }),
        json!({ "type": "audio", "data": "aGVsbG8=", "filename": "a.mid" }),
        json!({ "type": "input_audio", "audio_url": "https://example.com/voice", "filename": "v.opus" }),
    ]);
    let mut video: Vec<Value> = [
        "mp4", "webm", "mov", "quicktime", "avi", "x-msvideo", "mpeg", "ogg", "mkv", "x-matroska",
        "flv", "x-flv", "3gpp", "ogv", "3gp", "h264", "weird", "video/x-custom", "",
    ]
    .iter()
    .map(|format| json!({ "type": "input_video", "input_video": { "data": "aGVsbG8=", "format": format } }))
    .collect();
    video.extend([
        json!({ "type": "input_video", "video_url": "data:application/octet-stream;base64,aGVsbG8=", "format": "webm" }),
        json!({ "type": "video", "video_url": "data:;base64,aGVsbG8=", "filename": "a.avi" }),
        json!({ "type": "video_url", "video_url": "data:application/octet-stream;base64,aGVsbG8=" }),
        json!({ "type": "input_video", "video_url": "relative/clip", "filename": "movie.mkv" }),
        json!({ "type": "input_video", "video_url": "https://example.com/noext" }),
        json!({ "type": "input_video", "video_url": "https://example.com/clip", "filename": "x.ogv" }),
        json!({ "type": "input_video", "data": "aGVsbG8=", "filename": "a.3gp" }),
    ]);
    let files = vec![
        json!({ "type": "input_file", "file_data": "data:application/octet-stream;base64,aGVsbG8=", "filename": "blob.bin" }),
        json!({ "type": "input_file", "file_url": "https://example.com/files/notes", "filename": "notes. " }),
        json!({ "type": "input_file", "file_data": "aGVsbG8=", "filename": "notes. " }),
        json!({ "type": "input_file", "file_data": "aGVsbG8=", "filename": "clip.h264" }),
        json!({ "type": "file", "url": "https://example.com/a.mp3", "format": "weird" }),
        json!({ "type": "input_file", "file_data": "data:;base64,aGVsbG8=", "mime_type": "weird", "filename": "x.pdf" }),
        json!({ "type": "input_image", "image_url": "https://example.com/pic", "format": "JPEG" }),
        json!({ "type": "input_image", "image_url": "https://example.com/pic", "format": "jpg" }),
    ];
    vec![
        Case::new("audio-formats", "gemini-2.5-pro", message(audio)),
        Case::new("video-formats", "gemini-2.5-pro", message(video)),
        Case::new("file-and-image-formats", "gemini-2.5-pro", message(files)),
    ]
}

/// Carriers after calls and binding backwards, malformed and oversized
/// carriers, and signed calls for a model of another provider.
fn carrier_requests() -> Vec<Case> {
    let (first, second) = signatures();
    let detached = |id: Option<&str>, content: &str| match id {
        Some(id) => {
            json!({ "type": "reasoning", "id": id, "summary": [], "encrypted_content": content })
        }
        None => json!({ "type": "reasoning", "summary": [], "encrypted_content": content }),
    };
    // Longer than a carrier may be, so upstream drops it unread.
    let oversized = format!(
        "{CARRIER_PREFIX}next:text:{}",
        "A".repeat(32 * 1024 * 1024 * 4 / 3 + 1024)
    );
    vec![
        Case::new(
            "carriers-after-calls",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Go."),
                    call("call_pc1", "get_weather", "{\"city\":\"Oslo\"}"),
                    carried(&first, "previous", "function"),
                    output("call_pc1", json!("cold")),
                    user("Two more."),
                    call("call_pc2", "get_weather", "{\"city\":\"Rome\"}"),
                    detached(None, &second),
                    detached(None, &first),
                    call("call_pc3", "get_weather", "{\"city\":\"Nice\"}"),
                    output("call_pc3", json!("mild")),
                    output("call_pc2", json!("warm")),
                    user("Unkeyed."),
                    json!({ "type": "function_call", "name": "get_weather", "arguments": "{}" }),
                    carried(&second, "previous", "function"),
                    output("call_elsewhere", json!("lost")),
                    user("Forward."),
                    call("call_pc4", "get_weather", "{}"),
                    carried(&first, "next", "function"),
                    call("call_pc5", "get_weather", "{}"),
                    output("call_pc4", json!("a")),
                    output("call_pc5", json!("b")),
                    user("Mismatched."),
                    call("call_pc6", "get_weather", "{}"),
                    carried(&second, "previous", "any"),
                    output("call_pc9", json!("other")),
                    user("Done."),
                ],
            ),
        ),
        Case::new(
            "carriers-binding-backwards",
            "gemini-3-pro-preview",
            with_tools(
                "gemini-3-pro-preview",
                vec![
                    user("Go."),
                    assistant("Sure."),
                    carried(&first, "previous", "text"),
                    user("Call it."),
                    call("call_bb", "get_weather", "{\"city\":\"Bern\"}"),
                    carried(&second, "previous", "function"),
                    user("Never mind."),
                    assistant("Plain."),
                    detached(None, &first),
                    user("Again."),
                    call("call_bc", "get_weather", "{}"),
                    detached(Some("rs_abc_detached_after_2"), &second),
                    user("Done?"),
                ],
            ),
        ),
        Case::new(
            "malformed-carriers",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Go."),
                    detached(None, &format!("{CARRIER_PREFIX}next")),
                    assistant("One."),
                    json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Kept." }], "encrypted_content": format!("{CARRIER_PREFIX}next:text") }),
                    assistant("Two."),
                    user("Next."),
                ],
            ),
        ),
        Case::new(
            "oversized-carrier",
            "gemini-2.5-pro",
            with_tools(
                "gemini-2.5-pro",
                vec![
                    user("Go."),
                    detached(None, &oversized),
                    assistant("Hi."),
                    user("Next."),
                ],
            ),
        ),
        Case::new(
            "signed-calls-for-another-provider",
            "claude-sonnet-4-6",
            with_tools(
                "claude-sonnet-4-6",
                vec![
                    user("Go."),
                    detached(None, &gpt_signature()),
                    call("call_g", "get_weather", "{\"city\":\"Lima\"}"),
                    output("call_g", json!("warm")),
                    reasoning(&gpt_signature()),
                    call("call_h", "get_weather", "{}"),
                    output("call_h", json!("ok")),
                    user("Thanks."),
                ],
            ),
        ),
        // Gemini's bypass signatures are no carrier, so the layout stays
        // the one for other providers; only one of them is the translator's
        // own placeholder.
        Case::new(
            "bypass-signatures-for-another-provider",
            "claude-sonnet-4-6",
            with_tools(
                "claude-sonnet-4-6",
                vec![
                    user("Go."),
                    reasoning("context_engineering_is_the_way_to_go"),
                    assistant("One."),
                    reasoning("skip_thought_signature_validator"),
                    user("Next."),
                ],
            ),
        ),
    ]
}

fn usage() -> Value {
    json!({ "promptTokenCount": 12, "candidatesTokenCount": 30, "totalTokenCount": 50, "thoughtsTokenCount": 8, "cachedContentTokenCount": 4 })
}

/// A chunk with these parts.
fn chunk(parts: Value) -> Value {
    json!({ "candidates": [{ "content": { "role": "model", "parts": parts } }] })
}

/// A chunk with these parts that ends the response, with usage if given.
fn last(parts: Value, finish: &str, usage: Option<Value>) -> Value {
    let mut chunk = json!({ "candidates": [{ "content": { "role": "model", "parts": parts }, "finishReason": finish }] });
    if let Some(usage) = usage {
        chunk["usageMetadata"] = usage;
    }
    chunk
}

fn line(chunk: &Value) -> String {
    format!("data: {chunk}")
}

/// A stream case: `request` is the client's, and each chunk is one line.
fn stream(name: &str, model: &str, request: &Value, chunks: &[Value]) -> Case {
    lines(name, model, request, chunks.iter().map(line).collect())
}

fn lines(name: &str, model: &str, request: &Value, lines: Vec<String>) -> Case {
    Case {
        model: model.into(),
        ..Case::response(name, request.to_string(), lines)
    }
}

/// The name the request translator declares the client's tool `at` (an
/// index into [`client_tools`]) under to Gemini, or its namespace's child
/// `child`.
fn gemini_name(at: usize, child: Option<usize>) -> String {
    let mut tool = client_tools()[at].clone();
    if let Some(child) = child {
        tool["tools"] = json!([tool["tools"][child].clone()]);
    }
    let request = json!({ "input": "x", "tools": [tool] });
    let gemini = convert_openai_responses_request_to_gemini("gemini-2.5-pro", &request, true);
    gemini["tools"][0]["functionDeclarations"][0]["name"]
        .as_str()
        .expect("the tool is declared")
        .to_owned()
}

fn typical_request() -> Value {
    json!({
        "model": "gemini-2.5-pro",
        "instructions": "You are terse.",
        "input": [user("Weather in Paris?")],
        "tools": tools_without_patch(),
        "tool_choice": "auto",
        "reasoning": { "effort": "medium", "summary": "auto" },
        "max_output_tokens": 1024,
        "temperature": 0.7,
        "parallel_tool_calls": true,
        "store": false,
        "metadata": { "trace": "t-1" },
        "prompt_cache_key": "cache-1",
        "safety_identifier": "user-hash",
        "service_tier": "default",
        "truncation": "disabled",
        "user": "u-1"
    })
}

fn patch_request() -> Value {
    json!({ "model": "gemini-2.5-pro", "input": [user("Add hello.txt.")], "tools": client_tools() })
}

fn search_request() -> Value {
    json!({ "model": "gemini-2.5-pro", "input": "Café news?", "tools": [{ "type": "web_search" }] })
}

/// Grounding for an answer about cafés: supports whose segments start and
/// end inside multi-byte characters, point past the text, at another part,
/// and at sources that are missing.
fn grounding() -> Value {
    json!({
        "webSearchQueries": ["café news", "café news", ""],
        "groundingChunks": [
            { "web": { "uri": "https://example.com/a", "title": "Example" } },
            { "web": { "uri": "https://example.org/b?x=1&y=2", "title": "café" } },
            { "web": { "uri": "https://example.com/a", "title": "Again" } }
        ],
        "groundingSupports": [
            { "segment": { "startIndex": 0, "endIndex": 4, "text": "Café" }, "groundingChunkIndices": [0] },
            { "segment": { "startIndex": 4, "endIndex": 17 }, "groundingChunkIndices": [1, 0] },
            { "segment": { "endIndex": 9 }, "groundingChunkIndices": [2, 7] },
            { "segment": { "startIndex": 3, "endIndex": 200 }, "groundingChunkIndices": [1] },
            { "segment": { "partIndex": 1, "startIndex": 0, "endIndex": 3 }, "groundingChunkIndices": [0] }
        ],
        "searchEntryPoint": { "renderedContent": "<div>results</div>" }
    })
}

/// Grounding without queries, so the search's query is the request's input,
/// and with a segment that starts before the text.
fn grounding_without_queries() -> Value {
    json!({
        "groundingChunks": [{ "web": { "uri": "https://example.com/a", "title": "Example" } }],
        "groundingSupports": [
            { "segment": { "startIndex": -3, "endIndex": 4 }, "groundingChunkIndices": [0] }
        ]
    })
}

/// A chunk that ends the response with these parts, wrapped in `response`,
/// whose grounding is wrapped once more.
fn doubly_wrapped_grounding(parts: Value) -> Value {
    json!({ "response": {
        "responseId": "dw1",
        "candidates": [{ "content": { "role": "model", "parts": parts }, "finishReason": "STOP" }],
        "usageMetadata": usage(),
        "response": { "candidates": [{ "groundingMetadata": grounding() }] }
    } })
}

/// Gemini streams, each with the client's original request.
pub fn streams() -> Vec<Case> {
    let (first, second) = signatures();
    let typical = typical_request();
    let patch = patch_request();
    let search = search_request();
    let patch_call = |id: Option<&str>, input: &str| {
        let mut call = json!({ "name": "apply_patch", "args": { "input": input } });
        if let Some(id) = id {
            call["id"] = json!(id);
        }
        json!({ "functionCall": call })
    };
    let mut cases = vec![
        stream(
            "typical-text-stream",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "responseId": "abc123", "createTime": "2025-06-01T12:00:00.5Z", "modelVersion": "gemini-2.5-pro-002", "candidates": [{ "content": { "role": "model", "parts": [{ "text": "Planning", "thought": true }] } }] }),
                chunk(
                    json!([{ "text": " the answer.", "thought": true, "thoughtSignature": first }]),
                ),
                chunk(json!([{ "text": "Hello" }])),
                last(json!([{ "text": " world." }]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "usage-after-finish",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(json!([{ "text": "Hi" }])),
                last(json!([{ "text": "!" }]), "STOP", None),
                json!({ "usageMetadata": usage() }),
            ],
        ),
        stream(
            "cpa-usage-metadata",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "candidates": [{ "content": { "parts": [{ "text": "Hi" }] }, "finishReason": "STOP" }], "cpaUsageMetadata": usage() }),
            ],
        ),
        stream(
            "wrapped-cpa-usage-metadata",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "response": chunk(json!([{ "text": "Hi" }])) }),
                json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": "!" }] }, "finishReason": "STOP" }], "cpaUsageMetadata": usage() } }),
            ],
        ),
        stream(
            "usage-replaced-count-by-count",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "candidates": [{ "content": { "parts": [{ "text": "a" }] } }], "usageMetadata": { "promptTokenCount": 10, "thoughtsTokenCount": 3 } }),
                json!({ "candidates": [{ "content": { "parts": [{ "text": "b" }] } }], "usageMetadata": { "candidatesTokenCount": 7 } }),
                json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }], "usageMetadata": { "totalTokenCount": 25, "cachedContentTokenCount": 2 } }),
            ],
        ),
        stream(
            "max-tokens-incomplete",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(json!([{ "text": "Thinking", "thought": true }])),
                chunk(json!([{ "text": "A long answer" }])),
                last(
                    json!([{ "text": " that is cut" }]),
                    "MAX_TOKENS",
                    Some(usage()),
                ),
            ],
        ),
        stream(
            "max-tokens-padded-lowercase",
            "gemini-2.5-pro",
            &typical,
            &[last(
                json!([{ "text": "Cut" }]),
                " max_tokens ",
                Some(usage()),
            )],
        ),
        lines(
            "done-after-finish",
            "gemini-2.5-pro",
            &typical,
            vec![
                line(&chunk(json!([{ "text": "Hi" }]))),
                line(&last(json!([{ "text": "!" }]), "STOP", None)),
                "data: [DONE]".to_owned(),
            ],
        ),
        lines(
            "done-without-finish",
            "gemini-2.5-pro",
            &typical,
            vec![
                line(&chunk(json!([{ "text": "Hi" }]))),
                "data: [DONE]".to_owned(),
            ],
        ),
        stream(
            "finish-without-usage",
            "gemini-2.5-pro",
            &typical,
            &[last(json!([{ "text": "Hi" }]), "STOP", None)],
        ),
        stream(
            "usage-without-finish",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "candidates": [{ "content": { "parts": [{ "text": "Hi" }] } }], "usageMetadata": usage() }),
            ],
        ),
        stream(
            "chunks-after-completion",
            "gemini-2.5-pro",
            &typical,
            &[
                last(json!([{ "text": "Done." }]), "STOP", Some(usage())),
                chunk(json!([{ "text": "Ignored." }])),
                last(json!([{ "text": "Also ignored." }]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "function-calls-mapped-back",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(
                    json!([{ "text": "Let me check.", "thought": true, "thoughtSignature": first }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }, "thoughtSignature": second }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": gemini_name(1, None), "args": { "q": "serde" }, "id": "call_upstream1" } }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": gemini_name(2, Some(0)), "args": { "path": "README.md" } } }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": gemini_name(2, Some(1)), "args": { "input": "ls -la" } } }]),
                ),
                last(
                    json!([{ "functionCall": { "name": "unknown_tool", "args": {} } }]),
                    "STOP",
                    Some(usage()),
                ),
            ],
        ),
        stream(
            "function-call-snapshots",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "args": { "city": "Pa" } }, "partIndex": 0 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }, "partIndex": 0 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "args": { "city": "Rome" } }, "partIndex": 2 }]),
                ),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-call",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(json!([{ "text": "Adding it.", "thought": true }])),
                chunk(json!([patch_call(Some("call_patch1"), PATCH)])),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-repeated-snapshot",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(json!([patch_call(None, PATCH)])),
                chunk(json!([patch_call(Some("call_patch2"), PATCH)])),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-without-finish",
            "gemini-2.5-pro",
            &patch,
            &[chunk(json!([patch_call(Some("call_patch3"), PATCH)]))],
        ),
        stream(
            "apply-patch-finish-without-usage",
            "gemini-2.5-pro",
            &patch,
            &[last(json!([patch_call(None, PATCH)]), "STOP", None)],
        ),
        stream(
            "apply-patch-not-a-patch",
            "gemini-2.5-pro",
            &patch,
            &[last(
                json!([patch_call(None, "rm -rf /")]),
                "STOP",
                Some(usage()),
            )],
        ),
        stream(
            "apply-patch-conflicting-snapshots",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "args": { "input": PATCH } }, "partIndex": 0 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "args": { "input": format!("{PATCH}\n") } }, "partIndex": 0 }]),
                ),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-conflicting-ids",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "args": { "input": PATCH }, "id": "call_one" }, "partIndex": 1 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "args": { "input": PATCH }, "id": "call_two" }, "partIndex": 1 }]),
                ),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-unnamed-call",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(json!([{ "functionCall": { "args": { "input": PATCH } } }])),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-unnamed-call-without-finish",
            "gemini-2.5-pro",
            &patch,
            &[chunk(
                json!([{ "functionCall": { "args": { "input": PATCH } } }]),
            )],
        ),
        lines(
            "apply-patch-with-done",
            "gemini-2.5-pro",
            &patch,
            vec![
                line(&chunk(json!([patch_call(None, PATCH)]))),
                "data: [DONE]".to_owned(),
            ],
        ),
        // Not at a multiple of five: the registry suite also runs those
        // through pairs with no translator, where nothing differs.
        lines(
            "unreadable-line-with-apply-patch",
            "gemini-2.5-pro",
            &patch,
            vec![
                line(&chunk(json!([{ "text": "Hi" }]))),
                ": keep-alive".to_owned(),
                line(&last(json!([{ "text": "!" }]), "STOP", Some(usage()))),
            ],
        )
        .known_difference(UNREADABLE_WITH_PATCH),
        lines(
            "unreadable-lines-without-apply-patch",
            "gemini-2.5-pro",
            &typical,
            vec![
                ": keep-alive".to_owned(),
                "event: message".to_owned(),
                line(&chunk(json!([{ "text": "Hi" }]))),
                " data: {}".to_owned(),
                "data: oops".to_owned(),
                line(&last(json!([{ "text": "!" }]), "STOP", Some(usage()))),
            ],
        ),
        lines(
            "json-lines-that-are-not-chunks",
            "gemini-2.5-pro",
            &typical,
            vec![
                "5".to_owned(),
                "data: null".to_owned(),
                String::new(),
                "data:".to_owned(),
                "[]".to_owned(),
                line(&last(json!([{ "text": "Hi" }]), "STOP", Some(usage()))),
            ],
        ),
        stream(
            "grounded-answer-with-citations",
            "gemini-2.5-pro",
            &search,
            &[
                chunk(json!([{ "text": "Café au lait 🚀 " }])),
                json!({ "candidates": [{ "content": { "parts": [{ "text": "is popular." }] }, "groundingMetadata": grounding() }] }),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "grounding-before-text",
            "gemini-3-pro-preview",
            &search,
            &[
                json!({ "candidates": [{ "content": { "parts": [] }, "groundingMetadata": { "webSearchQueries": ["café news"] } }] }),
                chunk(json!([{ "text": "Café au lait 🚀 is popular." }])),
                json!({ "candidates": [{ "content": { "parts": [{ "text": " More." }] }, "finishReason": "STOP", "groundingMetadata": grounding() }], "usageMetadata": usage() }),
            ],
        ),
        stream(
            "grounding-without-search-tool",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(json!([{ "text": "Café au lait 🚀 is popular." }])),
                json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP", "groundingMetadata": grounding() }], "usageMetadata": usage() }),
            ],
        ),
        stream(
            "trailing-signature-after-text",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(json!([{ "text": "Hello." }])),
                chunk(json!([{ "text": "", "thoughtSignature": first }])),
                chunk(json!([{ "thoughtSignature": second }])),
                last(
                    json!([{ "functionCall": { "name": "get_weather", "args": { "city": "Paris" } } }]),
                    "STOP",
                    Some(usage()),
                ),
            ],
        ),
        stream(
            "signature-before-call",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(json!([{ "thoughtSignature": first }])),
                chunk(json!([{ "functionCall": { "name": "get_weather", "args": {} } }])),
                last(
                    json!([{ "text": "Then text.", "thoughtSignature": second }]),
                    "STOP",
                    Some(usage()),
                ),
            ],
        ),
        stream(
            "standalone-signature",
            "gemini-2.5-pro",
            &typical,
            &[last(
                json!([{ "thought_signature": first }]),
                "STOP",
                Some(usage()),
            )],
        ),
        stream(
            "signatures-with-part-indexes",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(
                    json!([{ "text": "One", "partIndex": 0 }, { "text": "Two", "partIndex": 2 }]),
                ),
                chunk(json!([{ "text": "", "thoughtSignature": first, "partIndex": 0 }])),
                last(
                    json!([{ "text": " more", "partIndex": 2, "thoughtSignature": second }]),
                    "STOP",
                    Some(usage()),
                ),
            ],
        ),
        stream(
            "response-wrapped-stream",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "response": { "responseId": "wrapped1", "candidates": [{ "content": { "parts": [{ "text": "Hi" }] } }] } }),
                json!({ "response": last(json!([{ "text": "!" }]), "STOP", Some(usage())) }),
            ],
        ),
        stream(
            "no-request",
            "",
            &Value::Null,
            &[last(json!([{ "text": "Hi" }]), "STOP", Some(usage()))],
        ),
        lines(
            "done-before-any-chunk",
            "gemini-2.5-pro",
            &typical,
            vec![
                "data: [DONE]".to_owned(),
                line(&last(json!([{ "text": "Late." }]), "STOP", Some(usage()))),
            ],
        ),
        stream(
            "several-candidates-and-prompt-feedback",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "candidates": [
                    { "content": { "parts": [{ "text": "First." }] }, "index": 0 },
                    { "content": { "parts": [{ "text": "Second." }] }, "index": 1, "finishReason": "STOP" }
                ], "promptFeedback": { "safetyRatings": [] } }),
                json!({ "candidates": [
                    { "content": { "parts": [] }, "finishReason": "SAFETY" },
                    { "content": { "parts": [{ "text": "Ignored." }] } }
                ], "usageMetadata": usage() }),
            ],
        ),
        stream(
            "blocked-prompt",
            "gemini-2.5-pro",
            &typical,
            &[json!({ "promptFeedback": { "blockReason": "SAFETY" }, "usageMetadata": usage() })],
        ),
        stream(
            "response-key-without-a-response",
            "gemini-2.5-pro",
            &typical,
            &[
                json!({ "response": { "note": "not a wrapper" }, "candidates": [{ "content": { "parts": [{ "text": "Hi" }] } }] }),
                last(json!([{ "text": "!" }]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "signature-pending-before-a-thought",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(json!([{ "text": "", "thoughtSignature": first }])),
                chunk(
                    json!([{ "text": "Thinking.", "thought": true, "thoughtSignature": second }]),
                ),
                chunk(json!([{ "text": "", "thoughtSignature": second }])),
                chunk(json!([{ "text": "More.", "thought": true, "thoughtSignature": second }])),
                last(json!([{ "text": "Answer." }]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-same-call-twice",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(json!([patch_call(Some("call_twice"), PATCH)])),
                chunk(json!([patch_call(Some("call_twice"), PATCH)])),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "calls-sharing-keys",
            "gemini-2.5-pro",
            &typical,
            &[
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "id": "call_a", "args": { "city": "A" } }, "partIndex": 0 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "id": "call_b", "args": { "city": "B" } }, "partIndex": 1 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "get_weather", "id": "call_b", "args": { "city": "C" } }, "partIndex": 0 }]),
                ),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "apply-patch-calls-sharing-keys",
            "gemini-2.5-pro",
            &patch,
            &[
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "id": "call_pa", "args": { "input": PATCH } }, "partIndex": 0 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "id": "call_pb", "args": { "input": PATCH } }, "partIndex": 1 }]),
                ),
                chunk(
                    json!([{ "functionCall": { "name": "apply_patch", "id": "call_pb", "args": { "input": PATCH } }, "partIndex": 0 }]),
                ),
                last(json!([]), "STOP", Some(usage())),
            ],
        ),
        stream(
            "search-query-from-the-request",
            "gemini-2.5-pro",
            &search,
            &[
                chunk(json!([{ "text": "Café au lait 🚀 is popular." }])),
                json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP", "groundingMetadata": grounding_without_queries() }], "usageMetadata": usage() }),
            ],
        ),
        stream(
            "grounding-support-resolved-later",
            "gemini-2.5-pro",
            &search,
            &[
                json!({ "candidates": [{ "content": { "parts": [{ "text": "Café au lait 🚀 is popular." }] }, "groundingMetadata": {
                    "webSearchQueries": ["café"],
                    "groundingChunks": [{ "web": { "uri": "https://example.com/a", "title": "Example" } }],
                    "groundingSupports": [
                        { "segment": { "startIndex": 0, "endIndex": 5 }, "groundingChunkIndices": [0] },
                        { "segment": { "startIndex": 0, "endIndex": 5 }, "groundingChunkIndices": [1] }
                    ]
                } }] }),
                json!({ "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP", "groundingMetadata": {
                    "groundingChunks": [{ "web": { "uri": "https://example.com/a", "title": "Again" } }],
                    "groundingSupports": [
                        { "segment": { "startIndex": 0, "endIndex": 5 }, "groundingChunkIndices": [0] }
                    ]
                } }], "usageMetadata": usage() }),
            ],
        ),
        stream(
            "grounding-in-a-doubly-wrapped-chunk",
            "gemini-2.5-pro",
            &search,
            &[
                chunk(json!([{ "text": "Café au lait 🚀 is popular." }])),
                doubly_wrapped_grounding(json!([])),
            ],
        ),
    ];

    // The request only as sent to Gemini, or wrapped.
    let mut search_upstream = stream(
        "search-from-the-upstream-request",
        "gemini-2.5-pro",
        &Value::Null,
        &[
            chunk(json!([{ "text": "Café au lait 🚀 " }])),
            json!({ "candidates": [{ "content": { "parts": [{ "text": "is popular." }] }, "finishReason": "STOP", "groundingMetadata": grounding() }], "usageMetadata": usage() }),
        ],
    );
    search_upstream.request = String::new();
    search_upstream.translated_request = json!({ "model": "gemini-2.5-pro", "contents": [{ "role": "user", "parts": [{ "text": "Café news?" }] }], "tools": [{ "googleSearch": {} }] }).to_string();
    cases.push(search_upstream);
    let mut wrapped = stream(
        "wrapped-original-request",
        "gemini-2.5-pro",
        &json!({ "request": typical }),
        &[
            chunk(
                json!([{ "functionCall": { "name": gemini_name(1, None), "args": { "q": "x" } } }]),
            ),
            last(json!([]), "STOP", Some(usage())),
        ],
    );
    wrapped.translated_request =
        json!({ "requestType": "agent", "request": { "contents": [] } }).to_string();
    cases.push(wrapped);
    cases
}

/// Whole Gemini responses, each with the client's original request.
pub fn finals() -> Vec<Case> {
    let (first, second) = signatures();
    let typical = typical_request();
    let patch = patch_request();
    let body = |name: &str, request: &Value, body: Value| Case {
        model: "gemini-2.5-pro".into(),
        ..Case::response(name, request.to_string(), vec![body.to_string()])
    };
    let mut cases = vec![
        body(
            "typical-response",
            &typical,
            json!({
                "responseId": "abc123",
                "createTime": "2025-06-01T12:00:00Z",
                "modelVersion": "gemini-2.5-pro-002",
                "candidates": [{ "content": { "role": "model", "parts": [
                    { "text": "Planning.", "thought": true, "thoughtSignature": first },
                    { "text": "It is sunny." },
                    { "functionCall": { "name": "get_weather", "args": { "city": "Paris" } }, "thoughtSignature": second },
                    { "functionCall": { "name": gemini_name(1, None), "args": { "q": "x" } } }
                ]}, "finishReason": "STOP" }],
                "usageMetadata": usage()
            }),
        ),
        body(
            "max-tokens-response",
            &typical,
            last(json!([{ "text": "Cut" }]), "MAX_TOKENS", Some(usage())),
        ),
        body(
            "wrapped-cpa-usage-response",
            &typical,
            json!({ "response": { "candidates": [{ "content": { "parts": [{ "text": "Hi" }] }, "finishReason": "STOP" }], "cpaUsageMetadata": usage() } }),
        ),
        body(
            "grounded-response",
            &search_request(),
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Café au lait 🚀 is popular." }, { "text": "Two." }] }, "finishReason": "STOP", "groundingMetadata": grounding() }], "usageMetadata": usage() }),
        ),
        body(
            "apply-patch-response",
            &patch,
            last(
                json!([{ "functionCall": { "name": "apply_patch", "args": { "input": PATCH }, "id": "call_upstream" } }]),
                "STOP",
                Some(usage()),
            ),
        ),
        body(
            "apply-patch-not-a-patch-response",
            &patch,
            last(
                json!([{ "functionCall": { "name": "apply_patch", "args": { "input": "nope" } } }]),
                "STOP",
                Some(usage()),
            ),
        ),
        body(
            "apply-patch-unnamed-response",
            &patch,
            last(
                json!([{ "functionCall": { "args": { "input": PATCH } } }]),
                "STOP",
                Some(usage()),
            ),
        ),
        body(
            "signatures-in-a-response",
            &typical,
            last(
                json!([
                    { "thoughtSignature": first },
                    { "text": "Hello." },
                    { "text": "", "thoughtSignature": second },
                    { "functionCall": { "name": "get_weather", "args": {} } }
                ]),
                "STOP",
                None,
            ),
        ),
        body(
            "model-version-fallback",
            &json!({ "input": "hi" }),
            json!({ "modelVersion": "gemini-2.5-flash-001", "candidates": [{ "content": { "parts": [{ "text": "Hi" }] } }] }),
        ),
        body(
            "array-of-chunks-response",
            &typical,
            json!([
                chunk(json!([{ "text": "One." }])),
                last(json!([{ "text": "Two." }]), "STOP", Some(usage()))
            ]),
        ),
        body(
            "several-candidates-response",
            &typical,
            json!({ "candidates": [
                { "content": { "parts": [{ "text": "First." }] }, "finishReason": "SAFETY" },
                { "content": { "parts": [{ "text": "Second." }] }, "finishReason": "STOP" }
            ], "promptFeedback": { "safetyRatings": [] }, "usageMetadata": usage() }),
        ),
        body("no-candidates", &typical, json!({ "candidates": [] })),
        Case {
            model: "gemini-2.5-pro".into(),
            ..Case::response(
                "unreadable-body",
                typical.to_string(),
                vec!["garbage".to_owned()],
            )
        },
        // Not at a multiple of five: the registry suite also runs those
        // through a pair with no translator, where nothing differs.
        Case {
            model: "gemini-2.5-pro".into(),
            ..Case::response(
                "unreadable-body-with-apply-patch",
                patch.to_string(),
                vec!["garbage".to_owned()],
            )
        }
        .known_difference(UNREADABLE_WITH_PATCH),
        body(
            "apply-patch-same-call-twice-response",
            &patch,
            last(
                json!([
                    { "functionCall": { "name": "apply_patch", "args": { "input": PATCH }, "id": "call_twice" } },
                    { "functionCall": { "name": "apply_patch", "args": { "input": PATCH }, "id": "call_twice" } }
                ]),
                "STOP",
                Some(usage()),
            ),
        ),
        body(
            "search-query-from-the-request-response",
            &search_request(),
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Café au lait 🚀 is popular." }] }, "finishReason": "STOP", "groundingMetadata": grounding_without_queries() }], "usageMetadata": usage() }),
        ),
        body(
            "doubly-wrapped-grounding-response",
            &search_request(),
            doubly_wrapped_grounding(json!([{ "text": "Café au lait 🚀 is popular." }])),
        ),
        body(
            "response-key-without-a-response-body",
            &typical,
            json!({ "response": { "note": "not a wrapper" }, "candidates": [{ "content": { "parts": [{ "text": "Hi" }] }, "finishReason": "STOP" }], "usageMetadata": usage() }),
        ),
        body(
            "calls-sharing-keys-response",
            &typical,
            last(
                json!([
                    { "functionCall": { "name": "get_weather", "id": "call_a", "args": { "city": "A" } }, "partIndex": 0 },
                    { "functionCall": { "name": "get_weather", "id": "call_b", "args": { "city": "B" } }, "partIndex": 1 },
                    { "functionCall": { "name": "get_weather", "id": "call_b", "args": { "city": "C" } }, "partIndex": 0 }
                ]),
                "STOP",
                Some(usage()),
            ),
        ),
        body(
            "apply-patch-calls-sharing-keys-response",
            &patch,
            last(
                json!([
                    { "functionCall": { "name": "apply_patch", "id": "call_pa", "args": { "input": PATCH } }, "partIndex": 0 },
                    { "functionCall": { "name": "apply_patch", "id": "call_pb", "args": { "input": PATCH } }, "partIndex": 1 },
                    { "functionCall": { "name": "apply_patch", "id": "call_pb", "args": { "input": PATCH } }, "partIndex": 0 }
                ]),
                "STOP",
                Some(usage()),
            ),
        ),
    ];
    // Grounding without queries, so the search's query comes from wherever
    // the request has one.
    let search_tools = json!([{ "type": "web_search" }]);
    for (name, input, instructions) in [
        (
            "search-query-from-flat-parts",
            json!([{ "type": "input_text", "text": " Café " }, { "type": "input_text", "text": "news?" }]),
            None,
        ),
        (
            "search-query-from-the-last-user-message",
            json!([
                { "role": "user", "content": "Earlier." },
                { "role": "assistant", "content": "Reply." },
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "  " }, { "type": "input_text", "text": "Café news?" }] },
                { "type": "function_call_output", "call_id": "call_x", "output": "skipped" }
            ]),
            None,
        ),
        (
            "search-query-from-a-text-field",
            json!([{ "role": "user", "text": " Café text? " }, { "role": "assistant", "content": "x" }]),
            None,
        ),
        (
            "search-query-from-the-instructions",
            json!([{ "role": "assistant", "content": "x" }, { "role": "user", "content": "   " }]),
            Some(" Find café news. "),
        ),
        ("search-query-from-nothing", json!([]), None),
    ] {
        let mut request =
            json!({ "model": "gemini-2.5-pro", "input": input, "tools": search_tools });
        if let Some(instructions) = instructions {
            request["instructions"] = json!(instructions);
        }
        cases.push(body(
            name,
            &request,
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Café au lait 🚀 is popular." }] }, "finishReason": "STOP", "groundingMetadata": grounding_without_queries() }] }),
        ));
    }
    cases
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry suites also run every fifth case through pairs with no
    /// translator, where a known difference doesn't differ.
    #[test]
    fn known_differences_are_not_at_multiples_of_five() {
        for cases in [requests(), streams(), finals()] {
            for (index, case) in cases.iter().enumerate() {
                assert!(
                    case.known_difference.is_none() || index % 5 != 0,
                    "{} is at {index}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn names_are_unique() {
        for cases in [requests(), streams(), finals()] {
            let mut names: Vec<&str> = cases.iter().map(|case| case.name.as_str()).collect();
            names.sort_unstable();
            let count = names.len();
            names.dedup();
            assert_eq!(names.len(), count);
        }
    }
}
