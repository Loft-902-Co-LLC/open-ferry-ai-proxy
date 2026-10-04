//! Not upstream's: what the request log scrubs from its files. Upstream
//! writes bodies, errors and names with the secrets in them; these are the
//! stricter rules of this port (see the module's deviations).

use std::io::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};

use super::{answer, files, logger};
use crate::auth::Auth;
use crate::exec::Format;
use crate::observe::redact::Secrets;
use crate::observe::request_log::{Answer, Downstream, Mode, RequestBody, finish};
use crate::observe::{AttemptKind, AttemptRequest, Outcome, RequestContext, Tap};

const COOKIE: &str = "cookie-secret-0123456789";
const MANAGEMENT_KEY: &str = "management-secret-0123456789";
const UPSTREAM_COOKIE: &str = "upstream-cookie-secret-0123";
const SECONDARY_KEY: &str = "secondary-key-0123456789";

fn context_with_key(path: &str, key: &str) -> Arc<RequestContext> {
    let context = RequestContext::new(Method::POST, path.to_owned());
    context.set_client_key(key);
    Arc::new(context)
}

fn downstream_with(path: &str, headers: &[(&'static str, &str)], body: Vec<u8>) -> Downstream {
    let mut map = HeaderMap::new();
    map.insert("content-type", HeaderValue::from_static("application/json"));
    for (name, value) in headers {
        map.append(*name, HeaderValue::from_str(value).unwrap());
    }
    Downstream {
        url: Downstream::url(path, None),
        secrets: Downstream::url_secrets(path, None),
        method: "POST".to_owned(),
        headers: map,
        body: RequestBody::Captured {
            raw: Bytes::from(body),
            truncated: false,
        },
    }
}

/// An upstream attempt sending `secrets`, echoed back in a `status` answer
/// whose head sets an upstream cookie, and failing after the head with an
/// error that quotes them all.
fn echoing_attempt(tap: &Arc<dyn Tap>, secrets: &Secrets, status: u16) {
    let auth = Auth::default();
    let format = Format::from("codex");
    let echo = secrets.iter().collect::<Vec<_>>().join(" ");
    let body = Bytes::from(format!("{{\"echo\":\"{echo}\"}}"));
    tap.attempt_request(&AttemptRequest {
        kind: AttemptKind::Execute,
        method: &Method::POST,
        url: "https://api.example.test/v1/responses",
        headers: &HeaderMap::new(),
        body: &body,
        provider: "codex",
        model: "gpt-5",
        format: &format,
        auth: &auth,
        secrets,
    });
    let mut head = HeaderMap::new();
    head.insert("content-type", HeaderValue::from_static("application/json"));
    head.insert(
        "set-cookie",
        HeaderValue::from_str(&format!("upstream={UPSTREAM_COOKIE}; Path=/")).unwrap(),
    );
    tap.response_head(status, &head);
    tap.chunk(&Bytes::from(format!(
        "{{\"error\":\"Invalid key: {echo} {UPSTREAM_COOKIE}\"}}"
    )));
    tap.attempt_error(&format!("stream broke: {echo} {UPSTREAM_COOKIE}"));
    tap.finish(Outcome::Failed);
}

// Not upstream's: a secret shorter than eight bytes, the attempt's or the
// client's key, is scrubbed from the file all the same, and a credential
// header of two bytes is hidden whole.
#[test]
fn scrubs_short_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let context = context_with_key("/v1/chat/completions", "1234567");
    assert_eq!(logger.start(&context), Some(Mode::Full));
    echoing_attempt(
        &logger.tap(&context).unwrap(),
        &Secrets::from_iter(["abc123"]),
        401,
    );
    finish(
        &context,
        downstream_with(
            "/v1/chat/completions",
            &[("x-api-key", "xq")],
            b"{\"key\":\"1234567\",\"input\":\"abc123\"}".to_vec(),
        ),
        answer(401, "application/json", b"{\"echo\":\"abc123\"}"),
    );
    logger.flush();

    let files = files(dir.path());
    let (_, log) = &files[0];
    assert!(!log.contains("abc123"), "{log}");
    assert!(!log.contains("1234567"), "{log}");
    assert!(log.contains("X-Api-Key: ...\n"), "{log}");
    assert!(!log.contains("xq"), "{log}");
    assert!(log.contains("{\"key\":\"[redacted]\""), "{log}");
}

// Not upstream's: the client's cookies and management key, an upstream's
// cookie and every credential an attempt sent are scrubbed from the bodies,
// the attempt's `Error:` line and the API ERROR section, with the request
// log on and in an error log.
#[test]
fn scrubs_cookies_management_keys_and_every_sent_credential() {
    for request_log in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let logger = logger(dir.path(), request_log);
        let context = context_with_key("/v1/responses", "client-key-abcdefgh");
        logger.start(&context).unwrap();
        let mut sent = Secrets::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer primary-key-0123456789"),
        );
        headers.insert("x-api-key", HeaderValue::from_static(SECONDARY_KEY));
        sent.add_headers(&headers);
        echoing_attempt(&logger.tap(&context).unwrap(), &sent, 500);
        context.request_log().record_api_error(
            500,
            &format!("upstream said {SECONDARY_KEY} {COOKIE} {MANAGEMENT_KEY}"),
            false,
        );
        finish(
            &context,
            downstream_with(
                "/v1/responses",
                &[
                    ("cookie", &format!("session={COOKIE}; theme=dark-mode")),
                    ("x-management-key", MANAGEMENT_KEY),
                ],
                format!("{{\"a\":\"{COOKIE}\",\"b\":\"{MANAGEMENT_KEY}\"}}").into_bytes(),
            ),
            answer(
                500,
                "application/json",
                b"{\"error\":\"secondary-key-0123456789\"}",
            ),
        );
        logger.flush();

        let files = files(dir.path());
        assert_eq!(files.len(), 1, "{files:?}");
        let (_, log) = &files[0];
        for secret in [
            COOKIE,
            MANAGEMENT_KEY,
            UPSTREAM_COOKIE,
            SECONDARY_KEY,
            "primary-key-0123456789",
        ] {
            assert!(!log.contains(secret), "{secret} in {log}");
        }
        assert!(log.contains("[redacted]"), "{log}");
        if request_log {
            assert!(log.contains("Error: stream broke: [redacted]"), "{log}");
            assert!(log.contains("=== API ERROR RESPONSE ===\n"), "{log}");
        }
    }
}

