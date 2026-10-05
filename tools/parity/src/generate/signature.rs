//! Seeded random reasoning signatures from every provider, and the Claude
//! Messages and Gemini requests that carry them.
//!
//! Each provider's signatures are built with its real layout (see the
//! signature module of open-ferry-translate), with fields left out or changed
//! now and then. Antigravity's Q-form Claude signatures change one thing at a
//! time, in the protobuf or in either base64 layer. Some signatures are then
//! damaged: a cache prefix, whitespace, a cut, a changed or inserted
//! character. The aim is to land on both sides of every check, not to look
//! like real traffic.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use super::{Generator, TEXTS, to_object};
use crate::cases::Case;

/// Every provider, by upstream's name.
const PROVIDERS: &[&str] = &[
    "unknown",
    "claude",
    "gemini",
    "gemini_bypass",
    "gpt",
    "kimi",
    "grok",
    "swe",
];

/// Target models, covering each provider's name patterns and some names that
/// match none.
const TARGET_MODELS: &[&str] = &[
    "claude-opus-5",
    "claude-sonnet-4-6",
    " Claude-Fable-5 ",
    "gemini-3.1-pro",
    "gemini-3.6-flash",
    "gpt-5.6-luna",
    "codex-mini",
    "o3",
    "o4-mini",
    "kimi-k3",
    "k2-thinking",
    "moonshot-v1-128k",
    "grok-4.5",
    "grok-code-fast-1",
    "swe-1.5",
    "deepseek-v4",
    "gpt-5.4(grok)",
    "",
];

/// Cache prefixes: every alias upstream accepts, and some it doesn't.
const PREFIXES: &[&str] = &[
    "claude#",
    "anthropic#",
    "cais#",
    "claude-cais#",
    "ccmax#",
    "claude_code_max#",
    "gemini#",
    "google#",
    "gpt#",
    "openai#",
    "codex#",
    "swe#",
    "sealed#",
    " Claude # ",
    "CODEX#",
    "codex##",
    "unknown#",
    "claude-cache#",
    "#",
    // Go lowercases İ to i, so upstream reads this prefix as "openai".
    "OPENAİ#",
];

/// Gemini's documented bypass sentinels.
const GEMINI_BYPASS: &[&str] = &[
    "skip_thought_signature_validator",
    "context_engineering_is_the_way_to_go",
];

/// Model texts in Claude signatures, valid and not.
const CLAUDE_MODEL_TEXTS: &[&str] = &[
    "claude-sonnet-4-6",
    "claude-opus-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-",
    "gpt-5",
    "",
];

/// Case inputs for `signature/inspect`: a raw signature, a target model, and a
/// target provider in the options.
pub fn inspect_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.rng.pick(TARGET_MODELS);
            let signature = generator.signature_text();
            let target = generator.rng.pick(PROVIDERS);
            Case::new(format!("random-{seed}-{index}"), model, signature)
                .with_options(json!({ "target": target }))
        })
        .collect()
}

/// Claude Messages requests for `signature/claude-messages`, with a target
/// model and random options.
pub fn claude_messages_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let model = generator.rng.pick(TARGET_MODELS);
            let request = to_object(vec![("messages", generator.messages())]);
            let text = generator.render(&request);
            let rng = &mut generator.rng;
            let options = json!({
                "validation": {
                    "PrefixOnly": rng.chance(15),
                    "Base64Only": rng.chance(15),
                    "AllowEmptySignatureWithEmptyText": rng.chance(30),
                    "Strict": rng.chance(40),
                },
                "target": {
                    "TargetProvider": rng.pick(PROVIDERS),
                    "DropEmptyMessages": rng.chance(50),
                    "DropToolSignatures": rng.chance(50),
                    "DropEmptyThinkingPlaceholders": rng.chance(50),
                    "PreserveEmptyThinkingBlocks": rng.chance(25),
                },
            });
            Case::new(format!("random-{seed}-{index}"), model, text).with_options(options)
        })
        .collect()
}

