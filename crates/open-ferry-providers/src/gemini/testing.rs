//! What the tests of the Gemini and Vertex AI executors share: a mock server
//! on an ephemeral port of 127.0.0.1, requests, and a throwaway service
//! account whose RSA key is generated for the test run.

use std::io;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rsa::{KeyPair, KeySize};
use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures_util::StreamExt as _;
use http::HeaderMap;
use open_ferry_core::auth::Auth;
use open_ferry_core::exec::{ExecError, Format, Options, Request, StreamResponse};
use open_ferry_translate::registry::Registry;
use serde_json::{Map, Value, json};

/// One request a mock received.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    /// The request target as sent: absolute through a proxy.
    pub(crate) uri: String,
    pub(crate) path: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body: String,
}

impl Seen {
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|value| value.to_str().unwrap())
    }

    /// The path and query.
    pub(crate) fn target(&self) -> String {
        let uri: Uri = self.uri.parse().unwrap();
        uri.path_and_query().unwrap().to_string()
    }
}

/// What a mock answers a request with.
#[derive(Clone)]
pub(crate) struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Reply {
    pub(crate) fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_owned(),
        }
    }

    pub(crate) fn sse(body: &str) -> Self {
        Self {
            content_type: "text/event-stream",
            ..Self::json(body)
        }
    }

    pub(crate) fn error(status: u16, body: &str) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }
}

type Answer = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

/// A mock server bound to an ephemeral port on 127.0.0.1.
pub(crate) struct Mock {
    pub(crate) url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    /// A mock that answers every request with `reply`.
    pub(crate) async fn start(reply: Reply) -> Self {
        Self::answering(move |_| reply.clone()).await
    }

    /// A mock that answers each request as `answer` says.
    pub(crate) async fn answering(answer: impl Fn(&Seen) -> Reply + Send + Sync + 'static) -> Self {
        let answer: Answer = Arc::new(answer);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let answer = Arc::clone(&answer);
            let recorder = Arc::clone(&recorder);
            async move {
                let request = Seen {
                    uri: uri.to_string(),
                    path: uri.path().to_owned(),
                    headers,
                    body: String::from_utf8_lossy(&body).into_owned(),
                };
                let reply = answer(&request);
                recorder
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(request);
                let parts = vec![Ok::<_, io::Error>(Bytes::from(reply.body))];
                axum::response::Response::builder()
                    .status(reply.status)
                    .header("content-type", reply.content_type)
                    .body(Body::from_stream(futures_util::stream::iter(parts)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    pub(crate) fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn hits(&self) -> usize {
        self.requests().len()
    }

    pub(crate) fn last(&self) -> Seen {
        self.requests().pop().expect("no request reached the mock")
    }
}

/// A Gemini answer.
pub(crate) const OK_ANSWER: &str = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}"#;

/// A Gemini stream of one chunk.
pub(crate) const OK_STREAM: &str = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"chunk\"}]}}]}\n\n";

/// A Claude thought signature (upstream's `testClaudeCAISSample`), which
/// Gemini would reject.
pub(crate) const CLAUDE_SIGNATURE: &str = "CAISqwIKiAEIEBgCKkBHRlRBsNiptQUWfPoOhuQKwi5LnncZVO9bB5jqOs76D7uBtgktML0zqJtNmLHXHHcgD6lk4MQu4QBXzFd1lbC3Mg5jbGF1ZGUtZmFibGUtNTgBQgh0aGlua2luZ1okZDk3NDM5NzUtNGJiMC00OTM2LTllMjgtZDViMGQyMWJkYzQ4EgxCGh+XVFFFeySAjtAaDL/A1LltGu6MMJ+eXSIwsN0oBpDrqLv22UBfkMnTotnIbkvkOyb9xZHgigG6OZVHaI3gThm+maLKmgO5PrFLKlDFYp+YZksy/wKwszJlnLTPzAK+NUlfzagOE1ymtZTXhAYK260XyFYmg/te/C231+Fr/hoX+EJoUBnrn0gD7hqMISOT+TaFEuOXYsN517GfaxgB";

/// A thought signature of Gemini 3's own (upstream's
/// `testNativeGemini3ThoughtSignature`): field 2 holding field 1.
pub(crate) fn native_gemini_signature() -> String {
    STANDARD.encode([0x12, 0x08, 0x0a, 0x06, 0x01, 0x0c, 0x39, 0xd6, 0xc7, 0x34])
}

/// A Gemini request whose model turn calls a function with the thought
/// signature `signature`, and whose user turn answers it.
pub(crate) fn function_call_payload(signature: &str) -> String {
    json!({
        "contents": [
            {
                "role": "model",
                "parts": [{
                    "functionCall": {"name": "search", "args": {"q": "go"}},
                    "thoughtSignature": signature,
                }],
            },
            {
                "role": "user",
                "parts": [{"functionResponse": {"name": "search", "response": {"result": "found"}}}],
            },
        ]
    })
    .to_string()
}

/// Whether requests from `from` can be translated to Gemini yet. A test
/// that needs it is skipped, and says so, until the translator is ported.
pub(crate) fn translates_to_gemini(from: &Format, test: &str) -> bool {
    let ready = Registry::global().has_request_transformer(from, &Format::GEMINI);
    if !ready {
        eprintln!(
            "{test}: skipped, no {} to gemini request translator",
            from.as_str()
        );
    }
    ready
}

pub(crate) fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_owned()),
    }
}

pub(crate) fn options(format: &Format) -> Options {
    Options::new(format.clone())
}

pub(crate) fn stream_options(format: &Format) -> Options {
    Options {
        stream: true,
        ..options(format)
    }
}

/// A credential with an API key and a base URL.
pub(crate) fn key_auth(provider: &str, key: &str, base_url: &str) -> Arc<Auth> {
    let mut auth = Auth {
        provider: provider.into(),
        ..Auth::default()
    };
    auth.attributes.insert("api_key".into(), key.into());
    auth.attributes.insert("base_url".into(), base_url.into());
    Arc::new(auth)
}

/// The chunks of a stream, and its error if it failed.
pub(crate) async fn collect(response: StreamResponse) -> (Vec<String>, Option<ExecError>) {
    let mut chunks = response.chunks;
    let mut out = Vec::new();
    let mut error = None;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                assert!(error.is_none(), "a chunk after the error");
                out.push(String::from_utf8_lossy(&chunk).into_owned());
            }
            Err(failure) => {
                assert!(error.is_none(), "a second error: {failure:?}");
                error = Some(failure);
            }
        }
    }
    (out, error)
}

/// The PKCS #8 DER of an RSA key made for this test run.
pub(crate) fn test_key_pkcs8() -> &'static [u8] {
    static KEY: OnceLock<Vec<u8>> = OnceLock::new();
    KEY.get_or_init(|| {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = key.as_der().unwrap();
        der.as_ref().to_vec()
    })
}

/// `der` as a PEM block of `kind`, in lines of 64 characters.
pub(crate) fn pem(kind: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {kind}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {kind}-----\n"));
    out
}

/// A service account with the test key that gets its tokens from
/// `token_uri`, as upstream's `testVertexServiceAccountJSON` writes it.
pub(crate) fn test_service_account(token_uri: &str) -> Map<String, Value> {
    let Value::Object(fields) = json!({
        "type": "service_account",
        "project_id": "proxy-test",
        "private_key_id": "kid",
        "private_key": pem("PRIVATE KEY", test_key_pkcs8()),
        "client_email": "proxy-test@proxy-test.iam.gserviceaccount.com",
        "token_uri": token_uri,
    }) else {
        unreachable!()
    };
    fields
}