// Not upstream's: a compressed client body is decoded before it is
// scrubbed, and one that can't be decoded is left out, never written as
// it came.
#[test]
fn decodes_compressed_request_bodies_before_scrubbing() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let key = "client-secret-0123456789";
    let json = format!("{{\"key\":\"{key}\"}}").repeat(500);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(json.as_bytes()).unwrap();
    let gzip = gzip.finish().unwrap();
    for (encoding, body) in [("gzip", gzip.clone()), ("compress", gzip)] {
        let context = context_with_key("/v1/chat/completions", key);
        logger.start(&context).unwrap();
        finish(
            &context,
            downstream_with(
                "/v1/chat/completions",
                &[("content-encoding", encoding)],
                body,
            ),
            answer(200, "application/json", b"{}"),
        );
    }
    logger.flush();

    let files = files(dir.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let logs: Vec<&str> = files.iter().map(|(_, log)| log.as_str()).collect();
    assert!(
        logs.iter()
            .any(|log| log.contains("{\"key\":\"[redacted]\"}{\"key\":\"[redacted]\"}")),
        "{logs:?}"
    );
    assert!(
        logs.iter().any(|log| log.contains(
            "=== REQUEST BODY ===\n[ENCODED REQUEST BODY OMITTED: its Content-Encoding isn't supported]\n"
        )),
        "{logs:?}"
    );
    for log in logs {
        assert!(!log.contains(key), "{log}");
        assert!(!log.contains('\u{1f}'), "{log}");
    }
}

/// An answer to a request, `application/json` in the `encoding` it names.
fn encoded_answer(encoding: &'static str, body: Vec<u8>) -> Answer {
    let mut encoded = answer(200, "application/json", b"");
    encoded
        .headers
        .insert("content-encoding", HeaderValue::from_static(encoding));
    encoded.body.push(&Bytes::from(body));
    encoded
}

// Not upstream's: a compressed answer is decoded before it is scrubbed, and
// one that can't be decoded, or has an encoding not known here, is left out,
// never written as it came.
#[test]
fn decodes_compressed_answers_before_scrubbing() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logger(dir.path(), true);
    let key = "client-secret-0123456789";
    let json = format!("{{\"echo\":\"{key}\"}}");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(json.as_bytes()).unwrap();
    let gzip = gzip.finish().unwrap();
    for (encoding, body) in [
        ("gzip", gzip.clone()),
        ("gzip", b"RAWANSWERBYTES".to_vec()),
        ("compress", gzip),
    ] {
        let context = context_with_key("/v1/chat/completions", key);
        logger.start(&context).unwrap();
        finish(
            &context,
            downstream_with("/v1/chat/completions", &[], b"{}".to_vec()),
            encoded_answer(encoding, body),
        );
    }
    logger.flush();

    let files = files(dir.path());
    assert_eq!(files.len(), 3, "{files:?}");
    let logs: Vec<&str> = files.iter().map(|(_, log)| log.as_str()).collect();
    for expected in [
        "=== RESPONSE ===\nStatus: 200\nContent-Encoding: gzip\nContent-Type: application/json\n\n{\"echo\":\"[redacted]\"}\n",
        "[ENCODED RESPONSE BODY OMITTED: it couldn't be decoded]\n",
        "[ENCODED RESPONSE BODY OMITTED: its Content-Encoding isn't supported]\n",
    ] {
        assert!(
            logs.iter().any(|log| log.contains(expected)),
            "{expected}: {logs:?}"
        );
    }
    for log in logs {
        assert!(!log.contains(key), "{log}");
        assert!(!log.contains("RAWANSWERBYTES"), "{log}");
        assert!(!log.contains("DECOMPRESSION ERROR"), "{log}");
    }
}

// Not upstream's: a secret in the path is scrubbed from the name the file
// is given, as from its content.
#[test]
fn scrubs_secrets_from_file_names() {
    let dir = tempfile::tempdir().unwrap();
    for request_log in [true, false] {
        let logger = logger(dir.path(), request_log);
        let key = "client-secret-0123456789";
        let path = format!("/v1beta/models/{key}:generateContent");
        let context = context_with_key(&path, key);
        logger.start(&context).unwrap();
        finish(
            &context,
            downstream_with(&path, &[], b"{}".to_vec()),
            answer(400, "application/json", b"{\"error\":\"bad\"}"),
        );
        logger.flush();
    }
    let files = files(dir.path());
    assert_eq!(files.len(), 2, "{files:?}");
    for (name, log) in &files {
        assert!(!name.contains("client-secret"), "{name}");
        assert!(
            name.contains("v1beta-models-[redacted]-generateContent-"),
            "{name}"
        );
        assert!(!log.contains("client-secret"), "{log}");
    }
}
