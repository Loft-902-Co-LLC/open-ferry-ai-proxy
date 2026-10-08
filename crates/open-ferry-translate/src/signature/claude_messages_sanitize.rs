// Ported from CLIProxyAPI internal/signature/claude_messages_sanitize.go
// (v8.0.20, MIT). https://github.com/router-for-me/CLIProxyAPI

//! Replaying Claude Messages history to another provider.

use serde_json::Value;

use super::claude::is_empty_claude_thinking_placeholder;
use super::{
    Action, BlockKind, Decision, Provider, decide_signature_compatibility_for_model,
    normalize_target_provider,
};
use crate::json::{self, str_of};

/// Where a `tool_use` block may carry a signature.
const TOOL_USE_SIGNATURE_PATHS: [&str; 4] = [
    "signature",
    "thoughtSignature",
    "thought_signature",
    "extra_content.google.thought_signature",
];

/// How [`sanitize_claude_messages_signatures_for_target`] treats signed history.
///
/// Upstream's zero-value target provider skips the fallback to the target
/// model's provider. Here the default target is [`Provider::Unknown`], which
/// falls back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClaudeMessagesSanitizeOptions<'a> {
    pub target_provider: Provider,
    pub target_model: &'a str,
    /// Remove a message left with no content.
    pub drop_empty_messages: bool,
    /// Remove every signature, and `model`, from `tool_use` blocks.
    pub drop_tool_signatures: bool,
    /// For a Claude target, also judge thinking blocks with neither a signature
    /// nor text, which are otherwise kept.
    pub drop_empty_thinking_placeholders: bool,
    /// Keep every thinking block with its signature, whatever it is.
    pub preserve_empty_thinking_blocks: bool,
}

/// What a sanitizer did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SanitizeReport {
    pub target_provider: Provider,
    pub preserved: usize,
    pub dropped_blocks: usize,
    pub dropped_signatures: usize,
    pub replaced_signatures: usize,
    /// One per signature judged, in order.
    pub decisions: Vec<Decision>,
}

/// `SanitizeClaudeMessagesSignaturesForModel`: keeps or removes signed history
/// according to the provider `target_model` belongs to.
pub fn sanitize_claude_messages_signatures_for_model(
    payload: &mut Value,
    target_model: &str,
) -> SanitizeReport {
    sanitize_claude_messages_signatures_for_target(
        payload,
        ClaudeMessagesSanitizeOptions {
            target_provider: Provider::from_model_name(target_model),
            target_model,
            drop_empty_messages: true,
            ..ClaudeMessagesSanitizeOptions::default()
        },
    )
}

/// `SanitizeClaudeMessagesForClaudeUpstream`: prepares a Messages body for a
/// Claude upstream. Claude signatures are normalized to the single-layer form
/// and CAIS signatures kept, other thinking blocks are dropped, and `tool_use`
/// blocks lose their signatures.
pub fn sanitize_claude_messages_for_claude_upstream(
    payload: &mut Value,
    target_model: &str,
    preserve_empty_thinking_blocks: bool,
) -> SanitizeReport {
    sanitize_claude_messages_signatures_for_target(
        payload,
        ClaudeMessagesSanitizeOptions {
            target_provider: Provider::Claude,
            target_model,
            drop_empty_messages: true,
            drop_tool_signatures: true,
            drop_empty_thinking_placeholders: !preserve_empty_thinking_blocks,
            preserve_empty_thinking_blocks,
        },
    )
}

/// `SanitizeClaudeMessagesSignaturesForTarget`: applies each provider's replay
/// rules to Messages history, so a conversation can move between Claude, GPT,
/// Gemini and the rest. Compatible signatures are kept and incompatible
/// thinking blocks removed.
pub fn sanitize_claude_messages_signatures_for_target(
    payload: &mut Value,
    opts: ClaudeMessagesSanitizeOptions<'_>,
) -> SanitizeReport {
    let mut target = normalize_target_provider(opts.target_provider);
    if target == Provider::Unknown && !opts.target_model.is_empty() {
        target = Provider::from_model_name(opts.target_model);
    }
    let mut report = SanitizeReport {
        target_provider: target,
        ..SanitizeReport::default()
    };
    let Some(Value::Array(messages)) = payload.get_mut("messages") else {
        return report;
    };

    let mut i = 0;
    messages.retain_mut(|message| {
        let message_index = i;
        i += 1;
        let Some(Value::Array(content)) = message.get_mut("content") else {
            return true;
        };
        let mut j = 0;
        let mut modified = false;
        content.retain_mut(|part| {
            let outcome = sanitize_block(part, message_index, j, target, &opts, &mut report);
            j += 1;
            modified |= outcome != Outcome::Unchanged;
            outcome != Outcome::Dropped
        });
        !(modified && content.is_empty() && opts.drop_empty_messages)
    });
    report
}

/// What happened to one content block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Unchanged,
    Changed,
    Dropped,
}

