//! Service-account keys and tokens. Upstream has no tests of `keyutil.go`;
//! its error messages were checked against upstream's code run with Go
//! 1.26. Tokens come from mock endpoints on 127.0.0.1, with a key made for
//! the test run.

use std::sync::atomic::{AtomicUsize, Ordering};

use aws_lc_rs::signature::{
    Ed25519KeyPair, KeyPair as _, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey,
};
use base64::engine::general_purpose::STANDARD;

use super::*;
use crate::gemini::testing::{Mock, Reply, pem, test_key_pkcs8, test_service_account};

/// The PKCS #1 `RSAPrivateKey` inside the test key's PKCS #8 DER.
fn test_key_pkcs1() -> Vec<u8> {
    let (_, info, _) = der_element(test_key_pkcs8()).unwrap();
    let (_, _version, rest) = der_element(info).unwrap();
    let (_, _algorithm, rest) = der_element(rest).unwrap();
    let (tag, key, _) = der_element(rest).unwrap();
    assert_eq!(tag, 0x04);
    key.to_vec()
}

fn account_with_key(key: &str) -> Map<String, Value> {
    let mut fields = test_service_account("http://127.0.0.1:9/token");
    fields.insert("private_key".into(), Value::from(key));
    fields
}

fn key_error(key: Value) -> String {
    let mut fields = test_service_account("http://127.0.0.1:9/token");
    fields.insert("private_key".into(), key);
    match service_account(&fields) {
        Ok(_) => panic!("the key was taken"),
        Err(error) => error,
    }
}

fn public_key() -> Vec<u8> {
    KeyPair::from_pkcs8(test_key_pkcs8())
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec()
}

#[test]
fn takes_pkcs8_and_pkcs1_keys() {
    let pkcs8 = pem("PRIVATE KEY", test_key_pkcs8());
    let pkcs1 = pem("RSA PRIVATE KEY", &test_key_pkcs1());
    // Of another type, either is found by trying both.
    let other_pkcs8 = pem("KEY", test_key_pkcs8());
    let other_pkcs1 = pem("KEY", &test_key_pkcs1());
    for key in [pkcs8, pkcs1, other_pkcs8, other_pkcs1] {
        let account = service_account(&account_with_key(&key)).unwrap();
        assert_eq!(account.key.public_key().as_ref(), public_key().as_slice());
    }
}

#[test]
fn cleans_up_keys() {
    let key = pem("PRIVATE KEY", test_key_pkcs8());
    let cases = [
        // Windows and old Mac line endings, and space around.
        format!("  \n{}\n\t", key.replace('\n', "\r\n")),
        key.replace('\n', "\r"),
        // Terminal escapes pasted with it.
        format!("\u{1b}[1;31m{key}\u{1b}]0;title\u{7}\u{1b}"),
        // Headers are skipped.
        key.replacen('\n', "\nProc-Type: 4,ENCRYPTED\n\n", 1),
        // Text before it, and a BEGIN line without its END.
        format!("garbage\n-----BEGIN NOTHING-----\n{key}"),
        // PEM framing lost: rebuilt from the base64 between the markers.
        key.replace('\n', " "),
    ];
    for (index, key) in cases.iter().enumerate() {
        let account = service_account(&account_with_key(key))
            .unwrap_or_else(|error| panic!("case {index}: {error}"));
        assert_eq!(account.key.public_key().as_ref(), public_key().as_slice());
    }
}

