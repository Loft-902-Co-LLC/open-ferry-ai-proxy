//! Checks that an answer is the fake upstream's, in the format the client
//! asked in: the whole text of [`ANSWER`], whichever format the fake
//! upstream gave it in, and for a stream, the event that ends it. Anything
//! else, such as an error the proxy made up or an answer cut short, is an
//! error of the run.

use serde_json::Value;

use crate::body::Format;
use crate::fake::ANSWER;

/// Checks `body`, the answer to a request in `format`, streamed or not.
pub fn check(format: Format, stream: bool, body: &[u8]) -> Result<(), String> {
    let (text, ended) = if stream {
        streamed(format, body)?
    } else {
        let value: Value = serde_json::from_slice(body)
            .map_err(|err| format!("an answer that isn't JSON ({err}): {}", start(body)))?;
        (whole(format, &value), true)
    };
    if text != ANSWER {
        return Err(format!(
            "an answer without the fake upstream's text, which had {:?}: {}",
            text.chars().take(100).collect::<String>(),
            start(body)
        ));
    }
    if !ended {
        return Err(format!("a stream without its last event: {}", start(body)));
    }
    Ok(())
}

/// The text of a whole answer.
fn whole(format: Format, value: &Value) -> String {
    match format {
        Format::Chat => {
            let content = &value["choices"][0]["message"]["content"];
            match content {
                Value::String(text) => text.clone(),
                // Some send the content as parts.
                Value::Array(parts) => texts(parts, "text"),
                _ => String::new(),
            }
        }
        Format::Claude => value["content"]
            .as_array()
            .map(|blocks| texts(blocks, "text"))
            .unwrap_or_default(),
        Format::Responses => value["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "message")
            .filter_map(|item| item["content"].as_array())
            .map(|parts| texts(parts, "output_text"))
            .collect(),
    }
}

/// The `text` of the items of type `kind`, joined.
fn texts(items: &[Value], kind: &str) -> String {
    items
        .iter()
        .filter(|item| item["type"] == kind)
        .filter_map(|item| item["text"].as_str())
        .collect()
}

/// The text of a stream of server-sent events, and whether it ended as
/// the format ends one: a choice with a finish reason, `message_stop`, or
/// `response.completed`.
fn streamed(format: Format, body: &[u8]) -> Result<(String, bool), String> {
    let body = std::str::from_utf8(body)
        .map_err(|_| format!("a stream that isn't UTF-8: {}", start(body)))?;
    let mut text = String::new();
    let mut ended = false;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim_start();
        if data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|err| format!("an event that isn't JSON ({err}): {data}"))?;
        match format {
            Format::Chat => {
                for choice in event["choices"].as_array().into_iter().flatten() {
                    if let Some(delta) = choice["delta"]["content"].as_str() {
                        text.push_str(delta);
                    }
                    if choice["finish_reason"].is_string() {
                        ended = true;
                    }
                }
            }
            Format::Claude => match event["type"].as_str() {
                Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                    text.push_str(event["delta"]["text"].as_str().unwrap_or_default());
                }
                Some("message_stop") => ended = true,
                _ => {}
            },
            Format::Responses => match event["type"].as_str() {
                Some("response.output_text.delta") => {
                    text.push_str(event["delta"].as_str().unwrap_or_default());
                }
                Some("response.completed") => ended = true,
                _ => {}
            },
        }
    }
    Ok((text, ended))
}

/// The start of an answer, for an error.
fn start(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .take(300)
        .collect::<String>()
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse(events: &[Value]) -> Vec<u8> {
        events
            .iter()
            .map(|event| format!("event: x\ndata: {event}\n\n"))
            .collect::<String>()
            .into_bytes()
    }

    // Not upstream's: the whole text in each client format passes, as one
    // answer or as events split anywhere.
    #[test]
    fn passes_the_whole_text() {
        let (head, tail) = ANSWER.split_at(7);
        let chat = json!({ "choices": [{ "message": { "content": ANSWER } }] });
        assert_eq!(
            check(Format::Chat, false, chat.to_string().as_bytes()),
            Ok(())
        );
        let chat = sse(&[
            json!({ "choices": [{ "delta": { "role": "assistant" }, "finish_reason": null }] }),
            json!({ "choices": [{ "delta": { "content": head } }] }),
            json!({ "choices": [{ "delta": { "content": tail } }] }),
            json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
        ]);
        let mut chat = chat;
        chat.extend_from_slice(b"data: [DONE]\n\n");
        assert_eq!(check(Format::Chat, true, &chat), Ok(()));

        let claude = json!({ "content": [{ "type": "text", "text": ANSWER }] });
        assert_eq!(
            check(Format::Claude, false, claude.to_string().as_bytes()),
            Ok(())
        );
        let claude = sse(&[
            json!({ "type": "message_start" }),
            json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": head } }),
            json!({ "type": "content_block_delta", "delta": { "type": "text_delta", "text": tail } }),
            json!({ "type": "message_stop" }),
        ]);
        assert_eq!(check(Format::Claude, true, &claude), Ok(()));

        let responses = json!({ "output": [
            { "type": "reasoning", "summary": [] },
            { "type": "message", "content": [{ "type": "output_text", "text": ANSWER }] },
        ] });
        assert_eq!(
            check(Format::Responses, false, responses.to_string().as_bytes()),
            Ok(())
        );
        let responses = sse(&[
            json!({ "type": "response.created" }),
            json!({ "type": "response.output_text.delta", "delta": head }),
            json!({ "type": "response.output_text.delta", "delta": tail }),
            json!({ "type": "response.output_text.done", "text": ANSWER }),
            json!({ "type": "response.completed" }),
        ]);
        assert_eq!(check(Format::Responses, true, &responses), Ok(()));
    }

    // Not upstream's: an answer in another format, cut short, without its
    // last event or with an error in it fails, saying what it had.
    #[test]
    fn fails_anything_else() {
        let claude = json!({ "content": [{ "type": "text", "text": ANSWER }] });
        let error = check(Format::Chat, false, claude.to_string().as_bytes()).unwrap_err();
        assert!(error.starts_with("an answer without the fake upstream's text, which had \"\""));

        let short = json!({ "choices": [{ "message": { "content": "BENCH the" } }] });
        assert!(check(Format::Chat, false, short.to_string().as_bytes()).is_err());

        let unended = sse(&[json!({ "type": "response.output_text.delta", "delta": ANSWER })]);
        assert_eq!(
            check(Format::Responses, true, &unended).unwrap_err(),
            format!("a stream without its last event: {}", start(&unended))
        );

        let error = check(Format::Claude, true, b"data: {nope\n\n").unwrap_err();
        assert!(error.starts_with("an event that isn't JSON"), "{error}");
        let error = check(Format::Claude, false, b"upstream error").unwrap_err();
        assert!(error.starts_with("an answer that isn't JSON"), "{error}");
    }
}
