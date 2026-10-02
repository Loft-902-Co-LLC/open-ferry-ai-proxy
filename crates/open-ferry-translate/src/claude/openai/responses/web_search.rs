// Ported from CLIProxyAPI internal/translator/claude/openai/responses/claude_openai-responses_web_search.go
// (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Claude's server-side web search as a Responses `web_search_call`.
//!
//! Claude reports a search as two assistant blocks: a `server_tool_use` with
//! the query and a `web_search_tool_result` with the hits. Responses has one
//! `web_search_call` item for both. The pair folds into one item on the way
//! out and expands back on the way in, so a replayed turn still shows the
//! search behind its answer; without it the model treats its own answer as
//! unsourced and searches again.
//!
//! Results and citations ride through whole: Claude rejects a replayed result
//! without its genuine `encrypted_content`, so nothing dropped here could be
//! rebuilt later.

use serde_json::{Map, Value, json};

use crate::json::lenient::{self, Found};
use crate::json::str_of;

/// The name of Claude's web search tool.
pub(super) const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Prefixes the Claude `server_tool_use` ID in a Responses item ID, as `fc_`
/// and `ctc_` do for tool calls.
const RESPONSES_WEB_SEARCH_ID_PREFIX: &str = "ws_";

/// Claude server tool IDs must match `^srvtoolu_[a-zA-Z0-9_]+$`.
const CLAUDE_SERVER_TOOL_ID_PREFIX: &str = "srvtoolu_";

/// `responsesWebSearchCallID`.
pub(super) fn responses_web_search_call_id(claude_tool_use_id: &str) -> String {
    format!("{RESPONSES_WEB_SEARCH_ID_PREFIX}{claude_tool_use_id}")
}

/// `claudeWebSearchToolUseID`: the Claude `server_tool_use` ID for a Responses
/// item ID. A history needn't come from Claude: OpenAI's own searches have IDs
/// like `ws_00112233aabb`, which Claude rejects, so the ID is always sanitized
/// and given Claude's prefix. A Claude ID comes back as it was.
pub(super) fn claude_web_search_tool_use_id(responses_item_id: &str) -> String {
    let body = responses_item_id.trim();
    let body = body
        .strip_prefix(RESPONSES_WEB_SEARCH_ID_PREFIX)
        .unwrap_or(body);
    let body = body
        .strip_prefix(CLAUDE_SERVER_TOOL_ID_PREFIX)
        .unwrap_or(body);
    // Stricter than a tool_use ID, which may have `-`.
    let body: String = body
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' => c,
            _ => '_',
        })
        .collect();
    if body.is_empty() {
        return String::new();
    }
    format!("{CLAUDE_SERVER_TOOL_ID_PREFIX}{body}")
}