#[test]
fn rejects_unusable_keys() {
    let ed25519 = Ed25519KeyPair::generate_pkcs8v1(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
    let cases = [
        (Value::Null, "service account missing private_key"),
        (Value::from("   "), "service account missing private_key"),
        (Value::from(5), "service account missing private_key"),
        (
            Value::from("not a key"),
            "private_key is not valid pem: missing pem markers",
        ),
        (
            Value::from("-----END PRIVATE KEY----- -----BEGIN PRIVATE KEY-----"),
            "private_key is not valid pem: missing pem markers",
        ),
        (
            Value::from("-----BEGIN PRIVATE KEY----- !!! -----END PRIVATE KEY-----"),
            "private_key is not valid pem: private_key base64 payload empty",
        ),
        (
            Value::from("-----BEGIN PRIVATE KEY----- abc -----END PRIVATE KEY-----"),
            "private_key is not valid pem: private_key base64 decode failed: illegal base64 data at input byte 0",
        ),
        (
            Value::from("-----BEGIN PRIVATE KEY----- ab=c -----END PRIVATE KEY-----"),
            "private_key is not valid pem: private_key base64 decode failed: illegal base64 data at input byte 2",
        ),
        (
            Value::from("-----BEGIN PRIVATE KEY----- abcde -----END PRIVATE KEY-----"),
            "private_key is not valid pem: private_key base64 decode failed: illegal base64 data at input byte 4",
        ),
        (
            Value::from(pem("PRIVATE KEY", ed25519.as_ref())),
            "private_key is not an RSA key",
        ),
        (
            Value::from(pem("CERTIFICATE", &[1, 2, 3])),
            "private_key uses unsupported format",
        ),
    ];
    for (key, want) in cases {
        assert_eq!(key_error(key.clone()), want, "{key}");
    }
    // Upstream gives Go's reasons here; these are the library's.
    let error = key_error(Value::from(
        "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----",
    ));
    assert!(error.starts_with("private_key invalid pkcs8: "), "{error}");
    let error = key_error(Value::from(pem("RSA PRIVATE KEY", &[0x30, 0x03, 2, 1, 0])));
    assert!(error.starts_with("private_key invalid rsa: "), "{error}");
}

#[test]
fn key_errors_show_no_key() {
    let encoded = STANDARD.encode(test_key_pkcs8());
    // A key cut short, as one copied in part.
    let cut = &encoded[..encoded.len() / 2];
    for key in [
        format!("-----BEGIN PRIVATE KEY-----\n{cut}\n-----END PRIVATE KEY-----"),
        format!("-----BEGIN RSA PRIVATE KEY-----\n{cut}\n-----END RSA PRIVATE KEY-----"),
        format!("-----BEGIN PRIVATE KEY----- {cut}% -----END PRIVATE KEY-----"),
    ] {
        let error = key_error(Value::from(key));
        assert!(!error.contains(&cut[..16]), "{error}");
        assert!(!error.contains(&encoded[64..80]), "{error}");
    }
}

#[test]
fn decodes_pem_as_go_does() {
    let block = |data: &str| pem_decode(data.as_bytes()).map(|pem| (pem.kind, pem.der));
    assert_eq!(
        block("-----BEGIN A-----\nAQID\n-----END A-----\n"),
        Some(("A".into(), vec![1, 2, 3]))
    );
    // An empty block, trailing spaces, and text around.
    assert_eq!(
        block("x\n-----BEGIN A-----\n-----END A-----  \ny"),
        Some(("A".into(), Vec::new()))
    );
    // The first END, and the last BEGIN before it.
    assert_eq!(
        block("-----BEGIN A-----\n-----BEGIN B-----\nAQID\n-----END B-----\n"),
        Some(("B".into(), vec![1, 2, 3]))
    );
    // A block that doesn't parse is skipped for the next.
    assert_eq!(
        block("-----BEGIN A-----\n!!\n-----END A-----\n-----BEGIN C-----\nBA==\n-----END C-----"),
        Some(("C".into(), vec![4]))
    );
    for data in [
        "",
        "-----BEGIN A-----\nAQID\n",
        "-----BEGIN A-----\nAQID\n-----END B-----\n",
        "-----BEGIN A-----\nAQID\n-----END A----- x\n",
        "-BEGIN A-----\nAQID\n-----END A-----\n",
        "x-----BEGIN A-----\nAQID\n-----END A-----\n",
        "-----BEGIN A-----\nKey: value\n-----END A-----\n",
    ] {
        assert_eq!(block(data), None, "{data:?}");
    }
}

#[test]
fn reads_the_account() {
    let exchange_error = |field: &str, value: Value| {
        let mut fields = test_service_account("");
        fields.insert(field.into(), value);
        match service_account(&fields).unwrap().exchange() {
            Ok(_) => panic!("{field} was taken"),
            Err(error) => error,
        }
    };
    assert_eq!(
        exchange_error("type", Value::Null),
        "missing 'type' field in credentials"
    );
    assert_eq!(
        exchange_error("type", Value::from("authorized_user")),
        r#"unknown credential type: "authorized_user""#
    );
    assert_eq!(
        exchange_error("client_email", Value::from(1)),
        "field client_email is not a string"
    );

    let account = service_account(&test_service_account("")).unwrap();
    let exchange = account.exchange().unwrap();
    assert_eq!(exchange.token_uri, DEFAULT_TOKEN_URI);
    assert_eq!(exchange.private_key_id, "kid");
}

#[test]
fn signs_the_assertion() {
    let mut fields = test_service_account("https://token.test/token");
    let account = service_account(&fields).unwrap();
    let jwt = account.exchange().unwrap().assertion(1_000_000).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let decode = |part: &str| -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    };
    assert_eq!(
        String::from_utf8(URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap(),
        r#"{"alg":"RS256","typ":"JWT","kid":"kid"}"#
    );
    assert_eq!(
        decode(parts[1]),
        json!({
            "iss": "proxy-test@proxy-test.iam.gserviceaccount.com",
            "scope": SCOPE,
            "aud": "https://token.test/token",
            "exp": 1_000_000 - 10 + 3600,
            "iat": 1_000_000 - 10,
        })
    );
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_key())
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .unwrap();

    // Without a key ID, and with an audience of its own.
    fields.remove("private_key_id");
    fields.insert("audience".into(), Value::from("https://aud.test"));
    let account = service_account(&fields).unwrap();
    let jwt = account.exchange().unwrap().assertion(1_000_000).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(decode(parts[0]), json!({"alg": "RS256", "typ": "JWT"}));
    assert_eq!(decode(parts[1])["aud"], "https://aud.test");
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// A token endpoint that answers with `answer` and counts its calls.
async fn token_endpoint(answer: &str) -> Mock {
    Mock::start(Reply::json(answer)).await
}

#[tokio::test]
async fn fetches_and_caches_tokens() {
    let endpoint =
        token_endpoint(r#"{"access_token":"token-1","expires_in":3600,"token_type":"Bearer"}"#)
            .await;
    let account =
        service_account(&test_service_account(&format!("{}/token", endpoint.url))).unwrap();
    let cache = TokenCache::default();
    assert_eq!(cache.token(&client(), &account).await.unwrap(), "token-1");
    assert_eq!(cache.token(&client(), &account).await.unwrap(), "token-1");
    assert_eq!(endpoint.hits(), 1);

    let seen = endpoint.last();
    assert_eq!(seen.path, "/token");
    assert_eq!(
        seen.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    let form: HashMap<String, String> = url::form_urlencoded::parse(seen.body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(form["grant_type"], GRANT_TYPE);
    let parts: Vec<&str> = form["assertion"].split('.').collect();
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_key())
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .unwrap();
    // Nothing that names a Google client.
    assert!(seen.header("x-goog-api-client").is_none());

    // Another endpoint is another account.
    let other = token_endpoint(r#"{"access_token":"token-2","expires_in":3600}"#).await;
    let account = service_account(&test_service_account(&format!("{}/token", other.url))).unwrap();
    assert_eq!(cache.token(&client(), &account).await.unwrap(), "token-2");
    assert_eq!(cache.len(), 2);
}

#[tokio::test]
async fn keeps_only_tokens_that_last() {
    for answer in [
        // No expiry.
        r#"{"access_token":"token"}"#,
        // About to expire.
        r#"{"access_token":"token","expires_in":5}"#,
        r#"{"access_token":"token","expires_in":-1}"#,
    ] {
        let endpoint = token_endpoint(answer).await;
        let account =
            service_account(&test_service_account(&format!("{}/token", endpoint.url))).unwrap();
        let cache = TokenCache::default();
        for _ in 0..2 {
            assert_eq!(cache.token(&client(), &account).await.unwrap(), "token");
        }
        assert_eq!(endpoint.hits(), 2, "{answer}");
    }

    // No token at all is an empty one, and isn't kept either.
    let endpoint = token_endpoint(r#"{"expires_in":3600}"#).await;
    let account =
        service_account(&test_service_account(&format!("{}/token", endpoint.url))).unwrap();
    let cache = TokenCache::default();
    assert_eq!(cache.token(&client(), &account).await.unwrap(), "");
    assert_eq!(cache.token(&client(), &account).await.unwrap(), "");
    assert_eq!(endpoint.hits(), 2);
}

#[tokio::test]
async fn reports_failures_without_the_answer() {
    let cases = [
        (
            Reply::error(
                400,
                r#"{"error":"invalid_grant","error_description":"Invalid JWT","echo":"private"}"#,
            ),
            "400 Bad Request (invalid_grant: Invalid JWT)",
        ),
        (
            Reply::error(401, r#"{"error":"unauthorized_client"}"#),
            "401 Unauthorized (unauthorized_client)",
        ),
        (Reply::error(500, "private"), "500 Internal Server Error"),
        (Reply::json("private"), "the answer isn't a token"),
    ];
    for (reply, want) in cases {
        let endpoint = Mock::start(reply).await;
        let account =
            service_account(&test_service_account(&format!("{}/token", endpoint.url))).unwrap();
        let error = TokenCache::default()
            .token(&client(), &account)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            format!("vertex executor: get access token failed: oauth2: cannot fetch token: {want}")
        );
    }

    // An account that can't be exchanged is a parse error.
    let mut fields = test_service_account("http://127.0.0.1:9/token");
    fields.insert("type".into(), Value::from("external_account"));
    let error = TokenCache::default()
        .token(&client(), &service_account(&fields).unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        r#"vertex executor: parse service account json failed: unknown credential type: "external_account""#
    );
}

#[tokio::test]
async fn fetches_one_token_at_a_time_per_account() {
    let endpoint = token_endpoint(r#"{"access_token":"token","expires_in":3600}"#).await;
    let account =
        service_account(&test_service_account(&format!("{}/token", endpoint.url))).unwrap();
    let cache = TokenCache::default();
    let client = client();
    let calls = AtomicUsize::new(0);
    let fetch = || async {
        let token = cache.token(&client, &account).await.unwrap();
        calls.fetch_add(1, Ordering::Relaxed);
        token
    };
    let (first, second, third) = tokio::join!(fetch(), fetch(), fetch());
    assert_eq!([first, second, third], ["token", "token", "token"]);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(endpoint.hits(), 1);
}

#[test]
fn bounds_the_cache() {
    let cache = TokenCache::default();
    for index in 0..CACHE_LIMIT + 10 {
        let mut key = [0; 32];
        key[..8].copy_from_slice(&(index as u64).to_be_bytes());
        drop(cache.slot(key));
    }
    assert!(cache.len() <= CACHE_LIMIT);

    // A slot in use stays.
    let held = cache.slot([0xFF; 32]);
    for index in 0..CACHE_LIMIT + 10 {
        let mut key = [1; 32];
        key[..8].copy_from_slice(&(index as u64).to_be_bytes());
        drop(cache.slot(key));
    }
    assert!(cache.len() <= CACHE_LIMIT);
    assert!(Arc::ptr_eq(&held, &cache.slot([0xFF; 32])));
}

#[test]
fn go_base64_offsets() {
    for (data, want) in [
        ("", None),
        ("AQID", None),
        ("AQ==", None),
        ("AQI=", None),
        ("AQIDBA==", None),
        ("A", Some(0)),
        ("AQIDB", Some(4)),
        ("=AAA", Some(0)),
        ("A=AA", Some(1)),
        ("AQ=", Some(3)),
        ("AQ=A", Some(2)),
        ("AQ==AQID", Some(4)),
        ("AQI=A", Some(4)),
        ("AQIDAQIDAQ=", Some(11)),
        ("AQIDAQIDAQIDAQID=A", Some(16)),
    ] {
        assert_eq!(go_base64_error(data.as_bytes()), want, "{data}");
    }
}
