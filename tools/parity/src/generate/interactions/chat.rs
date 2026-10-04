//! Random input for the Chat Completions and Interactions translators'
//! suites (P4 WP4-B, see `crate::interactions::chat`):
//! - [`chat_request_cases`]: Chat Completions requests from the Chat
//!   Completions generator ([`crate::generate::chat`]), to which are added
//!   now and then what only the translator to Interactions reads: media
//!   parts of every kind it knows (images, audio, files and documents, as
//!   `data:` URLs, raw data or URLs), Responses-style text parts, the
//!   Interactions IDs, an agent config, modalities and generation settings
//!   under each of their names;
//! - [`chunk_cases`]: Chat Completions streams and whole responses from the
//!   upstream generator ([`crate::generate::openai_chat`]);
//! - [`request_cases`]: Interactions requests from the parent module, to
//!   which are added now and then the top-level fields the translator to
//!   Chat Completions copies, its settings under their other names, and
//!   media parts with URLs, file names and audio of every format;
//! - [`event_cases`]: Interactions event streams and responses from the
//!   parent module.

use serde_json::{Map, Value, json};

use super::super::{EFFORTS, NUMBERS, Rng, num};
use super::{odd_value, render, text};
use crate::cases::Case;

/// `data:` URLs, good and bad, and plain URLs.
const URLS: &[&str] = &[
    "data:image/png;base64,aGVsbG8=",
    "data:image/jpeg;BASE64,aGVsbG8=",
    "data:application/pdf;base64,JVBERi0=",
    "data:text/plain;base64,aGk=",
    "data:;base64,aGVsbG8=",
    "data:image/png,aGVsbG8=",
    "data:image/png;base64,",
    "data:image/png;charset=x;base64,aGVsbG8=",
    "https://example.com/a.png",
    "",
];

const FILE_NAMES: &[&str] = &[
    "report.pdf",
    "notes.TXT",
    "photo.png",
    "archive.tar.gz",
    "名前.txt",
    "noext",
    "",
];

const MIME_TYPES: &[&str] = &[
    "application/pdf",
    "image/png",
    "text/plain",
    "text/csv",
    "application/json",
    "image/svg+xml",
    "application/octet-stream",
    "audio/ogg",
    "audio/x-wav",
    "audio/L16",
    "nonsense",
    "",
];

/// `input_audio` formats, known and not.
const AUDIO_FORMATS: &[&str] = &["wav", "mp3", "flac", "opus", "pcm16", " WAV ", "aac", ""];

/// Builds `count` Chat Completions requests for the translator to
/// Interactions.
pub fn chat_request_cases(seed: u64, count: usize) -> Vec<Case> {
    let source = seed.rotate_left(23);
    crate::generate::chat::request_cases(source, count)
        .into_iter()
        .enumerate()
        .map(|(index, mut case)| {
            let mut rng = super::rng(source, (index as u64) ^ 0x4348_4154);
            if rng.chance(60)
                && let Ok(Value::Object(mut request)) = serde_json::from_str(&case.request)
            {
                add_chat_fields(&mut rng, &mut request);
                case.request = render(&mut rng, &Value::Object(request));
            }
            case.name = format!("interactions-chat-{seed}-{index}");
            case
        })
        .collect()
}

/// Builds `count` Chat Completions streams, and a whole response for each,
/// for the translators to Interactions.
pub fn chunk_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    crate::generate::openai_chat::event_cases(seed.rotate_left(29), count)
}

/// Builds `count` Interactions requests for the translator to Chat
/// Completions.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    let source = seed.rotate_left(31);
    super::request_cases(source, count)
        .into_iter()
        .enumerate()
        .map(|(index, mut case)| {
            let mut rng = super::rng(source, (index as u64) ^ 0x4F41_4943);
            if rng.chance(60)
                && let Ok(Value::Object(mut request)) = serde_json::from_str(&case.request)
            {
                add_interactions_fields(&mut rng, &mut request);
                case.request = render(&mut rng, &Value::Object(request));
            }
            case
        })
        .collect()
}

/// Builds `count` Interactions event streams, and a whole response for
/// each, for the translators to Chat Completions.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    super::event_cases(seed.rotate_left(37), count)
}

