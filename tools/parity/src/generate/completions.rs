//! Seeded random input for the legacy Completions conversions.
//!
//! Requests carry the fields upstream copies, with every type gjson coerces:
//! prompts that are text, batches of text or token IDs, objects, numbers or
//! null; integers, floats and flags written as numbers of every form, as
//! strings or as other types; and `stop` in every shape. A few aren't objects.
//!
//! Chat Completions responses and stream chunks hold choices with a message,
//! a delta, both or neither, finish reasons that are null, the string
//! `"null"`, empty or not strings, logprobs of every type, and usage that is
//! there, null or missing. A few aren't JSON objects, in the ways a stream
//! might carry them: empty, `[DONE]`, an SSE line or comment, or cut off before
//! gjson finds a field.
//!
//! Left out is input the port deliberately reads differently (see its module
//! docs and the hand-written cases): float parameters that aren't finite,
//! logprobs beyond `f64`, repeated keys, and malformed JSON gjson reads part of.

use std::ops::{Deref, DerefMut};

use serde_json::{Value, json};

use super::{NUMBERS, Rng, num, to_object};
use crate::cases::Case;

/// Builds `count` random Completions requests.
pub fn request_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let request = generator.request();
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), "", text)
        })
        .collect()
}

/// Builds `count` random Chat Completions responses, each the one event of
/// its case.
pub fn response_cases(seed: u64, count: usize) -> Vec<Case> {
    let mixed = seed.rotate_left(23);
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(mixed, index);
            let body = generator.body(Generator::response);
            Case::response(format!("random-{seed}-{index}"), "", vec![body])
        })
        .collect()
}

/// Builds `count` random runs of Chat Completions stream chunks, each chunk
/// an event of its case.
pub fn chunk_cases(seed: u64, count: usize) -> Vec<Case> {
    let mixed = seed.rotate_left(11);
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(mixed, index);
            let chunks = 1 + generator.rng.below(8);
            let events = (0..chunks)
                .map(|_| generator.body(Generator::chunk))
                .collect();
            Case::response(format!("random-{seed}-{index}"), "", events)
        })
        .collect()
}

/// Bodies and chunks that aren't JSON objects. gjson finds no field in any
/// of them either.
const NOT_OBJECTS: &[&str] = &[
    "",
    " ",
    "[DONE]",
    "data: [DONE]",
    ": ping",
    "event: message",
    "not json",
    "{",
    "{not json",
    "null",
    "true",
    "5",
    "\"text\"",
    "[]",
    r#"[{"id":"chatcmpl-1","choices":[]}]"#,
    r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
];

const MODELS: &[&str] = &[
    "gpt-3.5-turbo-instruct",
    "gpt-3.5-turbo-instruct",
    "davinci-002",
    "gpt-4o-mini",
    "",
    " ",
    "模型",
];

/// Integer literals around the edges of what gjson's `Int()` reads exactly.
const INTEGERS: &[&str] = &[
    "16",
    "256",
    "4096",
    "0",
    "-1",
    "1.9",
    "-1.9",
    "9007199254740991",
    "9223372036854775807",
    "9223372036854775808",
    "-9223372036854775809",
    "18446744073709551616",
    "1e19",
    "-1e19",
    "1e400",
    "-1e400",
];

/// Integers written as strings, which gjson reads only when they are all
/// digits.
const INTEGER_TEXTS: &[&str] = &[
    "16",
    "-1",
    "16.5",
    "+16",
    " 16",
    "1e3",
    "",
    "0x10",
    "9223372036854775808",
];

/// Finite float literals, written as Go writes them without an exponent.
const FLOATS: &[&str] = &[
    "0.7",
    "1",
    "2",
    "-2",
    "1e-7",
    "1e21",
    "1e-400",
    "1.7976931348623157e308",
    "4.9e-324",
    "0.30000000000000004",
    "123.456e-2",
];

