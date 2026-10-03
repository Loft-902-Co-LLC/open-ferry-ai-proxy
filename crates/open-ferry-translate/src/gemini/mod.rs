//! Translators to a Gemini upstream (upstream's `internal/translator/gemini`).

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "the translators that call it land next")
)]
pub(crate) mod common;