/// Adds to a Chat Completions request some of what only the translator to
/// Interactions reads.
fn add_chat_fields(rng: &mut Rng, request: &mut Map<String, Value>) {
    if rng.chance(60)
        && let Some(Value::Array(messages)) = request.get_mut("messages")
    {
        for _ in 0..1 + rng.below(2) {
            let at = rng.below(messages.len() + 1);
            let role = rng.pick(&["user", "user", "assistant", "developer", "system", "tool"]);
            let parts: Vec<Value> = (0..1 + rng.below(3)).map(|_| chat_part(rng)).collect();
            let mut message = json!({ "role": role, "content": parts });
            if role == "tool" && rng.chance(70) {
                message["tool_call_id"] = json!(format!("call_{}", rng.below(5)));
            }
            messages.insert(at, message);
        }
    }
    let mut fields: Vec<(&str, Value)> = Vec::new();
    if rng.chance(20) {
        let id = rng.pick(&[json!("interaction_1"), json!(""), json!(" "), json!(7)]);
        fields.push(("previous_interaction_id", id));
    }
    if rng.chance(15) {
        fields.push(("environment_id", rng.pick(&[json!("env_1"), json!("")])));
    }
    if rng.chance(10) {
        fields.push(("environment", json!({ "id": "env_2" })));
    }
    if rng.chance(15) {
        let config = rng.pick(&[
            json!({ "max_total_tokens": 100 }),
            json!({ "type": "agent" }),
            Value::Null,
            json!("text"),
        ]);
        fields.push(("agent_config", config));
    }
    if rng.chance(15) {
        let key = rng.pick(&["modalities", "response_modalities"]);
        let modalities = rng.pick(&[
            json!(["text", "image"]),
            json!(["TEXT"]),
            json!([]),
            json!("text"),
        ]);
        fields.push((key, modalities));
    }
    if rng.chance(10) {
        fields.push(("stream", odd_value(rng)));
    }
    for key in [
        "max_completion_tokens",
        "max_output_tokens",
        "n",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if rng.chance(8) {
            fields.push((key, num(rng.pick(NUMBERS))));
        }
    }
    if rng.chance(10) {
        fields.push(("reasoning_effort", rng.pick(EFFORTS).into()));
    }
    if rng.chance(10) {
        let stop = rng.pick(&[json!("END"), json!(["a", "b"]), json!([]), json!(3)]);
        fields.push(("stop", stop));
    }
    for (key, value) in fields {
        request.insert(key.to_owned(), value);
    }
}

/// A Chat Completions content part the translator to Interactions reads.
fn chat_part(rng: &mut Rng) -> Value {
    let url = |rng: &mut Rng| rng.pick(URLS);
    match rng.below(12) {
        0 => json!({ "type": "image_url", "image_url": { "url": url(rng) } }),
        1 => {
            json!({ "type": rng.pick(&["image_url", "input_image", "Image"]), "image_url": url(rng) })
        }
        2 => json!({ "type": "image", "data": "aGVsbG8=", "mime_type": rng.pick(MIME_TYPES) }),
        3 => json!({ "type": "image", "url": url(rng) }),
        4 => {
            let format = rng.pick(AUDIO_FORMATS);
            json!({ "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": format } })
        }
        5 => {
            let data = rng.pick(&["UklGRg==", ""]);
            json!({ "type": "audio", "data": data, "format": rng.pick(AUDIO_FORMATS) })
        }
        6 | 7 => {
            let mut file = Map::new();
            if rng.chance(70) {
                file.insert("filename".into(), rng.pick(FILE_NAMES).into());
            }
            if rng.chance(70) {
                let data = if rng.chance(50) {
                    rng.pick(URLS)
                } else {
                    rng.pick(&["JVBERi0=", "aGk=", ""])
                };
                file.insert("file_data".into(), data.into());
            }
            if rng.chance(30) {
                file.insert("file_url".into(), "https://example.com/f".into());
            }
            if rng.chance(30) {
                let key = rng.pick(&["mime_type", "mimeType"]);
                file.insert(key.into(), rng.pick(MIME_TYPES).into());
            }
            json!({ "type": "file", "file": file })
        }
        8 => {
            let data_key = rng.pick(&["file_data", "data"]);
            json!({
                "type": rng.pick(&["input_file", "document"]),
                "filename": rng.pick(FILE_NAMES),
                data_key: rng.pick(&["JVBERi0=", "data:application/pdf;base64,JVBERi0=", ""]),
                rng.pick(&["mime_type", "mimeType"]): rng.pick(MIME_TYPES),
            })
        }
        9 => json!({ "type": "input_file", "file_url": "https://example.com/f" }),
        10 => {
            json!({ "type": rng.pick(&["input_text", "output_text", "TEXT", ""]), "text": text(rng) })
        }
        _ => json!({ "type": rng.pick(&["refusal", "video_url"]), "text": text(rng) }),
    }
}