impl Outcome {
    fn changed_if(changed: bool) -> Self {
        if changed {
            Outcome::Changed
        } else {
            Outcome::Unchanged
        }
    }
}

fn sanitize_block(
    part: &mut Value,
    i: usize,
    j: usize,
    target: Provider,
    opts: &ClaudeMessagesSanitizeOptions<'_>,
    report: &mut SanitizeReport,
) -> Outcome {
    let block_type = str_of(part.get("type"));
    let (is_tool_use, is_thinking) = (block_type == "tool_use", block_type == "thinking");
    if is_tool_use && opts.drop_tool_signatures {
        let changed = strip_tool_use_signature_fields(part);
        if changed {
            report.dropped_signatures += 1;
        }
        return Outcome::changed_if(changed);
    }
    if is_tool_use {
        return Outcome::changed_if(sanitize_tool_use_signatures(
            part,
            target,
            opts.target_model,
            i,
            j,
            report,
        ));
    }
    if !is_thinking {
        return Outcome::Unchanged;
    }

    if opts.preserve_empty_thinking_blocks {
        report.preserved += 1;
        return Outcome::Unchanged;
    }
    if target == Provider::Claude
        && is_empty_claude_thinking_placeholder(part)
        && !opts.drop_empty_thinking_placeholders
    {
        return Outcome::Unchanged;
    }

    let raw = str_of(part.get("signature")).into_owned();
    let mut decision = decide_signature_compatibility_for_model(
        target,
        opts.target_model,
        &raw,
        BlockKind::ClaudeThinking,
    );
    decision.reason = format!("messages[{i}].content[{j}]: {}", decision.reason);
    let outcome = match decision.action {
        Action::Preserve => {
            report.preserved += 1;
            let normalized = &decision.normalized_signature;
            let changed = !normalized.is_empty() && *normalized != raw;
            if changed {
                set_signature(part, normalized.clone());
            }
            Outcome::changed_if(changed)
        }
        Action::ReplaceWithGeminiBypass => {
            report.replaced_signatures += 1;
            set_signature(part, decision.replacement_signature.clone());
            Outcome::Changed
        }
        Action::DropSignature => {
            report.dropped_signatures += 1;
            json::delete_path(part, "signature");
            Outcome::Changed
        }
        Action::DropBlock | Action::NoCompatibleReplacement => {
            report.dropped_blocks += 1;
            Outcome::Dropped
        }
    };
    report.decisions.push(decision);
    outcome
}

fn set_signature(part: &mut Value, signature: String) {
    if let Some(object) = part.as_object_mut() {
        object.insert("signature".to_owned(), Value::String(signature));
    }
}

/// Removes every signature and the `model` from a `tool_use` block.
fn strip_tool_use_signature_fields(part: &mut Value) -> bool {
    let mut changed = false;
    for path in TOOL_USE_SIGNATURE_PATHS.iter().chain(&["model"]) {
        changed |= json::delete_path(part, path);
    }
    changed | delete_empty_extra_content(part)
}

/// Judges each signature on a `tool_use` block for `target`, returning whether
/// the block changed.
fn sanitize_tool_use_signatures(
    part: &mut Value,
    target: Provider,
    target_model: &str,
    i: usize,
    j: usize,
    report: &mut SanitizeReport,
) -> bool {
    let block_kind = match target {
        Provider::Claude => BlockKind::ClaudeThinking,
        Provider::Gpt => BlockKind::GptReasoning,
        _ => BlockKind::GeminiFunctionCall,
    };
    let mut changed = false;
    for path in TOOL_USE_SIGNATURE_PATHS {
        let Some(value) = json::path(part, path) else {
            continue;
        };
        let raw = str_of(Some(value)).into_owned();
        let mut decision =
            decide_signature_compatibility_for_model(target, target_model, &raw, block_kind);
        decision.reason = format!("messages[{i}].content[{j}].{path}: {}", decision.reason);
        let replacement = match decision.action {
            Action::Preserve => {
                report.preserved += 1;
                let normalized = &decision.normalized_signature;
                (!normalized.is_empty() && *normalized != raw).then(|| normalized.clone())
            }
            Action::ReplaceWithGeminiBypass => {
                report.replaced_signatures += 1;
                Some(decision.replacement_signature.clone())
            }
            _ => {
                report.dropped_signatures += 1;
                json::delete_path(part, path);
                changed = true;
                None
            }
        };
        if let Some(signature) = replacement
            && let Some(slot) = json::path_mut(part, path)
        {
            *slot = Value::String(signature);
            changed = true;
        }
        report.decisions.push(decision);
    }
    changed | delete_empty_extra_content(part)
}

/// Removes `extra_content.google`, then `extra_content`, when left empty.
fn delete_empty_extra_content(part: &mut Value) -> bool {
    let google = delete_empty_object(part, "extra_content.google");
    delete_empty_object(part, "extra_content") | google
}

fn delete_empty_object(part: &mut Value, path: &str) -> bool {
    matches!(json::path(part, path), Some(Value::Object(object)) if object.is_empty())
        && json::delete_path(part, path)
}
