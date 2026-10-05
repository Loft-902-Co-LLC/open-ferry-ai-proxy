// Ported from CLIProxyAPI internal/runtime/executor/helps/meta_tools.go
// (SanitizeMetaWebSearchTools) and meta_tools_test.go (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The tool definitions Meta rejects.

use serde_json::Value;

use crate::json::{delete, str_of};

/// Strips `search_content_types` from the `web_search` tools of `body`, those
/// in a namespace too (`SanitizeMetaWebSearchTools`). Meta rejects it on a
/// `web_search` tool and accepts it only on `web_search_preview`, which is
/// left alone. A body without a `tools` array is left as it is.
pub(super) fn sanitize_web_search_tools(body: &mut Value) {
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    for tool in tools {
        strip_search_content_types(tool);
        if str_of(tool.get("type")) == "namespace"
            && let Some(Value::Array(nested)) = tool.get_mut("tools")
        {
            for nested_tool in nested {
                strip_search_content_types(nested_tool);
            }
        }
    }
}

fn strip_search_content_types(tool: &mut Value) {
    if str_of(tool.get("type")) == "web_search" {
        delete(tool, "search_content_types");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // TestSanitizeMetaWebSearchTools_StripsSearchContentTypesFromWebSearch.
    #[test]
    fn strips_search_content_types_from_web_search() {
        let mut body = json!({
            "model": "muse-spark-1.3",
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {
                    "type": "web_search",
                    "external_web_access": true,
                    "search_content_types": ["text", "image"]
                },
                {"type": "web_search_preview", "search_content_types": ["text", "image"]},
                {
                    "type": "namespace",
                    "name": "search_group",
                    "tools": [{"type": "web_search", "search_content_types": ["text"]}]
                }
            ]
        });
        sanitize_web_search_tools(&mut body);

        let tool = &body["tools"][1];
        assert_eq!(tool["type"], "web_search");
        assert!(tool.get("search_content_types").is_none(), "{tool}");
        assert_eq!(tool["external_web_access"], true);

        let preview = &body["tools"][2];
        assert_eq!(preview["type"], "web_search_preview");
        assert!(preview.get("search_content_types").is_some(), "{preview}");

        let nested = &body["tools"][3]["tools"][0];
        assert_eq!(nested["type"], "web_search");
        assert!(nested.get("search_content_types").is_none(), "{nested}");

        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][0]["parameters"], json!({"type": "object"}));
    }

    // TestSanitizeMetaWebSearchTools_NoToolsOrEmpty. Upstream's nil input
    // has no counterpart: a body here is a parsed value.
    #[test]
    fn leaves_a_body_without_tools_alone() {
        let without = json!({"model": "muse-spark-1.3", "input": "hello"});
        let mut body = without.clone();
        sanitize_web_search_tools(&mut body);
        assert_eq!(body, without);

        // Not an array: nothing to strip.
        let odd = json!({"tools": {"type": "web_search", "search_content_types": ["text"]}});
        let mut body = odd.clone();
        sanitize_web_search_tools(&mut body);
        assert_eq!(body, odd);
    }
}
