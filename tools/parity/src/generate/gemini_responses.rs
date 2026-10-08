//! Seeded random input for the translators between Responses and Gemini.
//!
//! Requests aim at what the Responses → Gemini translator reads: tools of
//! every kind (functions with their schema under each key, custom tools,
//! `apply_patch`, namespaces, web search with allowed domains, built-in tools
//! and tools in `additional_tools` items), every shape of `tool_choice`, and
//! input items of every kind: messages with text and media parts of every
//! type and key, system and developer messages before and after the
//! conversation starts, flat content parts, tool calls with IDs under each
//! key, missing, repeated or padded, their outputs paired, reordered,
//! orphaned or missing, as text, JSON or media, reasoning items signed by
//! Gemini, by other providers or in the translator's own signature carriers,
//! and the reasoning fields the translator keeps on calls. Generation
//! settings come loosely typed.
//!
//! Event streams are Gemini's: thought, text and function call parts split
//! across chunks, signed or not, with explicit part indexes, repeated or
//! conflicting call snapshots (`apply_patch` ones included), search grounding
//! with queries, sources and citations, usage under either key, and every
//! way a stream can end: with usage, with a later usage chunk, with `[DONE]`,
//! with a finish reason alone or with none, and lines that aren't chunks.
//! Each comes with a generated request as the client's original, so tool
//! names are mapped back, web search is switched on and request fields are
//! echoed.
//!
//! Inputs the port reads differently by design (see its module docs) are
//! left out:
//! - An object or array where upstream reads text. Upstream then uses its
//!   JSON text as written and the port compact JSON, so requests holding one
//!   are rendered as `serde_json` writes them compactly (see
//!   `Generator::raw_text`), and so are streams (see `Generator::loose`).
//! - A line or body that isn't JSON when the request may declare
//!   `apply_patch`: the port then fails the response where upstream reads
//!   nothing.
//! - Text that starts like JSON but isn't, as a line, a body or function
//!   call arguments, which gjson reads in part.
//! - Integers out of `i64`'s range and temperatures that read as infinite or
//!   NaN, which Go converts by the CPU's rules or writes as non-JSON.
//! - File names and URL paths with `\` or `:`, which Go's `filepath` reads
//!   by the rules of the OS it runs on.
//! - Signature carriers whose payload isn't UTF-8.
//! - Message IDs of the form the stream translator keeps trailing signatures
//!   under (`msg_resp_…`), since upstream's cache would carry those from one
//!   case to the next.

use std::ops::{Deref, DerefMut};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use open_ferry_translate::gemini::openai::responses::convert_openai_responses_request_to_gemini;
use serde_json::{Value, json};

use super::claude_responses::qualify;
use super::{EFFORTS, Rng, SERVICE_TIERS, escape_text, num, to_object};
use crate::cases::Case;

/// Builds `count` random Responses requests for a Gemini upstream.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.rng.pick(MODELS);
            let request = generator.request();
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), model, text)
        })
        .collect()
}

/// Builds `count` random Gemini streams, and a non-streaming case from the
/// whole of each, with the client's request they answer.
pub fn event_cases(seed: u64, count: usize) -> (Vec<Case>, Vec<Case>) {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed.rotate_left(11), index);
            let model = generator.rng.pick(STREAM_MODELS);
            let request = generator.original_request();
            let translated_request = generator.translated_request();
            generator.read_request(&request, &translated_request);
            let chunks = generator.stream();
            let lines = generator.lines(&chunks);
            let body = generator.body(&chunks);
            let case = |events| Case {
                translated_request: translated_request.clone(),
                events,
                ..Case::new(format!("random-{seed}-{index}"), model, request.clone())
            };
            (case(lines), case(vec![body]))
        })
        .unzip()
}

/// Makes `{}` the `arguments` of every function call in a Responses request
/// whose text starts like a JSON object or array but isn't JSON. Upstream
/// copies what gjson reads of it into its output; the port puts the text in
/// `args.arguments` (see its module docs).
pub fn json_arguments(request: &mut Value) {
    let Some(Value::Array(items)) = request.get_mut("input") else {
        return;
    };
    for item in items {
        let Some(Value::String(arguments)) = item.get_mut("arguments") else {
            continue;
        };
        let start = arguments.bytes().position(|b| b > b' ');
        let container =
            start.is_some_and(|start| matches!(arguments.as_bytes()[start], b'{' | b'['));
        if container && serde_json::from_str::<Value>(arguments).is_err() {
            *arguments = "{}".to_owned();
        }
    }
}

/// Makes another suite's stream and response fit the Gemini → Responses
/// translators, for the registry to send them through that pair (see the
/// module docs). The stream's lines the port can't read but gjson reads in
/// part (such as `not json`, which it reads as NaN) are left out: the port
/// reads them as nothing, where upstream starts the response on them. If the
/// request may declare `apply_patch`, so are the other lines the port can't
/// read, and a response it can't read becomes `{}`: the port fails the
/// response there, where upstream reads them as nothing.
pub fn readable_with_patch(mut stream: Case, mut last: Case) -> (Case, Case) {
    fn may_patch(case: &Case) -> bool {
        case.request.contains("apply_patch") || case.translated_request.contains("apply_patch")
    }
    fn readable(text: &str) -> bool {
        let text = text.trim();
        text.is_empty() || text == "[DONE]" || serde_json::from_str::<Value>(text).is_ok()
    }
    stream
        .events
        .retain(|line| !read_in_part(line.strip_prefix("data:").unwrap_or(line)));
    if may_patch(&stream) {
        stream
            .events
            .retain(|line| readable(line.strip_prefix("data:").unwrap_or(line)));
    }
    if may_patch(&last) {
        for body in &mut last.events {
            if !readable(body) {
                *body = "{}".to_owned();
            }
        }
    }
    (stream, last)
}

/// Whether gjson reads `text` in part where `serde_json` can't read it: its
/// first character other than space starts a value for gjson (an object, an
/// array, a number, `null`, `true`, `false` or a string, and `n` other than
/// `null` as NaN).
fn read_in_part(text: &str) -> bool {
    let text = text.trim();
    let starts_value = text
        .chars()
        .next()
        .is_some_and(|first| "{[+-0123456789iINntf\"".contains(first));
    starts_value && text != "[DONE]" && serde_json::from_str::<Value>(text).is_err()
}

/// Makes a response of this suite's that comes with a `data:` prefix the
/// JSON after it, for the registry to send through another pair. The
/// Gemini → Responses translators read such a body as nothing, as their port
/// does; other translators read it with gjson, which reads past the prefix,
/// where their ports read nothing.
pub fn without_data_prefix(mut last: Case) -> Case {
    for body in &mut last.events {
        if let Some(json) = body.strip_prefix("data:") {
            *body = json.trim().to_owned();
        }
    }
    last
}

/// Models with Gemini's reasoning layout, with web search and without, with
/// a thinking suffix, cased differently, unknown to the catalog, and other
/// providers'.
const MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-3-pro-preview",
    "gemini-3.1-pro-preview",
    "gemini-3-flash-preview",
    "gemini-2.5-flash-lite",
    "gemini-2.5-pro(high)",
    "Gemini-2.5-Flash",
    "gemini-test",
    "claude-sonnet-4-6",
    "gpt-5",
    "",
];

/// Models a stream is translated for: the replay cache needs one, and web
/// search mode depends on it.
const STREAM_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-pro",
    "gemini-3-pro-preview",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
    "claude-sonnet-4-6",
    "",
    " ",
];

/// Loosely written integers, none out of `i64`'s range: Go converts those by
/// the CPU's rules.
const INTEGERS: &[&str] = &[
    "0",
    "1",
    "7",
    "1024",
    "64000",
    "-3",
    "1.5",
    "2.0",
    "1e3",
    "9007199254740993",
    "-0",
];

/// Floats, all finite as Go reads them. The last two are halfway between
/// two shortest decimals, which Go rounds to even.
const FLOATS: &[&str] = &[
    "0",
    "1",
    "0.5",
    "1.50",
    "2.0",
    "-0.0",
    "1e3",
    "1E+2",
    "-1.5e-3",
    "0.1",
    "5e-324",
    "9007199254740993",
    "1e30",
    "123456789012345678901234567890",
    "-191224687729131.625",
    "2.98023223876953125e-8",
];

/// Namespace names: plain, already ending in the separator, padded, empty,
/// and long.
const NAMESPACES: &[&str] = &[
    "mcp__github",
    "browser",
    "tools__",
    " spaced ",
    "",
    "mcp__",
    "namespace_with_a_rather_long_name_for_shortening",
];

/// Function call arguments: JSON of every kind, and text that isn't JSON but
/// doesn't start like it either.
const ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"city":"Paris"}"#,
    r#"{"path":"C:\\temp\\a.txt","lines":[1,2]}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    "{ \"spaced\" : true }",
    "{}",
    "",
    "[1,2]",
    " [ ] ",
    "\"text\"",
    "5",
    "true",
    "null",
    "not json",
];

/// Custom tool input as a client sends it back.
const CUSTOM_INPUTS: &[&str] = &[
    "ls -la",
    "",
    PATCH,
    "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch\n",
    "{\"input\":\"x\"}",
    "multi\nline",
];

/// Function call arguments as Gemini streams them.
const STREAMED_ARGUMENTS: &[&str] = &[
    r#"{"city":"Paris"}"#,
    r#"{"city":"Paris"}"#,
    r#"{"q":"café 🚀","n":1.50}"#,
    "{}",
    r#"{"nested":{"a":[1,2]},"b":null}"#,
    r#"{"input":"ls -la"}"#,
    r#"{"input":""}"#,
    r#"{"input":5}"#,
    "[1,2]",
    "\"text\"",
    "5",
    "null",
];

/// A patch as `apply_patch` takes it.
const PATCH: &str = "*** Begin Patch\n*** Add File: hello.txt\n+Hello, world!\n*** End Patch";

/// A patch that changes a file.
const UPDATE_PATCH: &str =
    "*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** End Patch\n";

/// A patch cut short.
const CUT_PATCH: &str = "*** Begin Patch\n*** Add File: cut.txt\n+no end";

/// The signature Gemini accepts without checking it.
const BYPASS: &str = "skip_thought_signature_validator";

/// Where the translator's own signature carriers start.
pub(crate) const CARRIER_PREFIX: &str = "cpa-gemini-responses-carrier-v1:";

/// Request fields the response translators repeat, other than those that
/// also steer the request translator.
const ECHOED: &[&str] = &[
    "max_tool_calls",
    "parallel_tool_calls",
    "previous_response_id",
    "prompt_cache_key",
    "safety_identifier",
    "service_tier",
    "store",
    "top_logprobs",
    "truncation",
    "user",
    "metadata",
];

