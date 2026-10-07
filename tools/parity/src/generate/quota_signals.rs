//! Seeded random responses for the quota snapshot: Claude's and Codex's
//! quota headers, the Codex limits named by marker, `X-Ratelimit-*`,
//! `Retry-After` and headers that aren't quota's, in any case and repeated;
//! sometimes past the 64 a snapshot keeps. Values are numbers, words, empty,
//! padded with ASCII or Unicode white space, around the 512-byte limit,
//! holding a tab, non-ASCII, or bytes that aren't UTF-8. Providers are
//! Claude's and Codex's in any case and padding, and some that aren't
//! observed (never Devin, which isn't ported); prior snapshots are none,
//! a time alone, signals alone, or both.

use serde_json::{Map, Value, json};

use super::Generator;
use crate::cases::Case;

const PROVIDERS: &[&str] = &[
    "claude",
    "claude",
    "claude",
    "codex",
    "codex",
    "codex",
    "Claude",
    "CODEX",
    " codex\t",
    "gemini",
    "kimi",
    "grok",
    "openai",
    "antigravity",
    "",
    "claude-code",
    "codex2",
];

/// Header names upstream keeps for one provider or another.
const QUOTA_NAMES: &[&str] = &[
    "Retry-After",
    "Anthropic-Ratelimit-Unified-Status",
    "Anthropic-Ratelimit-Unified-Reset",
    "Anthropic-Ratelimit-Unified-Representative-Claim",
    "Anthropic-Ratelimit-Unified-Fallback-Percentage",
    "Anthropic-Ratelimit-Unified-Overage-Status",
    "Anthropic-Ratelimit-Unified-Overage-Disabled-Reason",
    "Anthropic-Ratelimit-Unified-5h-Status",
    "Anthropic-Ratelimit-Unified-5h-Utilization",
    "Anthropic-Ratelimit-Unified-5h-Reset",
    "Anthropic-Ratelimit-Unified-7d-Status",
    "Anthropic-Ratelimit-Unified-7d-Utilization",
    "Anthropic-Ratelimit-Unified-7d-Reset",
    "Anthropic-Ratelimit-Unified-7d_sonnet-Utilization",
    "X-Codex-Plan-Type",
    "X-Codex-Active-Limit",
    "X-Codex-Credits-Has-Credits",
    "X-Codex-Credits-Balance",
    "X-Codex-Credits-Unlimited",
    "X-Codex-Allowed",
    "X-Codex-Limit-Reached",
    "X-Codex-Primary-Used-Percent",
    "X-Codex-Primary-Window-Minutes",
    "X-Codex-Primary-Reset-After-Seconds",
    "X-Codex-Primary-Reset-At",
    "X-Codex-Secondary-Used-Percent",
    "X-Codex-Secondary-Window-Minutes",
    "X-Codex-Secondary-Reset-After-Seconds",
    "X-Codex-Secondary-Reset-At",
    "X-Codex-Primary-Over-Secondary-Limit-Percent",
    "X-Codex-Code-Review-Allowed",
    "X-Codex-Code-Review-Primary-Used-Percent",
    "X-Codex-Bengalfox-Limit-Name",
    "X-Codex-Bengalfox-Primary-Used-Percent",
    "X-Codex-Bengalfox-Secondary-Reset-At",
    "X-Codex-Additional-Spark-Limit-Reached",
    "X-Codex-Additional-Spark-Primary-Window-Minutes",
    "X-Ratelimit-Remaining-Requests",
    "X-Ratelimit-Limit-Tokens",
];

/// Header names no provider's snapshot keeps.
const OTHER_NAMES: &[&str] = &[
    "Content-Type",
    "Authorization",
    "X-Request-Id",
    "X-Codex-Turn-State",
    "X-Codex-Safety-Buffering-Enabled",
    "X-Codex-Credits",
    "X-Codex-Ratelimit",
    "Anthropic-Workspace-Id",
    "Anthropic-Ratelimit-Requests-Remaining",
    "Retry-After-Ms",
    "X-Retry-After",
];

const WORDS: &[&str] = &[
    "0",
    "2",
    "51",
    "100",
    "0.0",
    "0.53",
    "1.2e3",
    "-1",
    "10080",
    "309718",
    "1787296800",
    "allowed",
    "allowed_warning",
    "rejected",
    "pro",
    "plus",
    "five_hour",
    "seven_day",
    "True",
    "False",
    "codex_bengalfox",
    "GPT-5.3-Codex-Spark",
    "Wed, 21 Oct 2026 07:28:00 GMT",
    "a b  c",
    "\"quoted\"",
    "<b>&amp;</b>",
    "é",
    "日本語",
    "x\u{a0}y",
    "\u{200b}",
];

/// White space around a value: ASCII, and Unicode's as UTF-8.
const PADDING: &[&str] = &[
    " ", "  ", "\t", " \t", "\u{a0}", "\u{3000}", "\u{85}", "\u{2028}",
];