/// Gemini requests for `signature/gemini`, with a contents path and
/// validation options.
pub fn gemini_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let contents = generator.gemini_contents();
            let (request, path) = match generator.rng.below(20) {
                0..=11 => (json!({ "contents": contents }), "contents"),
                12..=16 => (
                    json!({ "model": "gemini-3.1-pro", "request": { "contents": contents } }),
                    "request.contents",
                ),
                17 => (
                    json!({ "contents": contents, "request": { "contents": [] } }),
                    "contents",
                ),
                18 => {
                    let contents = generator.one_of(&[
                        json!({}),
                        json!("contents"),
                        Value::Null,
                        json!([5, "x", null, { "parts": "x" }]),
                    ]);
                    (json!({ "contents": contents }), "contents")
                }
                _ => (json!({ "request": "x" }), "request.contents"),
            };
            let rng = &mut generator.rng;
            let path = if rng.chance(80) {
                path
            } else {
                rng.pick(&["", " contents ", "contents", "request.contents", "missing"])
            };
            let options = json!({
                "contents_path": path,
                "validation": {
                    "AllowBypassSentinel": rng.chance(50),
                    "RequireKnownEnvelope": rng.chance(30),
                    "RequireObservedMarker": rng.chance(15),
                },
            });
            let text = generator.render(&request);
            Case::new(format!("random-{seed}-{index}"), "", text).with_options(options)
        })
        .collect()
}

impl Generator {
    /// A signature from any provider, sometimes damaged.
    pub(super) fn signature_text(&mut self) -> String {
        let signature = match self.rng.below(100) {
            0..=24 => self.gpt_signature(),
            25..=37 => self.grok_signature(),
            38..=52 => self.claude_signature(),
            53..=58 => self.cais_signature(),
            59..=63 => self.antigravity_caqs_signature(),
            64..=79 => self.gemini_signature(),
            80..=81 => self.kimi_signature(),
            82..=85 => self.swe_signature(),
            _ => self.odd_signature(),
        };
        self.damage(signature)
    }

    /// Adds a cache prefix, whitespace, or a broken character.
    pub(super) fn damage(&mut self, signature: String) -> String {
        let signature = if self.rng.chance(15) {
            format!("{}{signature}", self.rng.pick(PREFIXES))
        } else {
            signature
        };
        match self.rng.below(40) {
            0 => format!(" {signature}"),
            1 => format!("{signature}\n"),
            2 => format!("\u{a0}{signature}"),
            3 => {
                let at = self.char_boundary(&signature);
                let inserted = self
                    .rng
                    .pick(&["!", "+", "/", "-", "_", "=", " ", "\n", "…"]);
                format!("{}{inserted}{}", &signature[..at], &signature[at..])
            }
            4 => {
                let at = self.char_boundary(&signature);
                signature[..at].to_owned()
            }
            5 => {
                let at = self.char_boundary(&signature);
                let replacement = self.rng.pick(&["A", "a", "0", "+", "-"]);
                let rest = &signature[at..];
                let skip = rest.chars().next().map_or(0, char::len_utf8);
                format!("{}{replacement}{}", &signature[..at], &rest[skip..])
            }
            6 => with_trailing_bits(&signature),
            _ => signature,
        }
    }

    /// A random character boundary in `text`.
    fn char_boundary(&mut self, text: &str) -> usize {
        let boundaries: Vec<usize> = text
            .char_indices()
            .map(|(at, _)| at)
            .chain([text.len()])
            .collect();
        self.rng.pick(&boundaries)
    }

    /// GPT reasoning `encrypted_content`: a Fernet token in URL-safe base64.
    pub(super) fn gpt_signature(&mut self) -> String {
        let blocks = 1 + self.rng.below(3);
        let mut raw = self.gpt_signature_bytes(blocks);
        let encode = |raw: &[u8], padded| {
            if padded {
                URL_SAFE.encode(raw)
            } else {
                URL_SAFE_NO_PAD.encode(raw)
            }
        };
        let padded = self.rng.chance(50);
        match self.rng.below(12) {
            0..=7 => encode(&raw, padded),
            8 => {
                raw.push(0);
                encode(&raw, padded)
            }
            9 => {
                raw[0] = 0x81;
                encode(&raw, padded)
            }
            10 => encode(&raw[..72], padded),
            _ => STANDARD.encode(&raw),
        }
    }