/// URLs a media part points at: remote ones with and without an extension,
/// data URLs of every shape, and others the translator passes on as data.
const MEDIA_URLS: &[&str] = &[
    "https://example.com/cat.png",
    "HTTPS://EXAMPLE.COM/CAT.JPG",
    "http://example.com/clip.mp4?x=1#frag",
    "gs://bucket/audio.wav",
    "https://example.com/my%20file.pdf",
    "https://example.com/noext",
    "https://example.com/dir/",
    "https://example.com/report.PDF",
    "https://example.com/a.webm",
    "https://example.com/v.mov",
    " https://example.com/padded.gif ",
    "data:image/png;base64,aGVsbG8=",
    "data:image/jpeg;base64,aGVsbG8",
    "data:application/octet-stream;base64,aGVsbG8=",
    "data:;base64,aGVsbG8=",
    "DATA:audio/wav;BASE64,aGVsbG8=",
    " data:video/mp4;base64,aGVsbG8= ",
    "data:text/plain,hello",
    "data:image/png;base64,not base64!",
    "data:image/png;base64,",
    "data:image/png;base64",
    "file-abc123",
    "/tmp/cat.png",
    "relative/clip.mp4",
    "",
];

/// Inline media data: base64 padded and not, a data URL, and text that isn't
/// base64.
const MEDIA_DATA: &[&str] = &[
    "aGVsbG8=",
    "aGVsbG8",
    "AAAA",
    "not base64",
    "",
    "data:image/png;base64,aGVsbG8=",
    "data:application/octet-stream;base64,aGVsbG8=",
    "data:;base64,aGVsbG8",
    "data:text/plain,hello",
];

/// Formats as clients give them: extensions, MIME types and generic ones.
const FORMATS: &[&str] = &[
    "png",
    "JPG",
    "jpeg",
    "webp",
    "application/octet-stream",
    "binary/octet-stream",
    "",
    "wav",
    "mp3",
    "mpeg",
    "ogg",
    "flac",
    "aac",
    "pcm16",
    "pcm",
    "g711_ulaw",
    "g711_alaw",
    "opus",
    "m4a",
    "wma",
    "aiff",
    "mid",
    "mp4",
    "webm",
    "mov",
    "quicktime",
    "avi",
    "x-msvideo",
    "mkv",
    "x-matroska",
    "flv",
    "x-flv",
    "3gpp",
    "3gp",
    "ogv",
    "h264",
    "bin",
    "pdf",
    "txt",
    "csv",
    "weird",
    " PNG ",
    "image/gif",
    "audio/x-custom",
    "video/x-custom",
];

const MIME_TYPES: &[&str] = &[
    "image/png",
    "application/octet-stream",
    "audio/mpeg",
    "video/mp4",
    "application/pdf",
    "text/plain",
    "",
    "image/svg+xml",
];

/// File names, none with `\` or `:` (see the module docs).
const FILENAMES: &[&str] = &[
    "photo.JPG",
    "doc.pdf",
    "clip.mov",
    "song.mp3",
    "noext",
    "archive.tar.gz",
    ".hidden",
    "",
    " spaced.png ",
    "data.csv",
    "notes.txt",
    "voice.WAV",
    "movie.mkv",
    "x.webm",
    "a.b.c",
    "blob.bin",
    "voice.aiff",
    "clip.ogv",
    "notes. ",
];

const DIRECTIONS: &[&str] = &["next", "previous", "standalone"];

const TARGETS: &[&str] = &["text", "function", "any"];

/// How a stream says it is done.
const DONE_LINES: &[&str] = &[
    "data: [DONE]",
    "data: [DONE]",
    "[DONE]",
    "data:[DONE]",
    "data: [DONE]\r",
];

/// A function call part's fields, kept to repeat it as a later snapshot.
#[derive(Clone)]
struct Call {
    name: Option<Value>,
    args: Option<Value>,
    id: Option<String>,
    index: Option<(&'static str, Value)>,
}

/// The generator, for its leaf values: text, numbers, tool names, schemas
/// and signatures.
struct Generator {
    base: super::Generator,
    /// Whether the request holds an object or array where upstream reads
    /// text, so must be rendered compactly and unescaped.
    raw_text: bool,
    /// Namespaces declared in `tools`, for calls and `tool_choice` to use.
    namespaces: Vec<String>,
    /// Call IDs in the request so far, for later calls to repeat.
    call_ids: Vec<Value>,
    /// Whether the request may declare `apply_patch`, so every line and
    /// body must be JSON.
    patch: bool,
    /// Whether a chunk holds an object or array where upstream reads text,
    /// or a source with no URL, so lines and bodies must be written as
    /// `serde_json` writes them compactly.
    loose: bool,
    /// The function names the request declares to Gemini.
    gemini_names: Vec<String>,
    /// Signatures used in the stream so far, for later parts to repeat.
    signatures: Vec<String>,
}

impl Deref for Generator {
    type Target = super::Generator;

