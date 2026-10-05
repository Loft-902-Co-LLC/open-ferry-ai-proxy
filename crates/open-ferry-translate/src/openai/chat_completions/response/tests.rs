// Ported from CLIProxyAPI internal/translator/openai/openai/chat-completions/openai_openai_response_test.go
// (v8.0.15, MIT). https://github.com/router-for-me/CLIProxyAPI
//
// drops_chunks_after_done checks the stream's `done` flag where upstream
// checks the `true` it leaves in the shared parameter.

use super::*;

#[test]
fn drops_chunks_after_done() {
    let mut stream = OpenAIToOpenAIStream::new();

    let first = stream.translate_line(br#"data: {"id":"x","choices":[]}"#);
    assert_eq!(first, Some(&br#"{"id":"x","choices":[]}"#[..]));

    assert_eq!(stream.translate_line(b"data: [DONE]"), None);
    assert!(stream.done);

    assert_eq!(
        stream.translate_line(br#"data: {"choices":[],"cost":"0"}"#),
        None
    );
}

#[test]
fn passthrough_without_done() {
    let mut stream = OpenAIToOpenAIStream::new();
    assert_eq!(
        stream.translate_line(br#"{"id":"y"}"#),
        Some(&br#"{"id":"y"}"#[..])
    );
}
