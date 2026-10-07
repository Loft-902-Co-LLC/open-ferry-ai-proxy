//! Records for open-ferry's `claude-cli` entries, which upstream doesn't
//! have.
//!
//! Each enabled entry makes one record for the `claude-cli` provider, whose
//! executor runs the user's own installed Claude Code for each request.
//! Anthropic's terms let only Claude Code itself use a Claude subscription
//! sign-in, so the record holds no credential: Claude Code signs itself in
//! with the account of its config directory. The record only says how to
//! run it.
//!
//! - The ID is `claude-cli:<hash>`, from a hash of the entry's name and
//!   config directory, and the label the entry's name.
//! - Attributes: `source` (`config:claude-cli[<hash>]`), `config_index`,
//!   `priority`, `weight`, `models_hash`, `excluded_models`,
//!   `excluded_models_hash` and `auth_kind` (`apikey`), as for an API key,
//!   and the settings the executor reads: [`ATTRIBUTE_COMMAND`],
//!   [`ATTRIBUTE_CONFIG_DIR`], [`ATTRIBUTE_SYSTEM_PROMPT`],
//!   [`ATTRIBUTE_MAX_CONCURRENCY`] and [`ATTRIBUTE_TIMEOUT_MS`].
//!
//! A disabled entry makes no record.

use std::collections::BTreeMap;

use serde_json::Map;

use super::super::classification::{
    ATTRIBUTE_CONFIG_INDEX, ATTRIBUTE_SOURCE, ATTRIBUTE_WEIGHT, AUTH_KIND_API_KEY,
};
use super::super::weight::normalize_weight;
use super::super::{Auth, Status};
use super::api_key::{ApiKeyModel, compute_models_hash};
use super::{StableIdGenerator, SynthesisContext, SynthesisError, apply_auth_excluded_models_meta};
use crate::config::ClaudeCli;

/// The provider, and the executor's ID.
pub const CLAUDE_CLI_PROVIDER: &str = "claude-cli";

/// The `claude` executable to run; absent for `claude` on `PATH`.
pub const ATTRIBUTE_COMMAND: &str = "command";

/// Claude Code's config directory (`CLAUDE_CONFIG_DIR`); absent for its
/// default.
pub const ATTRIBUTE_CONFIG_DIR: &str = "config_dir";

/// How the client's system prompt is given: `replace` or `append`.
pub const ATTRIBUTE_SYSTEM_PROMPT: &str = "system_prompt";

/// How many requests run at once.
pub const ATTRIBUTE_MAX_CONCURRENCY: &str = "max_concurrency";

/// How long a request may run, in milliseconds.
pub const ATTRIBUTE_TIMEOUT_MS: &str = "timeout_ms";

/// Records for every enabled entry in `entries`, after checking every
/// weight. An invalid weight fails the whole list, naming the entry.
pub fn synthesize_claude_cli_auths(
    entries: &[ClaudeCli],
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Result<Vec<Auth>, SynthesisError> {
    validate_claude_cli_weights(entries)?;
    Ok(entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.disabled)
        .map(|(index, entry)| claude_cli_auth(index, entry, ctx, ids))
        .collect())
}

/// Checks the weight of every entry.
pub fn validate_claude_cli_weights(entries: &[ClaudeCli]) -> Result<(), SynthesisError> {
    for (index, entry) in entries.iter().enumerate() {
        if let Some(weight) = entry.weight
            && let Err(err) = normalize_weight(weight)
        {
            return Err(SynthesisError::new(format!(
                "synthesize config API key auths: claude-cli[{index}].weight: {err}"
            )));
        }
    }
    Ok(())
}

