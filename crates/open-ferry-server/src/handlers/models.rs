// Ported from unifiedModelsHandler and isAnthropicModelsRequest in CLIProxyAPI
// internal/api/server_routes.go, OpenAIModels in
// sdk/api/handlers/openai/openai_handlers.go, BuildResponse in
// internal/client/claude/models/models.go and convertModelToMap in
// internal/registry/model_registry.go (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! `GET /v1/models`, in the OpenAI or the Anthropic format.
//!
//! Deviations from upstream:
//! - A request with a `client_version` parameter, which Codex sends, gets an
//!   empty Codex model list for now. Upstream builds one from its catalog.
//! - The Grok shell's model list isn't served yet; it gets the OpenAI list.
//! - The OpenAI list is sorted by ID. Upstream's order varies.
//! - Model IDs in the Anthropic list are as they are. Upstream disguises
//!   IDs that don't start with `claude-` unless told not to, which is a
//!   client impersonation measure and isn't ported.

use axum::extract::State;
use axum::response::Response;
use http::{HeaderMap, Uri, header};
use open_ferry_core::models::ModelInfo;
use serde_json::{Map, Value, json};

use super::json_utf8;
use crate::query;
use crate::state::AppState;

/// Claude's default for `max_input_tokens` when a model's context length is
/// unknown (`DefaultClaudeMaxInputTokens`).
const DEFAULT_CLAUDE_MAX_INPUT_TOKENS: u64 = 200_000;

/// Claude's default for `max_tokens` when a model's output limit is unknown
/// (`DefaultClaudeMaxOutputTokens`).
const DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// `GET /v1/models`.
pub(crate) async fn unified(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let params = query::parse(uri.query().unwrap_or(""));
    if params.iter().any(|(name, _)| name == "client_version") {
        return json_utf8(json!({"models": []}).to_string());
    }
    let models = state.catalog().available_models();
    if is_anthropic_request(&headers) {
        json_utf8(claude_list(models).to_string())
    } else {
        json_utf8(openai_list(models).to_string())
    }
}

/// Whether the client wants the Anthropic format: it sends
/// `Anthropic-Version`, or is Claude Code.
fn is_anthropic_request(headers: &HeaderMap) -> bool {
    if headers
        .get("anthropic-version")
        .is_some_and(|value| !value.is_empty())
    {
        return true;
    }
    headers
        .get(header::USER_AGENT)
        .is_some_and(|value| value.as_bytes().starts_with(b"claude-cli"))
}

/// The OpenAI model list.
fn openai_list(mut models: Vec<ModelInfo>) -> Value {
    models.sort_by(|a, b| a.id.trim().cmp(b.id.trim()));
    let data: Vec<Value> = models
        .into_iter()
        .map(|model| {
            // Keys in the order Go's map marshalling sorts them.
            let mut entry = Map::new();
            if model.created > 0 {
                entry.insert("created".into(), model.created.into());
            }
            entry.insert("id".into(), model.id.into());
            entry.insert("object".into(), "model".into());
            entry.insert("owned_by".into(), model.owned_by.into());
            Value::Object(entry)
        })
        .collect();
    json!({"data": data, "object": "list"})
}

/// The Anthropic model list, sorted by display name, then ID.
fn claude_list(models: Vec<ModelInfo>) -> Value {
    let mut entries: Vec<(String, String, Value)> = models
        .into_iter()
        .map(|model| {
            let display_name = if model.display_name.is_empty() {
                model.id.clone()
            } else {
                model.display_name
            };
            let max_input = match model.context_length {
                0 => DEFAULT_CLAUDE_MAX_INPUT_TOKENS,
                n => n,
            };
            let max_output = match model.max_completion_tokens {
                0 => DEFAULT_CLAUDE_MAX_OUTPUT_TOKENS,
                n => n,
            };
            let mut entry = Map::new();
            if model.created > 0 {
                entry.insert("created_at".into(), rfc3339(model.created).into());
            }
            entry.insert("display_name".into(), display_name.clone().into());
            entry.insert("id".into(), model.id.clone().into());
            entry.insert("max_input_tokens".into(), max_input.into());
            entry.insert("max_tokens".into(), max_output.into());
            entry.insert("object".into(), "model".into());
            entry.insert("owned_by".into(), model.owned_by.into());
            entry.insert("type".into(), "model".into());
            (display_name, model.id, Value::Object(entry))
        })
        .collect();
    entries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let first_id = entries.first().map(|e| e.1.clone()).unwrap_or_default();
    let last_id = entries.last().map(|e| e.1.clone()).unwrap_or_default();
    let data: Vec<Value> = entries.into_iter().map(|e| e.2).collect();
    json!({
        "data": data,
        "first_id": first_id,
        "has_more": false,
        "last_id": last_id,
    })
}

/// Unix seconds as Go's `time.Unix(s, 0).UTC().Format(time.RFC3339)`
/// writes them.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn model(id: &str, display_name: &str, created: i64) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            owned_by: "anthropic".into(),
            created,
            display_name: display_name.into(),
            ..ModelInfo::default()
        }
    }

    #[test]
    fn formats_dates_as_go_does() {
        assert_eq!(rfc3339(1), "1970-01-01T00:00:01Z");
        assert_eq!(rfc3339(1_729_555_200), "2024-10-22T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn builds_the_openai_list() {
        let mut gpt = model("gpt-5", "", 0);
        gpt.owned_by = "openai".into();
        let list = openai_list(vec![gpt, model("claude-x", "X", 1_700_000_000)]);
        assert_eq!(
            list.to_string(),
            r#"{"data":[{"created":1700000000,"id":"claude-x","object":"model","owned_by":"anthropic"},{"id":"gpt-5","object":"model","owned_by":"openai"}],"object":"list"}"#
        );
    }

    #[test]
    fn builds_the_claude_list() {
        let mut opus = model("claude-opus", "Opus", 1_729_555_200);
        opus.context_length = 1_000_000;
        opus.max_completion_tokens = 128_000;
        let list = claude_list(vec![
            opus,
            model("gpt-5", "", 0),
            model("a-model", "Opus", 0),
        ]);
        assert_eq!(
            list.to_string(),
            concat!(
                r#"{"data":["#,
                r#"{"display_name":"Opus","id":"a-model","max_input_tokens":200000,"max_tokens":64000,"object":"model","owned_by":"anthropic","type":"model"},"#,
                r#"{"created_at":"2024-10-22T00:00:00Z","display_name":"Opus","id":"claude-opus","max_input_tokens":1000000,"max_tokens":128000,"object":"model","owned_by":"anthropic","type":"model"},"#,
                r#"{"display_name":"gpt-5","id":"gpt-5","max_input_tokens":200000,"max_tokens":64000,"object":"model","owned_by":"anthropic","type":"model"}"#,
                r#"],"first_id":"a-model","has_more":false,"last_id":"gpt-5"}"#
            )
        );
        assert_eq!(
            claude_list(Vec::new()).to_string(),
            r#"{"data":[],"first_id":"","has_more":false,"last_id":""}"#
        );
    }

    #[test]
    fn picks_the_format() {
        let mut headers = HeaderMap::new();
        assert!(!is_anthropic_request(&headers));
        headers.insert("anthropic-version", HeaderValue::from_static(""));
        assert!(!is_anthropic_request(&headers));
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1"),
        );
        assert!(is_anthropic_request(&headers));
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        assert!(is_anthropic_request(&headers));
    }
}
