//! Seeded random registrations for the Codex client model list: models with
//! and without a template of their own, aliases and prefixed names, image
//! models, models several providers serve with different reasoning levels
//! and modalities, display names that sort or escape differently, and client
//! versions around the one that gets `max` and `ultra`.

use serde_json::{Map, Value, json};

use super::Generator;
use crate::cases::Case;

const PROVIDERS: &[&str] = &[
    "codex",
    "codex",
    "claude",
    "openai-compatibility",
    "openai-compatible-acme",
    "gemini",
    "vertex",
    "openai",
    "Codex",
    "xai",
];

/// The catalog's templates.
const TEMPLATE_IDS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-reserve",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "codex-auto-review",
];

/// Models in the static catalog, which lookups fall back to.
const STATIC_IDS: &[&str] = &[
    "claude-sonnet-4-6",
    "claude-opus-4-6",
    "gemini-2.5-pro",
    "gemini-2.5-flash-image",
    "kimi-k2-thinking",
    "grok-4.7",
];

const IMAGE_IDS: &[&str] = &[
    "gpt-image-2",
    "GPT-IMAGE-2",
    "team/gpt-image-2",
    "grok-imagine-video",
    "grok-imagine-image-quality",
    "gpt-image-2.5",
];

const CUSTOM_IDS: &[&str] = &[
    "custom-model",
    "alpha-model",
    "zeta-model",
    "Alpha",
    "my-model(8192)",
    "team/gpt-5.5",
    "team/nested/gpt-6-astra",
    "GPT-5.5",
    "codex-main",
    "codex-luna",
    "\u{fc}nic\u{f6}de-model",
    "model.with.dots",
    "gpt-5.5-mini",
];

const DISPLAY_NAMES: &[&str] = &[
    "Alpha",
    "alpha",
    "Zeta",
    "GPT 5.5",
    "\u{dc}nic\u{f6}de",
    "<b>&</b>",
    "Tab\there",
    "line\u{2028}break",
    "\"quoted\" \\ name",
    "\u{130}stanbul",
    "\u{391}\u{3a3}",
    "\u{65e5}\u{672c}\u{8a9e}",
    " Padded ",
];

const DESCRIPTIONS: &[&str] = &[
    "Custom model from registry",
    "A <tag> & more",
    "ctl\u{1}\u{7f}\u{2029}",
    "A long description that goes on past the length at which the summary hashes strings, so the hash and the text both come into play here.",
];

const LEVELS: &[&str] = &[
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
    "unsupported",
    " High ",
    "MEDIUM",
    "auto",
    "",
];

const MODALITIES: &[&str] = &["text", "image", "TEXT", " Image ", "audio", "video", ""];

const TYPES: &[&str] = &["openai", "claude", "gemini", "openai-image", ""];

const VERSIONS: &[&str] = &[
    "",
    "0.137.0",
    "0.143.9",
    "0.144.0",
    "0.144",
    "0.149.1",
    "0.153.4",
    "cpa",
    "1",
    "0..144",
    "v0.150.0",
    " 0.150.0 ",
    "0.144.0-alpha",
    "abc",
];

const CONTEXTS: &[u64] = &[0, 8192, 123_456, 272_000, 400_000, 1_000_000, 2_000_000];

const COMPLETION_TOKENS: &[u64] = &[0, 4096, 64_000, 128_000];

/// Case options for `codex-models/list`.
pub fn list_cases(seed: u64, count: usize) -> Vec<Case> {
    (0..count as u64)
        .map(|index| {
            let mut generator = Generator::new(seed, index);
            let options = generator.codex_models_options();
            Case::new(format!("random-{seed}-{index}"), "", "").with_options(options)
        })
        .collect()
}