    /// Version byte, timestamp, IV, `blocks` AES blocks of ciphertext and HMAC.
    fn gpt_signature_bytes(&mut self, blocks: usize) -> Vec<u8> {
        let len = 1 + 8 + 16 + 16 * blocks + 32;
        let mut raw = self.random_bytes(len);
        raw[0] = 0x80;
        // Real timestamps start with zero bytes, which gives the "gAAAA" prefix.
        raw[1..4].fill(0);
        raw
    }

    /// xAI `encrypted_content`: random bytes in standard base64, around the
    /// length floor and the entropy threshold.
    fn grok_signature(&mut self) -> String {
        let len = match self.rng.below(10) {
            0 => 30 + self.rng.below(4),
            1..=6 => 40 + self.rng.below(200),
            _ => 240 + self.rng.below(600),
        };
        let mut raw = self.random_bytes(len);
        if self.rng.chance(20) {
            // Fewer distinct byte values lower the entropy.
            let width = 2 + self.rng.below(60) as u8;
            raw.iter_mut().for_each(|b| *b %= width);
        }
        match self.rng.below(10) {
            0..=6 => STANDARD_NO_PAD.encode(&raw),
            7 | 8 => STANDARD.encode(&raw),
            _ => URL_SAFE_NO_PAD.encode(&raw),
        }
    }

    /// A classic Claude signature: an `E` protobuf in base64, or an `R` one
    /// with a second base64 layer.
    fn claude_signature(&mut self) -> String {
        let mut channel = Pb::default();
        if self.rng.chance(95) {
            channel = if self.rng.chance(5) {
                channel.bytes(1, b"\x0b")
            } else {
                let id = self.rng.pick(&[11, 11, 12, 12, 13, 0, u64::MAX]);
                channel.varint(1, id)
            };
        }
        if self.rng.chance(60) {
            channel = channel.varint(2, self.rng.pick(&[1, 2, 2, 3]));
        }
        if self.rng.chance(40) {
            // Signature bytes. 64 bytes after the channel ID and field 2 is
            // the 70-byte compact schema.
            let len = self.rng.pick(&[62, 64, 64, 66, 20]);
            channel = channel.bytes(5, &self.random_bytes(len));
        }
        if self.rng.chance(50) {
            channel = match self.rng.below(10) {
                0 => channel.varint(6, 1),
                1 => channel.bytes(6, &[0xff, 0xfe]),
                _ => channel.string(6, self.rng.pick(CLAUDE_MODEL_TEXTS)),
            };
        }
        if self.rng.chance(30) {
            channel = if self.rng.chance(10) {
                channel.bytes(7, b"x")
            } else {
                channel.varint(7, 1)
            };
        }

        let container = if self.rng.chance(95) {
            Pb::default().bytes(1, &channel.0)
        } else {
            Pb::default().varint(1, 5)
        };
        let mut payload = if self.rng.chance(97) {
            Pb::default().bytes(2, &container.0)
        } else {
            Pb::default().varint(2, 1)
        };
        if self.rng.chance(80) {
            payload = payload.varint(3, 1);
        }
        if self.rng.chance(50) {
            let len = self.rng.below(80);
            payload = payload.bytes(4, &self.random_bytes(len));
        }
        let mut payload = payload.0;
        if self.rng.chance(5) {
            let at = self.rng.below(payload.len());
            payload.truncate(at.max(1));
        }

        let single = STANDARD.encode(&payload);
        if self.rng.chance(35) {
            STANDARD.encode(single)
        } else {
            single
        }
    }

