// Ported from CLIProxyAPI internal/runtime/executor/meta_executor_execute.go
// and helps/antigravity_compaction.go (v8.0.36, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Remote compaction v2: summarize a Responses history and seal a CPA capsule.
//! Claude's established capsule encoding and SSE construction are reused;
//! its summary execution and tool handling remain unchanged.

use bytes::Bytes;
use chrono::Utc;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response};
use open_ferry_core::observe::usage::{parse_codex_usage, parse_openai_usage};
use open_ferry_translate::json::exact;
use serde_json::{Value, json};

use super::MetaExecutor;
use crate::claude::compaction;
use crate::codex::request::base_model;
use crate::json::str_at;

fn invalid(message: impl Into<String>) -> ExecError {
    ExecError::upstream(502, message).with_usage_kept()
}

fn validate_summary(root: &Value) -> Result<(), ExecError> {
    let mut status = str_at(root, "status").trim().to_ascii_lowercase();
    if status.is_empty() {
        status = str_at(root, "response.status").trim().to_ascii_lowercase();
    }
    if status != "completed" {
        if status.is_empty() {
            status = "missing".into();
        }
        return Err(invalid(format!(
            "compaction summary response status is {status:?}, want \"completed\""
        )));
    }
    let kind = str_at(root, "type").trim().to_ascii_lowercase();
    if matches!(kind.as_str(), "response.incomplete" | "response.failed") {
        return Err(invalid(format!(
            "compaction summary response terminal event is {kind:?}"
        )));
    }
    for path in [
        "incomplete_details",
        "response.incomplete_details",
        "error",
        "response.error",
    ] {
        if crate::json::get(root, path).is_some_and(|node| !node.is_null()) {
            return Err(invalid(format!(
                "compaction summary response contains {path}"
            )));
        }
    }
    for item in root["output"].as_array().into_iter().flatten() {
        let status = str_at(item, "status").trim().to_ascii_lowercase();
        if !status.is_empty() && status != "completed" {
            return Err(invalid(format!(
                "compaction summary output item status is {status:?}, want \"completed\""
            )));
        }
    }
    Ok(())
}

fn build_response(model: &str, payload: &[u8]) -> Result<Bytes, ExecError> {
    let root: Value = serde_json::from_slice(payload).map_err(|_| {
        invalid("compaction summary response status is \"missing\", want \"completed\"")
    })?;
    validate_summary(&root)?;
    let mut text = Vec::new();
    for item in root["output"].as_array().into_iter().flatten() {
        if str_at(item, "type") != "message" {
            continue;
        }
        if let Some(content) = item["content"].as_str() {
            if !content.is_empty() {
                text.push(content.to_owned());
            }
        } else {
            for part in item["content"].as_array().into_iter().flatten() {
                if matches!(str_at(part, "type").as_str(), "output_text" | "text") {
                    let part = str_at(part, "text");
                    if !part.is_empty() {
                        text.push(part);
                    }
                }
            }
        }
    }
    let summary = text.join("\n");
    if summary.is_empty() {
        return Err(invalid(
            "extract summary: no summary text found in upstream response",
        ));
    }
    let capsule = compaction::seal(&summary, model)
        .map_err(|error| invalid(format!("seal compaction capsule: {}", error.message)))?;
    let detail = parse_codex_usage(payload)
        .filter(|detail| {
            detail.total_tokens > 0 || detail.input_tokens > 0 || detail.output_tokens > 0
        })
        .unwrap_or_else(|| parse_openai_usage(payload));
    let now = Utc::now();
    let id = now.timestamp_nanos_opt().unwrap_or_default();
    Ok(Bytes::from(json!({
        "object":"response.compaction", "status":"completed", "id":format!("resp_ag_compact_{id}"),
        "created_at":now.timestamp(), "model":model,
        "output":[{"type":"compaction", "status":"completed", "id":format!("cmp_ag_compact_{id}"), "encrypted_content":capsule}],
        "usage":{"input_tokens":detail.input_tokens, "output_tokens":detail.output_tokens, "total_tokens":detail.total_tokens,
            "input_tokens_details":{"cached_tokens":detail.cached_tokens}, "output_tokens_details":{"reasoning_tokens":detail.reasoning_tokens}}
    }).to_string()))
}

impl MetaExecutor {
    pub(super) async fn execute_compaction(
        &self,
        auth: &Auth,
        request: &Request,
        options: &Options,
    ) -> Result<Response, ExecError> {
        let mut payload: Value = exact::from_slice(&compaction::summary_payload(request, options))
            .expect("summary object");
        // Claude preserves tools to satisfy its tool-result pairing rules;
        // Responses summary requests remove both tool declarations as upstream does.
        for field in ["tools", "additional_tools"] {
            payload.as_object_mut().unwrap().shift_remove(field);
        }
        let summary_request = Request {
            model: request.model.clone(),
            payload: Bytes::from(payload.to_string()),
        };
        let mut summary_options = options.clone();
        summary_options.alt.clear();
        summary_options.stream = false;
        summary_options.original_request = Bytes::new();
        summary_options.source_format = Format::OPENAI_RESPONSE;
        summary_options.response_format = Format::OPENAI_RESPONSE;
        let response =
            Box::pin(self.execute_inner(auth, &summary_request, &summary_options)).await?;
        Ok(Response {
            payload: build_response(base_model(&request.model), &response.payload)?,
            headers: response.headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TestValidateResponsesCompactionSummaryStatus and summary extraction edge cases.
    #[test]
    fn only_completed_error_free_summaries_are_sealed() {
        let valid = json!({"status":"completed","output":[{"type":"message","content":[{"type":"text","text":"summary"}]}]});
        assert!(build_response("muse", valid.to_string().as_bytes()).is_ok());
        for patch in [
            json!({"status":"incomplete"}),
            json!({"status":"failed"}),
            json!({"status":null}),
            json!({"type":"response.incomplete"}),
            json!({"type":"response.failed"}),
            json!({"error":{}}),
            json!({"incomplete_details":{}}),
            json!({"response":{"error":{}}}),
            json!({"response":{"incomplete_details":{}}}),
            json!({"output":[{"type":"message","status":"in_progress","content":"partial"}]}),
            json!({"output":[]}),
            json!({"output":[{"type":"reasoning","content":"hidden"}]}),
        ] {
            let mut root = valid.clone();
            root.as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            assert_eq!(
                build_response("muse", root.to_string().as_bytes())
                    .unwrap_err()
                    .status,
                502,
                "{root}"
            );
        }
    }
}