impl Generator {
    fn codex_models_options(&mut self) -> Value {
        let mut registrations = Vec::new();
        let mut ids: Vec<String> = Vec::new();
        for client in 0..=self.rng.below(4) {
            let provider = self.rng.pick(PROVIDERS);
            let mut models = Vec::new();
            let mut seen: Vec<String> = Vec::new();
            for _ in 0..=self.rng.below(6) {
                let id = self.codex_model_id();
                if seen.contains(&id) {
                    continue;
                }
                models.push(self.codex_model(&id));
                if !ids.contains(&id) {
                    ids.push(id.clone());
                }
                seen.push(id);
            }
            registrations.push(json!({
                "client": format!("parity-codex-models-{client}"),
                "provider": provider,
                "models": models,
            }));
        }
        let apply_patch = if self.rng.chance(30) {
            Value::Null
        } else {
            ids.retain(|_| self.rng.chance(60));
            json!(ids)
        };
        json!({
            "registrations": registrations,
            "client_version": self.rng.pick(VERSIONS),
            "optimize_multi_agent_v2": self.rng.chance(50),
            "providers": self.rng.chance(85),
            "apply_patch": apply_patch,
        })
    }

    fn codex_model_id(&mut self) -> String {
        let pool = match self.rng.below(10) {
            0..=3 => TEMPLATE_IDS,
            4 => STATIC_IDS,
            5 => IMAGE_IDS,
            _ => CUSTOM_IDS,
        };
        self.rng.pick(pool).to_owned()
    }

    fn codex_model(&mut self, id: &str) -> Value {
        let mut model = Map::new();
        model.insert("id".into(), id.into());
        if self.rng.chance(25) {
            let target = match self.rng.below(4) {
                0..=2 => self.rng.pick(TEMPLATE_IDS),
                _ => self.rng.pick(STATIC_IDS),
            };
            model.insert("metadata_model_id".into(), target.into());
        }
        if self.rng.chance(60) {
            model.insert("object".into(), "model".into());
        }
        if self.rng.chance(40) {
            model.insert("created".into(), 1_700_000_000.into());
        }
        if self.rng.chance(60) {
            model.insert("owned_by".into(), "openai".into());
        }
        if self.rng.chance(50) {
            model.insert("type".into(), self.rng.pick(TYPES).into());
        }
        if self.rng.chance(60) {
            model.insert("display_name".into(), self.rng.pick(DISPLAY_NAMES).into());
        }
        if self.rng.chance(20) {
            model.insert("version".into(), "2026-01-01".into());
        }
        if self.rng.chance(50) {
            model.insert("description".into(), self.rng.pick(DESCRIPTIONS).into());
        }
        if self.rng.chance(60) {
            model.insert("context_length".into(), self.rng.pick(CONTEXTS).into());
        }
        if self.rng.chance(30) {
            model.insert("max_context_length".into(), self.rng.pick(CONTEXTS).into());
        }
        if self.rng.chance(40) {
            let tokens = self.rng.pick(COMPLETION_TOKENS);
            model.insert("max_completion_tokens".into(), tokens.into());
        }
        if self.rng.chance(20) {
            model.insert(
                "supported_parameters".into(),
                json!(["tools", "temperature"]),
            );
        }
        if self.rng.chance(50) {
            let thinking = if self.rng.chance(70) {
                let levels = self.codex_subset(LEVELS, 5);
                json!({ "levels": levels })
            } else {
                json!({
                    "min": 1024,
                    "max": 32_000,
                    "zero_allowed": self.rng.chance(50),
                    "dynamic_allowed": self.rng.chance(50),
                })
            };
            model.insert("thinking".into(), thinking);
        }
        if self.rng.chance(30) {
            model.insert("explicit_thinking".into(), true.into());
        }
        if self.rng.chance(40) {
            let modalities = self.codex_subset(MODALITIES, 3);
            model.insert("supported_input_modalities".into(), json!(modalities));
        }
        if self.rng.chance(30) {
            model.insert("explicit_input_modalities".into(), true.into());
        }
        Value::Object(model)
    }

    /// Up to `most` items of `pool`, in random order, maybe repeated.
    fn codex_subset(&mut self, pool: &[&str], most: usize) -> Vec<String> {
        (0..self.rng.below(most + 1))
            .map(|_| self.rng.pick(pool).to_owned())
            .collect()
    }
}