    /// A Claude CAIS signature, or with envelope version 4 and up a CAQS one,
    /// which keeps its signature bytes in the container.
    fn cais_signature(&mut self) -> String {
        let version = self.rng.pick(&[2, 2, 2, 4, 4, 5, 1, 3]);
        let caqs = version >= 4;
        let mut channel = Pb::default();
        if self.rng.chance(95) {
            channel = if self.rng.chance(4) {
                channel.bytes(1, b"\x10")
            } else {
                channel.varint(1, self.rng.pick(&[16, 16, 17, 0]))
            };
        }
        if self.rng.chance(15) {
            // The infrastructure class, which the Q form requires.
            channel = if self.rng.chance(10) {
                channel.bytes(2, b"\x02")
            } else {
                channel.varint(2, self.rng.pick(&[2, 2, 1]))
            };
        }
        if self.rng.chance(85) {
            channel = channel.varint(3, 2);
        }
        if (!caqs && self.rng.chance(92)) || (caqs && self.rng.chance(15)) {
            let len = self.rng.pick(&[64, 64, 32, 0]);
            channel = channel.bytes(5, &self.random_bytes(len));
        }
        if (!caqs && self.rng.chance(92)) || (caqs && self.rng.chance(40)) {
            channel = if self.rng.chance(4) {
                channel.bytes(6, &[0xc3])
            } else {
                channel.string(6, self.rng.pick(CLAUDE_MODEL_TEXTS))
            };
        }
        if self.rng.chance(70) {
            channel = channel.varint(7, 1);
        }
        if self.rng.chance(85) {
            let kinds = ["thinking", "thinking", "narration", "redacted_thinking", ""];
            channel = channel.string(8, self.rng.pick(&kinds));
        }
        if self.rng.chance(80) {
            let context_id = match self.rng.below(10) {
                0 => "not-a-uuid".to_owned(),
                1 => self.uuid().to_uppercase(),
                _ => self.uuid(),
            };
            channel = channel.string(11, &context_id);
        }

        let mut container = Pb::default();
        if self.rng.chance(97) {
            container = container.bytes(1, &channel.0);
        }
        if caqs && self.rng.chance(85) {
            let len = self.rng.pick(&[64, 96, 0]);
            container = container.bytes(5, &self.random_bytes(len));
        }
        let mut payload = Pb::default();
        if self.rng.chance(95) {
            payload = payload.varint(1, version);
        }
        if self.rng.chance(97) {
            payload = payload.bytes(2, &container.0);
        }
        if self.rng.chance(90) {
            payload = payload.varint(3, 1);
        }
        if self.rng.chance(50) {
            STANDARD_NO_PAD.encode(&payload.0)
        } else {
            STANDARD.encode(&payload.0)
        }
    }

