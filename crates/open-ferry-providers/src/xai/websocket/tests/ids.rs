//! What a session remembers between its calls: the IDs the client is given
//! for xAI's repeated response IDs, and the transcript sent when xAI no
//! longer knows the previous response, ported from upstream's
//! `xai_websockets_executor_test.go`.

use std::sync::Arc;

use open_ferry_core::executor::ProviderExecutor as _;
use serde_json::{Value, json};

use super::super::ids::{Mapper, Store};
use super::{auth, auth_as, executor, streamed, ws_options};
use crate::codex::websocket::mock::{Answer, Server};
use crate::json::str_at;

/// A server whose every connection answers each message with a completed
/// response from `completed(message, authorization)`, until the client
/// ends it.
async fn answering(completed: fn(&Value, &str) -> Value) -> Server {
    Server::start(move |_| {
        Answer::accept(move |mut peer| async move {
            let authorization = peer
                .handshake()
                .header("authorization")
                .unwrap_or_default()
                .to_owned();
            while let Some(text) = peer.recv().await {
                let message: Value = serde_json::from_str(&text).unwrap();
                let event = json!({"type": "response.completed", "response": completed(&message, &authorization)});
                peer.send(&event.to_string()).await;
            }
        })
    })
    .await
}

/// The `n`th message the server read, as JSON.
fn sent(server: &Server, n: usize) -> Value {
    super::message(server, n)
}

/// The completed response of a call's chunks.
fn completed(chunks: &[String]) -> Value {
    super::last_event(chunks, "response.completed")["response"].clone()
}

// TestXAIWebsocketsExecuteStreamRewritesRepeatedResponseIDForDownstream
#[tokio::test]
async fn rewrites_repeated_response_id_for_downstream() {
    let server = answering(|message, _| {
        json!({
            "id": "resp-real",
            "previous_response_id": str_at(message, "previous_response_id"),
            "output": [{"id": "rs_resp-real", "type": "reasoning", "status": "completed"}],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
        })
    })
    .await;
    let executor = executor();
    let auth = auth_as("xai-auth-id-map", super::API_KEY, &server.url);
    let turn = |previous: String| {
        let body = if previous.is_empty() {
            r#"{"model":"grok-4.3","input":[{"type":"message","role":"user","content":"hello"}]}"#
                .to_owned()
        } else {
            json!({"model": "grok-4.3", "previous_response_id": previous, "input": [{"type": "function_call_output", "call_id": "call-1", "output": "ok"}]}).to_string()
        };
        let executor = &executor;
        let auth = &auth;
        async move {
            let response =
                completed(&streamed(executor, auth, &body, ws_options("xai-id-map-session")).await);
            (
                str_at(&response, "id"),
                str_at(&response, "output.0.id"),
                str_at(&response, "previous_response_id"),
            )
        }
    };

    let (first, first_output, first_previous) = turn(String::new()).await;
    assert_eq!(first, "resp-real");
    assert_eq!(first_output, "rs_resp-real");
    assert_eq!(first_previous, "");
    assert_eq!(str_at(&sent(&server, 0), "previous_response_id"), "");

    let (second, second_output, second_previous) = turn(first.clone()).await;
    assert!(
        !second.is_empty() && second != "resp-real",
        "the second ID {second:?} isn't one made up"
    );
    assert!(
        second_output != "rs_resp-real" && second_output.contains(&second),
        "{second_output:?} doesn't name {second:?}"
    );
    assert_eq!(second_previous, first);
    assert_eq!(
        str_at(&sent(&server, 1), "previous_response_id"),
        "resp-real"
    );

    let (third, third_output, third_previous) = turn(second.clone()).await;
    assert!(
        !third.is_empty() && third != "resp-real" && third != second,
        "the third ID {third:?} isn't a new one"
    );
    assert!(
        third_output != "rs_resp-real" && third_output.contains(&third),
        "{third_output:?} doesn't name {third:?}"
    );
    assert_eq!(third_previous, second);
    assert_eq!(
        str_at(&sent(&server, 2), "previous_response_id"),
        "resp-real"
    );
    assert_eq!(server.record().handshakes.len(), 1);
    executor.close_execution_session("xai-id-map-session");
}