/// Floats written as strings, which gjson reads with Go's `ParseFloat`: the
/// Go literal forms it takes, and text it doesn't. None reads as infinite.
const FLOAT_TEXTS: &[&str] = &[
    "0.7", "1e3", ".5", "5.", "+1.5", "-0", "0x1p-2", "0X1.8P1", "1_000.5", "0x_1p0", "1__0", "1e",
    "", " 1", "1.5 ", "abc", "1e-400", "1e308", "0x1p1023",
];

const FINISH_REASONS: &[&str] = &[
    "stop",
    "stop",
    "length",
    "tool_calls",
    "content_filter",
    "function_call",
    "null",
    "",
    "NULL",
    " stop ",
];

struct Generator {
    base: super::Generator,
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
                rng: Rng(seed.rotate_left(56) ^ index.wrapping_mul(0x94D0_49BB_1331_11EB)),
                tool_names: Vec::new(),
                tool_use_ids: Vec::new(),
            },
        }
    }

    /// A response or chunk as text: usually what `make` builds, in one of the
    /// generator's forms, and now and then text that isn't a JSON object.
    fn body(&mut self, make: fn(&mut Self) -> Value) -> String {
        if self.rng.chance(6) {
            return self.rng.pick(NOT_OBJECTS).to_owned();
        }
        let value = make(self);
        self.render(&value)
    }

    // --- Requests ---

    fn request(&mut self) -> Value {
        if self.rng.chance(4) {
            return self.one_of(&[
                json!([]),
                json!([{ "prompt": "x" }]),
                json!("prompt"),
                json!(5),
                Value::Null,
                json!(true),
            ]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(85) {
            fields.push(("prompt", self.prompt()));
        }
        if self.rng.chance(80) {
            fields.push(("model", self.model_value()));
        }
        if self.rng.chance(50) {
            fields.push(("max_tokens", self.integer()));
        }
        for key in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
        ] {
            if self.rng.chance(35) {
                fields.push((key, self.float()));
            }
        }
        if self.rng.chance(40) {
            fields.push(("stop", self.stop()));
        }
        if self.rng.chance(35) {
            fields.push(("stream", self.bool_like()));
        }
        if self.rng.chance(25) {
            // The legacy API's logprobs is a count; Chat Completions' a flag.
            let logprobs = if self.rng.chance(50) {
                self.bool_like()
            } else {
                self.integer()
            };
            fields.push(("logprobs", logprobs));
        }
        if self.rng.chance(20) {
            fields.push(("top_logprobs", self.integer()));
        }
        if self.rng.chance(25) {
            fields.push(("echo", self.bool_like()));
        }
        // Fields the conversion drops.
        if self.rng.chance(20) {
            fields.push(("n", json!(2)));
        }
        if self.rng.chance(15) {
            fields.push(("suffix", self.text().into()));
        }
        if self.rng.chance(10) {
            fields.push(("best_of", json!(3)));
        }
        if self.rng.chance(10) {
            fields.push(("logit_bias", json!({ "50256": -100 })));
        }
        if self.rng.chance(10) {
            fields.push(("user", json!("user-1")));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// A prompt: text, a batch of texts or token IDs, or another type.
    fn prompt(&mut self) -> Value {
        match self.rng.below(12) {
            0 => {
                let count = self.rng.below(4);
                Value::Array((0..count).map(|_| self.text().into()).collect())
            }
            1 => json!([1212, 318, 257, 1332]),
            2 => json!([[1212, 318], [257]]),
            3 => json!({ "text": "a prompt" }),
            4 => self.number(),
            5 => self.one_of(&[json!(true), json!(false), Value::Null, json!("")]),
            _ => self.text().into(),
        }
    }

    fn model_value(&mut self) -> Value {
        if self.rng.chance(85) {
            self.rng.pick(MODELS).into()
        } else {
            self.one_of(&[
                json!(5),
                Value::Null,
                json!(true),
                json!({ "id": "gpt-4o" }),
                json!(["gpt-4o"]),
            ])
        }
    }

    /// A value read with gjson's `Int()`: numbers of every form, numeric
    /// strings, and other types.
    fn integer(&mut self) -> Value {
        match self.rng.below(10) {
            0..=3 => self.number(),
            4 | 5 => num(self.rng.pick(INTEGERS)),
            6 => self.rng.pick(INTEGER_TEXTS).into(),
            7 => self.one_of(&[json!(true), json!(false), Value::Null]),
            _ => self.one_of(&[json!([16]), json!({ "n": 16 }), json!("sixteen")]),
        }
    }

    /// A value read with gjson's `Float()`. Never one that reads as infinite
    /// or NaN, which upstream writes as invalid JSON.
    fn float(&mut self) -> Value {
        match self.rng.below(10) {
            0..=4 => self.number(),
            5 | 6 => num(self.rng.pick(FLOATS)),
            7 => self.rng.pick(FLOAT_TEXTS).into(),
            8 => self.one_of(&[json!(true), json!(false), Value::Null]),
            _ => self.one_of(&[json!([0.5]), json!({ "value": 0.5 }), json!("warm")]),
        }
    }

    /// Stop sequences: one, a list, or another type.
    fn stop(&mut self) -> Value {
        match self.rng.below(8) {
            0 | 1 => self.text().into(),
            2 | 3 => {
                let count = self.rng.below(5);
                Value::Array((0..count).map(|_| self.text().into()).collect())
            }
            4 => self.one_of(&[Value::Null, json!([]), json!(""), json!([null, 1, "x"])]),
            5 => self.number(),
            6 => self.one_of(&[json!(true), json!({ "sequence": "\n" }), json!([["\n"]])]),
            _ => json!(["\n", "END"]),
        }
    }

    // --- Responses and chunks ---

    fn response(&mut self) -> Value {
        let mut fields = self.header("chat.completion");
        if self.rng.chance(92) {
            fields.push(("choices", self.choices(Self::response_choice)));
        }
        if self.rng.chance(50) {
            fields.push(("usage", self.usage()));
        }
        if self.rng.chance(20) {
            fields.push(("system_fingerprint", json!("fp_44709d6fcb")));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn chunk(&mut self) -> Value {
        let mut fields = self.header("chat.completion.chunk");
        if self.rng.chance(90) {
            fields.push(("choices", self.choices(Self::chunk_choice)));
        }
        if self.rng.chance(20) {
            fields.push(("usage", self.usage()));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// The fields before the choices: ID, object type, creation time and model.
    fn header(&mut self, object: &str) -> Vec<(&'static str, Value)> {
        let mut fields = Vec::new();
        if self.rng.chance(85) {
            let id = if self.rng.chance(85) {
                self.rng
                    .pick(&["chatcmpl-AbC123", "cmpl-1", "", "id with spaces"])
                    .into()
            } else {
                self.one_of(&[
                    json!(42),
                    Value::Null,
                    json!({ "id": "x" }),
                    json!(["x"]),
                    json!(true),
                ])
            };
            fields.push(("id", id));
        }
        if self.rng.chance(70) {
            let object = self.one_of(&[
                json!(object),
                json!(object),
                json!("text_completion"),
                json!(5),
            ]);
            fields.push(("object", object));
        }
        if self.rng.chance(75) {
            let created = if self.rng.chance(60) {
                json!(1_700_000_000)
            } else {
                self.integer()
            };
            fields.push(("created", created));
        }
        if self.rng.chance(80) {
            fields.push(("model", self.model_value()));
        }
        fields
    }

    /// A list of choices built by `choice`, given each one's position, with a
    /// few that aren't objects; or now and then not a list.
    fn choices(&mut self, choice: fn(&mut Self, usize) -> Value) -> Value {
        if self.rng.chance(8) {
            return self.one_of(&[
                json!({}),
                json!("choices"),
                Value::Null,
                json!(5),
                json!({ "0": { "delta": { "content": "x" } } }),
            ]);
        }
        let count = self.rng.below(4);
        Value::Array(
            (0..count)
                .map(|index| {
                    if self.rng.chance(5) {
                        self.one_of(&[
                            json!(1),
                            json!("choice"),
                            Value::Null,
                            json!([]),
                            json!([{ "index": 1, "delta": { "content": "x" } }]),
                        ])
                    } else {
                        choice(self, index)
                    }
                })
                .collect(),
        )
    }

    fn response_choice(&mut self, index: usize) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(85) {
            fields.push(("index", self.index(index)));
        }
        match self.rng.below(10) {
            0..=5 => fields.push(("message", self.message())),
            6 | 7 => fields.push(("delta", self.message())),
            8 => {
                fields.push(("message", self.message()));
                fields.push(("delta", self.message()));
            }
            _ => {}
        }
        if self.rng.chance(75) {
            fields.push(("finish_reason", self.finish_reason()));
        }
        if self.rng.chance(40) {
            fields.push(("logprobs", self.logprobs()));
        }
        if self.rng.chance(5) {
            fields.push(("text", json!("already legacy")));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn chunk_choice(&mut self, index: usize) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(85) {
            fields.push(("index", self.index(index)));
        }
        match self.rng.below(20) {
            0..=15 => fields.push(("delta", self.message())),
            16 => fields.push(("message", self.message())),
            17 => {
                fields.push(("message", self.message()));
                fields.push(("delta", self.message()));
            }
            _ => {}
        }
        if self.rng.chance(60) {
            // Mostly null, as providers send it until the last chunk.
            let reason = if self.rng.chance(60) {
                Value::Null
            } else {
                self.finish_reason()
            };
            fields.push(("finish_reason", reason));
        }
        if self.rng.chance(20) {
            fields.push(("logprobs", self.logprobs()));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// A choice's index: usually its position, sometimes any value.
    fn index(&mut self, position: usize) -> Value {
        if self.rng.chance(80) {
            json!(position)
        } else {
            self.integer()
        }
    }

    /// A message or delta: a role, content of any type or none, and fields
    /// the conversions ignore. Now and then not an object.
    fn message(&mut self) -> Value {
        if self.rng.chance(6) {
            return self.one_of(&[
                Value::Null,
                json!("hi"),
                json!(5),
                json!([]),
                json!([{ "content": "x" }]),
            ]);
        }
        let mut fields = Vec::new();
        if self.rng.chance(70) {
            let role = self.one_of(&[
                json!("assistant"),
                json!("assistant"),
                json!(""),
                Value::Null,
            ]);
            fields.push(("role", role));
        }
        if self.rng.chance(80) {
            fields.push(("content", self.content()));
        }
        if self.rng.chance(10) {
            fields.push((
                "tool_calls",
                json!([{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "f", "arguments": "{}" } }]),
            ));
        }
        if self.rng.chance(5) {
            fields.push(("refusal", json!("I can't help with that.")));
        }
        if self.rng.chance(5) {
            fields.push(("reasoning_content", self.text().into()));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    /// Content: mostly text, sometimes empty, null, parts or another type.
    fn content(&mut self) -> Value {
        match self.rng.below(12) {
            0 => json!(""),
            1 => Value::Null,
            2 => json!([{ "type": "text", "text": "part" }]),
            3 => self.loose_text(),
            _ => self.text().into(),
        }
    }

    fn finish_reason(&mut self) -> Value {
        if self.rng.chance(75) {
            self.rng.pick(FINISH_REASONS).into()
        } else {
            self.one_of(&[
                Value::Null,
                json!(0),
                json!(1),
                num("1.50"),
                json!(true),
                json!(false),
                json!({ "type": "stop" }),
                json!(["stop"]),
            ])
        }
    }

    /// Logprobs in the Chat Completions form, the legacy one, or another type.
    /// Their numbers come in forms `json.Marshal` writes differently.
    fn logprobs(&mut self) -> Value {
        match self.rng.below(8) {
            0 | 1 => Value::Null,
            2..=4 => {
                let count = self.rng.below(3);
                let content: Vec<Value> = (0..count).map(|_| self.token_logprob()).collect();
                let mut fields = vec![("content", Value::Array(content))];
                if self.rng.chance(30) {
                    fields.push(("refusal", Value::Null));
                }
                self.rng.shuffle(&mut fields);
                to_object(fields)
            }
            5 => self.number(),
            6 => self.one_of(&[json!("logprobs"), json!(true), json!([])]),
            _ => json!({
                "tokens": ["Hi", "!"],
                "token_logprobs": [self.number(), num(self.rng.pick(NUMBERS))],
                "top_logprobs": [{ "Hi": self.number(), "Hello": num("-2.50") }, null],
                "text_offset": [0, 2],
            }),
        }
    }

    fn token_logprob(&mut self) -> Value {
        let mut fields = vec![
            ("token", self.text().into()),
            ("logprob", self.number()),
            ("bytes", json!([72, 105])),
        ];
        if self.rng.chance(50) {
            let top = json!([{ "token": "Hi", "logprob": self.number(), "bytes": null }]);
            fields.push(("top_logprobs", top));
        }
        self.rng.shuffle(&mut fields);
        to_object(fields)
    }

    fn usage(&mut self) -> Value {
        match self.rng.below(10) {
            0 => Value::Null,
            1 => self.one_of(&[json!(5), json!("usage"), json!([]), json!({})]),
            _ => {
                let mut fields = vec![
                    ("prompt_tokens", self.number()),
                    ("completion_tokens", json!(7)),
                    ("total_tokens", json!(12)),
                ];
                if self.rng.chance(30) {
                    let details = json!({ "cached_tokens": 0, "audio_tokens": null });
                    fields.push(("prompt_tokens_details", details));
                }
                to_object(fields)
            }
        }
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
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.request, b.request);
            serde_json::from_str::<Value>(&a.request).expect("generated request is valid JSON");
        }
        for make in [response_cases, chunk_cases] {
            let (first, second) = (make(7, 200), make(7, 200));
            for (a, b) in first.iter().zip(&second) {
                assert_eq!(a.events, b.events);
                for event in &a.events {
                    // Only the kinds of text made on purpose aren't JSON.
                    if serde_json::from_str::<Value>(event).is_err() {
                        assert!(NOT_OBJECTS.contains(&event.as_str()), "{event}");
                    }
                }
            }
        }
    }

    /// Guards against a generator that never reaches the conversions' branches.
    #[test]
    fn cases_cover_the_conversions_branches() {
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
        let check = |outputs: &[String], needles: &[&str]| {
            for needle in needles {
                let count = outputs
                    .iter()
                    .filter(|output| output.contains(needle))
                    .count();
                assert!(count >= 20, "{needle} in {count} outputs");
            }
        };

        let requests = outputs(Translator::CompletionsRequest, &request_cases(1, 2000));
        check(
            &requests,
            &[
                r#""content":"Complete this:""#,
                r#""content":"["#,
                r#""content":"{"#,
                r#""model":"""#,
                r#""max_tokens":0"#,
                r#""temperature":0,"#,
                r#""top_p":0.7"#,
                r#""stop":["#,
                r#""stop":null"#,
                r#""stream":true"#,
                r#""logprobs":false"#,
                r#""top_logprobs":"#,
                r#""echo":true"#,
            ],
        );

        let responses = outputs(Translator::CompletionsResponse, &response_cases(1, 2000));
        check(
            &responses,
            &[
                r#"{"index":0}"#,
                r#""index":1"#,
                r#""text":"""#,
                r#""text":"["#,
                r#""finish_reason":"""#,
                r#""finish_reason":"null""#,
                r#""finish_reason":"stop""#,
                r#""logprobs":null"#,
                r#""logprobs":{"content":["#,
                r#""choices":[]"#,
                r#""usage":{"#,
                r#""usage":null"#,
            ],
        );

        let chunks = outputs(Translator::CompletionsStreamChunk, &chunk_cases(1, 2000));
        check(
            &chunks,
            &[
                "[null",
                r#""text":"""#,
                r#""finish_reason":"""#,
                r#""finish_reason":"stop""#,
                r#""logprobs":{"#,
                r#""choices":[],"usage""#,
                r#""usage":null"#,
            ],
        );
    }
}
