// Ported from CLIProxyAPI internal/runtime/executor/claude_executor_compaction.go
// and helps/antigravity_compaction.go (the Claude capsule path) (v8.0.27, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Responses compaction through a Claude summary, with CPA-compatible sealed
//! context capsules. The fixed upstream key makes this a portable opaque
//! format, not a means of keeping summaries secret from other proxy users.
//! Generated response and item IDs stay downstream; none is sent to Claude.

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::rand::{SecureRandom as _, SystemRandom};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt as _;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Format, Options, Request, Response, StreamResponse};
use open_ferry_translate::json::exact;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::executor::{COMPACT_ALT, ClaudeExecutor};
use super::ratelimit::plain_error;
use super::thinking;
use crate::json::{int_at, str_at};

const PREFIX: &str = "cpa-ag-compact-v1:";
const SUMMARY_PROMPT: &str = "Please provide a concise and comprehensive summary of the preceding conversation and task progress so far, including user goals, key findings, actions taken, and current status, so that work can continue smoothly.";

#[derive(Serialize, Deserialize)]
struct Capsule {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    created_at: i64,
}

fn key() -> Result<LessSafeKey, ExecError> {
    let digest = Sha256::digest(b"CLIProxyAPI");
    UnboundKey::new(&AES_256_GCM, &digest)
        .map(LessSafeKey::new)
        .map_err(|_| plain_error("create cipher: invalid compaction key"))
}

pub(super) fn seal(summary: &str, model: &str) -> Result<String, ExecError> {
    let mut plaintext = serde_json::to_vec(&Capsule {
        summary: summary.into(),
        model: model.into(),
        created_at: Utc::now().timestamp(),
    })
    .map_err(|error| plain_error(format!("marshal compaction capsule: {error}")))?;
    let mut nonce = [0; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| plain_error("generate nonce: random source failed"))?;
    key()?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut plaintext,
        )
        .map_err(|_| plain_error("seal compaction capsule: encryption failed"))?;
    let mut ciphertext = nonce.to_vec();
    ciphertext.extend(plaintext);
    Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(ciphertext)))
}

fn unseal(content: &str) -> Result<String, ExecError> {
    let raw = content
        .strip_prefix(PREFIX)
        .ok_or_else(|| plain_error("unrecognized compaction capsule format"))?;
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|error| plain_error(format!("decode compaction capsule: {error}")))?;
    if ciphertext.len() < 12 + 16 {
        return Err(plain_error("compaction capsule ciphertext too short"));
    }
    let nonce: [u8; 12] = ciphertext[..12].try_into().expect("nonce length checked");
    let plaintext = key()?
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut ciphertext[12..],
        )
        .map_err(|_| {
            plain_error("invalid or corrupted compaction capsule: authentication failed")
        })?;
    serde_json::from_slice::<Capsule>(plaintext)
        .map(|data| data.summary)
        .map_err(|error| plain_error(format!("unmarshal compaction capsule: {error}")))
}

fn expand_payload(raw: &Bytes) -> Result<Bytes, ExecError> {
    let Ok(mut body) = exact::from_slice(raw) else {
        return Ok(raw.clone());
    };
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(raw.clone());
    };
    let mut changed = false;
    let mut dropped = 0;
    let mut expanded = Vec::with_capacity(input.len());
    for item in input.iter() {
        if str_at(item, "type") != "compaction" {
            expanded.push(item.clone());
            continue;
        }
        changed = true;
        let content = str_at(item, "encrypted_content");
        if !content.trim().starts_with(PREFIX) {
            dropped += 1;
            continue;
        }
        let summary = unseal(&content).map_err(|error| {
            ExecError::upstream(
                400,
                format!("invalid compaction capsule: {}", error.message),
            )
        })?;
        expanded.push(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":format!("Context summary from previous turns:\n{summary}")}]}));
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "claude compaction: dropped non-CPA compaction items"
        );
    }
    if !changed {
        return Ok(raw.clone());
    }
    *input = expanded;
    Ok(Bytes::from(body.to_string()))
}

pub(super) fn expand(
    request: &Request,
    options: &Options,
) -> Result<(Request, Options), ExecError> {
    let mut request = request.clone();
    let mut options = options.clone();
    request.payload = expand_payload(&request.payload)?;
    options.original_request = expand_payload(&options.original_request)?;
    Ok((request, options))
}

fn has_trigger(raw: &[u8]) -> bool {
    serde_json::from_slice::<Value>(raw)
        .ok()
        .is_some_and(|body| {
            body.get("input")
                .and_then(Value::as_array)
                .is_some_and(|input| {
                    input
                        .iter()
                        .any(|item| str_at(item, "type") == "compaction_trigger")
                })
        })
}

pub(super) fn requested(request: &Request, options: &Options) -> bool {
    options.alt == COMPACT_ALT
        || has_trigger(&request.payload)
        || has_trigger(&options.original_request)
}

fn summary_payload(request: &Request, options: &Options) -> Bytes {
    let source = if request.payload.is_empty()
        || (!has_trigger(&request.payload) && has_trigger(&options.original_request))
    {
        &options.original_request
    } else {
        &request.payload
    };
    let mut body = exact::from_slice(source)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut input = match body.get("input") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| str_at(item, "type") != "compaction_trigger")
            .cloned()
            .collect(),
        Some(Value::String(text)) if !text.is_empty() => vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
        ],
        _ => Vec::new(),
    };
    input.push(json!({"type":"message","role":"user","content":[{"type":"input_text","text":SUMMARY_PROMPT}]}));
    body["input"] = Value::Array(input);
    for field in [
        "stream",
        "tool_choice",
        "previous_response_id",
        "parallel_tool_calls",
        "truncation",
        "metadata",
    ] {
        body.as_object_mut().expect("object").shift_remove(field);
    }
    body["stream"] = false.into();
    Bytes::from(body.to_string())
}