// TestXAIWebsocketsExecuteStreamRewritesRepeatedResponseIDWithoutPreviousResponseID
#[tokio::test]
async fn rewrites_repeated_response_id_without_previous_response_id() {
    let server = answering(|_, _| {
        json!({
            "id": "resp-real",
            "output": [{"id": "msg_resp-real", "type": "message", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
        })
    })
    .await;
    let executor = executor();
    let auth = auth_as("xai-auth-id-map-no-prev", super::API_KEY, &server.url);
    let mut ids = Vec::new();
    for content in ["first", "second"] {
        let body = json!({"model": "grok-4.3", "input": [{"type": "message", "role": "user", "content": content}]});
        let response = completed(
            &streamed(
                &executor,
                &auth,
                &body.to_string(),
                ws_options("xai-id-map-no-prev-session"),
            )
            .await,
        );
        ids.push((str_at(&response, "id"), str_at(&response, "output.0.id")));
    }
    assert_eq!(ids[0], ("resp-real".to_owned(), "msg_resp-real".to_owned()));
    let (second, second_output) = &ids[1];
    assert!(
        !second.is_empty() && second != "resp-real",
        "the second ID {second:?} isn't one made up"
    );
    assert!(
        second_output != "msg_resp-real" && second_output.contains(second.as_str()),
        "{second_output:?} doesn't name {second:?}"
    );
    for n in 0..2 {
        assert_eq!(str_at(&sent(&server, n), "previous_response_id"), "");
    }
    executor.close_execution_session("xai-id-map-no-prev-session");
}

// TestXAIWebsocketsExecuteStreamReplaysTranscriptWhenAuthChanges: a
// credential xAI refused in between doesn't count as the session's.
#[tokio::test]
async fn replays_transcript_when_auth_changes() {
    let server = answering(|_, authorization| {
        let id = if authorization.contains("token-c") {
            "resp-auth-c"
        } else {
            "resp-auth-a"
        };
        json!({
            "id": id,
            "output": [{"type": "message", "id": format!("msg-{id}"), "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
        })
    })
    .await;
    let rejecting = Server::refusing(401, "rejected").await;
    let executor = executor();
    let session = "xai-auth-switch-session";

    let first = completed(
        &streamed(
            &executor,
            &auth_as("auth-a", "token-a", &server.url),
            r#"{"model":"grok-4.3","input":[{"type":"message","id":"user-1","role":"user","content":"first"}]}"#,
            ws_options(session),
        )
        .await,
    );
    let first_id = str_at(&first, "id");
    assert_eq!(first_id, "resp-auth-a");
    assert_eq!(
        server.record().handshakes[0].header("authorization"),
        Some("Bearer token-a")
    );

    let second_body = json!({"model": "grok-4.3", "previous_response_id": first_id, "input": [{"type": "message", "id": "user-2", "role": "user", "content": "second"}]}).to_string();
    let error = super::refused(
        &executor,
        &auth_as("auth-b", "token-b", &rejecting.url),
        &second_body,
        ws_options(session),
    )
    .await;
    assert_eq!(error.status, 401);

    streamed(
        &executor,
        &auth_as("auth-c", "token-c", &server.url),
        &second_body,
        ws_options(session),
    )
    .await;
    let record = server.record();
    assert_eq!(record.handshakes.len(), 2);
    assert_eq!(
        record.handshakes[1].header("authorization"),
        Some("Bearer token-c")
    );
    let replayed = sent(&server, 1);
    assert!(
        replayed.get("previous_response_id").is_none(),
        "previous_response_id was sent after the credential changed: {replayed}"
    );
    let ids: Vec<String> = replayed["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| str_at(item, "id"))
        .collect();
    assert_eq!(ids, ["user-1", "msg-resp-auth-a", "user-2"], "{replayed}");
    executor.close_execution_session(session);
}

// Not upstream's: a session's ID and transcript are forgotten when it is
// closed, so a previous response it named is xAI's again.
#[tokio::test]
async fn closing_a_session_forgets_its_ids() {
    let server = answering(|message, _| {
        json!({
            "id": "resp-real",
            "previous_response_id": str_at(message, "previous_response_id"),
            "output": [],
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
        })
    })
    .await;
    let executor = executor();
    let auth = auth(&server.url);
    let mut made_up = String::new();
    for _ in 0..2 {
        let chunks = streamed(&executor, &auth, super::HELLO, ws_options("forgetting")).await;
        made_up = str_at(&completed(&chunks), "id");
    }
    // The second response was given an ID of its own, which only the
    // session knows.
    assert_ne!(made_up, "resp-real");
    let body =
        json!({"model": "grok-4.3", "previous_response_id": made_up, "input": []}).to_string();
    executor.close_execution_session("forgetting");
    streamed(&executor, &auth, &body, ws_options("forgetting")).await;
    assert_eq!(str_at(&sent(&server, 2), "previous_response_id"), made_up);
    executor.close_execution_session("forgetting");
}

/// The mapper of a call in the session `id` of `store` whose client payload
/// is `payload`, and the body it sends.
fn upstream(store: &Store, id: &str, payload: &str) -> (Mapper, Value) {
    let payload: Value = serde_json::from_str(payload).unwrap();
    let mut mapper = Mapper::new(store.get(id).unwrap(), &payload);
    let mut body = payload;
    mapper.upstream_request(&mut body);
    (mapper, body)
}

fn input_ids(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .map(|items| items.iter().map(|item| str_at(item, "id")).collect())
        .unwrap_or_default()
}

// TestXAIWebsocketPostCompactionAppendWithoutPreviousReplaysCompactedTranscript
#[test]
fn post_compaction_append_without_previous_replays_compacted_transcript() {
    let store = Store::default();
    let id = "post-compaction-append-session";
    let state = store.get(id).unwrap();
    state.replace_transcript(vec![
        json!({"type": "compaction", "encrypted_content": "compact-state"}),
    ]);
    state.map_downstream_to_upstream("resp-compact", "");

    let (_, full) = upstream(
        &store,
        id,
        r#"{"type":"response.create","model":"grok-4.3","input":[{"type":"message","id":"msg-full"}]}"#,
    );
    assert_eq!(
        full["input"].as_array().unwrap().len(),
        1,
        "a self-contained response.create got the compacted transcript: {full}"
    );

    let (_, got) = upstream(
        &store,
        id,
        r#"{"type":"response.append","model":"grok-4.3","input":[{"type":"message","id":"msg-2","role":"user","content":"second"}]}"#,
    );
    let input = got["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "{got}");
    assert_eq!(input[0]["type"], "compaction", "{got}");
    assert_eq!(input[1]["id"], "msg-2", "{got}");

    state.record_turn(
        &got,
        br#"{"type":"response.completed","response":{"id":"resp-after-compact","output":[{"type":"message","id":"out-2"}]}}"#,
        true,
    );
    let (_, next) = upstream(
        &store,
        id,
        r#"{"type":"response.create","model":"grok-4.3","input":[{"type":"message","id":"msg-3"}]}"#,
    );
    assert_eq!(
        input_ids(&next),
        ["msg-3"],
        "the compacted transcript's replay wasn't cleared after a success: {next}"
    );
}

// TestXAIWebsocketPostCompactionWarmupPreservesTranscriptForLaterCompaction
#[test]
fn post_compaction_warmup_preserves_transcript_for_later_compaction() {
    let store = Store::default();
    let id = "warmup-reset-session";
    let state = store.get(id).unwrap();
    state.replace_transcript(vec![
        json!({"type": "compaction", "encrypted_content": "compact-state"}),
    ]);

    let (warmup, warmup_body) = upstream(
        &store,
        id,
        r#"{"type":"response.append","model":"grok-4.3","generate":false,"input":[{"type":"message","id":"warmup-context"}]}"#,
    );
    assert!(
        warmup.replayed(),
        "the warmup after a compaction didn't replay the transcript"
    );
    state.record_turn(
        &warmup_body,
        br#"{"type":"response.completed","response":{"id":"resp-warmup","output":[]}}"#,
        true,
    );

    let (_, append) = upstream(
        &store,
        id,
        r#"{"type":"response.append","model":"grok-4.3","input":[{"type":"message","id":"msg-after-warmup"}]}"#,
    );
    assert_eq!(
        input_ids(&append),
        ["msg-after-warmup"],
        "the warmup kept the replay pending: {append}"
    );
    state.record_turn(
        &append,
        br#"{"type":"response.completed","response":{"id":"resp-after-warmup","output":[{"type":"message","id":"out-after-warmup"}]}}"#,
        false,
    );

    let types: Vec<String> = state
        .transcript()
        .iter()
        .map(|item| str_at(item, "type"))
        .collect();
    assert_eq!(types, ["compaction", "message", "message", "message"]);
}

// TestXAIWebsocketEmptyFullResetClearsPendingCompactionReplay
#[test]
fn empty_full_reset_clears_pending_compaction_replay() {
    let store = Store::default();
    let id = "empty-reset-session";
    let state = store.get(id).unwrap();
    state.replace_transcript(vec![
        json!({"type": "compaction", "encrypted_content": "stale-compact-state"}),
    ]);
    state.record_turn(
        &json!({"type": "response.create", "model": "grok-4.3", "input": []}),
        br#"{"type":"response.completed","response":{"id":"resp-empty","output":[]}}"#,
        true,
    );
    let (_, got) = upstream(
        &store,
        id,
        r#"{"type":"response.append","model":"grok-4.3","input":[{"type":"message","id":"msg-new"}]}"#,
    );
    assert_eq!(
        input_ids(&got),
        ["msg-new"],
        "an empty full reset kept the stale compaction's replay: {got}"
    );
}

// Not upstream's: a blank session ID remembers nothing.
#[test]
fn a_blank_session_remembers_nothing() {
    let store = Store::default();
    assert!(store.get("  ").is_none());
    let state = store.get("kept").unwrap();
    assert!(Arc::ptr_eq(&state, &store.get(" kept ").unwrap()));
    store.delete("kept");
    assert!(!Arc::ptr_eq(&state, &store.get("kept").unwrap()));
}