    /// Antigravity's Q form of a Claude 5.5 signature: a CAQS protobuf for
    /// Google's thinking channel inside two layers of base64. Often the
    /// observed layout; otherwise one thing about it is changed, in the
    /// protobuf or in either layer.
    fn antigravity_caqs_signature(&mut self) -> String {
        let variant = if self.rng.chance(45) {
            0
        } else {
            1 + self.rng.below(23)
        };
        if variant == 23 {
            // A native CAIS or CAQS signature in a second layer.
            let native = self.cais_signature();
            return STANDARD.encode(native);
        }

        let channel_id = if variant == 1 {
            self.rng.pick(&[16, 17, 0, 19])
        } else {
            18
        };
        let mut channel = Pb::default().varint(1, channel_id);
        channel = match variant {
            2 => channel.varint(2, self.rng.pick(&[0, 1, 3])),
            3 => channel,
            4 => channel.bytes(2, b"\x02"),
            _ => channel.varint(2, 2),
        };
        channel = channel.varint(3, 2);
        let signature_len = self.rng.pick(&[1020, 1020, 64, 511, 1021]);
        if variant == 7 {
            // The signature where CAIS keeps it, not in the container.
            channel = channel.bytes(5, &self.random_bytes(signature_len));
        }
        if variant == 12 {
            channel = channel.string(6, self.rng.pick(CLAUDE_MODEL_TEXTS));
        }
        channel = channel.varint(7, 1);
        channel = match variant {
            6 if self.rng.chance(25) => channel,
            6 => channel.string(
                8,
                self.rng
                    .pick(&["narration", "redacted_thinking", "Thinking", ""]),
            ),
            _ => channel.string(8, "thinking"),
        };
        if variant == 12 {
            let context_id = self.uuid();
            channel = channel.string(11, &context_id);
        }
        let channel = channel.0;

        let mut container = Pb::default();
        if variant == 10 {
            let first = first_duplicate(self.rng.below(3), &channel);
            container = container.bytes(1, &first);
        }
        container = container.bytes(1, &channel);
        for (field, len) in [(2, 12), (3, 12), (4, 48)] {
            container = container.bytes(field, &self.random_bytes(len));
        }
        container = match variant {
            7 | 9 => container,
            8 => container.bytes(5, &[]),
            _ => container.bytes(5, &self.random_bytes(signature_len)),
        };
        let container = container.0;

        let version = if variant == 5 {
            self.rng.pick(&[5, 3, 2])
        } else {
            4
        };
        let mut payload = Pb::default().varint(1, version);
        if variant == 11 {
            let first = first_duplicate(self.rng.below(3), &container);
            payload = payload.bytes(2, &first);
        }
        let mut payload = payload.bytes(2, &container).varint(3, 1).0;
        if variant == 21 {
            let at = 1 + self.rng.below(payload.len() - 1);
            payload.truncate(at);
        }

        let mut inner = match variant {
            15 => STANDARD_NO_PAD.encode(&payload),
            20 => URL_SAFE.encode(&payload),
            _ => STANDARD.encode(&payload),
        };
        if variant == 13 {
            inner = with_trailing_bits(&inner);
        }
        if variant == 14 {
            let inserted = self
                .rng
                .pick(&[" ", "#", "\t", "\r\n", "\n", "=", "claude#"]);
            let at = if inserted == "claude#" {
                0
            } else {
                self.char_boundary(&inner)
            };
            inner.insert_str(at, inserted);
        }
        let outer = match variant {
            16 => STANDARD_NO_PAD.encode(&inner),
            19 => STANDARD.encode(STANDARD.encode(&inner)),
            22 => URL_SAFE.encode(&inner),
            _ => STANDARD.encode(&inner),
        };
        match variant {
            17 => with_trailing_bits(&outer),
            18 => {
                let at = self.char_boundary(&outer);
                format!("{}\r\n{}", &outer[..at], &outer[at..])
            }
            _ => outer,
        }
    }

    /// A Gemini thought signature: the field-2 envelope around a Tink payload,
    /// a UUID or a server-side tool block, or a shape upstream rejects.
    fn gemini_signature(&mut self) -> String {
        let field2 = |value: &[u8]| Pb::default().bytes(2, &Pb::default().bytes(1, value).0).0;
        let decoded = match self.rng.below(20) {
            0 | 1 => return self.rng.pick(GEMINI_BYPASS).to_owned(),
            2 => self.uuid().into_bytes(),
            3 => field2(self.uuid().as_bytes()),
            4 | 5 => {
                let tink = self.tink_payload();
                let mut block = Pb::default()
                    .varint(1, self.rng.next() % 300)
                    .bytes(2, &tink);
                if self.rng.chance(50) {
                    block = block.fixed32(3, self.rng.next() as u32);
                }
                if self.rng.chance(50) {
                    block = block.fixed64(4, self.rng.next());
                }
                if self.rng.chance(10) {
                    // A start-group wire type, which isn't a tool block.
                    block = block.tag(5, 3);
                }
                field2(&block.0)
            }
            // The retired Gemini 2.5 envelope: repeated field 1.
            6 => {
                (0..1 + self.rng.below(3))
                    .fold(Pb::default(), |pb, _| {
                        let tink = self.tink_payload();
                        pb.bytes(1, &tink)
                    })
                    .0
            }
            7 => {
                let tink = self.tink_payload();
                let mut envelope = field2(&tink);
                envelope.extend(Pb::default().varint(3, 1).0);
                envelope
            }
            8 => {
                let len = 20 + self.rng.below(200);
                self.random_bytes(len)
            }
            9 => {
                let mut value = self.tink_payload();
                value[0] = 0x02;
                field2(&value)
            }
            _ => field2(&self.tink_payload()),
        };
        if self.rng.chance(30) {
            STANDARD_NO_PAD.encode(&decoded)
        } else {
            STANDARD.encode(&decoded)
        }
    }