/// Adds to an Interactions request some of what only the translator to
/// Chat Completions reads.
fn add_interactions_fields(rng: &mut Rng, request: &mut Map<String, Value>) {
    if rng.chance(50)
        && let Some(Value::Array(input)) = request.get_mut("input")
    {
        let at = rng.below(input.len() + 1);
        let parts: Vec<Value> = (0..1 + rng.below(3))
            .map(|_| interactions_part(rng))
            .collect();
        let kind = rng.pick(&["user_input", "user_input", "model_output"]);
        input.insert(at, json!({ "type": kind, "content": parts }));
    }
    let mut fields: Vec<(&str, Value)> = Vec::new();
    for key in ["parallel_tool_calls", "seed", "user", "agent_config"] {
        if rng.chance(10) {
            let value = match key {
                "parallel_tool_calls" => json!(rng.chance(50)),
                "seed" => num(rng.pick(NUMBERS)),
                "user" => text(rng).into(),
                _ => json!({ "max_total_tokens": 100 }),
            };
            fields.push((key, value));
        }
    }
    if rng.chance(10) {
        fields.push(("previous_response_id", json!("resp_1")));
    }
    if rng.chance(10) {
        fields.push(("service_tier", odd_value(rng)));
    }
    if rng.chance(10) {
        fields.push(("reasoning_effort", rng.pick(EFFORTS).into()));
    }
    if rng.chance(15) {
        let mut config = Map::new();
        for key in [
            "candidateCount",
            "maxOutputTokens",
            "topP",
            "topK",
            "top_k",
            "stopSequences",
            "thinkingLevel",
        ] {
            if rng.chance(30) {
                let value = match key {
                    "stopSequences" => json!([text(rng)]),
                    "thinkingLevel" => rng.pick(EFFORTS).into(),
                    _ => num(rng.pick(NUMBERS)),
                };
                config.insert(key.into(), value);
            }
        }
        if rng.chance(20) {
            config.insert(
                "thinkingConfig".into(),
                json!({ "thinkingLevel": rng.pick(EFFORTS) }),
            );
        }
        let key = rng.pick(&["generation_config", "generationConfig"]);
        request.entry(key).or_insert_with(|| Value::Object(config));
    }
    for key in [
        "max_tokens",
        "max_completion_tokens",
        "n",
        "temperature",
        "stop",
    ] {
        if rng.chance(5) {
            fields.push((key, num(rng.pick(NUMBERS))));
        }
    }
    for (key, value) in fields {
        request.insert(key.to_owned(), value);
    }
}

/// An Interactions content part the translator to Chat Completions reads.
fn interactions_part(rng: &mut Rng) -> Value {
    match rng.below(8) {
        0 => json!({ "type": "image", rng.pick(&["url", "image_url"]): rng.pick(URLS) }),
        1 => json!({ "type": "audio", "data": "UklGRg==", "mime_type": rng.pick(MIME_TYPES) }),
        2 => json!({ "type": "audio", "url": "https://example.com/a.wav" }),
        3 => json!({ "type": "video", "data": "AAAA", "mime_type": rng.pick(&["video/webm", ""]) }),
        4 => json!({
            "type": rng.pick(&["document", "file"]),
            "data": rng.pick(&["JVBERi0=", ""]),
            "mime_type": rng.pick(MIME_TYPES),
        }),
        5 => json!({
            "type": rng.pick(&["document", "file"]),
            rng.pick(&["url", "file_data"]): rng.pick(URLS),
            "filename": rng.pick(FILE_NAMES),
        }),
        6 => json!({ "type": "text", "text": text(rng) }),
        _ => json!({ "text": text(rng) }),
    }
}
