//! Not upstream's, which passes xAI's answers on as they came: the secrets
//! a request sent are redacted from what the client gets of a successful
//! answer, as they are from an error, while the call's taps read the answer
//! as xAI sent it. The mock echoes a dummy key; nothing reaches xAI.

use super::*;

/// A key a model could echo back: longer than the eight bytes under which a
/// secret isn't redacted from what a client gets.
const ECHOED_KEY: &str = "xai-echo-key-0123456789";

/// A credential with [`ECHOED_KEY`] as its API key.
fn echoing_auth(base_url: &str) -> Arc<Auth> {
    let mut auth = (*api_key_auth(base_url)).clone();
    auth.attributes.insert("api_key".into(), ECHOED_KEY.into());
    Arc::new(auth)
}

const PAYLOAD: &str = r#"{"model":"grok-4.3","input":"hello"}"#;

/// xAI's stream for an answer in which the model says the key: a delta and
/// the completed response.
fn stream_saying_the_key() -> String {
    format!(
        concat!(
            "event: response.output_text.delta\n",
            "data: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"key {key}\"}}\n\n",
            "event: response.completed\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"created_at\":0,\"status\":\"completed\",\"model\":\"grok-4.3\",\"output\":[{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"key {key}\"}}]}}],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}}}\n\n",
        ),
        key = ECHOED_KEY
    )
}

/// xAI's stream for a truncated answer that says the key.
fn incomplete_saying_the_key() -> String {
    format!(
        "data: {{\"type\":\"response.incomplete\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"incomplete\",\"incomplete_details\":{{\"reason\":\"max_output_tokens\"}},\"output\":[{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"key {ECHOED_KEY}\"}}]}}],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}}}}\n\n"
    )
}

/// xAI's stream for a failure, with a 200, that quotes the key.
fn failed_quoting_the_key() -> String {
    format!(
        concat!(
            "event: response.failed\n",
            "data: {{\"type\":\"response.failed\",\"response\":{{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"failed\",\"error\":{{\"code\":\"invalid_api_key\",\"message\":\"bad key {key}\"}},\"output\":[]}}}}\n\n",
        ),
        key = ECHOED_KEY
    )
}

/// xAI's stream for an `error` event, with a 200, that quotes the key.
fn error_event_quoting_the_key() -> String {
    format!(
        "event: error\ndata: {{\"type\":\"error\",\"code\":\"invalid_api_key\",\"message\":\"bad key {ECHOED_KEY}\"}}\n\n"
    )
}

/// A compact answer of 200 with an `error` object that quotes the key.
fn compact_error_quoting_the_key() -> String {
    format!(
        r#"{{"id":"resp_1","object":"response.compaction","error":{{"message":"bad key {ECHOED_KEY}"}},"usage":{{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}}"#
    )
}

/// A compaction that has the key in its ID and its item, as one that
/// quotes the history it compacted can.
fn compaction_saying_the_key() -> String {
    format!(
        r#"{{"id":"resp_{ECHOED_KEY}","object":"response.compaction","output":[{{"type":"compaction","encrypted_content":"opaque","summary":"key {ECHOED_KEY}"}}],"usage":{{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}}"#
    )
}

/// A streaming request that asks for a compaction.
const TRIGGER_PAYLOAD: &str = r#"{"model":"grok-4.3","stream":true,"input":[{"role":"user","content":"hello"},{"type":"compaction_trigger"}]}"#;