/// `count` random cases for `quota-signals/observe`, each depending only on
/// `seed` and its index.
pub fn observe_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let provider = generator.rng.pick(PROVIDERS);
            let headers = generator.quota_headers();
            let prior = generator.quota_prior();
            Case::new(format!("random-{seed}-{index}"), "", "").with_options(json!({
                "provider": provider,
                "headers": headers,
                "prior": prior,
            }))
        })
        .collect()
}

impl Generator {
    /// A response's headers, in the order they came.
    fn quota_headers(&mut self) -> Vec<Value> {
        let mut headers = Vec::new();
        if self.rng.chance(10) {
            // Past the snapshot's limit, with limits named by number.
            let prefix =
                self.rng
                    .pick(&["X-Codex-Additional-L", "X-Codex-L", "X-Codex-Code-Review-L"]);
            for i in 0..55 + self.rng.below(25) {
                let name = format!("{prefix}{i:03}-Primary-Used-Percent");
                headers
                    .push(json!({ "name": self.quota_name_case(&name), "value": i.to_string() }));
            }
        }
        let count = self.rng.below(12);
        let mut names: Vec<String> = Vec::new();
        for _ in 0..count {
            let name = if !names.is_empty() && self.rng.chance(10) {
                // A name again, maybe in another case.
                self.rng.pick(&names)
            } else if self.rng.chance(75) {
                self.rng.pick(QUOTA_NAMES).to_owned()
            } else {
                self.rng.pick(OTHER_NAMES).to_owned()
            };
            names.push(name.clone());
            let mut header = Map::new();
            header.insert("name".into(), json!(self.quota_name_case(&name)));
            match self.quota_value() {
                Ok(text) => header.insert("value".into(), json!(text)),
                Err(hex) => header.insert("hex".into(), json!(hex)),
            };
            headers.push(Value::Object(header));
        }
        if self.rng.chance(20) {
            self.rng.shuffle(&mut headers);
        }
        headers
    }

    /// `name` as written, in lower or upper case, or each letter in either.
    fn quota_name_case(&mut self, name: &str) -> String {
        match self.rng.below(8) {
            0 => name.to_ascii_lowercase(),
            1 => name.to_ascii_uppercase(),
            2 => name
                .chars()
                .map(|c| {
                    if self.rng.chance(50) {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    }
                })
                .collect(),
            _ => name.to_owned(),
        }
    }

    /// A header value: text, or the hex of bytes that aren't all UTF-8.
    fn quota_value(&mut self) -> Result<String, String> {
        let value = match self.rng.below(14) {
            0 => String::new(),
            1 => self.rng.pick(PADDING).to_owned(),
            2 => {
                let fill = self.rng.pick(&["9", "é", "x"]);
                let bytes = 505 + self.rng.below(12);
                fill.repeat(bytes / fill.len())
            }
            3 => format!("{}\t{}", self.rng.pick(WORDS), self.rng.pick(WORDS)),
            4 | 5 => return Err(self.quota_raw_bytes()),
            _ => self.rng.pick(WORDS).to_owned(),
        };
        if self.rng.chance(25) {
            let before = self.rng.pick(PADDING);
            let after = self.rng.pick(PADDING);
            return Ok(format!("{before}{value}{after}"));
        }
        Ok(value)
    }

    /// The hex of bytes a header value can hold, some of them not UTF-8:
    /// stray continuation bytes, truncated sequences, surrogates and 0xff.
    fn quota_raw_bytes(&mut self) -> String {
        let mut bytes: Vec<u8> = Vec::new();
        let parts = if self.rng.chance(10) {
            // Around the length limit.
            170 + self.rng.below(6)
        } else {
            1 + self.rng.below(6)
        };
        for _ in 0..parts {
            let part: &[u8] = self.rng.pick(&[
                b"a".as_slice(),
                b"51",
                b" ",
                b"\t",
                b"\x80",
                b"\xff",
                b"\xc2",
                b"\xc2\xa0",
                b"\xe2\x80",
                b"\xed\xa0\x80",
                b"\xf0\x9f\x98",
                b"\xf0\x9f\x98\x80",
                b"\xe3\x80\x80",
            ]);
            bytes.extend_from_slice(part);
        }
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The snapshot before the response.
    fn quota_prior(&mut self) -> Value {
        let observed_at = match self.rng.below(6) {
            0 | 1 => 0,
            2 => 10,
            3 => 999,
            4 => 1000,
            _ => 5000,
        };
        let mut signals = Map::new();
        if self.rng.chance(60) {
            for _ in 0..1 + self.rng.below(3) {
                let name = if self.rng.chance(85) {
                    self.rng.pick(QUOTA_NAMES)
                } else {
                    "old"
                };
                signals.insert(name.to_owned(), json!(self.rng.pick(WORDS)));
            }
        }
        json!({ "observed_at": observed_at, "signals": signals })
    }
}