    /// A Tink ciphertext: the format byte `0x01`, a key ID and random bytes.
    fn tink_payload(&mut self) -> Vec<u8> {
        let len = 5 + self.rng.below(300);
        let mut payload = self.random_bytes(len);
        payload[0] = 0x01;
        payload
    }

    /// A Kimi thinking signature: random bytes of one of two fixed lengths,
    /// in unpadded standard base64.
    fn kimi_signature(&mut self) -> String {
        let len = self.rng.pick(&[3255, 3255, 9709, 3254, 9710]);
        let raw = if self.rng.chance(15) {
            vec![0x41; len]
        } else {
            self.random_bytes(len)
        };
        STANDARD_NO_PAD.encode(raw)
    }

    /// A Cognition SWE `sealed.v1.` envelope, or a near miss.
    fn swe_signature(&mut self) -> String {
        let prefix = self.rng.pick(&[
            "sealed.v1.",
            "sealed.v1.",
            "sealed.v2.",
            "sealed.v1",
            "SEALED.V1.",
        ]);
        let len = self.rng.below(120);
        format!("{prefix}{}", self.alphanumeric(len))
    }

    /// Short strings and text that are no provider's signature.
    fn odd_signature(&mut self) -> String {
        match self.rng.below(4) {
            0 => self
                .rng
                .pick(&[
                    "", " ", "x", "E", "R", "C", "g", "EAAA", "gAAAA", "EiIA", "null", "…",
                    "claude#", "##",
                ])
                .to_owned(),
            1 => self.rng.pick(TEXTS).to_owned(),
            2 => {
                let len = 1 + self.rng.below(80);
                self.alphanumeric(len)
            }
            _ => format!(" {} ", self.rng.pick(GEMINI_BYPASS)),
        }
    }

