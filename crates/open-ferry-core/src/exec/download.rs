// Modelled on CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers.go
// (writeVideoContentFromURL, videoContentHTTPClient,
// videoContentDownloadAuth) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Fetching a file a provider made, such as a finished video, from the URL
//! the provider gave for it: what [`Dispatcher::download`] and
//! [`ProviderExecutor::download`] take and give back.
//!
//! The file is fetched without the credential's token or headers, through
//! the proxy of the credential that made it, or the global proxy when there
//! is none, as upstream's video handler fetches it. Nothing about the fetch
//! is recorded on the credential: a refused or expired URL says nothing
//! about the credential, and upstream's handler doesn't go through the auth
//! manager for it either.
//!
//! Deviations from upstream:
//! - Upstream's handler builds its own HTTP client from the credential's
//!   proxy; here the provider's executor makes the request with its shared
//!   clients, so the fetch takes the same proxy setting and redirect rules
//!   as the provider's calls.
//! - The `Debug` output of these types leaves out the URL's user info,
//!   query and fragment, which may hold a signature, and header values.
//!
//! [`Dispatcher::download`]: super::Dispatcher::download
//! [`ProviderExecutor::download`]: crate::executor::ProviderExecutor::download

use std::fmt;

use http::HeaderMap;

use super::http_call::{HeaderNames, redact_url};
use super::{ChunkStream, ProviderId};

/// A file to fetch, as the HTTP layer asks for it.
#[derive(Clone, Default)]
pub struct Download {
    /// The provider whose executor fetches the file, such as `xai`.
    pub provider: ProviderId,
    /// The credential that made the file, whose proxy the fetch goes
    /// through; empty, or one the manager no longer has, for the global
    /// proxy. Its token isn't sent.
    pub auth_id: String,
    /// The file's `http` or `https` URL.
    pub url: String,
}

impl fmt::Debug for Download {
    /// The download without its URL's user info, query or fragment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Download")
            .field("provider", &self.provider)
            .field("auth_id", &self.auth_id)
            .field("url", &redact_url(&self.url))
            .finish()
    }
}

/// The answer to a [`Download`], whatever its status.
pub struct Downloaded {
    /// The HTTP status.
    pub status: u16,
    /// The status line's text, such as `404 Not Found` (Go's
    /// `Response.Status`).
    pub status_text: String,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, read as the caller reads the stream. When the status isn't
    /// a success it has been read already, up to the executor's limit for
    /// error bodies, without the secrets the request sent, and comes as at
    /// most one chunk.
    pub body: ChunkStream,
}

impl fmt::Debug for Downloaded {
    /// The answer without header values or the body.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Downloaded")
            .field("status", &self.status)
            .field("status_text", &self.status_text)
            .field("headers", &HeaderNames(&self.headers))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;
    use http::HeaderValue;

    use super::*;

    // Not upstream's: Debug output leaves out what may be a secret.
    #[test]
    fn debug_output_leaves_out_secrets() {
        let download = Download {
            provider: "xai".into(),
            auth_id: "xai-1".into(),
            url: "https://user:USERINFO-SECRET@vidgen.example.com/v.mp4?sig=QUERY-SECRET#FRAGMENT-SECRET".into(),
        };
        let mut headers = HeaderMap::new();
        headers.insert("set-cookie", HeaderValue::from_static("HEADER-SECRET"));
        let downloaded = Downloaded {
            status: 200,
            status_text: "200 OK".into(),
            headers,
            body: futures_util::stream::empty().boxed(),
        };
        let text = format!("{download:?} {downloaded:?}");
        for secret in [
            "USERINFO-SECRET",
            "QUERY-SECRET",
            "FRAGMENT-SECRET",
            "HEADER-SECRET",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(
            text.contains(r#"url: "https://[redacted]@vidgen.example.com/v.mp4?[redacted]""#),
            "{text}"
        );
        assert!(text.contains(r#"headers: ["set-cookie"]"#), "{text}");
    }
}