// Not upstream's: an answer that succeeds but quotes the key, as a model can
// echo it back or a 200 can hold an `error` object, reaches a client whose
// call isn't streamed with it redacted: the stream such a call reads line by
// line, for a Responses client and for one that gets the answer translated,
// and a compact answer whole.
#[tokio::test]
async fn an_answer_that_echoes_the_key_hides_it() {
    for (name, reply, options) in [
        (
            "a completion",
            Reply::sse(&stream_saying_the_key()),
            options("openai-response"),
        ),
        (
            "a completion for a chat client",
            Reply::sse(&stream_saying_the_key()),
            options("openai"),
        ),
        (
            "a truncated answer",
            Reply::sse(&incomplete_saying_the_key()),
            options("openai-response"),
        ),
        (
            "a compact answer with an error object",
            Reply::json(&compact_error_quoting_the_key()),
            compact_options("openai-response"),
        ),
        (
            "a compaction",
            Reply::json(&compaction_saying_the_key()),
            compact_options("openai-response"),
        ),
    ] {
        let mock = Mock::start(reply).await;
        let response = executor()
            .execute(
                echoing_auth(&mock.url),
                request("grok-4.3", PAYLOAD),
                options,
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let shown = String::from_utf8_lossy(&response.payload);
        assert!(!shown.contains(ECHOED_KEY), "{name}: {shown}");
        assert!(shown.contains("[redacted]"), "{name}: {shown}");
    }
}

// Not upstream's: the same for a stream, whose lines are redacted one at a
// time, an event's or not: what the model says, a `response.failed` and an
// `error` event that quote the key, all with a 200.
#[tokio::test]
async fn a_stream_that_echoes_the_key_hides_it() {
    for (name, body, format) in [
        (
            "events for a Responses client",
            stream_saying_the_key(),
            "openai-response",
        ),
        (
            "events for a chat client",
            stream_saying_the_key(),
            "openai",
        ),
        (
            "a failed response",
            failed_quoting_the_key(),
            "openai-response",
        ),
        (
            "an error event",
            error_event_quoting_the_key(),
            "openai-response",
        ),
    ] {
        let mock = Mock::start(Reply::sse(&body)).await;
        let response = executor()
            .execute_stream(
                echoing_auth(&mock.url),
                request("grok-4.3", PAYLOAD),
                stream_options(format),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let (shown, _) = collect(response).await;
        assert!(!shown.contains(ECHOED_KEY), "{name}: {shown}");
        assert!(shown.contains("[redacted]"), "{name}: {shown}");
    }
}

// Not upstream's: a `compaction_trigger` request's events are made from the
// compact answer redacted whole, so a compaction that says the key, in its
// item or its ID, streams back without it.
#[tokio::test]
async fn a_compaction_trigger_stream_hides_the_key() {
    let mock = Mock::start(Reply::json(&compaction_saying_the_key())).await;
    let response = executor()
        .execute_stream(
            echoing_auth(&mock.url),
            request("grok-4.3", TRIGGER_PAYLOAD),
            stream_options("openai-response"),
        )
        .await
        .unwrap();
    let shown = streamed(Ok(response)).await;
    assert_eq!(mock.last().path, "/responses/compact");
    assert!(!shown.contains(ECHOED_KEY), "{shown}");
    let done = last_event(&shown, "response.output_item.done");
    assert_eq!(done["item"]["summary"], "key [redacted]", "{shown}");
    assert_eq!(done["item"]["encrypted_content"], "opaque", "{shown}");
}

// Not upstream's: the redaction is of what the client gets; the taps, which
// write to disk with their own redaction, read xAI's answer as it came, for
// a call that isn't streamed, a stream, a compaction and a compaction
// trigger.
#[tokio::test]
async fn the_taps_see_the_answer_as_it_came() {
    for (name, reply, payload, options) in [
        (
            "a call",
            Reply::sse(&stream_saying_the_key()),
            PAYLOAD,
            options("openai-response"),
        ),
        (
            "a stream",
            Reply::sse(&failed_quoting_the_key()),
            PAYLOAD,
            stream_options("openai-response"),
        ),
        (
            "a compaction",
            Reply::json(&compact_error_quoting_the_key()),
            PAYLOAD,
            compact_options("openai-response"),
        ),
        (
            "a compaction trigger",
            Reply::json(&compaction_saying_the_key()),
            TRIGGER_PAYLOAD,
            stream_options("openai-response"),
        ),
    ] {
        let mock = Mock::start(reply).await;
        let (observation, raw) = crate::secret_echo::Raw::observe();
        let options = Options {
            observation: Some(observation),
            ..options
        };
        let shown = if options.stream {
            let response = executor()
                .execute_stream(
                    echoing_auth(&mock.url),
                    request("grok-4.3", payload),
                    options,
                )
                .await
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            collect(response).await.0
        } else {
            let response = executor()
                .execute(
                    echoing_auth(&mock.url),
                    request("grok-4.3", payload),
                    options,
                )
                .await
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            String::from_utf8_lossy(&response.payload).into_owned()
        };
        assert!(!shown.contains(ECHOED_KEY), "{name}: {shown}");
        assert!(raw.seen().contains(ECHOED_KEY), "{name}: {}", raw.seen());
    }
}