    fn random_bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.rng.next() as u8).collect()
    }

    /// A random UUID in canonical lowercase form.
    fn uuid(&mut self) -> String {
        let hex = format!("{:016x}{:016x}", self.rng.next(), self.rng.next());
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    }

    // --- Gemini requests ---

    /// Gemini `contents`. A model turn's function calls are usually answered
    /// by the next content, sometimes with responses missing, out of order or
    /// misnamed.
    pub(super) fn gemini_contents(&mut self) -> Value {
        let count = self.rng.below(7);
        let mut calls = Vec::new();
        let mut contents = Vec::with_capacity(count);
        for _ in 0..count {
            let content = if !calls.is_empty() && self.rng.chance(80) {
                let calls = std::mem::take(&mut calls);
                self.gemini_response_content(calls)
            } else if self.rng.chance(60) {
                self.gemini_model_content(&mut calls)
            } else {
                let parts = (0..1 + self.rng.below(2))
                    .map(|_| self.gemini_text_part(false))
                    .collect();
                self.gemini_content(json!("user"), parts)
            };
            contents.push(content);
        }
        Value::Array(contents)
    }

    fn gemini_content(&mut self, role: Value, parts: Vec<Value>) -> Value {
        let mut fields = Vec::new();
        if self.rng.chance(95) {
            fields.push(("role", role));
        }
        if self.rng.chance(97) {
            fields.push(("parts", Value::Array(parts)));
        } else {
            fields.push(("parts", self.one_of(&[json!("x"), Value::Null, json!({})])));
        }
        self.object(fields)
    }

    /// A model turn. Each function call made is added to `calls` as its name
    /// and ID.
    fn gemini_model_content(&mut self, calls: &mut Vec<(Value, Option<Value>)>) -> Value {
        let role = self.one_of(&[
            json!("model"),
            json!("model"),
            json!("model"),
            json!("MODEL"),
            json!(""),
            json!(5),
        ]);
        let count = 1 + self.rng.below(4);
        let parts = (0..count)
            .map(|_| match self.rng.below(16) {
                0..=3 => self.gemini_text_part(true),
                4 | 5 => self.gemini_text_part(false),
                6..=11 => {
                    let (part, call) = self.gemini_call_part();
                    calls.push(call);
                    part
                }
                12 | 13 => self.gemini_tool_part(),
                14 => self.one_of(&[json!("x"), json!(5), Value::Null, json!({})]),
                _ => json!({ "inlineData": { "mimeType": "image/png", "data": "iVBORw0K" } }),
            })
            .collect();
        self.gemini_content(role, parts)
    }

    fn gemini_text_part(&mut self, thought: bool) -> Value {
        let mut fields = vec![("text", self.text().into())];
        if thought {
            fields.push((
                "thought",
                self.one_of(&[json!(true), json!(true), json!("true")]),
            ));
        }
        self.with_part_signature(&mut fields);
        self.object(fields)
    }

    /// A function call part, with the call's name and ID.
    fn gemini_call_part(&mut self) -> (Value, (Value, Option<Value>)) {
        let name = self.one_of(&[
            json!("get_weather"),
            json!("get_weather"),
            json!("search"),
            json!(""),
            json!(5),
        ]);
        let id = self
            .rng
            .chance(40)
            .then(|| self.one_of(&[json!("call_1"), json!("call_2"), json!(""), json!(7)]));
        let mut call = vec![("name", name.clone()), ("args", json!({ "city": "Paris" }))];
        if let Some(id) = &id {
            call.push(("id", id.clone()));
        }
        if self.rng.chance(5) {
            let signature = self.gemini_part_signature();
            call.push(("thoughtSignature", signature));
        }
        let key = if self.rng.chance(10) {
            "function_call"
        } else {
            "functionCall"
        };
        let mut fields = vec![(key, self.object(call))];
        self.with_part_signature(&mut fields);
        (self.object(fields), (name, id))
    }

    /// A server-side tool block, which keeps its signature.
    fn gemini_tool_part(&mut self) -> Value {
        let key = self
            .rng
            .pick(&["toolCall", "toolResponse", "tool_call", "tool_response"]);
        let mut fields = vec![(key, json!({ "toolType": "GOOGLE_SEARCH_WEB", "id": "t1" }))];
        self.with_part_signature(&mut fields);
        self.object(fields)
    }

    /// The turn answering `calls`.
    fn gemini_response_content(&mut self, mut calls: Vec<(Value, Option<Value>)>) -> Value {
        match self.rng.below(10) {
            0..=5 => {}
            6 => self.rng.shuffle(&mut calls),
            7 => {
                calls.pop();
            }
            8 => calls.push(calls[0].clone()),
            _ => calls[0].0 = json!("other_tool"),
        }
        let mut parts: Vec<Value> = calls
            .into_iter()
            .map(|(name, id)| {
                let mut response = vec![("name", name), ("response", json!({ "result": "ok" }))];
                if let Some(id) = id {
                    response.push(("id", id));
                }
                if self.rng.chance(5) {
                    response.push(("thoughtSignature", self.gemini_part_signature()));
                }
                let key = if self.rng.chance(10) {
                    "function_response"
                } else {
                    "functionResponse"
                };
                let mut fields = vec![(key, self.object(response))];
                if self.rng.chance(10) {
                    fields.push(("thoughtSignature", self.gemini_part_signature()));
                }
                self.object(fields)
            })
            .collect();
        if self.rng.chance(10) {
            let at = self.rng.below(parts.len() + 1);
            let text = self.gemini_text_part(false);
            parts.insert(at, text);
        }
        let role = self.one_of(&[json!("user"), json!("user"), json!("function"), json!("")]);
        self.gemini_content(role, parts)
    }

    /// Gives a part a signature, under either key, most of the time.
    fn with_part_signature(&mut self, fields: &mut Vec<(&str, Value)>) {
        match self.rng.below(20) {
            0..=10 => fields.push(("thoughtSignature", self.gemini_part_signature())),
            11 | 12 => fields.push(("thought_signature", self.gemini_part_signature())),
            13 => {
                fields.push(("thoughtSignature", self.gemini_part_signature()));
                fields.push(("thought_signature", self.gemini_part_signature()));
            }
            _ => {}
        }
    }

    /// Usually Gemini's own signature, otherwise another provider's or not a
    /// string.
    fn gemini_part_signature(&mut self) -> Value {
        match self.rng.below(20) {
            0..=11 => {
                let signature = self.gemini_signature();
                self.damage(signature).into()
            }
            12..=18 => self.signature_text().into(),
            _ => self.one_of(&[json!(5), Value::Null, json!(true), json!({})]),
        }
    }
}