    fn deref(&self) -> &super::Generator {
        &self.base
    }
}

impl DerefMut for Generator {
    fn deref_mut(&mut self) -> &mut super::Generator {
        &mut self.base
    }
}

impl Generator {
    fn new(seed: u64, index: u64) -> Self {
        Self {
            base: super::Generator {
                // A different mix from the other generators', so cases don't
                // share their random choices.
                rng: Rng(seed.rotate_left(39) ^ index.wrapping_mul(0xD6E8_FEB8_6659_FD93)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
            raw_text: false,
            namespaces: Vec::new(),
            call_ids: Vec::new(),
            patch: false,
            loose: false,
            gemini_names: Vec::new(),
            signatures: Vec::new(),
        }
    }

    /// Renders the request as the base generator does, unless it holds an
    /// object or array where upstream reads text: then compactly, as the
    /// port writes that text.
    fn render(&mut self, request: &Value) -> String {
        if self.raw_text {
            request.to_string()
        } else {
            self.base.render(request)
        }
    }

    /// Notes that `value`, put where upstream reads text, is an object or
    /// array.
    fn mark_raw(&mut self, value: &Value) {
        if value.is_object() || value.is_array() {
            self.raw_text = true;
        }
    }

    /// A value read as text: usually text, sometimes not.
    fn text_value(&mut self) -> Value {
        let value = self.loose_text();
        self.mark_raw(&value);
        value
    }

    fn integer(&mut self) -> Value {
        if self.rng.chance(85) {
            num(self.rng.pick(INTEGERS))
        } else {
            self.one_of(&[json!("12"), json!("x"), json!(true), Value::Null])
        }
    }

    fn float(&mut self) -> Value {
        if self.rng.chance(80) {
            num(self.rng.pick(FLOATS))
        } else {
            self.one_of(&[
                json!("0.5"),
                json!("abc"),
                json!(""),
                json!("1e3"),
                json!("-2"),
                json!(" 1"),
                json!(true),
                Value::Null,
            ])
        }
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        self.raw_text = false;
        self.base.tool_names.clear();
        self.namespaces.clear();
        self.call_ids.clear();
        let mut fields = Vec::new();
        // Tools and input come first so calls and tool_choice can use their names.
        if self.rng.chance(60) {
            fields.push(("tools", self.tools()));
        }
        if self.rng.chance(95) {
            fields.push(("input", self.input()));
        }
        if self.rng.chance(40) {
            let model =
                self.loose_choice(&["gemini-2.5-pro", "gemini-3-pro-preview", "gpt-5", "", " "]);
            fields.push(("model", model));
        }
        if self.rng.chance(35) {
            let instructions = if self.rng.chance(85) {
                self.text().into()
            } else {
                self.one_of(&[
                    json!(5),
                    Value::Null,
                    json!(["rules"]),
                    json!({ "text": "x" }),
                ])
            };
            self.mark_raw(&instructions);
            fields.push(("instructions", instructions));
        }
        if self.rng.chance(40) {
            fields.push(("tool_choice", self.tool_choice()));
        }
        if self.rng.chance(35) {
            fields.push(("reasoning", self.reasoning()));
        }
        if self.rng.chance(25) {
            fields.push(("max_output_tokens", self.integer()));
        }
        if self.rng.chance(20) {
            fields.push(("temperature", self.float()));
        }
        if self.rng.chance(15) {
            fields.push(("top_p", self.float()));
        }
        if self.rng.chance(15) {
            fields.push(("stop_sequences", self.stop_sequences()));
        }
        if self.rng.chance(20) {
            fields.push(("text", self.text_config()));
        }
        if self.rng.chance(10) {
            fields.push(("stream", self.bool_like()));
        }
        for &key in ECHOED {
            if self.rng.chance(8) {
                let value = self.echoed(key);
                fields.push((key, value));
            }
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// A value for a field the response translators only repeat.
    fn echoed(&mut self, key: &str) -> Value {
        match key {
            "parallel_tool_calls" | "store" => self.bool_like(),
            "max_tool_calls" | "top_logprobs" => self.integer(),
            "service_tier" => self.loose_choice(SERVICE_TIERS),
            "truncation" => self.loose_choice(&["auto", "disabled", ""]),
            // Repeated as JSON, so any value will do.
            "user" => {
                if self.rng.chance(85) {
                    self.loose_choice(&["user-123", "", " padded "])
                } else {
                    json!({ "id": "u-1" })
                }
            }
            "metadata" => self.one_of(&[
                json!({ "k": "v" }),
                json!({}),
                json!("meta"),
                Value::Null,
                json!({ "n": 1.5 }),
            ]),
            _ => self.text_value(),
        }
    }

    // --- Tools ---

    fn tools(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[json!({}), json!("tools"), Value::Null]);
        }
        let count = self.rng.below(5);
        Value::Array((0..count).map(|_| self.tool()).collect())
    }

    fn tool(&mut self) -> Value {
        match self.rng.below(20) {
            0..=6 => self.function_tool(),
            7 => self.chat_function_tool(),
            8 | 9 => self.custom_tool(),
            10 | 11 => self.apply_patch_tool(),
            12 | 13 => self.namespace_tool(),
            14 | 15 => self.web_search_tool(),
            16 => self.one_of(&[
                json!({ "type": "file_search", "vector_store_ids": ["vs_1"] }),
                json!({ "type": "code_interpreter", "container": { "type": "auto" } }),
                json!({ "type": "image_generation" }),
                json!({ "type": "local_shell" }),
                json!({ "type": "mcp", "server_label": "docs", "server_url": "https://example.com/mcp" }),
            ]),
            _ => self.one_of(&[
                json!({ "name": "no_type" }),
                json!({ "type": 5, "name": "numeric_type" }),
                json!(5),
                Value::Null,
                json!({ "type": "function" }),
                json!({ "type": "custom" }),
                json!({ "type": "namespace", "name": "empty" }),
            ]),
        }
    }

    /// A tool name, kept for calls and `tool_choice` to use.
    fn declared_name(&mut self) -> Value {
        let name = self.tool_name();
        self.base.tool_names.push(name.clone());
        name
    }

    fn function_tool(&mut self) -> Value {
        let mut fields = Vec::new();
        match self.rng.below(10) {
            0..=6 => fields.push(("type", json!("function"))),
            7 => fields.push(("type", json!(""))),
            8 => fields.push(("type", json!(" function "))),
            _ => {}
        }
        fields.push(("name", self.declared_name()));
        if self.rng.chance(50) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(85) {
            let key = self.rng.pick(&[
                "parameters",
                "parameters",
                "parameters",
                "parameters",
                "parametersJsonSchema",
                "input_schema",
            ]);
            fields.push((key, self.parameters()));
        }
        if self.rng.chance(20) {
            fields.push(("strict", self.bool_like()));
        }
        self.object(fields)
    }

    /// A function declared the Chat Completions way.
    fn chat_function_tool(&mut self) -> Value {
        let mut function = vec![("name", self.declared_name())];
        if self.rng.chance(50) {
            function.push(("description", self.text().into()));
        }
        if self.rng.chance(80) {
            function.push(("parameters", self.parameters()));
        }
        let function = self.object(function);
        json!({ "type": "function", "function": function })
    }

    /// A schema for a function's parameters, or now and then not one.
    fn parameters(&mut self) -> Value {
        let schema = if self.rng.chance(5) {
            self.one_of(&[Value::Null, json!({}), json!([])])
        } else {
            self.schema(0)
        };
        if schema_reads_as_text(&schema) {
            self.raw_text = true;
        }
        schema
    }

    fn custom_tool(&mut self) -> Value {
        let kind = if self.rng.chance(85) {
            "custom"
        } else {
            " custom "
        };
        let mut fields = vec![("type", json!(kind)), ("name", self.declared_name())];
        if self.rng.chance(50) {
            fields.push(("description", self.text().into()));
        }
        if self.rng.chance(50) {
            fields.push((
                "format",
                json!({ "type": "grammar", "syntax": "lark", "definition": "start: /.+/" }),
            ));
        }
        self.object(fields)
    }

    fn apply_patch_tool(&mut self) -> Value {
        let name = if self.rng.chance(85) {
            "apply_patch"
        } else {
            " apply_patch "
        };
        self.base.tool_names.push(json!("apply_patch"));
        let mut fields = vec![("type", json!("custom")), ("name", json!(name))];
        if self.rng.chance(60) {
            fields.push(("description", json!("Edit files with a patch.")));
        }
        if self.rng.chance(60) {
            fields.push((
                "format",
                json!({ "type": "grammar", "syntax": "lark", "definition": "start: begin_patch hunk+ end_patch" }),
            ));
        }
        self.object(fields)
    }

    fn namespace_tool(&mut self) -> Value {
        let namespace = self.rng.pick(NAMESPACES);
        self.namespaces.push(namespace.to_owned());
        let count = 1 + self.rng.below(3);
        let children: Vec<Value> = (0..count)
            .map(|_| self.namespace_child(namespace))
            .collect();
        let key = if self.rng.chance(85) {
            "tools"
        } else {
            "children"
        };
        let mut fields = vec![("type", json!("namespace")), ("name", json!(namespace))];
        if self.rng.chance(40) {
            fields.push(("description", json!("A namespace.")));
        }
        fields.push((key, Value::Array(children)));
        self.object(fields)
    }

    fn namespace_child(&mut self, namespace: &str) -> Value {
        let (kind, name) = match self.rng.below(10) {
            0 => ("custom", json!("apply_patch")),
            1 | 2 => (self.rng.pick(&["custom", " custom "]), self.tool_name()),
            3 => return json!({ "type": "web_search" }),
            _ => (
                self.rng.pick(&["function", "function", "", " function "]),
                self.tool_name(),
            ),
        };
        if let Value::String(child) = &name {
            let qualified = qualify(namespace.trim(), child);
            self.base.tool_names.push(qualified.into());
        }
        let mut fields = Vec::new();
        if !kind.is_empty() || self.rng.chance(50) {
            fields.push(("type", json!(kind)));
        }
        fields.push(("name", name));
        if kind.trim() != "custom" && self.rng.chance(70) {
            fields.push(("parameters", self.parameters()));
        }
        self.object(fields)
    }

    fn web_search_tool(&mut self) -> Value {
        let kind = self.rng.pick(&[
            "web_search",
            "web_search",
            "web_search_preview",
            "web_search_2025_08_26",
            "web_search_preview_2025_03_11",
            "Web_Search",
        ]);
        let mut fields = vec![("type", json!(kind))];
        if self.rng.chance(50) {
            let domains = self.domains();
            fields.push(("filters", json!({ "allowed_domains": domains })));
        }
        if self.rng.chance(20) {
            fields.push((
                "user_location",
                json!({ "type": "approximate", "country": "FR" }),
            ));
        }
        if self.rng.chance(10) {
            fields.push(("search_context_size", json!("low")));
        }
        self.object(fields)
    }

    fn domains(&mut self) -> Value {
        if self.rng.chance(15) {
            return self.one_of(&[json!("docs.rs"), Value::Null, json!({})]);
        }
        if self.rng.chance(10) {
            return json!([]);
        }
        let count = 1 + self.rng.below(3);
        let domains = (0..count)
            .map(|_| {
                self.one_of(&[
                    json!("docs.rs"),
                    json!("docs.rs"),
                    json!(" example.com "),
                    json!(""),
                    json!(5),
                    Value::Null,
                    json!("github.com"),
                ])
            })
            .collect();
        Value::Array(domains)
    }

    // --- Input items ---

    fn input(&mut self) -> Value {
        if self.rng.chance(5) {
            return match self.rng.below(4) {
                0 | 1 => self.text().into(),
                2 => json!({}),
                _ => self.one_of(&[json!(5), Value::Null]),
            };
        }
        let mut items = Vec::new();
        for _ in 0..1 + self.rng.below(6) {
            self.push_items(&mut items);
        }
        Value::Array(items)
    }

    fn push_items(&mut self, items: &mut Vec<Value>) {
        match self.rng.below(24) {
            0..=5 => items.push(self.message_item("user")),
            6..=8 => items.push(self.message_item("assistant")),
            9 | 10 => items.push(self.system_item()),
            11 => self.flat_parts(items),
            12..=16 => self.tool_calls(items),
            17..=19 => self.reasoning_items(items),
            20 => items.push(json!({
                "type": "web_search_call",
                "id": "ws_1",
                "status": "completed",
                "action": { "type": "search", "query": "weather" }
            })),
            21 => items.push(self.additional_tools()),
            _ => items.push(self.odd_item()),
        }
    }

    fn message_item(&mut self, role: &str) -> Value {
        let assistant = role == "assistant";
        let role_value = match self.rng.below(14) {
            0 => role.to_uppercase().into(),
            1 if assistant => json!("model"),
            1 => json!("User"),
            2 => json!(""),
            3 => self.one_of(&[json!(5), Value::Null]),
            4 => self.one_of(&[json!("tool"), json!("critic")]),
            _ => role.into(),
        };
        let content = match self.rng.below(12) {
            0 => self.text_value(),
            1 => self.one_of(&[Value::Null, json!(5)]),
            _ => {
                let count = 1 + self.rng.below(3);
                let parts = (0..count)
                    .map(|_| {
                        if assistant {
                            self.assistant_part()
                        } else {
                            self.user_part()
                        }
                    })
                    .collect();
                Value::Array(parts)
            }
        };
        let mut fields = Vec::new();
        if self.rng.chance(85) {
            fields.push(("type", json!("message")));
        }
        fields.push(("role", role_value));
        fields.push(("content", content));
        if self.rng.chance(20) {
            // Never `msg_resp_…`: see the module docs.
            fields.push(("id", json!(format!("msg_{}", self.alphanumeric(8)))));
        }
        if assistant && self.rng.chance(15) {
            fields.push(("status", json!("completed")));
        }
        self.object(fields)
    }

    fn user_part(&mut self) -> Value {
        match self.rng.below(14) {
            0..=5 => self.text_part("input_text"),
            6 => self.image_part(),
            7 => self.file_part(),
            8 => self.audio_part(),
            9 => self.video_part(),
            10 => {
                let kind = self.rng.pick(&["output_text", "text", ""]);
                self.text_part(kind)
            }
            _ => self.odd_part(),
        }
    }

    fn assistant_part(&mut self) -> Value {
        match self.rng.below(10) {
            0..=6 => self.text_part("output_text"),
            7 => self.text_part("input_text"),
            8 => json!({ "type": "refusal", "refusal": "I can't." }),
            _ => self.odd_part(),
        }
    }

    fn text_part(&mut self, kind: &str) -> Value {
        let mut fields = Vec::new();
        if !kind.is_empty() {
            fields.push(("type", json!(kind)));
        }
        if self.rng.chance(95) {
            fields.push(("text", self.text_value()));
        }
        if kind == "output_text" && self.rng.chance(20) {
            fields.push(("annotations", json!([])));
        }
        self.object(fields)
    }

    fn odd_part(&mut self) -> Value {
        self.one_of(&[
            json!("plain string part"),
            json!(5),
            Value::Null,
            json!({}),
            json!({ "type": "input_text" }),
            json!({ "type": "unknown_part", "text": "x" }),
            json!({ "type": " INPUT_IMAGE ", "image_url": "https://example.com/odd.png" }),
            json!({ "type": "input_file", "file_id": "file-abc123" }),
        ])
    }

    /// A URL for a media part, now and then not text.
    fn media_url(&mut self) -> Value {
        if self.rng.chance(3) {
            return json!(5);
        }
        json!(self.rng.pick(MEDIA_URLS))
    }

    fn media_data(&mut self) -> Value {
        if self.rng.chance(3) {
            return json!(5);
        }
        json!(self.rng.pick(MEDIA_DATA))
    }

    /// A URL object, with its URL or, read as text by upstream, without.
    fn url_object(&mut self) -> Value {
        if self.rng.chance(20) {
            self.raw_text = true;
            return json!({ "detail": "low" });
        }
        let url = self.media_url();
        self.raw_text = true;
        json!({ "url": url, "detail": "high" })
    }

    fn base64_source(&mut self) -> Value {
        let kind = self.rng.pick(&["base64", "base64", "url"]);
        let media_type =
            self.rng
                .pick(&["image/png", "", "application/octet-stream", "audio/mpeg"]);
        let data = self.media_data();
        json!({ "type": kind, "media_type": media_type, "data": data })
    }

    /// Adds the fields a media part's type is read from: format, MIME type,
    /// file name, at the top or nested.
    fn media_details(&mut self, fields: &mut Vec<(&'static str, Value)>, nested: &'static str) {
        if self.rng.chance(25) {
            fields.push(("format", json!(self.rng.pick(FORMATS))));
        }
        if self.rng.chance(15) {
            fields.push(("mime_type", json!(self.rng.pick(MIME_TYPES))));
        }
        if self.rng.chance(20) {
            fields.push(("filename", json!(self.rng.pick(FILENAMES))));
        }
        if self.rng.chance(5) {
            let filename = self.rng.pick(FILENAMES);
            let mime_type = self.rng.pick(MIME_TYPES);
            fields.push((
                "file",
                json!({ "filename": filename, "mime_type": mime_type }),
            ));
        }
        if self.rng.chance(8) && !fields.iter().any(|(key, _)| *key == nested) {
            let format = self.rng.pick(FORMATS);
            fields.push((nested, json!({ "format": format })));
        }
    }

    fn image_part(&mut self) -> Value {
        let kind = self.rng.pick(&[
            "input_image",
            "input_image",
            "image_url",
            "image",
            " Input_Image ",
        ]);
        let mut fields = vec![("type", json!(kind))];
        match self.rng.below(9) {
            0 | 1 => fields.push(("image_url", self.media_url())),
            2 | 3 => fields.push(("image_url", self.url_object())),
            4 => fields.push(("url", self.media_url())),
            5 => fields.push(("source", self.base64_source())),
            6 | 7 => fields.push(("data", self.media_data())),
            _ => fields.push(("file_id", json!("file-abc123"))),
        }
        let nested = self.rng.pick(&["input_image", "image"]);
        self.media_details(&mut fields, nested);
        self.object(fields)
    }

    fn audio_part(&mut self) -> Value {
        let kind = self.rng.pick(&["input_audio", "input_audio", "audio"]);
        let mut fields = vec![("type", json!(kind))];
        let key = self.rng.pick(&["input_audio", "audio"]);
        match self.rng.below(9) {
            0..=2 => {
                let data = self.media_data();
                let format = self.rng.pick(FORMATS);
                fields.push((key, json!({ "data": data, "format": format })));
            }
            3 => fields.push(("data", self.media_data())),
            4 => fields.push(("audio_url", self.media_url())),
            5 => fields.push(("audio_url", self.url_object())),
            6 => fields.push(("url", self.media_url())),
            7 => fields.push(("source", self.base64_source())),
            _ => {}
        }
        self.media_details(&mut fields, key);
        self.object(fields)
    }

    fn video_part(&mut self) -> Value {
        let kind = self
            .rng
            .pick(&["input_video", "input_video", "video_url", "video"]);
        let mut fields = vec![("type", json!(kind))];
        let key = self.rng.pick(&["input_video", "video"]);
        match self.rng.below(9) {
            0 | 1 => {
                let data = self.media_data();
                let format = self.rng.pick(FORMATS);
                fields.push((key, json!({ "data": data, "format": format })));
            }
            2 => fields.push(("data", self.media_data())),
            3 | 4 => fields.push(("video_url", self.media_url())),
            5 => fields.push(("video_url", self.url_object())),
            6 => fields.push(("url", self.media_url())),
            7 => fields.push(("source", self.base64_source())),
            _ => {}
        }
        self.media_details(&mut fields, key);
        self.object(fields)
    }

    fn file_part(&mut self) -> Value {
        let kind = self.rng.pick(&["input_file", "input_file", "file"]);
        let mut fields = vec![("type", json!(kind))];
        match self.rng.below(10) {
            0 | 1 => fields.push(("file_data", self.media_data())),
            2 => {
                let data = self.media_data();
                let filename = self.rng.pick(FILENAMES);
                fields.push(("file", json!({ "file_data": data, "filename": filename })));
            }
            3 => fields.push(("file_url", self.media_url())),
            4 => fields.push(("file_url", self.url_object())),
            5 => {
                let url = self.media_url();
                fields.push(("file", json!({ "file_url": url })));
            }
            6 => fields.push(("url", self.media_url())),
            7 => fields.push(("data", self.media_data())),
            _ => fields.push(("file_id", json!("file-abc123"))),
        }
        self.media_details(&mut fields, "file");
        self.object(fields)
    }

    /// A system or developer message, before the conversation or in it.
    fn system_item(&mut self) -> Value {
        let role = self
            .rng
            .pick(&["system", "developer", "System", "DEVELOPER"]);
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            fields.push(("type", json!("message")));
        }
        fields.push(("role", json!(role)));
        match self.rng.below(8) {
            0..=2 => fields.push(("content", self.text_value())),
            3..=6 => {
                let count = 1 + self.rng.below(3);
                let parts = (0..count)
                    .map(|_| match self.rng.below(6) {
                        0 => json!("plain system text"),
                        1 => json!({ "text": 5 }),
                        _ => {
                            let text = self.text_value();
                            json!({ "type": "input_text", "text": text })
                        }
                    })
                    .collect();
                fields.push(("content", Value::Array(parts)));
            }
            _ => {}
        }
        self.object(fields)
    }

    /// Content parts given as items of their own, with no role.
    fn flat_parts(&mut self, items: &mut Vec<Value>) {
        for _ in 0..1 + self.rng.below(3) {
            let part = match self.rng.below(5) {
                0 => self.image_part(),
                1 => self.file_part(),
                _ => {
                    let kind = self
                        .rng
                        .pick(&["input_text", "input_text", "output_text", "text"]);
                    self.text_part(kind)
                }
            };
            items.push(part);
        }
    }

    /// Tool calls, then their outputs: all, some or none, in order or not,
    /// and now and then one for no call.
    fn tool_calls(&mut self, items: &mut Vec<Value>) {
        let mut calls = Vec::new();
        for _ in 0..1 + self.rng.below(3) {
            let id = self.call_id();
            let custom = self.rng.chance(25);
            items.push(self.tool_call(id.clone(), custom));
            calls.push((id.map(|(_, id)| id), custom));
            if self.rng.chance(12) {
                items.push(self.post_call_reasoning());
            }
        }
        if self.rng.chance(10) {
            let item = if self.rng.chance(50) {
                self.message_item("user")
            } else {
                self.system_item()
            };
            items.push(item);
        }
        if self.rng.chance(12) {
            return;
        }
        let mut outputs = Vec::new();
        for (id, custom) in calls {
            if self.rng.chance(85) {
                outputs.push(self.tool_output(id, custom));
            }
        }
        if self.rng.chance(10) {
            let id = self.rng.chance(50).then(|| json!("call_orphan"));
            outputs.push(self.tool_output(id, false));
        }
        if self.rng.chance(40) {
            self.rng.shuffle(&mut outputs);
        }
        items.extend(outputs);
    }

    /// A reasoning item with no summary after a call, whose signature the
    /// call may take: Gemini's, or in a carrier pointing either way.
    fn post_call_reasoning(&mut self) -> Value {
        let signature = self.gemini_signature();
        let content = match self.rng.below(5) {
            0 | 1 => signature,
            2 => carrier(&signature, "previous", "function"),
            3 => carrier(&signature, "previous", "any"),
            _ => carrier(&signature, "next", "function"),
        };
        let mut fields = vec![("type", json!("reasoning"))];
        if self.rng.chance(20) {
            let id = format!("rs_{}_detached_after_0", self.alphanumeric(6));
            fields.push(("id", json!(id)));
        }
        fields.push(("summary", json!([])));
        fields.push(("encrypted_content", json!(content)));
        self.object(fields)
    }

    /// A call's ID and the key it is under, or none.
    fn call_id(&mut self) -> Option<(&'static str, Value)> {
        if !self.call_ids.is_empty() && self.rng.chance(5) {
            return Some(("call_id", self.base.rng.pick(&self.call_ids)));
        }
        let id = match self.rng.below(20) {
            0 => json!(""),
            1 => json!(5),
            2 => json!(" call_padded "),
            _ => json!(format!("call_{}", self.alphanumeric(8))),
        };
        let key = match self.rng.below(20) {
            0 => return None,
            1 => "tool_call_id",
            2 => "callId",
            3 => "id",
            _ => "call_id",
        };
        self.call_ids.push(id.clone());
        Some((key, id))
    }

    fn call_name(&mut self) -> Value {
        if !self.tool_names.is_empty() && self.rng.chance(70) {
            self.base.rng.pick(&self.base.tool_names)
        } else {
            self.tool_name()
        }
    }

    fn tool_call(&mut self, id: Option<(&'static str, Value)>, custom: bool) -> Value {
        let kind = if custom {
            "custom_tool_call"
        } else {
            "function_call"
        };
        let mut fields = vec![("type", json!(kind))];
        let id_key = id.as_ref().map(|(key, _)| *key);
        if let Some((key, id)) = id {
            fields.push((key, id));
        }
        if self.rng.chance(95) {
            fields.push(("name", self.call_name()));
        }
        if self.rng.chance(15) {
            fields.push(("namespace", self.choice_namespace()));
        }
        if custom {
            match self.rng.below(10) {
                0..=6 => fields.push(("input", json!(self.rng.pick(CUSTOM_INPUTS)))),
                7 => {
                    // Copied as JSON; whether read as text varies, so rendered compactly.
                    self.raw_text = true;
                    let input = self.one_of(&[json!({ "command": "ls" }), json!(["a"])]);
                    fields.push(("input", input));
                }
                8 => {
                    let input = self.one_of(&[json!(5), Value::Null, json!(true), num("1.50")]);
                    fields.push(("input", input));
                }
                _ => {}
            }
        } else {
            match self.rng.below(20) {
                0..=15 => fields.push(("arguments", json!(self.rng.pick(ARGUMENTS)))),
                16 => {
                    self.raw_text = true;
                    fields.push(("arguments", json!({ "city": "Paris" })));
                }
                17 => {
                    let arguments = self.one_of(&[json!(5), Value::Null, json!(true)]);
                    fields.push(("arguments", arguments));
                }
                _ => {}
            }
        }
        if self.rng.chance(10) {
            fields.push(("_cpa_reasoning_signature", self.call_signature()));
        }
        if self.rng.chance(8) {
            fields.push(("_cpa_reasoning_summary", self.text().into()));
        }
        if self.rng.chance(3) {
            fields.push(("_cpa_reasoning_direction", json!("next")));
            fields.push(("_cpa_reasoning_target", json!("function")));
        }
        if self.rng.chance(10) {
            fields.push(("status", json!("completed")));
        }
        if id_key != Some("id") && self.rng.chance(10) {
            fields.push(("id", json!(format!("fc_{}", self.alphanumeric(8)))));
        }
        self.object(fields)
    }

    /// The signature a call carries from the reasoning before it.
    fn call_signature(&mut self) -> Value {
        match self.rng.below(6) {
            0..=2 => json!(self.gemini_signature()),
            3 => json!(BYPASS),
            4 => json!(self.signature_text()),
            _ => self.one_of(&[json!(""), json!(" ")]),
        }
    }

    fn tool_output(&mut self, id: Option<Value>, custom: bool) -> Value {
        let kind = if custom != self.rng.chance(10) {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        let mut fields = vec![("type", json!(kind))];
        match (id, self.rng.below(10)) {
            (Some(id), 0..=7) => fields.push(("call_id", id)),
            (Some(id), 8) => {
                let key = self.rng.pick(&["tool_call_id", "callId"]);
                fields.push((key, id));
            }
            _ => {
                if self.rng.chance(50) {
                    fields.push(("id", json!(format!("fco_{}", self.alphanumeric(6)))));
                }
            }
        }
        if self.rng.chance(20) {
            fields.push(("name", self.call_name()));
        }
        if let Some(output) = self.output() {
            fields.push(("output", output));
        }
        self.object(fields)
    }

    /// A tool's output: text, JSON, media, or blocks of them.
    fn output(&mut self) -> Option<Value> {
        let output = match self.rng.below(16) {
            0..=4 => self.text().into(),
            5 => json!(""),
            6 => json!("null"),
            7 => self.number(),
            8 => self.one_of(&[
                json!({ "ok": true, "n": 1.50 }),
                json!({ "$ref": "#/defs/a" }),
                json!({}),
            ]),
            9 => self.image_part(),
            10..=12 => {
                let count = 1 + self.rng.below(3);
                let blocks = (0..count)
                    .map(|_| match self.rng.below(7) {
                        0 | 1 => {
                            let text = self.text();
                            json!({ "type": "input_text", "text": text })
                        }
                        2 => json!({ "type": "output_text", "text": "done" }),
                        3 => json!("plain block"),
                        4 => self.image_part(),
                        5 => self.file_part(),
                        _ => json!({ "type": "json", "value": 1 }),
                    })
                    .collect();
                Value::Array(blocks)
            }
            13 => json!([]),
            14 => self.one_of(&[Value::Null, json!(true)]),
            _ => return None,
        };
        self.mark_raw(&output);
        Some(output)
    }

    /// A reasoning item, and what follows it: the text or call it signs,
    /// another reasoning item, or nothing.
    fn reasoning_items(&mut self, items: &mut Vec<Value>) {
        if self.rng.chance(15) {
            // A reasoning item whose signature follows in a detached one.
            let text = self.text();
            items.push(json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": text }] }));
            let signature = self.gemini_signature();
            let id = format!(
                "rs_{}_detached_after_{}",
                self.alphanumeric(6),
                self.rng.below(4)
            );
            items.push(json!({ "type": "reasoning", "id": id, "summary": [], "encrypted_content": signature }));
        } else {
            items.push(self.reasoning_item());
        }
        match self.rng.below(5) {
            0 | 1 => items.push(self.message_item("assistant")),
            2 => self.tool_calls(items),
            3 => items.push(self.reasoning_item()),
            _ => {}
        }
    }

    fn reasoning_item(&mut self) -> Value {
        let mut fields = vec![("type", json!("reasoning"))];
        if self.rng.chance(30) {
            let id = match self.rng.below(4) {
                0 => format!(
                    "rs_{}_detached_after_{}",
                    self.alphanumeric(6),
                    self.rng.below(4)
                ),
                1 => format!(
                    "rs_{}_detached_before_{}",
                    self.alphanumeric(6),
                    self.rng.below(4)
                ),
                _ => format!("rs_{}", self.alphanumeric(8)),
            };
            fields.push(("id", json!(id)));
        }
        match self.rng.below(10) {
            0..=5 => {
                let text = self.text();
                fields.push(("summary", json!([{ "type": "summary_text", "text": text }])));
            }
            6 => fields.push(("summary", json!([]))),
            7 => fields.push(("summary", json!([{ "type": "summary_text", "text": 5 }]))),
            8 => {
                let (first, second) = (self.text(), self.text());
                fields.push((
                    "summary",
                    json!([
                        { "type": "summary_text", "text": first },
                        { "type": "summary_text", "text": second }
                    ]),
                ));
            }
            _ => {}
        }
        if let Some(content) = self.encrypted_content() {
            fields.push(("encrypted_content", content));
        }
        self.object(fields)
    }

    /// A reasoning item's signature: Gemini's, in a carrier, another
    /// provider's, or none.
    fn encrypted_content(&mut self) -> Option<Value> {
        let content = match self.rng.below(20) {
            0..=5 => self.gemini_signature(),
            6 => BYPASS.to_owned(),
            7..=9 => {
                let signature = self.gemini_signature();
                let direction = self.rng.pick(DIRECTIONS);
                let target = self.rng.pick(TARGETS);
                carrier(&signature, direction, target)
            }
            10 => self.bad_carrier(),
            11 => {
                let direction = self.rng.pick(DIRECTIONS);
                carrier(BYPASS, direction, "text")
            }
            12 | 13 => self.signature_text(),
            14 => String::new(),
            15 => format!(" {} ", self.gemini_signature()),
            16 => return Some(self.one_of(&[Value::Null, json!(5)])),
            _ => return None,
        };
        Some(content.into())
    }

    /// A carrier upstream rejects: an unknown direction or target, a payload
    /// that isn't unpadded base64 or is empty, too few fields, or a carrier
    /// in a carrier.
    fn bad_carrier(&mut self) -> String {
        let signature = self.gemini_signature();
        match self.rng.below(8) {
            0 => carrier(&signature, "sideways", "text"),
            1 => carrier(&signature, "next", "image"),
            2 => format!("{CARRIER_PREFIX}next:text:!!!"),
            3 => format!("{CARRIER_PREFIX}next:text:{}", STANDARD.encode(&signature)),
            4 => format!("{CARRIER_PREFIX}next:text:"),
            5 => format!("{CARRIER_PREFIX}next"),
            6 => format!("{CARRIER_PREFIX}previous:function"),
            _ => carrier(&carrier(&signature, "next", "text"), "next", "text"),
        }
    }

    fn gemini_signature(&mut self) -> String {
        let len = 5 + self.rng.below(96);
        let tink: Vec<u8> = (0..len).map(|_| self.rng.next() as u8).collect();
        gemini_signature(&tink)
    }

    fn additional_tools(&mut self) -> Value {
        let count = 1 + self.rng.below(2);
        let tools: Vec<Value> = (0..count)
            .map(|_| {
                if self.rng.chance(30) {
                    self.custom_tool()
                } else {
                    self.function_tool()
                }
            })
            .collect();
        json!({ "type": "additional_tools", "tools": tools })
    }

    fn odd_item(&mut self) -> Value {
        self.one_of(&[
            json!({ "type": "message" }),
            json!({ "role": "user", "content": "no type" }),
            json!({ "type": "unknown_item" }),
            json!(5),
            json!("plain item"),
            Value::Null,
            json!({ "type": "compaction", "encrypted_content": "opaque" }),
            json!({ "type": "item_reference", "id": "ref_1" }),
        ])
    }

    // --- Other request fields ---

    fn tool_choice(&mut self) -> Value {
        match self.rng.below(12) {
            0..=2 => self.loose_choice(&[
                "auto", "required", "none", "any", " auto ", "AUTO", "", "bogus",
            ]),
            3..=6 => {
                let name = self.choice_name();
                let kind = self
                    .rng
                    .pick(&["function", "function", "tool", "", " Function "]);
                match self.rng.below(5) {
                    0 | 1 => json!({ "type": kind, "name": name }),
                    2 => json!({ "type": kind, "function": { "name": name } }),
                    3 => {
                        let namespace = self.choice_namespace();
                        json!({ "type": kind, "name": name, "namespace": namespace })
                    }
                    _ => {
                        let namespace = self.choice_namespace();
                        json!({ "type": kind, "function": { "name": name, "namespace": namespace } })
                    }
                }
            }
            7 => {
                let name = self.choice_name();
                match self.rng.below(3) {
                    0 => json!({ "type": "custom", "name": name }),
                    1 => json!({ "type": "custom", "custom": { "name": name } }),
                    _ => {
                        let namespace = self.choice_namespace();
                        json!({ "type": "custom", "custom": { "name": name, "namespace": namespace } })
                    }
                }
            }
            8 => {
                let name = self.choice_name();
                json!({ "type": "allowed_tools", "mode": "required", "tools": [{ "type": "function", "name": name }, { "type": "web_search" }] })
            }
            9 => self.one_of(&[
                json!({ "type": "web_search" }),
                json!({ "type": "web_search_preview" }),
                json!({ "type": "file_search" }),
            ]),
            10 => self.one_of(&[
                json!({ "type": "auto" }),
                json!({ "type": "none" }),
                json!({ "type": "required" }),
                json!({ "type": "any" }),
                json!({}),
            ]),
            _ => self.one_of(&[Value::Null, json!(5), json!(["auto"])]),
        }
    }

    fn choice_name(&mut self) -> Value {
        let name = if !self.tool_names.is_empty() && self.rng.chance(75) {
            self.base.rng.pick(&self.base.tool_names)
        } else {
            self.tool_name()
        };
        match name {
            Value::String(text) if self.rng.chance(10) => format!(" {text} ").into(),
            name => name,
        }
    }

    fn choice_namespace(&mut self) -> Value {
        if !self.namespaces.is_empty() && self.rng.chance(70) {
            json!(self.base.rng.pick(&self.namespaces))
        } else {
            json!(self.rng.pick(NAMESPACES))
        }
    }

    fn reasoning(&mut self) -> Value {
        if self.rng.chance(8) {
            return self.one_of(&[json!("high"), Value::Null, json!([])]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(80) {
            fields.push(("effort", self.effort()));
        }
        if self.rng.chance(30) {
            let summary = self.loose_choice(&["auto", "concise", "detailed", "none"]);
            fields.push(("summary", summary));
        }
        self.object(fields)
    }

    fn effort(&mut self) -> Value {
        match self.rng.below(10) {
            0..=5 => {
                self.loose_choice(&["auto", "minimal", "low", "medium", "high", "none", "xhigh"])
            }
            6..=7 => self.loose_choice(&[" High ", "AUTO", " auto ", "", "ultra"]),
            _ => self.loose_choice(EFFORTS),
        }
    }

    fn stop_sequences(&mut self) -> Value {
        if self.rng.chance(10) {
            return self.one_of(&[json!("stop"), Value::Null, json!({})]);
        }
        let count = self.rng.below(4);
        Value::Array((0..count).map(|_| self.text_value()).collect())
    }

    fn text_config(&mut self) -> Value {
        match self.rng.below(9) {
            0..=2 => {
                let schema = self.schema(0);
                let kind = self.loose_choice(&["json_schema", "json_schema", " JSON_Schema "]);
                json!({ "format": { "type": kind, "name": "out", "schema": schema, "strict": true } })
            }
            3 => {
                let schema = self.schema(0);
                json!({ "format": { "type": "json_schema", "json_schema": { "schema": schema } } })
            }
            4 => {
                let kind = self.loose_choice(&["json_object", " JSON_OBJECT ", "text"]);
                json!({ "format": { "type": kind } })
            }
            5 => json!({ "verbosity": "low" }),
            6 => json!({ "format": { "type": "json_schema" } }),
            _ => self.one_of(&[json!("text"), Value::Null, json!(5)]),
        }
    }

    // --- Event streams ---

    /// The client's request, as upstream's response translators are given
    /// it: usually one the generator builds, now and then wrapped, absent,
    /// or not JSON.
    fn original_request(&mut self) -> String {
        match self.rng.below(100) {
            0..=9 => String::new(),
            10..=12 => "garbage".to_owned(),
            13 | 14 => "[]".to_owned(),
            15 => "{not json".to_owned(),
            16..=20 => {
                let request = self.request();
                self.render(&json!({ "request": request }))
            }
            _ => {
                let request = self.request();
                self.render(&request)
            }
        }
    }

    /// The request as sent to Gemini, which upstream reads where the original
    /// is missing, and for whether web search is on.
    fn translated_request(&mut self) -> String {
        match self.rng.below(20) {
            0..=9 => String::new(),
            10 => "garbage".to_owned(),
            11 | 12 => {
                let mut fields = vec![(
                    "contents",
                    json!([{ "role": "user", "parts": [{ "text": "hi" }] }]),
                )];
                if self.rng.chance(60) {
                    fields.push(("tools", json!([{ "googleSearch": {} }])));
                }
                if self.rng.chance(40) {
                    fields.push(("model", json!("gemini-2.5-pro")));
                }
                to_object(fields).to_string()
            }
            13 => self
                .one_of(&[
                    json!({ "requestType": "web_search", "request": { "contents": [] } }),
                    json!({ "request": { "contents": [], "tools": [{ "googleSearch": {} }] } }),
                    json!({ "requestType": "agent", "request": { "contents": [] } }),
                ])
                .to_string(),
            14 | 15 => {
                let model = self.loose_choice(&["gemini-2.5-pro", "", " "]);
                json!({ "model": model }).to_string()
            }
            _ => {
                let request = self.request();
                self.render(&request)
            }
        }
    }

    /// Reads what the stream's chunks depend on from the request upstream
    /// takes tool names from: whether it may declare `apply_patch`, and the
    /// names it declares to Gemini.
    fn read_request(&mut self, request: &str, translated_request: &str) {
        self.patch = request.contains("apply_patch") || translated_request.contains("apply_patch");
        let picked = [request, translated_request]
            .into_iter()
            .filter(|text| !text.is_empty())
            .find_map(|text| serde_json::from_str::<Value>(text).ok());
        let Some(mut root) = picked else {
            return;
        };
        let inner = root
            .get("request")
            .filter(|inner| {
                ["model", "input", "tools"]
                    .iter()
                    .any(|key| inner.get(key).is_some())
            })
            .cloned();
        if let Some(inner) = inner {
            root = inner;
        }
        if !root.is_object() {
            return;
        }
        let (gemini, _) = convert_openai_responses_request_to_gemini("gemini-2.5-pro", &root, true);
        self.gemini_names = gemini["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["functionDeclarations"].as_array())
            .flatten()
            .filter_map(|declaration| declaration["name"].as_str().map(str::to_owned))
            .collect();
    }

    /// The chunks of a stream, `None` standing for `[DONE]`.
    fn stream(&mut self) -> Vec<Option<Value>> {
        let parts = self.parts();
        let mut groups: Vec<Vec<Value>> = vec![Vec::new()];
        for part in parts {
            if groups.last().is_some_and(|group| !group.is_empty()) && self.rng.chance(50) {
                groups.push(Vec::new());
            }
            groups.last_mut().expect("there is a group").push(part);
        }
        if self.rng.chance(5) {
            groups.insert(0, Vec::new());
        }
        let grounding = self.rng.chance(25);
        let finish = self.finish_reason();
        // 0-3 and 9: finish and usage at the end; 4: usage in a chunk of its
        // own after; 5: [DONE] after; 6: no usage; 7: no finish reason; 8:
        // usage early too.
        let ending = self.rng.below(10);
        let last = groups.len() - 1;
        let mut chunks = Vec::new();
        for (at, parts) in groups.into_iter().enumerate() {
            let mut candidate = Vec::new();
            if !parts.is_empty() || self.rng.chance(60) {
                let mut content = vec![("parts", Value::Array(parts))];
                if self.rng.chance(80) {
                    content.insert(0, ("role", json!("model")));
                }
                candidate.push(("content", self.object(content)));
            }
            if (at == last && ending != 7) || (at < last && self.rng.chance(4)) {
                candidate.push(("finishReason", finish.clone()));
            }
            if grounding && (at == last || self.rng.chance(30)) {
                candidate.push(("groundingMetadata", self.grounding()));
            }
            if self.rng.chance(30) {
                candidate.push(("index", json!(0)));
            }
            let candidate = self.object(candidate);
            let mut fields = vec![("candidates", json!([candidate]))];
            if at == 0 {
                self.meta(&mut fields);
            } else if self.rng.chance(10) {
                fields.push(("modelVersion", json!("gemini-2.5-pro-002")));
            }
            if (at == last && !matches!(ending, 4..=6)) || (ending == 8 && at == 0) {
                let usage = self.usage();
                fields.push((self.usage_key(), usage));
            }
            let chunk = self.object(fields);
            chunks.push(Some(self.wrap(chunk)));
        }
        match ending {
            4 => {
                let usage = self.usage();
                let chunk = to_object(vec![(self.usage_key(), usage)]);
                chunks.push(Some(self.wrap(chunk)));
            }
            5 => chunks.push(None),
            9 => {
                // Chunks after the end, which upstream ignores.
                let text = self.text();
                chunks.push(Some(
                    json!({ "candidates": [{ "content": { "parts": [{ "text": text }] } }] }),
                ));
                if self.rng.chance(50) {
                    chunks.push(None);
                }
            }
            _ => {}
        }
        if ending != 5 && self.rng.chance(10) {
            chunks.push(None);
        }
        chunks
    }

    /// Wraps a chunk as Vertex does, now and then.
    fn wrap(&mut self, chunk: Value) -> Value {
        if self.rng.chance(10) {
            json!({ "response": chunk })
        } else {
            chunk
        }
    }

    fn meta(&mut self, fields: &mut Vec<(&'static str, Value)>) {
        if self.rng.chance(70) {
            let id = self.one_of(&[
                json!("abc123"),
                json!("abc123"),
                json!("resp_xyz"),
                json!(""),
                json!(5),
                json!("resp_"),
            ]);
            fields.push(("responseId", id));
        }
        if self.rng.chance(60) {
            let created = self.one_of(&[
                json!("2025-06-01T12:00:00Z"),
                json!("2025-06-01T12:00:00.5Z"),
                json!("2025-06-01T12:00:00.123456789+02:00"),
                json!("1970-01-01T00:00:00Z"),
                json!("0001-01-01T00:00:00Z"),
                json!("2025-06-01 12:00:00"),
                json!(""),
                json!(5),
            ]);
            fields.push(("createTime", created));
        }
        if self.rng.chance(50) {
            let model = self.one_of(&[json!("gemini-2.5-pro-002"), json!(""), json!(5)]);
            fields.push(("modelVersion", model));
        }
    }

    fn finish_reason(&mut self) -> Value {
        self.one_of(&[
            json!("STOP"),
            json!("STOP"),
            json!("STOP"),
            json!("MAX_TOKENS"),
            json!(" max_tokens "),
            json!("SAFETY"),
            json!("RECITATION"),
            json!(""),
            json!(5),
        ])
    }

    fn usage_key(&mut self) -> &'static str {
        if self.rng.chance(85) {
            "usageMetadata"
        } else {
            "cpaUsageMetadata"
        }
    }

    fn usage(&mut self) -> Value {
        let mut fields = Vec::new();
        for key in [
            "promptTokenCount",
            "candidatesTokenCount",
            "totalTokenCount",
            "thoughtsTokenCount",
            "cachedContentTokenCount",
        ] {
            if self.rng.chance(70) {
                fields.push((key, self.count()));
            }
        }
        if self.rng.chance(10) {
            fields.push(("trafficType", json!("ON_DEMAND")));
        }
        self.object(fields)
    }

    fn count(&mut self) -> Value {
        match self.rng.below(12) {
            0..=8 => json!(self.rng.below(5000)),
            9 => json!("12"),
            10 => num("1.5"),
            _ => self.one_of(&[Value::Null, json!(true), json!(-1)]),
        }
    }

    /// The parts of a whole response, in order.
    fn parts(&mut self) -> Vec<Value> {
        let mut parts = Vec::new();
        for _ in 0..1 + self.rng.below(5) {
            match self.rng.below(20) {
                0..=4 => self.thought_parts(&mut parts),
                5..=10 => self.text_parts(&mut parts, false),
                11..=14 => self.call_parts(&mut parts),
                15 | 16 => parts.push(self.signature_part()),
                17 => parts.push(self.odd_chunk_part()),
                _ => self.text_parts(&mut parts, true),
            }
        }
        parts
    }

    /// `text` cut at random character boundaries.
    fn pieces(&mut self, text: &str) -> Vec<String> {
        let boundaries: Vec<usize> = text.char_indices().skip(1).map(|(at, _)| at).collect();
        let mut cuts = Vec::new();
        if !boundaries.is_empty() {
            for _ in 0..self.rng.below(3) {
                cuts.push(self.rng.pick(&boundaries));
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut pieces = Vec::new();
        let mut from = 0;
        for cut in cuts {
            pieces.push(text[from..cut].to_owned());
            from = cut;
        }
        pieces.push(text[from..].to_owned());
        pieces
    }

    fn thought_parts(&mut self, parts: &mut Vec<Value>) {
        let text = self.text();
        let pieces = self.pieces(&text);
        let signed = self.rng.chance(60);
        let last = pieces.len() - 1;
        for (at, piece) in pieces.into_iter().enumerate() {
            let thought = if self.rng.chance(92) {
                json!(true)
            } else {
                self.bool_like()
            };
            let mut fields = vec![("text", json!(piece)), ("thought", thought)];
            if signed && at == last {
                self.push_signature(&mut fields);
            }
            parts.push(self.object(fields));
        }
    }

    fn text_parts(&mut self, parts: &mut Vec<Value>, signed: bool) {
        let text = self.text();
        let pieces = self.pieces(&text);
        let index = self.rng.chance(15).then(|| self.part_index());
        let last = pieces.len() - 1;
        for (at, piece) in pieces.into_iter().enumerate() {
            let text = if self.rng.chance(4) {
                self.loose_part_text()
            } else {
                json!(piece)
            };
            let mut fields = vec![("text", text)];
            if let Some(index) = &index {
                fields.push(index.clone());
            }
            if signed && at == last {
                self.push_signature(&mut fields);
            }
            parts.push(self.object(fields));
        }
    }

    /// A part's text that isn't text.
    fn loose_part_text(&mut self) -> Value {
        let value = self.one_of(&[
            json!(5),
            json!(true),
            Value::Null,
            num("1.50"),
            json!({ "a": 1 }),
            json!(["a", 1]),
        ]);
        if value.is_object() || value.is_array() {
            self.loose = true;
        }
        value
    }

    fn part_index(&mut self) -> (&'static str, Value) {
        let key = if self.rng.chance(80) {
            "partIndex"
        } else {
            "index"
        };
        let value = self.one_of(&[
            json!(0),
            json!(0),
            json!(1),
            json!(2),
            json!(3),
            json!("2"),
            num("1.5"),
            json!(-1),
        ]);
        (key, value)
    }

    fn push_signature(&mut self, fields: &mut Vec<(&'static str, Value)>) {
        let key = if self.rng.chance(90) {
            "thoughtSignature"
        } else {
            "thought_signature"
        };
        fields.push((key, self.part_signature()));
    }

    /// A part's signature: usually Gemini's, now and then one seen before,
    /// another provider's, padded, empty or not text.
    fn part_signature(&mut self) -> Value {
        if !self.signatures.is_empty() && self.rng.chance(20) {
            return json!(self.base.rng.pick(&self.signatures));
        }
        let signature = match self.rng.below(12) {
            0..=6 => self.gemini_signature(),
            7 => BYPASS.to_owned(),
            8 => self.signature_text(),
            9 => format!(" {} ", self.gemini_signature()),
            10 => String::new(),
            _ => return self.one_of(&[json!(5), Value::Null]),
        };
        self.signatures.push(signature.clone());
        json!(signature)
    }

    /// A function call, and now and then another snapshot of it: the same,
    /// with other arguments, or under another name, ID or index.
    fn call_parts(&mut self, parts: &mut Vec<Value>) {
        let name = self.chunk_call_name();
        let patchy = name
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|name| name.contains("apply_patch"));
        let call = Call {
            args: self.call_args(patchy),
            id: self
                .rng
                .chance(35)
                .then(|| format!("call_{}", self.alphanumeric(8))),
            index: self.rng.chance(30).then(|| self.part_index()),
            name,
        };
        let signed = self.rng.chance(30);
        parts.push(self.call_part(&call, signed));
        if self.rng.chance(20) {
            let mut again = call;
            match self.rng.below(10) {
                0 => again.name = self.chunk_call_name(),
                1 => again.name = None,
                _ => {}
            }
            if self.rng.chance(40) {
                again.args = self.call_args(patchy);
            }
            if self.rng.chance(15) {
                again.id = Some(format!("call_{}", self.alphanumeric(8)));
            }
            if self.rng.chance(15) {
                again.index = Some(self.part_index());
            }
            parts.push(self.call_part(&again, false));
        }
    }

    fn call_part(&mut self, call: &Call, signed: bool) -> Value {
        let mut function = Vec::new();
        if let Some(name) = &call.name {
            function.push(("name", name.clone()));
        }
        if let Some(args) = &call.args {
            function.push(("args", args.clone()));
        }
        if let Some(id) = &call.id {
            function.push(("id", json!(id)));
        }
        let function = self.object(function);
        let mut fields = vec![("functionCall", function)];
        if let Some(index) = &call.index {
            fields.push(index.clone());
        }
        if signed {
            self.push_signature(&mut fields);
        }
        self.object(fields)
    }

    /// The name of a call Gemini makes: one the request declared to it,
    /// `apply_patch`, an unknown one, or none.
    fn chunk_call_name(&mut self) -> Option<Value> {
        let name = match self.rng.below(20) {
            0..=10 if !self.gemini_names.is_empty() => {
                json!(self.base.rng.pick(&self.gemini_names))
            }
            11 | 12 if self.patch => json!("apply_patch"),
            13 => json!("unknown_tool"),
            14 => json!(""),
            15 => return None,
            16 => json!(5),
            17 if !self.tool_names.is_empty() => self.base.rng.pick(&self.base.tool_names),
            _ => json!(
                self.rng
                    .pick(&["get_weather", "Bash", "mcp__github__create_issue"])
            ),
        };
        Some(name)
    }

    fn call_args(&mut self, patchy: bool) -> Option<Value> {
        let patch_chance = if patchy { 75 } else { 10 };
        if (patchy || self.patch) && self.rng.chance(patch_chance) {
            return match self.rng.below(10) {
                0..=3 => Some(json!({ "input": PATCH })),
                4 => Some(json!({ "input": UPDATE_PATCH })),
                5 => Some(json!({ "input": CUT_PATCH })),
                6 => Some(json!({ "input": "not a patch" })),
                7 => Some(json!({ "input": PATCH, "extra": 1 })),
                8 => Some(self.one_of(&[json!({}), json!({ "input": 5 }), json!(PATCH)])),
                _ => None,
            };
        }
        match self.rng.below(16) {
            0..=10 => {
                let args = self.rng.pick(STREAMED_ARGUMENTS);
                Some(serde_json::from_str(args).expect("arguments are JSON"))
            }
            11 => None,
            12 => {
                // Custom tool input upstream copies as JSON text.
                self.loose = true;
                Some(json!({ "input": { "command": "ls" } }))
            }
            _ => Some(json!({ "city": "Paris", "unit": "celsius" })),
        }
    }

    /// A part with a signature only, or with empty text.
    fn signature_part(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(50) {
            fields.push(("text", json!("")));
        }
        self.push_signature(&mut fields);
        if self.rng.chance(10) {
            fields.push(self.part_index());
        }
        self.object(fields)
    }

    fn odd_chunk_part(&mut self) -> Value {
        self.one_of(&[
            json!({ "inlineData": { "mimeType": "image/png", "data": "aGVsbG8=" } }),
            json!({ "executableCode": { "language": "PYTHON", "code": "print(1)" } }),
            json!({ "codeExecutionResult": { "outcome": "OUTCOME_OK", "output": "1" } }),
            json!({}),
            json!({ "thought": true }),
            json!({ "text": "" }),
            json!({ "functionResponse": { "name": "x", "response": {} } }),
            json!(5),
        ])
    }

    fn grounding(&mut self) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(75) {
            let count = self.rng.below(4);
            let queries = (0..count)
                .map(|_| {
                    self.one_of(&[
                        json!("weather paris"),
                        json!("weather paris"),
                        json!("news today"),
                        json!(" "),
                        json!(""),
                        json!(5),
                        json!("café"),
                    ])
                })
                .collect();
            fields.push(("webSearchQueries", Value::Array(queries)));
        } else if self.rng.chance(10) {
            fields.push(("webSearchQueries", json!("not a list")));
        }
        if self.rng.chance(80) {
            let count = 1 + self.rng.below(3);
            let chunks = (0..count).map(|_| self.grounding_chunk()).collect();
            fields.push(("groundingChunks", Value::Array(chunks)));
        }
        if self.rng.chance(70) {
            let count = 1 + self.rng.below(3);
            let supports = (0..count).map(|_| self.grounding_support()).collect();
            fields.push(("groundingSupports", Value::Array(supports)));
        }
        if self.rng.chance(15) {
            fields.push((
                "searchEntryPoint",
                json!({ "renderedContent": "<div>results</div>" }),
            ));
        }
        if self.rng.chance(10) {
            fields.push(("retrievalQueries", json!(["weather paris"])));
        }
        self.object(fields)
    }

    fn grounding_chunk(&mut self) -> Value {
        match self.rng.below(10) {
            // Upstream tells sources with no URL apart by their JSON text.
            0 => {
                self.loose = true;
                json!({ "web": { "title": "no uri" } })
            }
            1 => json!({ "web": { "uri": "", "title": "empty" } }),
            2 => {
                self.loose = true;
                json!({ "retrievedContext": { "title": "doc" } })
            }
            _ => {
                let uri = self.one_of(&[
                    json!("https://example.com/a"),
                    json!("https://example.com/a"),
                    json!("https://vertexaisearch.cloud.google.com/grounding-api-redirect/abc"),
                    json!("https://example.org/b?x=1&y=2"),
                    json!(" https://example.com/a "),
                    json!(5),
                ]);
                let mut web = vec![("uri", uri)];
                if self.rng.chance(85) {
                    let title = self.one_of(&[
                        json!("Example"),
                        json!(""),
                        json!("café"),
                        json!(7),
                        Value::Null,
                        json!("example.com"),
                    ]);
                    web.push(("title", title));
                }
                if self.rng.chance(10) {
                    web.push(("domain", json!("example.com")));
                }
                let web = self.object(web);
                json!({ "web": web })
            }
        }
    }

    fn grounding_support(&mut self) -> Value {
        let start = self.rng.pick(&[0, 0, 1, 2, 3, 5, 10]);
        let end = start + self.rng.below(12);
        let mut segment = Vec::new();
        if self.rng.chance(90) {
            segment.push(("startIndex", json!(start)));
        }
        let end = if self.rng.chance(95) {
            json!(end)
        } else {
            json!(-1)
        };
        segment.push(("endIndex", end));
        if self.rng.chance(30) {
            let part = self.one_of(&[json!(0), json!(1), json!(2)]);
            segment.push(("partIndex", part));
        }
        if self.rng.chance(30) {
            segment.push(("text", json!("cited")));
        }
        let segment = self.object(segment);
        let indices = self.one_of(&[
            json!([0]),
            json!([0]),
            json!([1, 0]),
            json!([5]),
            json!(["1"]),
            json!([-1]),
            json!([0, 0]),
            json!([2]),
        ]);
        let mut fields = vec![("segment", segment), ("groundingChunkIndices", indices)];
        if self.rng.chance(20) {
            fields.push(("confidenceScores", json!([0.9])));
        }
        self.object(fields)
    }

    /// The stream's lines: each chunk's, with lines that aren't chunks
    /// between them.
    fn lines(&mut self, chunks: &[Option<Value>]) -> Vec<String> {
        let mut lines = Vec::new();
        for chunk in chunks {
            if self.rng.chance(8) {
                lines.push(self.noise_line());
            }
            let line = match chunk {
                Some(chunk) => self.line(chunk),
                None => self.rng.pick(DONE_LINES).to_owned(),
            };
            lines.push(line);
        }
        if self.rng.chance(5) {
            lines.push(self.noise_line());
        }
        lines
    }

    /// A line that isn't a chunk: one read as nothing or as JSON, or, when
    /// the request can't declare `apply_patch`, one that isn't JSON.
    fn noise_line(&mut self) -> String {
        if self.patch || self.rng.chance(50) {
            self.rng
                .pick(&[
                    "", "data:", "data: ", "\r", "5", "null", "[]", "{}", "data: {}",
                ])
                .to_owned()
        } else {
            self.rng
                .pick(&[
                    ": keep-alive",
                    "event: message",
                    "garbage",
                    "<html>",
                    "data: oops",
                    " data: {}",
                ])
                .to_owned()
        }
    }

    /// A chunk as a stream line: usually `data: <JSON>`, sometimes spaced,
    /// escaped or spread over lines.
    fn line(&mut self, chunk: &Value) -> String {
        let json = chunk.to_string();
        let plain = self.loose;
        match self.rng.below(100) {
            0..=69 => format!("data: {json}"),
            70..=77 => format!("data:{json}"),
            78..=83 => json,
            84..=87 => format!("data:  {json} \r"),
            88..=93 if !plain => format!("data: {}", escape_text(&json)),
            94..=97 if !plain => format!(
                "data: {}",
                serde_json::to_string_pretty(chunk).expect("a Value always serializes")
            ),
            _ => format!("data: {json}"),
        }
    }

    /// The whole response for the non-streaming translator: the stream's
    /// chunks merged, one of them alone, wrapped, or no response at all.
    fn body(&mut self, chunks: &[Option<Value>]) -> String {
        let readable: Vec<&Value> = chunks.iter().flatten().collect();
        let merged = self.merged(&readable);
        let value = match self.rng.below(100) {
            0..=69 => merged,
            70..=81 => self.rng.pick(&readable).clone(),
            82..=88 => json!({ "response": merged }),
            89 | 90 => json!([merged]),
            91 | 92 => return String::new(),
            93 | 94 => return "{}".to_owned(),
            _ if !self.patch => {
                return if self.rng.chance(50) {
                    "garbage".to_owned()
                } else {
                    format!("data: {merged}")
                };
            }
            _ => merged,
        };
        let text = value.to_string();
        if self.loose {
            return text;
        }
        match self.rng.below(100) {
            0..=69 => text,
            70..=84 => escape_text(&text),
            _ => serde_json::to_string_pretty(&value).expect("a Value always serializes"),
        }
    }

    /// The chunks as one response: every part, the first finish reason, and
    /// the last grounding and usage.
    fn merged(&mut self, chunks: &[&Value]) -> Value {
        let mut parts = Vec::new();
        let mut finish = None;
        let mut grounding = None;
        let mut usage = None;
        let mut meta: Vec<(&str, Value)> = Vec::new();
        for &chunk in chunks {
            let chunk = match chunk.get("response") {
                Some(inner) if inner.is_object() => inner,
                _ => chunk,
            };
            let candidate = &chunk["candidates"][0];
            if let Some(more) = candidate["content"]["parts"].as_array() {
                parts.extend(more.iter().cloned());
            }
            if finish.is_none() {
                finish = candidate.get("finishReason").cloned();
            }
            if let Some(found) = candidate.get("groundingMetadata") {
                grounding = Some(found.clone());
            }
            for key in ["usageMetadata", "cpaUsageMetadata"] {
                if let Some(found) = chunk.get(key) {
                    usage = Some((key, found.clone()));
                }
            }
            for key in ["responseId", "createTime", "modelVersion"] {
                if let Some(found) = chunk.get(key)
                    && !meta.iter().any(|(seen, _)| *seen == key)
                {
                    meta.push((key, found.clone()));
                }
            }
        }
        let mut candidate = vec![("content", json!({ "role": "model", "parts": parts }))];
        if let Some(finish) = finish {
            candidate.push(("finishReason", finish));
        }
        if let Some(grounding) = grounding {
            candidate.push(("groundingMetadata", grounding));
        }
        let mut fields = vec![("candidates", json!([to_object(candidate)]))];
        if let Some(usage) = usage {
            fields.push(usage);
        }
        fields.extend(meta);
        self.object(fields)
    }
}

/// A valid Gemini thought signature: `tink`, its first byte set to Tink's
/// version 1, in a field 1 in a field 2, in padded standard base64. `tink`
/// must be 1 to 125 bytes long.
pub(crate) fn gemini_signature(tink: &[u8]) -> String {
    let mut inner = vec![0x0a, tink.len() as u8, 0x01];
    inner.extend(&tink[1..]);
    let mut outer = vec![0x12, inner.len() as u8];
    outer.extend(inner);
    STANDARD.encode(outer)
}

/// `signature` in one of the translator's carriers.
pub(crate) fn carrier(signature: &str, direction: &str, target: &str) -> String {
    format!(
        "{CARRIER_PREFIX}{direction}:{target}:{}",
        STANDARD_NO_PAD.encode(signature.trim())
    )
}

/// Whether upstream reads part of `schema` as text: a value under one of the
/// keys its schema cleaner reads as text that is an object, or an array
/// holding one. A property named like such a key counts too.
fn schema_reads_as_text(schema: &Value) -> bool {
    match schema {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            let read_as_text = matches!(
                key.as_str(),
                "enum" | "const" | "type" | "$ref" | "required" | "default" | "examples"
            );
            let nested = match value {
                Value::Object(_) => true,
                Value::Array(items) => items.iter().any(|item| item.is_object() || item.is_array()),
                _ => false,
            };
            (read_as_text && nested) || schema_reads_as_text(value)
        }),
        Value::Array(items) => items.iter().any(schema_reads_as_text),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translator::Translator;

    #[test]
    fn cases_are_reproducible_and_valid_json() {
        let first = request_cases(7, 200);
        let second = request_cases(7, 200);
        assert_eq!(first.len(), 200);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.model, b.model);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
        let (streams, finals) = event_cases(7, 200);
        let (again, finals_again) = event_cases(7, 200);
        assert_eq!((streams.len(), finals.len()), (200, 200));
        for (a, b) in streams.iter().zip(&again) {
            assert_eq!(a.request, b.request);
            assert_eq!(a.translated_request, b.translated_request);
            assert_eq!(a.events, b.events);
        }
        for (a, b) in finals.iter().zip(&finals_again) {
            assert_eq!(a.events, b.events);
        }
    }

    /// How gjson reads a line or body: `Some(true)` if as serde_json does
    /// (JSON, nothing or `[DONE]`), `Some(false)` if as nothing where
    /// serde_json reads nothing either, and `None` if in part.
    fn reading(text: &str, strip_data: bool) -> Option<bool> {
        let text = if strip_data {
            text.strip_prefix("data:").unwrap_or(text)
        } else {
            text
        };
        let text = text.trim();
        if text.is_empty() || text == "[DONE]" || serde_json::from_str::<Value>(text).is_ok() {
            return Some(true);
        }
        let first = text.chars().next()?;
        (!"{[+-0123456789iINntf\"".contains(first)).then_some(false)
    }

    #[test]
    fn lines_and_bodies_read_alike() {
        let (streams, finals) = event_cases(3, 2000);
        for (cases, strip_data) in [(&streams, true), (&finals, false)] {
            for case in cases {
                let patch = case.request.contains("apply_patch")
                    || case.translated_request.contains("apply_patch");
                for line in &case.events {
                    let reading = reading(line, strip_data)
                        .unwrap_or_else(|| panic!("{}: gjson reads {line:?} in part", case.name));
                    assert!(
                        reading || !patch,
                        "{}: {line:?} isn't JSON but the request may declare apply_patch",
                        case.name
                    );
                }
            }
        }
    }

    /// Not upstream's: lines gjson reads in part leave another suite's stream
    /// whatever its request declares, and the others stay unless it may
    /// declare `apply_patch`.
    #[test]
    fn lines_read_in_part_are_left_out() {
        let lines = [
            "not json",
            "data: not json",
            " {not json",
            "\"open",
            "garbage",
            ": comment",
            "5",
            "null",
            "",
            "data: [DONE]",
            "data: {}",
        ];
        for line in lines {
            assert_eq!(
                read_in_part(line.strip_prefix("data:").unwrap_or(line)),
                reading(line, true).is_none(),
                "{line:?}"
            );
        }
        let case = |request: &str| {
            let events = lines.iter().map(|line| (*line).to_owned()).collect();
            Case::response("stream", request, events)
        };
        let (stream, _) = readable_with_patch(case("{}"), case("{}"));
        assert_eq!(
            stream.events,
            [
                "garbage",
                ": comment",
                "5",
                "null",
                "",
                "data: [DONE]",
                "data: {}"
            ]
        );
        let patch = r#"{"tools":[{"type":"apply_patch"}]}"#;
        let (stream, _) = readable_with_patch(case(patch), case(patch));
        assert_eq!(stream.events, ["5", "null", "", "data: [DONE]", "data: {}"]);
    }

    #[test]
    fn request_cases_cover_the_translators_branches() {
        let cases = request_cases(1, 2000);
        let requests: Vec<String> = cases
            .iter()
            .map(|case| {
                Translator::GeminiResponsesRequest
                    .run_rust(case)
                    .expect("cases translate")
                    .to_string()
            })
            .collect();
        check(
            &requests,
            &[
                r#""systemInstruction""#,
                r#""functionCall""#,
                r#""functionResponse""#,
                r#""inline_data""#,
                r#""file_data""#,
                r#""inlineData""#,
                r#""thoughtSignature""#,
                r#""thought":true"#,
                r#""googleSearch""#,
                r#""includedDomains""#,
                r#""parametersJsonSchema""#,
                r#""toolConfig""#,
                r#""allowedFunctionNames""#,
                r#""responseJsonSchema""#,
                r#""responseMimeType""#,
                r#""thinkingLevel""#,
                r#""thinkingBudget""#,
                r#""stopSequences""#,
                r#""maxOutputTokens""#,
                "<system-reminder>",
                "call interrupted, no output",
            ],
        );
    }

    #[test]
    fn event_cases_cover_the_translators_branches() {
        let (streams, finals) = event_cases(1, 2000);
        let outputs = |translator: Translator, cases: &[Case]| -> Vec<String> {
            cases
                .iter()
                .map(|case| {
                    translator
                        .run_rust(case)
                        .expect("cases translate")
                        .to_string()
                })
                .collect()
        };
        let streams = outputs(Translator::GeminiResponsesStream, &streams);
        check(
            &streams,
            &[
                "response.output_text.delta",
                "response.reasoning_summary_text.delta",
                "response.function_call_arguments.delta",
                "response.custom_tool_call_input.done",
                r#""type":"web_search_call""#,
                r#""annotations":[{"#,
                "response.completed",
                "response.incomplete",
                "response.failed",
                "_detached_",
                r#""created_at":"(now)""#,
                r#""cached_tokens""#,
            ],
        );
        let finals = outputs(Translator::GeminiResponsesNonStream, &finals);
        check(
            &finals,
            &[
                r#""type":"message""#,
                r#""type":"reasoning""#,
                r#""type":"function_call""#,
                r#""type":"custom_tool_call""#,
                r#""type":"web_search_call""#,
                r#""status":"incomplete""#,
                "url_citation",
                "_detached_",
            ],
        );
    }

    fn check(outputs: &[String], needles: &[&str]) {
        for needle in needles {
            let count = outputs
                .iter()
                .filter(|output| output.contains(needle))
                .count();
            assert!(count >= 20, "{needle} in {count} outputs");
        }
    }
}