pub(super) fn finalize_summary(body: &mut Value) {
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        body["tool_choice"] = json!({"type":"none"});
        return;
    }
    for message in body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for block in message
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let text = match str_at(block, "type").as_str() {
                "tool_use" => Some(format!(
                    "Tool call {} ({}): {}",
                    str_at(block, "name"),
                    str_at(block, "id"),
                    block.get("input").map(Value::to_string).unwrap_or_default()
                )),
                "tool_result" => Some(format!(
                    "Tool result {}: {}",
                    str_at(block, "tool_use_id"),
                    block
                        .get("content")
                        .map(|content| content
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| content.to_string()))
                        .unwrap_or_default()
                )),
                _ => None,
            };
            if let Some(text) = text {
                *block = json!({"type":"text","text":text});
            }
        }
    }
}

impl ClaudeExecutor {
    pub(super) async fn execute_compaction(
        &self,
        request: &Request,
        options: &Options,
        auth: &Auth,
    ) -> Result<Response, ExecError> {
        let base_model = thinking::parse_suffix(&request.model).0;
        let summary_request = Request {
            model: request.model.clone(),
            payload: summary_payload(request, options),
        };
        let mut summary_options = options.clone();
        summary_options.alt.clear();
        summary_options.stream = false;
        summary_options.original_request = Bytes::new();
        summary_options.source_format = Format::OPENAI_RESPONSE;
        summary_options.response_format = Format::CLAUDE;
        let response = self
            .execute_messages_inner(auth, &summary_request, &summary_options, true)
            .await?;
        let body: Value = serde_json::from_slice(&response.payload).map_err(|_| {
            plain_error("extract summary: no summary text found in upstream response")
        })?;
        let summary = body
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|block| str_at(block, "type") == "text")
            .map(|block| str_at(block, "text"))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if summary.is_empty() {
            return Err(plain_error(
                "extract summary: no summary text found in upstream response",
            ));
        }
        let capsule = seal(&summary, base_model)?;
        let cached = int_at(&body, "usage.cache_read_input_tokens");
        let input = int_at(&body, "usage.input_tokens")
            + int_at(&body, "usage.cache_creation_input_tokens")
            + cached;
        let output = int_at(&body, "usage.output_tokens");
        let now = Utc::now();
        let id = now.timestamp_nanos_opt().unwrap_or_default();
        let payload = json!({"object":"response.compaction","status":"completed","id":format!("resp_ag_compact_{id}"),"created_at":now.timestamp(),"model":base_model,
            "output":[{"type":"compaction","status":"completed","id":format!("cmp_ag_compact_{id}"),"encrypted_content":capsule}],
            "usage":{"input_tokens":input,"output_tokens":output,"total_tokens":input+output,"input_tokens_details":{"cached_tokens":cached},"output_tokens_details":{"reasoning_tokens":0}}});
        Ok(Response {
            payload: Bytes::from(payload.to_string()),
            headers: response.headers,
        })
    }
}

pub(super) fn into_stream(response: Response) -> Result<StreamResponse, ExecError> {
    let mut completed: Value = serde_json::from_slice(&response.payload)
        .map_err(|error| plain_error(error.to_string()))?;
    let item = completed["output"][0].clone();
    completed["object"] = "response".into();
    completed["background"] = false.into();
    completed["error"] = Value::Null;
    completed["completed_at"] = completed["created_at"].clone();
    let mut created = completed.clone();
    created["status"] = "in_progress".into();
    created["output"] = json!([]);
    created
        .as_object_mut()
        .expect("object")
        .shift_remove("usage");
    created
        .as_object_mut()
        .expect("object")
        .shift_remove("completed_at");
    let mut pending_item = item.clone();
    pending_item["status"] = "in_progress".into();
    let events = [
        json!({"type":"response.created","sequence_number":0,"response":created}),
        json!({"type":"response.in_progress","sequence_number":1,"response":created}),
        json!({"type":"response.output_item.added","sequence_number":2,"output_index":0,"item":pending_item}),
        json!({"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":item}),
        json!({"type":"response.completed","sequence_number":4,"response":completed}),
    ];
    let chunks = events
        .into_iter()
        .map(|event| {
            Ok(Bytes::from(format!(
                "event: {}\ndata: {event}\n\n",
                str_at(&event, "type")
            )))
        })
        .collect::<Vec<_>>();
    let mut headers = response.headers;
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    Ok(StreamResponse {
        headers,
        chunks: futures_util::stream::iter(chunks).boxed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsules_round_trip_and_reject_tampering() {
        let capsule = seal("summary\n日本語", "claude-haiku").unwrap();
        assert_eq!(unseal(&capsule).unwrap(), "summary\n日本語");
        let mut raw = URL_SAFE_NO_PAD
            .decode(capsule.strip_prefix(PREFIX).unwrap())
            .unwrap();
        raw[12] ^= 1;
        assert!(unseal(&format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))).is_err());
        assert!(unseal(&format!("{PREFIX}AA")).is_err());
    }
}