/// `claudeWebSearchQuery`: the query in a `server_tool_use`'s input JSON,
/// read as gjson reads it, even from input that isn't valid JSON, such as one
/// whose last `partial_json` piece never came.
pub(super) fn claude_web_search_query(input: &str) -> String {
    lenient::get(input, "query")
        .map(Found::into_string)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// `claudeWebSearchResultsToResponses`: a `web_search_tool_result`'s content
/// as a `web_search_call`'s `results`. Errors and results with a URL are kept.
/// `None` if the content is neither an object nor an array.
pub(super) fn claude_web_search_results_to_responses(content: Option<&Value>) -> Option<Value> {
    match content? {
        object @ Value::Object(_) => Some(object.clone()),
        Value::Array(entries) => Some(Value::Array(
            entries
                .iter()
                .filter(|entry| {
                    str_of(entry.get("type")) == "web_search_tool_result_error"
                        || !str_of(entry.get("url")).trim().is_empty()
                })
                .cloned()
                .collect(),
        )),
        _ => None,
    }
}

/// `buildResponsesWebSearchCallItem`: the item for one Claude search.
/// `results` is `None` when the turn ended before the results came.
pub(super) fn build_responses_web_search_call_item(
    claude_tool_use_id: &str,
    query: &str,
    results: Option<&Value>,
) -> Map<String, Value> {
    let mut item = Map::new();
    item.insert(
        "id".into(),
        responses_web_search_call_id(claude_tool_use_id).into(),
    );
    item.insert("type".into(), "web_search_call".into());
    item.insert("status".into(), "completed".into());
    item.insert("action".into(), json!({"type": "search", "query": query}));
    if let Some(results) = results {
        item.insert("results".into(), results.clone());
    }
    item
}

/// `convertResponsesWebSearchCallToClaudeBlocks`: the Claude
/// `server_tool_use` and `web_search_tool_result` blocks for a replayed
/// `web_search_call`. `None` if the item has no usable ID.
pub(super) fn convert_responses_web_search_call_to_claude_blocks(
    item: &Value,
) -> Option<[Value; 2]> {
    let tool_use_id = claude_web_search_tool_use_id(str_of(item.get("id")).trim());
    if tool_use_id.is_empty() {
        return None;
    }
    let mut input = Map::new();
    let query = responses_web_search_call_query(item);
    if !query.is_empty() {
        input.insert("query".into(), query.into());
    }
    let tool_use = json!({
        "type": "server_tool_use",
        "id": tool_use_id,
        "name": WEB_SEARCH_TOOL_NAME,
        "input": input,
    });
    let content =
        responses_web_search_results_to_claude(item.get("results")).unwrap_or_else(|| json!([]));
    let result = json!({
        "type": "web_search_tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
    });
    Some([tool_use, result])
}

/// `responsesWebSearchCallQuery`: the query of a `web_search_call`, also
/// taking OpenAI's `queries` list and `open_page` actions.
fn responses_web_search_call_query(item: &Value) -> String {
    let action = item.get("action");
    let field = |key: &str| {
        str_of(action.and_then(|action| action.get(key)))
            .trim()
            .to_owned()
    };
    let query = field("query");
    if !query.is_empty() {
        return query;
    }
    // gjson's `queries.0` is an array's first item or an object's key "0".
    let first = action
        .and_then(|action| action.get("queries"))
        .and_then(|queries| match queries {
            Value::Array(queries) => queries.first(),
            queries => queries.get("0"),
        });
    let query = str_of(first).trim().to_owned();
    if !query.is_empty() {
        return query;
    }
    field("url")
}

/// `responsesWebSearchResultsToClaude`: a `web_search_call`'s `results` as a
/// `web_search_tool_result`'s content. Claude rejects a result whose
/// `encrypted_content` is missing or forged, so such results are dropped; an
/// empty list is the only safe fallback. `None` if nothing is left.
fn responses_web_search_results_to_claude(results: Option<&Value>) -> Option<Value> {
    let entries = match results? {
        object @ Value::Object(_) => return Some(object.clone()),
        Value::Array(entries) => entries,
        _ => return None,
    };
    let blocks: Vec<Value> = entries
        .iter()
        .filter_map(|entry| {
            if str_of(entry.get("type")) == "web_search_tool_result_error" {
                return Some(entry.clone());
            }
            if str_of(entry.get("encrypted_content")).trim().is_empty() {
                return None;
            }
            let mut block = entry.clone();
            if let Value::Object(fields) = &mut block {
                fields.insert("type".into(), "web_search_result".into());
            }
            Some(block)
        })
        .collect();
    (!blocks.is_empty()).then_some(Value::Array(blocks))
}

/// `attachClaudeCitations`: a text part's Responses `annotations` as its
/// Claude `citations`. They were copied from Claude on the way out, so they
/// go back as they are; one without the `encrypted_index` Claude needs can't
/// be replayed and is dropped.
pub(super) fn attach_claude_citations(block: &mut Map<String, Value>, annotations: Option<&Value>) {
    let Some(Value::Array(annotations)) = annotations else {
        return;
    };
    let citations: Vec<Value> = annotations
        .iter()
        .filter(|annotation| !str_of(annotation.get("encrypted_index")).trim().is_empty())
        .cloned()
        .collect();
    if !citations.is_empty() {
        block.insert("citations".into(), citations.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_use_ids_are_sanitized_and_prefixed() {
        assert_eq!(
            claude_web_search_tool_use_id("ws_srvtoolu_abc"),
            "srvtoolu_abc"
        );
        assert_eq!(
            claude_web_search_tool_use_id(" ws_00112233aabb "),
            "srvtoolu_00112233aabb"
        );
        assert_eq!(claude_web_search_tool_use_id("ws_a-b.c"), "srvtoolu_a_b_c");
        assert_eq!(claude_web_search_tool_use_id("ws_"), "");
        assert_eq!(claude_web_search_tool_use_id("srvtoolu_"), "");
    }

    #[test]
    fn claude_queries_are_read_as_gjson_reads_them() {
        for (input, want) in [
            (r#"{"query":" rust "}"#, "rust"),
            ("", ""),
            (r#"{"query":12}"#, "12"),
            (r#"{"query":null}"#, ""),
            // Two inputs run together, and one cut off.
            (r#"{"query":" padded "}{"input":"ls -la"}"#, "padded"),
            (r#"{"query":"cut\n off"#, ""),
            (r#"{"query":"a\qb", "#, "a"),
            (r#"[{"query":"q"}]"#, ""),
        ] {
            assert_eq!(claude_web_search_query(input), want, "{input}");
        }
    }

    #[test]
    fn queries_fall_back_to_the_list_and_the_url() {
        let item = json!({"action": {"queries": [" first ", "second"]}});
        assert_eq!(responses_web_search_call_query(&item), "first");
        let item = json!({"action": {"query": " ", "url": " https://example.com "}});
        assert_eq!(
            responses_web_search_call_query(&item),
            "https://example.com"
        );
    }

    #[test]
    fn results_without_encrypted_content_are_dropped() {
        let item = json!({
            "id": "ws_srvtoolu_1",
            "action": {"query": "q"},
            "results": [
                {"type": "web_search_result", "url": "https://a", "encrypted_content": "e"},
                {"url": "https://b"},
                {"type": "web_search_tool_result_error", "error_code": "unavailable"}
            ]
        });
        let [tool_use, result] = convert_responses_web_search_call_to_claude_blocks(&item).unwrap();
        assert_eq!(
            tool_use,
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "q"}})
        );
        assert_eq!(
            result["content"],
            json!([
                {"type": "web_search_result", "url": "https://a", "encrypted_content": "e"},
                {"type": "web_search_tool_result_error", "error_code": "unavailable"}
            ])
        );
    }
}