/// Protobuf wire format, written field by field.
#[derive(Default)]
struct Pb(Vec<u8>);

impl Pb {
    fn uvarint(mut self, mut value: u64) -> Self {
        while value >= 0x80 {
            self.0.push(value as u8 | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
        self
    }

    fn tag(self, field: u64, wire_type: u64) -> Self {
        self.uvarint(field << 3 | wire_type)
    }

    fn raw(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    fn varint(self, field: u64, value: u64) -> Self {
        self.tag(field, 0).uvarint(value)
    }

    fn fixed64(self, field: u64, value: u64) -> Self {
        self.tag(field, 1).raw(&value.to_le_bytes())
    }

    fn bytes(self, field: u64, value: &[u8]) -> Self {
        self.tag(field, 2).uvarint(value.len() as u64).raw(value)
    }

    fn string(self, field: u64, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    fn fixed32(self, field: u64, value: u32) -> Self {
        self.tag(field, 5).raw(&value.to_le_bytes())
    }
}

/// The first of two channel blocks or containers: broken, empty or a copy of
/// the second.
fn first_duplicate(choice: usize, copy: &[u8]) -> Vec<u8> {
    match choice {
        0 => vec![0x80],
        1 => Vec::new(),
        _ => copy.to_vec(),
    }
}

/// Sets padding bits in the last base64 character, which strict decoders
/// reject. Text that doesn't end in a base64 character is returned unchanged.
fn with_trailing_bits(sig: &str) -> String {
    const STANDARD_ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const URL_SAFE_ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let trimmed = sig.trim_end_matches('=');
    let Some(&last) = trimmed.as_bytes().last() else {
        return sig.to_owned();
    };
    let alphabet = if matches!(last, b'-' | b'_') {
        URL_SAFE_ALPHABET
    } else {
        STANDARD_ALPHABET
    };
    let Some(value) = alphabet.iter().position(|&c| c == last) else {
        return sig.to_owned();
    };
    let mut bytes = sig.as_bytes().to_vec();
    bytes[trimmed.len() - 1] = alphabet[value | 1];
    String::from_utf8(bytes).expect("only an ASCII byte changed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_are_reproducible() {
        for build in [inspect_cases, claude_messages_cases, gemini_cases] {
            let first = build(3, 100);
            let second = build(3, 100);
            for (a, b) in first.iter().zip(&second) {
                assert_eq!(a.request, b.request);
                assert_eq!(a.options, b.options);
            }
        }
    }

    #[test]
    fn requests_are_valid_json() {
        for build in [claude_messages_cases, gemini_cases] {
            for case in build(5, 200) {
                serde_json::from_str::<Value>(&case.request).expect("valid JSON");
            }
        }
    }

    #[test]
    fn antigravity_caqs_signatures_land_on_both_sides() {
        use open_ferry_translate::signature::inspect_antigravity_claude_caqs_signature;
        let (mut valid, mut invalid) = (0, 0);
        for index in 0..200 {
            let signature = Generator::new(7, index).antigravity_caqs_signature();
            match inspect_antigravity_claude_caqs_signature(&signature) {
                Ok(_) => valid += 1,
                Err(_) => invalid += 1,
            }
        }
        assert!(
            valid > 50 && invalid > 50,
            "{valid} valid, {invalid} invalid"
        );
    }

    #[test]
    fn trailing_bits_change_only_the_last_character() {
        assert_eq!(with_trailing_bits("QUJD"), "QUJD");
        assert_eq!(with_trailing_bits("QUI="), "QUJ=");
        assert_eq!(with_trailing_bits("gA-"), "gA_");
        assert_eq!(with_trailing_bits("not base64!"), "not base64!");
        assert_eq!(with_trailing_bits(""), "");
    }
}