/// The record of the entry at `index` in the config's list, whether or not
/// it is disabled. The weight isn't checked; see
/// [`validate_claude_cli_weights`].
pub fn claude_cli_auth(
    index: usize,
    entry: &ClaudeCli,
    ctx: &SynthesisContext,
    ids: &mut StableIdGenerator,
) -> Auth {
    let name = entry.name.trim();
    let config_dir = entry.config_dir.trim();
    let command = entry.command.trim();
    let (id, token) = ids.next(CLAUDE_CLI_PROVIDER, &[name, config_dir]);

    let mut attrs = BTreeMap::new();
    attrs.insert(
        ATTRIBUTE_SOURCE.to_owned(),
        format!("config:{CLAUDE_CLI_PROVIDER}[{token}]"),
    );
    attrs.insert(ATTRIBUTE_CONFIG_INDEX.to_owned(), index.to_string());
    if entry.priority != 0 {
        attrs.insert("priority".to_owned(), entry.priority.to_string());
    }
    if let Some(weight) = entry.weight {
        attrs.insert(ATTRIBUTE_WEIGHT.to_owned(), weight.max(0).to_string());
    }
    if !command.is_empty() {
        attrs.insert(ATTRIBUTE_COMMAND.to_owned(), command.to_owned());
    }
    if !config_dir.is_empty() {
        attrs.insert(ATTRIBUTE_CONFIG_DIR.to_owned(), config_dir.to_owned());
    }
    attrs.insert(
        ATTRIBUTE_SYSTEM_PROMPT.to_owned(),
        entry.system_prompt_mode().as_str().to_owned(),
    );
    attrs.insert(
        ATTRIBUTE_MAX_CONCURRENCY.to_owned(),
        entry.max_concurrency().to_string(),
    );
    attrs.insert(
        ATTRIBUTE_TIMEOUT_MS.to_owned(),
        entry.timeout().as_millis().to_string(),
    );
    let models: Vec<ApiKeyModel> = entry
        .models
        .iter()
        .map(|model| ApiKeyModel {
            name: model.name.clone(),
            alias: model.alias.clone(),
            display_name: model.display_name.clone(),
            force_mapping: model.force_mapping,
            is_compat: model.is_compat,
            thinking: model.thinking.clone(),
        })
        .collect();
    let models_hash = compute_models_hash(&models);
    if !models_hash.is_empty() {
        attrs.insert("models_hash".to_owned(), models_hash);
    }

    let mut auth = Auth {
        id,
        provider: CLAUDE_CLI_PROVIDER.to_owned(),
        label: name.to_owned(),
        prefix: entry.prefix.trim().to_owned(),
        status: Status::Active,
        attributes: attrs,
        metadata: Map::new(),
        created_at: Some(ctx.now),
        updated_at: Some(ctx.now),
        ..Auth::default()
    };
    apply_auth_excluded_models_meta(
        &mut auth,
        &ctx.oauth_excluded_models,
        &entry.excluded_models,
        AUTH_KIND_API_KEY,
    );
    auth
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::config::{ClaudeModel, Config};

    fn ctx() -> SynthesisContext {
        SynthesisContext::new("", chrono::Utc.timestamp_opt(100, 0).unwrap())
    }

    #[test]
    fn makes_one_record_per_enabled_entry() {
        let config = Config::parse(
            "claude-cli:\n  - name: max-1\n    config-dir: /srv/a\n    priority: 2\n    weight: 3\n    \
             prefix: team\n    excluded-models: [Claude-Haiku-*]\n    models: [{name: m, alias: a}]\n  \
             - name: off\n    disabled: true\n  - name: max-3\n    command: /bin/claude\n    \
             system-prompt: append\n    max-concurrency: 5\n    timeout: 30s\n",
        )
        .expect("load");
        let auths =
            synthesize_claude_cli_auths(&config.claude_cli, &ctx(), &mut StableIdGenerator::new())
                .expect("synthesize");
        let [first, third] = auths.as_slice() else {
            panic!("{auths:?}");
        };
        assert!(first.id.starts_with("claude-cli:"), "{}", first.id);
        assert_eq!(first.provider, "claude-cli");
        assert_eq!(first.label, "max-1");
        assert_eq!(first.prefix, "team");
        assert_eq!(first.attribute("config_index"), Some("0"));
        assert_eq!(first.attribute("priority"), Some("2"));
        assert_eq!(first.attribute("weight"), Some("3"));
        assert_eq!(first.attribute("config_dir"), Some("/srv/a"));
        assert_eq!(first.attribute("command"), None);
        assert_eq!(first.attribute("system_prompt"), Some("replace"));
        assert_eq!(first.attribute("max_concurrency"), Some("2"));
        assert_eq!(first.attribute("timeout_ms"), Some("600000"));
        assert_eq!(first.attribute("auth_kind"), Some("apikey"));
        assert_eq!(first.attribute("excluded_models"), Some("claude-haiku-*"));
        assert!(first.attribute("models_hash").is_some());
        assert!(first.metadata.is_empty());
        assert!(
            first
                .attribute("source")
                .is_some_and(|source| source.starts_with("config:claude-cli["))
        );

        assert_eq!(third.attribute("config_index"), Some("2"));
        assert_eq!(third.attribute("command"), Some("/bin/claude"));
        assert_eq!(third.attribute("system_prompt"), Some("append"));
        assert_eq!(third.attribute("max_concurrency"), Some("5"));
        assert_eq!(third.attribute("timeout_ms"), Some("30000"));
        assert_eq!(third.attribute("models_hash"), None);
        assert_ne!(first.id, third.id);

        // The ID follows the name and config directory, nothing else.
        let again = Config::parse(
            "claude-cli:\n  - name: max-1\n    config-dir: /srv/a\n    timeout: 1m\n",
        )
        .expect("load");
        let auths =
            synthesize_claude_cli_auths(&again.claude_cli, &ctx(), &mut StableIdGenerator::new())
                .expect("synthesize");
        assert_eq!(auths[0].id, first.id);
    }

    #[test]
    fn a_bad_weight_fails_the_list() {
        let entries = [ClaudeCli {
            name: "a".into(),
            weight: Some(2_000_000),
            models: vec![ClaudeModel::default()],
            ..ClaudeCli::default()
        }];
        let error = synthesize_claude_cli_auths(&entries, &ctx(), &mut StableIdGenerator::new())
            .expect_err("weight");
        assert!(
            error
                .to_string()
                .starts_with("synthesize config API key auths: claude-cli[0].weight: "),
            "{error}"
        );
    }
}
