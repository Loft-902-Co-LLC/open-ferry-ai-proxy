// Ported from CLIProxyAPI internal/translator/openai/openai/chat-completions/openai_openai_response.go
// (ConvertOpenAIResponseToOpenAI, ConvertOpenAIResponseToOpenAINonStream) (v8.0.10, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Chat Completions chunks → Chat Completions chunks.
//!
//! Each line passes through, a `data:` line's payload without the prefix,
//! until `[DONE]`, which is dropped along with everything after it.
//!
//! Deviations from upstream: none.

use crate::go;

/// Passes a Chat Completions stream through, one line at a time.
#[derive(Debug, Default)]
pub struct OpenAIToOpenAIStream {
    /// Whether `[DONE]` has been seen.
    done: bool,
}

impl OpenAIToOpenAIStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// The chunk to send the client for one line of the stream: a `data:`
    /// line's payload, trimmed, or any other line as it is. Gives nothing for
    /// `[DONE]` and for every line after it. The chunk may be empty.
    pub fn translate_line<'l>(&mut self, line: &'l [u8]) -> Option<&'l [u8]> {
        if self.done {
            return None;
        }
        let line = match line.strip_prefix(b"data:") {
            Some(data) => go::trim_space(data),
            None => line,
        };
        if line == b"[DONE]" {
            self.done = true;
            return None;
        }
        Some(line)
    }
}

/// Passes a whole Chat Completions response through as it is.
pub fn convert_openai_response_to_openai_non_stream(body: &[u8]) -> &[u8] {
    body
}

#[cfg(test)]
mod tests;
